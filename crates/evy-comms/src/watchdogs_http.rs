//! W5 — v4-native watchdog diagnostics surface.
//!
//! Ports v3's `components/evy/watchdog-diag.ts` rich diagnostic surface
//! to a v4-native Rust registry the daemon boots. Three operator-facing
//! routes consume it (registered in [`crate::http`]):
//!
//! | Method | Path | Shape |
//! |---|---|---|
//! | `GET`  | `/api/evy/watchdogs/diag` | `{ watchdogs: [WatchdogDiag, …] }` |
//! | `POST` | `/api/evy/watchdogs/{id}/restart` | `{ ok }` \| `{ ok:false, error }` (404 when not restartable / unknown) |
//! | `POST` | `/api/evy/watchdogs/{id}/kill` | `{ ok, killed_id }` \| `{ ok:false, error }` (404 when unknown) |
//!
//! # Parity with v3
//!
//! The [`WatchdogDiag`] entry mirrors v3's `WatchdogDiagEntry` field-for-field
//! (`status`, `tick_history`, `recent_notifications`, `last_error`,
//! `can_restart`, `expected_interval_seconds`, `last_tick_ago_seconds`,
//! `memory_bytes`). Two fields are intentionally inert in v4, matching v3's
//! own behaviour:
//!
//! - `recent_notifications` is always `[]` — v3 correlates the operator
//!   notification channel to watchdogs by kind prefix; the v4 notification
//!   family is out of this slice's scope (one family only), so nothing is
//!   attributed. The frontend renders the empty case ("no notifications
//!   attributed to this watchdog") natively.
//! - `memory_bytes` is always `null` — v3 leaves it null too (per-watchdog
//!   heap diff was never wired).
//!
//! # Why a bespoke registry (not the framework's `WatchdogRegistry`)
//!
//! `evy-watchdog`'s [`WatchdogRegistry`](../../evy_watchdog/registry) runs a
//! single fan-out tick loop and discards per-watchdog tick state — it answers
//! "did anything tick?" over SSE, not "is THIS watchdog dead/slow, and what
//! were its last 20 ticks?". The diag surface needs retained per-watchdog
//! state plus per-watchdog kill/restart control, which a shared loop under one
//! cancellation token can't offer. This registry supplies both. It ticks an
//! opaque [`TickFn`] so it lives in `evy-comms` without depending on
//! `evy-watchdog` (which already depends on `evy-comms` — the reverse edge
//! would be a cycle); the daemon adapts the framework's real `Watchdog` impls
//! into `TickFn`s in `evy_watchdog::register_default_watchdogs`.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::{
    extract::{Path as AxPath, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::http::HttpState;

/// Max tick records retained per watchdog (v3 `TICK_HISTORY_LIMIT`).
const TICK_HISTORY_LIMIT: usize = 20;

/// Per-tick timeout. A [`TickFn`] that runs longer is recorded as an
/// unhealthy tick (mirrors `evy-watchdog`'s `DEFAULT_TICK_TIMEOUT`).
const DEFAULT_TICK_TIMEOUT: Duration = Duration::from_secs(5);

/// The healthy/unhealthy outcome of one [`TickFn`] invocation.
///
/// `error` is `Some` only when the tick itself failed (timeout, internal
/// error); it is surfaced as the watchdog's `last_error`.
#[derive(Debug, Clone)]
pub struct TickOutcome {
    /// Whether the watchdog ran cleanly this tick.
    pub healthy: bool,
    /// Error message recorded as `last_error` when the tick failed.
    pub error: Option<String>,
}

/// The future returned by a [`TickFn`].
pub type TickFuture = Pin<Box<dyn Future<Output = TickOutcome> + Send>>;

/// An opaque, re-invokable tick body. The registry calls it on the
/// watchdog's cadence; the daemon builds one per framework `Watchdog`.
///
/// `Fn` (not `FnOnce`) so a single registration can be ticked forever and
/// re-armed by [`WatchdogDiagRegistry::restart`] without rebuilding.
pub type TickFn = Arc<dyn Fn() -> TickFuture + Send + Sync>;

/// Registration parameters for one watchdog.
#[derive(Debug, Clone)]
pub struct WatchdogSpec {
    /// Stable id (also the kill/restart path segment).
    pub id: String,
    /// Category label. For framework watchdogs this equals `id` (matching
    /// v3, where e.g. `inbox-poll`'s id and kind coincide).
    pub kind: String,
    /// Expected tick interval in seconds, surfaced as
    /// `expected_interval_seconds`. `None` → unknown (status `unknown`);
    /// a negative value → long-poll / no fixed cadence (status `healthy`
    /// while registered), matching v3's `-1` sentinel.
    pub expected_interval_secs: Option<i64>,
    /// How often the registry invokes the tick body.
    pub period: Duration,
    /// Whether `restart` may re-arm this watchdog (drives `can_restart`).
    pub can_restart: bool,
}

/// One tick observation (v3 `TickRecord`). `ts` is ISO-8601 millis-Z.
#[derive(Debug, Clone, Serialize)]
pub struct TickRecord {
    /// ISO-8601 (millis, `Z`) timestamp the tick was recorded at.
    pub ts: String,
    /// Milliseconds since the previous tick, or `null` for the first tick.
    pub delta_ms: Option<i64>,
}

/// The last error a watchdog tick recorded (v3 `last_error`).
#[derive(Debug, Clone, Serialize)]
pub struct LastError {
    /// ISO-8601 (millis, `Z`) timestamp the error was recorded at.
    pub ts: String,
    /// The error message.
    pub message: String,
    /// Stack trace if available. Always `null` in v4 (no Rust backtrace
    /// captured for a tick error) — present for v3 shape parity.
    pub stack: Option<String>,
}

/// Derived health band for a watchdog (v3 `WatchdogStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchdogStatus {
    /// Last tick within 2× the expected interval (or long-poll, registered).
    Healthy,
    /// Last tick within 5× the expected interval.
    Degraded,
    /// No tick within 10× the expected interval.
    Dead,
    /// Expected interval unknown — cannot classify.
    Unknown,
}

/// One row of `GET /api/evy/watchdogs/diag` — mirrors v3 `WatchdogDiagEntry`.
#[derive(Debug, Clone, Serialize)]
pub struct WatchdogDiag {
    /// Stable watchdog id.
    pub id: String,
    /// Category label.
    pub kind: String,
    /// ISO-8601 (millis, `Z`) registration time.
    pub started_at: String,
    /// ISO-8601 (millis, `Z`) last tick, or `null` if never ticked.
    pub last_tick_at: Option<String>,
    /// Seconds since registration.
    pub age_seconds: i64,
    /// Derived status band.
    pub status: WatchdogStatus,
    /// Expected interval seconds (`null` unknown, `-1` long-poll, `N` known).
    pub expected_interval_seconds: Option<i64>,
    /// Seconds since the last tick, or `null` if never ticked.
    pub last_tick_ago_seconds: Option<i64>,
    /// Whether `restart` will re-arm this watchdog.
    pub can_restart: bool,
    /// Last 20 ticks observed (oldest first).
    pub tick_history: Vec<TickRecord>,
    /// Notifications attributed to this watchdog. Always `[]` in v4 — see
    /// module docs.
    pub recent_notifications: Vec<serde_json::Value>,
    /// Last recorded tick error, or `null`.
    pub last_error: Option<LastError>,
    /// Per-watchdog memory bytes. Always `null` (v3 parity).
    pub memory_bytes: Option<i64>,
}

/// Outcome of [`WatchdogDiagRegistry::kill`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillOutcome {
    /// The watchdog was found and removed; carries its id.
    Killed(String),
    /// No watchdog with that id is registered.
    Unknown,
}

/// Outcome of [`WatchdogDiagRegistry::restart`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartOutcome {
    /// The watchdog was re-armed (bounced).
    Restarted,
    /// The watchdog exists but `can_restart` is false.
    NotRestartable,
    /// No watchdog with that id is registered.
    Unknown,
}

/// Internal per-watchdog state. Not exposed; the wire shape is
/// [`WatchdogDiag`].
struct Entry {
    kind: String,
    expected_interval_secs: Option<i64>,
    period: Duration,
    started_at: DateTime<Utc>,
    last_tick_at: Option<DateTime<Utc>>,
    tick_history: VecDeque<TickRecord>,
    last_error: Option<LastError>,
    can_restart: bool,
    /// Reusable tick body — held so `restart` can re-spawn the loop.
    tick: TickFn,
    /// Per-watchdog cancellation — `kill`/`restart` cancel just this one.
    cancel: CancellationToken,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
}

/// The daemon's live watchdog registry. Holds per-watchdog tick state and
/// owns one detached tick task per watchdog. Cloneable handles share the
/// same state via the inner `Arc`.
///
/// See the module docs for why this is separate from `evy-watchdog`'s
/// `WatchdogRegistry`.
pub struct WatchdogDiagRegistry {
    inner: Arc<Mutex<Inner>>,
    /// Parent token; per-watchdog tokens are children so [`shutdown`](Self::shutdown)
    /// stops every loop at once.
    parent_cancel: CancellationToken,
    tick_timeout: Duration,
}

impl Default for WatchdogDiagRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WatchdogDiagRegistry {
    /// Build an empty registry. Register watchdogs with
    /// [`register`](Self::register); read state with
    /// [`diag_snapshot`](Self::diag_snapshot).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            parent_cancel: CancellationToken::new(),
            tick_timeout: DEFAULT_TICK_TIMEOUT,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Register and arm a watchdog: insert its state and spawn a detached
    /// task that invokes `tick` on `spec.period`. Re-registering the same id
    /// cancels and replaces the previous arming.
    ///
    /// Must be called from within a Tokio runtime (the daemon's
    /// `run_daemon`, or a `#[tokio::test]`), as it spawns the tick task.
    pub fn register(&self, spec: WatchdogSpec, tick: TickFn) {
        let cancel = self.parent_cancel.child_token();
        {
            let mut g = self.lock();
            if let Some(old) = g.entries.remove(&spec.id) {
                old.cancel.cancel();
            }
            g.entries.insert(
                spec.id.clone(),
                Entry {
                    kind: spec.kind,
                    expected_interval_secs: spec.expected_interval_secs,
                    period: spec.period,
                    started_at: Utc::now(),
                    last_tick_at: None,
                    tick_history: VecDeque::new(),
                    last_error: None,
                    can_restart: spec.can_restart,
                    tick: tick.clone(),
                    cancel: cancel.clone(),
                },
            );
        }
        spawn_tick_loop(
            spec.id,
            tick,
            spec.period,
            self.tick_timeout,
            self.inner.clone(),
            cancel,
        );
    }

    /// Kill a watchdog by id: cancel its tick task and drop its state so it
    /// disappears from [`diag_snapshot`](Self::diag_snapshot). Mirrors v3
    /// `killWatchdog`.
    pub fn kill(&self, id: &str) -> KillOutcome {
        let mut g = self.lock();
        match g.entries.remove(id) {
            Some(entry) => {
                entry.cancel.cancel();
                KillOutcome::Killed(id.to_owned())
            }
            None => KillOutcome::Unknown,
        }
    }

    /// Restart (bounce) a watchdog: cancel its current tick task, reset its
    /// tick history / timestamps, and re-arm with the same tick body. Only
    /// watchdogs registered with `can_restart = true` are restartable.
    /// Mirrors v3 `runRestartFactory` (re-arm without removing the entry).
    pub fn restart(&self, id: &str) -> RestartOutcome {
        let (tick, period) = {
            let g = self.lock();
            match g.entries.get(id) {
                None => return RestartOutcome::Unknown,
                Some(e) if !e.can_restart => return RestartOutcome::NotRestartable,
                Some(e) => (e.tick.clone(), e.period),
            }
        };
        let new_cancel = self.parent_cancel.child_token();
        {
            let mut g = self.lock();
            // Re-check under the lock: a concurrent kill may have removed it.
            let Some(entry) = g.entries.get_mut(id) else {
                return RestartOutcome::Unknown;
            };
            entry.cancel.cancel();
            entry.cancel = new_cancel.clone();
            entry.started_at = Utc::now();
            entry.last_tick_at = None;
            entry.tick_history.clear();
            entry.last_error = None;
        }
        spawn_tick_loop(
            id.to_owned(),
            tick,
            period,
            self.tick_timeout,
            self.inner.clone(),
            new_cancel,
        );
        RestartOutcome::Restarted
    }

    /// Snapshot every registered watchdog in v3 wire shape, sorted
    /// dead → degraded → unknown → healthy then id-ascending so the
    /// operator's eye lands on the bad ones first (mirrors v3
    /// `listWatchdogDiag`). `now` is injectable for deterministic tests.
    #[must_use]
    pub fn diag_snapshot(&self, now: DateTime<Utc>) -> Vec<WatchdogDiag> {
        let g = self.lock();
        let mut out: Vec<WatchdogDiag> = g
            .entries
            .iter()
            .map(|(id, e)| {
                let status =
                    classify_status(e.expected_interval_secs, e.started_at, e.last_tick_at, now);
                let last_tick_ago_seconds = e.last_tick_at.map(|t| (now - t).num_seconds().max(0));
                WatchdogDiag {
                    id: id.clone(),
                    kind: e.kind.clone(),
                    started_at: iso(e.started_at),
                    last_tick_at: e.last_tick_at.map(iso),
                    age_seconds: (now - e.started_at).num_seconds().max(0),
                    status,
                    expected_interval_seconds: e.expected_interval_secs,
                    last_tick_ago_seconds,
                    can_restart: e.can_restart,
                    tick_history: e.tick_history.iter().cloned().collect(),
                    recent_notifications: Vec::new(),
                    last_error: e.last_error.clone(),
                    memory_bytes: None,
                }
            })
            .collect();
        out.sort_by(|a, b| {
            status_rank(a.status)
                .cmp(&status_rank(b.status))
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    /// Cancel every watchdog's tick task. Called from the daemon's drain
    /// path; idempotent.
    pub fn shutdown(&self) {
        self.parent_cancel.cancel();
        // Drop the entries so a post-shutdown snapshot is empty and the
        // tasks (already cancelled) hold no references.
        self.lock().entries.clear();
    }
}

/// Spawn the detached per-watchdog tick loop. Ticks immediately so diag has
/// data promptly after register/restart, then sleeps `period` between ticks.
/// Exits when its `cancel` token fires or its entry is removed.
fn spawn_tick_loop(
    id: String,
    tick: TickFn,
    period: Duration,
    timeout: Duration,
    inner: Arc<Mutex<Inner>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let outcome = match tokio::time::timeout(timeout, (tick)()).await {
                Ok(o) => o,
                Err(_) => TickOutcome {
                    healthy: false,
                    error: Some(format!("tick timed out after {}s", timeout.as_secs())),
                },
            };
            // A concurrent kill/restart may have superseded this loop while
            // the tick ran — don't record into a reset or removed entry.
            if cancel.is_cancelled() {
                return;
            }
            let now = Utc::now();
            {
                let mut g = inner.lock().unwrap_or_else(PoisonError::into_inner);
                let Some(entry) = g.entries.get_mut(&id) else {
                    return;
                };
                let delta_ms = entry.last_tick_at.map(|p| (now - p).num_milliseconds());
                entry.tick_history.push_back(TickRecord {
                    ts: iso(now),
                    delta_ms,
                });
                while entry.tick_history.len() > TICK_HISTORY_LIMIT {
                    entry.tick_history.pop_front();
                }
                entry.last_tick_at = Some(now);
                if let Some(message) = outcome.error {
                    entry.last_error = Some(LastError {
                        ts: iso(now),
                        message,
                        stack: None,
                    });
                }
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(period) => {}
            }
        }
    });
}

/// Classify a watchdog's health from time-since-last-tick vs. its expected
/// interval. Faithful port of v3 `classifyStatus`:
/// healthy < 2× expected, degraded < 5×, dead ≥ 5× (10× per the v3 header
/// prose, but the code's `< 5×` else-dead is the authority). A never-ticked
/// watchdog is graded against its age since `started_at`.
fn classify_status(
    expected_secs: Option<i64>,
    started_at: DateTime<Utc>,
    last_tick_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> WatchdogStatus {
    let Some(expected) = expected_secs else {
        return WatchdogStatus::Unknown;
    };
    // Long-poll sentinel: healthy as long as registered.
    if expected < 0 {
        return WatchdogStatus::Healthy;
    }
    let expected_ms = expected.saturating_mul(1000);
    let elapsed_ms = match last_tick_at {
        Some(t) => (now - t).num_milliseconds(),
        None => (now - started_at).num_milliseconds(),
    };
    if elapsed_ms < expected_ms.saturating_mul(2) {
        WatchdogStatus::Healthy
    } else if elapsed_ms < expected_ms.saturating_mul(5) {
        WatchdogStatus::Degraded
    } else {
        WatchdogStatus::Dead
    }
}

/// Sort key: dead first, healthy last (v3 `listWatchdogDiag` rank).
fn status_rank(s: WatchdogStatus) -> u8 {
    match s {
        WatchdogStatus::Dead => 0,
        WatchdogStatus::Degraded => 1,
        WatchdogStatus::Unknown => 2,
        WatchdogStatus::Healthy => 3,
    }
}

/// Format a timestamp as ISO-8601 with millisecond precision and a `Z`
/// suffix — byte-compatible with v3's `new Date().toISOString()`.
fn iso(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

// ── HTTP handlers ─────────────────────────────────────────────────────────

/// `GET /api/evy/watchdogs/diag` → `{ watchdogs: [WatchdogDiag, …] }`.
///
/// Returns `200` with an empty list when no registry is wired (matching the
/// frontend's "no watchdogs registered" state).
pub(crate) async fn diag_handler(State(state): State<HttpState>) -> Response {
    let watchdogs = match state.app.watchdog_diag() {
        Some(reg) => reg.diag_snapshot(Utc::now()),
        None => Vec::new(),
    };
    Json(serde_json::json!({ "watchdogs": watchdogs })).into_response()
}

/// `POST /api/evy/watchdogs/{id}/restart` — re-arm a restartable watchdog.
/// `200 {ok:true}` on success; `404 {ok:false,error}` when the watchdog is
/// unknown or not restartable (mirrors v3 `runRestartFactory`).
pub(crate) async fn restart_handler(
    State(state): State<HttpState>,
    AxPath(id): AxPath<String>,
) -> Response {
    let Some(reg) = state.app.watchdog_diag() else {
        return not_found(&format!("unknown watchdog id: {id}"));
    };
    match reg.restart(&id) {
        RestartOutcome::Restarted => Json(serde_json::json!({ "ok": true })).into_response(),
        RestartOutcome::NotRestartable => not_found(&format!("no restart factory for: {id}")),
        RestartOutcome::Unknown => not_found(&format!("unknown watchdog id: {id}")),
    }
}

/// `POST /api/evy/watchdogs/{id}/kill` — kill a watchdog by id.
/// `200 {ok:true, killed_id}` on success; `404 {ok:false,error}` when
/// unknown (mirrors v3 `killWatchdog`).
pub(crate) async fn kill_handler(
    State(state): State<HttpState>,
    AxPath(id): AxPath<String>,
) -> Response {
    let outcome = match state.app.watchdog_diag() {
        Some(reg) => reg.kill(&id),
        None => KillOutcome::Unknown,
    };
    match outcome {
        KillOutcome::Killed(killed_id) => {
            Json(serde_json::json!({ "ok": true, "killed_id": killed_id })).into_response()
        }
        KillOutcome::Unknown => not_found(&format!("unknown watchdog id: {id}")),
    }
}

/// `404 { ok:false, error }` — the shared not-found body for restart/kill.
fn not_found(error: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "ok": false, "error": error })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_tick() -> TickFn {
        Arc::new(|| {
            Box::pin(async {
                TickOutcome {
                    healthy: true,
                    error: None,
                }
            })
        })
    }

    fn spec(id: &str, period: Duration, can_restart: bool) -> WatchdogSpec {
        WatchdogSpec {
            id: id.to_owned(),
            kind: id.to_owned(),
            expected_interval_secs: Some(1),
            period,
            can_restart,
        }
    }

    /// Poll the registry until `id`'s tick_history is non-empty or the
    /// budget elapses. Returns whether it ticked.
    async fn wait_for_tick(reg: &WatchdogDiagRegistry, id: &str) -> bool {
        for _ in 0..50 {
            let snap = reg.diag_snapshot(Utc::now());
            if snap
                .iter()
                .find(|w| w.id == id)
                .is_some_and(|w| !w.tick_history.is_empty())
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[test]
    fn classify_status_matches_v3_thresholds() {
        let started = Utc::now();
        // Unknown interval → unknown.
        assert_eq!(
            classify_status(None, started, None, started),
            WatchdogStatus::Unknown
        );
        // Long-poll sentinel → healthy.
        assert_eq!(
            classify_status(Some(-1), started, None, started),
            WatchdogStatus::Healthy
        );
        // expected 10s, last tick 5s ago (< 2×) → healthy.
        let last = started;
        let now = started + chrono::Duration::seconds(5);
        assert_eq!(
            classify_status(Some(10), started, Some(last), now),
            WatchdogStatus::Healthy
        );
        // last tick 25s ago (≥ 2×, < 5×) → degraded.
        let now = started + chrono::Duration::seconds(25);
        assert_eq!(
            classify_status(Some(10), started, Some(last), now),
            WatchdogStatus::Degraded
        );
        // last tick 60s ago (≥ 5×) → dead.
        let now = started + chrono::Duration::seconds(60);
        assert_eq!(
            classify_status(Some(10), started, Some(last), now),
            WatchdogStatus::Dead
        );
    }

    #[test]
    fn never_ticked_grades_against_age() {
        let started = Utc::now();
        // Age 1s, expected 10s → healthy (< 2×).
        let now = started + chrono::Duration::seconds(1);
        assert_eq!(
            classify_status(Some(10), started, None, now),
            WatchdogStatus::Healthy
        );
        // Age 60s, expected 10s, never ticked → dead.
        let now = started + chrono::Duration::seconds(60);
        assert_eq!(
            classify_status(Some(10), started, None, now),
            WatchdogStatus::Dead
        );
    }

    #[test]
    fn iso_is_millis_z() {
        let dt = DateTime::parse_from_rfc3339("2026-06-10T01:57:52.984Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(iso(dt), "2026-06-10T01:57:52.984Z");
    }

    #[tokio::test]
    async fn registered_watchdog_ticks_and_populates_history() {
        let reg = WatchdogDiagRegistry::new();
        reg.register(spec("hb", Duration::from_millis(30), true), healthy_tick());
        assert!(wait_for_tick(&reg, "hb").await, "watchdog should tick");
        let snap = reg.diag_snapshot(Utc::now());
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, "hb");
        assert_eq!(snap[0].kind, "hb");
        assert!(!snap[0].tick_history.is_empty());
        assert!(snap[0].last_tick_at.is_some());
        assert_eq!(snap[0].status, WatchdogStatus::Healthy);
        assert!(snap[0].can_restart);
        assert!(snap[0].recent_notifications.is_empty());
        assert!(snap[0].memory_bytes.is_none());
        // First tick has null delta; later ticks carry a delta.
        assert!(snap[0].tick_history[0].delta_ms.is_none());
        reg.shutdown();
    }

    #[tokio::test]
    async fn kill_removes_watchdog() {
        let reg = WatchdogDiagRegistry::new();
        reg.register(spec("hb", Duration::from_millis(30), true), healthy_tick());
        assert!(wait_for_tick(&reg, "hb").await);
        assert_eq!(reg.kill("hb"), KillOutcome::Killed("hb".to_owned()));
        assert!(reg.diag_snapshot(Utc::now()).is_empty());
        assert_eq!(reg.kill("hb"), KillOutcome::Unknown);
        reg.shutdown();
    }

    #[tokio::test]
    async fn restart_rearms_and_resets_history() {
        let reg = WatchdogDiagRegistry::new();
        reg.register(spec("hb", Duration::from_millis(20), true), healthy_tick());
        assert!(wait_for_tick(&reg, "hb").await);
        assert_eq!(reg.restart("hb"), RestartOutcome::Restarted);
        // After restart it re-arms and ticks again.
        assert!(wait_for_tick(&reg, "hb").await, "should tick after restart");
        reg.shutdown();
    }

    #[tokio::test]
    async fn restart_rejects_unrestartable_and_unknown() {
        let reg = WatchdogDiagRegistry::new();
        reg.register(
            spec("fixed", Duration::from_millis(30), false),
            healthy_tick(),
        );
        assert!(wait_for_tick(&reg, "fixed").await);
        assert_eq!(reg.restart("fixed"), RestartOutcome::NotRestartable);
        assert_eq!(reg.restart("nope"), RestartOutcome::Unknown);
        reg.shutdown();
    }

    #[tokio::test]
    async fn unhealthy_tick_records_last_error() {
        let reg = WatchdogDiagRegistry::new();
        let tick: TickFn = Arc::new(|| {
            Box::pin(async {
                TickOutcome {
                    healthy: false,
                    error: Some("boom".to_owned()),
                }
            })
        });
        reg.register(spec("err", Duration::from_millis(30), true), tick);
        assert!(wait_for_tick(&reg, "err").await);
        let snap = reg.diag_snapshot(Utc::now());
        let err = snap[0].last_error.as_ref().expect("last_error set");
        assert_eq!(err.message, "boom");
        assert!(err.stack.is_none());
        reg.shutdown();
    }

    #[tokio::test]
    async fn snapshot_sorts_dead_before_healthy() {
        let reg = WatchdogDiagRegistry::new();
        // A healthy watchdog (expected 1s, ticks every 20ms → always fresh).
        reg.register(
            spec("alive", Duration::from_millis(20), true),
            healthy_tick(),
        );
        // A dead watchdog: expected interval 0s means any elapsed time since
        // its (immediate) first tick exceeds 5× the interval ⇒ dead at real now.
        let stalled = WatchdogSpec {
            id: "stalled".to_owned(),
            kind: "stalled".to_owned(),
            expected_interval_secs: Some(0),
            period: Duration::from_secs(3600),
            can_restart: true,
        };
        reg.register(stalled, healthy_tick());
        assert!(wait_for_tick(&reg, "alive").await);
        // Classify at real now: 'alive' is healthy, 'stalled' (expected 0) dead.
        let snap = reg.diag_snapshot(Utc::now());
        assert_eq!(snap.len(), 2);
        // Dead sorts first.
        assert_eq!(snap[0].id, "stalled");
        assert_eq!(snap[0].status, WatchdogStatus::Dead);
        assert_eq!(snap[1].id, "alive");
        assert_eq!(snap[1].status, WatchdogStatus::Healthy);
        reg.shutdown();
    }
}
