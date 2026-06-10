//! Cutover — native settings / auth-status / update-status read surface.
//!
//! Ports the **dashboard-owned, read-only** members of the v3
//! settings/preferences/profile/auth/update/upstreams family from
//! `dashboard/server.ts` to v4-native Rust:
//!
//! | Method | Path | v3 source | Returns |
//! |--------|------|-----------|---------|
//! | GET | `/api/settings/keys` | server.ts:4125 | `{ ok, keys:[{name,ok,env,secrets_json,length,purpose}], note }` |
//! | GET | `/api/settings/secrets` | server.ts:4166 | `{ ok, secrets:[{key,isSet,envOverride,lastModified}], priority }` |
//! | GET | `/api/settings/oauth` | server.ts:4243 | `{ ok, accounts:[{alias,provider,email,config_dir,auth_status,description}] }` |
//! | GET | `/api/settings/obsidian` | server.ts:4032 | `{ ok, vault_root, configured, exists, config_path }` |
//! | GET | `/api/settings/config/{name}` | server.ts:4362 | `{ ok, name, path, content }` (bot_token redacted) |
//! | GET | `/api/update/check` | server.ts:5100 | `{ running_version, latest_tag, has_update, channel, stale, [stub] }` |
//!
//! ## Deliberately NOT ported here (covered by the `/api/{*rest}` catch-all
//! reverse-proxy, so they keep returning 200 + v3-shape via the v3 Bun
//! dashboard):
//!
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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axum::extract::{Path as AxPath, Query};
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

fn secrets_path() -> PathBuf {
    config_dir().join("secrets.json")
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

/// `secrets.json` mtime as ISO-8601 millis (`2026-05-20T03:11:07.700Z`), or
/// `None` when the file is absent — matches v3 `new Date(mtimeMs).toISOString()`.
fn mtime_iso(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let dt: chrono::DateTime<chrono::Utc> = modified.into();
    Some(dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn ok_json(v: Value) -> Response {
    Json(v).into_response()
}

fn err_json(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "ok": false, "error": msg.into() }))).into_response()
}

// ── secrets.json read layer (v3 components/evy/secrets.ts) ────────────────

/// Known secret keys in declaration order (port of `SECRET_KEYS`,
/// secrets.ts:57). Drives stable "Not set" rows when the file is absent.
const SECRET_KEYS: [&str; 10] = [
    "lmstudio_api_token",
    "brave_api_key",
    "firecrawl_api_key",
    "linear_api_key",
    "context7_api_key",
    "tinyfish_api_key",
    "openrouter_api_key",
    "cognee_auth_token",
    "memori_api_key",
    // MCP-Expose bearer token; env fallback `SUBCTL_MCP_TOKEN` (no explicit
    // `env_var_for` case — the uppercase default already yields it).
    "subctl_mcp_token",
];

/// Map a `secrets.json` key to its canonical env-var name (port of
/// `envVarFor`, secrets.ts:270). Falls back to upper-casing for unknown keys.
fn env_var_for(key: &str) -> String {
    match key {
        "lmstudio_api_token" => "LMSTUDIO_API_TOKEN".to_string(),
        "brave_api_key" => "BRAVE_API_KEY".to_string(),
        "firecrawl_api_key" => "FIRECRAWL_API_KEY".to_string(),
        "linear_api_key" => "LINEAR_API_KEY".to_string(),
        "context7_api_key" => "CONTEXT7_API_KEY".to_string(),
        "tinyfish_api_key" => "TINYFISH_API_KEY".to_string(),
        "openrouter_api_key" => "OPENROUTER_API_KEY".to_string(),
        "cognee_auth_token" => "COGNEE_AUTH_TOKEN".to_string(),
        "memori_api_key" => "MEMORI_API_KEY".to_string(),
        other => other.to_uppercase(),
    }
}

/// Read the string-valued entries of `secrets.json` (port of
/// `readSecretsFromDisk`). Null / non-string values are dropped; a missing or
/// malformed file yields an empty map (never an error), matching v3.
fn read_secrets_map(path: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(raw) = std::fs::read_to_string(path) else {
        return out;
    };
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&raw) else {
        return out;
    };
    for (k, v) in obj {
        if let Value::String(s) = v {
            out.insert(k, s);
        }
    }
    out
}

/// `loadSecret`-equivalent: the stored value iff it's a non-empty string.
fn secret_value<'a>(map: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    map.get(key).map(String::as_str).filter(|s| !s.is_empty())
}

/// Build the `/api/settings/secrets` body (port of `listSecrets`, secrets.ts:242).
/// `env_lookup` is injected so tests can supply a fake environment.
fn build_secrets_value(secrets_path: &Path, env_lookup: &dyn Fn(&str) -> Option<String>) -> Value {
    let map = read_secrets_map(secrets_path);
    let last_modified = if secrets_path.exists() {
        mtime_iso(secrets_path)
    } else {
        None
    };
    let secrets: Vec<Value> = SECRET_KEYS
        .iter()
        .map(|&key| {
            let is_set = secret_value(&map, key).is_some();
            let env_override = env_lookup(&env_var_for(key)).is_some_and(|v| !v.is_empty());
            json!({
                "key": key,
                "isSet": is_set,
                "envOverride": env_override,
                "lastModified": last_modified,
            })
        })
        .collect();
    json!({
        "ok": true,
        "secrets": secrets,
        "priority": ["env_var", "secrets_json", "absent"],
    })
}

// ── /api/settings/keys (server.ts:4125) ──────────────────────────────────

/// One row of the API-keys status table: env-var name, human purpose, and the
/// optional `secrets.json` key that can also satisfy it.
struct KeyVar {
    name: &'static str,
    purpose: &'static str,
    secret_key: Option<&'static str>,
}

const KEY_VARS: [KeyVar; 12] = [
    KeyVar { name: "ANTHROPIC_API_KEY", purpose: "Anthropic direct API (claude-sonnet, claude-opus)", secret_key: None },
    KeyVar { name: "OPENAI_API_KEY", purpose: "OpenAI direct API (gpt-4, gpt-5, codex)", secret_key: None },
    KeyVar { name: "GEMINI_API_KEY", purpose: "Google Gemini", secret_key: None },
    KeyVar { name: "GROQ_API_KEY", purpose: "Groq inference", secret_key: None },
    KeyVar { name: "OPENROUTER_API_KEY", purpose: "OpenRouter (200+ models via one key)", secret_key: None },
    KeyVar { name: "GITHUB_TOKEN", purpose: "gh CLI fallback (usually authed via gh auth login)", secret_key: None },
    KeyVar { name: "CONTEXT7_API_KEY", purpose: "Context7 — up-to-date library docs (master tool + MCP for dev-team Claude leads)", secret_key: Some("context7_api_key") },
    KeyVar { name: "BRAVE_API_KEY", purpose: "Brave AI Search (web_search master tool — v2.7.2)", secret_key: Some("brave_api_key") },
    KeyVar { name: "FIRECRAWL_API_KEY", purpose: "Firecrawl scraping (web_fetch master tool — v2.7.2)", secret_key: Some("firecrawl_api_key") },
    KeyVar { name: "TINYFISH_API_KEY", purpose: "TinyFish search + fetch (tinyfish_search, tinyfish_fetch master tools — v2.7.16, free tier)", secret_key: Some("tinyfish_api_key") },
    KeyVar { name: "LINEAR_API_KEY", purpose: "Linear API (linear_list_issues, linear_search, linear_create_issue, linear_update_issue master tools — v2.7.2)", secret_key: Some("linear_api_key") },
    KeyVar { name: "LMSTUDIO_API_TOKEN", purpose: "LM Studio API auth (when 'Require API Token' is enabled in LM Studio server settings — v2.7.4)", secret_key: Some("lmstudio_api_token") },
];

const KEYS_NOTE: &str = "v2.7.4 priority: process env beats ~/.config/subctl/secrets.json beats absent. Edit secrets via the Settings → API Tokens panel (chmod 600 file, never echoed back). Env vars set in your shell ~/.zshrc are NOT inherited by launchd services; set them in ~/Library/LaunchAgents/com.subctl.evy.plist EnvironmentVariables.";

/// Build the `/api/settings/keys` body. Reflects BOTH sources (env var +
/// `secrets.json`); never serializes a value, only presence + length.
fn build_keys_value(secrets_path: &Path, env_lookup: &dyn Fn(&str) -> Option<String>) -> Value {
    let map = read_secrets_map(secrets_path);
    let keys: Vec<Value> = KEY_VARS
        .iter()
        .map(|v| {
            let env_val = env_lookup(v.name).filter(|s| !s.is_empty());
            let secrets_val = v.secret_key.and_then(|sk| secret_value(&map, sk));
            let length = env_val
                .as_deref()
                .or(secrets_val)
                .map_or(0, |s| s.chars().count());
            // `secrets_json` is a tri-state: true/false for keys that have a
            // secrets.json counterpart, JSON `null` for the cloud-provider rows.
            let secrets_json = match v.secret_key {
                Some(_) => Value::Bool(secrets_val.is_some()),
                None => Value::Null,
            };
            json!({
                "name": v.name,
                "ok": env_val.is_some() || secrets_val.is_some(),
                "env": env_val.is_some(),
                "secrets_json": secrets_json,
                "length": length,
                "purpose": v.purpose,
            })
        })
        .collect();
    json!({ "ok": true, "keys": keys, "note": KEYS_NOTE })
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

// ── /api/update/check (server.ts:5100) ────────────────────────────────────

const UPDATE_CHECK_TTL_MS: i64 = 5 * 60 * 1000;

/// Cached latest-tag probe so the 5-min dashboard poll doesn't hammer the git
/// remote. Serves the stale value on a flaky remote (matches v3).
struct UpdateCheckCache {
    latest_tag: String,
    fetched_at_ms: i64,
    stale: bool,
}

static UPDATE_CACHE: std::sync::Mutex<Option<UpdateCheckCache>> = std::sync::Mutex::new(None);

/// Compare two semver-ish strings (port of `cmpSemver`, server.ts:411). Strips
/// a leading `v` and any `-`/`+` suffix, then compares the numeric triplet.
fn cmp_semver(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |s: &str| -> Vec<u64> {
        s.trim_start_matches(['v', 'V'])
            .split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let ap = parts(a);
    let bp = parts(b);
    let len = ap.len().max(bp.len());
    for i in 0..len {
        let av = ap.get(i).copied().unwrap_or(0);
        let bv = bp.get(i).copied().unwrap_or(0);
        if av != bv {
            return av.cmp(&bv);
        }
    }
    std::cmp::Ordering::Equal
}

/// Does `tag` start `v?\d+\.\d+\.\d+`? (port of v3's tag filter regex).
fn tag_is_semver(tag: &str) -> bool {
    let body = tag.strip_prefix(['v', 'V']).unwrap_or(tag);
    let mut nums = body.split('.');
    let three_leading_numeric = |s: Option<&str>| {
        s.map(|seg| {
            let digits: String = seg.chars().take_while(char::is_ascii_digit).collect();
            !digits.is_empty()
        })
        .unwrap_or(false)
    };
    // Need at least major.minor.patch each starting with ≥1 digit.
    three_leading_numeric(nums.next())
        && three_leading_numeric(nums.next())
        && three_leading_numeric(nums.next())
}

/// Parse `git ls-remote --tags --refs` stdout, returning the highest
/// semver-shaped tag (port of server.ts:5126-5134), or `None` if there are none.
fn parse_latest_tag(stdout: &str) -> Option<String> {
    const MARK: &str = "refs/tags/";
    let mut tags: Vec<String> = Vec::new();
    for line in stdout.lines() {
        if let Some(idx) = line.find(MARK) {
            let tail = &line[idx + MARK.len()..];
            let tag = tail.split_whitespace().next().unwrap_or("");
            if !tag.is_empty() && tag_is_semver(tag) {
                tags.push(tag.to_string());
            }
        }
    }
    tags.into_iter().max_by(|a, b| cmp_semver(a, b))
}

/// The running subctl version: `$SUBCTL_VERSION`, else `$SUBCTL_REPO_ROOT/VERSION`,
/// else `(unknown)` (v3 reads `<repo>/VERSION`; cross-process the daemon takes
/// the env/file the launcher provides).
fn running_version() -> String {
    if let Ok(v) = std::env::var("SUBCTL_VERSION") {
        let t = v.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Ok(root) = std::env::var("SUBCTL_REPO_ROOT") {
        if let Ok(s) = std::fs::read_to_string(Path::new(&root).join("VERSION")) {
            let t = s.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    "(unknown)".to_string()
}

/// Assemble the `/api/update/check` body. `has_update` iff `latest_tag` >
/// `running` by semver. The `stub` flag is added only on the operator override.
fn build_update_value(
    running: &str,
    latest_tag: Option<&str>,
    stale: bool,
    used_stub: bool,
) -> Value {
    let has_update =
        latest_tag.is_some_and(|l| cmp_semver(l, running) == std::cmp::Ordering::Greater);
    let mut obj = serde_json::Map::new();
    obj.insert("running_version".into(), json!(running));
    obj.insert(
        "latest_tag".into(),
        latest_tag.map_or(Value::Null, |t| json!(t)),
    );
    obj.insert("has_update".into(), json!(has_update));
    obj.insert("channel".into(), json!("stable"));
    obj.insert("stale".into(), json!(stale));
    if used_stub {
        obj.insert("stub".into(), json!(true));
    }
    Value::Object(obj)
}

/// `git -C <repo> ls-remote --tags --refs origin`, bounded to 5s. Returns
/// stdout on a clean exit, else `None` (timeout / spawn error / non-zero).
async fn git_ls_remote(repo_root: &Path) -> Option<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(repo_root)
        .args(["ls-remote", "--tags", "--refs", "origin"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let fut = cmd.output();
    match tokio::time::timeout(std::time::Duration::from_secs(5), fut).await {
        Ok(Ok(out)) if out.status.success() => {
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        _ => None,
    }
}

fn repo_root() -> PathBuf {
    std::env::var("SUBCTL_REPO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

// ── handlers ──────────────────────────────────────────────────────────────

/// `GET /api/settings/keys` → API-key presence/length table (never values).
pub(crate) async fn keys_handler() -> Response {
    let env_lookup = |k: &str| std::env::var(k).ok();
    ok_json(build_keys_value(&secrets_path(), &env_lookup))
}

/// `GET /api/settings/secrets` → per-key `secrets.json` presence + env-override.
pub(crate) async fn secrets_handler() -> Response {
    let env_lookup = |k: &str| std::env::var(k).ok();
    ok_json(build_secrets_value(&secrets_path(), &env_lookup))
}

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

/// `GET /api/update/check` → running vs latest-tag comparison (5-min cached).
///
/// Honours `?stub_latest=` / `$SUBCTL_UPDATE_STUB_LATEST` (operator override)
/// and `?force=1`. Unlike v3 it does NOT emit the `update_available` SSE event —
/// that belongs to the deferred `/api/update/events` surface.
pub(crate) async fn update_check_handler(
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let running = running_version();
    let now = chrono::Utc::now().timestamp_millis();

    // Operator override hatch — pin a synthetic latest_tag, bypass git + cache.
    let stub = params
        .get("stub_latest")
        .cloned()
        .or_else(|| std::env::var("SUBCTL_UPDATE_STUB_LATEST").ok())
        .filter(|s| !s.is_empty());
    if let Some(stub) = stub {
        return ok_json(build_update_value(&running, Some(&stub), false, true));
    }

    let force = params.get("force").map(String::as_str) == Some("1");

    // Snapshot the cache, decide whether to re-probe, never hold the lock across
    // the await.
    let (mut latest_tag, mut stale, fresh) = {
        let guard = UPDATE_CACHE.lock().unwrap();
        match guard.as_ref() {
            Some(c) => (
                Some(c.latest_tag.clone()),
                c.stale,
                now - c.fetched_at_ms <= UPDATE_CHECK_TTL_MS,
            ),
            None => (None, false, false),
        }
    };

    if !fresh || force {
        match git_ls_remote(&repo_root()).await {
            Some(stdout) => match parse_latest_tag(&stdout) {
                Some(tag) => {
                    latest_tag = Some(tag.clone());
                    stale = false;
                    *UPDATE_CACHE.lock().unwrap() = Some(UpdateCheckCache {
                        latest_tag: tag,
                        fetched_at_ms: now,
                        stale: false,
                    });
                }
                None => mark_stale(&mut stale),
            },
            None => mark_stale(&mut stale),
        }
    }

    ok_json(build_update_value(
        &running,
        latest_tag.as_deref(),
        stale,
        false,
    ))
}

/// On a failed/empty probe, keep the cached tag but flag it stale (matches v3).
fn mark_stale(stale: &mut bool) {
    *stale = true;
    if let Some(c) = UPDATE_CACHE.lock().unwrap().as_mut() {
        c.stale = true;
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

    fn no_env(_: &str) -> Option<String> {
        None
    }

    // ── secrets ──

    #[test]
    fn secrets_absent_file_yields_all_unset_null_mtime() {
        let dir = tmpdir();
        let v = build_secrets_value(&dir.join("secrets.json"), &no_env);
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["priority"], json!(["env_var", "secrets_json", "absent"]));
        let rows = v["secrets"].as_array().unwrap();
        assert_eq!(rows.len(), SECRET_KEYS.len());
        assert_eq!(rows[0]["key"], json!("lmstudio_api_token"));
        assert_eq!(rows[0]["isSet"], json!(false));
        assert_eq!(rows[0]["envOverride"], json!(false));
        assert_eq!(rows[0]["lastModified"], Value::Null);
    }

    #[test]
    fn secrets_present_sets_flags_and_mtime() {
        let dir = tmpdir();
        let path = dir.join("secrets.json");
        std::fs::write(&path, r#"{"brave_api_key":"abc","lmstudio_api_token":""}"#).unwrap();
        let env = |k: &str| (k == "LINEAR_API_KEY").then(|| "x".to_string());
        let v = build_secrets_value(&path, &env);
        let rows = v["secrets"].as_array().unwrap();
        let find = |k: &str| rows.iter().find(|r| r["key"] == json!(k)).unwrap();
        assert_eq!(find("brave_api_key")["isSet"], json!(true));
        // empty-string value is NOT "set"
        assert_eq!(find("lmstudio_api_token")["isSet"], json!(false));
        // env override surfaces independent of the file
        assert_eq!(find("linear_api_key")["envOverride"], json!(true));
        assert!(find("brave_api_key")["lastModified"].is_string());
    }

    // ── keys ──

    #[test]
    fn keys_env_and_secrets_sources_combine() {
        let dir = tmpdir();
        let path = dir.join("secrets.json");
        std::fs::write(&path, r#"{"context7_api_key":"ctx7value"}"#).unwrap();
        let env = |k: &str| (k == "ANTHROPIC_API_KEY").then(|| "sk-ant-123".to_string());
        let v = build_keys_value(&path, &env);
        let rows = v["keys"].as_array().unwrap();
        assert_eq!(rows.len(), 12);
        let find = |n: &str| rows.iter().find(|r| r["name"] == json!(n)).unwrap();

        let anth = find("ANTHROPIC_API_KEY");
        assert_eq!(anth["ok"], json!(true));
        assert_eq!(anth["env"], json!(true));
        assert_eq!(anth["secrets_json"], Value::Null); // no secret_key counterpart
        assert_eq!(anth["length"], json!(10));

        let ctx = find("CONTEXT7_API_KEY");
        assert_eq!(ctx["ok"], json!(true));
        assert_eq!(ctx["env"], json!(false));
        assert_eq!(ctx["secrets_json"], json!(true));
        assert_eq!(ctx["length"], json!("ctx7value".len()));

        let groq = find("GROQ_API_KEY");
        assert_eq!(groq["ok"], json!(false));
        assert_eq!(groq["secrets_json"], Value::Null);
        assert_eq!(groq["length"], json!(0));
        assert!(v["note"].as_str().unwrap().contains("priority"));
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

    // ── update / semver ──

    #[test]
    fn cmp_semver_orders_numerically() {
        use std::cmp::Ordering::*;
        assert_eq!(cmp_semver("v3.3.12", "v3.3.2"), Greater);
        assert_eq!(cmp_semver("3.3.2", "v3.3.12"), Less);
        assert_eq!(cmp_semver("v2.0.0", "2.0.0"), Equal);
        assert_eq!(cmp_semver("v3.3.12-rc1", "3.3.12"), Equal); // suffix stripped
    }

    #[test]
    fn parse_latest_tag_picks_max_semver() {
        let stdout = "sha1\trefs/tags/v3.3.2\n\
                      sha2\trefs/tags/v3.3.12\n\
                      sha3\trefs/tags/not-a-version\n\
                      sha4\trefs/tags/v3.2.9\n";
        assert_eq!(parse_latest_tag(stdout), Some("v3.3.12".to_string()));
    }

    #[test]
    fn parse_latest_tag_none_when_no_semver_tags() {
        assert_eq!(parse_latest_tag("sha\trefs/tags/nightly\n"), None);
        assert_eq!(parse_latest_tag(""), None);
    }

    #[test]
    fn tag_is_semver_filters() {
        assert!(tag_is_semver("v3.3.12"));
        assert!(tag_is_semver("3.0.0"));
        assert!(!tag_is_semver("v3.3"));
        assert!(!tag_is_semver("nightly"));
    }

    #[test]
    fn build_update_value_shape_and_has_update() {
        let v = build_update_value("3.3.12", Some("v3.3.20"), false, false);
        assert_eq!(v["running_version"], json!("3.3.12"));
        assert_eq!(v["latest_tag"], json!("v3.3.20"));
        assert_eq!(v["has_update"], json!(true));
        assert_eq!(v["channel"], json!("stable"));
        assert_eq!(v["stale"], json!(false));
        assert_eq!(v.get("stub"), None);

        let same = build_update_value("3.3.12", Some("v3.3.12"), false, false);
        assert_eq!(same["has_update"], json!(false));

        let null_latest = build_update_value("3.3.12", None, true, false);
        assert_eq!(null_latest["latest_tag"], Value::Null);
        assert_eq!(null_latest["has_update"], json!(false));
        assert_eq!(null_latest["stale"], json!(true));

        let stub = build_update_value("3.3.12", Some("v9.9.9"), false, true);
        assert_eq!(stub["stub"], json!(true));
    }
}
