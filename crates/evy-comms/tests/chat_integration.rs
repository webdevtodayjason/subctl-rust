//! End-to-end tests for `POST /api/evy/chat`.
//!
//! Spins up a real axum server backed by an `AppState` that returns
//! either:
//! * a [`ThinkingPartner`] wired to a hand-rolled `ScriptedBackend`
//!   (so we never touch the Anthropic API), or
//! * `None` to exercise the 503 / unconfigured path.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use evy_comms::{
    AppState, ChatRequest, ChatResponse, EventBroadcaster, HttpConfig, HttpServer, JobSummary,
    StubAppState, WorkerSummary,
};
use evy_policy::Policy;
use evy_skills::SkillRegistry;
use evy_thinking::{LlmBackend, Message, Result as ThinkingResult, ThinkingError, ThinkingPartner};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Backend that returns canned strings in order.
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

/// AppState that returns the supplied partner + optional skills.
struct ChatTestState {
    partner: Option<Arc<ThinkingPartner>>,
    skills: Option<Arc<SkillRegistry>>,
}

#[async_trait]
impl AppState for ChatTestState {
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

#[tokio::test]
async fn post_chat_returns_503_when_partner_unavailable() {
    let (base, shutdown) = spawn(Arc::new(StubAppState)).await;
    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .json(&ChatRequest {
            session_id: None,
            message: "hi".into(),
        })
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 503);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["kind"], "unavailable");
    shutdown.cancel();
}

#[tokio::test]
async fn post_chat_opens_new_session_when_session_id_omitted() {
    // Opening now defaults to a *conversational* session; this
    // prompt-agnostic backend returns the canned reply regardless of
    // mode. (The mode is asserted in the CapturingBackend tests below.)
    let backend = ScriptedBackend::new(vec!["1. What's the target?\n2. What's the budget?"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(ChatTestState {
        partner: Some(partner.clone()),
        skills: None,
    });
    let (base, shutdown) = spawn(state).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .json(&ChatRequest {
            session_id: None,
            message: "migrate postgres".into(),
        })
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: ChatResponse = res.json().await.expect("json");
    assert!(body.session_id != Uuid::nil());
    assert!(body.response.contains("target"));
    assert!(body.skills_loaded.is_empty(), "no skills configured");
    shutdown.cancel();
}

#[tokio::test]
async fn post_chat_continues_existing_session() {
    let backend = ScriptedBackend::new(vec!["opening?", "draft v1"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(ChatTestState {
        partner: Some(partner.clone()),
        skills: None,
    });
    let (base, shutdown) = spawn(state).await;

    let client = reqwest::Client::new();
    // First request opens the session.
    let res = client
        .post(format!("{base}/api/evy/chat"))
        .json(&ChatRequest {
            session_id: None,
            message: "topic".into(),
        })
        .send()
        .await
        .expect("send 1");
    let body: ChatResponse = res.json().await.unwrap();
    let sid = body.session_id;

    // Second request reuses the id.
    let res = client
        .post(format!("{base}/api/evy/chat"))
        .json(&ChatRequest {
            session_id: Some(sid),
            message: "follow-up".into(),
        })
        .send()
        .await
        .expect("send 2");
    assert_eq!(res.status(), 200);
    let body: ChatResponse = res.json().await.unwrap();
    assert_eq!(body.session_id, sid);
    assert_eq!(body.response, "draft v1");
    shutdown.cancel();
}

#[tokio::test]
async fn post_chat_rejects_empty_message() {
    let backend = ScriptedBackend::new(vec!["never used"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(ChatTestState {
        partner: Some(partner),
        skills: None,
    });
    let (base, shutdown) = spawn(state).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .json(&ChatRequest {
            session_id: None,
            message: "   ".into(),
        })
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["kind"], "bad_request");
    shutdown.cancel();
}

#[tokio::test]
async fn post_chat_returns_404_for_unknown_session() {
    let backend = ScriptedBackend::new(vec!["nope"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(ChatTestState {
        partner: Some(partner),
        skills: None,
    });
    let (base, shutdown) = spawn(state).await;

    let bogus = Uuid::new_v4();
    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .json(&ChatRequest {
            session_id: Some(bogus),
            message: "anyone there".into(),
        })
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 404);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["kind"], "unknown_session");
    assert_eq!(body["session_id"], bogus.to_string());
    shutdown.cancel();
}

#[tokio::test]
async fn post_chat_master_alias_routes_to_native_handler() {
    // The dashboard chat tab posts the legacy `/api/master/chat` form. The
    // `/api/master/*` → `/api/evy/*` rewrite (applied before routing) lands it
    // on the SAME native chat handler as `/api/evy/chat` — response parity, not
    // a proxy fall-through. The ScriptedBackend returns the canned reply on
    // both prefixes, so we assert the master path yields the identical answer.
    let backend = ScriptedBackend::new(vec!["via evy", "via master"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(ChatTestState {
        partner: Some(partner),
        skills: None,
    });
    let (base, shutdown) = spawn(state).await;
    let client = reqwest::Client::new();

    let evy: ChatResponse = client
        .post(format!("{base}/api/evy/chat"))
        .json(&ChatRequest {
            session_id: None,
            message: "topic".into(),
        })
        .send()
        .await
        .expect("send evy")
        .json()
        .await
        .expect("evy json");
    assert_eq!(evy.response, "via evy");

    let res = client
        .post(format!("{base}/api/master/chat"))
        .json(&ChatRequest {
            session_id: None,
            message: "topic".into(),
        })
        .send()
        .await
        .expect("send master");
    assert_eq!(
        res.status(),
        200,
        "/api/master/chat must be served by the native handler via the rewrite"
    );
    let master: ChatResponse = res.json().await.expect("master json");
    assert_eq!(
        master.response, "via master",
        "/api/master/chat must reach the same native chat handler as /api/evy/chat"
    );
    shutdown.cancel();
}

/// Backend that records the system prompt of each call so a test can
/// assert which persona/mode the handler selected, returning canned
/// replies in order.
struct CapturingBackend {
    replies: StdMutex<Vec<String>>,
    prompts: Arc<StdMutex<Vec<String>>>,
}

impl CapturingBackend {
    fn new(replies: Vec<&str>) -> (Arc<Self>, Arc<StdMutex<Vec<String>>>) {
        let prompts = Arc::new(StdMutex::new(Vec::new()));
        let b = Arc::new(Self {
            replies: StdMutex::new(replies.into_iter().map(String::from).collect()),
            prompts: prompts.clone(),
        });
        (b, prompts)
    }
}

#[async_trait]
impl LlmBackend for CapturingBackend {
    async fn respond(&self, system_prompt: &str, _messages: &[Message]) -> ThinkingResult<String> {
        self.prompts.lock().unwrap().push(system_prompt.to_string());
        let mut q = self.replies.lock().unwrap();
        if q.is_empty() {
            Err(ThinkingError::BackendRefused(
                "capturing backend ran out".into(),
            ))
        } else {
            Ok(q.remove(0))
        }
    }
}

#[tokio::test]
async fn post_chat_opens_conversational_session_by_default() {
    let (backend, prompts) = CapturingBackend::new(vec!["Hey — good to see you."]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(ChatTestState {
        partner: Some(partner),
        skills: None,
    });
    let (base, shutdown) = spawn(state).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .json(&ChatRequest {
            session_id: None,
            message: "hello".into(),
        })
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: ChatResponse = res.json().await.unwrap();
    assert_eq!(body.response, "Hey — good to see you.");

    // The handler must have opened a CONVERSATIONAL session — the system
    // prompt the backend saw is Evy's persona, not the planning
    // instrument. This is the proof that "hello → Evy" at the HTTP layer.
    let captured = prompts.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert!(
        captured[0].contains("Hold a natural conversation"),
        "default open must use the conversational persona prompt"
    );
    shutdown.cancel();
}

#[tokio::test]
async fn post_chat_slash_plan_opens_planning_session() {
    let (backend, prompts) = CapturingBackend::new(vec!["1. What's the target?"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(ChatTestState {
        partner: Some(partner),
        skills: None,
    });
    let (base, shutdown) = spawn(state).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .json(&ChatRequest {
            session_id: None,
            message: "/plan migrate postgres".into(),
        })
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);

    // `/plan <topic>` routes to the planning prompt with the topic
    // extracted (the planning prompt embeds the topic verbatim).
    let captured = prompts.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert!(
        captured[0].contains("migrate postgres"),
        "planning prompt must embed the extracted topic, not the /plan literal"
    );
    assert!(
        !captured[0].contains("Hold a natural conversation"),
        "/plan must NOT use the conversational prompt"
    );
    shutdown.cancel();
}

#[tokio::test]
async fn post_chat_bare_plan_requires_a_topic() {
    let (backend, _prompts) = CapturingBackend::new(vec!["unused"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(ChatTestState {
        partner: Some(partner),
        skills: None,
    });
    let (base, shutdown) = spawn(state).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .json(&ChatRequest {
            session_id: None,
            message: "/plan".into(),
        })
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["kind"], "bad_request");
    shutdown.cancel();
}
