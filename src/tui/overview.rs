//! Top-left: the session at a glance — navigation trail, request waterfall,
//! and the rollup counters.

use super::{method_color, pane_block, status_style};
use crate::app::{human_size, App, Pane};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let block = pane_block(app, Pane::Overview, Pane::Overview.title());
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    // --- navigation trail ---
    if app.navigations.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("nav  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                app.target.clone(),
                Style::default().fg(Color::DarkGray).italic(),
            ),
        ]));
    } else {
        let trail: Vec<String> = app
            .navigations
            .iter()
            .rev()
            .take(4)
            .rev()
            .map(|n| short_path(&n.url))
            .collect();
        let mut spans = vec![Span::styled("nav  ", Style::default().fg(Color::DarkGray))];
        for (i, p) in trail.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" → ", Style::default().fg(Color::DarkGray)));
            }
            let last = i == trail.len() - 1;
            spans.push(Span::styled(
                p.clone(),
                if last {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::Gray)
                },
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));

    // --- waterfall ---
    // Bars are positioned by arrival order and scaled by duration; the point is
    // to show which calls are slow and where they cluster, not wall-clock truth.
    let rows = inner.height.saturating_sub(4) as usize;
    let recent: Vec<_> = app.exchanges.iter().rev().take(rows).rev().collect();
    let max_ms = recent.iter().map(|e| e.duration_ms).fold(1.0_f64, f64::max);
    let track = inner.width.saturating_sub(20) as usize;

    for e in &recent {
        let frac = (e.duration_ms / max_ms).clamp(0.0, 1.0);
        let filled = ((frac * track as f64).round() as usize).max(1).min(track);
        let colour = if e.is_error() {
            Color::Red
        } else {
            method_color(&e.method)
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<6} ", short_method(&e.method)),
                Style::default().fg(colour).bold(),
            ),
            Span::styled("█".repeat(filled), Style::default().fg(colour)),
            Span::styled(
                "·".repeat(track.saturating_sub(filled)),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!(" {:>7}", fmt_ms(e.duration_ms)),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    // --- rollup ---
    let (total, failed, console_errors) = app.counters();
    lines.push(Line::from(""));
    let mut roll = vec![
        Span::styled(format!("{total} req"), Style::default().fg(Color::Gray)),
        Span::styled(" · ", Style::default().fg(Color::DarkGray)),
    ];
    roll.push(Span::styled(
        format!("{failed} failed"),
        status_style(if failed > 0 { Some(500) } else { Some(200) }, false),
    ));
    roll.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
    roll.push(Span::styled(
        format!("{console_errors} console err"),
        Style::default().fg(if console_errors > 0 {
            Color::Red
        } else {
            Color::Gray
        }),
    ));
    roll.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
    roll.push(Span::styled(
        human_size(app.total_bytes()),
        Style::default().fg(Color::Gray),
    ));
    lines.push(Line::from(roll));

    f.render_widget(Paragraph::new(lines), inner);
}

fn short_method(m: &str) -> String {
    let m = m.to_ascii_uppercase();
    if m.len() > 6 {
        m[..6].to_string()
    } else {
        m
    }
}

pub fn fmt_ms(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.1}s", ms / 1000.0)
    } else {
        format!("{ms:.0}ms")
    }
}

/// `http://host/a/b?c` → `/a/b`, and `/` for a bare origin.
pub fn short_path(url: &str) -> String {
    let after = match url.split_once("://") {
        Some((_, r)) => r,
        None => url,
    };
    match after.find('/') {
        Some(i) => {
            let p = after[i..].split('?').next().unwrap_or("/");
            if p.is_empty() {
                "/".into()
            } else {
                p.to_string()
            }
        }
        None => "/".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(fmt_ms(86.0), "86ms");
        assert_eq!(fmt_ms(999.4), "999ms");
        assert_eq!(fmt_ms(2100.0), "2.1s");
    }

    #[test]
    fn paths_shorten_to_something_readable() {
        assert_eq!(
            short_path("http://localhost:8080/checkout?step=2"),
            "/checkout"
        );
        assert_eq!(short_path("http://localhost:8080"), "/");
        assert_eq!(short_path("http://localhost:8080/"), "/");
        assert_eq!(short_path("about:blank"), "/");
    }
}
