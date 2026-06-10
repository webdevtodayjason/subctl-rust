//! Phase 1 slice 1d — rate-limit + usage-history 24h buckets, ported from v3
//! `readUsageHistory24h` (server.ts:913-944) and `buildRateLimits` (1324-1416).
//!
//! Two independent 24-hour bucket systems, both oldest→newest (`idx = 23 - hoursAgo`):
//!   • **usage-history buckets** — per-alias hourly max of 5h/7d utilization, read
//!     from `~/.config/subctl/cache/usage-history.jsonl` (written each usage poll).
//!   • **rate-limit-event buckets** — per-account hourly count of 429/529 events,
//!     read from `~/.claude/rate-limit-events.log`, plus `today_total` and
//!     `recent_429_count` (429s within the trailing 2h window that drives verdict).

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

const HOUR_MS: i64 = 60 * 60 * 1000;
const DAY_MS: i64 = 24 * HOUR_MS;
/// Trailing window (sec) for `recent_429_count` — v3 `RL_RECENT_WINDOW_SEC` (2h).
const RL_RECENT_WINDOW_SEC: i64 = 2 * 60 * 60;

// ─── usage-history buckets (utilization over time) ───────────────────────────

/// One hour-bucket of usage history: the max utilization observed that hour.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageBucket {
    /// Max 5-hour utilization % seen this hour (None if no samples).
    pub five_hour_max: Option<f64>,
    /// Max 7-day utilization % seen this hour (None if no samples).
    pub seven_day_max: Option<f64>,
    /// Number of samples recorded this hour.
    pub samples: u32,
}

#[derive(Debug, Deserialize)]
struct HistoryEntry {
    ts: i64,
    alias: String,
    #[serde(default)]
    five_hour: Option<f64>,
    #[serde(default)]
    seven_day: Option<f64>,
}

/// Port of `readUsageHistory24h`. 24 buckets/alias (oldest [0] → current [23]);
/// per bucket: max 5h util, max 7d util, sample count. Entries older than 24h or
/// in the future are dropped.
#[must_use]
pub fn read_usage_history_24h(path: &Path, now_ms: i64) -> HashMap<String, Vec<UsageBucket>> {
    let mut buckets: HashMap<String, Vec<UsageBucket>> = HashMap::new();
    let cutoff = now_ms - DAY_MS;
    let current_hour = now_ms.div_euclid(HOUR_MS);
    let Ok(raw) = std::fs::read_to_string(path) else {
        return buckets;
    };
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(e) = serde_json::from_str::<HistoryEntry>(line) else {
            continue;
        };
        if e.ts < cutoff {
            continue;
        }
        let hours_ago = current_hour - e.ts.div_euclid(HOUR_MS);
        if !(0..24).contains(&hours_ago) {
            continue;
        }
        let idx = (23 - hours_ago) as usize;
        let slots = buckets
            .entry(e.alias.clone())
            .or_insert_with(|| vec![UsageBucket::default(); 24]);
        let slot = &mut slots[idx];
        slot.samples += 1;
        if let Some(fh) = e.five_hour {
            slot.five_hour_max = Some(slot.five_hour_max.map_or(fh, |m| m.max(fh)));
        }
        if let Some(sd) = e.seven_day {
            slot.seven_day_max = Some(slot.seven_day_max.map_or(sd, |m| m.max(sd)));
        }
    }
    buckets
}

// ─── rate-limit-event buckets (429/529 counts) ───────────────────────────────

/// Per-account rate-limit event tally.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RlAccount {
    /// Count of rate-limit events for this account today.
    pub count_today: u32,
    /// 24 hourly counts of rate-limit events, oldest [0] → current [23].
    pub buckets_24h: Vec<u32>,
}

impl Default for RlAccount {
    fn default() -> Self {
        Self {
            count_today: 0,
            buckets_24h: vec![0; 24],
        }
    }
}

/// Aggregated rate-limit data across all accounts (the `/api/state.rate_limits`
/// + `/api/evy/rate-limits` source).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RlData {
    /// Total rate-limit events across all accounts today.
    pub today_total: u32,
    /// Count of 429s within the trailing 2h window (drives verdict).
    pub recent_429_count: u32,
    /// Per-account tallies, keyed by alias.
    pub by_account: HashMap<String, RlAccount>,
}

/// Local calendar date `YYYY-MM-DD` (matches v3 `todayDateStr`, local tz).
#[must_use]
pub fn today_date_str() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Port of `buildRateLimits` (session-id resolution omitted — v3 passes an empty
/// `sidToAlias` to /api/state, so accounts resolve from the event's explicit
/// `account`/`alias` field only). Counts 429+529 events per account: `count_today`
/// (event.date == today), hourly `buckets_24h`, plus global `today_total` and
/// `recent_429_count` (429s with age ≤ 2h).
#[must_use]
pub fn build_rate_limits(rl_log: &Path, aliases: &[String], now_ms: i64, today: &str) -> RlData {
    let mut by_account: HashMap<String, RlAccount> = aliases
        .iter()
        .map(|a| (a.clone(), RlAccount::default()))
        .collect();
    let mut today_total = 0u32;
    let mut recent_429 = 0u32;
    let current_hour_ms = now_ms.div_euclid(HOUR_MS) * HOUR_MS;

    let Ok(raw) = std::fs::read_to_string(rl_log) else {
        return RlData {
            today_total: 0,
            recent_429_count: 0,
            by_account,
        };
    };
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(t) else {
            continue;
        };
        let acct = ev
            .get("account")
            .or_else(|| ev.get("alias"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let ts_str = ev.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        let date = ev
            .get("date")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| (ts_str.len() >= 10).then(|| ts_str[..10].to_string()));
        let type_label = ev.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");
        let type_code = if type_label.starts_with("429") {
            429
        } else if type_label.starts_with("529") {
            529
        } else {
            0
        };

        let ts_ms = chrono::DateTime::parse_from_rfc3339(ts_str)
            .ok()
            .map(|dt| dt.timestamp_millis());
        let age_sec = ts_ms.map_or(0, |ms| ((now_ms - ms) / 1000).max(0));

        if date.as_deref() == Some(today) {
            today_total += 1;
            if let Some(a) = &acct {
                if let Some(r) = by_account.get_mut(a) {
                    r.count_today += 1;
                }
            }
        }
        if type_code == 429 && age_sec <= RL_RECENT_WINDOW_SEC {
            recent_429 += 1;
        }
        if let (Some(ms), Some(a)) = (ts_ms, &acct) {
            if let Some(r) = by_account.get_mut(a) {
                let hours_ago = (current_hour_ms - ms).div_euclid(HOUR_MS);
                if (0..24).contains(&hours_ago) {
                    r.buckets_24h[(23 - hours_ago) as usize] += 1;
                }
            }
        }
    }
    RlData {
        today_total,
        recent_429_count: recent_429,
        by_account,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmpfile(contents: &str) -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "evy-rl-test-{}-{}.jsonl",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn usage_history_buckets_are_24_oldest_to_newest() {
        let now: i64 = 100 * DAY_MS; // arbitrary fixed clock
        let this_hour = now.div_euclid(HOUR_MS) * HOUR_MS;
        let lines = format!(
            "{}\n{}\n{}\n",
            // current hour: two samples for alias a → max 5h
            serde_json::json!({"ts": this_hour + 60_000, "alias":"a", "five_hour": 40.0, "seven_day": 10.0}),
            serde_json::json!({"ts": this_hour + 120_000, "alias":"a", "five_hour": 55.0, "seven_day": 12.0}),
            // 3 hours ago
            serde_json::json!({"ts": this_hour - 3*HOUR_MS, "alias":"a", "five_hour": 5.0, "seven_day": 2.0}),
        );
        let f = tmpfile(&lines);
        let b = read_usage_history_24h(&f, now);
        let a = &b["a"];
        assert_eq!(a.len(), 24);
        assert_eq!(a[23].samples, 2); // current hour
        assert_eq!(a[23].five_hour_max, Some(55.0)); // max of 40,55
        assert_eq!(a[20].samples, 1); // 3h ago → idx 23-3
        assert_eq!(a[20].five_hour_max, Some(5.0));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn usage_history_drops_older_than_24h() {
        let now: i64 = 100 * DAY_MS;
        let lines = format!(
            "{}\n",
            serde_json::json!({"ts": now - 25*HOUR_MS, "alias":"a", "five_hour": 1.0})
        );
        let f = tmpfile(&lines);
        assert!(read_usage_history_24h(&f, now).is_empty());
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn rate_limit_events_count_and_bucket() {
        let now: i64 = 100 * DAY_MS;
        let today = "2099-01-01";
        // ts within 2h (recent 429), today's date; plus a 529 (not a 429)
        let ts_recent = chrono::DateTime::from_timestamp_millis(now - HOUR_MS)
            .unwrap()
            .to_rfc3339();
        let lines = format!(
            "{}\n{}\n",
            serde_json::json!({"ts": ts_recent, "date": today, "account":"claude-jason", "type":"429 (rate_limit)"}),
            serde_json::json!({"ts": ts_recent, "date": today, "account":"claude-jason", "type":"529 (overload)"}),
        );
        let f = tmpfile(&lines);
        let rl = build_rate_limits(&f, &["claude-jason".to_string()], now, today);
        assert_eq!(rl.today_total, 2); // both events count today
        assert_eq!(rl.recent_429_count, 1); // only the 429 is a recent rate-limit
        let acct = &rl.by_account["claude-jason"];
        assert_eq!(acct.count_today, 2);
        assert_eq!(acct.buckets_24h.len(), 24);
        assert_eq!(acct.buckets_24h.iter().sum::<u32>(), 2); // both bucketed in-window
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn rate_limits_empty_when_no_log() {
        let rl = build_rate_limits(
            Path::new("/nonexistent/rl.log"),
            &["a".to_string()],
            0,
            "2099-01-01",
        );
        assert_eq!(rl.today_total, 0);
        assert_eq!(rl.by_account["a"].buckets_24h.len(), 24);
    }
}
