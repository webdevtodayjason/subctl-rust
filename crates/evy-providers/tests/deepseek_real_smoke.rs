//! End-to-end smoke against the real DeepSeek API.
//!
//! `#[ignore]`'d so CI never burns API credit on accident. Run manually:
//!
//! ```bash
//! export DEEPSEEK_API_KEY="sk-..."
//! # Optional overrides:
//! # export EVY_TEST_DEEPSEEK_MODEL="deepseek-coder"
//! # export EVY_TEST_DEEPSEEK_OUTPUT_DIR="/tmp/evy-deepseek-smoke"
//! cargo test --ignored -p evy-providers --test deepseek_real_smoke -- --nocapture
//! ```
//!
//! The test skips (returns Ok) when `DEEPSEEK_API_KEY` is unset so a
//! developer who runs `cargo test --workspace --ignored` without
//! exporting the key doesn't get a spurious failure.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use evy_core::{Mandate, MandateId, PolicyMode, Provider, ProviderKind, WorkerStatus};
use evy_providers::{DeepSeekConfig, DeepSeekProvider};

#[tokio::test]
#[ignore = "requires real DEEPSEEK_API_KEY; run with --ignored"]
async fn dispatch_against_real_deepseek_and_get_deliverable() -> anyhow::Result<()> {
    let api_key = match std::env::var("DEEPSEEK_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!(
                "skipping: DEEPSEEK_API_KEY not set — see test file rustdoc for the manual-run recipe"
            );
            return Ok(());
        }
    };

    let defaults = DeepSeekConfig::default();
    let cfg = DeepSeekConfig {
        api_key,
        model: std::env::var("EVY_TEST_DEEPSEEK_MODEL").unwrap_or(defaults.model),
        output_dir: std::env::var("EVY_TEST_DEEPSEEK_OUTPUT_DIR")
            .map(PathBuf::from)
            .unwrap_or(defaults.output_dir),
        // Be liberal with the upstream timeout — DeepSeek's queues can
        // run slow during model-release events.
        timeout_secs: 120,
        policy_mode: PolicyMode::Trusted,
        api_endpoint: defaults.api_endpoint,
    };
    std::fs::create_dir_all(&cfg.output_dir)?;

    let output_dir = cfg.output_dir.clone();
    let provider = DeepSeekProvider::with_config(cfg);

    // Sanity-check the live endpoint first — cheaper to fail here than
    // burn a completion on a misconfigured key.
    provider.healthcheck().await?;

    let mandate = Mandate {
        id: MandateId::new(),
        provider: ProviderKind::DeepSeek,
        goal: "produce the literal text `hello phase3 deepseek`".to_string(),
        context: "Phase 3 Slice F end-to-end smoke against the real DeepSeek API.".to_string(),
        deliverable: "exactly the goal text and nothing else".to_string(),
        done_when: vec!["deliverable file is non-empty".to_string()],
        constraints: vec!["no preamble, no postscript".to_string()],
        policy_mode: PolicyMode::Trusted,
        timeout: Some(Duration::from_secs(60)),
        metadata: HashMap::new(),
    };

    let worker = provider.dispatch(&mandate).await?;
    eprintln!("dispatched DeepSeek worker id={:?}", worker.id());

    let status = worker.wait().await?;
    assert_eq!(status, WorkerStatus::Succeeded, "worker should succeed");

    let path = output_dir.join(format!("{}.md", worker.id().0.simple()));
    let contents = std::fs::read_to_string(&path)?;
    assert!(
        !contents.trim().is_empty(),
        "deliverable file should be non-empty"
    );
    eprintln!("deliverable ({} bytes): {contents}", contents.len());
    Ok(())
}
