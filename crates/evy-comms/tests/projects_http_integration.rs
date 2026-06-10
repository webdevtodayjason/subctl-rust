//! End-to-end tests for the native projects CRUD + policy-preset surface.
//!
//! Spins up a real axum server on an ephemeral port (`StubAppState` — these
//! routes need no app state) and drives every endpoint over HTTP against
//! temp-dir fixtures. The handlers resolve their roots from process env
//! (`SUBCTL_CODE_ROOT`, `SUBCTL_CONFIG_DIR`, `HOME`, `SUBCTL_INSTALL_ROOT`),
//! which is process-global — so the env-dependent tests serialise on a
//! `tokio::sync::Mutex` (held across `.await`, unlike a `std` guard) and each
//! uses its own disposable tree.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evy_comms::{EventBroadcaster, HttpConfig, HttpServer, StubAppState};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Serialises tests that mutate process env (see module docs).
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

async fn spawn() -> (String, CancellationToken) {
    let broadcaster = EventBroadcaster::new(64);
    let server = HttpServer::new(HttpConfig::ephemeral(), broadcaster, Arc::new(StubAppState));
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

/// Point all four env roots at `base` and build the standard fixture layout:
/// `code/`, `cfg/evy/policy.json`, and an install tree with `node`/`generic`
/// presets. Returns nothing; the caller already holds `base`.
fn wire_env(base: &Path) {
    std::env::set_var("SUBCTL_CODE_ROOT", base.join("code"));
    std::env::set_var("SUBCTL_CONFIG_DIR", base.join("cfg"));
    std::env::set_var("HOME", base);
    std::env::set_var("SUBCTL_INSTALL_ROOT", base.join("install"));

    std::fs::create_dir_all(base.join("code")).unwrap();
    std::fs::create_dir_all(base.join("cfg").join("evy")).unwrap();
    let presets = base
        .join("install")
        .join("config")
        .join("policy")
        .join("presets");
    std::fs::create_dir_all(&presets).unwrap();
    std::fs::write(presets.join("node.toml"), "").unwrap();
    std::fs::write(presets.join("generic.toml"), "").unwrap();
}

fn seed_project(base: &Path, name: &str, in_policy: bool) {
    let proj = base.join("code").join(name);
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("CLAUDE.md"), "x").unwrap();
    let projects = if in_policy {
        json!([{ "path": proj.to_string_lossy(), "autonomy_level": "drive" }])
    } else {
        json!([])
    };
    std::fs::write(
        base.join("cfg").join("evy").join("policy.json"),
        json!({ "projects": projects }).to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn projects_list_both_prefixes_serve_natively() {
    let _g = ENV_LOCK.lock().await;
    let base = TempDir::new().unwrap();
    wire_env(base.path());
    seed_project(base.path(), "subctl", true);
    let (url, shutdown) = spawn().await;
    let client = reqwest::Client::new();

    for path in ["/api/evy/projects", "/api/projects"] {
        let res = client
            .get(format!("{url}{path}"))
            .send()
            .await
            .expect("send");
        assert_eq!(res.status(), 200, "{path} should be served natively");
        let body: Value = res.json().await.expect("json");
        assert_eq!(body["ok"], json!(true));
        let projects = body["projects"].as_array().unwrap();
        assert_eq!(projects.len(), 1, "{path}");
        assert_eq!(projects[0]["name"], json!("subctl"));
        assert_eq!(projects[0]["in_policy"], json!(true));
        assert_eq!(projects[0]["autonomy_level"], json!("drive"));
        assert_eq!(projects[0]["has_claude_md"], json!(true));
    }
    shutdown.cancel();
}

#[tokio::test]
async fn project_detail_returns_v3_shape() {
    let _g = ENV_LOCK.lock().await;
    let base = TempDir::new().unwrap();
    wire_env(base.path());
    seed_project(base.path(), "demo", false);
    let (url, shutdown) = spawn().await;
    let client = reqwest::Client::new();

    let res = client
        .get(format!("{url}/api/evy/projects/demo"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["name"], json!("demo"));
    assert_eq!(body["dev_teams"], json!([]));
    assert_eq!(body["prs"], json!([]));
    assert_eq!(body["issues"], json!([]));
    assert_eq!(body["flags"]["has_claude_md"], json!(true));
    assert_eq!(body["vault"]["exists"], json!(false));

    let missing = client
        .get(format!("{url}/api/evy/projects/nope"))
        .send()
        .await
        .expect("send");
    assert_eq!(missing.status(), 404);
    shutdown.cancel();
}

#[tokio::test]
async fn project_create_mkdir_vault_and_policy() {
    let _g = ENV_LOCK.lock().await;
    let base = TempDir::new().unwrap();
    wire_env(base.path());
    std::fs::write(
        base.path().join("cfg").join("evy").join("policy.json"),
        json!({ "projects": [] }).to_string(),
    )
    .unwrap();
    let (url, shutdown) = spawn().await;

    let res = reqwest::Client::new()
        .post(format!("{url}/api/evy/projects/create"))
        .json(
            &json!({ "name": "New Thing", "autonomy_level": "drive", "create_github_repo": false }),
        )
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["name"], json!("New-Thing"));
    assert!(base
        .path()
        .join("code")
        .join("New-Thing")
        .join("README.md")
        .exists());
    assert!(base
        .path()
        .join("Documents")
        .join("Obsidian Vault")
        .join("New-Thing")
        .join("RESUME.md")
        .exists());
    shutdown.cancel();
}

#[tokio::test]
async fn policy_presets_list_sorted() {
    let _g = ENV_LOCK.lock().await;
    let base = TempDir::new().unwrap();
    wire_env(base.path());
    let (url, shutdown) = spawn().await;

    let res = reqwest::Client::new()
        .get(format!("{url}/api/evy/policy/presets"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["presets"], json!(["generic", "node"]));
    shutdown.cancel();
}

#[tokio::test]
async fn policy_preset_apply_writes_toml_and_rejects_unknown() {
    let _g = ENV_LOCK.lock().await;
    let base = TempDir::new().unwrap();
    wire_env(base.path());
    seed_project(base.path(), "subctl", false);
    let (url, shutdown) = spawn().await;
    let client = reqwest::Client::new();

    let ok = client
        .post(format!("{url}/api/evy/policy/preset/subctl"))
        .json(&json!({ "preset": "node" }))
        .send()
        .await
        .expect("send");
    assert_eq!(ok.status(), 200);
    let body: Value = ok.json().await.expect("json");
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["doc"]["preset"], json!("node"));
    let written = std::fs::read_to_string(
        base.path()
            .join("code")
            .join("subctl")
            .join(".subctl")
            .join("policy.toml"),
    )
    .unwrap();
    assert!(written.contains("preset = \"node\""));

    let unknown = client
        .post(format!("{url}/api/evy/policy/preset/subctl"))
        .json(&json!({ "preset": "ghost" }))
        .send()
        .await
        .expect("send");
    assert_eq!(unknown.status(), 400);
    let ubody: Value = unknown.json().await.expect("json");
    assert!(ubody["error"].as_str().unwrap().contains("unknown preset"));
    shutdown.cancel();
}
