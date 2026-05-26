//! Library-side error type for `evy-thinking`.
//!
//! Typed via [`thiserror`] so callers can pattern-match. Per house style
//! this crate does NOT depend on `anyhow`; the binary may wrap these
//! into `anyhow::Result` with `.context(...)` at its boundary.
//!
//! We deliberately do NOT thread these into [`evy_core::Error`]. The
//! variants here (`HttpStatus`, `Decode`, `UnknownSession`,
//! `BackendRefused`) don't fit cleanly into the workspace enum, and
//! evy-research already established the precedent of crate-local errors
//! at integration seams ([see `evy-research/src/error.rs`]).

use thiserror::Error;

use crate::session::SessionId;

/// Errors surfaced by the thinking-partner surface.
#[derive(Debug, Error)]
pub enum ThinkingError {
    /// Transport-level failure talking to the LLM backend
    /// (DNS, TLS, connection refused, broken pipe, etc.).
    #[error("thinking transport: {0}")]
    Transport(String),

    /// LLM backend returned a non-2xx HTTP status. The body snippet is
    /// truncated to keep error rendering bounded and to avoid leaking
    /// arbitrarily large payloads into logs.
    #[error("thinking http {status}: {snippet}")]
    HttpStatus {
        /// HTTP status code returned by the backend.
        status: u16,
        /// First 200 chars of the response body.
        snippet: String,
    },

    /// Backend response could not be decoded into the expected schema.
    #[error("thinking decode: {0}")]
    Decode(String),

    /// The LLM produced content but the partner could not extract a
    /// usable response (e.g. empty `content` array, no `text` block).
    /// Distinct from `Decode` — the wire shape was valid, the content
    /// was just unusable for the planning UX.
    #[error("thinking backend refused: {0}")]
    BackendRefused(String),

    /// Configuration missing a required field (e.g. `ANTHROPIC_API_KEY`
    /// absent in [`crate::AnthropicConfig::from_env`]).
    #[error("thinking config: {0}")]
    Config(String),

    /// Caller referenced a session id the partner doesn't know about
    /// (never started, or already evicted).
    #[error("thinking: unknown session {0:?}")]
    UnknownSession(SessionId),

    /// Caller passed empty / invalid input (empty topic, empty operator
    /// message, etc.).
    #[error("thinking input: {0}")]
    Input(String),
}

/// Crate `Result` alias.
pub type Result<T> = std::result::Result<T, ThinkingError>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionId;
    use uuid::Uuid;

    #[test]
    fn http_error_is_descriptive() {
        let e = ThinkingError::HttpStatus {
            status: 429,
            snippet: "rate limited".to_owned(),
        };
        let s = e.to_string();
        assert!(s.contains("429"), "got: {s}");
        assert!(s.contains("rate limited"), "got: {s}");
    }

    #[test]
    fn transport_error_carries_cause() {
        let e = ThinkingError::Transport("connection refused".to_owned());
        assert!(e.to_string().contains("connection refused"));
    }

    #[test]
    fn unknown_session_renders_id() {
        let id = SessionId(Uuid::new_v4());
        let e = ThinkingError::UnknownSession(id);
        assert!(e.to_string().contains("unknown session"));
    }

    #[test]
    fn config_error_is_descriptive() {
        let e = ThinkingError::Config("missing ANTHROPIC_API_KEY".to_owned());
        assert!(e.to_string().contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn backend_refused_is_distinct_from_decode() {
        let refused = ThinkingError::BackendRefused("empty content array".to_owned());
        let decoded = ThinkingError::Decode("missing field".to_owned());
        assert_ne!(refused.to_string(), decoded.to_string());
    }
}
