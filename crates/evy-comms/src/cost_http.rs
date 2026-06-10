//! Phase (cutover) — native cost/savings synthesis, the v3 `buildCostBundle`
//! brain ported byte-faithfully from `dashboard/server.ts:985-1010` +
//! `dashboard/lib/cost.ts`.
//!
//! v3 has **no on-disk cost ledger** — the "ledger" is the Claude Code session
//! transcripts themselves. This module reproduces the same numbers by reading the
//! *same* inputs the v3 Bun dashboard reads:
//!   1. each claude account's `<config_dir>/projects/*/*.jsonl` (the `usage`
//!      blocks Claude Code records per assistant turn),
//!   2. `config/pricing.json` (list-price API rates → API-equivalent cost),
//!   3. `~/.config/subctl/accounts.conf` (which accounts to walk).
//!
//! It estimates what the same token usage would have cost at Anthropic API list
//! price, then `savings = api_cost − subscription` for the 30-day month window.
//!
//! Parity-critical details preserved from v3:
//!   * **claude-only** accounts (`provider == "claude"`); non-claude excluded.
//!   * the unattributed **`default (~/.claude)`** bucket is *prepended* when
//!     `~/.claude` isn't already a configured account and it has ≥1 turn.
//!   * models absent from pricing keep `cost_usd == 0` but still contribute their
//!     tokens/turns to `total_tokens`/`total_turns`.
//!   * `by_model` is **stably** sorted by `cost_usd` descending (first-seen order
//!     preserved among ties, matching JS `Map` + stable `Array.sort`).
//!   * per-model `cost_usd` is rounded to 4 dp; the account `total_cost_usd` sums
//!     the **unrounded** per-model costs then rounds to 4 dp; the bundle `totals`
//!     sum the **rounded** per-account values then round to 2 dp.
//!   * `scanned_files` counts files whose mtime ≥ the window start (v3's fast-skip
//!     of stale transcripts) — a pure optimization that never changes token totals
//!     (a file older than the window has only out-of-window lines anyway).
//!   * a 5-minute in-process cache, matching v3 `COST_CACHE_TTL_MS`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use axum::response::Json;
use serde::{Deserialize, Serialize};

/// Cache TTL — 5 min, matching v3 `COST_CACHE_TTL_MS`.
const COST_CACHE_TTL_MS: i64 = 5 * 60 * 1000;

/// Default monthly subscription cost when pricing.json omits the provider.
/// Matches v3 `pricing.subscription_usd_monthly[provider] ?? 200`.
const DEFAULT_SUBSCRIPTION_USD: f64 = 200.0;

/// Window lengths in milliseconds (v3 `WINDOWS`).
const WINDOW_TODAY_MS: i64 = 24 * 60 * 60 * 1000;
const WINDOW_WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const WINDOW_MONTH_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Embedded list-price rate table — the parity anchor for the deployed daemon.
///
/// v3 reads these from its repo's `config/pricing.json`; the v4 daemon has no
/// repo-relative path and (per the cutover constraints) can't be reconfigured or
/// redeployed here, so the same rates are embedded as the final fallback. The
/// values are identical to `config/pricing.json` (updated 2026-05-04), so the
/// native numbers match `:8787` with or without an external file. Override at
/// runtime with `SUBCTL_PRICING_FILE` or `~/.config/subctl/pricing.json`.
const DEFAULT_PRICING_JSON: &str = r#"{
  "models": {
    "claude-opus-4-7":   { "input": 5.00, "output": 25.00, "cacheRead": 0.50, "cacheWrite": 6.25 },
    "claude-opus-4-6":   { "input": 5.00, "output": 25.00, "cacheRead": 0.50, "cacheWrite": 6.25 },
    "claude-opus-4-5":   { "input": 5.00, "output": 25.00, "cacheRead": 0.50, "cacheWrite": 6.25 },
    "claude-sonnet-4-6": { "input": 3.00, "output": 15.00, "cacheRead": 0.30, "cacheWrite": 3.75 },
    "claude-sonnet-4-5": { "input": 3.00, "output": 15.00, "cacheRead": 0.30, "cacheWrite": 3.75 },
    "claude-haiku-4-5":  { "input": 1.00, "output":  5.00, "cacheRead": 0.10, "cacheWrite": 1.25 }
  },
  "subscription_usd_monthly": { "claude": 200.00 }
}"#;

// ───────────────────────── pricing ─────────────────────────

/// One model's API list rates, in `$/M tokens`. Deserialized from the
/// camelCase keys (`cacheRead`/`cacheWrite`) used by `config/pricing.json`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRate {
    /// Input-token rate, USD per million.
    pub input: f64,
    /// Output-token rate, USD per million.
    pub output: f64,
    /// Cache-read rate, USD per million.
    pub cache_read: f64,
    /// Cache-write (creation) rate, USD per million.
    pub cache_write: f64,
}

/// The pricing table: per-model rates plus per-provider monthly subscription.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PricingTable {
    /// Model id → list rates. Keys may be exact (`claude-sonnet-4-6`) or prefix
    /// stems matched against suffixed ids (`claude-sonnet-4-5-20250929`).
    #[serde(default)]
    pub models: HashMap<String, ModelRate>,
    /// Provider → monthly subscription USD.
    #[serde(default)]
    pub subscription_usd_monthly: HashMap<String, f64>,
}

impl PricingTable {
    /// Resolve the rate for a model id. Exact match preferred; otherwise the
    /// first key that is a prefix of `model` (v3 `rateFor`). Returns `None` when
    /// unpriced — the caller leaves `cost_usd == 0`.
    ///
    /// The pricing schema uses mutually non-overlapping prefix stems (no key is a
    /// prefix of another), so prefix-match selection is order-independent.
    #[must_use]
    pub fn rate_for(&self, model: &str) -> Option<ModelRate> {
        if model.is_empty() {
            return None;
        }
        if let Some(r) = self.models.get(model) {
            return Some(*r);
        }
        self.models
            .iter()
            .find(|(k, _)| model.starts_with(k.as_str()))
            .map(|(_, r)| *r)
    }

    /// Monthly subscription for a provider, defaulting to `$200` (v3 default).
    #[must_use]
    pub fn subscription_for(&self, provider: &str) -> f64 {
        self.subscription_usd_monthly
            .get(provider)
            .copied()
            .unwrap_or(DEFAULT_SUBSCRIPTION_USD)
    }
}

/// Load the pricing table, mirroring v3 `loadPricing` resolution but adapted for
/// the daemon: `$SUBCTL_PRICING_FILE` → `~/.config/subctl/pricing.json` →
/// embedded defaults. A malformed external file falls through to the next source.
#[must_use]
pub fn load_pricing() -> PricingTable {
    if let Ok(p) = std::env::var("SUBCTL_PRICING_FILE") {
        if let Some(t) = read_pricing_file(Path::new(&p)) {
            return t;
        }
    }
    let home_default = PathBuf::from(format!("{}/.config/subctl/pricing.json", home()));
    if let Some(t) = read_pricing_file(&home_default) {
        return t;
    }
    // Embedded fallback — guaranteed valid, so this is the parity anchor.
    serde_json::from_str(DEFAULT_PRICING_JSON).unwrap_or_default()
}

/// Parse a pricing JSON file, returning `None` if it is missing or malformed.
fn read_pricing_file(path: &Path) -> Option<PricingTable> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<PricingTable>(&raw).ok()
}

// ───────────────────────── output shapes (v3-faithful) ─────────────────────────

/// Per-window token totals (v3 `TokenTotals`). Integer counts.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TokenTotals {
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache-read tokens.
    pub cache_read: u64,
    /// Cache-creation tokens.
    pub cache_write: u64,
}

/// One model's contribution within an account window (v3 `ModelBreakdown`).
#[derive(Debug, Clone, Serialize)]
pub struct ModelBreakdown {
    /// Model id as recorded in the transcript.
    pub model: String,
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache-read tokens.
    pub cache_read: u64,
    /// Cache-creation tokens.
    pub cache_write: u64,
    /// API list-price cost for this model's tokens (4 dp); `0` when unpriced.
    pub cost_usd: f64,
    /// Assistant turns counted for this model.
    pub turns: u64,
}

/// One account's cost summary for a window (v3 `AccountCostSummary`).
#[derive(Debug, Clone, Serialize)]
pub struct AccountCostSummary {
    /// Account alias (or `"default (~/.claude)"` for the unattributed bucket).
    pub alias: String,
    /// The account's config dir (transcript root parent).
    pub cfg_dir: String,
    /// `"today" | "week" | "month" | "all"`.
    pub window_label: String,
    /// Epoch-ms window start; `-1` for all-time.
    pub window_start_ms: i64,
    /// Per-model breakdown, sorted by `cost_usd` descending (stable).
    pub by_model: Vec<ModelBreakdown>,
    /// Summed tokens across all models (priced and unpriced).
    pub total_tokens: TokenTotals,
    /// API list-price cost for the window (4 dp).
    pub total_cost_usd: f64,
    /// Total assistant turns in the window.
    pub total_turns: u64,
    /// Monthly subscription USD attributed to this account.
    pub subscription_usd: f64,
    /// `total_cost_usd − subscription_usd` for the month window; `0` otherwise (4 dp).
    pub savings_usd: f64,
    /// Number of transcript files scanned (passed the mtime window gate).
    pub scanned_files: u64,
}

/// Bundle totals across the month window (v3 `CostBundle.totals`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct CostTotals {
    /// Sum of per-account `total_cost_usd` for the month (2 dp).
    pub api_cost_month_usd: f64,
    /// Sum of per-account `subscription_usd` (2 dp).
    pub subscription_total_usd: f64,
    /// Sum of per-account `savings_usd` for the month (2 dp).
    pub savings_month_usd: f64,
}

/// The `/api/state` `cost` field + `/api/evy/cost` body (v3 `CostBundle`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct CostBundle {
    /// Per-account summaries for the 30-day month window.
    pub this_month: Vec<AccountCostSummary>,
    /// Per-account summaries for the 7-day week window.
    pub this_week: Vec<AccountCostSummary>,
    /// Cross-account month totals.
    pub totals: CostTotals,
}

// ───────────────────────── core synthesis ─────────────────────────

/// Round to `dp` decimals the way JS `Number(x.toFixed(dp))` does (half away
/// from zero), so emitted numbers match the v3 dashboard.
fn round_dp(x: f64, dp: u32) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let f = 10f64.powi(dp as i32);
    (x * f).round() / f
}

/// Window start epoch-ms for a label (`"all"` → `-1`). Unknown labels fall back
/// to the month window, matching v3 `windowStartMs`.
fn window_start_ms(label: &str, now_ms: i64) -> i64 {
    match label {
        "all" => -1,
        "today" => now_ms - WINDOW_TODAY_MS,
        "week" => now_ms - WINDOW_WEEK_MS,
        _ => now_ms - WINDOW_MONTH_MS,
    }
}

/// List every `*.jsonl` under `<cfg_dir>/projects/*/` (v3 `listSessionJsonls`).
/// Best-effort: unreadable dirs are skipped silently.
fn list_session_jsonls(cfg_dir: &Path) -> Vec<PathBuf> {
    let projects = cfg_dir.join("projects");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return out;
    };
    for entry in entries.flatten() {
        let pdir = entry.path();
        if !std::fs::metadata(&pdir)
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        if let Ok(files) = std::fs::read_dir(&pdir) {
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// File modification time as epoch-ms, or `None` if unavailable.
fn file_mtime_ms(path: &Path) -> Option<i64> {
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    let dur = mtime.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(dur.as_millis()).ok()
}

/// Read a JSON number-ish field as `u64`, mirroring JS `Number(v ?? 0)` for the
/// non-negative integer token counts (`null`/absent/non-numeric → 0).
fn token_u64(usage: &serde_json::Value, key: &str) -> u64 {
    usage
        .get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        .unwrap_or(0)
}

/// Insertion-ordered model accumulator (mirrors JS `Map` iteration order so the
/// stable cost-descending sort breaks ties by first-seen, like v3).
#[derive(Default)]
struct ModelAccum {
    order: Vec<String>,
    by_model: HashMap<String, ModelBreakdown>,
}

impl ModelAccum {
    fn add(&mut self, model: &str, inp: u64, out: u64, cr: u64, cw: u64) {
        let entry = self.by_model.entry(model.to_string()).or_insert_with(|| {
            self.order.push(model.to_string());
            ModelBreakdown {
                model: model.to_string(),
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
                cost_usd: 0.0,
                turns: 0,
            }
        });
        entry.input += inp;
        entry.output += out;
        entry.cache_read += cr;
        entry.cache_write += cw;
        entry.turns += 1;
    }
}

/// Aggregate one account's transcripts for a window (port of `aggregateAccount`).
///
/// Walks `<cfg_dir>/projects/*/*.jsonl`, sums the `usage` block of each in-window
/// assistant turn per model, applies `pricing`, and returns the v3-shape summary.
#[must_use]
pub fn aggregate_account(
    alias: &str,
    cfg_dir: &Path,
    window: &str,
    now_ms: i64,
    pricing: &PricingTable,
    subscription_usd: f64,
) -> AccountCostSummary {
    let wstart = window_start_ms(window, now_ms);
    let mut accum = ModelAccum::default();
    let mut scanned: u64 = 0;

    for file in list_session_jsonls(cfg_dir) {
        // Fast-skip transcripts whose mtime predates the window start.
        if wstart != -1 {
            match file_mtime_ms(&file) {
                Some(m) if m < wstart => continue,
                None => continue, // unstatable → skip (matches v3 `try/catch continue`)
                _ => {}
            }
        }
        scanned += 1;
        let Ok(raw) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in raw.split('\n') {
            let t = line.trim();
            if t.is_empty() || !t.contains("\"usage\"") {
                continue;
            }
            let Ok(obj) = serde_json::from_str::<serde_json::Value>(t) else {
                continue;
            };
            // Per-line timestamp filter (v3): drop turns older than the window.
            if wstart != -1 {
                if let Some(ts) = obj.get("timestamp").and_then(|v| v.as_str()) {
                    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                        if dt.timestamp_millis() < wstart {
                            continue;
                        }
                    }
                }
            }
            let usage = obj
                .pointer("/message/usage")
                .filter(|v| !v.is_null())
                .or_else(|| obj.get("usage").filter(|v| !v.is_null()));
            let Some(usage) = usage else { continue };
            let model = obj
                .pointer("/message/model")
                .and_then(|v| v.as_str())
                .or_else(|| obj.get("model").and_then(|v| v.as_str()))
                .unwrap_or("unknown");
            let inp = token_u64(usage, "input_tokens");
            let out = token_u64(usage, "output_tokens");
            let cr = token_u64(usage, "cache_read_input_tokens");
            let cw = token_u64(usage, "cache_creation_input_tokens");
            if inp + out + cr + cw == 0 {
                continue;
            }
            accum.add(model, inp, out, cr, cw);
        }
    }

    // Apply pricing: per-model rounded cost; total accumulates the unrounded sum.
    let mut total_cost = 0.0f64;
    for model in &accum.order {
        let entry = accum.by_model.get_mut(model).expect("present in order");
        if let Some(rate) = pricing.rate_for(&entry.model) {
            let cost = entry.input as f64 * rate.input / 1_000_000.0
                + entry.output as f64 * rate.output / 1_000_000.0
                + entry.cache_read as f64 * rate.cache_read / 1_000_000.0
                + entry.cache_write as f64 * rate.cache_write / 1_000_000.0;
            entry.cost_usd = round_dp(cost, 4);
            total_cost += cost;
        }
    }

    // Build breakdown in first-seen order, then stable sort by cost desc.
    let mut by_model: Vec<ModelBreakdown> = accum
        .order
        .iter()
        .map(|m| accum.by_model.get(m).cloned().expect("present in order"))
        .collect();
    by_model.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let total_tokens = by_model
        .iter()
        .fold(TokenTotals::default(), |acc, m| TokenTotals {
            input: acc.input + m.input,
            output: acc.output + m.output,
            cache_read: acc.cache_read + m.cache_read,
            cache_write: acc.cache_write + m.cache_write,
        });
    let total_turns: u64 = by_model.iter().map(|m| m.turns).sum();
    let savings = if window == "month" {
        total_cost - subscription_usd
    } else {
        0.0
    };

    AccountCostSummary {
        alias: alias.to_string(),
        cfg_dir: cfg_dir.to_string_lossy().into_owned(),
        window_label: window.to_string(),
        window_start_ms: wstart,
        by_model,
        total_tokens,
        total_cost_usd: round_dp(total_cost, 4),
        total_turns,
        subscription_usd,
        savings_usd: round_dp(savings, 4),
        scanned_files: scanned,
    }
}

/// Multi-account walk for one window (port of `aggregateAll`). Reads claude rows
/// from `accounts_conf`, then prepends the unattributed `default (~/.claude)`
/// bucket (rooted at `home/.claude`) when it isn't already a configured account
/// and has ≥1 turn.
#[must_use]
pub fn aggregate_all(
    window: &str,
    now_ms: i64,
    accounts_conf: &Path,
    home_dir: &Path,
    pricing: &PricingTable,
) -> Vec<AccountCostSummary> {
    let rows = evy_providers::AccountsStore::open(accounts_conf)
        .and_then(|s| s.list_rows())
        .unwrap_or_default();
    let claude_rows: Vec<_> = rows
        .into_iter()
        .filter(|r| r.provider == "claude")
        .collect();

    let subscription = pricing.subscription_for("claude");
    let mut result: Vec<AccountCostSummary> = claude_rows
        .iter()
        .map(|r| {
            aggregate_account(
                &r.alias,
                &r.config_dir,
                window,
                now_ms,
                pricing,
                subscription,
            )
        })
        .collect();

    let default_dir = home_dir.join(".claude");
    let already_account = claude_rows.iter().any(|r| r.config_dir == default_dir);
    if !already_account && default_dir.join("projects").is_dir() {
        let def = aggregate_account(
            "default (~/.claude)",
            &default_dir,
            window,
            now_ms,
            pricing,
            0.0,
        );
        if def.total_turns > 0 {
            result.insert(0, def);
        }
    }
    result
}

/// Assemble the full bundle (port of `buildCostBundle`, sans cache).
#[must_use]
pub fn build_bundle(
    now_ms: i64,
    accounts_conf: &Path,
    home_dir: &Path,
    pricing: &PricingTable,
) -> CostBundle {
    let this_month = aggregate_all("month", now_ms, accounts_conf, home_dir, pricing);
    let this_week = aggregate_all("week", now_ms, accounts_conf, home_dir, pricing);

    let mut api = 0.0;
    let mut sub = 0.0;
    let mut sav = 0.0;
    for r in &this_month {
        api += r.total_cost_usd;
        sub += r.subscription_usd;
        sav += r.savings_usd;
    }
    let totals = CostTotals {
        api_cost_month_usd: round_dp(api, 2),
        subscription_total_usd: round_dp(sub, 2),
        savings_month_usd: round_dp(sav, 2),
    };

    CostBundle {
        this_month,
        this_week,
        totals,
    }
}

// ───────────────────────── env paths + cache ─────────────────────────

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

fn accounts_conf_path() -> PathBuf {
    std::env::var("SUBCTL_ACCOUNTS_CONF")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{}/.config/subctl/accounts.conf", home())))
}

/// 5-minute cache, mirroring v3 `_costCache`. `(built_at_ms, bundle)`.
static COST_CACHE: Mutex<Option<(i64, CostBundle)>> = Mutex::new(None);

/// Cached bundle if built within the TTL relative to `now_ms`.
fn cached(now_ms: i64) -> Option<CostBundle> {
    let guard = COST_CACHE.lock().ok()?;
    let (built, bundle) = guard.as_ref()?;
    if now_ms >= *built && now_ms - *built < COST_CACHE_TTL_MS {
        Some(bundle.clone())
    } else {
        None
    }
}

/// Store the freshly built bundle.
fn store(now_ms: i64, bundle: &CostBundle) {
    if let Ok(mut guard) = COST_CACHE.lock() {
        *guard = Some((now_ms, bundle.clone()));
    }
}

/// Build the cost bundle from the live environment, with the 5-minute cache. The
/// transcript walk is heavy (Jason has 80K+ turns), so it runs on a blocking
/// thread and only re-walks once per TTL.
pub async fn build_cost_bundle(now_ms: i64) -> CostBundle {
    if let Some(b) = cached(now_ms) {
        return b;
    }
    let bundle = tokio::task::spawn_blocking(move || {
        let pricing = load_pricing();
        let conf = accounts_conf_path();
        let home_dir = PathBuf::from(home());
        build_bundle(now_ms, &conf, &home_dir, &pricing)
    })
    .await
    .unwrap_or_default();
    store(now_ms, &bundle);
    bundle
}

/// The cost bundle as a JSON value for the `/api/state` `cost` seam. Returns
/// `Null` only if serialization fails (it cannot for this shape).
pub(crate) async fn cost_value(now_ms: i64) -> serde_json::Value {
    serde_json::to_value(build_cost_bundle(now_ms).await).unwrap_or(serde_json::Value::Null)
}

/// `GET /api/evy/cost` → the v3 `CostBundle` (`{ this_month, this_week, totals }`).
pub(crate) async fn cost_handler() -> Json<CostBundle> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    Json(build_cost_bundle(now_ms).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, SystemTime};

    fn tmpdir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "evy-cost-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Pricing table matching the embedded defaults (opus/sonnet/haiku + $200).
    fn pricing() -> PricingTable {
        serde_json::from_str(DEFAULT_PRICING_JSON).unwrap()
    }

    /// Write a transcript file under `<cfg>/projects/<proj>/<name>.jsonl`.
    fn write_jsonl(cfg: &Path, proj: &str, name: &str, lines: &[&str]) -> PathBuf {
        let dir = cfg.join("projects").join(proj);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.jsonl"));
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    /// An assistant-turn line with a `usage` block at `message.usage`.
    fn turn(model: &str, ts: &str, inp: u64, out: u64, cr: u64, cw: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","message":{{"model":"{model}","usage":{{"input_tokens":{inp},"output_tokens":{out},"cache_read_input_tokens":{cr},"cache_creation_input_tokens":{cw}}}}}}}"#
        )
    }

    fn now() -> i64 {
        // Fixed reference instant used across tests (2026-06-01T00:00:00Z).
        1_780_272_000_000
    }

    fn iso(ms_before_now: i64) -> String {
        let dt = chrono::DateTime::from_timestamp_millis(now() - ms_before_now).unwrap();
        dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    #[test]
    fn round_dp_matches_tofixed() {
        assert_eq!(round_dp(13446.81784, 4), 13446.8178);
        assert_eq!(round_dp(0.00005, 4), 0.0001); // half away from zero
        assert_eq!(round_dp(282.6774, 2), 282.68);
        assert_eq!(round_dp(-12.345, 2), -12.35);
    }

    #[test]
    fn rate_for_exact_prefix_and_none() {
        let p = pricing();
        assert_eq!(p.rate_for("claude-opus-4-7").unwrap().input, 5.0);
        // suffixed id matches the prefix stem
        assert_eq!(
            p.rate_for("claude-sonnet-4-5-20250929").unwrap().output,
            15.0
        );
        // unpriced model → None (cost stays 0)
        assert!(p.rate_for("claude-opus-4-8").is_none());
        assert!(p.rate_for("").is_none());
    }

    #[test]
    fn subscription_default_when_absent() {
        let p = pricing();
        assert_eq!(p.subscription_for("claude"), 200.0);
        assert_eq!(p.subscription_for("openai-codex"), 200.0); // default
    }

    #[test]
    fn aggregate_account_costs_tokens_turns() {
        let cfg = tmpdir("acct");
        // opus turn: 1M input, 1M output, 1M cacheRead, 1M cacheWrite.
        // cost = 5 + 25 + 0.5 + 6.25 = 36.75
        write_jsonl(
            &cfg,
            "proj-a",
            "s1",
            &[
                &turn(
                    "claude-opus-4-7",
                    &iso(1000),
                    1_000_000,
                    1_000_000,
                    1_000_000,
                    1_000_000,
                ),
                // unpriced model — tokens/turns count, cost stays 0
                &turn("claude-opus-4-8", &iso(2000), 500_000, 0, 0, 0),
            ],
        );
        let s = aggregate_account("claude-x", &cfg, "month", now(), &pricing(), 200.0);
        assert_eq!(s.total_turns, 2);
        assert_eq!(s.by_model.len(), 2);
        // priced model is first (cost desc); unpriced (cost 0) last
        assert_eq!(s.by_model[0].model, "claude-opus-4-7");
        assert_eq!(s.by_model[0].cost_usd, 36.75);
        assert_eq!(s.by_model[1].cost_usd, 0.0);
        assert_eq!(s.total_cost_usd, 36.75);
        // total_tokens includes the unpriced model's input
        assert_eq!(s.total_tokens.input, 1_500_000);
        assert_eq!(s.savings_usd, round_dp(36.75 - 200.0, 4));
        assert_eq!(s.scanned_files, 1);
        let _ = std::fs::remove_dir_all(&cfg);
    }

    #[test]
    fn malformed_and_non_usage_lines_skipped() {
        let cfg = tmpdir("malformed");
        write_jsonl(
            &cfg,
            "p",
            "s",
            &[
                "",
                "not json at all",
                r#"{"no":"usage here"}"#,
                "{ broken json \"usage\"", // contains "usage" but unparseable
                r#"{"message":{"model":"claude-opus-4-7","usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#, // sum 0 → skip
                &turn("claude-opus-4-7", &iso(1000), 1_000_000, 0, 0, 0), // counts
            ],
        );
        let s = aggregate_account("a", &cfg, "month", now(), &pricing(), 200.0);
        assert_eq!(s.total_turns, 1);
        assert_eq!(s.total_tokens.input, 1_000_000);
        let _ = std::fs::remove_dir_all(&cfg);
    }

    #[test]
    fn top_level_usage_and_model_fallback() {
        let cfg = tmpdir("toplevel");
        // usage + model at the top level (not under message)
        write_jsonl(
            &cfg,
            "p",
            "s",
            &[
                r#"{"timestamp":"2026-05-30T00:00:00.000Z","model":"claude-haiku-4-5","usage":{"input_tokens":1000000,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#,
            ],
        );
        let s = aggregate_account("a", &cfg, "month", now(), &pricing(), 200.0);
        assert_eq!(s.by_model.len(), 1);
        assert_eq!(s.by_model[0].model, "claude-haiku-4-5");
        assert_eq!(s.by_model[0].cost_usd, 1.0); // 1M input * $1/M
        let _ = std::fs::remove_dir_all(&cfg);
    }

    #[test]
    fn per_line_timestamp_filters_out_of_window() {
        let cfg = tmpdir("tsfilter");
        // One in-window turn, one before the week window (8 days ago).
        write_jsonl(
            &cfg,
            "p",
            "s",
            &[
                &turn(
                    "claude-opus-4-7",
                    &iso(WINDOW_TODAY_MS / 2),
                    1_000_000,
                    0,
                    0,
                    0,
                ),
                &turn(
                    "claude-opus-4-7",
                    &iso(WINDOW_WEEK_MS + WINDOW_TODAY_MS),
                    9_000_000,
                    0,
                    0,
                    0,
                ),
            ],
        );
        let week = aggregate_account("a", &cfg, "week", now(), &pricing(), 200.0);
        assert_eq!(week.total_turns, 1); // old line excluded
        assert_eq!(week.total_tokens.input, 1_000_000);
        let month = aggregate_account("a", &cfg, "month", now(), &pricing(), 200.0);
        assert_eq!(month.total_turns, 2); // both within 30d
        let _ = std::fs::remove_dir_all(&cfg);
    }

    #[test]
    fn mtime_gate_skips_stale_files_from_scanned() {
        let cfg = tmpdir("mtime");
        let fresh = write_jsonl(
            &cfg,
            "p",
            "fresh",
            &[&turn("claude-opus-4-7", &iso(1000), 1_000_000, 0, 0, 0)],
        );
        let stale = write_jsonl(
            &cfg,
            "p",
            "stale",
            &[&turn("claude-opus-4-7", &iso(1000), 7_000_000, 0, 0, 0)],
        );
        // Backdate the stale file's mtime to 40 days ago (before the month window).
        let old = SystemTime::now() - Duration::from_secs(40 * 24 * 60 * 60);
        std::fs::File::open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();
        // Use real "now" so the backdated mtime sorts before the window.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let s = aggregate_account("a", &cfg, "month", now_ms, &pricing(), 200.0);
        assert_eq!(s.scanned_files, 1, "stale file must be skipped from scan");
        // The fresh file's line uses iso(1000) anchored at the test `now()`, far in
        // the past relative to real now — its per-line ts is out of window, so 0 turns,
        // but the fresh file itself was still scanned. Assert scanned, not turns.
        assert!(fresh.exists());
        let _ = std::fs::remove_dir_all(&cfg);
    }

    #[test]
    fn all_window_uses_negative_one_start() {
        let cfg = tmpdir("allwin");
        write_jsonl(
            &cfg,
            "p",
            "s",
            &[&turn(
                "claude-opus-4-7",
                "2000-01-01T00:00:00.000Z",
                1_000_000,
                0,
                0,
                0,
            )],
        );
        let s = aggregate_account("a", &cfg, "all", now(), &pricing(), 200.0);
        assert_eq!(s.window_start_ms, -1);
        assert_eq!(s.total_turns, 1); // ancient line still counted for all-time
        assert_eq!(s.savings_usd, 0.0); // savings only for month
        let _ = std::fs::remove_dir_all(&cfg);
    }

    fn write_accounts_conf(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("accounts.conf");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn aggregate_all_claude_only_and_default_bucket_prepended() {
        let root = tmpdir("all");
        let home = root.join("home");
        let jason = root.join("cfg-jason");
        std::fs::create_dir_all(&home).unwrap();
        // configured claude account + a codex account (must be excluded)
        write_jsonl(
            &jason,
            "p",
            "s",
            &[&turn("claude-opus-4-7", &iso(1000), 1_000_000, 0, 0, 0)],
        );
        let codex = root.join("cfg-codex");
        write_jsonl(
            &codex,
            "p",
            "s",
            &[&turn("claude-opus-4-7", &iso(1000), 5_000_000, 0, 0, 0)],
        );
        // default ~/.claude bucket (under home) with usage
        let default_dir = home.join(".claude");
        write_jsonl(
            &default_dir,
            "p",
            "s",
            &[&turn("claude-haiku-4-5", &iso(1000), 1_000_000, 0, 0, 0)],
        );

        let conf = write_accounts_conf(
            &root,
            &format!(
                "claude-jason|claude|j@x.com|{}\nopenai-jason|openai-codex|o@x.com|{}\n",
                jason.display(),
                codex.display()
            ),
        );
        let rows = aggregate_all("month", now(), &conf, &home, &pricing());
        // default bucket first, then claude-jason; codex excluded → len 2
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].alias, "default (~/.claude)");
        assert_eq!(rows[0].subscription_usd, 0.0);
        assert_eq!(rows[1].alias, "claude-jason");
        assert_eq!(rows[1].subscription_usd, 200.0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_bucket_suppressed_when_already_account() {
        let root = tmpdir("dedup");
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let default_dir = home.join(".claude");
        write_jsonl(
            &default_dir,
            "p",
            "s",
            &[&turn("claude-opus-4-7", &iso(1000), 1_000_000, 0, 0, 0)],
        );
        // accounts.conf points an account AT ~/.claude → no separate default bucket
        let conf = write_accounts_conf(
            &root,
            &format!("claude-main|claude|m@x.com|{}\n", default_dir.display()),
        );
        let rows = aggregate_all("month", now(), &conf, &home, &pricing());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].alias, "claude-main");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn build_bundle_totals_round_to_two_dp() {
        let root = tmpdir("bundle");
        let home = root.join("home");
        let cfg = root.join("cfg");
        std::fs::create_dir_all(&home).unwrap();
        // 1M of each token type on opus → 36.75 cost; savings 36.75-200 = -163.25
        write_jsonl(
            &cfg,
            "p",
            "s",
            &[&turn(
                "claude-opus-4-7",
                &iso(1000),
                1_000_000,
                1_000_000,
                1_000_000,
                1_000_000,
            )],
        );
        let conf = write_accounts_conf(
            &root,
            &format!("claude-a|claude|a@x.com|{}\n", cfg.display()),
        );
        let bundle = build_bundle(now(), &conf, &home, &pricing());
        assert_eq!(bundle.this_month.len(), 1);
        assert_eq!(bundle.totals.api_cost_month_usd, 36.75);
        assert_eq!(bundle.totals.subscription_total_usd, 200.0);
        assert_eq!(bundle.totals.savings_month_usd, -163.25);
        // week window present and shape-valid (savings 0 outside month)
        assert_eq!(bundle.this_week.len(), 1);
        assert_eq!(bundle.this_week[0].savings_usd, 0.0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn embedded_pricing_parses() {
        let p: PricingTable = serde_json::from_str(DEFAULT_PRICING_JSON).unwrap();
        assert!(p.models.contains_key("claude-opus-4-7"));
        assert_eq!(p.subscription_for("claude"), 200.0);
    }
}
