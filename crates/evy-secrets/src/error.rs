//! Library-side error type for `evy-secrets`.
//!
//! Typed via [`thiserror`] so callers (the daemon binary, dashboard
//! handlers, the eventual evy-comms wiring) can pattern-match on
//! failure modes. Per house style the library does NOT depend on
//! `anyhow`; binaries wrap these with `.context(...)`.
//!
//! ## Security note
//!
//! No variant carries a resolved secret value. The closest we get is
//! [`SecretsError::NotFound`], which only contains the *key name* the
//! caller asked for — never a value.

use std::path::PathBuf;

use thiserror::Error;

/// Errors surfaced by the evy-secrets crate.
#[derive(Debug, Error)]
pub enum SecretsError {
    /// The resolver consulted every backend in its chain and none of
    /// them produced a value for this key. Carries the requested key
    /// name so the operator can grep their config for it.
    #[error("secret \"{0}\" not found in any backend")]
    NotFound(String),

    /// Underlying I/O failure (file backend read, etc.). The path that
    /// was being accessed is preserved for the breadcrumb trail; the
    /// file *contents* never appear in this variant.
    #[error("io error at {path}: {source}")]
    Io {
        /// Path that was being accessed.
        path: PathBuf,
        /// Underlying I/O cause.
        #[source]
        source: std::io::Error,
    },

    /// File backend's on-disk payload was not a `{ "key": "value" }`
    /// JSON object. We surface the parser error but never the raw
    /// bytes — a malformed secrets.json could itself contain partial
    /// secret material from a botched edit.
    #[error("file backend json parse: {0}")]
    Json(#[from] serde_json::Error),

    /// The `op` CLI (1Password) could not be spawned. Distinct from a
    /// non-zero exit (which is treated as "not found" by the backend
    /// to match v3 silent-no-op semantics): if we can't even fork the
    /// process, the operator needs to know.
    #[error("failed to spawn `op` CLI: {0}")]
    OpSpawn(#[source] std::io::Error),
}

/// Crate-local `Result` alias. All public APIs return this.
pub type Result<T> = std::result::Result<T, SecretsError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_carries_key_name() {
        let e = SecretsError::NotFound("openai-api-key".into());
        let s = e.to_string();
        assert!(s.contains("openai-api-key"), "got: {s}");
    }

    #[test]
    fn io_does_not_leak_contents() {
        let e = SecretsError::Io {
            path: PathBuf::from("/tmp/secrets.json"),
            source: std::io::Error::other("permission denied"),
        };
        let s = e.to_string();
        assert!(s.contains("/tmp/secrets.json"), "got: {s}");
        assert!(s.contains("permission denied"), "got: {s}");
    }

    #[test]
    fn json_variant_from_serde() {
        let bad: serde_json::Result<serde_json::Value> = serde_json::from_str("not json");
        let e: SecretsError = bad.unwrap_err().into();
        assert!(matches!(e, SecretsError::Json(_)));
    }
}
