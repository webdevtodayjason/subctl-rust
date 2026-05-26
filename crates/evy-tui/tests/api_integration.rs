//! Integration tests for [`evy_tui::ApiClient`] against a wiremock
//! server. These tests verify the wire format documented by
//! `evy-comms` is parsed correctly into the local types and that
//! decode errors carry the endpoint name for operator diagnosis.

use evy_core::{MandateId, ProviderKind, WorkerId, WorkerStatus};
use evy_tui::api::{ApiClient, ApiError, WorkerSummary};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn client_for(server: &MockServer) -> ApiClient {
    ApiClient::new(&server.uri()).expect("api client constructs against wiremock uri")
}

#[tokio::test]
async fn fetch_workers_parses_evy_comms_shape() {
    let server = MockServer::start().await;

    let worker = WorkerSummary {
        id: WorkerId::new(),
        provider: ProviderKind::ClaudeCode,
        mandate_id: MandateId::new(),
        status: WorkerStatus::Running,
    };

    Mock::given(method("GET"))
        .and(path("/api/evy/workers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![worker.clone()]))
        .mount(&server)
        .await;

    let client = client_for(&server).await;
    let workers = client.fetch_workers().await.expect("fetch workers");
    assert_eq!(workers, vec![worker]);
}

#[tokio::test]
async fn fetch_workers_handles_empty_array() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/evy/workers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<WorkerSummary>::new()))
        .mount(&server)
        .await;
    let client = client_for(&server).await;
    let workers = client.fetch_workers().await.expect("fetch workers");
    assert!(workers.is_empty());
}

#[tokio::test]
async fn fetch_workers_surfaces_decode_error_with_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/evy/workers"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    let client = client_for(&server).await;
    let err = client.fetch_workers().await.unwrap_err();
    match err {
        ApiError::Decode { endpoint, .. } => assert_eq!(endpoint, "workers"),
        other => panic!("expected Decode, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_jobs_parses_documented_shape() {
    let server = MockServer::start().await;
    let body = json!([
        {
            "id": "9b0a5b6e-0000-4000-8000-000000000000",
            "name": "heartbeat",
            "cron_expr": "*/5 * * * *",
            "action_kind": "log_heartbeat",
            "enabled": true
        }
    ]);
    Mock::given(method("GET"))
        .and(path("/api/evy/scheduler/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let client = client_for(&server).await;
    let jobs = client.fetch_jobs().await.expect("fetch jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].name, "heartbeat");
    assert_eq!(jobs[0].cron_expr, "*/5 * * * *");
    assert_eq!(jobs[0].action_kind, "log_heartbeat");
    assert!(jobs[0].enabled);
}

#[tokio::test]
async fn fetch_policy_returns_arbitrary_json() {
    let server = MockServer::start().await;
    let body = json!({
        "default_mode": "gated",
        "mode": {
            "trusted": {"allow": ["ls", "pwd"]},
            "gated": {"allow": []}
        }
    });
    Mock::given(method("GET"))
        .and(path("/api/evy/policy"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
        .mount(&server)
        .await;
    let client = client_for(&server).await;
    let view = client.fetch_policy().await.expect("fetch policy");
    assert_eq!(view.0, body);
}

#[tokio::test]
async fn http_500_surfaces_transport_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/evy/workers"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let client = client_for(&server).await;
    let err = client.fetch_workers().await.unwrap_err();
    assert!(matches!(err, ApiError::Transport(_)));
}

#[tokio::test]
async fn populates_app_state_from_fetched_workers() {
    let server = MockServer::start().await;
    let worker = WorkerSummary {
        id: WorkerId::new(),
        provider: ProviderKind::Codex,
        mandate_id: MandateId::new(),
        status: WorkerStatus::Pending,
    };
    Mock::given(method("GET"))
        .and(path("/api/evy/workers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![worker.clone()]))
        .mount(&server)
        .await;
    let client = client_for(&server).await;

    let mut app = evy_tui::App::new();
    let workers = client.fetch_workers().await.expect("fetch");
    app.set_workers(workers);
    assert_eq!(app.workers.len(), 1);
    assert_eq!(app.workers[0].id, worker.id);
}
