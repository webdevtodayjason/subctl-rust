//! Operator-defined cron jobs from `jobs.toml`.
//!
//! Closes cutover REPORT follow-up #10: the long-lived daemon registers
//! the operator's jobs at boot instead of relying on rows seeded by
//! tests. The file is synced *into* the scheduler by job name — entries
//! are added or replaced, but jobs absent from the file are NOT reaped,
//! so jobs registered through other surfaces survive a re-sync.
//!
//! Format (one `[[job]]` table per job):
//!
//! ```toml
//! [[job]]
//! name = "usage-snapshot"
//! cron = "0 * * * *"            # 5-field cron, minute-first
//! action = "shell"              # "shell" | "heartbeat"
//! cmd = "/absolute/bin --flag"  # required for action = "shell"
//! enabled = true                # optional, default true
//! ```
//!
//! A malformed entry (bad cron, missing `cmd`, duplicate name) is
//! logged and skipped; it never aborts daemon boot. A missing file is
//! a clean no-op.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use evy_scheduler::{Job, JobAction, JobId, Scheduler};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct JobsFile {
    #[serde(default, rename = "job")]
    jobs: Vec<JobEntry>,
}

#[derive(Debug, Deserialize)]
struct JobEntry {
    name: String,
    cron: String,
    action: ActionKind,
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    Heartbeat,
    Shell,
}

fn default_true() -> bool {
    true
}

impl JobEntry {
    fn to_action(&self) -> Result<JobAction> {
        match self.action {
            ActionKind::Heartbeat => Ok(JobAction::LogHeartbeat),
            ActionKind::Shell => {
                let cmd = self
                    .cmd
                    .as_deref()
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .context("action = \"shell\" requires a non-empty `cmd`")?;
                Ok(JobAction::InvokeShell(cmd.to_owned()))
            }
        }
    }
}

/// What one sync pass did, for the boot log line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncSummary {
    /// New names registered.
    pub registered: usize,
    /// Existing names whose definition changed (unregister + register).
    pub replaced: usize,
    /// Existing names whose definition already matched.
    pub unchanged: usize,
    /// Entries dropped for being malformed or duplicated.
    pub skipped: usize,
}

/// Sync `jobs.toml` at `path` into the scheduler.
///
/// Never returns an error: every failure mode is logged and reflected
/// in the summary so a bad file can't keep the daemon down.
pub async fn sync_jobs_file(scheduler: &Scheduler, path: &Path) -> SyncSummary {
    let mut summary = SyncSummary::default();

    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(path = %path.display(), "no jobs.toml; nothing to sync");
            return summary;
        }
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "could not read jobs.toml");
            return summary;
        }
    };

    let file: JobsFile = match toml::from_str(&raw) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "could not parse jobs.toml");
            return summary;
        }
    };

    let existing: HashMap<String, Job> = match scheduler.list().await {
        Ok(jobs) => jobs.into_iter().map(|j| (j.name.clone(), j)).collect(),
        Err(e) => {
            tracing::error!(error = %e, "could not list jobs; skipping jobs.toml sync");
            return summary;
        }
    };

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in file.jobs {
        if !seen.insert(entry.name.clone()) {
            tracing::warn!(name = %entry.name, "duplicate job name in jobs.toml; skipping");
            summary.skipped += 1;
            continue;
        }
        let action = match entry.to_action() {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(name = %entry.name, error = %e, "bad jobs.toml entry; skipping");
                summary.skipped += 1;
                continue;
            }
        };

        if let Some(current) = existing.get(&entry.name) {
            if definition_matches(current, &entry, &action) {
                summary.unchanged += 1;
                continue;
            }
            if let Err(e) = scheduler.unregister(current.id).await {
                tracing::warn!(name = %entry.name, error = %e, "could not replace job; skipping");
                summary.skipped += 1;
                continue;
            }
            match register(scheduler, &entry, action).await {
                Ok(()) => summary.replaced += 1,
                Err(e) => {
                    tracing::warn!(name = %entry.name, error = %e, "re-register failed; job dropped");
                    summary.skipped += 1;
                }
            }
        } else {
            match register(scheduler, &entry, action).await {
                Ok(()) => summary.registered += 1,
                Err(e) => {
                    tracing::warn!(name = %entry.name, error = %e, "register failed; skipping");
                    summary.skipped += 1;
                }
            }
        }
    }

    summary
}

fn definition_matches(current: &Job, entry: &JobEntry, action: &JobAction) -> bool {
    // JobAction doesn't impl PartialEq (Mandate payloads); compare the
    // serialized form, which is also what the jobs table stores.
    let same_action = serde_json::to_value(&current.action).ok()
        == serde_json::to_value(action).ok();
    current.cron_expr == entry.cron && current.enabled == entry.enabled && same_action
}

async fn register(scheduler: &Scheduler, entry: &JobEntry, action: JobAction) -> Result<()> {
    scheduler
        .register(Job {
            id: JobId::new(),
            name: entry.name.clone(),
            cron_expr: entry.cron.clone(),
            action,
            enabled: entry.enabled,
            created_at: Utc::now(),
            last_run: None,
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn open_scheduler(dir: &Path) -> Scheduler {
        Scheduler::open(&dir.join("s.db")).await.unwrap()
    }

    fn write(dir: &Path, content: &str) -> std::path::PathBuf {
        let p = dir.join("jobs.toml");
        std::fs::write(&p, content).unwrap();
        p
    }

    #[tokio::test]
    async fn missing_file_is_noop() {
        let dir = tempdir().unwrap();
        let s = open_scheduler(dir.path()).await;
        let summary = sync_jobs_file(&s, &dir.path().join("jobs.toml")).await;
        assert_eq!(summary, SyncSummary::default());
        assert!(s.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn registers_shell_and_heartbeat_jobs() {
        let dir = tempdir().unwrap();
        let s = open_scheduler(dir.path()).await;
        let p = write(
            dir.path(),
            r#"
            [[job]]
            name = "snap"
            cron = "0 * * * *"
            action = "shell"
            cmd = "/bin/echo hi"

            [[job]]
            name = "hb"
            cron = "*/5 * * * *"
            action = "heartbeat"
            "#,
        );
        let summary = sync_jobs_file(&s, &p).await;
        assert_eq!(summary.registered, 2);
        assert_eq!(summary.skipped, 0);
        let jobs = s.list().await.unwrap();
        assert_eq!(jobs.len(), 2);
        let snap = jobs.iter().find(|j| j.name == "snap").unwrap();
        assert!(matches!(&snap.action, JobAction::InvokeShell(c) if c == "/bin/echo hi"));
        assert!(snap.enabled);
    }

    #[tokio::test]
    async fn resync_is_idempotent_then_replaces_on_change() {
        let dir = tempdir().unwrap();
        let s = open_scheduler(dir.path()).await;
        let p = write(
            dir.path(),
            "[[job]]\nname = \"snap\"\ncron = \"0 * * * *\"\naction = \"heartbeat\"\n",
        );
        assert_eq!(sync_jobs_file(&s, &p).await.registered, 1);

        // Same file again: nothing changes, nothing duplicates.
        let again = sync_jobs_file(&s, &p).await;
        assert_eq!(again.unchanged, 1);
        assert_eq!(again.registered + again.replaced + again.skipped, 0);
        assert_eq!(s.list().await.unwrap().len(), 1);

        // Changed cron: the definition is replaced under the same name.
        let p = write(
            dir.path(),
            "[[job]]\nname = \"snap\"\ncron = \"30 * * * *\"\naction = \"heartbeat\"\n",
        );
        let changed = sync_jobs_file(&s, &p).await;
        assert_eq!(changed.replaced, 1);
        let jobs = s.list().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].cron_expr, "30 * * * *");
    }

    #[tokio::test]
    async fn bad_entries_skip_without_aborting_good_ones() {
        let dir = tempdir().unwrap();
        let s = open_scheduler(dir.path()).await;
        let p = write(
            dir.path(),
            r#"
            [[job]]
            name = "no-cmd"
            cron = "0 * * * *"
            action = "shell"

            [[job]]
            name = "bad-cron"
            cron = "not a cron"
            action = "heartbeat"

            [[job]]
            name = "good"
            cron = "0 9 * * 1-5"
            action = "heartbeat"
            "#,
        );
        let summary = sync_jobs_file(&s, &p).await;
        assert_eq!(summary.registered, 1);
        assert_eq!(summary.skipped, 2);
        let jobs = s.list().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "good");
    }

    #[tokio::test]
    async fn duplicate_names_in_file_keep_first() {
        let dir = tempdir().unwrap();
        let s = open_scheduler(dir.path()).await;
        let p = write(
            dir.path(),
            r#"
            [[job]]
            name = "snap"
            cron = "0 * * * *"
            action = "heartbeat"

            [[job]]
            name = "snap"
            cron = "5 * * * *"
            action = "heartbeat"
            "#,
        );
        let summary = sync_jobs_file(&s, &p).await;
        assert_eq!(summary.registered, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(s.list().await.unwrap()[0].cron_expr, "0 * * * *");
    }

    #[tokio::test]
    async fn parse_error_is_noop() {
        let dir = tempdir().unwrap();
        let s = open_scheduler(dir.path()).await;
        let p = write(dir.path(), "this is not toml [[[");
        let summary = sync_jobs_file(&s, &p).await;
        assert_eq!(summary, SyncSummary::default());
        assert!(s.list().await.unwrap().is_empty());
    }
}
