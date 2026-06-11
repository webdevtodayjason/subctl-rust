//! SSE-through-proxy head-grace test (W6 row ①).
//!
//! Root cause (reproduced byte-level against live :8787 on 2026-06-11): Bun
//! (the v3 upstream) finalizes a streaming response's HEAD only when the
//! first body chunk is enqueued. `/api/update/events` enqueues nothing at
//! open (first chunk = the 15s keep-alive tick), so its head sits on the
//! wire UNTERMINATED — status + handler headers, no `Date`, no framing
//! header, no final blank line. curl leniently *displays* the partial head
//! (which is why direct probes looked fine), but hyper/reqwest correctly
//! wait for the terminator, so the proxied browser saw nothing for 15s.
//! Streams that emit a frame immediately (`/api/notifications/stream`) get a
//! complete chunked head from Bun and always worked through the proxy.
//!
//! The fix under test: SSE-accepting GETs wait `SSE_HEAD_GRACE` (750ms) for
//! the upstream head, then synthesize the SSE head + a proof-of-life comment
//! frame and splice the upstream body through when it finally arrives.
//!
//! Scenarios (sequential in ONE test fn — they repoint the process-global
//! `EVY_PROXY_UPSTREAM` between mock upstreams, same precedent as
//! `master_alias_fallthrough_integration`'s own-binary isolation):
//!   1. Lazy Bun head (terminator + first event at t=2s) + SSE accept →
//!      synthesized 200 `text/event-stream` + comment frame well before t=2s,
//!      then the real event splices through.
//!   2. Complete head, immediate first frame + SSE accept → faithful mirror
//!      (real upstream headers pass through, no synthesis).
//!   3. Slow but complete JSON head (t=2s) + `Accept: */*` → NO synthesis;
//!      the client gets the real status/body after 2s (non-SSE requests are
//!      untouched by the grace logic).

use std::sync::Arc;
use std::time::Duration;

use evy_comms::{EventBroadcaster, HttpConfig, HttpServer, StubAppState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// What live Bun writes at open for a frame-less SSE stream: an UNTERMINATED
/// header block (captured byte-for-byte from :8787 `/api/update/events`).
const LAZY_PARTIAL_HEAD: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n";

/// What Bun appends when the first chunk finally flushes: the rest of the
/// head (close-delimited here; framing choice is reqwest's concern either
/// way) and the first event.
const LAZY_HEAD_COMPLETION: &[u8] = b"\r\ndata: {\"phase\":\"idle\"}\n\n";

/// A well-behaved SSE response: complete head + immediate comment frame
/// (what `/api/notifications/stream` sends).
const EAGER_SSE_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache, no-transform\r\n\r\n: stream open\n\n";

/// A slow-but-complete JSON response (head arrives whole, just late).
const SLOW_JSON_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";

/// Mock upstream: accept one connection at a time, read the request head,
/// write `first` immediately, then `rest` after `delay`, then hold open.
async fn spawn_mock_upstream(
    first: &'static [u8],
    rest: &'static [u8],
    delay: Duration,
) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().expect("mock upstream addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut seen: Vec<u8> = Vec::new();
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            seen.extend_from_slice(&buf[..n]);
                            if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let _ = sock.write_all(first).await;
                let _ = sock.flush().await;
                if !rest.is_empty() {
                    tokio::time::sleep(delay).await;
                    let _ = sock.write_all(rest).await;
                    let _ = sock.flush().await;
                }
                // Keep the stream open like a real SSE source.
                tokio::time::sleep(Duration::from_secs(60)).await;
            });
        }
    });
    addr
}

/// Read from `sock` until `needle` appears in the accumulated bytes or
/// `budget` elapses. Returns (matched, accumulated).
async fn read_until(sock: &mut TcpStream, needle: &[u8], budget: Duration) -> (bool, Vec<u8>) {
    let mut acc: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return (false, acc);
        }
        match tokio::time::timeout(remaining, sock.read(&mut buf)).await {
            Err(_) => return (false, acc), // budget exhausted
            Ok(Ok(0)) | Ok(Err(_)) => return (false, acc),
            Ok(Ok(n)) => {
                acc.extend_from_slice(&buf[..n]);
                if acc.windows(needle.len()).any(|w| w == needle) {
                    return (true, acc);
                }
            }
        }
    }
}

async fn raw_get(addr: std::net::SocketAddr, path: &str, accept: &str) -> TcpStream {
    let mut sock = TcpStream::connect(addr).await.expect("connect v4");
    sock.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: {accept}\r\n\r\n").as_bytes(),
    )
    .await
    .expect("send request");
    sock
}

#[tokio::test]
async fn sse_head_grace_scenarios() {
    let broadcaster = EventBroadcaster::new(64);
    let server = HttpServer::new(HttpConfig::ephemeral(), broadcaster, Arc::new(StubAppState));
    let bound = server.bind().await.expect("bind ephemeral");
    let addr = bound.local_addr();
    let shutdown = CancellationToken::new();
    let st = shutdown.clone();
    tokio::spawn(async move {
        let _ = bound.serve(st).await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // ── Scenario 1: lazy Bun head + SSE accept → synthesized head fast ──
    let upstream = spawn_mock_upstream(
        LAZY_PARTIAL_HEAD,
        LAZY_HEAD_COMPLETION,
        Duration::from_secs(2),
    )
    .await;
    // SAFETY: only test in this binary — no concurrent env access.
    unsafe {
        std::env::set_var("EVY_PROXY_UPSTREAM", upstream.to_string());
    }

    let mut sock = raw_get(addr, "/api/update/events", "text/event-stream").await;
    // Head + proof-of-life comment must arrive while the upstream head is
    // still unterminated (grace is 750ms; the upstream completes at 2s;
    // asserted at 1.5s to be CI-noise-proof but strictly before 2s).
    let (got_open, head_bytes) =
        read_until(&mut sock, b": v4-proxy open", Duration::from_millis(1500)).await;
    let head_text = String::from_utf8_lossy(&head_bytes).to_string();
    assert!(
        got_open,
        "synthesized SSE head + comment frame did not arrive within grace+margin \
         (got {} bytes: {head_text:?})",
        head_bytes.len(),
    );
    assert!(
        head_text.starts_with("HTTP/1.1 200"),
        "expected synthesized 200 head, got: {head_text:?}",
    );
    assert!(
        head_text
            .to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "synthesized head must declare text/event-stream, got: {head_text:?}",
    );
    // The upstream's first real event (t=2s) must splice through.
    let (got_event, body_bytes) =
        read_until(&mut sock, b"\"phase\":\"idle\"", Duration::from_secs(4)).await;
    assert!(
        got_event,
        "upstream event did not splice through after its head completed (got: {:?})",
        String::from_utf8_lossy(&body_bytes),
    );
    drop(sock);

    // ── Scenario 2: eager SSE upstream → faithful mirror, no synthesis ──
    let upstream = spawn_mock_upstream(EAGER_SSE_RESPONSE, b"", Duration::ZERO).await;
    unsafe {
        std::env::set_var("EVY_PROXY_UPSTREAM", upstream.to_string());
    }
    let mut sock = raw_get(addr, "/api/notifications/stream", "text/event-stream").await;
    let (got_frame, bytes) =
        read_until(&mut sock, b": stream open", Duration::from_millis(1500)).await;
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(
        got_frame,
        "eager SSE upstream must mirror promptly (got: {text:?})"
    );
    assert!(
        text.to_ascii_lowercase()
            .contains("cache-control: no-cache, no-transform"),
        "faithful path must pass real upstream headers through, got: {text:?}",
    );
    assert!(
        !text.contains("v4-proxy open"),
        "fast-completing upstream head must NOT be synthesized, got: {text:?}",
    );
    drop(sock);

    // ── Scenario 3: slow JSON head (t=1s, past grace) + Accept: */* →
    // grace logic not applied; the real response mirrors through late. ──
    let upstream = spawn_mock_upstream(b"", SLOW_JSON_RESPONSE, Duration::from_secs(1)).await;
    unsafe {
        std::env::set_var("EVY_PROXY_UPSTREAM", upstream.to_string());
    }
    let mut sock = raw_get(addr, "/api/some/json", "*/*").await;
    let (got_json, bytes) = read_until(&mut sock, b"{\"ok\":true}", Duration::from_secs(3)).await;
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(
        got_json,
        "plain JSON GET must mirror through (got: {text:?})"
    );
    assert!(
        text.to_ascii_lowercase()
            .contains("content-type: application/json"),
        "non-SSE request must keep its real content-type, got: {text:?}",
    );
    assert!(
        !text.contains("v4-proxy open"),
        "non-SSE accept must never receive a synthesized SSE head, got: {text:?}",
    );

    shutdown.cancel();
    // SAFETY: still the only test in this binary.
    unsafe {
        std::env::remove_var("EVY_PROXY_UPSTREAM");
    }
}
