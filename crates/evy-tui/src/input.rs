//! Keyboard input → [`App`] state transitions.
//!
//! Centralized here so the run loop can stay focused on event
//! multiplexing and the [`App`] tests can drive transitions through
//! the same code path the runtime uses.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::App;

/// What the run loop should do after handling a key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOutcome {
    /// No follow-up needed beyond the App state mutation already
    /// applied.
    Handled,
    /// The operator pressed `r`; the run loop should trigger a
    /// snapshot refresh.
    Refresh,
    /// The operator pressed `q` / `Ctrl-C`; the run loop should exit.
    Quit,
}

/// Apply a key event to `app` and report the follow-up action.
///
/// Key bindings:
///
/// | Key            | Action                        |
/// |----------------|-------------------------------|
/// | `Tab`          | Cycle to next tab             |
/// | `Shift-Tab`    | Cycle to previous tab         |
/// | `↑` / `k`      | Move selection up             |
/// | `↓` / `j`      | Move selection down           |
/// | `r`            | Trigger snapshot refresh      |
/// | `q` / `Ctrl-C` | Quit                          |
pub fn handle_key(app: &mut App, ev: KeyEvent) -> KeyOutcome {
    // crossterm's `event-stream` may emit `Release` events on
    // platforms / terminals that report them; we only act on
    // `Press` so a key held down doesn't double-fire.
    if ev.kind != KeyEventKind::Press {
        return KeyOutcome::Handled;
    }

    // Ctrl-C always quits, regardless of which character.
    if ev.modifiers.contains(KeyModifiers::CONTROL) && matches!(ev.code, KeyCode::Char('c')) {
        app.request_quit();
        return KeyOutcome::Quit;
    }

    match ev.code {
        KeyCode::Tab => {
            if ev.modifiers.contains(KeyModifiers::SHIFT) {
                app.cycle_tab_backward();
            } else {
                app.cycle_tab_forward();
            }
            KeyOutcome::Handled
        }
        KeyCode::BackTab => {
            app.cycle_tab_backward();
            KeyOutcome::Handled
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.select_prev();
            KeyOutcome::Handled
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.select_next();
            KeyOutcome::Handled
        }
        KeyCode::Char('r') => KeyOutcome::Refresh,
        KeyCode::Char('q') | KeyCode::Esc => {
            app.request_quit();
            KeyOutcome::Quit
        }
        _ => KeyOutcome::Handled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Tab;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press_with(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn tab_advances_forward() {
        let mut app = App::new();
        let out = handle_key(&mut app, press(KeyCode::Tab));
        assert_eq!(out, KeyOutcome::Handled);
        assert_eq!(app.tab, Tab::Scheduler);
    }

    #[test]
    fn shift_tab_goes_backward() {
        let mut app = App::new();
        let out = handle_key(&mut app, press_with(KeyCode::Tab, KeyModifiers::SHIFT));
        assert_eq!(out, KeyOutcome::Handled);
        assert_eq!(app.tab, Tab::Policy);
    }

    #[test]
    fn back_tab_goes_backward() {
        let mut app = App::new();
        let out = handle_key(&mut app, press(KeyCode::BackTab));
        assert_eq!(out, KeyOutcome::Handled);
        assert_eq!(app.tab, Tab::Policy);
    }

    #[test]
    fn q_quits() {
        let mut app = App::new();
        let out = handle_key(&mut app, press(KeyCode::Char('q')));
        assert_eq!(out, KeyOutcome::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_quits() {
        let mut app = App::new();
        let out = handle_key(
            &mut app,
            press_with(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert_eq!(out, KeyOutcome::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn r_requests_refresh() {
        let mut app = App::new();
        let out = handle_key(&mut app, press(KeyCode::Char('r')));
        assert_eq!(out, KeyOutcome::Refresh);
        assert!(!app.should_quit);
    }

    #[test]
    fn arrow_keys_move_selection() {
        let mut app = App::new();
        // Populate so the cursor has somewhere to go.
        app.workers = vec![
            crate::api::WorkerSummary {
                id: evy_core::WorkerId::new(),
                provider: evy_core::ProviderKind::ClaudeCode,
                mandate_id: evy_core::MandateId::new(),
                status: evy_core::WorkerStatus::Running,
            };
            3
        ];
        handle_key(&mut app, press(KeyCode::Down));
        assert_eq!(app.workers_cursor, 1);
        handle_key(&mut app, press(KeyCode::Up));
        assert_eq!(app.workers_cursor, 0);
    }

    #[test]
    fn j_k_are_vim_aliases() {
        let mut app = App::new();
        app.workers = vec![
            crate::api::WorkerSummary {
                id: evy_core::WorkerId::new(),
                provider: evy_core::ProviderKind::ClaudeCode,
                mandate_id: evy_core::MandateId::new(),
                status: evy_core::WorkerStatus::Running,
            };
            2
        ];
        handle_key(&mut app, press(KeyCode::Char('j')));
        assert_eq!(app.workers_cursor, 1);
        handle_key(&mut app, press(KeyCode::Char('k')));
        assert_eq!(app.workers_cursor, 0);
    }

    #[test]
    fn key_release_events_ignored() {
        let mut app = App::new();
        let mut ev = press(KeyCode::Char('q'));
        ev.kind = KeyEventKind::Release;
        let out = handle_key(&mut app, ev);
        assert_eq!(out, KeyOutcome::Handled);
        assert!(!app.should_quit, "Release of 'q' must not quit");
    }
}
