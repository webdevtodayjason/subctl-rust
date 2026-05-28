// Static constants shared across modules. No runtime imports — safe to use
// from entry.tsx before Ink mounts.

export const VERSION = '0.1.0'

/// Default daemon endpoint. The v4 plist binds 127.0.0.1:8797 (see
/// `config.toml`); override with `EVY_HTTP_URL` for dev runs against a
/// different port.
export const DEFAULT_DAEMON_URL =
  process.env.EVY_HTTP_URL ?? 'http://127.0.0.1:8797'

/// LaunchAgent label installed by the v4 plist. Used by the daemon
/// autostart helper to decide whether to `launchctl load`.
export const DAEMON_LABEL = 'com.subctl.evy-v4'

/// Path to the plist; relative to the operator's home so it works on
/// both dev installs (under the worktree) and production installs (the
/// installer drops a copy into ~/Library/LaunchAgents).
export const DAEMON_PLIST_PATH = `${
  process.env.HOME ?? ''
}/Library/LaunchAgents/${DAEMON_LABEL}.plist`

/// User-facing config file. `/backend` slash command rewrites a single key
/// in the `[thinking_partner]` table here.
export const CONFIG_TOML_PATH = `${
  process.env.HOME ?? ''
}/.config/subctl/v4/config.toml`

export const USAGE = `subctl — chat with the v4 Evy daemon

Usage:
  subctl                 launch the chat TUI (default)
  subctl chat            explicit alias
  subctl --help          show this help
  subctl --version       print version

Inside the chat surface, type /help for slash commands (e.g. /quit, /clear,
/sessions, /skills, /status, /backend lm-studio|codex|anthropic).

Connects to ${DEFAULT_DAEMON_URL} by default. Auto-starts the daemon via
launchctl when it isn't already loaded (label: ${DAEMON_LABEL}).
`
