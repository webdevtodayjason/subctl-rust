//! Daemon configuration. Layered TOML + env override via figment.
//!
//! Source priority (highest wins):
//!
//! 1. Process environment, double-underscore-nested, `EVY_` prefix.
//!    (`EVY_SCHEDULER__DB_PATH=/tmp/foo.db` overrides `[scheduler].db_path`.)
//! 2. TOML file at the path passed to [`Config::load_from`] (the binary
//!    resolves this from `$SUBCTL_EVY_CONFIG` falling back to
//!    `config/evy.toml`).
//!
//! # Provider config translation
//!
//! The on-disk TOML keys are intentionally operator-friendly
//! (`config_dir`, `account`); the construction-time structs in
//! `evy-providers` use the underlying env-var names
//! (`claude_config_dir`, `codex_home`). We bridge with intermediate
//! structs that own a `From` impl into the real config. This was flagged
//! as friction point #2 in the Slice E report — Phase 2 could reconcile
//! by changing the provider crate's field names, but that's a public-API
//! break we're not ready to make at Slice E close.
//!
//! # Phase 3 Slice E — comms + memory sections
//!
//! `[comms.http]`, `[comms.telegram]`, `[comms.discord]`, and `[memory]`
//! land alongside the existing provider config. Each of the four is
//! `#[serde(default)]` so the Slice E smoke test (which hand-constructs a
//! `Config` with only the original three sections) keeps compiling, and
//! so a daemon installation with no telegram/discord tokens falls through
//! without an "unset key" parse error. `[memory]` itself carries
//! production-shaped defaults (sqlite files under `/tmp/evy-*.db`,
//! playbooks under `./playbooks/`) so a fresh operator who runs `evy`
//! without authoring a TOML at all still boots — the daemon
//! [`crate::run_daemon`] creates the playbook directory if missing.

use std::path::PathBuf;

use anyhow::{Context, Result};
use evy_core::PolicyMode;
use evy_providers::{ClaudeCodeConfig, CodexConfig};
use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};

/// Root daemon config.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Scheduler-side configuration (db path, etc.).
    pub scheduler: SchedulerConfig,
    /// Policy gate configuration (path to the resolved policy.toml).
    pub policy: PolicyConfig,
    /// Per-provider configuration. Both providers are optional; at
    /// least one must be set or [`crate::run_daemon`] will refuse to boot.
    pub providers: ProvidersConfig,
    /// Multi-channel router configuration: HTTP/SSE plus optional
    /// Telegram and Discord bridges. Defaulted so an operator config
    /// that omits the section still boots a working HTTP surface on
    /// `127.0.0.1:8787`.
    #[serde(default)]
    pub comms: CommsConfig,
    /// Learning-loop substrate configuration: observation log, playbook
    /// store, scoring ledger, preference model, optional claude-mem db.
    /// Defaulted so a fresh install boots without a TOML.
    #[serde(default)]
    pub memory: MemoryConfig,
    /// Skill catalog configuration: directory the daemon hands to
    /// `evy_skills::SkillRegistry::load` at boot. Defaulted so a fresh
    /// install boots with `skills/` resolved relative to the working dir.
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Optional thinking-partner configuration. When absent (the
    /// default), the daemon boots without a chat surface; the
    /// `POST /api/evy/chat` endpoint then returns 503. Phase 6.
    #[serde(default)]
    pub thinking_partner: Option<ThinkingPartnerSectionConfig>,
}

/// `[scheduler]` table.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchedulerConfig {
    /// sqlite db path the scheduler opens; migrations run automatically.
    pub db_path: PathBuf,
}

/// `[policy]` table.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PolicyConfig {
    /// Path to a single policy TOML file consumed by
    /// [`evy_policy::load_policy`].
    pub path: PathBuf,
}

/// `[providers]` table.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProvidersConfig {
    /// `[providers.claude_code]` — optional.
    #[serde(default)]
    pub claude_code: Option<ClaudeCodeConfigToml>,
    /// `[providers.codex]` — optional.
    #[serde(default)]
    pub codex: Option<CodexConfigToml>,
}

/// On-disk shape for `[providers.claude_code]`. Maps to
/// [`evy_providers::ClaudeCodeConfig`] via `From`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClaudeCodeConfigToml {
    /// `CLAUDE_CONFIG_DIR` — picks the per-account auth + settings dir.
    pub config_dir: PathBuf,
    /// Detached tmux session that owns the worker windows.
    pub tmux_session: String,
    /// Working directory the `claude` CLI launches in.
    pub working_dir: PathBuf,
    /// Policy mode (`"Trusted" | "Gated" | "Sealed"`).
    pub policy_mode: PolicyMode,
    /// Absolute path to the `claude` binary. Optional — defaults to
    /// `~/.local/bin/claude` (the native install) when omitted, so
    /// existing configs keep working. Set this to pin a specific binary
    /// and stay independent of the launchd/tmux PATH.
    #[serde(default)]
    pub claude_bin: Option<PathBuf>,
}

/// Default `claude` binary path — `~/.local/bin/claude`, the native
/// install. Falls back to a bare `claude` name only if `$HOME` is unset
/// (which does not happen for the launchd-run daemon).
fn default_claude_bin() -> PathBuf {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".local/bin/claude"))
        .unwrap_or_else(|| PathBuf::from("claude"))
}

impl From<ClaudeCodeConfigToml> for ClaudeCodeConfig {
    fn from(toml: ClaudeCodeConfigToml) -> Self {
        // `hmac_key` is intentionally `None` at TOML-deserialize time:
        // the daemon mints a per-session key at boot and attaches it
        // via `with_hmac_key` before the config is handed to the
        // provider constructor. Persisting the key in TOML would
        // contradict ADR 0011's secret-hygiene rule.
        Self {
            claude_config_dir: toml.config_dir,
            claude_bin: toml.claude_bin.unwrap_or_else(default_claude_bin),
            tmux_session: toml.tmux_session,
            working_dir: toml.working_dir,
            policy_mode: toml.policy_mode,
            hmac_key: None,
        }
    }
}

/// On-disk shape for `[providers.codex]`. Maps to
/// [`evy_providers::CodexConfig`] via `From`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CodexConfigToml {
    /// `CODEX_HOME` — picks the per-account `auth.json` + `config.toml`.
    pub codex_home: PathBuf,
    /// Detached tmux session that owns the worker windows.
    pub tmux_session: String,
    /// Working directory the `codex` CLI launches in.
    pub working_dir: PathBuf,
    /// Optional model override (e.g. `"gpt-5.5"`).
    #[serde(default)]
    pub model: Option<String>,
    /// Policy mode (`"Trusted" | "Gated" | "Sealed"`).
    pub policy_mode: PolicyMode,
}

impl From<CodexConfigToml> for CodexConfig {
    fn from(toml: CodexConfigToml) -> Self {
        // See ClaudeCodeConfigToml::from — `hmac_key` is attached at
        // daemon boot, never serialized.
        Self {
            codex_home: toml.codex_home,
            tmux_session: toml.tmux_session,
            working_dir: toml.working_dir,
            model: toml.model,
            policy_mode: toml.policy_mode,
            hmac_key: None,
        }
    }
}

// ─── Phase 3 Slice E — comms ──────────────────────────────────────────

/// `[comms]` table — wraps the three channel-specific configs.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CommsConfig {
    /// `[comms.http]` — HTTP/SSE dashboard bind config.
    #[serde(default)]
    pub http: HttpSectionConfig,
    /// `[comms.telegram]` — optional; daemon skips spawning the bridge
    /// when absent.
    #[serde(default)]
    pub telegram: Option<TelegramSectionConfig>,
    /// `[comms.discord]` — optional; daemon skips spawning the bridge
    /// when absent.
    #[serde(default)]
    pub discord: Option<DiscordSectionConfig>,
}

/// On-disk shape for `[comms.http]`. Maps to [`evy_comms::HttpConfig`].
///
/// Defaults bind loopback on the v3 dashboard port (`127.0.0.1:8787`)
/// so existing curl recipes keep working out of the box.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpSectionConfig {
    /// Bind host. Default `"127.0.0.1"`.
    pub host: String,
    /// Bind port. Default `8787` (v3 dashboard parity).
    pub port: u16,
    /// CORS allowlist. Default empty (CORS disabled).
    #[serde(default)]
    pub allow_origins: Vec<String>,
    /// Cutover Phase 0: directory whose contents v4 serves as the dashboard
    /// UI (via `ServeDir` fallback). `None` = API-only. Absolute path.
    #[serde(default)]
    pub static_dir: Option<std::path::PathBuf>,
}

impl Default for HttpSectionConfig {
    fn default() -> Self {
        Self {
            host: evy_comms::DEFAULT_HOST.to_owned(),
            port: evy_comms::DEFAULT_PORT,
            allow_origins: Vec::new(),
            static_dir: None,
        }
    }
}

impl From<HttpSectionConfig> for evy_comms::HttpConfig {
    fn from(s: HttpSectionConfig) -> Self {
        Self {
            host: s.host,
            port: s.port,
            allow_origins: s.allow_origins,
            // Cutover Phase 0: the dashboard bundle dir, if the operator set
            // `[comms.http] static_dir` in config.toml.
            static_dir: s.static_dir,
        }
    }
}

/// On-disk shape for `[comms.telegram]`. Maps to
/// [`evy_comms::TelegramConfig`].
///
/// Operators source `bot_token` from `@BotFather` and `chat_id` from
/// `/getUpdates` against the bot after sending it a message; both are
/// typically captured under env `TELEGRAM_BOT_TOKEN` /
/// `TELEGRAM_CHAT_ID` and overridden via figment env layering.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramSectionConfig {
    /// Bot API token (`@BotFather`). Never logged.
    pub bot_token: String,
    /// Authorized chat id. Inbound messages from other chats are dropped.
    pub chat_id: i64,
    /// Inbound poll interval, milliseconds. Default 1000.
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    /// `timeout=` query argument to `getUpdates`, milliseconds.
    /// Default 25000.
    #[serde(default = "default_long_poll_timeout_ms")]
    pub long_poll_timeout_ms: u64,
}

const fn default_poll_interval_ms() -> u64 {
    1000
}

const fn default_long_poll_timeout_ms() -> u64 {
    25_000
}

impl From<TelegramSectionConfig> for evy_comms::TelegramConfig {
    fn from(s: TelegramSectionConfig) -> Self {
        let mut cfg = Self::new(s.bot_token, s.chat_id);
        cfg.poll_interval = std::time::Duration::from_millis(s.poll_interval_ms);
        cfg.long_poll_timeout = std::time::Duration::from_millis(s.long_poll_timeout_ms);
        cfg
    }
}

/// On-disk shape for `[comms.discord]`. Maps to
/// [`evy_comms::DiscordConfig`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiscordSectionConfig {
    /// Bot API token. Never logged.
    pub bot_token: String,
    /// Authorized channel id (Discord snowflake).
    pub channel_id: u64,
    /// Bot's application id (Discord snowflake). Required for slash-
    /// command registration.
    pub application_id: u64,
}

impl From<DiscordSectionConfig> for evy_comms::DiscordConfig {
    fn from(s: DiscordSectionConfig) -> Self {
        Self::new(s.bot_token, s.channel_id, s.application_id)
    }
}

// ─── Phase 3 Slice E — memory ─────────────────────────────────────────

/// `[memory]` table — paths the learning-loop substrate opens.
///
/// All four sqlite files may safely point at the *same* path: the
/// evy-memory migrator is idempotent across stores and each table has a
/// distinct name. We default to four separate `/tmp/evy-*.db` files so
/// a fresh install boots without any operator config, AND so an
/// operator who wants to wipe one substrate (`rm /tmp/evy-scores.db`)
/// doesn't accidentally take out the observation log.
///
/// Feedback rows live in `score_db` — the brief enumerated four db
/// paths and `FeedbackIngest::open` accepts any of them (migrations
/// are workspace-shared). Reusing `score_db` keeps the config table
/// short while still letting operators isolate by tearing down that
/// one file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemoryConfig {
    /// sqlite path the observation log opens. Default
    /// `/tmp/evy-observations.db`.
    pub observation_db: PathBuf,
    /// Directory of operator-authored `.md` playbooks. Created if
    /// missing at daemon boot. Default `playbooks/`.
    pub playbook_dir: PathBuf,
    /// sqlite path the scoring ledger + feedback ingest open. Default
    /// `/tmp/evy-scores.db`.
    pub score_db: PathBuf,
    /// sqlite path the operator-preference model opens. Default
    /// `/tmp/evy-preferences.db`.
    pub preferences_db: PathBuf,
    /// Optional path to the operator's claude-mem sqlite db (read-only
    /// consumer). `None` = retrieval falls open without cross-session
    /// priors.
    #[serde(default)]
    pub claude_mem_db: Option<PathBuf>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            observation_db: PathBuf::from("/tmp/evy-observations.db"),
            playbook_dir: PathBuf::from("playbooks"),
            score_db: PathBuf::from("/tmp/evy-scores.db"),
            preferences_db: PathBuf::from("/tmp/evy-preferences.db"),
            claude_mem_db: None,
        }
    }
}

// ─── Phase 5 Slice — skills ───────────────────────────────────────────

/// `[skills]` table — points the daemon at the skill catalog directory
/// `evy_skills::SkillRegistry::load` walks at boot.
///
/// Default resolves to `skills/` relative to the daemon's working
/// directory so a fresh install with the in-tree catalog boots without
/// operator config. Set `enabled = false` to skip catalog load entirely
/// (useful for boot smoke tests and minimal-deploy debugging).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SkillsConfig {
    /// Directory the registry walks (`<directory>/*/SKILL.md`).
    pub directory: PathBuf,
    /// When `false`, the daemon skips loading the catalog. Default `true`.
    pub enabled: bool,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("skills"),
            enabled: true,
        }
    }
}

// ─── Phase 6 — thinking-partner ───────────────────────────────────────

/// `[thinking_partner]` table — wires the chat surface to an LLM
/// backend. Absent → no chat endpoint is served (503).
///
/// The backend selector is stringly-typed today (`"lm-studio"`,
/// `"codex"`, `"anthropic"`, or `"stub"`) so the operator's TOML stays
/// readable without a custom serde adapter. The daemon validates the
/// value at boot.
///
/// # Choosing a backend
///
/// - `"lm-studio"` — local OpenAI-compatible model (LM Studio binds
///   `127.0.0.1:1234` by default). Free, fast, private, no API key
///   required. **Recommended default** for lightweight operator chat.
/// - `"codex"` — Codex Responses API using the operator's existing
///   ChatGPT Pro subscription via OAuth tokens minted by Phase 4
///   Slice D's device-flow. Requires `[thinking_partner.codex]` with
///   the account alias. **Preferred** when more reasoning power is
///   needed without a separate API key purchase.
/// - `"anthropic"` — direct Anthropic Messages API. Highest quality;
///   requires a paid `ANTHROPIC_API_KEY` (or whatever
///   [`api_key_env`](Self::api_key_env) names).
/// - `"stub"` — fixed-reply backend for smoke tests; never touches a
///   network.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThinkingPartnerSectionConfig {
    /// Backend selector. `"lm-studio"` (local), `"codex"` (ChatGPT Pro
    /// OAuth), `"anthropic"` (paid API key), or `"stub"` (smoke tests).
    #[serde(default = "default_backend")]
    pub backend: String,
    /// Environment variable holding the Anthropic API key. Default
    /// `ANTHROPIC_API_KEY`. Only consulted when `backend = "anthropic"`.
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    /// Optional model override. For `backend = "anthropic"` this is
    /// e.g. `"claude-sonnet-4-5"`; for `backend = "codex"` this is the
    /// Codex model ID (e.g. `"gpt-5.5"`); for `backend = "lm-studio"`
    /// this is the LM Studio model identifier (only required when
    /// multiple models are loaded). When absent, each backend's pinned
    /// default applies.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional `max_tokens` override sent on every request.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Optional `[thinking_partner.lm_studio]` block — LM Studio
    /// endpoint + temperature overrides. Only consulted when
    /// `backend = "lm-studio"`; absent = backend defaults
    /// (`http://127.0.0.1:1234`, temperature `0.7`).
    #[serde(default)]
    pub lm_studio: Option<LmStudioSectionConfig>,
    /// `[thinking_partner.codex]` sub-section. Required when
    /// `backend = "codex"`; ignored otherwise.
    #[serde(default)]
    pub codex: Option<CodexSectionConfig>,
}

/// `[thinking_partner.codex]` sub-table — points the Codex backend at
/// the operator's existing OAuth bundle.
///
/// The tokens themselves live on disk: `accounts_conf_path` is the
/// pipe-delimited roster (one row per provider/account), and the
/// `auth.json` referenced by that row holds the actual JWT + refresh
/// token. The Codex backend reads both lazily on every chat turn so a
/// background `subctl auth codex` refresh is picked up without a daemon
/// restart.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CodexSectionConfig {
    /// Path to `accounts.conf`. Default
    /// `~/.config/subctl/accounts.conf`. Operator typically leaves this
    /// at the default — only override for parallel-test mode that
    /// isolates v4 from v3 state.
    #[serde(default = "default_accounts_conf_path")]
    pub accounts_conf_path: PathBuf,
    /// Which alias in `accounts.conf` to use (e.g. `openai-jason`).
    pub account: String,
    /// Override the Codex API endpoint. Default
    /// `https://chatgpt.com/backend-api/codex`. Tests + dev setups
    /// override this; production operators should leave it absent.
    #[serde(default)]
    pub endpoint: Option<String>,
}

fn default_backend() -> String {
    "anthropic".to_string()
}

fn default_api_key_env() -> String {
    "ANTHROPIC_API_KEY".to_string()
}

/// On-disk shape for `[thinking_partner.lm_studio]`. Maps to
/// [`evy_thinking::LmStudioConfig`] in [`crate::run_daemon`].
///
/// Each field is `#[serde(default)]` so an operator can author the
/// block with only the field they're overriding (most commonly
/// `endpoint` when LM Studio is bound on a non-default port). When the
/// whole block is omitted, [`evy_thinking::LmStudioConfig::default`]
/// applies — `127.0.0.1:1234`, no pinned model.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LmStudioSectionConfig {
    /// Base URL the daemon hits. Default
    /// [`evy_thinking::DEFAULT_LM_STUDIO_ENDPOINT`]
    /// (`http://127.0.0.1:1234`). Tests / non-default LM Studio
    /// installs override this.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Optional sampling temperature override. Default `0.7`.
    #[serde(default)]
    pub temperature: Option<f32>,
}

fn default_accounts_conf_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    home.join(".config").join("subctl").join("accounts.conf")
}

impl Config {
    /// Resolve from the binary's default sources: `$SUBCTL_EVY_CONFIG`
    /// falling back to `./config/evy.toml`, plus env override.
    pub fn load() -> Result<Self> {
        let path = std::env::var("SUBCTL_EVY_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("config/evy.toml"));
        Self::load_from(&path)
    }

    /// Resolve from an explicit TOML path plus env override.
    ///
    /// `EVY_<SECTION>__<KEY>` env vars override their TOML siblings.
    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        let fig = Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("EVY_").split("__"));
        fig.extract()
            .with_context(|| format!("parsing evy config from {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_toml(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn full_config_parses() {
        let f = write_toml(
            r#"
[scheduler]
db_path = "/tmp/evy.db"

[policy]
path = "/tmp/policy.toml"

[providers.claude_code]
config_dir = "/tmp/claude"
tmux_session = "claude-smoke"
working_dir = "."
policy_mode = "Trusted"

[providers.codex]
codex_home = "/tmp/codex"
tmux_session = "codex-smoke"
working_dir = "."
policy_mode = "Trusted"
"#,
        );
        let cfg = Config::load_from(f.path()).expect("parse");
        assert_eq!(cfg.scheduler.db_path, PathBuf::from("/tmp/evy.db"));
        assert!(cfg.providers.claude_code.is_some());
        assert!(cfg.providers.codex.is_some());
        // Comms + memory absent on disk — defaults must fill in.
        assert_eq!(cfg.comms.http.port, evy_comms::DEFAULT_PORT);
        assert!(cfg.comms.telegram.is_none());
        assert!(cfg.comms.discord.is_none());
        assert_eq!(
            cfg.memory.observation_db,
            PathBuf::from("/tmp/evy-observations.db")
        );
        // Skills section omitted on disk — defaults must fill in.
        assert_eq!(cfg.skills.directory, PathBuf::from("skills"));
        assert!(cfg.skills.enabled);
    }

    #[test]
    fn missing_providers_section_is_ok() {
        let f = write_toml(
            r#"
[scheduler]
db_path = "/tmp/evy.db"

[policy]
path = "/tmp/policy.toml"

[providers]
"#,
        );
        let cfg = Config::load_from(f.path()).expect("parse");
        assert!(cfg.providers.claude_code.is_none());
        assert!(cfg.providers.codex.is_none());
    }

    #[test]
    fn into_claude_config_maps_fields() {
        let t = ClaudeCodeConfigToml {
            config_dir: PathBuf::from("/cfg"),
            tmux_session: "s".into(),
            working_dir: PathBuf::from("/w"),
            policy_mode: PolicyMode::Gated,
            claude_bin: None,
        };
        let c: ClaudeCodeConfig = t.into();
        assert_eq!(c.claude_config_dir, PathBuf::from("/cfg"));
        assert_eq!(c.tmux_session, "s");
        // Omitted claude_bin → defaults to ~/.local/bin/claude (native).
        assert!(
            c.claude_bin.ends_with(".local/bin/claude"),
            "default claude_bin should be ~/.local/bin/claude, got {:?}",
            c.claude_bin
        );

        // Explicit claude_bin is honored verbatim.
        let t2 = ClaudeCodeConfigToml {
            config_dir: PathBuf::from("/cfg"),
            tmux_session: "s".into(),
            working_dir: PathBuf::from("/w"),
            policy_mode: PolicyMode::Gated,
            claude_bin: Some(PathBuf::from("/opt/claude/bin/claude")),
        };
        let c2: ClaudeCodeConfig = t2.into();
        assert_eq!(c2.claude_bin, PathBuf::from("/opt/claude/bin/claude"));
    }

    #[test]
    fn into_codex_config_maps_fields() {
        let t = CodexConfigToml {
            codex_home: PathBuf::from("/h"),
            tmux_session: "s".into(),
            working_dir: PathBuf::from("/w"),
            model: Some("gpt-5.5".into()),
            policy_mode: PolicyMode::Sealed,
        };
        let c: CodexConfig = t.into();
        assert_eq!(c.codex_home, PathBuf::from("/h"));
        assert_eq!(c.model.as_deref(), Some("gpt-5.5"));
    }

    #[test]
    fn comms_section_parses_with_telegram_and_discord() {
        let f = write_toml(
            r#"
[scheduler]
db_path = "/tmp/evy.db"

[policy]
path = "/tmp/policy.toml"

[providers]

[comms.http]
host = "127.0.0.1"
port = 9999
allow_origins = ["http://localhost:3000"]

[comms.telegram]
bot_token = "test-token"
chat_id = -1001234567890
poll_interval_ms = 500
long_poll_timeout_ms = 30000

[comms.discord]
bot_token = "discord-token"
channel_id = 111
application_id = 222
"#,
        );
        let cfg = Config::load_from(f.path()).expect("parse");
        assert_eq!(cfg.comms.http.port, 9999);
        let tg = cfg.comms.telegram.expect("telegram present");
        assert_eq!(tg.chat_id, -1001234567890);
        assert_eq!(tg.poll_interval_ms, 500);
        let dc = cfg.comms.discord.expect("discord present");
        assert_eq!(dc.application_id, 222);
    }

    #[test]
    fn memory_section_parses_and_overrides_defaults() {
        let f = write_toml(
            r#"
[scheduler]
db_path = "/tmp/evy.db"

[policy]
path = "/tmp/policy.toml"

[providers]

[memory]
observation_db = "/tmp/custom-obs.db"
playbook_dir = "/tmp/custom-playbooks"
score_db = "/tmp/custom-scores.db"
preferences_db = "/tmp/custom-prefs.db"
claude_mem_db = "/Users/sem/.claude-mem/db.sqlite"
"#,
        );
        let cfg = Config::load_from(f.path()).expect("parse");
        assert_eq!(
            cfg.memory.observation_db,
            PathBuf::from("/tmp/custom-obs.db")
        );
        assert_eq!(
            cfg.memory.playbook_dir,
            PathBuf::from("/tmp/custom-playbooks")
        );
        assert_eq!(
            cfg.memory.claude_mem_db,
            Some(PathBuf::from("/Users/sem/.claude-mem/db.sqlite"))
        );
    }

    #[test]
    fn http_section_into_evy_comms_config() {
        let s = HttpSectionConfig {
            host: "0.0.0.0".into(),
            port: 7000,
            allow_origins: vec!["http://x".into()],
            static_dir: Some("/var/lib/evy/web".into()),
        };
        let c: evy_comms::HttpConfig = s.into();
        assert_eq!(c.host, "0.0.0.0");
        assert_eq!(c.port, 7000);
        assert_eq!(c.allow_origins, vec!["http://x".to_owned()]);
        assert_eq!(c.static_dir, Some("/var/lib/evy/web".into()));
    }

    #[test]
    fn telegram_section_into_evy_comms_config() {
        let s = TelegramSectionConfig {
            bot_token: "tok".into(),
            chat_id: 42,
            poll_interval_ms: 100,
            long_poll_timeout_ms: 200,
        };
        let c: evy_comms::TelegramConfig = s.into();
        assert_eq!(c.bot_token, "tok");
        assert_eq!(c.chat_id, 42);
        assert_eq!(c.poll_interval, std::time::Duration::from_millis(100));
        assert_eq!(c.long_poll_timeout, std::time::Duration::from_millis(200));
    }

    #[test]
    fn discord_section_into_evy_comms_config() {
        let s = DiscordSectionConfig {
            bot_token: "tok".into(),
            channel_id: 100,
            application_id: 200,
        };
        let c: evy_comms::DiscordConfig = s.into();
        assert_eq!(c.bot_token, "tok");
        assert_eq!(c.channel_id, 100);
        assert_eq!(c.application_id, 200);
    }

    #[test]
    fn skills_section_parses_and_overrides_defaults() {
        let f = write_toml(
            r#"
[scheduler]
db_path = "/tmp/evy.db"

[policy]
path = "/tmp/policy.toml"

[providers]

[skills]
directory = "/opt/evy/skills"
enabled = false
"#,
        );
        let cfg = Config::load_from(f.path()).expect("parse");
        assert_eq!(cfg.skills.directory, PathBuf::from("/opt/evy/skills"));
        assert!(!cfg.skills.enabled);
    }

    #[test]
    fn skills_defaults_are_relative_skills_dir() {
        let s = SkillsConfig::default();
        assert_eq!(s.directory, PathBuf::from("skills"));
        assert!(s.enabled);
    }

    #[test]
    fn thinking_partner_codex_section_parses() {
        let f = write_toml(
            r#"
[scheduler]
db_path = "/tmp/evy.db"

[policy]
path = "/tmp/policy.toml"

[providers]

[thinking_partner]
backend = "codex"
model = "gpt-5.5"
max_tokens = 2048

[thinking_partner.codex]
accounts_conf_path = "/tmp/test/accounts.conf"
account = "openai-jason"
endpoint = "https://chatgpt.com/backend-api/codex"
"#,
        );
        let cfg = Config::load_from(f.path()).expect("parse");
        let tp = cfg.thinking_partner.expect("thinking_partner present");
        assert_eq!(tp.backend, "codex");
        assert_eq!(tp.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(tp.max_tokens, Some(2048));
        let codex = tp.codex.expect("codex sub-section present");
        assert_eq!(
            codex.accounts_conf_path,
            PathBuf::from("/tmp/test/accounts.conf")
        );
        assert_eq!(codex.account, "openai-jason");
        assert_eq!(
            codex.endpoint.as_deref(),
            Some("https://chatgpt.com/backend-api/codex")
        );
    }

    #[test]
    fn thinking_partner_codex_endpoint_optional_and_accounts_conf_defaults() {
        // Operator omits both the endpoint and the accounts.conf path
        // — defaults must fill in, only `account` is mandatory.
        let f = write_toml(
            r#"
[scheduler]
db_path = "/tmp/evy.db"

[policy]
path = "/tmp/policy.toml"

[providers]

[thinking_partner]
backend = "codex"

[thinking_partner.codex]
account = "openai-jason"
"#,
        );
        let cfg = Config::load_from(f.path()).expect("parse");
        let tp = cfg.thinking_partner.expect("thinking_partner present");
        let codex = tp.codex.expect("codex sub-section present");
        assert_eq!(codex.account, "openai-jason");
        assert!(codex.endpoint.is_none());
        // Default accounts.conf path lives under $HOME/.config/subctl.
        // We don't assert the exact path because $HOME varies per CI.
        assert!(codex.accounts_conf_path.ends_with("accounts.conf"));
    }

    #[test]
    fn memory_defaults_are_tmp_paths() {
        let m = MemoryConfig::default();
        assert_eq!(m.observation_db, PathBuf::from("/tmp/evy-observations.db"));
        assert_eq!(m.score_db, PathBuf::from("/tmp/evy-scores.db"));
        assert_eq!(m.preferences_db, PathBuf::from("/tmp/evy-preferences.db"));
        assert!(m.claude_mem_db.is_none());
    }

    #[test]
    fn thinking_partner_with_lm_studio_backend_parses() {
        let f = write_toml(
            r#"
[scheduler]
db_path = "/tmp/evy.db"

[policy]
path = "/tmp/policy.toml"

[providers]

[thinking_partner]
backend = "lm-studio"
model = "gemma-4-26b-a4b-it-mlx"
max_tokens = 1024

[thinking_partner.lm_studio]
endpoint = "http://127.0.0.1:9999"
temperature = 0.3
"#,
        );
        let cfg = Config::load_from(f.path()).expect("parse");
        let tp = cfg.thinking_partner.expect("present");
        assert_eq!(tp.backend, "lm-studio");
        assert_eq!(tp.model.as_deref(), Some("gemma-4-26b-a4b-it-mlx"));
        assert_eq!(tp.max_tokens, Some(1024));
        let lm = tp.lm_studio.expect("lm_studio block present");
        assert_eq!(lm.endpoint.as_deref(), Some("http://127.0.0.1:9999"));
        assert_eq!(lm.temperature, Some(0.3));
    }

    #[test]
    fn thinking_partner_with_lm_studio_backend_omitting_lm_studio_block_parses() {
        // An operator who just wants the defaults (127.0.0.1:1234,
        // temperature 0.7) shouldn't have to author the sub-block at
        // all. Guard against accidentally making it required.
        let f = write_toml(
            r#"
[scheduler]
db_path = "/tmp/evy.db"

[policy]
path = "/tmp/policy.toml"

[providers]

[thinking_partner]
backend = "lm-studio"
"#,
        );
        let cfg = Config::load_from(f.path()).expect("parse");
        let tp = cfg.thinking_partner.expect("present");
        assert_eq!(tp.backend, "lm-studio");
        assert!(tp.lm_studio.is_none());
    }
}
