//! The [`Watchdog`] trait + supporting [`WatchdogSchedule`] / [`WatchdogContext`].
//!
//! Every watchdog impl exposes three things:
//!
//! 1. A stable `name()` string — used as the log tag, the
//!    [`crate::TickReport::watchdog`] field, and the dashboard label.
//! 2. A `schedule()` declaring its preferred cadence — the registry's
//!    fire loop uses this to decide when to invoke `tick()`.
//! 3. `tick(&ctx)` — the I/O-bearing body, expected to complete within a
//!    short timeout (5 seconds by default; see [`crate::registry`]).
//!
//! Cron schedules are accepted in [`WatchdogSchedule::Cron`] but
//! Phase 4 only fires the [`WatchdogSchedule::EveryNSecs`] arm. Cron
//! firing lands in Phase 5 when the registry shares the scheduler's cron
//! evaluator.

use std::sync::Arc;

use async_trait::async_trait;
use evy_comms::EventBroadcaster;
use evy_core::{Provider, Result};
use evy_memory::ObservationLog;
use evy_scheduler::Scheduler;

use crate::report::TickReport;

/// How often a watchdog wants to be ticked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogSchedule {
    /// Fire every N seconds, anchored to the registry's start time.
    EveryNSecs(u64),
    /// Cron-syntax schedule. **Not fired in Phase 4** — registered but
    /// inert until Phase 5 wires the scheduler's cron evaluator into the
    /// watchdog loop. See `// TODO: Phase 5` in [`crate::registry`].
    Cron(String),
}

impl WatchdogSchedule {
    /// The next interval (in seconds) for an `EveryNSecs` schedule, or
    /// `None` for cron schedules. Used by the registry to compute when
    /// each watchdog is next due.
    #[must_use]
    pub fn period_secs(&self) -> Option<u64> {
        match self {
            Self::EveryNSecs(s) => Some(*s),
            // TODO: Phase 5 — evaluate cron string against `Utc::now()`.
            Self::Cron(_) => None,
        }
    }
}

/// Per-tick context handed to every watchdog.
///
/// The context is built once at registry start and cloned cheaply
/// (`Vec<Arc<...>>` + cloned broadcaster + cloned `Arc<…>`). Watchdogs
/// should NOT mutate it.
///
/// Today only `events` + `obs_log` are exercised by the Phase-4 impls:
/// `providers` is held for future `list_workers()`-style enumeration
/// (see Phase 5 TODOs in [`crate::idle_pane`]) and `scheduler` is held
/// so future watchdogs can audit pending / overdue scheduler jobs
/// without rebuilding the wiring.
#[derive(Clone)]
pub struct WatchdogContext {
    /// Live providers registered with the daemon. Phase 4 watchdogs do
    /// not iterate this — kept in the signature for Phase 5 (auto-nudge,
    /// team-staleness) which will need per-worker reach.
    pub providers: Vec<Arc<dyn Provider>>,
    /// Shared scheduler handle. Phase 4 watchdogs do not query it;
    /// reserved for `JobStallWatchdog` in Phase 5.
    pub scheduler: Arc<Scheduler>,
    /// Fan-out for [`evy_comms::DaemonEvent::WatchdogTick`] events.
    pub events: EventBroadcaster,
    /// Append-only observation log. Watchdogs append one observation
    /// per tick via the registry.
    pub obs_log: Arc<ObservationLog>,
}

/// One watchdog — a periodic check that emits findings and observations.
///
/// `tick()` MUST return within the registry's configured per-tick
/// timeout (5s default). Implementations should bound their I/O
/// internally; the registry's outer timeout is a safety net, not a
/// substitute for adapter-level deadlines.
///
/// `tick()` MAY return an `Err` — the registry catches it, emits an
/// unhealthy [`TickReport`], and continues. A panicking `tick` poisons
/// only its own join future; the loop keeps running.
#[async_trait]
pub trait Watchdog: Send + Sync {
    /// Stable identifier for this watchdog — used as the log tag and
    /// the [`crate::TickReport::watchdog`] field. Must be unique within
    /// a single registry (the registry's `add` panics on duplicates).
    fn name(&self) -> &str;

    /// Preferred cadence.
    fn schedule(&self) -> WatchdogSchedule;

    /// Perform the check. The registry handles the timeout, the
    /// broadcaster fan-out, and the observation-log append.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Provider`] for tmux / I/O failures
    /// the watchdog couldn't recover from inside the tick. The registry
    /// translates these into an unhealthy [`TickReport`].
    async fn tick(&self, ctx: &WatchdogContext) -> Result<TickReport>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_secs_returns_value_for_every_n() {
        let s = WatchdogSchedule::EveryNSecs(30);
        assert_eq!(s.period_secs(), Some(30));
    }

    #[test]
    fn period_secs_returns_none_for_cron() {
        let s = WatchdogSchedule::Cron("0 * * * *".into());
        assert_eq!(s.period_secs(), None);
    }

    /// Compile-time check that `Watchdog` is dyn-compatible. The fn is
    /// never called; if the trait stops being object-safe this file
    /// fails to type-check.
    #[allow(dead_code)]
    fn assert_watchdog_object_safe(w: Box<dyn Watchdog>) -> Box<dyn Watchdog> {
        w
    }
}
