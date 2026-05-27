//! Wiremock integration tests for the Codex OAuth device-flow + refresh.
//!
//! These tests never touch `auth.openai.com` — we point the [`CodexOauth`]
//! client at a wiremock server and assert that:
//!   - the device-code request hits `/api/accounts/deviceauth/usercode`
//!     with the expected JSON body
//!   - poll → exchange → access_token round-trip lands a `AccessToken`
//!     with a sane `expires_at`
//!   - refresh against `/oauth/token` with `grant_type=refresh_token`
//!     succeeds and surfaces a fresh token
//!   - HTTP error bodies are propagated into `Error::Provider { kind:
//!     Codex, ... }` with a sanitized reason

use chrono::Utc;
use evy_providers::oauth::{CodexOauth, OauthFlow};
use wiremock::matchers::{body_json, body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn start_device_flow_returns_parsed_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .and(body_json(serde_json::json!({ "client_id": "test-client" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "device_auth_id": "DAID-123",
            "user_code": "ABCD-1234",
            "interval": 3
        })))
        .mount(&server)
        .await;

    let codex = CodexOauth::with_base_url("test-client".to_string(), server.uri());
    let resp = codex.start_device_flow().await.unwrap();
    assert_eq!(resp.device_code, "DAID-123");
    assert_eq!(resp.user_code, "ABCD-1234");
    assert_eq!(resp.interval, 3);
    assert!(resp.verification_uri.contains("auth.openai.com"));
}

#[tokio::test]
async fn start_device_flow_propagates_http_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream blew up"))
        .mount(&server)
        .await;

    let codex = CodexOauth::with_base_url("test-client".to_string(), server.uri());
    let err = codex.start_device_flow().await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("500"), "expected status in error: {msg}");
}

#[tokio::test]
async fn poll_for_token_completes_after_one_pending_then_success() {
    let server = MockServer::start().await;

    // First poll returns 404 (operator hasn't verified yet) — but with
    // `up_to_n_times(1)` so the second poll falls through.
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(404).set_body_string("pending"))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    // Second poll returns the authorization_code + code_verifier.
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "authorization_code": "AC-789",
            "code_verifier": "VER-XYZ"
        })))
        .mount(&server)
        .await;

    // Exchange returns the tokens.
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=AC-789"))
        .and(body_string_contains("code_verifier=VER-XYZ"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "AT-final",
            "refresh_token": "RT-final",
            "expires_in": 600
        })))
        .mount(&server)
        .await;

    let codex = CodexOauth::with_base_url("test-client".to_string(), server.uri());
    // The polling loop sleeps 5s between attempts; for the test we tolerate
    // that by running the call with the default tokio runtime — the second
    // 200 lands on the first retry. Total wall time ≈ 5s.
    let tok = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        codex.poll_for_token("DAID-123"),
    )
    .await
    .expect("poll did not return in 15s")
    .unwrap();
    assert_eq!(tok.token, "AT-final");
    // `expires_at` should be ~600s in the future.
    let delta = (tok.expires_at - Utc::now()).num_seconds();
    assert!(delta > 500 && delta <= 600, "expected ~600s, got {delta}");
}

#[tokio::test]
async fn refresh_returns_new_access_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=RT-old"))
        .and(body_string_contains("client_id=test-client"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "AT-rotated",
            "refresh_token": "RT-rotated",
            "expires_in": 1200
        })))
        .mount(&server)
        .await;

    let codex = CodexOauth::with_base_url("test-client".to_string(), server.uri());
    let tok = codex.refresh("RT-old").await.unwrap();
    assert_eq!(tok.token, "AT-rotated");
    let delta = (tok.expires_at - Utc::now()).num_seconds();
    assert!(
        delta > 1100 && delta <= 1200,
        "expected ~1200s, got {delta}"
    );
}

#[tokio::test]
async fn refresh_propagates_400_as_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string(r#"{"error":"invalid_grant","error_description":"expired"}"#),
        )
        .mount(&server)
        .await;

    let codex = CodexOauth::with_base_url("test-client".to_string(), server.uri());
    let err = codex.refresh("RT-stale").await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("400"), "expected 400 in error: {msg}");
}
