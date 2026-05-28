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

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json, Response,
    },
};
use evy_skills::SkillRegistry;
use evy_thinking::{Role, SessionId, SessionStatus, StreamChunk, ThinkingError, ThinkingPartner};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
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

/// SSE wire variants carried by `data:` frames when the operator
/// requests streaming. Each variant becomes one SSE `data: {...}` line
/// followed by a blank-line separator, per the EventSource protocol.
///
/// | Variant | Wire shape | Emitted when |
/// |---------|------------|--------------|
/// | `Token` | `{"kind":"token","content":"H"}` | every backend delta |
/// | `SkillLoaded` | `{"kind":"skill_loaded","name":"plan"}` | backend autoloaded a skill |
/// | `Done` | `{"kind":"done","session_id":"<uuid>"}` | end of stream, success |
/// | `Error` | `{"kind":"error","message":"..."}` | end of stream, failure |
///
/// `Done` is always the last frame on a successful stream; `Error` is
/// always the last frame on a failed stream. The TUI keeps reading until
/// one of those arrives or the connection closes.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatStreamEvent {
    /// Next slice of assistant text.
    Token {
        /// Concatenate every `Token` frame in order to reconstruct the
        /// full reply.
        content: String,
    },
    /// Backend autoloaded a skill from the registry.
    SkillLoaded {
        /// Skill name as listed in `GET /api/evy/skills`.
        name: String,
    },
    /// End of stream — partner reply assembled successfully.
    Done {
        /// Session id the reply belongs to. Echoes back the client-
        /// supplied id on `send`, or a freshly minted one on
        /// `start_session`.
        session_id: Uuid,
    },
    /// End of stream — backend or partner failure. The outer `kind`
    /// tag is `"error"`; the inner `error_kind` field carries the
    /// underlying [`ChatError`] discriminator so the TUI can branch on
    /// failure mode (e.g. `"unavailable"` vs `"backend"`).
    Error {
        /// Inner `ChatError` discriminator (e.g. `"unavailable"`,
        /// `"unknown_session"`, `"backend"`).
        error_kind: String,
        /// Human-readable description.
        message: String,
    },
}

/// True when the request's `Accept` header opts into SSE streaming.
///
/// We accept `text/event-stream` exactly or as a comma-separated
/// component of a wider Accept list. Quality parameters (`;q=0.8`) are
/// not parsed — the TUI sends an unambiguous header.
fn wants_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|h| {
            h.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("text/event-stream"))
        })
        .unwrap_or(false)
}

/// POST `/api/evy/chat` handler — content-negotiates between blocking
/// JSON (default) and `text/event-stream` (when `Accept` opts in).
///
/// `pub(crate)` because the handler signature carries the crate-private
/// `HttpState`; only `crate::http::build_router` references it. The
/// public surface of the module is the request/response types and the
/// [`ChatError`] enum, which downstream consumers (e.g. the chat TUI
/// crate's wire shapes) re-derive structurally.
///
/// # Errors
/// In the blocking path: surfaced via [`ChatError`] → status code; see
/// module docs. In the streaming path: surfaced inline as
/// [`ChatStreamEvent::Error`] frames so the HTTP status stays 200 once
/// headers are flushed.
pub(crate) async fn chat_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(body): Json<ChatRequest>,
) -> Response {
    if wants_event_stream(&headers) {
        chat_handler_streaming(state, body).await.into_response()
    } else {
        chat_handler_blocking(state, body).await.into_response()
    }
}

/// Blocking JSON branch — the original Phase 6 behaviour, unchanged.
async fn chat_handler_blocking(
    state: HttpState,
    body: ChatRequest,
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

/// Streaming branch. Spawns a worker task that drives
/// [`ThinkingPartner::stream_send`] / [`ThinkingPartner::stream_start_session`]
/// into an `mpsc::Sender<StreamChunk>`; the receiver side is converted
/// into an axum SSE stream so the operator's TUI renders tokens as they
/// arrive.
///
/// Failure modes are surfaced inline as
/// [`ChatStreamEvent::Error`] frames once headers are flushed (the wire
/// status is 200). Failures detected *before* the worker spawns
/// (missing partner, empty message) are surfaced as a tiny SSE stream
/// carrying a single `Error` frame and immediately closed — the wire
/// status is still 200 to keep the EventSource client's reconnect logic
/// from firing.
async fn chat_handler_streaming(
    state: HttpState,
    body: ChatRequest,
) -> Sse<impl futures::Stream<Item = std::result::Result<Event, Infallible>>> {
    /// Capacity for the partner → handler chunk channel. 64 is wide
    /// enough that a fast backend doesn't back-pressure on the SSE
    /// flush, narrow enough that a slow client surfaces quickly.
    const CHUNK_BUFFER: usize = 64;

    // We always return an `Sse` — even when the request fails
    // validation, in which case we emit one Error frame and close.
    let (tx, rx) = mpsc::channel::<ChatStreamEvent>(CHUNK_BUFFER);

    let skills = state.app.skills();

    // Validate before spawning the worker so we don't spin up a task
    // just to fail. `match` keeps clippy happy about not double-checking
    // the Option's variant.
    let msg = body.message.trim().to_string();
    let session_id = body.session_id;
    match (state.app.thinking_partner(), msg.is_empty()) {
        (None, _) => {
            emit_error(
                &tx,
                ChatError::Unavailable {
                    message: "thinking-partner is not configured for this daemon".to_string(),
                },
            )
            .await;
        }
        (Some(_), true) => {
            emit_error(
                &tx,
                ChatError::BadRequest {
                    message: "message must be non-empty".to_string(),
                },
            )
            .await;
        }
        (Some(partner), false) => {
            let skill_names = skills_index(skills.as_ref());
            tokio::spawn(stream_worker(
                partner,
                session_id,
                msg,
                skill_names,
                tx.clone(),
            ));
        }
    }
    // Drop the handler-local sender — the worker (if spawned) cloned
    // it, so the receiver only completes once the worker exits.
    drop(tx);

    Sse::new(events_from_receiver(rx))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Per-chunk forwarding loop driving one chat turn. Owns the partner
/// handle and the message+session id; on completion sends a `Done`
/// frame (or `Error` on failure) so the client knows the stream is
/// terminal.
async fn stream_worker(
    partner: Arc<ThinkingPartner>,
    session_id: Option<Uuid>,
    msg: String,
    skill_names: Vec<String>,
    tx: mpsc::Sender<ChatStreamEvent>,
) {
    // Backend → handler chunk channel. Drained inline below into
    // ChatStreamEvent::Token frames; the partner side closes its
    // sender once the assembled text is committed to the session.
    let (chunk_tx, chunk_rx) = mpsc::channel::<StreamChunk>(64);

    // Up-front skill_loaded frames give the TUI the catalog the model
    // could see this turn. Per-skill autoload frames come from the
    // backend during the stream (Anthropic only at v0.5.0).
    for name in skill_names {
        if tx
            .send(ChatStreamEvent::SkillLoaded { name })
            .await
            .is_err()
        {
            return; // client gone before we even started
        }
    }

    // Spawn the partner drive on a separate task so we can interleave
    // chunk forwarding with the partner call. The partner takes a
    // borrow of the sender; we own it in this task.
    let partner_arc = partner.clone();
    let chunk_tx_inner = chunk_tx.clone();
    let session_id_for_worker = session_id;
    let msg_for_worker = msg.clone();
    let drive: tokio::task::JoinHandle<Result<Uuid, ChatError>> = tokio::spawn(async move {
        let result = match session_id_for_worker {
            None => partner_arc
                .stream_start_session(msg_for_worker, &chunk_tx_inner)
                .await
                .map(|id| id.0)
                .map_err(|e| map_thinking_error(e, None)),
            Some(raw) => {
                let id = SessionId(raw);
                partner_arc
                    .stream_send(id, msg_for_worker, &chunk_tx_inner)
                    .await
                    .map(|_| raw)
                    .map_err(|e| map_thinking_error(e, Some(raw)))
            }
        };
        drop(chunk_tx_inner);
        result
    });
    drop(chunk_tx);

    // Forward every chunk the partner emits to the SSE client.
    let mut chunk_stream = ReceiverStream::new(chunk_rx);
    while let Some(chunk) = chunk_stream.next().await {
        let event = match chunk {
            StreamChunk::Token(content) => ChatStreamEvent::Token { content },
            StreamChunk::SkillLoaded(name) => ChatStreamEvent::SkillLoaded { name },
        };
        if tx.send(event).await.is_err() {
            // Client disconnected. Keep draining `chunk_stream` so the
            // partner task isn't stuck on its sender; we just stop
            // forwarding to a dropped sink.
            while chunk_stream.next().await.is_some() {}
            break;
        }
    }

    // Partner task is done — collect its outcome.
    let outcome = match drive.await {
        Ok(r) => r,
        Err(join_err) => {
            // Panic or cancellation in the worker — surface as
            // Internal so the client knows the stream is terminal.
            emit_error(
                &tx,
                ChatError::Internal {
                    message: format!("partner task panicked: {join_err}"),
                },
            )
            .await;
            return;
        }
    };

    match outcome {
        Ok(sid) => {
            let _ = tx.send(ChatStreamEvent::Done { session_id: sid }).await;
        }
        Err(err) => emit_error(&tx, err).await,
    }
}

/// Build an SSE event stream from the receiver. Each [`ChatStreamEvent`]
/// becomes one `data: {...}\n\n` frame. Serialization failures (should
/// never happen for our static enum) are swallowed with a warn-log to
/// avoid taking down the whole stream.
fn events_from_receiver(
    rx: mpsc::Receiver<ChatStreamEvent>,
) -> impl futures::Stream<Item = std::result::Result<Event, Infallible>> {
    ReceiverStream::new(rx).filter_map(|ev| match Event::default().json_data(&ev) {
        Ok(frame) => Some(Ok(frame)),
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize ChatStreamEvent for SSE");
            None
        }
    })
}

/// Emit an `Error` frame on the wire stream. `into_kind_str` keeps the
/// discriminator stable for the TUI to switch on.
async fn emit_error(tx: &mpsc::Sender<ChatStreamEvent>, err: ChatError) {
    let kind = match &err {
        ChatError::BadRequest { .. } => "bad_request",
        ChatError::UnknownSession { .. } => "unknown_session",
        ChatError::SessionClosed { .. } => "session_closed",
        ChatError::Backend { .. } => "backend",
        ChatError::Unavailable { .. } => "unavailable",
        ChatError::Internal { .. } => "internal",
    };
    // Pull the message field for client-display via serde so we don't
    // need a giant match arm per variant.
    let payload = serde_json::to_value(&err).unwrap_or_else(|_| json!({"kind": kind}));
    let message = payload
        .get("message")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_default();
    let _ = tx
        .send(ChatStreamEvent::Error {
            error_kind: kind.to_string(),
            message,
        })
        .await;
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
