//! HTTP client for the daemon's chat endpoint.
//!
//! Mirrors the wire shape declared in `evy-comms::chat`. We deliberately
//! re-declare the request/response types here (rather than importing
//! evy-comms) so the TUI binary doesn't pull axum / serenity / etc. into
//! its dependency graph — matches the precedent set by `evy-tui::api`.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// JSON body POSTed to `/api/evy/chat`. Schema-equivalent to
/// `evy_comms::ChatRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    /// `None` opens a new session with `message` as the topic.
    #[serde(default)]
    pub session_id: Option<Uuid>,
    /// Operator text — non-empty.
    pub message: String,
}

/// JSON body returned on success. Schema-equivalent to
/// `evy_comms::ChatResponse`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    /// Stable session id. Echo this back on the next request to
    /// continue the conversation.
    pub session_id: Uuid,
    /// Evy's reply text. Verbatim from the LLM.
    pub response: String,
    /// Skill names the model could see this turn. May be empty.
    #[serde(default)]
    pub skills_loaded: Vec<String>,
}

/// Error variants surfaced from the daemon (mirrors evy-comms::ChatError).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub struct WireError {
    /// Body text or extra fields ignored — we only care about the
    /// tagged variant + a free-form `message` field.
    #[serde(default)]
    pub message: String,
}

/// Library-side error from the API client.
#[derive(Debug, Error)]
pub enum ApiError {
    /// Transport-level failure (connect, dns, tls).
    #[error("transport: {0}")]
    Transport(String),

    /// Server returned non-2xx. `body` is best-effort.
    #[error("http {status}: {body}")]
    HttpStatus {
        /// HTTP status code.
        status: u16,
        /// Best-effort response body snippet.
        body: String,
    },

    /// Response body did not parse as expected JSON.
    #[error("decode: {0}")]
    Decode(String),

    /// Construction error (bad base url, etc.).
    #[error("config: {0}")]
    Config(String),
}

/// Cheap-to-clone HTTP client. Holds a single `reqwest::Client` for
/// connection reuse.
#[derive(Debug, Clone)]
pub struct ApiClient {
    base: String,
    http: Client,
}

impl ApiClient {
    /// Build a client pointed at `base_url` (e.g.
    /// `"http://127.0.0.1:8787"`). Per-request timeout defaults to 120s
    /// to accommodate slow LLM turns under load — start_session can
    /// take 10-30s on a cold path.
    ///
    /// # Errors
    /// Returns [`ApiError::Config`] only on a `reqwest::Client::builder`
    /// failure, which is infallible for the feature flags we enable;
    /// the error path exists for future-proofing.
    pub fn new(base_url: &str) -> Result<Self, ApiError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| ApiError::Config(e.to_string()))?;
        Ok(Self {
            base: base_url.trim_end_matches('/').to_string(),
            http,
        })
    }

    /// POST one chat turn. `session_id == None` opens a new session;
    /// `Some` appends to an existing one.
    ///
    /// # Errors
    /// Any of [`ApiError`]'s variants — see the type docs.
    pub async fn send(
        &self,
        session_id: Option<Uuid>,
        message: String,
    ) -> Result<ChatResponse, ApiError> {
        let url = format!("{}/api/evy/chat", self.base);
        let body = ChatRequest {
            session_id,
            message,
        };
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }
        resp.json::<ChatResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))
    }

    /// Base URL the client targets — exposed for the status line.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn send_round_trips_a_response() {
        let server = MockServer::start().await;
        let resp_body = json!({
            "session_id": "00000000-0000-0000-0000-000000000001",
            "response": "hello operator",
            "skills_loaded": ["plan", "debug"]
        });
        Mock::given(method("POST"))
            .and(path("/api/evy/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(resp_body))
            .mount(&server)
            .await;

        let client = ApiClient::new(&server.uri()).expect("client");
        let reply = client.send(None, "hi".to_string()).await.expect("send ok");
        assert_eq!(reply.response, "hello operator");
        assert_eq!(reply.skills_loaded, vec!["plan", "debug"]);
    }

    #[tokio::test]
    async fn send_surfaces_503_when_partner_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/evy/chat"))
            .respond_with(
                ResponseTemplate::new(503)
                    .set_body_json(json!({"kind":"unavailable","message":"no partner"})),
            )
            .mount(&server)
            .await;

        let client = ApiClient::new(&server.uri()).expect("client");
        let err = client
            .send(None, "hi".to_string())
            .await
            .expect_err("should fail");
        match err {
            ApiError::HttpStatus { status, .. } => assert_eq!(status, 503),
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    #[test]
    fn client_strips_trailing_slash() {
        let c = ApiClient::new("http://example.com/").expect("client");
        assert_eq!(c.base(), "http://example.com");
    }

    #[test]
    fn chat_request_serializes_with_null_session_id() {
        let r = ChatRequest {
            session_id: None,
            message: "x".into(),
        };
        let s = serde_json::to_string(&r).expect("serialize");
        assert!(s.contains("\"session_id\":null"));
    }
}
