//! OpenAI Codex adapter — spawns a tmux window running the `codex`
//! CLI and pastes the mandate in as a directive.
//!
//! Ports the v3 `providers/openai-codex/teams.sh` flow into the v4
//! trait surface. Differences from the Claude adapter:
//!
//! - Per-account isolation via `CODEX_HOME` instead of
//!   `CLAUDE_CONFIG_DIR`.
//! - The `command codex` launch line carries the trust-level override
//!   (`-c projects."<cwd>".trust_level="trusted"`) so Codex's
//!   first-run "Do you trust this directory?" modal is bypassed.
//! - The directive includes the v3 "reporting vocabulary" block —
//!   Claude workers pick up the staleness watchdog's classifier
//!   phrases emergently from their template prompts, Codex doesn't.
//!
//! HMAC trust marker is omitted in Phase 1 (same TODO as the Claude
//! adapter — slot for Phase 2 once `evy-comms` provides the signer).
//! The Codex update-modal dismissal + `Context % left` ready-poll
//! from v3 are also Phase-2 work; Phase 1 uses a fixed 2s sleep.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use evy_core::{
    Error, Mandate, MandateId, Provider, ProviderKind, Result, WorkerHandle, WorkerId, WorkerStatus,
};
use tokio::time::sleep;
use tracing::{debug, info, instrument};

use crate::config::CodexConfig;
use crate::hmac::{default_key, HmacKey, TrustMarker};
use crate::tmux::{self, TmuxScope};

const SCOPE: TmuxScope = TmuxScope(ProviderKind::Codex);

/// Phase-1 boot delay after `codex` launch before the directive is
/// pasted. TODO(phase-2): replace with capture-pane polling for
/// `Context % left` and dismissal of the "Update available" modal.
const CODEX_BOOT_SLEEP: Duration = Duration::from_secs(2);

const WAIT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// `Provider` impl for OpenAI Codex via tmux + the `codex` CLI.
pub struct CodexProvider {
    config: CodexConfig,
}

impl CodexProvider {
    /// Construct a Codex provider pinned to one account / session.
    #[must_use]
    pub fn new(config: CodexConfig) -> Self {
        Self { config }
    }

    /// Visible for testing — the construction-time config.
    #[must_use]
    pub fn config(&self) -> &CodexConfig {
        &self.config
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
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
                kind: ProviderKind::Codex,
                reason: format!(
                    "tmux session {} does not exist (Phase 1 requires the operator or smoke-test to spawn the session before dispatch)",
                    self.config.tmux_session
                ),
            });
        }

        let worker_id = WorkerId::new();
        let window_name = window_name_for(worker_id);
        // Same wrap pattern as Claude: compose the body, then envelope
        // it in the ADR-0011 HMAC trust marker. v3 workers verify the
        // marker via in-prompt model reasoning; native verification is
        // future work (see hmac::TrustMarker::verify).
        let body = compose_directive(mandate, self.config.policy_mode);
        let key = resolve_key(self.config.hmac_key.as_ref());
        let directive = TrustMarker::new(body, Some(phase_for(mandate)), key).to_directive_string();

        info!(
            worker = ?worker_id,
            window = %window_name,
            "spawning Codex worker (HMAC-wrapped directive)"
        );

        tmux::new_window(
            SCOPE,
            &self.config.tmux_session,
            &window_name,
            &self.config.working_dir,
        )
        .await?;

        let launch_line = launch_command(&self.config)?;
        debug!(launch = %launch_line, "launching codex CLI in worker window");
        tmux::send_keys(
            SCOPE,
            &self.config.tmux_session,
            &window_name,
            &[launch_line.as_str()],
        )
        .await?;
        tmux::press_enter(SCOPE, &self.config.tmux_session, &window_name).await?;

        sleep(CODEX_BOOT_SLEEP).await;

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

        let handle = CodexWorker {
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
        if !tmux::session_exists(SCOPE, &self.config.tmux_session).await? {
            return Err(Error::Provider {
                kind: ProviderKind::Codex,
                reason: format!("tmux session {} does not exist", self.config.tmux_session),
            });
        }
        Ok(())
    }
}

/// Handle returned by [`CodexProvider::dispatch`]. Same shape as
/// `ClaudeCodeWorker`: tmux window + cancel-flag for status
/// disambiguation. See that type for the rationale.
#[derive(Clone)]
pub struct CodexWorker {
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
impl WorkerHandle for CodexWorker {
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
        if self.inner.cancel_requested.load(Ordering::SeqCst) {
            Ok(WorkerStatus::Cancelled)
        } else {
            // Same Phase-1 limitation as Claude: no exit-code signal
            // from tmux, so any non-cancel close is Succeeded.
            // TODO(phase-2): tail inbox / pane scrollback for terminal
            // phrases (DONE / BLOCKED) and surface Failed accordingly.
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

/// Compose the Codex launch line. Mirrors v3 teams.sh: `CODEX_HOME` set
/// inline, trust-level override embedded so the first-run "trust this
/// directory?" modal is bypassed. Model override appended when present.
fn launch_command(cfg: &CodexConfig) -> Result<String> {
    let home = cfg.codex_home.to_str().ok_or_else(|| Error::Provider {
        kind: ProviderKind::Codex,
        reason: format!(
            "codex_home is not valid UTF-8: {}",
            cfg.codex_home.display()
        ),
    })?;
    let cwd = cfg.working_dir.to_str().ok_or_else(|| Error::Provider {
        kind: ProviderKind::Codex,
        reason: format!(
            "working_dir is not valid UTF-8: {}",
            cfg.working_dir.display()
        ),
    })?;
    let trust_arg = format!("projects.\"{cwd}\".trust_level=\"trusted\"");
    let mut cmd = format!("CODEX_HOME={home} command codex -c '{trust_arg}'");
    if let Some(model) = &cfg.model {
        cmd.push_str(&format!(" -c model=\"{model}\""));
    }
    Ok(cmd)
}

/// Compose the directive text for a Codex worker.
///
/// Same Phase-1 mandate shape as Claude, plus a "reporting vocabulary"
/// trailer ported from v3 `providers/openai-codex/teams.sh`. Pure;
/// golden-tested.
///
/// TODO(phase-2): wrap in the v3 HMAC `[subctl-master directive · …]`
/// envelope once `evy-comms` provides the signer.
#[must_use]
pub fn compose_directive(mandate: &Mandate, policy_mode: evy_core::PolicyMode) -> String {
    let mut out = crate::claude_code::compose_directive(mandate, policy_mode);
    // Trim a trailing newline so the appended trailer renders cleanly.
    while out.ends_with('\n') {
        out.pop();
    }
    out.push_str(
        "\n\n## Reporting Vocabulary (required for staleness classifier)\n\n\
End your turn with one of these phrases so the master daemon's\n\
staleness classifier can route correctly:\n\
- DONE     → \"task complete, idle by design — awaiting next directive.\"\n\
- BLOCKED  → \"blocked on <reason>\"\n\
- AWAITING → \"awaiting your input on <question>\"\n\
- WORKING  → no phrase needed; keep working.\n",
    );
    out
}

fn window_name_for(worker_id: WorkerId) -> String {
    let s = worker_id.0.simple().to_string();
    format!("codex-{}", &s[..8])
}

fn buffer_name_for(worker_id: WorkerId) -> String {
    let s = worker_id.0.simple().to_string();
    format!("subctl-codex-{}", &s[..8])
}

/// Phase string baked into the HMAC trust marker — same convention as
/// the Claude adapter (`metadata["phase"]` if set, else `"dispatch"`).
fn phase_for(mandate: &Mandate) -> String {
    mandate
        .metadata
        .get("phase")
        .cloned()
        .unwrap_or_else(|| "dispatch".to_string())
}

/// Resolve HMAC key — caller-supplied wins, else the process-global
/// default. Same pattern as `claude_code::resolve_key`; deduped only by
/// convention since the two adapters stay symmetrically simple.
fn resolve_key(configured: Option<&HmacKey>) -> &HmacKey {
    configured.unwrap_or_else(|| default_key())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    fn fixture_mandate() -> Mandate {
        let id =
            MandateId(Uuid::parse_str("12345678-1234-5678-1234-567812345678").expect("valid uuid"));
        let mut metadata = HashMap::new();
        metadata.insert("account".to_string(), "openai-jason".to_string());
        Mandate {
            id,
            provider: ProviderKind::Codex,
            goal: "smoke-test the Codex adapter".to_string(),
            context: "Phase 1 Slice D dispatch test.".to_string(),
            deliverable: "a codex worker that prints hello and exits".to_string(),
            done_when: vec!["pane shows 'hello'".to_string()],
            constraints: vec![],
            policy_mode: evy_core::PolicyMode::Trusted,
            timeout: None,
            metadata,
        }
    }

    fn fixture_config() -> CodexConfig {
        CodexConfig {
            codex_home: PathBuf::from("/Users/sem/.codex-jason"),
            tmux_session: "codex-test".to_string(),
            working_dir: PathBuf::from("/tmp/codex-test"),
            model: Some("gpt-5.5".to_string()),
            policy_mode: evy_core::PolicyMode::Trusted,
            hmac_key: None,
        }
    }

    #[test]
    fn directive_includes_reporting_vocabulary() {
        let m = fixture_mandate();
        let d = compose_directive(&m, m.policy_mode);
        assert!(d.contains("## Goal"));
        assert!(d.contains("smoke-test the Codex adapter"));
        assert!(d.contains("## Reporting Vocabulary"));
        assert!(d.contains("DONE"));
        assert!(d.contains("BLOCKED"));
        assert!(d.contains("AWAITING"));
    }

    #[test]
    fn launch_command_carries_codex_home_and_trust_level() {
        let cfg = fixture_config();
        let cmd = launch_command(&cfg).expect("valid utf-8 paths");
        assert!(cmd.starts_with("CODEX_HOME=/Users/sem/.codex-jason"));
        assert!(cmd.contains("command codex"));
        assert!(cmd.contains("projects.\"/tmp/codex-test\".trust_level=\"trusted\""));
        assert!(cmd.contains("model=\"gpt-5.5\""));
    }

    #[test]
    fn launch_command_omits_model_when_unset() {
        let mut cfg = fixture_config();
        cfg.model = None;
        let cmd = launch_command(&cfg).expect("valid utf-8 paths");
        assert!(!cmd.contains("model="));
    }

    #[test]
    fn worker_name_is_stable_for_same_id() {
        let id = WorkerId(Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap());
        assert_eq!(window_name_for(id), "codex-aaaaaaaa");
        assert_eq!(buffer_name_for(id), "subctl-codex-aaaaaaaa");
    }

    #[test]
    fn provider_exposes_kind() {
        let cfg = fixture_config();
        let p = CodexProvider::new(cfg);
        assert_eq!(p.kind(), ProviderKind::Codex);
        // `working_dir` is reachable through the visible-for-testing
        // accessor — useful when binary-wirer wants to log the spawn.
        assert_eq!(p.config().working_dir, Path::new("/tmp/codex-test"));
    }
}
