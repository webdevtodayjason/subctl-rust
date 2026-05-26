//! Library-side error type for `evy-research`.
//!
//! Typed via [`thiserror`] so callers (the daemon binary, the thinking-
//! partner surface, integration tests) can pattern-match on failure
//! modes. Per house style this crate does NOT depend on `anyhow`; the
//! binary may wrap these into `anyhow::Result` with `.context(...)` at
//! its boundary.
//!
//! Note: per ADR 0020 evy-research is a **leaf crate** — it does not
//! depend on `evy-core`. Errors stay local to this crate and the caller
//! adapts them at the integration seam (this avoids a circular dep when
//! the eventual thinking-partner surface in evy-core wants to call us).

use thiserror::Error;

/// Errors surfaced by the evy-research backends.
///
/// The non-`Display` `reqwest::Error` carries the request URL in its
/// formatting by default; we strip it where the URL embeds credentials
/// (none here — TinyFish auth is a header — but we keep the discipline
/// for forward-compatibility with other backends).
#[derive(Debug, Error)]
pub enum ResearchError {
    /// Transport-level failure talking to the upstream research backend
    /// (DNS, TLS, connection refused, broken pipe, etc.).
    #[error("research transport: {0}")]
    Transport(String),

    /// Backend returned a non-2xx HTTP status. The body snippet is
    /// truncated to keep error rendering bounded and to avoid leaking
    /// arbitrarily large payloads into logs.
    #[error("research http {status}: {snippet}")]
    Http {
        /// HTTP status code returned by the backend.
        status: u16,
        /// First 200 chars of the response body.
        snippet: String,
    },

    /// Backend response could not be decoded into the expected schema.
    #[error("research decode: {0}")]
    Decode(String),

    /// Caller passed an invalid URL (Fetch) or query (Search).
    #[error("research input: {0}")]
    Input(String),

    /// Configuration missing a required field (e.g. API token absent
    /// when the backend requires one).
    #[error("research config: {0}")]
    Config(String),
}

/// Crate `Result` alias.
pub type Result<T> = std::result::Result<T, ResearchError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_error_is_descriptive() {
        let e = ResearchError::Http {
            status: 429,
            snippet: "rate limited".to_owned(),
        };
        let s = e.to_string();
        assert!(s.contains("429"), "got: {s}");
        assert!(s.contains("rate limited"), "got: {s}");
    }

    #[test]
    fn transport_error_carries_cause() {
        let e = ResearchError::Transport("connection refused".to_owned());
        assert!(e.to_string().contains("connection refused"));
    }

    #[test]
    fn decode_error_is_descriptive() {
        let e = ResearchError::Decode("expected object got array".to_owned());
        assert!(e.to_string().contains("expected object"));
    }

    #[test]
    fn config_error_is_descriptive() {
        let e = ResearchError::Config("missing api token".to_owned());
        assert!(e.to_string().contains("missing"));
    }

    #[test]
    fn input_error_is_descriptive() {
        let e = ResearchError::Input("empty query".to_owned());
        assert!(e.to_string().contains("empty"));
    }
}
