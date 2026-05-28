//! End-to-end tests for the SSE branch of `POST /api/evy/chat`.
//!
//! Spins up a real axum server backed by an `AppState` whose
//! `ThinkingPartner` is built on a `ScriptedBackend` — no live LLM
//! calls. The backend's default `stream_respond` impl (the one in
//! `LlmBackend::stream_respond`) emits the canned reply as a single
//! `Token` chunk, exercising the full event flow:
//!
//! 1. handler reads `Accept: text/event-stream` → spawns worker
//! 2. worker calls `ThinkingPartner::stream_send`
//! 3. partner calls `backend.stream_respond` → emits one `Token`
//! 4. handler converts to SSE `data: {"kind":"token",...}` frame
//! 5. handler emits final `data: {"kind":"done", ...}` and closes
//!
//! Backwards-compat: `chat_integration.rs` still exercises the JSON
//! branch (no Accept header) and must keep passing.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use evy_comms::{
    AppState, ChatRequest, EventBroadcaster, HttpConfig, HttpServer, JobSummary, StubAppState,
    WorkerSummary,
};
use evy_policy::Policy;
use evy_skills::SkillRegistry;
use evy_thinking::{LlmBackend, Message, Result as ThinkingResult, ThinkingError, ThinkingPartner};
use futures_util::StreamExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct ScriptedBackend {
    replies: StdMutex<Vec<String>>,
}

impl ScriptedBackend {
    fn new(replies: Vec<&str>) -> Arc<Self> {
        Arc::new(Self {
            replies: StdMutex::new(replies.into_iter().map(String::from).collect()),
        })
    }
}

#[async_trait]
impl LlmBackend for ScriptedBackend {
    async fn respond(&self, _system_prompt: &str, _messages: &[Message]) -> ThinkingResult<String> {
        let mut q = self.replies.lock().unwrap();
        if q.is_empty() {
            Err(ThinkingError::BackendRefused(
                "scripted backend ran out".into(),
            ))
        } else {
            Ok(q.remove(0))
        }
    }
}

struct StreamTestState {
    partner: Option<Arc<ThinkingPartner>>,
    skills: Option<Arc<SkillRegistry>>,
}

#[async_trait]
impl AppState for StreamTestState {
    async fn workers(&self) -> Vec<WorkerSummary> {
        Vec::new()
    }
    async fn jobs(&self) -> Vec<JobSummary> {
        Vec::new()
    }
    async fn policy(&self) -> Policy {
        Policy::default()
    }
    fn thinking_partner(&self) -> Option<Arc<ThinkingPartner>> {
        self.partner.clone()
    }
    fn skills(&self) -> Option<Arc<SkillRegistry>> {
        self.skills.clone()
    }
}

async fn spawn(state: Arc<dyn AppState>) -> (String, CancellationToken) {
    let broadcaster = EventBroadcaster::new(64);
    let server = HttpServer::new(HttpConfig::ephemeral(), broadcaster, state);
    let bound = server.bind().await.expect("bind");
    let addr = bound.local_addr();
    let shutdown = CancellationToken::new();
    let st = shutdown.clone();
    tokio::spawn(async move {
        let _ = bound.serve(st).await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}"), shutdown)
}

/// Collect SSE events from a response body. Stops on first `done` or
/// `error` event, returning the full event list (including the
/// terminator).
async fn collect_events(res: reqwest::Response) -> Vec<Value> {
    let mut stream = res.bytes_stream().eventsource();
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        let event = item.expect("event parse");
        let payload: Value = serde_json::from_str(&event.data).expect("payload json");
        let kind = payload["kind"].as_str().unwrap_or("").to_string();
        events.push(payload);
        if kind == "done" || kind == "error" {
            break;
        }
    }
    events
}

#[tokio::test]
async fn streaming_chat_emits_token_then_done_for_new_session() {
    let backend = ScriptedBackend::new(vec!["hello world"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(StreamTestState {
        partner: Some(partner.clone()),
        skills: None,
    });
    let (base, shutdown) = spawn(state).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .header("Accept", "text/event-stream")
        .json(&ChatRequest {
            session_id: None,
            message: "open me up".into(),
        })
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let ct = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.starts_with("text/event-stream"),
        "unexpected content-type `{ct}`",
    );

    let events = collect_events(res).await;
    // At least one token frame + the terminal done frame.
    assert!(events.len() >= 2, "got events: {events:?}");
    // The default-impl stream_respond emits the whole reply as one
    // Token chunk — assert that's what we got.
    assert_eq!(events[0]["kind"], "token");
    assert_eq!(events[0]["content"], "hello world");
    // Last frame is `done` with the freshly-minted session id.
    let last = events.last().unwrap();
    assert_eq!(last["kind"], "done");
    assert!(last["session_id"].is_string());
    let sid = Uuid::parse_str(last["session_id"].as_str().unwrap()).expect("uuid");
    assert!(sid != Uuid::nil());

    // The session must now be visible to the partner.
    let stored = partner.session(evy_thinking::SessionId(sid)).await.unwrap();
    assert!(stored.is_some(), "session must persist after streaming");

    shutdown.cancel();
}

#[tokio::test]
async fn streaming_chat_continues_existing_session() {
    let backend = ScriptedBackend::new(vec!["opening?", "next reply"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(StreamTestState {
        partner: Some(partner.clone()),
        skills: None,
    });
    let (base, shutdown) = spawn(state).await;

    // Open via JSON path (default Accept) — easier to read the id.
    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .json(&ChatRequest {
            session_id: None,
            message: "topic".into(),
        })
        .send()
        .await
        .expect("send open");
    let body: Value = res.json().await.unwrap();
    let sid = Uuid::parse_str(body["session_id"].as_str().unwrap()).expect("uuid");

    // Stream a follow-up.
    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .header("Accept", "text/event-stream")
        .json(&ChatRequest {
            session_id: Some(sid),
            message: "more please".into(),
        })
        .send()
        .await
        .expect("send stream");
    assert_eq!(res.status(), 200);

    let events = collect_events(res).await;
    assert_eq!(events[0]["kind"], "token");
    assert_eq!(events[0]["content"], "next reply");
    let last = events.last().unwrap();
    assert_eq!(last["kind"], "done");
    assert_eq!(last["session_id"], sid.to_string());

    shutdown.cancel();
}

#[tokio::test]
async fn streaming_chat_emits_error_when_partner_unavailable() {
    let (base, shutdown) = spawn(Arc::new(StubAppState)).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .header("Accept", "text/event-stream")
        .json(&ChatRequest {
            session_id: None,
            message: "hi".into(),
        })
        .send()
        .await
        .expect("send");
    // The wire status is 200 because headers were already flushed by
    // the time we discovered the partner was absent — the error rides
    // inside the SSE stream.
    assert_eq!(res.status(), 200);

    let events = collect_events(res).await;
    let last = events.last().unwrap();
    assert_eq!(last["kind"], "error");
    assert_eq!(last["error_kind"], "unavailable");
    shutdown.cancel();
}

#[tokio::test]
async fn streaming_chat_emits_error_for_empty_message() {
    let backend = ScriptedBackend::new(vec!["never used"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(StreamTestState {
        partner: Some(partner),
        skills: None,
    });
    let (base, shutdown) = spawn(state).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .header("Accept", "text/event-stream")
        .json(&ChatRequest {
            session_id: None,
            message: "   ".into(),
        })
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);

    let events = collect_events(res).await;
    let last = events.last().unwrap();
    assert_eq!(last["kind"], "error");
    assert_eq!(last["error_kind"], "bad_request");
    shutdown.cancel();
}

#[tokio::test]
async fn streaming_chat_emits_error_for_unknown_session() {
    let backend = ScriptedBackend::new(vec!["never used"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(StreamTestState {
        partner: Some(partner),
        skills: None,
    });
    let (base, shutdown) = spawn(state).await;

    let bogus = Uuid::new_v4();
    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .header("Accept", "text/event-stream")
        .json(&ChatRequest {
            session_id: Some(bogus),
            message: "anyone there".into(),
        })
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);

    let events = collect_events(res).await;
    let last = events.last().unwrap();
    assert_eq!(last["kind"], "error");
    assert_eq!(last["error_kind"], "unknown_session");
    shutdown.cancel();
}

#[tokio::test]
async fn streaming_chat_emits_skill_loaded_when_registry_attached() {
    // Build a registry with one skill so the handler emits a
    // SkillLoaded frame up-front (per the current "skills index" v0.5.0
    // semantics — the backend doesn't drive per-turn autoload via the
    // default-impl path).
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("plan")).unwrap();
    std::fs::write(
        dir.path().join("plan").join("SKILL.md"),
        r#"---
name: plan
description: draft a plan
---

Plan body.
"#,
    )
    .unwrap();
    let reg = Arc::new(SkillRegistry::load(dir.path()).unwrap());

    let backend = ScriptedBackend::new(vec!["reply"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(StreamTestState {
        partner: Some(partner),
        skills: Some(reg),
    });
    let (base, shutdown) = spawn(state).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .header("Accept", "text/event-stream")
        .json(&ChatRequest {
            session_id: None,
            message: "go".into(),
        })
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);

    let events = collect_events(res).await;
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "skill_loaded" && e["name"] == "plan"),
        "expected a skill_loaded frame for `plan`; got {events:?}",
    );
    assert_eq!(events.last().unwrap()["kind"], "done");
    shutdown.cancel();
}

#[tokio::test]
async fn json_branch_still_works_when_accept_absent() {
    // Backwards-compatibility: the existing JSON behaviour must remain
    // the default. This duplicates one of `chat_integration.rs`'s
    // assertions but keeps the streaming test file self-contained for
    // CI bisection.
    let backend = ScriptedBackend::new(vec!["json reply"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(StreamTestState {
        partner: Some(partner),
        skills: None,
    });
    let (base, shutdown) = spawn(state).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        // No Accept header → JSON path.
        .json(&ChatRequest {
            session_id: None,
            message: "topic".into(),
        })
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let ct = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.starts_with("application/json"),
        "expected json content-type, got `{ct}`",
    );
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["response"], "json reply");
    shutdown.cancel();
}
