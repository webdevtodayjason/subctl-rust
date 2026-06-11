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
//! tiny trait here and ship an [`InMemoryTeamRegistry`] for tests.
//!
//! Wave 4 (watchdog boot) adds [`WorkerTeamRegistry`]: the production
//! impl backing the watchdogs with the daemon's real bookkeeping — a
//! team-granular view over [`evy_core::WorkerRegistry`] (the registry
//! the spawn path writes). The daemon passes it to
//! [`crate::register_default_watchdogs`] at boot.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use evy_core::{Result, WorkerRegistry};

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
/// Production boots use [`WorkerTeamRegistry`] instead.
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

/// [`TeamRegistry`] view over the daemon's live
/// [`evy_core::WorkerRegistry`] — one "team" per tmux session.
///
/// The daemon's bookkeeping is worker-granular (the spawn path in
/// `evy::state` registers a [`evy_core::WorkerRecord`] per dispatch);
/// the team watchdogs reason about tmux sessions. This adapter folds
/// workers into teams:
///
/// - Workers with no `tmux_session` are skipped — there is no session
///   to probe for liveness, and worker-level health belongs to
///   [`crate::AutoNudgeWatchdog`].
/// - `team_id` is the tmux session name, matching the spawn path's
///   `dashboard_team_event(&session, …)` vocabulary so the cockpit's
///   `team_event` / `watchdog_fire` frames name the same entity.
/// - `last_activity` is the most recent `last_activity_ms` across the
///   session's workers. Registration counts as activity (the spawn
///   path sets `last_activity_ms = now`), so a freshly-spawned team is
///   never instantly stale.
///
/// `remove(team_id)` drops every worker hosted by that session — the
/// GC watchdog's "session is gone AND activity is stale" sweep.
#[derive(Debug, Clone)]
pub struct WorkerTeamRegistry {
    workers: WorkerRegistry,
}

impl WorkerTeamRegistry {
    /// Wrap the daemon's shared worker registry. `WorkerRegistry` is
    /// `Arc`-backed, so the clone shares state with the spawn path.
    #[must_use]
    pub fn new(workers: WorkerRegistry) -> Self {
        Self { workers }
    }
}

#[async_trait]
impl TeamRegistry for WorkerTeamRegistry {
    async fn list(&self) -> Result<Vec<TeamRecord>> {
        let mut by_session: HashMap<String, TeamRecord> = HashMap::new();
        for record in self.workers.list() {
            let Some(session) = record.tmux_session else {
                continue;
            };
            // Out-of-range millis can't happen for real clocks; map the
            // theoretical failure to "no activity" rather than erroring.
            let activity = DateTime::from_timestamp_millis(record.last_activity_ms);
            match by_session.entry(session) {
                Entry::Occupied(mut e) => {
                    let team = e.get_mut();
                    // `None < Some(_)` and `Some(a) < Some(b)` iff `a < b`,
                    // so this keeps the most recent activity in the team.
                    if activity > team.last_activity {
                        team.last_activity = activity;
                    }
                }
                Entry::Vacant(e) => {
                    let record = TeamRecord {
                        team_id: e.key().clone(),
                        tmux_session: e.key().clone(),
                        last_activity: activity,
                    };
                    e.insert(record);
                }
            }
        }
        Ok(by_session.into_values().collect())
    }

    async fn remove(&self, team_id: &str) -> Result<()> {
        for record in self.workers.list() {
            if record.tmux_session.as_deref() == Some(team_id) {
                self.workers.remove(&record.id);
            }
        }
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

    mod worker_team_registry {
        use super::super::*;
        use evy_core::{MandateId, ProviderKind, WorkerId, WorkerRecord};

        /// Register a worker with the given session + activity stamp,
        /// returning its id for later assertions.
        fn add_worker(
            workers: &WorkerRegistry,
            session: Option<&str>,
            last_activity_ms: i64,
        ) -> WorkerId {
            let mut record = WorkerRecord::running(
                WorkerId::new(),
                ProviderKind::ClaudeCode,
                MandateId::new(),
                last_activity_ms,
            );
            record.tmux_session = session.map(str::to_owned);
            let id = record.id;
            workers.register(record);
            id
        }

        #[tokio::test]
        async fn folds_workers_into_one_team_per_session() {
            let workers = WorkerRegistry::new();
            add_worker(&workers, Some("claude-alpha"), 1_000);
            add_worker(&workers, Some("claude-alpha"), 5_000);
            add_worker(&workers, Some("claude-beta"), 2_000);

            let teams = WorkerTeamRegistry::new(workers);
            let mut listed = teams.list().await.unwrap();
            listed.sort_by(|a, b| a.team_id.cmp(&b.team_id));
            assert_eq!(listed.len(), 2);

            let alpha = &listed[0];
            assert_eq!(alpha.team_id, "claude-alpha");
            assert_eq!(alpha.tmux_session, "claude-alpha");
            // The team's activity is the MOST RECENT worker's.
            assert_eq!(alpha.last_activity, DateTime::from_timestamp_millis(5_000),);
            assert_eq!(listed[1].team_id, "claude-beta");
        }

        #[tokio::test]
        async fn sessionless_workers_are_not_teams() {
            let workers = WorkerRegistry::new();
            add_worker(&workers, None, 1_000);
            let teams = WorkerTeamRegistry::new(workers.clone());
            assert!(teams.list().await.unwrap().is_empty());
            assert_eq!(workers.len(), 1, "worker itself remains tracked");
        }

        #[tokio::test]
        async fn remove_drops_only_that_sessions_workers() {
            let workers = WorkerRegistry::new();
            add_worker(&workers, Some("claude-dead"), 1_000);
            add_worker(&workers, Some("claude-dead"), 2_000);
            let survivor = add_worker(&workers, Some("claude-alive"), 3_000);
            let orphan = add_worker(&workers, None, 4_000);

            let teams = WorkerTeamRegistry::new(workers.clone());
            teams.remove("claude-dead").await.unwrap();

            assert_eq!(workers.len(), 2);
            assert!(workers.get(&survivor).is_some());
            assert!(workers.get(&orphan).is_some());
            // Idempotent — removing an unknown team is fine.
            teams.remove("claude-dead").await.unwrap();
            teams.remove("nope").await.unwrap();
            assert_eq!(workers.len(), 2);
        }

        #[tokio::test]
        async fn shares_state_with_the_spawn_paths_registry_clone() {
            // The daemon hands the watchdogs a CLONE of the registry the
            // spawn path writes — both must see the same rows.
            let workers = WorkerRegistry::new();
            let teams = WorkerTeamRegistry::new(workers.clone());
            assert!(teams.list().await.unwrap().is_empty());
            add_worker(&workers, Some("claude-late"), 9_000);
            let listed = teams.list().await.unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].team_id, "claude-late");
        }
    }
}
