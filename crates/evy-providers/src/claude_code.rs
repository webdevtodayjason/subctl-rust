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
use crate::hmac::{default_key, HmacKey, TrustMarker};
use crate::tmux::{self, TmuxScope};

const SCOPE: TmuxScope = TmuxScope(ProviderKind::ClaudeCode);

/// Short settle after the TUI reports ready (2h) and before the directive
/// paste — covers the gap between the input box rendering and accepting input.
/// The primary wait is now [`wait_for_claude_ready`] (poll), not this sleep.
const CLAUDE_BOOT_SLEEP: Duration = Duration::from_secs(2);

/// Beat between the directive paste and the submitting Enter. Pasting and
/// submitting in the same breath raced the TUI's paste ingestion — the
/// Enter could land while the composer was still consuming the buffer
/// (W6.5 mandate-loss fix, closet entry 2026-06-11).
const PASTE_TO_ENTER_BEAT: Duration = Duration::from_millis(500);

/// Upper bound on deliberate trust-dialog dismissals during one
/// ready-wait. A dialog that survives this many Enter presses is wedged;
/// keep polling instead of key-spamming an unknown screen.
const MAX_DIALOG_DISMISSALS: u32 = 3;

/// Substrings that identify Claude Code's directory-trust dialog. The
/// dialog renders a `❯` selector — which is exactly why a bare-`❯` ready
/// heuristic mistook it for the composer (see [`PaneState::TrustDialog`]).
const CLAUDE_DIALOG_MARKERS: [&str; 2] = ["Do you trust", "Yes, proceed"];

/// Substrings that only render once the composer is actually accepting
/// input: the `Try "` empty-composer placeholder, the `? for shortcuts`
/// hint under the input box, and the `⏵` mode chevron in the status line.
/// Deliberately NO bare `❯` count — the trust dialog satisfies that.
const CLAUDE_READY_MARKERS: [&str; 3] = ["Try \"", "? for shortcuts", "⏵"];

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

    /// Cutover Phase 2 slice 2e — create the pinned tmux session if it doesn't
    /// already exist, injecting `CLAUDE_CONFIG_DIR` + the team/role/spawn-ts
    /// markers (ports v3 `teams.sh:576-581`). Idempotent. The daemon's spawn
    /// path calls this before [`dispatch`](Provider::dispatch), which requires
    /// the session to exist.
    ///
    /// # Errors
    /// Returns [`Error::Provider`] if the `tmux new-session` call fails or the
    /// config dir isn't valid UTF-8.
    pub async fn ensure_session(&self) -> Result<()> {
        if tmux::session_exists(SCOPE, &self.config.tmux_session).await? {
            return Ok(());
        }
        let cfg_dir = self
            .config
            .claude_config_dir
            .to_str()
            .ok_or_else(|| Error::Provider {
                kind: ProviderKind::ClaudeCode,
                reason: format!(
                    "claude_config_dir is not valid UTF-8: {}",
                    self.config.claude_config_dir.display()
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
                ("CLAUDE_CONFIG_DIR", cfg_dir),
                ("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS", "1"),
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
        // Compose the body (Phase 1 markdown shape), then wrap in the
        // ADR-0011 HMAC trust marker. The marker is what actually gets
        // pasted; `compose_directive` remains the canonical body builder
        // so its golden tests still apply to the inner payload.
        let body = compose_directive(mandate, self.config.policy_mode);
        let key = resolve_key(self.config.hmac_key.as_ref());
        let directive = TrustMarker::new(body, Some(phase_for(mandate)), key).to_directive_string();

        info!(
            worker = ?worker_id,
            window = %window_name,
            "spawning Claude Code worker (HMAC-wrapped directive)"
        );

        // W6.5 ① — provable mandate delivery, part 1 (race-free prevention):
        // (a) pre-trust the working dir in the account's .claude.json so the
        //     directory-trust dialog never renders for this spawn (the dialog
        //     is what silently swallowed the paste in the 2026-06-11 live
        //     reproductions);
        // (b) drop the session HMAC key into the worker's config dir so it
        //     can authenticate envelopes via `subctl directive verify`.
        // Both are best-effort: a failure degrades to the in-loop dialog
        // dismissal / in-prompt trust contract, never to a lost spawn.
        if let Err(err) =
            pre_trust_project_dir(&self.config.claude_config_dir, &self.config.working_dir)
        {
            tracing::warn!(
                %err,
                "failed to pre-trust working dir in .claude.json; relying on dialog dismissal"
            );
        }
        if let Err(err) = provision_directive_key(&self.config.claude_config_dir, key) {
            tracing::warn!(
                %err,
                "failed to provision .subctl-directive-key; worker-side `subctl directive verify` will need --key-file"
            );
        }

        tmux::new_window(
            SCOPE,
            &self.config.tmux_session,
            &window_name,
            &self.config.working_dir,
        )
        .await?;

        // Launch `claude` by ABSOLUTE path (not `command claude`) so the
        // worker resolves the same native binary regardless of the
        // tmux/launchd PATH — which differs from the operator's
        // interactive shell and is the root of the v3 install/PATH
        // split-brain. An absolute path also bypasses any `claude`
        // shell-function shadow. `CLAUDE_CONFIG_DIR` is passed inline so
        // the worker stays pinned to its configured account even if the
        // operator's session has a different one set.
        let launch_line =
            build_launch_line(&self.config.claude_config_dir, &self.config.claude_bin)?;

        debug!(launch = %launch_line, "launching claude CLI in worker window");
        tmux::send_keys(
            SCOPE,
            &self.config.tmux_session,
            &window_name,
            &[launch_line.as_str()],
        )
        .await?;
        tmux::press_enter(SCOPE, &self.config.tmux_session, &window_name).await?;

        // 2h / W6.5 ① — wait for Claude's COMPOSER to actually be ready
        // before pasting, by polling the pane (not a fixed sleep, which
        // raced the ~8-10s TUI boot). The wait dismisses the trust dialog
        // deliberately if it appears, and never declares ready on a dialog
        // screen (the old bare-`❯`-count heuristic did exactly that and
        // silently lost the mandate). A short settle follows.
        if !wait_for_claude_ready(&self.config.tmux_session, &window_name).await {
            tracing::warn!(window = %window_name, "claude ready composer not seen in time; pasting anyway");
        }
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
        info!(
            worker = ?worker_id,
            window = %window_name,
            directive_bytes = directive.len(),
            "directive paste complete; checking delivery before submit"
        );

        // W6.5 ① — provable delivery, part 2: a beat between paste and
        // Enter, then a pane capture that must show the directive head (or
        // the TUI's collapsed paste placeholder). Silent loss is no longer
        // possible: every spawn either logs delivery or warns loudly.
        sleep(PASTE_TO_ENTER_BEAT).await;
        let paste_target = format!("{}:{}", self.config.tmux_session, window_name);
        match tmux::tmux_capture(&paste_target, 80).await {
            Some(pane) if directive_visible(&pane) => {
                info!(window = %window_name, "directive visible in composer; submitting");
            }
            Some(pane) => {
                let tail: Vec<&str> = pane.lines().rev().take(5).collect();
                let tail = tail.into_iter().rev().collect::<Vec<_>>().join(" | ");
                tracing::warn!(
                    window = %window_name,
                    pane_tail = %tail,
                    "POST-PASTE CHECK FAILED — directive not visible in pane; the mandate may have been swallowed (dialog race?). Submitting anyway; inspect this worker"
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

/// Phase string baked into the HMAC trust marker. Derived from the
/// mandate's `metadata["phase"]` when present, otherwise the literal
/// `"dispatch"` so the marker still verifies. The phase is part of the
/// HMAC input so v3 workers see a stable, predictable identifier.
fn phase_for(mandate: &Mandate) -> String {
    mandate
        .metadata
        .get("phase")
        .cloned()
        .unwrap_or_else(|| "dispatch".to_string())
}

/// Resolve the HMAC key to use for this dispatch. Caller-supplied key
/// wins; otherwise fall back to the process-global default. See
/// [`crate::hmac::default_key`] for why the fallback exists.
///
/// The returned reference's lifetime tracks the caller's `Option`. When
/// we fall back to `default_key()` (`&'static HmacKey`) the static
/// lifetime is implicitly coerced down to the elided one — no unsafe
/// required.
fn resolve_key(configured: Option<&HmacKey>) -> &HmacKey {
    configured.unwrap_or_else(|| default_key())
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

/// What a captured worker pane is currently showing, as far as directive
/// delivery is concerned. Derived by [`classify_claude_pane`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneState {
    /// The directory-trust dialog is on screen. Pasting now LOSES the
    /// directive: the paste is swallowed by the dialog and the trailing
    /// Enter accepts it. This was the silent mandate-loss mechanism
    /// (W6-D conversion act, 2026-06-11): the dialog renders a `❯`
    /// selector which, plus the shell launch-line `❯`, satisfied the old
    /// `matches('❯').count() >= 2` heuristic DURING the dialog.
    TrustDialog,
    /// The composer is ready for input.
    Ready,
    /// Still booting (banner, spinner, blank pane).
    Booting,
}

/// Classify a captured pane. Dialog detection takes precedence over
/// ready detection — a dialog screen must NEVER classify as ready, no
/// matter what else has scrolled into view. Pure; unit-tested against
/// fixtures of the live failure screens.
fn classify_claude_pane(pane: &str) -> PaneState {
    if CLAUDE_DIALOG_MARKERS.iter().any(|m| pane.contains(m)) {
        return PaneState::TrustDialog;
    }
    if CLAUDE_READY_MARKERS.iter().any(|m| pane.contains(m)) {
        return PaneState::Ready;
    }
    PaneState::Booting
}

/// Post-paste delivery check: is the pasted directive actually visible in
/// the pane? Every dispatched directive starts with the HMAC trust-marker
/// line, so its `subctl-master directive` ident is the head we look for.
/// Large pastes may render as the TUI's collapsed placeholder
/// (`[Pasted text #1 +N lines]`) instead of the literal text — that
/// counts as delivered too. Shared with the Codex adapter.
pub(crate) fn directive_visible(pane: &str) -> bool {
    pane.contains("subctl-master directive") || pane.contains("Pasted text")
}

/// Cutover Phase 2 (2h), hardened in W6.5 ① — poll the worker pane until
/// Claude's composer is ready before pasting the directive.
///
/// - Ready = a composer-specific marker ([`CLAUDE_READY_MARKERS`]) with NO
///   dialog marker on screen. Never a bare `❯` count — the trust dialog
///   renders one and the old heuristic pasted straight into it.
/// - The directory-trust dialog, if it appears despite the config
///   pre-trust ([`pre_trust_project_dir`]), is dismissed deliberately:
///   `1` (select "Yes, proceed") then Enter, at most
///   [`MAX_DIALOG_DISMISSALS`] times.
///
/// Polls every 500ms up to ~40s; returns whether ready was observed
/// (caller pastes regardless, degrading to the old fixed-sleep behavior
/// on timeout — with the post-paste check downstream to make any loss
/// loud).
async fn wait_for_claude_ready(session: &str, window: &str) -> bool {
    let target = format!("{session}:{window}");
    let mut dismissals = 0u32;
    for _ in 0..80 {
        if let Some(pane) = tmux::tmux_capture(&target, 80).await {
            match classify_claude_pane(&pane) {
                PaneState::Ready => return true,
                PaneState::TrustDialog if dismissals < MAX_DIALOG_DISMISSALS => {
                    dismissals += 1;
                    tracing::warn!(
                        %target,
                        attempt = dismissals,
                        "trust dialog rendered despite pre-trust; dismissing deliberately"
                    );
                    let _ = tmux::send_keys(SCOPE, session, window, &["1"]).await;
                    sleep(Duration::from_millis(200)).await;
                    let _ = tmux::press_enter(SCOPE, session, window).await;
                    sleep(Duration::from_millis(800)).await;
                    continue;
                }
                PaneState::TrustDialog | PaneState::Booting => {}
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    false
}

/// Pre-trust the working dir in the account's `.claude.json` so the
/// directory-trust dialog never renders for this spawn. Claude Code keys
/// the dialog off `projects["<dir>"].hasTrustDialogAccepted` in
/// `$CLAUDE_CONFIG_DIR/.claude.json` (schema verified against a live
/// operator config, 2026-06-11). Race-free, unlike dialog dismissal — the
/// entry is on disk before the CLI boots. Dismissal stays as the in-loop
/// fallback for the cases this can't handle.
///
/// Preserves every existing key (the file accretes per-project state the
/// CLI owns) and refuses to touch a file it can't parse rather than
/// clobber it — the CLI hard-fails on a corrupt `.claude.json`.
fn pre_trust_project_dir(claude_config_dir: &Path, working_dir: &Path) -> Result<()> {
    let cfg_err = |reason: String| Error::Provider {
        kind: ProviderKind::ClaudeCode,
        reason,
    };
    let wd = path_to_arg(working_dir)?;
    let cfg_path = claude_config_dir.join(".claude.json");
    let mut root: serde_json::Value = if cfg_path.exists() {
        let raw = std::fs::read_to_string(&cfg_path)
            .map_err(|e| cfg_err(format!("read {}: {e}", cfg_path.display())))?;
        serde_json::from_str(&raw).map_err(|e| {
            cfg_err(format!(
                "parse {}: {e} (refusing to clobber)",
                cfg_path.display()
            ))
        })?
    } else {
        serde_json::json!({})
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| cfg_err(format!("{} top level is not an object", cfg_path.display())))?;
    let projects = obj
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}));
    let projects = projects.as_object_mut().ok_or_else(|| {
        cfg_err(format!(
            "{} `projects` is not an object",
            cfg_path.display()
        ))
    })?;
    let entry = projects.entry(wd).or_insert_with(|| serde_json::json!({}));
    let entry = entry
        .as_object_mut()
        .ok_or_else(|| cfg_err("project entry is not an object".to_string()))?;
    entry.insert(
        "hasTrustDialogAccepted".to_string(),
        serde_json::Value::Bool(true),
    );
    // Write-then-rename so a crash never leaves a truncated .claude.json.
    let tmp = claude_config_dir.join(".claude.json.subctl-tmp");
    let serialized = serde_json::to_string_pretty(&root)
        .map_err(|e| cfg_err(format!("serialize .claude.json: {e}")))?;
    std::fs::write(&tmp, serialized)
        .map_err(|e| cfg_err(format!("write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, &cfg_path)
        .map_err(|e| cfg_err(format!("rename into {}: {e}", cfg_path.display())))?;
    Ok(())
}

/// Drop the per-session HMAC key into the worker's config dir
/// (`.subctl-directive-key`, 0600) so the worker-side
/// `subctl directive verify` verb can authenticate envelopes without the
/// secret riding in the spawn prompt. Same bytes + hygiene as v3's
/// `~/.local/state/subctl/teams/<id>/hmac.secret`. The hex value goes to
/// disk only — never log it ([`HmacKey`]'s `Debug` redacts).
pub(crate) fn provision_directive_key(dir: &Path, key: &HmacKey) -> Result<()> {
    use std::io::Write;
    let key_err = |reason: String| Error::Provider {
        kind: ProviderKind::ClaudeCode,
        reason,
    };
    let path = dir.join(".subctl-directive-key");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&path)
        .map_err(|e| key_err(format!("open {}: {e}", path.display())))?;
    writeln!(f, "{}", key.to_hex())
        .map_err(|e| key_err(format!("write {}: {e}", path.display())))?;
    // `mode(0o600)` only applies at create time; tighten a pre-existing
    // wider-permissioned file too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Build the worker launch command: an inline `CLAUDE_CONFIG_DIR=…`
/// assignment followed by the `claude` binary invoked by ABSOLUTE path
/// (never `command claude`), so the spawned worker resolves the same
/// native binary regardless of the tmux/launchd PATH.

fn build_launch_line(claude_config_dir: &Path, claude_bin: &Path) -> Result<String> {
    let config_dir = path_to_arg(claude_config_dir)?;
    let claude_bin = path_to_arg(claude_bin)?;
    Ok(format!("CLAUDE_CONFIG_DIR={config_dir} {claude_bin}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use uuid::Uuid;

    #[test]
    fn launch_line_uses_absolute_claude_binary_not_path_lookup() {
        let line = build_launch_line(
            Path::new("/home/op/.claude-jason"),
            Path::new("/home/op/.local/bin/claude"),
        )
        .unwrap();
        assert!(line.contains("CLAUDE_CONFIG_DIR=/home/op/.claude-jason"));
        assert!(line.contains("/home/op/.local/bin/claude"));
        // The whole point of A2: never PATH-relative `command claude`.
        assert!(
            !line.contains("command claude"),
            "launch line must not depend on PATH resolution: {line}"
        );
    }

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

    // ── W6.5 ① — composer-specific ready matcher ─────────────────────────

    /// Regression fixture for the 2026-06-11 live mandate loss: the trust
    /// dialog plus the shell launch line render TWO `❯` chars, which the
    /// old `matches('❯').count() >= 2` heuristic declared ready. The
    /// classifier must call this a dialog, never ready.
    const TRUST_DIALOG_PANE: &str = "\
❯ CLAUDE_CONFIG_DIR=/Users/op/.claude-argent /Users/op/.local/bin/claude

 Do you trust the files in this folder?

 /tmp/w6-argent-proof

 ❯ 1. Yes, proceed
   2. No, exit
";

    #[test]
    fn trust_dialog_with_two_chevrons_is_not_ready() {
        assert_eq!(
            classify_claude_pane(TRUST_DIALOG_PANE),
            PaneState::TrustDialog
        );
        assert!(
            TRUST_DIALOG_PANE.matches('❯').count() >= 2,
            "fixture must reproduce the old heuristic's false positive"
        );
    }

    #[test]
    fn ready_composer_classifies_ready() {
        let pane = "╭───╮\n│ ❯ Try \"fix a bug\"  │\n╰───╯\n  ? for shortcuts";
        assert_eq!(classify_claude_pane(pane), PaneState::Ready);
        let chevron_only = "status line ⏵⏵ accept edits on";
        assert_eq!(classify_claude_pane(chevron_only), PaneState::Ready);
    }

    #[test]
    fn dialog_marker_wins_over_ready_marker() {
        // Even if a composer marker has scrolled into view, a dialog on
        // screen means pasting loses the directive — dialog must win.
        let pane = format!("? for shortcuts\n{TRUST_DIALOG_PANE}");
        assert_eq!(classify_claude_pane(&pane), PaneState::TrustDialog);
    }

    #[test]
    fn booting_banner_is_not_ready() {
        let pane = "❯ CLAUDE_CONFIG_DIR=/x /y/claude\n  Loading…\n  Claude Code v2";
        assert_eq!(classify_claude_pane(pane), PaneState::Booting);
    }

    #[test]
    fn directive_visible_matches_head_or_paste_placeholder() {
        assert!(directive_visible(
            "│ [subctl-master directive · phase=dispatch · ts:2026-06-11T00:00:00.000Z · hmac:abcdef0123456789] │"
        ));
        assert!(directive_visible("│ [Pasted text #1 +120 lines] │"));
        assert!(!directive_visible("❯ Try \"fix a bug\""));
    }

    // ── W6.5 ① — config pre-trust + key provisioning ─────────────────────

    #[test]
    fn pre_trust_creates_claude_json_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        pre_trust_project_dir(dir.path(), Path::new("/tmp/wt")).unwrap();
        let raw = std::fs::read_to_string(dir.path().join(".claude.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["projects"]["/tmp/wt"]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn pre_trust_preserves_existing_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"oauthAccount":{"email":"op@example.com"},"projects":{"/other":{"hasTrustDialogAccepted":false,"allowedTools":["Bash"]}}}"#,
        )
        .unwrap();
        pre_trust_project_dir(dir.path(), Path::new("/tmp/wt")).unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["oauthAccount"]["email"], "op@example.com");
        assert_eq!(v["projects"]["/other"]["hasTrustDialogAccepted"], false);
        assert_eq!(v["projects"]["/other"]["allowedTools"][0], "Bash");
        assert_eq!(v["projects"]["/tmp/wt"]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn pre_trust_upserts_existing_project_entry_without_dropping_state() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"projects":{"/tmp/wt":{"hasTrustDialogAccepted":false,"ignorePatterns":["dist"]}}}"#,
        )
        .unwrap();
        pre_trust_project_dir(dir.path(), Path::new("/tmp/wt")).unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["projects"]["/tmp/wt"]["hasTrustDialogAccepted"], true);
        assert_eq!(v["projects"]["/tmp/wt"]["ignorePatterns"][0], "dist");
    }

    #[test]
    fn pre_trust_refuses_to_clobber_unparsable_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = pre_trust_project_dir(dir.path(), Path::new("/tmp/wt")).unwrap_err();
        assert!(
            err.to_string().contains("refusing to clobber"),
            "got: {err}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn provision_directive_key_writes_hex_with_0600() {
        let dir = tempfile::tempdir().unwrap();
        let key = HmacKey::generate();
        provision_directive_key(dir.path(), &key).unwrap();
        let path = dir.path().join(".subctl-directive-key");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.trim(), key.to_hex());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "key file must be 0600");
        }
    }

    #[test]
    fn provision_directive_key_overwrite_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let k1 = HmacKey::generate();
        let k2 = HmacKey::generate();
        provision_directive_key(dir.path(), &k1).unwrap();
        provision_directive_key(dir.path(), &k2).unwrap();
        let raw = std::fs::read_to_string(dir.path().join(".subctl-directive-key")).unwrap();
        assert_eq!(raw.trim(), k2.to_hex(), "later key must replace earlier");
    }
}
