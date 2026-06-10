//! `evy-comms` — multi-channel router for Evy v4.
//!
//! Phase 2 Slices 2B1 + 2B2 land the **HTTP / SSE** and **Telegram**
//! channels. TUI and Discord come in Phase 3.
//!
//! # HTTP / SSE (2B1)
//!
//! axum-based HTTP server bound at `127.0.0.1:8787` (configurable)
//! exposing the operator console's read-only surface plus a Server-Sent
//! Events stream of [`DaemonEvent`]s for live dashboards.
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
//! | POST | `/api/evy/chat` | operator chat turn → Evy's reply ([`ChatRequest`] → [`ChatResponse`]) |
//! | GET | `/api/master/*` | URI-rewrite alias for `/api/evy/*` (legacy curl recipes) |
//!
//! # Telegram (2B2)
//!
//! Telegram Bot API bridge with outbound notifications, inbound message
//! dispatch, and an ask-round-trip lifecycle (Evy posts a question →
//! operator replies → bridge resolves the pending ask).
//!
//! # Quick start (HTTP/SSE)
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

#![warn(missing_docs)]

// ── Slice 2B1: HTTP / SSE ────────────────────────────────────────────
pub mod config;
pub mod error;
pub mod events;
pub mod http;
pub mod sse;

// ── Slice 2B2: Telegram ──────────────────────────────────────────────
pub mod ask;
pub mod notification;
pub mod telegram;

// ── Phase 3 Slice B: Discord ─────────────────────────────────────────
pub mod discord;
pub mod discord_config;

// ── Phase 6 Slice: chat surface ──────────────────────────────────────
pub mod chat;

// ── Phase 6 follow-up: TUI-driving endpoints ─────────────────────────
pub mod sessions_http;
pub mod skills_http;

// ── P2: transcript + context meter (v3-shape) ────────────────────────
pub mod transcript_http;

// ── Cutover Phase 0: reverse-proxy fallback to v3 Bun + native /api/host ──
pub mod proxy_http;

// ── Cutover Phase 1: dashboard state synthesis (verdict, rate-limits, /api/state) ──
pub mod dashboard_state;
// ── Cutover Phase 1 slice 1c: 3-layer usage cache (shells `subctl usage --json`) ──
pub mod usage_cache;
// ── Cutover Phase 1 slice 1d: rate-limit + usage-history 24h buckets ──
pub mod rate_limits;
// ── Cutover Phase 1 slice 1e: /api/evy/accounts (integrated) ──
pub mod accounts_http;
// ── Cutover Phase 2 slice 2m: team-template CRUD ──
pub mod teams_http;
// ── Cutover — native settings/auth-status/update-status read surface ──
pub mod preferences_http;
// ── Cutover: native projects CRUD + policy-preset surface ──
pub mod projects_http;

// ── Public re-exports — the surface the daemon binary consumes ───────

pub use config::{HttpConfig, DEFAULT_HOST, DEFAULT_PORT};
pub use error::{CommsError, Result};
pub use events::DaemonEvent;
pub use http::{
    AppState, BoundHttpServer, HttpServer, JobSummary, OrchestrationCapture, OrchestrationRow,
    SpawnError, SpawnRequest, StubAppState, WorkerSummary,
};
pub use sse::EventBroadcaster;

pub use ask::{Ask, AskId, AskRegistry};
pub use notification::Notification;
pub use telegram::{InboundMessage, TelegramBridge, TelegramConfig};

pub use discord::{render_embed, DiscordBridge, Embed, EmbedField};
pub use discord_config::DiscordConfig;

pub use chat::{ChatError, ChatRequest, ChatResponse, ChatStreamEvent};

pub use sessions_http::{SessionSummary, SessionsError, SessionsListResponse};
pub use skills_http::{SkillSummary, SkillsError, SkillsListResponse};
