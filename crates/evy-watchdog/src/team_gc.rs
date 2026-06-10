//! [`TeamGcWatchdog`] — sweep dead team registrations.
//!
//! Inspects every record in the injected [`TeamRegistry`] and flags
//! anything whose `last_activity` is older than `stale_threshold_secs`
//! AND whose `tmux_session` is no longer alive. Both predicates must
//! hold — a freshly-spawned team with no activity yet stays alive
//! (its tmux session is up); a long-running team with stale activity
//! also stays alive (its session is up). Only the AND of "stale
//! activity" + "dead session" qualifies for GC.
//!
//! GC means **drop from registry + emit a `DeadTeam` finding**. Phase 4
//! does not archive on-disk team dirs — the v4 daemon doesn't write
//! those yet (Phase 5 will, when team bookkeeping lands properly).
//!
//! # v3 → v4 mapping
//!
//! v3's `team-gc.ts` (boot-time) inspected `policy.snapshot.toml` +
//! audit-log mtime to decide "GC this team dir." v4 has no on-disk team
//! dir to read; the decision happens entirely against the in-memory
//! registry. The `DeadTeam.reason` string mirrors v3's vocabulary
//! (`"tmux_session_gone"`, `"stale_no_activity"`) so dashboard
//! consumers can keep their categorisation.
//!
//! TODO: Phase 5 — port the on-disk archival side-effect (move team
//! dir to `<state>/teams/.killed/<id>/`) once Phase 5 ports the team
//! state directory.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use evy_comms::DaemonEvent;
use evy_core::Result;

use crate::report::{Finding, TickReport};
use crate::team_registry::TeamRegistry;
use crate::tmux_query::TmuxQuery;
use crate::trait_def::{Watchdog, WatchdogContext, WatchdogSchedule};

/// Default staleness threshold — 1 hour. v3 used 7 days for full
/// `.killed/` archival; the in-memory v4 sweep can be much more
/// aggressive since the side-effect is a single `Map::remove`.
pub const DEFAULT_STALE_THRESHOLD_SECS: u64 = 3600;
/// Default tick cadence — once a minute.
pub const DEFAULT_TICK_INTERVAL_SECS: u64 = 60;

/// Watchdog that sweeps dead team registrations from a
/// [`TeamRegistry`].
pub struct TeamGcWatchdog {
    /// Registry to read + prune.
    teams: Arc<dyn TeamRegistry>,
    /// Tmux query — for checking session liveness.
    tmux: Arc<dyn TmuxQuery>,
    /// Seconds-since-`last_activity` before a team is considered stale.
    pub stale_threshold_secs: u64,
    /// Cadence the registry should fire this watchdog at.
    pub tick_interval_secs: u64,
}

impl TeamGcWatchdog {
    /// Build with default thresholds.
    #[must_use]
    pub fn new(teams: Arc<dyn TeamRegistry>, tmux: Arc<dyn TmuxQuery>) -> Self {
        Self {
            teams,
            tmux,
            stale_threshold_secs: DEFAULT_STALE_THRESHOLD_SECS,
            tick_interval_secs: DEFAULT_TICK_INTERVAL_SECS,
        }
    }

    /// Override the staleness threshold (seconds).
    #[must_use]
    pub fn with_stale_threshold_secs(mut self, secs: u64) -> Self {
        self.stale_threshold_secs = secs;
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
impl Watchdog for TeamGcWatchdog {
    fn name(&self) -> &str {
        "team-gc"
    }

    fn schedule(&self) -> WatchdogSchedule {
        WatchdogSchedule::EveryNSecs(self.tick_interval_secs)
    }

    async fn tick(&self, ctx: &WatchdogContext) -> Result<TickReport> {
        let teams = self.teams.list().await?;
        let now = Utc::now();
        let mut findings = Vec::new();

        for record in teams {
            // Predicate 1: tmux session liveness.
            let session_alive = self.tmux.session_exists(&record.tmux_session).await?;
            if session_alive {
                continue;
            }

            // Predicate 2: activity staleness. A team with no activity
            // at all qualifies immediately (matches v3's "missing
            // audit file → audit_age = absent" treatment).
            let stale = match record.last_activity {
                None => true,
                Some(ts) => {
                    let age = now.signed_duration_since(ts).num_seconds();
                    age >= i64::try_from(self.stale_threshold_secs).unwrap_or(i64::MAX)
                }
            };
            if !stale {
                continue;
            }

            let reason = if record.last_activity.is_none() {
                "tmux_session_gone:no_activity"
            } else {
                "tmux_session_gone:stale_activity"
            };
            self.teams.remove(&record.team_id).await?;
            // Registry state-change → named `team_event` frame for the
            // dashboard cockpit's live feed (orch.js renders {team, type,
            // text}).
            ctx.events.emit(DaemonEvent::dashboard_team_event(
                &record.team_id,
                "dead",
                reason,
            ));
            findings.push(Finding::DeadTeam {
                team: record.team_id,
                reason: reason.into(),
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
    use chrono::Duration as ChronoDuration;

    fn registry_with(records: Vec<TeamRecord>) -> Arc<InMemoryTeamRegistry> {
        let r = Arc::new(InMemoryTeamRegistry::new());
        for rec in records {
            r.insert(rec);
        }
        r
    }

    #[tokio::test]
    async fn live_session_is_never_gc_d() {
        let (ctx, _guards) = watchdog_ctx().await;
        let registry = registry_with(vec![TeamRecord {
            team_id: "alive".into(),
            tmux_session: "claude-alive".into(),
            last_activity: None,
        }]);
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-alive"]);
        let w = TeamGcWatchdog::new(registry.clone(), tmux);
        let r = w.tick(&ctx).await.unwrap();
        assert!(r.healthy);
        assert_eq!(registry.len(), 1);
    }

    #[tokio::test]
    async fn dead_session_with_no_activity_is_gc_d_immediately() {
        let (ctx, _guards) = watchdog_ctx().await;
        let registry = registry_with(vec![TeamRecord {
            team_id: "ghost".into(),
            tmux_session: "claude-ghost".into(),
            last_activity: None,
        }]);
        let tmux = Arc::new(MockTmuxQuery::new()); // no sessions registered
        let w = TeamGcWatchdog::new(registry.clone(), tmux);
        let r = w.tick(&ctx).await.unwrap();
        assert!(r.healthy); // healthy != findings.is_empty()
        assert_eq!(r.findings.len(), 1);
        assert!(matches!(
            &r.findings[0],
            Finding::DeadTeam { team, reason }
            if team == "ghost" && reason.contains("no_activity")
        ));
        assert_eq!(registry.len(), 0);
    }

    #[tokio::test]
    async fn gc_removal_emits_team_event_frame() {
        // The cockpit live feed (orch.js `team_event` listener) must see
        // every registry removal as {team, type:"dead", text:<reason>}.
        let (ctx, _guards) = watchdog_ctx().await;
        let mut rx = ctx.events.subscribe();
        let registry = registry_with(vec![TeamRecord {
            team_id: "ghost".into(),
            tmux_session: "claude-ghost".into(),
            last_activity: None,
        }]);
        let tmux = Arc::new(MockTmuxQuery::new()); // session gone
        let w = TeamGcWatchdog::new(registry.clone(), tmux);
        w.tick(&ctx).await.unwrap();

        let frame = loop {
            match rx.try_recv().expect("a DashboardFrame must be buffered") {
                DaemonEvent::DashboardFrame { event, data } => break (event, data),
                _ => continue,
            }
        };
        assert_eq!(frame.0, "team_event");
        let data: serde_json::Value = serde_json::from_str(&frame.1).expect("frame data is JSON");
        assert_eq!(data["team"], "ghost");
        assert_eq!(data["type"], "dead");
        assert_eq!(data["text"], "tmux_session_gone:no_activity");
    }

    #[tokio::test]
    async fn dead_session_with_recent_activity_is_still_gc_d() {
        // Phase-4 rule: when tmux says the session is gone, activity
        // recency doesn't matter for the GC predicate (a tmux session
        // can't be reaped while still receiving writes). The reason
        // string changes for forensics though.
        let (ctx, _guards) = watchdog_ctx().await;
        let registry = registry_with(vec![TeamRecord {
            team_id: "ghost".into(),
            tmux_session: "claude-ghost".into(),
            last_activity: Some(Utc::now()),
        }]);
        let tmux = Arc::new(MockTmuxQuery::new());
        let w = TeamGcWatchdog::new(registry.clone(), tmux).with_stale_threshold_secs(0);
        let r = w.tick(&ctx).await.unwrap();
        assert_eq!(r.findings.len(), 1);
        assert!(matches!(
            &r.findings[0],
            Finding::DeadTeam { team, reason }
            if team == "ghost" && reason.contains("stale_activity")
        ));
        assert_eq!(registry.len(), 0);
    }

    #[tokio::test]
    async fn dead_session_with_recent_activity_held_below_threshold() {
        // If the operator pushes the staleness threshold above zero,
        // a team whose `last_activity` is fresh stays alive even when
        // its session disappears. This mirrors v3's "give it the
        // benefit of the doubt" treatment.
        let (ctx, _guards) = watchdog_ctx().await;
        let registry = registry_with(vec![TeamRecord {
            team_id: "fresh".into(),
            tmux_session: "claude-fresh".into(),
            last_activity: Some(Utc::now() - ChronoDuration::seconds(5)),
        }]);
        let tmux = Arc::new(MockTmuxQuery::new());
        let w = TeamGcWatchdog::new(registry.clone(), tmux).with_stale_threshold_secs(3600);
        let r = w.tick(&ctx).await.unwrap();
        assert!(r.healthy);
        assert_eq!(registry.len(), 1, "fresh activity kept the team");
    }

    #[tokio::test]
    async fn mixed_population_yields_partial_gc() {
        let (ctx, _guards) = watchdog_ctx().await;
        let registry = registry_with(vec![
            TeamRecord {
                team_id: "alive".into(),
                tmux_session: "claude-alive".into(),
                last_activity: None,
            },
            TeamRecord {
                team_id: "dead-a".into(),
                tmux_session: "claude-dead-a".into(),
                last_activity: None,
            },
            TeamRecord {
                team_id: "dead-b".into(),
                tmux_session: "claude-dead-b".into(),
                last_activity: Some(Utc::now() - ChronoDuration::hours(2)),
            },
        ]);
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-alive"]);
        let w = TeamGcWatchdog::new(registry.clone(), tmux);
        let r = w.tick(&ctx).await.unwrap();
        assert_eq!(r.findings.len(), 2);
        assert_eq!(registry.len(), 1);
    }
}
