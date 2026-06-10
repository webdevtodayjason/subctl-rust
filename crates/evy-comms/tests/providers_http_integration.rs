//! End-to-end tests for the v4-native providers/models/catalogs family
//! (W1). Spins up a real axum server on an ephemeral port, drives it with
//! `reqwest`, and asserts the v3 wire shape — the contract the dashboard
//! sees.
//!
//! These handlers read process-global env (`SUBCTL_CONFIG_DIR`,
//! `SUBCTL_LMSTUDIO_HOST`, `SUBCTL_ACCOUNTS_CONF`), so every test that
//! depends on them holds a shared async lock and points them at hermetic
//! temp paths / a closed port. LM Studio is never reachable in these tests
//! (host → `127.0.0.1:1`), exercising the `unreachable` / empty-catalog
//! paths deterministically.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use evy_comms::{
    AppState, EventBroadcaster, HttpConfig, HttpServer, JobSummary, ProviderCatalogData,
    StubAppState, WorkerSummary,
};
use evy_policy::Policy;
use serde_json::{json, Value};
use tokio::sync::{Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;

/// Serializes env-dependent tests within this binary (env is process-global).
fn env_lock() -> &'static Mutex<()> {
    static L: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// A definitely-closed host → connection refused (fast `unreachable`).
const CLOSED_LM_HOST: &str = "http://127.0.0.1:1";

async fn lock_env() -> MutexGuard<'static, ()> {
    env_lock().lock().await
}

async fn spawn(state: Arc<dyn AppState>) -> (String, CancellationToken) {
    let server = HttpServer::new(HttpConfig::ephemeral(), EventBroadcaster::new(16), state);
    let bound = server.bind().await.expect("bind ephemeral");
    let addr = bound.local_addr();
    let shutdown = CancellationToken::new();
    let s2 = shutdown.clone();
    tokio::spawn(async move {
        let _ = bound.serve(s2).await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}"), shutdown)
}

/// AppState whose `provider_catalog()` returns wired pi-ai data, so the
/// catalog-derived branches are exercised.
struct CatalogState(ProviderCatalogData);

#[async_trait]
impl AppState for CatalogState {
    async fn workers(&self) -> Vec<WorkerSummary> {
        Vec::new()
    }
    async fn jobs(&self) -> Vec<JobSummary> {
        Vec::new()
    }
    async fn policy(&self) -> Policy {
        Policy::default()
    }
    fn provider_catalog(&self) -> Option<ProviderCatalogData> {
        Some(self.0.clone())
    }
}

#[tokio::test]
async fn models_unreachable_returns_502_unreachable_shape() {
    let _env = lock_env().await;
    std::env::set_var("SUBCTL_LMSTUDIO_HOST", CLOSED_LM_HOST);
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("SUBCTL_CONFIG_DIR", dir.path());

    let (base, shutdown) = spawn(Arc::new(StubAppState)).await;
    let res = reqwest::get(format!("{base}/api/models")).await.unwrap();
    assert_eq!(res.status(), 502);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["kind"], "unreachable");
    assert_eq!(body["host"], CLOSED_LM_HOST);
    shutdown.cancel();
}

#[tokio::test]
async fn models_refresh_unreachable_returns_502() {
    let _env = lock_env().await;
    std::env::set_var("SUBCTL_LMSTUDIO_HOST", CLOSED_LM_HOST);
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("SUBCTL_CONFIG_DIR", dir.path());

    let (base, shutdown) = spawn(Arc::new(StubAppState)).await;
    let res = reqwest::Client::new()
        .post(format!("{base}/api/models/refresh"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 502);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["kind"], "unreachable");
    shutdown.cancel();
}

#[tokio::test]
async fn providers_empty_catalog_returns_shape_with_array() {
    let _env = lock_env().await;
    std::env::set_var("SUBCTL_LMSTUDIO_HOST", CLOSED_LM_HOST);
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("SUBCTL_CONFIG_DIR", dir.path());

    let (base, shutdown) = spawn(Arc::new(StubAppState)).await;
    let res = reqwest::get(format!("{base}/api/providers")).await.unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["ok"], true);
    // LM Studio unreachable + stub catalog → empty list, correct shape.
    assert!(body["providers"].is_array());
    assert_eq!(body["providers"].as_array().unwrap().len(), 0);
    shutdown.cancel();
}

#[tokio::test]
async fn providers_with_wired_catalog_includes_cloud_rows() {
    let _env = lock_env().await;
    std::env::set_var("SUBCTL_LMSTUDIO_HOST", CLOSED_LM_HOST);
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("SUBCTL_CONFIG_DIR", dir.path());

    let data = ProviderCatalogData {
        provider_entries: vec![json!({ "id": "anthropic", "display": "Anthropic Claude", "kind": "cloud" })],
        uncached: vec![json!({ "provider": "groq", "cached": false, "models_in_bundle": 18 })],
        provider_ids: HashSet::from(["anthropic".to_string()]),
    };
    let (base, shutdown) = spawn(Arc::new(CatalogState(data))).await;

    let body: Value = reqwest::get(format!("{base}/api/providers"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["id"].as_str())
        .collect();
    assert!(ids.contains(&"anthropic"), "cloud row surfaced: {ids:?}");
    shutdown.cancel();
}

#[tokio::test]
async fn catalogs_returns_cached_and_uncached_arrays() {
    let _env = lock_env().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("SUBCTL_CONFIG_DIR", dir.path());
    // Seed one on-disk catalog cache file.
    let cat_dir = dir.path().join("catalogs");
    std::fs::create_dir_all(&cat_dir).unwrap();
    std::fs::write(
        cat_dir.join("openai.json"),
        json!({ "provider": "openai", "source": "live-fetch", "fetched_at": "t", "models": [{}, {}] })
            .to_string(),
    )
    .unwrap();

    let data = ProviderCatalogData {
        provider_entries: vec![],
        uncached: vec![json!({ "provider": "groq", "cached": false, "models_in_bundle": 18 })],
        provider_ids: HashSet::new(),
    };
    let (base, shutdown) = spawn(Arc::new(CatalogState(data))).await;

    let body: Value = reqwest::get(format!("{base}/api/catalogs"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["ok"], true);
    let cached = body["cached"].as_array().unwrap();
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0]["provider"], "openai");
    assert_eq!(cached[0]["model_count"], 2);
    let uncached = body["uncached"].as_array().unwrap();
    assert_eq!(uncached.len(), 1);
    assert_eq!(uncached[0]["provider"], "groq");
    shutdown.cancel();
}

#[tokio::test]
async fn profiles_post_then_delete_round_trip() {
    let _env = lock_env().await;
    let dir = tempfile::tempdir().unwrap();
    let conf = dir.path().join("accounts.conf");
    std::env::set_var("SUBCTL_ACCOUNTS_CONF", &conf);

    let client = reqwest::Client::new();
    // Add a profile.
    let add = client
        .post(format!(
            "{}/api/providers/profiles",
            spawn(Arc::new(StubAppState)).await.0
        ))
        .json(&json!({
            "alias": "claude-it", "provider": "anthropic",
            "email": "it@test.com", "config_dir": "~/.claude-it"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(add.status(), 200);
    let body: Value = add.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["mode"], "add");
    assert!(conf.exists(), "accounts.conf written");

    // Delete it.
    let (base, shutdown) = spawn(Arc::new(StubAppState)).await;
    let del = client
        .request(reqwest::Method::DELETE, format!("{base}/api/providers/profiles"))
        .json(&json!({ "alias": "claude-it" }))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 200);
    let body: Value = del.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["alias"], "claude-it");
    shutdown.cancel();
}

#[tokio::test]
async fn profiles_post_invalid_json_returns_400() {
    let _env = lock_env().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("SUBCTL_ACCOUNTS_CONF", dir.path().join("accounts.conf"));

    let (base, shutdown) = spawn(Arc::new(StubAppState)).await;
    let res = reqwest::Client::new()
        .post(format!("{base}/api/providers/profiles"))
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"], "invalid JSON");
    shutdown.cancel();
}
