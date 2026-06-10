//! End-to-end tests for the W5 watchdog diagnostics surface:
//! `GET /api/evy/watchdogs/diag`, `POST /api/evy/watchdogs/{id}/restart`,
//! `POST /api/evy/watchdogs/{id}/kill`.
//!
//! Spins up a real axum server backed by an `AppState` whose
//! `watchdog_diag()` returns a live registry with a fast-ticking watchdog,
//! and drives the HTTP surface with `reqwest`. No daemon, no tmux.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use evy_comms::{
    AppState, EventBroadcaster, HttpConfig, HttpServer, JobSummary, TickFn, TickOutcome,
    WatchdogDiagRegistry, WatchdogSpec, WorkerSummary,
};
use evy_policy::Policy;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

struct WatchdogTestState {
    registry: Option<Arc<WatchdogDiagRegistry>>,
}

#[async_trait]
impl AppState for WatchdogTestState {
    async fn workers(&self) -> Vec<WorkerSummary> {
        Vec::new()
    }
    async fn jobs(&self) -> Vec<JobSummary> {
        Vec::new()
    }
    async fn policy(&self) -> Policy {
        Policy::default()
    }
    fn watchdog_diag(&self) -> Option<Arc<WatchdogDiagRegistry>> {
        self.registry.clone()
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

fn fast_healthy_tick() -> TickFn {
    Arc::new(|| {
        Box::pin(async {
            TickOutcome {
                healthy: true,
                error: None,
            }
        })
    })
}

fn fast_spec(id: &str, can_restart: bool) -> WatchdogSpec {
    WatchdogSpec {
        id: id.to_owned(),
        kind: id.to_owned(),
        expected_interval_secs: Some(1),
        period: Duration::from_millis(40),
        can_restart,
    }
}

/// Poll `/api/evy/watchdogs/diag` until `id` reports a non-empty
/// tick_history, returning the matching entry (or `None` on timeout).
async fn wait_for_diag(client: &reqwest::Client, base: &str, id: &str) -> Option<Value> {
    for _ in 0..60 {
        let resp = client
            .get(format!("{base}/api/evy/watchdogs/diag"))
            .send()
            .await
            .expect("diag get");
        assert_eq!(resp.status(), 200, "diag must be 200");
        let body: Value = resp.json().await.expect("diag json");
        let list = body
            .get("watchdogs")
            .and_then(Value::as_array)
            .expect("watchdogs array");
        if let Some(entry) = list.iter().find(|w| w["id"] == id) {
            if entry["tick_history"]
                .as_array()
                .is_some_and(|h| !h.is_empty())
            {
                return Some(entry.clone());
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

#[tokio::test]
async fn diag_reports_a_real_ticking_watchdog_in_v3_shape() {
    let registry = Arc::new(WatchdogDiagRegistry::new());
    registry.register(fast_spec("inbox-poll", true), fast_healthy_tick());
    let state = Arc::new(WatchdogTestState {
        registry: Some(registry.clone()),
    });
    let (base, shutdown) = spawn(state).await;
    let client = reqwest::Client::new();

    let entry = wait_for_diag(&client, &base, "inbox-poll")
        .await
        .expect("watchdog should tick and populate tick_history");

    // v3-shape parity — every field the dashboard reads is present.
    assert_eq!(entry["id"], "inbox-poll");
    assert_eq!(entry["kind"], "inbox-poll");
    assert_eq!(entry["status"], "healthy");
    assert_eq!(entry["expected_interval_seconds"], 1);
    assert_eq!(entry["can_restart"], true);
    assert!(entry["started_at"].is_string());
    assert!(entry["last_tick_at"].is_string());
    assert!(entry["age_seconds"].is_number());
    assert!(entry["last_tick_ago_seconds"].is_number());
    assert!(entry["tick_history"]
        .as_array()
        .is_some_and(|h| !h.is_empty()));
    assert_eq!(entry["recent_notifications"], serde_json::json!([]));
    assert!(entry["last_error"].is_null());
    assert!(entry["memory_bytes"].is_null());
    // First tick has a null delta_ms.
    assert!(entry["tick_history"][0]["delta_ms"].is_null());

    shutdown.cancel();
    registry.shutdown();
}

#[tokio::test]
async fn restart_round_trip() {
    let registry = Arc::new(WatchdogDiagRegistry::new());
    registry.register(fast_spec("auto-compact", true), fast_healthy_tick());
    let state = Arc::new(WatchdogTestState {
        registry: Some(registry.clone()),
    });
    let (base, shutdown) = spawn(state).await;
    let client = reqwest::Client::new();

    wait_for_diag(&client, &base, "auto-compact")
        .await
        .expect("initial tick");

    // Restart a restartable watchdog → 200 { ok: true }.
    let resp = client
        .post(format!("{base}/api/evy/watchdogs/auto-compact/restart"))
        .send()
        .await
        .expect("restart post");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("restart json");
    assert_eq!(body["ok"], true);

    // It re-arms and ticks again.
    wait_for_diag(&client, &base, "auto-compact")
        .await
        .expect("ticks again after restart");

    // Restarting an unknown id → 404 { ok: false, error }.
    let resp = client
        .post(format!("{base}/api/evy/watchdogs/nope/restart"))
        .send()
        .await
        .expect("restart unknown");
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["ok"], false);
    assert!(body["error"].is_string());

    shutdown.cancel();
    registry.shutdown();
}

#[tokio::test]
async fn restart_rejects_non_restartable() {
    let registry = Arc::new(WatchdogDiagRegistry::new());
    registry.register(fast_spec("telegram-listener", false), fast_healthy_tick());
    let state = Arc::new(WatchdogTestState {
        registry: Some(registry.clone()),
    });
    let (base, shutdown) = spawn(state).await;
    let client = reqwest::Client::new();

    wait_for_diag(&client, &base, "telegram-listener")
        .await
        .expect("tick");

    let resp = client
        .post(format!(
            "{base}/api/evy/watchdogs/telegram-listener/restart"
        ))
        .send()
        .await
        .expect("restart post");
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["ok"], false);

    shutdown.cancel();
    registry.shutdown();
}

#[tokio::test]
async fn kill_round_trip() {
    let registry = Arc::new(WatchdogDiagRegistry::new());
    registry.register(fast_spec("verifier-cluster", true), fast_healthy_tick());
    let state = Arc::new(WatchdogTestState {
        registry: Some(registry.clone()),
    });
    let (base, shutdown) = spawn(state).await;
    let client = reqwest::Client::new();

    wait_for_diag(&client, &base, "verifier-cluster")
        .await
        .expect("tick");

    // Kill → 200 { ok: true, killed_id }.
    let resp = client
        .post(format!("{base}/api/evy/watchdogs/verifier-cluster/kill"))
        .send()
        .await
        .expect("kill post");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("kill json");
    assert_eq!(body["ok"], true);
    assert_eq!(body["killed_id"], "verifier-cluster");

    // After kill it's gone from diag.
    let resp = client
        .get(format!("{base}/api/evy/watchdogs/diag"))
        .send()
        .await
        .expect("diag");
    let body: Value = resp.json().await.expect("json");
    let list = body["watchdogs"].as_array().expect("array");
    assert!(list.iter().all(|w| w["id"] != "verifier-cluster"));

    // Killing again → 404 { ok: false, error }.
    let resp = client
        .post(format!("{base}/api/evy/watchdogs/verifier-cluster/kill"))
        .send()
        .await
        .expect("kill again");
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["ok"], false);
    assert!(body["error"].is_string());

    shutdown.cancel();
    registry.shutdown();
}

#[tokio::test]
async fn diag_returns_empty_when_no_registry_wired() {
    let state = Arc::new(WatchdogTestState { registry: None });
    let (base, shutdown) = spawn(state).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/evy/watchdogs/diag"))
        .send()
        .await
        .expect("diag");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["watchdogs"], serde_json::json!([]));

    // Kill / restart against the no-registry surface → 404.
    let resp = client
        .post(format!("{base}/api/evy/watchdogs/anything/kill"))
        .send()
        .await
        .expect("kill");
    assert_eq!(resp.status(), 404);

    shutdown.cancel();
}
