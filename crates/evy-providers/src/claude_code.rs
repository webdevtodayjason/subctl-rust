//! Claude Code adapter — spawns a tmux window running the `claude` CLI
//! and pastes the mandate in as a directive.
//!
//! Ports the v3 `providers/claude/teams.sh` flow into the v4 trait
//! surface. The HMAC trust-marker wrapper from v3 is deferred to Phase
//! 2 (see [`compose_directive`]'s rustdoc).
//!
//! Lifetime: the spawned `claude` CLI runs in a tmux window inside an
//! operator-owned long-running session. The worker handle holds the
//! session + window names; tmux owns the actual process.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use evy_core::{
    Error, Mandate, MandateId, Provider, ProviderKind, Result, WorkerHandle, WorkerId, WorkerStatus,
};
use tokio::time::sleep;
use tracing::{debug, info, instrument};

use crate::config::ClaudeCodeConfig;
use crate::tmux::{self, TmuxScope};

const SCOPE: TmuxScope = TmuxScope(ProviderKind::ClaudeCode);

/// Phase-1 boot delay after `claude` launch before the directive is
/// pasted. v3 polls the pane for `❯` instead; the spec authorizes a
/// fixed sleep for Phase 1. TODO(phase-2): replace with capture-pane
/// polling for the `❯` marker so we stop guessing.
const CLAUDE_BOOT_SLEEP: Duration = Duration::from_secs(2);

/// Status-poll interval inside [`ClaudeCodeWorker::wait`].
const WAIT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Bounded wait when the mandate carries no explicit timeout. Prevents
/// `wait()` from running forever against a wedged worker.
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// `Provider` impl for Claude Code via tmux + the `claude` CLI.
pub struct ClaudeCodeProvider {
    config: ClaudeCodeConfig,
}

impl ClaudeCodeProvider {
    /// Construct a Claude Code provider pinned to one account / session.
    #[must_use]
    pub fn new(config: ClaudeCodeConfig) -> Self {
        Self { config }
    }

    /// Visible for testing — the construction-time config.
    #[must_use]
    pub fn config(&self) -> &ClaudeCodeConfig {
        &self.config
    }
}

#[async_trait]
impl Provider for ClaudeCodeProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::ClaudeCode
    }

    #[instrument(
        skip(self, mandate),
        fields(
            mandate_id = ?mandate.id,
            session = %self.config.tmux_session,
        )
    )]
    async fn dispatch(&self, mandate: &Mandate) -> Result<Box<dyn WorkerHandle>> {
        if !tmux::session_exists(SCOPE, &self.config.tmux_session).await? {
            return Err(Error::Provider {
                kind: ProviderKind::ClaudeCode,
                reason: format!(
                    "tmux session {} does not exist (Phase 1 requires the operator or smoke-test to spawn the session before dispatch)",
                    self.config.tmux_session
                ),
            });
        }

        let worker_id = WorkerId::new();
        let window_name = window_name_for(worker_id);
        let directive = compose_directive(mandate, self.config.policy_mode);

        info!(
            worker = ?worker_id,
            window = %window_name,
            "spawning Claude Code worker"
        );

        tmux::new_window(
            SCOPE,
            &self.config.tmux_session,
            &window_name,
            &self.config.working_dir,
        )
        .await?;

        // `command claude` bypasses any shell function shadow (matches
        // v3 teams.sh). `CLAUDE_CONFIG_DIR` is passed inline so that even
        // if the operator's session has a different one set in env, this
        // worker stays pinned to the configured account.
        let config_dir = path_to_arg(&self.config.claude_config_dir)?;
        let launch_line = format!("CLAUDE_CONFIG_DIR={config_dir} command claude");

        debug!(launch = %launch_line, "launching claude CLI in worker window");
        tmux::send_keys(
            SCOPE,
            &self.config.tmux_session,
            &window_name,
            &[launch_line.as_str()],
        )
        .await?;
        tmux::press_enter(SCOPE, &self.config.tmux_session, &window_name).await?;

        // Wait for Claude Code's TUI to render before we paste the
        // mandate. See `CLAUDE_BOOT_SLEEP`'s rustdoc for why this is a
        // fixed sleep in Phase 1.
        sleep(CLAUDE_BOOT_SLEEP).await;

        let buffer_name = buffer_name_for(worker_id);
        tmux::paste_text(
            SCOPE,
            &self.config.tmux_session,
            &window_name,
            &buffer_name,
            &directive,
        )
        .await?;
        tmux::press_enter(SCOPE, &self.config.tmux_session, &window_name).await?;

        let handle = ClaudeCodeWorker {
            inner: Arc::new(WorkerInner {
                worker_id,
                mandate_id: mandate.id,
                tmux_session: self.config.tmux_session.clone(),
                window_name,
                timeout: mandate.timeout,
                cancel_requested: AtomicBool::new(false),
            }),
        };
        Ok(Box::new(handle))
    }

    async fn healthcheck(&self) -> Result<()> {
        // Phase 1 healthcheck is the session-existence probe — cheap
        // and accurate enough for the bootstrap. TODO(phase-2): also
        // exec `claude --version` to confirm the CLI is on PATH and
        // matches the operator's expected version.
        if !tmux::session_exists(SCOPE, &self.config.tmux_session).await? {
            return Err(Error::Provider {
                kind: ProviderKind::ClaudeCode,
                reason: format!("tmux session {} does not exist", self.config.tmux_session),
            });
        }
        Ok(())
    }
}

/// Handle returned by [`ClaudeCodeProvider::dispatch`].
///
/// Holds the tmux session + window identifiers and an
/// `AtomicBool` recording whether the operator called `cancel()`. The
/// flag lets `status()` disambiguate between "window gone because the
/// worker finished" (→ `Succeeded`) and "window gone because we killed
/// it" (→ `Cancelled`). v3 doesn't make this distinction at all; Phase
/// 1 is an improvement.
#[derive(Clone)]
pub struct ClaudeCodeWorker {
    inner: Arc<WorkerInner>,
}

struct WorkerInner {
    worker_id: WorkerId,
    mandate_id: MandateId,
    tmux_session: String,
    window_name: String,
    timeout: Option<Duration>,
    cancel_requested: AtomicBool,
}

#[async_trait]
impl WorkerHandle for ClaudeCodeWorker {
    fn id(&self) -> WorkerId {
        self.inner.worker_id
    }

    fn mandate_id(&self) -> MandateId {
        self.inner.mandate_id
    }

    async fn status(&self) -> Result<WorkerStatus> {
        let alive =
            tmux::window_exists(SCOPE, &self.inner.tmux_session, &self.inner.window_name).await?;
        if alive {
            return Ok(WorkerStatus::Running);
        }
        // Window is gone. Was it our doing?
        if self.inner.cancel_requested.load(Ordering::SeqCst) {
            Ok(WorkerStatus::Cancelled)
        } else {
            // Phase 1 limitation: tmux gives us no exit-code signal for
            // a pane that closed on its own. Treat any external close
            // as success — the v3 bash equivalent doesn't try harder.
            // TODO(phase-2): tail the worker's inbox / scrollback for a
            // terminal-status phrase and surface Failed(...) accordingly.
            Ok(WorkerStatus::Succeeded)
        }
    }

    async fn cancel(&self) -> Result<()> {
        self.inner.cancel_requested.store(true, Ordering::SeqCst);
        tmux::kill_window(SCOPE, &self.inner.tmux_session, &self.inner.window_name).await
    }

    async fn wait(&self) -> Result<WorkerStatus> {
        let deadline = Instant::now() + self.inner.timeout.unwrap_or(DEFAULT_WAIT_TIMEOUT);
        loop {
            let status = self.status().await?;
            if matches!(
                status,
                WorkerStatus::Succeeded | WorkerStatus::Failed(_) | WorkerStatus::Cancelled
            ) {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(Error::WorkerFailed(format!(
                    "wait() exceeded timeout for worker {:?}",
                    self.inner.worker_id
                )));
            }
            sleep(WAIT_POLL_INTERVAL).await;
        }
    }
}

/// Compose the directive text the operator's mandate becomes when it
/// lands in the Claude Code pane.
///
/// **Phase 1 format** — plain text, human-readable, no HMAC.
/// **Phase 2 TODO** — wrap in the v3 `[subctl-master directive · phase=…
/// · ts:… · hmac:…]` envelope per [ADR 0011] once `evy-comms` provides
/// the signer. The directive text itself becomes the `SPEC:` body.
///
/// Pure function — golden-tested against fixed-id mandates.
#[must_use]
pub fn compose_directive(mandate: &Mandate, policy_mode: evy_core::PolicyMode) -> String {
    let mut out = String::with_capacity(512 + mandate.context.len());
    out.push_str("# Subctl Mandate\n\n");
    out.push_str(&format!("Mandate-Id: {:?}\n", mandate.id));
    out.push_str(&format!("Provider:   {:?}\n", mandate.provider));
    out.push_str(&format!("Policy:     {policy_mode:?}\n"));
    if let Some(t) = mandate.timeout {
        out.push_str(&format!("Timeout:    {}s\n", t.as_secs()));
    }
    out.push_str("\n## Goal\n\n");
    out.push_str(mandate.goal.trim());
    out.push_str("\n\n## Context\n\n");
    out.push_str(mandate.context.trim());
    out.push_str("\n\n## Deliverable\n\n");
    out.push_str(mandate.deliverable.trim());

    if !mandate.done_when.is_empty() {
        out.push_str("\n\n## Done When\n\n");
        for item in &mandate.done_when {
            out.push_str(&format!("- {}\n", item.trim()));
        }
    }
    if !mandate.constraints.is_empty() {
        out.push_str("\n## Constraints\n\n");
        for item in &mandate.constraints {
            out.push_str(&format!("- {}\n", item.trim()));
        }
    }
    if !mandate.metadata.is_empty() {
        out.push_str("\n## Metadata\n\n");
        // Sort for deterministic golden tests.
        let mut entries: Vec<(&String, &String)> = mandate.metadata.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in entries {
            out.push_str(&format!("- `{k}` = `{v}`\n"));
        }
    }
    out.push('\n');
    out
}

/// `worker-<8 hex of uuid>` — short enough to read in tmux's window
/// list, long enough to stay unique within a session.
fn window_name_for(worker_id: WorkerId) -> String {
    let s = worker_id.0.simple().to_string();
    format!("worker-{}", &s[..8])
}

/// tmux buffer name. Same uniqueness shape as the window name; the
/// buffer is named per-worker so concurrent dispatches don't clobber
/// each other's pasted directives.
fn buffer_name_for(worker_id: WorkerId) -> String {
    let s = worker_id.0.simple().to_string();
    format!("subctl-claude-{}", &s[..8])
}

/// Convert a path to a `String` argv value, surfacing a typed error
/// rather than panicking on non-UTF8 paths.
fn path_to_arg(p: &Path) -> Result<String> {
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| Error::Provider {
            kind: ProviderKind::ClaudeCode,
            reason: format!("path is not valid UTF-8: {}", p.display()),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn fixture_mandate() -> Mandate {
        // Fixed UUID for deterministic golden-test output.
        let id =
            MandateId(Uuid::parse_str("12345678-1234-5678-1234-567812345678").expect("valid uuid"));
        let mut metadata = HashMap::new();
        metadata.insert("model".to_string(), "claude-opus-4.7".to_string());
        metadata.insert("account".to_string(), "claude-jason".to_string());
        Mandate {
            id,
            provider: ProviderKind::ClaudeCode,
            goal: "ship Slice D".to_string(),
            context: "Phase 1 bootstrap — three workers in parallel.".to_string(),
            deliverable: "evy-providers crate with Claude/Codex/DeepSeek adapters".to_string(),
            done_when: vec![
                "cargo test passes".to_string(),
                "cargo clippy clean".to_string(),
            ],
            constraints: vec!["may only touch crates/evy-providers/**".to_string()],
            policy_mode: evy_core::PolicyMode::Gated,
            timeout: Some(Duration::from_secs(900)),
            metadata,
        }
    }

    #[test]
    fn directive_contains_all_mandate_sections() {
        let m = fixture_mandate();
        let d = compose_directive(&m, m.policy_mode);
        assert!(d.contains("## Goal"));
        assert!(d.contains("ship Slice D"));
        assert!(d.contains("## Context"));
        assert!(d.contains("Phase 1 bootstrap"));
        assert!(d.contains("## Deliverable"));
        assert!(d.contains("evy-providers crate"));
        assert!(d.contains("## Done When"));
        assert!(d.contains("- cargo test passes"));
        assert!(d.contains("## Constraints"));
        assert!(d.contains("crates/evy-providers/**"));
        assert!(d.contains("Policy:     Gated"));
        assert!(d.contains("Timeout:    900s"));
        // Metadata is sorted alphabetically for determinism.
        let acct_pos = d.find("`account`").expect("account key present");
        let model_pos = d.find("`model`").expect("model key present");
        assert!(acct_pos < model_pos, "metadata must be sorted");
    }

    #[test]
    fn directive_golden() {
        // Stable snapshot of the Phase-1 directive shape. If this fails,
        // a v4 caller (binary-wirer, dashboard preview) may have baked
        // assumptions about the format — coordinate before changing.
        let m = fixture_mandate();
        let d = compose_directive(&m, m.policy_mode);
        let expected = "\
# Subctl Mandate

Mandate-Id: MandateId(12345678-1234-5678-1234-567812345678)
Provider:   ClaudeCode
Policy:     Gated
Timeout:    900s

## Goal

ship Slice D

## Context

Phase 1 bootstrap — three workers in parallel.

## Deliverable

evy-providers crate with Claude/Codex/DeepSeek adapters

## Done When

- cargo test passes
- cargo clippy clean

## Constraints

- may only touch crates/evy-providers/**

## Metadata

- `account` = `claude-jason`
- `model` = `claude-opus-4.7`

";
        assert_eq!(d, expected);
    }

    #[test]
    fn directive_omits_optional_blocks_when_empty() {
        let mut m = fixture_mandate();
        m.timeout = None;
        m.done_when.clear();
        m.constraints.clear();
        m.metadata.clear();
        let d = compose_directive(&m, m.policy_mode);
        assert!(!d.contains("Timeout:"));
        assert!(!d.contains("## Done When"));
        assert!(!d.contains("## Constraints"));
        assert!(!d.contains("## Metadata"));
    }

    #[test]
    fn worker_name_is_stable_for_same_id() {
        let id = WorkerId(Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap());
        assert_eq!(window_name_for(id), "worker-aaaaaaaa");
        assert_eq!(buffer_name_for(id), "subctl-claude-aaaaaaaa");
    }
}
