//! Integration smoke test for [`evy_watchdog::WatchdogRegistry`].
//!
//! Spins up the registry with three mock watchdogs that each count
//! their tick invocations, lets the loop run under real wall-clock
//! time, then verifies every watchdog ticked at least once and that
//! the broadcaster received [`evy_comms::DaemonEvent::WatchdogTick`]
//! events for each.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use evy_comms::{DaemonEvent, EventBroadcaster};
use evy_core::Result;
use evy_memory::ObservationLog;
use evy_scheduler::Scheduler;
use evy_watchdog::{TickReport, Watchdog, WatchdogContext, WatchdogRegistry, WatchdogSchedule};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

struct CountingWatchdog {
    name: String,
    period_secs: u64,
    ticks: Arc<AtomicUsize>,
}

#[async_trait]
impl Watchdog for CountingWatchdog {
    fn name(&self) -> &str {
        &self.name
    }
    fn schedule(&self) -> WatchdogSchedule {
        WatchdogSchedule::EveryNSecs(self.period_secs)
    }
    async fn tick(&self, _ctx: &WatchdogContext) -> Result<TickReport> {
        self.ticks.fetch_add(1, Ordering::SeqCst);
        Ok(TickReport::healthy(&self.name))
    }
}

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

#[tokio::test]
async fn three_watchdogs_each_tick_within_window() {
    let (ctx, _s_dir, _o_dir) = build_ctx().await;
    let mut rx = ctx.events.subscribe();

    let ticks_a = Arc::new(AtomicUsize::new(0));
    let ticks_b = Arc::new(AtomicUsize::new(0));
    let ticks_c = Arc::new(AtomicUsize::new(0));

    let mut reg = WatchdogRegistry::new();
    reg.add(Arc::new(CountingWatchdog {
        name: "alpha".into(),
        period_secs: 1,
        ticks: ticks_a.clone(),
    }));
    reg.add(Arc::new(CountingWatchdog {
        name: "beta".into(),
        period_secs: 1,
        ticks: ticks_b.clone(),
    }));
    reg.add(Arc::new(CountingWatchdog {
        name: "gamma".into(),
        period_secs: 1,
        ticks: ticks_c.clone(),
    }));
    let reg = Arc::new(reg);

    let cancel = CancellationToken::new();
    reg.clone().start(ctx, cancel.clone()).await.unwrap();

    // Collect events (best-effort) while the loop runs.
    let event_handle = tokio::spawn(async move {
        let mut got = Vec::new();
        // Drain at most 6 events or until the channel closes.
        for _ in 0..6 {
            match tokio::time::timeout(Duration::from_millis(800), rx.recv()).await {
                Ok(Ok(ev)) => got.push(ev),
                _ => break,
            }
        }
        got
    });

    // Let the loop run for ~1.5s — every 1s-period watchdog should
    // tick at least once in that window.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    cancel.cancel();
    reg.join().await;
    let events = event_handle.await.unwrap();

    assert!(
        ticks_a.load(Ordering::SeqCst) >= 1,
        "alpha did not tick (count={})",
        ticks_a.load(Ordering::SeqCst),
    );
    assert!(
        ticks_b.load(Ordering::SeqCst) >= 1,
        "beta did not tick (count={})",
        ticks_b.load(Ordering::SeqCst),
    );
    assert!(
        ticks_c.load(Ordering::SeqCst) >= 1,
        "gamma did not tick (count={})",
        ticks_c.load(Ordering::SeqCst),
    );

    // We expect at least one WatchdogTick event per registered watchdog.
    let mut saw_alpha = false;
    let mut saw_beta = false;
    let mut saw_gamma = false;
    for ev in events {
        if let DaemonEvent::WatchdogTick { name, healthy, .. } = ev {
            assert!(healthy, "{name} should report healthy");
            match name.as_str() {
                "alpha" => saw_alpha = true,
                "beta" => saw_beta = true,
                "gamma" => saw_gamma = true,
                other => panic!("unexpected watchdog name: {other}"),
            }
        }
    }
    assert!(saw_alpha && saw_beta && saw_gamma, "missed an event");
}

#[tokio::test]
async fn empty_registry_starts_and_stops_cleanly() {
    let (ctx, _s_dir, _o_dir) = build_ctx().await;
    let reg = Arc::new(WatchdogRegistry::new());
    assert!(reg.is_empty());
    let cancel = CancellationToken::new();
    reg.clone().start(ctx, cancel.clone()).await.unwrap();
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), reg.join())
        .await
        .expect("join should complete promptly");
}
