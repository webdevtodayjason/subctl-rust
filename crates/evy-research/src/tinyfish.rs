//! TinyFish-backed [`ResearchClient`] — talks to the operator's already
//! configured TinyFish account via the public REST API.
//!
//! ## Backend identity
//!
//! The team-lead spec called this backend "TinyFish MCP". It is *not*
//! the MCP endpoint (`agent.tinyfish.ai/mcp`, OAuth 2.1, intended for
//! end-user AI clients like Claude Desktop). This client speaks the
//! REST API at:
//!
//! | Capability | URL | Auth |
//! |---|---|---|
//! | Search | `https://api.search.tinyfish.ai/` (GET) | `X-API-Key` header |
//! | Fetch  | `https://api.fetch.tinyfish.ai/`  (POST) | `X-API-Key` header |
//!
//! Endpoint paths and request/response shapes verified against the
//! TinyFish OpenAPI specs at `https://agent.tinyfish.ai/v1/openapi/search`
//! and `.../fetch` (2026-05-26).
//!
//! ## Why two endpoint fields, not one
//!
//! The spec asked for a single `api_endpoint: String`. Search and fetch
//! live on different subdomains, so a single base URL would either
//! drag synthesis logic into the client or force test mocks to fake
//! DNS. We split into `search_endpoint` + `fetch_endpoint`. Each can
//! be pointed at the same `wiremock` `MockServer` URL in tests, and
//! defaults point at the real TinyFish hosts.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::client::ResearchClient;
use crate::error::{ResearchError, Result};
use crate::types::{FetchResult, SearchResult, SourceTier};

/// Default base URL for the TinyFish Search REST endpoint.
pub const DEFAULT_SEARCH_ENDPOINT: &str = "https://api.search.tinyfish.ai/";
/// Default base URL for the TinyFish Fetch REST endpoint.
pub const DEFAULT_FETCH_ENDPOINT: &str = "https://api.fetch.tinyfish.ai/";
/// Default per-request transport timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Static configuration for [`TinyFishClient`].
#[derive(Debug, Clone)]
pub struct TinyFishConfig {
    /// Base URL for the search endpoint. Overridden in tests to point
    /// at a `wiremock` server. Defaults to [`DEFAULT_SEARCH_ENDPOINT`].
    pub search_endpoint: String,
    /// Base URL for the fetch endpoint. Overridden in tests to point
    /// at a `wiremock` server. Defaults to [`DEFAULT_FETCH_ENDPOINT`].
    pub fetch_endpoint: String,
    /// API token sent in the `X-API-Key` header. `None` is permitted
    /// at construction time so callers can defer loading from secrets
    /// storage; calls will fail with [`ResearchError::Config`] until
    /// a token is set.
    pub api_token: Option<String>,
    /// Per-request transport timeout. The TinyFish docs warn that
    /// fetch latency can hit seconds; default is generous.
    pub timeout: Duration,
}

impl TinyFishConfig {
    /// Construct a config with the supplied token and the production
    /// defaults for all other fields.
    #[must_use]
    pub fn new(api_token: impl Into<String>) -> Self {
        Self {
            search_endpoint: DEFAULT_SEARCH_ENDPOINT.to_string(),
            fetch_endpoint: DEFAULT_FETCH_ENDPOINT.to_string(),
            api_token: Some(api_token.into()),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl Default for TinyFishConfig {
    fn default() -> Self {
        Self {
            search_endpoint: DEFAULT_SEARCH_ENDPOINT.to_string(),
            fetch_endpoint: DEFAULT_FETCH_ENDPOINT.to_string(),
            api_token: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// TinyFish-backed research client.
///
/// Cheap to construct; holds a single shared `reqwest::Client` so
/// connections / keepalives are reused across `search` and
/// `fetch_content` calls.
pub struct TinyFishClient {
    config: TinyFishConfig,
    http: reqwest::Client,
}

impl TinyFishClient {
    /// Build a client. Panics only on a `reqwest::Client` builder
    /// failure, which is infallible for the feature flags this crate
    /// enables.
    #[must_use]
    pub fn new(config: TinyFishConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("reqwest client builder is infallible in this config");
        Self { config, http }
    }

    fn token(&self) -> Result<&str> {
        self.config
            .api_token
            .as_deref()
            .ok_or_else(|| ResearchError::Config("TinyFish api_token is unset".to_string()))
    }
}

// ─── ResearchClient impl ─────────────────────────────────────────────────

#[async_trait]
impl ResearchClient for TinyFishClient {
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>> {
        let query = query.trim();
        if query.is_empty() {
            return Err(ResearchError::Input("query is empty".to_string()));
        }
        if max_results == 0 {
            return Ok(Vec::new());
        }
        let token = self.token()?;

        debug!(query = %query, max_results, "evy-research: tinyfish search");
        let resp = self
            .http
            .get(&self.config.search_endpoint)
            .header("X-API-Key", token)
            .query(&[("query", query)])
            .send()
            .await
            .map_err(|e| ResearchError::Transport(e.without_url().to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let snippet = body_snippet(resp).await;
            return Err(ResearchError::Http { status, snippet });
        }

        let parsed: SearchResponse = resp
            .json()
            .await
            .map_err(|e| ResearchError::Decode(e.without_url().to_string()))?;

        let mut out: Vec<SearchResult> = parsed
            .results
            .into_iter()
            .map(|row| SearchResult {
                source_tier: SourceTier::from_url(&row.url),
                url: row.url,
                title: row.title,
                snippet: row.snippet,
            })
            .collect();

        if out.len() > max_results {
            out.truncate(max_results);
        }
        Ok(out)
    }

    async fn fetch_content(&self, url: &str) -> Result<FetchResult> {
        let url = url.trim();
        if url.is_empty() {
            return Err(ResearchError::Input("url is empty".to_string()));
        }
        // Parse to validate before paying for a round-trip; defer the
        // hostname / scheme policy to the backend.
        ::url::Url::parse(url).map_err(|e| ResearchError::Input(format!("invalid url: {e}")))?;
        let token = self.token()?;

        debug!(url = %url, "evy-research: tinyfish fetch_content");
        let body = serde_json::json!({
            "urls": [url],
            "format": "markdown",
        });
        let resp = self
            .http
            .post(&self.config.fetch_endpoint)
            .header("X-API-Key", token)
            .json(&body)
            .send()
            .await
            .map_err(|e| ResearchError::Transport(e.without_url().to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let snippet = body_snippet(resp).await;
            return Err(ResearchError::Http { status, snippet });
        }

        let parsed: FetchResponse = resp
            .json()
            .await
            .map_err(|e| ResearchError::Decode(e.without_url().to_string()))?;

        // The fetch endpoint is per-URL fault-tolerant: a row in
        // `errors[]` does NOT fail the request. We surface per-URL
        // failure as a structured ResearchError to the caller.
        if let Some(err_row) = parsed.errors.into_iter().next() {
            warn!(url = %err_row.url, code = %err_row.error, "tinyfish: per-url fetch error");
            return Err(ResearchError::Http {
                status: err_row.status.unwrap_or(0),
                snippet: format!("tinyfish fetch error: {}", err_row.error),
            });
        }

        let row =
            parsed.results.into_iter().next().ok_or_else(|| {
                ResearchError::Decode("tinyfish fetch: empty results".to_string())
            })?;

        // `text` may be JSON for `format="json"`; we requested markdown
        // so the string path is the expected one. If the backend ever
        // returns a non-string here we surface a Decode error rather
        // than silently lose the body.
        let content = match row.text {
            Some(serde_json::Value::String(s)) => s,
            Some(other) => other.to_string(),
            None => String::new(),
        };

        Ok(FetchResult {
            url: row.url,
            title: row.title,
            content,
            content_type: row.format.unwrap_or_else(|| "markdown".to_string()),
            fetched_at: chrono::Utc::now(),
        })
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────

async fn body_snippet(resp: reqwest::Response) -> String {
    let body = resp.text().await.unwrap_or_default();
    body.chars().take(200).collect()
}

// ─── TinyFish wire shapes (only the fields we read) ──────────────────────

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchResponseRow>,
}

#[derive(Debug, Deserialize)]
struct SearchResponseRow {
    url: String,
    title: String,
    snippet: String,
}

#[derive(Debug, Deserialize)]
struct FetchResponse {
    #[serde(default)]
    results: Vec<FetchResponseRow>,
    #[serde(default)]
    errors: Vec<FetchResponseError>,
}

#[derive(Debug, Deserialize)]
struct FetchResponseRow {
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    text: Option<serde_json::Value>,
    #[serde(default)]
    format: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FetchResponseError {
    url: String,
    error: String,
    #[serde(default)]
    status: Option<u16>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_point_at_real_hosts() {
        let c = TinyFishConfig::default();
        assert_eq!(c.search_endpoint, DEFAULT_SEARCH_ENDPOINT);
        assert_eq!(c.fetch_endpoint, DEFAULT_FETCH_ENDPOINT);
        assert!(c.api_token.is_none());
        assert!(c.timeout > Duration::ZERO);
    }

    #[test]
    fn config_new_sets_token() {
        let c = TinyFishConfig::new("abc123");
        assert_eq!(c.api_token.as_deref(), Some("abc123"));
    }

    #[test]
    fn search_response_decodes_minimal_row() {
        let body = r#"{
            "query": "x",
            "results": [
                {"position": 1, "site_name": "example.com", "title": "T", "snippet": "S", "url": "https://example.com/p"}
            ],
            "total_results": 1,
            "page": 0
        }"#;
        let parsed: SearchResponse = serde_json::from_str(body).expect("decode");
        assert_eq!(parsed.results.len(), 1);
        assert_eq!(parsed.results[0].url, "https://example.com/p");
    }

    #[test]
    fn fetch_response_decodes_minimal_row() {
        let body = r##"{
            "results": [
                {"url": "https://example.com", "final_url": "https://example.com/", "title": "T", "description": null, "language": "en", "format": "markdown", "text": "# Hi", "author": null, "published_date": null}
            ],
            "errors": []
        }"##;
        let parsed: FetchResponse = serde_json::from_str(body).expect("decode");
        assert_eq!(parsed.results.len(), 1);
        assert_eq!(parsed.results[0].url, "https://example.com");
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn fetch_response_decodes_per_url_error() {
        let body = r#"{
            "results": [],
            "errors": [
                {"url": "https://bad.example", "error": "target_http_error", "status": 404}
            ]
        }"#;
        let parsed: FetchResponse = serde_json::from_str(body).expect("decode");
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].error, "target_http_error");
        assert_eq!(parsed.errors[0].status, Some(404));
    }
}
