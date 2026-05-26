//! `evy-research` — web search + page-fetch substrate for Evy v4.
//!
//! Phase 3 Slice C lands the [`ResearchClient`] trait plus a single
//! concrete backend ([`TinyFishClient`]) that talks to the operator's
//! TinyFish account via the REST API. The eventual thinking-partner
//! surface in `evy` will hold this behind `Arc<dyn ResearchClient>`,
//! pull in `evy-memory` for recall + dedup, and stop here for Slice C.
//!
//! # Out of scope (per ADR 0020)
//!
//! * Embedding search / vector recall — that's `evy-memory`.
//! * Crawler / link-following — single-call search + fetch only.
//! * Summarization — caller decides what to do with the content.
//!
//! # Quick start
//!
//! ```no_run
//! use std::sync::Arc;
//! use evy_research::{ResearchClient, TinyFishClient, TinyFishConfig};
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let client: Arc<dyn ResearchClient> = Arc::new(TinyFishClient::new(
//!     TinyFishConfig::new("…api key from secrets store…"),
//! ));
//!
//! let hits = client.search("rust async runtime", 5).await?;
//! for h in &hits {
//!     println!("{:?} {} — {}", h.source_tier, h.title, h.url);
//! }
//!
//! if let Some(first) = hits.first() {
//!     let page = client.fetch_content(&first.url).await?;
//!     println!("{} chars of {}", page.content.len(), page.content_type);
//! }
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

pub mod client;
pub mod error;
pub mod tinyfish;
pub mod types;

// ── Public re-exports — the surface daemon-side code consumes ──────────

pub use client::ResearchClient;
pub use error::{ResearchError, Result};
pub use tinyfish::{
    TinyFishClient, TinyFishConfig, DEFAULT_FETCH_ENDPOINT, DEFAULT_SEARCH_ENDPOINT,
    DEFAULT_TIMEOUT,
};
pub use types::{FetchResult, SearchQuery, SearchResult, SourceTier};
