//! `evy-comms` — multi-channel router for Evy v4.
//!
//! Phase 2 Slice 2B1 lands the **HTTP / SSE channel**: an axum-based
//! HTTP server bound at `127.0.0.1:8787` (configurable) exposing the
//! operator console's read-only surface plus a Server-Sent Events
//! stream of [`DaemonEvent`]s for live dashboards.
//!
//! Routes ported from v3's `dashboard/server.ts` (the operator-facing
//! subset only — fitness / engagement / pending-asks panels are
//! intentionally NOT carried forward per ADR 0020):
//!
//! | Method | Path | Returns |
//! |--------|------|---------|
//! | GET | `/health` | `{ "ok": true, "version": "<ver>" }` |
//! | GET | `/api/version` | `{ "version": "<ver>" }` |
//! | GET | `/api/evy/events` | SSE stream of [`DaemonEvent`] (`text/event-stream`) |
//! | GET | `/api/evy/workers` | JSON list of [`WorkerSummary`] |
//! | GET | `/api/evy/scheduler/jobs` | JSON list of [`JobSummary`] |
//! | GET | `/api/evy/policy` | the loaded [`evy_policy::Policy`] as JSON |
//! | GET | `/api/master/*` | URI-rewrite alias for `/api/evy/*` (legacy curl recipes) |
//!
//! # Quick start
//!
//! ```no_run
//! use std::sync::Arc;
//! use evy_comms::{
//!     EventBroadcaster, HttpConfig, HttpServer, StubAppState,
//! };
//! use tokio_util::sync::CancellationToken;
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let broadcaster = EventBroadcaster::default();
//! let server = HttpServer::new(
//!     HttpConfig::default(),
//!     broadcaster.clone(),
//!     Arc::new(StubAppState),
//! );
//! let shutdown = CancellationToken::new();
//! // emit events from elsewhere in the daemon via `broadcaster.emit(...)`
//! server.serve(shutdown).await?;
//! # Ok(()) }
//! ```
//!
//! # Other channels
//!
//! Telegram, TUI, and Discord are sibling modules added by separate
//! workers in later slices. The router's channel-agnostic core surface
//! lives in `evy-core::router` (Phase 2.5).

#![warn(missing_docs)]

pub mod config;
pub mod error;
pub mod events;
pub mod http;
pub mod sse;

// ── Public re-exports — the surface the daemon binary consumes ───────

pub use config::{HttpConfig, DEFAULT_HOST, DEFAULT_PORT};
pub use error::{CommsError, Result};
pub use events::DaemonEvent;
pub use http::{AppState, BoundHttpServer, HttpServer, JobSummary, StubAppState, WorkerSummary};
pub use sse::EventBroadcaster;
