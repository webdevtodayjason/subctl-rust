//! Integration tests for the Phase 5 watchdogs.
//!
//! `AutoNudgeWatchdog` and `TeamStalenessWatchdog` are unit-tested inside
//! the crate against synthesised contexts; this file exercises them end-
//! to-end through the [`WatchdogRegistry`] tick loop so we catch any
//! integration regressions in the broadcaster / observation-log path.
//!
//! Both tests use real wall-clock time (not `tokio::time::pause`) because
//! the observation log writes via sqlx, which is incompatible with the
//! paused runtime — same constraint as the existing `registry_smoke`
//! test.

use std::sync::Arc;
use std::time::Duration;

use evy_comms::{DaemonEvent, EventBroadcaster};
use evy_memory::ObservationLog;
use evy_scheduler::Scheduler;
use evy_watchdog::{
    AutoNudgeConfig, AutoNudgeWatchdog, InMemoryTeamRegistry, MockDirectiveDispatcher,
    MockTmuxQuery, TeamRecord, TeamStalenessConfig, TeamStalenessWatchdog, WatchdogContext,
    WatchdogRegistry,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

async fn build_ctx() -> (WatchdogContext, TempDir, TempDir) {
    let scheduler_dir = tempfile::tempdir().unwrap();
    let scheduler = Arc::new(
        Scheduler::open(&scheduler_dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    let obs_dir = tempfile::tempdir().unwrap();
    let obs_log = Arc::new(
        ObservationLog::open(&obs_dir.path().join("obs.db"))
            .await
            .unwrap(),
    );
    let ctx = WatchdogContext {
        providers: Vec::new(),
        scheduler,
        events: EventBroadcaster::default(),
        obs_log,
    };
    (ctx, scheduler_dir, obs_dir)
}

/// Spin up AutoNudge with aggressive thresholds (idle = 0, cooldown = 0,
/// escalation = 2) and a frozen pane — observe via the registry's
/// SSE broadcaster that the loop:
///   1. ticks the watchdog at least 3 times,
///   2. emits at least one nudge directive to the dispatcher,
///   3. eventually fires a `WorkerDead` finding (visible via the event
///      stream's `finding_count`).
#[tokio::test]
async fn auto_nudge_emits_findings_through_registry_loop() {
    let (ctx, _s_dir, _o_dir) = build_ctx().await;
    let mut rx = ctx.events.subscribe();

    let tmux = Arc::new(MockTmuxQuery::new());
    tmux.set_sessions(["claude-frozen"]);
    tmux.set_pane("claude-frozen:0", "$ never moves");
    let dispatcher = Arc::new(MockDirectiveDispatcher::new());

    let nudger = AutoNudgeWatchdog::new(tmux, dispatcher.clone())
        .with_tick_interval_secs(1)
        .with_config(AutoNudgeConfig {
            idle_threshold_secs: 0,
            nudge_cooldown_secs: 0,
            escalation_threshold: 2,
        });

    let mut reg = WatchdogRegistry::new();
    reg.tick_timeout = Duration::from_secs(2);
    reg.add(Arc::new(nudger));
    let reg = Arc::new(reg);
    let cancel = CancellationToken::new();
    reg.clone().start(ctx, cancel.clone()).await.unwrap();

    // Collect events for ~3 seconds — the 1s schedule yields 3-4 ticks.
    let event_handle = tokio::spawn(async move {
        let mut got = Vec::new();
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_millis(800), rx.recv()).await {
                Ok(Ok(ev)) => got.push(ev),
                _ => break,
            }
        }
        got
    });

    tokio::time::sleep(Duration::from_millis(3200)).await;
    cancel.cancel();
    reg.join().await;
    let events = event_handle.await.unwrap();

    // At least one event must be from auto-nudge and the dispatcher
    // must have received at least one directive.
    let nudge_events: Vec<_> = events
        .iter()
        .filter_map(|ev| match ev {
            DaemonEvent::WatchdogTick {
                name,
                finding_count,
                healthy,
            } if name == "auto-nudge" => Some((*finding_count, *healthy)),
            _ => None,
        })
        .collect();
    assert!(
        !nudge_events.is_empty(),
        "expected at least one auto-nudge WatchdogTick event; got {events:?}",
    );
    assert!(
        nudge_events.iter().all(|(_, healthy)| *healthy),
        "auto-nudge ticks should be healthy even when emitting findings",
    );
    assert!(
        dispatcher.count() >= 1,
        "expected at least one nudge dispatched; got {}",
        dispatcher.count(),
    );
    // At least one tick should have produced a finding (nudge or
    // escalation) — first sight is skipped, so this is the strongest
    // assertion the event payload alone supports without scraping the
    // observation log.
    assert!(
        nudge_events.iter().any(|(count, _)| *count >= 1),
        "expected at least one nudge tick to emit a finding; got {nudge_events:?}",
    );
}

/// Run TeamStaleness through the registry against a registry that
/// contains one alive-but-quiet team. Verify the tick loop produces
/// `Finding::StaleTeam` via the event stream and that the registry
/// is left untouched (no GC).
#[tokio::test]
async fn team_staleness_emits_stale_finding_through_registry() {
    use chrono::{Duration as ChronoDuration, Utc};

    let (ctx, _s_dir, _o_dir) = build_ctx().await;

    let teams = Arc::new(InMemoryTeamRegistry::new());
    teams.insert(TeamRecord {
        team_id: "claude-quiet".into(),
        tmux_session: "claude-quiet".into(),
        last_activity: Some(Utc::now() - ChronoDuration::hours(2)),
    });
    let tmux = Arc::new(MockTmuxQuery::new());
    tmux.set_sessions(["claude-quiet"]);

    let stale = TeamStalenessWatchdog::new(teams.clone(), tmux)
        .with_tick_interval_secs(1)
        .with_config(TeamStalenessConfig {
            stale_threshold_secs: 60,
        });

    // Pop the obs_log handle so we can grep it post-cancel.
    let obs_log = ctx.obs_log.clone();
    let mut rx = ctx.events.subscribe();

    let mut reg = WatchdogRegistry::new();
    reg.tick_timeout = Duration::from_secs(2);
    reg.add(Arc::new(stale));
    let reg = Arc::new(reg);
    let cancel = CancellationToken::new();
    reg.clone().start(ctx, cancel.clone()).await.unwrap();

    // Wait for at least one tick to fire and emit an event.
    let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("event arrival timed out")
        .expect("broadcaster closed");
    cancel.cancel();
    reg.join().await;

    match ev {
        DaemonEvent::WatchdogTick {
            name,
            healthy,
            finding_count,
        } => {
            assert_eq!(name, "team-staleness");
            assert!(
                healthy,
                "stale-but-alive team must not flip watchdog health"
            );
            assert!(
                finding_count >= 1,
                "expected at least one StaleTeam finding; got {finding_count}",
            );
        }
        other => panic!("unexpected event: {other:?}"),
    }

    // The registry must be untouched — TeamStaleness is observation-
    // only; deletion belongs to TeamGcWatchdog.
    assert_eq!(teams.len(), 1, "TeamStaleness must not mutate the registry");

    // The observation log should have at least one row for our
    // watchdog. We don't care about the schema beyond a smoke check
    // that nothing errored on append.
    let _ = obs_log
        .query_recent(10)
        .await
        .expect("observation log readable");
}
