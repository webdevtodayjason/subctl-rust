//! v4-native operator notification tray — port of v3's
//! `components/evy/notifications.ts` + the `/notifications` REST routes in
//! `components/evy/server.ts` (≈4595-4625), surfaced by the dashboard as
//! `/api/evy/notifications*` (oracle: `http://127.0.0.1:8787`).
//!
//! This is the **operator-facing notification feed with read-state** — an
//! in-memory ring buffer the tray polls and marks read. It is deliberately
//! distinct from [`crate::notification`], which is the *outbound* Notification
//! payload type the Telegram/Discord channels render. The two never collide.
//!
//! ## Surface (v3-shape parity)
//! - `GET  /api/evy/notifications?since=<iso>&limit=N` → `{ ok, notifications:[…] }`
//! - `POST /api/evy/notifications/{id}/read`           → `{ ok, found }`
//! - `POST /api/evy/notifications/read-all`            → `{ ok, marked }`
//!
//! The live SSE channel (`/api/evy/notifications/stream`) is intentionally NOT
//! ported here — it keeps being served by the v3 reverse-proxy fallback
//! (`/api/{*rest}`), mirroring how `/api/evy/teams/tools` stays proxied. The
//! spec scoped this slice to the three REST tray endpoints above.
//!
//! ## Storage semantics (match v3)
//! In-memory only — a restart wipes the ring; every "is this still happening?"
//! signal is rebuilt on the next watchdog tick. The ring caps at
//! [`RING_LIMIT`] (oldest dropped). [`emit`] is the public entry point future
//! v4 watchdogs call to push a notification (v3's `emitNotification`); there is
//! no HTTP route that creates one, exactly as in v3.

use std::sync::{Mutex, OnceLock};

use axum::extract::{Path as AxPath, Query};
use axum::response::Json;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Ring capacity — oldest entries are dropped once exceeded. Matches v3's
/// `RING_LIMIT` (≈16h of 5-min watchdog ticks).
pub const RING_LIMIT: usize = 200;

/// Default `limit` when the caller omits it (matches v3's `?? 50`).
const DEFAULT_LIMIT: usize = 50;

/// Severity of a tray notification. Serializes lowercase to match v3's
/// `"info" | "warn" | "alert"` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotificationSeverity {
    /// Informational — no operator action required.
    Info,
    /// Warning — degraded but self-healing or low-stakes.
    Warn,
    /// Alert — operator attention warranted; pushed to Telegram in v3.
    Alert,
}

/// A single operator-facing notification (v3 `Notification` shape). Field
/// presence mirrors v3's `JSON.stringify`: `team_id`/`metadata` are omitted
/// when absent, `read_at` is always present (null until read).
#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    /// UUID v4 assigned at emit time.
    pub id: String,
    /// Stable kind string for filter/group (e.g. `"team-stale"`, `"upstream-available"`).
    pub kind: String,
    /// Severity bucket.
    pub severity: NotificationSeverity,
    /// Headline shown in the tray (truncated to ≤80 chars at emit).
    pub title: String,
    /// Full details; rendered as text by the tray.
    pub body: String,
    /// Optional team id (tmux session name) so the tray can group by team.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// ISO-8601 emit time.
    pub ts: String,
    /// Marked-read timestamp; `null` until read.
    pub read_at: Option<String>,
    /// Optional structured payload consumers read verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

/// Input to [`NotificationRing::emit`] / [`emit`] — the caller-supplied fields
/// (id/ts/read_at are assigned by the ring). Mirrors v3 `EmitNotificationInput`.
#[derive(Debug, Clone)]
pub struct EmitNotificationInput {
    /// Stable kind string.
    pub kind: String,
    /// Severity bucket.
    pub severity: NotificationSeverity,
    /// Headline (truncated to ≤80 chars on emit).
    pub title: String,
    /// Full details.
    pub body: String,
    /// Optional team id.
    pub team_id: Option<String>,
    /// Optional structured payload.
    pub metadata: Option<Map<String, Value>>,
}

/// Build a dedup fingerprint from `(kind, metadata.package, metadata.from,
/// metadata.to)`. Returns `None` unless all three metadata fields are present
/// **and string-typed** — exactly v3's `fingerprintFor`, which avoids
/// `[object Object]` false-positive collisions by strict type-checking.
fn fingerprint(kind: &str, metadata: Option<&Map<String, Value>>) -> Option<String> {
    let md = metadata?;
    let pkg = md.get("package")?.as_str()?;
    let from = md.get("from")?.as_str()?;
    let to = md.get("to")?.as_str()?;
    Some(format!("{kind} {pkg} {from} {to}"))
}

/// Truncate a title to ≤80 chars, appending `…` when cut — v3's
/// `title.length > 80 ? title.slice(0, 77) + "…" : title`. Counts by `char`
/// so multi-byte titles stay on a valid boundary.
fn truncate_title(title: &str) -> String {
    if title.chars().count() > 80 {
        let head: String = title.chars().take(77).collect();
        format!("{head}…")
    } else {
        title.to_string()
    }
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Parse an ISO-8601 timestamp to unix-millis. Mirrors v3's `Date.parse`:
/// unparseable input yields `None` (no `since` filtering is applied).
fn parse_ts_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// In-memory notification ring. Parameterized (not a bare global) so unit
/// tests can exercise emit/list/mark against an isolated instance, the way
/// `teams_http`'s `do_*` fns take a `&Path`.
#[derive(Debug, Default)]
pub struct NotificationRing {
    entries: Vec<Notification>,
}

impl NotificationRing {
    /// A fresh, empty ring.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a notification and return the materialized record.
    ///
    /// Dedup (v3 v2.8.8): if an **unread** entry with a matching fingerprint
    /// already lives in the ring, its `ts` is refreshed in place and that
    /// existing record is returned — no new row, no subscriber fan-out. Read
    /// entries always get a fresh row so an acknowledged-then-recurring
    /// condition still surfaces. Otherwise a new record is pushed and the ring
    /// is capped at [`RING_LIMIT`] (oldest dropped).
    pub fn emit(&mut self, input: EmitNotificationInput) -> Notification {
        if let Some(fp) = fingerprint(&input.kind, input.metadata.as_ref()) {
            for existing in self.entries.iter_mut().rev() {
                if existing.read_at.is_some() {
                    continue;
                }
                if fingerprint(&existing.kind, existing.metadata.as_ref()).as_deref() != Some(&fp) {
                    continue;
                }
                existing.ts = now_iso();
                return existing.clone();
            }
        }

        let n = Notification {
            id: uuid::Uuid::new_v4().to_string(),
            kind: input.kind,
            severity: input.severity,
            title: truncate_title(&input.title),
            body: input.body,
            team_id: input.team_id,
            ts: now_iso(),
            read_at: None,
            metadata: input.metadata,
        };
        self.entries.push(n.clone());
        while self.entries.len() > RING_LIMIT {
            self.entries.remove(0);
        }
        n
    }

    /// Snapshot the ring newest-first, optionally filtered by `since` (strictly
    /// newer) and capped by `limit` (clamped to `[1, RING_LIMIT]`). Returns
    /// owned clones — callers cannot mutate stored records.
    #[must_use]
    pub fn list(&self, since: Option<&str>, limit: Option<usize>) -> Vec<Notification> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, RING_LIMIT);
        let since_ms = since.and_then(parse_ts_ms);
        let mut out = Vec::new();
        for n in self.entries.iter().rev() {
            if out.len() >= limit {
                break;
            }
            if let Some(since_ms) = since_ms {
                // Strictly newer than `since` (v3 skips `ts <= sinceMs`).
                if parse_ts_ms(&n.ts).map(|t| t <= since_ms).unwrap_or(false) {
                    continue;
                }
            }
            out.push(n.clone());
        }
        out
    }

    /// Mark one notification read by id. Returns `true` if found (idempotent —
    /// re-reading keeps the original `read_at`).
    pub fn mark_read(&mut self, id: &str) -> bool {
        for n in self.entries.iter_mut().rev() {
            if n.id == id {
                if n.read_at.is_none() {
                    n.read_at = Some(now_iso());
                }
                return true;
            }
        }
        false
    }

    /// Mark every unread notification read. Returns the count newly read.
    pub fn mark_all_read(&mut self) -> usize {
        let now = now_iso();
        let mut marked = 0;
        for n in self.entries.iter_mut() {
            if n.read_at.is_none() {
                n.read_at = Some(now.clone());
                marked += 1;
            }
        }
        marked
    }

    /// Count of unread notifications — the tray badge.
    #[must_use]
    pub fn unread_count(&self) -> usize {
        self.entries.iter().filter(|n| n.read_at.is_none()).count()
    }
}

/// Process-global production ring shared across requests (v3's module-global
/// `_ring`). Unit tests use isolated [`NotificationRing`] instances instead.
fn ring() -> &'static Mutex<NotificationRing> {
    static RING: OnceLock<Mutex<NotificationRing>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(NotificationRing::new()))
}

/// Emit a notification into the process-global ring. The public entry point
/// future v4 watchdogs call (v3 `emitNotification`). Returns the materialized
/// record for callers that want the assigned id/ts.
pub fn emit(input: EmitNotificationInput) -> Notification {
    ring().lock().expect("notification ring mutex").emit(input)
}

/// Unread count of the process-global ring (tray badge helper).
#[must_use]
pub fn unread_count() -> usize {
    ring()
        .lock()
        .expect("notification ring mutex")
        .unread_count()
}

/// Query params for `GET /api/evy/notifications`.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// ISO-8601 lower bound — only strictly-newer entries are returned.
    since: Option<String>,
    /// Cap on returned entries (clamped to `[1, 200]`, default 50).
    limit: Option<usize>,
}

/// `GET /api/evy/notifications?since=<iso>&limit=N` → `{ ok, notifications:[…] }`.
pub(crate) async fn list_handler(Query(q): Query<ListQuery>) -> Json<Value> {
    let notifications = ring()
        .lock()
        .expect("notification ring mutex")
        .list(q.since.as_deref(), q.limit);
    Json(json!({ "ok": true, "notifications": notifications }))
}

/// `POST /api/evy/notifications/{id}/read` → `{ ok, found }`.
pub(crate) async fn mark_read_handler(AxPath(id): AxPath<String>) -> Json<Value> {
    let found = ring()
        .lock()
        .expect("notification ring mutex")
        .mark_read(&id);
    Json(json!({ "ok": true, "found": found }))
}

/// `POST /api/evy/notifications/read-all` → `{ ok, marked }`.
pub(crate) async fn read_all_handler() -> Json<Value> {
    let marked = ring()
        .lock()
        .expect("notification ring mutex")
        .mark_all_read();
    Json(json!({ "ok": true, "marked": marked }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: &str, title: &str) -> EmitNotificationInput {
        EmitNotificationInput {
            kind: kind.into(),
            severity: NotificationSeverity::Info,
            title: title.into(),
            body: "body".into(),
            team_id: None,
            metadata: None,
        }
    }

    fn upstream_input(from: &str, to: &str) -> EmitNotificationInput {
        let mut md = Map::new();
        md.insert("package".into(), json!("pi-ai"));
        md.insert("from".into(), json!(from));
        md.insert("to".into(), json!(to));
        EmitNotificationInput {
            kind: "upstream-available".into(),
            severity: NotificationSeverity::Info,
            title: "pi-ai available".into(),
            body: "b".into(),
            team_id: None,
            metadata: Some(md),
        }
    }

    #[test]
    fn emit_then_list_is_newest_first() {
        let mut r = NotificationRing::new();
        r.emit(input("a", "first"));
        r.emit(input("b", "second"));
        let list = r.list(None, None);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].title, "second");
        assert_eq!(list[1].title, "first");
        // Severity serializes lowercase; read_at present-and-null.
        let v = serde_json::to_value(&list[0]).unwrap();
        assert_eq!(v["severity"], json!("info"));
        assert_eq!(v["read_at"], Value::Null);
        // team_id + metadata omitted when absent (v3 JSON.stringify parity).
        assert!(v.get("team_id").is_none());
        assert!(v.get("metadata").is_none());
    }

    #[test]
    fn list_limit_clamps_and_defaults() {
        let mut r = NotificationRing::new();
        for i in 0..10 {
            r.emit(input("k", &format!("n{i}")));
        }
        assert_eq!(r.list(None, Some(3)).len(), 3);
        assert_eq!(r.list(None, Some(0)).len(), 1); // clamp min 1
        assert_eq!(r.list(None, Some(9999)).len(), 10); // clamp max RING_LIMIT
        assert_eq!(r.list(None, None).len(), 10); // default 50 ≥ 10
    }

    #[test]
    fn mark_read_and_read_all() {
        let mut r = NotificationRing::new();
        let a = r.emit(input("k", "a"));
        let b = r.emit(input("k", "b"));
        assert_eq!(r.unread_count(), 2);
        assert!(r.mark_read(&a.id));
        assert!(!r.mark_read("no-such-id"));
        assert_eq!(r.unread_count(), 1);
        // read-all marks the remaining unread (b) only.
        assert_eq!(r.mark_all_read(), 1);
        assert_eq!(r.unread_count(), 0);
        let _ = b;
    }

    #[test]
    fn dedup_touches_unread_matching_fingerprint() {
        let mut r = NotificationRing::new();
        let first = r.emit(upstream_input("0.74.0", "0.79.1"));
        let again = r.emit(upstream_input("0.74.0", "0.79.1"));
        // Same record refreshed, not a new one.
        assert_eq!(first.id, again.id);
        assert_eq!(r.list(None, None).len(), 1);
        // A different (from,to) is a distinct fingerprint → new row.
        r.emit(upstream_input("0.79.1", "0.80.0"));
        assert_eq!(r.list(None, None).len(), 2);
    }

    #[test]
    fn dedup_skips_read_entries() {
        let mut r = NotificationRing::new();
        let first = r.emit(upstream_input("0.74.0", "0.79.1"));
        assert!(r.mark_read(&first.id));
        // Now-read entry must not be touched; a fresh row appears.
        r.emit(upstream_input("0.74.0", "0.79.1"));
        assert_eq!(r.list(None, None).len(), 2);
    }

    #[test]
    fn since_filters_strictly_newer() {
        let mut r = NotificationRing::new();
        let a = r.emit(input("k", "old"));
        // Anything at-or-before `a.ts` is excluded.
        let filtered = r.list(Some(&a.ts), None);
        assert!(filtered.iter().all(|n| n.id != a.id));
        // Unparseable `since` → no filtering (v3 Date.parse NaN).
        assert_eq!(r.list(Some("not-a-date"), None).len(), 1);
    }

    #[test]
    fn ring_caps_at_limit() {
        let mut r = NotificationRing::new();
        for i in 0..(RING_LIMIT + 25) {
            r.emit(input("k", &format!("n{i}")));
        }
        assert_eq!(r.list(None, Some(RING_LIMIT)).len(), RING_LIMIT);
        // Newest survives, oldest dropped.
        assert_eq!(
            r.list(None, Some(1))[0].title,
            format!("n{}", RING_LIMIT + 24)
        );
    }

    #[test]
    fn title_truncates_past_80_chars() {
        let mut r = NotificationRing::new();
        let long = "x".repeat(100);
        let n = r.emit(input("k", &long));
        assert_eq!(n.title.chars().count(), 78); // 77 + ellipsis
        assert!(n.title.ends_with('…'));
    }
}
