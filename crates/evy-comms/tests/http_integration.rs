//! End-to-end tests for the evy-comms HTTP surface.
//!
//! These spin up a real axum server on an ephemeral port (via
//! [`HttpConfig::ephemeral`] + [`HttpServer::bind`]), drive it with
//! `reqwest`, and assert against the wire shape — not internal types.
//! That's deliberate: these tests are the contract the dashboard sees.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use eventsource_stream::Eventsource;
use evy_comms::{
    AppState, DaemonEvent, EventBroadcaster, HttpConfig, HttpServer, JobSummary, StubAppState,
    WorkerSummary,
};
use evy_core::{MandateId, ProviderKind, WorkerId, WorkerStatus};
use evy_scheduler::JobId;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// Helper: bind a fresh HTTP server on an ephemeral port, spawn its
/// serve future, and hand back (base_url, broadcaster, shutdown).
async fn spawn_server_with_state(
    state: Arc<dyn AppState>,
) -> (String, EventBroadcaster, CancellationToken) {
    let broadcaster = EventBroadcaster::new(64);
    let server = HttpServer::new(HttpConfig::ephemeral(), broadcaster.clone(), state);
    let bound = server.bind().await.expect("bind ephemeral");
    let addr = bound.local_addr();
    let shutdown = CancellationToken::new();
    let shutdown_for_task = shutdown.clone();
    tokio::spawn(async move {
        if let Err(e) = bound.serve(shutdown_for_task).await {
            eprintln!("serve task errored: {e}");
        }
    });
    // Give the listener a beat to start accepting.
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}"), broadcaster, shutdown)
}

async fn spawn_stub_server() -> (String, EventBroadcaster, CancellationToken) {
    spawn_server_with_state(Arc::new(StubAppState)).await
}

/// Helper: bind an ephemeral server that also serves the in-repo
/// `static/` directory. Used by the Phase 4 Slice A regression + happy-
/// path tests below.
async fn spawn_stub_server_with_static() -> (String, EventBroadcaster, CancellationToken, PathBuf) {
    let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    let config = HttpConfig::ephemeral().with_static_dir(static_dir.clone());
    let broadcaster = EventBroadcaster::new(64);
    let server = HttpServer::with_stub_state(config, broadcaster.clone());
    let bound = server.bind().await.expect("bind ephemeral");
    let addr = bound.local_addr();
    let shutdown = CancellationToken::new();
    let shutdown_for_task = shutdown.clone();
    tokio::spawn(async move {
        if let Err(e) = bound.serve(shutdown_for_task).await {
            eprintln!("serve task errored: {e}");
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}"), broadcaster, shutdown, static_dir)
}

#[tokio::test]
async fn http_server_binds_and_health_endpoint_responds() {
    let (base, _bcast, shutdown) = spawn_stub_server().await;

    let res = reqwest::get(format!("{base}/health"))
        .await
        .expect("GET /health");
    assert_eq!(res.status(), 200, "expected 200 from /health");
    let body: Value = res.json().await.expect("parse JSON body");
    assert_eq!(body["ok"], Value::Bool(true));
    assert!(
        body["version"].is_string(),
        "version must be a string, got {body:?}",
    );

    shutdown.cancel();
}

#[tokio::test]
async fn version_endpoint_returns_workspace_version() {
    let (base, _bcast, shutdown) = spawn_stub_server().await;

    let res = reqwest::get(format!("{base}/api/version")).await.unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert!(body["version"].is_string());

    shutdown.cancel();
}

#[tokio::test]
async fn workers_endpoint_returns_empty_array_for_stub() {
    let (base, _bcast, shutdown) = spawn_stub_server().await;

    let res = reqwest::get(format!("{base}/api/evy/workers"))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert!(body.is_array(), "expected array, got {body:?}");
    assert_eq!(body.as_array().unwrap().len(), 0);

    shutdown.cancel();
}

#[tokio::test]
async fn jobs_endpoint_returns_empty_array_for_stub() {
    let (base, _bcast, shutdown) = spawn_stub_server().await;

    let res = reqwest::get(format!("{base}/api/evy/scheduler/jobs"))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert!(body.is_array());
    assert_eq!(body.as_array().unwrap().len(), 0);

    shutdown.cancel();
}

#[tokio::test]
async fn policy_endpoint_serves_default_policy() {
    let (base, _bcast, shutdown) = spawn_stub_server().await;

    let res = reqwest::get(format!("{base}/api/evy/policy"))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    // Default policy is a JSON object; exact shape depends on
    // evy_policy::Policy serialization but it must be an object.
    let body: Value = res.json().await.unwrap();
    assert!(body.is_object(), "expected object, got {body:?}");

    shutdown.cancel();
}

/// GET both the `/api/evy/<rest>` and `/api/master/<rest>` forms of an
/// endpoint and assert they resolve to the SAME native handler — same
/// status and byte-identical JSON body. Proves the `/api/master/*` →
/// `/api/evy/*` rewrite lands legacy callers on the canonical route.
async fn assert_prefix_parity(base: &str, rest: &str) {
    let evy = reqwest::get(format!("{base}/api/evy/{rest}"))
        .await
        .unwrap_or_else(|e| panic!("GET /api/evy/{rest}: {e}"));
    let master = reqwest::get(format!("{base}/api/master/{rest}"))
        .await
        .unwrap_or_else(|e| panic!("GET /api/master/{rest}: {e}"));
    assert_eq!(
        evy.status(),
        200,
        "native /api/evy/{rest} must serve directly"
    );
    assert_eq!(
        master.status(),
        evy.status(),
        "/api/master/{rest} must mirror its /api/evy twin's status"
    );
    let evy_body: Value = evy.json().await.expect("evy json");
    let master_body: Value = master.json().await.expect("master json");
    assert_eq!(
        master_body, evy_body,
        "/api/master/{rest} must return the same body as /api/evy/{rest}"
    );
}

#[tokio::test]
async fn master_alias_dropped_post_cutover() {
    // The rewrite is SCOPED to the chat-tab surfaces v4 owns natively;
    // `/api/master/workers` is NOT claimed, so it is left untouched and falls
    // through to the reverse-proxy → v3 (v3 still owns that data). The native
    // `/api/evy/*` route still serves directly; the master form is no longer
    // natively handled (no Bun upstream in this harness → non-200).
    let (base, _bcast, shutdown) = spawn_stub_server().await;

    let res_evy = reqwest::get(format!("{base}/api/evy/workers"))
        .await
        .unwrap();
    assert_eq!(res_evy.status(), 200, "native /api/evy/* still serves");

    let res_master = reqwest::get(format!("{base}/api/master/workers"))
        .await
        .unwrap();
    assert_ne!(
        res_master.status(),
        200,
        "unclaimed /api/master/workers must NOT be served natively (rides the proxy)"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn master_alias_rewrites_claimed_chat_tab_paths_to_native_twins() {
    // The chat tab's CLAIMED `/api/master/*` paths are rewritten to `/api/evy/*`
    // BEFORE routing, so they resolve to the same native handler — byte-for-byte
    // parity. StubAppState has no partner, so both prefixes return the same
    // empty-session transcript / context-meter shape.
    let (base, _bcast, shutdown) = spawn_stub_server().await;

    assert_prefix_parity(&base, "transcript").await;
    assert_prefix_parity(&base, "context").await;

    shutdown.cancel();
}

#[tokio::test]
async fn master_events_sse_streams_through_the_rewrite() {
    // The SSE caveat: `/api/master/events` must stream exactly like
    // `/api/evy/events`. The rewrite is a request-only `map_request`, so the
    // long-lived response is never buffered — prove it end-to-end by reading
    // a real frame off the master-prefixed stream.
    let (base, broadcaster, shutdown) = spawn_stub_server().await;

    let stream_res = reqwest::Client::new()
        .get(format!("{base}/api/master/events"))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .expect("send SSE GET via /api/master/events");
    assert_eq!(stream_res.status(), 200);
    let content_type = stream_res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        content_type.starts_with("text/event-stream"),
        "expected text/event-stream from the rewritten master path, got {content_type}",
    );

    let mut events = stream_res.bytes_stream().eventsource();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let emitted = DaemonEvent::Heartbeat {
        ts: Utc::now(),
        providers_healthy: 3,
    };
    broadcaster.emit(emitted.clone());

    let frame = timeout(Duration::from_secs(2), events.next())
        .await
        .expect("SSE next() timed out — rewrite must not buffer the stream")
        .expect("event stream ended")
        .expect("event stream errored");
    let parsed: DaemonEvent = serde_json::from_str(&frame.data)
        .unwrap_or_else(|e| panic!("could not parse SSE frame '{}': {e}", frame.data));
    assert_eq!(parsed, emitted);

    shutdown.cancel();
}

// The unmatched-master-path → proxy fall-through is verified in its own test
// binary (`master_alias_fallthrough_integration.rs`): it pins
// `EVY_PROXY_UPSTREAM` at a closed port, which must not race the other tests
// in this file that share the process env.

#[tokio::test]
async fn sse_stream_delivers_emitted_events_to_clients() {
    let (base, broadcaster, shutdown) = spawn_stub_server().await;

    let client = reqwest::Client::new();
    let stream_res = client
        .get(format!("{base}/api/evy/events"))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .expect("send SSE GET");
    assert_eq!(stream_res.status(), 200);
    let content_type = stream_res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        content_type.starts_with("text/event-stream"),
        "expected text/event-stream, got {content_type}",
    );

    let mut events = stream_res.bytes_stream().eventsource();

    // Give the GET-handler a moment to subscribe before we emit. Without
    // this, the broadcast::send can race the subscribe and the event is
    // dropped (broadcast::Sender::send only delivers to *current*
    // receivers).
    tokio::time::sleep(Duration::from_millis(50)).await;

    let emitted = DaemonEvent::Heartbeat {
        ts: Utc::now(),
        providers_healthy: 3,
    };
    broadcaster.emit(emitted.clone());

    let frame = timeout(Duration::from_secs(2), events.next())
        .await
        .expect("SSE next() timed out")
        .expect("event stream ended")
        .expect("event stream errored");

    let parsed: DaemonEvent = serde_json::from_str(&frame.data)
        .unwrap_or_else(|e| panic!("could not parse SSE frame '{}': {e}", frame.data));
    assert_eq!(parsed, emitted);

    shutdown.cancel();
}

#[tokio::test]
async fn populated_app_state_surfaces_through_workers_endpoint() {
    use async_trait::async_trait;
    use evy_policy::Policy;

    #[derive(Clone)]
    struct PopulatedState {
        workers: Vec<WorkerSummary>,
        jobs: Vec<JobSummary>,
    }

    #[async_trait]
    impl AppState for PopulatedState {
        async fn workers(&self) -> Vec<WorkerSummary> {
            self.workers.clone()
        }
        async fn jobs(&self) -> Vec<JobSummary> {
            self.jobs.clone()
        }
        async fn policy(&self) -> Policy {
            Policy::default()
        }
    }

    let worker = WorkerSummary {
        id: WorkerId::new(),
        provider: ProviderKind::ClaudeCode,
        mandate_id: MandateId::new(),
        status: WorkerStatus::Running,
    };
    let job = JobSummary {
        id: JobId::new(),
        name: "nightly-research".to_owned(),
        cron_expr: "0 3 * * *".to_owned(),
        action_kind: "log_heartbeat".to_owned(),
        enabled: true,
    };
    let state = Arc::new(PopulatedState {
        workers: vec![worker.clone()],
        jobs: vec![job.clone()],
    });

    let (base, _bcast, shutdown) = spawn_server_with_state(state).await;

    let body: Vec<WorkerSummary> = reqwest::get(format!("{base}/api/evy/workers"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body, vec![worker]);

    let body: Vec<JobSummary> = reqwest::get(format!("{base}/api/evy/scheduler/jobs"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body, vec![job]);

    shutdown.cancel();
}

#[tokio::test]
async fn shutdown_token_stops_server_promptly() {
    let (base, _bcast, shutdown) = spawn_stub_server().await;

    // Sanity: server is up.
    let pre = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(pre.status(), 200);

    shutdown.cancel();
    // After cancellation, the listener should close. Give it a moment,
    // then assert further requests fail to connect.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let post = reqwest::Client::new()
        .get(format!("{base}/health"))
        .timeout(Duration::from_millis(200))
        .send()
        .await;
    assert!(
        post.is_err(),
        "expected connection refused after shutdown, got {post:?}",
    );
}

// ─── Phase 4 Slice A: static-file surface ───────────────────────────

#[tokio::test]
async fn root_serves_operator_console_html_when_static_dir_set() {
    let (base, _bcast, shutdown, static_dir) = spawn_stub_server_with_static().await;
    assert!(
        static_dir.join("index.html").is_file(),
        "test prerequisite: {} must exist",
        static_dir.join("index.html").display(),
    );

    let res = reqwest::get(format!("{base}/"))
        .await
        .expect("GET / against static-enabled server");
    assert_eq!(res.status(), 200, "expected 200 from /");
    let ct = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        ct.starts_with("text/html"),
        "expected text/html content-type, got {ct}",
    );
    let body = res.text().await.expect("read body");
    assert!(
        body.contains("evy") && body.contains("<html"),
        "expected HTML containing 'evy' brand, got {body:?}",
    );

    shutdown.cancel();
}

#[tokio::test]
async fn static_dir_serves_named_assets() {
    let (base, _bcast, shutdown, _static_dir) = spawn_stub_server_with_static().await;

    // app.js — module entry. Content-type should be JS-flavoured.
    let res = reqwest::get(format!("{base}/app.js"))
        .await
        .expect("GET /app.js");
    assert_eq!(res.status(), 200);
    let ct = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        ct.contains("javascript"),
        "expected javascript content-type, got {ct}",
    );

    // A per-tab module under tabs/.
    let res = reqwest::get(format!("{base}/tabs/workers.js"))
        .await
        .expect("GET /tabs/workers.js");
    assert_eq!(res.status(), 200);

    shutdown.cancel();
}

#[tokio::test]
async fn json_endpoints_still_win_against_static_fallback() {
    // Regression: when static_dir is set, ServeDir is registered as the
    // *fallback* — the API routes must still take precedence. Without
    // this guard, a future refactor could accidentally route /health to
    // the static surface and break every operator script.
    let (base, _bcast, shutdown, _) = spawn_stub_server_with_static().await;

    let res = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(res.status(), 200);
    let ct = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        ct.starts_with("application/json"),
        "expected application/json from /health, got {ct}",
    );
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["ok"], Value::Bool(true));

    let res = reqwest::get(format!("{base}/api/version")).await.unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert!(body["version"].is_string());

    let res = reqwest::get(format!("{base}/api/evy/workers"))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.json::<Value>().await.unwrap().is_array());

    let res = reqwest::get(format!("{base}/api/evy/scheduler/jobs"))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.json::<Value>().await.unwrap().is_array());

    let res = reqwest::get(format!("{base}/api/evy/policy"))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.json::<Value>().await.unwrap().is_object());

    shutdown.cancel();
}

#[tokio::test]
async fn missing_static_asset_yields_404() {
    let (base, _bcast, shutdown, _) = spawn_stub_server_with_static().await;

    let res = reqwest::get(format!("{base}/does-not-exist.js"))
        .await
        .expect("GET unknown asset");
    assert_eq!(res.status(), 404);

    shutdown.cancel();
}

// ── criterion #6 — POST /api/evy/notify + /api/evy/ask ────────────────────

/// Stub state that carries a Telegram bridge, for the notify/ask routes.
struct TelegramTestState(evy_comms::TelegramBridge);

#[async_trait::async_trait]
impl AppState for TelegramTestState {
    async fn workers(&self) -> Vec<WorkerSummary> {
        Vec::new()
    }
    async fn jobs(&self) -> Vec<JobSummary> {
        Vec::new()
    }
    async fn policy(&self) -> evy_policy::Policy {
        evy_policy::Policy::default()
    }
    fn telegram_bridge(&self) -> Option<evy_comms::TelegramBridge> {
        Some(self.0.clone())
    }
}

fn telegram_test_bridge(base_url: &str) -> evy_comms::TelegramBridge {
    let mut cfg = evy_comms::TelegramConfig::new("TESTTOKEN".to_string(), 12345);
    cfg.base_url = base_url.to_string();
    cfg.long_poll_timeout = Duration::from_millis(50);
    cfg.poll_interval = Duration::from_millis(20);
    evy_comms::TelegramBridge::new(cfg, Arc::new(evy_comms::AskRegistry::new()))
}

#[tokio::test]
async fn notify_without_bridge_returns_503() {
    let (base, _bc, shutdown) = spawn_stub_server().await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/evy/notify"))
        .json(&serde_json::json!({ "text": "hello" }))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["ok"], false);
    shutdown.cancel();
}

#[tokio::test]
async fn ask_without_bridge_returns_503() {
    let (base, _bc, shutdown) = spawn_stub_server().await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/evy/ask"))
        .json(&serde_json::json!({ "question": "go?" }))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 503);
    shutdown.cancel();
}

#[tokio::test]
async fn notify_with_bridge_sends_and_returns_ok() {
    let tg = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/botTESTTOKEN/sendMessage"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "ok": true, "result": { "message_id": 7 } })),
        )
        .expect(1)
        .mount(&tg)
        .await;

    let state = Arc::new(TelegramTestState(telegram_test_bridge(&tg.uri())));
    let (base, _bc, shutdown) = spawn_server_with_state(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/evy/notify"))
        .json(&serde_json::json!({ "text": "criterion six says hi" }))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["ok"], true);
    shutdown.cancel();
}

#[tokio::test]
async fn ask_with_no_reply_returns_504() {
    let tg = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/botTESTTOKEN/sendMessage"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "ok": true, "result": { "message_id": 8 } })),
        )
        .mount(&tg)
        .await;

    let state = Arc::new(TelegramTestState(telegram_test_bridge(&tg.uri())));
    let (base, _bc, shutdown) = spawn_server_with_state(state).await;

    // timeout_s = 0 expires immediately — no reply will ever arrive.
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/evy/ask"))
        .json(&serde_json::json!({ "question": "anyone there?", "timeout_s": 0 }))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 504);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["ok"], false);
    shutdown.cancel();
}
