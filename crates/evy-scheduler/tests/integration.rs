//! End-to-end smoke tests for the scheduler.
//!
//! Two scenarios:
//!
//! 1. [`live_fire_within_window`] — register a `LogHeartbeat` job with the
//!    every-minute cron `* * * * *`, start the scheduler, wait up to
//!    ~70 seconds for one fire, then assert a `runs` row appeared and
//!    `last_run` was bumped on the job row. This is necessarily a wall-
//!    clock test: 5-field cron's minimum granularity is one minute and
//!    we deliberately do not introduce a 6-field (seconds) surface in
//!    Phase 1.
//! 2. [`survives_restart`] — open the scheduler against a tempfile,
//!    register a job, drop the scheduler, re-open against the same
//!    path, and assert the job is still there with the same cron and
//!    name. Confirms the restart-survival path is a pure persistence
//!    property (no in-memory state beyond the `jobs` table).

use std::time::{Duration, Instant};

use evy_scheduler::{Job, JobAction, JobId, Scheduler};
use tempfile::tempdir;
use tokio::time::sleep;

fn heartbeat(cron: &str, name: &str) -> Job {
    Job {
        id: JobId::new(),
        name: name.to_owned(),
        cron_expr: cron.to_owned(),
        action: JobAction::LogHeartbeat,
        enabled: true,
        created_at: chrono::Utc::now(),
        last_run: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_fire_within_window() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("scheduler.db");
    let sched = Scheduler::open(&db).await.expect("open");
    let job = heartbeat("* * * * *", "live-fire-heartbeat");
    let job_id = job.id;
    sched.register(job).await.expect("register");
    sched.start().await.expect("start");

    // Worst-case wait: the next minute boundary is up to 60 seconds
    // away; the fire loop wakes within sub-second granularity once
    // tokio's timer fires. Budget 75 seconds to absorb scheduler-side
    // latency on a loaded test runner.
    let budget = Duration::from_secs(75);
    let started = Instant::now();
    let mut runs = Vec::new();
    while started.elapsed() < budget {
        runs = sched.list_runs(job_id).await.expect("list_runs");
        if !runs.is_empty() {
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }

    sched.stop().await.expect("stop");

    assert!(
        !runs.is_empty(),
        "expected at least one run within {}s, got 0",
        budget.as_secs(),
    );
    let listed = sched.list().await.expect("list");
    assert_eq!(listed.len(), 1);
    assert!(
        listed[0].last_run.is_some(),
        "last_run should be set after a fire",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn survives_restart() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("scheduler.db");

    // First lifetime: register, then drop without firing.
    let original_id = {
        let sched = Scheduler::open(&db).await.expect("open #1");
        let job = heartbeat("0 0 * * *", "daily-midnight");
        let id = job.id;
        sched.register(job).await.expect("register");
        let listed = sched.list().await.expect("list #1");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "daily-midnight");
        // Explicit drop for clarity; the scoping block also drops it.
        drop(sched);
        id
    };

    // Second lifetime: same db path, expect the job to still be there.
    let sched = Scheduler::open(&db).await.expect("open #2");
    let listed = sched.list().await.expect("list #2");
    assert_eq!(
        listed.len(),
        1,
        "job should have survived scheduler drop+reopen",
    );
    assert_eq!(listed[0].id, original_id);
    assert_eq!(listed[0].name, "daily-midnight");
    assert_eq!(listed[0].cron_expr, "0 0 * * *");

    // Lifecycle on the re-opened scheduler still works cleanly.
    sched.start().await.expect("start #2");
    sched.stop().await.expect("stop #2");
}
