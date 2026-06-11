//! `WorkerHandle` trait + `WorkerId` newtype + `WorkerStatus` enum.
//!
//! A handle is the caller-facing wrapper around a provider's
//! provider-specific worker (tmux pane, child process, queued job).
//! Implementations live in `evy-providers`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{MandateId, Result};

/// Opaque identifier for a dispatched worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkerId(pub Uuid);

impl WorkerId {
    /// Mint a fresh v4 UUID-backed worker id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for WorkerId {
    fn default() -> Self {
        Self::new()
    }
}

/// Lifecycle state of a dispatched worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerStatus {
    /// Accepted by the provider, not yet running.
    Pending,
    /// Actively executing.
    Running,
    /// Completed successfully.
    Succeeded,
    /// Completed with failure; the string carries the provider's reason.
    Failed(String),
    /// Cancelled by the orchestrator (e.g., timeout, operator action).
    Cancelled,
}

impl WorkerStatus {
    /// True for the states a worker never leaves — `Succeeded`,
    /// `Failed`, `Cancelled`. Terminal records represent finished work:
    /// the registry's reap sweep (W6 row ⑨) retires them after a grace
    /// window instead of letting them feed the team watchdogs forever.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed(_) | Self::Cancelled)
    }
}

/// Caller-facing handle to a dispatched worker.
///
/// Methods are async because most providers need an out-of-process round
/// trip (HTTP, tmux query, queue lookup) to answer.
#[async_trait]
pub trait WorkerHandle: Send + Sync {
    /// Stable id assigned at dispatch time.
    fn id(&self) -> WorkerId;

    /// The mandate this worker is fulfilling.
    fn mandate_id(&self) -> MandateId;

    /// Latest lifecycle state from the provider.
    ///
    /// # Errors
    /// Returns [`crate::Error::Provider`] if the provider's status
    /// transport fails, or [`crate::Error::WorkerNotFound`] if the
    /// provider no longer knows about this handle.
    async fn status(&self) -> Result<WorkerStatus>;

    /// Request cancellation. Idempotent — a cancelled or finished worker
    /// returns `Ok(())`.
    ///
    /// # Errors
    /// Returns [`crate::Error::Provider`] when the cancellation transport
    /// fails.
    async fn cancel(&self) -> Result<()>;

    /// Block until the worker reaches a terminal state.
    ///
    /// # Errors
    /// Returns [`crate::Error::Provider`] if the wait transport fails or
    /// [`crate::Error::WorkerFailed`] if the worker terminates abnormally
    /// in a way the provider cannot surface as a `WorkerStatus`.
    async fn wait(&self) -> Result<WorkerStatus>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_ids_are_unique() {
        let a = WorkerId::new();
        let b = WorkerId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn default_is_new() {
        let a: WorkerId = WorkerId::default();
        let b: WorkerId = WorkerId::default();
        assert_ne!(a, b, "Default should mint, not return a constant");
    }

    #[test]
    fn status_serde_roundtrip() {
        let cases = [
            WorkerStatus::Pending,
            WorkerStatus::Running,
            WorkerStatus::Succeeded,
            WorkerStatus::Failed("boom".to_owned()),
            WorkerStatus::Cancelled,
        ];
        for s in cases {
            let json = serde_json::to_string(&s).expect("serialize");
            let back: WorkerStatus = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(s, back);
        }
    }

    #[test]
    fn status_equality_includes_failure_reason() {
        assert_ne!(
            WorkerStatus::Failed("one".into()),
            WorkerStatus::Failed("two".into())
        );
        assert_eq!(
            WorkerStatus::Failed("same".into()),
            WorkerStatus::Failed("same".into())
        );
    }
}
