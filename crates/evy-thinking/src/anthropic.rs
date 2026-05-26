//! Anthropic-backed [`LlmBackend`] — talks to the Anthropic Messages
//! API at `POST {api_base}/v1/messages`.
//!
//! ## Wire shape
//!
//! Request (only the fields we send):
//!
//! ```json
//! {
//!   "model": "claude-sonnet-4-5",
//!   "max_tokens": 4096,
//!   "system": "<planning_system_prompt(topic)>",
//!   "messages": [
//!     {"role": "user",      "content": "..."},
//!     {"role": "assistant", "content": "..."}
//!   ]
//! }
//! ```
//!
//! Required headers (verified 2026-05-26 against
//! <https://docs.anthropic.com/en/api/messages>):
//!
//! | Header              | Value                          |
//! |---------------------|--------------------------------|
//! | `x-api-key`         | `ANTHROPIC_API_KEY`            |
//! | `anthropic-version` | `2023-06-01`                   |
//! | `content-type`      | `application/json`             |
//!
//! Response (only the fields we read):
//!
//! ```json
//! {
//!   "content": [{"type": "text", "text": "..."}],
//!   "stop_reason": "end_turn",
//!   "usage": {"input_tokens": 10, "output_tokens": 20}
//! }
//! ```
//!
//! ## Role mapping
//!
//! Anthropic's `messages` array only accepts `"user"` and `"assistant"`.
//! [`crate::Role::System`] entries are surface-side scaffolding and are
//! **skipped** here — the system prompt is rendered separately into the
//! top-level `system` field. See [`crate::session::Role`].

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::backend::LlmBackend;
use crate::error::{Result, ThinkingError};
use crate::session::{Message, Role};

/// Default base URL for the Anthropic API. Overridden in tests to point
/// at a `wiremock` server.
pub const DEFAULT_ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";

/// Default model identifier. Pin to a specific Sonnet so behaviour is
/// reproducible; the operator can override in [`AnthropicConfig`].
///
/// `claude-sonnet-4-5` is the most capable Sonnet available at v0.5.0;
/// when newer Sonnets ship the operator should bump this.
// TODO: Phase 4 — let the operator pin a dated alias (e.g.
// `claude-sonnet-4-5-20250929`) per project once we have project-scoped
// config in evy-memory.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-5";

/// Default `max_tokens` for partner replies. Generous because a draft
/// plan with all four sections plus the iteration closer is typically
/// 600-1200 output tokens; 4096 leaves headroom for big projects.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Default per-request transport timeout. Generous because Anthropic
/// latency varies widely under load and partial-plan timeouts are worse
/// than just waiting.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Static configuration for [`AnthropicBackend`].
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// Base URL for the API. Defaults to [`DEFAULT_ANTHROPIC_API_BASE`]
    /// in production; tests override this to point at a `wiremock`
    /// `MockServer::uri()`.
    pub api_base: String,
    /// API key sent in the `x-api-key` header. Loaded from the
    /// environment via [`AnthropicConfig::from_env`].
    pub api_key: String,
    /// Model identifier sent in the request body. Defaults to
    /// [`DEFAULT_MODEL`].
    pub model: String,
    /// Output token cap sent in the request body. Defaults to
    /// [`DEFAULT_MAX_TOKENS`].
    pub max_tokens: u32,
    /// Per-request transport timeout. Defaults to [`DEFAULT_TIMEOUT`].
    pub timeout: Duration,
}

impl AnthropicConfig {
    /// Construct with the supplied key and production defaults.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_base: DEFAULT_ANTHROPIC_API_BASE.to_string(),
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Load the key from `ANTHROPIC_API_KEY`; everything else gets
    /// defaults. Returns [`ThinkingError::Config`] if the env var is
    /// missing or empty.
    ///
    /// # Errors
    /// See above.
    pub fn from_env() -> Result<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
            ThinkingError::Config("ANTHROPIC_API_KEY environment variable is missing".to_string())
        })?;
        if key.trim().is_empty() {
            return Err(ThinkingError::Config(
                "ANTHROPIC_API_KEY is empty".to_string(),
            ));
        }
        Ok(Self::new(key))
    }
}

/// Anthropic-backed thinking-partner LLM client.
///
/// Cheap to construct; holds a single shared `reqwest::Client` so
/// connections / keepalives are reused across turns. The full config is
/// kept by value so the type stays `Send + Sync` without an `Arc`.
pub struct AnthropicBackend {
    config: AnthropicConfig,
    http: reqwest::Client,
}

impl AnthropicBackend {
    /// Build a backend. Panics only on a `reqwest::Client` builder
    /// failure, which is infallible for the feature flags this crate
    /// enables.
    #[must_use]
    pub fn new(config: AnthropicConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("reqwest client builder is infallible in this config");
        Self { config, http }
    }

    /// The configured model — exposed for diagnostics + tests.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.config.model
    }

    fn endpoint(&self) -> String {
        let base = self.config.api_base.trim_end_matches('/');
        format!("{base}/v1/messages")
    }
}

#[async_trait]
impl LlmBackend for AnthropicBackend {
    async fn respond(&self, system_prompt: &str, messages: &[Message]) -> Result<String> {
        if self.config.api_key.trim().is_empty() {
            return Err(ThinkingError::Config(
                "AnthropicConfig.api_key is empty".to_string(),
            ));
        }

        // Translate the session log into Anthropic's wire envelope.
        // `Role::System` entries are surface-side scaffolding — the
        // system prompt is already in the top-level `system` field.
        let wire_messages: Vec<WireMessage> = messages
            .iter()
            .filter_map(|m| match m.role {
                Role::Operator => Some(WireMessage {
                    role: "user",
                    content: m.content.clone(),
                }),
                Role::Partner => Some(WireMessage {
                    role: "assistant",
                    content: m.content.clone(),
                }),
                Role::System => None,
            })
            .collect();

        let body = WireRequest {
            model: &self.config.model,
            max_tokens: self.config.max_tokens,
            system: system_prompt,
            messages: &wire_messages,
        };

        debug!(
            model = %self.config.model,
            turns = wire_messages.len(),
            "evy-thinking: anthropic respond",
        );

        let resp = self
            .http
            .post(self.endpoint())
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ThinkingError::Transport(e.without_url().to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let snippet = body_snippet(resp).await;
            warn!(status, snippet = %snippet, "anthropic non-2xx");
            return Err(ThinkingError::HttpStatus { status, snippet });
        }

        let parsed: WireResponse = resp
            .json()
            .await
            .map_err(|e| ThinkingError::Decode(e.without_url().to_string()))?;

        // Concatenate every `text`-typed content block. The Messages
        // API can return multiple blocks (e.g. tool_use interleaved
        // with text); we only consume `text`. If no text blocks were
        // emitted, surface as BackendRefused so the caller knows the
        // wire shape was valid but the LLM produced nothing useful.
        let text = parsed
            .content
            .into_iter()
            .filter_map(|b| match b {
                WireContentBlock::Text { text } => Some(text),
                WireContentBlock::Other => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        if text.trim().is_empty() {
            return Err(ThinkingError::BackendRefused(
                "anthropic returned no text content".to_string(),
            ));
        }

        Ok(text)
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────

async fn body_snippet(resp: reqwest::Response) -> String {
    let body = resp.text().await.unwrap_or_default();
    body.chars().take(200).collect()
}

// ─── Anthropic wire shapes (only the fields we send / read) ──────────────

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: &'a [WireMessage],
}

#[derive(Debug, Serialize)]
struct WireMessage {
    role: &'static str,
    content: String,
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    content: Vec<WireContentBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireContentBlock {
    /// `{"type": "text", "text": "..."}`
    Text { text: String },
    /// Any other block type (`tool_use`, `tool_result`, image refs, …)
    /// is silently ignored — v0.5.0 doesn't speak tool-use over this
    /// channel. The `#[serde(other)]` variant catches them.
    // TODO: Phase 4 — surface tool_use blocks when Evy gains the
    // ability to call her own research/memory tools mid-conversation.
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionId;

    #[test]
    fn config_defaults_point_at_real_host() {
        let c = AnthropicConfig::new("test-key");
        assert_eq!(c.api_base, DEFAULT_ANTHROPIC_API_BASE);
        assert_eq!(c.model, DEFAULT_MODEL);
        assert_eq!(c.max_tokens, DEFAULT_MAX_TOKENS);
        assert!(c.timeout > Duration::ZERO);
        assert_eq!(c.api_key, "test-key");
    }

    #[test]
    fn endpoint_appends_v1_messages_without_double_slash() {
        let b = AnthropicBackend::new(AnthropicConfig {
            api_base: "https://example.com/".to_string(),
            ..AnthropicConfig::new("k")
        });
        assert_eq!(b.endpoint(), "https://example.com/v1/messages");

        let b2 = AnthropicBackend::new(AnthropicConfig {
            api_base: "https://example.com".to_string(),
            ..AnthropicConfig::new("k")
        });
        assert_eq!(b2.endpoint(), "https://example.com/v1/messages");
    }

    #[test]
    fn wire_response_decodes_text_block() {
        let body = r#"{
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [
                {"type": "text", "text": "hello"}
            ],
            "stop_reason": "end_turn"
        }"#;
        let parsed: WireResponse = serde_json::from_str(body).expect("decode");
        assert_eq!(parsed.content.len(), 1);
        match &parsed.content[0] {
            WireContentBlock::Text { text } => assert_eq!(text, "hello"),
            WireContentBlock::Other => panic!("expected text block"),
        }
    }

    #[test]
    fn wire_response_skips_unknown_block_type() {
        // tool_use blocks (or any future block type) must decode into
        // `Other` so we don't break when Anthropic adds variants.
        let body = r#"{
            "content": [
                {"type": "text", "text": "draft"},
                {"type": "tool_use", "id": "x", "name": "foo", "input": {}}
            ]
        }"#;
        let parsed: WireResponse = serde_json::from_str(body).expect("decode");
        assert_eq!(parsed.content.len(), 2);
        let text_blocks: Vec<&str> = parsed
            .content
            .iter()
            .filter_map(|b| match b {
                WireContentBlock::Text { text } => Some(text.as_str()),
                WireContentBlock::Other => None,
            })
            .collect();
        assert_eq!(text_blocks, vec!["draft"]);
    }

    #[test]
    fn wire_request_serializes_role_mapping() {
        let sid = SessionId::new();
        // System rows must be filtered BEFORE building WireRequest;
        // this test asserts the filter at the call site.
        let msgs = [
            Message::new(sid, Role::Operator, "what about postgres 16?"),
            Message::new(sid, Role::Partner, "here is a draft..."),
            Message::new(sid, Role::System, "session opened"),
        ];
        let wire: Vec<WireMessage> = msgs
            .iter()
            .filter_map(|m| match m.role {
                Role::Operator => Some(WireMessage {
                    role: "user",
                    content: m.content.clone(),
                }),
                Role::Partner => Some(WireMessage {
                    role: "assistant",
                    content: m.content.clone(),
                }),
                Role::System => None,
            })
            .collect();
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[0].role, "user");
        assert_eq!(wire[1].role, "assistant");

        let body = WireRequest {
            model: "m",
            max_tokens: 100,
            system: "sys",
            messages: &wire,
        };
        let s = serde_json::to_string(&body).expect("serialize");
        assert!(s.contains("\"role\":\"user\""));
        assert!(s.contains("\"role\":\"assistant\""));
        assert!(s.contains("\"system\":\"sys\""));
    }

    #[tokio::test]
    async fn empty_api_key_returns_config_error_without_network() {
        let mut cfg = AnthropicConfig::new("real-key");
        cfg.api_key = "  ".to_string();
        cfg.api_base = "http://127.0.0.1:1".to_string(); // would refuse if reached
        let b = AnthropicBackend::new(cfg);
        let err = b.respond("sys", &[]).await.expect_err("must fail");
        assert!(matches!(err, ThinkingError::Config(_)), "got: {err:?}");
    }

    #[test]
    fn from_env_missing_returns_config_error() {
        // SAFETY: we only mutate ANTHROPIC_API_KEY here; tests in this
        // module that depend on it set it explicitly. This is a
        // single-threaded test on a process-wide global; serial-test
        // would be cleaner but is an unused dep. Run with
        // `cargo test -- --test-threads=1` if env-var races matter.
        // unsafe: std::env::remove_var is `unsafe` as of edition 2024.
        // We're 2021 here so it's safe; if/when the crate moves to
        // 2024 wrap with `unsafe { ... }`.
        let prev = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");
        let err = AnthropicConfig::from_env().expect_err("must fail");
        assert!(matches!(err, ThinkingError::Config(_)));
        if let Some(v) = prev {
            std::env::set_var("ANTHROPIC_API_KEY", v);
        }
    }
}
