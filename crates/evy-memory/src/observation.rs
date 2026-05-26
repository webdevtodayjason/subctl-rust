//! Observation types — pure data, no I/O.
//!
//! An [`Observation`] is the append-only unit written to the log. Every
//! dispatch, every worker outcome, every operator interaction, every
//! scheduler tick becomes one of these. Layers 3–7 of the learning loop
//! read these rows; the log itself is event-sourced substrate.
//!
//! ADR 0020 §"Layer 1 — Observation log".

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use evy_core::{MandateId, ProviderKind, WorkerId, WorkerStatus};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Tagged enum of the things Evy observes. The `tag = "kind"` form on
/// `serde` keeps round-tripping legible inside the JSON payload column.
///
/// Variants are intentionally additive — adding a new one is a
/// non-breaking change for readers that match on `kind_prefix` strings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservationKind {
    /// A worker was dispatched against a mandate.
    WorkerDispatched {
        /// The worker that was started.
        worker_id: WorkerId,
        /// The mandate the worker is fulfilling.
        mandate_id: MandateId,
        /// Which provider the worker is running on.
        provider: ProviderKind,
    },
    /// A worker reached a terminal state.
    WorkerCompleted {
        /// The worker that finished.
        worker_id: WorkerId,
        /// Final lifecycle state.
        status: WorkerStatus,
    },
    /// The policy gate evaluated a command.
    PolicyChecked {
        /// The command (or other gated artifact) that was checked.
        command: String,
        /// Free-form outcome description, e.g. `"allowed"`, `"blocked: sealed"`.
        outcome: String,
    },
    /// The scheduler fired a job.
    SchedulerFiredJob {
        /// The named job (matches the scheduler's registry key).
        job_name: String,
        /// Free-form outcome description.
        outcome: String,
    },
    /// An operator message arrived from a comms channel.
    OperatorMessage {
        /// The channel the message came in on, e.g. `"telegram"`, `"http"`.
        channel: String,
        /// The verbatim text.
        text: String,
    },
    /// The daemon finished booting.
    DaemonBooted {
        /// Version string (typically `env!("CARGO_PKG_VERSION")`).
        version: String,
    },
    /// The daemon is shutting down.
    DaemonShutdown {
        /// Reason for shutdown, e.g. `"sigterm"`, `"operator request"`.
        reason: String,
    },
}

impl ObservationKind {
    /// The top-level discriminator string written to the indexed `kind`
    /// column. Matches the `#[serde(rename_all = "snake_case")]` tag.
    #[must_use]
    pub fn discriminator(&self) -> &'static str {
        match self {
            Self::WorkerDispatched { .. } => "worker_dispatched",
            Self::WorkerCompleted { .. } => "worker_completed",
            Self::PolicyChecked { .. } => "policy_checked",
            Self::SchedulerFiredJob { .. } => "scheduler_fired_job",
            Self::OperatorMessage { .. } => "operator_message",
            Self::DaemonBooted { .. } => "daemon_booted",
            Self::DaemonShutdown { .. } => "daemon_shutdown",
        }
    }
}

/// One row in the observation log.
///
/// `id` and `ts` are minted by [`Observation::new`] (or set explicitly
/// when replaying / synthesising historical events). `metadata` is free-
/// form string-keyed data that lets callers attach hints without
/// expanding the `ObservationKind` enum.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Observation {
    /// Unique row id; minted v4 UUID by default.
    pub id: Uuid,
    /// Wall-clock timestamp at which the observation was recorded.
    pub ts: DateTime<Utc>,
    /// What happened.
    pub kind: ObservationKind,
    /// Optional id that links related observations (e.g. dispatch →
    /// completion → policy-check on the same logical operation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<Uuid>,
    /// Free-form string-keyed metadata.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

impl Observation {
    /// Build a fresh observation with a minted id and `Utc::now()` ts.
    #[must_use]
    pub fn new(kind: ObservationKind) -> Self {
        Self {
            id: Uuid::new_v4(),
            ts: Utc::now(),
            kind,
            correlation_id: None,
            metadata: HashMap::new(),
        }
    }

    /// Builder: attach a correlation id linking this observation to a
    /// group of related events.
    #[must_use]
    pub fn with_correlation(mut self, correlation_id: Uuid) -> Self {
        self.correlation_id = Some(correlation_id);
        self
    }

    /// Builder: attach a single metadata pair.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminator_matches_serde_tag() {
        let cases = [
            (
                ObservationKind::DaemonBooted {
                    version: "0.1.0".into(),
                },
                "daemon_booted",
            ),
            (
                ObservationKind::DaemonShutdown {
                    reason: "sigterm".into(),
                },
                "daemon_shutdown",
            ),
            (
                ObservationKind::OperatorMessage {
                    channel: "telegram".into(),
                    text: "ping".into(),
                },
                "operator_message",
            ),
        ];
        for (kind, expected) in cases {
            // Discriminator helper agrees with the serialised JSON tag.
            assert_eq!(kind.discriminator(), expected);
            let json = serde_json::to_value(&kind).expect("serialize");
            assert_eq!(json["kind"].as_str(), Some(expected));
        }
    }

    #[test]
    fn observation_roundtrips_via_json() {
        let obs = Observation::new(ObservationKind::DaemonBooted {
            version: "0.1.0".to_string(),
        })
        .with_correlation(Uuid::new_v4())
        .with_metadata("host", "m3-studio");
        let json = serde_json::to_string(&obs).expect("serialize");
        let back: Observation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, obs);
    }

    #[test]
    fn optional_fields_omit_when_empty() {
        let obs = Observation::new(ObservationKind::DaemonShutdown {
            reason: "sigterm".into(),
        });
        let json = serde_json::to_string(&obs).expect("serialize");
        assert!(
            !json.contains("\"correlation_id\""),
            "correlation_id should skip when None"
        );
        assert!(
            !json.contains("\"metadata\""),
            "metadata should skip when empty"
        );
    }

    #[test]
    fn fresh_observations_have_unique_ids() {
        let a = Observation::new(ObservationKind::DaemonBooted {
            version: "v".into(),
        });
        let b = Observation::new(ObservationKind::DaemonBooted {
            version: "v".into(),
        });
        assert_ne!(a.id, b.id);
    }
}
