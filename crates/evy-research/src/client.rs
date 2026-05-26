//! [`ResearchClient`] trait — the abstract surface evy-research exposes
//! to the rest of the workspace.
//!
//! Concrete backends live in sibling modules (`tinyfish`, future
//! `mock`, etc.). Callers should hold a `Box<dyn ResearchClient>` (or
//! `Arc<dyn ResearchClient>`) so they can swap backends in tests
//! without touching the call sites.

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{FetchResult, SearchResult};

/// Web-research operations Evy v4 depends on.
///
/// The trait is `Send + Sync` so implementations can be shared across
/// tokio tasks (`Arc<dyn ResearchClient>` is the typical handle).
///
/// ## `max_results`
///
/// The TinyFish search endpoint does not expose a per-call result cap
/// — it returns up to a backend-default page size. Implementations
/// **must** truncate to `max_results` client-side before returning so
/// the caller's resource budget is honoured.
#[async_trait]
pub trait ResearchClient: Send + Sync {
    /// Run a web search and return up to `max_results` ranked rows.
    ///
    /// # Errors
    /// - [`crate::error::ResearchError::Transport`] on a network failure.
    /// - [`crate::error::ResearchError::Http`] on a non-2xx response.
    /// - [`crate::error::ResearchError::Decode`] when the response body
    ///   does not match the expected schema.
    /// - [`crate::error::ResearchError::Input`] when `query` is empty.
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>>;

    /// Fetch and extract the textual content of a single page.
    ///
    /// # Errors
    /// - [`crate::error::ResearchError::Transport`] on a network failure.
    /// - [`crate::error::ResearchError::Http`] on a non-2xx response.
    /// - [`crate::error::ResearchError::Decode`] when the response body
    ///   does not match the expected schema or returns an empty
    ///   `results` array.
    /// - [`crate::error::ResearchError::Input`] when `url` is malformed.
    async fn fetch_content(&self, url: &str) -> Result<FetchResult>;
}
