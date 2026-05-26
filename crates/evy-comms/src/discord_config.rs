//! Static configuration for [`crate::DiscordBridge`].
//!
//! Lives in its own module (parallel to [`crate::config::HttpConfig`]
//! and [`crate::telegram::TelegramConfig`]) so the daemon binary can
//! `use evy_comms::DiscordConfig;` without pulling in the bridge type
//! and the serenity transitive deps at type-name resolution time.
//!
//! Discord-vs-Telegram shape differences worth noting:
//!
//! - `channel_id` (Discord snowflake, `u64`) replaces Telegram's
//!   `chat_id` (`i64`). Snowflakes are unsigned; preserve that.
//! - `application_id` (also a snowflake) is required for registering
//!   the global `/ask-reply` slash command. The Telegram bridge needs
//!   no analogous field because Telegram has no command-registration
//!   step.
//! - No `inbound` mpsc handle — per spec, Discord's only inbound path
//!   is the `/ask-reply` slash command, which is registry-resolution,
//!   not general-purpose message dispatch.
//! - No HTTP-timeout knob — serenity 0.12's `Http` manages its own
//!   reqwest client with hardcoded timeout policy, so exposing a
//!   timeout field on this config would be dead weight until we move
//!   to a custom `HttpBuilder`. Add it back at that point.

/// Static configuration for the Discord bridge.
///
/// All three numeric fields are Discord snowflakes (`u64`); the bot
/// token is the OAuth bot secret. Operators source the snowflakes
/// from the developer-portal UI or the right-click context menu in a
/// Discord client with developer mode enabled.
#[derive(Debug, Clone)]
pub struct DiscordConfig {
    /// Bot API token. From env `DISCORD_BOT_TOKEN`; never logged.
    pub bot_token: String,

    /// Authorized channel id. Outbound messages target this channel;
    /// inbound slash-command interactions from any other channel are
    /// dropped at the handler. From env `DISCORD_CHANNEL_ID`.
    pub channel_id: u64,

    /// The bot's application id, required for registering global
    /// slash commands via the REST API. From env
    /// `DISCORD_APPLICATION_ID`.
    pub application_id: u64,
}

impl DiscordConfig {
    /// Construct a config from its three identifying fields. No
    /// optional timing knobs today — see module docs for why.
    #[must_use]
    pub fn new(bot_token: String, channel_id: u64, application_id: u64) -> Self {
        Self {
            bot_token,
            channel_id,
            application_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_new_records_fields_verbatim() {
        let c = DiscordConfig::new("tok".into(), 42, 1337);
        assert_eq!(c.bot_token, "tok");
        assert_eq!(c.channel_id, 42);
        assert_eq!(c.application_id, 1337);
    }
}
