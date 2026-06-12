//! EA1 — Evy's agency surface: the daemon-side executors behind the
//! backend-neutral [`evy_thinking::ToolRegistry`], plus the
//! [`LiveStatusSource`] that injects fleet telemetry into every chat
//! turn's system prompt.
//!
//! # Two paths to self-knowledge
//!
//! 1. **Tools** — tool-capable backends (Anthropic always; LM Studio
//!    behind `[thinking_partner.lm_studio] tools_enabled`) call the
//!    read-only tools (`evy_usage`, `evy_sessions`, `evy_workers`,
//!    `evy_watchdogs`, `evy_accounts`) and the policy-gated action
//!    tools (`evy_spawn_worker`, `evy_kill_worker`,
//!    `evy_notify_operator`). Every invocation is audit-logged by
//!    [`evy_thinking::ToolRegistry::execute`].
//! 2. **Context injection** — [`LiveStatus`] renders a compact status
//!    block (cached, [`STATUS_TTL`]) that the partner appends to the
//!    system prompt for EVERY backend, so the operator's live
//!    lm-studio/gemma pin answers fleet questions without tool calls.
//!
//! # Policy gating
//!
//! The daemon's existing spawn surface (`POST /api/evy/orchestration/
//! spawn` → [`crate::state::spawn_worker_shared`]) does not gate the
//! spawn itself — it embeds `PolicyMode::Gated` in the mandate so the
//! worker-side bash gate enforces. Evy-initiated actions ride the SAME
//! shared path (identical Gated posture, registry rows, SSE frames) and
//! add one stricter daemon-side check: when the resolved policy's
//! `default_mode` is `sealed`, the dispatch tools (`evy_spawn_worker`,
//! `evy_kill_worker`) refuse outright. `evy_notify_operator` is never
//! policy-blocked — telling the operator things is how a sealed system
//! asks for help.
//!
//! # No secrets
//!
//! `evy_accounts` reads only `accounts.conf` rows (alias / provider /
//! email / config_dir / description). Nothing here ever opens an
//! `auth.json`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use evy_comms::usage_cache::{AccountUsageResult, UsageWindow};
use evy_comms::{
    EventBroadcaster, Notification, SpawnRequest, TelegramBridge, WatchdogDiagRegistry,
};
use evy_core::{WorkerId, WorkerRegistry};
use evy_policy::{Mode, Policy};
use evy_providers::AccountsStore;
use evy_thinking::{EvyTool, LiveStatusSource, ToolRegistry, ToolSpec};
use evy_watchdog::TmuxQuery;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::{kill_worker_shared, spawn_worker_shared};

/// TTL for the cached live-status block. Long enough that a burst of
/// chat turns doesn't hammer tmux; short enough that "what's running?"
/// answers are honest.
pub const STATUS_TTL: Duration = Duration::from_secs(10);

/// Hard character budget for the rendered status block — well under the
/// partner's defensive 4096 cap so a busy fleet still leaves prompt
/// room. Sections summarize and truncate to stay inside it.
const STATUS_BUDGET_CHARS: usize = 1800;

/// Path to the hourly usage snapshots written by the operator's
/// `jobs.toml` job (`EVY_USAGE_SNAPSHOTS` or
/// `~/.config/subctl/v4/usage-snapshots.jsonl`).
pub(crate) fn usage_snapshots_path() -> PathBuf {
    std::env::var("EVY_USAGE_SNAPSHOTS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(format!("{home}/.config/subctl/v4/usage-snapshots.jsonl"))
        })
}

/// One line of `usage-snapshots.jsonl` — the hourly job appends
/// `{"ts": "...", "usage": [<subctl usage --json rows>]}`.
#[derive(Debug, Deserialize)]
struct SnapshotLine {
    ts: String,
    #[serde(default)]
    usage: Vec<AccountUsageResult>,
}

/// Read the newest parseable snapshot line. `None` when the file is
/// missing, empty, or wholly unparseable — callers render that as
/// "no usage data" rather than erroring the turn.
async fn latest_snapshot(path: &Path) -> Option<SnapshotLine> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    text.lines()
        .rev()
        .find_map(|line| serde_json::from_str::<SnapshotLine>(line).ok())
}

/// `"46%"` / `"?"` for one usage window.
fn pct(w: Option<&UsageWindow>) -> String {
    w.and_then(|w| w.utilization)
        .map_or_else(|| "?".to_string(), |u| format!("{u:.0}%"))
}

/// One-line-per-account usage summary (status-block density).
fn render_usage_brief(rows: &[AccountUsageResult]) -> String {
    rows.iter()
        .map(|r| match (&r.usage, &r.error) {
            (Some(u), _) => format!(
                "{} 5h:{} 7d:{}{}",
                r.alias,
                pct(u.five_hour.as_ref()),
                pct(u.seven_day.as_ref()),
                if r.stale == Some(true) {
                    " (stale)"
                } else {
                    ""
                },
            ),
            (None, Some(e)) if e.contains("429") => format!("{} rate-limited", r.alias),
            (None, _) => format!("{} no-data", r.alias),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

// ─── live status source ─────────────────────────────────────────────────

/// Gathers the fleet snapshot injected into every chat turn. All reads,
/// no mutations; cached for [`STATUS_TTL`].
pub struct LiveStatus {
    accounts_conf: PathBuf,
    snapshots: PathBuf,
    tmux: Arc<dyn TmuxQuery>,
    workers: WorkerRegistry,
    watchdogs: Option<Arc<WatchdogDiagRegistry>>,
    cache: tokio::sync::Mutex<Option<(Instant, String)>>,
    ttl: Duration,
}

impl LiveStatus {
    /// Build a status source over the daemon's live handles.
    #[must_use]
    pub fn new(
        accounts_conf: PathBuf,
        snapshots: PathBuf,
        tmux: Arc<dyn TmuxQuery>,
        workers: WorkerRegistry,
        watchdogs: Option<Arc<WatchdogDiagRegistry>>,
    ) -> Self {
        Self {
            accounts_conf,
            snapshots,
            tmux,
            workers,
            watchdogs,
            cache: tokio::sync::Mutex::new(None),
            ttl: STATUS_TTL,
        }
    }

    #[cfg(test)]
    fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Render the block fresh (no cache). Each section degrades to a
    /// short "unavailable" note on failure — one broken source must
    /// never blank the whole block.
    async fn gather(&self) -> String {
        let mut sections: Vec<String> = Vec::with_capacity(5);

        // Accounts — roster only, no tokens.
        match AccountsStore::open(&self.accounts_conf).and_then(|s| s.list_rows()) {
            Ok(rows) if !rows.is_empty() => {
                let list = rows
                    .iter()
                    .map(|r| format!("{} ({})", r.alias, r.provider))
                    .collect::<Vec<_>>()
                    .join(", ");
                sections.push(format!("accounts ({}): {list}", rows.len()));
            }
            Ok(_) => sections.push("accounts: none configured".to_string()),
            Err(e) => sections.push(format!("accounts: unavailable ({e})")),
        }

        // Usage — newest hourly snapshot.
        match latest_snapshot(&self.snapshots).await {
            Some(snap) => sections.push(format!(
                "usage (snapshot {}): {}",
                snap.ts,
                render_usage_brief(&snap.usage)
            )),
            None => sections.push("usage: no snapshot data".to_string()),
        }

        // Tmux sessions — names only here; the evy_sessions tool has windows.
        match self.tmux.list_sessions().await {
            Ok(names) if !names.is_empty() => {
                let shown = names.iter().take(12).cloned().collect::<Vec<_>>();
                let suffix = if names.len() > shown.len() {
                    format!(" (+{} more)", names.len() - shown.len())
                } else {
                    String::new()
                };
                sections.push(format!(
                    "tmux sessions ({}): {}{suffix}",
                    names.len(),
                    shown.join(", ")
                ));
            }
            Ok(_) => sections.push("tmux sessions: none".to_string()),
            Err(e) => sections.push(format!("tmux sessions: unavailable ({e})")),
        }

        // Registered workers.
        let workers = self.workers.list();
        if workers.is_empty() {
            sections.push("workers: none registered".to_string());
        } else {
            let rows = workers
                .iter()
                .take(10)
                .map(|w| {
                    format!(
                        "{} {:?} {:?}{}",
                        &w.id.0.to_string()[..8],
                        w.provider,
                        w.status,
                        w.tmux_session
                            .as_deref()
                            .map(|s| format!(" @{s}"))
                            .unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            sections.push(format!("workers ({}): {rows}", workers.len()));
        }

        // Watchdogs — counts by status band.
        match &self.watchdogs {
            Some(reg) => {
                let diags = reg.diag_snapshot(chrono::Utc::now());
                let total = diags.len();
                let unhealthy: Vec<String> = diags
                    .iter()
                    .filter(|d| {
                        !matches!(
                            d.status,
                            evy_comms::WatchdogStatus::Healthy | evy_comms::WatchdogStatus::Unknown
                        )
                    })
                    .map(|d| format!("{} ({:?})", d.id, d.status))
                    .collect();
                if unhealthy.is_empty() {
                    sections.push(format!(
                        "watchdogs: {total} registered, all healthy/unknown"
                    ));
                } else {
                    sections.push(format!(
                        "watchdogs: {total} registered, attention: {}",
                        unhealthy.join(", ")
                    ));
                }
            }
            None => sections.push("watchdogs: registry not booted".to_string()),
        }

        let mut block = sections.join("\n");
        if block.len() > STATUS_BUDGET_CHARS {
            let truncated: String = block.chars().take(STATUS_BUDGET_CHARS).collect();
            block = format!("{truncated}…[truncated]");
        }
        block
    }
}

#[async_trait]
impl LiveStatusSource for LiveStatus {
    async fn status_block(&self) -> Option<String> {
        let mut guard = self.cache.lock().await;
        if let Some((at, block)) = guard.as_ref() {
            if at.elapsed() < self.ttl {
                return Some(block.clone());
            }
        }
        let block = self.gather().await;
        *guard = Some((Instant::now(), block.clone()));
        Some(block)
    }
}

// ─── read-only tools ────────────────────────────────────────────────────

/// `evy_usage` — per-account usage detail from the newest snapshot.
pub struct UsageTool {
    snapshots: PathBuf,
}

#[async_trait]
impl EvyTool for UsageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "evy_usage".into(),
            description: "Per-account Claude usage/radar summary (5-hour and 7-day windows, \
                          reset times) from the latest hourly snapshot. Call when the operator \
                          asks about account usage, rate limits, or which account has headroom."
                .into(),
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }
    }

    async fn execute(&self, _input: &Value) -> std::result::Result<String, String> {
        let snap = latest_snapshot(&self.snapshots)
            .await
            .ok_or_else(|| format!("no usage snapshots at {}", self.snapshots.display()))?;
        let mut out = format!("usage snapshot {}\n", snap.ts);
        for r in &snap.usage {
            match (&r.usage, &r.error) {
                (Some(u), _) => {
                    out.push_str(&format!(
                        "{}: 5h {} (resets {}), 7d {} (resets {}), 7d-sonnet {}, 7d-opus {}{}\n",
                        r.alias,
                        pct(u.five_hour.as_ref()),
                        u.five_hour
                            .as_ref()
                            .and_then(|w| w.resets_at.as_deref())
                            .unwrap_or("?"),
                        pct(u.seven_day.as_ref()),
                        u.seven_day
                            .as_ref()
                            .and_then(|w| w.resets_at.as_deref())
                            .unwrap_or("?"),
                        pct(u.seven_day_sonnet.as_ref()),
                        pct(u.seven_day_opus.as_ref()),
                        if r.stale == Some(true) {
                            " [stale]"
                        } else {
                            ""
                        },
                    ));
                }
                (None, Some(e)) => {
                    let brief = if e.contains("429") {
                        "rate-limited (HTTP 429)".to_string()
                    } else {
                        e.chars().take(120).collect()
                    };
                    out.push_str(&format!("{}: fetch failed — {brief}\n", r.alias));
                }
                (None, None) => out.push_str(&format!("{}: no data\n", r.alias)),
            }
        }
        Ok(out)
    }
}

/// `evy_sessions` — live tmux sessions with their windows.
pub struct SessionsTool {
    tmux: Arc<dyn TmuxQuery>,
}

#[async_trait]
impl EvyTool for SessionsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "evy_sessions".into(),
            description: "List live tmux sessions and their window names. Call when the \
                          operator asks what's running, which orchestrators/workers have \
                          sessions, or whether a named session exists."
                .into(),
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }
    }

    async fn execute(&self, _input: &Value) -> std::result::Result<String, String> {
        let sessions = self
            .tmux
            .list_sessions()
            .await
            .map_err(|e| format!("tmux unavailable: {e}"))?;
        if sessions.is_empty() {
            return Ok("no live tmux sessions".to_string());
        }
        let mut out = format!("{} live tmux session(s)\n", sessions.len());
        for name in sessions.iter().take(20) {
            let windows = self.tmux.list_windows(name).await.unwrap_or_default();
            if windows.is_empty() {
                out.push_str(&format!("{name}\n"));
            } else {
                out.push_str(&format!("{name}: [{}]\n", windows.join(", ")));
            }
        }
        if sessions.len() > 20 {
            out.push_str(&format!("(+{} more sessions)\n", sessions.len() - 20));
        }
        Ok(out)
    }
}

/// `evy_workers` — the daemon's worker registry rows.
pub struct WorkersTool {
    registry: WorkerRegistry,
}

#[async_trait]
impl EvyTool for WorkersTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "evy_workers".into(),
            description: "List workers registered with this daemon (id, provider, status, \
                          hosting tmux session, age, last event). Call when the operator asks \
                          which workers are running or what a worker is doing."
                .into(),
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }
    }

    async fn execute(&self, _input: &Value) -> std::result::Result<String, String> {
        let rows = self.registry.list();
        if rows.is_empty() {
            return Ok("no workers registered".to_string());
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut out = format!("{} registered worker(s)\n", rows.len());
        for r in rows {
            let age_s = (now_ms - r.created_at_ms).max(0) / 1000;
            out.push_str(&format!(
                "{} provider={:?} status={:?} session={} age={}s last_event={}\n",
                r.id.0,
                r.provider,
                r.status,
                r.tmux_session.as_deref().unwrap_or("-"),
                age_s,
                r.last_event.as_deref().unwrap_or("-"),
            ));
        }
        Ok(out)
    }
}

/// `evy_watchdogs` — diagnostics registry summary.
pub struct WatchdogsTool {
    diag: Option<Arc<WatchdogDiagRegistry>>,
}

#[async_trait]
impl EvyTool for WatchdogsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "evy_watchdogs".into(),
            description: "Watchdog health summary (per-watchdog status band, last tick age, \
                          last error). Call when the operator asks whether the daemon's \
                          watchdogs are healthy or why one fired."
                .into(),
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }
    }

    async fn execute(&self, _input: &Value) -> std::result::Result<String, String> {
        let Some(reg) = &self.diag else {
            return Err("watchdog diagnostics registry is not booted".to_string());
        };
        let diags = reg.diag_snapshot(chrono::Utc::now());
        if diags.is_empty() {
            return Ok("no watchdogs registered".to_string());
        }
        let mut out = format!("{} watchdog(s)\n", diags.len());
        for d in diags {
            out.push_str(&format!(
                "{} kind={} status={:?} last_tick_ago={} last_error={}\n",
                d.id,
                d.kind,
                d.status,
                d.last_tick_ago_seconds
                    .map_or_else(|| "never".to_string(), |s| format!("{s}s")),
                d.last_error.map_or_else(|| "-".to_string(), |e| e.message),
            ));
        }
        Ok(out)
    }
}

/// `evy_accounts` — accounts.conf roster. NO secrets: this reads the
/// pipe-delimited roster only, never any `auth.json`.
pub struct AccountsTool {
    accounts_conf: PathBuf,
}

#[async_trait]
impl EvyTool for AccountsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "evy_accounts".into(),
            description: "List configured accounts (alias, provider, email, config dir, \
                          description). No tokens or secrets are ever returned. Call when the \
                          operator asks which accounts exist or which alias maps to which \
                          provider."
                .into(),
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }
    }

    async fn execute(&self, _input: &Value) -> std::result::Result<String, String> {
        let rows = AccountsStore::open(&self.accounts_conf)
            .and_then(|s| s.list_rows())
            .map_err(|e| format!("accounts.conf unreadable: {e}"))?;
        if rows.is_empty() {
            return Ok("no accounts configured".to_string());
        }
        let mut out = format!("{} account(s)\n", rows.len());
        for r in rows {
            out.push_str(&format!(
                "{} provider={} email={} config_dir={}{}\n",
                r.alias,
                r.provider,
                r.email,
                r.config_dir.display(),
                if r.description.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", r.description)
                },
            ));
        }
        Ok(out)
    }
}

// ─── action tools (policy-gated) ────────────────────────────────────────

/// Shared sealed-mode refusal for the dispatch tools — see the module
/// docs for how this relates to the daemon's existing gating posture.
fn check_dispatch_allowed(policy: &Policy) -> std::result::Result<(), String> {
    if policy.default_mode == Some(Mode::Sealed) {
        return Err(
            "policy default_mode is `sealed` — dispatch actions are disabled; ask the \
             operator to change policy.toml if this should be allowed"
                .to_string(),
        );
    }
    Ok(())
}

/// `evy_spawn_worker` — dispatch a worker through the existing spawn
/// path. Claude-provider accounts only this wave.
pub struct SpawnWorkerTool {
    registry: WorkerRegistry,
    broadcaster: EventBroadcaster,
    accounts_conf: PathBuf,
    policy: Arc<Policy>,
}

#[async_trait]
impl EvyTool for SpawnWorkerTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "evy_spawn_worker".into(),
            description: "Spawn a Claude Code worker on a named account with a goal \
                          (optionally pinned to a project directory). This is a REAL dispatch \
                          — policy-gated and audit-logged. Only call when the operator \
                          explicitly asked to spawn/dispatch. Claude-provider accounts only; \
                          codex and other providers are not dispatchable from chat this wave."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "account": {"type": "string", "description": "accounts.conf alias, e.g. claude-semfreak"},
                    "goal": {"type": "string", "description": "the worker's goal / first directive"},
                    "project": {"type": "string", "description": "optional absolute working directory"}
                },
                "required": ["account", "goal"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, input: &Value) -> std::result::Result<String, String> {
        check_dispatch_allowed(&self.policy)?;
        let account = input
            .get("account")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("`account` is required")?;
        let goal = input
            .get("goal")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("`goal` is required")?;
        let project = input
            .get("project")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);

        // Claude-only this wave: resolve the row up front so a codex
        // alias gets a clear refusal instead of a half-spawned session.
        let row = AccountsStore::open(&self.accounts_conf)
            .and_then(|s| s.find_row(account))
            .map_err(|e| format!("accounts.conf unreadable: {e}"))?
            .ok_or_else(|| format!("no account named `{account}` in accounts.conf"))?;
        if row.provider != "claude" {
            return Err(format!(
                "account `{account}` is provider `{}` — only claude-provider accounts are \
                 dispatchable from chat this wave",
                row.provider
            ));
        }

        let req = SpawnRequest {
            account: account.to_string(),
            goal: goal.to_string(),
            project,
        };
        let worker_id = spawn_worker_shared(&self.registry, &self.broadcaster, req)
            .await
            .map_err(|e| format!("spawn failed: {e:?}"))?;
        let session = self
            .registry
            .get(&worker_id)
            .and_then(|r| r.tmux_session)
            .unwrap_or_else(|| "-".to_string());
        Ok(format!(
            "spawned worker {} on {account} (tmux session {session}); goal: {goal}",
            worker_id.0
        ))
    }
}

/// `evy_kill_worker` — kill a registered worker through the existing
/// kill path.
pub struct KillWorkerTool {
    registry: WorkerRegistry,
    broadcaster: EventBroadcaster,
    policy: Arc<Policy>,
}

#[async_trait]
impl EvyTool for KillWorkerTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "evy_kill_worker".into(),
            description: "Kill a registered worker by id (kills its tmux session and removes \
                          the registry row). This is a REAL kill — policy-gated and \
                          audit-logged. Only call when the operator explicitly asked. Get ids \
                          from evy_workers first."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "worker_id": {"type": "string", "description": "UUID from evy_workers"}
                },
                "required": ["worker_id"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, input: &Value) -> std::result::Result<String, String> {
        check_dispatch_allowed(&self.policy)?;
        let raw = input
            .get("worker_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("`worker_id` is required")?;
        let uuid = uuid::Uuid::parse_str(raw)
            .map_err(|e| format!("`{raw}` is not a valid worker UUID: {e}"))?;
        let killed = kill_worker_shared(&self.registry, &self.broadcaster, WorkerId(uuid))
            .await
            .map_err(|e| format!("kill failed: {e:?}"))?;
        if killed {
            Ok(format!("worker {raw} killed"))
        } else {
            Ok(format!(
                "no registered worker with id {raw} — nothing killed"
            ))
        }
    }
}

/// `evy_notify_operator` — push a message to the operator's Telegram.
pub struct NotifyOperatorTool {
    telegram: Option<TelegramBridge>,
}

#[async_trait]
impl EvyTool for NotifyOperatorTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "evy_notify_operator".into(),
            description: "Send a short message to the operator via Telegram. Use for \
                          out-of-band alerts the operator should see even if they've left \
                          this chat — not for normal replies."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string", "description": "the message text, sent verbatim"}
                },
                "required": ["message"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, input: &Value) -> std::result::Result<String, String> {
        let message = input
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("`message` is required")?;
        let Some(bridge) = &self.telegram else {
            return Err("telegram is not configured on this daemon".to_string());
        };
        bridge
            .notify(Notification::Note {
                text: message.to_string(),
            })
            .await
            .map_err(|e| format!("telegram send failed: {e}"))?;
        Ok("notified the operator via telegram".to_string())
    }
}

// ─── assembly ───────────────────────────────────────────────────────────

/// Handles the registry builder needs — everything the daemon already
/// owns by the time the thinking-partner is constructed.
pub struct AgencyDeps {
    /// The shared worker registry (same instance the watchdogs sweep).
    pub workers: WorkerRegistry,
    /// The shared SSE broadcaster (spawn/kill frames ride the live bus).
    pub broadcaster: EventBroadcaster,
    /// The resolved policy (sealed-mode gate for dispatch tools).
    pub policy: Arc<Policy>,
    /// Read-only tmux interrogation (W6.5 `tmux_bin()` resolution —
    /// never bare `Command::new("tmux")` under launchd).
    pub tmux: Arc<dyn TmuxQuery>,
    /// Watchdog diagnostics, when booted.
    pub watchdog_diag: Option<Arc<WatchdogDiagRegistry>>,
    /// Telegram bridge, when configured.
    pub telegram: Option<TelegramBridge>,
    /// `accounts.conf` path (same resolution as the spawn path).
    pub accounts_conf: PathBuf,
    /// `usage-snapshots.jsonl` path.
    pub snapshots: PathBuf,
}

/// Build the full EA1 tool registry: five read-only tools, three
/// policy-gated action tools.
#[must_use]
pub fn build_tool_registry(deps: &AgencyDeps) -> ToolRegistry {
    ToolRegistry::new()
        .with_tool(Arc::new(UsageTool {
            snapshots: deps.snapshots.clone(),
        }))
        .with_tool(Arc::new(SessionsTool {
            tmux: deps.tmux.clone(),
        }))
        .with_tool(Arc::new(WorkersTool {
            registry: deps.workers.clone(),
        }))
        .with_tool(Arc::new(WatchdogsTool {
            diag: deps.watchdog_diag.clone(),
        }))
        .with_tool(Arc::new(AccountsTool {
            accounts_conf: deps.accounts_conf.clone(),
        }))
        .with_tool(Arc::new(SpawnWorkerTool {
            registry: deps.workers.clone(),
            broadcaster: deps.broadcaster.clone(),
            accounts_conf: deps.accounts_conf.clone(),
            policy: deps.policy.clone(),
        }))
        .with_tool(Arc::new(KillWorkerTool {
            registry: deps.workers.clone(),
            broadcaster: deps.broadcaster.clone(),
            policy: deps.policy.clone(),
        }))
        .with_tool(Arc::new(NotifyOperatorTool {
            telegram: deps.telegram.clone(),
        }))
}

/// Build the [`LiveStatus`] source over the same handles.
#[must_use]
pub fn build_live_status(deps: &AgencyDeps) -> LiveStatus {
    LiveStatus::new(
        deps.accounts_conf.clone(),
        deps.snapshots.clone(),
        deps.tmux.clone(),
        deps.workers.clone(),
        deps.watchdog_diag.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use evy_core::{MandateId, ProviderKind, WorkerRecord};
    use evy_watchdog::MockTmuxQuery;
    use std::io::Write;

    fn write_temp(content: &str, name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(content.as_bytes()).expect("write");
        (dir, path)
    }

    const SNAPSHOT: &str = r#"{"ts":"2026-06-12T01:07:01Z","usage":[{"alias":"claude-a","cfg_dir":"/c/a","ok":true,"usage":{"five_hour":{"utilization":12.0,"resets_at":"2026-06-12T06:00:00Z"},"seven_day":{"utilization":46.0,"resets_at":"2026-06-14T14:00:00Z"}}},{"alias":"claude-b","cfg_dir":"/c/b","ok":false,"error":"HTTP 429 rate limit"}]}"#;

    const ACCOUNTS: &str = "\
# alias | provider | email | config_dir | description
claude-a | claude | a@x.com | /tmp/claude-a | main
codex-c | openai-codex | c@x.com | /tmp/codex-c | codex box
";

    #[tokio::test]
    async fn usage_tool_renders_latest_snapshot() {
        // Two lines: tool must pick the NEWER (last parseable) one.
        let old = r#"{"ts":"2026-06-11T00:00:00Z","usage":[]}"#;
        let (_d, path) = write_temp(&format!("{old}\n{SNAPSHOT}\n"), "usage.jsonl");
        let tool = UsageTool { snapshots: path };
        let out = tool.execute(&json!({})).await.expect("ok");
        assert!(out.contains("2026-06-12T01:07:01Z"), "got: {out}");
        assert!(out.contains("claude-a: 5h 12%"), "got: {out}");
        assert!(
            out.contains("claude-b: fetch failed — rate-limited"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn usage_tool_errors_cleanly_without_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = UsageTool {
            snapshots: dir.path().join("missing.jsonl"),
        };
        let err = tool.execute(&json!({})).await.unwrap_err();
        assert!(err.contains("no usage snapshots"), "got: {err}");
    }

    #[tokio::test]
    async fn sessions_tool_lists_sessions_with_windows() {
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-foo", "evy"]);
        tmux.set_windows("claude-foo", ["lead", "worker-1"]);
        let tool = SessionsTool { tmux };
        let out = tool.execute(&json!({})).await.expect("ok");
        assert!(out.contains("2 live tmux session(s)"), "got: {out}");
        assert!(out.contains("claude-foo: [lead, worker-1]"), "got: {out}");
        assert!(out.contains("evy\n"), "got: {out}");
    }

    #[tokio::test]
    async fn sessions_tool_surfaces_tmux_probe_failure() {
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_probe_error("spawn tmux: No such file or directory");
        let tool = SessionsTool { tmux };
        let err = tool.execute(&json!({})).await.unwrap_err();
        assert!(err.contains("tmux unavailable"), "got: {err}");
    }

    #[tokio::test]
    async fn workers_tool_renders_registry_rows() {
        let reg = WorkerRegistry::new();
        let id = WorkerId::new();
        let mut rec = WorkerRecord::running(
            id,
            ProviderKind::ClaudeCode,
            MandateId::new(),
            chrono::Utc::now().timestamp_millis(),
        );
        rec.tmux_session = Some("claude-foo".to_string());
        reg.register(rec);
        let tool = WorkersTool { registry: reg };
        let out = tool.execute(&json!({})).await.expect("ok");
        assert!(out.contains("1 registered worker(s)"), "got: {out}");
        assert!(out.contains(&id.0.to_string()), "got: {out}");
        assert!(out.contains("session=claude-foo"), "got: {out}");
    }

    #[tokio::test]
    async fn accounts_tool_lists_roster_without_secrets() {
        let (_d, path) = write_temp(ACCOUNTS, "accounts.conf");
        let tool = AccountsTool {
            accounts_conf: path,
        };
        let out = tool.execute(&json!({})).await.expect("ok");
        assert!(out.contains("2 account(s)"), "got: {out}");
        assert!(
            out.contains("claude-a provider=claude email=a@x.com"),
            "got: {out}"
        );
        assert!(out.contains("codex-c provider=openai-codex"), "got: {out}");
    }

    #[tokio::test]
    async fn watchdogs_tool_without_registry_errors() {
        let tool = WatchdogsTool { diag: None };
        let err = tool.execute(&json!({})).await.unwrap_err();
        assert!(err.contains("not booted"), "got: {err}");
    }

    fn sealed_policy() -> Arc<Policy> {
        Arc::new(Policy {
            default_mode: Some(Mode::Sealed),
            ..Policy::default()
        })
    }

    #[tokio::test]
    async fn spawn_tool_refuses_under_sealed_policy() {
        let (_d, path) = write_temp(ACCOUNTS, "accounts.conf");
        let tool = SpawnWorkerTool {
            registry: WorkerRegistry::new(),
            broadcaster: EventBroadcaster::default(),
            accounts_conf: path,
            policy: sealed_policy(),
        };
        let err = tool
            .execute(&json!({"account": "claude-a", "goal": "do x"}))
            .await
            .unwrap_err();
        assert!(err.contains("sealed"), "got: {err}");
    }

    #[tokio::test]
    async fn spawn_tool_rejects_non_claude_provider() {
        let (_d, path) = write_temp(ACCOUNTS, "accounts.conf");
        let tool = SpawnWorkerTool {
            registry: WorkerRegistry::new(),
            broadcaster: EventBroadcaster::default(),
            accounts_conf: path,
            policy: Arc::new(Policy::default()),
        };
        let err = tool
            .execute(&json!({"account": "codex-c", "goal": "do x"}))
            .await
            .unwrap_err();
        assert!(err.contains("only claude-provider accounts"), "got: {err}");
    }

    #[tokio::test]
    async fn spawn_tool_rejects_unknown_account_and_missing_args() {
        let (_d, path) = write_temp(ACCOUNTS, "accounts.conf");
        let tool = SpawnWorkerTool {
            registry: WorkerRegistry::new(),
            broadcaster: EventBroadcaster::default(),
            accounts_conf: path,
            policy: Arc::new(Policy::default()),
        };
        let err = tool
            .execute(&json!({"account": "ghost", "goal": "x"}))
            .await
            .unwrap_err();
        assert!(err.contains("no account named"), "got: {err}");
        let err2 = tool
            .execute(&json!({"account": "claude-a"}))
            .await
            .unwrap_err();
        assert!(err2.contains("`goal` is required"), "got: {err2}");
    }

    #[tokio::test]
    async fn kill_tool_validates_uuid_and_reports_unknown() {
        let tool = KillWorkerTool {
            registry: WorkerRegistry::new(),
            broadcaster: EventBroadcaster::default(),
            policy: Arc::new(Policy::default()),
        };
        let err = tool
            .execute(&json!({"worker_id": "not-a-uuid"}))
            .await
            .unwrap_err();
        assert!(err.contains("not a valid worker UUID"), "got: {err}");
        let out = tool
            .execute(&json!({"worker_id": uuid::Uuid::new_v4().to_string()}))
            .await
            .expect("unknown id is a clean no-op");
        assert!(out.contains("nothing killed"), "got: {out}");
    }

    #[tokio::test]
    async fn kill_tool_refuses_under_sealed_policy() {
        let tool = KillWorkerTool {
            registry: WorkerRegistry::new(),
            broadcaster: EventBroadcaster::default(),
            policy: sealed_policy(),
        };
        let err = tool
            .execute(&json!({"worker_id": uuid::Uuid::new_v4().to_string()}))
            .await
            .unwrap_err();
        assert!(err.contains("sealed"), "got: {err}");
    }

    #[tokio::test]
    async fn notify_tool_without_telegram_errors() {
        let tool = NotifyOperatorTool { telegram: None };
        let err = tool.execute(&json!({"message": "hi"})).await.unwrap_err();
        assert!(err.contains("telegram is not configured"), "got: {err}");
    }

    fn test_deps(
        accounts_conf: PathBuf,
        snapshots: PathBuf,
        tmux: Arc<MockTmuxQuery>,
    ) -> AgencyDeps {
        AgencyDeps {
            workers: WorkerRegistry::new(),
            broadcaster: EventBroadcaster::default(),
            policy: Arc::new(Policy::default()),
            tmux,
            watchdog_diag: None,
            telegram: None,
            accounts_conf,
            snapshots,
        }
    }

    #[tokio::test]
    async fn registry_carries_all_eight_tools() {
        let (_d1, accounts) = write_temp(ACCOUNTS, "accounts.conf");
        let (_d2, snaps) = write_temp(SNAPSHOT, "usage.jsonl");
        let deps = test_deps(accounts, snaps, Arc::new(MockTmuxQuery::new()));
        let reg = build_tool_registry(&deps);
        assert_eq!(reg.count(), 8);
        for name in [
            "evy_usage",
            "evy_sessions",
            "evy_workers",
            "evy_watchdogs",
            "evy_accounts",
            "evy_spawn_worker",
            "evy_kill_worker",
            "evy_notify_operator",
        ] {
            assert!(reg.contains(name), "missing tool {name}");
        }
    }

    #[tokio::test]
    async fn live_status_gathers_all_sections_within_budget() {
        let (_d1, accounts) = write_temp(ACCOUNTS, "accounts.conf");
        let (_d2, snaps) = write_temp(&format!("{SNAPSHOT}\n"), "usage.jsonl");
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-foo"]);
        let deps = test_deps(accounts, snaps, tmux);
        let status = build_live_status(&deps);
        let block = status.status_block().await.expect("block present");
        assert!(block.contains("accounts (2)"), "got: {block}");
        assert!(
            block.contains("usage (snapshot 2026-06-12T01:07:01Z)"),
            "got: {block}"
        );
        assert!(block.contains("claude-a 5h:12% 7d:46%"), "got: {block}");
        assert!(block.contains("claude-b rate-limited"), "got: {block}");
        assert!(
            block.contains("tmux sessions (1): claude-foo"),
            "got: {block}"
        );
        assert!(block.contains("workers: none registered"), "got: {block}");
        assert!(
            block.contains("watchdogs: registry not booted"),
            "got: {block}"
        );
        assert!(
            block.len() <= STATUS_BUDGET_CHARS + 20,
            "block must stay within budget; got {} chars",
            block.len()
        );
    }

    #[tokio::test]
    async fn live_status_caches_within_ttl_and_degrades_per_section() {
        let (_d1, accounts) = write_temp(ACCOUNTS, "accounts.conf");
        let dir = tempfile::tempdir().unwrap();
        let missing_snaps = dir.path().join("missing.jsonl");
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["s1"]);
        let status = LiveStatus::new(
            accounts,
            missing_snaps,
            tmux.clone(),
            WorkerRegistry::new(),
            None,
        )
        .with_ttl(Duration::from_secs(60));

        let first = status.status_block().await.expect("block");
        // Missing snapshot file degrades to a note, not a failure.
        assert!(first.contains("usage: no snapshot data"), "got: {first}");
        assert!(first.contains("tmux sessions (1): s1"), "got: {first}");

        // Mutate the world; the cached block must NOT change within TTL.
        tmux.set_sessions(["s1", "s2"]);
        let second = status.status_block().await.expect("block");
        assert_eq!(first, second, "second read within TTL must be cached");
    }
}
