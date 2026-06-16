//! EA2 agency tool tests — gh (fake binary), Linear (wiremock), Telegram.
//!
//! Each tool gets at least one round-trip test:
//!
//! - **gh tools**: a temporary shell script stands in for the `gh` binary.
//! - **linear tools**: a `wiremock::MockServer` stands in for the GraphQL
//!   endpoint; Authorization header is verified to have NO "Bearer" prefix.
//! - **telegram_send / telegram_send_digest**: verified via a mock server
//!   standing in for the Telegram Bot API (`TelegramConfig::base_url`
//!   override). `telegram_send_voice` is a checkpoint test — asserts the
//!   error message explains the missing multipart wiring.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use async_trait::async_trait;
use evy::agency::{
    GhIssueListTool, GhPrChecksTool, GhPrListTool, GhPrViewTool, LinearClient,
    LinearCreateIssueTool, LinearListIssuesTool, LinearSearchTool, LinearUpdateIssueTool,
    TelegramSendDigestTool, TelegramSendTool, TelegramSendVoiceTool,
};
use evy_comms::{AskRegistry, TelegramBridge, TelegramConfig};
use evy_secrets::{Result as SecretsResult, SecretValue, SecretsBackend, SecretsResolver};
use evy_thinking::EvyTool;
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ─── StaticBackend — bakes a fixed key→value for tests ───────────────────────

/// A dead-simple in-memory secrets backend for tests. Avoids env-var
/// mutation so tests can run safely in parallel.
struct StaticBackend {
    key: &'static str,
    value: &'static str,
}

#[async_trait]
impl SecretsBackend for StaticBackend {
    fn name(&self) -> &str {
        "static-test"
    }

    async fn resolve(&self, key: &str) -> SecretsResult<Option<SecretValue>> {
        if key == self.key {
            Ok(Some(SecretValue {
                value: self.value.to_string(),
                source: "static-test".to_string(),
            }))
        } else {
            Ok(None)
        }
    }
}

/// Build a `LinearClient` with a baked-in API key for a mock server URL.
fn linear_client(server_url: &str, api_key: &'static str) -> Arc<LinearClient> {
    Arc::new(LinearClient {
        http: reqwest::Client::new(),
        secrets: Arc::new(SecretsResolver::new(vec![Arc::new(StaticBackend {
            key: "linear_api_key",
            value: api_key,
        })
            as Arc<dyn SecretsBackend>])),
        endpoint: format!("{server_url}/graphql"),
    })
}

/// Build a `LinearClient` with an empty resolver (simulates missing key).
fn linear_client_no_key(server_url: &str) -> Arc<LinearClient> {
    Arc::new(LinearClient {
        http: reqwest::Client::new(),
        secrets: Arc::new(SecretsResolver::new(vec![])),
        endpoint: format!("{server_url}/graphql"),
    })
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Write a tiny shell script acting as a fake `gh` binary, make it executable,
/// and return its path. `script_body` should begin with `#!/bin/sh\n`.
fn fake_gh_script(dir: &tempfile::TempDir, script_body: &str) -> std::path::PathBuf {
    let p = dir.path().join("gh");
    std::fs::write(&p, script_body).expect("write fake gh");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod fake gh");
    p
}

/// Build a `TelegramBridge` pointing at `base_url` with the given bot token.
fn mock_telegram_bridge(base_url: &str, bot_token: &str) -> TelegramBridge {
    let cfg = TelegramConfig {
        bot_token: bot_token.to_string(),
        chat_id: 12345,
        poll_interval: std::time::Duration::from_secs(1),
        long_poll_timeout: std::time::Duration::from_secs(5),
        base_url: base_url.to_string(),
        inbound: None,
        events: None,
    };
    TelegramBridge::new(cfg, Arc::new(AskRegistry::new()))
}

// ─── gh tools ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn gh_pr_list_returns_stdout_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake_gh_script(
        &dir,
        "#!/bin/sh\necho '[{\"number\":42,\"title\":\"fix: the thing\"}]'\n",
    );
    let tool = GhPrListTool { gh_bin: fake };
    let out = tool
        .execute(&json!({"repo": "acme/repo"}))
        .await
        .expect("ok");
    assert!(
        out.contains("\"number\":42") || out.contains("\"number\": 42"),
        "got: {out}"
    );
}

#[tokio::test]
async fn gh_pr_list_requires_repo() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake_gh_script(&dir, "#!/bin/sh\necho '[]\n'\n");
    let tool = GhPrListTool { gh_bin: fake };
    let err = tool.execute(&json!({})).await.unwrap_err();
    assert!(err.contains("`repo` is required"), "got: {err}");
}

#[tokio::test]
async fn gh_pr_list_propagates_gh_stderr_on_nonzero_exit() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake_gh_script(&dir, "#!/bin/sh\necho 'no auth credentials' >&2\nexit 1\n");
    let tool = GhPrListTool { gh_bin: fake };
    let err = tool
        .execute(&json!({"repo": "acme/repo"}))
        .await
        .unwrap_err();
    assert!(err.contains("no auth credentials"), "got: {err}");
}

#[tokio::test]
async fn gh_pr_view_requires_pr_number() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake_gh_script(&dir, "#!/bin/sh\necho '{}'\n");
    let tool = GhPrViewTool { gh_bin: fake };
    let err = tool
        .execute(&json!({"repo": "acme/repo"}))
        .await
        .unwrap_err();
    assert!(err.contains("`pr` must be an integer"), "got: {err}");
}

#[tokio::test]
async fn gh_pr_view_passes_pr_number_to_binary() {
    let dir = tempfile::tempdir().unwrap();
    // Echo all args so the test can verify they were passed through.
    let fake = fake_gh_script(&dir, "#!/bin/sh\necho \"$@\"\n");
    let tool = GhPrViewTool { gh_bin: fake };
    let out = tool
        .execute(&json!({"repo": "acme/repo", "pr": 99}))
        .await
        .expect("ok");
    assert!(out.contains("99"), "pr number not in args; got: {out}");
    assert!(out.contains("acme/repo"), "repo not in args; got: {out}");
}

#[tokio::test]
async fn gh_pr_checks_passes_pr_and_repo() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake_gh_script(&dir, "#!/bin/sh\necho \"$@\"\n");
    let tool = GhPrChecksTool { gh_bin: fake };
    let out = tool
        .execute(&json!({"repo": "acme/repo", "pr": 7}))
        .await
        .expect("ok");
    assert!(out.contains("7"), "pr not in args; got: {out}");
    assert!(out.contains("checks"), "subcommand not in args; got: {out}");
}

#[tokio::test]
async fn gh_issue_list_passes_state_and_label() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake_gh_script(&dir, "#!/bin/sh\necho \"$@\"\n");
    let tool = GhIssueListTool { gh_bin: fake };
    let out = tool
        .execute(&json!({"repo": "acme/repo", "state": "closed", "label": "bug"}))
        .await
        .expect("ok");
    assert!(out.contains("closed"), "state not in args; got: {out}");
    assert!(out.contains("bug"), "label not in args; got: {out}");
}

// ─── linear tools ─────────────────────────────────────────────────────────────
//
// Each test gets its own MockServer so mocks never cross-pollinate.
// Mocks are distinguished by body_string_contains so multi-step mutations
// (team lookup → issue create) can run on the same server.

#[tokio::test]
async fn linear_list_issues_sends_no_bearer_prefix_and_returns_nodes() {
    let server = MockServer::start().await;

    // Authorization header must be the raw token — NO "Bearer " prefix.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "lin_api_list_key"))
        .and(body_string_contains("issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "issues": {
                    "nodes": [
                        {"id": "abc", "identifier": "ENG-1", "title": "Test issue",
                         "priority": 2, "url": "https://linear.app/x",
                         "state": {"name": "Todo"}, "assignee": null}
                    ]
                }
            }
        })))
        .mount(&server)
        .await;

    let client = linear_client(&server.uri(), "lin_api_list_key");
    let tool = LinearListIssuesTool { client };
    let out = tool
        .execute(&json!({"team_key": "ENG", "limit": 10}))
        .await
        .expect("ok");
    assert!(out.contains("ENG-1"), "got: {out}");
    // If this test passed, Authorization was matched WITHOUT "Bearer" —
    // the wiremock header matcher asserts the exact value above.
}

#[tokio::test]
async fn linear_list_issues_errors_without_api_key() {
    let server = MockServer::start().await;
    // No API key in resolver — expect NotFound error before any HTTP call.
    let client = linear_client_no_key(&server.uri());
    let tool = LinearListIssuesTool { client };
    let err = tool.execute(&json!({})).await.unwrap_err();
    assert!(err.contains("LINEAR_API_KEY not configured"), "got: {err}");
}

#[tokio::test]
async fn linear_search_sends_term_and_returns_nodes() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "lin_api_search"))
        .and(body_string_contains("searchIssues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "searchIssues": {
                    "nodes": [{"id": "x", "identifier": "ENG-99", "title": "Bug in search",
                               "priority": 1, "url": "https://linear.app/x",
                               "state": {"name": "In Progress"}, "assignee": null}]
                }
            }
        })))
        .mount(&server)
        .await;

    let client = linear_client(&server.uri(), "lin_api_search");
    let tool = LinearSearchTool { client };
    let out = tool
        .execute(&json!({"query": "search bug"}))
        .await
        .expect("ok");
    assert!(out.contains("ENG-99"), "got: {out}");
}

#[tokio::test]
async fn linear_create_issue_resolves_team_then_creates() {
    let server = MockServer::start().await;

    // Team lookup response (matched by body containing "TeamByKey").
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "lin_api_create"))
        .and(body_string_contains("TeamByKey"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "teams": { "nodes": [{"id": "team-uuid-123", "key": "ENG", "name": "Engineering"}] }
            }
        })))
        .mount(&server)
        .await;

    // Issue create response (matched by body containing "issueCreate").
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "lin_api_create"))
        .and(body_string_contains("issueCreate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "issueCreate": {
                    "success": true,
                    "issue": {"id": "i-456", "identifier": "ENG-7",
                              "url": "https://linear.app/issue", "title": "Ship it"}
                }
            }
        })))
        .mount(&server)
        .await;

    let client = linear_client(&server.uri(), "lin_api_create");
    let tool = LinearCreateIssueTool { client };
    let out = tool
        .execute(&json!({"team_key": "ENG", "title": "Ship it", "priority": 2}))
        .await
        .expect("ok");
    assert!(out.contains("ENG-7"), "got: {out}");
}

#[tokio::test]
async fn linear_update_issue_resolves_state_and_adds_comment() {
    let server = MockServer::start().await;

    // Issue lookup (matched by body containing "IssueLookup").
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "lin_api_update"))
        .and(body_string_contains("IssueLookup"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "issue": {"id": "issue-abc", "identifier": "ENG-42",
                          "team": {"id": "team-t1", "key": "ENG"}}
            }
        })))
        .mount(&server)
        .await;

    // Workflow states lookup (matched by body containing "WorkflowStates").
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "lin_api_update"))
        .and(body_string_contains("WorkflowStates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "workflowStates": {
                    "nodes": [{"id": "state-done", "name": "Done", "type": "completed"}]
                }
            }
        })))
        .mount(&server)
        .await;

    // Issue update (matched by body containing "IssueUpdate").
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "lin_api_update"))
        .and(body_string_contains("IssueUpdate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "issueUpdate": {
                    "success": true,
                    "issue": {"id": "issue-abc", "identifier": "ENG-42",
                              "url": "https://linear.app/x", "state": {"name": "Done"}}
                }
            }
        })))
        .mount(&server)
        .await;

    // Comment create (matched by body containing "CommentCreate").
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "lin_api_update"))
        .and(body_string_contains("CommentCreate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "commentCreate": {
                    "success": true,
                    "comment": {"id": "c-1", "url": "https://linear.app/comment/1"}
                }
            }
        })))
        .mount(&server)
        .await;

    let client = linear_client(&server.uri(), "lin_api_update");
    let tool = LinearUpdateIssueTool { client };
    let out = tool
        .execute(&json!({"issue_id": "ENG-42", "state": "Done", "comment": "Shipped!"}))
        .await
        .expect("ok");
    assert!(out.contains("state updated"), "got: {out}");
    assert!(out.contains("comment added"), "got: {out}");
}

#[tokio::test]
async fn linear_update_issue_requires_state_or_comment() {
    let server = MockServer::start().await;
    let client = linear_client(&server.uri(), "any");
    let tool = LinearUpdateIssueTool { client };
    let err = tool
        .execute(&json!({"issue_id": "ENG-1"}))
        .await
        .unwrap_err();
    assert!(
        err.contains("at least one of `state` or `comment`"),
        "got: {err}"
    );
}

// ─── telegram tools ────────────────────────────────────────────────────────────

#[tokio::test]
async fn telegram_send_delivers_text() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/bottest-bot-token/sendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"message_id": 1}
        })))
        .mount(&server)
        .await;

    let bridge = mock_telegram_bridge(&server.uri(), "test-bot-token");
    let tool = TelegramSendTool {
        telegram: Some(bridge),
    };
    let out = tool
        .execute(&json!({"text": "Hello from EA2"}))
        .await
        .expect("ok");
    assert_eq!(out, "sent");
}

#[tokio::test]
async fn telegram_send_errors_without_bridge() {
    let tool = TelegramSendTool { telegram: None };
    let err = tool.execute(&json!({"text": "hi"})).await.unwrap_err();
    assert!(err.contains("telegram is not configured"), "got: {err}");
}

#[tokio::test]
async fn telegram_send_digest_renders_and_delivers() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/botdigest-token/sendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"message_id": 2}
        })))
        .mount(&server)
        .await;

    let bridge = mock_telegram_bridge(&server.uri(), "digest-token");
    let tool = TelegramSendDigestTool {
        telegram: Some(bridge),
    };
    let out = tool
        .execute(&json!({
            "title": "Sprint status",
            "entries": [
                {"project": "api", "verdict": "green", "line": "all tests pass"},
                {"project": "ui",  "verdict": "amber", "line": "2 flaky tests"},
                {"project": "db",  "verdict": "red",   "line": "migration failed"}
            ]
        }))
        .await
        .expect("ok");
    assert!(out.contains("3 entries"), "got: {out}");
}

#[tokio::test]
async fn telegram_send_voice_returns_not_yet_wired_checkpoint() {
    // CHECKPOINT test: voice tool must return an error that explains the
    // multipart wiring gap — not a panic, not a silent no-op.
    let tool = TelegramSendVoiceTool { telegram: None };
    let err = tool
        .execute(&json!({"text": "say something"}))
        .await
        .unwrap_err();
    assert!(
        err.contains("not yet wired") || err.contains("multipart"),
        "expected checkpoint message; got: {err}"
    );
}

#[tokio::test]
async fn telegram_send_voice_with_bridge_still_returns_checkpoint_error() {
    // Even with a configured bridge, voice must fail (checkpoint) because
    // multipart is not wired yet. The error message should reflect that
    // creds ARE present.
    let server = MockServer::start().await;
    let bridge = mock_telegram_bridge(&server.uri(), "voice-tok");
    let tool = TelegramSendVoiceTool {
        telegram: Some(bridge),
    };
    let err = tool
        .execute(&json!({"text": "say something"}))
        .await
        .unwrap_err();
    assert!(
        err.contains("not yet wired") || err.contains("multipart"),
        "expected checkpoint message; got: {err}"
    );
    // The checkpoint message should note that creds ARE present.
    assert!(
        err.contains("true"),
        "expected creds_present=true in msg; got: {err}"
    );
}
