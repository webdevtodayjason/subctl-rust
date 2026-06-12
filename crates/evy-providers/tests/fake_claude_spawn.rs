//! W6.5 ① integration test — mandate delivery through the directory-trust
//! dialog, against a FAKE `claude` binary in a real tmux session.
//!
//! The fake binary reproduces the exact screen sequence that silently ate
//! the mandate in the 2026-06-11 live reproductions: it boots to the
//! directory-trust dialog (which renders a `❯` selector — the old
//! `matches('❯').count() >= 2` ready heuristic pasted straight into it),
//! waits for the dismissal keys, then clears to a ready composer and
//! echoes stdin so the pasted directive becomes visible in the pane.
//!
//! Proves the full hardened sequence end to end:
//!   1. config pre-trust lands in `.claude.json` before launch;
//!   2. the ready-wait does NOT declare ready on the dialog screen;
//!   3. the dialog is dismissed deliberately (`1` + Enter);
//!   4. the directive is pasted into the READY composer and is visible in
//!      the pane afterwards (the post-paste delivery check's criterion);
//!   5. the per-session verify key is provisioned at
//!      `.subctl-directive-key` (0600).
//!
//! Skips cleanly (no failure) when tmux is unavailable. No real Claude
//! account, daemon, or production config is touched — everything lives in
//! a tempdir + a throwaway tmux session.

#![cfg(unix)]

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use evy_core::{Mandate, MandateId, PolicyMode, Provider, ProviderKind};
use evy_providers::{
    tmux_capture, tmux_kill_session, ClaudeCodeConfig, ClaudeCodeProvider, HmacKey,
};

/// Is a usable tmux on this machine (same resolution order as the
/// crate's `tmux_bin`)? Tests skip — not fail — without one.
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

/// Kill the test session on scope exit, even on assertion failure.
struct SessionGuard(String);
impl Drop for SessionGuard {
    fn drop(&mut self) {
        let session = self.0.clone();
        // Best-effort sync kill — Drop can't await.
        for candidate in [
            "tmux",
            "/opt/homebrew/bin/tmux",
            "/usr/local/bin/tmux",
            "/usr/bin/tmux",
        ] {
            if Command::new(candidate)
                .args(["kill-session", "-t", &session])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                break;
            }
        }
    }
}

/// Fake `claude` that renders the trust dialog, waits for the dismissal,
/// then clears to a ready composer and echoes stdin (so the paste shows
/// up in the pane like a real composer would display it).
const FAKE_CLAUDE: &str = r#"#!/bin/bash
printf '\n Do you trust the files in this folder?\n\n %s\n\n \xe2\x9d\xaf 1. Yes, proceed\n   2. No, exit\n' "$PWD"
read -r _dismissal
printf '\033[2J\033[H'
printf '\xe2\x95\xad\xe2\x94\x80\xe2\x95\xae\n\xe2\x94\x82 \xe2\x9d\xaf Try "fix a bug" \xe2\x94\x82\n\xe2\x95\xb0\xe2\x94\x80\xe2\x95\xaf\n  ? for shortcuts\n'
exec cat
"#;

fn fixture_mandate() -> Mandate {
    Mandate {
        id: MandateId::new(),
        provider: ProviderKind::ClaudeCode,
        goal: "prove mandate delivery through the trust dialog".to_string(),
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
async fn mandate_delivery_is_provable_through_trust_dialog() -> anyhow::Result<()> {
    if !tmux_available() {
        eprintln!("skipping: tmux not available on this machine");
        return Ok(());
    }

    let root = tempfile::tempdir()?;
    let config_dir = root.path().join("claude-cfg");
    let working_dir = root.path().join("proj");
    std::fs::create_dir_all(&config_dir)?;
    std::fs::create_dir_all(&working_dir)?;

    let fake_bin = root.path().join("claude");
    std::fs::write(&fake_bin, FAKE_CLAUDE)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755))?;
    }

    let session = format!("w65-fake-claude-{}", std::process::id());
    let _guard = SessionGuard(session.clone());
    let key = HmacKey::generate();
    let key_hex = key.to_hex();

    let provider = ClaudeCodeProvider::new(ClaudeCodeConfig {
        claude_config_dir: config_dir.clone(),
        claude_bin: fake_bin,
        tmux_session: session.clone(),
        working_dir: working_dir.clone(),
        policy_mode: PolicyMode::Trusted,
        hmac_key: Some(key),
    });
    provider.ensure_session().await?;

    let handle = provider.dispatch(&fixture_mandate()).await?;

    // (1) pre-trust landed in .claude.json before the CLI booted.
    let claude_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(config_dir.join(".claude.json"))?)?;
    let wd_key = working_dir.to_str().unwrap();
    assert_eq!(
        claude_json["projects"][wd_key]["hasTrustDialogAccepted"], true,
        "dispatch must pre-trust the working dir"
    );

    // (5) verify key provisioned, 0600, same bytes the daemon signs with.
    let key_path = config_dir.join(".subctl-directive-key");
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

    // (2)+(3)+(4) the directive made it INTO the composer — the fake
    // binary only reaches its echo stage after the dialog was dismissed,
    // so the marker being visible proves the whole sequence held.
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
        "directive never became visible in the worker pane — mandate lost. Pane:\n{final_pane}"
    );
    assert!(
        !final_pane.contains("Do you trust"),
        "trust dialog still on screen after dispatch. Pane:\n{final_pane}"
    );

    handle.cancel().await?;
    tmux_kill_session(&session).await?;
    Ok(())
}
