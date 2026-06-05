//! Phase 1 slice 1c — per-account usage cache (3-layer), ported from v3
//! `subctlUsageFetchAll`/`subctlUsageApplyFallback` (dashboard/server.ts:600-810).
//!
//! Layers:
//!   L1 in-process TTL cache (5min) — on a hit, stale rows get `stale_age_ms`
//!      ticked up by elapsed so the UI indicator advances inside the window.
//!   L2 per-alias last-good map — on a failed/missing alias, substitute the last
//!      successful entry tagged `stale` (real numbers + "·stale Xm").
//!   L3 429 backoff — base 5min, ×2 per consecutive 429, cap 30min; an empty
//!      array is NOT a success (don't clear backoff); `force` bypasses the
//!      short-circuit (operator /api/refresh).
//!
//! Source of truth for the raw fetch is the `subctl usage --json` CLI (which does
//! the OAuth/keychain → `api.anthropic.com/api/oauth/usage` call; claude-only —
//! non-claude accounts get synthesized fallback rows only). The CLI is a separate
//! tool from the dashboard/master being retired, so shelling it stays valid.
//! Override the binary with `EVY_SUBCTL_BIN`.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const TTL_MS: u64 = 5 * 60 * 1000;
const BACKOFF_BASE_MS: u64 = 5 * 60 * 1000;
const BACKOFF_CAP_MS: u64 = 30 * 60 * 1000;
const FETCH_TIMEOUT: Duration = Duration::from_secs(12);

/// One usage window (`five_hour` / `seven_day` / …). `{utilization, resets_at}`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageWindow {
    #[serde(default)]
    pub utilization: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<String>,
}

/// Per-account usage payload, mirroring v3 `UsageEntry`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageEntry {
    #[serde(default)]
    pub five_hour: Option<UsageWindow>,
    #[serde(default)]
    pub seven_day: Option<UsageWindow>,
    #[serde(default)]
    pub seven_day_sonnet: Option<UsageWindow>,
    #[serde(default)]
    pub seven_day_opus: Option<UsageWindow>,
    #[serde(default)]
    pub extra_usage: Option<serde_json::Value>,
}

/// One alias's fetch result, mirroring v3 `AccountUsageResult` (the JSON shape
/// emitted by `subctl usage --json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountUsageResult {
    pub alias: String,
    #[serde(default)]
    pub cfg_dir: String,
    pub ok: bool,
    #[serde(default)]
    pub usage: Option<UsageEntry>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub stale: Option<bool>,
    #[serde(default)]
    pub stale_age_ms: Option<u64>,
}

#[derive(Default)]
struct CacheState {
    /// (fetched_at_ms, data)
    cache: Option<(u64, Vec<AccountUsageResult>)>,
    /// alias -> (last successful entry, at_ms)
    last_good: HashMap<String, (AccountUsageResult, u64)>,
    backoff_until_ms: u64,
    consecutive_429: u32,
}

/// The 3-layer usage cache. One per daemon (see [`instance`]).
pub struct UsageCache {
    state: Mutex<CacheState>,
    subctl_bin: String,
}

/// Exponential backoff: `min(BASE * 2^(consecutive-1), CAP)`. v3 increments the
/// counter BEFORE computing, so consecutive=1 → 5min, 2 → 10min, … cap 30min.
fn backoff_ms(consecutive: u32) -> u64 {
    let k = consecutive.saturating_sub(1);
    let factor = 2u64.checked_pow(k).unwrap_or(u64::MAX);
    BACKOFF_BASE_MS.saturating_mul(factor).min(BACKOFF_CAP_MS)
}

/// Substitute last-good (tagged stale) for failed/missing aliases; update
/// last-good for fresh successes. Ports `subctlUsageApplyFallback` incl. Fix 2
/// (synthesize rows for cached aliases the fresh fetch omitted entirely).
fn apply_fallback(
    last_good: &mut HashMap<String, (AccountUsageResult, u64)>,
    parsed: Vec<AccountUsageResult>,
    now_ms: u64,
) -> Vec<AccountUsageResult> {
    let mut out: Vec<AccountUsageResult> = parsed
        .into_iter()
        .map(|u| {
            if u.ok && u.usage.is_some() {
                last_good.insert(u.alias.clone(), (u.clone(), now_ms));
                return u;
            }
            if let Some((entry, at_ms)) = last_good.get(&u.alias) {
                return AccountUsageResult {
                    alias: u.alias,
                    cfg_dir: u.cfg_dir,
                    ok: false,
                    usage: entry.usage.clone(),
                    error: u.error,
                    stale: Some(true),
                    stale_age_ms: Some(now_ms.saturating_sub(*at_ms)),
                };
            }
            u
        })
        .collect();

    let present: HashSet<String> = out.iter().map(|u| u.alias.clone()).collect();
    for (alias, (entry, at_ms)) in last_good.iter() {
        if !present.contains(alias) {
            out.push(AccountUsageResult {
                alias: alias.clone(),
                cfg_dir: entry.cfg_dir.clone(),
                ok: false,
                usage: entry.usage.clone(),
                error: Some("alias missing from fresh fetch".to_string()),
                stale: Some(true),
                stale_age_ms: Some(now_ms.saturating_sub(*at_ms)),
            });
        }
    }
    out
}

/// Count fresh 429s (raw parsed, not post-fallback) — a `\b429\b` in `error`.
fn count_429s(parsed: &[AccountUsageResult]) -> u32 {
    parsed
        .iter()
        .filter(|u| {
            !u.ok
                && u.error
                    .as_deref()
                    .is_some_and(|e| e.split(|c: char| !c.is_ascii_digit()).any(|t| t == "429"))
        })
        .count() as u32
}

/// Apply TTL stale-age tick to a cached snapshot on a cache-hit read.
fn tick_stale_age(data: &[AccountUsageResult], elapsed_ms: u64) -> Vec<AccountUsageResult> {
    if elapsed_ms == 0 {
        return data.to_vec();
    }
    data.iter()
        .map(|u| {
            if u.stale == Some(true) {
                let mut c = u.clone();
                c.stale_age_ms = Some(c.stale_age_ms.unwrap_or(0) + elapsed_ms);
                c
            } else {
                u.clone()
            }
        })
        .collect()
}

impl UsageCache {
    #[must_use]
    pub fn new() -> Self {
        let subctl_bin = std::env::var("EVY_SUBCTL_BIN")
            .unwrap_or_else(|_| "/Users/sem/code/subctl/bin/subctl".to_string());
        Self {
            state: Mutex::new(CacheState::default()),
            subctl_bin,
        }
    }

    /// Fetch all accounts' usage, honoring TTL + backoff. `force` (operator
    /// /api/refresh) bypasses both short-circuits. `now_ms` is unix-millis.
    pub async fn fetch_all(&self, now_ms: u64, force: bool) -> Vec<AccountUsageResult> {
        // L1 TTL hit + L3 backoff short-circuit — both under one lock, no await.
        {
            let st = self.state.lock().expect("usage cache lock");
            if !force {
                if let Some((fetched_at, data)) = &st.cache {
                    if now_ms.saturating_sub(*fetched_at) < TTL_MS {
                        return tick_stale_age(data, now_ms.saturating_sub(*fetched_at));
                    }
                }
                if now_ms < st.backoff_until_ms {
                    // Backed off: re-run fallback against last-good (empty parsed)
                    // so stale_age_ms recomputes every call (v3 Fix 3).
                    let mut lg = st.last_good.clone();
                    return apply_fallback(&mut lg, Vec::new(), now_ms);
                }
            }
        } // lock dropped before the async shell-out

        let (parsed, fetch_succeeded) = self.shell_fetch().await;

        let mut st = self.state.lock().expect("usage cache lock");
        let with_fallback = apply_fallback(&mut st.last_good, parsed.clone(), now_ms);

        let n429 = count_429s(&parsed);
        if n429 > 0 {
            st.consecutive_429 += 1;
            st.backoff_until_ms = now_ms + backoff_ms(st.consecutive_429);
        } else if fetch_succeeded && (st.consecutive_429 > 0 || st.backoff_until_ms > 0) {
            st.consecutive_429 = 0;
            st.backoff_until_ms = 0;
        }

        st.cache = Some((now_ms, with_fallback.clone()));
        with_fallback
    }

    /// Run `subctl usage --json` with a 12s timeout. Returns (parsed, succeeded).
    /// A non-empty parsed array is the only "success" (empty = silent CLI fail →
    /// don't clear backoff). Hard errors leave parsed empty + succeeded=false.
    async fn shell_fetch(&self) -> (Vec<AccountUsageResult>, bool) {
        let run = tokio::process::Command::new(&self.subctl_bin)
            .args(["usage", "--json"])
            .output();
        let output = match tokio::time::timeout(FETCH_TIMEOUT, run).await {
            Ok(Ok(o)) if o.status.success() => o,
            _ => return (Vec::new(), false),
        };
        match serde_json::from_slice::<Vec<AccountUsageResult>>(&output.stdout) {
            Ok(parsed) => {
                let ok = !parsed.is_empty();
                (parsed, ok)
            }
            Err(_) => (Vec::new(), false),
        }
    }
}

impl Default for UsageCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide singleton (mirrors v3's module-global cache).
pub fn instance() -> &'static UsageCache {
    use std::sync::OnceLock;
    static CACHE: OnceLock<UsageCache> = OnceLock::new();
    CACHE.get_or_init(UsageCache::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(alias: &str, ok: bool, util: Option<f64>, err: Option<&str>) -> AccountUsageResult {
        AccountUsageResult {
            alias: alias.to_string(),
            cfg_dir: format!("/cfg/{alias}"),
            ok,
            usage: util.map(|u| UsageEntry {
                seven_day: Some(UsageWindow { utilization: Some(u), resets_at: None }),
                ..Default::default()
            }),
            error: err.map(String::from),
            stale: None,
            stale_age_ms: None,
        }
    }

    #[test]
    fn backoff_is_exponential_capped() {
        assert_eq!(backoff_ms(1), 5 * 60 * 1000); // k=0 → 5min
        assert_eq!(backoff_ms(2), 10 * 60 * 1000); // k=1 → 10min
        assert_eq!(backoff_ms(3), 20 * 60 * 1000); // k=2 → 20min
        assert_eq!(backoff_ms(4), 30 * 60 * 1000); // k=3 → 40min capped to 30
        assert_eq!(backoff_ms(99), 30 * 60 * 1000); // cap holds, no overflow
    }

    #[test]
    fn count_429s_matches_word_boundary() {
        let p = vec![
            entry("a", false, None, Some("HTTP 429 Too Many Requests")),
            entry("b", false, None, Some("network 4290 error")), // not a 429 token
            entry("c", true, Some(10.0), None),
        ];
        assert_eq!(count_429s(&p), 1);
    }

    #[test]
    fn fallback_substitutes_last_good_tagged_stale() {
        let mut lg = HashMap::new();
        // first: a succeeds → recorded as last-good
        let out = apply_fallback(&mut lg, vec![entry("a", true, Some(42.0), None)], 1000);
        assert!(out[0].ok && out[0].usage.is_some());
        assert!(lg.contains_key("a"));
        // next: a fails (429) → substitute last-good, tagged stale, real numbers
        let out2 = apply_fallback(&mut lg, vec![entry("a", false, None, Some("429"))], 4000);
        assert_eq!(out2.len(), 1);
        assert_eq!(out2[0].ok, false);
        assert_eq!(out2[0].stale, Some(true));
        assert_eq!(out2[0].stale_age_ms, Some(3000));
        assert_eq!(out2[0].usage.as_ref().unwrap().seven_day.as_ref().unwrap().utilization, Some(42.0));
    }

    #[test]
    fn fallback_synthesizes_omitted_cached_alias() {
        let mut lg = HashMap::new();
        apply_fallback(&mut lg, vec![entry("a", true, Some(5.0), None)], 1000);
        // fresh fetch omits "a" entirely → synthesized stale row appears
        let out = apply_fallback(&mut lg, vec![entry("b", true, Some(9.0), None)], 2000);
        let aliases: HashSet<_> = out.iter().map(|u| u.alias.as_str()).collect();
        assert!(aliases.contains("a") && aliases.contains("b"));
        let a = out.iter().find(|u| u.alias == "a").unwrap();
        assert_eq!(a.stale, Some(true));
    }

    #[test]
    fn tick_stale_age_advances_only_stale_rows() {
        let mut e = entry("a", false, Some(1.0), None);
        e.stale = Some(true);
        e.stale_age_ms = Some(1000);
        let fresh = entry("b", true, Some(2.0), None);
        let out = tick_stale_age(&[e, fresh], 500);
        assert_eq!(out[0].stale_age_ms, Some(1500)); // stale advanced
        assert_eq!(out[1].stale_age_ms, None); // fresh untouched
    }
}
