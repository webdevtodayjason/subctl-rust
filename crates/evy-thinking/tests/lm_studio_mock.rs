//! Integration tests for [`evy_thinking::LmStudioBackend`] against a
//! `wiremock` mock of LM Studio's OpenAI-compatible chat completions
//! API. The real `http://127.0.0.1:1234` is NEVER hit by these tests —
//! [`LmStudioConfig::endpoint`] is overridden to point at the mock.
//!
//! A separate `#[ignore]`'d test ([`real_lm_studio_smoke`]) at the
//! bottom hits the operator's actual local LM Studio server; run it
//! manually with `cargo test --package evy-thinking -- --ignored
//! real_lm_studio` when verifying end-to-end against a live model.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use evy_thinking::{
    LlmBackend, LmStudioBackend, LmStudioConfig, Message, Role, SessionId, StreamChunk,
    ThinkingError,
};

fn cfg(server_url: &str) -> LmStudioConfig {
    LmStudioConfig {
        endpoint: server_url.trim_end_matches('/').to_string(),
        model: None,
        max_tokens: 256,
        temperature: 0.7,
        timeout: Duration::from_secs(5),
    }
}

fn chat_response(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "choices": [
            {"message": {"role": "assistant", "content": text},
             "finish_reason": "stop"}
        ],
        "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
    }))
}

#[tokio::test]
async fn backend_sends_required_content_type_and_body_shape() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("content-type", "application/json"))
        .respond_with(chat_response("ok"))
        .expect(1)
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(cfg(&server.uri()));
    let sid = SessionId::new();
    let msgs = vec![Message::new(sid, Role::Operator, "first question")];
    let out = backend
        .respond("you are evy", &msgs)
        .await
        .expect("respond ok");
    assert_eq!(out, "ok");
}

#[tokio::test]
async fn backend_renders_system_prompt_into_messages_array() {
    // The #1 OpenAI-vs-Anthropic mistake: system must be inside
    // messages[], NOT a sibling field at the top level. Lock it down.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(chat_response("ack"))
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(cfg(&server.uri()));
    let sid = SessionId::new();
    let msgs = vec![
        Message::new(sid, Role::Operator, "what's up"),
        Message::new(sid, Role::Partner, "running low on ice"),
    ];
    backend.respond("sys-prompt-here", &msgs).await.expect("ok");

    let recv = server.received_requests().await.expect("recorded");
    assert_eq!(recv.len(), 1);
    let body: Value = serde_json::from_slice(&recv[0].body).expect("json body");

    // Top-level `system` MUST be absent — OpenAI-compat doesn't use it.
    assert!(
        body.get("system").is_none(),
        "OpenAI-compat puts system in messages[], not at top level; got {body}"
    );

    let wire_msgs = body["messages"].as_array().expect("messages array");
    assert_eq!(wire_msgs.len(), 3);
    assert_eq!(wire_msgs[0]["role"], "system");
    assert_eq!(wire_msgs[0]["content"], "sys-prompt-here");
    assert_eq!(wire_msgs[1]["role"], "user");
    assert_eq!(wire_msgs[1]["content"], "what's up");
    assert_eq!(wire_msgs[2]["role"], "assistant");
    assert_eq!(wire_msgs[2]["content"], "running low on ice");
}

#[tokio::test]
async fn backend_omits_model_field_when_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(chat_response("ack"))
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(LmStudioConfig {
        model: None,
        ..cfg(&server.uri())
    });
    backend.respond("sys", &[]).await.expect("ok");

    let recv = server.received_requests().await.expect("recorded");
    let body: Value = serde_json::from_slice(&recv[0].body).expect("json body");
    assert!(
        body.get("model").is_none(),
        "model must be absent on wire when config.model is None; got {body}"
    );
    assert_eq!(body["stream"], false);
    assert_eq!(body["max_tokens"], 256);
}

#[tokio::test]
async fn backend_sends_model_when_configured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(chat_response("ack"))
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(LmStudioConfig {
        model: Some("gemma-4-26b-a4b-it-mlx".into()),
        ..cfg(&server.uri())
    });
    backend.respond("sys", &[]).await.expect("ok");

    let recv = server.received_requests().await.expect("recorded");
    let body: Value = serde_json::from_slice(&recv[0].body).expect("json body");
    assert_eq!(body["model"], "gemma-4-26b-a4b-it-mlx");
}

#[tokio::test]
async fn backend_filters_session_system_rows_from_wire() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(chat_response("ack"))
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(cfg(&server.uri()));
    let sid = SessionId::new();
    let msgs = vec![
        Message::new(sid, Role::System, "session opened"),
        Message::new(sid, Role::Operator, "hello"),
    ];
    backend.respond("sys", &msgs).await.expect("ok");

    let recv = server.received_requests().await.expect("recorded");
    let body: Value = serde_json::from_slice(&recv[0].body).expect("json body");
    let wire_msgs = body["messages"].as_array().expect("messages array");
    // system_prompt + 1 operator turn = 2 (session-system row filtered).
    assert_eq!(wire_msgs.len(), 2);
    assert_eq!(wire_msgs[0]["role"], "system");
    assert_eq!(wire_msgs[1]["role"], "user");
}

#[tokio::test]
async fn backend_surfaces_http_4xx_as_http_status_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("no model loaded"))
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(cfg(&server.uri()));
    let err = backend.respond("sys", &[]).await.expect_err("must fail");
    match err {
        ThinkingError::HttpStatus { status, snippet } => {
            assert_eq!(status, 400);
            assert!(snippet.contains("no model loaded"), "got: {snippet}");
        }
        other => panic!("expected HttpStatus, got {other:?}"),
    }
}

#[tokio::test]
async fn backend_surfaces_empty_choices_as_backend_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "x",
            "object": "chat.completion",
            "choices": []
        })))
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(cfg(&server.uri()));
    let err = backend.respond("sys", &[]).await.expect_err("must fail");
    assert!(
        matches!(err, ThinkingError::BackendRefused(_)),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn backend_surfaces_malformed_response_as_decode() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(cfg(&server.uri()));
    let err = backend.respond("sys", &[]).await.expect_err("must fail");
    assert!(matches!(err, ThinkingError::Decode(_)), "got: {err:?}");
}

#[tokio::test]
async fn health_returns_true_when_models_endpoint_returns_2xx() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [
                {"id": "gemma-4-26b-a4b-it-mlx", "object": "model"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(cfg(&server.uri()));
    assert!(backend.health().await.expect("health ok"));
}

#[tokio::test]
async fn health_returns_false_when_models_endpoint_returns_non_2xx() {
    // LM Studio's local server toggled OFF in the UI returns 404 on
    // /v1/models. We surface that as Ok(false) — server reachable but
    // not serving the API.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(cfg(&server.uri()));
    assert!(!backend.health().await.expect("health ok"));
}

#[tokio::test]
async fn health_returns_transport_error_when_endpoint_unreachable() {
    // Loopback on a port we know is not listening — should fail at the
    // TCP-connect layer and surface as Transport, not as Ok(false).
    let backend = LmStudioBackend::new(LmStudioConfig {
        endpoint: "http://127.0.0.1:1".into(),
        timeout: Duration::from_millis(500),
        ..LmStudioConfig::default()
    });
    let err = backend.health().await.expect_err("must fail");
    assert!(matches!(err, ThinkingError::Transport(_)), "got: {err:?}");
}

// ─── Real-server smoke test ────────────────────────────────────────────
//
// Hits the operator's actual LM Studio instance. Ignored by default —
// run manually with:
//
//   cargo test --package evy-thinking --test lm_studio_mock \
//       -- --ignored real_lm_studio
//
// Requires LM Studio's Local Server toggled ON with at least one model
// loaded. Asserts the round-trip succeeds and returns a non-empty
// response. No specific content assertion — local model output varies.

/// Format an OpenAI-compat SSE stream body — `data: {...}\n\n` per
/// chunk plus the terminator `data: [DONE]\n\n`. Used by the streaming
/// mocks below.
fn sse_body(chunks: &[&str]) -> String {
    let mut out = String::new();
    for c in chunks {
        let frame = format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"content": c}}]})
        );
        out.push_str(&frame);
    }
    out.push_str("data: [DONE]\n\n");
    out
}

#[tokio::test]
async fn stream_respond_forwards_tokens_from_sse_body() {
    let server = MockServer::start().await;
    let body = sse_body(&["Hel", "lo ", "world"]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(cfg(&server.uri()));
    let sid = SessionId::new();
    let msgs = vec![Message::new(sid, Role::Operator, "go")];
    let (tx, mut rx) = mpsc::channel::<StreamChunk>(16);
    let assembled = backend
        .stream_respond("sys", &msgs, &tx)
        .await
        .expect("stream ok");
    drop(tx);

    let mut tokens = Vec::new();
    while let Some(chunk) = rx.recv().await {
        match chunk {
            StreamChunk::Token(t) => tokens.push(t),
            _ => panic!("expected only Token chunks"),
        }
    }
    assert_eq!(tokens, vec!["Hel", "lo ", "world"]);
    assert_eq!(assembled, "Hello world");
}

#[tokio::test]
async fn stream_respond_skips_role_only_preamble() {
    let server = MockServer::start().await;
    let preamble = format!(
        "data: {}\n\n",
        json!({"choices":[{"delta":{"role":"assistant"}}]})
    );
    let body = format!(
        "{preamble}data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":"hi"}}]})
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(cfg(&server.uri()));
    let sid = SessionId::new();
    let msgs = vec![Message::new(sid, Role::Operator, "go")];
    let (tx, mut rx) = mpsc::channel::<StreamChunk>(8);
    let text = backend
        .stream_respond("sys", &msgs, &tx)
        .await
        .expect("stream ok");
    drop(tx);
    let mut chunks = Vec::new();
    while let Some(c) = rx.recv().await {
        chunks.push(c);
    }
    assert_eq!(chunks.len(), 1, "preamble must be skipped");
    assert!(matches!(chunks[0], StreamChunk::Token(ref t) if t == "hi"));
    assert_eq!(text, "hi");
}

#[tokio::test]
async fn stream_respond_returns_backend_refused_on_empty_stream() {
    let server = MockServer::start().await;
    // Stream that only carries the terminator.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("data: [DONE]\n\n")
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;

    let backend = LmStudioBackend::new(cfg(&server.uri()));
    let sid = SessionId::new();
    let msgs = vec![Message::new(sid, Role::Operator, "go")];
    let (tx, _rx) = mpsc::channel::<StreamChunk>(4);
    let err = backend
        .stream_respond("sys", &msgs, &tx)
        .await
        .expect_err("empty stream must surface");
    assert!(
        matches!(err, ThinkingError::BackendRefused(_)),
        "got {err:?}"
    );
}

#[tokio::test]
#[ignore = "hits operator's local LM Studio; run manually with --ignored"]
async fn real_lm_studio_smoke() {
    let backend = LmStudioBackend::new(LmStudioConfig::default());

    // Probe health first so a "server not running" failure surfaces as
    // a clear assertion message rather than a 1-2s timeout on respond.
    let healthy = backend
        .health()
        .await
        .expect("health probe transport must succeed; is LM Studio running?");
    assert!(
        healthy,
        "LM Studio's /v1/models returned non-2xx; toggle the Local Server on in LM Studio's UI",
    );

    let sid = SessionId::new();
    let msgs = vec![Message::new(
        sid,
        Role::Operator,
        "Say the word 'hello' and nothing else.",
    )];
    let reply = backend
        .respond("You are a concise assistant.", &msgs)
        .await
        .expect("respond ok against live LM Studio");
    assert!(
        !reply.trim().is_empty(),
        "live LM Studio returned empty reply",
    );
    eprintln!("real LM Studio reply: {reply}");
}
