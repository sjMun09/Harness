//! Pure render — `(Frame, &TuiApp) → ()`. No mutation, no I/O.
//!
//! Layout:
//!   ┌────────────── header ───────────────┐
//!   │ harness — model · session           │
//!   ├─────────────────────────────────────┤
//!   │                                     │
//!   │ scrollback (messages + tool cards)  │
//!   │                                     │
//!   ├─────────────────────────────────────┤
//!   │ status line                         │
//!   ├─────────────────────────────────────┤
//!   │ > input (multi-line)                │
//!   └─────────────────────────────────────┘
//!
//! Permission modal overlays the centre when `app.modal.is_some()`.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{Entry, ToolCard, ToolStatus, TuiApp};
use crate::markdown;

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub fn draw(frame: &mut Frame<'_>, app: &TuiApp) {
    let size = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(3),    // scrollback
            Constraint::Length(1), // status
            Constraint::Length(input_height(&app.input)),
        ])
        .split(size);

    draw_header(frame, chunks[0], app);
    draw_scrollback(frame, chunks[1], app);
    draw_status(frame, chunks[2], app);
    draw_input(frame, chunks[3], app);

    if app.modal.is_some() {
        draw_modal(frame, size, app);
    }
}

fn input_height(input: &str) -> u16 {
    let lines = input.lines().count().max(1) as u16;
    (lines + 2).min(8) // +2 for borders, cap at 8 rows
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let title = format!(" harness · {} · {} ", app.model, app.session_label);
    let p = Paragraph::new(title).style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(p, area);
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let mut spans = vec![Span::styled(
        format!(" {} ", app.status),
        Style::default().fg(Color::White).bg(Color::DarkGray),
    )];
    if app.turn_active {
        let tick = (std::time::Instant::now().elapsed().as_millis() / 80) as usize;
        spans.push(Span::styled(
            format!(" {} working", SPINNER[tick % SPINNER.len()]),
            Style::default().fg(Color::Yellow),
        ));
    }
    if app.cancel_requested {
        spans.push(Span::styled(
            "  [cancelling]".to_string(),
            Style::default().fg(Color::Red),
        ));
    }
    let p = Paragraph::new(Line::from(spans));
    frame.render_widget(p, area);
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " input  (Enter=submit · Shift+Enter=newline · Ctrl+C/Esc=cancel · q=quit) ",
            Style::default().fg(Color::Gray),
        ));
    let p = Paragraph::new(format!("> {}", app.input))
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

fn draw_scrollback(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in &app.scrollback {
        match entry {
            Entry::User(text) => {
                lines.push(Line::from(Span::styled(
                    "▎ user".to_string(),
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                )));
                for ln in text.lines() {
                    lines.push(Line::from(Span::raw(format!("  {ln}"))));
                }
                lines.push(Line::from(""));
            }
            Entry::Assistant(text) => {
                lines.push(Line::from(Span::styled(
                    "▎ assistant".to_string(),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )));
                let rendered = markdown::render(text);
                for line in rendered.lines {
                    let mut prefixed = vec![Span::raw("  ".to_string())];
                    prefixed.extend(line.spans);
                    lines.push(Line::from(prefixed));
                }
                lines.push(Line::from(""));
            }
            Entry::Tool(id) => {
                if let Some(card) = app.tool_cards.get(id) {
                    lines.extend(render_tool_card(card));
                    lines.push(Line::from(""));
                }
            }
            Entry::Notice(msg) => {
                lines.push(Line::from(Span::styled(
                    format!("⚠ {msg}"),
                    Style::default().fg(Color::Yellow),
                )));
                lines.push(Line::from(""));
            }
        }
    }

    // In-flight assistant: show streaming text under a dim header.
    if let Some(pending) = app.pending_assistant.as_deref() {
        if !pending.is_empty() {
            lines.push(Line::from(Span::styled(
                "▎ assistant (streaming…)".to_string(),
                Style::default().fg(Color::Green),
            )));
            let rendered = markdown::render(pending);
            for line in rendered.lines {
                let mut prefixed = vec![Span::raw("  ".to_string())];
                prefixed.extend(line.spans);
                lines.push(Line::from(prefixed));
            }
        }
    }

    let total_lines = lines.len() as u16;
    // ratatui's `Paragraph::scroll((y, _))` is from the *top*. We want
    // bottom-anchored: scroll = max(0, total - viewport) - user_offset.
    let viewport = area.height.saturating_sub(2);
    let base = total_lines.saturating_sub(viewport);
    let scroll = base.saturating_sub(app.scroll_offset);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " scrollback ",
            Style::default().fg(Color::Gray),
        ));
    let p = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(p, area);
}

fn render_tool_card(card: &ToolCard) -> Vec<Line<'static>> {
    let (status_glyph, status_style) = match &card.status {
        ToolStatus::Running => {
            let tick = (std::time::Instant::now().elapsed().as_millis() / 80) as usize;
            (
                SPINNER[tick % SPINNER.len()].to_string(),
                Style::default().fg(Color::Yellow),
            )
        }
        ToolStatus::Ok { .. } => ("✓".to_string(), Style::default().fg(Color::Green)),
        ToolStatus::Err { .. } => ("✗".to_string(), Style::default().fg(Color::Red)),
    };

    let header = Line::from(vec![
        Span::styled(format!(" {status_glyph} "), status_style),
        Span::styled(
            card.name.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(card.preview.clone(), Style::default().fg(Color::Gray)),
    ]);

    let mut lines = vec![header];
    match &card.status {
        ToolStatus::Running => {}
        ToolStatus::Ok { summary, elapsed } | ToolStatus::Err { summary, elapsed } => {
            for ln in summary.lines().take(8) {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(ln.to_string(), Style::default().fg(Color::DarkGray)),
                ]));
            }
            lines.push(Line::from(Span::styled(
                format!("    [{elapsed:?}]"),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    lines
}

fn draw_modal(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let Some(modal) = &app.modal else {
        return;
    };
    let popup = centred_rect(60, 30, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " permission required ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));

    let body = Text::from(vec![
        Line::from(vec![
            Span::styled("tool: ", Style::default().fg(Color::Gray)),
            Span::styled(
                modal.tool.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("call: ", Style::default().fg(Color::Gray)),
            Span::raw(modal.preview.clone()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "[y]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" allow once    "),
            Span::styled(
                "[a]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" allow always    "),
            Span::styled(
                "[n]",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" deny"),
        ]),
    ]);
    let p = Paragraph::new(body).block(block).wrap(Wrap { trim: false });
    frame.render_widget(p, popup);
}

fn centred_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(v[1])[1]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::time::Duration;

    fn dump(buf: &ratatui::buffer::Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn renders_empty_app_without_panic() {
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        let app = TuiApp::new("claude-opus-4-7", "demo").unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let out = dump(term.backend().buffer());
        assert!(out.contains("harness"));
        assert!(out.contains("input"));
    }

    #[test]
    fn renders_user_and_assistant_message() {
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut app = TuiApp::new("m", "s").unwrap();
        app.scrollback.push(Entry::User("hello".into()));
        app.scrollback.push(Entry::Assistant("**hi**".into()));
        term.draw(|f| draw(f, &app)).unwrap();
        let out = dump(term.backend().buffer());
        assert!(out.contains("user"));
        assert!(out.contains("hello"));
        assert!(out.contains("assistant"));
        assert!(out.contains("hi"));
    }

    #[test]
    fn renders_tool_card_running() {
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut app = TuiApp::new("m", "s").unwrap();
        app.tool_start("t1".into(), "Read".into(), "/etc/hosts".into());
        term.draw(|f| draw(f, &app)).unwrap();
        let out = dump(term.backend().buffer());
        assert!(out.contains("Read"));
        assert!(out.contains("/etc/hosts"));
    }

    #[test]
    fn renders_tool_card_ok() {
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut app = TuiApp::new("m", "s").unwrap();
        app.tool_start("t1".into(), "Read".into(), "/etc/hosts".into());
        app.tool_end("t1", true, "20 lines".into(), Duration::from_millis(5));
        term.draw(|f| draw(f, &app)).unwrap();
        let out = dump(term.backend().buffer());
        assert!(out.contains("✓") || out.contains("Read"));
        assert!(out.contains("20 lines"));
    }

    #[test]
    fn renders_modal_when_open() {
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut app = TuiApp::new("m", "s").unwrap();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.open_permission_modal("p1".into(), "Edit".into(), "Edit foo.rs".into(), tx);
        term.draw(|f| draw(f, &app)).unwrap();
        let out = dump(term.backend().buffer());
        assert!(out.contains("permission"));
        assert!(out.contains("Edit"));
        assert!(out.contains("[y]"));
        assert!(out.contains("[a]"));
        assert!(out.contains("[n]"));
    }
}
