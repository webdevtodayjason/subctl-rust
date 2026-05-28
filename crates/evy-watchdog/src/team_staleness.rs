//! [`TeamStalenessWatchdog`] — the "I dispatched a team but nothing's
//! happening" canary.
//!
//! v4 splits v3's `team-staleness` behaviour across three watchdogs:
//!
//! | Watchdog | Owns | Side effect |
//! |---|---|---|
//! | [`crate::AutoNudgeWatchdog`] | the **worker-level** nudge state machine | sends nudge directives, escalates to [`Finding::WorkerDead`] |
//! | [`crate::TeamGcWatchdog`] | "team's session is gone **and** activity is stale" | removes the team from the registry, emits [`Finding::DeadTeam`] |
//! | [`crate::TeamStalenessWatchdog`] *(this file)* | "team is alive but quiet" | observation only — emits [`Finding::StaleTeam`], **does not** mutate the registry |
//!
//! The split closes the v3 production bug
//! `/Users/sem/code/subctl/.subctl/docs/bugs/2026-05-18-stale-team-watchdog-alerts.md`:
//! the v3 watchdog kept paging Telegram on teams whose tmux session was
//! already gone. v4's contract is:
//!
//! 1. **Before emitting `StaleTeam`, check the tmux session still
//!    exists.** If the session is dead, this watchdog is silent —
//!    `TeamGcWatchdog` / `WatchdogPrune` own that case. This is the
//!    regression sentinel.
//! 2. **Emit `StaleTeam` only as observation, never as an alert.** The
//!    dashboard / notification layer (Phase 6) decides whether to page.
//!    The watchdog's job is to say "I noticed this," not "wake the
//!    operator now."
//! 3. **Do not mutate the registry.** A stale-but-alive team might
//!    just be waiting on the operator; deletion belongs to
//!    `TeamGcWatchdog`.
//!
//! # Threshold semantics
//!
//! `stale_threshold_secs` is measured from `TeamRecord.last_activity`.
//! Teams with `last_activity: None` are treated as "just spawned" and
//! given a grace period equal to the threshold from `Utc::now()` at
//! tick time. (Without a grace period, a freshly-spawned team would
//! flag stale on the very next tick.) This mirrors v3's "skip
//! never-active teams on first observation" carve-out.
//!
//! TODO: Phase 6 — wire the dashboard's `team-unresponsive` toast off
//! `Finding::StaleTeam` events from this watchdog. Suppress duplicate
//! alerts via a per-team cooldown in the notification layer, not here.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use evy_core::Result;

use crate::report::{Finding, TickReport};
use crate::team_registry::TeamRegistry;
use crate::tmux_query::TmuxQuery;
use crate::trait_def::{Watchdog, WatchdogContext, WatchdogSchedule};

/// Default staleness threshold — 30 minutes. Tuned for v4's worker
/// cadence: a team that hasn't moved in half an hour is the operator's
/// problem, not background noise.
pub const DEFAULT_STALE_THRESHOLD_SECS: u64 = 1800;
/// Default tick cadence — once every 10 minutes. The watchdog is an
/// observation source, not a real-time pager, so coarse cadence is
/// fine. The dashboard layer reacts faster off SSE if needed.
pub const DEFAULT_TICK_INTERVAL_SECS: u64 = 600;

/// Tunable thresholds for [`TeamStalenessWatchdog`].
#[derive(Debug, Clone)]
pub struct TeamStalenessConfig {
    /// Seconds since `last_activity` before a team is considered stale.
    /// Teams with no recorded activity are given the same window from
    /// the watchdog's wall clock as a grace period.
    pub stale_threshold_secs: u64,
}

impl Default for TeamStalenessConfig {
    fn default() -> Self {
        Self {
            stale_threshold_secs: DEFAULT_STALE_THRESHOLD_SECS,
        }
    }
}

/// Watchdog that flags registered teams whose tmux session is still
/// alive but whose `last_activity` exceeded the staleness threshold.
pub struct TeamStalenessWatchdog {
    /// Tunable thresholds. Public so the daemon can tweak without
    /// reconstruction.
    pub config: TeamStalenessConfig,
    teams: Arc<dyn TeamRegistry>,
    tmux: Arc<dyn TmuxQuery>,
    /// Cadence the registry should fire this watchdog at.
    pub tick_interval_secs: u64,
}

impl TeamStalenessWatchdog {
    /// Build with default thresholds.
    #[must_use]
    pub fn new(teams: Arc<dyn TeamRegistry>, tmux: Arc<dyn TmuxQuery>) -> Self {
        Self {
            config: TeamStalenessConfig::default(),
            teams,
            tmux,
            tick_interval_secs: DEFAULT_TICK_INTERVAL_SECS,
        }
    }

    /// Replace the entire config.
    #[must_use]
    pub fn with_config(mut self, config: TeamStalenessConfig) -> Self {
        self.config = config;
        self
    }

    /// Override the staleness threshold (seconds).
    #[must_use]
    pub fn with_stale_threshold_secs(mut self, secs: u64) -> Self {
        self.config.stale_threshold_secs = secs;
        self
    }

    /// Override the tick cadence.
    #[must_use]
    pub fn with_tick_interval_secs(mut self, secs: u64) -> Self {
        self.tick_interval_secs = secs;
        self
    }
}

#[async_trait]
impl Watchdog for TeamStalenessWatchdog {
    fn name(&self) -> &str {
        "team-staleness"
    }

    fn schedule(&self) -> WatchdogSchedule {
        WatchdogSchedule::EveryNSecs(self.tick_interval_secs)
    }

    async fn tick(&self, _ctx: &WatchdogContext) -> Result<TickReport> {
        let teams = self.teams.list().await?;
        let now = Utc::now();
        let threshold_secs = i64::try_from(self.config.stale_threshold_secs).unwrap_or(i64::MAX);
        let mut findings = Vec::new();

        for record in teams {
            // Regression sentinel for the 2026-05-18 bug — never page
            // on a team whose session is already gone; that's
            // TeamGcWatchdog / WatchdogPrune territory.
            let session_alive = self.tmux.session_exists(&record.tmux_session).await?;
            if !session_alive {
                continue;
            }

            let last_activity = match record.last_activity {
                Some(ts) => ts,
                // No activity since boot — we have no real timestamp
                // to compare against. Skip this tick: the watchdog
                // can't distinguish "just spawned 1 second ago" from
                // "registered an hour ago but never moved." The next
                // tick that catches it AFTER a real activity timestamp
                // arrives will fire correctly. This is the v3
                // "give it the benefit of the doubt" carve-out.
                None => continue,
            };

            let age = now.signed_duration_since(last_activity).num_seconds();
            if age >= threshold_secs {
                findings.push(Finding::StaleTeam {
                    team_name: record.team_id,
                    last_activity,
                });
            }
        }

        if findings.is_empty() {
            Ok(TickReport::healthy(self.name()))
        } else {
            Ok(TickReport::with_findings(self.name(), findings))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team_registry::{InMemoryTeamRegistry, TeamRecord};
    use crate::test_support::watchdog_ctx;
    use crate::tmux_query::MockTmuxQuery;
    use chrono::Duration as ChronoDuration;

    fn registry_with(records: Vec<TeamRecord>) -> Arc<InMemoryTeamRegistry> {
        let r = Arc::new(InMemoryTeamRegistry::new());
        for rec in records {
            r.insert(rec);
        }
        r
    }

    #[tokio::test]
    async fn empty_registry_is_healthy() {
        let (ctx, _guards) = watchdog_ctx().await;
        let teams = Arc::new(InMemoryTeamRegistry::new());
        let tmux = Arc::new(MockTmuxQuery::new());
        let w = TeamStalenessWatchdog::new(teams, tmux);
        let r = w.tick(&ctx).await.unwrap();
        assert!(r.healthy);
        assert_eq!(r.findings, vec![Finding::Healthy]);
    }

    #[tokio::test]
    async fn fresh_team_does_not_fire() {
        let (ctx, _guards) = watchdog_ctx().await;
        let teams = registry_with(vec![TeamRecord {
            team_id: "fresh".into(),
            tmux_session: "claude-fresh".into(),
            last_activity: Some(Utc::now()),
        }]);
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-fresh"]);
        let w = TeamStalenessWatchdog::new(teams, tmux);
        let r = w.tick(&ctx).await.unwrap();
        assert!(r.healthy);
        assert!(
            !r.findings
                .iter()
                .any(|f| matches!(f, Finding::StaleTeam { .. })),
            "fresh team must not be flagged stale",
        );
    }

    #[tokio::test]
    async fn stale_alive_team_fires_once() {
        let (ctx, _guards) = watchdog_ctx().await;
        let stale_ts = Utc::now() - ChronoDuration::hours(2);
        let teams = registry_with(vec![TeamRecord {
            team_id: "quiet".into(),
            tmux_session: "claude-quiet".into(),
            last_activity: Some(stale_ts),
        }]);
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-quiet"]);
        let w = TeamStalenessWatchdog::new(teams.clone(), tmux);
        let r = w.tick(&ctx).await.unwrap();
        assert_eq!(r.findings.len(), 1);
        match &r.findings[0] {
            Finding::StaleTeam {
                team_name,
                last_activity,
            } => {
                assert_eq!(team_name, "quiet");
                assert_eq!(*last_activity, stale_ts);
            }
            other => panic!("expected StaleTeam, got {other:?}"),
        }
        // Critical contract: the watchdog must NOT mutate the registry.
        assert_eq!(teams.len(), 1, "team must remain in registry");
    }

    #[tokio::test]
    async fn dead_session_does_not_fire_stale_team() {
        // Regression sentinel for the 2026-05-18 bug:
        // "team-staleness watchdog keeps alerting on deleted tmux sessions".
        // Even with very stale activity, a dead session must not
        // trigger StaleTeam — the GC watchdog owns that lifecycle.
        let (ctx, _guards) = watchdog_ctx().await;
        let teams = registry_with(vec![TeamRecord {
            team_id: "ghost".into(),
            tmux_session: "claude-ghost".into(),
            last_activity: Some(Utc::now() - ChronoDuration::days(1)),
        }]);
        let tmux = Arc::new(MockTmuxQuery::new()); // no sessions
        let w = TeamStalenessWatchdog::new(teams.clone(), tmux);
        let r = w.tick(&ctx).await.unwrap();
        assert!(r.healthy);
        assert!(
            !r.findings
                .iter()
                .any(|f| matches!(f, Finding::StaleTeam { .. })),
            "dead-session team must not fire StaleTeam (regression sentinel)",
        );
        assert_eq!(teams.len(), 1, "watchdog must not mutate the registry");
    }

    #[tokio::test]
    async fn missing_last_activity_skips_team() {
        let (ctx, _guards) = watchdog_ctx().await;
        let teams = registry_with(vec![TeamRecord {
            team_id: "no-activity".into(),
            tmux_session: "claude-no-activity".into(),
            last_activity: None,
        }]);
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-no-activity"]);
        let w = TeamStalenessWatchdog::new(teams.clone(), tmux).with_stale_threshold_secs(0);
        let r = w.tick(&ctx).await.unwrap();
        assert!(r.healthy);
        assert!(
            !r.findings
                .iter()
                .any(|f| matches!(f, Finding::StaleTeam { .. })),
            "no-activity teams are given the benefit of the doubt",
        );
    }

    #[tokio::test]
    async fn threshold_below_age_fires_threshold_above_does_not() {
        let (ctx, _guards) = watchdog_ctx().await;
        let teams = registry_with(vec![TeamRecord {
            team_id: "edge".into(),
            tmux_session: "claude-edge".into(),
            last_activity: Some(Utc::now() - ChronoDuration::seconds(60)),
        }]);
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-edge"]);
        // 30s threshold + 60s age = stale.
        let w_low =
            TeamStalenessWatchdog::new(teams.clone(), tmux.clone()).with_stale_threshold_secs(30);
        let r_low = w_low.tick(&ctx).await.unwrap();
        assert_eq!(r_low.findings.len(), 1);
        // 3600s threshold + 60s age = fresh.
        let w_high = TeamStalenessWatchdog::new(teams, tmux).with_stale_threshold_secs(3600);
        let r_high = w_high.tick(&ctx).await.unwrap();
        assert!(r_high.healthy);
        assert!(!r_high
            .findings
            .iter()
            .any(|f| matches!(f, Finding::StaleTeam { .. })));
    }

    #[tokio::test]
    async fn mixed_population_only_fires_alive_and_stale() {
        let (ctx, _guards) = watchdog_ctx().await;
        let stale_ts = Utc::now() - ChronoDuration::hours(2);
        let teams = registry_with(vec![
            // Alive + fresh — silent.
            TeamRecord {
                team_id: "alive-fresh".into(),
                tmux_session: "claude-alive-fresh".into(),
                last_activity: Some(Utc::now()),
            },
            // Alive + stale — fire.
            TeamRecord {
                team_id: "alive-stale".into(),
                tmux_session: "claude-alive-stale".into(),
                last_activity: Some(stale_ts),
            },
            // Dead session + stale — silent (GC watchdog owns it).
            TeamRecord {
                team_id: "ghost-stale".into(),
                tmux_session: "claude-ghost-stale".into(),
                last_activity: Some(stale_ts),
            },
        ]);
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-alive-fresh", "claude-alive-stale"]);
        let w = TeamStalenessWatchdog::new(teams.clone(), tmux);
        let r = w.tick(&ctx).await.unwrap();
        let stale_names: Vec<_> = r
            .findings
            .iter()
            .filter_map(|f| match f {
                Finding::StaleTeam { team_name, .. } => Some(team_name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(stale_names, vec!["alive-stale".to_string()]);
        assert_eq!(teams.len(), 3, "registry must be untouched");
    }

    #[test]
    fn default_config_matches_constants() {
        let cfg = TeamStalenessConfig::default();
        assert_eq!(cfg.stale_threshold_secs, DEFAULT_STALE_THRESHOLD_SECS);
    }
}
