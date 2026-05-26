//! Discord bridge — REST `POST /channels/{id}/messages` for outbound
//! notifications + gateway-driven `/ask-reply` slash command for ask
//! resolution.
//!
//! Architecture
//! ------------
//! ```text
//!   daemon ────────────▶ DiscordBridge.notify(Notification)
//!                              │
//!                              ▼
//!                     serenity Http POST /channels/{id}/messages  ──▶  Discord
//!
//!   daemon ────────────▶ DiscordBridge.ask(question, timeout)
//!                              │
//!              registry.open() ▼  + send_embed(AskPending)
//!                              │
//!                              ▼  ── parks on AskRegistry::wait_for ──▶
//!
//!   operator  ──▶ /ask-reply ask_id:<uuid> answer:<text>  ─▶  gateway WS
//!                              │
//!                       EventHandler::interaction_create
//!                              │
//!                              ▼
//!                  DiscordBridge::resolve_from_slash  (testable, no serenity)
//!                              │
//!                              ▼
//!                  AskRegistry::resolve(ask_id, answer)
//! ```
//!
//! Why the slash-command path (vs. Telegram-style reply-to-message)
//! ----------------------------------------------------------------
//! Discord's `MESSAGE_CONTENT` is a privileged intent that requires a
//! verification step for bots in 100+ servers, and channel messages
//! aren't a first-class "reply to ask N" UX in the Discord client.
//! `/ask-reply` is explicit (operator picks the ask id), works with
//! zero privileged intents, and renders as a typed slash autocomplete.
//!
//! Why we render to our own [`Embed`] type
//! ---------------------------------------
//! Golden-file fixtures over serenity's [`serenity::builder::CreateEmbed`]
//! would break every time the serenity version bumps and re-shapes the
//! internal builder fields. Our [`Embed`] is owned, plain-serde, and
//! converted to a `CreateEmbed` only at the send boundary in
//! [`Embed::into_create_embed`].
//!
//! Errors
//! ------
//! Per the v4.0 workspace constraint we can't add a new `Error::Comms`
//! variant to `evy-core` from this slice (the orchestrator owns that
//! merge). Discord-shaped failures land on `Error::Provider { kind:
//! ClaudeCode, reason: "discord: ..." }` — the `"discord:"` prefix is
//! the breadcrumb that disambiguates at the operator's logs. A TODO
//! comment on every callsite flags the swap once `Error::Comms` is
//! available. (Telegram bridge uses the identical pattern; matching it
//! keeps merge churn down.)

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use evy_core::{Error, ProviderKind, Result, WorkerStatus};

use serenity::all::{
    ApplicationId, ChannelId, CommandDataOptionValue, CommandOptionType, CommandType, Context,
    CreateCommand, CreateCommandOption, CreateEmbed, CreateEmbedFooter, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, EventHandler, GatewayIntents, Http,
    Interaction, Ready,
};
use serenity::Client;

use crate::ask::{AskId, AskRegistry};
use crate::discord_config::DiscordConfig;
use crate::notification::Notification;

// ─── Slash command name (used at registration time and at dispatch time) ─

/// The single global slash command this bridge registers.
const ASK_REPLY_COMMAND: &str = "ask-reply";
/// Slash-command option name for the ask id (UUID string).
const ASK_REPLY_OPT_ID: &str = "ask_id";
/// Slash-command option name for the operator's answer text.
const ASK_REPLY_OPT_ANSWER: &str = "answer";

// ─── Color palette ────────────────────────────────────────────────────────
//
// Discord embed colors are 24-bit RGB packed into a u32. The palette below
// is intentionally small + stable so golden fixtures stay legible. Codex /
// Claude / DeepSeek lean on each provider's brand color so operators can
// pattern-match a notification's sidebar at a glance.

/// Anthropic / claude-code brand orange.
pub const COLOR_CLAUDE: u32 = 0x00CC_785C;
/// OpenAI / codex brand teal-green.
pub const COLOR_CODEX: u32 = 0x0010_A37F;
/// DeepSeek brand indigo-blue.
pub const COLOR_DEEPSEEK: u32 = 0x004D_6BFE;
/// Discord-native success green (`#57F287`).
pub const COLOR_SUCCESS: u32 = 0x0057_F287;
/// Discord-native error red (`#ED4245`).
pub const COLOR_ERROR: u32 = 0x00ED_4245;
/// Discord-native neutral blurple (`#5865F2`).
pub const COLOR_NEUTRAL: u32 = 0x0058_65F2;
/// Discord-native warning amber (`#FEE75C`).
pub const COLOR_WARN: u32 = 0x00FE_E75C;

fn provider_color(p: ProviderKind) -> u32 {
    match p {
        ProviderKind::ClaudeCode => COLOR_CLAUDE,
        ProviderKind::Codex => COLOR_CODEX,
        ProviderKind::DeepSeek => COLOR_DEEPSEEK,
    }
}

// ─── Embed shape we own (golden-file fixture target) ─────────────────────

/// Single key/value field inside an [`Embed`]. `inline` controls whether
/// Discord lays fields side-by-side or stacked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmbedField {
    /// Field label (≤ 256 chars per Discord limits).
    pub name: String,
    /// Field value (≤ 1024 chars per Discord limits).
    pub value: String,
    /// Whether Discord may render this field on the same row as the next.
    pub inline: bool,
}

/// Owned, version-agnostic representation of a Discord rich embed.
///
/// Why not [`serenity::builder::CreateEmbed`] directly? Two reasons:
/// 1. `CreateEmbed`'s internal shape is a serenity implementation detail.
///    Pinning golden fixtures to it couples our tests to a transitive
///    library version.
/// 2. The daemon may want to forward the embed JSON to other channels
///    (webhook, audit log) in the future — a plain serde struct is the
///    portable hand-off, not a builder.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Embed {
    /// Embed title rendered in bold above the description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Free-form body text. Discord supports Markdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 24-bit packed RGB rendered as the embed's left sidebar.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<u32>,
    /// Footer text (no icon for v4; keeps the fixture small).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<String>,
    /// Fields rendered as a 2-column grid (when `inline = true`) below
    /// the description.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<EmbedField>,
}

impl Embed {
    /// Convert into a serenity `CreateEmbed` at the send boundary.
    fn into_create_embed(self) -> CreateEmbed {
        let mut e = CreateEmbed::new();
        if let Some(t) = self.title {
            e = e.title(t);
        }
        if let Some(d) = self.description {
            e = e.description(d);
        }
        if let Some(c) = self.color {
            e = e.color(c);
        }
        if let Some(f) = self.footer {
            e = e.footer(CreateEmbedFooter::new(f));
        }
        for field in self.fields {
            e = e.field(field.name, field.value, field.inline);
        }
        e
    }
}

/// Render a [`Notification`] as a Discord [`Embed`].
///
/// Public — golden fixtures and any future channel that wants the same
/// rich-message rendering (webhook, audit log) reuse it.
#[must_use]
pub fn render_embed(n: &Notification) -> Embed {
    match n {
        Notification::WorkerStarted {
            worker_id,
            provider,
            goal,
        } => Embed {
            title: Some(format!("▶️  Worker started ({provider:?})")),
            description: Some(goal.clone()),
            color: Some(provider_color(*provider)),
            footer: None,
            fields: vec![EmbedField {
                name: "worker_id".to_string(),
                value: worker_id.0.to_string(),
                inline: true,
            }],
        },
        Notification::WorkerFinished { worker_id, outcome } => {
            // Pick a sidebar color that lets the operator triage from the
            // channel scroll without expanding the embed.
            let (color, outcome_label) = match outcome {
                WorkerStatus::Succeeded => (COLOR_SUCCESS, "succeeded".to_string()),
                WorkerStatus::Failed(reason) => (COLOR_ERROR, format!("failed: {reason}")),
                other => (COLOR_NEUTRAL, format!("{other:?}")),
            };
            Embed {
                title: Some("🏁  Worker finished".to_string()),
                description: Some(outcome_label),
                color: Some(color),
                footer: None,
                fields: vec![EmbedField {
                    name: "worker_id".to_string(),
                    value: worker_id.0.to_string(),
                    inline: true,
                }],
            }
        }
        Notification::SchedulerFiredJob {
            name,
            outcome_summary,
        } => Embed {
            // Plain message-ish but still embedded for visual consistency.
            title: Some(format!("⏰  Scheduler fired: {name}")),
            description: Some(outcome_summary.clone()),
            color: Some(COLOR_NEUTRAL),
            footer: None,
            fields: vec![],
        },
        Notification::AskPending { ask_id, question } => Embed {
            title: Some("❓  Ask pending".to_string()),
            description: Some(question.clone()),
            color: Some(COLOR_WARN),
            // Spec: "AskPending → message + /ask-reply <id> hint"
            footer: Some(format!(
                "Reply with: /{ASK_REPLY_COMMAND} {ASK_REPLY_OPT_ID}:{id} {ASK_REPLY_OPT_ANSWER}:<your answer>",
                id = ask_id.0
            )),
            fields: vec![EmbedField {
                name: ASK_REPLY_OPT_ID.to_string(),
                value: ask_id.0.to_string(),
                inline: false,
            }],
        },
        Notification::AskResolved { ask_id, answer } => Embed {
            title: Some("✅  Ask resolved".to_string()),
            description: Some(answer.clone()),
            color: Some(COLOR_SUCCESS),
            footer: None,
            fields: vec![EmbedField {
                name: ASK_REPLY_OPT_ID.to_string(),
                value: ask_id.0.to_string(),
                inline: false,
            }],
        },
        Notification::Error { context, message } => Embed {
            title: Some(format!("⚠️  Error: {context}")),
            description: Some(message.clone()),
            color: Some(COLOR_ERROR),
            footer: None,
            fields: vec![],
        },
    }
}

// ─── Bridge ──────────────────────────────────────────────────────────────

/// Cheaply-`Clone`-able handle. Spawn `bridge.clone().run(token)` and
/// keep the original to call `notify` / `ask` from the daemon.
///
/// Same shape as [`crate::telegram::TelegramBridge`] so the daemon can
/// pick one or the other at config-load time without branching the call
/// sites.
#[derive(Clone)]
pub struct DiscordBridge {
    inner: Arc<Inner>,
}

struct Inner {
    config: DiscordConfig,
    /// Standalone serenity `Http` so [`DiscordBridge::notify`] /
    /// [`DiscordBridge::ask`] can post messages WITHOUT requiring the
    /// gateway loop (`run`) to be alive. Useful for one-shot daemon
    /// notifications and for tests that exercise the bridge without
    /// spinning a WebSocket.
    http: Arc<Http>,
    asks: Arc<AskRegistry>,
    /// `MessageId.get()` of an outbound ask → `AskId` so the slash
    /// command handler can disambiguate when an operator answers an
    /// old ask after a newer one was posted. (Not strictly required —
    /// the slash command carries the `ask_id` directly — but kept for
    /// parity with the Telegram bridge's `open_asks` and to power a
    /// future "list open asks" affordance in the operator console.)
    open_asks: Mutex<HashMap<u64, AskId>>,
}

impl DiscordBridge {
    /// Construct a bridge. `asks` is shared with the daemon so other
    /// surfaces (HTTP, TUI, Telegram) can see / answer pending asks too.
    ///
    /// # Panics
    /// Panics if `config.application_id` is `0`. Snowflakes are
    /// `NonZeroU64`; passing `0` is a config bug we want to surface
    /// loudly at boot, not silently degrade at first interaction.
    #[must_use]
    pub fn new(config: DiscordConfig, asks: Arc<AskRegistry>) -> Self {
        let http = Http::new(&config.bot_token);
        // Required so create_global_command knows the application id
        // without us having to wait for the gateway READY event.
        let app_id = ApplicationId::new(config.application_id);
        http.set_application_id(app_id);
        Self {
            inner: Arc::new(Inner {
                config,
                http: Arc::new(http),
                asks,
                open_asks: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Send a [`Notification`] as a rich embed in the configured channel.
    ///
    /// # Errors
    /// [`Error::Provider`] with `"discord:"`-prefixed reason on any
    /// REST / API failure.
    pub async fn notify(&self, notification: Notification) -> Result<()> {
        let embed = render_embed(&notification);
        self.send_embed(embed).await.map(|_| ())
    }

    /// Post a question as a rich embed and block (with timeout) until
    /// the operator answers via `/ask-reply`.
    ///
    /// Internally:
    /// 1. [`AskRegistry::open`] mints an [`AskId`].
    /// 2. Send the AskPending embed (footer carries the `/ask-reply`
    ///    invocation template).
    /// 3. Record the outbound `message_id → AskId` mapping so any
    ///    future affordance can correlate.
    /// 4. [`AskRegistry::wait_for`] parks until resolved or the
    ///    deadline elapses.
    ///
    /// # Errors
    /// - [`Error::Provider`] on Discord transport.
    /// - [`Error::WorkerFailed`] when the operator doesn't reply
    ///   before `timeout` elapses (see [`AskRegistry::wait_for`]).
    pub async fn ask(&self, question: String, timeout: Duration) -> Result<String> {
        let ask_id = self.inner.asks.open(question.clone()).await;
        let embed = render_embed(&Notification::AskPending {
            ask_id,
            question: question.clone(),
        });
        // Mirror Telegram's lock-around-send pattern so a fast operator
        // /ask-reply can't observe a registry entry without the mapping
        // being in place. The slash-command path doesn't strictly need
        // the mapping (it carries the ask_id), but keeping the pattern
        // symmetric across bridges shrinks the "how is this different"
        // surface for future maintainers.
        let mut open = self.inner.open_asks.lock().await;
        let message_id = self.send_embed(embed).await?;
        open.insert(message_id, ask_id);
        drop(open);
        self.inner.asks.wait_for(ask_id, timeout).await
    }

    /// Connect to the Discord gateway, register the `/ask-reply` global
    /// slash command, and dispatch operator-initiated interactions
    /// until `shutdown` is cancelled.
    ///
    /// Consumes the bridge handle; clone before spawning if you need
    /// the original alive for [`Self::notify`] / [`Self::ask`].
    ///
    /// # Errors
    /// Propagates a fatal transport error only when the gateway loop
    /// cannot usefully continue (auth failure, etc.). Transient
    /// reconnects are handled inside serenity.
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        // We deliberately ask for ZERO privileged intents. The
        // /ask-reply path is interaction-driven (gateway delivers
        // command interactions without MESSAGE_CONTENT) so we never
        // need to read raw message bodies. This also avoids the
        // verification dance Discord requires for bots in 100+ guilds.
        let intents = GatewayIntents::empty();

        let handler = SlashHandler {
            bridge: self.clone(),
        };

        // ClientBuilder creates its own internal Http; we keep
        // `self.inner.http` separate so outbound `notify` / `ask` can
        // run concurrently with — or entirely without — the gateway
        // loop. `application_id` is plumbed through the builder so the
        // gateway-side command-registration path doesn't have to await
        // a READY event to learn its own id.
        let mut client = Client::builder(&self.inner.config.bot_token, intents)
            .application_id(ApplicationId::new(self.inner.config.application_id))
            .event_handler(handler)
            .await
            .map_err(|e| Error::Provider {
                kind: ProviderKind::ClaudeCode,
                // TODO(team-lead): replace with Error::Comms once added to evy-core.
                reason: format!("discord: client build: {e}"),
            })?;

        info!("discord bridge: starting gateway loop");
        let shard_manager = client.shard_manager.clone();

        let outcome = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("discord bridge: shutdown signalled, closing gateway");
                shard_manager.shutdown_all().await;
                Ok(())
            }
            r = client.start() => {
                r.map_err(|e| Error::Provider {
                    kind: ProviderKind::ClaudeCode,
                    // TODO(team-lead): replace with Error::Comms once added to evy-core.
                    reason: format!("discord: gateway: {e}"),
                })
            }
        };

        outcome
    }

    // ─── internals ──────────────────────────────────────────────────

    /// Send a rendered [`Embed`] to the configured channel and return
    /// the outbound message id (as `u64` — the snowflake unwrapped).
    async fn send_embed(&self, embed: Embed) -> Result<u64> {
        let channel = ChannelId::new(self.inner.config.channel_id);
        let msg = CreateMessage::new().embed(embed.into_create_embed());
        let posted = channel
            .send_message(self.inner.http.as_ref(), msg)
            .await
            .map_err(|e| Error::Provider {
                kind: ProviderKind::ClaudeCode,
                // TODO(team-lead): replace with Error::Comms once added to evy-core.
                // Serenity's Display impl scrubs bot tokens; we don't
                // need a separate `.without_url()`-style guard here.
                reason: format!("discord: send_message: {e}"),
            })?;
        Ok(posted.id.get())
    }

    /// Resolve a pending ask from a parsed slash-command invocation.
    ///
    /// Public on the crate so the gateway [`EventHandler`] dispatches
    /// into it; also the surface the unit tests exercise directly so we
    /// never need to spin up a real gateway in CI.
    ///
    /// # Errors
    /// - [`Error::InvalidMandate`] on malformed UUID or unknown ask id
    ///   (the registry returns `InvalidMandate` for unknown ids; we
    ///   pre-validate the UUID format with the same variant for
    ///   consistency).
    pub(crate) async fn resolve_from_slash(&self, ask_id_str: &str, answer: String) -> Result<()> {
        let uuid = Uuid::from_str(ask_id_str.trim())
            .map_err(|e| Error::InvalidMandate(format!("ask id not a uuid: {e}")))?;
        let ask_id = AskId(uuid);
        self.inner.asks.resolve(ask_id, answer.clone()).await?;
        // Cleanup the open_asks map opportunistically — the entry's
        // existence is informational only (the ask_id came in on the
        // slash command), so we just drop whatever key happens to map
        // to this ask. O(n) over open_asks but n is small (operator
        // is one human, asks expire on timeout).
        let mut open = self.inner.open_asks.lock().await;
        open.retain(|_msg_id, registered| *registered != ask_id);
        Ok(())
    }
}

// ─── Gateway event handler ───────────────────────────────────────────────

/// serenity glue. Holds an `Arc`-backed clone of the bridge so the
/// gateway-driven callbacks can resolve pending asks against the same
/// shared [`AskRegistry`] the daemon hands around.
///
/// Intentionally small: every branch delegates to a pure method on
/// [`DiscordBridge`] so the test suite can exercise the resolution
/// logic without standing up a WebSocket.
struct SlashHandler {
    bridge: DiscordBridge,
}

#[serenity::async_trait]
impl EventHandler for SlashHandler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(
            bot = %ready.user.name,
            "discord bridge: gateway ready; registering /{ASK_REPLY_COMMAND}",
        );
        let cmd = build_ask_reply_command();
        match serenity::all::Command::create_global_command(&ctx.http, cmd).await {
            Ok(c) => debug!(command_id = %c.id, "discord bridge: /ask-reply registered"),
            Err(e) => error!(error = %e, "discord bridge: failed to register /ask-reply"),
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Some(cmd) = interaction.command() else {
            // Component / autocomplete / modal — not our concern.
            return;
        };

        // Hard channel gate: reject interactions invoked outside the
        // configured channel. The slash command is registered globally,
        // so a server admin who installs the bot in another channel
        // would otherwise be able to resolve asks. Drop with a private
        // (`ephemeral`) reply explaining why.
        let configured = self.bridge.inner.config.channel_id;
        if cmd.channel_id.get() != configured {
            warn!(
                invoked_in = cmd.channel_id.get(),
                configured, "discord bridge: rejecting /ask-reply from non-authorized channel"
            );
            let _ = cmd
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("This Evy bot is bound to a different channel.")
                            .ephemeral(true),
                    ),
                )
                .await;
            return;
        }

        if cmd.data.name != ASK_REPLY_COMMAND {
            debug!(name = %cmd.data.name, "discord bridge: ignoring unknown command");
            return;
        }

        // Pull the two String options out of the interaction payload.
        let mut ask_id_opt: Option<String> = None;
        let mut answer_opt: Option<String> = None;
        for opt in &cmd.data.options {
            match (&opt.name[..], &opt.value) {
                (ASK_REPLY_OPT_ID, CommandDataOptionValue::String(s)) => {
                    ask_id_opt = Some(s.clone());
                }
                (ASK_REPLY_OPT_ANSWER, CommandDataOptionValue::String(s)) => {
                    answer_opt = Some(s.clone());
                }
                _ => {}
            }
        }

        let (Some(ask_id_str), Some(answer)) = (ask_id_opt, answer_opt) else {
            warn!("discord bridge: /ask-reply missing required option");
            let _ = cmd
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content(
                                "Usage: `/ask-reply ask_id:<uuid> answer:<text>` — both options are required.",
                            )
                            .ephemeral(true),
                    ),
                )
                .await;
            return;
        };

        match self
            .bridge
            .resolve_from_slash(&ask_id_str, answer.clone())
            .await
        {
            Ok(()) => {
                info!(ask_id = %ask_id_str, "discord bridge: ask resolved via slash command");
                let _ = cmd
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(format!("Resolved ask `{ask_id_str}` with: {answer}"))
                                .ephemeral(false),
                        ),
                    )
                    .await;
            }
            Err(e) => {
                warn!(error = %e, ask_id = %ask_id_str, "discord bridge: failed to resolve ask");
                let _ = cmd
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(format!("Could not resolve ask: {e}"))
                                .ephemeral(true),
                        ),
                    )
                    .await;
            }
        }
    }
}

/// Build the `CreateCommand` payload for global registration. Extracted
/// so a future "re-register on schema change" cron can call it directly.
fn build_ask_reply_command() -> CreateCommand {
    CreateCommand::new(ASK_REPLY_COMMAND)
        .kind(CommandType::ChatInput)
        .description("Reply to a pending Evy ask.")
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::String,
                ASK_REPLY_OPT_ID,
                "The ask UUID (copy from the embed footer).",
            )
            .required(true),
        )
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::String,
                ASK_REPLY_OPT_ANSWER,
                "Your answer to the ask.",
            )
            .required(true),
        )
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use evy_core::WorkerId;

    fn worker_id() -> WorkerId {
        // `Uuid::nil()` keeps the golden fixtures byte-stable across runs.
        WorkerId(Uuid::nil())
    }

    fn ask_id() -> AskId {
        AskId(Uuid::nil())
    }

    // Golden fixtures — JSON snapshot of `render_embed()` for each
    // Notification variant. If you change the rendering, update the
    // fixture intentionally; do NOT make the test compare to the new
    // shape automatically. The whole point is to make the wire format
    // a reviewable artifact.

    fn assert_embed_json(n: &Notification, expected: &str) {
        let embed = render_embed(n);
        let actual = serde_json::to_string_pretty(&embed).expect("embed serialize");
        let expected = expected.trim();
        let actual = actual.trim();
        assert_eq!(
            actual, expected,
            "embed JSON drifted for {n:?}:\n--- actual ---\n{actual}\n--- expected ---\n{expected}",
        );
    }

    #[test]
    fn embed_fixture_worker_started_claude() {
        assert_embed_json(
            &Notification::WorkerStarted {
                worker_id: worker_id(),
                provider: ProviderKind::ClaudeCode,
                goal: "ship slice 3B".into(),
            },
            r#"{
  "title": "▶️  Worker started (ClaudeCode)",
  "description": "ship slice 3B",
  "color": 13400156,
  "fields": [
    {
      "name": "worker_id",
      "value": "00000000-0000-0000-0000-000000000000",
      "inline": true
    }
  ]
}"#,
        );
    }

    #[test]
    fn embed_fixture_worker_started_codex_color() {
        // The fixture for ClaudeCode pins the format; this one pins the
        // per-provider color branch so we catch a palette regression.
        let embed = render_embed(&Notification::WorkerStarted {
            worker_id: worker_id(),
            provider: ProviderKind::Codex,
            goal: "g".into(),
        });
        assert_eq!(embed.color, Some(COLOR_CODEX));
    }

    #[test]
    fn embed_fixture_worker_started_deepseek_color() {
        let embed = render_embed(&Notification::WorkerStarted {
            worker_id: worker_id(),
            provider: ProviderKind::DeepSeek,
            goal: "g".into(),
        });
        assert_eq!(embed.color, Some(COLOR_DEEPSEEK));
    }

    #[test]
    fn embed_fixture_worker_finished_succeeded() {
        assert_embed_json(
            &Notification::WorkerFinished {
                worker_id: worker_id(),
                outcome: WorkerStatus::Succeeded,
            },
            r#"{
  "title": "🏁  Worker finished",
  "description": "succeeded",
  "color": 5763719,
  "fields": [
    {
      "name": "worker_id",
      "value": "00000000-0000-0000-0000-000000000000",
      "inline": true
    }
  ]
}"#,
        );
    }

    #[test]
    fn embed_fixture_worker_finished_failed_carries_reason() {
        assert_embed_json(
            &Notification::WorkerFinished {
                worker_id: worker_id(),
                outcome: WorkerStatus::Failed("oom".into()),
            },
            r#"{
  "title": "🏁  Worker finished",
  "description": "failed: oom",
  "color": 15548997,
  "fields": [
    {
      "name": "worker_id",
      "value": "00000000-0000-0000-0000-000000000000",
      "inline": true
    }
  ]
}"#,
        );
    }

    #[test]
    fn embed_fixture_scheduler_fired_job() {
        assert_embed_json(
            &Notification::SchedulerFiredJob {
                name: "nightly-sweep".into(),
                outcome_summary: "ok (12 workers)".into(),
            },
            r#"{
  "title": "⏰  Scheduler fired: nightly-sweep",
  "description": "ok (12 workers)",
  "color": 5793266
}"#,
        );
    }

    #[test]
    fn embed_fixture_ask_pending_has_slash_hint() {
        let embed = render_embed(&Notification::AskPending {
            ask_id: ask_id(),
            question: "continue?".into(),
        });
        // The footer is the operator's UI hint — assert it explicitly.
        let footer = embed.footer.expect("ask pending must carry footer");
        assert!(
            footer.contains(&format!("/{ASK_REPLY_COMMAND}")),
            "footer should hint the slash command: {footer}"
        );
        assert!(
            footer.contains(&ask_id().0.to_string()),
            "footer should embed the concrete ask id: {footer}"
        );
        assert_eq!(embed.color, Some(COLOR_WARN));
    }

    #[test]
    fn embed_fixture_ask_resolved() {
        let embed = render_embed(&Notification::AskResolved {
            ask_id: ask_id(),
            answer: "yes".into(),
        });
        assert_eq!(embed.title.as_deref(), Some("✅  Ask resolved"));
        assert_eq!(embed.description.as_deref(), Some("yes"));
        assert_eq!(embed.color, Some(COLOR_SUCCESS));
    }

    #[test]
    fn embed_fixture_error_uses_red() {
        assert_embed_json(
            &Notification::Error {
                context: "providers".into(),
                message: "401 from anthropic".into(),
            },
            r#"{
  "title": "⚠️  Error: providers",
  "description": "401 from anthropic",
  "color": 15548997
}"#,
        );
    }

    #[test]
    fn embed_serde_roundtrip() {
        let original = render_embed(&Notification::AskPending {
            ask_id: ask_id(),
            question: "continue?".into(),
        });
        let json = serde_json::to_string(&original).expect("serialize");
        let back: Embed = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, back);
    }

    // ─── slash-command resolution logic (no gateway required) ────────

    fn bridge_with_registry(asks: Arc<AskRegistry>) -> DiscordBridge {
        // Snowflakes must be NonZeroU64; small synthetic values are fine.
        let config = DiscordConfig::new("test-bot-token".into(), 1, 2);
        DiscordBridge::new(config, asks)
    }

    #[tokio::test]
    async fn resolve_from_slash_resolves_open_ask() {
        let registry = Arc::new(AskRegistry::new());
        let id = registry.open("continue?".into()).await;
        let bridge = bridge_with_registry(registry.clone());

        bridge
            .resolve_from_slash(&id.0.to_string(), "yes".into())
            .await
            .expect("resolve");

        let all = registry.all().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].answer.as_deref(), Some("yes"));
        assert!(all[0].answered_at.is_some());
    }

    #[tokio::test]
    async fn resolve_from_slash_with_whitespace_around_id_succeeds() {
        // Operators copy/paste; tolerate stray surrounding whitespace
        // (but trust serde / the slash UI to clean the inner shape).
        let registry = Arc::new(AskRegistry::new());
        let id = registry.open("q?".into()).await;
        let bridge = bridge_with_registry(registry.clone());

        let padded = format!("  {}  ", id.0);
        bridge
            .resolve_from_slash(&padded, "answer".into())
            .await
            .expect("resolve with padded id");

        let all = registry.all().await;
        assert_eq!(all[0].answer.as_deref(), Some("answer"));
    }

    #[tokio::test]
    async fn resolve_from_slash_rejects_invalid_uuid() {
        let registry = Arc::new(AskRegistry::new());
        let bridge = bridge_with_registry(registry);

        let err = bridge
            .resolve_from_slash("not-a-uuid", "yes".into())
            .await
            .expect_err("must reject malformed uuid");
        match err {
            Error::InvalidMandate(msg) => assert!(
                msg.contains("uuid"),
                "error should call out the uuid problem: {msg}"
            ),
            other => panic!("expected InvalidMandate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_from_slash_unknown_id_errors() {
        let registry = Arc::new(AskRegistry::new());
        let bridge = bridge_with_registry(registry);
        let unknown = Uuid::new_v4();

        let err = bridge
            .resolve_from_slash(&unknown.to_string(), "yes".into())
            .await
            .expect_err("unknown id must error");
        // The registry returns InvalidMandate for unknown ids; surface
        // that verbatim so operator gets a clear diagnostic.
        assert!(matches!(err, Error::InvalidMandate(_)));
    }

    #[tokio::test]
    async fn resolve_from_slash_clears_open_asks_entry() {
        let registry = Arc::new(AskRegistry::new());
        let id = registry.open("q?".into()).await;
        let bridge = bridge_with_registry(registry);
        // Seed open_asks as if `ask()` had posted message id 42.
        bridge.inner.open_asks.lock().await.insert(42, id);

        bridge
            .resolve_from_slash(&id.0.to_string(), "ok".into())
            .await
            .expect("resolve");

        assert!(
            bridge.inner.open_asks.lock().await.is_empty(),
            "open_asks should be cleared after resolution"
        );
    }

    // ─── command-registration shape (offline, no API call) ───────────

    #[test]
    fn build_ask_reply_command_has_required_options() {
        // We can't introspect CreateCommand fields directly (private),
        // but we can serialize it (it's `Serialize`) and assert the
        // shape via JSON. This guards against an accidental "switched
        // off the `required: true` flag" regression.
        let cmd = build_ask_reply_command();
        let json = serde_json::to_value(&cmd).expect("serialize CreateCommand");
        assert_eq!(json["name"], serde_json::json!(ASK_REPLY_COMMAND));
        let opts = json["options"].as_array().expect("options array");
        assert_eq!(opts.len(), 2, "should register exactly two options");
        for opt in opts {
            assert_eq!(
                opt["required"],
                serde_json::Value::Bool(true),
                "every /ask-reply option must be required: {opt}"
            );
            // CommandOptionType::String discriminant is 3 in Discord's
            // wire schema; we hardcode it here so a refactor that
            // accidentally swaps the option kind (Integer, etc.) trips.
            assert_eq!(
                opt["type"],
                serde_json::json!(3),
                "option should be String (type=3): {opt}"
            );
        }
    }

    #[test]
    fn discord_config_round_trip_through_bridge_new() {
        // `DiscordBridge::new` panics if application_id is 0; this just
        // confirms a normal config path completes without panicking and
        // we can clone the resulting bridge handle.
        let asks = Arc::new(AskRegistry::new());
        let bridge = DiscordBridge::new(DiscordConfig::new("token".into(), 100, 200), asks.clone());
        let _cloned = bridge.clone();
        // No assertion on bot_token / channel_id reachability — those
        // are private. The shape compiles and clones, which is the
        // public contract.
    }
}
