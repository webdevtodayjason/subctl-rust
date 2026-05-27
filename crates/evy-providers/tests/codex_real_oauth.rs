//! Real Codex OAuth smoke test — operator runs this manually.
//!
//! `cargo test -p evy-providers --test codex_real_oauth -- --ignored --nocapture`
//!
//! What it does:
//!   1. Starts a fresh device-code flow against the real auth.openai.com
//!   2. Prints the user code + verification URL to stdout
//!   3. Polls until the operator completes the verification page
//!   4. Asserts the returned access_token decodes as a valid JWT with a
//!      future `exp`
//!
//! Not run in CI — needs a human to open the URL.

use evy_providers::oauth::codex::{decode_jwt_exp, CodexOauth, OPENAI_CODEX_CLIENT_ID};
use evy_providers::oauth::OauthFlow;

#[ignore = "real OAuth — operator runs manually"]
#[tokio::test]
async fn real_codex_device_flow() {
    let codex = CodexOauth::new(OPENAI_CODEX_CLIENT_ID.to_string());
    let prompt = codex.start_device_flow().await.expect("device flow start");
    eprintln!(
        "\n\n>>> Open this URL and enter the code:\n    URL:  {}\n    CODE: {}\n\n",
        prompt.verification_uri, prompt.user_code
    );
    let token = codex
        .poll_for_token(&prompt.device_code)
        .await
        .expect("poll completed");
    eprintln!("got access_token (len={})", token.token.len());
    // JWT exp claim must be in the future.
    let exp = decode_jwt_exp(&token.token).expect("token must be a JWT");
    let now = chrono::Utc::now().timestamp();
    assert!(exp > now, "exp {exp} must be in the future (now={now})");
}
