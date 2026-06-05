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

use crate::dashboard_state::{
    auth_status, compute_account_verdict, AccountVerdict, AccountVerdictInput, AuthStatus, Verdict,
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
