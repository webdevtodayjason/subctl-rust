//! Cutover Phase 1 — dashboard state synthesis (the v3 `buildState()` brain).
//!
//! This module ports the operator-console's most complex piece: the per-account
//! dispatch verdict, the 24h rate-limit buckets, and the `/api/state` composite.
//! It lands in slices; this first slice is the **verdict** — ported byte-faithfully
//! from `dashboard/server.ts:1028-1076` (`computeAccountVerdict`) so the Overview
//! verdict pill matches v3 exactly.
//!
//! Thresholds (server.ts:1029-1032):
//!   7-day weekly util: ≥70% → yellow, ≥90% → red
//!   5-hour session util: ≥80% → yellow, ≥95% → red
//!   recent 429s today: ≥1 → yellow, ≥3 → red
//!   parallel sessions on the account: ≥3 → yellow, ≥5 → red
//! Unauthenticated → red. `red` sticks; `yellow` only bumps if not already red.

use serde::{Deserialize, Serialize};

const THRESH_7D_YELLOW: f64 = 70.0;
const THRESH_7D_RED: f64 = 90.0;
const THRESH_5H_YELLOW: f64 = 80.0;
const THRESH_5H_RED: f64 = 95.0;

/// Dispatch readiness color. Serializes to `"green" | "yellow" | "red"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Green,
    Yellow,
    Red,
}

/// Inputs to the per-account verdict — exactly the fields v3's
/// `computeAccountVerdict` reads (utilizations are percentages, may be absent).
#[derive(Debug, Clone)]
pub struct AccountVerdictInput {
    pub auth_ready: bool,
    pub seven_day_util: Option<f64>,
    pub five_hour_util: Option<f64>,
    pub recent_429: u32,
    pub parallel_on_account: u32,
}

/// Verdict + the human-readable reasons (shown in the Overview pill tooltip).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountVerdict {
    pub verdict: Verdict,
    pub reasons: Vec<String>,
}

/// JS `${n}` number formatting: integral floats print without a decimal
/// (`72.0 → "72"`), fractional keep their digits (`72.5 → "72.5"`). Matches
/// the v3 template-literal output for byte-for-byte reasons strings.
fn js_num(n: f64) -> String {
    if n.fract() == 0.0 && n.is_finite() {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Port of `computeAccountVerdict` (server.ts:1039-1076).
#[must_use]
pub fn compute_account_verdict(a: &AccountVerdictInput) -> AccountVerdict {
    if !a.auth_ready {
        return AccountVerdict {
            verdict: Verdict::Red,
            reasons: vec!["account not authenticated".to_string()],
        };
    }

    let mut level = Verdict::Green;
    let mut reasons: Vec<String> = Vec::new();
    let mut bump = |l: Verdict, reasons: &mut Vec<String>, reason: String| {
        match l {
            Verdict::Red => level = Verdict::Red,
            Verdict::Yellow if level != Verdict::Red => level = Verdict::Yellow,
            _ => {}
        }
        reasons.push(reason);
    };

    if let Some(wkly) = a.seven_day_util {
        if wkly >= THRESH_7D_RED {
            bump(Verdict::Red, &mut reasons, format!("weekly {}% (red ≥{}%)", js_num(wkly), js_num(THRESH_7D_RED)));
        } else if wkly >= THRESH_7D_YELLOW {
            bump(Verdict::Yellow, &mut reasons, format!("weekly {}%", js_num(wkly)));
        }
    }

    if let Some(sess) = a.five_hour_util {
        if sess >= THRESH_5H_RED {
            bump(Verdict::Red, &mut reasons, format!("5h {}% (red ≥{}%)", js_num(sess), js_num(THRESH_5H_RED)));
        } else if sess >= THRESH_5H_YELLOW {
            bump(Verdict::Yellow, &mut reasons, format!("5h {}%", js_num(sess)));
        }
    }

    if a.recent_429 >= 3 {
        bump(Verdict::Red, &mut reasons, format!("{} RL hits today", a.recent_429));
    } else if a.recent_429 >= 1 {
        let plural = if a.recent_429 == 1 { "" } else { "s" };
        bump(Verdict::Yellow, &mut reasons, format!("{} RL hit{} today", a.recent_429, plural));
    }

    if a.parallel_on_account >= 5 {
        bump(Verdict::Red, &mut reasons, format!("{} parallel sessions on this account", a.parallel_on_account));
    } else if a.parallel_on_account >= 3 {
        bump(Verdict::Yellow, &mut reasons, format!("{} parallel sessions on this account", a.parallel_on_account));
    }

    AccountVerdict { verdict: level, reasons }
}

// ─── slice 1b: per-account auth status (ported from v3 authStatus) ────────────

/// Per-account auth readiness. Serializes to v3's `"ready" | "not_authenticated"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStatus {
    Ready,
    NotAuthenticated,
}

/// Port of v3 `authStatus(account)` (server.ts:1421-1447). Provider-agnostic
/// disk-marker check, in order: `<config_dir>/.credentials.json` present → ready;
/// or `<config_dir>/projects/` has ≥1 subdirectory → ready; or codex
/// `<config_dir>/auth.json` with a non-empty `tokens.id_token` or
/// `tokens.access_token` → ready; else not_authenticated.
#[must_use]
pub fn auth_status(config_dir: &std::path::Path) -> AuthStatus {
    if config_dir.join(".credentials.json").exists() {
        return AuthStatus::Ready;
    }
    if let Ok(entries) = std::fs::read_dir(config_dir.join("projects")) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                return AuthStatus::Ready;
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(config_dir.join("auth.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            let tokens = v.get("tokens");
            let non_empty = |k: &str| {
                tokens
                    .and_then(|t| t.get(k))
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|s| !s.is_empty())
            };
            if non_empty("id_token") || non_empty("access_token") {
                return AuthStatus::Ready;
            }
        }
    }
    AuthStatus::NotAuthenticated
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmpdir() -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "evy-auth-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn input() -> AccountVerdictInput {
        AccountVerdictInput {
            auth_ready: true,
            seven_day_util: None,
            five_hour_util: None,
            recent_429: 0,
            parallel_on_account: 0,
        }
    }

    #[test]
    fn unauthenticated_is_red() {
        let v = compute_account_verdict(&AccountVerdictInput { auth_ready: false, ..input() });
        assert_eq!(v.verdict, Verdict::Red);
        assert_eq!(v.reasons, vec!["account not authenticated"]);
    }

    #[test]
    fn all_clear_is_green_no_reasons() {
        let v = compute_account_verdict(&input());
        assert_eq!(v.verdict, Verdict::Green);
        assert!(v.reasons.is_empty());
    }

    #[test]
    fn weekly_thresholds() {
        // boundary: <70 green, 70..90 yellow, >=90 red
        assert_eq!(compute_account_verdict(&AccountVerdictInput { seven_day_util: Some(69.0), ..input() }).verdict, Verdict::Green);
        assert_eq!(compute_account_verdict(&AccountVerdictInput { seven_day_util: Some(70.0), ..input() }).verdict, Verdict::Yellow);
        assert_eq!(compute_account_verdict(&AccountVerdictInput { seven_day_util: Some(89.0), ..input() }).verdict, Verdict::Yellow);
        assert_eq!(compute_account_verdict(&AccountVerdictInput { seven_day_util: Some(90.0), ..input() }).verdict, Verdict::Red);
    }

    #[test]
    fn five_hour_thresholds() {
        assert_eq!(compute_account_verdict(&AccountVerdictInput { five_hour_util: Some(79.0), ..input() }).verdict, Verdict::Green);
        assert_eq!(compute_account_verdict(&AccountVerdictInput { five_hour_util: Some(80.0), ..input() }).verdict, Verdict::Yellow);
        assert_eq!(compute_account_verdict(&AccountVerdictInput { five_hour_util: Some(95.0), ..input() }).verdict, Verdict::Red);
    }

    #[test]
    fn rl_hits_singular_plural_and_levels() {
        let one = compute_account_verdict(&AccountVerdictInput { recent_429: 1, ..input() });
        assert_eq!(one.verdict, Verdict::Yellow);
        assert_eq!(one.reasons, vec!["1 RL hit today"]);
        let two = compute_account_verdict(&AccountVerdictInput { recent_429: 2, ..input() });
        assert_eq!(two.reasons, vec!["2 RL hits today"]);
        let three = compute_account_verdict(&AccountVerdictInput { recent_429: 3, ..input() });
        assert_eq!(three.verdict, Verdict::Red);
        assert_eq!(three.reasons, vec!["3 RL hits today"]);
    }

    #[test]
    fn parallel_sessions_levels() {
        assert_eq!(compute_account_verdict(&AccountVerdictInput { parallel_on_account: 3, ..input() }).verdict, Verdict::Yellow);
        assert_eq!(compute_account_verdict(&AccountVerdictInput { parallel_on_account: 5, ..input() }).verdict, Verdict::Red);
    }

    #[test]
    fn red_sticks_over_yellow() {
        // weekly red + 5h yellow → red overall, both reasons present, order preserved
        let v = compute_account_verdict(&AccountVerdictInput {
            seven_day_util: Some(91.0),
            five_hour_util: Some(82.0),
            ..input()
        });
        assert_eq!(v.verdict, Verdict::Red);
        assert_eq!(v.reasons, vec!["weekly 91% (red ≥90%)", "5h 82%"]);
    }

    #[test]
    fn js_num_formats_like_template_literal() {
        assert_eq!(js_num(72.0), "72");
        assert_eq!(js_num(72.5), "72.5");
    }

    #[test]
    fn auth_ready_via_credentials_json() {
        let d = tmpdir();
        std::fs::write(d.join(".credentials.json"), "{}").unwrap();
        assert_eq!(auth_status(&d), AuthStatus::Ready);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn auth_ready_via_projects_subdir() {
        let d = tmpdir();
        std::fs::create_dir_all(d.join("projects").join("some-session")).unwrap();
        assert_eq!(auth_status(&d), AuthStatus::Ready);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn auth_ready_via_codex_access_token() {
        let d = tmpdir();
        std::fs::write(d.join("auth.json"), r#"{"tokens":{"access_token":"abc"}}"#).unwrap();
        assert_eq!(auth_status(&d), AuthStatus::Ready);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn auth_not_authenticated_when_empty() {
        let d = tmpdir();
        assert_eq!(auth_status(&d), AuthStatus::NotAuthenticated);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn auth_not_authenticated_codex_empty_tokens() {
        let d = tmpdir();
        std::fs::write(d.join("auth.json"), r#"{"tokens":{}}"#).unwrap();
        assert_eq!(auth_status(&d), AuthStatus::NotAuthenticated);
        // empty projects dir (no subdirs) must not flip to ready
        std::fs::create_dir_all(d.join("projects")).unwrap();
        assert_eq!(auth_status(&d), AuthStatus::NotAuthenticated);
        let _ = std::fs::remove_dir_all(&d);
    }
}
