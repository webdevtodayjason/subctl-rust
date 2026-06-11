//! W5 — v4-native Memory tab families (port of `dashboard/server.ts`'s
//! memory surface; wire spec = `dashboard/public/tabs/memory.js`).
//!
//! | Method | Path | v3 source | Response envelope |
//! |--------|------|-----------|-------------------|
//! | GET | `/api/memory/tier1` | server.ts:4751 | `{ ok, memory: {path, exists, content, char_count, char_limit}, user_profile: {…} }` |
//! | POST | `/api/memory/tier1` | server.ts:4770 | `{ ok, path, char_count, char_limit, message }` |
//! | GET | `/api/memory` | server.ts:5027 | `{ ok, obsidian_installed, obsidian_app_path, vaults, suggested_install, suggested_vault_path }` |
//! | any | `/api/cognee/{*rest}` | server.ts:6692 | sidecar body verbatim (thin forward, 127.0.0.1:`$SUBCTL_COGNEE_PORT` ?? 8745) |
//! | any | `/api/memori/{*rest}` | server.ts:6722 | sidecar body verbatim (thin forward, 127.0.0.1:`$SUBCTL_MEMORI_PORT` ?? 8746) |
//!
//! # Ownership verifications (why these are safe to serve natively)
//!
//! - **Tier 1** reads/writes `<config>/evy/{memory.md,user.md}` — the exact
//!   files the v3 master re-reads **fresh on every prompt** (no daemon-side
//!   cache): `components/evy/server.ts:4023-4026` rebuilds the system prompt
//!   per turn via `buildMemoryBlock()`, and `components/evy/tools/
//!   tier1-memory.ts:17-19` documents the re-read. The daemon's
//!   `~/.config/subctl/master/` dir is a symlink to `evy/` on the deployed
//!   host, so a v4 write lands in the master's next turn exactly like a v3
//!   dashboard write did.
//! - **Bare `/api/memory`** reads operator-owned files on disk only
//!   (`evy/obsidian.json` + vault dirs) — no v3 state involved.
//! - **Cognee / Memori** are thin forwards straight to the local sidecars,
//!   mirroring v3's proxy semantics byte-for-byte (only a `Content-Type:
//!   application/json` request header, body buffered, upstream status +
//!   `Content-Type` echoed, 502 `{ok:false,error:"<label> sidecar
//!   unreachable: …"}` when the sidecar is dark). Sidecar ports verified
//!   from server.ts:6693 (cognee 8745) and :6723 (memori 8746).
//!
//! # What deliberately STAYS on the `/api/{*rest}` catch-all (tripwire)
//!
//! `/api/memory/{stats,recent,search,entries/*}` is backed by the v3
//! master's **own** SQLite store (`~/.local/state/subctl/memory/evy.db`,
//! `components/evy/memory.ts:24,73`) with daemon-side egress redaction
//! (`redactEntryForEgress`, `components/evy/server.ts:4882-4886`) — NOT the
//! operator's claude-mem DB, so v4's `ClaudeMemReader` cannot serve it.
//! Those subpaths keep riding the reverse-proxy to v3 → master (:8788).
//! `/api/master/memory/*` (tier1 workflow + kernel) is likewise untouched.
//!
//! # Benign diffs vs the :8787 oracle
//!
//! - JSON **key order** differs (serde_json sorts keys; Bun preserves
//!   insertion order) for the v4-constructed bodies. Sidecar forwards are
//!   byte-identical (upstream body passed through verbatim).
//! - Path resolution honors `$SUBCTL_CONFIG_DIR` (v3's tier1 block hardcodes
//!   `$HOME/.config/subctl`); identical when the override is unset, which is
//!   the deployed configuration.
//! - Error-path messages embed reqwest's error text where v3 embeds Bun's.

use std::path::{Path, PathBuf};

use axum::body::Bytes;
use axum::extract::Request;
use axum::http::{header::CONTENT_TYPE, Method, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

/// Hermes-derived Tier 1 budgets (`tools/tier1-memory.ts:40-41`); v3's
/// dashboard endpoint hardcodes the same two numbers (server.ts:4766-4767).
const MEMORY_CHAR_LIMIT: usize = 2200;
const USER_CHAR_LIMIT: usize = 1375;

/// `$SUBCTL_CONFIG_DIR` or `~/.config/subctl` — same resolution as
/// `preferences_http` (and v3's `SUBCTL_CONFIG_DIR` const, server.ts:876).
fn config_dir() -> PathBuf {
    std::env::var("SUBCTL_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".config/subctl")
        })
}

fn err_json(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "ok": false, "error": msg.into() }))).into_response()
}

// ── /api/memory/tier1 (GET server.ts:4751, POST server.ts:4770) ───────────

/// v3's `readSafe` (server.ts:4755-4763): missing file → `exists:false` +
/// empty content; read error → same plus an `error` field. `char_count` is
/// UTF-16 code units to match JS `String.length`.
fn tier1_file_value(path: &Path, limit: usize) -> Value {
    let path_s = path.display().to_string();
    if !path.exists() {
        return json!({
            "path": path_s, "exists": false, "content": "",
            "char_count": 0, "char_limit": limit,
        });
    }
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let count = content.encode_utf16().count();
            json!({
                "path": path_s, "exists": true, "content": content,
                "char_count": count, "char_limit": limit,
            })
        }
        Err(e) => json!({
            "path": path_s, "exists": false, "content": "",
            "char_count": 0, "char_limit": limit, "error": e.to_string(),
        }),
    }
}

/// `GET /api/memory/tier1` → both Tier 1 files (read fresh per request).
pub(crate) async fn tier1_get_handler() -> Response {
    let dir = config_dir().join("evy");
    Json(json!({
        "ok": true,
        "memory": tier1_file_value(&dir.join("memory.md"), MEMORY_CHAR_LIMIT),
        "user_profile": tier1_file_value(&dir.join("user.md"), USER_CHAR_LIMIT),
    }))
    .into_response()
}

/// `POST /api/memory/tier1` body `{ which: "memory"|"user", content }` →
/// trim, enforce the char budget, write the file. The master's next prompt
/// composition re-reads it (no daemon restart, no cache to bust).
pub(crate) async fn tier1_post_handler(body: Bytes) -> Response {
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return err_json(StatusCode::BAD_REQUEST, "invalid JSON"),
    };
    let (file, limit) = match parsed.get("which").and_then(Value::as_str) {
        Some("memory") => ("memory.md", MEMORY_CHAR_LIMIT),
        Some("user") => ("user.md", USER_CHAR_LIMIT),
        _ => return err_json(StatusCode::BAD_REQUEST, "which must be 'memory' or 'user'"),
    };
    let content = parsed
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let count = content.encode_utf16().count();
    if count > limit {
        return err_json(
            StatusCode::BAD_REQUEST,
            format!("content exceeds char limit ({count} > {limit})"),
        );
    }
    let dir = config_dir().join("evy");
    let path = dir.join(file);
    let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &content));
    if let Err(e) = write {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    Json(json!({
        "ok": true,
        "path": path.display().to_string(),
        "char_count": count,
        "char_limit": limit,
        "message": "next agent prompt will pick up the new content",
    }))
    .into_response()
}

// ── GET /api/memory — Obsidian + vault state (server.ts:5027) ─────────────

/// Count `.md` files under `dir` recursively, skipping dot-entries, with
/// v3's defensive 5000 cap (server.ts:5056-5066).
fn count_notes(dir: &Path) -> u64 {
    let mut count: u64 = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        if count >= 5000 {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&cur) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if name.ends_with(".md") {
                count += 1;
            }
        }
    }
    count
}

/// JS `new Date(ms).toISOString()` — UTC with milliseconds.
fn iso_millis(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t)
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// The configured vault root from `<config>/evy/obsidian.json`
/// (`{ vault_root }`, leading `~` expanded) — same file the dashboard
/// Settings tab writes (and `preferences_http::obsidian_*` serve).
fn configured_vault_root(home: &str) -> Option<String> {
    let cfg_path = config_dir().join("evy").join("obsidian.json");
    let text = std::fs::read_to_string(cfg_path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let root = v.get("vault_root")?.as_str()?;
    if root.is_empty() {
        return None;
    }
    Some(if let Some(rest) = root.strip_prefix('~') {
        format!("{home}{rest}")
    } else {
        root.to_string()
    })
}

/// `GET /api/memory` → Obsidian install + per-vault note counts. Each
/// subdirectory of a candidate root containing `.obsidian/` is a vault.
pub(crate) async fn memory_overview_handler() -> Response {
    let home = std::env::var("HOME").unwrap_or_default();
    let obsidian_app = "/Applications/Obsidian.app";
    let installed = Path::new(obsidian_app).exists();

    let candidates: Vec<String> = match configured_vault_root(&home) {
        Some(root) => vec![root],
        None => vec![
            format!("{home}/Documents/Obsidian Vault"),
            format!("{home}/Documents/ObsidianVault"),
            format!("{home}/Obsidian"),
            format!("{home}/vaults"),
        ],
    };

    let mut vaults: Vec<Value> = Vec::new();
    for candidate in &candidates {
        let candidate = Path::new(candidate);
        let Ok(entries) = std::fs::read_dir(candidate) else {
            continue;
        };
        for entry in entries.flatten() {
            let vault_dir = entry.path();
            if !vault_dir.is_dir() || !vault_dir.join(".obsidian").exists() {
                continue;
            }
            let last_modified = std::fs::metadata(&vault_dir)
                .and_then(|m| m.modified())
                .map(iso_millis)
                .ok();
            vaults.push(json!({
                "path": vault_dir.display().to_string(),
                "note_count": count_notes(&vault_dir),
                "last_modified": last_modified,
            }));
        }
    }

    Json(json!({
        "ok": true,
        "obsidian_installed": installed,
        "obsidian_app_path": if installed { Value::from(obsidian_app) } else { Value::Null },
        "vaults": vaults,
        "suggested_install": "brew install --cask obsidian",
        "suggested_vault_path": format!("{home}/Documents/Obsidian Vault"),
    }))
    .into_response()
}

// ── /api/cognee/* + /api/memori/* — thin sidecar forwards ─────────────────

/// `any /api/cognee/{*rest}` → `http://127.0.0.1:$SUBCTL_COGNEE_PORT??8745`
/// (server.ts:6692-6715). Defaults follow `components/evy/cognee-client.ts:100`.
pub(crate) async fn cognee_forward_handler(req: Request) -> Response {
    sidecar_forward(req, "/api/cognee", "SUBCTL_COGNEE_PORT", 8745, "cognee").await
}

/// `any /api/memori/{*rest}` → `http://127.0.0.1:$SUBCTL_MEMORI_PORT??8746`
/// (server.ts:6722-6745). Defaults follow `components/evy/memori-client.ts:185`.
pub(crate) async fn memori_forward_handler(req: Request) -> Response {
    sidecar_forward(req, "/api/memori", "SUBCTL_MEMORI_PORT", 8746, "memori").await
}

/// v3's sidecar-proxy idiom, ported 1:1: strip the `/api/<label>` prefix,
/// keep the query, send ONLY a `Content-Type: application/json` request
/// header (v3 deliberately does not forward browser headers), attach the
/// body for non-GET/HEAD, buffer the upstream body, and echo upstream
/// status + `Content-Type` (default `application/json`). No timeout —
/// matching v3 (`/cognify` legitimately runs long; the sidecars are local,
/// so connection-refused fails instantly when one is down).
async fn sidecar_forward(
    req: Request,
    prefix: &str,
    port_env: &str,
    default_port: u16,
    label: &str,
) -> Response {
    let port = std::env::var(port_env)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_port.to_string());
    let path_q = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    let rest = path_q.strip_prefix(prefix).unwrap_or(&path_q);
    let url = format!("http://127.0.0.1:{port}{rest}");

    let method = req.method().clone();
    let mut builder = crate::proxy_http::client()
        .request(method.clone(), &url)
        .header(CONTENT_TYPE, "application/json");
    if method != Method::GET && method != Method::HEAD {
        let body = match axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024).await {
            Ok(b) => b,
            Err(e) => return err_json(StatusCode::BAD_GATEWAY, format!("read request body: {e}")),
        };
        builder = builder.body(body);
    }

    let unreachable = |e: &dyn std::fmt::Display| format!("{label} sidecar unreachable: {e}");
    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => return err_json(StatusCode::BAD_GATEWAY, unreachable(&e)),
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    // v3 awaits `upstream.text()` inside the same try — a body-read failure
    // lands in the same 502 catch.
    let body = match resp.text().await {
        Ok(t) => t,
        Err(e) => return err_json(StatusCode::BAD_GATEWAY, unreachable(&e)),
    };
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|e| {
            err_json(
                StatusCode::BAD_GATEWAY,
                format!("build forwarded response: {e}"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier1_file_value_missing_file_is_empty_not_error() {
        let v = tier1_file_value(Path::new("/nonexistent/memory.md"), MEMORY_CHAR_LIMIT);
        assert_eq!(v["exists"], false);
        assert_eq!(v["content"], "");
        assert_eq!(v["char_count"], 0);
        assert_eq!(v["char_limit"], 2200);
        assert!(v.get("error").is_none());
    }

    #[test]
    fn tier1_file_value_counts_utf16_units_like_js() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("memory.md");
        // '𝄞' is one char but two UTF-16 code units — JS `length` says 2.
        std::fs::write(&p, "a𝄞").unwrap();
        let v = tier1_file_value(&p, MEMORY_CHAR_LIMIT);
        assert_eq!(v["exists"], true);
        assert_eq!(v["char_count"], 3);
    }

    #[test]
    fn count_notes_skips_dot_entries_and_recurses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/.obsidian")).unwrap();
        std::fs::write(dir.path().join("a.md"), "x").unwrap();
        std::fs::write(dir.path().join("sub/b.md"), "x").unwrap();
        std::fs::write(dir.path().join("sub/.hidden.md"), "x").unwrap();
        std::fs::write(dir.path().join("sub/c.txt"), "x").unwrap();
        assert_eq!(count_notes(dir.path()), 2);
    }

    #[test]
    fn iso_millis_matches_js_to_iso_string_shape() {
        let t =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_123);
        assert_eq!(iso_millis(t), "2023-11-14T22:13:20.123Z");
    }
}
