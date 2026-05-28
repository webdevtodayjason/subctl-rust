//! End-to-end integration test for Hermes-style LLM-driven skill
//! autoload (Phase 5 Slice — Skill Auto-loading).
//!
//! Wires the full chain:
//!
//! 1. Load three fixture skills from `tests/fixture_skills/` into a
//!    [`SkillRegistry`].
//! 2. Attach the registry to an [`AnthropicBackend`] via `with_skills`.
//! 3. Mock Anthropic so the first response is a `tool_use(skill_view,
//!    {name: "test-skill-alpha"})` and the second is a plain text
//!    response. (The second mock asserts the `tool_result` body sent in
//!    the next request matches the loaded skill body, end-to-end.)
//! 4. Drive [`LlmBackend::respond`] and assert: the first request's
//!    `system` field carries the `## Skills (mandatory)` index, the
//!    request advertises the `skill_view` tool, the autoload trace
//!    fires, and the final returned text is the second mock's reply.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evy_skills::SkillRegistry;
use evy_thinking::{
    AnthropicBackend, AnthropicConfig, LlmBackend, Message, Role, SessionId, DEFAULT_MAX_TOKENS,
    DEFAULT_MODEL,
};
use serde_json::{json, Value};
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::fmt::MakeWriter;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixture_skills")
}

fn load_registry() -> Arc<SkillRegistry> {
    Arc::new(SkillRegistry::load(&fixture_dir()).expect("load fixture skills"))
}

// ─── tracing capture (simple Arc<Mutex<Vec<u8>>> writer) ───────────────

/// `MakeWriter` impl that writes every formatted log line into a shared
/// buffer. The test installs this as the default subscriber for the
/// duration of one `respond()` call and then asserts on the buffer.
#[derive(Clone)]
struct BufferedWriter(Arc<Mutex<Vec<u8>>>);

struct LockedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LockedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for BufferedWriter {
    type Writer = LockedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        LockedBuf(self.0.clone())
    }
}

fn install_capturing_subscriber() -> (Arc<Mutex<Vec<u8>>>, DefaultGuard) {
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let writer = BufferedWriter(buf.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_target(true)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (buf, guard)
}

fn captured(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(buf.lock().unwrap().clone()).expect("trace buffer is utf-8")
}

// ─── the test ──────────────────────────────────────────────────────────

#[tokio::test]
async fn backend_with_skills_handles_skill_view_tool_call_end_to_end() {
    let server = MockServer::start().await;

    // First request → return a tool_use asking for `test-skill-alpha`.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_first",
            "type": "message",
            "role": "assistant",
            "model": DEFAULT_MODEL,
            "content": [
                {
                    "type": "tool_use",
                    "id": "toolu_alpha",
                    "name": "skill_view",
                    "input": {"name": "test-skill-alpha"}
                }
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 20}
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;

    // Second request (after we send the tool_result) → return plain
    // text, which is what respond() ultimately returns to the caller.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_second",
            "type": "message",
            "role": "assistant",
            "model": DEFAULT_MODEL,
            "content": [
                {"type": "text", "text": "applied skill alpha; here is the plan."}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 30, "output_tokens": 12}
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;

    let registry = load_registry();
    assert_eq!(registry.count(), 3, "three fixture skills must load");
    assert!(registry.find("test-skill-alpha").is_some());

    let backend = AnthropicBackend::new(cfg(&server.uri())).with_skills(registry.clone());
    assert!(backend.has_skills());

    let (log_buf, _guard) = install_capturing_subscriber();

    let sid = SessionId::new();
    let msgs = vec![Message::new(sid, Role::Operator, "operator turn")];
    let out = backend
        .respond("CALLER SYSTEM PROMPT", &msgs)
        .await
        .expect("respond ok");

    assert_eq!(out, "applied skill alpha; here is the plan.");

    // Both mocks must have fired exactly once (asserted via `.expect(1)`
    // on each Mock — wiremock panics on drop if expectations are unmet).
    let recv = server.received_requests().await.expect("recorded");
    assert_eq!(recv.len(), 2, "exactly two POSTs to /v1/messages");

    // ── First request: system prompt carries the skills index AND the
    //    caller's original prompt, plus the tools field advertises
    //    `skill_view`. ─────────────────────────────────────────────────
    let body1: Value = serde_json::from_slice(&recv[0].body).expect("json body");
    let sys1 = body1["system"].as_str().expect("system field");
    assert!(
        sys1.starts_with("## Skills (mandatory — load via skill_view when a skill applies)"),
        "system prompt must lead with the skills index; got: {sys1:.200}"
    );
    assert!(
        sys1.contains("- test-skill-alpha: A test skill for the autoload integration test (alpha)"),
        "skills index must list the alpha fixture; got: {sys1}",
    );
    assert!(
        sys1.contains("CALLER SYSTEM PROMPT"),
        "caller's system prompt must follow the skills index",
    );

    let tools1 = body1["tools"].as_array().expect("tools array");
    assert_eq!(tools1.len(), 1);
    assert_eq!(tools1[0]["name"], "skill_view");
    assert_eq!(
        tools1[0]["input_schema"]["properties"]["name"]["type"],
        "string"
    );
    assert_eq!(
        tools1[0]["input_schema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["name"]
    );

    // The first request carries exactly the operator turn (System rows
    // are filtered; nothing else was pushed yet).
    let msgs1 = body1["messages"].as_array().expect("messages array");
    assert_eq!(msgs1.len(), 1);
    assert_eq!(msgs1[0]["role"], "user");
    assert_eq!(msgs1[0]["content"], "operator turn");

    // ── Second request: the original operator turn, the assistant
    //    tool_use block echoed back, then the user tool_result block
    //    carrying the loaded skill body. ────────────────────────────
    let body2: Value = serde_json::from_slice(&recv[1].body).expect("json body");
    let msgs2 = body2["messages"].as_array().expect("messages array");
    assert_eq!(
        msgs2.len(),
        3,
        "operator turn + assistant tool_use + user tool_result"
    );

    assert_eq!(msgs2[0]["role"], "user");
    assert_eq!(msgs2[0]["content"], "operator turn");

    assert_eq!(msgs2[1]["role"], "assistant");
    let asst_content = msgs2[1]["content"].as_array().expect("assistant content");
    assert_eq!(asst_content.len(), 1);
    assert_eq!(asst_content[0]["type"], "tool_use");
    assert_eq!(asst_content[0]["id"], "toolu_alpha");
    assert_eq!(asst_content[0]["name"], "skill_view");
    assert_eq!(asst_content[0]["input"]["name"], "test-skill-alpha");

    assert_eq!(msgs2[2]["role"], "user");
    let user_content = msgs2[2]["content"].as_array().expect("user content");
    assert_eq!(user_content.len(), 1);
    assert_eq!(user_content[0]["type"], "tool_result");
    assert_eq!(user_content[0]["tool_use_id"], "toolu_alpha");
    let body_returned = user_content[0]["content"]
        .as_str()
        .expect("tool_result content is a string");
    let expected_body = registry
        .find("test-skill-alpha")
        .expect("alpha is registered")
        .body
        .as_str();
    assert_eq!(
        body_returned, expected_body,
        "tool_result must carry the full SKILL.md body"
    );
    // is_error must NOT be set on a successful lookup.
    assert!(
        user_content[0].get("is_error").is_none(),
        "is_error must be absent on a successful skill lookup"
    );

    // ── Trace event for skill autoload must have fired. ────────────────
    let log = captured(&log_buf);
    assert!(
        log.contains("skill autoloaded via skill_view"),
        "autoload trace event missing; captured log:\n{log}"
    );
    assert!(
        log.contains("test-skill-alpha"),
        "autoload trace must name the skill; captured log:\n{log}"
    );
}

#[tokio::test]
async fn backend_without_skills_omits_index_and_tools() {
    // Sanity check: when no SkillRegistry is attached, behavior is
    // unchanged from the pre-Phase-5 backend — no `tools` field, no
    // `## Skills (mandatory)` block prepended to the system prompt.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg",
            "type": "message",
            "role": "assistant",
            "model": DEFAULT_MODEL,
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let backend = AnthropicBackend::new(cfg(&server.uri()));
    assert!(!backend.has_skills());

    let sid = SessionId::new();
    let msgs = vec![Message::new(sid, Role::Operator, "hi")];
    let out = backend.respond("plain system", &msgs).await.expect("ok");
    assert_eq!(out, "ok");

    let recv = server.received_requests().await.expect("recorded");
    let body: Value = serde_json::from_slice(&recv[0].body).expect("json");
    assert_eq!(body["system"], "plain system");
    assert!(
        body.get("tools").is_none(),
        "tools field must be absent when no registry is attached"
    );
}

#[tokio::test]
async fn skill_view_for_unknown_skill_returns_error_tool_result() {
    // The LLM hallucinates a skill name. The backend should surface
    // is_error: true to the model and then loop — we mock a text
    // response on the second turn so the test still terminates.
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_bogus",
                "name": "skill_view",
                "input": {"name": "does-not-exist"}
            }],
            "stop_reason": "tool_use"
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "no skill available; here is a plan."}],
            "stop_reason": "end_turn"
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let backend = AnthropicBackend::new(cfg(&server.uri())).with_skills(load_registry());
    let sid = SessionId::new();
    let out = backend
        .respond("sys", &[Message::new(sid, Role::Operator, "go")])
        .await
        .expect("respond ok");
    assert_eq!(out, "no skill available; here is a plan.");

    let recv = server.received_requests().await.expect("recorded");
    let body2: Value = serde_json::from_slice(&recv[1].body).expect("json");
    let user_content = body2["messages"][2]["content"]
        .as_array()
        .expect("user content");
    assert_eq!(user_content[0]["type"], "tool_result");
    assert_eq!(user_content[0]["tool_use_id"], "toolu_bogus");
    assert_eq!(user_content[0]["is_error"], true);
    let msg = user_content[0]["content"].as_str().unwrap();
    assert!(
        msg.contains("does-not-exist"),
        "error message must name the missing skill; got `{msg}`"
    );
}
