//! `Mandate` — the provider-agnostic work order.
//!
//! Every dispatched task is described by a `Mandate`. Providers translate
//! it into their native envelope; the scheduler stores it verbatim;
//! `evy-memory` may snapshot it for the learning loop.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{PolicyMode, ProviderKind};

/// Opaque identifier for a mandate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MandateId(pub Uuid);

impl MandateId {
    /// Mint a fresh v4 UUID-backed mandate id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for MandateId {
    fn default() -> Self {
        Self::new()
    }
}

/// Provider-agnostic work order.
///
/// Fields mirror the team-protocol mandate shape used by the v3 TS
/// daemon: goal / context / deliverable / done_when / constraints. The
/// envelope also carries dispatch hints (`provider`, `policy_mode`,
/// `timeout`) and free-form `metadata` for adapters that need extra
/// per-provider context without expanding the trait surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mandate {
    /// Stable id assigned at mandate creation.
    pub id: MandateId,
    /// Which provider this mandate is targeted at.
    pub provider: ProviderKind,
    /// One-line operator-visible goal.
    pub goal: String,
    /// Free-form context the worker needs to start.
    pub context: String,
    /// What the worker must produce.
    pub deliverable: String,
    /// Acceptance criteria, one per entry; all must be satisfied.
    pub done_when: Vec<String>,
    /// Hard constraints (paths the worker may not touch, etc.).
    pub constraints: Vec<String>,
    /// How the dispatch is gated.
    pub policy_mode: PolicyMode,
    /// Optional dispatch timeout; absence means "provider default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
    /// Provider-specific hints (model id, account, profile, etc.).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Mandate {
        let mut metadata = HashMap::new();
        metadata.insert("model".to_owned(), "claude-opus-4.7".to_owned());
        Mandate {
            id: MandateId::new(),
            provider: ProviderKind::ClaudeCode,
            goal: "ship slice A".to_owned(),
            context: "phase 1 bootstrap".to_owned(),
            deliverable: "evy-core types".to_owned(),
            done_when: vec!["cargo test passes".to_owned()],
            constraints: vec!["no anyhow in lib".to_owned()],
            policy_mode: PolicyMode::Gated,
            timeout: Some(Duration::from_secs(900)),
            metadata,
        }
    }

    #[test]
    fn fresh_ids_are_unique() {
        let a = MandateId::new();
        let b = MandateId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn default_is_new() {
        let a = MandateId::default();
        let b = MandateId::default();
        assert_ne!(a, b);
    }

    #[test]
    fn mandate_roundtrips_through_serde_json() {
        let m = sample();
        let json = serde_json::to_string(&m).expect("serialize");
        let back: Mandate = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.id, m.id);
        assert_eq!(back.provider, m.provider);
        assert_eq!(back.goal, m.goal);
        assert_eq!(back.context, m.context);
        assert_eq!(back.deliverable, m.deliverable);
        assert_eq!(back.done_when, m.done_when);
        assert_eq!(back.constraints, m.constraints);
        assert_eq!(back.policy_mode, m.policy_mode);
        assert_eq!(back.timeout, m.timeout);
        assert_eq!(back.metadata, m.metadata);
    }

    #[test]
    fn optional_fields_omitted_when_empty() {
        let mut m = sample();
        m.timeout = None;
        m.metadata.clear();
        let json = serde_json::to_string(&m).expect("serialize");
        assert!(!json.contains("\"timeout\""), "timeout should be skipped");
        assert!(!json.contains("\"metadata\""), "metadata should be skipped");
    }
}
