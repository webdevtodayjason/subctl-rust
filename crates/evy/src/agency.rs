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
use evy_memory::CogneeClient;
use evy_policy::{Mode, Policy};
use evy_providers::AccountsStore;
use evy_secrets::{EnvBackend, FileBackend, SecretsResolver};
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

// ─── EA2 — read-only knowledge tools ────────────────────────────────────

// ── 1. system_subctl_knowledge ───────────────────────────────────────────

/// The v4 knowledge asset, embedded at compile time. Sections are
/// separated by top-level TOON keys (`^([a-z_][a-z0-9_]*):\s*$`).
static KNOWLEDGE_TOON: &str = include_str!("../knowledge/subctl.toon");

/// Return true iff `line` is a TOON top-level key: starts at column 0,
/// matches `[a-z_][a-z0-9_]*:` with optional trailing whitespace only,
/// and no value on the same line. Pure-string — no regex dependency.
fn is_toon_top_level_key(line: &str) -> Option<&str> {
    // Must not start with whitespace.
    let trimmed_end = line.trim_end();
    if trimmed_end.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
        return None;
    }
    let without_colon = trimmed_end.strip_suffix(':')?;
    // Key must be non-empty and start with [a-z_].
    let first = without_colon.chars().next()?;
    if !(first.is_ascii_lowercase() || first == '_') {
        return None;
    }
    // Remainder must be [a-z0-9_]*.
    if without_colon
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        Some(without_colon)
    } else {
        None
    }
}

/// Parse the embedded TOON into a map of section-name → raw content.
/// Returns `(ordered_names, map)`. Called once and cached behind `std::sync::OnceLock`.
fn parse_knowledge_toon(
    raw: &str,
) -> (
    Vec<String>,
    std::collections::HashMap<String, (String, String)>,
) {
    // map: name → (summary, content)
    let mut order: Vec<String> = Vec::new();
    let mut map: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();

    let mut cur_name: Option<String> = None;
    let mut cur_lines: Vec<&str> = Vec::new();
    let mut cur_summary: Option<String> = None;

    macro_rules! flush {
        () => {
            if let Some(ref name) = cur_name {
                let content = cur_lines.join("\n").trim_end().to_string();
                let summary = cur_summary
                    .take()
                    .unwrap_or_else(|| "(no summary)".to_string());
                map.insert(name.clone(), (summary, content));
                order.push(name.clone());
            }
        };
    }

    for line in raw.lines() {
        if let Some(key) = is_toon_top_level_key(line) {
            flush!();
            cur_name = Some(key.to_string());
            cur_lines = vec![line];
            cur_summary = None;
        } else if cur_name.is_some() {
            if cur_summary.is_none() {
                let t = line.trim();
                if t.starts_with('#') {
                    let s = t.trim_start_matches('#').trim().to_string();
                    if !s.is_empty() {
                        cur_summary = Some(s);
                    }
                }
            }
            cur_lines.push(line);
        }
    }
    flush!();
    (order, map)
}

/// Type alias for the parsed TOON index: `(order, name → (summary, content))`.
type KnowledgeIndex = (
    Vec<String>,
    std::collections::HashMap<String, (String, String)>,
);

/// Cached parse of `KNOWLEDGE_TOON`. Initialized on first access.
fn knowledge_index() -> &'static KnowledgeIndex {
    static CACHE: std::sync::OnceLock<KnowledgeIndex> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parse_knowledge_toon(KNOWLEDGE_TOON))
}

/// `system_subctl_knowledge` — self-introspection over the v4 TOON
/// knowledge asset embedded in the binary.
pub struct SubctlKnowledgeTool;

#[async_trait]
impl EvyTool for SubctlKnowledgeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "system_subctl_knowledge".into(),
            description: "v4 daemon self-introspection over the embedded TOON knowledge asset. \
                          Call with no args (or omit `section`) to list all available sections \
                          with summaries. Call with {\"section\": \"<name>\"} to retrieve the \
                          full TOON content for that section. Use when the operator asks how a \
                          v4 component works, what tools are available, what config options \
                          exist, or any architectural question about the running daemon."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "section": {
                        "type": "string",
                        "description": "Optional section name (e.g. \"overview\", \"crates\", \
                                        \"agency_tools\", \"config\", \"cognee\", \"context7\"). \
                                        Omit to list all sections with summaries."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, input: &Value) -> std::result::Result<String, String> {
        let (order, map) = knowledge_index();
        let section = input
            .get("section")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        match section {
            None => {
                let mut lines = vec![format!(
                    "{} sections available. Call again with {{ \"section\": \"<name>\" }} \
                         for full content.\n",
                    order.len()
                )];
                for name in order {
                    let (summary, _) = &map[name];
                    lines.push(format!("  {name}: {summary}"));
                }
                Ok(lines.join("\n"))
            }
            Some(name) => match map.get(name) {
                Some((_, content)) => Ok(content.clone()),
                None => Err(format!(
                    "unknown section: {name}. Available: {}",
                    order.join(", ")
                )),
            },
        }
    }
}

// ── 2–4. knowledge_graph_* ───────────────────────────────────────────────

/// Error string returned when Cognee is unreachable, matching v3 pattern.
const KG_NOT_READY_ERR: &str =
    "Cognee graph not reachable — knowledge_graph_* tools need a running Cognee service. \
     Check `subctl status` or the /api/evy/health endpoint for cognee_health state.";

/// `knowledge_graph_neighbors` — adjacency of a Cognee graph node.
pub struct KgNeighborsTool {
    client: Arc<CogneeClient>,
}

#[async_trait]
impl EvyTool for KgNeighborsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "knowledge_graph_neighbors".into(),
            description: "**Use when** the operator asks 'what's connected to X?' or you need \
                          to follow a relationship chain — projects ↔ decisions, decisions ↔ \
                          files, teams ↔ tool calls. Returns the node plus its direct neighbors \
                          with edge labels. Requires a running Cognee service."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "node_id": {
                        "type": "string",
                        "description": "Cognee node id. Use knowledge_graph_query first \
                                        to discover ids if you don't have one."
                    },
                    "depth": {
                        "type": "integer",
                        "description": "Hops out (default 1, max 3). Prefer 1 unless \
                                        explicit chain context is needed.",
                        "minimum": 1,
                        "maximum": 3
                    },
                    "relation": {
                        "type": "string",
                        "description": "Optional relation-type filter \
                                        (e.g. DECIDED_BY, TOUCHED_FILE). Omit for all."
                    }
                },
                "required": ["node_id"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, input: &Value) -> std::result::Result<String, String> {
        let node_id = input
            .get("node_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "`node_id` is required".to_string())?;
        let depth = input
            .get("depth")
            .and_then(Value::as_u64)
            .map(|d| d.clamp(1, 3) as u32)
            .unwrap_or(1);
        let relation = input
            .get("relation")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // Reachability gate (non-cached per the stateless tool contract —
        // CogneeClient::health is cheap enough to probe inline).
        match self.client.health().await {
            Ok(true) => {}
            _ => return Err(KG_NOT_READY_ERR.to_string()),
        }

        let result = self
            .client
            .graph_neighbors(node_id, depth, relation)
            .await
            .map_err(|e| format!("cognee graph_neighbors: {e}"))?;
        serde_json::to_string(&json!({
            "ok": true,
            "node": result.node,
            "neighbors": result.neighbors,
            "depth": depth,
            "relation": relation,
        }))
        .map_err(|e| format!("serialize: {e}"))
    }
}

/// `knowledge_graph_path` — shortest path between two Cognee graph nodes.
pub struct KgPathTool {
    client: Arc<CogneeClient>,
}

#[async_trait]
impl EvyTool for KgPathTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "knowledge_graph_path".into(),
            description: "**Use when** the operator asks 'how is X related to Y?' — finds the \
                          shortest path between two Cognee nodes. Returns node + edge chain. \
                          Useful for tracing decision provenance (decision → spec → file → bug). \
                          Requires a running Cognee service."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from": {
                        "type": "string",
                        "description": "Source node id."
                    },
                    "to": {
                        "type": "string",
                        "description": "Target node id."
                    },
                    "max_hops": {
                        "type": "integer",
                        "description": "Cap on path length (default 6, max 10).",
                        "minimum": 1,
                        "maximum": 10
                    }
                },
                "required": ["from", "to"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, input: &Value) -> std::result::Result<String, String> {
        let from = input
            .get("from")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "`from` is required".to_string())?;
        let to = input
            .get("to")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "`to` is required".to_string())?;
        let max_hops = input
            .get("max_hops")
            .and_then(Value::as_u64)
            .map(|h| h.clamp(1, 10) as u32)
            .unwrap_or(6);

        match self.client.health().await {
            Ok(true) => {}
            _ => return Err(KG_NOT_READY_ERR.to_string()),
        }

        let result = self
            .client
            .graph_path(from, to, max_hops)
            .await
            .map_err(|e| format!("cognee graph_path: {e}"))?;
        let hop_count = result.nodes.len().saturating_sub(1);
        serde_json::to_string(&json!({
            "ok": true,
            "from": from,
            "to": to,
            "max_hops": max_hops,
            "hop_count": hop_count,
            "nodes": result.nodes,
            "edges": result.edges,
        }))
        .map_err(|e| format!("serialize: {e}"))
    }
}

/// `knowledge_graph_query` — raw Cypher / Cognee DSL query escape hatch.
pub struct KgQueryTool {
    client: Arc<CogneeClient>,
}

#[async_trait]
impl EvyTool for KgQueryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "knowledge_graph_query".into(),
            description: "**Use as an escape hatch** when knowledge_graph_neighbors and \
                          knowledge_graph_path aren't expressive enough — sends a raw query \
                          (Cypher or Cognee DSL) to the graph. Prefer the structured tools \
                          above; this is for aggregations, multi-relation joins, counting. \
                          Requires a running Cognee service."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Query string. Backend-specific syntax (Cypher / Cognee DSL)."
                    },
                    "params": {
                        "type": "object",
                        "description": "Optional parameter bag if the query uses bound parameters."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, input: &Value) -> std::result::Result<String, String> {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "`query` is required".to_string())?;
        let params = input.get("params");

        match self.client.health().await {
            Ok(true) => {}
            _ => return Err(KG_NOT_READY_ERR.to_string()),
        }

        let result = self
            .client
            .graph_query(query, params)
            .await
            .map_err(|e| format!("cognee graph_query: {e}"))?;
        let row_count = result.rows.len();
        serde_json::to_string(&json!({
            "ok": true,
            "query": query,
            "row_count": row_count,
            "rows": result.rows,
        }))
        .map_err(|e| format!("serialize: {e}"))
    }
}

// ── 5–7. context7_* ──────────────────────────────────────────────────────

/// Resolve `CONTEXT7_API_KEY` via the standard secrets chain
/// (env → `~/.config/subctl/secrets.json`).
///
/// `async` and awaited directly from each tool's `async fn execute` —
/// the `SecretsResolver` chain is async, so there's no sync/async
/// bridge here (no `block_in_place`, which would panic on a
/// current-thread runtime and is unnecessary inside an async caller).
async fn resolve_context7_key() -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let secrets_path = format!("{home}/.config/subctl/secrets.json");
    let resolver = SecretsResolver::new(vec![
        Arc::new(EnvBackend::new()) as Arc<dyn evy_secrets::SecretsBackend>,
        Arc::new(FileBackend::new(&secrets_path)) as Arc<dyn evy_secrets::SecretsBackend>,
    ]);
    resolver
        .resolve("context7_api_key")
        .await
        .ok()
        .map(|v| v.value)
}

/// Build a `reqwest::Client` with the given timeout.
fn context7_http_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .expect("reqwest client build must succeed")
}

/// Shared JSON-RPC call helper for the Context7 MCP gateway.
async fn context7_rpc(
    http: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    method: &str,
    params: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    static RPC_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = RPC_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": { "name": method, "arguments": params },
        "id": id
    });

    let resp = http
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("CONTEXT7_API_KEY", api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("context7 transport: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        return Err(format!(
            "context7 HTTP {}: {}",
            status.as_u16(),
            txt.chars().take(300).collect::<String>()
        ));
    }

    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let parsed: serde_json::Value = if ct.contains("text/event-stream") {
        let text = resp
            .text()
            .await
            .map_err(|e| format!("context7 read: {e}"))?;
        let data_line = text
            .lines()
            .find(|l| l.starts_with("data: "))
            .ok_or_else(|| "context7 SSE: no data line".to_string())?;
        serde_json::from_str(&data_line[6..]).map_err(|e| format!("context7 SSE parse: {e}"))?
    } else {
        resp.json()
            .await
            .map_err(|e| format!("context7 json parse: {e}"))?
    };

    if let Some(err) = parsed.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("context7 rpc error: {msg}"));
    }

    Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
}

/// `context7_resolve` — resolve a library name to a Context7 library ID.
pub struct Context7ResolveTool {
    endpoint: String,
    timeout_secs: u64,
}

#[async_trait]
impl EvyTool for Context7ResolveTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "context7_resolve".into(),
            description: "Resolve a library name (e.g. 'react', 'next.js', 'typebox') to a \
                          Context7-compatible library ID. Pre-step before context7_docs when \
                          you don't already know the canonical ID. Returns candidate libraries \
                          with their IDs and snippet counts."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "library": {
                        "type": "string",
                        "description": "Library name to resolve, e.g. 'react' or 'tailwindcss'."
                    }
                },
                "required": ["library"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, input: &Value) -> std::result::Result<String, String> {
        let library = input
            .get("library")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "`library` is required".to_string())?;
        let key = resolve_context7_key().await.ok_or_else(|| {
            "CONTEXT7_API_KEY not configured. Set via env or \
             ~/.config/subctl/secrets.json"
                .to_string()
        })?;
        let http = context7_http_client(self.timeout_secs);
        // Send both libraryName + query for compatibility (the v3 TS client
        // observed both field names accepted by different Context7 versions).
        let result = context7_rpc(
            &http,
            &self.endpoint,
            &key,
            "resolve-library-id",
            json!({ "libraryName": library, "query": library }),
        )
        .await?;
        let text: String = result
            .get("content")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|c| c.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        serde_json::to_string(&json!({
            "ok": true,
            "library": library,
            "raw": text.chars().take(4000).collect::<String>()
        }))
        .map_err(|e| format!("serialize: {e}"))
    }
}

/// `context7_docs` — fetch up-to-date library docs by Context7 library ID.
pub struct Context7DocsTool {
    endpoint: String,
    timeout_secs: u64,
}

#[async_trait]
impl EvyTool for Context7DocsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "context7_docs".into(),
            description: "Fetch up-to-date documentation for a library by topic via Context7. \
                          Use when you need current API references / examples / config for a \
                          third-party tool — don't guess from training data, especially for \
                          fast-moving libraries (React, Next.js, Tailwind, AI SDKs). The \
                          library_id can come from context7_resolve, or use a known canonical \
                          ID like '/vercel/next.js'. Topic narrows the docs."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "library_id": {
                        "type": "string",
                        "description": "Context7-compatible library ID (e.g. '/vercel/next.js'). \
                                        Use context7_resolve first if unknown."
                    },
                    "topic": {
                        "type": "string",
                        "description": "Optional topic narrowing (e.g. 'hooks', 'app router', \
                                        'authentication')."
                    },
                    "tokens": {
                        "type": "number",
                        "description": "Approximate max docs token budget. Default 5000, cap 20000."
                    }
                },
                "required": ["library_id"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, input: &Value) -> std::result::Result<String, String> {
        let library_id = input
            .get("library_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "`library_id` is required".to_string())?;
        let topic = input
            .get("topic")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let tokens = input
            .get("tokens")
            .and_then(Value::as_f64)
            .map(|t| t.clamp(500.0, 20_000.0) as u64)
            .unwrap_or(5_000);

        let key = resolve_context7_key().await.ok_or_else(|| {
            "CONTEXT7_API_KEY not configured. Set via env or \
             ~/.config/subctl/secrets.json"
                .to_string()
        })?;
        let http = context7_http_client(self.timeout_secs);

        let mut args = json!({
            "context7CompatibleLibraryID": library_id,
            "tokens": tokens
        });
        if let Some(t) = topic {
            args["topic"] = Value::String(t.to_string());
        }
        let result = context7_rpc(&http, &self.endpoint, &key, "get-library-docs", args).await?;
        let text: String = result
            .get("content")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|c| c.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        serde_json::to_string(&json!({
            "ok": true,
            "library_id": library_id,
            "topic": topic,
            "tokens_requested": tokens,
            "chars_returned": text.len(),
            "docs": text
        }))
        .map_err(|e| format!("serialize: {e}"))
    }
}

/// `context7_health` — verify Context7 is configured and reachable.
pub struct Context7HealthTool {
    endpoint: String,
    timeout_secs: u64,
}

#[async_trait]
impl EvyTool for Context7HealthTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "context7_health".into(),
            description: "Verify that Context7 is configured (CONTEXT7_API_KEY set via env or \
                          ~/.config/subctl/secrets.json) and reachable. Returns a status \
                          snapshot including key_present and sample_resolved."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, _input: &Value) -> std::result::Result<String, String> {
        let key = match resolve_context7_key().await {
            Some(k) => k,
            None => {
                return Err("CONTEXT7_API_KEY not configured (checked env + \
                     ~/.config/subctl/secrets.json)"
                    .to_string())
            }
        };
        let http = context7_http_client(self.timeout_secs);
        let result = context7_rpc(
            &http,
            &self.endpoint,
            &key,
            "resolve-library-id",
            json!({ "libraryName": "react", "query": "react" }),
        )
        .await;
        match result {
            Ok(r) => {
                let sample_resolved = r
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|a| !a.is_empty())
                    .unwrap_or(false);
                serde_json::to_string(&json!({
                    "ok": true,
                    "host": self.endpoint,
                    "key_present": true,
                    "sample_resolved": sample_resolved
                }))
                .map_err(|e| format!("serialize: {e}"))
            }
            Err(e) => Err(format!("context7 unreachable: {e}")),
        }
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
    /// EA2 — Cognee client for the `knowledge_graph_*` tools. Wrapped
    /// in `Arc` so it's cheap to clone into each tool struct.
    pub cognee: Arc<CogneeClient>,
    /// EA2 — Context7 configuration for the `context7_*` tools.
    pub context7: crate::config::Context7Config,
}

/// Build the full EA1+EA2 tool registry: five read-only tools, three
/// policy-gated action tools, and seven new EA2 knowledge tools.
#[must_use]
pub fn build_tool_registry(deps: &AgencyDeps) -> ToolRegistry {
    ToolRegistry::new()
        // ── EA1 read-only ──────────────────────────────────────────
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
        // ── EA1 action (policy-gated) ──────────────────────────────
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
        // ── EA2 knowledge — system_subctl_knowledge ────────────────
        .with_tool(Arc::new(SubctlKnowledgeTool))
        // ── EA2 knowledge — knowledge_graph_* ─────────────────────
        .with_tool(Arc::new(KgNeighborsTool {
            client: deps.cognee.clone(),
        }))
        .with_tool(Arc::new(KgPathTool {
            client: deps.cognee.clone(),
        }))
        .with_tool(Arc::new(KgQueryTool {
            client: deps.cognee.clone(),
        }))
        // ── EA2 knowledge — context7_* ─────────────────────────────
        .with_tool(Arc::new(Context7ResolveTool {
            endpoint: deps.context7.endpoint.clone(),
            timeout_secs: deps.context7.timeout_secs,
        }))
        .with_tool(Arc::new(Context7DocsTool {
            endpoint: deps.context7.endpoint.clone(),
            timeout_secs: deps.context7.timeout_secs,
        }))
        .with_tool(Arc::new(Context7HealthTool {
            endpoint: deps.context7.endpoint.clone(),
            timeout_secs: deps.context7.timeout_secs,
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
            cognee: Arc::new(evy_memory::CogneeClient::new(
                evy_memory::CogneeConfig::default(),
            )),
            context7: crate::config::Context7Config::default(),
        }
    }

    #[tokio::test]
    async fn registry_carries_all_fifteen_tools() {
        let (_d1, accounts) = write_temp(ACCOUNTS, "accounts.conf");
        let (_d2, snaps) = write_temp(SNAPSHOT, "usage.jsonl");
        let deps = test_deps(accounts, snaps, Arc::new(MockTmuxQuery::new()));
        let reg = build_tool_registry(&deps);
        assert_eq!(reg.count(), 15, "EA1 (8) + EA2 (7) = 15 tools");
        for name in [
            // EA1 read-only
            "evy_usage",
            "evy_sessions",
            "evy_workers",
            "evy_watchdogs",
            "evy_accounts",
            // EA1 action
            "evy_spawn_worker",
            "evy_kill_worker",
            "evy_notify_operator",
            // EA2 knowledge
            "system_subctl_knowledge",
            "knowledge_graph_neighbors",
            "knowledge_graph_path",
            "knowledge_graph_query",
            "context7_resolve",
            "context7_docs",
            "context7_health",
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

    // ─── EA2 knowledge tools ────────────────────────────────────────────

    use wiremock::matchers::{header, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── system_subctl_knowledge (local-asset, no network) ───────────────

    #[tokio::test]
    async fn knowledge_tool_lists_sections_with_summaries() {
        let tool = SubctlKnowledgeTool;
        let out = tool.execute(&json!({})).await.expect("ok");
        // The embedded asset has real sections; assert a few canonical ones
        // and the call-again hint.
        assert!(out.contains("sections available"), "got: {out}");
        assert!(out.contains("overview:"), "got: {out}");
        assert!(out.contains("agency_tools:"), "got: {out}");
        assert!(out.contains("cognee:"), "got: {out}");
        assert!(out.contains("context7:"), "got: {out}");
    }

    #[tokio::test]
    async fn knowledge_tool_returns_section_content_verbatim() {
        let tool = SubctlKnowledgeTool;
        let out = tool
            .execute(&json!({ "section": "overview" }))
            .await
            .expect("ok");
        // Section content begins with the `overview:` top-level key line.
        assert!(out.starts_with("overview:"), "got: {out}");
        assert!(out.contains("subctl v4"), "got: {out}");
        // Listing the *next* section's key must NOT leak into this one.
        assert!(
            !out.contains("\narchitecture:"),
            "section bled into the next: {out}"
        );
    }

    #[tokio::test]
    async fn knowledge_tool_unknown_section_errors_with_available_list() {
        let tool = SubctlKnowledgeTool;
        let err = tool
            .execute(&json!({ "section": "no_such_section" }))
            .await
            .expect_err("unknown section must error");
        assert!(
            err.contains("unknown section: no_such_section"),
            "got: {err}"
        );
        assert!(err.contains("Available:"), "got: {err}");
        assert!(err.contains("overview"), "got: {err}");
    }

    // ── knowledge_graph_* (mocked cognee endpoint) ──────────────────────

    /// Build a CogneeClient pointed at a wiremock server.
    fn cognee_client_for(uri: &str) -> Arc<CogneeClient> {
        Arc::new(CogneeClient::new(evy_memory::CogneeConfig {
            endpoint: uri.trim_end_matches('/').to_string(),
            api_key: None,
            timeout_secs: 5,
        }))
    }

    #[tokio::test]
    async fn kg_neighbors_round_trip_against_mocked_cognee() {
        let server = MockServer::start().await;
        // Reachability gate hits GET /health.
        Mock::given(method("GET"))
            .and(wm_path("/health"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        // The neighbors call hits POST /graph/neighbors.
        Mock::given(method("POST"))
            .and(wm_path("/graph/neighbors"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "node": { "id": "n1", "label": "ProjectX" },
                "neighbors": [
                    { "id": "n2", "rel": "DECIDED_BY" },
                    { "id": "n3", "rel": "TOUCHED_FILE" }
                ]
            })))
            .mount(&server)
            .await;

        let tool = KgNeighborsTool {
            client: cognee_client_for(&server.uri()),
        };
        let out = tool
            .execute(&json!({ "node_id": "n1", "depth": 2 }))
            .await
            .expect("ok");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["ok"], true);
        assert_eq!(v["node"]["label"], "ProjectX");
        assert_eq!(v["neighbors"].as_array().unwrap().len(), 2);
        assert_eq!(v["depth"], 2);

        // Assert the request body Cognee actually received.
        let recv = server.received_requests().await.expect("recorded");
        let post = recv
            .iter()
            .find(|r| r.url.path() == "/graph/neighbors")
            .expect("neighbors POST");
        let body: Value = serde_json::from_slice(&post.body).expect("json");
        assert_eq!(body["node_id"], "n1");
        assert_eq!(body["depth"], 2);
    }

    #[tokio::test]
    async fn kg_path_round_trip_reports_hop_count() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/health"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wm_path("/graph/path"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "nodes": [{ "id": "a" }, { "id": "b" }, { "id": "c" }],
                "edges": [{ "rel": "X" }, { "rel": "Y" }]
            })))
            .mount(&server)
            .await;

        let tool = KgPathTool {
            client: cognee_client_for(&server.uri()),
        };
        let out = tool
            .execute(&json!({ "from": "a", "to": "c" }))
            .await
            .expect("ok");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["ok"], true);
        // hop_count = nodes.len() - 1
        assert_eq!(v["hop_count"], 2);
        assert_eq!(v["nodes"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn kg_query_round_trip_reports_row_count() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/health"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wm_path("/graph/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rows": [{ "n": 1 }, { "n": 2 }, { "n": 3 }]
            })))
            .mount(&server)
            .await;

        let tool = KgQueryTool {
            client: cognee_client_for(&server.uri()),
        };
        let out = tool
            .execute(&json!({ "query": "MATCH (n) RETURN n" }))
            .await
            .expect("ok");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["ok"], true);
        assert_eq!(v["row_count"], 3);
        assert_eq!(v["query"], "MATCH (n) RETURN n");
    }

    #[tokio::test]
    async fn kg_tools_error_cleanly_when_cognee_unreachable() {
        // No server mounted at this port → /health probe fails → clean
        // NOT_READY error, never a panic or transport leak.
        let tool = KgNeighborsTool {
            client: cognee_client_for("http://127.0.0.1:1"),
        };
        let err = tool
            .execute(&json!({ "node_id": "n1" }))
            .await
            .expect_err("unreachable cognee must error cleanly");
        assert_eq!(err, KG_NOT_READY_ERR);
    }

    #[tokio::test]
    async fn kg_neighbors_requires_node_id() {
        let tool = KgNeighborsTool {
            client: cognee_client_for("http://127.0.0.1:1"),
        };
        let err = tool.execute(&json!({})).await.expect_err("must require");
        assert!(err.contains("`node_id` is required"), "got: {err}");
    }

    // ── context7_* (mocked MCP gateway) ─────────────────────────────────

    /// Mount a Context7 MCP gateway that returns an MCP tools/call result
    /// whose `content[0].text` is `text`. Asserts the CONTEXT7_API_KEY
    /// header is present.
    async fn mount_context7_ok(server: &MockServer, text: &str) {
        Mock::given(method("POST"))
            .and(header("CONTEXT7_API_KEY", "test-c7-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "content": [{ "type": "text", "text": text }]
                }
            })))
            .mount(server)
            .await;
    }

    /// Env is process-global and cargo runs tests in a binary in
    /// parallel; the `context7_*` tools resolve their key from
    /// `CONTEXT7_API_KEY` env + `$HOME/.config/subctl/secrets.json`.
    /// Serialize every context7 env-touching test through this mutex so
    /// they can't clobber each other's environment.
    static C7_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// While held: forces `CONTEXT7_API_KEY` to `key` and points `HOME`
    /// at an empty temp dir (so the FileBackend finds no real
    /// `secrets.json`). Restores both on drop. The mutex guard makes the
    /// env mutation effectively single-threaded.
    struct C7Env {
        _guard: std::sync::MutexGuard<'static, ()>,
        _home: tempfile::TempDir,
        prev_home: Option<String>,
        prev_key: Option<String>,
    }
    impl C7Env {
        /// `key = Some(..)` sets the env var; `None` ensures it's unset
        /// (exercising the missing-key path) while also isolating HOME so
        /// no real `secrets.json` leaks in.
        fn new(key: Option<&str>) -> Self {
            let guard = C7_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let home = tempfile::tempdir().expect("tempdir");
            let prev_home = std::env::var("HOME").ok();
            let prev_key = std::env::var("CONTEXT7_API_KEY").ok();
            std::env::set_var("HOME", home.path());
            match key {
                Some(k) => std::env::set_var("CONTEXT7_API_KEY", k),
                None => std::env::remove_var("CONTEXT7_API_KEY"),
            }
            C7Env {
                _guard: guard,
                _home: home,
                prev_home,
                prev_key,
            }
        }
    }
    impl Drop for C7Env {
        fn drop(&mut self) {
            match &self.prev_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
            match &self.prev_key {
                Some(k) => std::env::set_var("CONTEXT7_API_KEY", k),
                None => std::env::remove_var("CONTEXT7_API_KEY"),
            }
        }
    }

    #[tokio::test]
    async fn context7_resolve_round_trip_against_mock() {
        let _g = C7Env::new(Some("test-c7-key"));
        let server = MockServer::start().await;
        mount_context7_ok(&server, "library: /facebook/react (3000 snippets)").await;

        let tool = Context7ResolveTool {
            endpoint: server.uri(),
            timeout_secs: 5,
        };
        let out = tool
            .execute(&json!({ "library": "react" }))
            .await
            .expect("ok");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["ok"], true);
        assert_eq!(v["library"], "react");
        assert!(
            v["raw"].as_str().unwrap().contains("/facebook/react"),
            "got: {v}"
        );

        // Assert the JSON-RPC envelope on the wire.
        let recv = server.received_requests().await.expect("recorded");
        let body: Value = serde_json::from_slice(&recv[0].body).expect("json");
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], "resolve-library-id");
        assert_eq!(body["params"]["arguments"]["libraryName"], "react");
        assert_eq!(body["params"]["arguments"]["query"], "react");
    }

    #[tokio::test]
    async fn context7_docs_round_trip_against_mock() {
        let _g = C7Env::new(Some("test-c7-key"));
        let server = MockServer::start().await;
        mount_context7_ok(&server, "# useState\nReturns a stateful value...").await;

        let tool = Context7DocsTool {
            endpoint: server.uri(),
            timeout_secs: 5,
        };
        let out = tool
            .execute(&json!({ "library_id": "/facebook/react", "topic": "hooks" }))
            .await
            .expect("ok");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["ok"], true);
        assert_eq!(v["library_id"], "/facebook/react");
        assert_eq!(v["topic"], "hooks");
        assert!(v["docs"].as_str().unwrap().contains("useState"), "got: {v}");

        let recv = server.received_requests().await.expect("recorded");
        let body: Value = serde_json::from_slice(&recv[0].body).expect("json");
        assert_eq!(body["params"]["name"], "get-library-docs");
        assert_eq!(
            body["params"]["arguments"]["context7CompatibleLibraryID"],
            "/facebook/react"
        );
        assert_eq!(body["params"]["arguments"]["topic"], "hooks");
    }

    #[tokio::test]
    async fn context7_health_round_trip_against_mock() {
        let _g = C7Env::new(Some("test-c7-key"));
        let server = MockServer::start().await;
        mount_context7_ok(&server, "react resolved").await;

        let tool = Context7HealthTool {
            endpoint: server.uri(),
            timeout_secs: 5,
        };
        let out = tool.execute(&json!({})).await.expect("ok");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["ok"], true);
        assert_eq!(v["key_present"], true);
        assert_eq!(v["sample_resolved"], true);
    }

    #[tokio::test]
    async fn context7_handles_sse_framed_response() {
        let _g = C7Env::new(Some("test-c7-key"));
        let server = MockServer::start().await;
        // Context7 may answer as a single SSE event rather than JSON.
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"/vercel/next.js\"}]}}\n\n";
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sse.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let tool = Context7ResolveTool {
            endpoint: server.uri(),
            timeout_secs: 5,
        };
        let out = tool
            .execute(&json!({ "library": "next.js" }))
            .await
            .expect("ok");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["ok"], true);
        assert!(
            v["raw"].as_str().unwrap().contains("/vercel/next.js"),
            "SSE data line must be parsed: {v}"
        );
    }

    #[tokio::test]
    async fn context7_errors_when_api_key_absent() {
        // Key unset AND HOME isolated to an empty dir → neither env nor
        // the real ~/.config/subctl/secrets.json can supply a key.
        let _g = C7Env::new(None);
        let tool = Context7ResolveTool {
            endpoint: "http://127.0.0.1:1".to_string(),
            timeout_secs: 5,
        };
        let err = tool
            .execute(&json!({ "library": "react" }))
            .await
            .expect_err("missing key must error");
        assert!(
            err.contains("CONTEXT7_API_KEY not configured"),
            "got: {err}"
        );
    }
}
