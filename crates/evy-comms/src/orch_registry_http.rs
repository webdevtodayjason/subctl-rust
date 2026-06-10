//! v4-native orchestration-registry reads on the **browser-bare** paths
//! (Wave 4, teams/orchestration — strangler step for the Orch tab's data).
//!
//! | Method | Path | Consumer | Response envelope |
//! |--------|------|----------|-------------------|
//! | GET | `/api/orchestration` | notify-listener `/sessions`, external integrations | `{ orchestrations: [row, …] }` |
//! | GET | `/api/orchestration/captures?lines=N` | orch tab camera grid (2 s poll) | `{ ok: true, count, captures: [row, …] }` |
//!
//! # Merged view — v4 reality first, v3 as optional-degraded upstream
//!
//! v4's worker registry ([`AppState::orchestrations`] /
//! [`AppState::orchestration_captures`]) is the **source of truth**; the v3
//! Bun dashboard's live registry (`EVY_PROXY_UPSTREAM`, default
//! `127.0.0.1:8787`) still matters during the transition because legacy
//! spawns only exist there. Each response is therefore:
//!
//! 1. v4 registry rows, field-mapped into the exact v3 wire shape and
//!    tagged `origin: "v4"`;
//! 2. plus the v3 upstream's rows verbatim, tagged `origin: "v3-legacy"`,
//!    **deduplicated by `name`** (a v4-spawned tmux session also shows up
//!    in v3's tmux enumeration — the v4 row wins).
//!
//! Both consumers tolerate the extra `origin` field: `orch.js` reads only
//! `c.{name,status,capture,last_activity_seconds_ago}` off each capture
//! (plain property access, no key iteration), and `notify-listener.ts`
//! reads `s.{name,path,attached,claude_account_dir}` — v3 itself already
//! sends fields both ignore.
//!
//! If the v3 upstream is down/unreachable the handlers serve v4's rows
//! alone — **200, never 5xx** — so the panel never errors because legacy
//! is dark (same "v3 master optional during strangler" pattern as the BFF;
//! see `tests/preferences_http_integration.rs` for the closed-upstream
//! precedent).
//!
//! # Read-only claim
//!
//! Only the two read paths above are claimed natively. The per-session
//! detail (`GET /api/orchestration/{name}`) and the v3 action dialect
//! (`POST …/spawn`, `…/{name}/msg`, `…/{name}/kill`) stay on the
//! `/api/{*rest}` reverse-proxy catch-all this wave — v3 actions must keep
//! operating on v3's registry.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Json, Response};
use evy_core::WorkerId;
use serde_json::{json, Value};

use crate::http::{HttpState, OrchestrationCapture, OrchestrationRow};

/// Bound on the v3 upstream fetch. Short — the orch tab polls every 2 s,
/// and a dark upstream must degrade (v4-only rows), not stall the panel.
/// Connection-refused fails instantly; this guards the accepted-but-hung
/// case. v3's own master-teams fetch uses a similar 1.5 s budget.
const V3_FETCH_TIMEOUT: Duration = Duration::from_millis(2500);

// ── handlers ──────────────────────────────────────────────────────────────────

/// `GET /api/orchestration` → `{ orchestrations: [...] }` (v3 envelope).
///
/// v4 rows are filtered to live tmux sessions (`alive`) to match v3's
/// semantics — its list is an enumeration of *currently-live* orchestrator
/// tmux sessions, never finished workers.
pub(crate) async fn list_handler(State(state): State<HttpState>) -> Response {
    let v4_rows: Vec<Value> = state
        .app
        .orchestrations()
        .await
        .iter()
        .filter(|r| r.alive)
        .map(v4_list_row)
        .collect();
    let v3_rows = fetch_v3_array("/api/orchestration", "orchestrations").await;
    let merged = merge_by_name(v4_rows, v3_rows);
    Json(json!({ "orchestrations": merged })).into_response()
}

/// `GET /api/orchestration/captures?lines=N` →
/// `{ ok: true, count, captures: [...] }` (v3 envelope).
///
/// `lines` mirrors v3's `tmuxCaptureFrame` clamp: default 40, clamped
/// 8..=200, tolerant of an unparseable value (v3: `lines || 40`). The
/// clamped value is forwarded to the v3 upstream so both halves of the
/// merged grid carry the same pane height.
pub(crate) async fn captures_handler(
    State(state): State<HttpState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let lines = q
        .get("lines")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(40)
        .clamp(8, 200);
    let v4_rows: Vec<Value> = state
        .app
        .orchestration_captures(lines)
        .await
        .iter()
        .map(v4_capture_row)
        .collect();
    let v3_rows = fetch_v3_array(
        &format!("/api/orchestration/captures?lines={lines}"),
        "captures",
    )
    .await;
    let merged = merge_by_name(v4_rows, v3_rows);
    Json(json!({ "ok": true, "count": merged.len(), "captures": merged })).into_response()
}

// ── v3 upstream (optional-degraded) ───────────────────────────────────────────

/// Fetch `path_q` from the v3 upstream and return the array under `key`.
/// Empty on ANY failure (connect, timeout, non-2xx, bad JSON, wrong shape)
/// — the degraded mode is "v4 rows alone", never an error response.
async fn fetch_v3_array(path_q: &str, key: &str) -> Vec<Value> {
    let url = format!("http://{}{}", crate::proxy_http::upstream_base(), path_q);
    let resp = match crate::proxy_http::client()
        .get(&url)
        .timeout(V3_FETCH_TIMEOUT)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::debug!(url = %url, status = %r.status(), "v3 orchestration upstream non-2xx; degrading to v4-only");
            return Vec::new();
        }
        Err(e) => {
            tracing::debug!(url = %url, error = %e, "v3 orchestration upstream unreachable; degrading to v4-only");
            return Vec::new();
        }
    };
    match resp.json::<Value>().await {
        Ok(v) => v
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        Err(e) => {
            tracing::debug!(url = %url, error = %e, "v3 orchestration upstream returned non-JSON; degrading to v4-only");
            Vec::new()
        }
    }
}

// ── merge (pure) ──────────────────────────────────────────────────────────────

/// v4 rows first (already `origin: "v4"`-tagged by the row builders), then
/// every v3 row whose `name` doesn't collide with a v4 row, tagged
/// `origin: "v3-legacy"`. The v4 row wins a collision — the dashboard
/// shows v4's reality first, and a v4-spawned tmux session re-appears in
/// v3's tmux enumeration under the same name.
fn merge_by_name(v4_rows: Vec<Value>, v3_rows: Vec<Value>) -> Vec<Value> {
    let seen: HashSet<String> = v4_rows
        .iter()
        .filter_map(|r| r.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let mut out = v4_rows;
    for mut row in v3_rows {
        if let Some(name) = row.get("name").and_then(Value::as_str) {
            if seen.contains(name) {
                continue;
            }
        }
        if let Some(obj) = row.as_object_mut() {
            obj.insert("origin".to_string(), json!("v3-legacy"));
        }
        out.push(row);
    }
    out
}

// ── v4 → v3 field mapping (pure) ──────────────────────────────────────────────

/// The row's public name: the tmux session when recorded, else the worker
/// uuid. Names must be tmux-real where possible so the still-proxied v3
/// action dialect (`/msg`, `/kill`) resolves them against tmux.
fn worker_name(session: Option<&String>, worker_id: &WorkerId) -> String {
    session.cloned().unwrap_or_else(|| worker_id.0.to_string())
}

/// Field-map one v4 registry row into v3's `buildOrchestrations()` row
/// shape. Fields v4 doesn't track (`path`, `windows`, `claude_account_dir`,
/// activity age) render as `null`/`false` — both consumers null-guard them.
fn v4_list_row(r: &OrchestrationRow) -> Value {
    json!({
        "name": worker_name(r.tmux_session.as_ref(), &r.worker_id),
        "path": Value::Null,
        "attached": false,
        "windows": Value::Null,
        "claude_account_dir": Value::Null,
        "is_orchestrator": true,
        "last_activity_seconds_ago": Value::Null,
        "last_event_type": Value::Null,
        "last_event_text": r.last_event.clone(),
        "origin": "v4",
    })
}

/// Field-map one v4 pane capture into v3's `/captures` row shape.
fn v4_capture_row(c: &OrchestrationCapture) -> Value {
    json!({
        "name": worker_name(c.session.as_ref(), &c.worker_id),
        "status": capture_status(&c.text),
        "last_activity_seconds_ago": Value::Null,
        "path": Value::Null,
        "claude_account_dir": Value::Null,
        "windows": Value::Null,
        "attached": false,
        "capture": c.text.clone(),
        "origin": "v4",
    })
}

/// v3's per-tile status heuristic, restricted to what v4 rows can know:
/// `"error"` when the last 10 lines show an error marker, else `"idle"` —
/// exactly v3's behavior for a row with a `null` activity age (its
/// active/idle/stale split needs `last_activity_seconds_ago`, which v4's
/// registry doesn't track).
fn capture_status(capture: &str) -> &'static str {
    let tail: Vec<&str> = capture.split('\n').collect();
    let start = tail.len().saturating_sub(10);
    let last_few = tail[start..].join("\n").to_ascii_lowercase();
    if has_error_marker(&last_few) {
        "error"
    } else {
        "idle"
    }
}

/// Port of v3's `/error\b|Error:|failed\b|fatal:/i` test (input is already
/// lowercased; `Error:` is subsumed by `error\b` under case folding).
fn has_error_marker(lower: &str) -> bool {
    word_hit(lower, "error") || word_hit(lower, "failed") || lower.contains("fatal:")
}

/// Does `needle` occur in `hay` followed by a JS word boundary (non
/// `[A-Za-z0-9_]` or end-of-string)?
fn word_hit(hay: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(i) = hay[start..].find(needle) {
        let end = start + i + needle.len();
        let at_boundary = hay[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
        if at_boundary {
            return true;
        }
        start = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use evy_core::ProviderKind;

    fn row(session: Option<&str>, alive: bool, last_event: Option<&str>) -> OrchestrationRow {
        OrchestrationRow {
            worker_id: WorkerId::new(),
            provider: ProviderKind::ClaudeCode,
            status: "Running".to_string(),
            tmux_session: session.map(str::to_owned),
            alive,
            age_seconds: 42,
            last_event: last_event.map(str::to_owned),
        }
    }

    #[test]
    fn worker_name_prefers_tmux_session() {
        let r = row(Some("dev-team-1"), true, None);
        assert_eq!(
            worker_name(r.tmux_session.as_ref(), &r.worker_id),
            "dev-team-1"
        );
        let bare = row(None, true, None);
        assert_eq!(
            worker_name(bare.tmux_session.as_ref(), &bare.worker_id),
            bare.worker_id.0.to_string()
        );
    }

    #[test]
    fn v4_list_row_carries_exact_v3_keys_plus_origin() {
        let v = v4_list_row(&row(Some("t"), true, Some("spawned")));
        let obj = v.as_object().unwrap();
        for key in [
            "name",
            "path",
            "attached",
            "windows",
            "claude_account_dir",
            "is_orchestrator",
            "last_activity_seconds_ago",
            "last_event_type",
            "last_event_text",
        ] {
            assert!(obj.contains_key(key), "missing v3 key {key}");
        }
        assert_eq!(v["origin"], "v4");
        assert_eq!(v["is_orchestrator"], true);
        assert_eq!(v["last_event_text"], "spawned");
    }

    #[test]
    fn v4_capture_row_carries_exact_v3_keys_plus_origin() {
        let c = OrchestrationCapture {
            worker_id: WorkerId::new(),
            session: Some("t".to_string()),
            text: "all good".to_string(),
        };
        let v = v4_capture_row(&c);
        let obj = v.as_object().unwrap();
        for key in [
            "name",
            "status",
            "last_activity_seconds_ago",
            "path",
            "claude_account_dir",
            "windows",
            "attached",
            "capture",
        ] {
            assert!(obj.contains_key(key), "missing v3 key {key}");
        }
        assert_eq!(v["origin"], "v4");
        assert_eq!(v["status"], "idle");
        assert_eq!(v["capture"], "all good");
    }

    #[test]
    fn capture_status_flags_error_markers_in_tail_only() {
        assert_eq!(capture_status("Error: boom"), "error");
        assert_eq!(capture_status("build FAILED."), "error");
        assert_eq!(capture_status("fatal: not a git repository"), "error");
        assert_eq!(capture_status("all tests passed"), "idle");
        // Word boundary: "errors" / "failedX" don't match `\b`.
        assert_eq!(capture_status("0 errors reported"), "idle");
        // Marker scrolled out of the last 10 lines → idle.
        let scrolled = format!("Error: old{}", "\nok".repeat(12));
        assert_eq!(capture_status(&scrolled), "idle");
        // Marker inside the last 10 lines of a longer pane → error.
        let recent = format!("{}\nfatal: dead", "ok\n".repeat(12));
        assert_eq!(capture_status(&recent), "error");
    }

    #[test]
    fn merge_dedupes_by_name_v4_wins_and_tags_legacy() {
        let v4 = vec![v4_list_row(&row(Some("shared"), true, None))];
        let v3 = vec![
            json!({ "name": "shared", "path": "/tmp/x" }),
            json!({ "name": "legacy-only", "path": "/tmp/y" }),
        ];
        let merged = merge_by_name(v4, v3);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0]["name"], "shared");
        assert_eq!(merged[0]["origin"], "v4");
        assert_eq!(merged[1]["name"], "legacy-only");
        assert_eq!(merged[1]["origin"], "v3-legacy");
        assert_eq!(merged[1]["path"], "/tmp/y"); // legacy row otherwise verbatim
    }

    #[test]
    fn merge_with_empty_v3_is_v4_only() {
        let v4 = vec![v4_list_row(&row(Some("a"), true, None))];
        let merged = merge_by_name(v4, Vec::new());
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["origin"], "v4");
    }
}
