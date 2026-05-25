//! The `Scheduler` struct + fire loop.
//!
//! # Lifecycle
//!
//! 1. [`Scheduler::open`] connects to (or creates) the sqlite db, runs
//!    migrations, and loads any previously-registered enabled jobs.
//! 2. [`Scheduler::start`] spawns the fire-loop task. The loop owns a
//!    `BTreeMap<DateTime<Utc>, JobId>` keyed by next-fire-time and
//!    sleeps until the earliest entry is due.
//! 3. [`Scheduler::register`] / [`Scheduler::unregister`] mutate both the
//!    `jobs` table and the live fire-loop schedule via a `tokio::mpsc`
//!    control channel.
//! 4. [`Scheduler::stop`] cancels the loop's [`CancellationToken`] and
//!    awaits the join handle. Idempotent.
//!
//! # Graceful shutdown — for binary-wirer
//!
//! `stop()` triggers the cancellation token, the loop's `select!` picks
//! up `cancel.cancelled()`, breaks out, and returns. The join handle is
//! awaited synchronously. If a job action is mid-execution when stop is
//! called, the run row stays `Pending` — that's documented as the
//! intended behaviour; we deliberately do not interrupt mid-flight
//! actions in Phase 1. Binary-wirer should call `stop()` from the daemon
//! shutdown handler and not assume zero in-flight runs.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use evy_core::Result;
use sqlx::SqlitePool;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::cron::next_fire_after;
use crate::job::{Job, JobAction, JobId, RunOutcome};
use crate::persistence;

/// Control messages sent to the fire-loop task.
///
/// `Upsert` boxes the `Job` so the enum stays small — the next-largest
/// variant is 16 bytes and we don't want every channel slot to carry a
/// `Job`-sized payload.
#[derive(Debug)]
enum Control {
    /// Add or replace a job in the live schedule.
    Upsert(Box<Job>),
    /// Remove a job from the live schedule.
    Remove(JobId),
}

/// Operator-defined cron job runner.
pub struct Scheduler {
    pool: SqlitePool,
    /// Sender into the fire loop. `None` before `start()` and after `stop()`.
    tx: Arc<Mutex<Option<mpsc::Sender<Control>>>>,
    /// Cancellation handle for the fire-loop task.
    cancel: CancellationToken,
    /// Join handle for the fire-loop task. `None` before `start()` and
    /// after `stop()` has awaited it.
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Scheduler {
    /// Open or create the scheduler's sqlite database at `db_path`, run
    /// migrations, and prepare a fresh `Scheduler` (not yet started).
    ///
    /// After this returns, the persisted jobs table is the source of
    /// truth; the in-memory schedule is empty until `start()` is called.
    pub async fn open(db_path: &Path) -> Result<Self> {
        let pool = persistence::open_pool(db_path).await?;
        Ok(Self {
            pool,
            tx: Arc::new(Mutex::new(None)),
            cancel: CancellationToken::new(),
            handle: Arc::new(Mutex::new(None)),
        })
    }

    /// Register a new job. Persists it to the `jobs` table and, if the
    /// fire loop is running, pushes it into the live schedule.
    ///
    /// The cron expression is validated up front; a malformed expression
    /// is rejected before the row is inserted.
    pub async fn register(&self, job: Job) -> Result<JobId> {
        crate::cron::validate(&job.cron_expr)?;
        let id = persistence::insert_job(&self.pool, &job).await?;
        if let Some(tx) = self.tx.lock().await.as_ref() {
            if tx.send(Control::Upsert(Box::new(job))).await.is_err() {
                // Loop is gone; the row is still on disk, which is fine
                // for restart-survival semantics. Log and continue.
                tracing::warn!(job_id = %id, "fire loop dropped; job persisted but not live");
            }
        }
        Ok(id)
    }

    /// Remove a job. Deletes the row and pulls it from the live schedule.
    pub async fn unregister(&self, id: JobId) -> Result<()> {
        persistence::delete_job(&self.pool, id).await?;
        if let Some(tx) = self.tx.lock().await.as_ref() {
            let _ = tx.send(Control::Remove(id)).await;
        }
        Ok(())
    }

    /// List all jobs in the table (enabled and disabled).
    pub async fn list(&self) -> Result<Vec<Job>> {
        persistence::list_jobs(&self.pool).await
    }

    /// List runs for a job, oldest first.
    pub async fn list_runs(&self, job_id: JobId) -> Result<Vec<crate::job::Run>> {
        persistence::list_runs_for_job(&self.pool, job_id).await
    }

    /// Boot the fire loop. Returns once the task is spawned (does not
    /// wait for the loop to drain or fire). Idempotent — calling it
    /// twice is a no-op after the first.
    pub async fn start(&self) -> Result<()> {
        let mut tx_slot = self.tx.lock().await;
        if tx_slot.is_some() {
            return Ok(());
        }
        let enabled = persistence::list_enabled_jobs(&self.pool).await?;
        let now = Utc::now();
        let mut initial: BTreeMap<DateTime<Utc>, JobId> = BTreeMap::new();
        for job in &enabled {
            match next_fire_after(&job.cron_expr, now)? {
                Some(next) => {
                    insert_unique(&mut initial, next, job.id);
                }
                None => {
                    tracing::warn!(
                        job_id = %job.id,
                        cron = %job.cron_expr,
                        "cron expression has no future fire time; skipping at boot",
                    );
                }
            }
        }
        // Index alongside the heap so we can both find a job by id (for
        // Remove) and look up the cron expression at fire time.
        let mut jobs_by_id: std::collections::HashMap<JobId, Job> =
            std::collections::HashMap::new();
        for job in enabled {
            jobs_by_id.insert(job.id, job);
        }

        let (tx, rx) = mpsc::channel::<Control>(64);
        let pool = self.pool.clone();
        let cancel = self.cancel.clone();
        let handle = tokio::spawn(fire_loop(pool, cancel, rx, initial, jobs_by_id));
        *tx_slot = Some(tx);
        *self.handle.lock().await = Some(handle);
        Ok(())
    }

    /// Gracefully shut down the fire loop. Awaits the spawned task; if
    /// it's already gone, returns immediately. Idempotent.
    pub async fn stop(&self) -> Result<()> {
        // Drop the sender first so the loop sees its rx close.
        {
            let mut tx_slot = self.tx.lock().await;
            tx_slot.take();
        }
        self.cancel.cancel();
        let handle_opt = self.handle.lock().await.take();
        if let Some(handle) = handle_opt {
            // We deliberately swallow the JoinError — a panicked fire
            // loop has already been logged via tracing.
            let _ = handle.await;
        }
        Ok(())
    }
}

fn insert_unique(map: &mut BTreeMap<DateTime<Utc>, JobId>, mut at: DateTime<Utc>, id: JobId) {
    // BTreeMap keyed by DateTime would collide if two jobs share a
    // fire-time. Bump by 1ns until we find a free slot — preserves
    // ordering and is monotonic within the loop's view.
    while map.contains_key(&at) {
        at += chrono::Duration::nanoseconds(1);
    }
    map.insert(at, id);
}

async fn fire_loop(
    pool: SqlitePool,
    cancel: CancellationToken,
    mut rx: mpsc::Receiver<Control>,
    mut schedule: BTreeMap<DateTime<Utc>, JobId>,
    mut jobs: std::collections::HashMap<JobId, Job>,
) {
    tracing::debug!(initial_jobs = jobs.len(), "scheduler fire loop started");
    loop {
        let next_fire = schedule.keys().next().copied();
        let sleep_until = next_fire.unwrap_or_else(|| Utc::now() + chrono::Duration::seconds(3600));
        let now = Utc::now();
        let sleep_dur = (sleep_until - now)
            .to_std()
            .unwrap_or(std::time::Duration::ZERO);

        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tracing::debug!("scheduler fire loop received cancel; exiting");
                return;
            }
            maybe_msg = rx.recv() => {
                match maybe_msg {
                    Some(Control::Upsert(boxed)) => {
                        let job = *boxed;
                        // Drop any existing schedule entry for this id, then
                        // recompute next fire time.
                        schedule.retain(|_, jid| *jid != job.id);
                        if job.enabled {
                            match next_fire_after(&job.cron_expr, Utc::now()) {
                                Ok(Some(next)) => insert_unique(&mut schedule, next, job.id),
                                Ok(None) => tracing::warn!(
                                    job_id = %job.id,
                                    "cron expression yields no future fire time",
                                ),
                                Err(e) => tracing::error!(
                                    job_id = %job.id,
                                    error = %e,
                                    "cron parse failed at upsert; job will not fire",
                                ),
                            }
                        }
                        jobs.insert(job.id, job);
                    }
                    Some(Control::Remove(id)) => {
                        schedule.retain(|_, jid| *jid != id);
                        jobs.remove(&id);
                    }
                    None => {
                        // Sender dropped (Scheduler::stop or Scheduler drop).
                        // A closed `rx.recv()` returns `None` instantly
                        // forever, which with `biased` selection would
                        // starve the sleep arm. Block on the cancel
                        // token instead — that's the only message we'll
                        // ever get from this point.
                        tracing::debug!(
                            "scheduler fire loop: control channel closed; awaiting cancel",
                        );
                        cancel.cancelled().await;
                        return;
                    }
                }
            }
            () = tokio::time::sleep(sleep_dur) => {
                fire_due(&pool, &mut schedule, &jobs).await;
            }
        }
    }
}

async fn fire_due(
    pool: &SqlitePool,
    schedule: &mut BTreeMap<DateTime<Utc>, JobId>,
    jobs: &std::collections::HashMap<JobId, Job>,
) {
    let now = Utc::now();
    let mut to_fire: Vec<(DateTime<Utc>, JobId)> = Vec::new();
    for (when, id) in schedule.iter() {
        if *when <= now {
            to_fire.push((*when, *id));
        } else {
            break; // BTreeMap is sorted; first non-due key ends the run.
        }
    }
    for (when, id) in to_fire {
        schedule.remove(&when);
        let Some(job) = jobs.get(&id) else {
            continue;
        };
        execute_one(pool, job).await;
        // Recompute next fire time from "now" rather than the missed
        // slot to avoid runaway catch-up on a slow loop.
        match next_fire_after(&job.cron_expr, Utc::now()) {
            Ok(Some(next)) => insert_unique(schedule, next, id),
            Ok(None) => tracing::info!(job_id = %id, "no further fires"),
            Err(e) => tracing::error!(job_id = %id, error = %e, "reschedule failed"),
        }
    }
}

async fn execute_one(pool: &SqlitePool, job: &Job) {
    let started = Utc::now();
    let run_id = match persistence::mark_run_started(pool, job.id, started).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(job_id = %job.id, error = %e, "could not record run start");
            return;
        }
    };
    let outcome = execute_action(&job.action, job).await;
    let finished = Utc::now();
    if let Err(e) = persistence::mark_run_finished(pool, run_id, finished, &outcome).await {
        tracing::error!(
            job_id = %job.id,
            run_id = %run_id,
            error = %e,
            "could not record run finish",
        );
    }
    if let Err(e) = persistence::update_last_run(pool, job.id, finished).await {
        tracing::error!(job_id = %job.id, error = %e, "could not update last_run");
    }
    tracing::info!(
        job_id = %job.id,
        run_id = %run_id,
        name = %job.name,
        outcome = ?outcome_kind(&outcome),
        "scheduler fired job",
    );
}

fn outcome_kind(o: &RunOutcome) -> &'static str {
    match o {
        RunOutcome::Pending => "pending",
        RunOutcome::Succeeded => "succeeded",
        RunOutcome::Failed(_) => "failed",
    }
}

/// Re-export the run id type used by `Scheduler::list_runs` callers so
/// they don't need to import `uuid` directly.
pub type RunId = Uuid;

async fn execute_action(action: &JobAction, job: &Job) -> RunOutcome {
    match action {
        JobAction::LogHeartbeat => {
            tracing::info!(job = %job.name, "heartbeat");
            RunOutcome::Succeeded
        }
        JobAction::DispatchMandate(mandate) => {
            // Phase 1: stub. Phase 2+ wires this through evy-providers.
            tracing::info!(
                job = %job.name,
                mandate_id = ?mandate.id,
                "dispatch-mandate stubbed; provider wiring lands in Phase 2",
            );
            RunOutcome::Succeeded
        }
        JobAction::InvokeShell(cmd) => {
            // Trusted-only and stubbed in Phase 1 per slice-C spec.
            tracing::warn!(job = %job.name, cmd, "invoke-shell stubbed");
            RunOutcome::Failed("InvokeShell stubbed in Phase 1".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobAction;
    use tempfile::tempdir;

    fn heartbeat_job(cron: &str) -> Job {
        Job {
            id: JobId::new(),
            name: format!("hb-{}", JobId::new()),
            cron_expr: cron.to_owned(),
            action: JobAction::LogHeartbeat,
            enabled: true,
            created_at: Utc::now(),
            last_run: None,
        }
    }

    #[tokio::test]
    async fn register_persists_job() {
        let dir = tempdir().unwrap();
        let s = Scheduler::open(&dir.path().join("s.db")).await.unwrap();
        let job = heartbeat_job("* * * * *");
        let id = s.register(job.clone()).await.unwrap();
        assert_eq!(id, job.id);
        let listed = s.list().await.unwrap();
        assert_eq!(listed.len(), 1);
    }

    #[tokio::test]
    async fn register_rejects_bad_cron() {
        let dir = tempdir().unwrap();
        let s = Scheduler::open(&dir.path().join("s.db")).await.unwrap();
        let mut job = heartbeat_job("* * * * *");
        job.cron_expr = "not a cron".into();
        let err = s.register(job).await.unwrap_err();
        assert!(err.to_string().contains("invalid cron"));
        // Bad cron must not leave a row behind.
        assert!(s.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unregister_removes_job() {
        let dir = tempdir().unwrap();
        let s = Scheduler::open(&dir.path().join("s.db")).await.unwrap();
        let job = heartbeat_job("* * * * *");
        s.register(job.clone()).await.unwrap();
        s.unregister(job.id).await.unwrap();
        assert!(s.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn start_then_stop_is_clean() {
        let dir = tempdir().unwrap();
        let s = Scheduler::open(&dir.path().join("s.db")).await.unwrap();
        s.start().await.unwrap();
        s.stop().await.unwrap();
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let dir = tempdir().unwrap();
        let s = Scheduler::open(&dir.path().join("s.db")).await.unwrap();
        s.start().await.unwrap();
        s.stop().await.unwrap();
        s.stop().await.unwrap(); // second call must not panic
    }
}
