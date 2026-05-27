//! xAI / Grok OAuth — discovery, refresh, host-pin validation.
//!
//! Port of v3 `components/evy/xai-oauth.ts`. xAI uses **PKCE-loopback**,
//! NOT device-code, so `XaiOauth` is its own type and does **not**
//! implement [`OauthFlow`]. Initial login (the loopback HTTP callback
//! server) is **deferred to a follow-up slice** — see [`XaiOauth::login`].
//! Refresh, discovery, and host-pin validation are all functional.
//!
//! ## Host-pin (DO NOT REMOVE)
//!
//! A single MITM during OIDC discovery could substitute a malicious
//! `token_endpoint`; that URL would then receive the refresh_token on
//! every refresh — turning a one-time MITM into a permanent credential
//! leak. We refuse to use any endpoint that isn't HTTPS on the xAI origin
//! (`x.ai` or `*.x.ai`). RFC 8414 §2 already requires HTTPS; the host
//! check is the additional defense v3 already enforces in
//! `_xai_validate_oauth_endpoint`.

use std::time::Duration;

use chrono::Utc;
use evy_core::Result;
use serde::Deserialize;
use url::Url;

use super::{sanitize_body, user_agent, AccessToken, OauthError};

/// xAI's OAuth issuer (the `iss` claim in the OIDC discovery document).
pub const XAI_OAUTH_ISSUER: &str = "https://auth.x.ai";

/// OIDC discovery URL.
pub const XAI_OAUTH_DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";

/// xAI's public Grok-CLI OAuth client id. xAI has not minted per-tool
/// client ids; we use the same one Hermes/v3 use, and identify subctl via
/// `referrer=subctl` on the authorize URL.
pub const XAI_OAUTH_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";

/// Refresh skew. xAI rotates more aggressively than Codex (5–10min access
/// token vs. ~10 days), so the v3 default is 120s.
pub const XAI_REFRESH_SKEW_SECONDS: i64 = 120;

/// xAI OAuth client. Holds the (optional, lazily-discovered) token endpoint
/// and a reqwest client.
#[derive(Debug, Clone)]
pub struct XaiOauth {
    discovery_url: String,
    client_id: String,
    http: reqwest::Client,
}

impl XaiOauth {
    /// Construct with the default xAI discovery URL.
    pub fn new(client_id: String) -> Self {
        Self::with_discovery_url(client_id, XAI_OAUTH_DISCOVERY_URL.to_string())
    }

    /// Construct with an overridden discovery URL (used by tests to point
    /// at a wiremock server). The wiremock URL is checked against the
    /// host-pin during normal `discover()` calls — tests that need to
    /// bypass it use [`XaiOauth::with_token_endpoint`] instead.
    pub fn with_discovery_url(client_id: String, discovery_url: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(user_agent())
            .build()
            .expect("reqwest client build cannot fail");
        Self {
            discovery_url,
            client_id,
            http,
        }
    }

    /// Fetch + validate the OIDC discovery document.
    pub async fn discover(&self) -> Result<XaiDiscovery> {
        let resp = self
            .http
            .get(&self.discovery_url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(OauthError::from)?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(OauthError::Http {
                provider: "xai",
                status: status.as_u16(),
                body: sanitize_body(&text),
            }
            .into());
        }
        let parsed: XaiDiscovery =
            serde_json::from_str(&text).map_err(|e| OauthError::InvalidResponse {
                provider: "xai",
                reason: format!("discovery JSON parse failed: {e}"),
            })?;
        validate_xai_endpoint(&parsed.authorization_endpoint, "authorization_endpoint")?;
        validate_xai_endpoint(&parsed.token_endpoint, "token_endpoint")?;
        Ok(parsed)
    }

    /// Refresh an access token. Re-discovers if `token_endpoint` is not
    /// passed; otherwise revalidates the supplied endpoint against the
    /// host-pin (defense against a cached-on-disk MITM substitution).
    pub async fn refresh_with_endpoint(
        &self,
        refresh_token: &str,
        token_endpoint: Option<&str>,
    ) -> Result<AccessToken> {
        let endpoint = match token_endpoint {
            Some(e) => {
                validate_xai_endpoint(e, "token_endpoint")?;
                e.to_string()
            }
            None => self.discover().await?.token_endpoint,
        };
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", &self.client_id),
            ("refresh_token", refresh_token),
        ];
        let resp = self
            .http
            .post(&endpoint)
            .header("Accept", "application/json")
            .form(&form)
            .send()
            .await
            .map_err(OauthError::from)?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(OauthError::Http {
                provider: "xai",
                status: status.as_u16(),
                body: sanitize_body(&text),
            }
            .into());
        }
        let parsed: XaiRefreshResponse =
            serde_json::from_str(&text).map_err(|e| OauthError::InvalidResponse {
                provider: "xai",
                reason: format!("refresh JSON parse failed: {e}"),
            })?;
        if parsed.access_token.is_empty() {
            return Err(OauthError::InvalidResponse {
                provider: "xai",
                reason: "refresh returned empty access_token".into(),
            }
            .into());
        }
        let expires_at =
            Utc::now() + chrono::Duration::seconds(parsed.expires_in.unwrap_or(600) as i64);
        Ok(AccessToken {
            token: parsed.access_token,
            expires_at,
        })
    }

    /// Convenience wrapper — refresh, re-discovering each time.
    pub async fn refresh(&self, refresh_token: &str) -> Result<AccessToken> {
        self.refresh_with_endpoint(refresh_token, None).await
    }

    /// Initial PKCE-loopback login. **Not implemented in this slice** —
    /// requires an HTTP callback server on 127.0.0.1, port-binding
    /// fallback, CORS handling, and state/nonce/PKCE verifier plumbing
    /// (~400 LOC in v3). Returns a typed `Unsupported` error so callers
    /// can surface "use v3 `subctl auth xai-oauth` until the v4 loopback
    /// server lands" to the operator.
    pub async fn login(&self) -> Result<AccessToken> {
        Err(OauthError::Unsupported {
            provider: "xai",
            reason: "initial PKCE-loopback login not yet ported to v4 — use v3 `subctl auth xai-oauth <alias>` until follow-up slice lands".into(),
        }
        .into())
    }
}

/// Discovery doc payload. Only the two endpoints we hot-path are deserialized;
/// `flatten` could capture the rest if a future caller needs them.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct XaiDiscovery {
    /// URL the operator opens in their browser to authorize.
    pub authorization_endpoint: String,
    /// URL we POST token-exchange + refresh requests to.
    pub token_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct XaiRefreshResponse {
    access_token: String,
    #[allow(dead_code)]
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[allow(dead_code)]
    #[serde(default)]
    token_type: Option<String>,
}

/// Refuse any endpoint that isn't HTTPS on `x.ai` or `*.x.ai`. Mirror of
/// v3's `validateXaiOauthEndpoint` (and Hermes's
/// `_xai_validate_oauth_endpoint` before that).
///
/// MUST be called on every endpoint pulled from discovery AND every cached
/// endpoint read from disk before reuse — the rationale is that a MITM
/// during initial discovery substitutes the `token_endpoint`, and any
/// future refresh sends the long-lived refresh_token to that hostile
/// endpoint. The host-pin breaks that persistence.
pub fn validate_xai_endpoint(url: &str, field: &str) -> std::result::Result<(), OauthError> {
    let parsed = Url::parse(url)
        .map_err(|e| OauthError::HostPin(format!("{field} {url:?} unparseable: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(OauthError::HostPin(format!("{field} {url:?} not https")));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| OauthError::HostPin(format!("{field} {url:?} has no host")))?
        .to_ascii_lowercase();
    if host != "x.ai" && !host.ends_with(".x.ai") {
        return Err(OauthError::HostPin(format!(
            "{field} host {host:?} not on x.ai origin (expected x.ai or *.x.ai)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_pin_accepts_xai_origin() {
        assert!(validate_xai_endpoint("https://auth.x.ai/oauth/token", "token").is_ok());
        assert!(validate_xai_endpoint("https://x.ai/foo", "x").is_ok());
        assert!(validate_xai_endpoint("https://accounts.x.ai/y", "y").is_ok());
    }

    #[test]
    fn host_pin_rejects_non_xai() {
        assert!(validate_xai_endpoint("https://attacker.com/token", "t").is_err());
        assert!(validate_xai_endpoint("https://x.ai.evil.com/y", "y").is_err());
    }

    #[test]
    fn host_pin_rejects_non_https() {
        assert!(validate_xai_endpoint("http://x.ai/token", "t").is_err());
    }

    #[test]
    fn host_pin_rejects_unparseable() {
        assert!(validate_xai_endpoint("not a url", "x").is_err());
    }

    #[tokio::test]
    async fn login_returns_unsupported_until_loopback_lands() {
        let xai = XaiOauth::new("test-client".into());
        let err = xai.login().await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("loopback")
                || msg.contains("Unsupported")
                || msg.contains("not yet ported"),
            "got: {msg}"
        );
    }
}
