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
//! - **Mutations** — `POST /api/settings/{obsidian,secrets/{key},telegram,
//!   telegram/test}`, `POST|DELETE /api/providers/profiles`,
//!   `POST /api/update/run`. The last is an explicit non-goal (it mutates the
//!   install); the rest write live operator config / restart the master /
//!   call the Telegram API.
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

use axum::extract::Path as AxPath;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

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
}
