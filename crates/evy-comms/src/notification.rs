//! Outbound notification taxonomy.
//!
//! `Notification` is the closed set of events the daemon can hand the
//! [`crate::TelegramBridge`] (and, in later slices, other channels) for
//! delivery to the operator. Variants mirror the v3 master-bot surface
//! kinds collapsed to the strategic-conversation subset — tactical
//! escalations (watchdog firings, etc.) belong to a different channel.

use serde::{Deserialize, Serialize};

use evy_core::{ProviderKind, WorkerId, WorkerStatus};

use crate::AskId;

/// Closed taxonomy of operator-visible events the daemon emits.
///
/// `kind` is the discriminant in JSON (`#[serde(tag = "kind")]`) which
/// keeps the wire shape stable for any external consumer (audit log,
/// dashboard SSE, future Discord renderer).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notification {
    /// A worker has been accepted by its provider and dispatched.
    WorkerStarted {
        /// Stable id assigned at dispatch.
        worker_id: WorkerId,
        /// Which provider accepted the work.
        provider: ProviderKind,
        /// One-line operator-visible goal from the mandate.
        goal: String,
    },
    /// A worker reached a terminal state.
    WorkerFinished {
        /// The id that was dispatched.
        worker_id: WorkerId,
        /// Terminal lifecycle state.
        outcome: WorkerStatus,
    },
    /// The scheduler fired a configured job and resolved it.
    SchedulerFiredJob {
        /// Job name from the scheduler config.
        name: String,
        /// One-line resolution summary.
        outcome_summary: String,
    },
    /// Evy posted a question and is awaiting an operator reply.
    AskPending {
        /// The ask id the operator will reply against.
        ask_id: AskId,
        /// Operator-visible prompt.
        question: String,
    },
    /// Evy received an operator answer to a previously-pending ask.
    AskResolved {
        /// Which ask was resolved.
        ask_id: AskId,
        /// The operator's reply text.
        answer: String,
    },
    /// An error the operator should see (transport, policy, etc.).
    Error {
        /// Short module / subsystem context (`"scheduler"`, `"providers"`).
        context: String,
        /// Human-readable error message.
        message: String,
    },
}

impl Notification {
    /// Render the notification as a single plain-text line suitable for
    /// Telegram's `sendMessage` body. Markdown is intentionally avoided
    /// to keep escaping simple; callers wanting MarkdownV2 should
    /// render themselves and shape the message before handing it off.
    #[must_use]
    pub fn render_text(&self) -> String {
        match self {
            Self::WorkerStarted {
                worker_id,
                provider,
                goal,
            } => format!(
                "▶️  worker started [{provider:?}] {goal} (id={uuid})",
                uuid = worker_id.0
            ),
            Self::WorkerFinished { worker_id, outcome } => format!(
                "🏁 worker finished {outcome:?} (id={uuid})",
                uuid = worker_id.0
            ),
            Self::SchedulerFiredJob {
                name,
                outcome_summary,
            } => format!("⏰ scheduler `{name}`: {outcome_summary}"),
            Self::AskPending { ask_id, question } => {
                format!("❓ ask {id}: {question}", id = ask_id.0)
            }
            Self::AskResolved { ask_id, answer } => {
                format!("✅ ask {id} resolved: {answer}", id = ask_id.0)
            }
            Self::Error { context, message } => format!("⚠️  {context}: {message}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn worker_id() -> WorkerId {
        WorkerId(Uuid::nil())
    }

    fn ask_id() -> AskId {
        AskId(Uuid::nil())
    }

    #[test]
    fn serde_roundtrip_each_variant() {
        let cases = [
            Notification::WorkerStarted {
                worker_id: worker_id(),
                provider: ProviderKind::ClaudeCode,
                goal: "ship slice 2B2".into(),
            },
            Notification::WorkerFinished {
                worker_id: worker_id(),
                outcome: WorkerStatus::Succeeded,
            },
            Notification::SchedulerFiredJob {
                name: "nightly".into(),
                outcome_summary: "ok".into(),
            },
            Notification::AskPending {
                ask_id: ask_id(),
                question: "continue?".into(),
            },
            Notification::AskResolved {
                ask_id: ask_id(),
                answer: "yes".into(),
            },
            Notification::Error {
                context: "providers".into(),
                message: "401".into(),
            },
        ];
        for n in cases {
            let json = serde_json::to_string(&n).expect("serialize");
            let back: Notification = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(n, back);
            // Tag discriminant present in JSON
            assert!(json.contains("\"kind\""));
        }
    }

    #[test]
    fn render_text_is_nonempty_for_every_variant() {
        let cases = [
            Notification::WorkerStarted {
                worker_id: worker_id(),
                provider: ProviderKind::Codex,
                goal: "g".into(),
            },
            Notification::WorkerFinished {
                worker_id: worker_id(),
                outcome: WorkerStatus::Failed("oom".into()),
            },
            Notification::SchedulerFiredJob {
                name: "n".into(),
                outcome_summary: "s".into(),
            },
            Notification::AskPending {
                ask_id: ask_id(),
                question: "q".into(),
            },
            Notification::AskResolved {
                ask_id: ask_id(),
                answer: "a".into(),
            },
            Notification::Error {
                context: "c".into(),
                message: "m".into(),
            },
        ];
        for n in cases {
            assert!(!n.render_text().is_empty(), "variant {n:?} rendered empty");
        }
    }
}
