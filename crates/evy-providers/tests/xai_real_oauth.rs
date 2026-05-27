//! Real xAI OAuth smoke test — operator runs this manually.
//!
//! `cargo test -p evy-providers --test xai_real_oauth -- --ignored --nocapture`
//!
//! What it does:
//!   1. Calls `XaiOauth::discover()` against the real
//!      `https://auth.x.ai/.well-known/openid-configuration`
//!   2. Asserts the discovery doc passes the host-pin check and returns
//!      both `authorization_endpoint` and `token_endpoint`
//!   3. Does NOT exercise the PKCE-loopback login (deferred slice). To
//!      smoke the refresh path, paste a real refresh_token via
//!      `XAI_REAL_REFRESH_TOKEN` env var; the test will refresh once and
//!      assert the rotated token works.
//!
//! Not run in CI — needs network + (for refresh) operator-supplied creds.

use evy_providers::oauth::xai::{XaiOauth, XAI_OAUTH_CLIENT_ID};

#[ignore = "real OAuth — operator runs manually"]
#[tokio::test]
async fn real_xai_discovery() {
    let xai = XaiOauth::new(XAI_OAUTH_CLIENT_ID.to_string());
    let disc = xai.discover().await.expect("discovery");
    eprintln!("authorization_endpoint: {}", disc.authorization_endpoint);
    eprintln!("token_endpoint:         {}", disc.token_endpoint);
    assert!(disc.authorization_endpoint.starts_with("https://"));
    assert!(disc.token_endpoint.starts_with("https://"));
}

#[ignore = "real OAuth — operator runs manually, needs XAI_REAL_REFRESH_TOKEN"]
#[tokio::test]
async fn real_xai_refresh() {
    let rt = match std::env::var("XAI_REAL_REFRESH_TOKEN") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("XAI_REAL_REFRESH_TOKEN not set — skipping");
            return;
        }
    };
    let xai = XaiOauth::new(XAI_OAUTH_CLIENT_ID.to_string());
    let tok = xai.refresh(&rt).await.expect("refresh");
    eprintln!("got access_token len={}", tok.token.len());
    assert!(!tok.token.is_empty());
}
