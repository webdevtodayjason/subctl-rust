//! End-to-end tests for the native settings / auth-status read surface (W2).
//!
//! Boots a real axum server on an ephemeral port backed by `StubAppState`
//! (these endpoints read `~/.config/subctl` files directly and need no daemon
//! state), points `SUBCTL_CONFIG_DIR` at a temp fixture tree, and asserts:
//!
//! - the **v4-owned** reads (`/api/settings/{oauth,obsidian,config/{name}}`)
//!   return 200 + the v3 wire shape natively;
//! - the **v3-owned, env/install-coupled** reads (`/api/settings/keys`,
//!   `/api/settings/secrets`, `/api/update/check`) are NOT served natively —
//!   they fall through to the `/api/{*rest}` reverse-proxy catch-all.
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
async fn settings_read_surface_native_live_proxies_hollow() {
    // ── fixture config tree ──────────────────────────────────────────────
    let cfg = tempfile::tempdir().expect("tempdir");
    let cfg_path = cfg.path();
    std::fs::create_dir_all(cfg_path.join("evy")).unwrap();

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
    // `EVY_PROXY_UPSTREAM` → a closed port so the proxy catch-all yields 502;
    // that 502 is what proves the hollow trio fall through to the proxy (a
    // natively-handled route would return 200 regardless of the upstream).
    unsafe {
        std::env::set_var("SUBCTL_CONFIG_DIR", cfg_path);
        std::env::set_var("SUBCTL_ACCOUNTS_CONF", &accounts_conf);
        std::env::set_var("EVY_PROXY_UPSTREAM", "127.0.0.1:1");
    }

    let (base, shutdown) = spawn().await;

    // ── /api/settings/oauth (native) ─────────────────────────────────────
    let (st, body) = get_json(&base, "/api/settings/oauth").await;
    assert_eq!(st, 200);
    let accts = body["accounts"].as_array().unwrap();
    assert_eq!(accts.len(), 2);
    assert_eq!(accts[0]["alias"], "claude-a");
    assert_eq!(accts[0]["auth_status"], "ready"); // .credentials.json present
    assert_eq!(accts[1]["auth_status"], "not_authenticated"); // empty codex dir

    // ── /api/settings/obsidian (native) ──────────────────────────────────
    let (st, body) = get_json(&base, "/api/settings/obsidian").await;
    assert_eq!(st, 200);
    assert_eq!(body["configured"], true);
    assert_eq!(body["exists"], true);
    assert_eq!(body["vault_root"], vault.to_string_lossy().as_ref());

    // ── /api/settings/config/{name} (native) ─────────────────────────────
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

    // ── hollow trio falls through to the v3 reverse-proxy ─────────────────
    // env/install-coupled reads v4-under-launchd can't serve. With the proxy
    // upstream pointed at a closed port, a proxied route yields 502 while a
    // native route would still 200 — so 502 here proves the fall-through.
    for path in [
        "/api/settings/keys",
        "/api/settings/secrets",
        "/api/update/check",
    ] {
        let res = reqwest::get(format!("{base}{path}")).await.unwrap();
        assert_eq!(
            res.status(),
            502,
            "{path} must fall through to the (now-dead) proxy, not be served natively"
        );
    }

    unsafe {
        std::env::remove_var("EVY_PROXY_UPSTREAM");
    }
    shutdown.cancel();
}
