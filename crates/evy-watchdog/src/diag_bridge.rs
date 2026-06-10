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
use evy_memory::ObservationLog;
use evy_scheduler::Scheduler;

use crate::heartbeat::{HeartbeatWatchdog, DEFAULT_HEARTBEAT_SECS};
use crate::trait_def::{Watchdog, WatchdogContext};

/// Register the daemon's default watchdog set into `registry`.
///
/// Today that's a single [`HeartbeatWatchdog`] — the boot-time liveness
/// canary that keeps `/api/evy/watchdogs/diag` populated with real ticking
/// data. The shared substrate (`scheduler`, `events`, `obs_log`) is folded
/// into the [`WatchdogContext`] every tick receives; `providers` is left
/// empty (the heartbeat doesn't enumerate workers).
///
/// Must be called from within a Tokio runtime — registration spawns each
/// watchdog's tick task.
pub fn register_default_watchdogs(
    registry: &WatchdogDiagRegistry,
    scheduler: Arc<Scheduler>,
    events: EventBroadcaster,
    obs_log: Arc<ObservationLog>,
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
    use crate::test_support::watchdog_ctx;

    #[tokio::test]
    async fn register_default_watchdogs_arms_a_ticking_heartbeat() {
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
}
