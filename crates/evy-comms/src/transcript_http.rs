//! P2 — transcript + context meter, in the v3 dashboard's wire shape.
//!
//! The v3 browser chat tab rehydrates on mount from `GET /transcript` and
//! polls `GET /context` + `GET /transcript/util` every 5s to drive the
//! context-budget meter and the 4-state compaction banner. These three
//! handlers reproduce that contract over the v4 daemon so the BFF can migrate
//! the routes off the legacy v3 master and the transcript reflects the *v4*
//! conversation (Goal 6: v4 is the single source of truth).
//!
//! | Method | Path | v3-shape returned |
//! |--------|------|-------------------|
//! | GET | `/api/evy/transcript?session_id&limit` | `{ok,total,returned,messages:[{role,content,timestamp}]}` |
//! | GET | `/api/evy/context?session_id`          | `{ok,transcript_msgs,transcript_chars,estimated_*,supervisor,…}` |
//! | GET | `/api/evy/transcript/util?session_id`  | `{ok,current_tokens,warn_at,compact_at,decision:{action,reason},…}` |
//!
//! Token estimation and the compaction decision are ported from the v3 master
//! (`chars/4 + 2500` overhead; `buildUtilSnapshot`'s ok/warn/compacting states)
//! so the dashboard meter agrees with the daemon. Sessions are still in-memory
//! (Phase 7 / P3 adds persistence) — an unknown/absent session yields a valid
//! empty transcript so the chat tab loads cleanly (graceful degradation).

use std::collections::HashMap;

use axum::{
    extract::{Query, State},
    response::Json,
};
use evy_thinking::{Role, Session, SessionId};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::events::DaemonEvent;
use crate::http::HttpState;

/// Fixed prompt overhead added on top of the transcript estimate. Mirrors the
/// v3 master's `fixed_overhead_tokens`.
const OVERHEAD_TOKENS: u64 = 2500;
/// Context-budget thresholds (absolute mode), matching the v3 master defaults.
const WARN_AT: u64 = 55_000;
const COMPACT_AT: u64 = 70_000;
const TARGET_TOKENS: u64 = 55_000;
/// Default transcript page size when the client omits `?limit`.
const DEFAULT_LIMIT: usize = 200;

/// Resolve the session a transcript request refers to. Resolution order
/// (absorbed from the v3 BFF, which injected its held session id for the
/// chat tab's session-less GETs — `dashboard/lib/v4-bridge.ts` `proxyV4*`):
///
/// 1. an explicit `?session_id` (highest priority — direct lookup);
/// 2. the shared **current dashboard chat session** ([`HttpState`]'s holder),
///    so a bare `/transcript` reflects the conversation the chat tab is in;
/// 3. failing both, the most-recently-active session (pre-absorption
///    fallback — keeps bare requests working before the first UI turn).
async fn resolve_session(state: &HttpState, q: &HashMap<String, String>) -> Option<Session> {
    let partner = state.app.thinking_partner()?;
    if let Some(sid) = q.get("session_id").and_then(|s| Uuid::parse_str(s).ok()) {
        return partner.session(SessionId(sid)).await.ok().flatten();
    }
    if let Some(current) = state.current_chat_session() {
        if let Ok(Some(session)) = partner.session(current).await {
            return Some(session);
        }
    }
    let mut sessions = partner.list_sessions().await.ok()?;
    sessions.sort_by_key(|s| s.last_activity);
    sessions.pop()
}

/// Operator/Partner messages only (System scaffolding is never shown), with
/// their character total. Returned in chronological order.
fn visible_messages(session: &Session) -> (Vec<&evy_thinking::Message>, u64) {
    let msgs: Vec<&evy_thinking::Message> = session
        .messages
        .iter()
        .filter(|m| matches!(m.role, Role::Operator | Role::Partner))
        .collect();
    let chars: u64 = msgs.iter().map(|m| m.content.chars().count() as u64).sum();
    (msgs, chars)
}

/// `chars / 4` — the v3 master's transcript token heuristic.
fn estimate_tokens(chars: u64) -> u64 {
    chars / 4
}

/// The v3 master's `buildUtilSnapshot` 4-state decision, ported verbatim.
fn decide(current_tokens: u64) -> Value {
    if current_tokens >= COMPACT_AT {
        json!({
            "action": "compacting",
            "current_tokens": current_tokens,
            "threshold_used": "compact_tokens",
            "reason": format!("current {current_tokens} tok >= compact_tokens {COMPACT_AT}"),
        })
    } else if current_tokens >= WARN_AT {
        json!({
            "action": "warn",
            "current_tokens": current_tokens,
            "threshold_used": "warn_tokens",
            "reason": format!("current {current_tokens} tok >= warn_tokens {WARN_AT}"),
        })
    } else {
        json!({
            "action": "ok",
            "current_tokens": current_tokens,
            "threshold_used": "warn_tokens",
            "reason": format!("current {current_tokens} tok < warn_tokens {WARN_AT}"),
        })
    }
}

/// `GET /api/evy/transcript?session_id&limit` — v3-shape message log.
pub(crate) async fn transcript_handler(
    State(state): State<HttpState>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LIMIT);

    let Some(session) = resolve_session(&state, &q).await else {
        return Json(json!({ "ok": true, "total": 0, "returned": 0, "messages": [] }));
    };

    let (msgs, _chars) = visible_messages(&session);
    let total = msgs.len();
    let tail = if msgs.len() > limit {
        &msgs[msgs.len() - limit..]
    } else {
        &msgs[..]
    };

    let messages: Vec<Value> = tail
        .iter()
        .map(|m| {
            let role = match m.role {
                Role::Operator => "user",
                _ => "assistant",
            };
            // v3 wraps text in a content-block array; empty content → [].
            let content = if m.content.is_empty() {
                json!([])
            } else {
                json!([{ "type": "text", "text": m.content }])
            };
            json!({
                "role": role,
                "content": content,
                "timestamp": m.ts.timestamp_millis(),
            })
        })
        .collect();

    Json(json!({
        "ok": true,
        "total": total,
        "returned": messages.len(),
        "messages": messages,
    }))
}

/// `GET /api/evy/context?session_id` — context-budget meter shape.
pub(crate) async fn context_handler(
    State(state): State<HttpState>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let (msgs, chars) = match resolve_session(&state, &q).await {
        Some(s) => {
            let (m, c) = visible_messages(&s);
            (m.len() as u64, c)
        }
        None => (0, 0),
    };
    let transcript_tokens = estimate_tokens(chars);
    let total_tokens = transcript_tokens + OVERHEAD_TOKENS;

    Json(json!({
        "ok": true,
        "transcript_msgs": msgs,
        "transcript_chars": chars,
        "estimated_transcript_tokens": transcript_tokens,
        "fixed_overhead_tokens": OVERHEAD_TOKENS,
        "estimated_total_tokens": total_tokens,
        "loaded_context_length": Value::Null,
        "utilization_pct": Value::Null,
        "supervisor": state.app.supervisor_label(),
    }))
}

/// `GET /api/evy/transcript/util?session_id` — drives the 4-state banner.
pub(crate) async fn transcript_util_handler(
    State(state): State<HttpState>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let chars = match resolve_session(&state, &q).await {
        Some(s) => visible_messages(&s).1,
        None => 0,
    };
    let transcript_tokens = estimate_tokens(chars);
    let current_tokens = transcript_tokens + OVERHEAD_TOKENS;

    Json(json!({
        "ok": true,
        "current_tokens": current_tokens,
        "transcript_tokens": transcript_tokens,
        "overhead_tokens": OVERHEAD_TOKENS,
        "loaded_ctx": Value::Null,
        "util_pct": Value::Null,
        "warn_at": WARN_AT,
        "compact_at": COMPACT_AT,
        "target_tokens": TARGET_TOKENS,
        "config_mode": "absolute",
        "decision": decide(current_tokens),
    }))
}

/// Messages kept after a compaction (the recent tail). Mirrors the v3
/// master's `keep_recent` default.
const DEFAULT_KEEP_RECENT: usize = 6;

/// `POST /api/evy/transcript/compact?session_id&keep_recent` — P3.
/// Drops the oldest messages (archiving them to disk), keeps the recent tail,
/// and persists. Returns `{ok,archived_count,kept_msgs,noop}`.
pub(crate) async fn compact_handler(
    State(state): State<HttpState>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let keep_recent = q
        .get("keep_recent")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_KEEP_RECENT);
    let Some(partner) = state.app.thinking_partner() else {
        return Json(json!({ "ok": false, "error": "thinking-partner not configured" }));
    };
    let Some(session) = resolve_session(&state, &q).await else {
        return Json(json!({ "ok": true, "archived_count": 0, "kept_msgs": 0, "noop": true }));
    };
    match partner.compact_session(session.id, keep_recent).await {
        Some((archived, kept)) => {
            // Ping the chat tab so it refreshes its transcript view (the BFF
            // broadcast `transcript_compacted`; chat.js listens for it).
            state
                .broadcaster
                .emit(DaemonEvent::dashboard_transcript_compacted());
            Json(json!({
                "ok": true,
                "archived_count": archived,
                "kept_msgs": kept,
                "noop": archived == 0,
            }))
        }
        None => Json(json!({ "ok": true, "archived_count": 0, "kept_msgs": 0, "noop": true })),
    }
}

/// `POST /api/evy/transcript/clear?session_id` — P3 ("New Chat").
/// Archives the whole session to disk, drops it, and persists. Returns
/// `{ok,archive:<path|null>}`.
pub(crate) async fn clear_handler(
    State(state): State<HttpState>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let Some(partner) = state.app.thinking_partner() else {
        return Json(json!({ "ok": false, "error": "thinking-partner not configured" }));
    };
    let Some(session) = resolve_session(&state, &q).await else {
        // Nothing to clear, but "New Chat" must still drop any held session so
        // the next turn starts fresh (the BFF's `resetV4Session` is
        // unconditional).
        state.reset_chat_session();
        state
            .broadcaster
            .emit(DaemonEvent::dashboard_transcript_cleared());
        return Json(json!({ "ok": true, "archive": Value::Null }));
    };
    let archive = partner.clear_session(session.id).await;
    // "New Chat" — forget the current session and ping the bus (BFF parity:
    // `resetV4Session` + broadcast `transcript_cleared`).
    state.reset_chat_session();
    state
        .broadcaster
        .emit(DaemonEvent::dashboard_transcript_cleared());
    Json(json!({
        "ok": true,
        "archive": archive.map(|p| p.display().to_string()),
    }))
}
