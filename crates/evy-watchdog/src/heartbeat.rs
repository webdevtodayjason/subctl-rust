//! [`HeartbeatWatchdog`] — a minimal, self-contained liveness watchdog.
//!
//! The framework's other impls ([`crate::IdlePaneWatchdog`],
//! [`crate::TeamGcWatchdog`], …) reach into tmux / team-registry state and
//! only produce findings when the host environment has live sessions. The
//! daemon needs *something* that ticks unconditionally so the diag surface
//! (`/api/evy/watchdogs/diag`) has live data on a clean boot — a heartbeat is
//! the minimal honest answer. It declares the watchdog subsystem is alive and
//! ticking; it never produces a finding.

use async_trait::async_trait;
use evy_core::Result;

use crate::report::TickReport;
use crate::trait_def::{Watchdog, WatchdogContext, WatchdogSchedule};

/// Default heartbeat cadence in seconds when none is specified.
pub const DEFAULT_HEARTBEAT_SECS: u64 = 15;

/// A watchdog that ticks healthy on a fixed cadence and never flags a
/// finding. Used as the daemon's boot-time liveness canary so the diag
/// surface always has at least one real ticking watchdog.
#[derive(Debug, Clone)]
pub struct HeartbeatWatchdog {
    period_secs: u64,
}

impl HeartbeatWatchdog {
    /// Build a heartbeat that ticks every `period_secs` seconds.
    #[must_use]
    pub fn new(period_secs: u64) -> Self {
        Self { period_secs }
    }
}

impl Default for HeartbeatWatchdog {
    fn default() -> Self {
        Self::new(DEFAULT_HEARTBEAT_SECS)
    }
}

#[async_trait]
impl Watchdog for HeartbeatWatchdog {
    fn name(&self) -> &str {
        "heartbeat"
    }

    fn schedule(&self) -> WatchdogSchedule {
        WatchdogSchedule::EveryNSecs(self.period_secs)
    }

    async fn tick(&self, _ctx: &WatchdogContext) -> Result<TickReport> {
        Ok(TickReport::healthy(self.name()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::watchdog_ctx;

    #[test]
    fn name_and_schedule_are_stable() {
        let hb = HeartbeatWatchdog::new(15);
        assert_eq!(hb.name(), "heartbeat");
        assert_eq!(hb.schedule(), WatchdogSchedule::EveryNSecs(15));
    }

    #[test]
    fn default_uses_default_cadence() {
        assert_eq!(
            HeartbeatWatchdog::default().schedule(),
            WatchdogSchedule::EveryNSecs(DEFAULT_HEARTBEAT_SECS)
        );
    }

    #[tokio::test]
    async fn tick_is_always_healthy() {
        let (ctx, _guards) = watchdog_ctx().await;
        let hb = HeartbeatWatchdog::new(1);
        let report = hb.tick(&ctx).await.expect("tick");
        assert!(report.healthy);
        assert_eq!(report.watchdog, "heartbeat");
    }
}
