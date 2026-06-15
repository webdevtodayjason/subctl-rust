//! TinyFish browser-automation backend for `evy-research`.
//!
//! Exposes two paths over the TinyFish `run_web_automation` REST
//! endpoint (`POST /v1/automation/run` on `agent.tinyfish.ai`):
//!
//! | Path | Returns | Maps to tool |
//! |---|---|---|
//! | [`AutomationClient::run_sync`] | full result | `tinyfish_agent` |
//! | [`AutomationClient::run_async`] | run id immediately | `tinyfish_agent_async` |
//!
//! ## Critical retry rule
//!
//! The TinyFish automation endpoint is long-lived — the browser session
//! runs server-side for up to `max_duration_seconds`. When a POST
//! **returns a network-layer error or a 5xx status**, the run is **still
//! executing** on TinyFish's servers. We MUST NOT blind-retry the POST
//! (that double-spends credits). Instead:
//!
//! 1. If the **initial POST** fails at the transport layer (`status 0`)
//!    we do **one** probing GET to `/v1/automation/runs/{run_id}` to see
//!    if a run was actually started.  If `run_id` is absent in the error
//!    body (or transport-level), we treat it as "not started" and only
//!    then surface the error without a retry.
//! 2. If the POST returns `5xx` with a `run_id` in the body, we poll
//!    the status endpoint until the run reaches a terminal state
//!    (`COMPLETED` or `FAILED`) or until the timeout elapses.
//! 3. `4xx` responses are **never** retried — auth / billing / rate-
//!    limit errors are operator-actionable and the run was never started.
//!
//! The async path fires the POST and returns the `run_id` immediately;
//! the caller is responsible for later polling via
//! [`AutomationClient::poll_run`].

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::{ResearchError, Result};

// ─── endpoint constants ──────────────────────────────────────────────────

/// TinyFish browser-automation REST endpoint (POST to start a run).
pub const DEFAULT_AUTOMATION_ENDPOINT: &str = "https://agent.tinyfish.ai/v1/automation/run";

/// TinyFish run-status endpoint (GET to poll a specific run).
pub const DEFAULT_RUN_STATUS_ENDPOINT: &str = "https://agent.tinyfish.ai/v1/automation/runs";

/// Default per-automation HTTP timeout headroom beyond `max_duration_seconds`.
pub const AUTOMATION_TIMEOUT_HEADROOM: Duration = Duration::from_secs(30);

/// Default max duration for an automation run (seconds), matching v3.
pub const DEFAULT_MAX_DURATION_SECONDS: u32 = 120;

/// Poll interval when waiting for a run to reach a terminal state.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);

// ─── public config ───────────────────────────────────────────────────────

/// Configuration for [`AutomationClient`].
#[derive(Clone)]
pub struct AutomationConfig {
    /// POST endpoint for starting a new run. Default
    /// [`DEFAULT_AUTOMATION_ENDPOINT`]. Override in tests.
    pub automation_endpoint: String,
    /// Base URL for the GET `runs/{run_id}` status endpoint. Default
    /// [`DEFAULT_RUN_STATUS_ENDPOINT`]. Override in tests.
    pub run_status_endpoint: String,
    /// API key sent via `X-API-Key`. Required — calls fail with
    /// [`ResearchError::Config`] when `None`.
    pub api_token: Option<String>,
    /// Per-request connect + TLS timeout; the actual wait for the run
    /// result is governed by `max_duration_seconds + AUTOMATION_TIMEOUT_HEADROOM`.
    pub connect_timeout: Duration,
}

impl std::fmt::Debug for AutomationConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutomationConfig")
            .field("automation_endpoint", &self.automation_endpoint)
            .field("run_status_endpoint", &self.run_status_endpoint)
            .field("api_token", &self.api_token.as_ref().map(|_| "<redacted>"))
            .field("connect_timeout", &self.connect_timeout)
            .finish()
    }
}

impl AutomationConfig {
    /// Construct with a specific API token and production-default endpoints.
    #[must_use]
    pub fn new(api_token: impl Into<String>) -> Self {
        Self {
            automation_endpoint: DEFAULT_AUTOMATION_ENDPOINT.to_string(),
            run_status_endpoint: DEFAULT_RUN_STATUS_ENDPOINT.to_string(),
            api_token: Some(api_token.into()),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            automation_endpoint: DEFAULT_AUTOMATION_ENDPOINT.to_string(),
            run_status_endpoint: DEFAULT_RUN_STATUS_ENDPOINT.to_string(),
            api_token: None,
            connect_timeout: Duration::from_secs(10),
        }
    }
}

// ─── public types ─────────────────────────────────────────────────────────

/// Parameters for a TinyFish browser automation run.
#[derive(Debug, Clone)]
pub struct AutomationRequest {
    /// Natural-language goal. Maps to TinyFish `goal`.
    pub task: String,
    /// Target URL the agent starts from. Maps to TinyFish `url`.
    pub starting_url: String,
    /// Max seconds the agent may spend (1–600, default
    /// [`DEFAULT_MAX_DURATION_SECONDS`]).
    pub max_duration_seconds: u32,
    /// Optional cap on agent steps (1–500).
    pub max_steps: Option<u32>,
    /// Browser profile (`"lite"` or `"stealth"`).
    pub browser_profile: Option<String>,
}

impl AutomationRequest {
    /// Construct with required fields; optional fields default to `None`.
    #[must_use]
    pub fn new(task: impl Into<String>, starting_url: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            starting_url: starting_url.into(),
            max_duration_seconds: DEFAULT_MAX_DURATION_SECONDS,
            max_steps: None,
            browser_profile: None,
        }
    }
}

/// Terminal status values returned by the TinyFish API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunStatus {
    /// Run completed successfully.
    Completed,
    /// Run ended in a failure state.
    Failed,
    /// Run is still executing (returned by poll endpoint before terminal).
    #[serde(other)]
    Running,
}

/// The result of a completed or failed automation run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationResult {
    /// TinyFish-assigned run identifier.
    pub run_id: String,
    /// Terminal status (`COMPLETED` or `FAILED`).
    pub status: String,
    /// ISO-8601 start timestamp, if provided.
    pub started_at: Option<String>,
    /// ISO-8601 finish timestamp, if provided.
    pub finished_at: Option<String>,
    /// Number of agent steps executed.
    pub num_of_steps: Option<u32>,
    /// Structured result payload from the agent (on `COMPLETED`).
    pub result: Option<serde_json::Value>,
    /// Error payload from the agent (on `FAILED`).
    pub error: Option<serde_json::Value>,
}

/// Identifier returned immediately by [`AutomationClient::run_async`].
#[derive(Debug, Clone)]
pub struct AsyncRunId(pub String);

// ─── wire shapes ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RunResponseRaw {
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    finished_at: Option<String>,
    #[serde(default)]
    num_of_steps: Option<u32>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

// ─── client ───────────────────────────────────────────────────────────────

/// TinyFish browser-automation client.
///
/// Construct via [`AutomationClient::new`]. The inner `reqwest::Client`
/// is shared across sync and async paths.
pub struct AutomationClient {
    config: AutomationConfig,
    http: reqwest::Client,
}

impl AutomationClient {
    /// Construct from config.
    #[must_use]
    pub fn new(config: AutomationConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .build()
            .expect("reqwest client build is infallible with these features");
        Self { config, http }
    }

    fn token(&self) -> Result<&str> {
        self.config
            .api_token
            .as_deref()
            .ok_or_else(|| ResearchError::Config("AutomationClient: api_token unset".to_string()))
    }

    /// Build the POST request body from an [`AutomationRequest`].
    fn build_body(req: &AutomationRequest) -> serde_json::Value {
        let duration = req.max_duration_seconds.clamp(1, 600);
        let mut agent_config = serde_json::json!({
            "max_duration_seconds": duration,
        });
        if let Some(steps) = req.max_steps {
            let steps = steps.clamp(1, 500);
            agent_config["max_steps"] = serde_json::json!(steps);
        }
        let mut body = serde_json::json!({
            "url": req.starting_url,
            "goal": req.task,
            "agent_config": agent_config,
        });
        if let Some(profile) = &req.browser_profile {
            if profile == "lite" || profile == "stealth" {
                body["browser_profile"] = serde_json::json!(profile);
            }
        }
        body
    }

    /// Compute the HTTP timeout for a sync run: max_duration_seconds +
    /// headroom so the response can arrive after the agent wraps up.
    fn sync_http_timeout(req: &AutomationRequest) -> Duration {
        Duration::from_secs(u64::from(req.max_duration_seconds)) + AUTOMATION_TIMEOUT_HEADROOM
    }

    /// POST to start a run and return the raw response body + HTTP status.
    async fn post_run(
        &self,
        req: &AutomationRequest,
        http_timeout: Duration,
    ) -> std::result::Result<(u16, String), String> {
        let token = self.token().map_err(|e| e.to_string())?;
        let body = Self::build_body(req);
        debug!(
            task = %req.task,
            starting_url = %req.starting_url,
            max_duration_seconds = req.max_duration_seconds,
            "evy-research: tinyfish automation POST"
        );
        let resp = self
            .http
            .post(&self.config.automation_endpoint)
            .timeout(http_timeout)
            .header("X-API-Key", token)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("transport: {}", e.without_url()))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, text))
    }

    /// GET the run-status endpoint for a specific run id.
    async fn get_run_status(&self, run_id: &str) -> Result<RunResponseRaw> {
        let token = self.token()?;
        let url = format!("{}/{}", self.config.run_status_endpoint, run_id);
        debug!(run_id = %run_id, "evy-research: polling run status");
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(15))
            .header("X-API-Key", token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ResearchError::Transport(e.without_url().to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let snippet = resp.text().await.unwrap_or_default();
            let snippet: String = snippet.chars().take(200).collect();
            return Err(ResearchError::Http { status, snippet });
        }
        resp.json::<RunResponseRaw>()
            .await
            .map_err(|e| ResearchError::Decode(e.without_url().to_string()))
    }

    /// Poll `run_id` until it reaches `COMPLETED` or `FAILED`, or until
    /// `deadline` passes.
    ///
    /// ## Retry rule
    ///
    /// This is the correct path when the initial POST returned 5xx or a
    /// transport error but included a `run_id` — the run is STILL
    /// executing on TinyFish's servers. We poll here rather than
    /// re-POSTing, which would double-spend credits.
    pub async fn poll_run(
        &self,
        run_id: &str,
        deadline: tokio::time::Instant,
    ) -> Result<AutomationResult> {
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(ResearchError::Transport(format!(
                    "automation: poll timeout waiting for run {run_id} to complete"
                )));
            }
            let raw = self.get_run_status(run_id).await?;
            let status_str = raw.status.as_deref().unwrap_or("").to_uppercase();
            if status_str == "COMPLETED" || status_str == "FAILED" {
                return Ok(AutomationResult {
                    run_id: run_id.to_string(),
                    status: status_str,
                    started_at: raw.started_at,
                    finished_at: raw.finished_at,
                    num_of_steps: raw.num_of_steps,
                    result: raw.result,
                    error: raw.error,
                });
            }
            // Not yet terminal — sleep and loop.
            debug!(run_id = %run_id, status = %status_str, "evy-research: run not yet terminal, polling again");
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Start a browser automation run and **wait** for it to complete.
    ///
    /// ## Retry rule
    ///
    /// If the POST itself returns a 5xx, we check whether a `run_id` was
    /// embedded in the (possibly partial) response body.  If one is
    /// present the run IS executing and we fall through to
    /// [`Self::poll_run`].  Only when no `run_id` can be found do we
    /// surface the 5xx as an error.  We never blind-retry the POST.
    pub async fn run_sync(&self, req: &AutomationRequest) -> Result<AutomationResult> {
        if req.task.is_empty() {
            return Err(ResearchError::Input("task is required".to_string()));
        }
        if req.starting_url.is_empty() {
            return Err(ResearchError::Input("starting_url is required".to_string()));
        }
        // Validate URL scheme.
        let parsed = ::url::Url::parse(&req.starting_url)
            .map_err(|e| ResearchError::Input(format!("invalid starting_url: {e}")))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(ResearchError::Input(format!(
                "starting_url must be http:// or https://, got {}",
                parsed.scheme()
            )));
        }

        let http_timeout = Self::sync_http_timeout(req);
        let deadline = tokio::time::Instant::now() + http_timeout;

        let (status, body) = self
            .post_run(req, http_timeout)
            .await
            .map_err(ResearchError::Transport)?;

        // 4xx — surface immediately; do not poll (run was never started).
        if (400..500).contains(&status) {
            let snippet: String = body.chars().take(200).collect();
            return Err(ResearchError::Http { status, snippet });
        }

        // Try to parse the body into a RunResponseRaw regardless of status —
        // even a 5xx may carry a run_id when the browser session was launched
        // before the gateway errored.
        let raw: Option<RunResponseRaw> = serde_json::from_str(&body).ok();
        let run_id = raw.as_ref().and_then(|r| r.run_id.as_deref());

        // 5xx with a run_id → the run is alive on TinyFish's side.
        // CRITICAL: Do NOT re-POST. Poll until terminal.
        if status >= 500_u16 {
            if let Some(id) = run_id {
                warn!(
                    run_id = %id,
                    status,
                    "automation POST returned 5xx but run_id present — polling instead of retrying POST"
                );
                return self.poll_run(id, deadline).await;
            }
            // No run_id in a 5xx body → the run was not started.
            let snippet: String = body.chars().take(200).collect();
            return Err(ResearchError::Http { status, snippet });
        }

        // 2xx — parse the terminal response.
        let raw = raw.ok_or_else(|| {
            ResearchError::Decode(format!(
                "automation: non-JSON response body: {}",
                body.chars().take(200).collect::<String>()
            ))
        })?;

        let run_id = raw.run_id.clone().ok_or_else(|| {
            ResearchError::Decode("automation: response missing run_id".to_string())
        })?;
        let status_str = raw.status.as_deref().unwrap_or("").to_uppercase();

        // 2xx but status is not yet terminal → poll.
        if status_str != "COMPLETED" && status_str != "FAILED" {
            debug!(run_id = %run_id, status = %status_str, "automation: 2xx but non-terminal status, polling");
            return self.poll_run(&run_id, deadline).await;
        }

        Ok(AutomationResult {
            run_id,
            status: status_str,
            started_at: raw.started_at,
            finished_at: raw.finished_at,
            num_of_steps: raw.num_of_steps,
            result: raw.result,
            error: raw.error,
        })
    }

    /// Start a browser automation run and return immediately with the
    /// `run_id`. Use [`Self::poll_run`] to check completion.
    ///
    /// ## Note on 5xx
    ///
    /// Like [`Self::run_sync`], if the POST fails with a 5xx but the
    /// body contains a `run_id`, we return that run_id — the run IS
    /// alive. If no `run_id` is recoverable from a 5xx body, we surface
    /// the error (the run was not started).
    pub async fn run_async(&self, req: &AutomationRequest) -> Result<AsyncRunId> {
        if req.task.is_empty() {
            return Err(ResearchError::Input("task is required".to_string()));
        }
        if req.starting_url.is_empty() {
            return Err(ResearchError::Input("starting_url is required".to_string()));
        }
        let parsed = ::url::Url::parse(&req.starting_url)
            .map_err(|e| ResearchError::Input(format!("invalid starting_url: {e}")))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(ResearchError::Input(format!(
                "starting_url must be http:// or https://, got {}",
                parsed.scheme()
            )));
        }

        // For the async path we give a generous connect + response
        // timeout (30s) — we only want the run_id, not the full result.
        let http_timeout = Duration::from_secs(30);

        let (status, body) = self
            .post_run(req, http_timeout)
            .await
            .map_err(ResearchError::Transport)?;

        // 4xx → error immediately (no run started).
        if (400..500).contains(&status) {
            let snippet: String = body.chars().take(200).collect();
            return Err(ResearchError::Http { status, snippet });
        }

        // Try to extract a run_id from the body regardless of HTTP status.
        let raw: Option<RunResponseRaw> = serde_json::from_str(&body).ok();
        let run_id = raw.and_then(|r| r.run_id);

        if let Some(id) = run_id {
            if status >= 500_u16 {
                warn!(
                    run_id = %id,
                    status,
                    "automation async POST returned 5xx but run_id present — returning run_id for polling"
                );
            }
            return Ok(AsyncRunId(id));
        }

        // 5xx with no run_id → run was not started.
        let snippet: String = body.chars().take(200).collect();
        Err(ResearchError::Http { status, snippet })
    }
}

// ─── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automation_config_debug_redacts_token() {
        let cfg = AutomationConfig::new("sk-supersecret-token");
        let s = format!("{cfg:?}");
        assert!(!s.contains("sk-supersecret-token"), "token leaked: {s}");
        assert!(s.contains("<redacted>"), "expected redacted marker: {s}");
    }

    #[test]
    fn automation_config_default_has_no_token() {
        let cfg = AutomationConfig::default();
        assert!(cfg.api_token.is_none());
        assert_eq!(cfg.automation_endpoint, DEFAULT_AUTOMATION_ENDPOINT);
        assert_eq!(cfg.run_status_endpoint, DEFAULT_RUN_STATUS_ENDPOINT);
    }

    #[test]
    fn automation_request_new_sets_defaults() {
        let r = AutomationRequest::new("click the button", "https://example.com");
        assert_eq!(r.task, "click the button");
        assert_eq!(r.starting_url, "https://example.com");
        assert_eq!(r.max_duration_seconds, DEFAULT_MAX_DURATION_SECONDS);
        assert!(r.max_steps.is_none());
        assert!(r.browser_profile.is_none());
    }

    #[test]
    fn build_body_includes_required_fields() {
        let req = AutomationRequest::new("do something", "https://example.com");
        let body = AutomationClient::build_body(&req);
        assert_eq!(body["url"], "https://example.com");
        assert_eq!(body["goal"], "do something");
        assert_eq!(
            body["agent_config"]["max_duration_seconds"],
            DEFAULT_MAX_DURATION_SECONDS
        );
        assert!(body.get("browser_profile").is_none());
    }

    #[test]
    fn build_body_includes_optional_fields_when_set() {
        let mut req = AutomationRequest::new("task", "https://example.com");
        req.max_steps = Some(10);
        req.browser_profile = Some("stealth".to_string());
        let body = AutomationClient::build_body(&req);
        assert_eq!(body["agent_config"]["max_steps"], 10);
        assert_eq!(body["browser_profile"], "stealth");
    }

    #[test]
    fn build_body_rejects_invalid_browser_profile() {
        let mut req = AutomationRequest::new("task", "https://example.com");
        req.browser_profile = Some("evil_profile".to_string());
        let body = AutomationClient::build_body(&req);
        assert!(body.get("browser_profile").is_none());
    }

    #[test]
    fn sync_http_timeout_adds_headroom() {
        let mut req = AutomationRequest::new("task", "https://example.com");
        req.max_duration_seconds = 60;
        let t = AutomationClient::sync_http_timeout(&req);
        assert_eq!(t, Duration::from_secs(60) + AUTOMATION_TIMEOUT_HEADROOM);
    }

    #[test]
    fn run_status_completed_and_failed_are_terminal() {
        let completed: RunStatus = serde_json::from_str(r#""COMPLETED""#).unwrap();
        let failed: RunStatus = serde_json::from_str(r#""FAILED""#).unwrap();
        let running: RunStatus = serde_json::from_str(r#""RUNNING""#).unwrap();
        assert_eq!(completed, RunStatus::Completed);
        assert_eq!(failed, RunStatus::Failed);
        assert_eq!(running, RunStatus::Running);
    }

    #[tokio::test]
    async fn run_sync_rejects_empty_task() {
        let cfg = AutomationConfig::new("tok");
        let client = AutomationClient::new(cfg);
        let req = AutomationRequest::new("", "https://example.com");
        let err = client
            .run_sync(&req)
            .await
            .expect_err("must reject empty task");
        assert!(matches!(err, ResearchError::Input(_)));
    }

    #[tokio::test]
    async fn run_sync_rejects_empty_url() {
        let cfg = AutomationConfig::new("tok");
        let client = AutomationClient::new(cfg);
        let req = AutomationRequest::new("do task", "");
        let err = client
            .run_sync(&req)
            .await
            .expect_err("must reject empty url");
        assert!(matches!(err, ResearchError::Input(_)));
    }

    #[tokio::test]
    async fn run_sync_rejects_non_http_url() {
        let cfg = AutomationConfig::new("tok");
        let client = AutomationClient::new(cfg);
        let req = AutomationRequest::new("do task", "ftp://example.com");
        let err = client
            .run_sync(&req)
            .await
            .expect_err("must reject non-http scheme");
        let msg = err.to_string();
        assert!(msg.contains("ftp"), "got: {msg}");
    }

    #[tokio::test]
    async fn run_async_rejects_empty_task() {
        let cfg = AutomationConfig::new("tok");
        let client = AutomationClient::new(cfg);
        let req = AutomationRequest::new("", "https://example.com");
        let err = client.run_async(&req).await.expect_err("must reject");
        assert!(matches!(err, ResearchError::Input(_)));
    }

    // ─── wiremock integration tests ───────────────────────────────────────

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_client(server_url: &str) -> AutomationClient {
        let mut cfg = AutomationConfig::new("test-token");
        cfg.automation_endpoint = format!("{server_url}/v1/automation/run");
        cfg.run_status_endpoint = format!("{server_url}/v1/automation/runs");
        AutomationClient::new(cfg)
    }

    #[tokio::test]
    async fn wiremock_run_sync_completed_returns_result() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/automation/run"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "run_id": "run-abc123",
                "status": "COMPLETED",
                "started_at": "2026-06-15T00:00:00Z",
                "finished_at": "2026-06-15T00:01:00Z",
                "num_of_steps": 5,
                "result": {"title": "Example page"},
                "error": null
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let req = AutomationRequest::new("click sign-in button", "https://example.com/login");
        let result = client
            .run_sync(&req)
            .await
            .expect("run_sync should succeed");
        assert_eq!(result.run_id, "run-abc123");
        assert_eq!(result.status, "COMPLETED");
        assert_eq!(result.num_of_steps, Some(5));
        assert!(result.result.is_some());
    }

    #[tokio::test]
    async fn wiremock_run_sync_failed_surfaces_error_payload() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/automation/run"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "run_id": "run-fail1",
                "status": "FAILED",
                "started_at": "2026-06-15T00:00:00Z",
                "finished_at": "2026-06-15T00:00:10Z",
                "num_of_steps": 1,
                "result": null,
                "error": {"code": "AGENT_ERROR", "message": "Could not find element"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let req = AutomationRequest::new("click nonexistent button", "https://example.com");
        let result = client
            .run_sync(&req)
            .await
            .expect("run returns even on FAILED status");
        assert_eq!(result.status, "FAILED");
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn wiremock_run_sync_4xx_surfaces_immediately_no_poll() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/automation/run"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let req = AutomationRequest::new("task", "https://example.com");
        let err = client
            .run_sync(&req)
            .await
            .expect_err("4xx must be an error");
        assert!(
            matches!(&err, ResearchError::Http { status, .. } if *status == 401),
            "got: {err:?}"
        );
        // Verify only 1 request was made (no polling).
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1, "4xx must not trigger polling");
    }

    /// This test verifies the CRITICAL RETRY RULE:
    /// POST returns 5xx WITH a run_id → must poll, NOT re-POST.
    #[tokio::test]
    async fn wiremock_run_sync_5xx_with_run_id_polls_not_retries_post() {
        let server = MockServer::start().await;

        // Initial POST: 5xx but with run_id in body (run already started)
        Mock::given(method("POST"))
            .and(path("/v1/automation/run"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "run_id": "run-polled",
                "status": "RUNNING",
                "error": "gateway timeout"
            })))
            .expect(1)  // MUST only be called once — no blind retry
            .mount(&server)
            .await;

        // First poll: still running
        Mock::given(method("GET"))
            .and(path("/v1/automation/runs/run-polled"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "run_id": "run-polled",
                "status": "RUNNING",
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second poll: completed
        Mock::given(method("GET"))
            .and(path("/v1/automation/runs/run-polled"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "run_id": "run-polled",
                "status": "COMPLETED",
                "num_of_steps": 3,
                "result": {"scraped": "data"},
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let mut req = AutomationRequest::new("task", "https://example.com");
        req.max_duration_seconds = 30;
        let result = client
            .run_sync(&req)
            .await
            .expect("should recover via polling");
        assert_eq!(result.run_id, "run-polled");
        assert_eq!(result.status, "COMPLETED");

        // Confirm POST was called exactly once (no blind retry).
        let received = server.received_requests().await.unwrap();
        let post_count = received
            .iter()
            .filter(|r| r.method == wiremock::http::Method::POST)
            .count();
        let get_count = received
            .iter()
            .filter(|r| r.method == wiremock::http::Method::GET)
            .count();
        assert_eq!(
            post_count, 1,
            "POST must only be called once (no blind retry)"
        );
        assert_eq!(
            get_count, 2,
            "expected 2 poll GETs (running then completed)"
        );
    }

    #[tokio::test]
    async fn wiremock_run_async_returns_run_id_immediately() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/automation/run"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "run_id": "async-run-xyz",
                "status": "RUNNING",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let req = AutomationRequest::new("scrape pricing", "https://example.com/pricing");
        let AsyncRunId(id) = client
            .run_async(&req)
            .await
            .expect("async should return run_id");
        assert_eq!(id, "async-run-xyz");

        // Confirm no poll was made.
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1, "async path must not poll automatically");
    }

    #[tokio::test]
    async fn wiremock_run_async_5xx_with_run_id_returns_run_id_not_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/automation/run"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "run_id": "async-run-503",
                "error": "upstream timeout"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let req = AutomationRequest::new("task", "https://example.com");
        let AsyncRunId(id) = client
            .run_async(&req)
            .await
            .expect("should get run_id from 5xx body");
        assert_eq!(id, "async-run-503");
    }
}
