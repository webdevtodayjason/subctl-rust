//! End-to-end tests for the dashboard chat-tab contract absorbed from the v3
//! BFF (`dashboard/lib/v4-bridge.ts`) into the native v4 handlers — Phase-3
//! slice 2.
//!
//! The chat tab speaks a fire-and-listen dialect: `POST /api/evy/chat`
//! `{text,...}` only checks `r.ok`, and the reply tokens arrive on the
//! long-lived `GET /api/evy/events` stream as NAMED frames (`message_update`
//! `{assistantMessageEvent:{type:"text_delta",delta}}`, `message_end`). These
//! tests prove the native daemon now speaks that contract directly, while the
//! canonical v4 `{message}` dialect (chat-tui/curl) is unchanged.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use evy_comms::{
    AppState, ChatResponse, EventBroadcaster, HttpConfig, HttpServer, JobSummary, WorkerSummary,
};
use evy_policy::Policy;
use evy_skills::SkillRegistry;
use evy_thinking::{LlmBackend, Message, Result as ThinkingResult, ThinkingError, ThinkingPartner};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// Backend returning canned strings; `stream_respond`'s default impl turns
/// each into a single `Token`, so the chat tab sees a real streamed delta.
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

struct ChatTestState {
    partner: Option<Arc<ThinkingPartner>>,
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
        None
    }
}

fn state_with(replies: Vec<&str>) -> Arc<ChatTestState> {
    let backend = ScriptedBackend::new(replies);
    Arc::new(ChatTestState {
        partner: Some(Arc::new(ThinkingPartner::new(backend))),
    })
}

async fn spawn(state: Arc<dyn AppState>) -> (String, CancellationToken) {
    let broadcaster = EventBroadcaster::new(256);
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

/// Poll `GET /api/evy/transcript` (session-less) until it reports at least `n`
/// visible messages, or panic after ~1s. Proves the UI turn (fire-and-forget)
/// landed AND that the session-less GET resolves to the current chat session.
async fn wait_transcript_total(base: &str, n: u64) -> Value {
    let client = reqwest::Client::new();
    for _ in 0..50 {
        let tr: Value = client
            .get(format!("{base}/api/evy/transcript"))
            .send()
            .await
            .expect("transcript send")
            .json()
            .await
            .expect("transcript json");
        if tr["total"].as_u64().unwrap_or(0) >= n {
            return tr;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("transcript never reached total {n}");
}

#[tokio::test]
async fn ui_dialect_acks_ok_and_streams_named_frames_onto_events_bus() {
    let (base, shutdown) = spawn(state_with(vec!["hello from evy"])).await;
    let client = reqwest::Client::new();

    // 1. Connect to the events bus FIRST — broadcast only reaches current subs.
    let events_res = client
        .get(format!("{base}/api/evy/events"))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .expect("events GET");
    assert_eq!(events_res.status(), 200);
    let mut events = events_res.bytes_stream().eventsource();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 2. Fire a UI-dialect turn — must ack {ok:true} immediately (no reply on
    //    the POST body; the chat tab only checks r.ok).
    let post = client
        .post(format!("{base}/api/evy/chat"))
        .json(&json!({ "text": "hello", "source": "input", "attachments": [] }))
        .send()
        .await
        .expect("chat POST");
    assert_eq!(post.status(), 200);
    let ack: Value = post.json().await.expect("ack json");
    assert_eq!(ack, json!({ "ok": true }));

    // 3. The reply streams onto the events bus as the chat.js vocabulary.
    let mut names = Vec::new();
    let mut deltas = String::new();
    loop {
        let frame = timeout(Duration::from_secs(2), events.next())
            .await
            .expect("events frame timed out")
            .expect("event stream ended")
            .expect("event stream errored");
        names.push(frame.event.clone());
        if frame.event == "message_update" {
            let d: Value = serde_json::from_str(&frame.data).expect("delta json");
            assert_eq!(d["assistantMessageEvent"]["type"], "text_delta");
            if let Some(s) = d["assistantMessageEvent"]["delta"].as_str() {
                deltas.push_str(s);
            }
        }
        if frame.event == "agent_end" {
            break;
        }
    }
    assert!(
        names.contains(&"message_update".to_string()),
        "expected a message_update frame, saw {names:?}"
    );
    assert!(
        names.contains(&"message_end".to_string()),
        "expected a message_end terminator, saw {names:?}"
    );
    // The v3-master `agent_end` alias must IMMEDIATELY follow message_end —
    // it's what releases chat.js's per-project one-shot captures
    // (attachOneShotAssistantCapture closes its EventSource only on
    // agent_end).
    let end_pos = names
        .iter()
        .position(|n| n == "message_end")
        .expect("message_end position");
    assert_eq!(
        names.get(end_pos + 1).map(String::as_str),
        Some("agent_end"),
        "agent_end must immediately follow message_end, saw {names:?}"
    );
    assert!(
        deltas.contains("hello from evy"),
        "streamed deltas must carry the reply, got {deltas:?}"
    );

    // 4. The transcript grew — session-less GET resolves to the current session
    //    (operator turn + assistant reply = 2 visible messages).
    let tr = wait_transcript_total(&base, 2).await;
    assert_eq!(tr["ok"], true);

    shutdown.cancel();
}

#[tokio::test]
async fn ui_dialect_empty_text_returns_400_in_bff_shape() {
    let (base, shutdown) = spawn(state_with(vec!["unused"])).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .json(&json!({ "text": "   ", "source": "input" }))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body, json!({ "ok": false, "error": "empty message" }));

    shutdown.cancel();
}

#[tokio::test]
async fn v4_dialect_message_body_still_returns_synchronous_chat_response() {
    // Regression guard: a `{message}` body is the v4 dialect — it must keep
    // returning the synchronous ChatResponse on the POST body, NOT {ok:true}.
    let (base, shutdown) = spawn(state_with(vec!["sync reply"])).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/chat"))
        .json(&json!({ "session_id": null, "message": "hi" }))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: ChatResponse = res.json().await.expect("ChatResponse json");
    assert_eq!(body.response, "sync reply");
    assert_ne!(body.session_id, uuid::Uuid::nil());

    shutdown.cancel();
}

#[tokio::test]
async fn clear_resets_the_current_session_so_next_transcript_is_empty() {
    let (base, shutdown) = spawn(state_with(vec!["first reply"])).await;
    let client = reqwest::Client::new();

    // A UI turn opens + becomes the current session; transcript grows to 2.
    let post = client
        .post(format!("{base}/api/evy/chat"))
        .json(&json!({ "text": "hello" }))
        .send()
        .await
        .expect("chat POST");
    assert_eq!(post.status(), 200);
    wait_transcript_total(&base, 2).await;

    // "New Chat": session-less clear resolves to the current session, archives
    // it, AND resets the holder (BFF's resetV4Session).
    let cleared: Value = client
        .post(format!("{base}/api/evy/transcript/clear"))
        .send()
        .await
        .expect("clear POST")
        .json()
        .await
        .expect("clear json");
    assert_eq!(cleared["ok"], true);

    // With the session dropped and the holder reset, a bare transcript is empty.
    let tr: Value = client
        .get(format!("{base}/api/evy/transcript"))
        .send()
        .await
        .expect("transcript send")
        .json()
        .await
        .expect("transcript json");
    assert_eq!(tr["ok"], true);
    assert_eq!(
        tr["total"].as_u64().unwrap_or(999),
        0,
        "clear must reset to a fresh chat"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn master_dialect_ui_post_acks_ok_end_to_end() {
    // The chat tab actually POSTs `/api/master/chat` {text}. The scoped
    // master→evy rewrite (slice 1) + the UI dialect (slice 2) must compose.
    let (base, shutdown) = spawn(state_with(vec!["via master"])).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/master/chat"))
        .json(&json!({ "text": "hello", "source": "input" }))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body, json!({ "ok": true }));

    shutdown.cancel();
}
