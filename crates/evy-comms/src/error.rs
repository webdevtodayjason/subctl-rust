//! Library-side error type for `evy-comms`.
//!
//! Typed via [`thiserror`] so callers (the daemon binary, integration
//! tests) can pattern-match on failure modes. Per house style the
//! library does NOT depend on `anyhow`; the binary may wrap these into
//! `anyhow::Result` with `.context(...)` at its boundary.

use thiserror::Error;

/// Errors surfaced by the evy-comms HTTP layer.
#[derive(Debug, Error)]
pub enum CommsError {
    /// The TCP listener could not be bound to the configured address.
    #[error("failed to bind to {addr}: {source}")]
    Bind {
        /// The address that was requested.
        addr: String,
        /// Underlying I/O cause.
        #[source]
        source: std::io::Error,
    },

    /// axum's `serve` returned an error during steady-state operation
    /// or shutdown.
    #[error("axum serve error: {source}")]
    Serve {
        /// Underlying axum / hyper cause.
        #[source]
        source: std::io::Error,
    },

    /// A configured CORS allow-origin was not a valid HTTP header value.
    #[error("invalid CORS allow_origin: {0}")]
    Cors(String),
}

/// Workspace `Result` alias for evy-comms library code.
pub type Result<T> = std::result::Result<T, CommsError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn bind_error_is_descriptive() {
        let e = CommsError::Bind {
            addr: "127.0.0.1:1".to_owned(),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "nope"),
        };
        let s = e.to_string();
        assert!(s.contains("127.0.0.1:1"), "got: {s}");
        assert!(s.contains("nope"), "got: {s}");
    }

    #[test]
    fn cors_error_carries_origin() {
        let e = CommsError::Cors("http://bad\norigin".to_owned());
        let s = e.to_string();
        assert!(s.contains("http://bad"), "got: {s}");
    }

    #[test]
    fn serve_error_is_descriptive() {
        let e = CommsError::Serve {
            source: io::Error::other("boom"),
        };
        assert!(e.to_string().contains("boom"));
    }
}
