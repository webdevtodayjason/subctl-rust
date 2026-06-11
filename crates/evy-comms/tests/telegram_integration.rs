//! Telegram bridge integration tests against a `wiremock` mock of the
//! Bot API. The real `api.telegram.org` is NEVER hit — `TelegramConfig`
//! exposes `base_url` precisely so this test can swap it.

use std::sync::Arc;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use evy_comms::{
    AskRegistry, EventBroadcaster, HttpConfig, HttpServer, InboundMessage, Notification,
    TelegramBridge, TelegramConfig,
};
use evy_core::{ProviderKind, WorkerId};

const TOKEN: &str = "TESTTOKEN";
const CHAT_ID: i64 = 12345;

fn cfg(server_url: &str) -> TelegramConfig {
    let mut c = TelegramConfig::new(TOKEN.to_string(), CHAT_ID);
    c.base_url = server_url.to_string();
    // Tighten timings so the run loop iterates quickly in tests.
    c.long_poll_timeout = Duration::from_millis(50);
    c.poll_interval = Duration::from_millis(20);
    c
}

#[tokio::test]
async fn notify_posts_sendmessage_with_expected_body() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"message_id": 1}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let bridge = TelegramBridge::new(cfg(&server.uri()), Arc::new(AskRegistry::new()));

    bridge
        .notify(Notification::WorkerStarted {
            worker_id: WorkerId::new(),
            provider: ProviderKind::ClaudeCode,
            goal: "ship slice 2B2".into(),
        })
        .await
        .expect("notify must succeed");

    // Mock's `.expect(1)` enforces the call count when the server is
    // dropped at end of scope.
}

#[tokio::test]
async fn ask_round_trip_resolves_on_in_reply_to() {
    let server = MockServer::start().await;

    // sendMessage returns a deterministic outbound message_id.
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"message_id": 100}
        })))
        .mount(&server)
        .await;

    // Reply mock first (fires once via up_to_n_times), then the
    // empty-result fallback for subsequent polls. wiremock matches in
    // insertion order, falling through once a mock's count is exhausted.
    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [{
                "update_id": 1,
                "message": {
                    "message_id": 51,
                    "text": "yes please",
                    "chat": {"id": CHAT_ID},
                    "from": {"id": 99, "first_name": "Jason"},
                    "reply_to_message": {"message_id": 100}
                }
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []})))
        .mount(&server)
        .await;

    let asks = Arc::new(AskRegistry::new());
    let bridge = TelegramBridge::new(cfg(&server.uri()), asks.clone());

    let shutdown = CancellationToken::new();
    let bridge_for_run = bridge.clone();
    let shutdown_for_run = shutdown.clone();
    let run_task = tokio::spawn(async move {
        bridge_for_run.run(shutdown_for_run).await.expect("run ok");
    });

    let answer = bridge
        .ask("continue?".into(), Duration::from_secs(3))
        .await
        .expect("ask must resolve");
    assert_eq!(answer, "yes please");

    shutdown.cancel();
    run_task.await.expect("run task joined");
}

#[tokio::test]
async fn ask_times_out_when_no_reply() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"message_id": 200}
        })))
        .mount(&server)
        .await;

    // Empty updates forever.
    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []})))
        .mount(&server)
        .await;

    let asks = Arc::new(AskRegistry::new());
    let bridge = TelegramBridge::new(cfg(&server.uri()), asks);

    let shutdown = CancellationToken::new();
    let bridge_for_run = bridge.clone();
    let shutdown_for_run = shutdown.clone();
    let run_task = tokio::spawn(async move {
        bridge_for_run.run(shutdown_for_run).await.expect("run ok");
    });

    let res = bridge
        .ask("anyone there?".into(), Duration::from_millis(120))
        .await;
    assert!(res.is_err(), "ask must time out");

    shutdown.cancel();
    run_task.await.expect("run task joined");
}

#[tokio::test]
async fn inbound_non_reply_forwarded_to_handler() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [{
                "update_id": 1,
                "message": {
                    "message_id": 7,
                    "text": "/status",
                    "chat": {"id": CHAT_ID},
                    "from": {"id": 99, "first_name": "Jason"}
                }
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []})))
        .mount(&server)
        .await;

    let (tx, mut rx) = mpsc::unbounded_channel::<InboundMessage>();
    let mut c = cfg(&server.uri());
    c.inbound = Some(tx);

    let bridge = TelegramBridge::new(c, Arc::new(AskRegistry::new()));

    let shutdown = CancellationToken::new();
    let bridge_for_run = bridge.clone();
    let shutdown_for_run = shutdown.clone();
    let run_task = tokio::spawn(async move {
        bridge_for_run.run(shutdown_for_run).await.expect("run ok");
    });

    let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("recv timeout")
        .expect("channel must yield message");
    assert_eq!(received.text, "/status");
    assert_eq!(received.chat_id, CHAT_ID);
    assert_eq!(received.from_name.as_deref(), Some("Jason"));

    shutdown.cancel();
    run_task.await.expect("run task joined");
}

#[tokio::test]
async fn inbound_from_unauthorized_chat_is_dropped() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [{
                "update_id": 1,
                "message": {
                    "message_id": 7,
                    "text": "hello",
                    "chat": {"id": 99999},
                    "from": {"id": 99, "first_name": "Stranger"}
                }
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []})))
        .mount(&server)
        .await;

    let (tx, mut rx) = mpsc::unbounded_channel::<InboundMessage>();
    let mut c = cfg(&server.uri());
    c.inbound = Some(tx);

    let bridge = TelegramBridge::new(c, Arc::new(AskRegistry::new()));

    let shutdown = CancellationToken::new();
    let bridge_for_run = bridge.clone();
    let shutdown_for_run = shutdown.clone();
    let run_task = tokio::spawn(async move {
        bridge_for_run.run(shutdown_for_run).await.expect("run ok");
    });

    // The bridge should silently drop the unauthorized chat. Give the
    // loop a moment, then assert nothing was forwarded.
    let recv = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
    assert!(recv.is_err(), "no message should have been forwarded");

    shutdown.cancel();
    run_task.await.expect("run task joined");
}

#[tokio::test]
async fn getupdates_advances_offset_via_query_param() {
    let server = MockServer::start().await;

    // First call: no offset, return an update with update_id=42.
    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [{"update_id": 42}]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    // Subsequent calls must carry offset=43.
    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .and(query_param("offset", "43"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []})))
        .expect(1..)
        .mount(&server)
        .await;

    let bridge = TelegramBridge::new(cfg(&server.uri()), Arc::new(AskRegistry::new()));
    let shutdown = CancellationToken::new();
    let bridge_for_run = bridge.clone();
    let shutdown_for_run = shutdown.clone();
    let run_task = tokio::spawn(async move {
        bridge_for_run.run(shutdown_for_run).await.expect("run ok");
    });

    // Let the loop tick a couple of times.
    tokio::time::sleep(Duration::from_millis(300)).await;

    shutdown.cancel();
    run_task.await.expect("run task joined");
}

/// Regression guard: when sendMessage's transport fails, the bot token
/// (a Telegram credential) must NEVER appear in the resulting `Error`
/// message. `reqwest::Error`'s `Display` includes the URL by default
/// (and our URL contains `/bot<TOKEN>/sendMessage`); the bridge calls
/// `.without_url()` to strip it. This test points the bridge at an
/// invalid host so reqwest errors, then asserts the token is absent.
#[tokio::test]
async fn send_error_does_not_leak_bot_token() {
    // Use a port that nothing's listening on — guaranteed connection
    // refusal, which surfaces as a reqwest transport error.
    let mut c = TelegramConfig::new("SUPER_SECRET_TOKEN_12345".to_string(), CHAT_ID);
    c.base_url = "http://127.0.0.1:1".to_string();
    c.long_poll_timeout = Duration::from_millis(50);
    c.poll_interval = Duration::from_millis(20);

    let bridge = TelegramBridge::new(c, Arc::new(AskRegistry::new()));
    let err = bridge
        .notify(Notification::Error {
            context: "test".into(),
            message: "trip the transport".into(),
        })
        .await
        .expect_err("transport must fail");

    let msg = err.to_string();
    assert!(
        !msg.contains("SUPER_SECRET_TOKEN_12345"),
        "bot token must not leak through reqwest's Display impl; got: {msg}"
    );
}

// ── lone-ask plain-message fallback (live finding, 2026-06-09) ───────────

#[tokio::test]
async fn plain_message_resolves_lone_open_ask() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"message_id": 300}
        })))
        .mount(&server)
        .await;

    // Operator types a PLAIN message — no reply_to_message at all.
    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [{
                "update_id": 1,
                "message": {
                    "message_id": 301,
                    "text": "got it",
                    "chat": {"id": CHAT_ID},
                    "from": {"id": 99, "first_name": "Jason"}
                }
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []})))
        .mount(&server)
        .await;

    let asks = Arc::new(AskRegistry::new());
    let bridge = TelegramBridge::new(cfg(&server.uri()), asks.clone());

    let shutdown = CancellationToken::new();
    let bridge_for_run = bridge.clone();
    let shutdown_for_run = shutdown.clone();
    let run_task = tokio::spawn(async move {
        bridge_for_run.run(shutdown_for_run).await.expect("run ok");
    });

    let answer = bridge
        .ask("anyone there?".into(), Duration::from_secs(3))
        .await
        .expect("plain message must resolve the lone ask");
    assert_eq!(answer, "got it");

    shutdown.cancel();
    run_task.await.expect("run task joined");
}

#[tokio::test]
async fn plain_message_after_timed_out_ask_resolves_the_new_one() {
    // Regression: a timed-out ask must clean its message_id mapping, or
    // the next ask sees TWO "open" entries and the fallback never fires.
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"message_id": 400}
        })))
        .mount(&server)
        .await;
    // No updates until the plain answer below.
    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [{
                "update_id": 1,
                "message": {
                    "message_id": 401,
                    "text": "second time lucky",
                    "chat": {"id": CHAT_ID},
                    "from": {"id": 99, "first_name": "Jason"}
                }
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []})))
        .mount(&server)
        .await;

    let asks = Arc::new(AskRegistry::new());
    let bridge = TelegramBridge::new(cfg(&server.uri()), asks.clone());

    // First ask times out instantly (no poll loop running yet) — its
    // mapping must be cleaned up by ask() itself.
    let timed_out = bridge
        .ask("first question".into(), Duration::from_millis(10))
        .await;
    assert!(timed_out.is_err(), "first ask must time out");

    let shutdown = CancellationToken::new();
    let bridge_for_run = bridge.clone();
    let shutdown_for_run = shutdown.clone();
    let run_task = tokio::spawn(async move {
        bridge_for_run.run(shutdown_for_run).await.expect("run ok");
    });

    let answer = bridge
        .ask("second question".into(), Duration::from_secs(3))
        .await
        .expect("plain message must resolve the new lone ask");
    assert_eq!(answer, "second time lucky");

    shutdown.cancel();
    run_task.await.expect("run task joined");
}

// ── dashboard live feed: named `inbound` frame (Wave 4 orch events) ──────

/// An authorized non-reply operator message must surface on a LIVE
/// `/api/evy/events` subscription as the named `inbound` frame the
/// dashboard cockpit listens for (`orch.js:439` parses `{source, text}`).
/// The HTTP server and the bridge share ONE broadcaster — the daemon's
/// exact wiring; the Bot API stays wiremocked.
#[tokio::test]
async fn inbound_message_surfaces_on_live_events_subscription() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [{
                "update_id": 1,
                "message": {
                    "message_id": 7,
                    "text": "deploy the thing",
                    "chat": {"id": CHAT_ID},
                    "from": {"id": 99, "first_name": "Jason"}
                }
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/bot{TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []})))
        .mount(&server)
        .await;

    let broadcaster = EventBroadcaster::new(64);
    let http = HttpServer::with_stub_state(HttpConfig::ephemeral(), broadcaster.clone());
    let bound = http.bind().await.expect("bind");
    let addr = bound.local_addr();
    let http_shutdown = CancellationToken::new();
    let http_shutdown_for_serve = http_shutdown.clone();
    tokio::spawn(async move {
        let _ = bound.serve(http_shutdown_for_serve).await;
    });

    // Subscribe FIRST — broadcast only reaches current subscribers.
    let events_res = reqwest::Client::new()
        .get(format!("http://{addr}/api/evy/events"))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .expect("events GET");
    assert_eq!(events_res.status(), 200);
    let mut events = events_res.bytes_stream().eventsource();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut c = cfg(&server.uri());
    c.events = Some(broadcaster);
    let bridge = TelegramBridge::new(c, Arc::new(AskRegistry::new()));

    let shutdown = CancellationToken::new();
    let bridge_for_run = bridge.clone();
    let shutdown_for_run = shutdown.clone();
    let run_task = tokio::spawn(async move {
        bridge_for_run.run(shutdown_for_run).await.expect("run ok");
    });

    // The named frame arrives carrying the orch.js contract fields.
    let frame = loop {
        let frame = tokio::time::timeout(Duration::from_secs(3), events.next())
            .await
            .expect("inbound frame timed out")
            .expect("event stream ended")
            .expect("event stream errored");
        if frame.event == "inbound" {
            break frame;
        }
    };
    let data: serde_json::Value = serde_json::from_str(&frame.data).expect("frame data json");
    assert_eq!(data["source"], "telegram");
    assert_eq!(data["text"], "deploy the thing");
    assert!(data["ts"].as_str().unwrap().ends_with('Z'));

    shutdown.cancel();
    run_task.await.expect("run task joined");
    http_shutdown.cancel();
}
