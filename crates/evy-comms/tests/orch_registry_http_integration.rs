//! End-to-end tests for the browser-bare orchestration-registry reads
//! (Wave 4 f2): `GET /api/orchestration` + `GET /api/orchestration/captures`.
//!
//! Boots a real axum server on an ephemeral port backed by a fixture
//! `AppState` carrying canned v4 registry rows, stubs the v3 upstream with
//! wiremock, and asserts the strangler merge semantics:
//!
//! - **both alive**: v4 rows (origin `"v4"`, exact v3 wire keys) merged
//!   with the v3 fixture rows (origin `"v3-legacy"`, verbatim), deduped by
//!   `name` with the v4 row winning;
//! - **upstream dead**: v4 rows alone, HTTP 200 — never a 5xx because
//!   legacy is dark;
//! - **shape parity**: the v3 envelope (`{orchestrations}` /
//!   `{ok,count,captures}`) and the per-row key set match the captured v3
//!   fixture (`dashboard/server.ts` `buildOrchestrations()` + `/captures`).
//!
//! All assertions live in ONE test: `tests/*.rs` files share a process, so
//! per-test `set_var` of `EVY_PROXY_UPSTREAM` would race. One test = one
//! env sequence = deterministic (house pattern, see
//! `preferences_http_integration.rs`). The upstream env var is read per
//! request, so the dead → alive repoint inside the single test is safe.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use evy_comms::{
    AppState, EventBroadcaster, HttpConfig, HttpServer, JobSummary, OrchestrationCapture,
    OrchestrationRow, WorkerSummary,
};
use evy_core::{ProviderKind, WorkerId};
use evy_policy::Policy;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Fixture `AppState`: canned v4 registry rows + captures, empty elsewhere.
struct FixtureApp {
    rows: Vec<OrchestrationRow>,
    captures: Vec<OrchestrationCapture>,
}

#[async_trait]
impl AppState for FixtureApp {
    async fn workers(&self) -> Vec<WorkerSummary> {
        Vec::new()
    }

    async fn jobs(&self) -> Vec<JobSummary> {
        Vec::new()
    }

    async fn policy(&self) -> Policy {
        Policy::default()
    }

    async fn orchestrations(&self) -> Vec<OrchestrationRow> {
        self.rows.clone()
    }

    async fn orchestration_captures(&self, _lines: usize) -> Vec<OrchestrationCapture> {
        self.captures.clone()
    }
}

async fn spawn(app: FixtureApp) -> (String, CancellationToken) {
    let broadcaster = EventBroadcaster::new(64);
    let server = HttpServer::new(HttpConfig::ephemeral(), broadcaster, Arc::new(app));
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

/// Per-row key set of v3's `buildOrchestrations()` (captured fixture:
/// `GET :8787/api/orchestration` → `{"orchestrations":[…]}`).
const V3_LIST_KEYS: [&str; 9] = [
    "name",
    "path",
    "attached",
    "windows",
    "claude_account_dir",
    "is_orchestrator",
    "last_activity_seconds_ago",
    "last_event_type",
    "last_event_text",
];

/// Per-row key set of v3's `/api/orchestration/captures` (captured fixture:
/// `{"ok":true,"count":N,"captures":[…]}`).
const V3_CAPTURE_KEYS: [&str; 8] = [
    "name",
    "status",
    "last_activity_seconds_ago",
    "path",
    "claude_account_dir",
    "windows",
    "attached",
    "capture",
];

fn assert_keys(row: &Value, keys: &[&str], ctx: &str) {
    let obj = row
        .as_object()
        .unwrap_or_else(|| panic!("{ctx}: not an object"));
    for key in keys {
        assert!(obj.contains_key(*key), "{ctx}: missing v3 key {key}");
    }
}

#[tokio::test]
async fn orch_registry_merges_v3_upstream_and_degrades_to_v4_only() {
    // ── v4 fixture registry ──────────────────────────────────────────────
    let rows = vec![
        OrchestrationRow {
            worker_id: WorkerId::new(),
            provider: ProviderKind::ClaudeCode,
            status: "Running".to_string(),
            tmux_session: Some("v4-team".to_string()),
            alive: true,
            age_seconds: 30,
            last_event: Some("spawned".to_string()),
        },
        // Same tmux session name as a v3 row — the v4 row must win the merge.
        OrchestrationRow {
            worker_id: WorkerId::new(),
            provider: ProviderKind::ClaudeCode,
            status: "Running".to_string(),
            tmux_session: Some("shared-team".to_string()),
            alive: true,
            age_seconds: 60,
            last_event: None,
        },
        // Dead worker — filtered out (v3 lists only live tmux sessions).
        OrchestrationRow {
            worker_id: WorkerId::new(),
            provider: ProviderKind::Codex,
            status: "Succeeded".to_string(),
            tmux_session: Some("finished-team".to_string()),
            alive: false,
            age_seconds: 600,
            last_event: None,
        },
    ];
    let captures = vec![OrchestrationCapture {
        worker_id: rows[0].worker_id,
        session: Some("v4-team".to_string()),
        text: "worker pane output".to_string(),
    }];
    let (base, _shutdown) = spawn(FixtureApp { rows, captures }).await;

    // ── upstream DEAD (closed port): 200, v4 rows alone, never 5xx ───────
    unsafe {
        std::env::set_var("EVY_PROXY_UPSTREAM", "127.0.0.1:1");
    }

    let (st, body) = get_json(&base, "/api/orchestration").await;
    assert_eq!(st, 200, "list must not 5xx when legacy is dark");
    let list = body["orchestrations"].as_array().unwrap();
    assert_eq!(list.len(), 2, "live v4 rows only (dead worker filtered)");
    for row in list {
        assert_eq!(row["origin"], "v4");
        assert_keys(row, &V3_LIST_KEYS, "degraded list row");
    }
    assert_eq!(list[0]["name"], "v4-team");
    assert_eq!(list[0]["is_orchestrator"], true);
    assert_eq!(list[0]["last_event_text"], "spawned");
    assert_eq!(list[1]["name"], "shared-team");

    let (st, body) = get_json(&base, "/api/orchestration/captures?lines=60").await;
    assert_eq!(st, 200, "captures must not 5xx when legacy is dark");
    assert_eq!(body["ok"], true);
    assert_eq!(body["count"], 1);
    let caps = body["captures"].as_array().unwrap();
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0]["origin"], "v4");
    assert_eq!(caps[0]["name"], "v4-team");
    assert_eq!(caps[0]["status"], "idle");
    assert_eq!(caps[0]["capture"], "worker pane output");
    assert_keys(&caps[0], &V3_CAPTURE_KEYS, "degraded capture row");

    // ── upstream ALIVE (wiremock serving the captured v3 shape) ──────────
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/orchestration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "orchestrations": [
                {
                    "name": "legacy-team",
                    "path": "/Users/op/code/legacy",
                    "attached": false,
                    "windows": 2,
                    "claude_account_dir": "/Users/op/.claude",
                    "is_orchestrator": true,
                    "last_activity_seconds_ago": 12,
                    "last_event_type": "note",
                    "last_event_text": "still here",
                },
                {
                    // Collides with the v4 "shared-team" row — must be dropped.
                    "name": "shared-team",
                    "path": "/Users/op/code/shared",
                    "attached": true,
                    "windows": 1,
                    "claude_account_dir": "/Users/op/.claude",
                    "is_orchestrator": true,
                    "last_activity_seconds_ago": 5,
                    "last_event_type": null,
                    "last_event_text": null,
                },
            ]
        })))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/orchestration/captures"))
        // The native handler must forward the (clamped) lines param.
        .and(query_param("lines", "60"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "count": 1,
            "captures": [{
                "name": "legacy-team",
                "status": "active",
                "last_activity_seconds_ago": 12,
                "path": "/Users/op/code/legacy",
                "claude_account_dir": "/Users/op/.claude",
                "windows": 2,
                "attached": false,
                "capture": "legacy pane",
            }]
        })))
        .mount(&upstream)
        .await;
    let host = upstream
        .uri()
        .strip_prefix("http://")
        .expect("wiremock uri scheme")
        .to_string();
    unsafe {
        std::env::set_var("EVY_PROXY_UPSTREAM", &host);
    }

    let (st, body) = get_json(&base, "/api/orchestration").await;
    assert_eq!(st, 200);
    let list = body["orchestrations"].as_array().unwrap();
    // 2 live v4 rows + 1 legacy row; the colliding "shared-team" deduped.
    assert_eq!(
        list.len(),
        3,
        "merged: v4 first, non-colliding legacy after"
    );
    assert_eq!(list[0]["name"], "v4-team");
    assert_eq!(list[0]["origin"], "v4");
    assert_eq!(list[1]["name"], "shared-team");
    assert_eq!(list[1]["origin"], "v4", "v4 wins the name collision");
    assert_eq!(list[2]["name"], "legacy-team");
    assert_eq!(list[2]["origin"], "v3-legacy");
    // Legacy row is otherwise verbatim (shape parity with the v3 fixture).
    assert_eq!(list[2]["path"], "/Users/op/code/legacy");
    assert_eq!(list[2]["windows"], 2);
    assert_eq!(list[2]["last_activity_seconds_ago"], 12);
    for row in list {
        assert_keys(row, &V3_LIST_KEYS, "merged list row");
    }

    let (st, body) = get_json(&base, "/api/orchestration/captures?lines=60").await;
    assert_eq!(st, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(body["count"], 2);
    let caps = body["captures"].as_array().unwrap();
    assert_eq!(caps.len(), 2);
    assert_eq!(caps[0]["name"], "v4-team");
    assert_eq!(caps[0]["origin"], "v4");
    assert_eq!(caps[1]["name"], "legacy-team");
    assert_eq!(caps[1]["origin"], "v3-legacy");
    assert_eq!(caps[1]["status"], "active", "legacy status verbatim");
    assert_eq!(caps[1]["capture"], "legacy pane");
    for row in caps {
        assert_keys(row, &V3_CAPTURE_KEYS, "merged capture row");
    }

    // ── read-only claim: the v3 action dialect still rides the proxy ─────
    // POST /api/orchestration/spawn must NOT be answered natively — with the
    // upstream stub mounted only for the two GETs, wiremock 404s it, proving
    // the request went upstream rather than to a native handler (a native
    // 405 would come from the axum router instead).
    let res = reqwest::Client::new()
        .post(format!("{base}/api/orchestration/spawn"))
        .json(&json!({}))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 404, "spawn fell through to the v3 proxy");
}
