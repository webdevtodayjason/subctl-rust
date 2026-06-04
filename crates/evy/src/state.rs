//! Daemon-side implementation of [`evy_comms::AppState`].
//!
//! evy-comms ships [`evy_comms::StubAppState`] for tests; the daemon
//! itself needs a state surface that reads through to the live
//! scheduler, policy, and observation log so the operator console's
//! dashboard endpoints (`/api/evy/workers`, `/api/evy/scheduler/jobs`,
//! `/api/evy/policy`) show real data.
//!
//! The trait is `async-trait`'d in evy-comms; we mirror that here so
//! the impl is visible to axum at handler-dispatch time.
//!
//! # Phase 3 Slice E scope
//!
//! - `workers()` returns an empty `Vec<WorkerSummary>` — the daemon
//!   doesn't yet maintain a worker registry (REPORT.md follow-up #7).
//!   The dashboard correctly serves the empty list; populating is
//!   additive when the registry lands.
//! - `jobs()` enumerates `Scheduler::list().await` and maps each row to
//!   the dashboard's narrow `JobSummary` shape.
//! - `policy()` returns a clone of the loaded `Policy`. Cheap enough
//!   (the policy is operator-sized, not gigabyte-scale).
//!
//! # Why hold `obs_log` here
//!
//! The `AppState` trait doesn't expose a `recent_events()` method
//! today, so the daemon's observation log isn't reached through this
//! trait. We still hold it as a field so a future evy-comms extension
//! (or a follow-up slice that adds a `/api/evy/observations` route)
//! can read through to the same handle without restructuring this
//! struct.

use std::sync::Arc;

use async_trait::async_trait;
use evy_comms::{AppState, JobSummary, WorkerSummary};
use evy_memory::ObservationLog;
use evy_policy::Policy;
use evy_scheduler::Scheduler;
use evy_skills::SkillRegistry;
use evy_thinking::ThinkingPartner;

/// Daemon-side `AppState` impl. Reads through to the live scheduler +
/// policy + observation log; mutations are explicitly NOT supported on
/// this surface (the dashboard is read-only).
///
/// Holds `Arc`-shared handles to keep clone-into-axum cheap and let
/// other surfaces (future TUI, future Discord status command) share the
/// same handles without rebuilding state.
#[derive(Clone)]
pub struct DaemonAppState {
    /// The live scheduler. `jobs()` calls `list().await` on it.
    pub scheduler: Arc<Scheduler>,
    /// The loaded policy. Cloned on every `policy()` call.
    pub policy: Arc<Policy>,
    /// The observation log. Reserved for a future
    /// `AppState::recent_events()` extension; not read by any current
    /// handler. Held so callers don't need to thread it separately.
    pub obs_log: Arc<ObservationLog>,
    /// Optional thinking-partner. Phase 6 — when present, the
    /// `POST /api/evy/chat` route delegates here. When absent, the
    /// route returns 503 (`unavailable`).
    pub thinking_partner: Option<Arc<ThinkingPartner>>,
    /// Optional skill registry the partner was built with. Surfaced
    /// to the chat client via the `skills_loaded` field in
    /// `ChatResponse` so the operator can see which skills the model
    /// could see this turn.
    pub skills: Option<Arc<SkillRegistry>>,
    /// P2 — display label for the active supervisor model, surfaced in the
    /// `/api/evy/context` meter (e.g. `"lm-studio/gemma-4-26b-a4b-it-mlx"`).
    pub supervisor_label: Option<String>,
}

impl DaemonAppState {
    /// Build a state surface from the three handles the daemon already
    /// owns. The `Arc`s are shared with the rest of the daemon, not
    /// cloned-into-isolation.
    #[must_use]
    pub fn new(
        scheduler: Arc<Scheduler>,
        policy: Arc<Policy>,
        obs_log: Arc<ObservationLog>,
    ) -> Self {
        Self {
            scheduler,
            policy,
            obs_log,
            thinking_partner: None,
            skills: None,
            supervisor_label: None,
        }
    }

    /// Attach a thinking-partner. Builder-style so the daemon
    /// construction in `run_daemon_with_shutdown` reads top-down.
    #[must_use]
    pub fn with_thinking_partner(mut self, partner: Arc<ThinkingPartner>) -> Self {
        self.thinking_partner = Some(partner);
        self
    }

    /// Attach the skill registry the partner was built with. The
    /// chat handler returns the registry's names in `skills_loaded`.
    #[must_use]
    pub fn with_skills(mut self, skills: Arc<SkillRegistry>) -> Self {
        self.skills = Some(skills);
        self
    }

    /// Attach the supervisor-model display label for the `/api/evy/context`
    /// meter. Builder-style for the same top-down construction.
    #[must_use]
    pub fn with_supervisor_label(mut self, label: String) -> Self {
        self.supervisor_label = Some(label);
        self
    }
}

#[async_trait]
impl AppState for DaemonAppState {
    async fn workers(&self) -> Vec<WorkerSummary> {
        // Phase 3 Slice E: no worker registry yet. Dashboard sees an
        // empty list; populating is additive when REPORT.md follow-up
        // #7 (worker registry) lands.
        Vec::new()
    }

    async fn jobs(&self) -> Vec<JobSummary> {
        match self.scheduler.list().await {
            Ok(jobs) => jobs
                .into_iter()
                .map(|j| JobSummary {
                    id: j.id,
                    name: j.name,
                    cron_expr: j.cron_expr,
                    action_kind: JobSummary::action_kind_tag(&j.action).to_owned(),
                    enabled: j.enabled,
                })
                .collect(),
            Err(e) => {
                // Surfacing this as an empty list lets the dashboard
                // stay alive; the error is logged for the operator's
                // log tail. Returning a populated Result via the trait
                // would need an evy-comms extension and is deliberately
                // out of scope for this slice.
                tracing::warn!(error = %e, "scheduler list() failed; returning empty jobs to dashboard");
                Vec::new()
            }
        }
    }

    async fn policy(&self) -> Policy {
        (*self.policy).clone()
    }

    fn thinking_partner(&self) -> Option<Arc<ThinkingPartner>> {
        self.thinking_partner.clone()
    }

    fn skills(&self) -> Option<Arc<SkillRegistry>> {
        self.skills.clone()
    }

    fn supervisor_label(&self) -> Option<String> {
        self.supervisor_label.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evy_scheduler::{Job, JobAction, JobId};
    use tempfile::tempdir;

    async fn fresh_state() -> (tempfile::TempDir, DaemonAppState) {
        let dir = tempdir().expect("tempdir");
        let scheduler_db = dir.path().join("scheduler.db");
        let obs_db = dir.path().join("obs.db");
        let scheduler = Arc::new(Scheduler::open(&scheduler_db).await.expect("scheduler"));
        let obs_log = Arc::new(ObservationLog::open(&obs_db).await.expect("obs log"));
        let state = DaemonAppState::new(scheduler, Arc::new(Policy::default()), obs_log);
        (dir, state)
    }

    #[tokio::test]
    async fn empty_scheduler_yields_empty_jobs_summary() {
        let (_dir, state) = fresh_state().await;
        let jobs = state.jobs().await;
        assert!(jobs.is_empty());
    }

    #[tokio::test]
    async fn workers_returns_empty_until_registry_lands() {
        let (_dir, state) = fresh_state().await;
        let workers = state.workers().await;
        assert!(workers.is_empty());
    }

    #[tokio::test]
    async fn policy_returns_clone_of_loaded_policy() {
        let (_dir, state) = fresh_state().await;
        let p = state.policy().await;
        // Default policy serializes; spot-check that we got the same
        // shape twice (cloned, not moved).
        let p2 = state.policy().await;
        assert_eq!(format!("{p:?}"), format!("{p2:?}"));
    }

    #[tokio::test]
    async fn registered_job_appears_in_jobs_summary() {
        let (_dir, state) = fresh_state().await;
        let job = Job {
            id: JobId::new(),
            name: "nightly-sweep".to_owned(),
            cron_expr: "0 2 * * *".to_owned(),
            action: JobAction::LogHeartbeat,
            enabled: true,
            created_at: chrono::Utc::now(),
            last_run: None,
        };
        let job_id = job.id;
        state.scheduler.register(job).await.expect("register");

        let jobs = state.jobs().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job_id);
        assert_eq!(jobs[0].name, "nightly-sweep");
        assert_eq!(jobs[0].cron_expr, "0 2 * * *");
        assert_eq!(jobs[0].action_kind, "log_heartbeat");
        assert!(jobs[0].enabled);
    }
}
