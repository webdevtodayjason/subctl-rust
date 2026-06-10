//! v4-native port of the dashboard "session browser" + tmux-kill surface.
//!
//! Ports four endpoints from the v3 Bun dashboard
//! (`dashboard/server.ts`) into native Rust, preserving the exact v3 wire
//! shape so the operator console renders unchanged:
//!
//! | Method | Path | v3 origin | Behaviour |
//! |--------|------|-----------|-----------|
//! | GET  | `/api/evy/sessions/list`         | `:7128` | enumerate every Claude Code session jsonl across all accounts, newest-first |
//! | GET  | `/api/evy/sessions/preview`      | `:6873` | first-user-message preview for one session (lazy hover load) |
//! | POST | `/api/evy/sessions/spawn`        | `:6966` | open an iTerm window resuming a session (macOS only) |
//! | POST | `/api/evy/sessions/{id}/kill`    | `:7461` | kill a tmux session by name |
//!
//! # Not the chat-session module
//!
//! [`crate::sessions_http`] serves the in-memory thinking-partner chat
//! sessions at `/api/evy/sessions` + `/api/evy/sessions/{id}` (DELETE).
//! This module is the **orchestrator / catalog** surface — disjoint paths,
//! disjoint data (jsonl transcripts on disk + tmux sessions). The two
//! coexist under the same `/api/evy/sessions/` prefix because matchit
//! lets static children (`list`/`preview`/`spawn`) sit beside the param
//! child (`{id}`).
//!
//! # Filesystem logic is parameterised, not env-bound
//!
//! The catalog/preview core fns take explicit directories so they're
//! unit-testable against a temp dir (house pattern, see
//! [`crate::teams_http`]). Only the thin handlers reach for `HOME` /
//! `SUBCTL_ACCOUNTS_CONF` to build the live account index.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::extract::{Path as AxPath, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Serialize;
use serde_json::{json, Value};

use evy_providers::{tmux_kill_session, tmux_session_alive, AccountsStore};

/// Default `limit` for `/api/evy/sessions/list` when the query omits it
/// (v3: heavy users carry ~5K sessions, so the default is generous).
const DEFAULT_LIST_LIMIT: usize = 1500;
/// Hard cap on `limit` regardless of what the client asks for (v3 parity).
const MAX_LIST_LIMIT: usize = 5000;
/// Bytes read from the head of a transcript to sniff cwd + first message
/// for the catalog (v3 `detectSessionMeta`: 64 KiB).
const HEAD_SNIFF_BYTES: usize = 65536;
/// Upper bound on bytes scanned by the preview endpoint before bailing —
/// guards against multi-MB base64-image first lines (v3: 8 MiB).
const PREVIEW_MAX_SCAN_BYTES: usize = 8 * 1024 * 1024;
/// Maximum preview length in characters (v3 slices content at 240).
const PREVIEW_MAX_CHARS: usize = 240;

// ── account index ────────────────────────────────────────────────────────────

/// `$HOME`, or empty string when unset (matches v3's `HOME` read).
fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Path to `accounts.conf` — `SUBCTL_ACCOUNTS_CONF` override, else the
/// default under `~/.config/subctl`. Mirrors [`crate::accounts_http`].
fn accounts_conf_path() -> PathBuf {
    std::env::var("SUBCTL_ACCOUNTS_CONF")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{}/.config/subctl/accounts.conf", home())))
}

/// The default account's config dir: `~/.claude`.
fn default_config_dir() -> PathBuf {
    PathBuf::from(format!("{}/.claude", home()))
}

/// One entry in the account index used to enumerate session transcripts.
#[derive(Debug, Clone)]
struct AccountDir {
    /// Per-account config directory (holds `projects/<dir>/<sid>.jsonl`).
    config_dir: PathBuf,
    /// Operator alias (`"default"` for the implicit `~/.claude`).
    alias: String,
    /// v3 color bucket for the alias.
    color_class: &'static str,
}

/// Port of v3 `colorClassFor`: alias → console color bucket.
fn color_class_for(alias: &str) -> &'static str {
    let a = alias.to_ascii_lowercase();
    if a.contains("personal") {
        "cyan"
    } else if a.contains("work") {
        "blue"
    } else if a.contains("overflow") {
        "magenta"
    } else {
        "grey"
    }
}

/// Build the account index: every `accounts.conf` row, plus the implicit
/// `default` (`~/.claude`) appended when no row already points there.
/// Deduplicated by config dir, preserving insertion order (v3 `Map`).
fn account_index() -> Vec<AccountDir> {
    let rows = AccountsStore::open(&accounts_conf_path())
        .and_then(|s| s.list_rows())
        .unwrap_or_default();
    let mut out: Vec<AccountDir> = Vec::with_capacity(rows.len() + 1);
    for r in rows {
        if out.iter().any(|a| a.config_dir == r.config_dir) {
            continue;
        }
        let color = color_class_for(&r.alias);
        out.push(AccountDir {
            config_dir: r.config_dir,
            alias: r.alias,
            color_class: color,
        });
    }
    let def = default_config_dir();
    if !out.iter().any(|a| a.config_dir == def) {
        out.push(AccountDir {
            config_dir: def,
            alias: "default".to_string(),
            color_class: "grey",
        });
    }
    out
}

/// Resolve one account alias to its config dir (`"default"` → `~/.claude`).
/// `None` mirrors v3's "unknown account" 404.
fn resolve_config_dir(account: &str) -> Option<PathBuf> {
    if account == "default" {
        return Some(default_config_dir());
    }
    AccountsStore::open(&accounts_conf_path())
        .and_then(|s| s.find_row(account))
        .ok()
        .flatten()
        .map(|r| r.config_dir)
}

// ── transcript sniffing (pure) ────────────────────────────────────────────────

/// Collapse every run of whitespace to a single space (v3 `\s+ → " "`).
/// Does **not** trim the ends — callers trim when v3 does.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out
}

/// Does the text open with an orchestrator teammate-message envelope?
/// Port of v3's `^<teammate-message\s+teammate_id="` (case-insensitive).
fn is_teammate_message(s: &str) -> bool {
    // Only the prefix matters; lowercase a bounded slice, not the body.
    let head: String = s
        .trim_start()
        .chars()
        .take(48)
        .collect::<String>()
        .to_ascii_lowercase();
    let Some(rest) = head.strip_prefix("<teammate-message") else {
        return false;
    };
    let trimmed = rest.trim_start_matches(char::is_whitespace);
    // Require at least one whitespace char (the `\s+`), then the attr.
    rest.len() != trimmed.len() && trimmed.starts_with("teammate_id=\"")
}

/// Extract the user-visible text from a message `content` value:
/// a plain string, else the first array part with a non-empty `.text`,
/// else a `(type, type, …)` marker built from the first three part types.
/// `None` when there's nothing renderable. Port of the shared v3 branch.
fn first_user_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => {
            if let Some(t) = arr.iter().find_map(|x| {
                x.get("text")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            }) {
                return Some(t.to_string());
            }
            // No text part — fall back to a type marker iff the first part
            // carries a `type` (v3 guards on `c[0]?.type`).
            if arr.first().and_then(|x| x.get("type")).is_some() {
                let types: Vec<&str> = arr
                    .iter()
                    .filter_map(|x| x.get("type").and_then(Value::as_str))
                    .take(3)
                    .collect();
                if !types.is_empty() {
                    return Some(format!("({})", types.join(", ")));
                }
            }
            None
        }
        _ => None,
    }
}

/// Sniffed metadata for one transcript (catalog row inputs).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SessionMeta {
    /// Canonical cwd from the first event that exposes one (may be empty).
    cwd: String,
    /// First user message, trimmed → 240 chars → whitespace-collapsed.
    preview: String,
    /// True when the first user message is a teammate-message envelope.
    is_worker: bool,
}

/// Port of v3 `detectSessionMeta`: read the head of `path`, capture the
/// canonical cwd, the first user-message preview, and the worker flag.
fn detect_session_meta(path: &Path) -> SessionMeta {
    let mut meta = SessionMeta::default();
    let Some(head) = read_head(path, HEAD_SNIFF_BYTES) else {
        return meta;
    };
    for line in head.split('\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if meta.cwd.is_empty() {
            if let Some(c) = ev.get("cwd").and_then(Value::as_str) {
                if !c.is_empty() {
                    meta.cwd = c.to_string();
                }
            }
        }
        if meta.preview.is_empty() && ev.get("type").and_then(Value::as_str) == Some("user") {
            if let Some(text) = ev
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(first_user_text)
            {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    meta.is_worker = is_teammate_message(trimmed);
                    let sliced: String = trimmed.chars().take(PREVIEW_MAX_CHARS).collect();
                    meta.preview = collapse_ws(&sliced);
                }
            }
        }
        if !meta.cwd.is_empty() && !meta.preview.is_empty() {
            break;
        }
    }
    meta
}

/// Read at most `cap` bytes from the start of `path` as lossy UTF-8.
/// `None` when the file can't be opened.
fn read_head(path: &Path, cap: usize) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; cap];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

// ── catalog (pure) ────────────────────────────────────────────────────────────

/// One row of the session catalog — exact v3 `SessionCatalogRow` shape.
#[derive(Debug, Clone, Serialize)]
struct SessionCatalogRow {
    /// Session UUID (jsonl filename stem).
    sid: String,
    /// Owning account alias.
    account: String,
    /// Console color bucket for the account.
    account_color_class: &'static str,
    /// Account config dir the transcript was found under.
    config_dir: String,
    /// Canonical working directory of the session.
    cwd: String,
    /// Basename of `cwd`.
    project: String,
    /// Transcript mtime, milliseconds since the Unix epoch.
    mtime_ts: i64,
    /// Transcript size rounded to whole KiB.
    size_kb: u64,
    /// First user-message preview (may be empty).
    first_message_preview: String,
    /// True for orchestrator-spawned team agents.
    is_worker: bool,
}

/// Enumerate every `projects/<dir>/<sid>.jsonl` across the account index,
/// newest-first, truncated to `limit`. Port of v3 `listAllClaudeSessions`.
fn list_all_claude_sessions(index: &[AccountDir], limit: usize) -> Vec<SessionCatalogRow> {
    let mut rows: Vec<SessionCatalogRow> = Vec::new();
    for acct in index {
        let projects_root = acct.config_dir.join("projects");
        let Ok(project_dirs) = std::fs::read_dir(&projects_root) else {
            continue;
        };
        for pd in project_dirs.flatten() {
            let project_path = pd.path();
            if !project_path.is_dir() {
                continue;
            }
            let pd_name = pd.file_name().to_string_lossy().into_owned();
            let Ok(files) = std::fs::read_dir(&project_path) else {
                continue;
            };
            for f in files.flatten() {
                let fpath = f.path();
                if fpath.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Ok(st) = f.metadata() else { continue };
                if !st.is_file() {
                    continue;
                }
                let sid = fpath
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let meta = detect_session_meta(&fpath);
                let cwd = if meta.cwd.is_empty() {
                    decode_project_cwd(&pd_name)
                } else {
                    meta.cwd
                };
                let project = Path::new(&cwd)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                rows.push(SessionCatalogRow {
                    sid,
                    account: acct.alias.clone(),
                    account_color_class: acct.color_class,
                    config_dir: acct.config_dir.to_string_lossy().into_owned(),
                    cwd,
                    project,
                    mtime_ts: mtime_ms(&st),
                    size_kb: ((st.len() as f64) / 1024.0).round() as u64,
                    first_message_preview: meta.preview,
                    is_worker: meta.is_worker,
                });
            }
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.mtime_ts));
    rows.truncate(limit);
    rows
}

/// Decode a project dir name (`-Users-sem-code-subctl`) to a lossy cwd
/// (`/Users/sem/code/subctl`). v3 fallback when the transcript has no cwd.
fn decode_project_cwd(pd_name: &str) -> String {
    format!(
        "/{}",
        pd_name
            .strip_prefix('-')
            .unwrap_or(pd_name)
            .replace('-', "/")
    )
}

/// File mtime in milliseconds since the Unix epoch (0 if unavailable).
fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

// ── preview (pure) ────────────────────────────────────────────────────────────

/// Result of scanning one transcript for the preview endpoint.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PreviewScan {
    /// First user-message preview, collapsed + trimmed (may be empty).
    preview: String,
    /// First event timestamp string, verbatim (may be empty).
    first_ts: String,
}

/// Find `<sid>.jsonl` under any `projects/<dir>/` of `config_dir` and
/// scan it for the first timestamp + first user-message preview. Port of
/// the v3 `/api/sessions/preview` scan (bounded at 8 MiB).
fn scan_session_preview(config_dir: &Path, sid: &str) -> PreviewScan {
    let projects_root = config_dir.join("projects");
    let Ok(project_dirs) = std::fs::read_dir(&projects_root) else {
        return PreviewScan::default();
    };
    for pd in project_dirs.flatten() {
        let candidate = pd.path().join(format!("{sid}.jsonl"));
        match std::fs::metadata(&candidate) {
            Ok(m) if m.is_file() => return scan_preview_file(&candidate),
            _ => continue,
        }
    }
    PreviewScan::default()
}

/// Scan a single transcript file for `first_ts` + `preview`, reading at
/// most [`PREVIEW_MAX_SCAN_BYTES`] and stopping once both are found.
fn scan_preview_file(path: &Path) -> PreviewScan {
    let mut scan = PreviewScan::default();
    let Some(text) = read_head(path, PREVIEW_MAX_SCAN_BYTES) else {
        return scan;
    };
    for line in text.split('\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if scan.first_ts.is_empty() {
            if let Some(ts) = ev.get("timestamp").and_then(Value::as_str) {
                if !ts.is_empty() {
                    scan.first_ts = ts.to_string();
                }
            }
        }
        if scan.preview.is_empty() && ev.get("type").and_then(Value::as_str) == Some("user") {
            if let Some(text) = ev
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(first_user_text)
            {
                let sliced: String = text.chars().take(PREVIEW_MAX_CHARS).collect();
                let p = collapse_ws(&sliced).trim().to_string();
                if !p.is_empty() {
                    scan.preview = p;
                }
            }
        }
        if !scan.first_ts.is_empty() && !scan.preview.is_empty() {
            break;
        }
    }
    scan
}

// ── shell / osascript helpers ─────────────────────────────────────────────────

/// POSIX single-quote escaping (v3 `shellEscape`): wrap in `'…'`,
/// rewriting embedded quotes as `'\''`.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// True when `sid` is a non-empty `[A-Za-z0-9_-]+` token (v3 validator).
fn valid_sid(sid: &str) -> bool {
    !sid.is_empty()
        && sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// ── handlers ──────────────────────────────────────────────────────────────────

/// `GET /api/evy/sessions/list?limit&workers` →
/// `{ sessions:[SessionCatalogRow,…], total }`.
///
/// `limit` defaults to 1500 (cap 5000); `workers=1` keeps orchestrator
/// worker sessions, which are otherwise filtered out **after** the limit
/// slice (exact v3 ordering, so a worker-heavy head can yield fewer rows).
pub(crate) async fn sessions_list_handler(Query(q): Query<HashMap<String, String>>) -> Response {
    let limit = match q.get("limit") {
        None => DEFAULT_LIST_LIMIT,
        // Unparseable mirrors v3's `Number("x") → NaN → slice(0,NaN) → []`.
        Some(raw) => raw.parse::<usize>().unwrap_or(0).min(MAX_LIST_LIMIT),
    };
    let include_workers = q.get("workers").map(String::as_str) == Some("1");

    let index = account_index();
    let mut sessions = list_all_claude_sessions(&index, limit);
    if !include_workers {
        sessions.retain(|s| !s.is_worker);
    }
    let total = sessions.len();
    Json(json!({ "sessions": sessions, "total": total })).into_response()
}

/// `GET /api/evy/sessions/preview?account&sid` →
/// `{ ok:true, sid, account, preview, first_ts }`.
///
/// 400 on a missing/invalid `account`/`sid`, 404 on an unknown account.
pub(crate) async fn sessions_preview_handler(Query(q): Query<HashMap<String, String>>) -> Response {
    let account = q.get("account").map(String::as_str).unwrap_or("");
    let sid = q.get("sid").map(String::as_str).unwrap_or("");
    if account.is_empty() || !valid_sid(sid) {
        return err_json(StatusCode::BAD_REQUEST, "missing/invalid account or sid");
    }
    let Some(cfg_dir) = resolve_config_dir(account) else {
        return err_json(StatusCode::NOT_FOUND, "unknown account");
    };
    let scan = scan_session_preview(&cfg_dir, sid);
    Json(json!({
        "ok": true,
        "sid": sid,
        "account": account,
        "preview": scan.preview,
        "first_ts": scan.first_ts,
    }))
    .into_response()
}

/// `POST /api/evy/sessions/spawn` — body `{ account, sid, cwd? }`.
///
/// Opens an iTerm window (macOS only) pre-running the resume command.
/// 400 invalid input, 404 unknown account, 501 off macOS (with a
/// copy-paste `fallback`), 500 on osascript failure, 200 `{ ok:true }`.
pub(crate) async fn sessions_spawn_handler(raw: axum::body::Bytes) -> Response {
    // v3 does `try { body = await req.json() } catch {}` — tolerant of a
    // missing/invalid body, defaulting to an empty object.
    let body = serde_json::from_slice::<Value>(&raw).unwrap_or(Value::Null);
    let account = body.get("account").and_then(Value::as_str).unwrap_or("");
    let sid = body.get("sid").and_then(Value::as_str).unwrap_or("");
    let cwd_raw = body.get("cwd").and_then(Value::as_str).unwrap_or("");
    if account.is_empty() || !valid_sid(sid) {
        return err_json(StatusCode::BAD_REQUEST, "missing/invalid account or sid");
    }
    let Some(cfg_dir) = resolve_config_dir(account) else {
        return err_json(StatusCode::NOT_FOUND, "unknown account");
    };
    // cwd must exist; default to $HOME (v3 uses existsSync, not isDir).
    let cwd = if !cwd_raw.is_empty() && Path::new(cwd_raw).exists() {
        cwd_raw.to_string()
    } else {
        home()
    };
    let fallback = format!(
        "cd {} && CLAUDE_CONFIG_DIR={} command claude --resume {}",
        shell_escape(&cwd),
        shell_escape(&cfg_dir.to_string_lossy()),
        shell_escape(sid),
    );

    if !cfg!(target_os = "macos") {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "ok": false, "error": "spawn requires macOS + iTerm", "fallback": fallback })),
        )
            .into_response();
    }

    match run_osascript(&fallback).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(msg) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": truncate(&msg, 500), "fallback": fallback })),
        )
            .into_response(),
    }
}

/// `POST /api/evy/sessions/{id}/kill` → `{ ok:true }`.
///
/// `id` is the tmux session **name**. 404 `{ ok:false, error:"session
/// not found" }` when the session isn't live, 500 on a tmux failure.
/// Reuses the evy-providers tmux plumbing (absolute-path `tmux` bin).
pub(crate) async fn sessions_kill_handler(AxPath(id): AxPath<String>) -> Response {
    if !tmux_session_alive(&id).await {
        return err_json(StatusCode::NOT_FOUND, "session not found");
    }
    match tmux_kill_session(&id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &truncate(&e.to_string(), 500),
        ),
    }
}

/// `{ ok:false, error }` body at `status` — the v3 failure envelope.
fn err_json(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "ok": false, "error": error }))).into_response()
}

/// Truncate `s` to at most `max` characters (codepoints, not bytes).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Run the iTerm-spawn AppleScript via `/usr/bin/osascript` with a 5 s
/// timeout. `Err(reason)` on non-zero exit, timeout, or spawn failure.
async fn run_osascript(fallback_cmd: &str) -> std::result::Result<(), String> {
    // Mirror v3's double escape: backslash then double-quote, interpolated
    // into the `write text "…"` line.
    let escaped = fallback_cmd.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "tell application \"iTerm\"\n  activate\n  set newWin to (create window with default profile)\n  tell current session of newWin to write text \"{escaped}\"\nend tell"
    );
    let fut = tokio::process::Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output();
    let output = match tokio::time::timeout(std::time::Duration::from_secs(5), fut).await {
        Err(_) => return Err("timed out".to_string()),
        Ok(Err(e)) => return Err(format!("spawn failed: {e}")),
        Ok(Ok(o)) => o,
    };
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let reason = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        "spawn failed".to_string()
    };
    Err(reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmpdir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "evy-orch-sessions-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Write `<config_dir>/projects/<project_dir>/<sid>.jsonl` with `lines`.
    fn write_session(config_dir: &Path, project_dir: &str, sid: &str, lines: &[&str]) -> PathBuf {
        let dir = config_dir.join("projects").join(project_dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{sid}.jsonl"));
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn index_for(config_dir: &Path, alias: &str) -> Vec<AccountDir> {
        vec![AccountDir {
            config_dir: config_dir.to_path_buf(),
            alias: alias.to_string(),
            color_class: color_class_for(alias),
        }]
    }

    #[test]
    fn color_class_buckets_match_v3() {
        assert_eq!(color_class_for("claude-personal"), "cyan");
        assert_eq!(color_class_for("claude-work"), "blue");
        assert_eq!(color_class_for("claude-overflow"), "magenta");
        assert_eq!(color_class_for("default"), "grey");
        assert_eq!(color_class_for("something-else"), "grey");
    }

    #[test]
    fn decode_project_cwd_strips_leading_dash() {
        assert_eq!(
            decode_project_cwd("-Users-sem-code-subctl"),
            "/Users/sem/code/subctl"
        );
        assert_eq!(decode_project_cwd("-tmp"), "/tmp");
    }

    #[test]
    fn shell_escape_wraps_and_escapes_quotes() {
        assert_eq!(shell_escape("/tmp/x"), "'/tmp/x'");
        assert_eq!(shell_escape("a'b"), "'a'\\''b'");
    }

    #[test]
    fn valid_sid_accepts_uuid_rejects_specials() {
        assert!(valid_sid("029c522b-7114-4dd9-86a6-bb151f70f86e"));
        assert!(valid_sid("abc_DEF-123"));
        assert!(!valid_sid(""));
        assert!(!valid_sid("not!valid"));
        assert!(!valid_sid("../etc"));
    }

    #[test]
    fn collapse_ws_collapses_runs_without_trimming() {
        assert_eq!(collapse_ws("a   b\n\tc"), "a b c");
        assert_eq!(collapse_ws("  lead"), " lead");
    }

    #[test]
    fn is_teammate_message_detects_envelope() {
        assert!(is_teammate_message(
            "<teammate-message teammate_id=\"team-lead\">"
        ));
        assert!(is_teammate_message(
            "<TEAMMATE-MESSAGE   teammate_id=\"x\">"
        ));
        // needs the whitespace + attr
        assert!(!is_teammate_message("<teammate-message>"));
        assert!(!is_teammate_message("<teammate-messageteammate_id=\"x\">"));
        assert!(!is_teammate_message("hello world"));
    }

    #[test]
    fn first_user_text_handles_string_array_and_marker() {
        assert_eq!(first_user_text(&json!("hi")), Some("hi".to_string()));
        assert_eq!(
            first_user_text(&json!([{"type": "text", "text": "from array"}])),
            Some("from array".to_string())
        );
        // No text → type marker from first 3 part types.
        assert_eq!(
            first_user_text(&json!([{"type": "image"}, {"type": "tool_use"}])),
            Some("(image, tool_use)".to_string())
        );
        assert_eq!(first_user_text(&json!([{"foo": 1}])), None);
        assert_eq!(first_user_text(&json!(42)), None);
    }

    #[test]
    fn detect_session_meta_reads_cwd_preview_worker() {
        let dir = tmpdir();
        let path = write_session(
            &dir,
            "-Users-sem-code-subctl",
            "sid-worker",
            &[
                r#"{"type":"system","cwd":"/Users/sem/code/subctl","timestamp":"2026-06-09T00:00:00Z"}"#,
                r#"{"type":"user","message":{"content":"<teammate-message teammate_id=\"team-lead\">  do the   thing"}}"#,
            ],
        );
        let meta = detect_session_meta(&path);
        assert_eq!(meta.cwd, "/Users/sem/code/subctl");
        assert!(meta.is_worker);
        assert_eq!(
            meta.preview,
            "<teammate-message teammate_id=\"team-lead\"> do the thing"
        );
    }

    #[test]
    fn detect_session_meta_non_worker_user() {
        let dir = tmpdir();
        let path = write_session(
            &dir,
            "-tmp-proj",
            "sid-plain",
            &[r#"{"type":"user","cwd":"/tmp/proj","message":{"content":"hello there"}}"#],
        );
        let meta = detect_session_meta(&path);
        assert!(!meta.is_worker);
        assert_eq!(meta.preview, "hello there");
        assert_eq!(meta.cwd, "/tmp/proj");
    }

    #[test]
    fn list_all_sorts_newest_first_and_truncates() {
        let dir = tmpdir();
        let p1 = write_session(
            &dir,
            "-a",
            "old",
            &[r#"{"type":"user","message":{"content":"first"}}"#],
        );
        let p2 = write_session(
            &dir,
            "-a",
            "new",
            &[r#"{"type":"user","message":{"content":"second"}}"#],
        );
        // Force a strict mtime ordering: old < new.
        let early = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let late = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000);
        filetime_set(&p1, early);
        filetime_set(&p2, late);

        let index = index_for(&dir, "default");
        let rows = list_all_claude_sessions(&index, 10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].sid, "new");
        assert_eq!(rows[1].sid, "old");
        // Truncation keeps the newest.
        let one = list_all_claude_sessions(&index, 1);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].sid, "new");
        assert_eq!(one[0].account, "default");
        assert_eq!(one[0].account_color_class, "grey");
    }

    #[test]
    fn list_all_marks_workers() {
        let dir = tmpdir();
        write_session(
            &dir,
            "-w",
            "wsid",
            &[
                r#"{"type":"user","message":{"content":"<teammate-message teammate_id=\"lead\"> go"}}"#,
            ],
        );
        let rows = list_all_claude_sessions(&index_for(&dir, "default"), 10);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_worker);
    }

    #[test]
    fn scan_preview_finds_first_ts_and_preview() {
        let dir = tmpdir();
        write_session(
            &dir,
            "-p",
            "psid",
            &[
                r#"{"type":"system","timestamp":"2026-06-09T01:02:03Z"}"#,
                r#"{"type":"user","message":{"content":"  the   operator    asked"}}"#,
            ],
        );
        let scan = scan_session_preview(&dir, "psid");
        assert_eq!(scan.first_ts, "2026-06-09T01:02:03Z");
        assert_eq!(scan.preview, "the operator asked");
    }

    #[test]
    fn scan_preview_missing_session_is_empty() {
        let dir = tmpdir();
        let scan = scan_session_preview(&dir, "does-not-exist");
        assert_eq!(scan, PreviewScan::default());
    }

    #[test]
    fn truncate_caps_chars() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 3), "hel");
        assert_eq!(truncate("héllo", 2).chars().count(), 2);
    }

    /// Best-effort mtime setter for the ordering test. Uses a tiny helper
    /// over `std::fs` set_times via the `filetime`-free path: we re-open
    /// and rely on `set_modified` from std (stable since 1.75).
    fn filetime_set(path: &Path, when: std::time::SystemTime) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }
}
