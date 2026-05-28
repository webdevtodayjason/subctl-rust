//! Phase 6 — operator-facing chat endpoint.
//!
//! `POST /api/evy/chat` is the HTTP surface for the operator's terminal
//! chat client (`evy-chat-tui`). It accepts one operator turn at a time
//! and delegates to [`evy_thinking::ThinkingPartner`].
//!
//! # Wire shape
//!
//! Request:
//!
//! ```json
//! { "session_id": "<uuid or null>", "message": "operator text" }
//! ```
//!
//! Response (200 OK):
//!
//! ```json
//! {
//!   "session_id": "<uuid>",
//!   "response": "Evy's reply text",
//!   "skills_loaded": ["plan", "debugging"]
//! }
//! ```
//!
//! Failure modes:
//!
//! | HTTP | Variant | When |
//! |------|---------|------|
//! | 400  | `bad_request` | empty message, malformed body |
//! | 404  | `unknown_session` | client-supplied session id is unknown |
//! | 422  | `session_closed` | session was concluded/timed-out |
//! | 502  | `backend` | LLM transport / HTTP-status / decode failure |
//! | 503  | `unavailable` | daemon has no thinking-partner configured |
//! | 500  | `internal` | unexpected error from the partner |
//!
//! # Session lifecycle mapping
//!
//! - `session_id: null` → [`ThinkingPartner::start_session`] with the
//!   operator's message as the topic. The response carries the freshly
//!   minted session id plus the partner's opening clarifying questions.
//! - `session_id: Some(id)` → [`ThinkingPartner::send`] against the
//!   existing session.
//!
//! # `skills_loaded`
//!
//! Phase 6 reports the *registry index* visible to the model — every
//! skill name the LLM could load this turn via the `skill_view` tool.
//! This is a conservative (and stable) signal: it tells the operator
//! which skills *were available*, not which the model actually loaded.
//! Per-turn skill_view usage requires backend instrumentation; see
//! Phase 7 input notes.
//!
//! [`ThinkingPartner::start_session`]: evy_thinking::ThinkingPartner::start_session
//! [`ThinkingPartner::send`]: evy_thinking::ThinkingPartner::send

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use evy_skills::SkillRegistry;
use evy_thinking::{Role, SessionId, SessionStatus, ThinkingError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::http::HttpState;

/// JSON body the operator's chat client POSTs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatRequest {
    /// `None` = open a new session using `message` as the topic.
    /// `Some(id)` = append `message` to the existing session.
    #[serde(default)]
    pub session_id: Option<Uuid>,
    /// Operator text. Must be non-empty (trimmed).
    pub message: String,
}

/// JSON body returned on success.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatResponse {
    /// Session id — newly minted when the request opened a fresh
    /// session, otherwise echoes the request value.
    pub session_id: Uuid,
    /// Evy's reply text. Verbatim from the LLM, no markdown stripping.
    pub response: String,
    /// Skill names the model could see this turn (registry index).
    /// Empty when the daemon was not built with a skill registry, or
    /// when the registry is empty. See module docs for the rationale.
    pub skills_loaded: Vec<String>,
}

/// Typed error variants the handler emits. Mapped to HTTP status codes
/// via `IntoResponse`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatError {
    /// Bad request body (empty message, malformed JSON).
    BadRequest {
        /// Human-readable description of the validation failure.
        message: String,
    },
    /// Client referenced a session the partner doesn't know.
    UnknownSession {
        /// The unknown id, echoed back for the operator's logs.
        session_id: Uuid,
    },
    /// Session was concluded or timed out; no new turns accepted.
    SessionClosed {
        /// The session in question.
        session_id: Uuid,
        /// Diagnostic detail (status name, etc.).
        message: String,
    },
    /// LLM backend transport / decode / refusal.
    Backend {
        /// Underlying cause as reported by the partner.
        message: String,
    },
    /// Daemon has no thinking-partner configured.
    Unavailable {
        /// Why the partner is missing (typically "not configured").
        message: String,
    },
    /// Unexpected / unmapped partner error.
    Internal {
        /// Free-form context.
        message: String,
    },
}

impl IntoResponse for ChatError {
    fn into_response(self) -> Response {
        let status = match &self {
            ChatError::BadRequest { .. } => StatusCode::BAD_REQUEST,
            ChatError::UnknownSession { .. } => StatusCode::NOT_FOUND,
            ChatError::SessionClosed { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            ChatError::Backend { .. } => StatusCode::BAD_GATEWAY,
            ChatError::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            ChatError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self)).into_response()
    }
}

/// Convert a [`ThinkingError`] surfaced by the partner into a
/// [`ChatError`]. Centralised so the start-session and send branches
/// map identically.
fn map_thinking_error(err: ThinkingError, fallback_id: Option<Uuid>) -> ChatError {
    match err {
        ThinkingError::Input(msg) => ChatError::BadRequest { message: msg },
        ThinkingError::UnknownSession(id) => ChatError::UnknownSession { session_id: id.0 },
        ThinkingError::BackendRefused(msg) => {
            // BackendRefused covers BOTH "session not Active" and
            // "LLM returned unusable content". The "session is …, not
            // Active" prefix is stable in `partner.rs` (lines 149-152,
            // 205-208) — match it to surface the right HTTP code.
            if msg.starts_with("session is ") {
                ChatError::SessionClosed {
                    session_id: fallback_id.unwrap_or_default(),
                    message: msg,
                }
            } else {
                ChatError::Backend { message: msg }
            }
        }
        ThinkingError::Transport(m)
        | ThinkingError::Decode(m)
        | ThinkingError::HttpStatus {
            status: _,
            snippet: m,
        } => ChatError::Backend { message: m },
        ThinkingError::Config(msg) => ChatError::Unavailable { message: msg },
    }
}

/// Skill names the registry exposes to the model, alphabetised. Empty
/// vec when the partner was not built with skills.
fn skills_index(skills: Option<&Arc<SkillRegistry>>) -> Vec<String> {
    skills
        .map(|reg| {
            let mut names: Vec<String> = reg.list().iter().map(|s| s.name.clone()).collect();
            names.sort();
            names
        })
        .unwrap_or_default()
}

/// POST `/api/evy/chat` handler. Reads `state.app.thinking_partner()`;
/// if `None` returns 503. Otherwise dispatches to start/send and shapes
/// the response.
///
/// `pub(crate)` because the handler signature carries the crate-private
/// `HttpState`; only `crate::http::build_router` references it. The
/// public surface of the module is the request/response types and the
/// `ChatError` enum, which downstream consumers (e.g. the chat TUI
/// crate's wire shapes) re-derive structurally.
///
/// # Errors
/// Surfaced via [`ChatError`] → status code; see module docs.
pub(crate) async fn chat_handler(
    State(state): State<HttpState>,
    Json(body): Json<ChatRequest>,
) -> std::result::Result<Json<ChatResponse>, ChatError> {
    let partner = state
        .app
        .thinking_partner()
        .ok_or_else(|| ChatError::Unavailable {
            message: "thinking-partner is not configured for this daemon".to_string(),
        })?;
    let skills = state.app.skills();

    let msg = body.message.trim();
    if msg.is_empty() {
        return Err(ChatError::BadRequest {
            message: "message must be non-empty".to_string(),
        });
    }

    // Branch on session_id presence: None opens a new session with the
    // message as the topic; Some appends.
    let (session_id, response_text) = match body.session_id {
        None => {
            let id = partner
                .start_session(msg.to_string())
                .await
                .map_err(|e| map_thinking_error(e, None))?;
            // The partner's opening clarifying questions live on the
            // last Partner message of the session. Pull them directly
            // so the response matches the chat-client expectation: one
            // request → one response string.
            let session = partner
                .session(id)
                .await
                .map_err(|e| map_thinking_error(e, Some(id.0)))?
                .ok_or_else(|| ChatError::Internal {
                    message: "session vanished after start_session".to_string(),
                })?;
            let opening = session
                .last_of(Role::Partner)
                .ok_or_else(|| ChatError::Internal {
                    message: "session opened but partner did not produce a reply".to_string(),
                })?;
            // Sanity-check the session status — if a timeout fired
            // between start and read, surface that instead of pretending.
            if session.status != SessionStatus::Active {
                return Err(ChatError::SessionClosed {
                    session_id: id.0,
                    message: format!("session opened but is now {:?}", session.status),
                });
            }
            (id.0, opening.content.clone())
        }
        Some(raw_id) => {
            let id = SessionId(raw_id);
            let reply = partner
                .send(id, msg.to_string())
                .await
                .map_err(|e| map_thinking_error(e, Some(raw_id)))?;
            (raw_id, reply)
        }
    };

    Ok(Json(ChatResponse {
        session_id,
        response: response_text,
        skills_loaded: skills_index(skills.as_ref()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_error_maps_to_expected_status_codes() {
        let cases = [
            (
                ChatError::BadRequest {
                    message: "x".into(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                ChatError::UnknownSession {
                    session_id: Uuid::nil(),
                },
                StatusCode::NOT_FOUND,
            ),
            (
                ChatError::SessionClosed {
                    session_id: Uuid::nil(),
                    message: "x".into(),
                },
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                ChatError::Backend {
                    message: "x".into(),
                },
                StatusCode::BAD_GATEWAY,
            ),
            (
                ChatError::Unavailable {
                    message: "x".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ChatError::Internal {
                    message: "x".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (e, expected) in cases {
            let response = e.into_response();
            assert_eq!(response.status(), expected);
        }
    }

    #[test]
    fn map_thinking_error_routes_session_closed() {
        // BackendRefused with the stable "session is …" prefix must
        // turn into SessionClosed (422), not Backend (502).
        let id = Uuid::new_v4();
        let err = ThinkingError::BackendRefused("session is Concluded, not Active".to_string());
        match map_thinking_error(err, Some(id)) {
            ChatError::SessionClosed { session_id, .. } => assert_eq!(session_id, id),
            other => panic!("expected SessionClosed, got {other:?}"),
        }
    }

    #[test]
    fn map_thinking_error_routes_backend_refused() {
        let err = ThinkingError::BackendRefused("empty content array".into());
        match map_thinking_error(err, None) {
            ChatError::Backend { .. } => {}
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[test]
    fn map_thinking_error_routes_unknown_session() {
        let sid = evy_thinking::SessionId::new();
        let err = ThinkingError::UnknownSession(sid);
        match map_thinking_error(err, None) {
            ChatError::UnknownSession { session_id } => assert_eq!(session_id, sid.0),
            other => panic!("expected UnknownSession, got {other:?}"),
        }
    }

    #[test]
    fn skills_index_returns_empty_when_partner_lacks_skills() {
        let v = skills_index(None);
        assert!(v.is_empty());
    }

    #[test]
    fn chat_request_round_trips_through_serde() {
        let body = r#"{"session_id":null,"message":"hi"}"#;
        let req: ChatRequest = serde_json::from_str(body).expect("parse");
        assert!(req.session_id.is_none());
        assert_eq!(req.message, "hi");

        let body2 = r#"{"message":"only message"}"#;
        let req2: ChatRequest = serde_json::from_str(body2).expect("parse no session");
        assert!(req2.session_id.is_none());
    }

    #[test]
    fn chat_response_round_trips_through_serde() {
        let resp = ChatResponse {
            session_id: Uuid::new_v4(),
            response: "ok".into(),
            skills_loaded: vec!["plan".into(), "debug".into()],
        };
        let s = serde_json::to_string(&resp).expect("serialize");
        let back: ChatResponse = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back.session_id, resp.session_id);
        assert_eq!(back.response, resp.response);
        assert_eq!(back.skills_loaded, resp.skills_loaded);
    }
}
