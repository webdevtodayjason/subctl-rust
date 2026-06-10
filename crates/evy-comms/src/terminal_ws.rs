//! Upgrade-capable WebSocket proxy for the web terminal attach path.
//!
//! Phase 0 left `/api/terminal/attach` riding the HTTP reverse proxy
//! ([`crate::proxy_http::reverse_proxy_handler`]), which is built on `reqwest`
//! and therefore **cannot complete a WebSocket upgrade** — a latent hole that
//! bites the moment the operator enables the terminal through the v4 front door
//! (`:8797`). This module closes it: it accepts the browser's WS upgrade
//! natively and splices frames to the still-authoritative v3 Bun dashboard's
//! `/api/terminal/attach`, dialed over a `tokio-tungstenite` client.
//!
//! It deliberately replicates the bridge pattern in
//! [`crate::proxy_http`]'s `/api/live` handler rather than reaching into it, so
//! the Phase 0 proxy internals stay untouched.
//!
//! ## The gate is NOT reimplemented
//!
//! The terminal's security gate (flag file `~/.config/subctl/terminal.enabled`,
//! the DNS-rebind host-header check, `team` validation, and tmux-session
//! existence) lives entirely on Bun — see `dashboard/terminal.ts`. There is no
//! HMAC on the terminal (the per-team `hmac.secret` belongs to ADR 0011
//! Layer 1, the signed-directive channel, not Layer 2's terminal — see
//! `TERMINAL_SPIKE.md`). Because we dial Bun **before** accepting the browser's
//! upgrade, a pre-upgrade rejection (Bun's `403`/`400`/`404` with the
//! `{ok:false,error}` body) is relayed back to the browser with Bun's exact
//! status and body, rather than degrading into a bare failed handshake.

use axum::{
    body::Body,
    extract::ws::{Message as AxMsg, WebSocket, WebSocketUpgrade},
    extract::RawQuery,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    tungstenite::{error::Error as WsError, protocol::Message as TgMsg},
    MaybeTlsStream, WebSocketStream,
};

/// Upstream Bun dashboard `host:port`. Mirrors `proxy_http`'s resolution so the
/// two proxy paths agree on where v3 lives (default `127.0.0.1:8787`, override
/// with `EVY_PROXY_UPSTREAM`).
fn upstream_base() -> String {
    std::env::var("EVY_PROXY_UPSTREAM").unwrap_or_else(|_| "127.0.0.1:8787".to_string())
}

/// `GET /api/terminal/attach` (WebSocket) — upgrade-capable proxy to Bun.
///
/// Dials Bun's `/api/terminal/attach` (preserving the `?team&cols&rows` query)
/// first: on a `101` we accept the browser's upgrade and splice frames both
/// ways; on Bun's pre-upgrade rejection we relay its exact status + body so the
/// v3-shape `{ok:false,error}` gate errors survive the extra hop.
pub(crate) async fn terminal_attach_handler(
    ws: WebSocketUpgrade,
    RawQuery(query): RawQuery,
) -> Response {
    let qs = query.unwrap_or_default();
    let url = if qs.is_empty() {
        format!("ws://{}/api/terminal/attach", upstream_base())
    } else {
        format!("ws://{}/api/terminal/attach?{qs}", upstream_base())
    };

    match tokio_tungstenite::connect_async(&url).await {
        // Bun completed the handshake (101) — hand the browser's socket and the
        // live upstream stream to the splice loop.
        Ok((upstream, _resp)) => ws.on_upgrade(move |client| splice(client, upstream)),
        // Bun answered the upgrade with a non-101 HTTP response (the gate's
        // 403/400/404). Relay its status + body verbatim.
        Err(WsError::Http(resp)) => relay_rejection(resp),
        // Transport-level failure (Bun down, refused, …). Surface as 502 in the
        // same `{ok:false,error}` shape the reverse proxy uses.
        Err(e) => {
            tracing::warn!(error = %e, url = %url, "terminal attach upstream dial failed");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "ok": false,
                    "error": format!("v3 dashboard (Bun) terminal unreachable: {e}"),
                })),
            )
                .into_response()
        }
    }
}

/// Turn Bun's non-101 upgrade response into an axum response, preserving the
/// status and JSON body so the browser sees the exact v3 gate error. We read
/// only the status code and raw body bytes (not the `http::Response` type
/// itself) to stay agnostic of `tungstenite`'s `http` version.
fn relay_rejection(resp: tokio_tungstenite::tungstenite::handshake::client::Response) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::FORBIDDEN);
    let body = resp.into_body().unwrap_or_default();
    (
        status,
        // Bun's terminal rejections are always JSON; pin the type so the browser
        // parses `{ok:false,error}` rather than guessing.
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(body),
    )
        .into_response()
}

/// Splice frames between the browser socket and the upstream Bun socket until
/// either side closes. Frame **types are preserved** (text↔text, binary↔binary,
/// ping/pong passed through) so the terminal's wire contract survives — JSON
/// text frames flow browser→Bun, raw binary pty bytes flow Bun→browser.
async fn splice(client: WebSocket, upstream: WebSocketStream<MaybeTlsStream<TcpStream>>) {
    let (mut client_tx, mut client_rx) = client.split();
    let (mut up_tx, mut up_rx) = upstream.split();

    // browser → Bun: `{"type":"data","b64":…}` / `{"type":"resize",…}` text frames.
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

    // Bun → browser: raw pty bytes as binary frames (xterm.js renders natively).
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
