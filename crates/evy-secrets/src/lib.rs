//! `evy-secrets` — pluggable secret-resolution layer for Evy v4.
//!
//! Phase 4 Slice F port of v3's `components/evy/secrets.ts` +
//! `secrets-backends.ts`. The v3 module wired a 3-source chain
//! (env → 1Password → file) behind a synchronous `resolveSecret` and
//! an async `resolveSecretChain`. This crate compresses that to a
//! single trait-driven async resolver:
//!
//! - [`SecretsBackend`] — async trait. One impl per source.
//! - [`SecretsResolver`] — holds an ordered `Vec<Arc<dyn SecretsBackend>>`
//!   and walks the chain. First non-`None` hit wins.
//! - [`SecretValue`] — carries the resolved string AND which backend
//!   produced it (so dashboard / telemetry surfaces can tell the
//!   operator "this key is currently coming from `env`").
//!
//! Three first-party backends ship in this crate:
//!
//! - [`EnvBackend`] — uppercases & dash-to-underscore the key, then
//!   reads `std::env`.
//! - [`FileBackend`] — JSON object on disk, `{ "<key>": "<value>" }`.
//! - [`OnePasswordBackend`] — shells out to `op read op://<vault>/<key>`
//!   with a Service Account token in the child env.
//!
//! Backends out of scope for this slice: the 5-minute `op` result
//! cache, the audit log, the `secrets-backends.json` config layer,
//! `listSecrets`/`setSecret`. They're a clean follow-up if/when the
//! daemon needs them — the trait surface won't change.
//!
//! ## Security contract
//!
//! - No resolved secret value EVER appears in a `tracing` field, error
//!   variant, or `Display` impl produced by this crate.
//! - [`SecretValue::Debug`] is custom: it shows `source` and the
//!   value's *length*, never the bytes.
//! - [`OnePasswordBackend::Debug`] redacts the Service Account token.
//!
//! ## Wiring example
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use evy_secrets::{
//!     EnvBackend, FileBackend, OnePasswordBackend, SecretsBackend, SecretsResolver,
//! };
//!
//! # async fn demo() -> evy_secrets::Result<()> {
//! let resolver = SecretsResolver::new(vec![
//!     Arc::new(EnvBackend::new()) as Arc<dyn SecretsBackend>,
//!     Arc::new(OnePasswordBackend::new(
//!         std::env::var("OP_SERVICE_ACCOUNT_TOKEN").unwrap_or_default(),
//!         "Engineering".into(),
//!     )) as Arc<dyn SecretsBackend>,
//!     Arc::new(FileBackend::new("/etc/subctl/secrets.json")) as Arc<dyn SecretsBackend>,
//! ]);
//!
//! let value = resolver.resolve("openai-api-key").await?;
//! // value.value is the secret; value.source is "env" / "onepassword" / "file"
//! # let _ = value;
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::debug;

pub mod env;
pub mod error;
pub mod file;
pub mod onepassword;

// ── Public re-exports — the surface the daemon binary and evy-comms
//    bridge consume. ────────────────────────────────────────────────────
pub use env::EnvBackend;
pub use error::{Result, SecretsError};
pub use file::FileBackend;
pub use onepassword::OnePasswordBackend;

/// A resolved secret. `value` is the credential itself; `source` names
/// the backend that produced it (matches [`SecretsBackend::name`]) so
/// telemetry and dashboard surfaces can describe provenance without
/// re-running the chain.
///
/// `Debug` is implemented manually to redact `value` — the field is
/// still `pub`, so callers that genuinely need the bytes can read them,
/// but a stray `tracing::error!(?got, ...)` will not leak the secret.
#[derive(Clone)]
pub struct SecretValue {
    /// The resolved secret value. **Sensitive.** Never log this field
    /// directly — pass it on to whichever subsystem actually needs it
    /// (HTTP Authorization header, env var for a child process, ...).
    pub value: String,
    /// Name of the backend that produced this value
    /// (e.g. `"env"`, `"file"`, `"onepassword"`). Always safe to log.
    pub source: String,
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretValue")
            .field("source", &self.source)
            .field(
                "value",
                &format_args!("<redacted len={}>", self.value.len()),
            )
            .finish()
    }
}

/// A pluggable secret source. Implementors are stateless or hold their
/// own configuration; they must be cheap to share across tasks
/// (`Send + Sync` and typically wrapped in [`Arc`] inside the resolver).
///
/// `resolve` returns `Ok(None)` when the backend simply has no value
/// for `key` — the resolver treats this as "fall through to the next
/// backend". An `Err` short-circuits the whole chain and propagates to
/// the caller.
#[async_trait]
pub trait SecretsBackend: Send + Sync {
    /// Short stable name for this backend. Used for the `source` field
    /// on returned [`SecretValue`]s and for log telemetry. SHOULD be
    /// lowercase ASCII with no whitespace (e.g. `"env"`, `"file"`,
    /// `"onepassword"`).
    fn name(&self) -> &str;

    /// Try to resolve `key`. Return `Ok(None)` for "no value here, try
    /// the next backend"; return `Err(...)` only for genuine failures
    /// the operator needs to see (I/O, JSON parse, spawn failure).
    async fn resolve(&self, key: &str) -> Result<Option<SecretValue>>;
}

/// Walks an ordered chain of backends. The first backend to return
/// `Ok(Some(v))` wins; a backend's `Err` short-circuits the chain.
///
/// Construct with [`SecretsResolver::new`]. The resolver is `Send +
/// Sync`; wrap in `Arc` once and share across the daemon's tasks.
pub struct SecretsResolver {
    backends: Vec<Arc<dyn SecretsBackend>>,
}

impl fmt::Debug for SecretsResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.backends.iter().map(|b| b.name()).collect();
        f.debug_struct("SecretsResolver")
            .field("chain", &names)
            .finish()
    }
}

impl SecretsResolver {
    /// Build a resolver from a priority-ordered backend list. The
    /// first backend gets first shot at every key; iteration stops on
    /// the first non-`None` result.
    #[must_use]
    pub fn new(backends: Vec<Arc<dyn SecretsBackend>>) -> Self {
        Self { backends }
    }

    /// Names of the configured backends in chain order. Useful for
    /// `/secrets/status` dashboard endpoints — exposes structure
    /// without exposing secret material.
    #[must_use]
    pub fn backend_names(&self) -> Vec<&str> {
        self.backends.iter().map(|b| b.name()).collect()
    }

    /// Resolve `key` through the chain. Returns [`SecretsError::NotFound`]
    /// when every backend declined.
    pub async fn resolve(&self, key: &str) -> Result<SecretValue> {
        for backend in &self.backends {
            let name = backend.name();
            match backend.resolve(key).await {
                Ok(Some(v)) => {
                    debug!(target: "secrets", key, source = name, "resolved");
                    return Ok(v);
                }
                Ok(None) => {
                    debug!(target: "secrets", key, source = name, "miss");
                    continue;
                }
                Err(e) => {
                    // Hard failure — surface it. Don't fall through:
                    // the operator needs to know that, say, their
                    // secrets.json is malformed rather than silently
                    // tumbling all the way to NotFound.
                    return Err(e);
                }
            }
        }
        Err(SecretsError::NotFound(key.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tiny in-memory backend used to drive the resolver's chain
    //    semantics without dragging the file/env/op machinery in. ─────

    struct StaticBackend {
        name: &'static str,
        map: std::collections::HashMap<&'static str, &'static str>,
    }

    #[async_trait]
    impl SecretsBackend for StaticBackend {
        fn name(&self) -> &str {
            self.name
        }
        async fn resolve(&self, key: &str) -> Result<Option<SecretValue>> {
            Ok(self.map.get(key).map(|v| SecretValue {
                value: (*v).to_string(),
                source: self.name.to_string(),
            }))
        }
    }

    struct AlwaysErrBackend;

    #[async_trait]
    impl SecretsBackend for AlwaysErrBackend {
        fn name(&self) -> &str {
            "explode"
        }
        async fn resolve(&self, _key: &str) -> Result<Option<SecretValue>> {
            Err(SecretsError::Io {
                path: "/dev/null".into(),
                source: std::io::Error::other("boom"),
            })
        }
    }

    #[test]
    fn secret_value_debug_redacts() {
        let v = SecretValue {
            value: "sk-supersecret-123".into(),
            source: "env".into(),
        };
        let s = format!("{v:?}");
        assert!(!s.contains("sk-supersecret-123"), "got: {s}");
        assert!(s.contains("<redacted"), "got: {s}");
        assert!(s.contains("env"), "got: {s}");
    }

    #[tokio::test]
    async fn resolver_returns_first_hit_in_order() {
        let mut m1 = std::collections::HashMap::new();
        m1.insert("k", "from-first");
        let mut m2 = std::collections::HashMap::new();
        m2.insert("k", "from-second");

        let resolver = SecretsResolver::new(vec![
            Arc::new(StaticBackend {
                name: "first",
                map: m1,
            }) as Arc<dyn SecretsBackend>,
            Arc::new(StaticBackend {
                name: "second",
                map: m2,
            }) as Arc<dyn SecretsBackend>,
        ]);
        let got = resolver.resolve("k").await.expect("ok");
        assert_eq!(got.value, "from-first");
        assert_eq!(got.source, "first");
    }

    #[tokio::test]
    async fn resolver_falls_through_to_second_on_miss() {
        let m1 = std::collections::HashMap::new(); // empty
        let mut m2 = std::collections::HashMap::new();
        m2.insert("k", "from-second");

        let resolver = SecretsResolver::new(vec![
            Arc::new(StaticBackend {
                name: "first",
                map: m1,
            }) as Arc<dyn SecretsBackend>,
            Arc::new(StaticBackend {
                name: "second",
                map: m2,
            }) as Arc<dyn SecretsBackend>,
        ]);
        let got = resolver.resolve("k").await.expect("ok");
        assert_eq!(got.value, "from-second");
        assert_eq!(got.source, "second");
    }

    #[tokio::test]
    async fn resolver_not_found_when_chain_exhausted() {
        let resolver = SecretsResolver::new(vec![Arc::new(StaticBackend {
            name: "empty",
            map: std::collections::HashMap::new(),
        }) as Arc<dyn SecretsBackend>]);
        let err = resolver.resolve("missing-key").await.expect_err("err");
        assert!(matches!(err, SecretsError::NotFound(ref k) if k == "missing-key"));
    }

    #[tokio::test]
    async fn resolver_short_circuits_on_backend_error() {
        let mut m2 = std::collections::HashMap::new();
        m2.insert("k", "would-have-worked");
        let resolver = SecretsResolver::new(vec![
            Arc::new(AlwaysErrBackend) as Arc<dyn SecretsBackend>,
            Arc::new(StaticBackend {
                name: "second",
                map: m2,
            }) as Arc<dyn SecretsBackend>,
        ]);
        let err = resolver.resolve("k").await.expect_err("must short-circuit");
        assert!(matches!(err, SecretsError::Io { .. }), "got: {err:?}");
    }

    #[test]
    fn resolver_backend_names_in_order() {
        let resolver = SecretsResolver::new(vec![
            Arc::new(EnvBackend::new()) as Arc<dyn SecretsBackend>,
            Arc::new(FileBackend::new("/tmp/nope")) as Arc<dyn SecretsBackend>,
        ]);
        assert_eq!(resolver.backend_names(), vec!["env", "file"]);
    }
}
