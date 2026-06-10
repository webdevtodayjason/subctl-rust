//! Phase 1 slice 1e (part) — `GET /api/evy/accounts`, the first integrated proof.
//!
//! Wires 1a (verdict) + 1b (auth) + 1c (usage cache) + 1d (buckets) into the v3
//! account-summary row (port of `buildAccountSummaries`, server.ts:1819-1869).
//! Sessions aren't native yet (Phase 1e sessions sub-object), so `active_sessions`
//! / `parallel_on_account` are 0 for now — the verdict still folds auth + usage +
//! 429s. Accounts come from `evy-providers::AccountsStore` (accounts.conf).

use std::path::PathBuf;

use axum::response::Json;
use serde::Serialize;

use std::time::Duration;

use crate::dashboard_state::{
    auth_status, compute_account_verdict, dispatch_verdict, AccountVerdict, AccountVerdictInput,
    AuthStatus, Verdict,
};
use crate::rate_limits::{build_rate_limits, read_usage_history_24h, today_date_str, UsageBucket};
use crate::usage_cache::{instance as usage_cache, UsageEntry};

/// One account row for the Overview accounts table (v3-shape).
#[derive(Debug, Serialize)]
pub struct AccountSummary {
    alias: String,
    provider: String,
    email: String,
    config_dir: String,
    auth_status: AuthStatus,
    /// Top-level color for `/api/evy/accounts` consumers (== `dispatch.verdict`).
    verdict: Verdict,
    active_sessions: u32,
    rl_hits_today: u32,
    last_activity_seconds_ago: Option<i64>,
    color_class: String,
    usage: Option<UsageEntry>,
    dispatch: AccountVerdict,
    usage_history_24h: Vec<UsageBucket>,
    usage_error: Option<String>,
    usage_stale: bool,
    usage_stale_age_ms: Option<u64>,
}

impl AccountSummary {
    /// Read accessor used by the `/api/state` composite (slice 1e) to fold the
    /// per-account verdict into the global dispatch.
    #[must_use]
    pub fn alias_and_verdict(&self) -> (String, AccountVerdict) {
        (self.alias.clone(), self.dispatch.clone())
    }
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

fn accounts_conf_path() -> PathBuf {
    std::env::var("SUBCTL_ACCOUNTS_CONF")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{}/.config/subctl/accounts.conf", home())))
}

fn history_path() -> PathBuf {
    let cfg = std::env::var("SUBCTL_CONFIG_DIR")
        .unwrap_or_else(|_| format!("{}/.config/subctl", home()));
    PathBuf::from(cfg).join("cache").join("usage-history.jsonl")
}

fn rl_log_path() -> PathBuf {
    std::env::var("SUBCTL_RL_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{}/.claude/rate-limit-events.log", home())))
}

/// Build the account summaries (port of `buildAccountSummaries`). `now_ms` is
/// unix-millis. Best-effort: a missing accounts.conf yields an empty list.
pub async fn build_account_summaries(now_ms: i64) -> Vec<AccountSummary> {
    let rows = evy_providers::AccountsStore::open(&accounts_conf_path())
        .and_then(|s| s.list_rows())
        .unwrap_or_default();

    let usage = usage_cache().fetch_all(now_ms.max(0) as u64, false).await;
    let history24 = read_usage_history_24h(&history_path(), now_ms);
    let aliases: Vec<String> = rows.iter().map(|r| r.alias.clone()).collect();
    let rl = build_rate_limits(&rl_log_path(), &aliases, now_ms, &today_date_str());

    rows.into_iter()
        .map(|r| {
            let u = usage.iter().find(|x| x.alias == r.alias);
            let auth = auth_status(&r.config_dir);
            let rl_hits = rl.by_account.get(&r.alias).map_or(0, |a| a.count_today);
            let usage_entry = u.and_then(|x| x.usage.clone());
            let seven = usage_entry
                .as_ref()
                .and_then(|e| e.seven_day.as_ref())
                .and_then(|w| w.utilization);
            let five = usage_entry
                .as_ref()
                .and_then(|e| e.five_hour.as_ref())
                .and_then(|w| w.utilization);
            let dispatch = compute_account_verdict(&AccountVerdictInput {
                auth_ready: auth == AuthStatus::Ready,
                seven_day_util: seven,
                five_hour_util: five,
                recent_429: rl_hits,
                parallel_on_account: 0,
            });
            let usage_history_24h = history24
                .get(&r.alias)
                .cloned()
                .unwrap_or_else(|| vec![UsageBucket::default(); 24]);
            AccountSummary {
                alias: r.alias,
                provider: r.provider,
                email: r.email,
                config_dir: r.config_dir.to_string_lossy().into_owned(),
                auth_status: auth,
                verdict: dispatch.verdict, // Verdict is Copy — read before moving dispatch
                active_sessions: 0,
                rl_hits_today: rl_hits,
                last_activity_seconds_ago: None,
                color_class: String::new(),
                usage: usage_entry,
                dispatch,
                usage_history_24h,
                usage_error: u.and_then(|x| x.error.clone()),
                usage_stale: u.and_then(|x| x.stale).unwrap_or(false),
                usage_stale_age_ms: u.and_then(|x| x.stale_age_ms),
            }
        })
        .collect()
}

/// `GET /api/evy/accounts` → `{ accounts: [AccountSummary, …] }`.
pub(crate) async fn accounts_handler() -> Json<serde_json::Value> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let accounts = build_account_summaries(now_ms).await;
    Json(serde_json::json!({ "accounts": accounts }))
}

/// `GET /api/evy/rate-limits` → `{ by_account:[{account,usage_history_24h[24],
/// buckets_24h[24],count_today}], today_total, recent_429_count }` (slice 1d/1e).
pub(crate) async fn rate_limits_handler() -> Json<serde_json::Value> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let rows = evy_providers::AccountsStore::open(&accounts_conf_path())
        .and_then(|s| s.list_rows())
        .unwrap_or_default();
    let aliases: Vec<String> = rows.iter().map(|r| r.alias.clone()).collect();
    let history = read_usage_history_24h(&history_path(), now_ms);
    let rl = build_rate_limits(&rl_log_path(), &aliases, now_ms, &today_date_str());

    let by_account: Vec<serde_json::Value> = aliases
        .iter()
        .map(|a| {
            let acct = rl.by_account.get(a);
            serde_json::json!({
                "account": a,
                "usage_history_24h": history.get(a).cloned().unwrap_or_else(|| vec![UsageBucket::default(); 24]),
                "buckets_24h": acct.map(|x| x.buckets_24h.clone()).unwrap_or_else(|| vec![0; 24]),
                "count_today": acct.map_or(0, |x| x.count_today),
            })
        })
        .collect();

    Json(serde_json::json!({
        "by_account": by_account,
        "today_total": rl.today_total,
        "recent_429_count": rl.recent_429_count,
    }))
}

/// Fetch the v3 Bun `/api/state` as the composite base (sessions/orchestrations/
/// service/cost/conversations — not yet v4-native, per Phase 1 non-goals).
async fn fetch_bun_state() -> Option<serde_json::Value> {
    let upstream =
        std::env::var("EVY_PROXY_UPSTREAM").unwrap_or_else(|_| "127.0.0.1:8787".to_string());
    let resp = reqwest::Client::new()
        .get(format!("http://{upstream}/api/state"))
        .timeout(Duration::from_secs(8))
        .send()
        .await
        .ok()?;
    resp.json::<serde_json::Value>().await.ok()
}

/// Build the native `/api/state` composite (slice 1e). Overlays v4-native
/// `accounts` + `dispatch` (severity best-of) + `cost` onto the Bun base, so the
/// verdict pill, accounts table, and cost panel are native while sessions/orch
/// keep rendering from v3 until their phases. Falls back to a minimal native
/// object if Bun is down. Shared by the HTTP handler and the `/api/live` liveness
/// broadcast (1f).
pub(crate) async fn build_state() -> serde_json::Value {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let accounts = build_account_summaries(now_ms).await;
    let av: Vec<(String, AccountVerdict)> = accounts.iter().map(AccountSummary::alias_and_verdict).collect();
    let dispatch = dispatch_verdict(&av);
    // Cost seam — native CostBundle replaces the v3-proxied `cost` field.
    let cost = crate::cost_http::cost_value(now_ms).await;

    let mut base = fetch_bun_state()
        .await
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({ "ok": true }));
    if let Some(obj) = base.as_object_mut() {
        obj.insert("accounts".into(), serde_json::to_value(&accounts).unwrap_or(serde_json::Value::Null));
        obj.insert("dispatch".into(), serde_json::to_value(&dispatch).unwrap_or(serde_json::Value::Null));
        obj.insert("cost".into(), cost);
    }
    base
}

/// `GET /api/state` — native composite handler.
pub(crate) async fn state_handler() -> Json<serde_json::Value> {
    Json(build_state().await)
}

/// `POST /api/refresh` — bust the usage cache (force-refresh) + ack. The next
/// `/api/state`/`/api/evy/accounts` call sees fresh usage.
pub(crate) async fn refresh_handler() -> Json<serde_json::Value> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    usage_cache().fetch_all(now_ms.max(0) as u64, true).await;
    Json(serde_json::json!({ "ok": true }))
}
