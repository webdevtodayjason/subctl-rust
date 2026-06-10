//! End-to-end tests for the v4-native session-browser + tmux-kill family:
//! `GET /api/evy/sessions/list`, `GET /api/evy/sessions/preview`,
//! `POST /api/evy/sessions/spawn`, `POST /api/evy/sessions/{id}/kill`.
//!
//! These spin up an ephemeral server with [`StubAppState`] (no
//! thinking-partner needed — none of these handlers touch `AppState`).
//! The error-path assertions are environment-independent: `default`
//! always resolves via `$HOME`, an unknown alias always 404s, and an
//! absent tmux session always 404s (tmux liveness folds errors to
//! "not alive"). The real-tmux kill-success proof is `#[ignore]` — run it
//! manually against a throwaway `w4-test-*` session.

use std::time::Duration;

use evy_comms::{EventBroadcaster, HttpConfig, HttpServer, StubAppState};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

async fn spawn_server() -> (String, CancellationToken) {
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

#[tokio::test]
async fn list_returns_sessions_array_and_total() {
    let (base, shutdown) = spawn_server().await;
    let res = reqwest::Client::new()
        .get(format!("{base}/api/evy/sessions/list?limit=3"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("json");
    assert!(
        body.get("sessions").and_then(Value::as_array).is_some(),
        "sessions must be an array"
    );
    assert!(
        body.get("total").and_then(Value::as_u64).is_some(),
        "total must be an integer"
    );
    // total mirrors the post-filter session count.
    let n = body["sessions"].as_array().unwrap().len();
    assert_eq!(body["total"].as_u64().unwrap() as usize, n);
    shutdown.cancel();
}

#[tokio::test]
async fn list_limit_zero_yields_empty() {
    let (base, shutdown) = spawn_server().await;
    let res = reqwest::Client::new()
        .get(format!("{base}/api/evy/sessions/list?limit=0"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["sessions"].as_array().unwrap().len(), 0);
    assert_eq!(body["total"].as_u64().unwrap(), 0);
    shutdown.cancel();
}

#[tokio::test]
async fn preview_rejects_invalid_sid() {
    let (base, shutdown) = spawn_server().await;
    let res = reqwest::Client::new()
        .get(format!(
            "{base}/api/evy/sessions/preview?account=default&sid=not!valid"
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.expect("json");
    assert_eq!(
        body,
        json!({ "ok": false, "error": "missing/invalid account or sid" })
    );
    shutdown.cancel();
}

#[tokio::test]
async fn preview_unknown_account_404s() {
    let (base, shutdown) = spawn_server().await;
    let res = reqwest::Client::new()
        .get(format!(
            "{base}/api/evy/sessions/preview?account=zz-nope-not-real&sid=00000000-0000-0000-0000-000000000000"
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 404);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body, json!({ "ok": false, "error": "unknown account" }));
    shutdown.cancel();
}

#[tokio::test]
async fn preview_default_account_missing_sid_is_ok_empty() {
    let (base, shutdown) = spawn_server().await;
    let sid = "00000000-0000-0000-0000-000000000000";
    let res = reqwest::Client::new()
        .get(format!(
            "{base}/api/evy/sessions/preview?account=default&sid={sid}"
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["sid"], json!(sid));
    assert_eq!(body["account"], json!("default"));
    assert_eq!(body["preview"], json!(""));
    assert_eq!(body["first_ts"], json!(""));
    shutdown.cancel();
}

#[tokio::test]
async fn spawn_rejects_invalid_sid() {
    let (base, shutdown) = spawn_server().await;
    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/sessions/spawn"))
        .json(&json!({ "account": "default", "sid": "bad!sid" }))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.expect("json");
    assert_eq!(
        body,
        json!({ "ok": false, "error": "missing/invalid account or sid" })
    );
    shutdown.cancel();
}

#[tokio::test]
async fn spawn_unknown_account_404s() {
    let (base, shutdown) = spawn_server().await;
    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/sessions/spawn"))
        .json(&json!({ "account": "zz-nope-not-real", "sid": "00000000-0000-0000-0000-000000000000" }))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 404);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body, json!({ "ok": false, "error": "unknown account" }));
    shutdown.cancel();
}

#[tokio::test]
async fn kill_nonexistent_session_404s() {
    let (base, shutdown) = spawn_server().await;
    let name = format!("w4-test-absent-{}", std::process::id());
    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/sessions/{name}/kill"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 404);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body, json!({ "ok": false, "error": "session not found" }));
    shutdown.cancel();
}

/// The new static children must coexist with `sessions_http`'s `{id}`
/// DELETE route (matchit allows it). With `StubAppState` the chat-session
/// delete has no partner → 503 `unavailable`, proving the route still
/// resolves rather than being shadowed.
#[tokio::test]
async fn chat_sessions_delete_still_routes_alongside() {
    let (base, shutdown) = spawn_server().await;
    let res = reqwest::Client::new()
        .delete(format!("{base}/api/evy/sessions/{}", uuid_like()))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 503);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["kind"], json!("unavailable"));
    shutdown.cancel();
}

fn uuid_like() -> &'static str {
    "11111111-2222-3333-4444-555555555555"
}

/// Live tmux proof — creates a throwaway `w4-test-*` session, kills it via
/// the endpoint, and asserts it's gone. Requires a running tmux server, so
/// it's `#[ignore]` (mirrors the evy-providers tmux smoke test).
///
/// Run manually:
///   cargo test -p evy-comms --test orch_sessions_http_integration -- --ignored kill_live
#[tokio::test]
#[ignore = "requires a running tmux server; run manually for live proof"]
async fn kill_live_tmux_session() {
    fn tmux_bin() -> &'static str {
        for p in [
            "/opt/homebrew/bin/tmux",
            "/usr/local/bin/tmux",
            "/usr/bin/tmux",
        ] {
            if std::path::Path::new(p).exists() {
                return p;
            }
        }
        "tmux"
    }
    let session = format!("w4-test-live-{}", std::process::id());
    // Create a detached throwaway session.
    let created = std::process::Command::new(tmux_bin())
        .args(["new-session", "-d", "-s", &session])
        .status()
        .expect("spawn tmux new-session");
    assert!(created.success(), "tmux new-session must succeed");

    let (base, shutdown) = spawn_server().await;
    let res = reqwest::Client::new()
        .post(format!("{base}/api/evy/sessions/{session}/kill"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200, "kill of a live session must 200");
    let body: Value = res.json().await.expect("json");
    assert_eq!(body, json!({ "ok": true }));

    // Confirm it's gone.
    let alive = std::process::Command::new(tmux_bin())
        .args(["has-session", "-t", &session])
        .status()
        .expect("spawn tmux has-session");
    assert!(!alive.success(), "session must be gone after kill");
    shutdown.cancel();
}
