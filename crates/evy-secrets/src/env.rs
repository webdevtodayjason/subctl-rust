//! Environment-variable backend.
//!
//! The simplest of the three backends. Reads from `std::env` after
//! normalizing the caller's key into env-var form:
//!
//! ```text
//!   "openai-api-key"  →  OPENAI_API_KEY
//!   "linear_api_key"  →  LINEAR_API_KEY
//! ```
//!
//! Empty strings are treated as "not set" — an env var that exists
//! but is blank doesn't satisfy a `resolve()` call. This matches v3's
//! `loadSecret` semantics (`v.length > 0`).

use async_trait::async_trait;

use crate::{Result, SecretValue, SecretsBackend};

/// Resolves secrets from process environment variables. Unit struct —
/// stateless; one instance per resolver is plenty.
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvBackend;

impl EnvBackend {
    /// Construct a new env backend. No configuration — env-var names
    /// are derived from the key at lookup time.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Normalize a logical key into the env-var name we'll look up.
    /// Exposed `pub(crate)` so the resolver's error/logging path can
    /// describe what we tried without re-implementing the rule.
    pub(crate) fn env_var_for(key: &str) -> String {
        key.to_ascii_uppercase().replace('-', "_")
    }
}

#[async_trait]
impl SecretsBackend for EnvBackend {
    fn name(&self) -> &str {
        "env"
    }

    async fn resolve(&self, key: &str) -> Result<Option<SecretValue>> {
        let env_key = Self::env_var_for(key);
        match std::env::var(&env_key) {
            Ok(v) if !v.is_empty() => Ok(Some(SecretValue {
                value: v,
                source: self.name().to_string(),
            })),
            // Either VarError::NotPresent, VarError::NotUnicode, or
            // an empty string — all mean "no value here, move on".
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_for_uppercases_and_swaps_dashes() {
        assert_eq!(EnvBackend::env_var_for("openai-api-key"), "OPENAI_API_KEY");
        assert_eq!(EnvBackend::env_var_for("linear_api_key"), "LINEAR_API_KEY");
        assert_eq!(EnvBackend::env_var_for("Mixed-Case_key"), "MIXED_CASE_KEY");
    }

    #[tokio::test]
    async fn returns_value_when_env_set() {
        // Use a key unlikely to collide with anything real in CI.
        let key = "evy-secrets-env-test-A";
        let env_key = EnvBackend::env_var_for(key);
        // SAFETY (test-only): we own this var name and remove it after.
        unsafe { std::env::set_var(&env_key, "hello-world") };
        let backend = EnvBackend::new();
        let got = backend.resolve(key).await.expect("resolve ok");
        unsafe { std::env::remove_var(&env_key) };
        let got = got.expect("some");
        assert_eq!(got.value, "hello-world");
        assert_eq!(got.source, "env");
    }

    #[tokio::test]
    async fn returns_none_when_env_absent() {
        let key = "evy-secrets-env-test-MISSING-xyz";
        // Ensure it really isn't set — we never set it.
        let backend = EnvBackend::new();
        let got = backend.resolve(key).await.expect("resolve ok");
        assert!(got.is_none(), "expected None, got {got:?}");
    }

    #[tokio::test]
    async fn empty_env_var_is_treated_as_none() {
        let key = "evy-secrets-env-test-EMPTY";
        let env_key = EnvBackend::env_var_for(key);
        unsafe { std::env::set_var(&env_key, "") };
        let backend = EnvBackend::new();
        let got = backend.resolve(key).await.expect("resolve ok");
        unsafe { std::env::remove_var(&env_key) };
        assert!(got.is_none(), "blank env var must be None, got {got:?}");
    }

    #[test]
    fn name_is_stable() {
        assert_eq!(EnvBackend::new().name(), "env");
    }
}
