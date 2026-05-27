//! 1Password Service Account backend.
//!
//! Shells out to the `op` CLI (1Password's official command-line tool)
//! with `OP_SERVICE_ACCOUNT_TOKEN` set in the child environment. We
//! deliberately use the CLI rather than the Connect SDK so this works
//! against any vault the operator's Service Account has read access
//! to, with no extra Rust dep tree.
//!
//! ### Reference shape
//!
//! 1Password references take the form `op://<vault>/<item>/<field>`.
//! This backend builds `op://<vault>/<key>` — the caller's `key` may
//! be a bare item name (in which case `op` returns its default field)
//! OR an `<item>/<field>` slash-path. This keeps the v4 surface
//! minimal: no per-key field map, no `secrets-backends.json` config
//! layer. Callers who want a specific field encode it in the key
//! itself, e.g. `secret("openai/api-key")`.
//!
//! ### Failure modes (silent-no-op)
//!
//! Following v3's `secrets-backends.ts`, this backend tries hard to
//! return `Ok(None)` rather than an error:
//!
//! - `op` not on PATH (`ErrorKind::NotFound` on spawn) → `Ok(None)`
//! - `op` exits non-zero (ref missing, network blip, token invalid) →
//!   `Ok(None)`
//! - `op` exits 0 with empty stdout → `Ok(None)`
//!
//! The one case that DOES error is a spawn failure with a non-`NotFound`
//! [`std::io::ErrorKind`] (permission denied, fd exhaustion, etc.) —
//! that's not "1Password isn't configured", that's the daemon itself
//! is sick and the operator needs to see it.
//!
//! ### Security
//!
//! - The Service Account token is passed via the child env, not argv.
//!   It will not appear in `ps`. The token is held in this struct as
//!   a `String`; callers should ensure the struct itself doesn't get
//!   dumped to logs (the `Debug` impl below redacts it).
//! - `op`'s stdout is treated as the secret value with a single
//!   trailing newline trim; nothing else is parsed.
//! - We never log the resolved value or the token. We do log the
//!   built `op://...` reference at `tracing::debug` — it's a name,
//!   not a credential.

use std::fmt;

use async_trait::async_trait;
use tokio::process::Command;
use tracing::debug;

use crate::{Result, SecretValue, SecretsBackend, SecretsError};

/// Resolves secrets via the `op` CLI using a 1Password Service Account.
pub struct OnePasswordBackend {
    service_account_token: String,
    vault: String,
}

// Custom Debug so the token never lands in a log line via a
// `tracing::debug!(?backend, ...)` callsite somewhere in the daemon.
impl fmt::Debug for OnePasswordBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnePasswordBackend")
            .field("vault", &self.vault)
            .field("service_account_token", &"<redacted>")
            .finish()
    }
}

impl OnePasswordBackend {
    /// Construct a backend bound to a single vault, authenticated with
    /// the supplied Service Account token. Both arguments are taken
    /// by value — the caller decides how to source them (env, config
    /// file, secrets manager, …).
    #[must_use]
    pub fn new(service_account_token: String, vault: String) -> Self {
        Self {
            service_account_token,
            vault,
        }
    }

    /// The vault this backend targets. The token is intentionally not
    /// exposed via an accessor.
    #[must_use]
    pub fn vault(&self) -> &str {
        &self.vault
    }

    /// Build the `op://...` reference we'll ask `op read` for. Pulled
    /// out as a method so the integration test can assert exact wire
    /// shape without re-spawning a child process.
    #[must_use]
    pub fn build_reference(&self, key: &str) -> String {
        // Trim a leading slash so `OnePasswordBackend::build_reference("/x")`
        // and `build_reference("x")` produce the same ref.
        let key = key.trim_start_matches('/');
        format!("op://{}/{}", self.vault, key)
    }
}

#[async_trait]
impl SecretsBackend for OnePasswordBackend {
    fn name(&self) -> &str {
        "onepassword"
    }

    async fn resolve(&self, key: &str) -> Result<Option<SecretValue>> {
        let reference = self.build_reference(key);
        debug!(target: "secrets", key, reference, "op read");

        let mut cmd = Command::new("op");
        // We do NOT `env_clear()` — `op` may legitimately need PATH,
        // HOME, and the various network proxy env vars to talk to
        // 1Password. We override the SA token explicitly so a stale
        // value in the parent's env can't shadow the configured one.
        cmd.arg("read")
            .arg(&reference)
            .env("OP_SERVICE_ACCOUNT_TOKEN", &self.service_account_token)
            .stdin(std::process::Stdio::null());

        let output = match cmd.output().await {
            Ok(o) => o,
            // `op` missing from PATH is the "1Password backend isn't
            // wired up on this host" case — surface as Ok(None) and
            // let the next backend in the chain try.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(SecretsError::OpSpawn(source)),
        };

        if !output.status.success() {
            // Anything from "ref not found" to "token revoked" — we
            // can't reliably distinguish without parsing op's stderr,
            // and we'd rather degrade gracefully than crash the
            // resolver. Match v3 semantics: try the next backend.
            return Ok(None);
        }

        // `op read` emits the value followed by a single newline.
        // Trim exactly one trailing `\n` (not all whitespace — a
        // trailing space inside the secret would be meaningful).
        let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if stdout.ends_with('\n') {
            stdout.pop();
            if stdout.ends_with('\r') {
                stdout.pop();
            }
        }
        if stdout.is_empty() {
            return Ok(None);
        }
        Ok(Some(SecretValue {
            value: stdout,
            source: self.name().to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_token() {
        let b = OnePasswordBackend::new("super-secret-token".into(), "Personal".into());
        let s = format!("{b:?}");
        assert!(!s.contains("super-secret-token"), "got: {s}");
        assert!(s.contains("<redacted>"), "got: {s}");
        assert!(s.contains("Personal"), "got: {s}");
    }

    #[test]
    fn build_reference_uses_vault_and_key() {
        let b = OnePasswordBackend::new("tok".into(), "Personal".into());
        assert_eq!(
            b.build_reference("openai/api-key"),
            "op://Personal/openai/api-key"
        );
    }

    #[test]
    fn build_reference_strips_leading_slash() {
        let b = OnePasswordBackend::new("tok".into(), "Personal".into());
        assert_eq!(
            b.build_reference("/openai/api-key"),
            "op://Personal/openai/api-key"
        );
    }

    #[test]
    fn build_reference_with_bare_item() {
        let b = OnePasswordBackend::new("tok".into(), "Engineering".into());
        assert_eq!(b.build_reference("openai"), "op://Engineering/openai");
    }

    #[test]
    fn name_is_stable() {
        assert_eq!(
            OnePasswordBackend::new("tok".into(), "v".into()).name(),
            "onepassword"
        );
    }

    // NB: `missing_op_cli_is_none_not_error` lives in
    // `tests/integration.rs` so it can share the `env_lock()` mutex
    // that serializes the `PATH` / `OP_SERVICE_ACCOUNT_TOKEN`
    // mutations. Putting it here would make this test binary unsafe
    // to extend with any other PATH-reading test.
}
