//! OAuth device-flow + refresh primitives for Codex (OpenAI) and xAI/Grok
//! subscriptions, ported from v3 (`components/evy/codex-oauth.ts`,
//! `xai-oauth.ts`).
//!
//! # Why this module exists
//!
//! Codex (ChatGPT Pro) and xAI/Grok are both *subscription-OAuth* providers:
//! a long-lived refresh token rotates a short-lived access token (typically
//! 5–10 minutes for xAI, ~10 days for Codex JWTs). The v4 daemon must be
//! able to:
//!
//! 1. Mint fresh credentials via OAuth — for Codex that's a **device code**
//!    flow against `auth.openai.com`. For xAI that's a **PKCE-loopback**
//!    flow (initial login is deferred to a follow-up slice — see
//!    [`xai::XaiOauth::login`]).
//! 2. Refresh on near-expiry without operator intervention, deduplicated
//!    across concurrent worker dispatches via [`RefreshDedup`] so two
//!    in-flight refreshes don't burn each other's rotated refresh_token.
//! 3. Persist tokens to disk in the *same auth.json shape* v3 writes, so a
//!    rollback (or sidecar v3 CLI access) keeps working without migration.
//!
//! # Trait scope
//!
//! [`OauthFlow`] models the device-flow shape Codex uses. xAI is **not**
//! device-flow — it uses PKCE-loopback — so [`xai::XaiOauth`] is its own
//! type with its own surface (`discover`, `refresh`, eventual `login`).
//! See the worker report for why the spec's "uniform trait" doesn't fit
//! both providers cleanly.
//!
//! # Error mapping
//!
//! Internal failures use [`OauthError`] (thiserror); the trait surface
//! returns workspace [`evy_core::Result`] via a `From` impl. Codex errors
//! land in `Error::Provider { kind: ProviderKind::Codex, ... }`; xAI errors
//! land in `Error::WorkerFailed` for now (adding `ProviderKind::Xai` is a
//! follow-up that touches `evy-core`, which is out of scope for this slice).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use evy_core::{Error, ProviderKind, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

pub mod accounts;
pub mod codex;
pub mod xai;

pub use accounts::{AccountRecord, AccountRow, AccountsStore, TokenRecord};
pub use codex::CodexOauth;
pub use xai::XaiOauth;

// ─── shared types ───────────────────────────────────────────────────────────

/// An access token plus the wall-clock instant at which the provider says it
/// expires. Callers compare `expires_at` against `Utc::now()` to decide
/// whether to refresh; the `expires_at` field is what an OAuth `expires_in`
/// seconds-from-now response translates to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccessToken {
    /// The bearer token. JWT for Codex (audience: api.openai.com), opaque
    /// for xAI (we don't decode it).
    pub token: String,
    /// Wall-clock expiry as derived from `expires_in` at mint time. For
    /// Codex we also have the JWT `exp` claim available — see
    /// [`codex::decode_jwt_exp`]; the two should agree.
    pub expires_at: DateTime<Utc>,
}

/// The provider's response to the "start a device flow" request. Surfaces
/// the operator-facing affordances (the URL to open, the short code to
/// type) plus the polling parameters the caller drives the second step
/// with.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceCodeResponse {
    /// The opaque handle the caller hands back to `poll_for_token` —
    /// **never shown to the operator**. For Codex this is the
    /// `device_auth_id`; we keep the spec's `device_code` field name for
    /// trait compatibility.
    pub device_code: String,
    /// Short code the operator types into the verification page.
    pub user_code: String,
    /// URL the operator opens in their browser.
    pub verification_uri: String,
    /// How long the device-code prompt is valid (seconds). Mirrors the
    /// OAuth device-flow `expires_in`.
    pub expires_in: u64,
    /// Minimum polling interval (seconds) per RFC 8628 §3.5. Callers SHOULD
    /// honor server-supplied bumps (`slow_down`) on top of this floor.
    pub interval: u64,
}

/// Flow-agnostic device-flow surface. Codex implements this fully; xAI does
/// **not** implement it — see [`xai::XaiOauth`] which is loopback-PKCE.
#[async_trait]
pub trait OauthFlow: Send + Sync {
    /// Step 1 — ask the provider for a device code + user code. Returns the
    /// polling parameters in [`DeviceCodeResponse`].
    async fn start_device_flow(&self) -> Result<DeviceCodeResponse>;

    /// Step 2 — poll the provider's token endpoint until the operator
    /// completes the verification page, then exchange for tokens. The
    /// returned [`AccessToken`] is what the caller hands to API requests.
    /// The *refresh_token* is returned separately via the impl's storage
    /// surface (Codex writes it into auth.json).
    async fn poll_for_token(&self, device_code: &str) -> Result<AccessToken>;

    /// Step 3 — refresh an access token using a stored refresh_token. The
    /// new refresh_token (if rotated) is returned alongside the new access
    /// token; persisting it is the caller's job.
    async fn refresh(&self, refresh_token: &str) -> Result<AccessToken>;
}

// ─── error taxonomy ─────────────────────────────────────────────────────────

/// OAuth-specific failure modes. Converted into the workspace [`Error`]
/// shape via `From<OauthError> for evy_core::Error`.
#[derive(Debug, thiserror::Error)]
pub enum OauthError {
    /// The provider's HTTP endpoint returned non-2xx. `status` is the HTTP
    /// status; `body` is the (sanitized, truncated) response body.
    #[error("{provider} oauth http {status}: {body}")]
    Http {
        /// Which provider's endpoint failed.
        provider: &'static str,
        /// HTTP status code.
        status: u16,
        /// Response body, sanitized of control chars + truncated.
        body: String,
    },

    /// The response JSON was missing a required field (e.g. `access_token`).
    #[error("{provider} oauth response invalid: {reason}")]
    InvalidResponse {
        /// Which provider.
        provider: &'static str,
        /// Why the response was rejected.
        reason: String,
    },

    /// Polling exceeded the device-code lifetime.
    #[error("{provider} oauth device code timed out")]
    DeviceCodeTimeout {
        /// Which provider.
        provider: &'static str,
    },

    /// The xAI OIDC discovery endpoint or token endpoint failed host-pin
    /// validation. See [`xai::validate_xai_endpoint`].
    #[error("xai oidc endpoint host pin failed: {0}")]
    HostPin(String),

    /// Filesystem error (auth.json read/write, accounts.conf access).
    #[error("oauth io: {0}")]
    Io(#[from] std::io::Error),

    /// JSON parse failure on disk-cached or on-wire payloads.
    #[error("oauth json: {0}")]
    Json(#[from] serde_json::Error),

    /// Reqwest transport failure (DNS, TCP, TLS).
    #[error("oauth transport: {0}")]
    Transport(#[from] reqwest::Error),

    /// Caller asked for a flow this provider doesn't implement (e.g.
    /// `start_device_flow` on xAI). Distinguished from the generic
    /// `InvalidResponse` so callers can pivot to the right flow.
    #[error("{provider} oauth flow unsupported: {reason}")]
    Unsupported {
        /// Which provider.
        provider: &'static str,
        /// Hint to the caller (e.g. "use loopback login").
        reason: String,
    },
}

impl From<OauthError> for Error {
    fn from(e: OauthError) -> Self {
        // Codex maps cleanly into `Error::Provider { kind: Codex, .. }`.
        // xAI doesn't have a `ProviderKind` variant yet (out of scope for
        // this slice; adding it touches `evy-core`); xAI errors land in
        // `WorkerFailed` with the "xai oauth: …" prefix so log greps catch
        // them either way. TODO(phase4-followup): add ProviderKind::Xai.
        let msg = e.to_string();
        if msg.contains("codex oauth") {
            Error::Provider {
                kind: ProviderKind::Codex,
                reason: msg,
            }
        } else {
            Error::WorkerFailed(msg)
        }
    }
}

// ─── refresh dedup ──────────────────────────────────────────────────────────

/// In-flight refresh deduplicator. When two workers spawn within the same
/// near-expiry window, they MUST NOT both fire `refresh()` independently —
/// OAuth refresh rotates the refresh_token, so the second call invalidates
/// the first's rotated token and one of the two workers ends up with a
/// stale refresh.
///
/// Keyed by the *storage path* of the auth.json (NOT by alias) so that two
/// aliases sharing a `config_dir` (degenerate but possible) collapse to the
/// same lock.
///
/// Use [`RefreshDedup::get_or_acquire`] to obtain the slot; the returned
/// `Arc<Mutex<()>>` lets the caller hold the lock across the entire
/// read-modify-write of auth.json.
#[derive(Debug, Default, Clone)]
pub struct RefreshDedup {
    inner: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>>,
}

impl RefreshDedup {
    /// Construct an empty dedup table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (or insert) the per-path mutex. Subsequent calls for the same
    /// path return the same `Arc<Mutex<()>>`, so all callers serialize on
    /// the same critical section.
    pub async fn slot(&self, path: PathBuf) -> Arc<Mutex<()>> {
        let mut tbl = self.inner.lock().await;
        tbl.entry(path)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

// ─── shared HTTP plumbing ───────────────────────────────────────────────────

/// Sanitize a response body for logging: strip ANSI/control chars and
/// truncate to 512 bytes. Mirrors v3's `sanitizeErrorText` minus the more
/// elaborate regex set (we don't need OSC8 unwrap for non-terminal logs).
pub(crate) fn sanitize_body(s: &str) -> String {
    let mut out: String = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();
    out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.len() > 512 {
        out.truncate(509);
        out.push_str("...");
    }
    out
}

/// User-Agent + `originator` identifier sent on every OAuth HTTP call so
/// subctl shows up cleanly in provider server logs (rather than
/// impersonating curl or another tool's OAuth client).
pub(crate) fn user_agent() -> String {
    format!("subctl-rust/{}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_control_chars() {
        let s = "hello\x1b[31mred\x1b[0m\x07world";
        let got = sanitize_body(s);
        assert!(!got.contains('\x1b'), "{}", got);
        assert!(!got.contains('\x07'), "{}", got);
    }

    #[test]
    fn sanitize_truncates_long_bodies() {
        let s = "a".repeat(2000);
        let got = sanitize_body(&s);
        assert!(got.len() <= 512);
        assert!(got.ends_with("..."));
    }

    #[tokio::test]
    async fn refresh_dedup_returns_same_arc_for_same_path() {
        let dedup = RefreshDedup::new();
        let a = dedup.slot(PathBuf::from("/tmp/a")).await;
        let b = dedup.slot(PathBuf::from("/tmp/a")).await;
        let c = dedup.slot(PathBuf::from("/tmp/b")).await;
        assert!(Arc::ptr_eq(&a, &b), "same path → same arc");
        assert!(!Arc::ptr_eq(&a, &c), "different path → different arc");
    }

    #[test]
    fn oauth_error_codex_maps_to_provider_error() {
        let e = OauthError::InvalidResponse {
            provider: "codex",
            reason: "no access_token".into(),
        };
        let mapped: Error = e.into();
        assert!(matches!(
            mapped,
            Error::Provider {
                kind: ProviderKind::Codex,
                ..
            }
        ));
    }

    #[test]
    fn oauth_error_xai_maps_to_worker_failed() {
        let e = OauthError::HostPin("not on x.ai".into());
        let mapped: Error = e.into();
        assert!(matches!(mapped, Error::WorkerFailed(_)));
    }
}
