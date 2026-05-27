//! `evy-memory` — the full learning-loop substrate for Evy v4.
//!
//! Phase 2 Slice C landed the substrate: an append-only observation log,
//! a read-only consumer of the operator's claude-mem corpus, a markdown
//! playbook store, and a naive retrieval API. Phase 3 Slice D closed
//! the loop by adding:
//!
//! - [`feedback`] — operator-correction ingest (layer 7).
//! - [`scoring`] — per-provider × per-task-class effectiveness ledger
//!   (layer 3).
//! - [`preferences`] — Evy-owned operator-preference model (layer 5).
//!
//! Phase 4 Slice E adds Tier 4 (Cognee) integration:
//!
//! - [`cognee`] — typed HTTP client for the local Cognee service.
//! - [`promotion`] — periodic Tier-3 → Tier-4 promotion loop.
//!
//! Retrieval ([`retrieval::NaiveRetriever`]) now interleaves all six
//! Tier-3 sources behind one `Retriever` trait; a Cognee-backed
//! retriever is a Phase 5 follow-up.
//!
//! Public surface, by learning-loop layer:
//!
//! | Layer | Type |
//! |---|---|
//! | 1. Observation log | [`Observation`], [`ObservationKind`], [`ObservationLog`] |
//! | 2. claude-mem | [`ClaudeMemReader`], [`Episode`] |
//! | 3. Scoring | [`WorkerEffectivenessScore`], [`ScoreLedger`] |
//! | 4. Playbooks | [`Playbook`], [`PlaybookStore`] |
//! | 5. Preferences | [`PreferenceKey`], [`PreferenceValue`], [`OperatorPreferenceModel`] |
//! | 6. Retrieval | [`Retriever`], [`NaiveRetriever`], [`RetrievedItem`] |
//! | 7. Feedback | [`Feedback`], [`FeedbackKind`], [`FeedbackContext`], [`FeedbackIngest`] |
//! | 8. Tier 4 (Cognee) | [`CogneeClient`], [`CogneeConfig`], [`CogneeMatch`], [`PromotionTicker`], [`TickReport`] |
//!
//! See ADR 0020 for the architectural context this crate fits into.

pub mod claude_mem;
pub mod cognee;
mod error;
pub mod feedback;
pub mod observation;
pub mod observation_log;
pub mod playbook;
pub mod preferences;
pub mod promotion;
pub mod retrieval;
pub mod scoring;

pub use claude_mem::{ClaudeMemReader, Episode};
pub use cognee::{CogneeClient, CogneeConfig, CogneeMatch};
pub use feedback::{Feedback, FeedbackContext, FeedbackIngest, FeedbackKind};
pub use observation::{Observation, ObservationKind};
pub use observation_log::ObservationLog;
pub use playbook::{Playbook, PlaybookStore};
pub use preferences::{OperatorPreferenceModel, PreferenceKey, PreferenceValue};
pub use promotion::{
    score_observation, PromotionTicker, TickReport, DEFAULT_THRESHOLD, SCORE_OPERATOR_MESSAGE,
    SCORE_POLICY_ALLOW, SCORE_WORKER_SUCCEEDED,
};
pub use retrieval::{NaiveRetriever, RetrievedItem, Retriever};
pub use scoring::{ScoreLedger, WorkerEffectivenessScore};
