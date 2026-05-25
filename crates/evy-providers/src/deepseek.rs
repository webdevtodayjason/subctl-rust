//! DeepSeek V4 Pro adapter — Phase-1 stub.
//!
//! ADR 0020 (Phase-deferred items, "DeepSeek V4 Pro provider wire
//! format") explicitly defers the real implementation to Phase 2 once
//! the operator has hands-on wire-format details. Phase 1 ships this
//! stub so the [`Provider`] trait is exercised against three providers
//! from day one — successfully extending it to the real implementation
//! later is the abstraction's correctness test.
//!
//! Every fallible method returns
//! [`Error::Provider { kind: DeepSeek, … }`] with the expected
//! "not implemented in Phase 1" reason. No tmux spawn, no network.

use async_trait::async_trait;
use evy_core::{Error, Mandate, Provider, ProviderKind, Result, WorkerHandle};

/// Phase-1 stub provider. See module docs.
#[derive(Debug, Default)]
pub struct DeepSeekProvider;

impl DeepSeekProvider {
    /// Construct the stub. There's no state to configure in Phase 1 —
    /// the real `DeepSeekConfig` lands in Phase 2 with the sibling ADR.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Provider for DeepSeekProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::DeepSeek
    }

    async fn dispatch(&self, _mandate: &Mandate) -> Result<Box<dyn WorkerHandle>> {
        Err(Error::Provider {
            kind: ProviderKind::DeepSeek,
            reason: "not implemented in Phase 1 — see ADR 0020 Phase-deferred items".to_string(),
        })
    }

    async fn healthcheck(&self) -> Result<()> {
        Err(Error::Provider {
            kind: ProviderKind::DeepSeek,
            reason: "not implemented in Phase 1".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evy_core::{MandateId, PolicyMode};
    use std::collections::HashMap;

    fn dummy_mandate() -> Mandate {
        Mandate {
            id: MandateId::new(),
            provider: ProviderKind::DeepSeek,
            goal: "ignored".into(),
            context: "ignored".into(),
            deliverable: "ignored".into(),
            done_when: vec![],
            constraints: vec![],
            policy_mode: PolicyMode::Trusted,
            timeout: None,
            metadata: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn dispatch_returns_phase_one_error() {
        // `Box<dyn WorkerHandle>` is not `Debug`, so `expect_err` is
        // unavailable. Unwrap the result by hand.
        let p = DeepSeekProvider::new();
        match p.dispatch(&dummy_mandate()).await {
            Err(Error::Provider { kind, reason }) => {
                assert_eq!(kind, ProviderKind::DeepSeek);
                assert!(
                    reason.contains("not implemented in Phase 1"),
                    "reason should mention Phase 1: {reason}"
                );
                assert!(
                    reason.contains("ADR 0020"),
                    "reason should cite the deferring ADR: {reason}"
                );
            }
            Err(other) => panic!("expected Error::Provider, got {other:?}"),
            Ok(_) => panic!("Phase 1 stub must reject dispatch"),
        }
    }

    #[tokio::test]
    async fn healthcheck_returns_phase_one_error() {
        let p = DeepSeekProvider::new();
        match p.healthcheck().await {
            Err(Error::Provider { kind, .. }) => assert_eq!(kind, ProviderKind::DeepSeek),
            Err(other) => panic!("expected Error::Provider, got {other:?}"),
            Ok(()) => panic!("Phase 1 stub must reject healthcheck"),
        }
    }

    #[test]
    fn default_constructs_a_provider() {
        // Smoke check on the `Default` derive — the stub has no state
        // today, but if a future Phase-2 expansion adds fields we want
        // a `Default` impl that compiles + runs.
        let p = DeepSeekProvider;
        assert_eq!(p.kind(), ProviderKind::DeepSeek);
    }
}
