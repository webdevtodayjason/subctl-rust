//! Session + Message types — pure data, no I/O.
//!
//! A [`Session`] is the in-memory record of one planning conversation:
//! a topic, a chronological list of [`Message`]s, a status, and minted
//! timestamps. Sessions are owned by the [`ThinkingPartner`] and live as
//! long as the partner is alive; persistence is a separate concern (see
//! `with_message_hook` on the partner).
//!
//! [`ThinkingPartner`]: crate::ThinkingPartner

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identifier for one planning session.
///
/// Cheap to copy. Round-trips through `serde` so it can be sent over the
/// daemon's IPC surfaces or stored alongside an observation row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// Mint a fresh v4 UUID-backed id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Who produced a [`Message`].
///
/// The three roles map to the Anthropic Messages API as follows:
///
/// | Role          | Anthropic mapping             |
/// |---------------|-------------------------------|
/// | [`Role::Operator`] | `role: "user"`             |
/// | [`Role::Partner`]  | `role: "assistant"`        |
/// | [`Role::System`]   | **skipped** — the system prompt is rendered separately via [`crate::templates::planning_system_prompt`] |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Human input from the operator.
    Operator,
    /// Evy's response.
    Partner,
    /// Scaffolding / status — NOT sent to the LLM; useful for surfaces
    /// (TUI / Discord) that want to render "session started" etc.
    System,
}

/// One message in a planning session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Unique row id; minted v4 UUID by default.
    pub id: Uuid,
    /// Session this message belongs to.
    pub session_id: SessionId,
    /// Wall-clock timestamp at which the message was produced.
    pub ts: DateTime<Utc>,
    /// Who produced it.
    pub role: Role,
    /// Verbatim text. The thinking-partner does not impose markdown
    /// structure — it's just what the LLM (or operator) said.
    pub content: String,
}

impl Message {
    /// Build a fresh message with a minted id and `Utc::now()` ts.
    #[must_use]
    pub fn new(session_id: SessionId, role: Role, content: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            session_id,
            ts: Utc::now(),
            role,
            content: content.into(),
        }
    }
}

/// Lifecycle state of a [`Session`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Operator can still send turns.
    Active,
    /// Operator (or partner, on operator instruction) closed the
    /// session. The final structured summary is the last message.
    Concluded,
    /// Inactivity timeout expired. No new turns will be accepted.
    /// Phase 4: the daemon-side scheduler is responsible for marking
    /// these — this crate exposes the variant so callers can render it.
    TimedOut,
}

/// In-memory record of one planning conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Stable id.
    pub id: SessionId,
    /// Free-form topic the operator framed when starting the session
    /// (e.g. `"brownfield migration for project X"`).
    pub topic: String,
    /// When the session was opened.
    pub started_at: DateTime<Utc>,
    /// Last operator-or-partner activity. Updated on every `send` and
    /// when the partner posts its opening clarifying questions.
    pub last_activity: DateTime<Utc>,
    /// Lifecycle state.
    pub status: SessionStatus,
    /// Chronological message log. The first message is always a
    /// `Role::System` "session opened" marker for surfaces that want
    /// it; the second is the partner's opening clarifying questions.
    pub messages: Vec<Message>,
}

impl Session {
    /// Fresh session with the supplied topic. `messages` is empty —
    /// callers (the [`ThinkingPartner`](crate::ThinkingPartner)) push
    /// the scaffolding marker and opening LLM turn before handing the
    /// session id to the operator.
    #[must_use]
    pub fn new(topic: String) -> Self {
        let now = Utc::now();
        Self {
            id: SessionId::new(),
            topic,
            started_at: now,
            last_activity: now,
            status: SessionStatus::Active,
            messages: Vec::new(),
        }
    }

    /// Append a message, bumping `last_activity` to the message's `ts`.
    pub(crate) fn push(&mut self, msg: Message) {
        self.last_activity = msg.ts;
        self.messages.push(msg);
    }

    /// Most recent message of the given role, if any.
    #[must_use]
    pub fn last_of(&self, role: Role) -> Option<&Message> {
        self.messages.iter().rev().find(|m| m.role == role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_round_trips_through_json() {
        let id = SessionId::new();
        let s = serde_json::to_string(&id).expect("serialize");
        let back: SessionId = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn role_serializes_snake_case() {
        let cases = [
            (Role::Operator, "\"operator\""),
            (Role::Partner, "\"partner\""),
            (Role::System, "\"system\""),
        ];
        for (role, expected) in cases {
            let s = serde_json::to_string(&role).expect("serialize");
            assert_eq!(s, expected);
        }
    }

    #[test]
    fn status_round_trips() {
        for st in [
            SessionStatus::Active,
            SessionStatus::Concluded,
            SessionStatus::TimedOut,
        ] {
            let s = serde_json::to_string(&st).expect("serialize");
            let back: SessionStatus = serde_json::from_str(&s).expect("deserialize");
            assert_eq!(st, back);
        }
    }

    #[test]
    fn message_round_trips() {
        let sid = SessionId::new();
        let m = Message::new(sid, Role::Partner, "what is the migration target?");
        let s = serde_json::to_string(&m).expect("serialize");
        let back: Message = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back.id, m.id);
        assert_eq!(back.session_id, m.session_id);
        assert_eq!(back.role, m.role);
        assert_eq!(back.content, m.content);
    }

    #[test]
    fn session_push_updates_last_activity() {
        let mut s = Session::new("test".to_string());
        let opened = s.last_activity;
        std::thread::sleep(std::time::Duration::from_millis(2));
        let m = Message::new(s.id, Role::Operator, "hi");
        s.push(m);
        assert!(
            s.last_activity > opened,
            "push must bump last_activity past start"
        );
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn last_of_returns_most_recent() {
        let mut s = Session::new("t".to_string());
        s.push(Message::new(s.id, Role::Partner, "first"));
        s.push(Message::new(s.id, Role::Operator, "second"));
        s.push(Message::new(s.id, Role::Partner, "third"));
        assert_eq!(s.last_of(Role::Partner).unwrap().content, "third");
        assert_eq!(s.last_of(Role::Operator).unwrap().content, "second");
        assert!(s.last_of(Role::System).is_none());
    }

    #[test]
    fn session_id_default_is_unique() {
        let a = SessionId::default();
        let b = SessionId::default();
        assert_ne!(a, b);
    }
}
