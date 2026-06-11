//! End-to-end tests for the W5 native Memory tab families.
//!
//! Boots a real axum server on an ephemeral port backed by `StubAppState`
//! (these endpoints read `$SUBCTL_CONFIG_DIR` files / local sidecars and
//! need no daemon state), points `SUBCTL_CONFIG_DIR` at a temp fixture
//! tree, stands up wiremock Cognee/Memori sidecars, and asserts:
//!
//! - `GET/POST /api/memory/tier1` and `GET /api/memory` return 200 + the v3
//!   wire shape natively (v3 upstream dark throughout — `EVY_PROXY_UPSTREAM`
//!   points at a closed port);
//! - `/api/cognee/*` and `/api/memori/*` forward verbatim to the sidecars
//!   and degrade to v3's 502 `…sidecar unreachable…` envelope when dark;
//! - the **tripwire family** (`/api/memory/{stats,…}`, backed by the v3
//!   master's own evy.db) and `/api/master/memory/*` are NOT served
//!   natively — they fall through to the `/api/{*rest}` reverse-proxy
//!   catch-all (proved by the closed upstream's 502).
//!
//! All assertions live in ONE test: `tests/*.rs` files share a process, so
//! per-test `set_var` of `SUBCTL_CONFIG_DIR` would race. One test = one env
//! setup = deterministic (same shape as `preferences_http_integration.rs`).

use std::sync::Arc;
use std::time::Duration;

use evy_comms::{EventBroadcaster, HttpConfig, HttpServer, StubAppState};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

async fn post_raw(base: &str, path: &str, body: &str) -> (reqwest::StatusCode, Value) {
    let res = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .expect("send");
    let status = res.status();
    let body: Value = res.json().await.expect("json");
    (status, body)
}

#[tokio::test]
async fn memory_families_native_tripwire_family_proxied() {
    // ── fixture config tree ──────────────────────────────────────────────
    let cfg = tempfile::tempdir().expect("tempdir");
    let cfg_path = cfg.path();
    let evy_dir = cfg_path.join("evy");
    std::fs::create_dir_all(&evy_dir).unwrap();

    // Tier 1 — memory.md present (v3's `§` entry delimiter), user.md absent.
    std::fs::write(evy_dir.join("memory.md"), "Fact one\n§\nFact two").unwrap();

    // Obsidian — configured root containing one vault (.obsidian + 2 notes).
    let root = cfg_path.join("vault-root");
    let vault = root.join("MyVault");
    std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
    std::fs::create_dir_all(vault.join("sub")).unwrap();
    std::fs::write(vault.join("a.md"), "# a").unwrap();
    std::fs::write(vault.join("sub/b.md"), "# b").unwrap();
    std::fs::write(vault.join("notes.txt"), "not markdown").unwrap();
    std::fs::write(
        evy_dir.join("obsidian.json"),
        format!(r#"{{"vault_root":"{}"}}"#, root.display()),
    )
    .unwrap();

    // ── stub sidecars (wiremock at the reqwest boundary) ─────────────────
    let cognee = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .and(query_param("verbose", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"status":"ok","total_memories":5,"graph_nodes":12}"#)
                .insert_header("Content-Type", "application/json"),
        )
        .mount(&cognee)
        .await;
    Mock::given(method("POST"))
        .and(path("/cognify"))
        .and(body_json(json!({ "datasets": ["main_dataset"] })))
        .respond_with(ResponseTemplate::new(202).set_body_string(r#"{"ok":true,"queued":true}"#))
        .mount(&cognee)
        .await;
    let memori = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"status":"ok","total_memories":7,"total_unreviewed":2}"#),
        )
        .mount(&memori)
        .await;

    // ── env wiring (one test fn ⇒ one deterministic setup) ───────────────
    // `EVY_PROXY_UPSTREAM` → closed port: catch-all hits yield 502, which is
    // what PROVES a path is proxied rather than native.
    unsafe {
        std::env::set_var("SUBCTL_CONFIG_DIR", cfg_path);
        std::env::set_var("EVY_PROXY_UPSTREAM", "127.0.0.1:1");
        std::env::set_var("SUBCTL_COGNEE_PORT", cognee.address().port().to_string());
        std::env::set_var("SUBCTL_MEMORI_PORT", memori.address().port().to_string());
    }

    let (base, shutdown) = spawn().await;

    // ── GET /api/memory/tier1 (native) ───────────────────────────────────
    let (st, body) = get_json(&base, "/api/memory/tier1").await;
    assert_eq!(st, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(body["memory"]["exists"], true);
    assert_eq!(body["memory"]["content"], "Fact one\n§\nFact two");
    assert_eq!(body["memory"]["char_count"], 19);
    assert_eq!(body["memory"]["char_limit"], 2200);
    assert_eq!(
        body["memory"]["path"],
        evy_dir.join("memory.md").to_string_lossy().as_ref()
    );
    assert_eq!(body["user_profile"]["exists"], false);
    assert_eq!(body["user_profile"]["content"], "");
    assert_eq!(body["user_profile"]["char_limit"], 1375);

    // ── POST /api/memory/tier1 (native write + re-read) ──────────────────
    let (st, body) = post_raw(
        &base,
        "/api/memory/tier1",
        r#"{"which":"user","content":"  Jason — MSP owner.  "}"#,
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(body["char_count"], 18); // trimmed
    assert_eq!(body["char_limit"], 1375);
    assert_eq!(
        body["message"],
        "next agent prompt will pick up the new content"
    );
    assert_eq!(
        std::fs::read_to_string(evy_dir.join("user.md")).unwrap(),
        "Jason — MSP owner."
    );
    let (_, body) = get_json(&base, "/api/memory/tier1").await;
    assert_eq!(body["user_profile"]["exists"], true);
    assert_eq!(body["user_profile"]["content"], "Jason — MSP owner.");

    // POST validation — v3's exact 400 envelopes (server.ts:4771-4784).
    let (st, body) = post_raw(&base, "/api/memory/tier1", "not json").await;
    assert_eq!(st, 400);
    assert_eq!(body["error"], "invalid JSON");
    let (st, body) = post_raw(
        &base,
        "/api/memory/tier1",
        r#"{"which":"nope","content":"x"}"#,
    )
    .await;
    assert_eq!(st, 400);
    assert_eq!(body["error"], "which must be 'memory' or 'user'");
    let oversized = json!({ "which": "user", "content": "x".repeat(1376) }).to_string();
    let (st, body) = post_raw(&base, "/api/memory/tier1", &oversized).await;
    assert_eq!(st, 400);
    assert_eq!(body["error"], "content exceeds char limit (1376 > 1375)");

    // ── GET /api/memory (native Obsidian/vault state) ────────────────────
    let (st, body) = get_json(&base, "/api/memory").await;
    assert_eq!(st, 200);
    assert_eq!(body["ok"], true);
    assert!(body["obsidian_installed"].is_boolean());
    assert_eq!(body["suggested_install"], "brew install --cask obsidian");
    assert!(body["suggested_vault_path"].is_string());
    let vaults = body["vaults"].as_array().unwrap();
    assert_eq!(vaults.len(), 1);
    assert_eq!(vaults[0]["path"], vault.to_string_lossy().as_ref());
    assert_eq!(vaults[0]["note_count"], 2); // a.md + sub/b.md, not notes.txt

    // JS `toISOString()` shape: 2026-06-10T12:34:56.789Z
    let lm = vaults[0]["last_modified"].as_str().unwrap();
    assert!(
        lm.ends_with('Z') && lm.contains('T') && lm.contains('.'),
        "{lm}"
    );

    // ── /api/cognee/* (native thin forward, body verbatim) ───────────────
    let res = reqwest::Client::new()
        .get(format!("{base}/api/cognee/health?verbose=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(
        res.text().await.unwrap(),
        r#"{"status":"ok","total_memories":5,"graph_nodes":12}"#,
        "sidecar body must pass through byte-identical"
    );
    let (st, body) = post_raw(
        &base,
        "/api/cognee/cognify",
        r#"{"datasets":["main_dataset"]}"#,
    )
    .await;
    assert_eq!(st, 202); // upstream status echoed
    assert_eq!(body["queued"], true);

    // ── /api/memori/* (native thin forward) ──────────────────────────────
    let (st, body) = get_json(&base, "/api/memori/health").await;
    assert_eq!(st, 200);
    assert_eq!(body["total_memories"], 7);

    // Sidecar dark → v3's 502 envelope (port read per request, so we can
    // repoint within the single test).
    unsafe {
        std::env::set_var("SUBCTL_COGNEE_PORT", "1");
    }
    let (st, body) = get_json(&base, "/api/cognee/health").await;
    assert_eq!(st, 502);
    assert_eq!(body["ok"], false);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("cognee sidecar unreachable:"),
        "{body}"
    );

    // ── tripwire family + master workflow stay proxied ───────────────────
    // Backed by the v3 master's own evy.db (components/evy/memory.ts:73),
    // NOT claude-mem — must fall through to the catch-all (502 here because
    // the upstream is a closed port; a native handler would return 200).
    for proxied in [
        "/api/memory/stats",
        "/api/memory/recent",
        "/api/memory/search?query=x",
        "/api/master/memory/tier1/pending",
        "/api/master/memory/kernel/status",
    ] {
        let (st, body) = get_json(&base, proxied).await;
        assert_eq!(st, 502, "{proxied} must ride the reverse-proxy catch-all");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("v3 dashboard (Bun) unreachable"),
            "{proxied}: {body}"
        );
    }

    shutdown.cancel();
}
