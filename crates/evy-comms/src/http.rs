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
    extract::{Path as AxPath, Query, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{any, delete, get, post},
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

/// Request body for `POST /api/evy/orchestration/spawn` (Phase 2 slice 2j).
#[derive(Debug, Clone, Deserialize)]
pub struct SpawnRequest {
    /// Account alias to spawn on (e.g. `claude-semfreak`).
    pub account: String,
    /// The worker's goal / first directive.
    pub goal: String,
    /// Working dir for the spawned session. Defaults to the account config dir.
    #[serde(default)]
    pub project: Option<String>,
}

/// Why a worker spawn failed.
#[derive(Debug)]
pub enum SpawnError {
    /// This `AppState` doesn't support spawning (e.g. the stub).
    Unsupported,
    /// The requested account alias wasn't found in `accounts.conf`.
    AccountNotFound(String),
    /// The provider / tmux spawn failed; carries the reason.
    Spawn(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => write!(f, "spawning is not supported by this daemon build"),
            Self::AccountNotFound(a) => write!(f, "account not found: {a}"),
            Self::Spawn(r) => write!(f, "spawn failed: {r}"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// One Orch-panel row (Phase 2 slice 2l): a registered worker enriched with
/// live tmux state, so the dashboard can show running teams + offer kill.
#[derive(Debug, Clone, Serialize)]
pub struct OrchestrationRow {
    /// Stable worker id.
    pub worker_id: WorkerId,
    /// Which provider produced it.
    pub provider: ProviderKind,
    /// Lifecycle status (debug rendering of `WorkerStatus`).
    pub status: String,
    /// The tmux session hosting the worker, if recorded.
    pub tmux_session: Option<String>,
    /// Whether that tmux session is currently alive.
    pub alive: bool,
    /// Seconds since the worker was registered.
    pub age_seconds: u64,
    /// Most recent event description, if any.
    pub last_event: Option<String>,
}

/// A pane snapshot for one worker (Phase 2 captures — criterion #7 observation).
#[derive(Debug, Clone, Serialize)]
pub struct OrchestrationCapture {
    /// Which worker this capture is from.
    pub worker_id: WorkerId,
    /// The tmux session captured, if recorded.
    pub session: Option<String>,
    /// The captured pane text (last N lines), or empty if capture failed.
    pub text: String,
}

/// The daemon's read-only + spawn surface as the HTTP layer sees it.
///
/// The HTTP server reads workers / jobs / policy and observes events via SSE;
/// the one mutating entry is [`spawn_worker`](AppState::spawn_worker).
/// [`StubAppState`] returns empty / default data; the daemon binary swaps in a
/// real `Arc<dyn AppState>` (`DaemonAppState`).
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

    /// Cutover criterion #6 — the daemon's Telegram bridge, if one is
    /// configured. `POST /api/evy/notify` and `POST /api/evy/ask`
    /// return 503 when this is `None`.
    ///
    /// Default impl returns `None` so existing implementations
    /// (notably [`StubAppState`]) keep compiling without change.
    fn telegram_bridge(&self) -> Option<crate::telegram::TelegramBridge> {
        None
    }

    /// P2 — display label for the active supervisor model
    /// (e.g. `"lm-studio/gemma-4-26b-a4b-it-mlx"`), surfaced in the
    /// `/api/evy/context` meter. Default `None` (renders as `null`); the
    /// daemon overrides it from `[thinking_partner]` config.
    fn supervisor_label(&self) -> Option<String> {
        None
    }

    /// Cutover Phase 2 (2j) — spawn a worker on the named account, registering
    /// it so `workers()` reflects it (criterion #1). Default impl returns
    /// [`SpawnError::Unsupported`] so the stub keeps compiling; the daemon's
    /// `DaemonAppState` overrides it with the real provider spawn.
    ///
    /// # Errors
    /// [`SpawnError`] when the account is unknown or the tmux/provider spawn fails.
    async fn spawn_worker(&self, _req: SpawnRequest) -> std::result::Result<WorkerId, SpawnError> {
        Err(SpawnError::Unsupported)
    }

    /// Cutover Phase 2 (2l) — the Orch panel's live worker list (registry +
    /// tmux liveness). Default empty; `DaemonAppState` overrides.
    async fn orchestrations(&self) -> Vec<OrchestrationRow> {
        Vec::new()
    }

    /// Cutover Phase 2 (2l) — kill a worker by id (kill its tmux session +
    /// drop it from the registry). `Ok(false)` if the id is unknown. Default
    /// returns `Ok(false)`; `DaemonAppState` overrides.
    ///
    /// # Errors
    /// [`SpawnError`] if the tmux kill fails.
    async fn kill_worker(&self, _id: WorkerId) -> std::result::Result<bool, SpawnError> {
        Ok(false)
    }

    /// Cutover Phase 2 (captures) — pane snapshots of running workers for the
    /// Orch camera grid + criterion-#7 observation. `lines` caps each snapshot's
    /// height. Default empty; `DaemonAppState` overrides.
    async fn orchestration_captures(&self, _lines: usize) -> Vec<OrchestrationCapture> {
        Vec::new()
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
        .route("/api/evy/workers", get(workers_handler))
        .route("/api/evy/scheduler/jobs", get(jobs_handler))
        .route("/api/evy/policy", get(policy_handler))
        // Phase 6 chat surface — POST only; aliased under /api/master
        // for parity with v3 dashboards even though there's no v3
        // equivalent (legacy operators may script either prefix).
        .route("/api/evy/chat", post(crate::chat::chat_handler))
        // Phase 6 follow-up — TUI-driving endpoints. Aliased under
        // /api/master for parity with the other operator routes.
        .route(
            "/api/evy/sessions",
            get(crate::sessions_http::sessions_list_handler),
        )
        .route(
            "/api/evy/sessions/{id}",
            delete(crate::sessions_http::sessions_delete_handler),
        )
        .route(
            "/api/evy/skills",
            get(crate::skills_http::skills_list_handler),
        )
        // P2 — transcript + context meter, in the v3 dashboard wire shape.
        .route(
            "/api/evy/transcript",
            get(crate::transcript_http::transcript_handler),
        )
        .route(
            "/api/evy/context",
            get(crate::transcript_http::context_handler),
        )
        .route(
            "/api/evy/transcript/util",
            get(crate::transcript_http::transcript_util_handler),
        )
        // P3 — compaction + clear write paths (archive to disk + persist).
        .route(
            "/api/evy/transcript/compact",
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

    // Phase 0 (cutover) — native /api/host, then a reverse-proxy fallback:
    // any /api/* not served natively above goes to the v3 Bun dashboard
    // (Fork A bridge + master proxy). Specific routes above always win.
    router = router
        .route("/api/host", get(crate::proxy_http::host_handler))
        // Phase 1 — native /api/evy/accounts (accounts + auth + usage + verdict).
        .route("/api/evy/accounts", get(crate::accounts_http::accounts_handler))
        // Phase 2 (2j) — native worker spawn (criterion #1).
        .route("/api/evy/notify", post(notify_handler))
        .route("/api/evy/ask", post(ask_handler))
        .route("/api/evy/orchestration/spawn", post(spawn_handler))
        // Phase 2 (2l) — Orch panel: live worker list + kill.
        .route("/api/evy/orchestration", get(orchestration_list_handler))
        // Phase 2 (captures) — pane snapshots for the camera grid / criterion #7.
        .route(
            "/api/evy/orchestration/captures",
            get(orchestration_captures_handler),
        )
        .route(
            "/api/evy/orchestration/{id}/kill",
            post(orchestration_kill_handler),
        )
        // Phase 2 (2m) — native team-template CRUD. `/tools` is reverse-proxied
        // (still v3) and registered as a literal so it wins over `/{name}`.
        .route(
            "/api/evy/teams",
            get(crate::teams_http::teams_list_handler).post(crate::teams_http::team_post_handler),
        )
        .route("/api/evy/teams/tools", any(crate::proxy_http::reverse_proxy_handler))
        .route(
            "/api/evy/teams/{name}",
            get(crate::teams_http::team_get_handler)
                .put(crate::teams_http::team_put_handler)
                .delete(crate::teams_http::team_delete_handler),
        )
        // Phase 1 — native /api/evy/rate-limits (24h buckets + today_total + 429s).
        .route("/api/evy/rate-limits", get(crate::accounts_http::rate_limits_handler))
        // Cutover — native /api/evy/cost (v3 CostBundle; also feeds /api/state.cost).
        .route("/api/evy/cost", get(crate::cost_http::cost_handler))
        // Phase 1 — native /api/state (overlay native accounts+dispatch on Bun base)
        // + /api/refresh (bust usage cache). Specific routes → no longer proxied (AC5).
        .route("/api/state", get(crate::accounts_http::state_handler))
        .route("/api/refresh", post(crate::accounts_http::refresh_handler))
        // /api/live (web terminal WS) — WS upgrade can't go through the HTTP
        // reverse-proxy, so it's bridged separately. Specific route wins over
        // the catch-all below.
        .route("/api/live", get(crate::proxy_http::ws_proxy_handler))
        .route("/api/{*rest}", any(crate::proxy_http::reverse_proxy_handler));

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

/// `POST /api/evy/orchestration/spawn` (Phase 2 slice 2j) — spawn a worker on
/// the named account; on success the registry (and `/api/evy/workers`) reflects it.
/// Request body for `POST /api/evy/notify`.
#[derive(Debug, Deserialize)]
struct NotifyRequest {
    /// Message text, sent to the operator verbatim.
    text: String,
}

/// Request body for `POST /api/evy/ask`.
#[derive(Debug, Deserialize)]
struct AskRequest {
    /// The question posted to the operator.
    question: String,
    /// How long to block waiting for the reply. Default 300s.
    #[serde(default = "default_ask_timeout_s")]
    timeout_s: u64,
}

const fn default_ask_timeout_s() -> u64 {
    300
}

/// `POST /api/evy/notify` — push a free-text message to the operator
/// over the configured Telegram bridge (criterion #6 outbound surface).
async fn notify_handler(
    State(state): State<HttpState>,
    Json(req): Json<NotifyRequest>,
) -> Response {
    let Some(bridge) = state.app.telegram_bridge() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "ok": false, "error": "no telegram bridge configured" })),
        )
            .into_response();
    };
    match bridge
        .notify(crate::notification::Notification::Note { text: req.text })
        .await
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `POST /api/evy/ask` — post a question to the operator and block
/// (bounded by `timeout_s`) for their Telegram reply (criterion #6
/// "ask the operator" surface). The operator answers by replying to
/// the question bubble.
async fn ask_handler(State(state): State<HttpState>, Json(req): Json<AskRequest>) -> Response {
    let Some(bridge) = state.app.telegram_bridge() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "ok": false, "error": "no telegram bridge configured" })),
        )
            .into_response();
    };
    let timeout = std::time::Duration::from_secs(req.timeout_s);
    match bridge.ask(req.question, timeout).await {
        Ok(reply) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "reply": reply })),
        )
            .into_response(),
        // The bridge maps an expired ask to a timeout-flavoured error;
        // surface every failure as 504 with the reason — callers treat
        // "no answer" and "couldn't deliver" the same way (retry or
        // escalate), and the body disambiguates.
        Err(e) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn spawn_handler(State(state): State<HttpState>, Json(req): Json<SpawnRequest>) -> Response {
    match state.app.spawn_worker(req).await {
        Ok(worker_id) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "worker_id": worker_id })),
        )
            .into_response(),
        Err(e @ SpawnError::Unsupported) => (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
        Err(e @ SpawnError::AccountNotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `GET /api/evy/orchestration` (Phase 2 slice 2l) — live worker list.
async fn orchestration_list_handler(State(state): State<HttpState>) -> impl IntoResponse {
    Json(state.app.orchestrations().await)
}

/// `POST /api/evy/orchestration/{id}/kill` (Phase 2 slice 2l) — kill a worker.
async fn orchestration_kill_handler(
    State(state): State<HttpState>,
    AxPath(id): AxPath<WorkerId>,
) -> Response {
    match state.app.kill_worker(id).await {
        Ok(true) => Json(serde_json::json!({ "ok": true, "killed": id })).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "ok": false, "error": "unknown worker" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Query for `GET /api/evy/orchestration/captures`.
#[derive(Debug, Deserialize)]
struct CapturesParams {
    lines: Option<usize>,
}

/// `GET /api/evy/orchestration/captures?lines=N` (criterion #7 observation).
async fn orchestration_captures_handler(
    State(state): State<HttpState>,
    Query(q): Query<CapturesParams>,
) -> impl IntoResponse {
    let lines = q.lines.unwrap_or(40).clamp(1, 400);
    Json(serde_json::json!({ "captures": state.app.orchestration_captures(lines).await }))
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
