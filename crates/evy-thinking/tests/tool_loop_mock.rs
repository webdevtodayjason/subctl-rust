//! EA1 — hermetic integration tests for the backend-neutral agency
//! tool loop. Mirrors the wiremock pattern in `skill_autoload.rs`:
//! mock servers stand in for Anthropic / LM Studio, canned tool-call
//! responses drive the round-trip loop, and we assert on the exact
//! wire bodies the backends send back — execution, result echo, and
//! cap behaviour.
//!
//! The audit-line capture assertion lives in its own binary
//! (`tool_audit_trace.rs`): several tests here hit the registry's
//! audit callsite concurrently, and tracing's per-callsite interest
//! cache races thread-local `set_default` subscribers when a shared
//! callsite fires on subscriber-less threads in parallel.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use evy_thinking::{
    AnthropicBackend, AnthropicConfig, EvyTool, LlmBackend, LmStudioBackend, LmStudioConfig,
    Message, Role, SessionId, StreamChunk, ThinkingError, ToolRegistry, ToolSpec,
    DEFAULT_MAX_TOKENS, DEFAULT_MODEL, MAX_TOOL_ROUNDTRIPS,
};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ─── shared fixtures ────────────────────────────────────────────────────

/// Canned read-only tool: `evy_usage` returning a fixed summary.
struct FakeUsage;

#[async_trait]
impl EvyTool for FakeUsage {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "evy_usage".into(),
            description: "per-account usage summary".into(),
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }
    }
    async fn execute(&self, _input: &Value) -> Result<String, String> {
        Ok("claude-a: 7d 46%; claude-b: rate-limited".into())
    }
}

/// Canned failing tool — proves executor errors surface as recoverable
/// error results, not transport failures.
struct FakeBroken;

#[async_trait]
impl EvyTool for FakeBroken {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "evy_broken".into(),
            description: "always fails".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    async fn execute(&self, _input: &Value) -> Result<String, String> {
        Err("data source offline".into())
    }
}

fn registry() -> Arc<ToolRegistry> {
    Arc::new(
        ToolRegistry::new()
            .with_tool(Arc::new(FakeUsage))
            .with_tool(Arc::new(FakeBroken)),
    )
}

fn anthropic_cfg(server_url: &str) -> AnthropicConfig {
    AnthropicConfig {
        api_base: server_url.trim_end_matches('/').to_string(),
        api_key: "test-key".to_string(),
        model: DEFAULT_MODEL.to_string(),
        max_tokens: DEFAULT_MAX_TOKENS,
        timeout: Duration::from_secs(5),
    }
}

fn lm_cfg(server_url: &str, tools_enabled: bool) -> LmStudioConfig {
    LmStudioConfig {
        endpoint: server_url.trim_end_matches('/').to_string(),
        tools_enabled,
        ..LmStudioConfig::default()
    }
}

fn operator_turn(text: &str) -> Vec<Message> {
    vec![Message::new(SessionId::new(), Role::Operator, text)]
}

// ─── anthropic ──────────────────────────────────────────────────────────

#[tokio::test]
async fn anthropic_agency_tool_round_trip_executes_and_echoes() {
    let server = MockServer::start().await;

    // First request → model asks for evy_usage.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [
                {"type": "tool_use", "id": "toolu_u1", "name": "evy_usage", "input": {}}
            ],
            "stop_reason": "tool_use"
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    // Second request → plain text built on the tool result.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "claude-b is rate-limited; use claude-a."}],
            "stop_reason": "end_turn"
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;

    let backend = AnthropicBackend::new(anthropic_cfg(&server.uri())).with_tools(registry());

    let out = backend
        .respond("SYS", &operator_turn("how are the accounts?"))
        .await
        .expect("respond ok");
    assert_eq!(out, "claude-b is rate-limited; use claude-a.");

    let recv = server.received_requests().await.expect("recorded");
    assert_eq!(recv.len(), 2);

    // First request advertises both agency tools (no skills attached).
    let body1: Value = serde_json::from_slice(&recv[0].body).expect("json");
    let tool_names: Vec<&str> = body1["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(tool_names, vec!["evy_usage", "evy_broken"]);

    // Second request: assistant tool_use echoed, then the tool_result
    // carrying the executor's exact output, no is_error.
    let body2: Value = serde_json::from_slice(&recv[1].body).expect("json");
    let msgs2 = body2["messages"].as_array().expect("messages");
    assert_eq!(
        msgs2.len(),
        3,
        "operator + assistant tool_use + tool_result"
    );
    assert_eq!(msgs2[1]["role"], "assistant");
    assert_eq!(msgs2[1]["content"][0]["name"], "evy_usage");
    let result = &msgs2[2]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "toolu_u1");
    assert_eq!(
        result["content"],
        "claude-a: 7d 46%; claude-b: rate-limited"
    );
    assert!(result.get("is_error").is_none());
}

#[tokio::test]
async fn anthropic_executor_error_surfaces_as_error_tool_result() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [
                {"type": "tool_use", "id": "toolu_b1", "name": "evy_broken", "input": {}}
            ],
            "stop_reason": "tool_use"
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "the data source is down, sorry."}],
            "stop_reason": "end_turn"
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let backend = AnthropicBackend::new(anthropic_cfg(&server.uri())).with_tools(registry());
    let out = backend
        .respond("SYS", &operator_turn("usage?"))
        .await
        .expect("respond ok — executor errors are recoverable");
    assert_eq!(out, "the data source is down, sorry.");

    let recv = server.received_requests().await.expect("recorded");
    let body2: Value = serde_json::from_slice(&recv[1].body).expect("json");
    let result = &body2["messages"][2]["content"][0];
    assert_eq!(result["is_error"], true);
    assert_eq!(result["content"], "data source offline");
}

#[tokio::test]
async fn anthropic_tool_loop_caps_pathological_looping() {
    let server = MockServer::start().await;
    // Every request gets another tool_use — the model never converges.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [
                {"type": "tool_use", "id": "toolu_loop", "name": "evy_usage", "input": {}}
            ],
            "stop_reason": "tool_use"
        })))
        .mount(&server)
        .await;

    let backend = AnthropicBackend::new(anthropic_cfg(&server.uri())).with_tools(registry());
    let err = backend
        .respond("SYS", &operator_turn("usage?"))
        .await
        .expect_err("must hit the cap");
    match err {
        ThinkingError::BackendRefused(msg) => {
            assert!(msg.contains("tool loop exceeded"), "got: {msg}");
        }
        other => panic!("expected BackendRefused, got {other:?}"),
    }

    // Cap semantics: rounds 0..MAX take a tool turn each, the final
    // round detects non-convergence — MAX+1 requests total.
    let recv = server.received_requests().await.expect("recorded");
    assert_eq!(recv.len(), MAX_TOOL_ROUNDTRIPS + 1);
}

#[tokio::test]
async fn anthropic_capability_brief_reflects_registry() {
    let with_tools =
        AnthropicBackend::new(anthropic_cfg("http://127.0.0.1:1")).with_tools(registry());
    let brief = with_tools.capability_brief().expect("brief present");
    assert!(brief.contains("evy_usage") && brief.contains("evy_broken"));
    assert!(brief.contains("NEVER invent"), "got: {brief}");

    let without = AnthropicBackend::new(anthropic_cfg("http://127.0.0.1:1"));
    assert!(without.capability_brief().is_none());

    let empty = AnthropicBackend::new(anthropic_cfg("http://127.0.0.1:1"))
        .with_tools(Arc::new(ToolRegistry::new()));
    assert!(
        empty.capability_brief().is_none(),
        "empty registry must not claim tool capability"
    );
}

// ─── lm-studio ──────────────────────────────────────────────────────────

fn lm_text_response(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "choices": [
            {"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}
        ]
    }))
}

#[tokio::test]
async fn lm_studio_flag_off_sends_no_tools_field_on_the_wire() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(lm_text_response("plain reply"))
        .expect(1)
        .mount(&server)
        .await;

    // Registry attached but flag OFF — the v0.5.0 wire shape must hold.
    let backend = LmStudioBackend::new(lm_cfg(&server.uri(), false)).with_tools(registry());
    let out = backend
        .respond("SYS", &operator_turn("hello"))
        .await
        .expect("ok");
    assert_eq!(out, "plain reply");

    let recv = server.received_requests().await.expect("recorded");
    let body: Value = serde_json::from_slice(&recv[0].body).expect("json");
    assert!(
        body.get("tools").is_none(),
        "tools must be absent with the flag off; got {body}"
    );
}

#[tokio::test]
async fn lm_studio_flag_on_drives_tool_calls_round_trip() {
    let server = MockServer::start().await;

    // First request → the model calls evy_usage (OpenAI tool_calls shape,
    // arguments as a JSON-encoded string, content null).
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_u1",
                        "type": "function",
                        "function": {"name": "evy_usage", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    // Second request → final text.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(lm_text_response("claude-a has headroom."))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(lm_cfg(&server.uri(), true)).with_tools(registry());
    let out = backend
        .respond("SYS", &operator_turn("which account has headroom?"))
        .await
        .expect("ok");
    assert_eq!(out, "claude-a has headroom.");

    let recv = server.received_requests().await.expect("recorded");
    assert_eq!(recv.len(), 2);

    // First request advertises the tools in OpenAI function shape.
    let body1: Value = serde_json::from_slice(&recv[0].body).expect("json");
    let tools = body1["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "evy_usage");

    // Second request replays the assistant tool_calls turn and carries
    // the executor output as a role:"tool" message echoing the call id.
    let body2: Value = serde_json::from_slice(&recv[1].body).expect("json");
    let msgs = body2["messages"].as_array().expect("messages");
    // system + user + assistant(tool_calls) + tool
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[2]["role"], "assistant");
    assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_u1");
    assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "evy_usage");
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "call_u1");
    assert_eq!(
        msgs[3]["content"],
        "claude-a: 7d 46%; claude-b: rate-limited"
    );
}

#[tokio::test]
async fn lm_studio_executor_error_renders_as_visible_error_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_b1",
                        "type": "function",
                        "function": {"name": "evy_broken", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(lm_text_response("source is down."))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(lm_cfg(&server.uri(), true)).with_tools(registry());
    let out = backend
        .respond("SYS", &operator_turn("usage?"))
        .await
        .expect("ok");
    assert_eq!(out, "source is down.");

    let recv = server.received_requests().await.expect("recorded");
    let body2: Value = serde_json::from_slice(&recv[1].body).expect("json");
    let tool_msg = &body2["messages"][3];
    assert_eq!(tool_msg["role"], "tool");
    assert_eq!(tool_msg["content"], "ERROR: data source offline");
}

#[tokio::test]
async fn lm_studio_tool_loop_caps_pathological_looping() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_loop",
                        "type": "function",
                        "function": {"name": "evy_usage", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })))
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(lm_cfg(&server.uri(), true)).with_tools(registry());
    let err = backend
        .respond("SYS", &operator_turn("usage?"))
        .await
        .expect_err("must hit the cap");
    assert!(
        matches!(&err, ThinkingError::BackendRefused(m) if m.contains("tool loop exceeded")),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn lm_studio_stream_respond_with_tools_active_emits_single_chunk() {
    // With tools active, streaming delegates to the blocking loop and
    // emits the assembled text as one Token — assert that contract.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(lm_text_response("one-shot reply"))
        .expect(1)
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(lm_cfg(&server.uri(), true)).with_tools(registry());
    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamChunk>(4);
    let out = backend
        .stream_respond("SYS", &operator_turn("hi"), &tx)
        .await
        .expect("ok");
    assert_eq!(out, "one-shot reply");
    drop(tx);
    let frame = rx.recv().await.expect("one chunk emitted");
    assert!(matches!(frame, StreamChunk::Token(t) if t == "one-shot reply"));
    assert!(rx.recv().await.is_none(), "exactly one chunk");

    // The request that went out was the blocking (stream: false) shape.
    let recv = server.received_requests().await.expect("recorded");
    let body: Value = serde_json::from_slice(&recv[0].body).expect("json");
    assert_eq!(body["stream"], false);
    assert!(body.get("tools").is_some());
}
