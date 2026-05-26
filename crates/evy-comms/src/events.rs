//! `DaemonEvent` — the SSE payload taxonomy.
//!
//! Every event the operator console can observe over `/api/evy/events`
//! is a variant of [`DaemonEvent`]. Serialized as a tagged union
//! (`{"type": "...", ...}`) for easy discrimination on the browser side.
//!
//! The set is intentionally narrow; adding a new variant is additive and
//! non-breaking. Consumers that don't know a variant should ignore it
//! rather than fail.

use chrono::{DateTime, Utc};
use evy_core::{MandateId, ProviderKind, WorkerId, WorkerStatus};
use evy_scheduler::{JobId, RunOutcome};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One observable event emitted by the Evy v4 daemon.
///
/// Variants are tagged with `"type"` as a `snake_case` string when
/// serialized to JSON, e.g. `{"type":"daemon_booted","version":"0.1.0",...}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonEvent {
    /// Emitted once when the daemon finishes booting. Carries the build
    /// version and the set of providers that registered.
    DaemonBooted {
        /// Workspace version of the binary (typically `env!("CARGO_PKG_VERSION")`).
        version: String,
        /// Which providers are wired into this daemon instance.
        providers: Vec<ProviderKind>,
    },

    /// A new worker handle was registered with the daemon's worker pool.
    WorkerRegistered {
        /// Stable id of the new worker.
        worker_id: WorkerId,
        /// Which provider produced it.
        provider: ProviderKind,
        /// The mandate the worker is fulfilling.
        mandate_id: MandateId,
    },

    /// An existing worker's lifecycle state changed.
    WorkerStatusChanged {
        /// Stable id of the worker.
        worker_id: WorkerId,
        /// New status reported by the provider.
        status: WorkerStatus,
    },

    /// The scheduler fired a registered job.
    SchedulerFired {
        /// Which job fired.
        job_id: JobId,
        /// Distinct run id (one row in the `runs` table).
        run_id: Uuid,
        /// What the fire produced.
        outcome: RunOutcome,
    },

    /// The policy gate evaluated a command. Useful for the audit-tail
    /// panel; not every check rises to a full audit-log entry.
    PolicyChecked {
        /// The command line that was checked (already-rendered string).
        command: String,
        /// Outcome kind (`allow` / `deny` / `gated` / etc.) as a short tag.
        outcome_kind: String,
    },

    /// Periodic liveness pulse. Multiplexed alongside real events so
    /// the SSE client can distinguish "quiet daemon" from "dead socket".
    Heartbeat {
        /// When the heartbeat was emitted (UTC).
        ts: DateTime<Utc>,
        /// How many providers passed their last healthcheck.
        providers_healthy: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_booted_roundtrips_through_json() {
        let ev = DaemonEvent::DaemonBooted {
            version: "0.1.0".to_owned(),
            providers: vec![ProviderKind::ClaudeCode, ProviderKind::DeepSeek],
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"type\":\"daemon_booted\""));
        let back: DaemonEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn worker_registered_roundtrips() {
        let ev = DaemonEvent::WorkerRegistered {
            worker_id: WorkerId::new(),
            provider: ProviderKind::Codex,
            mandate_id: MandateId::new(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: DaemonEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn worker_status_changed_roundtrips_failed_reason() {
        let ev = DaemonEvent::WorkerStatusChanged {
            worker_id: WorkerId::new(),
            status: WorkerStatus::Failed("oom".to_owned()),
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: DaemonEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn scheduler_fired_roundtrips() {
        let ev = DaemonEvent::SchedulerFired {
            job_id: JobId::new(),
            run_id: Uuid::new_v4(),
            outcome: RunOutcome::Succeeded,
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: DaemonEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn policy_checked_uses_snake_case_tag() {
        let ev = DaemonEvent::PolicyChecked {
            command: "ls -la".to_owned(),
            outcome_kind: "allow".to_owned(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"type\":\"policy_checked\""));
        let back: DaemonEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn heartbeat_uses_snake_case_tag() {
        let ev = DaemonEvent::Heartbeat {
            ts: Utc::now(),
            providers_healthy: 2,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"type\":\"heartbeat\""));
        let back: DaemonEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
    }
}
