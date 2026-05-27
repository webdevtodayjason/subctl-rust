//! JSON-file backend.
//!
//! Reads a flat `{ "<key>": "<value>" }` JSON object from disk. This is
//! the slot v3 called `~/.config/subctl/secrets.json` — the dashboard-
//! editable layer the operator uses when they don't want to touch
//! launchd plists.
//!
//! ### Semantics
//!
//! - File does not exist → `Ok(None)` (the file backend is opt-in;
//!   absence is normal).
//! - File exists but key is missing → `Ok(None)`.
//! - File exists, key present, value non-empty string → `Some(...)`.
//! - File exists but is malformed JSON → [`crate::SecretsError::Json`].
//! - I/O error reading the file → [`crate::SecretsError::Io`].
//!
//! Each `resolve()` call re-reads the file. A daemon-wide cache could
//! sit *in front of* this backend, but caching is out of scope for the
//! v4 minimal port (v3's 5-second mtime cache is a future enhancement).
//!
//! ### Security
//!
//! On a successful resolve we log the *key* and a `source = "file"`
//! field — never the value or the raw file payload.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::{Result, SecretValue, SecretsBackend, SecretsError};

/// Resolves secrets from a JSON object on disk.
#[derive(Debug, Clone)]
pub struct FileBackend {
    path: PathBuf,
}

impl FileBackend {
    /// Construct a file backend pointing at `path`. The file does not
    /// need to exist at construction time — missing files are handled
    /// as `Ok(None)` per the contract above.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Borrow the configured path. Useful for logging the on-disk
    /// location without exposing the contents.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl SecretsBackend for FileBackend {
    fn name(&self) -> &str {
        "file"
    }

    async fn resolve(&self, key: &str) -> Result<Option<SecretValue>> {
        let contents = match tokio::fs::read_to_string(&self.path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(SecretsError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        // The on-disk shape is a flat map. We deliberately use
        // HashMap<String, String> rather than `serde_json::Value` so a
        // non-string value (e.g. an accidental array) surfaces as a
        // parse error instead of being silently coerced.
        let parsed: HashMap<String, String> = serde_json::from_str(&contents)?;
        match parsed.get(key) {
            Some(v) if !v.is_empty() => Ok(Some(SecretValue {
                value: v.clone(),
                source: self.name().to_string(),
            })),
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn write_json(contents: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tmpfile");
        f.write_all(contents.as_bytes()).expect("write");
        f.flush().expect("flush");
        f
    }

    #[tokio::test]
    async fn missing_file_is_none() {
        let backend = FileBackend::new("/nonexistent/path/that/will/not/exist");
        let got = backend.resolve("any-key").await.expect("ok");
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn returns_value_when_key_present() {
        let f = write_json(r#"{"openai-api-key": "sk-abc123"}"#);
        let backend = FileBackend::new(f.path());
        let got = backend
            .resolve("openai-api-key")
            .await
            .expect("ok")
            .expect("some");
        assert_eq!(got.value, "sk-abc123");
        assert_eq!(got.source, "file");
    }

    #[tokio::test]
    async fn returns_none_when_key_missing() {
        let f = write_json(r#"{"some-other-key": "value"}"#);
        let backend = FileBackend::new(f.path());
        let got = backend.resolve("openai-api-key").await.expect("ok");
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn empty_string_value_is_none() {
        let f = write_json(r#"{"openai-api-key": ""}"#);
        let backend = FileBackend::new(f.path());
        let got = backend.resolve("openai-api-key").await.expect("ok");
        assert!(got.is_none(), "empty string should not satisfy resolve");
    }

    #[tokio::test]
    async fn malformed_json_surfaces_json_error() {
        let f = write_json("not valid json at all");
        let backend = FileBackend::new(f.path());
        let err = backend
            .resolve("openai-api-key")
            .await
            .expect_err("expected parse error");
        assert!(matches!(err, SecretsError::Json(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn non_string_value_surfaces_json_error() {
        // serde_json::from_str::<HashMap<String, String>> will refuse a
        // non-string leaf — this is the "accidental array" guardrail.
        let f = write_json(r#"{"openai-api-key": ["not", "a", "string"]}"#);
        let backend = FileBackend::new(f.path());
        let err = backend
            .resolve("openai-api-key")
            .await
            .expect_err("expected type error");
        assert!(matches!(err, SecretsError::Json(_)), "got: {err:?}");
    }

    #[test]
    fn name_is_stable() {
        let backend = FileBackend::new("/tmp/foo");
        assert_eq!(backend.name(), "file");
    }
}
