//! LM Studio-backed [`LlmBackend`] — talks to the operator's locally-
//! running LM Studio at `POST {endpoint}/v1/chat/completions`.
//!
//! LM Studio exposes an **OpenAI-compatible** chat completions API on
//! `127.0.0.1:1234` by default. No auth, no API key, just HTTP. This is
//! the recommended default for first-contact chat testing — local, free,
//! private, sub-second on the operator's hardware (Gemma-class models on
//! Apple Silicon).
//!
//! ## Wire shape
//!
//! Request (only the fields we send):
//!
//! ```json
//! {
//!   "model": "gemma-4-26b-a4b-it-mlx",
//!   "messages": [
//!     {"role": "system",    "content": "<planning_system_prompt(topic)>"},
//!     {"role": "user",      "content": "..."},
//!     {"role": "assistant", "content": "..."}
//!   ],
//!   "temperature": 0.7,
//!   "max_tokens": 2048,
//!   "stream": false
//! }
//! ```
//!
//! `model` is **omitted entirely** from the wire body when
//! [`LmStudioConfig::model`] is `None`. LM Studio falls through to
//! whichever model is currently loaded — exactly what the operator
//! wants when they've already selected one in the LM Studio UI.
//!
//! Response (only the fields we read):
//!
//! ```json
//! {
//!   "id": "chatcmpl-...",
//!   "object": "chat.completion",
//!   "choices": [
//!     {"message": {"role": "assistant", "content": "..."},
//!      "finish_reason": "stop"}
//!   ],
//!   "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
//! }
//! ```
//!
//! ## Role mapping
//!
//! Unlike Anthropic, OpenAI-compatible APIs put the system prompt in
//! the `messages` array as a `{"role": "system"}` entry. We prepend it.
//!
//! [`crate::Role::System`] entries that come from session scaffolding
//! ("session opened" markers, etc.) are still **skipped** — only the
//! caller-supplied `system_prompt` argument becomes a `system` wire
//! turn. Operator → `user`; Partner → `assistant`. Same filter rule as
//! [`crate::anthropic`], different rendering target.
//!
//! ## Tool use / skill autoload
//!
//! Out of scope for v0.5.0. LM Studio's OpenAI-compat layer accepts
//! `tools[]`, but Gemma-class local models in the operator's catalog
//! don't reliably honour the tool-call contract. Skill autoload remains
//! an Anthropic-only feature for now; local-model operators can still
//! reference skills by quoting them inline.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::backend::{LlmBackend, StreamChunk};
use crate::error::{Result, ThinkingError};
use crate::session::{Message, Role};

/// Default endpoint LM Studio binds when "Local Server" is enabled in
/// its UI. Loopback-only, no auth.
pub const DEFAULT_LM_STUDIO_ENDPOINT: &str = "http://127.0.0.1:1234";

/// Default `max_tokens` for partner replies. Smaller than the Anthropic
/// default because local-model throughput is the bottleneck — a 2048-
/// token reply on Apple Silicon is already 30-60s; pushing higher
/// pessimises the operator chat UX with no perceptible quality gain on
/// Gemma-class models.
pub const DEFAULT_MAX_TOKENS: u32 = 2048;

/// Default sampling temperature. `0.7` is the OpenAI / LM Studio default
/// and matches the operator's expectations for "balanced" output.
pub const DEFAULT_TEMPERATURE: f32 = 0.7;

/// Default per-request transport timeout. Generous because local-model
/// inference latency varies widely with model size and prompt length;
/// a 26B-class model can take 60s+ for a long reply on Apple Silicon.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Static configuration for [`LmStudioBackend`].
#[derive(Debug, Clone)]
pub struct LmStudioConfig {
    /// Base URL for the LM Studio server. Defaults to
    /// [`DEFAULT_LM_STUDIO_ENDPOINT`]. Tests override this to point at
    /// a `wiremock` `MockServer::uri()`.
    pub endpoint: String,
    /// Optional model identifier. When `None`, the wire body omits the
    /// `model` field entirely and LM Studio falls through to whichever
    /// model is currently loaded. Set when the operator has multiple
    /// models loaded and wants to pin one.
    pub model: Option<String>,
    /// Output token cap sent in the request body. Defaults to
    /// [`DEFAULT_MAX_TOKENS`].
    pub max_tokens: u32,
    /// Sampling temperature. Defaults to [`DEFAULT_TEMPERATURE`].
    pub temperature: f32,
    /// Per-request transport timeout. Defaults to [`DEFAULT_TIMEOUT`].
    pub timeout: Duration,
}

impl Default for LmStudioConfig {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_LM_STUDIO_ENDPOINT.to_string(),
            model: None,
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: DEFAULT_TEMPERATURE,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// LM Studio-backed thinking-partner LLM client.
///
/// Cheap to construct; holds a single shared `reqwest::Client` so
/// connections / keepalives are reused across turns. No auth state to
/// manage — LM Studio's local endpoint is unauthenticated by design.
pub struct LmStudioBackend {
    config: LmStudioConfig,
    http: Client,
}

impl LmStudioBackend {
    /// Build a backend with the supplied config.
    ///
    /// The `reqwest::Client` builder is infallible for the feature flags
    /// this crate enables (`rustls-tls` + `json`), matching the pattern
    /// in [`crate::anthropic::AnthropicBackend::new`].
    #[must_use]
    pub fn new(config: LmStudioConfig) -> Self {
        let http = Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("reqwest client builder is infallible in this config");
        Self { config, http }
    }

    /// The configured endpoint — exposed for diagnostics + tests.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.config.endpoint
    }

    /// The configured model override, if any — exposed for diagnostics.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.config.model.as_deref()
    }

    /// Probe `GET {endpoint}/v1/models` to verify the server is
    /// reachable and willing to talk.
    ///
    /// Semantics:
    /// - 2xx response → `Ok(true)` (LM Studio is up and responding)
    /// - non-2xx response → `Ok(false)` (server reachable but unhappy;
    ///   e.g. local server toggled off in LM Studio's UI returns 404)
    /// - transport failure (connection refused, DNS, timeout) →
    ///   `Err(ThinkingError::Transport(_))`
    ///
    /// Cheap (no body decode); call it at daemon boot to decide whether
    /// to log a "LM Studio not reachable" warning.
    ///
    /// # Errors
    /// See above — only transport-level failures bubble up.
    pub async fn health(&self) -> Result<bool> {
        let url = format!("{}/v1/models", self.endpoint_trimmed());
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| ThinkingError::Transport(e.without_url().to_string()))?;
        Ok(resp.status().is_success())
    }

    fn endpoint_trimmed(&self) -> &str {
        self.config.endpoint.trim_end_matches('/')
    }

    fn chat_url(&self) -> String {
        format!("{}/v1/chat/completions", self.endpoint_trimmed())
    }

    /// Compose the JSON request body. Extracted so tests can golden-
    /// assert the wire shape without spinning up a mock server.
    ///
    /// System prompt is rendered as a leading `{"role": "system"}` entry
    /// in `messages[]` — this is the #1 difference from the Anthropic
    /// envelope, where `system` is a sibling field.
    ///
    /// `stream` is set per-call: blocking [`LlmBackend::respond`] sends
    /// `false`, streaming [`LlmBackend::stream_respond`] sends `true`
    /// so LM Studio emits `data: {...}\n\n` chunks instead of a single
    /// JSON envelope.
    fn build_request_body_with_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
        stream: bool,
    ) -> Value {
        let mut wire_messages: Vec<WireMessage> = Vec::with_capacity(messages.len() + 1);
        wire_messages.push(WireMessage {
            role: "system",
            content: system_prompt.to_string(),
        });
        for m in messages {
            match m.role {
                Role::Operator => wire_messages.push(WireMessage {
                    role: "user",
                    content: m.content.clone(),
                }),
                Role::Partner => wire_messages.push(WireMessage {
                    role: "assistant",
                    content: m.content.clone(),
                }),
                // Surface-side scaffolding ("session opened" etc.) — the
                // wire system role is reserved for `system_prompt`.
                Role::System => {}
            }
        }

        let body = WireRequest {
            model: self.config.model.as_deref(),
            messages: wire_messages,
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            stream,
        };
        // Serialise through serde_json::Value so the public surface (and
        // the tests) can introspect individual fields without rebuilding
        // the body. `to_value` is infallible for `Serialize` types whose
        // fields are all themselves `Serialize` — which ours are.
        serde_json::to_value(&body).expect("WireRequest is always serializable")
    }

    /// Backwards-compatible shim used by [`LlmBackend::respond`] — always
    /// builds a non-streaming body.
    fn build_request_body(&self, system_prompt: &str, messages: &[Message]) -> Value {
        self.build_request_body_with_stream(system_prompt, messages, false)
    }
}

/// Outcome of feeding one SSE `data:` line into the streaming parser.
#[derive(Debug)]
enum SseAction {
    /// New token text to forward to the sink.
    Token(String),
    /// `data: [DONE]` sentinel — the stream is finished.
    Done,
    /// Frame parsed but carried nothing renderable (e.g. role-only
    /// preamble, finish_reason without delta content).
    Skip,
}

/// Parse one OpenAI-compat streaming `data: ...` payload.
///
/// LM Studio sends `data: [DONE]` as its terminator and `data: {...}`
/// chunks of the shape `{"choices":[{"delta":{"content":"x"}}]}`. Each
/// chunk may carry zero or more content characters; role-only previews
/// and finish-reason finalisers carry no content.
fn parse_sse_payload(payload: &str) -> std::result::Result<SseAction, ThinkingError> {
    let payload = payload.trim();
    if payload == "[DONE]" {
        return Ok(SseAction::Done);
    }
    let parsed: WireStreamChunk = serde_json::from_str(payload)
        .map_err(|e| ThinkingError::Decode(format!("lm-studio stream chunk: {e}")))?;
    let text = parsed
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.delta.content)
        .unwrap_or_default();
    if text.is_empty() {
        Ok(SseAction::Skip)
    } else {
        Ok(SseAction::Token(text))
    }
}

#[async_trait]
impl LlmBackend for LmStudioBackend {
    async fn respond(&self, system_prompt: &str, messages: &[Message]) -> Result<String> {
        let body = self.build_request_body(system_prompt, messages);

        debug!(
            endpoint = %self.config.endpoint,
            model = ?self.config.model,
            turns = messages.len(),
            "evy-thinking: lm-studio respond",
        );

        let resp = self
            .http
            .post(self.chat_url())
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ThinkingError::Transport(e.without_url().to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let snippet = body_snippet(resp).await;
            warn!(status, snippet = %snippet, "lm-studio non-2xx");
            return Err(ThinkingError::HttpStatus { status, snippet });
        }

        let parsed: WireResponse = resp
            .json()
            .await
            .map_err(|e| ThinkingError::Decode(e.without_url().to_string()))?;

        let text = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        if text.trim().is_empty() {
            return Err(ThinkingError::BackendRefused(
                "lm-studio returned no choice content".to_string(),
            ));
        }
        Ok(text)
    }

    /// Native OpenAI-compat streaming. Sends `stream: true` and parses
    /// `data: {...}\n\n` frames from the body byte stream, forwarding
    /// each delta as a `StreamChunk::Token` and returning the
    /// concatenated text once the upstream sends `data: [DONE]`.
    ///
    /// Sink-send failures are non-fatal (the client disconnected) — we
    /// stop forwarding but keep draining the upstream until the SSE
    /// frame's terminal sentinel so the server-side reqwest connection
    /// closes cleanly.
    async fn stream_respond(
        &self,
        system_prompt: &str,
        messages: &[Message],
        sink: &mpsc::Sender<StreamChunk>,
    ) -> Result<String> {
        let body = self.build_request_body_with_stream(system_prompt, messages, true);

        debug!(
            endpoint = %self.config.endpoint,
            model = ?self.config.model,
            turns = messages.len(),
            "evy-thinking: lm-studio stream_respond",
        );

        let resp = self
            .http
            .post(self.chat_url())
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| ThinkingError::Transport(e.without_url().to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let snippet = body_snippet(resp).await;
            warn!(status, snippet = %snippet, "lm-studio stream non-2xx");
            return Err(ThinkingError::HttpStatus { status, snippet });
        }

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut assembled = String::new();
        // Sink may have closed; once `false` we stop forwarding but
        // keep draining so the upstream connection terminates cleanly.
        let mut sink_alive = true;

        'outer: while let Some(item) = stream.next().await {
            let bytes =
                item.map_err(|e| ThinkingError::Transport(format!("lm-studio stream: {e}")))?;
            // Each chunk is utf8 — LM Studio sends only ASCII envelopes.
            // We tolerate a partial multi-byte char straddling chunks by
            // using from_utf8_lossy and re-buffering at the line level.
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            // Pull complete lines off the front of the buffer; OpenAI
            // streaming separates events with "\n\n" but individual
            // `data:` lines end at "\n", so a line-based scan works.
            while let Some(newline) = buffer.find('\n') {
                let line = buffer[..newline].trim().to_string();
                buffer.drain(..=newline);
                if line.is_empty() {
                    continue;
                }
                let payload = match line.strip_prefix("data:") {
                    Some(rest) => rest.trim(),
                    None => continue, // ignore comments / unknown event fields
                };
                match parse_sse_payload(payload)? {
                    SseAction::Token(tok) => {
                        assembled.push_str(&tok);
                        if sink_alive && sink.send(StreamChunk::Token(tok)).await.is_err() {
                            sink_alive = false;
                        }
                    }
                    SseAction::Done => {
                        break 'outer;
                    }
                    SseAction::Skip => {}
                }
            }
        }

        if assembled.trim().is_empty() {
            return Err(ThinkingError::BackendRefused(
                "lm-studio stream produced no content before terminating".to_string(),
            ));
        }
        Ok(assembled)
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────

async fn body_snippet(resp: reqwest::Response) -> String {
    let body = resp.text().await.unwrap_or_default();
    body.chars().take(200).collect()
}

// ─── LM Studio wire shapes (only the fields we send / read) ──────────────

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    /// Model identifier. `None` → field is omitted from the wire body so
    /// LM Studio falls through to its currently-loaded model.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    messages: Vec<WireMessage>,
    temperature: f32,
    max_tokens: u32,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct WireMessage {
    role: &'static str,
    content: String,
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    choices: Vec<WireChoice>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    message: WireChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct WireChoiceMessage {
    #[serde(default)]
    content: String,
}

/// Streaming chunk shape. Each frame carries `choices[0].delta.content`
/// when present; the role-only preamble and finish-reason finalisers
/// arrive with `delta` absent or `delta.content == None`. We tolerate
/// both via `Option`.
#[derive(Debug, Deserialize)]
struct WireStreamChunk {
    #[serde(default)]
    choices: Vec<WireStreamChoice>,
}

#[derive(Debug, Deserialize)]
struct WireStreamChoice {
    #[serde(default)]
    delta: WireStreamDelta,
}

#[derive(Debug, Default, Deserialize)]
struct WireStreamDelta {
    #[serde(default)]
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionId;

    fn cfg() -> LmStudioConfig {
        LmStudioConfig::default()
    }

    #[test]
    fn config_defaults_point_at_loopback_1234() {
        let c = cfg();
        assert_eq!(c.endpoint, "http://127.0.0.1:1234");
        assert!(c.model.is_none());
        assert_eq!(c.max_tokens, DEFAULT_MAX_TOKENS);
        assert!((c.temperature - DEFAULT_TEMPERATURE).abs() < f32::EPSILON);
        assert!(c.timeout > Duration::ZERO);
    }

    #[test]
    fn chat_url_appends_v1_chat_completions_without_double_slash() {
        let b = LmStudioBackend::new(LmStudioConfig {
            endpoint: "http://127.0.0.1:1234/".to_string(),
            ..cfg()
        });
        assert_eq!(b.chat_url(), "http://127.0.0.1:1234/v1/chat/completions");

        let b2 = LmStudioBackend::new(LmStudioConfig {
            endpoint: "http://127.0.0.1:1234".to_string(),
            ..cfg()
        });
        assert_eq!(b2.chat_url(), "http://127.0.0.1:1234/v1/chat/completions");
    }

    #[test]
    fn request_body_renders_system_as_first_messages_entry() {
        // The #1 mistake to avoid: OpenAI-compat puts system in
        // messages[], NOT at top level. Lock the wire shape down.
        let b = LmStudioBackend::new(cfg());
        let sid = SessionId::new();
        let msgs = [
            Message::new(sid, Role::Operator, "hello"),
            Message::new(sid, Role::Partner, "world"),
        ];
        let body = b.build_request_body("you are evy", &msgs);

        let wire_msgs = body["messages"].as_array().expect("messages array");
        assert_eq!(wire_msgs.len(), 3);
        assert_eq!(wire_msgs[0]["role"], "system");
        assert_eq!(wire_msgs[0]["content"], "you are evy");
        assert_eq!(wire_msgs[1]["role"], "user");
        assert_eq!(wire_msgs[1]["content"], "hello");
        assert_eq!(wire_msgs[2]["role"], "assistant");
        assert_eq!(wire_msgs[2]["content"], "world");
    }

    #[test]
    fn request_body_filters_session_system_rows_from_wire() {
        // `Role::System` entries on the session are surface scaffolding
        // ("session opened" markers etc.) — they must NOT appear on the
        // wire. Only the caller-supplied `system_prompt` becomes the
        // `role: "system"` wire turn.
        let b = LmStudioBackend::new(cfg());
        let sid = SessionId::new();
        let msgs = [
            Message::new(sid, Role::System, "session opened"),
            Message::new(sid, Role::Operator, "hi"),
        ];
        let body = b.build_request_body("sys", &msgs);
        let wire_msgs = body["messages"].as_array().expect("messages array");
        assert_eq!(wire_msgs.len(), 2, "session-system row must be filtered");
        assert_eq!(wire_msgs[0]["role"], "system");
        assert_eq!(wire_msgs[1]["role"], "user");
    }

    #[test]
    fn request_body_omits_model_when_none() {
        // `model: null` would be undefined behaviour — LM Studio expects
        // either a real name OR the field absent. Verify absent.
        let b = LmStudioBackend::new(LmStudioConfig {
            model: None,
            ..cfg()
        });
        let body = b.build_request_body("sys", &[]);
        assert!(
            body.get("model").is_none(),
            "model field must be absent when config.model is None; got {body}"
        );
    }

    #[test]
    fn request_body_includes_model_when_set() {
        let b = LmStudioBackend::new(LmStudioConfig {
            model: Some("gemma-4-26b-a4b-it-mlx".to_string()),
            ..cfg()
        });
        let body = b.build_request_body("sys", &[]);
        assert_eq!(body["model"], "gemma-4-26b-a4b-it-mlx");
    }

    #[test]
    fn request_body_carries_temperature_max_tokens_stream_false() {
        let b = LmStudioBackend::new(LmStudioConfig {
            max_tokens: 1024,
            temperature: 0.3,
            ..cfg()
        });
        let body = b.build_request_body("sys", &[]);
        assert_eq!(body["max_tokens"], 1024);
        // f64 comparison via serde_json — round-trip preserves f32.
        let temp = body["temperature"].as_f64().expect("temp f64");
        assert!((temp - 0.3).abs() < 1e-6, "got {temp}");
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn wire_response_decodes_minimal_chat_completion() {
        let raw = r#"{
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "choices": [
                {"message": {"role": "assistant", "content": "hello back"},
                 "finish_reason": "stop"}
            ]
        }"#;
        let parsed: WireResponse = serde_json::from_str(raw).expect("decode");
        assert_eq!(parsed.choices.len(), 1);
        assert_eq!(parsed.choices[0].message.content, "hello back");
    }

    #[test]
    fn wire_response_decodes_with_empty_choices_array() {
        // Edge case: a misbehaving server returns an empty choices
        // array. The backend translates this to BackendRefused at the
        // respond() layer, but the decode itself must succeed.
        let raw = r#"{"id": "x", "object": "chat.completion", "choices": []}"#;
        let parsed: WireResponse = serde_json::from_str(raw).expect("decode");
        assert_eq!(parsed.choices.len(), 0);
    }

    #[test]
    fn streaming_body_carries_stream_true() {
        let b = LmStudioBackend::new(cfg());
        let body = b.build_request_body_with_stream("sys", &[], true);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn parse_sse_payload_returns_done_for_sentinel() {
        match parse_sse_payload("[DONE]").unwrap() {
            SseAction::Done => {}
            other => panic!("expected Done, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn parse_sse_payload_extracts_delta_content() {
        let raw = r#"{"choices":[{"delta":{"content":"hi"}}]}"#;
        match parse_sse_payload(raw).unwrap() {
            SseAction::Token(t) => assert_eq!(t, "hi"),
            _ => panic!("expected token"),
        }
    }

    #[test]
    fn parse_sse_payload_returns_skip_when_delta_empty() {
        // Role-only preamble — `delta.content` is None.
        let raw = r#"{"choices":[{"delta":{"role":"assistant"}}]}"#;
        match parse_sse_payload(raw).unwrap() {
            SseAction::Skip => {}
            _ => panic!("expected skip"),
        }
    }

    #[test]
    fn parse_sse_payload_returns_skip_when_no_choices() {
        // Some servers emit terminal frames with empty choices.
        let raw = r#"{"choices":[]}"#;
        match parse_sse_payload(raw).unwrap() {
            SseAction::Skip => {}
            _ => panic!("expected skip"),
        }
    }

    #[test]
    fn parse_sse_payload_decode_error_surfaces_as_thinking_error() {
        let err = parse_sse_payload("not json").unwrap_err();
        assert!(matches!(err, ThinkingError::Decode(_)));
    }

    #[tokio::test]
    async fn stream_respond_default_impl_inherited_by_other_backends_compiles() {
        // Compile-time check that the trait method has a default impl —
        // we instantiate a tiny test backend that only implements
        // `respond` and confirm `stream_respond` is reachable via
        // dynamic dispatch.
        struct Stub;
        #[async_trait]
        impl LlmBackend for Stub {
            async fn respond(&self, _s: &str, _m: &[Message]) -> Result<String> {
                Ok("hi".into())
            }
        }
        let s: Box<dyn LlmBackend> = Box::new(Stub);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamChunk>(4);
        let text = s.stream_respond("sys", &[], &tx).await.unwrap();
        assert_eq!(text, "hi");
        drop(tx);
        let frame = rx.recv().await.expect("default-impl emits one chunk");
        assert!(matches!(frame, StreamChunk::Token(t) if t == "hi"));
        assert!(rx.recv().await.is_none());
    }
}
