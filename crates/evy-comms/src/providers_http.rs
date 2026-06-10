//! v4-parity sprint (W1) — providers / models / catalogs family.
//!
//! **Native (v4 owns the data):**
//!
//! | Method | Path | v3 source |
//! |--------|------|-----------|
//! | GET    | `/api/models`              | LM Studio `/api/v0/models` (30s cache) |
//! | POST   | `/api/models/refresh`      | same, force-busting the cache |
//! | POST   | `/api/providers/profiles`  | add/edit an `accounts.conf` row |
//! | DELETE | `/api/providers/profiles`  | remove an `accounts.conf` row |
//!
//! **Stays on the v3 reverse-proxy (pi-ai owns the data):**
//! `GET /api/providers`, `GET /api/catalogs`, and every per-provider
//! catalog sub-endpoint (`/api/catalogs/{p}`, `…/refresh`, model
//! enable/disable, `/api/providers/{p}/default-model`).
//!
//! Those list endpoints are driven by the **pi-ai catalog**
//! (`@earendil-works/pi-ai` via `components/evy/pi-ai-catalog.ts`), which
//! has no Rust equivalent — porting it is a named non-goal (V4_BRIDGE.md).
//! Serving them natively today would drop the cloud-provider rows the
//! Providers panel shows, a user-visible regression, so they fall through
//! the `/api/{*rest}` catch-all to the v3 Bun dashboard unchanged. The
//! boundary is: native where v4 owns the data, proxied where pi-ai owns it.
//!
//! # LM Studio cache + accounts.conf
//!
//! `/api/models` and `/api/models/refresh` fetch LM Studio's native
//! `/api/v0/models` endpoint through a 30-second, host-keyed, success-only
//! response cache (mirrors v3's `getLmstudioModels`). The profile CRUD
//! reads/writes the same pipe-delimited `accounts.conf` rows v3 writes,
//! byte-for-byte (column padding) — minus the pi-ai `isCatalogProvider`
//! write-gate, which is enforced only when the daemon supplies the catalog
//! id set via [`AppState::provider_catalog`](crate::AppState::provider_catalog)
//! and is permissive (skipped) when absent.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;

use crate::http::HttpState;

// ─── daemon-sourced pi-ai catalog hook ─────────────────────────────────

/// pi-ai-catalog data the daemon supplies to the providers family.
///
/// Returned (as `Some`) by [`AppState::provider_catalog`](crate::AppState::provider_catalog)
/// once the daemon surfaces the `@earendil-works/pi-ai` registry. The only
/// native consumer today is the `POST /api/providers/profiles` write-gate,
/// so the struct carries the accepted provider-id set; the list endpoints
/// that would need the rest of the registry stay on the v3 proxy. Future
/// native migration of those endpoints would extend this struct — it is the
/// designated wiring point.
#[derive(Debug, Clone, Default)]
pub struct ProviderCatalogData {
    /// Canonical pi-ai provider ids accepted by `POST /api/providers/profiles`.
    /// When present, a write whose `provider` is not in this set is rejected
    /// 400 (mirrors v3's `isCatalogProvider`); when the whole struct is
    /// `None` the gate is skipped (permissive).
    pub provider_ids: HashSet<String>,
}

// ─── env / path resolution ──────────────────────────────────────────────

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// `SUBCTL_CONFIG_DIR` or `~/.config/subctl`.
fn config_dir() -> PathBuf {
    std::env::var("SUBCTL_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{}/.config/subctl", home())))
}

/// `<config>/secrets.json` — dashboard-managed secret store.
fn secrets_path() -> PathBuf {
    config_dir().join("secrets.json")
}

/// `SUBCTL_ACCOUNTS_CONF` (v4 override, matches `accounts_http`) else
/// `<config>/accounts.conf` (v3 write target).
fn accounts_conf_path() -> PathBuf {
    if let Ok(p) = std::env::var("SUBCTL_ACCOUNTS_CONF") {
        return PathBuf::from(p);
    }
    config_dir().join("accounts.conf")
}

/// LM Studio base URL: `SUBCTL_LMSTUDIO_HOST` or `http://localhost:1234`.
fn lm_host() -> String {
    std::env::var("SUBCTL_LMSTUDIO_HOST").unwrap_or_else(|_| "http://localhost:1234".to_string())
}

/// Resolve the LM Studio API token like v3's `resolveSecret`: the
/// `LMSTUDIO_API_TOKEN` env var wins, else the `lmstudio_api_token` string
/// in `secrets.json`. `op://` references are treated as absent (they are
/// resolved by a backend chain v4 doesn't run on this hot path).
fn lm_token() -> Option<String> {
    if let Ok(v) = std::env::var("LMSTUDIO_API_TOKEN") {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    let text = std::fs::read_to_string(secrets_path()).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;
    let v = json.get("lmstudio_api_token").and_then(Value::as_str)?.trim();
    if v.is_empty() || v.starts_with("op://") {
        return None;
    }
    Some(v.to_string())
}

// ─── LM Studio model fetch + 30s response cache ─────────────────────────

/// 30-second freshness window — fresh enough that a newly-loaded model
/// surfaces quickly, slow enough to coalesce hundreds of polls.
const LMSTUDIO_CACHE_TTL: Duration = Duration::from_secs(30);
/// Upstream request timeout (v3 used a 2.5s `AbortSignal`).
const LMSTUDIO_TIMEOUT: Duration = Duration::from_millis(2500);

/// A single cached LM Studio catalog response, keyed by host (mirrors v3's
/// single-slot `_lmstudioModelsCache`).
struct LmCacheSlot {
    host: String,
    data: Vec<Value>,
    fetched: Instant,
}

/// Why an LM Studio fetch failed — preserved so the handler can emit v3's
/// distinct error shapes (`missing_token`/`invalid_token`/`http_error`/`unreachable`).
#[derive(Debug)]
enum LmError {
    /// Upstream returned a non-success HTTP status (carries the code).
    Http(u16),
    /// The request never completed (connection refused, DNS, timeout,
    /// malformed body). Carries a freeform diagnostic.
    Unreachable(String),
}

/// LM Studio's `/api/v0/models` envelope. `data` is passed through
/// verbatim (each model object kept as-is, like v3).
#[derive(Deserialize)]
struct LmModelsResponse {
    #[serde(default)]
    data: Option<Vec<Value>>,
}

fn lm_cache() -> &'static AsyncMutex<Option<LmCacheSlot>> {
    static C: OnceLock<AsyncMutex<Option<LmCacheSlot>>> = OnceLock::new();
    C.get_or_init(|| AsyncMutex::new(None))
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// Fetch LM Studio's model catalog through the cache.
///
/// Behaviour mirrors v3's `getLmstudioModels`: a fresh same-host cache hit
/// is returned without touching the network (unless `force`); only
/// successful responses are cached; the cache is keyed by host so a host
/// switch re-fetches. The lock is held across the request so concurrent
/// callers coalesce onto one upstream fetch rather than stampeding.
async fn get_lmstudio_models(host: &str, force: bool) -> Result<Vec<Value>, LmError> {
    let mut slot = lm_cache().lock().await;
    if !force {
        if let Some(c) = slot.as_ref() {
            if c.host == host && c.fetched.elapsed() < LMSTUDIO_CACHE_TTL {
                return Ok(c.data.clone());
            }
        }
    }

    let url = format!("{host}/api/v0/models");
    let mut req = http_client().get(&url).timeout(LMSTUDIO_TIMEOUT);
    if let Some(tok) = lm_token() {
        req = req.bearer_auth(tok);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| LmError::Unreachable(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(LmError::Http(status.as_u16()));
    }
    let body: LmModelsResponse = resp
        .json()
        .await
        .map_err(|e| LmError::Unreachable(e.to_string()))?;
    let data = body.data.unwrap_or_default();
    *slot = Some(LmCacheSlot {
        host: host.to_string(),
        data: data.clone(),
        fetched: Instant::now(),
    });
    Ok(data)
}

// ─── /api/models response builders ──────────────────────────────────────

/// ISO-8601 timestamp with millisecond precision + `Z` (matches JS
/// `new Date().toISOString()`).
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Success body for `/api/models` (and, with `refresh`, `/api/models/refresh`).
fn models_success_body(host: &str, models: &[Value], refresh: bool) -> Value {
    let loaded_count = models
        .iter()
        .filter(|m| m.get("state").and_then(Value::as_str) == Some("loaded"))
        .count();
    let mut body = json!({
        "ok": true,
        "host": host,
        "ts": now_iso(),
        "total": models.len(),
        "loaded_count": loaded_count,
        "models": models,
    });
    if refresh {
        body["refreshed"] = json!(true);
    }
    body
}

/// Map an [`LmError`] to v3's response shape. `refresh` selects the
/// `/api/models/refresh` copy (slightly shorter hints) where it differs
/// from `/api/models`.
fn lm_error_response(err: LmError, host: &str, has_token: bool, refresh: bool) -> Response {
    let (status, body) = match err {
        LmError::Http(401) if !has_token => (
            StatusCode::UNAUTHORIZED,
            json!({
                "ok": false,
                "kind": "missing_token",
                "error": "missing token",
                "message": "LM Studio is requiring an API token, but subctl doesn't have one configured.",
                "hint": "Either paste the current token into Settings → API Tokens, or turn off \"Require API Token\" in LM Studio (Developer → Server settings).",
                "host": host,
            }),
        ),
        LmError::Http(401) => {
            let (message, hint) = if refresh {
                (
                    "LM Studio rejected the saved API token. It's likely stale.",
                    "Rotate the token in LM Studio and update Settings → API Tokens, or turn off \"Require API Token\".",
                )
            } else {
                (
                    "LM Studio rejected the saved API token. It's likely stale — the token in LM Studio was rotated or cleared.",
                    "Rotate the token in LM Studio (Developer → Server settings), then paste the new value into Settings → API Tokens. Or turn off \"Require API Token\" if you don't need it.",
                )
            };
            (
                StatusCode::UNAUTHORIZED,
                json!({
                    "ok": false,
                    "kind": "invalid_token",
                    "error": "token rejected",
                    "message": message,
                    "hint": hint,
                    "host": host,
                }),
            )
        }
        LmError::Http(code) => (
            StatusCode::BAD_GATEWAY,
            json!({
                "ok": false,
                "kind": "http_error",
                "error": format!("HTTP {code}"),
                "message": format!("LM Studio returned HTTP {code} from /api/v0/models."),
                "hint": "Check the LM Studio app's server logs for the underlying error.",
                "host": host,
            }),
        ),
        LmError::Unreachable(msg) => {
            let hint = if refresh {
                "Make sure the LM Studio app is running and the server is started."
            } else {
                "Make sure the LM Studio app is running and the server is started (Developer → Start Server). If you bound it to 127.0.0.1, confirm subctl is on the same host."
            };
            (
                StatusCode::BAD_GATEWAY,
                json!({
                    "ok": false,
                    "kind": "unreachable",
                    "error": msg,
                    "message": format!("LM Studio at {host} didn't respond."),
                    "hint": hint,
                    "host": host,
                }),
            )
        }
    };
    (status, Json(body)).into_response()
}

// ─── accounts.conf profile CRUD ─────────────────────────────────────────

/// Either a success body (→ 200) or `(status, error-body)`.
type CrudResult = std::result::Result<Value, (StatusCode, Value)>;

fn resp(r: CrudResult) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err((status, body)) => (status, Json(body)).into_response(),
    }
}

/// v3's `^[a-zA-Z0-9._-]+$` alias guard (non-empty; alphanumerics + `.`/`-`/`_`).
fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// First pipe-delimited field of an `accounts.conf` row, trimmed — `None`
/// for blank/comment lines.
fn row_alias(line: &str) -> Option<&str> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    t.split('|').next().map(str::trim)
}

/// Add or edit an `accounts.conf` profile row (port of the v3
/// `POST /api/providers/profiles` handler).
///
/// `known_providers` is the pi-ai catalog id set: when `Some`, a `provider`
/// not in the set is rejected 400; when `None` the gate is skipped.
fn do_post_profile(
    path: &Path,
    body: &Value,
    known_providers: Option<&HashSet<String>>,
) -> CrudResult {
    let get_str = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let alias = get_str("alias");
    let provider = get_str("provider");
    let email = get_str("email");
    let config_dir = get_str("config_dir");
    let description = get_str("description");
    let mode = body
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("add")
        .to_string();

    if alias.is_empty() || provider.is_empty() || email.is_empty() || config_dir.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "alias, provider, email, config_dir required" }),
        ));
    }
    if !valid_alias(&alias) {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "alias must be alphanumerics + . - _" }),
        ));
    }
    if let Some(known) = known_providers {
        if !known.contains(&provider) {
            return Err((
                StatusCode::BAD_REQUEST,
                json!({
                    "ok": false,
                    "error": format!("provider \"{provider}\" is not in the pi-ai catalog"),
                    "hint": "pass a pi-ai canonical id (see /api/providers)",
                }),
            ));
        }
    }

    let mut lines: Vec<String> = match std::fs::read_to_string(path) {
        Ok(s) => s.split('\n').map(str::to_string).collect(),
        Err(_) => Vec::new(),
    };
    // Column widths match v3's `padEnd(15|7|32|25)` exactly so the row is
    // byte-identical to what the Bun dashboard writes.
    let new_row =
        format!("{alias:<15} | {provider:<7} | {email:<32} | {config_dir:<25} | {description}");
    let existing = lines.iter().position(|l| row_alias(l) == Some(alias.as_str()));

    if mode == "edit" {
        match existing {
            Some(i) => lines[i] = new_row,
            None => {
                return Err((
                    StatusCode::NOT_FOUND,
                    json!({ "ok": false, "error": "alias not found to edit" }),
                ))
            }
        }
    } else if existing.is_some() {
        return Err((
            StatusCode::CONFLICT,
            json!({ "ok": false, "error": format!("alias '{alias}' already exists") }),
        ));
    } else {
        lines.push(new_row);
    }

    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "ok": false, "error": e.to_string() }),
            ));
        }
    }
    match std::fs::write(path, lines.join("\n")) {
        Ok(()) => Ok(json!({ "ok": true, "alias": alias, "mode": mode })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "ok": false, "error": e.to_string() }),
        )),
    }
}

/// Remove an `accounts.conf` profile row by alias (port of the v3
/// `DELETE /api/providers/profiles` handler). Comment/blank lines are
/// preserved.
fn do_delete_profile(path: &Path, alias: &str) -> CrudResult {
    if alias.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "alias required" }),
        ));
    }
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => {
            return Err((
                StatusCode::NOT_FOUND,
                json!({ "ok": false, "error": "accounts.conf missing" }),
            ))
        }
    };
    let lines: Vec<&str> = content.split('\n').collect();
    let filtered: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| row_alias(l) != Some(alias))
        .collect();
    if filtered.len() == lines.len() {
        return Err((
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "error": "alias not found" }),
        ));
    }
    match std::fs::write(path, filtered.join("\n")) {
        Ok(()) => Ok(json!({ "ok": true, "alias": alias })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "ok": false, "error": e.to_string() }),
        )),
    }
}

// ─── HTTP handlers ──────────────────────────────────────────────────────

/// `GET /api/models` — LM Studio model catalog (native API, richer than `/v1`).
pub(crate) async fn models_handler() -> Response {
    let host = lm_host();
    let has_token = lm_token().is_some();
    match get_lmstudio_models(&host, false).await {
        Ok(models) => Json(models_success_body(&host, &models, false)).into_response(),
        Err(e) => lm_error_response(e, &host, has_token, false),
    }
}

/// `POST /api/models/refresh` — force-bust the 30s cache and re-fetch.
pub(crate) async fn models_refresh_handler() -> Response {
    let host = lm_host();
    let has_token = lm_token().is_some();
    match get_lmstudio_models(&host, true).await {
        Ok(models) => Json(models_success_body(&host, &models, true)).into_response(),
        Err(e) => lm_error_response(e, &host, has_token, true),
    }
}

/// `POST /api/providers/profiles` — add or edit an `accounts.conf` row.
pub(crate) async fn profiles_post_handler(State(state): State<HttpState>, body: String) -> Response {
    let Ok(parsed) = serde_json::from_str::<Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "invalid JSON" })),
        )
            .into_response();
    };
    let ids = state.app.provider_catalog().map(|c| c.provider_ids);
    resp(do_post_profile(&accounts_conf_path(), &parsed, ids.as_ref()))
}

/// `DELETE /api/providers/profiles` — remove an `accounts.conf` row by alias.
pub(crate) async fn profiles_delete_handler(body: String) -> Response {
    let Ok(parsed) = serde_json::from_str::<Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "invalid JSON" })),
        )
            .into_response();
    };
    let alias = parsed.get("alias").and_then(Value::as_str).unwrap_or_default();
    resp(do_delete_profile(&accounts_conf_path(), alias))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmpdir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "evy-providers-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Serializes the cache-touching LM Studio tests (the cache is a global
    /// single slot, like v3) and lets them reset it between runs.
    fn cache_test_lock() -> &'static AsyncMutex<()> {
        static L: OnceLock<AsyncMutex<()>> = OnceLock::new();
        L.get_or_init(|| AsyncMutex::new(()))
    }

    async fn reset_lm_cache() {
        *lm_cache().lock().await = None;
    }

    // ── LM Studio model fetch + cache ──

    #[tokio::test]
    async fn lmstudio_fetch_success_caches_and_coalesces() {
        let _guard = cache_test_lock().lock().await;
        reset_lm_cache().await;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v0/models"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    { "id": "m1", "type": "llm", "state": "loaded" },
                    { "id": "m2", "type": "vlm", "state": "not-loaded" }
                ]
            })))
            .expect(1) // second call must come from the cache
            .mount(&server)
            .await;

        let host = server.uri();
        let a = get_lmstudio_models(&host, false).await.expect("fetch ok");
        assert_eq!(a.len(), 2);
        // Within the TTL, no second upstream hit.
        let b = get_lmstudio_models(&host, false).await.expect("cache hit");
        assert_eq!(b.len(), 2);
    }

    #[tokio::test]
    async fn lmstudio_force_refresh_bypasses_cache() {
        let _guard = cache_test_lock().lock().await;
        reset_lm_cache().await;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v0/models"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(json!({ "data": [] })),
            )
            .expect(2) // forced second call re-hits upstream
            .mount(&server)
            .await;
        let host = server.uri();
        get_lmstudio_models(&host, false).await.unwrap();
        get_lmstudio_models(&host, true).await.unwrap();
    }

    #[tokio::test]
    async fn lmstudio_http_401_surfaces_as_http_error() {
        let _guard = cache_test_lock().lock().await;
        reset_lm_cache().await;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let err = get_lmstudio_models(&server.uri(), false).await.err().unwrap();
        assert!(matches!(err, LmError::Http(401)));
    }

    #[tokio::test]
    async fn lmstudio_unreachable_surfaces_as_unreachable() {
        let _guard = cache_test_lock().lock().await;
        reset_lm_cache().await;
        // Port 1 is reserved/closed → connection refused, fast.
        let err = get_lmstudio_models("http://127.0.0.1:1", false)
            .await
            .err()
            .unwrap();
        assert!(matches!(err, LmError::Unreachable(_)));
    }

    // ── error shape mapping ──

    async fn body_of(resp: Response) -> (StatusCode, Value) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn error_401_without_token_is_missing_token() {
        let (status, body) =
            body_of(lm_error_response(LmError::Http(401), "http://h", false, false)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["kind"], "missing_token");
        assert_eq!(body["host"], "http://h");
    }

    #[tokio::test]
    async fn error_401_with_token_is_invalid_token_and_refresh_copy_differs() {
        let (_s, models) = body_of(lm_error_response(LmError::Http(401), "h", true, false)).await;
        let (_s2, refresh) = body_of(lm_error_response(LmError::Http(401), "h", true, true)).await;
        assert_eq!(models["kind"], "invalid_token");
        assert_eq!(refresh["kind"], "invalid_token");
        assert_ne!(models["hint"], refresh["hint"]);
    }

    #[tokio::test]
    async fn error_http_and_unreachable_are_502() {
        let (s1, b1) = body_of(lm_error_response(LmError::Http(503), "h", false, false)).await;
        assert_eq!(s1, StatusCode::BAD_GATEWAY);
        assert_eq!(b1["kind"], "http_error");
        assert_eq!(b1["error"], "HTTP 503");
        let (s2, b2) =
            body_of(lm_error_response(LmError::Unreachable("boom".into()), "h", false, false)).await;
        assert_eq!(s2, StatusCode::BAD_GATEWAY);
        assert_eq!(b2["kind"], "unreachable");
        assert_eq!(b2["error"], "boom");
    }

    #[test]
    fn models_success_body_counts_loaded_and_flags_refresh() {
        let models = json!([
            { "state": "loaded" }, { "state": "not-loaded" }, { "state": "loaded" }
        ]);
        let arr = models.as_array().unwrap();
        let body = models_success_body("http://h", arr, false);
        assert_eq!(body["ok"], true);
        assert_eq!(body["total"], 3);
        assert_eq!(body["loaded_count"], 2);
        assert!(body.get("refreshed").is_none());
        assert!(body["ts"].as_str().unwrap().ends_with('Z'));
        let refreshed = models_success_body("http://h", arr, true);
        assert_eq!(refreshed["refreshed"], true);
    }

    // ── profile CRUD ──

    fn known(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn post_profile_add_writes_padded_row_and_round_trips() {
        let path = tmpdir().join("accounts.conf");
        let body = json!({
            "alias": "claude-x", "provider": "anthropic",
            "email": "x@y.com", "config_dir": "~/.claude-x", "description": "test"
        });
        let ok = do_post_profile(&path, &body, None).unwrap();
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["mode"], "add");
        let written = std::fs::read_to_string(&path).unwrap();
        // first pipe field trims back to the alias
        assert_eq!(row_alias(&written), Some("claude-x"));
        assert!(written.contains("anthropic"));
    }

    #[test]
    fn post_profile_duplicate_409_edit_missing_404_edit_replaces() {
        let path = tmpdir().join("accounts.conf");
        let add = json!({ "alias": "a", "provider": "anthropic", "email": "e", "config_dir": "d" });
        do_post_profile(&path, &add, None).unwrap();
        // duplicate add → 409
        let (s, _) = do_post_profile(&path, &add, None).unwrap_err();
        assert_eq!(s, StatusCode::CONFLICT);
        // edit missing → 404
        let edit_missing = json!({ "alias": "zzz", "provider": "anthropic", "email": "e", "config_dir": "d", "mode": "edit" });
        let (s, _) = do_post_profile(&path, &edit_missing, None).unwrap_err();
        assert_eq!(s, StatusCode::NOT_FOUND);
        // edit existing → replaces in place (still one row for the alias)
        let edit = json!({ "alias": "a", "provider": "openai", "email": "e2", "config_dir": "d2", "mode": "edit" });
        do_post_profile(&path, &edit, None).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("openai"));
        assert_eq!(written.lines().filter(|l| row_alias(l) == Some("a")).count(), 1);
    }

    #[test]
    fn post_profile_validation_errors() {
        let path = tmpdir().join("accounts.conf");
        // missing fields → 400
        let (s, _) = do_post_profile(&path, &json!({ "alias": "a" }), None).unwrap_err();
        assert_eq!(s, StatusCode::BAD_REQUEST);
        // bad alias → 400
        let bad = json!({ "alias": "a b", "provider": "anthropic", "email": "e", "config_dir": "d" });
        let (s, _) = do_post_profile(&path, &bad, None).unwrap_err();
        assert_eq!(s, StatusCode::BAD_REQUEST);
        // provider not in known set → 400 (write-gate strict when Some)
        let unknown = json!({ "alias": "a", "provider": "nope", "email": "e", "config_dir": "d" });
        let (s, body) = do_post_profile(&path, &unknown, Some(&known(&["anthropic"]))).unwrap_err();
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("pi-ai catalog"));
        // provider in known set → ok
        let okp = json!({ "alias": "a", "provider": "anthropic", "email": "e", "config_dir": "d" });
        assert!(do_post_profile(&path, &okp, Some(&known(&["anthropic"]))).is_ok());
    }

    #[test]
    fn delete_profile_removes_row_preserves_comments_and_404s() {
        let path = tmpdir().join("accounts.conf");
        std::fs::write(&path, "# header\na | anthropic | e | d | \nb | openai | e | d | ").unwrap();
        let ok = do_delete_profile(&path, "a").unwrap();
        assert_eq!(ok["ok"], true);
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("# header"), "comment preserved");
        assert!(!after.lines().any(|l| row_alias(l) == Some("a")));
        assert!(after.lines().any(|l| row_alias(l) == Some("b")));
        // deleting a missing alias → 404
        let (s, _) = do_delete_profile(&path, "zzz").unwrap_err();
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[test]
    fn delete_profile_missing_file_404_empty_alias_400() {
        let path = tmpdir().join("does-not-exist.conf");
        let (s, _) = do_delete_profile(&path, "a").unwrap_err();
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _) = do_delete_profile(&path, "").unwrap_err();
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }
}
