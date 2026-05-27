//! [`WatchdogPrune`] — defensive sweep removing vanished sessions.
//!
//! The v3 origin (`watchdog-prune.ts`) was the layered-defense response
//! to a real outage where the bulk session-prune in the dashboard
//! silently no-op'd after a transient `tmux list-sessions` error and
//! the team-staleness watchdog kept escalating ghosts for hours.
//!
//! In v4 we keep the same defense-in-depth posture even though the
//! daemon's bookkeeping is simpler: `WatchdogPrune` is independent of
//! [`crate::team_gc::TeamGcWatchdog`] — it doesn't care about activity
//! staleness, it just looks at every record in the [`TeamRegistry`]
//! and drops any whose `tmux_session` is unequivocally absent. If
//! `TeamGcWatchdog` already pruned the record, this sweep finds
//! nothing. If `TeamGcWatchdog` is killed or stalled, this sweep
//! still catches the obvious-vanished cases.
//!
//! The two watchdogs deliberately overlap. The intersection is
//! cheap (O(N) over N teams = single-digit at most) and the
//! redundancy survived v3's production fire — we keep it.
//!
//! TODO: Phase 5 — extend `WatchdogPrune` with the
//! `pruneOneTeam(kind="operator-killed")` lifecycle hook (currently
//! implicit in v3 via dashboard `POST /teams/:name/prune`).

use std::sync::Arc;

use async_trait::async_trait;
use evy_core::Result;

use crate::report::{Finding, TickReport};
use crate::team_registry::TeamRegistry;
use crate::tmux_query::TmuxQuery;
use crate::trait_def::{Watchdog, WatchdogContext, WatchdogSchedule};

/// Default cadence — defensive sweep every 30 seconds.
pub const DEFAULT_TICK_INTERVAL_SECS: u64 = 30;

/// Defensive sweep removing vanished tmux session references.
pub struct WatchdogPrune {
    teams: Arc<dyn TeamRegistry>,
    tmux: Arc<dyn TmuxQuery>,
    /// Cadence the registry should fire this watchdog at.
    pub tick_interval_secs: u64,
}

impl WatchdogPrune {
    /// Build with default cadence.
    #[must_use]
    pub fn new(teams: Arc<dyn TeamRegistry>, tmux: Arc<dyn TmuxQuery>) -> Self {
        Self {
            teams,
            tmux,
            tick_interval_secs: DEFAULT_TICK_INTERVAL_SECS,
        }
    }

    /// Override the cadence.
    #[must_use]
    pub fn with_tick_interval_secs(mut self, secs: u64) -> Self {
        self.tick_interval_secs = secs;
        self
    }
}

#[async_trait]
impl Watchdog for WatchdogPrune {
    fn name(&self) -> &str {
        "watchdog-prune"
    }

    fn schedule(&self) -> WatchdogSchedule {
        WatchdogSchedule::EveryNSecs(self.tick_interval_secs)
    }

    async fn tick(&self, _ctx: &WatchdogContext) -> Result<TickReport> {
        let teams = self.teams.list().await?;
        let mut findings = Vec::new();

        for record in teams {
            // The contract here is narrower than `TeamGcWatchdog`'s —
            // we only care about whether the tmux session exists. No
            // grace period, no activity heuristic.
            let alive = self.tmux.session_exists(&record.tmux_session).await?;
            if alive {
                continue;
            }
            self.teams.remove(&record.team_id).await?;
            findings.push(Finding::PrunedSession {
                session: record.tmux_session,
            });
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
    use chrono::Utc;

    fn registry_with(records: Vec<TeamRecord>) -> Arc<InMemoryTeamRegistry> {
        let r = Arc::new(InMemoryTeamRegistry::new());
        for rec in records {
            r.insert(rec);
        }
        r
    }

    #[tokio::test]
    async fn live_session_passes_through_untouched() {
        let (ctx, _guards) = watchdog_ctx().await;
        let registry = registry_with(vec![TeamRecord {
            team_id: "alive".into(),
            tmux_session: "claude-alive".into(),
            last_activity: Some(Utc::now()),
        }]);
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-alive"]);
        let w = WatchdogPrune::new(registry.clone(), tmux);
        let r = w.tick(&ctx).await.unwrap();
        assert!(r.healthy);
        assert_eq!(registry.len(), 1);
    }

    #[tokio::test]
    async fn vanished_session_is_pruned_regardless_of_activity() {
        // Even a team with brand-new activity gets pruned by this
        // sweep if the session itself is gone — the activity must
        // have been a stale write or a clock skew.
        let (ctx, _guards) = watchdog_ctx().await;
        let registry = registry_with(vec![TeamRecord {
            team_id: "ghost".into(),
            tmux_session: "claude-ghost".into(),
            last_activity: Some(Utc::now()),
        }]);
        let tmux = Arc::new(MockTmuxQuery::new()); // no sessions
        let w = WatchdogPrune::new(registry.clone(), tmux);
        let r = w.tick(&ctx).await.unwrap();
        assert_eq!(r.findings.len(), 1);
        match &r.findings[0] {
            Finding::PrunedSession { session } => assert_eq!(session, "claude-ghost"),
            other => panic!("expected PrunedSession, got {other:?}"),
        }
        assert_eq!(registry.len(), 0);
    }

    #[tokio::test]
    async fn empty_registry_yields_healthy_no_findings() {
        let (ctx, _guards) = watchdog_ctx().await;
        let registry = Arc::new(InMemoryTeamRegistry::new());
        let tmux = Arc::new(MockTmuxQuery::new());
        let w = WatchdogPrune::new(registry, tmux);
        let r = w.tick(&ctx).await.unwrap();
        assert!(r.healthy);
        assert_eq!(r.findings, vec![Finding::Healthy]);
    }

    #[tokio::test]
    async fn second_tick_after_prune_is_a_noop() {
        let (ctx, _guards) = watchdog_ctx().await;
        let registry = registry_with(vec![TeamRecord {
            team_id: "ghost".into(),
            tmux_session: "claude-ghost".into(),
            last_activity: None,
        }]);
        let tmux = Arc::new(MockTmuxQuery::new());
        let w = WatchdogPrune::new(registry.clone(), tmux);
        let r1 = w.tick(&ctx).await.unwrap();
        assert_eq!(r1.findings.len(), 1);
        let r2 = w.tick(&ctx).await.unwrap();
        assert!(r2.healthy);
        assert_eq!(r2.findings, vec![Finding::Healthy]);
        assert!(registry.is_empty());
    }
}
