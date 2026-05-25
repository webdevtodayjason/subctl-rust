//! `evy-scheduler` — cron-shaped job runner. Operator-definable schedules,
//! persisted to a sqlite database, must survive daemon restart cleanly.
//!
//! # Architecture
//!
//! - [`Scheduler`] owns a [`sqlx::SqlitePool`] and a spawned fire-loop task.
//! - [`Job`] is the persisted row shape (`jobs` table).
//! - [`Run`] is one execution of a job (`runs` table).
//! - [`JobAction`] is a closed-set enum over what a job *can* do at fire
//!   time. The set is intentionally narrow: no arbitrary shell, no
//!   plug-in code, no `eval`. New actions land by extending the enum.
//!
//! # Persistence
//!
//! Two tables in the scheduler's sqlite db; migrations live under
//! `migrations/` and run automatically on [`Scheduler::open`]. Restart
//! survival is purely "boot reads the `jobs` table" — no extra state
//! file. See the [`scheduler`] module rustdoc for fire-loop lifecycle.
//!
//! # Phase
//!
//! Phase 1 (Slice C) ships the cron parser + persistence + fire loop +
//! `LogHeartbeat` action wired end-to-end. `DispatchMandate` and
//! `InvokeShell` actions are stubbed (they log + record outcome) until
//! Phase 2 wires provider dispatch and the Trusted-shell guardrail.
//!
//! Operator-defined catch-up after downtime (per ADR 0020) is **not**
//! implemented in Phase 1; the loop only fires forward from `Utc::now()`
//! at boot. Adding catch-up is a future increment that needs the
//! "downtime tolerance" knob plumbed through config.

pub mod cron;
mod error;
pub mod job;
pub mod persistence;
pub mod scheduler;

pub use job::{Job, JobAction, JobId, Run, RunOutcome};
pub use scheduler::{RunId, Scheduler};

// Re-export the workspace error / result types so downstream crates can
// `use evy_scheduler::Result;` without also importing `evy-core`.
pub use evy_core::{Error, Result};
