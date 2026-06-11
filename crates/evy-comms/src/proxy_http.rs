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
//! Streaming is passed through chunk-by-chunk. WebSocket upgrades can't ride
//! `reqwest`, so they get dedicated bridges: [`ws_proxy_handler`] (`/api/live`)
//! and `terminal_ws` (`/api/terminal/attach`).
//!
//! ## SSE head grace (W6 row ①)
//!
//! Bun (the v3 upstream) finalizes a streaming response's head only when the
//! FIRST body chunk is enqueued — until then the bytes on the wire are an
//! unterminated header block (status + handler headers, no `Date`, no
//! framing header, no blank line). `/api/update/events` enqueues nothing at
//! open (its first chunk is the 15s keep-alive tick), so a spec-compliant
//! client like `reqwest`/hyper correctly sits waiting for the head terminator
//! and the proxied browser sees NOTHING for up to 15s. (Endpoints that emit a
//! frame immediately, like `/api/notifications/stream`, get a complete
//! chunked head from Bun and always worked through the proxy.)
//!
//! The fix: a GET whose `Accept` includes `text/event-stream` (exactly what
//! `EventSource` sends) waits [`SSE_HEAD_GRACE`] for the upstream head. If it
//! completes in time, the response is mirrored faithfully (real status, real
//! headers — same as every other proxied request). If not, the upstream is
//! assumed to be a lazy-headed Bun SSE stream: we synthesize the SSE head
//! immediately, emit a comment frame as proof-of-life, and splice the
//! upstream body through once its head finally completes. The synthesized
//! path drops the upstream's eventual status/headers (it already answered
//! 200 `text/event-stream`); if the upstream later errors, the body stream
//! errors out and the client's `EventSource` reconnects.

use std::sync::OnceLock;

use axum::{
    body::{Body, Bytes},
    extract::ws::{Message as AxMsg, WebSocket, WebSocketUpgrade},
    extract::{RawQuery, Request},
    http::{header, HeaderMap, HeaderName, Method, StatusCode},
    response::{IntoResponse, Json, Response},
    BoxError,
};
use futures::{future::BoxFuture, stream, SinkExt, StreamExt, TryStreamExt};
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

/// How long an SSE-accepting GET waits for the upstream's response head
/// before synthesizing one (see module docs). Local Bun answers complete
/// heads in ~1ms; this only trips on lazy-headed streams whose first chunk
/// is seconds away.
const SSE_HEAD_GRACE: std::time::Duration = std::time::Duration::from_millis(750);

/// `EventSource` sends `Accept: text/event-stream` (WHATWG spec); that — on a
/// GET — is the only pre-head signal that the caller wants a live stream.
fn accepts_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"))
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

    let sse_get = method == Method::GET && accepts_event_stream(&fwd_headers);

    let send_fut = client()
        .request(method, &url)
        .headers(fwd_headers)
        .body(body_bytes)
        .send();

    if !sse_get {
        return mirror_upstream(send_fut.await);
    }

    // SSE-accepting GET: give the upstream head a short grace window, then
    // assume a lazy-headed Bun stream and answer for it (module docs).
    let mut send_fut: BoxFuture<'static, Result<reqwest::Response, reqwest::Error>> =
        Box::pin(send_fut);
    match tokio::time::timeout(SSE_HEAD_GRACE, send_fut.as_mut()).await {
        Ok(upstream) => mirror_upstream(upstream),
        Err(_elapsed) => synthesized_sse_response(send_fut),
    }
}

/// Faithful mirror of the upstream response: real status, non-hop-by-hop
/// headers, body streamed through chunk-by-chunk (SSE passthrough).
fn mirror_upstream(upstream: Result<reqwest::Response, reqwest::Error>) -> Response {
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
    let stream = resp.bytes_stream();
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|e| bad_gateway(&format!("build proxied response: {e}")))
}

/// The upstream accepted the connection but hasn't finished its response
/// head within [`SSE_HEAD_GRACE`] — the lazy-headed Bun SSE signature.
/// Answer the client NOW with a synthesized SSE head and a proof-of-life
/// comment frame (spec-legal, invisible to `EventSource`), then splice the
/// upstream's body through once its head finally completes. reqwest keeps
/// ownership of the transfer framing (de-chunking) exactly as on the
/// faithful path.
fn synthesized_sse_response(
    send_fut: BoxFuture<'static, Result<reqwest::Response, reqwest::Error>>,
) -> Response {
    let open_frame = stream::once(async {
        Ok::<Bytes, BoxError>(Bytes::from_static(
            b": v4-proxy open, upstream head pending\n\n",
        ))
    });
    let upstream_body = stream::once(async move {
        match send_fut.await {
            Ok(resp) => resp
                .bytes_stream()
                .map_err(|e| Box::new(e) as BoxError)
                .left_stream(),
            // Upstream died before completing its head: error the body so
            // the connection aborts and the client's EventSource retries.
            Err(e) => stream::once(async move { Err::<Bytes, BoxError>(Box::new(e) as BoxError) })
                .right_stream(),
        }
    })
    .flatten();

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(open_frame.chain(upstream_body)))
        .unwrap_or_else(|e| bad_gateway(&format!("build synthesized SSE response: {e}")))
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
