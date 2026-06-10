//! Integration test for the upgrade-capable web-terminal WS proxy
//! (`/api/terminal/attach` → Bun) added in `crate::terminal_ws`.
//!
//! A mock "Bun" upstream stands in for the v3 dashboard: on a valid `team` it
//! completes the WS upgrade, pushes a binary pty frame, and echoes text frames
//! back as binary; on `team=disabled` it returns Bun's `403 {ok:false,error}`
//! shape *before* upgrading. The test drives a real WS client through the v4
//! front door and asserts:
//!
//! 1. **splice both ways** — binary flows Bun→browser, JSON-text flows
//!    browser→Bun (observed via the echo), frame types preserved.
//! 2. **gate rejection is proxied verbatim** — the pre-upgrade `403` with Bun's
//!    exact `{ok:false,error:"terminal disabled"}` body reaches the browser.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::ws::{Message as AxMsg, WebSocketUpgrade},
    extract::RawQuery,
    http::{header::CONTENT_TYPE, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use evy_comms::{EventBroadcaster, HttpConfig, HttpServer, StubAppState};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{error::Error as WsError, protocol::Message as TgMsg},
};
use tokio_util::sync::CancellationToken;

/// Mock Bun `/api/terminal/attach`. `team=disabled` → pre-upgrade 403 in Bun's
/// shape; anything else → upgrade, push a pty frame, echo text→binary.
async fn mock_attach(ws: WebSocketUpgrade, RawQuery(q): RawQuery) -> Response {
    let qs = q.unwrap_or_default();
    if qs.contains("team=disabled") {
        return (
            StatusCode::FORBIDDEN,
            [(CONTENT_TYPE, "application/json")],
            r#"{"ok":false,"error":"terminal disabled"}"#,
        )
            .into_response();
    }
    ws.on_upgrade(|mut sock| async move {
        // Raw pty bytes arrive as binary frames (xterm.js renders them).
        if sock
            .send(AxMsg::Binary(b"PTY-HELLO".to_vec().into()))
            .await
            .is_err()
        {
            return;
        }
        // Echo each browser text frame back as binary, proving browser→Bun
        // (JSON text) and Bun→browser (binary) both traverse the splice.
        while let Some(Ok(msg)) = sock.recv().await {
            match msg {
                AxMsg::Text(t) => {
                    let echo = format!("ECHO:{}", t.as_str());
                    if sock
                        .send(AxMsg::Binary(echo.into_bytes().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                AxMsg::Close(_) => break,
                _ => {}
            }
        }
    })
}

/// Spawn the mock upstream, returning its `host:port`.
async fn spawn_mock_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    let app = Router::new().route("/api/terminal/attach", get(mock_attach));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr.to_string()
}

/// Spawn the v4 evy-comms server (ephemeral), returning its `host:port`.
async fn spawn_v4() -> (String, CancellationToken) {
    let broadcaster = EventBroadcaster::new(64);
    let server = HttpServer::new(HttpConfig::ephemeral(), broadcaster, Arc::new(StubAppState));
    let bound = server.bind().await.expect("v4 bind");
    let addr = bound.local_addr();
    let shutdown = CancellationToken::new();
    let st = shutdown.clone();
    tokio::spawn(async move {
        let _ = bound.serve(st).await;
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    (addr.to_string(), shutdown)
}

#[tokio::test]
async fn attach_splices_both_ways_and_proxies_gate_rejection() {
    // One mock upstream serves both scenarios; env is set once (single test fn,
    // no cross-test env race).
    let mock = spawn_mock_upstream().await;
    std::env::set_var("EVY_PROXY_UPSTREAM", &mock);
    let (v4, shutdown) = spawn_v4().await;

    // ── 1. Happy path: splice both ways ──────────────────────────────────
    let url = format!("ws://{v4}/api/terminal/attach?team=ok&cols=80&rows=24");
    let (mut client, _resp) = connect_async(&url).await.expect("attach should upgrade");

    // Bun→browser: the binary pty frame.
    let first = client
        .next()
        .await
        .expect("expected a frame")
        .expect("frame ok");
    match first {
        TgMsg::Binary(b) => assert_eq!(b.as_slice(), b"PTY-HELLO", "first frame is pty bytes"),
        other => panic!("expected binary pty frame, got {other:?}"),
    }

    // browser→Bun: a JSON text data frame (the v3 wire shape).
    let data_frame = r#"{"type":"data","b64":"aGk="}"#;
    client
        .send(TgMsg::Text(data_frame.to_string()))
        .await
        .expect("send data frame");

    // Bun→browser: the echo, proving the text frame reached the upstream.
    let echo = client
        .next()
        .await
        .expect("expected echo frame")
        .expect("echo ok");
    match echo {
        TgMsg::Binary(b) => {
            let s = String::from_utf8_lossy(&b);
            assert!(s.starts_with("ECHO:"), "echo prefix missing: {s}");
            assert!(
                s.contains(r#""type":"data""#),
                "echo should carry our JSON text frame verbatim: {s}"
            );
        }
        other => panic!("expected binary echo, got {other:?}"),
    }
    let _ = client.close(None).await;

    // ── 2. Gate rejection is proxied verbatim (pre-upgrade 403) ───────────
    let dis = format!("ws://{v4}/api/terminal/attach?team=disabled");
    let err = connect_async(&dis)
        .await
        .expect_err("disabled must be rejected");
    match err {
        WsError::Http(resp) => {
            assert_eq!(resp.status().as_u16(), 403, "v3-shape status preserved");
            let body = resp.into_body().unwrap_or_default();
            let s = String::from_utf8_lossy(&body);
            assert!(
                s.contains("terminal disabled"),
                "Bun's exact error body must survive the proxy hop, got: {s}"
            );
        }
        other => panic!("expected Http 403 rejection, got {other:?}"),
    }

    shutdown.cancel();
}
