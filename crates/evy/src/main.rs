use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("evy=info".parse()?))
        .with_target(false)
        .init();

    tracing::info!("Evy v4 — scaffold only. No primitives wired yet. See ADR 0020.");
    Ok(())
}

// TODO: Phase 1+ — wire evy-core / evy-policy / evy-providers / evy-scheduler /
// evy-comms / evy-memory per ADR 0020 in the parent subctl repo.
