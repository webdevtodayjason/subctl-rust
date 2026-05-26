//! End-to-end integration test against a real OpenAI Codex CLI.
//!
//! Parallel structure to `real_claude_code_spawn.rs`. Marked
//! `#[ignore]` so CI doesn't try to spawn `codex` — run manually:
//!
//! ```bash
//! # 1. Prepare a long-running tmux session for the worker to live in.
//! tmux new-session -d -s evy-real-codex-test
//!
//! # 2. Point at a real CODEX_HOME with a logged-in account + a
//! #    config.toml that pins `model = "gpt-5.5"` (see
//! #    docs/codex-account-setup.md in the subctl v3 repo for the
//! #    exact recipe — v3 already seeds this for the operator).
//! export EVY_TEST_CODEX_HOME="$HOME/.codex-jason"
//! export EVY_TEST_CODEX_TMUX_SESSION="evy-real-codex-test"
//! export EVY_TEST_CODEX_WORKING_DIR="/tmp/evy-phase2-codex-smoke"
//!
//! # 3. Make sure the working dir exists and is trusted.
//! mkdir -p "$EVY_TEST_CODEX_WORKING_DIR"
//!
//! # 4. Run the test.
//! cargo test --ignored -p evy-providers --test real_codex_spawn -- --nocapture
//!
//! # 5. Clean up.
//! tmux kill-session -t evy-real-codex-test
//! ```
//!
//! # TODO — install gap
//!
//! The Codex CLI install path on this dev box is non-trivial: it ships
//! through OpenAI's private channels and the worker (me) cannot install
//! it without operator credentials. The test below is structurally
//! correct and compiles, but I have not been able to run it end-to-end
//! against a real `codex` binary. Treat this file as the contract for
//! 2B's first task: when the Codex CLI is reachable from the test
//! environment, this test should pass without code changes.
//!
//! The test skips (returns Ok) when `EVY_TEST_CODEX_HOME` is unset, so
//! a developer who runs `cargo test --workspace --ignored` without
//! exporting the env doesn't get a spurious failure.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use evy_core::{Mandate, MandateId, PolicyMode, Provider, ProviderKind, WorkerStatus};
use evy_providers::{CodexConfig, CodexProvider, HmacKey};

const MARKER_PATH: &str = "/tmp/evy-phase2-codex-smoke.txt";
const MARKER_CONTENT: &str = "hello phase2 codex";

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
#[ignore = "requires real Codex CLI + test account; run with --ignored"]
async fn dispatch_real_codex_worker_and_complete() -> anyhow::Result<()> {
    let Some(codex_home) = env_var_or_skip("EVY_TEST_CODEX_HOME") else {
        return Ok(());
    };
    let Some(tmux_session) = env_var_or_skip("EVY_TEST_CODEX_TMUX_SESSION") else {
        return Ok(());
    };
    let Some(working_dir) = env_var_or_skip("EVY_TEST_CODEX_WORKING_DIR") else {
        return Ok(());
    };

    let _ = std::fs::remove_file(MARKER_PATH);

    let cfg = CodexConfig {
        codex_home: PathBuf::from(codex_home),
        tmux_session,
        working_dir: PathBuf::from(working_dir),
        // gpt-5.5 because Codex on ChatGPT rejects the default `gpt-5`
        // model id (per the operator's reference notes on ChatGPT auth).
        model: Some("gpt-5.5".to_string()),
        policy_mode: PolicyMode::Trusted,
        hmac_key: Some(HmacKey::generate()),
    };
    let provider = CodexProvider::new(cfg);

    let mandate = Mandate {
        id: MandateId::new(),
        provider: ProviderKind::Codex,
        goal: format!("write the literal text `{MARKER_CONTENT}` to {MARKER_PATH}"),
        context: "Phase 2 Slice 2A end-to-end smoke (Codex variant). You are running \
            inside a tmux window spawned by Evy v4."
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
        timeout: Some(Duration::from_secs(90)),
        metadata: HashMap::new(),
    };

    let worker = provider.dispatch(&mandate).await?;
    eprintln!("dispatched codex worker id={:?}", worker.id());

    // Codex tends to be slower to first action than Claude Code (TUI
    // update modal + slower model-pick); 120s is the conservative
    // budget. The test asserts on the marker file, not on the worker's
    // exit signal — Codex's "DONE" phrase classifier lives in the
    // dashboard staleness watchdog, which this test doesn't reproduce.
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
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
            anyhow::bail!("codex worker terminated before writing marker: {last_status:?}");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    let _ = worker.cancel().await;

    let contents = std::fs::read_to_string(MARKER_PATH).map_err(|e| {
        anyhow::anyhow!(
            "marker file {MARKER_PATH} not present after codex worker drain (last status: {last_status:?}): {e}",
        )
    })?;
    assert!(
        contents.contains(MARKER_CONTENT),
        "marker file should contain {MARKER_CONTENT:?}; got {contents:?}",
    );

    let _ = std::fs::remove_file(MARKER_PATH);
    Ok(())
}
