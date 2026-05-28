//! Keyboard input handling — translates crossterm key events into
//! [`KeyOutcome`] verbs the run loop dispatches on.
//!
//! Bindings (mirrors Hermes's CLI where reasonable, vim-flavored):
//!
//! | Keys           | Action                                          |
//! |----------------|-------------------------------------------------|
//! | `Enter`        | Submit current draft (unless prefixed `/...`)   |
//! | `Alt+Enter`    | Newline (so multi-line drafts compose cleanly)  |
//! | `Ctrl+J`       | Newline (terminal fallback when Alt-Enter is intercepted) |
//! | `Backspace`    | Delete previous char                            |
//! | `Esc` / `Ctrl+C` | Quit                                          |
//! | `Ctrl+L`       | Clear scrollback (alias for `/clear`)           |
//! | `PageUp` / `PageDown` | Scroll history                           |
//!
//! Slash commands (only when the draft begins with `/`):
//!
//! | Command          | Effect                                       |
//! |------------------|----------------------------------------------|
//! | `/quit`, `/q`    | Quit                                         |
//! | `/help`, `/?`    | Push help system line                        |
//! | `/clear`         | Reset scrollback + session id                |
//! | `/new-session`   | Same as `/clear` (Hermes naming)             |

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, ChatLine, LineKind, Status};

/// Verb produced by [`handle_key`] for the run loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// State was mutated; nothing to do.
    Handled,
    /// Operator wants to exit.
    Quit,
    /// Operator submitted a non-empty turn (slash-stripped already
    /// drained); the run loop should POST the message and push
    /// operator-echo + waiting status.
    Submit {
        /// The turn text — already stripped of leading/trailing
        /// whitespace and confirmed non-empty.
        message: String,
    },
}

/// Recognised slash commands. Kept as an enum so tests stay tight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    /// `/quit` / `/q`
    Quit,
    /// `/help` / `/?`
    Help,
    /// `/clear` — drop scrollback + session id
    Clear,
    /// `/new-session` — alias for `/clear`
    NewSession,
    /// Unknown — surfaces an error system line.
    Unknown,
}

impl SlashCommand {
    /// Parse a draft (already known to start with `/`). Trailing args
    /// are ignored this slice.
    #[must_use]
    pub fn parse(draft: &str) -> Self {
        let head = draft.split_whitespace().next().unwrap_or("");
        match head {
            "/quit" | "/q" | "/exit" => Self::Quit,
            "/help" | "/?" | "/h" => Self::Help,
            "/clear" => Self::Clear,
            "/new-session" | "/new" => Self::NewSession,
            _ => Self::Unknown,
        }
    }
}

/// Dispatch one key event against the app.
///
/// The terminal raw-mode flag is set by the run loop; key codes are
/// pre-decoded by crossterm. Returns the [`KeyOutcome`] verb for the
/// run loop — callers that don't care about the outcome (e.g. tests
/// driving state mutations) may safely discard the return.
pub fn handle_key(app: &mut App, key: KeyEvent) -> KeyOutcome {
    // Ctrl-C and Esc are always quit.
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    if (ctrl && matches!(key.code, KeyCode::Char('c'))) || matches!(key.code, KeyCode::Esc) {
        app.should_quit = true;
        return KeyOutcome::Quit;
    }
    // Ctrl-L clears scrollback.
    if ctrl && matches!(key.code, KeyCode::Char('l')) {
        app.clear();
        app.push_system("scrollback cleared (Ctrl-L)");
        return KeyOutcome::Handled;
    }

    match key.code {
        KeyCode::Char(c) => {
            // Alt-Enter / Ctrl-J handled by Enter case below; only
            // plain printable chars get appended here.
            if !ctrl {
                app.input.push(c);
            } else if c == 'j' {
                // Ctrl-J as newline fallback (some terms intercept Alt).
                app.input.push('\n');
            }
            KeyOutcome::Handled
        }
        KeyCode::Backspace => {
            app.input.pop();
            KeyOutcome::Handled
        }
        KeyCode::Enter => {
            if alt {
                app.input.push('\n');
                return KeyOutcome::Handled;
            }
            // Submit. Empty submits are no-ops.
            let draft = app.input.trim().to_string();
            if draft.is_empty() {
                return KeyOutcome::Handled;
            }
            // Slash command? Don't fire the HTTP path.
            if draft.starts_with('/') {
                let cmd = SlashCommand::parse(&draft);
                app.input.clear();
                return apply_slash(app, cmd, &draft);
            }
            // Normal turn. Echo + clear + signal Submit.
            app.input.clear();
            app.push_operator(draft.clone());
            app.status = Status::Sending;
            KeyOutcome::Submit { message: draft }
        }
        KeyCode::PageUp => {
            app.scroll_offset = app.scroll_offset.saturating_add(5);
            KeyOutcome::Handled
        }
        KeyCode::PageDown => {
            app.scroll_offset = app.scroll_offset.saturating_sub(5);
            KeyOutcome::Handled
        }
        _ => KeyOutcome::Handled,
    }
}

fn apply_slash(app: &mut App, cmd: SlashCommand, raw: &str) -> KeyOutcome {
    match cmd {
        SlashCommand::Quit => {
            app.should_quit = true;
            KeyOutcome::Quit
        }
        SlashCommand::Help => {
            app.push_line(ChatLine::new(LineKind::System, HELP_TEXT.to_string()));
            KeyOutcome::Handled
        }
        SlashCommand::Clear | SlashCommand::NewSession => {
            app.clear();
            app.push_system("session reset — next message opens a new conversation");
            KeyOutcome::Handled
        }
        SlashCommand::Unknown => {
            app.push_error(format!("unknown command: {raw} — type /help for the list"));
            KeyOutcome::Handled
        }
    }
}

const HELP_TEXT: &str = "Commands:
  /quit, /q          exit
  /help, /?          this list
  /clear             reset scrollback + open a new session
  /new-session       alias for /clear
Keybindings:
  Enter              submit
  Alt-Enter, Ctrl-J  newline within draft
  Ctrl-C, Esc        exit
  Ctrl-L             clear scrollback
  PageUp / PageDown  scroll history";

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn k_ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn k_alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    #[test]
    fn parse_known_commands() {
        assert_eq!(SlashCommand::parse("/quit"), SlashCommand::Quit);
        assert_eq!(SlashCommand::parse("/q"), SlashCommand::Quit);
        assert_eq!(SlashCommand::parse("/exit"), SlashCommand::Quit);
        assert_eq!(SlashCommand::parse("/help"), SlashCommand::Help);
        assert_eq!(SlashCommand::parse("/?"), SlashCommand::Help);
        assert_eq!(SlashCommand::parse("/clear"), SlashCommand::Clear);
        assert_eq!(
            SlashCommand::parse("/new-session"),
            SlashCommand::NewSession
        );
        assert_eq!(SlashCommand::parse("/wat"), SlashCommand::Unknown);
    }

    #[test]
    fn typing_chars_appends_to_input() {
        let mut app = App::new();
        handle_key(&mut app, k(KeyCode::Char('h')));
        handle_key(&mut app, k(KeyCode::Char('i')));
        assert_eq!(app.input, "hi");
    }

    #[test]
    fn enter_with_empty_draft_is_noop() {
        let mut app = App::new();
        let out = handle_key(&mut app, k(KeyCode::Enter));
        assert_eq!(out, KeyOutcome::Handled);
        assert!(app.lines.is_empty());
    }

    #[test]
    fn enter_submits_non_empty_draft() {
        let mut app = App::new();
        app.input = "hello".into();
        let out = handle_key(&mut app, k(KeyCode::Enter));
        match out {
            KeyOutcome::Submit { message } => assert_eq!(message, "hello"),
            other => panic!("expected Submit, got {other:?}"),
        }
        assert!(app.input.is_empty());
        assert_eq!(app.lines.len(), 1);
        assert_eq!(app.lines[0].kind, LineKind::Operator);
        assert_eq!(app.status, Status::Sending);
    }

    #[test]
    fn alt_enter_inserts_newline() {
        let mut app = App::new();
        app.input = "line1".into();
        handle_key(&mut app, k_alt(KeyCode::Enter));
        assert_eq!(app.input, "line1\n");
    }

    #[test]
    fn ctrl_j_inserts_newline() {
        let mut app = App::new();
        app.input = "line1".into();
        handle_key(&mut app, k_ctrl('j'));
        assert_eq!(app.input, "line1\n");
    }

    #[test]
    fn ctrl_c_quits() {
        let mut app = App::new();
        let out = handle_key(&mut app, k_ctrl('c'));
        assert_eq!(out, KeyOutcome::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn esc_quits() {
        let mut app = App::new();
        let out = handle_key(&mut app, k(KeyCode::Esc));
        assert_eq!(out, KeyOutcome::Quit);
    }

    #[test]
    fn slash_quit_quits() {
        let mut app = App::new();
        app.input = "/quit".into();
        let out = handle_key(&mut app, k(KeyCode::Enter));
        assert_eq!(out, KeyOutcome::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn slash_help_pushes_help_text() {
        let mut app = App::new();
        app.input = "/help".into();
        handle_key(&mut app, k(KeyCode::Enter));
        assert_eq!(app.lines.len(), 1);
        assert!(app.lines[0].text.contains("/quit"));
    }

    #[test]
    fn slash_clear_drops_scrollback() {
        let mut app = App::new();
        app.session_id = Some(uuid::Uuid::nil());
        app.push_operator("old");
        app.input = "/clear".into();
        handle_key(&mut app, k(KeyCode::Enter));
        // After clear: scrollback emptied, then system line pushed.
        assert_eq!(app.lines.len(), 1);
        assert!(app.lines[0].text.contains("reset"));
        assert!(app.session_id.is_none());
    }

    #[test]
    fn slash_unknown_pushes_error() {
        let mut app = App::new();
        app.input = "/bogus".into();
        handle_key(&mut app, k(KeyCode::Enter));
        assert_eq!(app.lines.len(), 1);
        assert_eq!(app.lines[0].kind, LineKind::Error);
    }

    #[test]
    fn ctrl_l_clears_scrollback() {
        let mut app = App::new();
        app.push_operator("a");
        app.push_operator("b");
        handle_key(&mut app, k_ctrl('l'));
        assert_eq!(app.lines.len(), 1);
        assert!(app.lines[0].text.contains("cleared"));
    }

    #[test]
    fn page_up_increments_scroll_offset() {
        let mut app = App::new();
        handle_key(&mut app, k(KeyCode::PageUp));
        assert_eq!(app.scroll_offset, 5);
        handle_key(&mut app, k(KeyCode::PageDown));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut app = App::new();
        app.input = "hello".into();
        handle_key(&mut app, k(KeyCode::Backspace));
        assert_eq!(app.input, "hell");
    }
}
