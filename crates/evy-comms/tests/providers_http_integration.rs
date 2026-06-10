//! End-to-end tests for the v4-native providers/models/catalogs family
//! (W1) — the endpoints v4 serves natively: `/api/models`,
//! `/api/models/refresh`, and `/api/providers/profiles` (POST/DELETE).
//! `/api/providers` and `/api/catalogs` stay on the v3 reverse-proxy and
//! are not exercised here.
//!
//! Spins up a real axum server on an ephemeral port, drives it with
//! `reqwest`, and asserts the v3 wire shape. These handlers read
//! process-global env (`SUBCTL_CONFIG_DIR`, `SUBCTL_LMSTUDIO_HOST`,
//! `SUBCTL_ACCOUNTS_CONF`), so every test holds a shared async lock and
//! points them at hermetic temp paths / a closed port. LM Studio is never
//! reachable here (host → `127.0.0.1:1`), exercising the `unreachable`
//! path deterministically.

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

/// AppState whose `provider_catalog()` returns a wired pi-ai id set, so the
/// profile write-gate runs in strict mode.
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
async fn providers_and_catalogs_are_not_served_natively() {
    // These stay on the v3 reverse-proxy catch-all. Point the proxy upstream
    // at a closed port: a proxied route then yields 502 (bad gateway), while
    // a natively-handled route would return 200 regardless of the upstream.
    // So 502 here proves they fall through to the proxy.
    let _env = lock_env().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("SUBCTL_CONFIG_DIR", dir.path());
    std::env::set_var("EVY_PROXY_UPSTREAM", "127.0.0.1:1");

    let (base, shutdown) = spawn(Arc::new(StubAppState)).await;
    for path in ["/api/providers", "/api/catalogs"] {
        let res = reqwest::get(format!("{base}{path}")).await.unwrap();
        assert_eq!(
            res.status(),
            502,
            "{path} must fall through to the (now-dead) proxy, not be served natively"
        );
    }
    std::env::remove_var("EVY_PROXY_UPSTREAM");
    shutdown.cancel();
}

#[tokio::test]
async fn profiles_post_then_delete_round_trip() {
    let _env = lock_env().await;
    let dir = tempfile::tempdir().unwrap();
    let conf = dir.path().join("accounts.conf");
    std::env::set_var("SUBCTL_ACCOUNTS_CONF", &conf);

    let client = reqwest::Client::new();
    // Add a profile (StubAppState → write-gate None → permissive).
    let (base, shutdown) = spawn(Arc::new(StubAppState)).await;
    let add = client
        .post(format!("{base}/api/providers/profiles"))
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
    let del = client
        .request(
            reqwest::Method::DELETE,
            format!("{base}/api/providers/profiles"),
        )
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
async fn profiles_post_unknown_provider_400_when_catalog_wired() {
    // With a wired catalog id set, the write-gate runs in strict mode and
    // rejects providers not in the set (exercises provider_catalog() Some).
    let _env = lock_env().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("SUBCTL_ACCOUNTS_CONF", dir.path().join("accounts.conf"));

    let data = ProviderCatalogData {
        provider_ids: HashSet::from(["anthropic".to_string()]),
    };
    let (base, shutdown) = spawn(Arc::new(CatalogState(data))).await;
    let res = reqwest::Client::new()
        .post(format!("{base}/api/providers/profiles"))
        .json(&json!({ "alias": "x", "provider": "nope", "email": "e", "config_dir": "d" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("pi-ai catalog"));
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
