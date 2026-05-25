//! Evy v4 daemon binary entry point.
//!
//! Phase 1 wiring — scheduler + policy + providers integrated end-to-end.
//! See ADR 0020 for architectural context.
//!
//! The wiring lives in [`evy::run_daemon`]; this file is a thin entry so
//! the integration test in `crates/evy/tests/smoke.rs` can drive the
//! same code path without spawning a subprocess.

use anyhow::{Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    evy::init_tracing();

    let config = evy::Config::load().context("loading evy config")?;
    tracing::info!(?config, "evy v4 booting");

    let report = evy::run_daemon(config).await?;
    tracing::info!(
        job_id = %report.job_id,
        fire_latency_ms = u64::try_from(report.fire_latency.as_millis()).unwrap_or(u64::MAX),
        total_ms = u64::try_from(report.total_wall_clock.as_millis()).unwrap_or(u64::MAX),
        "phase 1 smoke report",
    );
    Ok(())
}
