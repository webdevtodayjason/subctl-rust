//! Real-TinyFish smoke test. Stays `#[ignore]`'d so CI never hits the
//! live endpoints; operators run it manually with credentials:
//!
//! ```bash
//! TINYFISH_API_KEY=<key> cargo test -p evy-research \
//!     --test tinyfish_real -- --ignored --nocapture
//! ```
//!
//! Asserts only that the response decodes — content varies day to day.

use std::time::Duration;

use evy_research::{ResearchClient, TinyFishClient, TinyFishConfig};

fn live_client() -> Option<TinyFishClient> {
    let token = std::env::var("TINYFISH_API_KEY").ok()?;
    if token.trim().is_empty() {
        return None;
    }
    let mut cfg = TinyFishConfig::new(token);
    cfg.timeout = Duration::from_secs(60);
    Some(TinyFishClient::new(cfg))
}

#[tokio::test]
#[ignore = "real TinyFish API — run manually with TINYFISH_API_KEY set"]
async fn real_search_decodes() {
    let client = live_client().expect("TINYFISH_API_KEY must be set for this test");
    let hits = client
        .search("rust programming language", 3)
        .await
        .expect("real search must succeed");
    assert!(!hits.is_empty(), "expected at least one result");
    for h in &hits {
        assert!(!h.url.is_empty());
        assert!(!h.title.is_empty());
    }
}

#[tokio::test]
#[ignore = "real TinyFish API — run manually with TINYFISH_API_KEY set"]
async fn real_fetch_decodes() {
    let client = live_client().expect("TINYFISH_API_KEY must be set for this test");
    let page = client
        .fetch_content("https://example.com/")
        .await
        .expect("real fetch must succeed");
    assert!(!page.url.is_empty());
    assert!(
        !page.content.is_empty() || page.title.is_some(),
        "expected either content or a title"
    );
}
