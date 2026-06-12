//! W6.5 ① integration test — Codex mandate delivery through BOTH cold-start
//! interstitials (update modal, then directory-trust dialog), against a
//! FAKE `codex` binary in a real tmux session.
//!
//! Reproduces the 2026-06-09 live failure (closet entry): codex booted to
//! "Do you trust the contents of this directory?" and the v4 directive
//! paste was lost — Phase 1 had no ready-poll at all, just a fixed sleep.
//!
//! The fake binary's screen sequence:
//!   1. "Update available … Press enter" modal  → expects `2` + Enter,
//!   2. trust dialog                            → expects `1` + Enter,
//!   3. ready status line (`Context 100% left`) → echoes stdin.
//!
//! Proves: the wait loop dismisses both interstitials with the
//! v3-validated keys, never pastes before the status line renders, and
//! the directive ends up visible in the pane. Also proves the verify-key
//! provisioning into CODEX_HOME.
//!
//! Skips cleanly when tmux is unavailable; touches no real account.

#![cfg(unix)]

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use evy_core::{Mandate, MandateId, PolicyMode, Provider, ProviderKind};
use evy_providers::{tmux_capture, tmux_kill_session, CodexConfig, CodexProvider, HmacKey};

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

/// Fake `codex`: update modal → trust dialog → ready status line → echo.
/// Records the dismissal keys it received so the test can assert the
/// v3-validated sequence (`2` for Skip, `1` for Yes) was sent.
const FAKE_CODEX: &str = r#"#!/bin/bash
printf '\n Update available! 0.130.0 -> 0.131.0\n\n \xe2\x9d\xaf 1. Update now\n   2. Skip\n\n Press enter to update\n'
read -r update_answer
printf '\033[2J\033[H'
printf '\n Do you trust the contents of this directory?\n\n %s\n\n \xe2\x9d\xaf 1. Yes, continue\n   2. No, quit\n' "$PWD"
read -r trust_answer
printf '\033[2J\033[H'
printf 'answers update=%s trust=%s\n' "$update_answer" "$trust_answer" > "$CODEX_HOME/dismissal-keys.log"
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
async fn codex_mandate_delivery_dismisses_update_modal_and_trust_dialog() -> anyhow::Result<()> {
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
    std::fs::write(&fake_bin, FAKE_CODEX)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755))?;
    }

    let session = format!("w65-fake-codex-{}", std::process::id());
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
