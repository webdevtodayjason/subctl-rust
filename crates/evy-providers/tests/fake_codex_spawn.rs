//! W6.5 ① / EA1 integration test — Codex mandate delivery through BOTH
//! cold-start interstitials (update modal, then directory-trust dialog),
//! against a FAKE `codex` binary in a real tmux session.
//!
//! Reproduces the 2026-06-09 live failure (closet entry): codex booted to
//! "Do you trust the contents of this directory?" and the v4 directive
//! paste was lost — Phase 1 had no ready-poll at all, just a fixed sleep.
//!
//! EA1 repinned the fake binary to codex **v0.130.0's real boot screens**
//! (live censure evidence, 2026-06-12T01:05Z): banner
//! `>_ OpenAI Codex (v0.130.0)`, composer placeholder
//! `› Implement {feature}`, status line `gpt-5.5 medium · <dir>` — and
//! deliberately NO legacy `Context … % left` line, so this test FAILS if
//! the ready-matcher ever regresses to requiring the old status line
//! (which is exactly how the censured live dispatch hung). A separate
//! old-screen case keeps backward compat proven.
//!
//! The fake binary's screen sequence:
//!   1. "Update available … Press enter" modal  → expects `2` + Enter,
//!   2. trust dialog                            → expects `1` + Enter,
//!   3. v0.130.0 composer + status line          → echoes stdin.
//!
//! Proves: the wait loop dismisses both interstitials with the
//! v3-validated keys, never pastes before the composer renders, and the
//! directive ends up visible in the pane. Also proves the verify-key
//! provisioning into CODEX_HOME and the register-or-cleanup
//! [`WindowGuard`] semantics.
//!
//! Skips cleanly when tmux is unavailable; touches no real account.

#![cfg(unix)]

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use evy_core::{Mandate, MandateId, PolicyMode, Provider, ProviderKind};
use evy_providers::{
    tmux_capture, tmux_kill_session, CodexConfig, CodexProvider, HmacKey, WindowGuard,
};

/// Run `tmux <args>` against the first available tmux binary, returning
/// the output, or `None` if no tmux candidate works.
fn tmux_run(args: &[&str]) -> Option<std::process::Output> {
    for candidate in [
        "tmux",
        "/opt/homebrew/bin/tmux",
        "/usr/local/bin/tmux",
        "/usr/bin/tmux",
    ] {
        if let Ok(out) = Command::new(candidate).args(args).output() {
            return Some(out);
        }
    }
    None
}

fn tmux_available() -> bool {
    for candidate in [
        "tmux",
        "/opt/homebrew/bin/tmux",
        "/usr/local/bin/tmux",
        "/usr/bin/tmux",
    ] {
        if Command::new(candidate)
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

struct SessionGuard(String);
impl Drop for SessionGuard {
    fn drop(&mut self) {
        for candidate in [
            "tmux",
            "/opt/homebrew/bin/tmux",
            "/usr/local/bin/tmux",
            "/usr/bin/tmux",
        ] {
            if Command::new(candidate)
                .args(["kill-session", "-t", &self.0])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                break;
            }
        }
    }
}

/// Fake `codex` pinned to v0.130.0's REAL screens: banner + update modal
/// → trust dialog → composer placeholder (`›`, \xe2\x80\xba) + status
/// line (`gpt-5.5 medium · <dir>`, `·` = \xc2\xb7) → echo. The ready
/// screen renders NO legacy `Context … % left` line — the matcher must
/// fire on v0.130.0 reality alone or this test hangs the paste exactly
/// like the censured live dispatch. Records the dismissal keys it
/// received so the test can assert the v3-validated sequence (`2` for
/// Skip, `1` for Yes) was sent.
const FAKE_CODEX_V0130: &str = r#"#!/bin/bash
printf '>_ OpenAI Codex (v0.130.0)\n\n Update available! 0.130.0 -> 0.131.0\n\n \xe2\x80\xba 1. Update now\n   2. Skip\n\n Press enter to confirm\n'
read -r update_answer
printf '\033[2J\033[H'
printf '\n Do you trust the contents of this directory?\n\n %s\n\n \xe2\x9d\xaf 1. Yes, continue\n   2. No, quit\n' "$PWD"
read -r trust_answer
printf '\033[2J\033[H'
printf 'answers update=%s trust=%s\n' "$update_answer" "$trust_answer" > "$CODEX_HOME/dismissal-keys.log"
printf '>_ OpenAI Codex (v0.130.0)\n\n \xe2\x80\xba Implement {feature}\n\n gpt-5.5 medium \xc2\xb7 %s\n' "$CODEX_HOME"
exec cat
"#;

/// Old-screen fake (pre-v0.130 era): boots straight to the legacy
/// `Context 100% left` status line, no composer chevron. Backward-compat
/// case — the repinned matcher must still fire ready on older codex
/// versions.
const FAKE_CODEX_LEGACY: &str = r#"#!/bin/bash
printf '\n  Context 100%% left \xc2\xb7 0 in \xc2\xb7 0 out\n'
exec cat
"#;

fn fixture_mandate() -> Mandate {
    Mandate {
        id: MandateId::new(),
        provider: ProviderKind::Codex,
        goal: "prove codex mandate delivery through both interstitials".to_string(),
        context: "W6.5 spawn-integrity fake-binary harness.".to_string(),
        deliverable: "the directive visible in the worker pane".to_string(),
        done_when: vec!["pane shows the trust-marker line".to_string()],
        constraints: vec![],
        policy_mode: PolicyMode::Trusted,
        timeout: Some(Duration::from_secs(120)),
        metadata: HashMap::new(),
    }
}

#[tokio::test]
async fn codex_v0130_mandate_delivery_dismisses_update_modal_and_trust_dialog() -> anyhow::Result<()>
{
    if !tmux_available() {
        eprintln!("skipping: tmux not available on this machine");
        return Ok(());
    }

    let root = tempfile::tempdir()?;
    let codex_home = root.path().join("codex-home");
    let working_dir = root.path().join("proj");
    std::fs::create_dir_all(&codex_home)?;
    std::fs::create_dir_all(&working_dir)?;

    let fake_bin = root.path().join("codex");
    std::fs::write(&fake_bin, FAKE_CODEX_V0130)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755))?;
    }

    let session = format!("ea1-fake-codex-{}", std::process::id());
    let _guard = SessionGuard(session.clone());
    let key = HmacKey::generate();
    let key_hex = key.to_hex();

    let provider = CodexProvider::new(CodexConfig {
        codex_home: codex_home.clone(),
        codex_bin: fake_bin,
        tmux_session: session.clone(),
        working_dir: working_dir.clone(),
        model: None,
        policy_mode: PolicyMode::Trusted,
        hmac_key: Some(key),
    });
    provider.ensure_session().await?;

    let handle = provider.dispatch(&fixture_mandate()).await?;

    // Verify key provisioned into CODEX_HOME, 0600.
    let key_path = codex_home.join(".subctl-directive-key");
    assert_eq!(
        std::fs::read_to_string(&key_path)?.trim(),
        key_hex,
        "provisioned key must match the session HMAC key"
    );
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key_path)?.permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "key file must be 0600");
    }

    // The directive must be visible in the pane — only reachable after
    // BOTH interstitials were dismissed and the status line rendered.
    let mut delivered = false;
    for _ in 0..20 {
        if let Some(pane) = tmux_capture(&session, 200).await {
            if pane.contains("subctl-master directive") {
                delivered = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let final_pane = tmux_capture(&session, 200).await.unwrap_or_default();
    assert!(
        delivered,
        "directive never became visible in the codex pane — mandate lost. Pane:\n{final_pane}"
    );

    // The fake binary logged which keys dismissed each interstitial —
    // assert the v3-validated sequence: `2` (Skip) then `1` (Yes).
    let keys_log = std::fs::read_to_string(codex_home.join("dismissal-keys.log"))?;
    assert_eq!(
        keys_log.trim(),
        "answers update=2 trust=1",
        "dismissal keys must match the v3-validated sequence"
    );

    handle.cancel().await?;
    tmux_kill_session(&session).await?;
    Ok(())
}

#[tokio::test]
async fn codex_legacy_status_line_still_delivers() -> anyhow::Result<()> {
    if !tmux_available() {
        eprintln!("skipping: tmux not available on this machine");
        return Ok(());
    }

    let root = tempfile::tempdir()?;
    let codex_home = root.path().join("codex-home");
    let working_dir = root.path().join("proj");
    std::fs::create_dir_all(&codex_home)?;
    std::fs::create_dir_all(&working_dir)?;

    let fake_bin = root.path().join("codex");
    std::fs::write(&fake_bin, FAKE_CODEX_LEGACY)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755))?;
    }

    let session = format!("ea1-legacy-codex-{}", std::process::id());
    let _guard = SessionGuard(session.clone());

    let provider = CodexProvider::new(CodexConfig {
        codex_home: codex_home.clone(),
        codex_bin: fake_bin,
        tmux_session: session.clone(),
        working_dir,
        model: None,
        policy_mode: PolicyMode::Trusted,
        hmac_key: Some(HmacKey::generate()),
    });
    provider.ensure_session().await?;

    let handle = provider.dispatch(&fixture_mandate()).await?;

    let mut delivered = false;
    for _ in 0..20 {
        if let Some(pane) = tmux_capture(&session, 200).await {
            if pane.contains("subctl-master directive") {
                delivered = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let final_pane = tmux_capture(&session, 200).await.unwrap_or_default();
    assert!(
        delivered,
        "directive never became visible against legacy codex screens — backward compat broken. Pane:\n{final_pane}"
    );

    handle.cancel().await?;
    tmux_kill_session(&session).await?;
    Ok(())
}

/// EA1 register-or-cleanup: an armed [`WindowGuard`] dropped (error
/// return or future abort after `tmux new-window`) must tear the window
/// down; a disarmed one (dispatch completed, handle handed to the
/// registry) must leave it alone. No silent zombie windows.
#[tokio::test]
async fn window_guard_tears_down_window_unless_disarmed() -> anyhow::Result<()> {
    if !tmux_available() {
        eprintln!("skipping: tmux not available on this machine");
        return Ok(());
    }

    let session = format!("ea1-guard-{}", std::process::id());
    let _guard = SessionGuard(session.clone());
    let new_session = tmux_run(&["new-session", "-d", "-s", &session, "-x", "80", "-y", "24"])
        .expect("tmux candidate available");
    anyhow::ensure!(new_session.status.success(), "tmux new-session failed");
    for name in ["w-armed", "w-disarmed"] {
        let out = tmux_run(&["new-window", "-t", &session, "-n", name])
            .expect("tmux candidate available");
        anyhow::ensure!(out.status.success(), "tmux new-window {name} failed");
    }

    // Armed guard dropped → the abandoned window is killed.
    drop(WindowGuard::new(&session, "w-armed"));
    // Disarmed guard dropped → the registered worker's window survives.
    let mut disarmed = WindowGuard::new(&session, "w-disarmed");
    disarmed.disarm();
    drop(disarmed);

    let out = tmux_run(&["list-windows", "-t", &session, "-F", "#{window_name}"])
        .expect("tmux candidate available");
    let windows = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        !windows.lines().any(|w| w.trim() == "w-armed"),
        "armed guard must tear its window down; windows: {windows}"
    );
    assert!(
        windows.lines().any(|w| w.trim() == "w-disarmed"),
        "disarmed guard must leave its window alone; windows: {windows}"
    );

    tmux_kill_session(&session).await?;
    Ok(())
}
