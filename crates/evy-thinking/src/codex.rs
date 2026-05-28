//! Codex OAuth-backed [`LlmBackend`] — talks to the OpenAI Responses
//! API at `POST <endpoint>/responses`, using the operator's existing
//! ChatGPT Pro subscription via Codex OAuth tokens (NOT a separate
//! Anthropic API key purchase).
//!
//! ## Why this backend exists
//!
//! subctl's whole point is operating across the operator's existing
//! subscriptions. The Phase 6 Anthropic backend defaulted to a paid
//! Anthropic API key — wrong for an operator who already pays for
//! ChatGPT Pro. This backend reuses Codex tokens minted by Phase 4
//! Slice D's device-flow ([`evy_providers::oauth::codex`]) so chat with
//! v4 Evy "just works" against the same subscription.
//!
//! ## Wire shape — Codex Responses API
//!
//! Request (only the fields we send):
//!
//! ```json
//! {
//!   "model": "gpt-5.5",
//!   "instructions": "<planning_system_prompt(topic)>",
//!   "input": [
//!     {"role": "user",
//!      "content": [{"type": "input_text",  "text": "..."}]},
//!     {"role": "assistant",
//!      "content": [{"type": "output_text", "text": "..."}]}
//!   ],
//!   "store": false,
//!   "max_output_tokens": 4096
//! }
//! ```
//!
//! Required headers (sourced from Hermes's `_codex_cloudflare_headers`
//! — verified 2026-05-27 against `agent/auxiliary_client.py`):
//!
//! | Header              | Value                          |
//! |---------------------|--------------------------------|
//! | `Authorization`     | `Bearer <jwt access_token>`    |
//! | `originator`        | `codex_cli_rs`                 |
//! | `User-Agent`        | `codex_cli_rs/0.0.0 (subctl)`  |
//! | `ChatGPT-Account-ID`| extracted from JWT claim       |
//! | `Content-Type`      | `application/json`             |
//!
//! Cloudflare in front of `chatgpt.com/backend-api/codex` whitelists a
//! small set of first-party originators; non-residential IPs without an
//! allowed originator are 403-challenged regardless of auth correctness.
//! Missing `ChatGPT-Account-ID` 401s on multi-account JWTs.
//!
//! Response (only the fields we read):
//!
//! ```json
//! {
//!   "output": [
//!     {"type": "message", "role": "assistant",
//!      "content": [{"type": "output_text", "text": "..."}]}
//!   ],
//!   "output_text": "..."
//! }
//! ```
//!
//! Walk `output` for `{type: "message"}` items, collect each `content`
//! entry whose `type == "output_text"`. Fall back to the top-level
//! `output_text` string when the array is empty (Responses streaming
//! occasionally drops the structured items).
//!
//! ## Role mapping
//!
//! The Responses API doesn't have a `system` slot — system text rides in
//! the top-level `instructions` field. [`crate::Role::System`] entries
//! are surface scaffolding and are **skipped**. Operator → `user`
//! (content: `input_text`), Partner → `assistant` (content:
//! `output_text`); the Responses API rejects `input_text` on assistant
//! messages and vice versa.
//!
//! ## Skill autoload — Phase 6.1 follow-up
//!
//! The Anthropic backend's `skill_view` tool-use loop translates
//! cleanly to Responses API `function_call` items, but tool-use on the
//! Responses surface also requires handling `reasoning` blocks with
//! `encrypted_content` replay rules (the issuer-pinning is non-trivial,
//! see Hermes's `_chat_messages_to_responses_input`). For this slice we
//! inline [`SkillRegistry::index_for_prompt`] into the instructions
//! and do NOT advertise tools — the model gets the full skill index in
//! system text but cannot autoload bodies. Bumping to LLM-driven
//! autoload via `function_call` is a Phase 6.1 deliverable.
//!
//! ## Refresh discipline
//!
//! Two concurrent `respond()` calls on the same account near expiry
//! would each fire `CodexOauth::refresh()` independently; the second
//! call rotates the refresh_token under the first one's feet and one of
//! the two ends up holding stale credentials. We serialise refresh via
//! [`evy_providers::oauth::RefreshDedup`] keyed on the storage path so
//! both callers share one critical section.
//!
//! Refresh threshold uses [`evy_providers::oauth::codex::REFRESH_SKEW_SECONDS`]
//! (300s) — the same skew window v3 and the device-flow polling use.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use chrono::Utc;
use evy_providers::oauth::codex::{CodexOauth, OPENAI_CODEX_CLIENT_ID, REFRESH_SKEW_SECONDS};
use evy_providers::oauth::{AccountRecord, AccountsStore, OauthFlow, RefreshDedup};
use evy_skills::SkillRegistry;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::backend::LlmBackend;
use crate::error::{Result, ThinkingError};
use crate::session::{Message, Role};

/// Default Codex API base URL. The Responses endpoint is at
/// `<base>/responses` (no `/v1/` prefix — `chatgpt.com/backend-api/codex`
/// is fully-qualified). Public so tests can override to point at a
/// wiremock server.
pub const DEFAULT_CODEX_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex";

/// Default model identifier. Matches the value subctl seeds into a fresh
/// operator's Codex `config.toml` — `gpt-5` is rejected by the Codex
/// account auth path; `gpt-5.5` is the current accepted alias. Operators
/// can override in [`CodexOauthConfig`].
pub const DEFAULT_CODEX_MODEL: &str = "gpt-5.5";

/// Default `max_output_tokens` for partner replies. Generous because a
/// draft plan with all four sections plus the iteration closer is
/// typically 600-1200 output tokens; 4096 leaves headroom for big
/// projects.
pub const DEFAULT_CODEX_MAX_TOKENS: u32 = 4096;

/// Default per-request transport timeout. Mirrors the Anthropic backend
/// — Codex latency varies under load and partial-plan timeouts are worse
/// than just waiting.
pub const DEFAULT_CODEX_TIMEOUT: Duration = Duration::from_secs(60);

/// Default `accounts.conf` path. Matches v3 (`~/.config/subctl/`).
fn default_accounts_conf_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    home.join(".config").join("subctl").join("accounts.conf")
}

/// Static configuration for [`CodexOauthBackend`].
#[derive(Debug, Clone)]
pub struct CodexOauthConfig {
    /// Path to `accounts.conf`. Defaults to
    /// `~/.config/subctl/accounts.conf`.
    pub accounts_conf_path: PathBuf,
    /// Which alias in `accounts.conf` to use (e.g. `openai-jason`). Must
    /// match a row whose `provider` is `openai-codex` and whose
    /// `config_dir/auth.json` holds a valid JWT.
    pub account_name: String,
    /// Model identifier sent in the request body. Defaults to
    /// [`DEFAULT_CODEX_MODEL`].
    pub model: String,
    /// `max_output_tokens` cap sent in the request body. Defaults to
    /// [`DEFAULT_CODEX_MAX_TOKENS`].
    pub max_tokens: u32,
    /// Per-request transport timeout. Defaults to
    /// [`DEFAULT_CODEX_TIMEOUT`].
    pub timeout: Duration,
    /// Codex API base URL. Defaults to [`DEFAULT_CODEX_ENDPOINT`]; tests
    /// override this to point at a `wiremock::MockServer::uri()`.
    pub endpoint: String,
}

impl CodexOauthConfig {
    /// Construct with the supplied account name and production defaults.
    #[must_use]
    pub fn new(account_name: impl Into<String>) -> Self {
        Self {
            accounts_conf_path: default_accounts_conf_path(),
            account_name: account_name.into(),
            model: DEFAULT_CODEX_MODEL.to_string(),
            max_tokens: DEFAULT_CODEX_MAX_TOKENS,
            timeout: DEFAULT_CODEX_TIMEOUT,
            endpoint: DEFAULT_CODEX_ENDPOINT.to_string(),
        }
    }
}

/// Codex OAuth-backed thinking-partner LLM client.
///
/// Cheap to construct; holds a single shared `reqwest::Client` for
/// connection reuse and an [`AccountsStore`] that reads
/// `accounts.conf` + `auth.json` on every turn. The full config is
/// kept by value so the type stays `Send + Sync` without an `Arc`.
pub struct CodexOauthBackend {
    config: CodexOauthConfig,
    accounts: AccountsStore,
    oauth: CodexOauth,
    refresh_dedup: RefreshDedup,
    http: reqwest::Client,
    /// Optional skills registry. When `Some`, every `respond()` call
    /// prepends [`SkillRegistry::index_for_prompt`] to the instructions
    /// field. Unlike the Anthropic backend, no tool advertising — the
    /// model sees the index but cannot autoload bodies (Phase 6.1).
    skills: Option<Arc<SkillRegistry>>,
}

impl CodexOauthBackend {
    /// Build a backend without skill autoload.
    ///
    /// # Errors
    /// Returns [`ThinkingError::Config`] when the accounts store can't
    /// be opened (extremely rare — [`AccountsStore::open`] is lazy and
    /// only errors on a path traversal we don't generate here).
    pub fn new(config: CodexOauthConfig) -> Result<Self> {
        let accounts = AccountsStore::open(&config.accounts_conf_path).map_err(|e| {
            ThinkingError::Config(format!(
                "opening accounts store at {}: {e}",
                config.accounts_conf_path.display()
            ))
        })?;
        let oauth = CodexOauth::new(OPENAI_CODEX_CLIENT_ID.to_string());
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("reqwest client builder is infallible in this config");
        Ok(Self {
            config,
            accounts,
            oauth,
            refresh_dedup: RefreshDedup::new(),
            http,
            skills: None,
        })
    }

    /// Attach a [`SkillRegistry`] so the index block is prepended to the
    /// `instructions` field on every turn.
    ///
    /// No tool advertising — see the module-level Phase 6.1 note.
    #[must_use]
    pub fn with_skills(mut self, skills: Arc<SkillRegistry>) -> Self {
        self.skills = Some(skills);
        self
    }

    /// The configured model — exposed for diagnostics + tests.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// The configured account alias — exposed for diagnostics + tests.
    #[must_use]
    pub fn account_name(&self) -> &str {
        &self.config.account_name
    }

    /// Whether this backend will inject the skills index on `respond()`.
    #[must_use]
    pub fn has_skills(&self) -> bool {
        self.skills.is_some()
    }

    /// Compute the full Responses endpoint URL.
    fn endpoint(&self) -> String {
        let base = self.config.endpoint.trim_end_matches('/');
        format!("{base}/responses")
    }

    /// Compose the full instructions sent to Codex: when a registry is
    /// attached, prepend the skills index block above the caller-supplied
    /// prompt. Otherwise pass the caller prompt through unchanged.
    fn compose_instructions(&self, base: &str) -> String {
        match &self.skills {
            Some(reg) => {
                let idx = reg.index_for_prompt();
                if idx.is_empty() {
                    base.to_string()
                } else {
                    format!("{idx}\n{base}")
                }
            }
            None => base.to_string(),
        }
    }

    /// Read the account record from disk, refreshing the access token if
    /// it's within [`REFRESH_SKEW_SECONDS`] of expiring. Returns the
    /// fresh `AccountRecord` ready for use.
    async fn get_fresh_token(&self) -> Result<AccountRecord> {
        // First read — outside the refresh lock — to check if we even
        // need to refresh. Avoids taking the lock on every chat turn.
        let initial = self
            .accounts
            .get(&self.config.account_name)
            .await
            .map_err(|e| {
                ThinkingError::Config(format!(
                    "reading account {} from accounts store: {e}",
                    self.config.account_name
                ))
            })?
            .ok_or_else(|| {
                ThinkingError::Config(format!(
                    "no token blob found for account {} (expected `{}/auth.json` from accounts.conf row)",
                    self.config.account_name,
                    self.config.accounts_conf_path.display(),
                ))
            })?;

        if !needs_refresh(&initial) {
            return Ok(initial);
        }

        // Near expiry — serialise the refresh with any concurrent
        // callers on the same account. RefreshDedup's docstring keys on
        // the auth.json *storage path* so two aliases sharing a
        // config_dir (degenerate but possible) collapse to the same
        // lock. We key on `<accounts_conf>/.lock-<alias>` instead —
        // synthetic, alias-scoped. Within a single backend instance
        // bound to one alias this is correct; the only case it misses
        // is two distinct aliases that point at the same config_dir.
        // TODO(phase6.1): resolve the real `auth.json` path via
        // `self.accounts.find_row(...)?.map(|r| r.config_dir.join("auth.json"))`
        // and use that as the dedup key so degenerate config_dir
        // sharing collapses to one critical section.
        let dedup_key = self
            .config
            .accounts_conf_path
            .join(format!(".lock-{}", self.config.account_name));
        let slot = self.refresh_dedup.slot(dedup_key).await;
        let _guard = slot.lock().await;

        // Re-read inside the lock — another caller may have refreshed
        // while we waited. Avoids burning a refresh credit unnecessarily.
        let inside = self
            .accounts
            .get(&self.config.account_name)
            .await
            .map_err(|e| {
                ThinkingError::Config(format!(
                    "re-reading account {} inside refresh lock: {e}",
                    self.config.account_name
                ))
            })?
            .ok_or_else(|| {
                ThinkingError::Config(format!(
                    "account {} disappeared during refresh window",
                    self.config.account_name
                ))
            })?;
        if !needs_refresh(&inside) {
            return Ok(inside);
        }

        let refresh_token = inside.refresh_token.clone().ok_or_else(|| {
            ThinkingError::Config(format!(
                "account {} has no refresh_token on disk; re-run `subctl auth codex` to mint a fresh OAuth bundle",
                self.config.account_name,
            ))
        })?;

        info!(
            account = %self.config.account_name,
            "codex token near expiry; refreshing",
        );
        let new_token = self
            .oauth
            .refresh(&refresh_token)
            .await
            .map_err(|e| ThinkingError::Transport(format!("codex refresh: {e}")))?;

        // NB: CodexOauth::refresh() does NOT surface a rotated
        // refresh_token even when the upstream rotates one. Persist
        // the new access_token + expires_at and keep the existing
        // refresh_token; if Codex rotated, the next refresh attempt
        // will 401 and the operator re-mints via `subctl auth codex`.
        // Tracked as Phase 6.1 follow-up. See evy-providers
        // crates/evy-providers/src/oauth/codex.rs:267 for the
        // upstream surface gap.
        let refreshed = AccountRecord {
            name: inside.name.clone(),
            provider: inside.provider.clone(),
            access_token: new_token.token.clone(),
            refresh_token: Some(refresh_token),
            expires_at: new_token.expires_at,
        };
        if let Err(e) = self.accounts.put(refreshed.clone()).await {
            // Persistence failure is non-fatal — we still have a valid
            // token in memory for this turn. The next turn will see the
            // old (still-expiring) on-disk token and try to refresh
            // again. Log loudly so the operator can investigate.
            warn!(
                error = %e,
                account = %self.config.account_name,
                "codex refresh persisted in memory but auth.json write failed",
            );
        }
        Ok(refreshed)
    }

    /// Build the wire body for a single Responses turn.
    fn build_request_body(&self, instructions: &str, messages: &[Message]) -> Value {
        // Translate the session log into Responses input items.
        // Role::System entries are surface-side scaffolding — the
        // system text is in the top-level `instructions` field.
        // Operator → user with input_text; Partner → assistant with
        // output_text (the Responses API rejects input_text on
        // assistant messages and vice versa).
        let input: Vec<Value> = messages
            .iter()
            .filter_map(|m| match m.role {
                Role::Operator => Some(json!({
                    "role": "user",
                    "content": [{"type": "input_text", "text": m.content.clone()}],
                })),
                Role::Partner => Some(json!({
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": m.content.clone()}],
                })),
                Role::System => None,
            })
            .collect();

        json!({
            "model": self.config.model,
            "instructions": instructions,
            "input": input,
            "store": false,
            "max_output_tokens": self.config.max_tokens,
        })
    }
}

#[async_trait]
impl LlmBackend for CodexOauthBackend {
    async fn respond(&self, system_prompt: &str, messages: &[Message]) -> Result<String> {
        if self.config.account_name.trim().is_empty() {
            return Err(ThinkingError::Config(
                "CodexOauthConfig.account_name is empty".to_string(),
            ));
        }

        let account = self.get_fresh_token().await?;
        let account_id_header = decode_chatgpt_account_id(&account.access_token);
        if account_id_header.is_none() {
            // Not fatal — multi-account JWTs need this header for the
            // server to route, but personal-account JWTs may work
            // without it. Log so the operator can correlate a later
            // 401/403 with the missing header.
            warn!(
                account = %self.config.account_name,
                "codex JWT did not yield a chatgpt_account_id claim; request will go out without ChatGPT-Account-ID header",
            );
        }

        let instructions = self.compose_instructions(system_prompt);
        let body = self.build_request_body(&instructions, messages);

        debug!(
            model = %self.config.model,
            account = %self.config.account_name,
            turns = messages.iter().filter(|m| m.role != Role::System).count(),
            "evy-thinking: codex respond",
        );

        let endpoint = self.endpoint();
        let mut req = self
            .http
            .post(&endpoint)
            .header("Authorization", format!("Bearer {}", account.access_token))
            .header("Content-Type", "application/json")
            .header("originator", "codex_cli_rs")
            .header(
                "User-Agent",
                format!(
                    "codex_cli_rs/0.0.0 (subctl-rust/{})",
                    env!("CARGO_PKG_VERSION")
                ),
            );
        if let Some(acct) = &account_id_header {
            req = req.header("ChatGPT-Account-ID", acct);
        }

        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| ThinkingError::Transport(e.without_url().to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let snippet = body_snippet(resp).await;
            warn!(status, snippet = %snippet, "codex non-2xx");
            return Err(ThinkingError::HttpStatus { status, snippet });
        }

        let parsed: WireResponse = resp
            .json()
            .await
            .map_err(|e| ThinkingError::Decode(e.without_url().to_string()))?;

        let text = extract_text(&parsed);
        if text.trim().is_empty() {
            return Err(ThinkingError::BackendRefused(
                "codex returned no output_text content".to_string(),
            ));
        }
        Ok(text)
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────

/// Should we refresh `record.access_token` now? True if the token is
/// within [`REFRESH_SKEW_SECONDS`] of expiring (or already expired).
fn needs_refresh(record: &AccountRecord) -> bool {
    let now = Utc::now();
    let skew = chrono::Duration::seconds(REFRESH_SKEW_SECONDS);
    record.expires_at - now <= skew
}

/// Walk a Responses API response for assistant text. Returns the
/// concatenation of every `output_text` block across every `message`
/// item, falling back to the top-level `output_text` convenience field
/// when the array is empty.
fn extract_text(resp: &WireResponse) -> String {
    let mut chunks: Vec<String> = Vec::new();
    for item in &resp.output {
        match item {
            WireOutputItem::Message { content, .. } => {
                for part in content {
                    if let WireContentPart::OutputText { text } = part {
                        if !text.is_empty() {
                            chunks.push(text.clone());
                        }
                    }
                }
            }
            WireOutputItem::Other => {}
        }
    }
    if chunks.is_empty() {
        if let Some(s) = &resp.output_text {
            if !s.is_empty() {
                return s.clone();
            }
        }
    }
    chunks.join("\n")
}

/// Decode the `chatgpt_account_id` claim out of a Codex OAuth JWT for
/// the `ChatGPT-Account-ID` header. Returns `None` on any decoding
/// failure — caller treats absence as "send the request without that
/// header" rather than panicking.
fn decode_chatgpt_account_id(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| {
            let mut padded = parts[1].to_string();
            while !padded.len().is_multiple_of(4) {
                padded.push('=');
            }
            base64::engine::general_purpose::URL_SAFE.decode(&padded)
        })
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

async fn body_snippet(resp: reqwest::Response) -> String {
    let body = resp.text().await.unwrap_or_default();
    body.chars().take(200).collect()
}

// ─── Codex Responses wire shapes (only the fields we send / read) ────────

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    output: Vec<WireOutputItem>,
    /// Convenience text field the Responses API emits alongside
    /// `output`. Some surfaces drop the structured items when reasoning
    /// alone produced the answer.
    #[serde(default)]
    output_text: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireOutputItem {
    /// `{"type": "message", "role": "assistant", "content": [...]}`
    Message {
        #[serde(default)]
        content: Vec<WireContentPart>,
    },
    /// `reasoning`, `function_call`, `custom_tool_call`, … — silently
    /// ignored. Reasoning items carry encrypted_content the Phase 6.0
    /// non-streaming backend doesn't need; tool-use items don't fire
    /// because we don't advertise tools (Phase 6.1).
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireContentPart {
    /// `{"type": "output_text", "text": "..."}` — assistant text the
    /// Responses API emits.
    OutputText { text: String },
    /// Anything else (`input_text` echoed back, image refs, …) is
    /// silently ignored.
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionId;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[test]
    fn config_defaults_point_at_real_codex_host() {
        let c = CodexOauthConfig::new("openai-jason");
        assert_eq!(c.endpoint, DEFAULT_CODEX_ENDPOINT);
        assert_eq!(c.model, DEFAULT_CODEX_MODEL);
        assert_eq!(c.max_tokens, DEFAULT_CODEX_MAX_TOKENS);
        assert!(c.timeout > Duration::ZERO);
        assert_eq!(c.account_name, "openai-jason");
    }

    #[test]
    fn endpoint_appends_responses_without_double_slash() {
        let mut cfg = CodexOauthConfig::new("a");
        cfg.endpoint = "https://example.com/".to_string();
        let b = CodexOauthBackend::new(cfg).expect("construct");
        assert_eq!(b.endpoint(), "https://example.com/responses");

        let mut cfg2 = CodexOauthConfig::new("a");
        cfg2.endpoint = "https://example.com".to_string();
        let b2 = CodexOauthBackend::new(cfg2).expect("construct");
        assert_eq!(b2.endpoint(), "https://example.com/responses");
    }

    #[test]
    fn build_request_body_role_mapping_and_content_types() {
        let cfg = CodexOauthConfig::new("a");
        let b = CodexOauthBackend::new(cfg).expect("construct");
        let sid = SessionId::new();
        let msgs = [
            Message::new(sid, Role::Operator, "Q1"),
            Message::new(sid, Role::Partner, "A1"),
            // System messages must be filtered out — they're scaffolding.
            Message::new(sid, Role::System, "session opened"),
        ];
        let body = b.build_request_body("sys", &msgs);
        let input = body["input"].as_array().expect("input array");
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "Q1");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["content"][0]["text"], "A1");
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["store"], false);
        assert_eq!(body["max_output_tokens"], DEFAULT_CODEX_MAX_TOKENS);
        assert_eq!(body["model"], DEFAULT_CODEX_MODEL);
    }

    #[test]
    fn wire_response_decodes_message_with_output_text() {
        let body = r#"{
            "output": [
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "hello"}]}
            ]
        }"#;
        let parsed: WireResponse = serde_json::from_str(body).expect("decode");
        let text = extract_text(&parsed);
        assert_eq!(text, "hello");
    }

    #[test]
    fn wire_response_falls_back_to_top_level_output_text() {
        let body = r#"{ "output": [], "output_text": "fallback" }"#;
        let parsed: WireResponse = serde_json::from_str(body).expect("decode");
        let text = extract_text(&parsed);
        assert_eq!(text, "fallback");
    }

    #[test]
    fn wire_response_ignores_reasoning_and_function_call_items() {
        let body = r#"{
            "output": [
                {"type": "reasoning", "encrypted_content": "opaque"},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "ok"}]},
                {"type": "function_call", "call_id": "c1", "name": "x", "arguments": "{}"}
            ]
        }"#;
        let parsed: WireResponse = serde_json::from_str(body).expect("decode");
        assert_eq!(extract_text(&parsed), "ok");
    }

    #[test]
    fn decode_chatgpt_account_id_returns_value_when_claim_present() {
        // Build a JWT-shaped string with payload =
        //   {"https://api.openai.com/auth": {"chatgpt_account_id": "acct-123"}}
        let payload = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-123"
            }
        });
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload.to_string());
        let token = format!("hdr.{payload_b64}.sig");
        assert_eq!(
            decode_chatgpt_account_id(&token),
            Some("acct-123".to_string())
        );
    }

    #[test]
    fn decode_chatgpt_account_id_returns_none_when_claim_missing() {
        let payload = serde_json::json!({ "iss": "openai", "sub": "u1" });
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload.to_string());
        let token = format!("hdr.{payload_b64}.sig");
        assert!(decode_chatgpt_account_id(&token).is_none());
    }

    #[test]
    fn decode_chatgpt_account_id_tolerates_malformed_tokens() {
        assert!(decode_chatgpt_account_id("not-a-jwt").is_none());
        assert!(decode_chatgpt_account_id("only.two").is_none());
        assert!(decode_chatgpt_account_id("aa.bb.cc").is_none());
    }

    #[test]
    fn needs_refresh_returns_true_when_within_skew() {
        let near = AccountRecord {
            name: "x".into(),
            provider: "openai-codex".into(),
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: Utc::now() + chrono::Duration::seconds(REFRESH_SKEW_SECONDS / 2),
        };
        assert!(needs_refresh(&near));
    }

    #[test]
    fn needs_refresh_returns_false_when_outside_skew() {
        let far = AccountRecord {
            name: "x".into(),
            provider: "openai-codex".into(),
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: Utc::now() + chrono::Duration::seconds(REFRESH_SKEW_SECONDS * 4),
        };
        assert!(!needs_refresh(&far));
    }

    #[test]
    fn backend_without_skills_renders_instructions_unchanged() {
        let b = CodexOauthBackend::new(CodexOauthConfig::new("a")).expect("construct");
        assert!(!b.has_skills());
        assert_eq!(b.compose_instructions("plain"), "plain");
    }

    #[tokio::test]
    async fn empty_account_name_returns_config_error_without_network() {
        let mut cfg = CodexOauthConfig::new("a");
        cfg.account_name = "  ".to_string();
        cfg.endpoint = "http://127.0.0.1:1".to_string(); // would refuse if reached
        let b = CodexOauthBackend::new(cfg).expect("construct");
        let err = b.respond("sys", &[]).await.expect_err("must fail");
        assert!(matches!(err, ThinkingError::Config(_)), "got: {err:?}");
    }
}
