//! Criterion #4 — dashboard skeleton serves the operator console.
//!
//! ADR 0020 cutover criterion #4: "Dashboard skeleton serves the
//! operator console (read-only project registry view at minimum)."
//!
//! What this verifies on top of `crates/evy-comms/tests/http_integration.rs`:
//!
//! - The dashboard endpoints work end-to-end against a **populated**
//!   `AppState` carrying realistic worker + job summaries — the test
//!   in `evy-comms` already covers the wire shape with a stub state;
//!   here we exercise what the operator would actually see in
//!   production once `run_daemon` wires a real state surface.
//! - All seven operator-facing routes documented in the `evy-comms`
//!   rustdoc (`/health`, `/api/version`, `/api/evy/events`,
//!   `/api/evy/workers`, `/api/evy/scheduler/jobs`, `/api/evy/policy`,
//!   and the legacy `/api/master/*` aliases) respond with the expected
//!   shape.
//! - SSE delivers a `SchedulerFired` event to a connected client — this
//!   is the event taxonomy criterion #5 / #7 also lean on; verifying
//!   here as a one-stop "dashboard sees what the operator sees" smoke.
//!
//! Caveat surfaced in REPORT.md: `run_daemon` itself never constructs
//! an `HttpServer` — that wiring lands in Phase 3. This test verifies
//! the library's behavior, not the daemon binary's wiring.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use evy_comms::{
    AppState, DaemonEvent, EventBroadcaster, HttpConfig, HttpServer, JobSummary, WorkerSummary,
};
use evy_core::{MandateId, ProviderKind, WorkerId, WorkerStatus};
use evy_policy::Policy;
use evy_scheduler::{JobId, RunOutcome};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Realistic operator-console state: two workers (one Running, one
/// Succeeded), one armed cron job. Mirrors what the daemon would
/// publish once Phase 3 lands the real AppState bridge.
#[derive(Clone)]
struct DashboardState {
    workers: Vec<WorkerSummary>,
    jobs: Vec<JobSummary>,
    policy: Policy,
}

#[async_trait]
impl AppState for DashboardState {
    async fn workers(&self) -> Vec<WorkerSummary> {
        self.workers.clone()
    }
    async fn jobs(&self) -> Vec<JobSummary> {
        self.jobs.clone()
    }
    async fn policy(&self) -> Policy {
        self.policy.clone()
    }
}

fn realistic_state() -> Arc<DashboardState> {
    Arc::new(DashboardState {
        workers: vec![
            WorkerSummary {
                id: WorkerId::new(),
                provider: ProviderKind::ClaudeCode,
                mandate_id: MandateId::new(),
                status: WorkerStatus::Running,
            },
            WorkerSummary {
                id: WorkerId::new(),
                provider: ProviderKind::Codex,
                mandate_id: MandateId::new(),
                status: WorkerStatus::Succeeded,
            },
        ],
        jobs: vec![JobSummary {
            id: JobId::new(),
            name: "daily-standup".to_owned(),
            cron_expr: "0 9 * * 1-5".to_owned(),
            action_kind: "log_heartbeat".to_owned(),
            enabled: true,
        }],
        policy: Policy::default(),
    })
}

/// Bind the server on an ephemeral port + spawn its serve loop. Returns
/// the base URL, broadcaster handle, and shutdown token.
async fn spawn_server(state: Arc<DashboardState>) -> (String, EventBroadcaster, CancellationToken) {
    let broadcaster = EventBroadcaster::new(64);
    let server = HttpServer::new(HttpConfig::ephemeral(), broadcaster.clone(), state);
    let bound = server.bind().await.expect("bind ephemeral");
    let addr = bound.local_addr();
    let shutdown = CancellationToken::new();
    let shutdown_for_task = shutdown.clone();
    tokio::spawn(async move {
        if let Err(e) = bound.serve(shutdown_for_task).await {
            eprintln!("dashboard serve errored: {e}");
        }
    });
    // Brief beat so the kernel finishes accepting on the listener.
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}"), broadcaster, shutdown)
}

#[tokio::test]
async fn all_seven_operator_routes_respond_2xx_with_populated_state() {
    let state = realistic_state();
    let expected_workers = state.workers.clone();
    let expected_jobs = state.jobs.clone();
    let (base, _bcast, shutdown) = spawn_server(state).await;

    // /health — { ok: true, version: <str> }
    let res = reqwest::get(format!("{base}/health"))
        .await
        .expect("GET /health");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("health JSON");
    assert_eq!(body["ok"], Value::Bool(true));
    assert!(body["version"].is_string(), "version must be a string");

    // /api/version — { version: <str> }
    let res = reqwest::get(format!("{base}/api/version"))
        .await
        .expect("GET /api/version");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("version JSON");
    assert!(body["version"].is_string());

    // /api/evy/workers — populated array
    let workers: Vec<WorkerSummary> = reqwest::get(format!("{base}/api/evy/workers"))
        .await
        .expect("GET workers")
        .json()
        .await
        .expect("workers JSON");
    assert_eq!(
        workers, expected_workers,
        "workers must round-trip through HTTP"
    );

    // /api/evy/scheduler/jobs — single armed job
    let jobs: Vec<JobSummary> = reqwest::get(format!("{base}/api/evy/scheduler/jobs"))
        .await
        .expect("GET jobs")
        .json()
        .await
        .expect("jobs JSON");
    assert_eq!(jobs, expected_jobs, "jobs must round-trip through HTTP");

    // /api/evy/policy — Policy serialized as JSON object
    let policy: Value = reqwest::get(format!("{base}/api/evy/policy"))
        .await
        .expect("GET policy")
        .json()
        .await
        .expect("policy JSON");
    assert!(policy.is_object(), "policy must serialize as JSON object");

    // NOTE: the legacy `/api/master/*` aliases were intentionally dropped in the
    // full-cutover Phase 0 (so `/api/master/events|chat` fall through to the v3
    // Bun dashboard's Fork A bridge, and `/api/master/transcript|context` keep
    // Bun's session-id injection). The native `/api/evy/*` routes asserted above
    // are the operator console's surface; `/api/master/*` now reverse-proxies to
    // v3 (not asserted here — no Bun upstream in this test harness).

    shutdown.cancel();
}

#[tokio::test]
async fn sse_stream_delivers_scheduler_fired_events_to_dashboard_client() {
    let state = realistic_state();
    let (base, broadcaster, shutdown) = spawn_server(state).await;

    let stream_res = reqwest::Client::new()
        .get(format!("{base}/api/evy/events"))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .expect("send SSE GET");
    assert_eq!(stream_res.status(), 200);
    assert!(stream_res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .starts_with("text/event-stream"));

    let mut events = stream_res.bytes_stream().eventsource();

    // Race guard: broadcast::Sender::send only delivers to *current*
    // receivers. Give the axum handler time to subscribe before we emit
    // — without this sleep the event vanishes silently. This pattern
    // mirrors `crates/evy-comms/tests/http_integration.rs` lines 175–180.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let job_id = JobId::new();
    let run_id = Uuid::new_v4();
    let emitted = DaemonEvent::SchedulerFired {
        job_id,
        run_id,
        outcome: RunOutcome::Succeeded,
    };
    broadcaster.emit(emitted.clone());

    let frame = timeout(Duration::from_secs(2), events.next())
        .await
        .expect("SSE next() timed out")
        .expect("SSE stream ended")
        .expect("SSE stream errored");

    let parsed: DaemonEvent = serde_json::from_str(&frame.data)
        .unwrap_or_else(|e| panic!("could not parse SSE frame {:?}: {e}", frame.data));
    assert_eq!(
        parsed, emitted,
        "dashboard SSE must deliver the scheduler-fired event verbatim"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn health_endpoint_carries_workspace_version_string() {
    // Tiny but important: operators read the version from /health when
    // diagnosing "is this the v4 daemon?". The shape stays JSON-stable.
    let (base, _bcast, shutdown) = spawn_server(realistic_state()).await;
    let body: Value = reqwest::get(format!("{base}/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let v = body["version"].as_str().expect("version must be string");
    assert!(!v.is_empty(), "version must be non-empty");
    // Sanity: looks like semver. evy-comms defines VERSION as
    // env!("CARGO_PKG_VERSION") which on a Cargo workspace is the
    // package's own version (currently 0.1.0 per workspace.package).
    assert!(
        v.chars().any(|c| c.is_ascii_digit()),
        "version should contain a digit"
    );
    shutdown.cancel();
}
