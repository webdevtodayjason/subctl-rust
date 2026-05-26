//! `evy-memory` — observation log, retrieval API, claude-mem consumer,
//! and the substrate for Evy v4's learning loop.
//!
//! The crate ships **layers 1, 2, 4, and a skeleton of layer 6** from
//! ADR 0020 §"The learning loop — seven layers". Layers 3, 5, and 7
//! (worker-effectiveness scoring, operator-preference model, feedback
//! ingest — the parts that close the loop) are deferred to Phase 3 of
//! the v4 build-out and are scaffolded with `// TODO: Phase 3` markers
//! in each module that will host them.
//!
//! Public surface:
//!
//! - [`Observation`] / [`ObservationKind`] — the append-only event unit.
//! - [`ObservationLog`] — sqlx-backed log, migrations applied on open.
//! - [`ClaudeMemReader`] / [`Episode`] — read-only consumer of the
//!   operator's claude-mem database.
//! - [`Playbook`] / [`PlaybookStore`] — markdown + YAML frontmatter,
//!   loaded from a configured directory.
//! - [`Retriever`] / [`NaiveRetriever`] / [`RetrievedItem`] — the
//!   decision-time substrate.
//!
//! See ADR 0020 for the architectural context this crate fits into.

pub mod claude_mem;
mod error;
pub mod observation;
pub mod observation_log;
pub mod playbook;
pub mod retrieval;

pub use claude_mem::{ClaudeMemReader, Episode};
pub use observation::{Observation, ObservationKind};
pub use observation_log::ObservationLog;
pub use playbook::{Playbook, PlaybookStore};
pub use retrieval::{NaiveRetriever, RetrievedItem, Retriever};
