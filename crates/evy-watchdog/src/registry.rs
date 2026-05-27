//! [`WatchdogRegistry`] — owns N watchdogs, ticks them on schedule.
//!
//! # Tick loop semantics
//!
//! 1. The registry holds `Vec<Arc<dyn Watchdog>>` plus a per-watchdog
//!    "next-due Instant" track.
//! 2. On `start`, a single tokio task runs the loop:
//!    - sleep until the earliest next-due Instant
//!    - for each due watchdog: spawn `tick()` inside a `tokio::time::timeout`
//!      (5s default), collect the [`TickReport`], emit a
//!      [`evy_comms::DaemonEvent::WatchdogTick`] event, append an
//!      [`evy_memory::ObservationKind::SchedulerFiredJob`]-shaped row to
//!      the observation log
//!    - reschedule next-due = now + schedule period
//! 3. Graceful shutdown via [`CancellationToken`]; the loop checks the
//!    token on every iteration.
//!
//! # Observation-log shape
//!
//! v4 doesn't (yet) have a watchdog-specific [`ObservationKind`] —
//! adding one would require touching `evy-memory`, which is outside
//! Phase 4's allowed-edits set. We append [`ObservationKind::SchedulerFiredJob`]
//! with `job_name = "watchdog:<name>"` and an `outcome` string summarising
//! the report. This is the v3 convention and keeps the log queryable
//! without an enum change.
//!
//! TODO: Phase 5 — add `ObservationKind::WatchdogTicked { name, findings }`
//! to `evy-memory` and switch this writer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use evy_comms::DaemonEvent;
use evy_core::Result;
use evy_memory::{Observation, ObservationKind};
use tokio::task::JoinHandle;
use tokio::time;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::report::TickReport;
use crate::trait_def::{Watchdog, WatchdogContext, WatchdogSchedule};

/// Per-tick timeout. A watchdog that takes longer than this is folded
/// into an unhealthy `TickReport` and reschedule continues.
pub const DEFAULT_TICK_TIMEOUT: Duration = Duration::from_secs(5);

/// Default sleep cap — even when the next due Instant is far away
/// (cron-only schedules, all watchdogs idle), the loop wakes at least
/// this often to honour the cancellation token.
const LOOP_TICK_CAP: Duration = Duration::from_secs(30);

/// Registry + spawned tick-loop for a set of [`Watchdog`]s.
///
/// Construct with [`WatchdogRegistry::new`], add watchdogs with
/// [`WatchdogRegistry::add`], then call [`WatchdogRegistry::start`]
/// to kick off the loop. `start` consumes the registry (via
/// `Arc<Self>`) so it can hand the watchdogs into the spawned task
/// without lifetime juggling.
pub struct WatchdogRegistry {
    watchdogs: Vec<Arc<dyn Watchdog>>,
    /// Per-watchdog tick timeout. Defaults to [`DEFAULT_TICK_TIMEOUT`].
    pub tick_timeout: Duration,
    /// JoinHandle for the started loop. `None` until `start` is called.
    handle: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl WatchdogRegistry {
    /// Build an empty registry. Call [`add`](Self::add) before
    /// [`start`](Self::start).
    #[must_use]
    pub fn new() -> Self {
        Self {
            watchdogs: Vec::new(),
            tick_timeout: DEFAULT_TICK_TIMEOUT,
            handle: tokio::sync::Mutex::new(None),
        }
    }

    /// Register a watchdog. Must be called before [`start`](Self::start).
    ///
    /// Panics on duplicate names — the registry's per-watchdog
    /// schedule tracking is keyed by `name()`, so duplicates would
    /// silently drop one. A panic at registration time is the loud
    /// failure mode we want.
    pub fn add(&mut self, w: Arc<dyn Watchdog>) {
        let name = w.name().to_owned();
        assert!(
            !self
                .watchdogs
                .iter()
                .any(|existing| existing.name() == name),
            "duplicate watchdog name: {name}"
        );
        self.watchdogs.push(w);
    }

    /// Number of registered watchdogs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.watchdogs.len()
    }

    /// True iff no watchdogs are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.watchdogs.is_empty()
    }

    /// Spawn the tick loop. Returns once the task is spawned; does
    /// not wait for any tick to complete.
    ///
    /// The loop continues until `shutdown.cancel()` is called.
    ///
    /// # Errors
    /// Currently infallible — returns [`evy_core::Result`] so future
    /// versions can validate the context (e.g. require non-empty
    /// providers when an auto-nudge watchdog is registered) without a
    /// breaking change.
    pub async fn start(
        self: Arc<Self>,
        ctx: WatchdogContext,
        shutdown: CancellationToken,
    ) -> Result<()> {
        info!(
            watchdogs = self.watchdogs.len(),
            timeout_secs = self.tick_timeout.as_secs(),
            "starting watchdog registry tick loop",
        );
        let watchdogs = self.watchdogs.clone();
        let tick_timeout = self.tick_timeout;
        let join = tokio::spawn(async move {
            run_loop(watchdogs, ctx, shutdown, tick_timeout).await;
        });
        *self.handle.lock().await = Some(join);
        Ok(())
    }

    /// Await the spawned tick loop's join handle, if any. Idempotent;
    /// safe to call when `start` was never called.
    pub async fn join(&self) {
        let h = self.handle.lock().await.take();
        if let Some(handle) = h {
            // Panics inside the loop are already logged via tracing;
            // we deliberately swallow the JoinError so shutdown stays
            // best-effort.
            let _ = handle.await;
        }
    }
}

impl Default for WatchdogRegistry {
    fn default() -> Self {
        Self::new()
    }
}

async fn run_loop(
    watchdogs: Vec<Arc<dyn Watchdog>>,
    ctx: WatchdogContext,
    shutdown: CancellationToken,
    tick_timeout: Duration,
) {
    // Per-watchdog next-due Instant. Cron-scheduled watchdogs (no
    // period) are mapped to `Instant::now() + LOOP_TICK_CAP` so the
    // loop doesn't busy-spin trying to tick them — Phase-4 inert per
    // module docs.
    let mut next_due: HashMap<String, Instant> = HashMap::with_capacity(watchdogs.len());
    let start = Instant::now();
    for w in &watchdogs {
        next_due.insert(w.name().to_owned(), start);
    }

    loop {
        // Wake at the earliest next-due Instant, but cap so we honour
        // the cancellation token even when watchdogs are far in the future.
        let now = Instant::now();
        let next_wake = next_due
            .values()
            .min()
            .copied()
            .unwrap_or_else(|| now + LOOP_TICK_CAP);
        let sleep_dur = next_wake.saturating_duration_since(now).min(LOOP_TICK_CAP);

        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                debug!("watchdog registry tick loop cancelled");
                return;
            }
            () = time::sleep(sleep_dur) => {}
        }

        let now = Instant::now();
        for w in &watchdogs {
            let name = w.name().to_owned();
            let due_at = next_due.get(&name).copied().unwrap_or(now);
            if due_at > now {
                continue;
            }
            let period = match w.schedule() {
                WatchdogSchedule::EveryNSecs(s) => Some(Duration::from_secs(s)),
                // TODO: Phase 5 — evaluate cron and schedule accordingly.
                WatchdogSchedule::Cron(_) => None,
            };

            let report = run_one_tick(w.as_ref(), &ctx, tick_timeout).await;

            // Fan out to SSE + observation log. Both are best-effort —
            // a slow subscriber or a sqlite-locked log must not stall
            // the loop.
            ctx.events.emit(DaemonEvent::WatchdogTick {
                name: report.watchdog.clone(),
                finding_count: report.findings.len(),
                healthy: report.healthy,
            });
            append_observation(&ctx, &report).await;

            // Reschedule. Cron-scheduled watchdogs become inert
            // (`next_due` set far in the future) until Phase 5 wires
            // a cron evaluator.
            let next = match period {
                Some(p) => now + p,
                None => now + Duration::from_secs(24 * 60 * 60),
            };
            next_due.insert(name, next);
        }
    }
}

async fn run_one_tick(w: &dyn Watchdog, ctx: &WatchdogContext, timeout: Duration) -> TickReport {
    let name = w.name().to_owned();
    let result = time::timeout(timeout, w.tick(ctx)).await;
    match result {
        Ok(Ok(report)) => report,
        Ok(Err(e)) => {
            warn!(watchdog = %name, error = %e, "watchdog tick returned Err");
            TickReport::unhealthy(name, format!("tick error: {e}"))
        }
        Err(_elapsed) => {
            warn!(
                watchdog = %name,
                timeout_secs = timeout.as_secs(),
                "watchdog tick exceeded timeout",
            );
            TickReport::unhealthy(name, format!("timed out after {}s", timeout.as_secs()))
        }
    }
}

async fn append_observation(ctx: &WatchdogContext, report: &TickReport) {
    let outcome = format!(
        "{healthy}|findings={count}",
        healthy = if report.healthy { "ok" } else { "unhealthy" },
        count = report.findings.len()
    );
    let obs = Observation::new(ObservationKind::SchedulerFiredJob {
        job_name: format!("watchdog:{}", report.watchdog),
        outcome,
    });
    if let Err(e) = ctx.obs_log.append(obs).await {
        warn!(watchdog = %report.watchdog, error = %e, "failed to append watchdog observation");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::watchdog_ctx;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock watchdog: counts ticks, returns healthy report.
    struct CountingWatchdog {
        name: String,
        period_secs: u64,
        ticks: Arc<AtomicUsize>,
    }

    impl CountingWatchdog {
        fn new(name: &str, period_secs: u64) -> (Self, Arc<AtomicUsize>) {
            let ticks = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    name: name.to_owned(),
                    period_secs,
                    ticks: ticks.clone(),
                },
                ticks,
            )
        }
    }

    #[async_trait]
    impl Watchdog for CountingWatchdog {
        fn name(&self) -> &str {
            &self.name
        }
        fn schedule(&self) -> WatchdogSchedule {
            WatchdogSchedule::EveryNSecs(self.period_secs)
        }
        async fn tick(&self, _ctx: &WatchdogContext) -> Result<TickReport> {
            self.ticks.fetch_add(1, Ordering::SeqCst);
            Ok(TickReport::healthy(&self.name))
        }
    }

    /// Mock watchdog that sleeps longer than the tick timeout.
    struct SlowWatchdog {
        sleep: Duration,
    }
    #[async_trait]
    impl Watchdog for SlowWatchdog {
        fn name(&self) -> &str {
            "slow"
        }
        fn schedule(&self) -> WatchdogSchedule {
            WatchdogSchedule::EveryNSecs(1)
        }
        async fn tick(&self, _ctx: &WatchdogContext) -> Result<TickReport> {
            tokio::time::sleep(self.sleep).await;
            Ok(TickReport::healthy("slow"))
        }
    }

    #[test]
    fn add_records_watchdogs() {
        let mut r = WatchdogRegistry::new();
        let (w, _ticks) = CountingWatchdog::new("a", 30);
        r.add(Arc::new(w));
        assert_eq!(r.len(), 1);
    }

    #[test]
    #[should_panic(expected = "duplicate watchdog name")]
    fn add_panics_on_duplicate_name() {
        let mut r = WatchdogRegistry::new();
        let (a, _) = CountingWatchdog::new("dup", 30);
        let (b, _) = CountingWatchdog::new("dup", 30);
        r.add(Arc::new(a));
        r.add(Arc::new(b));
    }

    #[tokio::test]
    async fn run_one_tick_times_out_slow_watchdog() {
        let (ctx, _guards) = watchdog_ctx().await;
        let slow = SlowWatchdog {
            sleep: Duration::from_secs(10),
        };
        let r = run_one_tick(&slow, &ctx, Duration::from_millis(50)).await;
        assert!(!r.healthy);
        assert_eq!(r.watchdog, "slow");
    }

    #[tokio::test]
    async fn run_one_tick_succeeds_within_timeout() {
        let (ctx, _guards) = watchdog_ctx().await;
        let (w, _) = CountingWatchdog::new("fast", 30);
        let r = run_one_tick(&w, &ctx, Duration::from_secs(1)).await;
        assert!(r.healthy);
        assert_eq!(r.watchdog, "fast");
    }

    #[tokio::test]
    async fn registry_ticks_fire_under_real_time() {
        // With a 1-second schedule and ~1.5s of real time, every
        // watchdog should tick at least once. We use real time rather
        // than tokio test-time because watchdog impls call ObservationLog
        // (sqlx) which is incompatible with `tokio::time::pause()`.
        let (ctx, _guards) = watchdog_ctx().await;
        let (w_a, ticks_a) = CountingWatchdog::new("a", 1);
        let (w_b, ticks_b) = CountingWatchdog::new("b", 1);
        let mut reg = WatchdogRegistry::new();
        reg.tick_timeout = Duration::from_secs(2);
        reg.add(Arc::new(w_a));
        reg.add(Arc::new(w_b));
        let reg = Arc::new(reg);
        let cancel = CancellationToken::new();
        reg.clone().start(ctx, cancel.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        cancel.cancel();
        reg.join().await;
        assert!(
            ticks_a.load(Ordering::SeqCst) >= 1,
            "watchdog A should have ticked at least once",
        );
        assert!(
            ticks_b.load(Ordering::SeqCst) >= 1,
            "watchdog B should have ticked at least once",
        );
    }

    #[tokio::test]
    async fn registry_emits_watchdog_tick_events() {
        let (ctx, _guards) = watchdog_ctx().await;
        let mut rx = ctx.events.subscribe();
        let (w, _) = CountingWatchdog::new("emit-test", 1);
        let mut reg = WatchdogRegistry::new();
        reg.tick_timeout = Duration::from_secs(2);
        reg.add(Arc::new(w));
        let reg = Arc::new(reg);
        let cancel = CancellationToken::new();
        reg.clone().start(ctx, cancel.clone()).await.unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("event arrival timed out")
            .expect("broadcaster closed");
        cancel.cancel();
        reg.join().await;
        match ev {
            DaemonEvent::WatchdogTick {
                name,
                healthy,
                finding_count,
            } => {
                assert_eq!(name, "emit-test");
                assert!(healthy);
                // healthy() emits a `Healthy` finding, so count == 1.
                assert_eq!(finding_count, 1);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn unhealthy_watchdog_still_emits_and_continues() {
        let (ctx, _guards) = watchdog_ctx().await;
        let mut rx = ctx.events.subscribe();
        let slow = SlowWatchdog {
            sleep: Duration::from_secs(5),
        };
        let mut reg = WatchdogRegistry::new();
        reg.tick_timeout = Duration::from_millis(20);
        reg.add(Arc::new(slow));
        let reg = Arc::new(reg);
        let cancel = CancellationToken::new();
        reg.clone().start(ctx, cancel.clone()).await.unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("event arrival timed out")
            .expect("broadcaster closed");
        cancel.cancel();
        reg.join().await;
        if let DaemonEvent::WatchdogTick { name, healthy, .. } = ev {
            assert_eq!(name, "slow");
            assert!(!healthy, "timed-out tick must report unhealthy");
        } else {
            panic!("expected WatchdogTick event");
        }
    }

    #[tokio::test]
    async fn shutdown_token_stops_the_loop() {
        let (ctx, _guards) = watchdog_ctx().await;
        let (w, _) = CountingWatchdog::new("stoppable", 1);
        let mut reg = WatchdogRegistry::new();
        reg.add(Arc::new(w));
        let reg = Arc::new(reg);
        let cancel = CancellationToken::new();
        reg.clone().start(ctx, cancel.clone()).await.unwrap();
        cancel.cancel();
        // join() must return promptly.
        tokio::time::timeout(Duration::from_secs(2), reg.join())
            .await
            .expect("join did not complete after cancel");
    }
}
