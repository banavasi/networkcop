//! Right-top: the session overview — traffic bucketed by page, kind, domain or
//! status, so "what does /checkout actually call?" is one glance and one Enter.

use super::{pane_block, status_style};
use crate::app::{human_size, App, GroupBy, Pane};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let title = format!("{} · by {}", Pane::Overview.title(), app.group_by.label());
    let block = pane_block(app, Pane::Overview, &title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    // --- grouping selector ---
    let mut sel: Vec<Span> = Vec::new();
    for (i, g) in GroupBy::ALL.iter().enumerate() {
        if i > 0 {
            sel.push(Span::styled(" ", Style::default()));
        }
        let on = app.group_by == *g;
        sel.push(Span::styled(
            g.label().to_string(),
            if on {
                Style::default().fg(Color::Magenta).bold().underlined()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ));
    }
    sel.push(Span::styled("  [g]", Style::default().fg(Color::DarkGray)));
    lines.push(Line::from(sel));

    // --- current page, so you always know where you are ---
    let here = app
        .current_page
        .as_deref()
        .map(crate::db::path_of)
        .unwrap_or_else(|| "—".into());
    lines.push(Line::from(vec![
        Span::styled("on ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            elide(&here, inner.width.saturating_sub(4) as usize),
            Style::default().fg(Color::Cyan).bold(),
        ),
    ]));
    lines.push(Line::from(""));

    // --- the groups ---
    let groups = app.groups();
    let rows = inner.height.saturating_sub(lines.len() as u16 + 2) as usize;
    let start = app.group_cursor.saturating_sub(rows.saturating_sub(1));
    let w = inner.width as usize;

    if groups.is_empty() {
        lines.push(Line::from(Span::styled(
            "waiting for traffic…",
            Style::default().fg(Color::DarkGray).italic(),
        )));
    }

    for (i, g) in groups.iter().enumerate().skip(start).take(rows) {
        let cursor = i == app.group_cursor && app.focus == Pane::Overview;
        // "  12  3⚠  1.2 kB" — counts right-aligned, label takes the rest
        let counts = format!("{:>4}", g.total);
        let errs = if g.errors > 0 {
            format!(" {:>3}✗", g.errors)
        } else {
            "     ".into()
        };
        let label_w = w.saturating_sub(counts.len() + errs.len() + 2);
        lines.push(Line::from(vec![
            Span::styled(
                if cursor { "▍" } else { " " },
                Style::default().fg(if cursor { Color::Cyan } else { Color::Reset }),
            ),
            Span::styled(
                format!("{:<label_w$}", elide(&g.label, label_w)),
                if cursor {
                    Style::default().fg(Color::White).bold()
                } else if g.errors > 0 {
                    Style::default().fg(Color::Gray)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(
                counts,
                if g.rest > 0 {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::Gray)
                },
            ),
            Span::styled(errs, status_style(Some(500), g.errors > 0)),
        ]));
    }

    // --- rollup ---
    let (total, failed, console_errors) = app.counters();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(format!("{total} req · "), Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{failed} failed"),
            Style::default().fg(if failed > 0 { Color::Red } else { Color::Gray }),
        ),
        Span::styled(" · ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{console_errors} err"),
            Style::default().fg(if console_errors > 0 {
                Color::Red
            } else {
                Color::Gray
            }),
        ),
        Span::styled(
            format!(" · {}", human_size(app.total_bytes())),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    f.render_widget(Paragraph::new(lines), inner);
}

/// Trim from the left so the distinguishing tail of a long path stays visible.
pub fn elide(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if w == 0 {
        return String::new();
    }
    if n <= w {
        return s.to_string();
    }
    let keep: String = s.chars().skip(n - (w - 1)).collect();
    format!("…{keep}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_labels_keep_their_tail() {
        let s = elide("/api/v1/organisations/42/members", 12);
        assert_eq!(s.chars().count(), 12);
        assert!(s.ends_with("members"));
        assert_eq!(elide("/short", 20), "/short");
        assert_eq!(elide("/x", 0), "");
    }
}
