//! Phase 0 (full cutover) — reverse-proxy fallback + native `/api/host`.
//!
//! The v4 daemon becomes the browser's single front door. Routes it implements
//! natively (`/api/evy/*`, `/health`, `/api/version`, `/api/host`) win; every
//! other `/api/*` request falls through to [`reverse_proxy_handler`], which
//! forwards it to the still-running v3 Bun dashboard (default `127.0.0.1:8787`,
//! override with `EVY_PROXY_UPSTREAM`). Bun keeps its Fork A BFF (chat-event
//! bridge, session-id injection) and its own proxy to the v3 master (:8788), so
//! the dashboard is 100% functional while panels migrate to v4 one phase at a time.
//!
//! Streaming is passed through chunk-by-chunk (SSE works). WebSocket upgrades
//! (`/api/live`, the web terminal) are NOT handled here — `reqwest` can't proxy a
//! WS upgrade; that's tracked as a Phase 0 known gap (hyper-upgrade work, lands
//! when the terminal panel is ported).

use std::sync::OnceLock;

use axum::{
    body::Body,
    extract::ws::{Message as AxMsg, WebSocket, WebSocketUpgrade},
    extract::{RawQuery, Request},
    http::{HeaderMap, HeaderName, StatusCode},
    response::{IntoResponse, Json, Response},
};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::protocol::Message as TgMsg;

/// Hop-by-hop headers (RFC 7230 §6.1) + host — never forwarded across a proxy hop.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

/// `pub(crate)` — `orch_registry_http` reuses the same upstream + client
/// for its optional-degraded v3 registry fetch (read per request, so tests
/// can repoint `EVY_PROXY_UPSTREAM` between scenarios).
pub(crate) fn upstream_base() -> String {
    std::env::var("EVY_PROXY_UPSTREAM").unwrap_or_else(|_| "127.0.0.1:8787".to_string())
}

pub(crate) fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // No global timeout: SSE streams (/api/master/events) are long-lived.
            .build()
            .expect("build reverse-proxy reqwest client")
    })
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP
        .iter()
        .any(|h| name.as_str().eq_ignore_ascii_case(h))
}

/// Catch-all for any `/api/*` not matched by a native route → forward to Bun.
/// Method, path+query, headers, and body are preserved; the response (incl. SSE)
/// is streamed back without buffering.
pub(crate) async fn reverse_proxy_handler(req: Request) -> Response {
    let path_q = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    let url = format!("http://{}{}", upstream_base(), path_q);

    let method = req.method().clone();
    let mut fwd_headers = HeaderMap::new();
    for (name, value) in req.headers() {
        if !is_hop_by_hop(name) {
            fwd_headers.insert(name.clone(), value.clone());
        }
    }

    // Collect the request body. JSON/chat bodies are small; the web-terminal WS
    // path never reaches here. (Streaming request bodies are a later refinement.)
    let body_bytes = match axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => return bad_gateway(&format!("read request body: {e}")),
    };

    let upstream = client()
        .request(method, &url)
        .headers(fwd_headers)
        .body(body_bytes)
        .send()
        .await;

    let resp = match upstream {
        Ok(r) => r,
        Err(e) => return bad_gateway(&format!("v3 dashboard (Bun) unreachable: {e}")),
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    if let Some(hdrs) = builder.headers_mut() {
        for (name, value) in resp.headers() {
            if !is_hop_by_hop(name) {
                hdrs.insert(name.clone(), value.clone());
            }
        }
    }
    // Stream the upstream body through chunk-by-chunk (SSE passthrough).
    let stream = resp.bytes_stream();
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|e| bad_gateway(&format!("build proxied response: {e}")))
}

fn bad_gateway(msg: &str) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "ok": false, "error": msg })),
    )
        .into_response()
}

/// `GET /api/host` — native. Removes one proxy dependency immediately. Returns
/// `{ hostname, host_label }`; `host_label` reads `~/.config/subctl/host_label`
/// (the operator's friendly machine name), falling back to the hostname.
pub(crate) async fn host_handler() -> Json<serde_json::Value> {
    let hostname = hostname_string();
    let label_path = dirs_config_dir().join("host_label");
    let host_label = std::fs::read_to_string(&label_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| hostname.clone());
    Json(json!({ "hostname": hostname, "host_label": host_label }))
}

fn hostname_string() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

fn dirs_config_dir() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config/subctl"))
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"))
}

/// `GET /api/live` (WebSocket) — Phase 0 web-terminal passthrough. `reqwest`
/// can't proxy a WS upgrade, so this is handled separately from the HTTP
/// reverse-proxy: accept the browser's upgrade, dial the v3 Bun dashboard's
/// `/api/live` WS (preserving the `?team&cols&rows` query), and splice frames
/// both ways until either side closes.
pub(crate) async fn ws_proxy_handler(ws: WebSocketUpgrade, RawQuery(q): RawQuery) -> Response {
    let raw = q.unwrap_or_default();
    // The web terminal attaches with `?team=...`; the liveness socket has none.
    // Terminal → proxy to Bun's /api/live WS (not yet native). Liveness → native
    // broadcast of /api/state every 2s (Phase 1 slice 1f).
    if raw.contains("team=") {
        ws.on_upgrade(move |sock| bridge_ws(sock, format!("?{raw}")))
    } else {
        ws.on_upgrade(liveness_broadcast)
    }
}

/// Native `/api/live` liveness (1f): push the native `/api/state` snapshot every
/// 2s. app.js parses each message as `/api/state` and runs `flashPulse` + `render`
/// — pulse-on-signature-change is client-side, so the server just streams snapshots.
async fn liveness_broadcast(mut socket: WebSocket) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        ticker.tick().await;
        let state = crate::accounts_http::build_state().await;
        let Ok(msg) = serde_json::to_string(&state) else {
            continue;
        };
        if socket.send(AxMsg::Text(msg.into())).await.is_err() {
            break; // client disconnected
        }
    }
}

async fn bridge_ws(client: WebSocket, query: String) {
    let url = format!("ws://{}/api/live{}", upstream_base(), query);
    let upstream = match tokio_tungstenite::connect_async(&url).await {
        Ok((stream, _resp)) => stream,
        Err(e) => {
            tracing::warn!(error = %e, url = %url, "ws /api/live upstream connect failed");
            return;
        }
    };
    let (mut client_tx, mut client_rx) = client.split();
    let (mut up_tx, mut up_rx) = upstream.split();

    // browser → Bun
    let c2u = async {
        while let Some(Ok(msg)) = client_rx.next().await {
            let tg = match msg {
                AxMsg::Text(t) => TgMsg::Text(t.to_string()),
                AxMsg::Binary(b) => TgMsg::Binary(b.to_vec()),
                AxMsg::Ping(b) => TgMsg::Ping(b.to_vec()),
                AxMsg::Pong(b) => TgMsg::Pong(b.to_vec()),
                AxMsg::Close(_) => break,
            };
            if up_tx.send(tg).await.is_err() {
                break;
            }
        }
        let _ = up_tx.close().await;
    };

    // Bun → browser
    let u2c = async {
        while let Some(Ok(msg)) = up_rx.next().await {
            let ax = match msg {
                TgMsg::Text(t) => AxMsg::Text(t.into()),
                TgMsg::Binary(b) => AxMsg::Binary(b.into()),
                TgMsg::Ping(b) => AxMsg::Ping(b.into()),
                TgMsg::Pong(b) => AxMsg::Pong(b.into()),
                TgMsg::Close(_) => break,
                TgMsg::Frame(_) => continue, // low-level; never yielded by a read stream
            };
            if client_tx.send(ax).await.is_err() {
                break;
            }
        }
        let _ = client_tx.close().await;
    };

    // Whichever side closes first tears down the other.
    tokio::select! {
        _ = c2u => {}
        _ = u2c => {}
    }
}
