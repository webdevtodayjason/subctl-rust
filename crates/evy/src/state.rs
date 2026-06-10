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

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use evy_comms::{
    AppState, EventBroadcaster, JobSummary, SpawnError, SpawnRequest, WatchdogDiagRegistry,
    WorkerSummary,
};
use evy_core::{Mandate, MandateId, PolicyMode, ProviderKind, WorkerId, WorkerRegistry};
use evy_memory::ObservationLog;
use evy_policy::Policy;
use evy_providers::{
    AccountsStore, ClaudeCodeConfig, ClaudeCodeProvider, CodexConfig, CodexProvider,
};
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
    /// Telegram bridge handle for the notify/ask HTTP surface
    /// (criterion #6). `None` when `[comms.telegram]` is absent.
    pub telegram: Option<evy_comms::TelegramBridge>,
    /// P2 — display label for the active supervisor model, surfaced in the
    /// `/api/evy/context` meter (e.g. `"lm-studio/gemma-4-26b-a4b-it-mlx"`).
    pub supervisor_label: Option<String>,
    /// Cutover Phase 2 (2b) — the shared worker registry. `workers()` maps it to
    /// `WorkerSummary`; the dispatch path (2c) registers workers into the same
    /// `Arc`-backed instance. Empty by default (no workers until a dispatch runs).
    pub worker_registry: WorkerRegistry,
    /// Cutover Phase 2 (2j) — the SSE broadcaster, shared with the HTTP server,
    /// so `spawn_worker` emits `WorkerRegistered` onto the live bus. Defaults to a
    /// fresh (subscriber-less) broadcaster; the daemon overrides with the shared one.
    pub broadcaster: EventBroadcaster,
    /// W5 — the live watchdog diagnostics registry. `watchdog_diag()` reads
    /// through it for `/api/evy/watchdogs/diag` + restart/kill. `None` until
    /// `run_daemon` boots one.
    pub watchdog_diag: Option<Arc<WatchdogDiagRegistry>>,
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
            telegram: None,
            supervisor_label: None,
            worker_registry: WorkerRegistry::new(),
            broadcaster: EventBroadcaster::default(),
            watchdog_diag: None,
        }
    }

    /// Attach the watchdog diagnostics registry so `/api/evy/watchdogs/diag`
    /// and the restart/kill routes read through it. Builder-style.
    #[must_use]
    pub fn with_watchdog_diag(mut self, registry: Arc<WatchdogDiagRegistry>) -> Self {
        self.watchdog_diag = Some(registry);
        self
    }

    /// Attach a shared worker registry (the dispatch path holds a clone of the
    /// same `Arc`-backed instance). Builder-style for top-down construction.
    #[must_use]
    pub fn with_worker_registry(mut self, registry: WorkerRegistry) -> Self {
        self.worker_registry = registry;
        self
    }

    /// Attach the shared SSE broadcaster so `spawn_worker` emits onto the same
    /// bus the HTTP server serves. Builder-style.
    #[must_use]
    pub fn with_event_broadcaster(mut self, broadcaster: EventBroadcaster) -> Self {
        self.broadcaster = broadcaster;
        self
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

    /// Attach the Telegram bridge so `POST /api/evy/notify` and
    /// `POST /api/evy/ask` reach the operator (criterion #6).
    #[must_use]
    pub fn with_telegram(mut self, bridge: evy_comms::TelegramBridge) -> Self {
        self.telegram = Some(bridge);
        self
    }
}

#[async_trait]
impl AppState for DaemonAppState {
    async fn workers(&self) -> Vec<WorkerSummary> {
        // Cutover Phase 2 (2b): map the shared worker registry to the wire
        // shape. Empty until the dispatch path (2c) registers a worker.
        self.worker_registry
            .list()
            .into_iter()
            .map(|r| WorkerSummary {
                id: r.id,
                provider: r.provider,
                mandate_id: r.mandate_id,
                status: r.status,
            })
            .collect()
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

    fn telegram_bridge(&self) -> Option<evy_comms::TelegramBridge> {
        self.telegram.clone()
    }

    fn supervisor_label(&self) -> Option<String> {
        self.supervisor_label.clone()
    }

    fn watchdog_diag(&self) -> Option<Arc<WatchdogDiagRegistry>> {
        self.watchdog_diag.clone()
    }

    /// Cutover Phase 2 (2j) — spawn a real Claude Code worker on `req.account`:
    /// resolve the account from `accounts.conf`, ensure its tmux session exists
    /// (2e), construct an account-pinned `ClaudeCodeProvider`, then dispatch +
    /// register (2c) so `workers()` reflects it. Closes criterion #1 live.
    async fn spawn_worker(&self, req: SpawnRequest) -> std::result::Result<WorkerId, SpawnError> {
        let store = AccountsStore::open(&accounts_conf_path())
            .map_err(|e| SpawnError::Spawn(e.to_string()))?;
        let row = store
            .find_row(&req.account)
            .map_err(|e| SpawnError::Spawn(e.to_string()))?
            .ok_or_else(|| SpawnError::AccountNotFound(req.account.clone()))?;

        let working_dir = req
            .project
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| row.config_dir.clone());
        let session_slug = working_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("evy")
            .replace([' ', '.', ':'], "_");

        // Route on the account's provider (2i — codex-teams completes criterion #1).
        let is_codex = row.provider == "openai-codex";
        let provider_kind = if is_codex {
            ProviderKind::Codex
        } else {
            ProviderKind::ClaudeCode
        };
        let mandate = Mandate {
            id: MandateId::new(),
            provider: provider_kind,
            goal: req.goal,
            context: String::new(),
            deliverable: String::new(),
            done_when: Vec::new(),
            constraints: Vec::new(),
            policy_mode: PolicyMode::Gated,
            timeout: None,
            metadata: HashMap::new(),
        };
        let now_ms = chrono::Utc::now().timestamp_millis();

        let (worker_id, session) = if is_codex {
            let session = format!("codex-{session_slug}");
            let provider = CodexProvider::new(CodexConfig {
                codex_home: row.config_dir.clone(),
                codex_bin: codex_bin_path(),
                tmux_session: session.clone(),
                working_dir,
                model: None, // Codex picks per its config.toml (operator seeds gpt-5.5).
                policy_mode: PolicyMode::Gated,
                hmac_key: None,
            });
            provider
                .ensure_session()
                .await
                .map_err(|e| SpawnError::Spawn(e.to_string()))?;
            let id = crate::dispatch::dispatch_and_register(
                &provider,
                &mandate,
                &self.worker_registry,
                &self.broadcaster,
                now_ms,
            )
            .await
            .map_err(|e| SpawnError::Spawn(e.to_string()))?;
            (id, session)
        } else {
            let session = format!("claude-{session_slug}");
            let provider = ClaudeCodeProvider::new(ClaudeCodeConfig {
                claude_config_dir: row.config_dir.clone(),
                claude_bin: claude_bin_path(),
                tmux_session: session.clone(),
                working_dir,
                policy_mode: PolicyMode::Gated,
                hmac_key: None,
            });
            provider
                .ensure_session()
                .await
                .map_err(|e| SpawnError::Spawn(e.to_string()))?;
            let id = crate::dispatch::dispatch_and_register(
                &provider,
                &mandate,
                &self.worker_registry,
                &self.broadcaster,
                now_ms,
            )
            .await
            .map_err(|e| SpawnError::Spawn(e.to_string()))?;
            (id, session)
        };
        // 2l — record the hosting session so the Orch panel can probe liveness + kill.
        self.worker_registry.set_tmux_session(&worker_id, session);
        Ok(worker_id)
    }

    async fn orchestrations(&self) -> Vec<evy_comms::OrchestrationRow> {
        let mut rows = Vec::new();
        for r in self.worker_registry.list() {
            let alive = match &r.tmux_session {
                Some(s) => evy_providers::tmux_session_alive(s).await,
                None => false,
            };
            rows.push(evy_comms::OrchestrationRow {
                worker_id: r.id,
                provider: r.provider,
                status: format!("{:?}", r.status),
                tmux_session: r.tmux_session.clone(),
                alive,
                age_seconds: ((chrono::Utc::now().timestamp_millis() - r.created_at_ms).max(0)
                    / 1000) as u64,
                last_event: r.last_event.clone(),
            });
        }
        rows
    }

    async fn kill_worker(&self, id: WorkerId) -> std::result::Result<bool, SpawnError> {
        let Some(rec) = self.worker_registry.get(&id) else {
            return Ok(false); // unknown worker — nothing to kill
        };
        if let Some(session) = rec.tmux_session.as_deref() {
            evy_providers::tmux_kill_session(session)
                .await
                .map_err(|e| SpawnError::Spawn(e.to_string()))?;
        }
        self.worker_registry.remove(&id);
        Ok(true)
    }

    async fn orchestration_captures(&self, lines: usize) -> Vec<evy_comms::OrchestrationCapture> {
        let mut out = Vec::new();
        for r in self.worker_registry.list() {
            let text = match &r.tmux_session {
                Some(s) => evy_providers::tmux_capture(s, lines)
                    .await
                    .unwrap_or_default(),
                None => String::new(),
            };
            out.push(evy_comms::OrchestrationCapture {
                worker_id: r.id,
                session: r.tmux_session.clone(),
                text,
            });
        }
        out
    }
}

/// Path to `accounts.conf` (`SUBCTL_ACCOUNTS_CONF` or `~/.config/subctl/accounts.conf`).
fn accounts_conf_path() -> PathBuf {
    std::env::var("SUBCTL_ACCOUNTS_CONF")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(format!("{home}/.config/subctl/accounts.conf"))
        })
}

/// Absolute `claude` binary (`EVY_CLAUDE_BIN` or `~/.local/bin/claude`, the
/// native install — never bare `claude`, to dodge the PATH split-brain).
fn claude_bin_path() -> PathBuf {
    std::env::var("EVY_CLAUDE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(format!("{home}/.local/bin/claude"))
        })
}

/// Absolute `codex` binary (`EVY_CODEX_BIN` or `/opt/homebrew/bin/codex`, then
/// `~/.local/bin/codex` — never bare `codex`, same launchd-PATH dodge).
fn codex_bin_path() -> PathBuf {
    if let Ok(p) = std::env::var("EVY_CODEX_BIN") {
        return PathBuf::from(p);
    }
    for cand in ["/opt/homebrew/bin/codex", "/usr/local/bin/codex"] {
        if std::path::Path::new(cand).exists() {
            return PathBuf::from(cand);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(format!("{home}/.local/bin/codex"))
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
