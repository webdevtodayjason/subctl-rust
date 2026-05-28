//! Phase 6 follow-up — sessions list + delete.
//!
//! Two thin proxies onto [`evy_thinking::ThinkingPartner`]:
//!
//! | Method | Path | Behaviour |
//! |--------|------|-----------|
//! | GET    | `/api/evy/sessions`        | list every in-memory session, newest first |
//! | DELETE | `/api/evy/sessions/:id`    | drop one session; 204 on hit, 404 on miss |
//!
//! # In-memory only
//!
//! v4's `ThinkingPartner` holds sessions in a `Mutex<HashMap<...>>`;
//! they do **not** survive a daemon restart. The listing therefore reflects
//! only what the operator has thought about since the daemon came up.
//! Persistence is a Phase 7 concern; this surface stays additive — when
//! sessions become durable the wire shape gains no fields, just gains
//! continuity across restarts.
//!
//! # Why `preview` derives from `session.topic`
//!
//! On a freshly opened session the message log opens with a `Role::System`
//! "Session opened: <topic>" marker, then a synthetic kickoff template
//! from [`evy_thinking::templates::kickoff_user_turn`] — neither is what
//! an operator typed. The operator's actual ask survives as
//! `Session::topic`, which is what every TUI surface wants to render in
//! the sidebar. We truncate at 80 chars so long topics don't blow up
//! a list view.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use chrono::{DateTime, Utc};
use evy_thinking::{Role, SessionId, SessionStatus};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::http::HttpState;

/// Maximum length of [`SessionSummary::preview`] in characters; longer
/// topics are truncated with a Unicode ellipsis. 80 keeps the row
/// renderable in a typical 100-col TUI without wrapping.
const PREVIEW_MAX_CHARS: usize = 80;

/// Operator-console-shaped projection of one in-memory session.
///
/// Narrow on purpose — the message log isn't included; clients that need
/// the full thread `POST /api/evy/chat` with the matching `session_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    /// Stable v4 UUID — round-trips with `ChatRequest::session_id`.
    pub id: Uuid,
    /// When the session was opened (`Utc::now()` at `start_session`).
    pub started_at: DateTime<Utc>,
    /// Last operator-or-partner activity.
    pub last_message_at: DateTime<Utc>,
    /// Total messages in the in-memory log, including the synthetic
    /// system marker and kickoff turn.
    pub message_count: usize,
    /// Truncated topic for sidebar rendering. See the module docs for
    /// why we read from `session.topic` rather than `messages[0]`.
    pub preview: String,
    /// Lifecycle state — `active`, `concluded`, or `timed_out`.
    pub status: SessionStatus,
}

/// JSON body returned by [`sessions_list_handler`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionsListResponse {
    /// All sessions, sorted newest-active first.
    pub sessions: Vec<SessionSummary>,
}

/// Failure modes for the sessions handlers. Serialised with the same
/// `{"kind":"..."}` shape used by `ChatError` so clients can branch
/// off a single discriminator across all evy-comms endpoints.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionsError {
    /// Daemon has no thinking-partner configured.
    Unavailable {
        /// Diagnostic message.
        message: String,
    },
    /// Client referenced a session the partner doesn't know.
    UnknownSession {
        /// The unknown id, echoed back for the operator's logs.
        session_id: Uuid,
    },
    /// Unexpected error from the partner (forward-compat — currently
    /// unreachable since `drop_session` is infallible).
    Internal {
        /// Free-form context.
        message: String,
    },
}

impl IntoResponse for SessionsError {
    fn into_response(self) -> Response {
        let status = match &self {
            SessionsError::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            SessionsError::UnknownSession { .. } => StatusCode::NOT_FOUND,
            SessionsError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self)).into_response()
    }
}

/// `GET /api/evy/sessions` handler.
///
/// # Errors
/// Returns [`SessionsError::Unavailable`] (HTTP 503) when no
/// thinking-partner is wired in.
pub(crate) async fn sessions_list_handler(
    State(state): State<HttpState>,
) -> std::result::Result<Json<SessionsListResponse>, SessionsError> {
    let partner = state
        .app
        .thinking_partner()
        .ok_or_else(|| SessionsError::Unavailable {
            message: "thinking-partner is not configured for this daemon".to_string(),
        })?;

    let sessions = partner
        .list_sessions()
        .await
        .map_err(|e| SessionsError::Internal {
            message: format!("listing sessions: {e}"),
        })?;

    let out: Vec<SessionSummary> = sessions
        .into_iter()
        .map(|s| {
            // Operator-typed text the TUI wants to display. Sessions
            // opened via `start_session` always have at least one
            // operator message (the synthetic kickoff turn); the topic
            // field carries the actual operator ask.
            let preview = truncate_preview(&s.topic);
            // Count partner+operator turns visible to a human — the
            // synthetic system marker and kickoff turn are still
            // included for shape stability; clients that want to
            // suppress them can filter on role from the chat thread.
            let _ = Role::System; // doc reference — see partner.rs
            SessionSummary {
                id: s.id.0,
                started_at: s.started_at,
                last_message_at: s.last_activity,
                message_count: s.messages.len(),
                preview,
                status: s.status,
            }
        })
        .collect();

    Ok(Json(SessionsListResponse { sessions: out }))
}

/// `DELETE /api/evy/sessions/:id` handler.
///
/// Returns 204 No Content on hit, 404 on miss. The body is empty on 204
/// to match RFC 7231; the 404 body carries the standard
/// `{"kind":"unknown_session"}` shape.
///
/// # Errors
/// Returns [`SessionsError::Unavailable`] (503) when no thinking-partner
/// is wired in, [`SessionsError::UnknownSession`] (404) when the id was
/// not registered.
pub(crate) async fn sessions_delete_handler(
    State(state): State<HttpState>,
    Path(id): Path<Uuid>,
) -> std::result::Result<Response, SessionsError> {
    let partner = state
        .app
        .thinking_partner()
        .ok_or_else(|| SessionsError::Unavailable {
            message: "thinking-partner is not configured for this daemon".to_string(),
        })?;

    let removed =
        partner
            .drop_session(SessionId(id))
            .await
            .map_err(|e| SessionsError::Internal {
                message: format!("dropping session: {e}"),
            })?;

    if removed {
        Ok((StatusCode::NO_CONTENT, ()).into_response())
    } else {
        Err(SessionsError::UnknownSession { session_id: id })
    }
}

/// Truncate `s` to at most [`PREVIEW_MAX_CHARS`] *characters* (Unicode
/// codepoints, not bytes), appending a single-character ellipsis when
/// truncation actually happened. Pure / unit-testable.
fn truncate_preview(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= PREVIEW_MAX_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(PREVIEW_MAX_CHARS - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_error_status_mapping() {
        let cases = [
            (
                SessionsError::Unavailable {
                    message: "x".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                SessionsError::UnknownSession {
                    session_id: Uuid::nil(),
                },
                StatusCode::NOT_FOUND,
            ),
            (
                SessionsError::Internal {
                    message: "x".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (err, expected) in cases {
            let resp = err.into_response();
            assert_eq!(resp.status(), expected);
        }
    }

    #[test]
    fn truncate_preview_passes_short_strings_through() {
        assert_eq!(truncate_preview("hello"), "hello");
        assert_eq!(truncate_preview("  hello  "), "hello");
    }

    #[test]
    fn truncate_preview_truncates_at_max_chars_with_ellipsis() {
        // 100 'x' chars — well past the 80 limit.
        let s: String = "x".repeat(100);
        let out = truncate_preview(&s);
        assert_eq!(out.chars().count(), PREVIEW_MAX_CHARS);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_preview_handles_multibyte_correctly() {
        // 'é' is one char but two bytes — verify we measure in chars.
        let s: String = "é".repeat(90);
        let out = truncate_preview(&s);
        assert_eq!(out.chars().count(), PREVIEW_MAX_CHARS);
    }

    #[test]
    fn session_summary_round_trips_through_serde() {
        let s = SessionSummary {
            id: Uuid::new_v4(),
            started_at: Utc::now(),
            last_message_at: Utc::now(),
            message_count: 3,
            preview: "a topic".into(),
            status: SessionStatus::Active,
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: SessionSummary = serde_json::from_str(&j).unwrap();
        assert_eq!(back, s);
    }
}
