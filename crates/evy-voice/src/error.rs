//! Library-side error type for `evy-voice`.
//!
//! Typed via [`thiserror`] so callers (the daemon binary, integration
//! tests, the eventual evy-comms voice bridge) can pattern-match on
//! failure modes. Per house style the library does NOT depend on
//! `anyhow`; binaries wrap these with `.context(...)`.

use std::path::PathBuf;

use thiserror::Error;

/// Errors surfaced by the evy-voice crate.
#[derive(Debug, Error)]
pub enum VoiceError {
    /// The voice layer is disabled in `voice.json` (`enabled: false`).
    /// The renderer refuses every render in this mode — operator must
    /// flip the flag (CLI / dashboard / direct edit) to opt in.
    #[error("voice disabled in voice.json")]
    Disabled,

    /// Caller supplied empty text after trimming.
    #[error("text required")]
    EmptyText,

    /// Caller supplied text past the hard ceiling. Mirrors v3's
    /// `MAX_TEXT_CHARS = 4000` protective floor — defends the local TTS
    /// server from runaway prompts.
    #[error("text exceeds {limit} chars (got {got})")]
    TextTooLong {
        /// Maximum characters allowed.
        limit: usize,
        /// Characters in the input.
        got: usize,
    },

    /// Egress redaction matched. Render refused — the input contained a
    /// pattern that looks like a secret (Bearer token, sk-* key, HMAC
    /// trust marker prefix, …). The text is NOT echoed back into the
    /// error to keep the secret out of logs.
    #[error("egress redaction: text matched secret pattern `{pattern}`")]
    Redacted {
        /// Which pattern fired (label, not the regex itself).
        pattern: &'static str,
    },

    /// TTS server returned a non-2xx status.
    #[error("tts server HTTP {status}: {detail}")]
    TtsServerStatus {
        /// HTTP status code.
        status: u16,
        /// Truncated response body, for diagnostics.
        detail: String,
    },

    /// TTS server returned an empty body.
    #[error("tts server returned empty audio")]
    TtsServerEmpty,

    /// TTS server transport error (unreachable, DNS, TLS, body read).
    #[error("tts server transport error: {0}")]
    TtsTransport(String),

    /// Underlying I/O failure (cache write, config read, watcher init).
    #[error("io error at {path}: {source}")]
    Io {
        /// Path that was being accessed.
        path: PathBuf,
        /// Underlying I/O cause.
        #[source]
        source: std::io::Error,
    },

    /// Filesystem watcher failed to start.
    #[error("fs watcher: {0}")]
    Watcher(String),

    /// JSON parse / serialize failed for the on-disk config.
    #[error("config json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Crate-local `Result` alias.
pub type Result<T> = std::result::Result<T, VoiceError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_does_not_leak_text() {
        assert_eq!(
            VoiceError::Disabled.to_string(),
            "voice disabled in voice.json"
        );
    }

    #[test]
    fn redacted_does_not_leak_text() {
        // Critical: the error message must NOT contain the offending
        // secret. We only carry the *label* of which pattern fired.
        let e = VoiceError::Redacted { pattern: "bearer" };
        let s = e.to_string();
        assert!(s.contains("bearer"), "got: {s}");
        assert!(
            !s.contains("sk-"),
            "redacted error must not echo input: {s}"
        );
    }

    #[test]
    fn text_too_long_is_descriptive() {
        let e = VoiceError::TextTooLong {
            limit: 4000,
            got: 8192,
        };
        let s = e.to_string();
        assert!(s.contains("4000"), "got: {s}");
        assert!(s.contains("8192"), "got: {s}");
    }
}
