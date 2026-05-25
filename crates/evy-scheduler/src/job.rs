//! `Job`, `JobId`, `JobAction`, `Run`, `RunOutcome`.
//!
//! These are the persisted shapes — `Job` round-trips through the `jobs`
//! table and `Run` round-trips through the `runs` table. `JobAction` is
//! a tagged-union enum serialized as JSON in the `jobs.action` column so
//! that adding a new action variant later only needs a migration if
//! existing rows must be rewritten.

use chrono::{DateTime, Utc};
use evy_core::Mandate;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Opaque identifier for a scheduled job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobId(pub Uuid);

impl JobId {
    /// Mint a fresh v4 UUID-backed job id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// What a job does when it fires.
///
/// Serialized as `{ "kind": "...", "data": ... }` in the `jobs.action`
/// column. The set is intentionally closed: no operator can register a
/// job that runs arbitrary code through the scheduler. New action shapes
/// are added by extending this enum and migrating rows on read if the
/// schema changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum JobAction {
    /// Dispatch a stored `Mandate` through the provider layer. Stubbed in
    /// Phase 1 — the fire loop logs the dispatch intent; provider wiring
    /// lands in Phase 2+ (binary-wirer is responsible for plumbing).
    DispatchMandate(Mandate),

    /// Emit a heartbeat log line. Used by the Phase 1 smoke test and as a
    /// reasonable default for operator-defined "is the daemon alive?"
    /// pings.
    LogHeartbeat,

    /// Invoke a shell command. Trusted-only and stubbed in Phase 1; the
    /// fire loop logs the intended command and records the run as
    /// `Failed("InvokeShell stubbed")`.
    InvokeShell(String),
}

/// An operator-defined cron job.
///
/// Fields mirror the `jobs` table directly to keep persistence boring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// Stable id assigned at registration.
    pub id: JobId,
    /// Human-readable name, unique across the jobs table.
    pub name: String,
    /// 5-field cron expression (minute hour day month weekday).
    pub cron_expr: String,
    /// What this job does when it fires.
    pub action: JobAction,
    /// Disabled jobs are stored but not fired.
    pub enabled: bool,
    /// When the row was first inserted.
    pub created_at: DateTime<Utc>,
    /// When the job most recently fired (UTC), if ever.
    pub last_run: Option<DateTime<Utc>>,
}

/// One fire of a job. One row in the `runs` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    /// Run id, distinct from the job id.
    pub id: Uuid,
    /// The job that produced this run.
    pub job_id: JobId,
    /// When the fire loop began executing the action.
    pub started_at: DateTime<Utc>,
    /// When the action returned (success or failure); `None` while pending.
    pub finished_at: Option<DateTime<Utc>>,
    /// Action outcome.
    pub outcome: RunOutcome,
}

/// Outcome of a run.
///
/// Serialized as `{ "kind": "...", "data": ... }` in the `runs.outcome`
/// column so that `Failed` can carry a message without inflating the row
/// shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "data")]
pub enum RunOutcome {
    /// Run inserted but not yet executed.
    Pending,
    /// Action completed without error.
    Succeeded,
    /// Action returned an error; message preserved.
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_id_default_is_unique() {
        assert_ne!(JobId::default(), JobId::default());
    }

    #[test]
    fn job_action_roundtrips_through_json() {
        let action = JobAction::LogHeartbeat;
        let s = serde_json::to_string(&action).unwrap();
        let back: JobAction = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, JobAction::LogHeartbeat));

        let shell = JobAction::InvokeShell("echo hi".to_owned());
        let s = serde_json::to_string(&shell).unwrap();
        let back: JobAction = serde_json::from_str(&s).unwrap();
        match back {
            JobAction::InvokeShell(cmd) => assert_eq!(cmd, "echo hi"),
            other => panic!("expected InvokeShell, got {other:?}"),
        }
    }

    #[test]
    fn run_outcome_failed_carries_message() {
        let o = RunOutcome::Failed("boom".to_owned());
        let s = serde_json::to_string(&o).unwrap();
        let back: RunOutcome = serde_json::from_str(&s).unwrap();
        assert_eq!(o, back);
    }
}
