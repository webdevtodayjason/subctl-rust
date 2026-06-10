//! Fall-through test for the scoped `/api/master/*` → `/api/evy/*` rewrite.
//!
//! The rewrite is scoped to the chat-tab surfaces v4 owns natively. A master
//! path OUTSIDE that claimed set (here `/api/master/diag`) must be left
//! UNTOUCHED and ride the `/api/{*rest}` catch-all reverse-proxy — carrying
//! the ORIGINAL `/api/master/diag` path to v3, which does its own rename. This
//! proves the layer neither swallows unclaimed paths nor rewrites what v4 does
//! not own.
//!
//! Lives in its OWN test binary because it pins `EVY_PROXY_UPSTREAM` at a
//! closed port (`127.0.0.1:1`) so the proxy deterministically yields 502 —
//! load-bearing here, since a real v3 Bun dashboard on the default
//! `127.0.0.1:8787` actually answers `/api/master/diag` with 200. A
//! process-global `set_var` would race the parallel tests in
//! `http_integration.rs`; isolating it to a single-test binary keeps it
//! race-free (same precedent as `preferences_http_integration.rs`).

use std::sync::Arc;
use std::time::Duration;

use evy_comms::{EventBroadcaster, HttpConfig, HttpServer, StubAppState};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn unclaimed_master_path_falls_through_to_proxy_unrewritten() {
    // Pin the proxy upstream at a guaranteed-closed port so the catch-all
    // yields 502 (connection refused) rather than reaching a live dashboard.
    // SAFETY: this is the only test in this binary, so no other thread reads
    // or writes the process environment concurrently.
    unsafe {
        std::env::set_var("EVY_PROXY_UPSTREAM", "127.0.0.1:1");
    }

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
    let base = format!("http://{addr}");

    // `/api/master/diag` is NOT in the claimed chat-tab set. It is left
    // unrewritten, matches no specific route, and rides the `/api/{*rest}`
    // catch-all proxy → the (closed) upstream → 502. A 404 would mean the
    // rewrite swallowed it; a 200 would mean it was served natively.
    let res = reqwest::get(format!("{base}/api/master/diag"))
        .await
        .expect("GET /api/master/diag");
    assert_eq!(
        res.status(),
        502,
        "unclaimed master path must fall through to the proxy carrying /api/master/diag",
    );

    shutdown.cancel();

    // SAFETY: still the only test in this binary; safe to unset.
    unsafe {
        std::env::remove_var("EVY_PROXY_UPSTREAM");
    }
}
