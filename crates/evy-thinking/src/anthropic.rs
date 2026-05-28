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
//!   ],
//!   "tools": [ { "name": "skill_view", "input_schema": { ... } } ]
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
//!   "content": [
//!     {"type": "text", "text": "..."},
//!     {"type": "tool_use", "id": "toolu_...", "name": "skill_view",
//!      "input": {"name": "some-skill"}}
//!   ],
//!   "stop_reason": "end_turn" | "tool_use",
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
//!
//! ## Skill autoload (Hermes-style)
//!
//! When the backend is constructed via
//! [`AnthropicBackend::with_skills`], every call to
//! [`AnthropicBackend::respond`] prepends the registry's
//! [`SkillRegistry::index_for_prompt`] block to the system prompt and
//! advertises a `skill_view` tool. If the model emits a
//! `tool_use(skill_view, {name})` block, the backend resolves the body
//! via [`SkillRegistry::find`], replies with a `tool_result` block, and
//! loops up to [`MAX_TOOL_ROUNDTRIPS`] times until the model produces a
//! plain text response. Every autoload is traced at `info` level so
//! operators can see which skill the model pulled.
//!
//! The trait surface ([`LlmBackend::respond`]) is unchanged — tool-call
//! handling is an implementation detail of this backend.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use evy_skills::SkillRegistry;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

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

/// Upper bound on consecutive `skill_view` tool round-trips per
/// [`AnthropicBackend::respond`] call.
///
/// A well-behaved model loads 1-3 skills then produces a text response.
/// Anything past this cap is almost certainly a pathological loop (the
/// model keeps re-loading skills instead of answering). We surface as
/// [`ThinkingError::BackendRefused`] so the caller knows the wire was
/// fine but the conversation didn't converge.
pub const MAX_TOOL_ROUNDTRIPS: usize = 5;

/// Tool name advertised to the LLM for Hermes-style skill autoload.
const SKILL_VIEW_TOOL: &str = "skill_view";

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
    /// Optional skills registry. When `Some`, every `respond()` call
    /// (a) prepends [`SkillRegistry::index_for_prompt`] to the system
    /// prompt, (b) advertises the `skill_view` tool to the model, and
    /// (c) loops to handle tool_use → tool_result round-trips. When
    /// `None`, the backend behaves exactly as it did before
    /// Phase 5 (no skill autoload, no tools field on the wire).
    skills: Option<Arc<SkillRegistry>>,
}

impl AnthropicBackend {
    /// Build a backend without skill autoload. Panics only on a
    /// `reqwest::Client` builder failure, which is infallible for the
    /// feature flags this crate enables.
    #[must_use]
    pub fn new(config: AnthropicConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("reqwest client builder is infallible in this config");
        Self {
            config,
            http,
            skills: None,
        }
    }

    /// Attach a [`SkillRegistry`] to enable Hermes-style LLM-driven
    /// skill autoload via the `skill_view` tool.
    ///
    /// See the module-level docs for the round-trip protocol.
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

    /// Whether this backend will inject the skills index + tool on
    /// `respond()`. Exposed for diagnostics + tests.
    #[must_use]
    pub fn has_skills(&self) -> bool {
        self.skills.is_some()
    }

    fn endpoint(&self) -> String {
        let base = self.config.api_base.trim_end_matches('/');
        format!("{base}/v1/messages")
    }

    /// Compose the full system prompt sent to Anthropic: when a
    /// registry is attached, prepend the skills index block above the
    /// caller-supplied prompt. Otherwise pass the caller prompt
    /// through unchanged.
    fn compose_system_prompt(&self, base: &str) -> String {
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

    /// Build the `tools` array for the request body when skills are
    /// configured AND the registry has at least one entry. Returns
    /// `None` otherwise — Anthropic accepts the field being absent
    /// entirely, and advertising `skill_view` against an empty registry
    /// would give the model a tool whose every invocation errors out.
    fn tools_for_request(&self) -> Option<Vec<Value>> {
        let reg = self.skills.as_ref()?;
        if reg.count() == 0 {
            return None;
        }
        Some(vec![json!({
            "name": SKILL_VIEW_TOOL,
            "description": "Load the full body of a named skill from the operator's skill catalog. Call this whenever the system-prompt skills index lists a skill that is even partially relevant to the current turn; the returned content is procedural knowledge you do not have by default.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The exact skill name as listed in the system-prompt index."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }
        })])
    }

    /// Send one HTTP request to Anthropic with the supplied wire body
    /// and decode the response. Centralised so the tool-loop and the
    /// first turn share transport / decode / error mapping.
    async fn send_one(&self, body: &Value) -> Result<WireResponse> {
        let resp = self
            .http
            .post(self.endpoint())
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| ThinkingError::Transport(e.without_url().to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let snippet = body_snippet(resp).await;
            warn!(status, snippet = %snippet, "anthropic non-2xx");
            return Err(ThinkingError::HttpStatus { status, snippet });
        }

        resp.json::<WireResponse>()
            .await
            .map_err(|e| ThinkingError::Decode(e.without_url().to_string()))
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
        // Operator → user (content: plain string), Partner → assistant
        // (content: plain string). Once we start the tool loop we'll
        // append assistant turns with structured content (text +
        // tool_use blocks) and user turns with tool_result blocks.
        let mut wire_messages: Vec<Value> = messages
            .iter()
            .filter_map(|m| match m.role {
                Role::Operator => Some(json!({"role": "user", "content": m.content.clone()})),
                Role::Partner => Some(json!({"role": "assistant", "content": m.content.clone()})),
                Role::System => None,
            })
            .collect();

        let composed_system = self.compose_system_prompt(system_prompt);
        let tools = self.tools_for_request();

        debug!(
            model = %self.config.model,
            turns = wire_messages.len(),
            tools = tools.is_some(),
            "evy-thinking: anthropic respond",
        );

        // Tool-call loop. Most calls return after one round-trip — the
        // model either produces text immediately or requests one or
        // two skills. We cap at MAX_TOOL_ROUNDTRIPS as a safety net.
        for round in 0..=MAX_TOOL_ROUNDTRIPS {
            let mut body = json!({
                "model": &self.config.model,
                "max_tokens": self.config.max_tokens,
                "system": composed_system,
                "messages": wire_messages,
            });
            if let Some(t) = &tools {
                body["tools"] = json!(t);
            }

            let parsed = self.send_one(&body).await?;

            // Did the model ask to call a tool?
            let tool_uses: Vec<&WireToolUseBlock> = parsed
                .content
                .iter()
                .filter_map(|b| match b {
                    WireContentBlock::ToolUse(t) => Some(t),
                    _ => None,
                })
                .collect();

            let asked_for_tool =
                !tool_uses.is_empty() && parsed.stop_reason.as_deref() == Some("tool_use");

            if !asked_for_tool {
                // No tool calls — collect text and return.
                let text = parsed
                    .content
                    .into_iter()
                    .filter_map(|b| match b {
                        WireContentBlock::Text { text } => Some(text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                if text.trim().is_empty() {
                    return Err(ThinkingError::BackendRefused(
                        "anthropic returned no text content".to_string(),
                    ));
                }
                return Ok(text);
            }

            // Cap on consecutive tool round-trips — round counts how
            // many tool turns we've already taken at this point. If we
            // hit the cap and the model is STILL asking for tools, the
            // conversation is not converging.
            if round == MAX_TOOL_ROUNDTRIPS {
                return Err(ThinkingError::BackendRefused(format!(
                    "skill_view loop exceeded max iterations ({MAX_TOOL_ROUNDTRIPS}); model never produced text"
                )));
            }

            // Replay the assistant turn (must include the full content
            // array verbatim, per Anthropic's protocol) and then a user
            // turn carrying tool_result blocks for every tool_use the
            // model emitted.
            //
            // Round-trip the assistant content as serde JSON to avoid
            // re-serialising the tool_use blocks ourselves — they may
            // carry fields beyond what we model.
            let assistant_content: Vec<Value> = parsed
                .content
                .iter()
                .map(|b| match b {
                    WireContentBlock::Text { text } => {
                        json!({"type": "text", "text": text})
                    }
                    WireContentBlock::ToolUse(t) => json!({
                        "type": "tool_use",
                        "id": t.id,
                        "name": t.name,
                        "input": t.input,
                    }),
                    WireContentBlock::Other => json!({"type": "unknown"}),
                })
                .collect();
            wire_messages.push(json!({
                "role": "assistant",
                "content": assistant_content,
            }));

            let mut tool_results: Vec<Value> = Vec::with_capacity(tool_uses.len());
            for tool in tool_uses {
                let result = self.handle_tool_call(tool);
                tool_results.push(result);
            }
            wire_messages.push(json!({
                "role": "user",
                "content": tool_results,
            }));
        }

        // Unreachable: the loop body returns or pushes another iteration
        // on every round, and the cap check exits before we'd fall out.
        // Keeping an explicit error path here protects against a future
        // edit that breaks the invariant.
        Err(ThinkingError::BackendRefused(
            "skill_view loop exited without producing text".to_string(),
        ))
    }
}

impl AnthropicBackend {
    /// Resolve one `skill_view` tool_use block into a `tool_result`
    /// value ready to drop into the next user turn's content array.
    ///
    /// On unknown skill name, surfaces `is_error: true` with a short
    /// message so the model can recover (try a different skill or just
    /// produce text without one).
    fn handle_tool_call(&self, tool: &WireToolUseBlock) -> Value {
        if tool.name != SKILL_VIEW_TOOL {
            // We only advertise one tool. Anything else is a
            // contract violation; surface as an error result so the
            // model abandons the line and doesn't loop on us.
            warn!(
                tool = %tool.name,
                "anthropic asked for unknown tool",
            );
            return json!({
                "type": "tool_result",
                "tool_use_id": tool.id,
                "is_error": true,
                "content": format!("unknown tool `{}`; only `skill_view` is supported", tool.name),
            });
        }

        let requested = tool.input.get("name").and_then(Value::as_str).unwrap_or("");
        if requested.is_empty() {
            warn!(
                tool = %tool.name,
                "skill_view called without a `name` parameter",
            );
            return json!({
                "type": "tool_result",
                "tool_use_id": tool.id,
                "is_error": true,
                "content": "skill_view requires a non-empty `name` parameter",
            });
        }

        match self.skills.as_ref().and_then(|r| r.find(requested)) {
            Some(skill) => {
                info!(
                    skill = %skill.name,
                    path = %skill.path.display(),
                    "evy-thinking: skill autoloaded via skill_view",
                );
                json!({
                    "type": "tool_result",
                    "tool_use_id": tool.id,
                    "content": skill.body.clone(),
                })
            }
            None => {
                warn!(
                    requested = %requested,
                    "skill_view requested unknown skill",
                );
                json!({
                    "type": "tool_result",
                    "tool_use_id": tool.id,
                    "is_error": true,
                    "content": format!("no skill named `{requested}` is registered"),
                })
            }
        }
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────

async fn body_snippet(resp: reqwest::Response) -> String {
    let body = resp.text().await.unwrap_or_default();
    body.chars().take(200).collect()
}

// ─── Anthropic wire shapes (only the fields we send / read) ──────────────

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    content: Vec<WireContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireContentBlock {
    /// `{"type": "text", "text": "..."}`
    Text { text: String },
    /// `{"type": "tool_use", "id": "...", "name": "...", "input": {...}}`
    ToolUse(WireToolUseBlock),
    /// Any other block type (`tool_result` in echoed history, image
    /// refs, …) is silently ignored.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct WireToolUseBlock {
    /// Opaque id minted by Anthropic; must be echoed back as
    /// `tool_use_id` on the corresponding `tool_result`.
    id: String,
    /// Tool name the model invoked. Only `skill_view` is advertised by
    /// this backend; anything else surfaces as an error result.
    name: String,
    /// Tool input — we only inspect `input.name` for `skill_view`.
    #[serde(default)]
    input: Value,
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
            _ => panic!("expected text block"),
        }
        assert_eq!(parsed.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn wire_response_decodes_tool_use_block() {
        let body = r#"{
            "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "skill_view",
                 "input": {"name": "demo"}}
            ],
            "stop_reason": "tool_use"
        }"#;
        let parsed: WireResponse = serde_json::from_str(body).expect("decode");
        assert_eq!(parsed.content.len(), 1);
        match &parsed.content[0] {
            WireContentBlock::ToolUse(t) => {
                assert_eq!(t.id, "toolu_1");
                assert_eq!(t.name, "skill_view");
                assert_eq!(t.input.get("name").and_then(Value::as_str), Some("demo"));
            }
            _ => panic!("expected tool_use block"),
        }
        assert_eq!(parsed.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn wire_response_skips_unknown_block_type() {
        // Future block types must decode into `Other` so we don't break
        // when Anthropic adds variants.
        let body = r#"{
            "content": [
                {"type": "text", "text": "draft"},
                {"type": "image", "source": {"type": "url", "url": "x"}}
            ]
        }"#;
        let parsed: WireResponse = serde_json::from_str(body).expect("decode");
        assert_eq!(parsed.content.len(), 2);
        let text_blocks: Vec<&str> = parsed
            .content
            .iter()
            .filter_map(|b| match b {
                WireContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text_blocks, vec!["draft"]);
    }

    #[test]
    fn wire_request_serializes_role_mapping() {
        let sid = SessionId::new();
        // System rows must be filtered BEFORE building the wire body;
        // this test asserts the filter at the call site.
        let msgs = [
            Message::new(sid, Role::Operator, "what about postgres 16?"),
            Message::new(sid, Role::Partner, "here is a draft..."),
            Message::new(sid, Role::System, "session opened"),
        ];
        let wire: Vec<Value> = msgs
            .iter()
            .filter_map(|m| match m.role {
                Role::Operator => Some(json!({"role": "user", "content": m.content.clone()})),
                Role::Partner => Some(json!({"role": "assistant", "content": m.content.clone()})),
                Role::System => None,
            })
            .collect();
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[0]["role"], "user");
        assert_eq!(wire[1]["role"], "assistant");

        let body = json!({
            "model": "m",
            "max_tokens": 100,
            "system": "sys",
            "messages": wire,
        });
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

    #[test]
    fn backend_without_skills_renders_no_tools_and_passes_system_through() {
        let b = AnthropicBackend::new(AnthropicConfig::new("k"));
        assert!(!b.has_skills());
        assert!(b.tools_for_request().is_none());
        let composed = b.compose_system_prompt("operator says hi");
        assert_eq!(composed, "operator says hi");
    }

    #[test]
    fn backend_with_empty_registry_skips_tools_advertisement() {
        // Guard against shipping a tool the model can never use
        // successfully — an empty registry would make every
        // `skill_view` call return is_error. Prevent the model from
        // even seeing the tool in that case.
        use evy_skills::SkillRegistry;
        let dir = tempfile::tempdir().unwrap();
        let empty = Arc::new(SkillRegistry::load(dir.path()).unwrap());
        let b = AnthropicBackend::new(AnthropicConfig::new("k")).with_skills(empty);
        assert!(b.has_skills());
        assert!(
            b.tools_for_request().is_none(),
            "empty registry must not advertise skill_view"
        );
        assert_eq!(b.compose_system_prompt("plain"), "plain");
    }
}
