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
//! W6.5 ① ported the v3 readiness flow natively: poll for the
//! `Context … % left` status line, dismiss the "Update available" modal
//! (`2` + Enter) and the directory-trust dialog (`1` + Enter) before
//! pasting, beat between paste and Enter, and post-paste delivery check
//! so a swallowed mandate is loud instead of silent (closet 2026-06-09:
//! the trust dialog ate the paste despite the `-c trust_level` flag).

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

/// Short settle after the TUI reports ready and before the directive
/// paste. The primary wait is [`wait_for_codex_ready`] (poll); this just
/// covers the gap between the status line rendering and the composer
/// accepting input.
const CODEX_BOOT_SLEEP: Duration = Duration::from_secs(2);

/// Beat between the directive paste and the submitting Enter — same
/// rationale as the Claude adapter's `PASTE_TO_ENTER_BEAT` (the closet
/// entry for the 2026-06-09 codex mandate loss explicitly calls for it).
const PASTE_TO_ENTER_BEAT: Duration = Duration::from_millis(500);

/// Upper bound on deliberate trust-dialog dismissals during one
/// ready-wait (same rationale as the Claude adapter).
const MAX_DIALOG_DISMISSALS: u32 = 3;

/// Codex's first-run directory-trust dialog ("Do you trust the contents
/// of this directory?"). Renders despite the `-c projects.….trust_level`
/// launch flag in some codex versions / cwd states — observed live
/// 2026-06-09 in `/tmp`, where the pasted mandate was lost to it.
const CODEX_TRUST_MARKER: &str = "Do you trust";

/// The "Update available" modal copy, verified against codex 0.130.0 by
/// the v3 launcher (providers/openai-codex/teams.sh). Dismissed with
/// `2` (Skip) + Enter. Both substrings are required to key the dismissal
/// so a casual mention of updates in output can't trigger it.
const CODEX_UPDATE_MARKERS: [&str; 2] = ["Update available", "Press enter"];

/// Ready signal: Codex's booted TUI renders `Context 100% left` in the
/// bottom status line (verified empirically against codex 0.130.0 — same
/// marker the v3 launcher polls for).
const CODEX_READY_MARKER: &str = "% left";

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

    /// Cutover Phase 2 — create the pinned tmux session if absent, injecting
    /// `CODEX_HOME` + role/spawn-ts markers. Idempotent. Mirrors
    /// [`ClaudeCodeProvider::ensure_session`](crate::ClaudeCodeProvider::ensure_session);
    /// the daemon's spawn path calls this before [`dispatch`](Provider::dispatch).
    ///
    /// # Errors
    /// [`Error::Provider`] if `tmux new-session` fails or `codex_home` isn't UTF-8.
    pub async fn ensure_session(&self) -> Result<()> {
        if tmux::session_exists(SCOPE, &self.config.tmux_session).await? {
            return Ok(());
        }
        let home = self
            .config
            .codex_home
            .to_str()
            .ok_or_else(|| Error::Provider {
                kind: ProviderKind::Codex,
                reason: format!(
                    "codex_home is not valid UTF-8: {}",
                    self.config.codex_home.display()
                ),
            })?;
        let spawn_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_string());
        tmux::new_session(
            SCOPE,
            &self.config.tmux_session,
            &self.config.working_dir,
            &[
                ("CODEX_HOME", home),
                ("SUBCTL_AGENT_ROLE", "worker"),
                ("SUBCTL_SPAWN_TS", &spawn_ts),
            ],
            220,
            50,
        )
        .await
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

        // W6.5 ① — provision the verify key so the worker can authenticate
        // the envelope with `subctl directive verify`. Best-effort: failure
        // degrades the worker to --key-file, never loses the spawn.
        if let Err(err) = crate::claude_code::provision_directive_key(&self.config.codex_home, key)
        {
            tracing::warn!(
                %err,
                "failed to provision .subctl-directive-key in CODEX_HOME; worker-side `subctl directive verify` will need --key-file"
            );
        }

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

        // W6.5 ① — wait for the Codex composer (status line) instead of a
        // blind fixed sleep, dismissing the update modal / trust dialog
        // that previously swallowed the paste. A short settle follows.
        if !wait_for_codex_ready(&self.config.tmux_session, &window_name).await {
            tracing::warn!(window = %window_name, "codex ready status line not seen in time; pasting anyway");
        }
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
        info!(
            worker = ?worker_id,
            window = %window_name,
            directive_bytes = directive.len(),
            "directive paste complete; checking delivery before submit"
        );

        // Beat + post-paste delivery check — same provable-delivery
        // sequence as the Claude adapter (see its dispatch for rationale).
        sleep(PASTE_TO_ENTER_BEAT).await;
        let paste_target = format!("{}:{}", self.config.tmux_session, window_name);
        match tmux::tmux_capture(&paste_target, 80).await {
            Some(pane) if crate::claude_code::directive_visible(&pane) => {
                info!(window = %window_name, "directive visible in composer; submitting");
            }
            Some(pane) => {
                let tail: Vec<&str> = pane.lines().rev().take(5).collect();
                let tail = tail.into_iter().rev().collect::<Vec<_>>().join(" | ");
                tracing::warn!(
                    window = %window_name,
                    pane_tail = %tail,
                    "POST-PASTE CHECK FAILED — directive not visible in pane; the mandate may have been swallowed (trust/update modal race?). Submitting anyway; inspect this worker"
                );
            }
            None => {
                tracing::warn!(
                    window = %window_name,
                    "post-paste pane capture failed; cannot prove directive delivery"
                );
            }
        }
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

/// What a captured Codex pane is currently showing. Same shape as the
/// Claude adapter's `PaneState`, with the extra update-modal arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexPaneState {
    /// "Update available" modal — dismissed with `2` (Skip) + Enter.
    UpdateModal,
    /// Directory-trust dialog — pasting now loses the directive.
    TrustDialog,
    /// Booted: the `Context … % left` status line is rendered.
    Ready,
    /// Still booting.
    Booting,
}

/// Classify a captured pane. Modal/dialog detection takes precedence over
/// ready detection — an interstitial must never classify as ready. Pure;
/// unit-tested below.
fn classify_codex_pane(pane: &str) -> CodexPaneState {
    if CODEX_UPDATE_MARKERS.iter().all(|m| pane.contains(m)) {
        return CodexPaneState::UpdateModal;
    }
    if pane.contains(CODEX_TRUST_MARKER) {
        return CodexPaneState::TrustDialog;
    }
    if pane.contains(CODEX_READY_MARKER) {
        return CodexPaneState::Ready;
    }
    CodexPaneState::Booting
}

/// W6.5 ① — poll the worker pane until Codex's TUI is ready for the
/// directive paste. Ports the v3 launcher's empirically-validated flow
/// (providers/openai-codex/teams.sh):
///
/// - "Update available" modal → `2` (Skip) + Enter, once.
/// - Directory-trust dialog (seen live 2026-06-09 despite the
///   `-c trust_level` launch flag) → `1` (Yes) + Enter, at most
///   [`MAX_DIALOG_DISMISSALS`] times.
/// - Ready = the `Context … % left` status line.
///
/// Polls every 500ms up to ~40s; returns whether ready was observed
/// (caller pastes regardless — the post-paste check downstream makes any
/// loss loud).
async fn wait_for_codex_ready(session: &str, window: &str) -> bool {
    let target = format!("{session}:{window}");
    let mut update_dismissed = false;
    let mut trust_dismissals = 0u32;
    for _ in 0..80 {
        if let Some(pane) = tmux::tmux_capture(&target, 80).await {
            match classify_codex_pane(&pane) {
                CodexPaneState::Ready => return true,
                CodexPaneState::UpdateModal if !update_dismissed => {
                    update_dismissed = true;
                    tracing::warn!(%target, "codex update modal rendered; dismissing with Skip");
                    let _ = tmux::send_keys(SCOPE, session, window, &["2"]).await;
                    sleep(Duration::from_millis(200)).await;
                    let _ = tmux::press_enter(SCOPE, session, window).await;
                    sleep(Duration::from_millis(800)).await;
                    continue;
                }
                CodexPaneState::TrustDialog if trust_dismissals < MAX_DIALOG_DISMISSALS => {
                    trust_dismissals += 1;
                    tracing::warn!(
                        %target,
                        attempt = trust_dismissals,
                        "codex trust dialog rendered despite trust_level flag; dismissing deliberately"
                    );
                    let _ = tmux::send_keys(SCOPE, session, window, &["1"]).await;
                    sleep(Duration::from_millis(200)).await;
                    let _ = tmux::press_enter(SCOPE, session, window).await;
                    sleep(Duration::from_millis(800)).await;
                    continue;
                }
                CodexPaneState::UpdateModal
                | CodexPaneState::TrustDialog
                | CodexPaneState::Booting => {}
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    false
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
    let bin = cfg.codex_bin.to_str().ok_or_else(|| Error::Provider {
        kind: ProviderKind::Codex,
        reason: format!("codex_bin is not valid UTF-8: {}", cfg.codex_bin.display()),
    })?;
    let trust_arg = format!("projects.\"{cwd}\".trust_level=\"trusted\"");
    // Absolute bin (not `command codex`) — launchd PATH lacks Homebrew.
    let mut cmd = format!("CODEX_HOME={home} {bin} -c '{trust_arg}'");
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
            codex_bin: PathBuf::from("/opt/homebrew/bin/codex"),
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
        // Absolute bin, NOT `command codex` (launchd PATH split-brain dodge).
        assert!(cmd.contains("/opt/homebrew/bin/codex"));
        assert!(!cmd.contains("command codex"));
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

    // ── W6.5 ① — readiness classifier ────────────────────────────────────

    #[test]
    fn trust_dialog_is_not_ready() {
        // Regression fixture for the 2026-06-09 live mandate loss: codex
        // booted to the trust dialog in /tmp and the paste was swallowed.
        let pane = "\
❯ CODEX_HOME=/Users/op/.codex-jason /opt/homebrew/bin/codex

  Do you trust the contents of this directory?

  /tmp/codex-test

  ❯ 1. Yes, continue
    2. No, quit
";
        assert_eq!(classify_codex_pane(pane), CodexPaneState::TrustDialog);
    }

    #[test]
    fn update_modal_requires_both_markers_and_wins_over_trust() {
        let modal = "Update available! 0.130.0 → 0.131.0\nPress enter to update\n  2. Skip";
        assert_eq!(classify_codex_pane(modal), CodexPaneState::UpdateModal);
        // A stray "Update available" in scrollback without the prompt copy
        // must NOT key the dismissal.
        assert_eq!(
            classify_codex_pane("changelog: Update available banner removed"),
            CodexPaneState::Booting
        );
        // Both interstitials on screen → dismiss the modal first (v3 order).
        let both = format!("{modal}\nDo you trust the contents of this directory?");
        assert_eq!(classify_codex_pane(&both), CodexPaneState::UpdateModal);
    }

    #[test]
    fn ready_status_line_classifies_ready() {
        let pane = "╭─╮\n│ ▌ │\n╰─╯\n  Context 100% left · 0 in · 0 out";
        assert_eq!(classify_codex_pane(pane), CodexPaneState::Ready);
    }

    #[test]
    fn ready_marker_does_not_override_trust_dialog() {
        // If the status line and the dialog somehow coexist (resize race),
        // the dialog must win — pasting into it loses the directive.
        let pane = "Context 100% left\nDo you trust the contents of this directory?";
        assert_eq!(classify_codex_pane(pane), CodexPaneState::TrustDialog);
    }

    #[test]
    fn booting_banner_is_not_ready() {
        assert_eq!(
            classify_codex_pane("❯ CODEX_HOME=/x /y/codex\n  loading model…"),
            CodexPaneState::Booting
        );
    }
}
