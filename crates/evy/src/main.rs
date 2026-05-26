//! Evy v4 daemon binary entry point.
//!
//! Phase 2 — long-lived daemon by default. The `--smoke` flag preserves
//! the Phase 1 smoke-test path so the integration test in
//! `crates/evy/tests/smoke.rs` keeps exercising the bootstrap wiring.
//!
//! See [`evy::run_daemon`] and [`evy::run_smoke_test`] for the actual
//! logic; this file is a thin wrapper that owns CLI parsing + the
//! anyhow::Result return.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

/// Evy v4 — learning orchestrator daemon. Run with `--smoke` to exercise
/// the Phase 1 bootstrap and exit; run with no flags for the long-lived
/// daemon (default).
#[derive(Debug, Parser)]
#[command(name = "evy", version, about, long_about = None)]
struct Args {
    /// Run the Phase 1 smoke sequence (register a heartbeat cron, wait
    /// for one Succeeded run, exit). Provides a regression check that
    /// the boot wiring still composes. Without this flag, the daemon
    /// stays up and blocks on SIGTERM / Ctrl-C.
    #[arg(long)]
    smoke: bool,

    /// Path to the daemon config TOML. Overrides the
    /// `SUBCTL_EVY_CONFIG` env var; falls back to `config/evy.toml`
    /// relative to the working directory when neither is set.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    evy::init_tracing();
    let args = Args::parse();

    let config = match args.config.as_deref() {
        Some(path) => evy::Config::load_from(path)
            .with_context(|| format!("loading evy config from {}", path.display()))?,
        None => evy::Config::load().context("loading evy config from default sources")?,
    };

    if args.smoke {
        let report = evy::run_smoke_test(config).await?;
        tracing::info!(
            job_id = %report.job_id,
            fire_latency_ms = u64::try_from(report.fire_latency.as_millis()).unwrap_or(u64::MAX),
            total_ms = u64::try_from(report.total_wall_clock.as_millis()).unwrap_or(u64::MAX),
            "smoke report",
        );
        return Ok(());
    }

    evy::run_daemon(config).await
}
