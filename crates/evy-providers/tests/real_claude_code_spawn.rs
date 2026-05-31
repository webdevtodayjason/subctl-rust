//! End-to-end integration test against a real Claude Code CLI.
//!
//! Marked `#[ignore]` so CI doesn't try to spawn `claude` — run manually
//! with:
//!
//! ```bash
//! # 1. Prepare a long-running tmux session for the worker to live in.
//! tmux new-session -d -s evy-real-claude-test
//!
//! # 2. Point at a real Claude Code config dir with a logged-in account.
//! export EVY_TEST_CLAUDE_CONFIG_DIR="$HOME/.claude-jason"
//! export EVY_TEST_CLAUDE_TMUX_SESSION="evy-real-claude-test"
//! export EVY_TEST_CLAUDE_WORKING_DIR="/tmp/evy-phase2-smoke"
//!
//! # 3. Make sure the working dir exists (Claude reads its `--cwd` from
//! #    the tmux window's pwd, so the dir must be present and writable).
//! mkdir -p "$EVY_TEST_CLAUDE_WORKING_DIR"
//!
//! # 4. Run the test.
//! cargo test --ignored -p evy-providers --test real_claude_code_spawn -- --nocapture
//!
//! # 5. Clean up.
//! tmux kill-session -t evy-real-claude-test
//! ```
//!
//! The test skips (returns Ok) when `EVY_TEST_CLAUDE_CONFIG_DIR` is
//! unset, so a developer who runs `cargo test --workspace --ignored`
//! without exporting the env doesn't get a spurious failure.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use evy_core::{Mandate, MandateId, PolicyMode, Provider, ProviderKind, WorkerStatus};
use evy_providers::{ClaudeCodeConfig, ClaudeCodeProvider, HmacKey};

/// Smoke marker file the test asks the worker to create. Lives in
/// `/tmp` so a stale file from a crashed previous run is obvious and
/// cheap to nuke.
const MARKER_PATH: &str = "/tmp/evy-phase2-smoke.txt";
const MARKER_CONTENT: &str = "hello phase2";

fn env_var_or_skip(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => {
            eprintln!("skipping: {name} not set — see test file rustdoc for the manual-run recipe",);
            None
        }
    }
}

#[tokio::test]
#[ignore = "requires real Claude Code CLI + test account; run with --ignored"]
async fn dispatch_real_claude_code_worker_and_complete() -> anyhow::Result<()> {
    let Some(config_dir) = env_var_or_skip("EVY_TEST_CLAUDE_CONFIG_DIR") else {
        return Ok(());
    };
    let Some(tmux_session) = env_var_or_skip("EVY_TEST_CLAUDE_TMUX_SESSION") else {
        return Ok(());
    };
    let Some(working_dir) = env_var_or_skip("EVY_TEST_CLAUDE_WORKING_DIR") else {
        return Ok(());
    };

    // Clean up any stale marker from a prior run.
    let _ = std::fs::remove_file(MARKER_PATH);

    let cfg = ClaudeCodeConfig {
        claude_config_dir: PathBuf::from(config_dir),
        // Native install — the real spawn test needs a real binary path.
        claude_bin: PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/bin/claude"),
        tmux_session,
        working_dir: PathBuf::from(working_dir),
        policy_mode: PolicyMode::Trusted,
        hmac_key: Some(HmacKey::generate()),
    };
    let provider = ClaudeCodeProvider::new(cfg);

    let mandate = Mandate {
        id: MandateId::new(),
        provider: ProviderKind::ClaudeCode,
        goal: format!("write the literal text `{MARKER_CONTENT}` to {MARKER_PATH}"),
        context: "Phase 2 Slice 2A end-to-end smoke. You are running inside a tmux \
            window spawned by Evy v4. Confirm by writing the marker file."
            .to_string(),
        deliverable: format!(
            "a file at {MARKER_PATH} whose contents are exactly `{MARKER_CONTENT}`"
        ),
        done_when: vec![
            format!("file {MARKER_PATH} exists"),
            format!("file contains `{MARKER_CONTENT}`"),
        ],
        constraints: vec!["do not edit any other file".to_string()],
        policy_mode: PolicyMode::Trusted,
        timeout: Some(Duration::from_secs(60)),
        metadata: HashMap::new(),
    };

    let worker = provider.dispatch(&mandate).await?;
    eprintln!("dispatched worker id={:?}", worker.id());

    // The worker has 60 seconds (per the mandate's timeout) to write
    // the marker. We poll for the marker independently of the worker's
    // exit signal so the test doesn't depend on the worker's exit code.
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut last_status = WorkerStatus::Running;
    while std::time::Instant::now() < deadline {
        if std::path::Path::new(MARKER_PATH).exists() {
            break;
        }
        last_status = worker.status().await?;
        if matches!(
            last_status,
            WorkerStatus::Cancelled | WorkerStatus::Failed(_)
        ) {
            anyhow::bail!("worker terminated before writing marker: {last_status:?}");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Drain the worker so we don't leak the tmux window even on assert
    // failure below. Best-effort cancel; ignore error.
    let _ = worker.cancel().await;

    let contents = std::fs::read_to_string(MARKER_PATH).map_err(|e| {
        anyhow::anyhow!(
            "marker file {MARKER_PATH} not present after worker drain (last status: {last_status:?}): {e}",
        )
    })?;
    assert!(
        contents.contains(MARKER_CONTENT),
        "marker file should contain {MARKER_CONTENT:?}; got {contents:?}",
    );

    // Final cleanup so a successful run leaves no trace.
    let _ = std::fs::remove_file(MARKER_PATH);
    Ok(())
}
