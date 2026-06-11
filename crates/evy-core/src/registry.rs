//! Cutover Phase 2 slice 2a — the in-memory worker registry.
//!
//! Tracks dispatched workers so the daemon can answer `GET /api/evy/workers`
//! (today an empty stub at `evy/src/state.rs`) and feed the Orch panel. The
//! dispatch path [`register`](WorkerRegistry::register)s a worker on spawn and
//! [`update_status`](WorkerRegistry::update_status)es it as the lifecycle moves;
//! the HTTP layer maps [`WorkerRecord`]s to the wire `WorkerSummary`.
//!
//! Timestamps are caller-supplied unix-millis (`evy-core` carries no clock dep),
//! so callers pass `chrono::Utc::now().timestamp_millis()` from the daemon side.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::{MandateId, ProviderKind, WorkerId, WorkerStatus};

/// A tracked worker. Superset of the wire `WorkerSummary` — the extra fields
/// (`classification`, `last_nudge_at_ms`, `last_reply_at_ms`, `last_event`)
/// carry the activity/nudge state the watchdog layer (later slices) needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRecord {
    /// Stable worker id.
    pub id: WorkerId,
    /// Which provider produced it.
    pub provider: ProviderKind,
    /// The mandate the worker is fulfilling.
    pub mandate_id: MandateId,
    /// Latest lifecycle state.
    pub status: WorkerStatus,
    /// When the worker was registered (unix-millis).
    pub created_at_ms: i64,
    /// Last time any activity was observed for this worker (unix-millis).
    pub last_activity_ms: i64,
    /// Short description of the most recent event, if any.
    pub last_event: Option<String>,
    /// Watchdog classification of the worker's last observed state.
    pub classification: Option<String>,
    /// Last time the orchestrator nudged this worker (unix-millis).
    pub last_nudge_at_ms: Option<i64>,
    /// Last time the worker replied (unix-millis).
    pub last_reply_at_ms: Option<i64>,
    /// The tmux session hosting this worker (set by the spawn path). Enables
    /// liveness checks + kill from the Orch panel.
    pub tmux_session: Option<String>,
}

impl WorkerRecord {
    /// Build a freshly-registered `Running` record (the common dispatch case):
    /// `created_at_ms == last_activity_ms == now_ms`, optional fields cleared.
    #[must_use]
    pub fn running(
        id: WorkerId,
        provider: ProviderKind,
        mandate_id: MandateId,
        now_ms: i64,
    ) -> Self {
        Self {
            id,
            provider,
            mandate_id,
            status: WorkerStatus::Running,
            created_at_ms: now_ms,
            last_activity_ms: now_ms,
            last_event: None,
            classification: None,
            last_nudge_at_ms: None,
            last_reply_at_ms: None,
            tmux_session: None,
        }
    }
}

/// Process-wide, cheaply-cloneable registry of dispatched workers (shares one
/// `Arc<RwLock<…>>`). Held by the daemon's `DaemonAppState`; the dispatch path
/// writes, the HTTP layer reads.
#[derive(Debug, Clone, Default)]
pub struct WorkerRegistry {
    inner: Arc<RwLock<HashMap<WorkerId, WorkerRecord>>>,
}

impl WorkerRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a worker record.
    pub fn register(&self, record: WorkerRecord) {
        self.write().insert(record.id, record);
    }

    /// All records, oldest-registered first.
    #[must_use]
    pub fn list(&self) -> Vec<WorkerRecord> {
        let mut v: Vec<WorkerRecord> = self.read().values().cloned().collect();
        v.sort_by_key(|r| r.created_at_ms);
        v
    }

    /// One record by id.
    #[must_use]
    pub fn get(&self, id: &WorkerId) -> Option<WorkerRecord> {
        self.read().get(id).cloned()
    }

    /// Update a worker's lifecycle status. Returns `false` if unknown.
    pub fn update_status(&self, id: &WorkerId, status: WorkerStatus, now_ms: i64) -> bool {
        let mut g = self.write();
        match g.get_mut(id) {
            Some(r) => {
                r.status = status;
                r.last_activity_ms = now_ms;
                true
            }
            None => false,
        }
    }

    /// Record activity (and optionally the latest event) for a worker. Returns
    /// `false` if unknown.
    pub fn touch(&self, id: &WorkerId, now_ms: i64, last_event: Option<String>) -> bool {
        let mut g = self.write();
        match g.get_mut(id) {
            Some(r) => {
                r.last_activity_ms = now_ms;
                if last_event.is_some() {
                    r.last_event = last_event;
                }
                true
            }
            None => false,
        }
    }

    /// Record the tmux session hosting a worker (set by the spawn path after the
    /// session is created). Returns `false` if the worker is unknown.
    pub fn set_tmux_session(&self, id: &WorkerId, session: String) -> bool {
        let mut g = self.write();
        match g.get_mut(id) {
            Some(r) => {
                r.tmux_session = Some(session);
                true
            }
            None => false,
        }
    }

    /// Remove a worker (e.g., on kill/cleanup). Returns the removed record.
    pub fn remove(&self, id: &WorkerId) -> Option<WorkerRecord> {
        self.write().remove(id)
    }

    /// W6 row ⑨ ruling — reap finished workers after a grace window.
    ///
    /// Removes (and returns) every record whose status
    /// [`is_terminal`](WorkerStatus::is_terminal) AND whose
    /// `last_activity_ms` is at least `max_age_ms` old. The tmux session
    /// is deliberately NOT touched: a lingering session stays visible in
    /// the session browser (and to the operator's own tmux) — only the
    /// worker-registry row retires, so finished workers stop counting
    /// toward the staleness watchdog's `teams_tracked` and can never trip
    /// `watchdog_fire` after completion. Running/Pending workers are
    /// never reaped, whatever their age.
    pub fn reap_terminal(&self, now_ms: i64, max_age_ms: i64) -> Vec<WorkerRecord> {
        let mut g = self.write();
        let dead: Vec<WorkerId> = g
            .values()
            .filter(|r| {
                r.status.is_terminal() && now_ms.saturating_sub(r.last_activity_ms) >= max_age_ms
            })
            .map(|r| r.id)
            .collect();
        dead.into_iter().filter_map(|id| g.remove(&id)).collect()
    }

    /// Number of tracked workers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<WorkerId, WorkerRecord>> {
        // Poison only on a panic-while-holding; recover the guard rather than
        // cascading the panic into every reader.
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<WorkerId, WorkerRecord>> {
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(now: i64) -> WorkerRecord {
        WorkerRecord::running(
            WorkerId::new(),
            ProviderKind::ClaudeCode,
            MandateId::new(),
            now,
        )
    }

    #[test]
    fn register_then_list_and_get() {
        let reg = WorkerRegistry::new();
        assert!(reg.is_empty());
        let r = rec(1000);
        let id = r.id;
        reg.register(r);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.list().len(), 1);
        assert_eq!(reg.get(&id).unwrap().status, WorkerStatus::Running);
    }

    #[test]
    fn update_status_reflects_and_reports_unknown() {
        let reg = WorkerRegistry::new();
        let r = rec(1000);
        let id = r.id;
        reg.register(r);
        assert!(reg.update_status(&id, WorkerStatus::Succeeded, 2000));
        let got = reg.get(&id).unwrap();
        assert_eq!(got.status, WorkerStatus::Succeeded);
        assert_eq!(got.last_activity_ms, 2000);
        assert!(!reg.update_status(&WorkerId::new(), WorkerStatus::Cancelled, 3000));
    }

    #[test]
    fn touch_updates_activity_and_event() {
        let reg = WorkerRegistry::new();
        let r = rec(1000);
        let id = r.id;
        reg.register(r);
        assert!(reg.touch(&id, 5000, Some("paste".into())));
        let got = reg.get(&id).unwrap();
        assert_eq!(got.last_activity_ms, 5000);
        assert_eq!(got.last_event.as_deref(), Some("paste"));
    }

    #[test]
    fn list_is_oldest_first() {
        let reg = WorkerRegistry::new();
        reg.register(rec(3000));
        reg.register(rec(1000));
        reg.register(rec(2000));
        let times: Vec<i64> = reg.list().iter().map(|r| r.created_at_ms).collect();
        assert_eq!(times, vec![1000, 2000, 3000]);
    }

    #[test]
    fn remove_returns_record() {
        let reg = WorkerRegistry::new();
        let r = rec(1000);
        let id = r.id;
        reg.register(r);
        assert_eq!(reg.remove(&id).unwrap().id, id);
        assert!(reg.is_empty());
        assert!(reg.remove(&id).is_none());
    }

    #[test]
    fn set_tmux_session_records_and_reports_unknown() {
        let reg = WorkerRegistry::new();
        let r = rec(1000);
        let id = r.id;
        reg.register(r);
        assert!(reg.set_tmux_session(&id, "claude-tmp".into()));
        assert_eq!(
            reg.get(&id).unwrap().tmux_session.as_deref(),
            Some("claude-tmp")
        );
        assert!(!reg.set_tmux_session(&WorkerId::new(), "x".into()));
    }

    #[test]
    fn reap_terminal_retires_only_old_finished_workers() {
        let reg = WorkerRegistry::new();
        let done_old = rec(1_000);
        let done_fresh = rec(1_000);
        let running_old = rec(1_000);
        let (done_old_id, done_fresh_id, running_old_id) =
            (done_old.id, done_fresh.id, running_old.id);
        reg.register(done_old);
        reg.register(done_fresh);
        reg.register(running_old);
        // done_old finished at t=10_000; done_fresh finished at t=95_000;
        // running_old has been Running and silent since t=1_000.
        reg.update_status(&done_old_id, WorkerStatus::Succeeded, 10_000);
        reg.update_status(&done_fresh_id, WorkerStatus::Failed("boom".into()), 95_000);

        // Reap at t=100_000 with a 60s grace window.
        let reaped = reg.reap_terminal(100_000, 60_000);
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].id, done_old_id);
        // Fresh-terminal stays (inside grace); running stays (never reaped).
        assert!(reg.get(&done_fresh_id).is_some());
        assert!(reg.get(&running_old_id).is_some());
        assert_eq!(reg.len(), 2);

        // Idempotent: a second sweep at the same instant finds nothing.
        assert!(reg.reap_terminal(100_000, 60_000).is_empty());
    }

    #[test]
    fn registry_clone_shares_state() {
        let reg = WorkerRegistry::new();
        let clone = reg.clone();
        reg.register(rec(1000));
        assert_eq!(clone.len(), 1); // shared Arc — clone sees the insert
    }
}
