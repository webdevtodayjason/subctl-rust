//! End-to-end tests for `GET /api/evy/sessions` and
//! `DELETE /api/evy/sessions/:id`.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use evy_comms::{
    AppState, ChatRequest, ChatResponse, EventBroadcaster, HttpConfig, HttpServer, JobSummary,
    SessionsListResponse, StubAppState, WorkerSummary,
};
use evy_policy::Policy;
use evy_skills::SkillRegistry;
use evy_thinking::{LlmBackend, Message, Result as ThinkingResult, ThinkingError, ThinkingPartner};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Scripted LLM — returns canned strings; ThinkingPartner needs it to
/// open sessions during the test setup.
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

struct SessionsTestState {
    partner: Option<Arc<ThinkingPartner>>,
}

#[async_trait]
impl AppState for SessionsTestState {
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
        None
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

/// Open `n` sessions via `POST /api/evy/chat`, returning their ids.
async fn open_sessions(base: &str, n: usize, topics: &[&str]) -> Vec<Uuid> {
    let client = reqwest::Client::new();
    let mut ids = Vec::with_capacity(n);
    for topic in topics.iter().take(n) {
        let res = client
            .post(format!("{base}/api/evy/chat"))
            .json(&ChatRequest {
                session_id: None,
                message: (*topic).into(),
            })
            .send()
            .await
            .expect("send");
        assert_eq!(res.status(), 200, "open session: {topic}");
        let body: ChatResponse = res.json().await.expect("json");
        ids.push(body.session_id);
        // Small jitter so `last_activity` timestamps differ — the list
        // is sorted newest-first.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    ids
}

#[tokio::test]
async fn sessions_list_returns_503_when_partner_absent() {
    let (base, shutdown) = spawn(Arc::new(StubAppState)).await;
    let res = reqwest::Client::new()
        .get(format!("{base}/api/evy/sessions"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 503);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["kind"], "unavailable");
    shutdown.cancel();
}

#[tokio::test]
async fn sessions_list_returns_empty_when_no_sessions_opened() {
    let backend = ScriptedBackend::new(vec![]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(SessionsTestState {
        partner: Some(partner),
    });
    let (base, shutdown) = spawn(state).await;

    let res = reqwest::Client::new()
        .get(format!("{base}/api/evy/sessions"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: SessionsListResponse = res.json().await.expect("json");
    assert!(body.sessions.is_empty());
    shutdown.cancel();
}

#[tokio::test]
async fn sessions_list_returns_open_sessions_newest_first() {
    let backend = ScriptedBackend::new(vec!["q1?", "q2?", "q3?"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(SessionsTestState {
        partner: Some(partner),
    });
    let (base, shutdown) = spawn(state).await;

    let ids = open_sessions(&base, 3, &["first", "second", "third"]).await;
    assert_eq!(ids.len(), 3);

    let res = reqwest::Client::new()
        .get(format!("{base}/api/evy/sessions"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: SessionsListResponse = res.json().await.expect("json");
    assert_eq!(body.sessions.len(), 3);
    // Newest-first: ids opened later show earlier in the list.
    assert_eq!(body.sessions[0].id, ids[2]);
    assert_eq!(body.sessions[1].id, ids[1]);
    assert_eq!(body.sessions[2].id, ids[0]);
    // preview comes from topic.
    assert_eq!(body.sessions[0].preview, "third");
    // message_count includes the synthetic system marker, kickoff
    // turn, and partner's opening question = 3.
    assert_eq!(body.sessions[0].message_count, 3);
    shutdown.cancel();
}

#[tokio::test]
async fn sessions_delete_returns_204_on_hit() {
    let backend = ScriptedBackend::new(vec!["q?"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(SessionsTestState {
        partner: Some(partner),
    });
    let (base, shutdown) = spawn(state).await;

    let ids = open_sessions(&base, 1, &["topic"]).await;
    let sid = ids[0];

    let res = reqwest::Client::new()
        .delete(format!("{base}/api/evy/sessions/{sid}"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 204);

    // Confirm the listing no longer carries it.
    let res = reqwest::Client::new()
        .get(format!("{base}/api/evy/sessions"))
        .send()
        .await
        .expect("send 2");
    let body: SessionsListResponse = res.json().await.expect("json");
    assert!(body.sessions.is_empty(), "deleted session must not list");

    shutdown.cancel();
}

#[tokio::test]
async fn sessions_delete_returns_404_on_miss() {
    let backend = ScriptedBackend::new(vec![]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(SessionsTestState {
        partner: Some(partner),
    });
    let (base, shutdown) = spawn(state).await;

    let bogus = Uuid::new_v4();
    let res = reqwest::Client::new()
        .delete(format!("{base}/api/evy/sessions/{bogus}"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 404);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["kind"], "unknown_session");
    assert_eq!(body["session_id"], bogus.to_string());
    shutdown.cancel();
}

#[tokio::test]
async fn sessions_delete_returns_503_when_partner_absent() {
    let (base, shutdown) = spawn(Arc::new(StubAppState)).await;
    let bogus = Uuid::new_v4();
    let res = reqwest::Client::new()
        .delete(format!("{base}/api/evy/sessions/{bogus}"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 503);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["kind"], "unavailable");
    shutdown.cancel();
}

#[tokio::test]
async fn sessions_master_alias_routes_also_work() {
    let backend = ScriptedBackend::new(vec!["q?"]);
    let partner = Arc::new(ThinkingPartner::new(backend));
    let state = Arc::new(SessionsTestState {
        partner: Some(partner),
    });
    let (base, shutdown) = spawn(state).await;

    let ids = open_sessions(&base, 1, &["alias-topic"]).await;
    let sid = ids[0];

    // List via alias.
    let res = reqwest::Client::new()
        .get(format!("{base}/api/master/sessions"))
        .send()
        .await
        .expect("send list");
    assert_eq!(res.status(), 200);
    let body: SessionsListResponse = res.json().await.expect("json");
    assert_eq!(body.sessions.len(), 1);

    // Delete via alias.
    let res = reqwest::Client::new()
        .delete(format!("{base}/api/master/sessions/{sid}"))
        .send()
        .await
        .expect("send delete");
    assert_eq!(res.status(), 204);
    shutdown.cancel();
}
