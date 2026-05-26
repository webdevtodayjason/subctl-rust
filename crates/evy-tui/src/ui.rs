//! ratatui rendering. One [`render`] entry point dispatches to a
//! per-tab function; each per-tab function paints into a sub-rect of
//! the frame.
//!
//! Layout:
//!
//! ```text
//! ┌─────────────────────────────────────────────┐
//! │ [Workers] Scheduler  Events  Policy         │  ← tab bar (3 rows incl. border)
//! ├─────────────────────────────────────────────┤
//! │                                             │
//! │           per-tab body                      │  ← fills remaining height
//! │                                             │
//! ├─────────────────────────────────────────────┤
//! │ daemon: live | base=http://… | q quit       │  ← status bar (1 row)
//! └─────────────────────────────────────────────┘
//! ```
//!
//! All rendering takes `&mut Frame` so it works in both
//! production (`Terminal::draw`) and the smoke test
//! (`ratatui::Terminal::with_options` against `TestBackend`).

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Row, Table, Tabs, Wrap},
    Frame,
};

use crate::app::{App, Tab};

/// Top-level render entry. Lays out tab bar / body / status bar and
/// dispatches to the per-tab renderer.
pub fn render(frame: &mut Frame, app: &App, base_url: &str) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tab bar
            Constraint::Min(1),    // body
            Constraint::Length(1), // status bar
        ])
        .split(area);

    render_tab_bar(frame, app, chunks[0]);
    render_body(frame, app, chunks[1]);
    render_status_bar(frame, app, base_url, chunks[2]);
}

fn render_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line> = Tab::ALL
        .iter()
        .map(|t| Line::from(Span::raw(t.label())))
        .collect();
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title("evy-tui"))
        .select(app.tab.index())
        .highlight_style(Style::default().add_modifier(Modifier::BOLD).reversed());
    frame.render_widget(tabs, area);
}

fn render_status_bar(frame: &mut Frame, app: &App, base_url: &str, area: Rect) {
    let line = Line::from(vec![
        Span::raw(" daemon: "),
        Span::styled(
            app.connection.label(),
            match app.connection {
                crate::app::ConnectionState::Live => Style::default().green(),
                crate::app::ConnectionState::Connecting => Style::default().yellow(),
                crate::app::ConnectionState::Disconnected { .. } => Style::default().red(),
            },
        ),
        Span::raw(" │ base="),
        Span::raw(base_url),
        Span::raw(" │ "),
        Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" quit "),
        Span::styled("r", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" refresh "),
        Span::styled("Tab", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" next "),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_body(frame: &mut Frame, app: &App, area: Rect) {
    match app.tab {
        Tab::Workers => render_workers(frame, app, area),
        Tab::Scheduler => render_scheduler(frame, app, area),
        Tab::Events => render_events(frame, app, area),
        Tab::Policy => render_policy(frame, app, area),
    }
}

fn render_workers(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Workers ({}) ", app.workers.len()));

    if app.workers.is_empty() {
        let p = Paragraph::new("No workers registered yet.")
            .block(block)
            .wrap(Wrap { trim: true });
        frame.render_widget(p, area);
        return;
    }

    let header = Row::new(vec!["IDX", "PROVIDER", "STATUS", "WORKER ID", "MANDATE ID"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .workers
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let marker = if i == app.workers_cursor { "▶" } else { " " };
            Row::new(vec![
                format!("{marker} {i}"),
                format!("{:?}", w.provider),
                format!("{:?}", w.status),
                format!("{}", w.id.0),
                format!("{}", w.mandate_id.0),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(14),
            Constraint::Length(20),
            Constraint::Length(38),
            Constraint::Length(38),
        ],
    )
    .header(header)
    .block(block);
    frame.render_widget(table, area);
}

fn render_scheduler(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Scheduler ({}) ", app.jobs.len()));

    if app.jobs.is_empty() {
        let p = Paragraph::new("No scheduler jobs registered yet.")
            .block(block)
            .wrap(Wrap { trim: true });
        frame.render_widget(p, area);
        return;
    }

    let header = Row::new(vec!["IDX", "NAME", "CRON", "ACTION", "ENABLED"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .jobs
        .iter()
        .enumerate()
        .map(|(i, j)| {
            let marker = if i == app.jobs_cursor { "▶" } else { " " };
            Row::new(vec![
                format!("{marker} {i}"),
                j.name.clone(),
                j.cron_expr.clone(),
                j.action_kind.clone(),
                if j.enabled { "yes" } else { "no" }.to_owned(),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Min(20),
            Constraint::Length(14),
            Constraint::Length(20),
            Constraint::Length(8),
        ],
    )
    .header(header)
    .block(block);
    frame.render_widget(table, area);
}

fn render_events(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(format!(
        " Events ({}/{}) ",
        app.events.len(),
        super::app::EVENT_LOG_CAPACITY
    ));

    if app.events.is_empty() {
        let p = Paragraph::new("Awaiting daemon events…").block(block);
        frame.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = app
        .events
        .iter()
        .enumerate()
        .map(|(i, ev)| {
            let marker = if i == app.events_cursor { "▶" } else { " " };
            let kind = ev.kind_tag();
            ListItem::new(Line::from(vec![
                Span::raw(format!("{marker} ")),
                Span::styled(format!("{kind:<14}"), Style::default().cyan()),
                Span::raw(" "),
                Span::raw(ev.summary()),
            ]))
        })
        .collect();

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

fn render_policy(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" Policy ");

    let body = match &app.policy {
        None => "No policy loaded yet.".to_owned(),
        Some(p) => render_policy_tree(&p.0, 0),
    };

    let p = Paragraph::new(body).block(block).wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

/// Render a `serde_json::Value` as an indented tree. Best-effort and
/// purely visual; the operator's looking for "which mode is default,
/// what's in the allowlist". Long string values get truncated.
fn render_policy_tree(value: &serde_json::Value, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    match value {
        serde_json::Value::Null => format!("{indent}<null>\n"),
        serde_json::Value::Bool(b) => format!("{indent}{b}\n"),
        serde_json::Value::Number(n) => format!("{indent}{n}\n"),
        serde_json::Value::String(s) => {
            if s.len() > 72 {
                format!("{indent}\"{}…\"\n", &s[..72])
            } else {
                format!("{indent}\"{s}\"\n")
            }
        }
        serde_json::Value::Array(arr) => {
            if arr.is_empty() {
                return format!("{indent}[]\n");
            }
            let mut s = String::new();
            for (i, v) in arr.iter().enumerate() {
                s.push_str(&format!("{indent}[{i}]\n"));
                s.push_str(&render_policy_tree(v, depth + 1));
            }
            s
        }
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                return format!("{indent}{{}}\n");
            }
            let mut s = String::new();
            for (k, v) in map {
                match v {
                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                        s.push_str(&format!("{indent}{k}:\n"));
                        s.push_str(&render_policy_tree(v, depth + 1));
                    }
                    _ => {
                        let inline = match v {
                            serde_json::Value::String(s) => format!("\"{s}\""),
                            other => other.to_string(),
                        };
                        s.push_str(&format!("{indent}{k}: {inline}\n"));
                    }
                }
            }
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{DaemonEvent, JobSummary, PolicyView, WorkerSummary};
    use crate::app::{App, ConnectionState};
    use evy_core::{MandateId, ProviderKind, WorkerId, WorkerStatus};
    use ratatui::{backend::TestBackend, Terminal};

    fn populated_app() -> App {
        let mut app = App::new();
        app.set_workers(vec![WorkerSummary {
            id: WorkerId::new(),
            provider: ProviderKind::ClaudeCode,
            mandate_id: MandateId::new(),
            status: WorkerStatus::Running,
        }]);
        app.set_jobs(vec![JobSummary {
            id: serde_json::Value::String("00000000-0000-0000-0000-000000000000".into()),
            name: "heartbeat".into(),
            cron_expr: "*/5 * * * *".into(),
            action_kind: "log_heartbeat".into(),
            enabled: true,
        }]);
        app.set_policy(PolicyView(serde_json::json!({
            "default_mode": "gated",
            "mode": {"trusted": {"allow": ["ls", "pwd"]}}
        })));
        app.push_event(DaemonEvent::Heartbeat {
            ts: chrono::Utc::now(),
            providers_healthy: 2,
        });
        app.set_connection(ConnectionState::Live);
        app
    }

    fn draw_for(tab: Tab) {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let mut app = populated_app();
        app.tab = tab;
        terminal
            .draw(|f| render(f, &app, "http://127.0.0.1:8787"))
            .expect("draw");
    }

    #[test]
    fn renders_workers_tab_on_80x24_without_panic() {
        draw_for(Tab::Workers);
    }

    #[test]
    fn renders_scheduler_tab_on_80x24_without_panic() {
        draw_for(Tab::Scheduler);
    }

    #[test]
    fn renders_events_tab_on_80x24_without_panic() {
        draw_for(Tab::Events);
    }

    #[test]
    fn renders_policy_tab_on_80x24_without_panic() {
        draw_for(Tab::Policy);
    }

    #[test]
    fn renders_empty_app_without_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::new();
        terminal
            .draw(|f| render(f, &app, "http://127.0.0.1:8787"))
            .unwrap();
    }

    #[test]
    fn policy_tree_handles_nested_object() {
        let v = serde_json::json!({
            "a": 1,
            "b": {"c": "deep"}
        });
        let rendered = render_policy_tree(&v, 0);
        assert!(rendered.contains("a: 1"));
        assert!(rendered.contains("b:"));
        assert!(rendered.contains("c: \"deep\""));
    }

    #[test]
    fn policy_tree_handles_empty_collections() {
        assert!(render_policy_tree(&serde_json::json!([]), 0).contains("[]"));
        assert!(render_policy_tree(&serde_json::json!({}), 0).contains("{}"));
    }
}
