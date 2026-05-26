//! Public data types returned by [`crate::client::ResearchClient`].
//!
//! `SearchResult` and `FetchResult` are wire-shape stable — they're
//! `Serialize` + `Deserialize` so the daemon can route them through
//! evy-memory's append-only log or expose them on the operator console
//! without a translation layer.

use serde::{Deserialize, Serialize};

/// Provenance tier inferred from the result URL.
///
/// The tier is a coarse hint the thinking-partner surface uses to weigh
/// evidence — a `.gov` page outranks a Medium post on the same question.
/// Heuristics are intentionally simple and host-based; the caller can
/// override (e.g., a curated allow-list of trusted blogs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceTier {
    /// Government, military, or academic first-party sources
    /// (`.gov`, `.edu`, `.mil`, `.ac.<tld>`).
    Primary,
    /// Encyclopedia / first-party documentation (`wikipedia.org`,
    /// hosts starting with `docs.` or `wiki.`).
    Reference,
    /// Mainstream news organizations.
    News,
    /// Blog platforms or personal blog domains.
    Blog,
    /// Q&A / discussion forums.
    Forum,
    /// Did not match any tier heuristic.
    Unknown,
}

impl SourceTier {
    /// Classify a URL into a tier using host-based heuristics.
    ///
    /// Returns [`SourceTier::Unknown`] on malformed URLs — callers
    /// validate URLs separately if they need to surface that error.
    ///
    /// Heuristics, in priority order:
    /// 1. `Primary` — host ends with `.gov`, `.edu`, `.mil`, or contains
    ///    `.ac.` (UK / AU academic).
    /// 2. `Reference` — `wikipedia.org`, host starts with `docs.` /
    ///    `wiki.`.
    /// 3. `News` — well-known mainstream news domains.
    /// 4. `Forum` — `reddit.com`, `news.ycombinator.com`,
    ///    `stackoverflow.com`, `stackexchange.com`, `lobste.rs`.
    /// 5. `Blog` — `medium.com`, `substack.com`, `blogspot.com`,
    ///    hosts ending with `.wordpress.com` or `.blog`.
    /// 6. `Unknown` otherwise.
    #[must_use]
    pub fn from_url(raw: &str) -> Self {
        let Ok(parsed) = url::Url::parse(raw) else {
            return Self::Unknown;
        };
        let Some(host) = parsed.host_str() else {
            return Self::Unknown;
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();

        // 1. Primary
        if host.ends_with(".gov")
            || host == "gov"
            || host.ends_with(".edu")
            || host == "edu"
            || host.ends_with(".mil")
            || host == "mil"
            || host.contains(".ac.")
        {
            return Self::Primary;
        }

        // 2. Reference
        if host == "wikipedia.org"
            || host.ends_with(".wikipedia.org")
            || host.starts_with("docs.")
            || host.starts_with("wiki.")
        {
            return Self::Reference;
        }

        // 3. News (closed allow-list — additions are deliberate).
        const NEWS_HOSTS: &[&str] = &[
            "nytimes.com",
            "washingtonpost.com",
            "bbc.com",
            "bbc.co.uk",
            "reuters.com",
            "ap.org",
            "apnews.com",
            "cnn.com",
            "theguardian.com",
            "npr.org",
            "wsj.com",
            "ft.com",
            "bloomberg.com",
            "economist.com",
        ];
        if NEWS_HOSTS
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}")))
        {
            return Self::News;
        }

        // 4. Forum (check before Blog so e.g. discourse.<blog-platform>
        // doesn't get pulled to Blog).
        const FORUM_HOSTS: &[&str] = &[
            "reddit.com",
            "news.ycombinator.com",
            "stackoverflow.com",
            "stackexchange.com",
            "lobste.rs",
        ];
        if FORUM_HOSTS
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}")))
        {
            return Self::Forum;
        }

        // 5. Blog
        if host == "medium.com"
            || host.ends_with(".medium.com")
            || host == "substack.com"
            || host.ends_with(".substack.com")
            || host == "blogspot.com"
            || host.ends_with(".blogspot.com")
            || host.ends_with(".wordpress.com")
            || host.ends_with(".blog")
            || host == "blog"
        {
            return Self::Blog;
        }

        Self::Unknown
    }
}

/// A single search-result row from the research backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResult {
    /// Result URL.
    pub url: String,
    /// Page title.
    pub title: String,
    /// Short snippet describing the result (from the backend).
    pub snippet: String,
    /// Inferred provenance tier — see [`SourceTier::from_url`].
    pub source_tier: SourceTier,
}

/// Extracted page content from the research backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FetchResult {
    /// Original URL passed to `fetch_content`.
    pub url: String,
    /// Page title, if extracted.
    pub title: Option<String>,
    /// Cleaned text content. Format determined by `content_type`
    /// (`"markdown"`, `"html"`, or `"json"`).
    pub content: String,
    /// Format identifier — passed through from the backend's `format`
    /// field (currently one of `"markdown"`, `"html"`, `"json"`).
    pub content_type: String,
    /// Synthesized client-side via `Utc::now()` at receive time. The
    /// backend does not return a fetched-at timestamp, so this is "when
    /// evy-research observed the content", not "when the page was last
    /// modified."
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

/// Search-query parameters. Reserved for future expansion (geo /
/// language / page) — for now the public trait takes the simpler
/// `(query, max_results)` form. Exposed so callers can pre-build
/// queries in a typed shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SearchQuery {
    /// Free-text query string.
    pub query: String,
    /// Optional country hint (ISO 3166-1 alpha-2, e.g. `"US"`).
    pub location: Option<String>,
    /// Optional language hint (ISO 639-1, e.g. `"en"`).
    pub language: Option<String>,
}

impl SearchQuery {
    /// Build a query from a bare string.
    #[must_use]
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            location: None,
            language: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_tier_classifies_gov() {
        assert_eq!(
            SourceTier::from_url("https://www.nasa.gov/about"),
            SourceTier::Primary
        );
        assert_eq!(
            SourceTier::from_url("https://cdc.gov/data"),
            SourceTier::Primary
        );
    }

    #[test]
    fn source_tier_classifies_edu_and_ac() {
        assert_eq!(
            SourceTier::from_url("https://mit.edu/research"),
            SourceTier::Primary
        );
        assert_eq!(
            SourceTier::from_url("https://www.ox.ac.uk/research"),
            SourceTier::Primary
        );
    }

    #[test]
    fn source_tier_classifies_wikipedia_and_docs() {
        assert_eq!(
            SourceTier::from_url("https://en.wikipedia.org/wiki/Rust"),
            SourceTier::Reference
        );
        assert_eq!(
            SourceTier::from_url("https://docs.rs/serde/latest/"),
            SourceTier::Reference
        );
        assert_eq!(
            SourceTier::from_url("https://wiki.archlinux.org/title/Pacman"),
            SourceTier::Reference
        );
    }

    #[test]
    fn source_tier_classifies_news() {
        assert_eq!(
            SourceTier::from_url("https://www.nytimes.com/section/world"),
            SourceTier::News
        );
        assert_eq!(
            SourceTier::from_url("https://bbc.co.uk/news/uk"),
            SourceTier::News
        );
        assert_eq!(
            SourceTier::from_url("https://apnews.com/article/x"),
            SourceTier::News
        );
    }

    #[test]
    fn source_tier_classifies_forums() {
        assert_eq!(
            SourceTier::from_url("https://www.reddit.com/r/rust"),
            SourceTier::Forum
        );
        assert_eq!(
            SourceTier::from_url("https://news.ycombinator.com/item?id=1"),
            SourceTier::Forum
        );
        assert_eq!(
            SourceTier::from_url("https://stackoverflow.com/questions/123"),
            SourceTier::Forum
        );
    }

    #[test]
    fn source_tier_classifies_blogs() {
        assert_eq!(
            SourceTier::from_url("https://medium.com/@alice/post"),
            SourceTier::Blog
        );
        assert_eq!(
            SourceTier::from_url("https://alice.substack.com/p/post"),
            SourceTier::Blog
        );
        assert_eq!(
            SourceTier::from_url("https://something.wordpress.com/2024/01"),
            SourceTier::Blog
        );
    }

    #[test]
    fn source_tier_falls_back_to_unknown() {
        assert_eq!(
            SourceTier::from_url("https://example.com/page"),
            SourceTier::Unknown
        );
        assert_eq!(SourceTier::from_url("not a url"), SourceTier::Unknown);
        // No host (data:, file:).
        assert_eq!(
            SourceTier::from_url("data:text/plain,hello"),
            SourceTier::Unknown
        );
    }

    #[test]
    fn source_tier_is_case_insensitive() {
        assert_eq!(
            SourceTier::from_url("https://Wikipedia.ORG/wiki/X"),
            SourceTier::Reference
        );
        assert_eq!(
            SourceTier::from_url("https://WWW.NYTIMES.COM/page"),
            SourceTier::News
        );
    }

    #[test]
    fn forum_check_precedes_blog() {
        // A reddit subdomain must not be misclassified as a blog even
        // if its label happens to contain blog-flavoured strings.
        assert_eq!(
            SourceTier::from_url("https://blog.reddit.com/announcements"),
            SourceTier::Forum
        );
    }

    #[test]
    fn search_query_new_is_minimal() {
        let q = SearchQuery::new("rust async");
        assert_eq!(q.query, "rust async");
        assert!(q.location.is_none());
        assert!(q.language.is_none());
    }

    #[test]
    fn search_result_roundtrips_json() {
        let r = SearchResult {
            url: "https://example.com".into(),
            title: "T".into(),
            snippet: "S".into(),
            source_tier: SourceTier::Unknown,
        };
        let s = serde_json::to_string(&r).expect("ser");
        let back: SearchResult = serde_json::from_str(&s).expect("de");
        assert_eq!(r, back);
    }

    #[test]
    fn fetch_result_roundtrips_json() {
        let r = FetchResult {
            url: "https://example.com".into(),
            title: Some("Example".into()),
            content: "# Hello".into(),
            content_type: "markdown".into(),
            fetched_at: chrono::Utc::now(),
        };
        let s = serde_json::to_string(&r).expect("ser");
        let back: FetchResult = serde_json::from_str(&s).expect("de");
        assert_eq!(r, back);
    }
}
