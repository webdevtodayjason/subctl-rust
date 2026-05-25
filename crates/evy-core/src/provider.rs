//! `Provider` trait — the provider-agnostic dispatch surface.
//!
//! A `Provider` translates a [`Mandate`] into a provider-specific worker
//! and returns an opaque [`WorkerHandle`] the caller can poll, cancel,
//! and wait on. Implementations live in `evy-providers`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{Mandate, Result, WorkerHandle};

/// Which provider an adapter speaks for.
///
/// Pinned at the v4.0 launch set; extend by adding a variant (additive,
/// non-breaking for downstream callers that match exhaustively only when
/// they need to).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderKind {
    /// Anthropic Claude Code via the `claude-teams` spawn pipeline.
    ClaudeCode,
    /// OpenAI Codex via the `codex-teams` spawn pipeline.
    Codex,
    /// DeepSeek V4 Pro. Phase 2 — Phase 1 ships a stub adapter.
    DeepSeek,
}

/// Provider-agnostic dispatch surface.
///
/// Implementations own provider-specific session state (tmux pane,
/// process, queue ID, etc.) and are responsible for translating a
/// `Mandate` into the native envelope of their provider.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Which provider this adapter speaks for.
    fn kind(&self) -> ProviderKind;

    /// Translate `mandate` into a native envelope and start a worker.
    ///
    /// # Errors
    /// Returns [`crate::Error::Provider`] if the provider rejects the
    /// envelope or the underlying transport fails, or
    /// [`crate::Error::PolicyViolation`] when surfaced by the policy gate.
    async fn dispatch(&self, mandate: &Mandate) -> Result<Box<dyn WorkerHandle>>;

    /// Cheap liveness probe. Should not dispatch real work.
    ///
    /// # Errors
    /// Returns [`crate::Error::Provider`] when the underlying transport,
    /// credential, or session is unhealthy.
    async fn healthcheck(&self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip() {
        for kind in [
            ProviderKind::ClaudeCode,
            ProviderKind::Codex,
            ProviderKind::DeepSeek,
        ] {
            let s = serde_json::to_string(&kind).expect("serialize");
            let back: ProviderKind = serde_json::from_str(&s).expect("deserialize");
            assert_eq!(kind, back);
        }
    }
}
