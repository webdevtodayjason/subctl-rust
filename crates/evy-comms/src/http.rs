//! axum HTTP server: routes + state + graceful shutdown.
//!
//! The operator console talks to the daemon over this surface. Routes
//! mirror v3's `dashboard/server.ts` for the *operator-facing* subset
//! (sessions list, health, event stream); speculative panels (fitness /
//! engagement / pending-asks) are intentionally NOT ported per ADR 0020.
//!
//! `/api/master/*` is preserved as an alias for `/api/evy/*` so v3-era
//! curl recipes continue to work. v3 implemented the alias as a URI
//! rewrite in `dashboard/server.ts` line 2920; we register both prefixes
//! on the same handler functions because axum 0.8's `Router::layer()`
//! wraps individual route handlers (not the path matcher), so
//! middleware can't change which handler dispatches.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    extract::State,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
    Router,
};
use evy_core::{MandateId, ProviderKind, WorkerId, WorkerStatus};
use evy_policy::Policy;
use evy_scheduler::{JobAction, JobId};
use evy_skills::SkillRegistry;
use evy_thinking::ThinkingPartner;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::config::HttpConfig;
use crate::error::{CommsError, Result};
use crate::sse::{into_sse_response, EventBroadcaster};

// ─── public state surface ─────────────────────────────────────────────

/// The daemon's read-only surface as the HTTP layer sees it.
///
/// The HTTP server does NOT mutate daemon state; the operator dashboard
/// reads workers / jobs / policy and observes events via SSE. Mutations
/// (start/stop a worker, register a job) land on a separate command
/// channel in a later slice.
///
/// Phase 2 ships [`StubAppState`] returning empty / default data; the
/// daemon binary swaps in a real `Arc<dyn AppState>` once 2A's daemon
/// wiring is extended to publish worker / job state into evy-comms.
#[async_trait]
pub trait AppState: Send + Sync + 'static {
    /// Snapshot of currently-registered workers, oldest first.
    async fn workers(&self) -> Vec<WorkerSummary>;

    /// Snapshot of currently-registered scheduler jobs, oldest first.
    async fn jobs(&self) -> Vec<JobSummary>;

    /// The currently-loaded policy. Cheap to clone for serialization.
    async fn policy(&self) -> Policy;

    /// Phase 6 — return the daemon's thinking-partner if one is
    /// configured. The chat handler at `POST /api/evy/chat` returns 503
    /// when this is `None`.
    ///
    /// Default impl returns `None` so existing implementations
    /// (notably [`StubAppState`]) keep compiling without change.
    fn thinking_partner(&self) -> Option<Arc<ThinkingPartner>> {
        None
    }

    /// Phase 6 — the skill registry attached to the partner. Returned
    /// by the chat handler in the `skills_loaded` field so the operator
    /// can see which skills the model could see this turn.
    ///
    /// Default impl returns `None`.
    fn skills(&self) -> Option<Arc<SkillRegistry>> {
        None
    }

    /// P2 — display label for the active supervisor model
    /// (e.g. `"lm-studio/gemma-4-26b-a4b-it-mlx"`), surfaced in the
    /// `/api/evy/context` meter. Default `None` (renders as `null`); the
    /// daemon overrides it from `[thinking_partner]` config.
    fn supervisor_label(&self) -> Option<String> {
        None
    }
}

/// Operator-console-shaped projection of an evy-core `WorkerHandle`.
///
/// Narrow on purpose: serialization-stable shape for the dashboard. The
/// underlying `WorkerHandle` trait is dyn-only and not directly
/// serializable, so the AppState impl is the natural place to flatten.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerSummary {
    /// Stable worker id.
    pub id: WorkerId,
    /// Which provider produced it.
    pub provider: ProviderKind,
    /// The mandate the worker is fulfilling.
    pub mandate_id: MandateId,
    /// Latest lifecycle state observed by the daemon.
    pub status: WorkerStatus,
}

/// Operator-console-shaped projection of an evy-scheduler `Job`.
///
/// Narrow on purpose: the dashboard wants name + cron + action-kind +
/// enabled flag. Operators don't need to see the full `JobAction`
/// payload over JSON; if they do, an `action_detail` field can land
/// later.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobSummary {
    /// Stable job id.
    pub id: JobId,
    /// Human-readable job name.
    pub name: String,
    /// 5-field cron expression.
    pub cron_expr: String,
    /// Short tag for the action kind (`"log_heartbeat"`, etc.).
    pub action_kind: String,
    /// Whether the job is currently armed.
    pub enabled: bool,
}

impl JobSummary {
    /// Short tag for a `JobAction`'s variant. Stable string the
    /// dashboard can switch on without parsing the full action payload.
    #[must_use]
    pub fn action_kind_tag(action: &JobAction) -> &'static str {
        match action {
            JobAction::DispatchMandate(_) => "dispatch_mandate",
            JobAction::LogHeartbeat => "log_heartbeat",
            JobAction::InvokeShell(_) => "invoke_shell",
        }
    }
}

/// Stub `AppState` returning empty / default data. Use until the
/// daemon binary wires the real state surface into evy-comms.
#[derive(Debug, Default, Clone)]
pub struct StubAppState;

#[async_trait]
impl AppState for StubAppState {
    async fn workers(&self) -> Vec<WorkerSummary> {
        Vec::new()
    }

    async fn jobs(&self) -> Vec<JobSummary> {
        Vec::new()
    }

    async fn policy(&self) -> Policy {
        Policy::default()
    }
}

// ─── public server surface ────────────────────────────────────────────

/// HTTP server, pre-bind.
///
/// Construct with [`HttpServer::new`], then either call [`HttpServer::serve`]
/// (binds + serves until shutdown) or [`HttpServer::bind`] (binds, hands
/// back a [`BoundHttpServer`] so the caller can read `local_addr()`).
///
/// Tests typically reach for `bind()` so they can connect to the ephemeral
/// port the kernel chose; production reaches for `serve()`.
pub struct HttpServer {
    config: HttpConfig,
    broadcaster: EventBroadcaster,
    state: Arc<dyn AppState>,
}

impl HttpServer {
    /// Build a new server. Defers binding until [`serve`](Self::serve)
    /// or [`bind`](Self::bind) is called.
    #[must_use]
    pub fn new(
        config: HttpConfig,
        broadcaster: EventBroadcaster,
        state: Arc<dyn AppState>,
    ) -> Self {
        Self {
            config,
            broadcaster,
            state,
        }
    }

    /// Convenience: build with [`StubAppState`].
    #[must_use]
    pub fn with_stub_state(config: HttpConfig, broadcaster: EventBroadcaster) -> Self {
        Self::new(config, broadcaster, Arc::new(StubAppState))
    }

    /// Bind to the configured address and return the bound server so
    /// the caller can read [`BoundHttpServer::local_addr`] before
    /// starting to serve.
    ///
    /// # Errors
    /// Returns [`CommsError::Bind`] if the TCP listener cannot be
    /// created (port in use, permission denied, etc.) or
    /// [`CommsError::Cors`] if any allow-origin is not a valid
    /// `HeaderValue`.
    pub async fn bind(self) -> Result<BoundHttpServer> {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|source| CommsError::Bind {
                addr: addr.clone(),
                source,
            })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| CommsError::Bind { addr, source })?;
        let router = build_router(
            self.broadcaster,
            self.state,
            &self.config.allow_origins,
            self.config.static_dir.as_deref(),
        )?;
        Ok(BoundHttpServer {
            listener,
            local_addr,
            router,
        })
    }

    /// Bind and serve until `shutdown` fires. The trigger is a
    /// [`CancellationToken`]; the server's accept loop exits as soon
    /// as the token is cancelled and any in-flight requests are
    /// allowed to finish.
    ///
    /// # Errors
    /// Returns the same errors as [`bind`](Self::bind), plus
    /// [`CommsError::Serve`] if axum's accept loop fails.
    pub async fn serve(self, shutdown: CancellationToken) -> Result<()> {
        let bound = self.bind().await?;
        bound.serve(shutdown).await
    }
}

/// Bound (but not yet serving) HTTP server. Holds a live `TcpListener`
/// and the constructed `Router`.
///
/// Typical test pattern:
///
/// ```ignore
/// let server = HttpServer::with_stub_state(HttpConfig::ephemeral(), EventBroadcaster::default());
/// let bound = server.bind().await?;
/// let addr = bound.local_addr();
/// let shutdown = CancellationToken::new();
/// let handle = tokio::spawn(bound.serve(shutdown.clone()));
/// // ... hit http://addr/health, run assertions ...
/// shutdown.cancel();
/// handle.await??;
/// ```
pub struct BoundHttpServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    router: Router,
}

impl BoundHttpServer {
    /// The address the OS actually bound. When the config requested
    /// port 0, this exposes the ephemeral port the kernel assigned.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serve requests until `shutdown` fires.
    ///
    /// # Errors
    /// Returns [`CommsError::Serve`] if axum's `serve` returns an
    /// error (typically only on listener-level failures during shutdown).
    pub async fn serve(self, shutdown: CancellationToken) -> Result<()> {
        tracing::info!(local_addr = %self.local_addr, "evy-comms HTTP server starting");
        axum::serve(self.listener, self.router)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
            .map_err(|source| CommsError::Serve { source })?;
        tracing::info!(local_addr = %self.local_addr, "evy-comms HTTP server stopped cleanly");
        Ok(())
    }
}

// ─── routing + handlers ───────────────────────────────────────────────

/// Per-request state passed to every handler via axum's [`State`]
/// extractor.
///
/// `pub(crate)` so sibling modules (`chat`) can extract it without
/// re-declaring; the type is still not exposed to downstream crates.
#[derive(Clone)]
pub(crate) struct HttpState {
    pub(crate) broadcaster: EventBroadcaster,
    pub(crate) app: Arc<dyn AppState>,
    pub(crate) version: &'static str,
}

/// Workspace version of `evy-comms`. The dashboard exposes this on
/// `/api/version` and `/health` so operators can see what's running.
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn build_router(
    broadcaster: EventBroadcaster,
    app: Arc<dyn AppState>,
    allow_origins: &[String],
    static_dir: Option<&Path>,
) -> Result<Router> {
    let state = HttpState {
        broadcaster,
        app,
        version: VERSION,
    };

    let cors = build_cors_layer(allow_origins)?;

    // Each operator-facing route is registered under both the canonical
    // `/api/evy/...` prefix and the legacy `/api/master/...` alias.
    // axum 0.8's `Router::layer()` applies middleware to each matched
    // route rather than wrapping the path matcher, so URI-rewriting in
    // middleware doesn't change which handler dispatches — we register
    // the alias path explicitly instead. Mirrors v3 dashboard's
    // operator-visible behaviour (line 2920 of `dashboard/server.ts`)
    // even though the rewrite happens in routing rather than a layer.
    let mut router = Router::new()
        .route("/health", get(health_handler))
        .route("/api/version", get(version_handler))
        .route("/api/evy/events", get(events_handler))
        .route("/api/master/events", get(events_handler))
        .route("/api/evy/workers", get(workers_handler))
        .route("/api/master/workers", get(workers_handler))
        .route("/api/evy/scheduler/jobs", get(jobs_handler))
        .route("/api/master/scheduler/jobs", get(jobs_handler))
        .route("/api/evy/policy", get(policy_handler))
        .route("/api/master/policy", get(policy_handler))
        // Phase 6 chat surface — POST only; aliased under /api/master
        // for parity with v3 dashboards even though there's no v3
        // equivalent (legacy operators may script either prefix).
        .route("/api/evy/chat", post(crate::chat::chat_handler))
        .route("/api/master/chat", post(crate::chat::chat_handler))
        // Phase 6 follow-up — TUI-driving endpoints. Aliased under
        // /api/master for parity with the other operator routes.
        .route(
            "/api/evy/sessions",
            get(crate::sessions_http::sessions_list_handler),
        )
        .route(
            "/api/master/sessions",
            get(crate::sessions_http::sessions_list_handler),
        )
        .route(
            "/api/evy/sessions/{id}",
            delete(crate::sessions_http::sessions_delete_handler),
        )
        .route(
            "/api/master/sessions/{id}",
            delete(crate::sessions_http::sessions_delete_handler),
        )
        .route(
            "/api/evy/skills",
            get(crate::skills_http::skills_list_handler),
        )
        .route(
            "/api/master/skills",
            get(crate::skills_http::skills_list_handler),
        )
        // P2 — transcript + context meter, in the v3 dashboard wire shape.
        .route(
            "/api/evy/transcript",
            get(crate::transcript_http::transcript_handler),
        )
        .route(
            "/api/master/transcript",
            get(crate::transcript_http::transcript_handler),
        )
        .route(
            "/api/evy/context",
            get(crate::transcript_http::context_handler),
        )
        .route(
            "/api/master/context",
            get(crate::transcript_http::context_handler),
        )
        .route(
            "/api/evy/transcript/util",
            get(crate::transcript_http::transcript_util_handler),
        )
        .route(
            "/api/master/transcript/util",
            get(crate::transcript_http::transcript_util_handler),
        )
        // P3 — compaction + clear write paths (archive to disk + persist).
        .route(
            "/api/evy/transcript/compact",
            post(crate::transcript_http::compact_handler),
        )
        .route(
            "/api/master/transcript/compact",
            post(crate::transcript_http::compact_handler),
        )
        .route(
            "/api/evy/transcript/clear",
            post(crate::transcript_http::clear_handler),
        )
        .route(
            "/api/master/transcript/clear",
            post(crate::transcript_http::clear_handler),
        );

    // Phase 4 Slice A: optionally serve the operator-console static
    // bundle as a fallback. ServeDir resolves `index.html` automatically
    // on `GET /`, so the SPA shell loads without extra wiring. The
    // fallback rank means API routes always win — only unmatched paths
    // fall through to the static surface.
    if let Some(dir) = static_dir {
        tracing::info!(static_dir = %dir.display(), "evy-comms serving operator console");
        router = router.fallback_service(ServeDir::new(dir));
    }

    let router = router
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    Ok(router)
}

fn build_cors_layer(allow_origins: &[String]) -> Result<CorsLayer> {
    if allow_origins.is_empty() {
        // No origins → permissive-disabled CORS. `tower-http`'s default
        // is "do nothing"; callers without a browser are unaffected.
        return Ok(CorsLayer::new());
    }
    let mut parsed = Vec::with_capacity(allow_origins.len());
    for origin in allow_origins {
        let hv: HeaderValue = origin
            .parse()
            .map_err(|_| CommsError::Cors(origin.clone()))?;
        parsed.push(hv);
    }
    Ok(CorsLayer::new()
        .allow_origin(AllowOrigin::list(parsed))
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any))
}

// ── individual handlers ──

#[derive(Serialize)]
struct HealthBody {
    ok: bool,
    version: &'static str,
}

async fn health_handler(State(state): State<HttpState>) -> impl IntoResponse {
    Json(HealthBody {
        ok: true,
        version: state.version,
    })
}

#[derive(Serialize)]
struct VersionBody {
    version: &'static str,
}

async fn version_handler(State(state): State<HttpState>) -> impl IntoResponse {
    Json(VersionBody {
        version: state.version,
    })
}

async fn events_handler(
    State(state): State<HttpState>,
) -> axum::response::Sse<
    impl futures::Stream<Item = std::result::Result<axum::response::sse::Event, Infallible>>,
> {
    let rx = state.broadcaster.subscribe();
    into_sse_response(rx)
}

async fn workers_handler(State(state): State<HttpState>) -> impl IntoResponse {
    let workers = state.app.workers().await;
    Json(workers)
}

async fn jobs_handler(State(state): State<HttpState>) -> impl IntoResponse {
    let jobs = state.app.jobs().await;
    Json(jobs)
}

async fn policy_handler(State(state): State<HttpState>) -> impl IntoResponse {
    let policy = state.app.policy().await;
    Json(policy)
}

// `IntoResponse` for the comms error so panics from inside handlers
// can be converted into a 500. Handlers themselves currently don't
// return Result; this lives here for the day they do.
impl IntoResponse for CommsError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self, "comms error surfaced to HTTP handler");
        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evy_scheduler::JobAction;

    #[test]
    fn action_kind_tags_match_serde_variants() {
        assert_eq!(
            JobSummary::action_kind_tag(&JobAction::LogHeartbeat),
            "log_heartbeat",
        );
        assert_eq!(
            JobSummary::action_kind_tag(&JobAction::InvokeShell("ls".to_owned())),
            "invoke_shell",
        );
    }

    #[tokio::test]
    async fn stub_app_state_returns_empty() {
        let s = StubAppState;
        assert!(s.workers().await.is_empty());
        assert!(s.jobs().await.is_empty());
        // Policy::default() round-trips through serde without panic.
        let p = s.policy().await;
        let _json = serde_json::to_value(&p).unwrap();
    }

    #[test]
    fn build_router_with_empty_origins_succeeds() {
        let broadcaster = EventBroadcaster::default();
        let state: Arc<dyn AppState> = Arc::new(StubAppState);
        let router = build_router(broadcaster, state, &[], None).unwrap();
        // Constructing the router is the assertion; if it builds, the
        // route set is internally consistent.
        let _ = router;
    }

    #[test]
    fn build_router_with_origins_succeeds() {
        let broadcaster = EventBroadcaster::default();
        let state: Arc<dyn AppState> = Arc::new(StubAppState);
        let router = build_router(
            broadcaster,
            state,
            &["http://127.0.0.1:8787".to_owned()],
            None,
        )
        .unwrap();
        let _ = router;
    }

    #[test]
    fn build_router_with_invalid_origin_fails() {
        let broadcaster = EventBroadcaster::default();
        let state: Arc<dyn AppState> = Arc::new(StubAppState);
        // Non-ASCII byte makes the HeaderValue parse fail.
        let bad = String::from("http://localhost\n:8787");
        let err = build_router(broadcaster, state, &[bad], None).unwrap_err();
        assert!(matches!(err, CommsError::Cors(_)));
    }

    #[test]
    fn build_router_with_static_dir_succeeds() {
        let broadcaster = EventBroadcaster::default();
        let state: Arc<dyn AppState> = Arc::new(StubAppState);
        // Pointing at the in-repo static bundle is fine; ServeDir does
        // not eagerly read the directory at construction time.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("static");
        let router = build_router(broadcaster, state, &[], Some(&dir)).unwrap();
        let _ = router;
    }
}
