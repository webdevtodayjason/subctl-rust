//! Application state — the scrollback buffer + input draft + status line.
//!
//! Owns nothing the UI needs to mutate directly; the run loop in
//! `main.rs` drives transitions and the UI paints from snapshots.

use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

/// Type of message in the scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// The operator's input turn (echoed into scrollback on submit).
    Operator,
    /// Evy's reply.
    Partner,
    /// Skill-loaded indicator (rendered dim under the Partner line).
    Skills,
    /// Informational system line (e.g. "session started", "/clear ran").
    System,
    /// Error from the backend.
    Error,
}

/// One line in the scrollback. We store a Vec of these and the UI
/// wraps them at paint time.
#[derive(Debug, Clone)]
pub struct ChatLine {
    /// Line classification.
    pub kind: LineKind,
    /// Text body. Multi-line content lands as one `ChatLine` and the
    /// renderer wraps it.
    pub text: String,
    /// Unix epoch seconds when the line was created. Surfaced in the
    /// status footer for the latest line.
    pub ts_unix: u64,
}

impl ChatLine {
    /// Build a fresh line with `SystemTime::now()` timestamp.
    #[must_use]
    pub fn new(kind: LineKind, text: impl Into<String>) -> Self {
        let ts_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            kind,
            text: text.into(),
            ts_unix,
        }
    }
}

/// Connection / pending-request status surfaced in the footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// No request in flight.
    Idle,
    /// A POST is in flight; the UI shows a spinner glyph in the footer.
    Sending,
    /// The last request failed; `message` rendered in the footer until
    /// the next successful turn or `/clear`.
    Error {
        /// Short human description.
        message: String,
    },
}

/// Whole-application state. Cheap to mutate; everything is owned.
#[derive(Debug, Clone)]
pub struct App {
    /// Scrollback in chronological order.
    pub lines: Vec<ChatLine>,
    /// Active session id. `None` until the first turn lands a response.
    pub session_id: Option<Uuid>,
    /// Current input draft (multi-line allowed).
    pub input: String,
    /// Connection + request status for the footer.
    pub status: Status,
    /// Whether the run loop should exit on the next paint.
    pub should_quit: bool,
    /// Vertical scroll offset (in wrapped lines) from the bottom of the
    /// scrollback. `0` = pinned to the latest message.
    pub scroll_offset: u16,
}

impl App {
    /// Fresh empty app. The run loop pushes a "connected to <url>"
    /// system line before the first paint so the surface isn't blank.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            session_id: None,
            input: String::new(),
            status: Status::Idle,
            should_quit: false,
            scroll_offset: 0,
        }
    }

    /// Append a line to the scrollback. Resets scroll to the bottom so
    /// the operator always sees the latest message after their action
    /// or a partner reply.
    pub fn push_line(&mut self, line: ChatLine) {
        self.lines.push(line);
        self.scroll_offset = 0;
    }

    /// Convenience: append a System line.
    pub fn push_system(&mut self, text: impl Into<String>) {
        self.push_line(ChatLine::new(LineKind::System, text));
    }

    /// Convenience: append an Error line and set status to Error.
    pub fn push_error(&mut self, text: impl Into<String>) {
        let s = text.into();
        self.push_line(ChatLine::new(LineKind::Error, s.clone()));
        self.status = Status::Error { message: s };
    }

    /// Convenience: append an Operator line — used when the user
    /// presses Enter to submit their draft.
    pub fn push_operator(&mut self, text: impl Into<String>) {
        self.push_line(ChatLine::new(LineKind::Operator, text));
    }

    /// Convenience: append a Partner line + (optionally) a Skills line.
    pub fn push_partner_with_skills(&mut self, body: impl Into<String>, skills: &[String]) {
        self.push_line(ChatLine::new(LineKind::Partner, body));
        if !skills.is_empty() {
            self.push_line(ChatLine::new(
                LineKind::Skills,
                format!("loaded: {}", skills.join(", ")),
            ));
        }
    }

    /// Reset to a fresh session: clear scrollback + drop the session
    /// id. The current input draft is preserved (operator-friendly).
    pub fn clear(&mut self) {
        self.lines.clear();
        self.session_id = None;
        self.scroll_offset = 0;
        self.status = Status::Idle;
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_app_is_empty() {
        let app = App::new();
        assert!(app.lines.is_empty());
        assert!(app.input.is_empty());
        assert!(app.session_id.is_none());
        assert_eq!(app.status, Status::Idle);
        assert!(!app.should_quit);
    }

    #[test]
    fn push_partner_with_skills_emits_two_lines_when_skills_present() {
        let mut app = App::new();
        app.push_partner_with_skills("hello", &["plan".into(), "debug".into()]);
        assert_eq!(app.lines.len(), 2);
        assert_eq!(app.lines[0].kind, LineKind::Partner);
        assert_eq!(app.lines[1].kind, LineKind::Skills);
        assert!(app.lines[1].text.contains("plan"));
    }

    #[test]
    fn push_partner_with_empty_skills_emits_only_partner_line() {
        let mut app = App::new();
        app.push_partner_with_skills("hello", &[]);
        assert_eq!(app.lines.len(), 1);
        assert_eq!(app.lines[0].kind, LineKind::Partner);
    }

    #[test]
    fn push_error_marks_status() {
        let mut app = App::new();
        app.push_error("boom");
        assert_eq!(app.lines.len(), 1);
        assert_eq!(app.lines[0].kind, LineKind::Error);
        assert_eq!(
            app.status,
            Status::Error {
                message: "boom".into()
            }
        );
    }

    #[test]
    fn clear_drops_lines_and_session() {
        let mut app = App::new();
        app.session_id = Some(Uuid::nil());
        app.push_operator("hi");
        app.push_line(ChatLine::new(LineKind::Partner, "hi back"));
        app.clear();
        assert!(app.lines.is_empty());
        assert!(app.session_id.is_none());
    }

    #[test]
    fn push_line_resets_scroll_offset() {
        let mut app = App::new();
        app.scroll_offset = 5;
        app.push_system("hello");
        assert_eq!(app.scroll_offset, 0);
    }
}
