//! Integration tests for [`evy_thinking::AnthropicBackend`] against a
//! `wiremock` mock of the Anthropic Messages API. The real
//! `api.anthropic.com` is NEVER hit — `AnthropicConfig` exposes
//! `api_base` precisely so these tests can swap it.
//!
//! These also exercise the [`ThinkingPartner`] composition end-to-end:
//! one wiremock server stands in for the LLM, the partner drives a
//! 3-turn planning session against it, and we assert on the session
//! state at each turn.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use evy_thinking::{
    AnthropicBackend, AnthropicConfig, LlmBackend, Message, Role, Session, SessionId,
    SessionStatus, ThinkingError, ThinkingPartner, DEFAULT_ANTHROPIC_API_BASE, DEFAULT_MAX_TOKENS,
    DEFAULT_MODEL,
};

const TOKEN: &str = "anthropic-test-key";

fn cfg(server_url: &str) -> AnthropicConfig {
    AnthropicConfig {
        api_base: server_url.trim_end_matches('/').to_string(),
        api_key: TOKEN.to_string(),
        model: DEFAULT_MODEL.to_string(),
        max_tokens: DEFAULT_MAX_TOKENS,
        timeout: Duration::from_secs(5),
    }
}

fn text_response(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": DEFAULT_MODEL,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 20}
    }))
}

#[tokio::test]
async fn defaults_constant_matches_real_host() {
    // Just a guard against accidental drift — if a future contributor
    // changes the constant they have to also change this test.
    assert_eq!(DEFAULT_ANTHROPIC_API_BASE, "https://api.anthropic.com");
}

#[tokio::test]
async fn backend_sends_required_headers_and_body_shape() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", TOKEN))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("content-type", "application/json"))
        .respond_with(text_response("ok"))
        .expect(1)
        .mount(&server)
        .await;

    let backend = AnthropicBackend::new(cfg(&server.uri()));
    let sid = SessionId::new();
    let msgs = vec![Message::new(sid, Role::Operator, "first question")];
    let out = backend.respond("system!", &msgs).await.expect("respond ok");
    assert_eq!(out, "ok");
}

#[tokio::test]
async fn backend_surfaces_http_4xx_as_http_status_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid_api_key"))
        .mount(&server)
        .await;

    let backend = AnthropicBackend::new(cfg(&server.uri()));
    let err = backend.respond("sys", &[]).await.expect_err("must fail");
    match err {
        ThinkingError::HttpStatus { status, snippet } => {
            assert_eq!(status, 401);
            assert!(snippet.contains("invalid_api_key"), "got: {snippet}");
        }
        other => panic!("expected HttpStatus, got {other:?}"),
    }
}

#[tokio::test]
async fn backend_surfaces_empty_content_as_backend_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg",
            "type": "message",
            "role": "assistant",
            "content": [],
            "stop_reason": "end_turn"
        })))
        .mount(&server)
        .await;

    let backend = AnthropicBackend::new(cfg(&server.uri()));
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
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;

    let backend = AnthropicBackend::new(cfg(&server.uri()));
    let err = backend.respond("sys", &[]).await.expect_err("must fail");
    assert!(matches!(err, ThinkingError::Decode(_)), "got: {err:?}");
}

#[tokio::test]
async fn backend_filters_system_role_from_wire_messages() {
    let server = MockServer::start().await;
    // Capture the request body so we can assert on it.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(text_response("ack"))
        .mount(&server)
        .await;

    let backend = AnthropicBackend::new(cfg(&server.uri()));
    let sid = SessionId::new();
    let msgs = vec![
        Message::new(sid, Role::System, "session opened"),
        Message::new(sid, Role::Operator, "hello"),
        Message::new(sid, Role::Partner, "world"),
    ];
    backend.respond("sys", &msgs).await.expect("ok");

    // wiremock 0.6 doesn't have a built-in body matcher we want here;
    // instead we re-read the recorded request via received_requests().
    let recv = server.received_requests().await.expect("recorded");
    assert_eq!(recv.len(), 1);
    let body: Value = serde_json::from_slice(&recv[0].body).expect("json body");
    let wire_msgs = body["messages"].as_array().expect("messages array");
    assert_eq!(wire_msgs.len(), 2, "system row must be filtered out");
    assert_eq!(wire_msgs[0]["role"], "user");
    assert_eq!(wire_msgs[0]["content"], "hello");
    assert_eq!(wire_msgs[1]["role"], "assistant");
    assert_eq!(wire_msgs[1]["content"], "world");
    assert_eq!(body["system"], "sys");
    assert_eq!(body["model"], DEFAULT_MODEL);
    assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
}

#[tokio::test]
async fn partner_start_session_sends_at_least_one_user_turn_to_anthropic() {
    // Regression guard for the "Anthropic Messages API requires
    // messages[] to contain at least one entry whose role is `user`"
    // constraint. Without the kickoff turn, start_session would send
    // an empty messages array and the real API would 400.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(text_response("opening questions?"))
        .expect(1)
        .mount(&server)
        .await;

    let backend = Arc::new(AnthropicBackend::new(cfg(&server.uri())));
    let partner = ThinkingPartner::new(backend);
    partner
        .start_session("topic".into())
        .await
        .expect("start ok");

    let recv = server.received_requests().await.expect("recorded");
    assert_eq!(recv.len(), 1, "exactly one POST to /v1/messages");
    let body: Value = serde_json::from_slice(&recv[0].body).expect("json body");
    let wire_msgs = body["messages"].as_array().expect("messages array");
    assert!(
        !wire_msgs.is_empty(),
        "messages must be non-empty (Anthropic Messages API requirement)"
    );
    assert_eq!(
        wire_msgs[0]["role"], "user",
        "first wire message must be a user turn"
    );
}

// ── End-to-end ThinkingPartner against wiremock ────────────────────────

#[tokio::test]
async fn partner_drives_three_turn_planning_session_against_anthropic_mock() {
    let server = MockServer::start().await;

    // Four canned replies, in the order the partner consumes them:
    // 1) clarifying questions (PHASE 1)
    // 2) draft plan       (PHASE 2 turn 1)
    // 3) refined plan     (PHASE 2 turn 2)
    // 4) final summary    (PHASE 3)
    let replies = [
        "1. What's the target version?\n2. Downtime budget?\n3. Existing tests?\n4. Rollback plan?\n5. Who owns the cutover?\n\nAnswer what you can; we'll iterate.",
        "**Goal** — migrate to PG16.\n**Unknowns** — extension list.\n**Approach** — 1) audit, 2) shadow-write.\n**Risks** — downtime.\n\nAnything else to refine, or shall we conclude?",
        "**Goal** — migrate to PG16, no downtime.\n**Unknowns** — extension list.\n**Approach** — 1) blue/green, 2) shadow-write.\n**Risks** — replication lag.\n\nAnything else to refine, or shall we conclude?",
        "**Goal** — migrate to PG16.\n**Unknowns** — none.\n**Approach** — blue/green.\n**Risks** — replication lag.\n**Next steps** — 1) provision green. 2) shadow-write.",
    ];

    // The mock server consumes responses in declaration order via
    // wiremock's `up_to_n_times` chaining — each Mock matches exactly
    // once.
    for reply in replies {
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(text_response(reply))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
    }

    let backend = Arc::new(AnthropicBackend::new(cfg(&server.uri())));
    let partner = ThinkingPartner::new(backend);

    // PHASE 1: open the session — clarifying questions come back.
    let id = partner
        .start_session("postgres migration".into())
        .await
        .expect("start ok");
    let session: Session = partner.session(id).await.unwrap().expect("session");
    assert_eq!(session.status, SessionStatus::Active);
    let opening = session
        .last_of(Role::Partner)
        .expect("opening partner turn");
    assert!(opening.content.contains("target version"));

    // PHASE 2 turn 1: draft.
    let draft = partner
        .send(id, "Target is PG16, zero downtime budget.".into())
        .await
        .expect("send ok");
    assert!(draft.contains("**Goal**"));
    assert!(draft.contains("**Risks**"));

    // PHASE 2 turn 2: refine.
    let refined = partner
        .send(
            id,
            "We need blue/green to honour the no-downtime constraint.".into(),
        )
        .await
        .expect("send ok");
    assert!(refined.contains("blue/green"));

    // PHASE 3: conclude.
    partner.conclude(id).await.expect("conclude ok");

    let final_session = partner.session(id).await.unwrap().expect("session");
    assert_eq!(final_session.status, SessionStatus::Concluded);
    let summary = final_session.last_of(Role::Partner).expect("summary");
    assert!(summary.content.contains("Next steps"));

    // System + kickoff_op + opening_p + (op1, p1) + (op2, p2)
    //   + (op3-conclude, p3) = 9
    assert_eq!(final_session.messages.len(), 9);
}

#[tokio::test]
async fn partner_send_failure_does_not_pollute_session_with_partner_turn() {
    let server = MockServer::start().await;

    // Two mocks: opening succeeds, the second turn fails.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(text_response("opening questions?"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let backend = Arc::new(AnthropicBackend::new(cfg(&server.uri())));
    let partner = ThinkingPartner::new(backend);
    let id = partner.start_session("topic".into()).await.expect("ok");

    let err = partner
        .send(id, "operator turn".into())
        .await
        .expect_err("must fail");
    assert!(
        matches!(err, ThinkingError::HttpStatus { status: 503, .. }),
        "got: {err:?}"
    );

    // Session should contain: System(open), Operator(kickoff),
    // Partner(opening), Operator(turn). No second Partner turn because
    // the backend errored.
    let s = partner.session(id).await.unwrap().expect("session");
    assert_eq!(s.messages.len(), 4);
    assert_eq!(s.messages[0].role, Role::System);
    assert_eq!(s.messages[1].role, Role::Operator); // kickoff
    assert_eq!(s.messages[2].role, Role::Partner); // opening
    assert_eq!(s.messages[3].role, Role::Operator); // operator turn
                                                    // Session remains Active so the operator can retry.
    assert_eq!(s.status, SessionStatus::Active);
}
