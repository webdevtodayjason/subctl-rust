//! Cutover — native settings / auth-status read surface.
//!
//! Ports the **dashboard-owned, read-only, v4-owned** members of the v3
//! settings/preferences/profile/auth/update/upstreams family from
//! `dashboard/server.ts` to v4-native Rust:
//!
//! | Method | Path | v3 source | Returns |
//! |--------|------|-----------|---------|
//! | GET | `/api/settings/oauth` | server.ts:4243 | `{ ok, accounts:[{alias,provider,email,config_dir,auth_status,description}] }` |
//! | GET | `/api/settings/obsidian` | server.ts:4032 | `{ ok, vault_root, configured, exists, config_path }` |
//! | GET | `/api/settings/config/{name}` | server.ts:4362 | `{ ok, name, path, content }` (bot_token redacted) |
//! | POST | `/api/settings/obsidian` | server.ts:4053 | `{ ok, vault_root, exists, bootstrapped, created, bootstrap_error, config_path }` |
//! | POST | `/api/settings/telegram` | server.ts:4287 | `{ ok, bot_username, chat_id_set, message }` (writes `[comms.telegram]` in `config.toml`) |
//! | POST | `/api/settings/telegram/test` | server.ts:4340 | `{ ok, bot_username, chat_id, error? }` (sends a test message via the live bridge) |
//!
//! ## Mutation parity notes (Wave 2)
//!
//! - **obsidian** (POST) is a straight parity port: v4 reads + writes the SAME
//!   `evy/obsidian.json` the GET serves, so the file is single-owner-coherent.
//!   File content shape (`vault_root` raw, `_comment` ISO timestamp), the
//!   `bootstrap` default-true vault scaffold, and `mkdir -p` semantics all
//!   mirror v3 byte-for-byte.
//! - **telegram** (POST) is a sanctioned deviation (lead ruling, ORCHESTRATION.md):
//!   v3 wrote `evy-notify.json`, which was DISARMED on 2026-06-09 when v4 took
//!   bot ownership. v4 instead writes the `[comms.telegram]` table of the
//!   daemon's `config.toml` (the section the live bridge is built from). The
//!   write is **restart-required**: the [`crate::telegram::TelegramBridge`]
//!   holds its config immutably behind `Arc<Inner>`, built once at boot and
//!   wired into an already-`Arc`'d `DaemonAppState`, so a live hot-swap is not a
//!   contained change (it would need interior-mutability across the bridge's
//!   poll loop, and the boot-absent case reaches into the `evy` daemon crate to
//!   construct + spawn a bridge — a new subsystem). The TOML write is surgical:
//!   only `bot_token` / `chat_id` inside `[comms.telegram]` change; comments and
//!   every other section are preserved byte-for-byte (no `toml`/`toml_edit`
//!   dependency).
//! - **telegram/test** (POST) is v4-native: it sends a real test message through
//!   the live bridge's own [`notify`](crate::telegram::TelegramBridge::notify)
//!   machinery (the same path behind `POST /api/evy/notify`).
//!
//! ## Deliberately NOT ported here (covered by the `/api/{*rest}` catch-all
//! reverse-proxy, so they keep returning 200 + v3-shape via the v3 Bun
//! dashboard):
//!
//! - **Env/install-coupled reads** — `GET /api/settings/keys`,
//!   `GET /api/settings/secrets`, `GET /api/update/check`. These reflect the v3
//!   Bun process's environment (API-key env vars) and install tree (the running
//!   `VERSION` + git remote tags). v4 under launchd has a bare env and no repo
//!   handle, so serving them natively returns hollow data (every key `env:false`,
//!   `running_version "(unknown)"`); they stay on the v3 reverse-proxy, which v3
//!   owns the data for. (Lead ruling: native where v4 owns the data, proxied
//!   where v3 owns it — same boundary as providers/catalogs in b42b311.)
//! - **Remaining mutations** — `POST /api/settings/secrets/{key}`,
//!   `POST|DELETE /api/providers/profiles`, `POST /api/update/run`. The last is
//!   an explicit non-goal (it mutates the install); the rest write live operator
//!   secrets / provider profiles owned by other route families.
//! - **Master-proxied surfaces** — `profile`, `preferences`, `upstreams`.
//!   `dashboard/V4_BRIDGE.md` marks these "deliberately left on the v3
//!   master — out of scope for v4"; a wholesale port of `components/evy` is a
//!   named non-goal.
//! - `GET /api/settings/install-checks` — subprocess dep-probing that reads
//!   the v3 repo's `lib/dep-manifest.json` (v3-repo-coupled); a clean
//!   follow-up.
//! - `GET /api/update/events` — SSE; its event source is the v3 in-process
//!   update-run bus, which pairs with the out-of-scope `update/run`.
//!
//! Handlers are thin wrappers over pure, dir/string-parameterized core fns so
//! the logic is unit-testable against temp dirs without touching process-global
//! env — same shape as `teams_http.rs` / `accounts_http.rs`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::Path as AxPath;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};

use crate::http::HttpState;
use crate::notification::Notification;

// ── shared path helpers ──────────────────────────────────────────────────

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// `$SUBCTL_CONFIG_DIR` or `~/.config/subctl` — the config root all of these
/// endpoints read from (mirrors `accounts_http`/`teams_http`).
fn config_dir() -> PathBuf {
    std::env::var("SUBCTL_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{}/.config/subctl", home())))
}

fn accounts_conf_path() -> PathBuf {
    std::env::var("SUBCTL_ACCOUNTS_CONF")
        .map(PathBuf::from)
        .unwrap_or_else(|_| config_dir().join("accounts.conf"))
}

/// The daemon's `config.toml` — `$SUBCTL_EVY_CONFIG` (the var the `evy` binary
/// resolves from) or `~/.config/subctl/v4/config.toml` (the install default the
/// chat-tui + installer reference). The telegram-write handler mutates the
/// `[comms.telegram]` table of this exact file so the live bridge picks the
/// change up on its next boot.
fn evy_config_path() -> PathBuf {
    std::env::var("SUBCTL_EVY_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| config_dir().join("v4").join("config.toml"))
}

/// Expand a single leading `~` to `home` (matches v3's `.replace(/^~/, HOME)` —
/// only the leading tilde, nothing mid-string).
fn expand_tilde(s: &str, home: &str) -> String {
    if let Some(rest) = s.strip_prefix('~') {
        format!("{home}{rest}")
    } else {
        s.to_string()
    }
}

fn ok_json(v: Value) -> Response {
    Json(v).into_response()
}

fn err_json(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "ok": false, "error": msg.into() }))).into_response()
}

// ── /api/settings/oauth (server.ts:4243) ──────────────────────────────────

/// JS-truthiness for the codex `auth.json` token check (`j.tokens || j.access_token`):
/// `null`/`false`/`0`/`""`/absent → false; objects/arrays/non-empty strings/true/
/// non-zero numbers → true.
fn is_truthy(opt: Option<&Value>) -> bool {
    match opt {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// Auth detection specific to `/api/settings/oauth` (server.ts:4259-4269):
/// claude `.credentials.json` on disk → ready; else codex `auth.json` with a
/// truthy `tokens` or `access_token` → ready; else not_authenticated.
///
/// NOTE: deliberately distinct from `dashboard_state::auth_status` (which also
/// honours a `projects/` subdir and `tokens.id_token`) — this matches the
/// oauth endpoint's narrower logic byte-for-byte.
fn detect_oauth_auth(config_dir_expanded: &Path) -> &'static str {
    if config_dir_expanded.join(".credentials.json").exists() {
        return "ready";
    }
    if let Ok(text) = std::fs::read_to_string(config_dir_expanded.join("auth.json")) {
        if let Ok(j) = serde_json::from_str::<Value>(&text) {
            if is_truthy(j.get("tokens")) || is_truthy(j.get("access_token")) {
                return "ready";
            }
        }
    }
    "not_authenticated"
}

/// Build the `/api/settings/oauth` body from a raw `accounts.conf` string.
/// Pipe-delimited `alias | provider | email | config_dir | description`;
/// blank lines and `#` comments are skipped; rows with < 4 fields are dropped.
/// `config_dir` is echoed back **raw** (un-expanded) to match v3.
fn build_oauth_value(accounts_raw: &str, home: &str) -> Value {
    let mut accounts: Vec<Value> = Vec::new();
    for line in accounts_raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = trimmed.split('|').map(str::trim).collect();
        if parts.len() < 4 {
            continue;
        }
        let alias = parts[0];
        let provider = parts[1];
        let email = parts[2];
        let config_dir_raw = parts[3];
        let description = parts.get(4).copied().unwrap_or("");
        let expanded = expand_tilde(config_dir_raw, home);
        let auth_status = detect_oauth_auth(Path::new(&expanded));
        accounts.push(json!({
            "alias": alias,
            "provider": provider,
            "email": email,
            "config_dir": config_dir_raw,
            "auth_status": auth_status,
            "description": description,
        }));
    }
    json!({ "ok": true, "accounts": accounts })
}

// ── /api/settings/obsidian GET (server.ts:4032) ───────────────────────────

/// Build the `/api/settings/obsidian` (GET) body: configured vault root +
/// existence flag. Default root is `~/Documents/Obsidian Vault`; a configured
/// `evy/obsidian.json` `{ vault_root }` overrides it (leading `~` expanded).
fn build_obsidian_value(config_dir: &Path, home: &str) -> Value {
    let cfg_path = config_dir.join("evy").join("obsidian.json");
    let mut vault_root = format!("{home}/Documents/Obsidian Vault");
    let mut configured = false;
    if cfg_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&cfg_path) {
            if let Ok(j) = serde_json::from_str::<Value>(&text) {
                if let Some(vr) = j.get("vault_root").and_then(Value::as_str) {
                    if !vr.is_empty() {
                        vault_root = expand_tilde(vr, home);
                        configured = true;
                    }
                }
            }
        }
    }
    json!({
        "ok": true,
        "vault_root": vault_root,
        "configured": configured,
        "exists": Path::new(&vault_root).exists(),
        "config_path": cfg_path.to_string_lossy(),
    })
}

// ── /api/settings/config/{name} (server.ts:4362) ──────────────────────────

/// Redact any `"bot_token": "<value>"` value to `<redacted>` (port of v3's
/// `/("bot_token"\s*:\s*")[^"]+(")/g` → `$1<redacted>$2`). Handles arbitrary
/// inter-token whitespace and multiple occurrences; an empty `""` value is left
/// untouched (the v3 regex requires `[^"]+`, ≥1 char).
fn redact_bot_token(input: &str) -> String {
    const NEEDLE: &str = "\"bot_token\"";
    let is_ws = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r');
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(i) = rest.find(NEEDLE) {
        // Emit up to and including the matched `"bot_token"`.
        out.push_str(&rest[..i + NEEDLE.len()]);
        let after = &rest[i + NEEDLE.len()..];
        let bytes = after.as_bytes();
        let mut j = 0;
        while j < bytes.len() && is_ws(bytes[j]) {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b':' {
            j += 1;
            while j < bytes.len() && is_ws(bytes[j]) {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'"' {
                let open = j; // index of the opening quote
                j += 1;
                let val_start = j;
                while j < bytes.len() && bytes[j] != b'"' {
                    j += 1;
                }
                if j < bytes.len() && j > val_start {
                    // `…<ws>:<ws>"` then redaction then the closing quote.
                    out.push_str(&after[..=open]);
                    out.push_str("<redacted>");
                    out.push('"');
                    rest = &after[j + 1..];
                    continue;
                }
            }
        }
        // No `: "<value>"` followed the key — keep scanning past the needle.
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Build the `/api/settings/config/{name}` body. `name` ∈ {policy, providers,
/// notify}; unknown → 400, missing file → 404, read error → 500. `bot_token`
/// values in the returned `content` are redacted.
fn build_config_get(
    config_dir: &Path,
    name: &str,
) -> std::result::Result<Value, (StatusCode, String)> {
    let path = match name {
        "policy" => config_dir.join("evy").join("policy.json"),
        "providers" => config_dir.join("evy").join("providers.json"),
        "notify" => config_dir.join("evy-notify.json"),
        _ => return Err((StatusCode::BAD_REQUEST, "unknown config".into())),
    };
    if !path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("{name} not found at {}", path.to_string_lossy()),
        ));
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(json!({
            "ok": true,
            "name": name,
            "path": path.to_string_lossy(),
            "content": redact_bot_token(&content),
        })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

// ── handlers ──────────────────────────────────────────────────────────────

/// `GET /api/settings/oauth` → per-account auth status from `accounts.conf`.
pub(crate) async fn oauth_handler() -> Response {
    let raw = std::fs::read_to_string(accounts_conf_path()).unwrap_or_default();
    ok_json(build_oauth_value(&raw, &home()))
}

/// `GET /api/settings/obsidian` → configured vault root + existence flag.
pub(crate) async fn obsidian_get_handler() -> Response {
    ok_json(build_obsidian_value(&config_dir(), &home()))
}

/// `GET /api/settings/config/{name}` → raw config file content (bot_token redacted).
pub(crate) async fn config_get_handler(AxPath(name): AxPath<String>) -> Response {
    match build_config_get(&config_dir(), &name) {
        Ok(v) => ok_json(v),
        Err((status, msg)) => err_json(status, msg),
    }
}

// ── /api/settings/obsidian POST (server.ts:4053) ───────────────────────────

/// `master/welcome.md` seed text — byte-for-byte port of server.ts:4084 so the
/// scaffolded vault reads identically whether v3 or v4 created it.
const WELCOME_MD: &str = "# subctl evy vault\n\n\
This vault is the master daemon's long-term memory store (tier 3).\n\n\
Per-project notes land here as the master records decisions, drafts \
specs, and tracks dev-team progress. Each spawned dev team gets a \
subdirectory with its own `decisions.md`.\n\n\
Created automatically by the subctl dashboard. Open this folder in \
Obsidian to browse. Safe to rename, restructure, or add your own \
notes — the master writes append-only and never deletes.\n";

/// Core of `POST /api/settings/obsidian`: validate + persist the vault-root
/// config (`evy/obsidian.json`) and optionally scaffold the default vault.
/// Pure over `config_dir` / `home` / `now_iso` so it's testable against temp
/// dirs. Mirrors server.ts:4053 (file content shape, bootstrap default-true,
/// `mkdir -p` semantics, response fields).
fn obsidian_write_core(
    config_dir: &Path,
    home: &str,
    body: &Value,
    now_iso: &str,
) -> std::result::Result<Value, (StatusCode, String)> {
    let path = body
        .get("vault_root")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if path.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "vault_root required".into()));
    }
    // Default true — only an explicit `{bootstrap:false}` skips scaffolding
    // (v3: `body.bootstrap !== false`).
    let bootstrap = body.get("bootstrap") != Some(&Value::Bool(false));
    let expanded = expand_tilde(path, home);
    let evy_dir = config_dir.join("evy");
    let cfg_path = evy_dir.join("obsidian.json");

    std::fs::create_dir_all(&evy_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Match v3's file content shape exactly: `vault_root` holds the RAW
    // (un-expanded) submitted path, then a `_comment` ISO stamp. Hand-format to
    // pin key order + 2-space indent regardless of serde map ordering.
    let content = format!(
        "{{\n  \"vault_root\": {},\n  \"_comment\": {}\n}}",
        serde_json::to_string(path).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(&format!("set via dashboard {now_iso}"))
            .unwrap_or_else(|_| "\"\"".into()),
    );
    std::fs::write(&cfg_path, content)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut created: Vec<String> = Vec::new();
    let mut bootstrap_err: Option<String> = None;
    if bootstrap {
        if let Err(e) = scaffold_vault(&expanded, &mut created) {
            bootstrap_err = Some(e);
        }
    }

    Ok(json!({
        "ok": true,
        "vault_root": expanded,
        "exists": Path::new(&expanded).exists(),
        "bootstrapped": bootstrap,
        "created": created,
        "bootstrap_error": bootstrap_err,
        "config_path": cfg_path.to_string_lossy(),
    }))
}

/// Create the vault root + the default `master/.obsidian` sub-vault stub the way
/// server.ts:4079 does, appending created paths to `created`. Returns the OS
/// error string on failure (the caller surfaces it as a non-fatal
/// `bootstrap_error`, never aborting the config write).
fn scaffold_vault(expanded: &str, created: &mut Vec<String>) -> std::result::Result<(), String> {
    std::fs::create_dir_all(expanded).map_err(|e| e.to_string())?;
    let master = Path::new(expanded).join("master");
    if !master.exists() {
        let dot_obsidian = master.join(".obsidian");
        std::fs::create_dir_all(&dot_obsidian).map_err(|e| e.to_string())?;
        let welcome = master.join("welcome.md");
        std::fs::write(&welcome, WELCOME_MD).map_err(|e| e.to_string())?;
        created.push(dot_obsidian.to_string_lossy().into_owned());
        created.push(welcome.to_string_lossy().into_owned());
    }
    Ok(())
}

/// `POST /api/settings/obsidian` → persist the vault root + scaffold the vault.
pub(crate) async fn obsidian_write_handler(bytes: Bytes) -> Response {
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return err_json(StatusCode::BAD_REQUEST, "invalid JSON"),
    };
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    match obsidian_write_core(&config_dir(), &home(), &body, &now) {
        Ok(v) => ok_json(v),
        Err((status, msg)) => err_json(status, msg),
    }
}

// ── /api/settings/telegram POST (server.ts:4287) ───────────────────────────
//
// v3 wrote `evy-notify.json` (disarmed 2026-06-09). v4 writes the
// `[comms.telegram]` table of the daemon's `config.toml` instead — surgically,
// so operator comments + every other section survive byte-for-byte without a
// `toml`/`toml_edit` dependency. Restart-required (see module docs).

const TELEGRAM_HEADER: &str = "[comms.telegram]";

/// Index of the next TOML table / array-of-tables header at or after
/// `body_start` (first non-blank char `[`), or `lines.len()` if none follows.
fn section_body_end<S: AsRef<str>>(lines: &[S], body_start: usize) -> usize {
    lines
        .iter()
        .enumerate()
        .skip(body_start)
        .find(|(_, l)| l.as_ref().trim_start().starts_with('['))
        .map_or(lines.len(), |(i, _)| i)
}

/// True if `line` assigns `key` (optional indent, the bare `key`, then `=`).
fn is_key_line(line: &str, key: &str) -> bool {
    line.trim_start()
        .strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// The trimmed value text to the right of `=`, if `line` assigns `key`.
fn key_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    if !is_key_line(line, key) {
        return None;
    }
    line.split_once('=').map(|(_, v)| v.trim())
}

/// Render a value as a TOML basic string (escaping `\` and `"`).
fn toml_basic_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Strip a TOML basic-string value to its contents (unescaping `\\` and `\"`).
/// Empty input → `None`; a non-quoted token is returned as-is.
fn parse_toml_string(v: &str) -> Option<String> {
    let bytes = v.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        let inner = &v[1..v.len() - 1];
        Some(inner.replace("\\\"", "\"").replace("\\\\", "\\"))
    } else if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// Read the current `bot_token` / `chat_id` from the `[comms.telegram]` table
/// (both `None` when the section / key is absent) — feeds v3's
/// merge-with-current semantics.
fn parse_telegram_section(toml_text: &str) -> (Option<String>, Option<i64>) {
    let lines: Vec<&str> = toml_text.split('\n').collect();
    let Some(hidx) = lines.iter().position(|l| l.trim() == TELEGRAM_HEADER) else {
        return (None, None);
    };
    let body_start = hidx + 1;
    let body_end = section_body_end(&lines, body_start);
    let mut token = None;
    let mut chat = None;
    for l in &lines[body_start..body_end] {
        if let Some(v) = key_value(l, "bot_token") {
            token = parse_toml_string(v);
        } else if let Some(v) = key_value(l, "chat_id") {
            chat = v
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<i64>().ok());
        }
    }
    (token, chat)
}

/// Surgically set `bot_token` / `chat_id` inside `[comms.telegram]`, preserving
/// every other byte (comments, key order, sibling sections). `None` leaves a
/// key untouched; a missing key (or the whole section) is created.
fn write_telegram_section(original: &str, bot_token: Option<&str>, chat_id: Option<i64>) -> String {
    let mut lines: Vec<String> = original.split('\n').map(String::from).collect();
    let Some(hidx) = lines.iter().position(|l| l.trim() == TELEGRAM_HEADER) else {
        return append_telegram_section(original, bot_token, chat_id);
    };
    let body_start = hidx + 1;
    if let Some(tok) = bot_token {
        let body_end = section_body_end(&lines, body_start);
        set_key(
            &mut lines,
            body_start,
            body_end,
            "bot_token",
            &toml_basic_string(tok),
        );
    }
    if let Some(cid) = chat_id {
        let body_end = section_body_end(&lines, body_start);
        set_key(
            &mut lines,
            body_start,
            body_end,
            "chat_id",
            &cid.to_string(),
        );
    }
    lines.join("\n")
}

/// Replace `key`'s value in `lines[body_start..body_end]` (preserving indent),
/// or insert `key = value_repr` after the section's last non-blank line.
fn set_key(
    lines: &mut Vec<String>,
    body_start: usize,
    body_end: usize,
    key: &str,
    value_repr: &str,
) {
    for line in lines.iter_mut().take(body_end).skip(body_start) {
        if is_key_line(line, key) {
            let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
            *line = format!("{indent}{key} = {value_repr}");
            return;
        }
    }
    let mut at = body_end;
    while at > body_start && lines[at - 1].trim().is_empty() {
        at -= 1;
    }
    lines.insert(at, format!("{key} = {value_repr}"));
}

/// Append a fresh `[comms.telegram]` table to a config that lacks one.
fn append_telegram_section(
    original: &str,
    bot_token: Option<&str>,
    chat_id: Option<i64>,
) -> String {
    let mut out = String::from(original.trim_end_matches('\n'));
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(TELEGRAM_HEADER);
    out.push('\n');
    if let Some(tok) = bot_token {
        out.push_str(&format!("bot_token = {}\n", toml_basic_string(tok)));
    }
    if let Some(cid) = chat_id {
        out.push_str(&format!("chat_id = {cid}\n"));
    }
    out
}

/// Result of merging a telegram-write request against the current config text.
struct TelegramMerge {
    /// Token after merge (always present on success — it's required).
    bot_token: String,
    /// New `config.toml` text to persist.
    new_toml: String,
    /// Whether a chat_id is set post-merge (provided now or already present).
    chat_id_set: bool,
}

/// Manual `Debug` — `bot_token` (and the `config.toml` text, which embeds it)
/// must never reach a log line or a test panic message. Mirrors the redaction
/// the [`crate::telegram::TelegramConfig`] / config-crate structs enforce.
impl std::fmt::Debug for TelegramMerge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramMerge")
            .field("bot_token", &"[redacted]")
            .field("new_toml_len", &self.new_toml.len())
            .field("chat_id_set", &self.chat_id_set)
            .finish()
    }
}

/// Apply v3's merge-with-current semantics (absent request fields keep current
/// values) to `current_toml`. Pure: no network, no fs. `Err` on the "no token
/// at all" guard (v3) and the v4-only "non-integer chat_id" guard (the typed
/// `[comms.telegram].chat_id` is an `i64`, unlike v3's string).
fn merge_telegram_write(
    current_toml: &str,
    req_bot_token: Option<&str>,
    req_chat_id: Option<&str>,
) -> std::result::Result<TelegramMerge, (StatusCode, String)> {
    let (cur_token, cur_chat) = parse_telegram_section(current_toml);

    let new_token = req_bot_token.map(str::trim).filter(|s| !s.is_empty());
    let new_chat_raw = req_chat_id.map(str::trim).filter(|s| !s.is_empty());

    let new_chat: Option<i64> = match new_chat_raw {
        Some(s) => match s.parse::<i64>() {
            Ok(n) => Some(n),
            Err(_) => return Err((StatusCode::BAD_REQUEST, "chat_id must be an integer".into())),
        },
        None => None,
    };

    let merged_token = new_token
        .map(str::to_string)
        .or(cur_token)
        .filter(|s| !s.is_empty());
    let Some(merged_token) = merged_token else {
        return Err((
            StatusCode::BAD_REQUEST,
            "bot_token missing — provide one or set previously".into(),
        ));
    };

    // Write the validated token always; write chat_id only when freshly
    // provided (`None` leaves any existing chat_id untouched).
    let new_toml = write_telegram_section(current_toml, Some(&merged_token), new_chat);
    let chat_id_set = new_chat.is_some() || cur_chat.is_some();

    Ok(TelegramMerge {
        bot_token: merged_token,
        new_toml,
        chat_id_set,
    })
}

/// Validate a bot token against Telegram `getMe`, returning the bot username
/// (`None` if Telegram omits it). Mirrors v3: API `ok:false` → 400
/// `getMe failed: <description>`; transport/decode failure → 500. Hard-coded to
/// `api.telegram.org`; the 4s timeout matches `AbortSignal.timeout(4000)`.
async fn telegram_get_me(token: &str) -> std::result::Result<Option<String>, (StatusCode, String)> {
    let url = format!("https://api.telegram.org/bot{token}/getMe");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let resp = client
        .get(&url)
        .send()
        .await
        // .without_url() strips the token-bearing URL from the error Display.
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.without_url().to_string()))?;
    let j: Value = resp.json().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            e.without_url().to_string(),
        )
    })?;
    if j.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(j.pointer("/result/username")
            .and_then(Value::as_str)
            .map(String::from))
    } else {
        let desc = j
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        Err((StatusCode::BAD_REQUEST, format!("getMe failed: {desc}")))
    }
}

/// `POST /api/settings/telegram` → merge `bot_token`/`chat_id` into the
/// `[comms.telegram]` table of `config.toml` (restart-required), after
/// validating the resulting token via `getMe`. A bad token never lands on disk.
pub(crate) async fn telegram_write_handler(bytes: Bytes) -> Response {
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return err_json(StatusCode::BAD_REQUEST, "invalid JSON body"),
    };
    let req_token = body.get("bot_token").and_then(Value::as_str);
    let req_chat = body.get("chat_id").and_then(Value::as_str);

    let cfg_path = evy_config_path();
    let current = std::fs::read_to_string(&cfg_path).unwrap_or_default();

    let merge = match merge_telegram_write(&current, req_token, req_chat) {
        Ok(m) => m,
        Err((status, msg)) => return err_json(status, msg),
    };

    let bot_username = match telegram_get_me(&merge.bot_token).await {
        Ok(u) => u,
        Err((status, msg)) => return err_json(status, msg),
    };

    if let Some(parent) = cfg_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    }
    if let Err(e) = std::fs::write(&cfg_path, &merge.new_toml) {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    ok_json(json!({
        "ok": true,
        "bot_username": bot_username,
        "chat_id_set": merge.chat_id_set,
        "message": "saved to config.toml — restart the evy daemon to apply to the live bridge",
    }))
}

/// `POST /api/settings/telegram/test` → send a live test message through the
/// daemon's Telegram bridge (the same path as `POST /api/evy/notify`). v3 shape:
/// server.ts:4340 (`{ ok, bot_username, chat_id, error? }`); `bot_username` is
/// `null` since v4 exercises `sendMessage`, not `getMe`.
pub(crate) async fn telegram_test_handler(State(state): State<HttpState>) -> Response {
    let Some(bridge) = state.app.telegram_bridge() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ok": false, "error": "no telegram bridge configured" })),
        )
            .into_response();
    };
    let chat_id = bridge.chat_id();
    match bridge
        .notify(Notification::Note {
            text: "✅ subctl v4 — Telegram bridge test message.".to_string(),
        })
        .await
    {
        Ok(()) => ok_json(json!({
            "ok": true,
            "bot_username": Value::Null,
            "chat_id": chat_id,
        })),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "ok": false,
                "bot_username": Value::Null,
                "chat_id": chat_id,
                "error": e.to_string(),
            })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmpdir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "evy-prefs-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // ── oauth ──

    #[test]
    fn oauth_parses_pipe_rows_and_keeps_raw_config_dir() {
        let home = tmpdir();
        let home_str = home.to_string_lossy().to_string();
        // claude account: ready via .credentials.json
        let claude_dir = home.join(".claude-x");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(claude_dir.join(".credentials.json"), "{}").unwrap();
        // codex account: ready via auth.json tokens
        let codex_dir = home.join(".codex-x");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("auth.json"),
            r#"{"tokens":{"access_token":"t"}}"#,
        )
        .unwrap();

        let raw = format!(
            "# comment\n\
             claude-x | claude | a@b.com | ~/.claude-x | Daily driver\n\
             codex-x  | openai-codex | c@d.com | {}/.codex-x | Codex\n\
             missing  | claude | e@f.com | ~/.nope | none\n\
             badrow | only | three\n",
            home_str
        );
        let v = build_oauth_value(&raw, &home_str);
        let accts = v["accounts"].as_array().unwrap();
        assert_eq!(accts.len(), 3, "comment + short row dropped");
        assert_eq!(accts[0]["alias"], json!("claude-x"));
        assert_eq!(accts[0]["config_dir"], json!("~/.claude-x")); // raw, not expanded
        assert_eq!(accts[0]["auth_status"], json!("ready"));
        assert_eq!(accts[0]["description"], json!("Daily driver"));
        assert_eq!(accts[1]["auth_status"], json!("ready"));
        assert_eq!(accts[2]["auth_status"], json!("not_authenticated"));
    }

    #[test]
    fn oauth_truthiness_rejects_empty_tokens() {
        let home = tmpdir();
        let home_str = home.to_string_lossy().to_string();
        let dir = home.join(".codex-empty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            r#"{"tokens":null,"access_token":""}"#,
        )
        .unwrap();
        let raw = format!("e | openai-codex | x@y.com | {}/.codex-empty |\n", home_str);
        let v = build_oauth_value(&raw, &home_str);
        assert_eq!(v["accounts"][0]["auth_status"], json!("not_authenticated"));
    }

    // ── obsidian ──

    #[test]
    fn obsidian_default_when_unconfigured() {
        let dir = tmpdir();
        let v = build_obsidian_value(&dir, "/Users/test");
        assert_eq!(v["ok"], json!(true));
        assert_eq!(
            v["vault_root"],
            json!("/Users/test/Documents/Obsidian Vault")
        );
        assert_eq!(v["configured"], json!(false));
        assert_eq!(v["exists"], json!(false));
    }

    #[test]
    fn obsidian_configured_expands_tilde() {
        let dir = tmpdir();
        std::fs::create_dir_all(dir.join("evy")).unwrap();
        std::fs::write(
            dir.join("evy").join("obsidian.json"),
            r#"{"vault_root":"~/MyVault"}"#,
        )
        .unwrap();
        let v = build_obsidian_value(&dir, "/Users/test");
        assert_eq!(v["vault_root"], json!("/Users/test/MyVault"));
        assert_eq!(v["configured"], json!(true));
    }

    // ── config/{name} ──

    #[test]
    fn config_unknown_is_400() {
        let dir = tmpdir();
        let err = build_config_get(&dir, "bogus").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn config_missing_is_404() {
        let dir = tmpdir();
        let err = build_config_get(&dir, "providers").unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn config_notify_redacts_bot_token() {
        let dir = tmpdir();
        std::fs::write(
            dir.join("evy-notify.json"),
            "{\n  \"bot_token\": \"123:ABCsecret\",\n  \"chat_id\": \"42\"\n}",
        )
        .unwrap();
        let v = build_config_get(&dir, "notify").unwrap();
        let content = v["content"].as_str().unwrap();
        assert!(!content.contains("ABCsecret"));
        assert!(content.contains("\"bot_token\": \"<redacted>\""));
        assert!(content.contains("\"chat_id\": \"42\""));
    }

    // ── redaction edge cases ──

    #[test]
    fn redact_handles_multiple_and_empty_and_whitespace() {
        let input = r#"{"bot_token":"aaa","x":{"bot_token"  :  "bbb"},"bot_token":""}"#;
        let out = redact_bot_token(input);
        assert!(!out.contains("aaa"));
        assert!(!out.contains("bbb"));
        assert_eq!(out.matches("<redacted>").count(), 2);
        // empty value is preserved (regex needs ≥1 char)
        assert!(out.contains(r#""bot_token":"""#));
    }

    #[test]
    fn redact_noop_without_token() {
        let input = r#"{"hello":"world"}"#;
        assert_eq!(redact_bot_token(input), input);
    }

    // ── obsidian write (POST) ──

    #[test]
    fn obsidian_write_persists_raw_path_and_scaffolds() {
        let cfg = tmpdir();
        let home = tmpdir();
        let home_str = home.to_string_lossy().to_string();
        let body = json!({ "vault_root": "~/MyVault" });
        let v = obsidian_write_core(&cfg, &home_str, &body, "2026-06-10T00:00:00.000Z").unwrap();

        // Response: vault_root is EXPANDED, bootstrapped true, 2 created entries.
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["vault_root"], json!(format!("{home_str}/MyVault")));
        assert_eq!(v["bootstrapped"], json!(true));
        assert_eq!(v["bootstrap_error"], Value::Null);
        assert_eq!(v["exists"], json!(true));
        let created = v["created"].as_array().unwrap();
        assert_eq!(created.len(), 2);
        assert!(created[0].as_str().unwrap().ends_with("/master/.obsidian"));
        assert!(created[1].as_str().unwrap().ends_with("/master/welcome.md"));

        // File content shape: vault_root holds the RAW (un-expanded) path + an
        // ISO `_comment`. Key order pinned by hand-formatting.
        let written = std::fs::read_to_string(cfg.join("evy").join("obsidian.json")).unwrap();
        let j: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(j["vault_root"], json!("~/MyVault"));
        assert_eq!(
            j["_comment"],
            json!("set via dashboard 2026-06-10T00:00:00.000Z")
        );
        assert!(written.find("vault_root").unwrap() < written.find("_comment").unwrap());

        // Scaffold landed with the exact welcome text.
        let welcome = home.join("MyVault").join("master").join("welcome.md");
        assert_eq!(std::fs::read_to_string(&welcome).unwrap(), WELCOME_MD);
        assert!(home
            .join("MyVault")
            .join("master")
            .join(".obsidian")
            .is_dir());
    }

    #[test]
    fn obsidian_write_empty_vault_root_is_400() {
        let cfg = tmpdir();
        let err = obsidian_write_core(&cfg, "/h", &json!({ "vault_root": "  " }), "t").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1, "vault_root required");
    }

    #[test]
    fn obsidian_write_bootstrap_false_skips_scaffold() {
        let cfg = tmpdir();
        let home = tmpdir();
        let home_str = home.to_string_lossy().to_string();
        let body = json!({ "vault_root": "~/V2", "bootstrap": false });
        let v = obsidian_write_core(&cfg, &home_str, &body, "t").unwrap();
        assert_eq!(v["bootstrapped"], json!(false));
        assert_eq!(v["created"].as_array().unwrap().len(), 0);
        // config still written, but no vault scaffold.
        assert!(cfg.join("evy").join("obsidian.json").exists());
        assert!(!home.join("V2").join("master").exists());
    }

    // ── telegram TOML surgical edit ──

    const SAMPLE_TOML: &str = "# top comment — keep me\n\
[scheduler]\n\
db_path = \"/tmp/x.db\"\n\
\n\
[comms.http]\n\
port = 8797\n\
allow_origins = []\n\
\n\
[comms.telegram]\n\
bot_token = \"OLD:tok\"\n\
chat_id = 111\n";

    #[test]
    fn telegram_parse_reads_current_section() {
        let (tok, chat) = parse_telegram_section(SAMPLE_TOML);
        assert_eq!(tok.as_deref(), Some("OLD:tok"));
        assert_eq!(chat, Some(111));
        // absent section → both None.
        assert_eq!(
            parse_telegram_section("[scheduler]\ndb_path = \"x\"\n"),
            (None, None)
        );
    }

    #[test]
    fn telegram_write_updates_both_preserving_everything_else() {
        let out = write_telegram_section(SAMPLE_TOML, Some("NEW:tok"), Some(222));
        // Updated values present, old gone.
        assert!(out.contains("bot_token = \"NEW:tok\""));
        assert!(out.contains("chat_id = 222"));
        assert!(!out.contains("OLD:tok"));
        assert!(!out.contains("chat_id = 111"));
        // Every other byte preserved: comment + sibling sections + their keys.
        assert!(out.contains("# top comment — keep me"));
        assert!(out.contains("[scheduler]"));
        assert!(out.contains("db_path = \"/tmp/x.db\""));
        assert!(out.contains("[comms.http]"));
        assert!(out.contains("port = 8797"));
        // No lines added or dropped — only two values changed in place.
        assert_eq!(out.lines().count(), SAMPLE_TOML.lines().count());
    }

    #[test]
    fn telegram_write_none_leaves_key_untouched() {
        // chat_id = None must leave the existing `chat_id = 111` intact.
        let out = write_telegram_section(SAMPLE_TOML, Some("NEW:tok"), None);
        assert!(out.contains("bot_token = \"NEW:tok\""));
        assert!(out.contains("chat_id = 111"));
    }

    #[test]
    fn telegram_write_escapes_quotes_in_token() {
        let out = write_telegram_section(SAMPLE_TOML, Some(r#"a"b\c"#), None);
        assert!(out.contains(r#"bot_token = "a\"b\\c""#));
        // Round-trips back through the parser to the original value.
        assert_eq!(parse_telegram_section(&out).0.as_deref(), Some(r#"a"b\c"#));
    }

    #[test]
    fn telegram_write_appends_section_when_absent() {
        let base = "[scheduler]\ndb_path = \"x\"\n";
        let out = write_telegram_section(base, Some("T:tok"), Some(42));
        assert!(out.contains("[scheduler]"));
        assert!(out.contains("[comms.telegram]"));
        // Re-parsing the appended section yields the written values.
        let (tok, chat) = parse_telegram_section(&out);
        assert_eq!(tok.as_deref(), Some("T:tok"));
        assert_eq!(chat, Some(42));
    }

    #[test]
    fn telegram_write_inserts_missing_key_into_existing_section() {
        // Section present but no chat_id — must be inserted, not appended after EOF.
        let base = "[comms.telegram]\nbot_token = \"T\"\n\n[other]\nx = 1\n";
        let out = write_telegram_section(base, None, Some(7));
        let (_, chat) = parse_telegram_section(&out);
        assert_eq!(chat, Some(7));
        // The inserted key stays inside the telegram section (before `[other]`).
        assert!(out.find("chat_id = 7").unwrap() < out.find("[other]").unwrap());
    }

    // ── telegram merge semantics (server.ts:4287 parity) ──

    #[test]
    fn telegram_merge_keeps_current_when_field_absent() {
        // Only a new token; chat_id absent → keeps current 111, chat_id_set true.
        let m = merge_telegram_write(SAMPLE_TOML, Some("NEW:tok"), None).unwrap();
        assert_eq!(m.bot_token, "NEW:tok");
        assert!(m.chat_id_set);
        assert!(m.new_toml.contains("chat_id = 111"));
        assert!(m.new_toml.contains("bot_token = \"NEW:tok\""));
    }

    #[test]
    fn telegram_merge_uses_provided_chat_id() {
        let m = merge_telegram_write(SAMPLE_TOML, None, Some("999")).unwrap();
        // token falls back to current; chat_id updated.
        assert_eq!(m.bot_token, "OLD:tok");
        assert!(m.new_toml.contains("chat_id = 999"));
    }

    #[test]
    fn telegram_merge_missing_token_is_400() {
        // Empty config + no token provided → 400 (v3 "bot_token missing" guard).
        let err = merge_telegram_write("", None, Some("5")).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("bot_token missing"));
    }

    #[test]
    fn telegram_merge_non_integer_chat_id_is_400() {
        let err = merge_telegram_write(SAMPLE_TOML, None, Some("not-a-number")).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1, "chat_id must be an integer");
    }

    #[test]
    fn telegram_merge_blank_request_fields_keep_current() {
        // Whitespace-only request fields are treated as absent (v3 `.trim()`).
        let m = merge_telegram_write(SAMPLE_TOML, Some("   "), Some("  ")).unwrap();
        assert_eq!(m.bot_token, "OLD:tok");
        assert!(m.new_toml.contains("chat_id = 111"));
    }
}
