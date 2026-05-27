//! Test-only helpers for building a [`WatchdogContext`].
//!
//! Phase 4 watchdogs ignore the `providers` and `scheduler` fields of
//! the context, but they're part of the public trait contract — so the
//! tests need to build a real (if empty) context per call.
//!
//! `test_support` is `#[cfg(test)]`-only to keep production builds free
//! of the tempdir + scheduler-open cost.

#![cfg(test)]

use std::sync::Arc;

use evy_comms::EventBroadcaster;
use evy_memory::ObservationLog;
use evy_scheduler::Scheduler;
use tempfile::TempDir;

use crate::trait_def::WatchdogContext;

/// RAII guards the test must keep alive for the duration of its body.
/// Dropping these closes the sqlite dbs and removes the temp dirs.
///
/// Fields are deliberately held (not consumed) so the temp dirs and
/// the scheduler outlive every reference the [`WatchdogContext`] holds.
pub struct CtxGuards {
    pub _scheduler_dir: TempDir,
    pub _obs_dir: TempDir,
    pub _scheduler: Arc<Scheduler>,
}

/// Build a working [`WatchdogContext`] backed by ephemeral sqlite dbs.
pub async fn watchdog_ctx() -> (WatchdogContext, CtxGuards) {
    let scheduler_dir = tempfile::tempdir().expect("scheduler tempdir");
    let scheduler = Arc::new(
        Scheduler::open(&scheduler_dir.path().join("s.db"))
            .await
            .expect("scheduler"),
    );

    let obs_dir = tempfile::tempdir().expect("obs tempdir");
    let obs_log = Arc::new(
        ObservationLog::open(&obs_dir.path().join("obs.db"))
            .await
            .expect("observation log"),
    );

    let events = EventBroadcaster::default();
    let ctx = WatchdogContext {
        providers: Vec::new(),
        scheduler: scheduler.clone(),
        events,
        obs_log,
    };
    (
        ctx,
        CtxGuards {
            _scheduler_dir: scheduler_dir,
            _obs_dir: obs_dir,
            _scheduler: scheduler,
        },
    )
}
