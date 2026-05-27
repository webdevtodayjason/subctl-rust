//! Team registry abstraction — placeholder until v4 lands a real one.
//!
//! v3 stored team records on disk under
//! `~/.local/state/subctl/teams/<id>/` plus an in-memory
//! `teamLastActivity: Map<string, ...>`. v4 has not yet ported either:
//! `evy-core::Provider` exposes `dispatch / healthcheck` but no
//! `list_workers / list_teams`.
//!
//! Phase 4's team-GC + watchdog-prune still need *some* surface to
//! query "what teams does the daemon think exist?" — so we define a
//! tiny trait here and ship an [`InMemoryTeamRegistry`] for tests. The
//! daemon will swap in a real registry impl in Phase 5, at which point
//! this trait probably moves to `evy-core`.
//!
//! TODO: Phase 5 — promote `TeamRegistry` (or an equivalent
//! `evy_core::WorkerRegistry`) into `evy-core` and back the watchdogs
//! with the real daemon-side bookkeeping.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use evy_core::Result;

/// One row in the team registry.
#[derive(Debug, Clone)]
pub struct TeamRecord {
    /// Stable team id. In v3 this is `claude-<slug>`; v4 is provider-agnostic.
    pub team_id: String,
    /// The tmux session name the team runs in. In v3 this matched the
    /// team id, but in v4 we keep them separate so the watchdog can
    /// recognise vanished teams whose sessions used a different name.
    pub tmux_session: String,
    /// When the team last produced any observable activity (worker
    /// turn, inbox write, scheduler tick). `None` means "no activity
    /// since boot."
    pub last_activity: Option<DateTime<Utc>>,
}

/// Read-and-prune surface for the daemon-side team bookkeeping.
///
/// Phase 4 watchdogs only need `list()` + `drop()`. Phase 5 will add a
/// `touch(team_id, ts)` once auto-nudge ports.
#[async_trait]
pub trait TeamRegistry: Send + Sync {
    /// Every team the daemon currently tracks. Order is unspecified.
    async fn list(&self) -> Result<Vec<TeamRecord>>;

    /// Remove one team from the registry. Idempotent — removing an
    /// unknown id returns `Ok(())`.
    ///
    /// Named `remove` rather than `drop` because `drop` shadows the
    /// `Drop::drop` destructor and confuses both the compiler and
    /// human readers.
    async fn remove(&self, team_id: &str) -> Result<()>;
}

/// In-process registry used by tests and the watchdog smoke harness.
///
/// Daemon wiring substitutes a real impl in Phase 5; until then this
/// is the only implementation in the workspace.
#[derive(Debug, Default)]
pub struct InMemoryTeamRegistry {
    inner: Mutex<HashMap<String, TeamRecord>>,
}

impl InMemoryTeamRegistry {
    /// Build an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or replace) a team record. Convenience for tests; not
    /// part of the public trait because Phase 4 watchdogs only read.
    pub fn insert(&self, record: TeamRecord) {
        self.inner
            .lock()
            .expect("in-memory team registry poisoned")
            .insert(record.team_id.clone(), record);
    }

    /// Count the live records — used in tests + the smoke harness.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("in-memory team registry poisoned")
            .len()
    }

    /// True iff the registry holds no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl TeamRegistry for InMemoryTeamRegistry {
    async fn list(&self) -> Result<Vec<TeamRecord>> {
        Ok(self
            .inner
            .lock()
            .expect("in-memory team registry poisoned")
            .values()
            .cloned()
            .collect())
    }

    async fn remove(&self, team_id: &str) -> Result<()> {
        self.inner
            .lock()
            .expect("in-memory team registry poisoned")
            .remove(team_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, session: &str) -> TeamRecord {
        TeamRecord {
            team_id: id.into(),
            tmux_session: session.into(),
            last_activity: None,
        }
    }

    #[tokio::test]
    async fn list_and_remove_are_consistent() {
        let r = InMemoryTeamRegistry::new();
        r.insert(rec("alpha", "claude-alpha"));
        r.insert(rec("beta", "claude-beta"));
        assert_eq!(r.len(), 2);
        let listed = r.list().await.unwrap();
        assert_eq!(listed.len(), 2);
        r.remove("alpha").await.unwrap();
        assert_eq!(r.len(), 1);
        // Removing an unknown id is fine.
        r.remove("alpha").await.unwrap();
        r.remove("nope").await.unwrap();
        assert_eq!(r.len(), 1);
    }

    #[tokio::test]
    async fn empty_registry_lists_nothing() {
        let r = InMemoryTeamRegistry::new();
        assert!(r.is_empty());
        assert!(r.list().await.unwrap().is_empty());
    }
}
