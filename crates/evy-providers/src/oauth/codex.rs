//! Codex (OpenAI ChatGPT Pro) OAuth — device-code flow + refresh.
//!
//! Port of v3 `components/evy/codex-oauth.ts`. The wire protocol mirrors
//! the official Codex CLI's device-code path against `auth.openai.com`,
//! using OpenAI's public Codex CLI client id
//! (`app_EMoamEEZ73f0CkXaXp7hrann`). Subctl identifies itself via the
//! `originator: subctl` header + `User-Agent: subctl-rust/<ver>` rather
//! than impersonating another tool.
//!
//! # Why we re-implement (vs. shell out to `codex login`)
//!
//! The v4 daemon needs to mint tokens for *named accounts* (alias →
//! config_dir → auth.json), and needs to refresh in-place without
//! re-prompting the operator. The Codex CLI doesn't expose those affordances
//! programmatically; v3 already ported the wire protocol and v4 inherits it.
//!
//! # Endpoints
//!
//! - `POST /api/accounts/deviceauth/usercode`  — request device code
//! - `POST /api/accounts/deviceauth/token`     — poll for completion
//! - `POST /oauth/token`                       — refresh / final exchange

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use evy_core::Result;
use serde::Deserialize;

use super::{sanitize_body, user_agent, AccessToken, DeviceCodeResponse, OauthError, OauthFlow};

/// OpenAI auth base URL. Public so tests can rebind it via the
/// `with_base_url` builder.
pub const OPENAI_AUTH_BASE_URL: &str = "https://auth.openai.com";

/// OpenAI's public Codex CLI client id. Used unchanged from v3 (matches
/// what `codex login` and v3's `codex-oauth.ts` send). Subctl identifies
/// itself via headers rather than a distinct client id.
pub const OPENAI_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Redirect URI baked into the device-flow exchange. Matches v3's
/// `OPENAI_CODEX_DEVICE_CALLBACK_URL`.
pub const OPENAI_CODEX_DEVICE_CALLBACK_URL: &str = "https://auth.openai.com/deviceauth/callback";

/// URL shown to the operator on the prompt screen (not the API target).
pub const OPENAI_CODEX_DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";

/// Device-code prompt lifetime, in seconds. 15 minutes mirrors v3.
pub const DEVICE_CODE_TIMEOUT_SECS: u64 = 15 * 60;

/// Default polling interval if the server doesn't supply one.
pub const DEVICE_CODE_DEFAULT_INTERVAL_SECS: u64 = 5;

/// Refresh skew — refresh when the token is within this many seconds of
/// expiring. Matches v3's `REFRESH_SKEW_SECONDS = 300`.
pub const REFRESH_SKEW_SECONDS: i64 = 300;

/// Codex OAuth client. Holds the base URL + client id + a reqwest client.
/// One construction per process is fine — the http client is internally
/// `Arc`'d.
#[derive(Debug, Clone)]
pub struct CodexOauth {
    base_url: String,
    client_id: String,
    http: reqwest::Client,
}

impl CodexOauth {
    /// Construct with the default OpenAI base URL.
    pub fn new(client_id: String) -> Self {
        Self::with_base_url(client_id, OPENAI_AUTH_BASE_URL.to_string())
    }

    /// Construct with an overridden base URL (used by tests to point at a
    /// wiremock server).
    pub fn with_base_url(client_id: String, base_url: String) -> Self {
        let http = reqwest::Client::builder()
            // Generous default — the device-code endpoint can hang while
            // the operator finishes the prompt; per-call timeouts are
            // applied to the polling loop separately.
            .timeout(Duration::from_secs(30))
            .user_agent(user_agent())
            .build()
            .expect("reqwest client build cannot fail with these options");
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client_id,
            http,
        }
    }

    /// Manual exchange of an authorization_code + code_verifier (used by
    /// the device-flow `poll_for_token` after a successful poll).
    async fn exchange(
        &self,
        authorization_code: &str,
        code_verifier: &str,
    ) -> std::result::Result<TokenExchangeResponse, OauthError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", authorization_code),
            ("redirect_uri", OPENAI_CODEX_DEVICE_CALLBACK_URL),
            ("client_id", &self.client_id),
            ("code_verifier", code_verifier),
        ];
        let resp = self
            .http
            .post(format!("{}/oauth/token", self.base_url))
            .header("originator", "subctl")
            .form(&form)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(OauthError::Http {
                provider: "codex",
                status: status.as_u16(),
                body: sanitize_body(&body),
            });
        }
        let parsed: TokenExchangeResponse =
            serde_json::from_str(&body).map_err(|e| OauthError::InvalidResponse {
                provider: "codex",
                reason: format!("token exchange JSON parse failed: {e}"),
            })?;
        if parsed.access_token.is_empty() {
            return Err(OauthError::InvalidResponse {
                provider: "codex",
                reason: "token exchange returned empty access_token".into(),
            });
        }
        Ok(parsed)
    }
}

#[async_trait]
impl OauthFlow for CodexOauth {
    async fn start_device_flow(&self) -> Result<DeviceCodeResponse> {
        let body = serde_json::json!({ "client_id": self.client_id });
        let resp = self
            .http
            .post(format!(
                "{}/api/accounts/deviceauth/usercode",
                self.base_url
            ))
            .header("originator", "subctl")
            .json(&body)
            .send()
            .await
            .map_err(OauthError::from)?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            // v3 emits a special hint on 404 (server hasn't enabled
            // device-code for this client) — surface the same string so
            // operator log greps interop.
            if status.as_u16() == 404 {
                return Err(OauthError::Http {
                    provider: "codex",
                    status: 404,
                    body: "device code login not enabled — use ChatGPT OAuth fallback".into(),
                }
                .into());
            }
            return Err(OauthError::Http {
                provider: "codex",
                status: status.as_u16(),
                body: sanitize_body(&text),
            }
            .into());
        }
        let resp: UserCodeResponse =
            serde_json::from_str(&text).map_err(|e| OauthError::InvalidResponse {
                provider: "codex",
                reason: format!("usercode JSON parse failed: {e}"),
            })?;
        let device_auth_id = resp
            .device_auth_id
            .ok_or_else(|| OauthError::InvalidResponse {
                provider: "codex",
                reason: "usercode response missing device_auth_id".into(),
            })?;
        let user_code =
            resp.user_code
                .or(resp.usercode)
                .ok_or_else(|| OauthError::InvalidResponse {
                    provider: "codex",
                    reason: "usercode response missing user_code".into(),
                })?;
        let interval = resp.interval.unwrap_or(DEVICE_CODE_DEFAULT_INTERVAL_SECS);
        Ok(DeviceCodeResponse {
            device_code: device_auth_id,
            user_code,
            verification_uri: OPENAI_CODEX_DEVICE_VERIFICATION_URL.to_string(),
            expires_in: DEVICE_CODE_TIMEOUT_SECS,
            interval,
        })
    }

    async fn poll_for_token(&self, device_code: &str) -> Result<AccessToken> {
        // v3 polls /api/accounts/deviceauth/token until it gets a 200 with
        // authorization_code + code_verifier, then POSTs /oauth/token to
        // exchange them for tokens. We mirror that two-step here.
        let deadline = std::time::Instant::now() + Duration::from_secs(DEVICE_CODE_TIMEOUT_SECS);
        let interval = Duration::from_secs(DEVICE_CODE_DEFAULT_INTERVAL_SECS);
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(OauthError::DeviceCodeTimeout { provider: "codex" }.into());
            }
            let body = serde_json::json!({
                "device_auth_id": device_code,
                "user_code": "", // server keys off device_auth_id; user_code is ignored on this hop
            });
            let resp = self
                .http
                .post(format!("{}/api/accounts/deviceauth/token", self.base_url))
                .header("originator", "subctl")
                .json(&body)
                .send()
                .await
                .map_err(OauthError::from)?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                let parsed: DeviceAuthResponse =
                    serde_json::from_str(&text).map_err(|e| OauthError::InvalidResponse {
                        provider: "codex",
                        reason: format!("deviceauth JSON parse failed: {e}"),
                    })?;
                let auth_code =
                    parsed
                        .authorization_code
                        .ok_or_else(|| OauthError::InvalidResponse {
                            provider: "codex",
                            reason: "deviceauth response missing authorization_code".into(),
                        })?;
                let verifier = parsed
                    .code_verifier
                    .ok_or_else(|| OauthError::InvalidResponse {
                        provider: "codex",
                        reason: "deviceauth response missing code_verifier".into(),
                    })?;
                let tokens = self.exchange(&auth_code, &verifier).await?;
                let expires_at = Utc::now()
                    + chrono::Duration::seconds(tokens.expires_in.unwrap_or(3600) as i64);
                return Ok(AccessToken {
                    token: tokens.access_token,
                    expires_at,
                });
            }
            // 403/404 = operator hasn't completed verification yet; keep polling.
            if status.as_u16() == 403 || status.as_u16() == 404 {
                tokio::time::sleep(interval).await;
                continue;
            }
            // Any other status is terminal.
            return Err(OauthError::Http {
                provider: "codex",
                status: status.as_u16(),
                body: sanitize_body(&text),
            }
            .into());
        }
    }

    async fn refresh(&self, refresh_token: &str) -> Result<AccessToken> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &self.client_id),
        ];
        let resp = self
            .http
            .post(format!("{}/oauth/token", self.base_url))
            .header("originator", "subctl")
            .form(&form)
            .send()
            .await
            .map_err(OauthError::from)?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(OauthError::Http {
                provider: "codex",
                status: status.as_u16(),
                body: sanitize_body(&body),
            }
            .into());
        }
        let parsed: TokenExchangeResponse =
            serde_json::from_str(&body).map_err(|e| OauthError::InvalidResponse {
                provider: "codex",
                reason: format!("refresh JSON parse failed: {e}"),
            })?;
        if parsed.access_token.is_empty() {
            return Err(OauthError::InvalidResponse {
                provider: "codex",
                reason: "refresh returned empty access_token".into(),
            }
            .into());
        }
        let expires_at =
            Utc::now() + chrono::Duration::seconds(parsed.expires_in.unwrap_or(3600) as i64);
        Ok(AccessToken {
            token: parsed.access_token,
            expires_at,
        })
    }
}

// ─── JWT helpers ────────────────────────────────────────────────────────────

/// Decode the `exp` claim from a JWT. Returns `None` on any decoding
/// failure — caller should treat absence as "don't speculatively refresh"
/// per v3 semantics.
pub fn decode_jwt_exp(token: &str) -> Option<i64> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    // Pad to multiple of 4 for base64 decode.
    let mut payload_b64 = parts[1].to_string();
    while !payload_b64.len().is_multiple_of(4) {
        payload_b64.push('=');
    }
    // Try URL_SAFE first (jose-style), fall back to standard base64.
    let bytes = URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(&payload_b64))
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    json.get("exp").and_then(|v| v.as_i64())
}

// ─── wire-shape DTOs ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct UserCodeResponse {
    device_auth_id: Option<String>,
    user_code: Option<String>,
    usercode: Option<String>, // v3 falls back to this variant
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DeviceAuthResponse {
    authorization_code: Option<String>,
    code_verifier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenExchangeResponse {
    access_token: String,
    #[allow(dead_code)]
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_exp_decode_returns_value() {
        // JWT header.payload.sig where payload = {"exp": 9999999999}
        // Header = {"alg":"none","typ":"JWT"} base64url -> eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0
        // Payload = {"exp":9999999999} -> eyJleHAiOjk5OTk5OTk5OTl9
        let token = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJleHAiOjk5OTk5OTk5OTl9.sig";
        assert_eq!(decode_jwt_exp(token), Some(9_999_999_999));
    }

    #[test]
    fn jwt_exp_decode_returns_none_on_malformed() {
        assert!(decode_jwt_exp("not.a.jwt.value").is_none());
        assert!(decode_jwt_exp("only-one-segment").is_none());
        assert!(decode_jwt_exp("aa.bb.cc").is_none()); // payload isn't valid b64 JSON
    }
}
