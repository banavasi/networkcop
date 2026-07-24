//! Bottom-left: the console log — timestamp, severity, message.

use super::{pane_block, severity_color};
use crate::app::{App, Pane};
use crate::db::ConsoleLine;
use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let errors = app.console.iter().filter(|c| c.severity == "error").count();
    let title = if errors > 0 {
        format!("{} ({errors} errors)", Pane::Console.title())
    } else {
        format!("{} ({})", Pane::Console.title(), app.console.len())
    };
    let block = pane_block(app, Pane::Console, &title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let cap = inner.height as usize;
    // pinned to the tail unless the user has scrolled up
    let max_off = app.console.len().saturating_sub(cap);
    let off = app.console_scroll.min(max_off);
    let slice: Vec<&ConsoleLine> = app.console.iter().skip(off).take(cap).collect();

    let lines: Vec<Line> = if slice.is_empty() {
        vec![Line::from(Span::styled(
            " no console output yet",
            Style::default().fg(Color::DarkGray).italic(),
        ))]
    } else {
        slice.iter().map(|c| render(c, inner.width)).collect()
    };

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

/// One console row. Split out so the canned-event test can assert on it without
/// standing up a terminal.
pub fn render(c: &ConsoleLine, width: u16) -> Line<'static> {
    let sev = c.severity.to_uppercase();
    let text_w = width.saturating_sub(15) as usize;
    Line::from(vec![
        Span::styled(
            format!("{} ", clock(&c.ts)),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("{:<5} ", &sev[..sev.len().min(5)]),
            Style::default().fg(severity_color(&c.severity)).bold(),
        ),
        Span::styled(
            one_line(&c.text, text_w),
            Style::default().fg(match c.severity.as_str() {
                "error" => Color::Red,
                "warn" => Color::Yellow,
                _ => Color::Gray,
            }),
        ),
    ])
}

/// `2026-07-24T10:14:02.123Z` → `10:14:02`
fn clock(ts: &str) -> String {
    ts.split('T')
        .nth(1)
        .map(|t| t.chars().take(8).collect())
        .unwrap_or_else(|| "--:--:--".into())
}

fn one_line(s: &str, w: usize) -> String {
    let flat = s.replace(['\n', '\r'], " ");
    if w == 0 || flat.chars().count() <= w {
        flat
    } else {
        flat.chars().take(w.saturating_sub(1)).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(sev: &str, text: &str) -> ConsoleLine {
        ConsoleLine {
            ts: "2026-07-24T10:14:02.123456+00:00".into(),
            severity: sev.into(),
            text: text.into(),
            url: None,
            line: None,
            source: "console".into(),
        }
    }

    /// M4's machine-checkable half: a canned event must produce a rendered row
    /// with the right severity colour. A console pane wired to nothing fails here.
    #[test]
    fn a_canned_error_event_renders_a_coloured_row() {
        let l = render(
            &line("error", "Uncaught TypeError: t.total is undefined"),
            80,
        );
        let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("10:14:02"), "timestamp: {text}");
        assert!(text.contains("ERROR"), "severity: {text}");
        assert!(text.contains("TypeError"), "message: {text}");
        assert_eq!(l.spans[1].style.fg, Some(Color::Red), "errors must be red");
        assert_eq!(l.spans[2].style.fg, Some(Color::Red));
    }

    #[test]
    fn severities_are_visually_distinct() {
        let cases = [
            ("error", Color::Red),
            ("warn", Color::Yellow),
            ("info", Color::Gray),
            ("debug", Color::DarkGray),
        ];
        for (sev, want) in cases {
            let l = render(&line(sev, "x"), 80);
            assert_eq!(l.spans[1].style.fg, Some(want), "{sev} badge colour");
        }
    }

    #[test]
    fn multiline_messages_collapse_to_one_row() {
        let l = render(&line("error", "line one\nline two\r\nline three"), 80);
        let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains('\n'), "must stay on one row");
        assert!(text.contains("line one line two"));
    }

    #[test]
    fn narrow_panes_do_not_panic_or_overflow() {
        for w in [0u16, 1, 8, 15, 16, 40] {
            let l = render(&line("warn", &"x".repeat(500)), w);
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.chars().count() < 600);
        }
    }

    #[test]
    fn a_bad_timestamp_does_not_break_the_row() {
        let mut c = line("info", "hello");
        c.ts = "garbage".into();
        let l = render(&c, 80);
        let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("--:--:--"));
    }
}
