//! End-to-end tests for the evy-comms HTTP surface.
//!
//! These spin up a real axum server on an ephemeral port (via
//! [`HttpConfig::ephemeral`] + [`HttpServer::bind`]), drive it with
//! `reqwest`, and assert against the wire shape — not internal types.
//! That's deliberate: these tests are the contract the dashboard sees.

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

#[tokio::test]
async fn master_alias_routes_to_evy() {
    let (base, _bcast, shutdown) = spawn_stub_server().await;

    let res_evy = reqwest::get(format!("{base}/api/evy/workers"))
        .await
        .unwrap();
    let res_master = reqwest::get(format!("{base}/api/master/workers"))
        .await
        .unwrap();

    assert_eq!(res_evy.status(), 200);
    assert_eq!(res_master.status(), 200);

    let body_evy: Value = res_evy.json().await.unwrap();
    let body_master: Value = res_master.json().await.unwrap();
    assert_eq!(body_evy, body_master);

    let res_master_jobs = reqwest::get(format!("{base}/api/master/scheduler/jobs"))
        .await
        .unwrap();
    assert_eq!(res_master_jobs.status(), 200);

    shutdown.cancel();
}

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
