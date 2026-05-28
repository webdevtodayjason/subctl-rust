//! [`DirectiveDispatcher`] — the mutating tmux surface used by
//! [`crate::AutoNudgeWatchdog`].
//!
//! Phase 4's [`crate::TmuxQuery`] is intentionally read-only:
//! watchdogs MUST NOT do `send-keys` / `kill-window`. AutoNudge is the
//! first watchdog that needs to *write* — it composes a status-check
//! directive and delivers it to the worker's pane. To keep the read-only
//! invariant intact for the other watchdogs we ship a separate trait
//! here, scoped to the directive path.
//!
//! # Why a local trait
//!
//! `evy-providers::Provider` does not (yet) expose a "dispatch this
//! directive to a running worker" method — `dispatch()` *starts* a
//! worker from a `Mandate`, but cannot speak to an existing one. Adding
//! that to the Provider trait is a Phase 6 conversation. Until then,
//! we let the daemon supply a small wrapper that calls into
//! `evy-providers::tmux::send_keys` (or whatever the dispatcher
//! eventually becomes), and ship a no-op + a recording mock for tests.
//!
//! TODO: Phase 6 — once `Provider::dispatch_directive(worker_id, body)`
//! lands, replace this with a thin adapter and delete the trait.

use std::sync::Mutex;

use async_trait::async_trait;
use evy_core::{Result, WorkerId};

/// Write-only surface for delivering a directive to a running worker.
///
/// Implementations are responsible for the on-the-wire path —
/// `tmux send-keys`, HTTP POST, queue enqueue, etc. AutoNudge does not
/// peek at the result text; success is "the dispatcher accepted the
/// directive without erroring."
#[async_trait]
pub trait DirectiveDispatcher: Send + Sync {
    /// Send `body` to the worker identified by `worker_id`. Idempotent
    /// from AutoNudge's perspective — duplicate sends inside the
    /// cooldown window are gated by the watchdog, not the dispatcher.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Provider`] when the underlying
    /// transport rejects the directive. AutoNudge folds a failure into
    /// the unhealthy [`crate::TickReport`] path; it does NOT advance
    /// the nudge counter on a delivery failure (mirrors v3's WEB-216
    /// fix — a worker that never saw the nudge must not be escalated).
    async fn dispatch_directive(&self, worker_id: WorkerId, body: &str) -> Result<()>;
}

/// In-process recording mock for tests.
///
/// Records every `(worker_id, body)` pair plus a configurable failure
/// switch. The default behaviour is "always succeed."
#[derive(Debug, Default)]
pub struct MockDirectiveDispatcher {
    inner: Mutex<MockState>,
}

#[derive(Debug, Default)]
struct MockState {
    sent: Vec<(WorkerId, String)>,
    /// When `Some(reason)`, every `dispatch_directive` call returns
    /// `Err(Error::Provider { reason, .. })`.
    fail_with: Option<String>,
}

impl MockDirectiveDispatcher {
    /// Build a fresh mock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a snapshot of every directive dispatched so far.
    #[must_use]
    pub fn sent(&self) -> Vec<(WorkerId, String)> {
        self.inner
            .lock()
            .expect("mock-directive-dispatcher poisoned")
            .sent
            .clone()
    }

    /// Count of dispatched directives — cheaper than `sent().len()`
    /// when the body content doesn't matter.
    #[must_use]
    pub fn count(&self) -> usize {
        self.inner
            .lock()
            .expect("mock-directive-dispatcher poisoned")
            .sent
            .len()
    }

    /// Make every subsequent dispatch fail with the given reason. Used
    /// in tests that verify the "nudge counter doesn't advance on
    /// delivery failure" invariant.
    pub fn fail_next_dispatches_with(&self, reason: impl Into<String>) {
        self.inner
            .lock()
            .expect("mock-directive-dispatcher poisoned")
            .fail_with = Some(reason.into());
    }

    /// Clear any failure switch set by
    /// [`Self::fail_next_dispatches_with`].
    pub fn clear_failure(&self) {
        self.inner
            .lock()
            .expect("mock-directive-dispatcher poisoned")
            .fail_with = None;
    }
}

#[async_trait]
impl DirectiveDispatcher for MockDirectiveDispatcher {
    async fn dispatch_directive(&self, worker_id: WorkerId, body: &str) -> Result<()> {
        let mut s = self
            .inner
            .lock()
            .expect("mock-directive-dispatcher poisoned");
        if let Some(reason) = &s.fail_with {
            return Err(crate::error::watchdog_io_error(
                "auto-nudge",
                format!("mock dispatcher failure: {reason}"),
            ));
        }
        s.sent.push((worker_id, body.to_owned()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_records_dispatch() {
        let m = MockDirectiveDispatcher::new();
        let id = WorkerId::new();
        m.dispatch_directive(id, "hello").await.unwrap();
        assert_eq!(m.count(), 1);
        let sent = m.sent();
        assert_eq!(sent[0].0, id);
        assert_eq!(sent[0].1, "hello");
    }

    #[tokio::test]
    async fn mock_can_simulate_failure() {
        let m = MockDirectiveDispatcher::new();
        m.fail_next_dispatches_with("simulated 5xx");
        let err = m
            .dispatch_directive(WorkerId::new(), "body")
            .await
            .unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("simulated 5xx"), "got {s}");
        assert_eq!(m.count(), 0, "failed dispatches do not record");
    }

    #[tokio::test]
    async fn mock_clear_failure_resumes_success() {
        let m = MockDirectiveDispatcher::new();
        m.fail_next_dispatches_with("x");
        assert!(m.dispatch_directive(WorkerId::new(), "a").await.is_err());
        m.clear_failure();
        m.dispatch_directive(WorkerId::new(), "b").await.unwrap();
        assert_eq!(m.count(), 1);
    }
}
