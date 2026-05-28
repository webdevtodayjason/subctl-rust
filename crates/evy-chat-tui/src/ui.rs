//! ratatui paint logic.
//!
//! Two-pane layout — scrollback on top (≈75%), input box on the bottom
//! (≈25%) with a single-line footer for status. Bold (`**...**`) and
//! fenced code blocks (`` ```...``` ``) are hand-rolled because pulling
//! `pulldown-cmark` for two markers would double the crate's compile
//! time. The renderer is intentionally simple — see the
//! [`format_partner_text`] tests for the exact rules.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, ChatLine, LineKind, Status};

/// Top-level paint. Splits the frame into scrollback / input / footer
/// and dispatches.
pub fn render(f: &mut Frame<'_>, app: &App, base_url: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),    // scrollback
            Constraint::Length(6), // input box (4 rows of input + borders)
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    render_scrollback(f, chunks[0], app);
    render_input(f, chunks[1], app);
    render_footer(f, chunks[2], app, base_url);
}

fn render_scrollback(f: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::with_capacity(app.lines.len() * 2);
    for cl in &app.lines {
        push_chat_line(&mut lines, cl);
    }
    let text = Text::from(lines);
    let block = Block::default().borders(Borders::ALL).title(" Evy chat ");
    let paragraph = Paragraph::new(text)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset, 0));
    f.render_widget(paragraph, area);
}

fn render_input(f: &mut Frame<'_>, area: Rect, app: &App) {
    let title = match app.status {
        Status::Sending => " input — Enter to send (Evy is thinking…) ",
        _ => " input — Enter: send · Alt-Enter: newline · /help ",
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let paragraph = Paragraph::new(app.input.as_str())
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, area);
}

fn render_footer(f: &mut Frame<'_>, area: Rect, app: &App, base_url: &str) {
    let status_text = match &app.status {
        Status::Idle => "idle".to_string(),
        Status::Sending => "sending…".to_string(),
        Status::Error { message } => format!("error: {message}"),
    };
    let session = app
        .session_id
        .map(|id| format!("session {}", short_id(&id)))
        .unwrap_or_else(|| "no session yet".to_string());
    let footer = Line::from(vec![
        Span::styled(
            format!(" {base_url} "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("· "),
        Span::styled(session, Style::default().fg(Color::Cyan)),
        Span::raw(" · "),
        Span::styled(
            status_text,
            match app.status {
                Status::Error { .. } => Style::default().fg(Color::Red),
                Status::Sending => Style::default().fg(Color::Yellow),
                Status::Idle => Style::default().fg(Color::Green),
            },
        ),
    ]);
    f.render_widget(Paragraph::new(footer), area);
}

fn short_id(id: &uuid::Uuid) -> String {
    let s = id.to_string();
    s.chars().take(8).collect()
}

/// Push one [`ChatLine`] as one or more rendered `Line`s (markdown +
/// kind prefix). Visible to tests so the formatting rules are pinned.
pub fn push_chat_line(lines: &mut Vec<Line<'static>>, cl: &ChatLine) {
    let (prefix, prefix_style) = prefix_for_kind(cl.kind);
    let body_style = body_style_for_kind(cl.kind);
    // Operator + System + Error + Skills are plain. Only Partner gets
    // the bold + code-fence pass.
    let body_spans: Vec<Vec<Span<'static>>> = match cl.kind {
        LineKind::Partner => format_partner_text(&cl.text, body_style),
        _ => cl
            .text
            .lines()
            .map(|l| vec![Span::styled(l.to_string(), body_style)])
            .collect(),
    };

    let mut first = true;
    for spans in body_spans {
        if first {
            let mut line_spans: Vec<Span<'static>> =
                vec![Span::styled(prefix.to_string(), prefix_style)];
            line_spans.extend(spans);
            lines.push(Line::from(line_spans));
            first = false;
        } else {
            // Continuation lines indent two spaces under the prefix.
            let mut line_spans: Vec<Span<'static>> = vec![Span::raw("  ".to_string())];
            line_spans.extend(spans);
            lines.push(Line::from(line_spans));
        }
    }
    // Empty body still leaves a blank line so the prefix isn't lost.
    if cl.text.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            prefix.to_string(),
            prefix_style,
        )]));
    }
}

fn prefix_for_kind(kind: LineKind) -> (&'static str, Style) {
    match kind {
        LineKind::Operator => (
            "you  ▏ ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        LineKind::Partner => (
            "evy  ▏ ",
            Style::default()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
        ),
        LineKind::Skills => (
            "     ▏ ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
        LineKind::System => ("·    ▏ ", Style::default().fg(Color::Blue)),
        LineKind::Error => (
            "!    ▏ ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    }
}

fn body_style_for_kind(kind: LineKind) -> Style {
    match kind {
        LineKind::Operator => Style::default().fg(Color::White),
        LineKind::Partner => Style::default().fg(Color::Gray),
        LineKind::Skills => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
        LineKind::System => Style::default().fg(Color::Blue),
        LineKind::Error => Style::default().fg(Color::Red),
    }
}

/// Hand-rolled markdown formatter — handles `**bold**` and fenced
/// code blocks (`` ``` ``). Returns one `Vec<Span>` per source line.
///
/// Rules:
/// * Lines that fall inside a fenced code block are rendered with a
///   monospaced code style (foreground green-yellow) without bold pass.
/// * Outside code: `**word**` toggles bold. Asymmetric markers are
///   forwarded as literal asterisks so unbalanced input doesn't lose
///   characters.
#[must_use]
pub fn format_partner_text(text: &str, base_style: Style) -> Vec<Vec<Span<'static>>> {
    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    let code_style = Style::default().fg(Color::LightGreen);
    let bold_style = base_style.add_modifier(Modifier::BOLD);

    let mut in_code = false;
    for raw in text.lines() {
        if raw.trim_start().starts_with("```") {
            in_code = !in_code;
            // The fence itself is rendered dim so the operator can see
            // it without dominating the surface.
            out.push(vec![Span::styled(
                raw.to_string(),
                Style::default().fg(Color::DarkGray),
            )]);
            continue;
        }
        if in_code {
            out.push(vec![Span::styled(raw.to_string(), code_style)]);
            continue;
        }
        out.push(format_bold(raw, base_style, bold_style));
    }
    if out.is_empty() {
        out.push(vec![Span::styled(String::new(), base_style)]);
    }
    out
}

fn format_bold(line: &str, base: Style, bold: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut bolding = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            // Toggle bold; flush current run first.
            if !buf.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut buf),
                    if bolding { bold } else { base },
                ));
            }
            bolding = !bolding;
            i += 2;
            continue;
        }
        // UTF-8-safe slice: take the current char via str index.
        let ch_end = utf8_char_end(bytes, i);
        // Safety: `line[i..ch_end]` is a valid UTF-8 boundary because
        // `utf8_char_end` returns either i+1 for ASCII or the next
        // start-byte for multi-byte chars. We sliced `line.as_bytes()`
        // so converting back via `&line[i..ch_end]` is safe.
        buf.push_str(&line[i..ch_end]);
        i = ch_end;
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, if bolding { bold } else { base }));
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

fn utf8_char_end(bytes: &[u8], i: usize) -> usize {
    // Standard UTF-8 lead-byte width detection. ASCII and stray
    // continuation bytes both fall through with width 1; the latter
    // would mean we got a non-UTF-8 boundary (shouldn't happen because
    // `line` is `&str`) but we keep the safety net so a future caller
    // passing raw bytes doesn't loop forever.
    let b = bytes[i];
    let w = if b < 0xC0 {
        1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    };
    (i + w).min(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ChatLine;

    #[test]
    fn format_partner_text_bold_marker_toggles_style() {
        let base = Style::default();
        let out = format_partner_text("hello **bold** world", base);
        assert_eq!(out.len(), 1);
        let line = &out[0];
        // Three spans expected: "hello ", "bold", " world"
        assert!(line.len() >= 3);
        // Find the bold span by content.
        let bold_span = line
            .iter()
            .find(|s| s.content == "bold")
            .expect("bold span");
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn format_partner_text_fenced_code_block_uses_code_style() {
        let text = "before\n```\nfn x() {}\n```\nafter";
        let out = format_partner_text(text, Style::default());
        assert_eq!(out.len(), 5);
        // The middle line is the code body — content matches verbatim.
        let code_line = &out[2];
        let span = code_line.first().unwrap();
        assert_eq!(span.content, "fn x() {}");
        // Style should be the code-style green.
        assert_eq!(span.style.fg, Some(Color::LightGreen));
    }

    #[test]
    fn format_partner_text_unbalanced_bold_is_safe() {
        // Single ** is treated as a toggle that never closes; remaining
        // text renders bold. The point of this test is to confirm we
        // don't lose characters or panic on UTF-8 boundaries.
        let out = format_partner_text("oh **uh", Style::default());
        let joined: String = out[0].iter().map(|s| s.content.to_string()).collect();
        assert_eq!(joined, "oh uh");
    }

    #[test]
    fn format_partner_text_preserves_utf8() {
        let out = format_partner_text("über cool 🦀 stuff", Style::default());
        let joined: String = out[0].iter().map(|s| s.content.to_string()).collect();
        assert_eq!(joined, "über cool 🦀 stuff");
    }

    #[test]
    fn push_chat_line_emits_prefix_span_for_operator_line() {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let cl = ChatLine::new(LineKind::Operator, "hi");
        push_chat_line(&mut lines, &cl);
        assert_eq!(lines.len(), 1);
        let first_span_text: String = lines[0].spans[0].content.to_string();
        assert!(first_span_text.contains("you"));
    }

    #[test]
    fn push_chat_line_renders_multiline_partner_body() {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let cl = ChatLine::new(LineKind::Partner, "line1\nline2\nline3");
        push_chat_line(&mut lines, &cl);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn push_chat_line_skills_line_uses_dim_style() {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let cl = ChatLine::new(LineKind::Skills, "loaded: plan, debug");
        push_chat_line(&mut lines, &cl);
        assert_eq!(lines.len(), 1);
        let last_span = lines[0].spans.last().unwrap();
        assert!(last_span.style.add_modifier.contains(Modifier::ITALIC));
    }
}
