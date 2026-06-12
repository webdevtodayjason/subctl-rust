//! Bridge framework [`Watchdog`]s into the daemon's
//! [`WatchdogDiagRegistry`](evy_comms::WatchdogDiagRegistry).
//!
//! The diag registry lives in `evy-comms` and ticks an opaque
//! [`TickFn`](evy_comms::TickFn) so it carries no dependency on this crate
//! (which already depends on `evy-comms` — the reverse edge would be a
//! cycle). This module is the adapter: it wraps a real `Watchdog` +
//! [`WatchdogContext`] into a `TickFn` and registers it, so the daemon's
//! `run_daemon` boot block stays a one-liner.

use std::sync::Arc;
use std::time::Duration;

use evy_comms::{EventBroadcaster, TickFn, TickOutcome, WatchdogDiagRegistry, WatchdogSpec};
use evy_core::WorkerRegistry;
use evy_memory::ObservationLog;
use evy_scheduler::Scheduler;

use crate::heartbeat::{HeartbeatWatchdog, DEFAULT_HEARTBEAT_SECS};
use crate::prune::WatchdogPrune;
use crate::team_gc::TeamGcWatchdog;
use crate::team_registry::TeamRegistry;
use crate::team_staleness::TeamStalenessWatchdog;
use crate::tmux_query::TmuxQuery;
use crate::trait_def::{Watchdog, WatchdogContext};

/// Register the daemon's default watchdog set into `registry`:
///
/// - [`HeartbeatWatchdog`] — the boot-time liveness canary that keeps
///   `/api/evy/watchdogs/diag` populated with real ticking data.
/// - [`TeamStalenessWatchdog`] — the "team is alive but quiet" canary.
///   Each tick broadcasts the cockpit's `watchdog_ok` / `watchdog_fire`
///   frame on `events` (the orchestration tab's live feed).
/// - [`TeamGcWatchdog`] — sweeps teams whose tmux session is dead AND
///   whose activity is stale, emitting `team_event` frames on removal.
/// - [`WatchdogPrune`] (armed W6, row ⑧ ruling) — the defensive sweep:
///   removes teams whose session is unequivocally absent (no grace, no
///   activity heuristic — deliberate overlap with gc, the layered
///   defense that survived v3's production fire) and reaps
///   terminal-status workers from `workers` after the 15-minute grace
///   (row ⑨ ruling — see `prune` module docs).
///
/// # Deliberately NOT registered (W6 row ⑧ ruling)
///
/// [`crate::IdlePaneWatchdog`] stays dormant. Three reasons:
/// 1. its findings feed nothing yet — the Phase 5 auto-retry gate and
///    recently-sent-directives registry it was scaffolded for don't
///    exist, and the diag bridge folds findings down to healthy/error,
///    so arming it adds zero operator signal;
/// 2. it enumerates by `claude-*` prefix-sniff over EVERY tmux session
///    on the box — on an operator machine that includes human-attached
///    panes, and capturing 50 lines of each every 30s is cost (and
///    pane-content reads) with no consumer;
/// 3. an idle pane is NORMAL on this fleet (operators park sessions),
///    so the finding would be mostly noise until the buffered-prompt
///    detection ports.
///
/// The impl + tests stay: they're the Phase 5 scaffold. Re-arm it here
/// when `Provider::list_workers()` and the directive registry land.
///
/// The team watchdogs read `teams` (the daemon passes a
/// [`crate::WorkerTeamRegistry`] over its live worker registry) and
/// probe session liveness through `tmux`. Cadences are each watchdog's
/// crate default ([`TeamStalenessWatchdog`] 600s,
/// [`TeamGcWatchdog`] 60s, [`WatchdogPrune`] 30s); the diag registry
/// fires the first tick immediately at registration, so frames flow
/// right after boot.
///
/// The shared substrate (`scheduler`, `events`, `obs_log`) is folded
/// into the [`WatchdogContext`] every tick receives; `providers` is left
/// empty (none of the defaults enumerate workers through providers).
///
/// Must be called from within a Tokio runtime — registration spawns each
/// watchdog's tick task.
pub fn register_default_watchdogs(
    registry: &WatchdogDiagRegistry,
    scheduler: Arc<Scheduler>,
    events: EventBroadcaster,
    obs_log: Arc<ObservationLog>,
    teams: Arc<dyn TeamRegistry>,
    tmux: Arc<dyn TmuxQuery>,
    workers: WorkerRegistry,
) {
    let ctx = WatchdogContext {
        providers: Vec::new(),
        scheduler,
        events,
        obs_log,
    };
    register_watchdog(
        registry,
        Arc::new(HeartbeatWatchdog::new(DEFAULT_HEARTBEAT_SECS)),
        ctx.clone(),
        true,
    );
    register_watchdog(
        registry,
        Arc::new(TeamStalenessWatchdog::new(teams.clone(), tmux.clone())),
        ctx.clone(),
        true,
    );
    register_watchdog(
        registry,
        Arc::new(TeamGcWatchdog::new(teams.clone(), tmux.clone())),
        ctx.clone(),
        true,
    );
    register_watchdog(
        registry,
        Arc::new(WatchdogPrune::new(teams, tmux).with_worker_reap(workers)),
        ctx,
        true,
    );
}

/// Adapt one framework [`Watchdog`] into a [`TickFn`] and register it with
/// the diag registry.
///
/// The watchdog's `id`/`kind` both come from its `name()` (matching v3,
/// where e.g. `inbox-poll`'s id and kind coincide). Its tick cadence and
/// `expected_interval_seconds` derive from its [`schedule`](Watchdog::schedule):
/// an `EveryNSecs(n)` schedule yields period `n`s and expected `n`; a cron
/// schedule (no fixed period) falls back to the heartbeat cadence with an
/// unknown expected interval.
///
/// Must be called from within a Tokio runtime.
pub fn register_watchdog(
    registry: &WatchdogDiagRegistry,
    watchdog: Arc<dyn Watchdog>,
    ctx: WatchdogContext,
    can_restart: bool,
) {
    let id = watchdog.name().to_owned();
    let period_secs = watchdog
        .schedule()
        .period_secs()
        .unwrap_or(DEFAULT_HEARTBEAT_SECS);
    // Cron schedules have no fixed cadence → expected interval is unknown.
    let expected_interval_secs = watchdog
        .schedule()
        .period_secs()
        .and_then(|s| i64::try_from(s).ok());

    let tick: TickFn = {
        let watchdog = watchdog.clone();
        Arc::new(move || {
            let watchdog = watchdog.clone();
            let ctx = ctx.clone();
            Box::pin(async move {
                match watchdog.tick(&ctx).await {
                    Ok(report) => TickOutcome {
                        healthy: report.healthy,
                        error: None,
                    },
                    Err(e) => TickOutcome {
                        healthy: false,
                        error: Some(e.to_string()),
                    },
                }
            })
        })
    };

    registry.register(
        WatchdogSpec {
            id: id.clone(),
            kind: id,
            expected_interval_secs,
            period: Duration::from_secs(period_secs),
            can_restart,
        },
        tick,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team_registry::InMemoryTeamRegistry;
    use crate::test_support::watchdog_ctx;
    use crate::tmux_query::MockTmuxQuery;

    #[tokio::test]
    async fn register_watchdog_arms_a_ticking_heartbeat() {
        let (ctx, _guards) = watchdog_ctx().await;
        let registry = WatchdogDiagRegistry::new();
        // Use a fast heartbeat so the test ticks quickly.
        register_watchdog(&registry, Arc::new(HeartbeatWatchdog::new(1)), ctx, true);

        // Poll until the heartbeat records its first tick.
        let mut ticked = false;
        for _ in 0..50 {
            let snap = registry.diag_snapshot(chrono::Utc::now());
            if snap
                .iter()
                .find(|w| w.id == "heartbeat")
                .is_some_and(|w| !w.tick_history.is_empty())
            {
                ticked = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ticked, "heartbeat should record a tick");

        let snap = registry.diag_snapshot(chrono::Utc::now());
        let hb = snap
            .iter()
            .find(|w| w.id == "heartbeat")
            .expect("heartbeat present");
        assert_eq!(hb.kind, "heartbeat");
        assert_eq!(hb.expected_interval_seconds, Some(1));
        assert!(hb.can_restart);
        assert_eq!(hb.status, evy_comms::WatchdogStatus::Healthy);
        registry.shutdown();
    }

    #[tokio::test]
    async fn default_set_arms_heartbeat_staleness_gc_and_prune() {
        let (ctx, _guards) = watchdog_ctx().await;
        let mut rx = ctx.events.subscribe();
        let registry = WatchdogDiagRegistry::new();
        register_default_watchdogs(
            &registry,
            ctx.scheduler.clone(),
            ctx.events.clone(),
            ctx.obs_log.clone(),
            Arc::new(InMemoryTeamRegistry::new()),
            Arc::new(MockTmuxQuery::new()),
            WorkerRegistry::new(),
        );

        // All four defaults are present with their crate-default
        // cadences (the heartbeat's diag surface is UNCHANGED by the
        // team-watchdog additions). IdlePaneWatchdog is deliberately
        // absent — see `register_default_watchdogs` docs (W6 row ⑧).
        let snap = registry.diag_snapshot(chrono::Utc::now());
        let expected = |id: &str| {
            snap.iter()
                .find(|w| w.id == id)
                .unwrap_or_else(|| panic!("{id} registered"))
                .expected_interval_seconds
        };
        assert_eq!(snap.len(), 4);
        assert!(
            !snap.iter().any(|w| w.id == "idle-pane"),
            "idle-pane must stay dormant until its Phase 5 consumers land",
        );
        assert_eq!(
            expected("heartbeat"),
            i64::try_from(DEFAULT_HEARTBEAT_SECS).ok()
        );
        assert_eq!(
            expected("team-staleness"),
            i64::try_from(crate::team_staleness::DEFAULT_TICK_INTERVAL_SECS).ok()
        );
        assert_eq!(
            expected("team-gc"),
            i64::try_from(crate::team_gc::DEFAULT_TICK_INTERVAL_SECS).ok()
        );
        assert_eq!(
            expected("watchdog-prune"),
            i64::try_from(crate::prune::DEFAULT_TICK_INTERVAL_SECS).ok()
        );

        // The diag registry fires the first tick immediately, so the
        // staleness sweep's `watchdog_ok` frame lands on the SHARED
        // broadcaster without waiting out the 600s cadence.
        let frame = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match rx.recv().await.expect("broadcaster open") {
                    evy_comms::DaemonEvent::DashboardFrame { event, data }
                        if event == "watchdog_ok" =>
                    {
                        break data;
                    }
                    _ => continue,
                }
            }
        })
        .await
        .expect("boot tick must broadcast watchdog_ok");
        let data: serde_json::Value = serde_json::from_str(&frame).expect("frame data is JSON");
        assert_eq!(data["teams_tracked"], 0);
        assert_eq!(data["stale"], 0);
        registry.shutdown();
    }

    #[tokio::test]
    async fn boot_tick_broadcasts_watchdog_ok_even_when_tmux_is_unreachable() {
        // W6.5 row ② — the test that would have caught the production
        // silence. The companion test above proves the boot tick on an
        // EMPTY registry (W4's live proof took the same path: no rows →
        // no tmux probe). The deployed daemon went dark the moment a
        // real team row landed, because launchd's bare PATH made every
        // tmux probe fail and the staleness tick aborted BEFORE its
        // emit. Same boot wiring as `run_daemon`: default set, shared
        // broadcaster, SSE-subscribed receiver — but with a team row
        // present and every probe erroring like production.
        let (ctx, _guards) = watchdog_ctx().await;
        let mut rx = ctx.events.subscribe();
        let teams = Arc::new(InMemoryTeamRegistry::new());
        teams.insert(crate::team_registry::TeamRecord {
            team_id: "w65-team".into(),
            tmux_session: "claude-w65-team".into(),
            last_activity: Some(chrono::Utc::now()),
        });
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_probe_error("spawn tmux: No such file or directory (os error 2)");

        let registry = WatchdogDiagRegistry::new();
        register_default_watchdogs(
            &registry,
            ctx.scheduler.clone(),
            ctx.events.clone(),
            ctx.obs_log.clone(),
            teams,
            tmux,
            WorkerRegistry::new(),
        );

        let frame = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match rx.recv().await.expect("broadcaster open") {
                    evy_comms::DaemonEvent::DashboardFrame { event, data }
                        if event == "watchdog_ok" =>
                    {
                        break data;
                    }
                    _ => continue,
                }
            }
        })
        .await
        .expect("boot tick must broadcast watchdog_ok despite failed tmux probes");
        let data: serde_json::Value = serde_json::from_str(&frame).expect("frame data is JSON");
        assert_eq!(data["teams_tracked"], 1);
        assert_eq!(data["stale"], 0);
        registry.shutdown();
    }
}
