//! Criterion #5 — scheduler runs at least one real operator-defined
//! cron job (and survives daemon restart).
//!
//! ADR 0020 cutover criterion #5: "Scheduler runs at least one real
//! operator-defined cron job, surviving one daemon restart."
//!
//! This file verifies the **operator-facing** parts of the scheduler
//! through its public API:
//!
//! 1. Register a realistic operator job (`daily-standup` at 9am
//!    weekdays).
//! 2. The scheduler's `list()` reports it; `list_runs()` initially
//!    returns nothing.
//! 3. Drop the scheduler, re-open the same db, the job is still there
//!    — restart survival is a pure persistence property.
//! 4. `start()` / `stop()` lifecycle on the reopened scheduler is
//!    clean.
//!
//! Why this does NOT wait 60s for a real fire: that's already covered
//! by `crates/evy-scheduler/tests/integration.rs::live_fire_within_window`
//! (75s budget, registers `* * * * *`, asserts a `Succeeded` run row).
//! The smoke test in `crates/evy/tests/smoke.rs` reaches the same
//! finish line via the daemon library. Adding a third minute-budget
//! test inflates the workspace test wall-clock without raising
//! coverage — REPORT.md cites both existing tests as criterion #5
//! evidence. This file owns the **persistence + restart** half of the
//! criterion, which the existing tests don't directly cover.

use chrono::Utc;
use evy_scheduler::{Job, JobAction, JobId, Scheduler};
use tempfile::tempdir;

/// Build the "daily standup" job the cutover-readiness scenario uses.
/// `0 9 * * 1-5` = 9 AM weekdays. The action is `LogHeartbeat` because
/// `DispatchMandate` and `InvokeShell` are stubbed in Phase 1 (see
/// `JobAction` rustdoc) — the persistence semantics are identical.
fn daily_standup_job() -> Job {
    Job {
        id: JobId::new(),
        name: "daily-standup-cutover".to_owned(),
        cron_expr: "0 9 * * 1-5".to_owned(),
        action: JobAction::LogHeartbeat,
        enabled: true,
        created_at: Utc::now(),
        last_run: None,
    }
}

#[tokio::test]
async fn operator_job_persists_across_scheduler_drop_and_reopen() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("scheduler.db");

    // First lifetime: register, sanity-check list, then drop. No
    // `start()` here — we're isolating the persistence property.
    let job_id = {
        let sched = Scheduler::open(&db).await.expect("open #1");
        let job = daily_standup_job();
        let id = job.id;
        sched.register(job).await.expect("register operator job");

        let listed = sched.list().await.expect("list #1");
        assert_eq!(
            listed.len(),
            1,
            "freshly registered job must appear in list"
        );
        assert_eq!(listed[0].name, "daily-standup-cutover");
        assert_eq!(listed[0].cron_expr, "0 9 * * 1-5");
        assert!(listed[0].enabled, "operator-registered job must be enabled");
        assert!(
            listed[0].last_run.is_none(),
            "fresh job must have no last_run"
        );
        id
        // sched dropped here.
    };

    // Second lifetime: same db path, same job must still be there.
    let sched = Scheduler::open(&db).await.expect("open #2");
    let listed = sched.list().await.expect("list #2");
    assert_eq!(
        listed.len(),
        1,
        "operator-registered job must survive scheduler drop + reopen",
    );
    assert_eq!(listed[0].id, job_id);
    assert_eq!(listed[0].name, "daily-standup-cutover");
    assert_eq!(listed[0].cron_expr, "0 9 * * 1-5");

    // And the lifecycle on the reopened scheduler still works — this
    // is the "restart survival" half of the criterion.
    sched.start().await.expect("start scheduler after restart");
    sched.stop().await.expect("stop scheduler cleanly");
}

#[tokio::test]
async fn list_runs_is_empty_before_any_fire() {
    // Sanity-checks the operator-visible runs view for a fresh job —
    // operators inspecting `/api/evy/scheduler/jobs/<id>/runs` (a
    // future endpoint) need to see an empty list rather than an error
    // when nothing has fired yet.
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("scheduler.db");
    let sched = Scheduler::open(&db).await.expect("open");
    let job = daily_standup_job();
    let job_id = job.id;
    sched.register(job).await.expect("register");
    let runs = sched.list_runs(job_id).await.expect("list_runs");
    assert!(runs.is_empty(), "no fires yet → runs list must be empty");
}

#[tokio::test]
async fn scheduler_start_stop_is_idempotent_against_no_jobs() {
    // The daemon will sometimes boot with zero operator-registered
    // jobs (fresh install / cleared config). The fire loop must not
    // panic in that case — covered indirectly by the scheduler crate's
    // own tests, but the operator-readiness story benefits from the
    // explicit guard.
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("empty.db");
    let sched = Scheduler::open(&db).await.expect("open");
    assert!(sched.list().await.expect("list").is_empty());
    sched.start().await.expect("start with no jobs");
    sched.stop().await.expect("stop with no jobs");
}
