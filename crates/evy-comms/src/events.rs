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
use serde_json::json;
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

    /// One watchdog tick completed.
    ///
    /// Emitted by `evy_watchdog::WatchdogRegistry` after each
    /// scheduled tick. Carries only summary information — full
    /// `TickReport`s are written to the observation log and queried
    /// from the dashboard on demand, keeping the SSE wire small.
    WatchdogTick {
        /// Watchdog identifier (`evy_watchdog::Watchdog::name()`).
        name: String,
        /// Number of findings produced by this tick. `Healthy`
        /// counts as one finding.
        finding_count: usize,
        /// True iff the watchdog itself ran cleanly (no timeout, no
        /// internal error). Independent of `finding_count`.
        healthy: bool,
    },

    /// A pre-formatted NAMED SSE frame for the dashboard chat tab, absorbed
    /// from the v3 BFF (`dashboard/lib/v4-bridge.ts`).
    ///
    /// Unlike the monitoring variants above — delivered on the default SSE
    /// event as `data:{ "type": ..., ... }` — a `DashboardFrame` is delivered
    /// as `event: <event>\ndata: <data>`, the exact vocabulary
    /// `dashboard/public/tabs/chat.js` listens for: streaming chat tokens
    /// (`message_update` → `{assistantMessageEvent:{type:"text_delta",delta}}`),
    /// the turn terminator (`message_end`), and the transcript-mutation pings
    /// (`transcript_compacted` / `transcript_cleared`). [`crate::sse`]'s
    /// `into_sse_response` special-cases this variant; construct it with the
    /// `dashboard_*` helpers so the wire shape stays centralised.
    DashboardFrame {
        /// SSE event name (e.g. `"message_update"`).
        event: String,
        /// Pre-encoded JSON payload string (e.g. `"{}"`).
        data: String,
    },
}

impl DaemonEvent {
    /// `event: message_start` — the BFF emits this before the first token.
    /// The chat tab lazy-creates the reply bubble on the first `text_delta`,
    /// so this carries no payload; it's emitted for BFF parity.
    #[must_use]
    pub(crate) fn dashboard_message_start() -> Self {
        Self::DashboardFrame {
            event: "message_start".to_string(),
            data: "{}".to_string(),
        }
    }

    /// `event: message_update` carrying one streamed token as a `text_delta`,
    /// in the shape `chat.js` parses (`d.assistantMessageEvent.delta`). `delta`
    /// is JSON-escaped via `serde_json`, so quotes/newlines are safe.
    #[must_use]
    pub(crate) fn dashboard_message_update(delta: &str) -> Self {
        let data = json!({
            "assistantMessageEvent": { "type": "text_delta", "delta": delta }
        })
        .to_string();
        Self::DashboardFrame {
            event: "message_update".to_string(),
            data,
        }
    }

    /// `event: message_end` — the turn terminator (finalises the bubble).
    #[must_use]
    pub(crate) fn dashboard_message_end() -> Self {
        Self::DashboardFrame {
            event: "message_end".to_string(),
            data: "{}".to_string(),
        }
    }

    /// `event: transcript_compacted` — the chat tab refreshes its transcript
    /// view on this ping (chat.js).
    #[must_use]
    pub(crate) fn dashboard_transcript_compacted() -> Self {
        Self::DashboardFrame {
            event: "transcript_compacted".to_string(),
            data: "{}".to_string(),
        }
    }

    /// `event: transcript_cleared` — emitted on "New Chat" (clear). Mirrors the
    /// BFF; the chat tab resets its own view client-side.
    #[must_use]
    pub(crate) fn dashboard_transcript_cleared() -> Self {
        Self::DashboardFrame {
            event: "transcript_cleared".to_string(),
            data: "{}".to_string(),
        }
    }
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

    #[test]
    fn watchdog_tick_uses_snake_case_tag() {
        let ev = DaemonEvent::WatchdogTick {
            name: "idle-pane".to_owned(),
            finding_count: 2,
            healthy: true,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"type\":\"watchdog_tick\""));
        let back: DaemonEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn dashboard_frame_constructors_carry_the_chat_js_vocabulary() {
        // Event names match the chat.js addEventListener set.
        match DaemonEvent::dashboard_message_start() {
            DaemonEvent::DashboardFrame { event, data } => {
                assert_eq!(event, "message_start");
                assert_eq!(data, "{}");
            }
            other => panic!("expected DashboardFrame, got {other:?}"),
        }
        match DaemonEvent::dashboard_message_end() {
            DaemonEvent::DashboardFrame { event, .. } => assert_eq!(event, "message_end"),
            other => panic!("expected DashboardFrame, got {other:?}"),
        }
        match DaemonEvent::dashboard_transcript_compacted() {
            DaemonEvent::DashboardFrame { event, .. } => assert_eq!(event, "transcript_compacted"),
            other => panic!("expected DashboardFrame, got {other:?}"),
        }
        match DaemonEvent::dashboard_transcript_cleared() {
            DaemonEvent::DashboardFrame { event, .. } => assert_eq!(event, "transcript_cleared"),
            other => panic!("expected DashboardFrame, got {other:?}"),
        }
    }

    #[test]
    fn dashboard_message_update_matches_bff_text_delta_shape_and_escapes() {
        // The shape chat.js parses: d.assistantMessageEvent.{type,delta}.
        let DaemonEvent::DashboardFrame { event, data } =
            DaemonEvent::dashboard_message_update("hi \"there\"\nnext")
        else {
            panic!("expected DashboardFrame");
        };
        assert_eq!(event, "message_update");
        let v: serde_json::Value = serde_json::from_str(&data).expect("data is valid JSON");
        assert_eq!(v["assistantMessageEvent"]["type"], "text_delta");
        assert_eq!(v["assistantMessageEvent"]["delta"], "hi \"there\"\nnext");
    }
}
