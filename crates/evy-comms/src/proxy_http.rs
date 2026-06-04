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
    extract::Request,
    http::{HeaderMap, HeaderName, StatusCode},
    response::{IntoResponse, Json, Response},
};
use serde_json::json;

/// Hop-by-hop headers (RFC 7230 §6.1) + host — never forwarded across a proxy hop.
const HOP_BY_HOP: &[&str] = &[
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
    "te", "trailers", "transfer-encoding", "upgrade", "host", "content-length",
];

fn upstream_base() -> String {
    std::env::var("EVY_PROXY_UPSTREAM").unwrap_or_else(|_| "127.0.0.1:8787".to_string())
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // No global timeout: SSE streams (/api/master/events) are long-lived.
            .build()
            .expect("build reverse-proxy reqwest client")
    })
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP.iter().any(|h| name.as_str().eq_ignore_ascii_case(h))
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
    (StatusCode::BAD_GATEWAY, Json(json!({ "ok": false, "error": msg }))).into_response()
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
