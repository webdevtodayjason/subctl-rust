//! Integration tests for [`evy_memory::CogneeClient`] against a
//! `wiremock` mock of the Cognee HTTP service. The real local Cognee
//! port (`127.0.0.1:8745` in v3) is NEVER hit — `CogneeConfig::endpoint`
//! exists precisely so these tests can swap it.
//!
//! The promotion ticker is also exercised end-to-end here: we drive
//! a small [`ObservationLog`] through one [`PromotionTicker::tick`]
//! call and assert on the wiremock interactions.

use std::sync::Arc;

use evy_core::{WorkerId, WorkerStatus};
use evy_memory::{
    CogneeClient, CogneeConfig, Observation, ObservationKind, ObservationLog, PromotionTicker,
    DEFAULT_THRESHOLD,
};
use serde_json::json;
use tempfile::tempdir;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "cognee-test-token";

fn client(server_url: &str, with_auth: bool) -> CogneeClient {
    CogneeClient::new(CogneeConfig {
        endpoint: server_url.trim_end_matches('/').to_string(),
        api_key: if with_auth {
            Some(TOKEN.to_string())
        } else {
            None
        },
        timeout_secs: 5,
    })
}

// ─── health ────────────────────────────────────────────────────────────

#[tokio::test]
async fn health_returns_true_on_2xx() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server.uri(), false);
    assert!(c.health().await.expect("health ok"));
}

#[tokio::test]
async fn health_returns_false_on_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let c = client(&server.uri(), false);
    assert!(!c.health().await.expect("health ok"));
}

#[tokio::test]
async fn health_swallows_transport_failure() {
    // A guaranteed-unreachable endpoint — Cognee isn't listening at
    // localhost:1 on any sane machine. The client should report
    // `Ok(false)` rather than surfacing the transport error.
    let c = CogneeClient::new(CogneeConfig {
        endpoint: "http://127.0.0.1:1".to_string(),
        api_key: None,
        timeout_secs: 1,
    });
    assert!(!c.health().await.expect("transport failure → Ok(false)"));
}

// ─── add_observation (POST /remember) ──────────────────────────────────

#[tokio::test]
async fn add_observation_posts_text_and_metadata() {
    let server = MockServer::start().await;
    let obs = Observation::new(ObservationKind::OperatorMessage {
        channel: "telegram".into(),
        text: "promote me".into(),
    });
    Mock::given(method("POST"))
        .and(path("/remember"))
        .and(header("content-type", "application/json"))
        .and(body_partial_json(json!({
            "metadata": { "source_obs_id": obs.id.to_string(), "kind": "operator_message" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "cog-123"})))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server.uri(), false);
    c.add_observation(&obs).await.expect("add ok");
}

#[tokio::test]
async fn add_observation_attaches_bearer_token_when_configured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/remember"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": null})))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server.uri(), true);
    let obs = Observation::new(ObservationKind::DaemonBooted {
        version: "0.1.0".into(),
    });
    c.add_observation(&obs).await.expect("add ok");
}

#[tokio::test]
async fn add_observation_surfaces_non_2xx_as_io_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/remember"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&server)
        .await;

    let c = client(&server.uri(), false);
    let obs = Observation::new(ObservationKind::DaemonBooted {
        version: "0.1.0".into(),
    });
    let err = c.add_observation(&obs).await.expect_err("must fail");
    let s = err.to_string();
    assert!(s.contains("503"), "got: {s}");
    assert!(s.contains("upstream down"), "got: {s}");
}

// ─── query (POST /recall) ──────────────────────────────────────────────

#[tokio::test]
async fn query_parses_hits_and_round_trips_source_obs_id() {
    let server = MockServer::start().await;
    let obs_id = uuid::Uuid::new_v4();
    Mock::given(method("POST"))
        .and(path("/recall"))
        .and(body_partial_json(
            json!({ "query": "what did we learn?", "top_k": 10 }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hits": [
                {
                    "text": "the operator prefers single-commit PRs",
                    "score": 0.87,
                    "metadata": { "source_obs_id": obs_id.to_string(), "kind": "operator_message" }
                },
                {
                    "text": "stale legacy backfill row",
                    "metadata": {}
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server.uri(), false);
    let hits = c.query("what did we learn?").await.expect("query ok");
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].content, "the operator prefers single-commit PRs");
    assert!(
        (hits[0].score - 0.87).abs() < 1e-6,
        "score: {}",
        hits[0].score
    );
    assert_eq!(hits[0].source_obs_id, Some(obs_id));
    // Missing-score hit defaults to 0.0; missing metadata → None.
    assert!((hits[1].score - 0.0).abs() < 1e-6);
    assert_eq!(hits[1].source_obs_id, None);
}

#[tokio::test]
async fn query_returns_empty_when_hits_omitted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/recall"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;

    let c = client(&server.uri(), false);
    let hits = c.query("anything").await.expect("query ok");
    assert!(hits.is_empty());
}

// ─── promotion ticker end-to-end ────────────────────────────────────────

#[tokio::test]
async fn ticker_promotes_only_above_threshold() {
    let server = MockServer::start().await;
    // Match any POST /remember and count.
    Mock::given(method("POST"))
        .and(path("/remember"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": null})))
        .mount(&server)
        .await;

    let dir = tempdir().expect("tempdir");
    let log = Arc::new(
        ObservationLog::open(&dir.path().join("obs.db"))
            .await
            .expect("open log"),
    );
    // 3 rows: one OperatorMessage (0.9 → promote), one successful
    // WorkerCompleted (0.8 → promote), one DaemonBooted (0.3 → skip),
    // one DaemonShutdown (filtered before scoring).
    log.append(Observation::new(ObservationKind::OperatorMessage {
        channel: "telegram".into(),
        text: "hello".into(),
    }))
    .await
    .expect("append");
    log.append(Observation::new(ObservationKind::WorkerCompleted {
        worker_id: WorkerId::new(),
        status: WorkerStatus::Succeeded,
    }))
    .await
    .expect("append");
    log.append(Observation::new(ObservationKind::DaemonBooted {
        version: "0.1.0".into(),
    }))
    .await
    .expect("append");
    log.append(Observation::new(ObservationKind::DaemonShutdown {
        reason: "sigterm".into(),
    }))
    .await
    .expect("append");

    let cognee = Arc::new(client(&server.uri(), false));
    let ticker = PromotionTicker::new(log.clone(), cognee, DEFAULT_THRESHOLD);

    let report = ticker.tick().await.expect("tick ok");
    // DaemonShutdown is invisible to the report — three considered.
    assert_eq!(report.considered, 3);
    assert_eq!(report.promoted, 2);
    assert_eq!(report.skipped, 1);
    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);

    // Second tick on the unchanged log re-evaluates nothing because
    // the watermark caught up to the newest row.
    let again = ticker.tick().await.expect("tick ok");
    assert_eq!(again.considered, 0);
    assert_eq!(again.promoted, 0);
    assert_eq!(again.skipped, 0);
}

#[tokio::test]
async fn ticker_collects_remote_failures_into_report() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/remember"))
        .respond_with(ResponseTemplate::new(500).set_body_string("kaboom"))
        .mount(&server)
        .await;

    let dir = tempdir().expect("tempdir");
    let log = Arc::new(
        ObservationLog::open(&dir.path().join("obs.db"))
            .await
            .expect("open log"),
    );
    log.append(Observation::new(ObservationKind::OperatorMessage {
        channel: "telegram".into(),
        text: "hello".into(),
    }))
    .await
    .expect("append");

    let cognee = Arc::new(client(&server.uri(), false));
    let ticker = PromotionTicker::new(log, cognee, DEFAULT_THRESHOLD);
    let report = ticker.tick().await.expect("tick ok");
    assert_eq!(report.considered, 1);
    assert_eq!(report.promoted, 0);
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("500"), "got: {:?}", report.errors);
}

// ─── #[ignore]'d real-Cognee smoke ──────────────────────────────────────

/// Smoke test against the operator's real local Cognee instance. Skipped
/// in CI; run with `cargo test -p evy-memory --test cognee_mock -- --ignored`.
#[tokio::test]
#[ignore = "hits real local Cognee on http://127.0.0.1:8745"]
async fn smoke_local_cognee_health() {
    let c = CogneeClient::new(CogneeConfig::default());
    let reachable = c.health().await.expect("health probe");
    eprintln!("cognee health @ {} → reachable={reachable}", c.endpoint());
}
