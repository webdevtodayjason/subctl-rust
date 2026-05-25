//! Workspace error type for Evy v4 library code.
//!
//! `evy-core` deliberately does NOT depend on `anyhow`. Binaries
//! (`crates/evy/`) are free to wrap these into `anyhow::Result` with
//! `.context(...)`; libraries return [`Result`] (= `Result<T, Error>`).

use crate::{ProviderKind, WorkerId};

/// Every fallible operation in the Evy v4 workspace returns this.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The policy gate refused to dispatch.
    #[error("policy violation: {0}")]
    PolicyViolation(String),

    /// A provider adapter surfaced a transport or session error.
    #[error("provider error ({kind:?}): {reason}")]
    Provider {
        /// Which provider the error came from.
        kind: ProviderKind,
        /// Human-readable reason (already includes any underlying cause).
        reason: String,
    },

    /// A worker terminated abnormally in a way the provider could not
    /// surface as a structured `WorkerStatus`.
    #[error("worker failed: {0}")]
    WorkerFailed(String),

    /// The worker pool was asked about an id it does not own.
    #[error("worker not found: {0:?}")]
    WorkerNotFound(WorkerId),

    /// A mandate failed local validation before dispatch.
    #[error("invalid mandate: {0}")]
    InvalidMandate(String),

    /// Underlying I/O failure (file, socket, pipe).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization or deserialization failed.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Workspace `Result` alias for library code.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn display_is_descriptive() {
        let cases: Vec<(Error, &str)> = vec![
            (Error::PolicyViolation("sealed".into()), "policy violation"),
            (
                Error::Provider {
                    kind: ProviderKind::ClaudeCode,
                    reason: "401".into(),
                },
                "provider error",
            ),
            (Error::WorkerFailed("oom".into()), "worker failed"),
            (Error::WorkerNotFound(WorkerId::new()), "worker not found"),
            (Error::InvalidMandate("no goal".into()), "invalid mandate"),
        ];
        for (e, prefix) in cases {
            let s = e.to_string();
            assert!(s.starts_with(prefix), "expected prefix `{prefix}` in `{s}`");
        }
    }

    #[test]
    fn io_error_converts_via_from() {
        fn try_it() -> Result<()> {
            // `?` exercises the `From<std::io::Error>` impl.
            Err(io::Error::new(io::ErrorKind::NotFound, "missing"))?;
            Ok(())
        }
        match try_it().unwrap_err() {
            Error::Io(inner) => assert_eq!(inner.kind(), io::ErrorKind::NotFound),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn serde_error_converts_via_from() {
        fn try_it() -> Result<()> {
            // Invalid JSON triggers the `From<serde_json::Error>` impl.
            let _: serde_json::Value = serde_json::from_str("{not json")?;
            Ok(())
        }
        assert!(matches!(try_it().unwrap_err(), Error::Serde(_)));
    }
}
