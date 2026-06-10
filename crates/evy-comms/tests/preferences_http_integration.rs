//! End-to-end tests for the native settings / auth-status / update-status
//! read surface (W2).
//!
//! Boots a real axum server on an ephemeral port backed by `StubAppState`
//! (these endpoints read `~/.config/subctl` files directly and need no daemon
//! state), points `SUBCTL_CONFIG_DIR` at a temp fixture tree, and asserts each
//! endpoint returns 200 + the v3 wire shape.
//!
//! All assertions live in ONE test: `tests/*.rs` files share a process, so
//! per-test `set_var` of `SUBCTL_CONFIG_DIR` would race. One test = one env
//! setup = deterministic.

use std::sync::Arc;
use std::time::Duration;

use evy_comms::{EventBroadcaster, HttpConfig, HttpServer, StubAppState};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

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

async fn get_json(base: &str, path: &str) -> (reqwest::StatusCode, Value) {
    let res = reqwest::Client::new()
        .get(format!("{base}{path}"))
        .send()
        .await
        .expect("send");
    let status = res.status();
    let body: Value = res.json().await.expect("json");
    (status, body)
}

#[tokio::test]
async fn settings_auth_update_read_surface_is_native() {
    // ── fixture config tree ──────────────────────────────────────────────
    let cfg = tempfile::tempdir().expect("tempdir");
    let cfg_path = cfg.path();
    std::fs::create_dir_all(cfg_path.join("evy")).unwrap();

    // secrets.json — brave set, lmstudio empty (= not set)
    std::fs::write(
        cfg_path.join("secrets.json"),
        r#"{"brave_api_key":"bravevalue","lmstudio_api_token":""}"#,
    )
    .unwrap();

    // accounts.conf — absolute config_dirs so auth detection is HOME-independent
    let claude_dir = cfg_path.join("acct-claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(claude_dir.join(".credentials.json"), "{}").unwrap();
    let codex_dir = cfg_path.join("acct-codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let accounts_conf = cfg_path.join("accounts.conf");
    std::fs::write(
        &accounts_conf,
        format!(
            "# header\n\
             claude-a | claude | a@b.com | {} | Daily\n\
             codex-a  | openai-codex | c@d.com | {} | Codex\n",
            claude_dir.display(),
            codex_dir.display(),
        ),
    )
    .unwrap();

    // obsidian.json — points at an existing dir
    let vault = cfg_path.join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(
        cfg_path.join("evy").join("obsidian.json"),
        format!(r#"{{"vault_root":"{}"}}"#, vault.display()),
    )
    .unwrap();

    // evy-notify.json — carries a bot_token to be redacted
    std::fs::write(
        cfg_path.join("evy-notify.json"),
        "{\n  \"bot_token\": \"99:SECRETTOKEN\",\n  \"chat_id\": \"7\"\n}",
    )
    .unwrap();

    // ── env wiring (mirrors evy-secrets test precedent) ──────────────────
    unsafe {
        std::env::set_var("SUBCTL_CONFIG_DIR", cfg_path);
        std::env::set_var("SUBCTL_ACCOUNTS_CONF", &accounts_conf);
        std::env::set_var("SUBCTL_VERSION", "3.3.12");
        std::env::set_var("SUBCTL_UPDATE_STUB_LATEST", "v9.9.9");
    }

    let (base, shutdown) = spawn().await;

    // ── /api/settings/keys ───────────────────────────────────────────────
    let (st, body) = get_json(&base, "/api/settings/keys").await;
    assert_eq!(st, 200);
    assert_eq!(body["ok"], true);
    let keys = body["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 12);
    let brave = keys.iter().find(|k| k["name"] == "BRAVE_API_KEY").unwrap();
    assert_eq!(brave["secrets_json"], Value::Bool(true)); // from secrets.json
    assert!(body["note"].is_string());

    // ── /api/settings/secrets ────────────────────────────────────────────
    let (st, body) = get_json(&base, "/api/settings/secrets").await;
    assert_eq!(st, 200);
    assert_eq!(body["priority"], serde_json::json!(["env_var", "secrets_json", "absent"]));
    let rows = body["secrets"].as_array().unwrap();
    let find = |k: &str| rows.iter().find(|r| r["key"] == k).unwrap().clone();
    assert_eq!(find("brave_api_key")["isSet"], true);
    assert_eq!(find("lmstudio_api_token")["isSet"], false); // empty string
    assert!(find("brave_api_key")["lastModified"].is_string());

    // ── /api/settings/oauth ──────────────────────────────────────────────
    let (st, body) = get_json(&base, "/api/settings/oauth").await;
    assert_eq!(st, 200);
    let accts = body["accounts"].as_array().unwrap();
    assert_eq!(accts.len(), 2);
    assert_eq!(accts[0]["alias"], "claude-a");
    assert_eq!(accts[0]["auth_status"], "ready"); // .credentials.json present
    assert_eq!(accts[1]["auth_status"], "not_authenticated"); // empty codex dir

    // ── /api/settings/obsidian ───────────────────────────────────────────
    let (st, body) = get_json(&base, "/api/settings/obsidian").await;
    assert_eq!(st, 200);
    assert_eq!(body["configured"], true);
    assert_eq!(body["exists"], true);
    assert_eq!(body["vault_root"], vault.to_string_lossy().as_ref());

    // ── /api/settings/config/{name} ──────────────────────────────────────
    let (st, body) = get_json(&base, "/api/settings/config/notify").await;
    assert_eq!(st, 200);
    let content = body["content"].as_str().unwrap();
    assert!(!content.contains("SECRETTOKEN"));
    assert!(content.contains("\"bot_token\": \"<redacted>\""));
    assert!(content.contains("\"chat_id\": \"7\""));

    let (st, body) = get_json(&base, "/api/settings/config/bogus").await;
    assert_eq!(st, 400);
    assert_eq!(body["ok"], false);

    let (st, body) = get_json(&base, "/api/settings/config/policy").await;
    assert_eq!(st, 404); // not written in fixture
    assert_eq!(body["ok"], false);

    // ── /api/update/check (stubbed latest, no git) ───────────────────────
    let (st, body) = get_json(&base, "/api/update/check").await;
    assert_eq!(st, 200);
    assert_eq!(body["running_version"], "3.3.12");
    assert_eq!(body["latest_tag"], "v9.9.9");
    assert_eq!(body["has_update"], true);
    assert_eq!(body["channel"], "stable");
    assert_eq!(body["stub"], true);

    shutdown.cancel();
}
