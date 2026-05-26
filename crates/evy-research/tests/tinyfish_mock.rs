//! Integration tests for [`evy_research::TinyFishClient`] against a
//! `wiremock` mock of the TinyFish REST endpoints. The real
//! `api.search.tinyfish.ai` and `api.fetch.tinyfish.ai` are NEVER hit
//! — `TinyFishConfig` exposes `search_endpoint` + `fetch_endpoint`
//! precisely so these tests can swap them.

use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use evy_research::{ResearchClient, ResearchError, SourceTier, TinyFishClient, TinyFishConfig};

const TOKEN: &str = "tinyfish-test-key";

fn cfg(server_url: &str) -> TinyFishConfig {
    let base = server_url.trim_end_matches('/').to_string();
    TinyFishConfig {
        // The two endpoints share the same mock server in tests; the
        // mocks match on method + path so search (GET /) and fetch
        // (POST /) don't collide.
        search_endpoint: format!("{base}/"),
        fetch_endpoint: format!("{base}/"),
        api_token: Some(TOKEN.to_string()),
        timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn search_returns_parsed_rows_with_inferred_source_tier() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/"))
        .and(header("X-API-Key", TOKEN))
        .and(query_param("query", "rust async runtime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "query": "rust async runtime",
            "total_results": 2,
            "page": 0,
            "results": [
                {
                    "position": 1,
                    "site_name": "Wikipedia",
                    "title": "Tokio (software)",
                    "snippet": "Tokio is an asynchronous runtime for Rust.",
                    "url": "https://en.wikipedia.org/wiki/Tokio_(software)"
                },
                {
                    "position": 2,
                    "site_name": "Reddit",
                    "title": "What's the best async runtime?",
                    "snippet": "Tokio is the default choice for most projects.",
                    "url": "https://www.reddit.com/r/rust/comments/abc/"
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = TinyFishClient::new(cfg(&server.uri()));
    let hits = client
        .search("rust async runtime", 10)
        .await
        .expect("search ok");

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].source_tier, SourceTier::Reference); // wikipedia
    assert_eq!(hits[1].source_tier, SourceTier::Forum); // reddit
    assert_eq!(
        hits[0].url,
        "https://en.wikipedia.org/wiki/Tokio_(software)"
    );
    assert_eq!(hits[1].title, "What's the best async runtime?");
}

#[tokio::test]
async fn search_truncates_to_max_results() {
    let server = MockServer::start().await;

    let many = (0..5)
        .map(|i| {
            json!({
                "position": i + 1,
                "site_name": "example.com",
                "title": format!("Result {i}"),
                "snippet": "...",
                "url": format!("https://example.com/{i}"),
            })
        })
        .collect::<Vec<_>>();

    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "query": "x",
            "total_results": 5,
            "page": 0,
            "results": many,
        })))
        .mount(&server)
        .await;

    let client = TinyFishClient::new(cfg(&server.uri()));
    let hits = client.search("x", 2).await.expect("search ok");
    assert_eq!(hits.len(), 2, "client must truncate to max_results");
}

#[tokio::test]
async fn search_empty_query_returns_input_error_without_network_call() {
    // No mock mounted — would panic on any HTTP call.
    let server = MockServer::start().await;
    let client = TinyFishClient::new(cfg(&server.uri()));

    let err = client.search("   ", 5).await.expect_err("must fail");
    assert!(matches!(err, ResearchError::Input(_)), "got: {err:?}");
}

#[tokio::test]
async fn search_zero_max_results_short_circuits() {
    let server = MockServer::start().await;
    // No mock — any HTTP call panics.
    let client = TinyFishClient::new(cfg(&server.uri()));
    let hits = client.search("rust", 0).await.expect("ok");
    assert!(hits.is_empty());
}

#[tokio::test]
async fn search_passes_api_key_header() {
    let server = MockServer::start().await;

    // The mock matches only when X-API-Key is present and correct;
    // wiremock 404s otherwise — which we turn into a clear assertion.
    Mock::given(method("GET"))
        .and(path("/"))
        .and(header("X-API-Key", TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "query": "x",
            "total_results": 0,
            "page": 0,
            "results": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = TinyFishClient::new(cfg(&server.uri()));
    let hits = client.search("x", 1).await.expect("ok");
    assert!(hits.is_empty());
}

#[tokio::test]
async fn search_surfaces_http_4xx() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let client = TinyFishClient::new(cfg(&server.uri()));
    let err = client.search("rust", 1).await.expect_err("must fail");
    match err {
        ResearchError::Http { status, snippet } => {
            assert_eq!(status, 401);
            assert!(snippet.contains("unauthorized"), "got: {snippet}");
        }
        other => panic!("expected Http, got {other:?}"),
    }
}

#[tokio::test]
async fn search_without_token_returns_config_error() {
    let server = MockServer::start().await;
    let mut c = cfg(&server.uri());
    c.api_token = None;
    let client = TinyFishClient::new(c);

    let err = client.search("rust", 1).await.expect_err("must fail");
    assert!(matches!(err, ResearchError::Config(_)), "got: {err:?}");
}

#[tokio::test]
async fn fetch_content_returns_parsed_row() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("X-API-Key", TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                {
                    "url": "https://example.com/article",
                    "final_url": "https://example.com/article/",
                    "title": "Article Title",
                    "description": "A brief description",
                    "language": "en",
                    "format": "markdown",
                    "text": "# Article Title\n\nBody.",
                    "author": "Jane Doe",
                    "published_date": "2026-01-15",
                    "latency_ms": 1430.39
                }
            ],
            "errors": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = TinyFishClient::new(cfg(&server.uri()));
    let page = client
        .fetch_content("https://example.com/article")
        .await
        .expect("fetch ok");

    assert_eq!(page.url, "https://example.com/article");
    assert_eq!(page.title.as_deref(), Some("Article Title"));
    assert!(page.content.contains("Article Title"));
    assert_eq!(page.content_type, "markdown");
    // fetched_at is "now-ish"; just check it's been set.
    assert!(page.fetched_at <= chrono::Utc::now());
}

#[tokio::test]
async fn fetch_content_surfaces_per_url_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [],
            "errors": [
                {
                    "url": "https://bad.example",
                    "error": "target_http_error",
                    "status": 404
                }
            ]
        })))
        .mount(&server)
        .await;

    let client = TinyFishClient::new(cfg(&server.uri()));
    let err = client
        .fetch_content("https://bad.example")
        .await
        .expect_err("must fail");
    match err {
        ResearchError::Http { status, snippet } => {
            assert_eq!(status, 404);
            assert!(snippet.contains("target_http_error"), "got: {snippet}");
        }
        other => panic!("expected Http, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_content_empty_results_returns_decode_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [],
            "errors": []
        })))
        .mount(&server)
        .await;

    let client = TinyFishClient::new(cfg(&server.uri()));
    let err = client
        .fetch_content("https://example.com")
        .await
        .expect_err("must fail");
    assert!(matches!(err, ResearchError::Decode(_)), "got: {err:?}");
}

#[tokio::test]
async fn fetch_content_invalid_url_returns_input_error_without_network_call() {
    let server = MockServer::start().await;
    let client = TinyFishClient::new(cfg(&server.uri()));
    let err = client
        .fetch_content("not a url")
        .await
        .expect_err("must fail");
    assert!(matches!(err, ResearchError::Input(_)), "got: {err:?}");
}

#[tokio::test]
async fn fetch_content_surfaces_http_5xx() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&server)
        .await;

    let client = TinyFishClient::new(cfg(&server.uri()));
    let err = client
        .fetch_content("https://example.com")
        .await
        .expect_err("must fail");
    match err {
        ResearchError::Http { status, snippet } => {
            assert_eq!(status, 503);
            assert!(snippet.contains("upstream"), "got: {snippet}");
        }
        other => panic!("expected Http, got {other:?}"),
    }
}
