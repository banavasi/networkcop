//! Top-right: the request list with clickable method tabs, and the detail modal.
//!
//! The list shows method, URL, status and size — nothing else, by spec. The modal
//! shows the complete request and response headers and bodies — and nothing else.

use super::{centered, method_color, pane_block, status_style};
use crate::app::{human_size, App, Kind, Pane, METHOD_TABS};
use crate::db::Exchange;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

/// Row offset of the method tab strip inside the pane's inner area.
pub const TAB_ROW: u16 = 0;
/// Row offset of the kind + domain strip.
pub const KIND_ROW: u16 = 1;
/// First row of the list inside the inner area.
pub const LIST_TOP: u16 = 2;

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let visible = app.visible();
    let title = format!(
        "{} ({}/{})",
        Pane::Network.title(),
        visible.len(),
        app.exchanges.len()
    );
    let block = pane_block(app, Pane::Network, &title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // --- tab strip ---
    let mut tabs: Vec<Span> = Vec::new();
    for (i, name) in METHOD_TABS.iter().enumerate() {
        if i > 0 {
            tabs.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
        }
        let on = app.tab == i;
        tabs.push(Span::styled(
            name.to_string(),
            if on {
                Style::default()
                    .fg(method_color(name))
                    .bold()
                    .underlined()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ));
    }
    if app.filters_active() {
        tabs.push(Span::styled(
            "   [esc] clear",
            Style::default().fg(Color::DarkGray),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(tabs)),
        Rect {
            y: inner.y + TAB_ROW,
            height: 1,
            ..inner
        },
    );

    // --- kind strip + domain selector ---
    let mut kinds: Vec<Span> = Vec::new();
    for (i, k) in Kind::ALL.iter().enumerate().skip(1) {
        if i > 1 {
            kinds.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
        }
        let on = app.kind == *k;
        kinds.push(Span::styled(
            k.label().to_string(),
            if on {
                Style::default().fg(Color::Magenta).bold().underlined()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ));
    }
    kinds.push(Span::styled("   ", Style::default()));
    kinds.push(Span::styled(
        "[d] ".to_string(),
        Style::default().fg(Color::DarkGray),
    ));
    match &app.domain {
        Some(d) => kinds.push(Span::styled(
            elide(d, 24),
            Style::default().fg(Color::Cyan).bold(),
        )),
        None => kinds.push(Span::styled(
            format!("all domains ({})", app.domains().len()),
            Style::default().fg(Color::DarkGray),
        )),
    }
    f.render_widget(
        Paragraph::new(Line::from(kinds)),
        Rect {
            y: inner.y + KIND_ROW,
            height: 1,
            ..inner
        },
    );

    // --- rows ---
    let rows_area = Rect {
        y: inner.y + LIST_TOP,
        height: inner.height.saturating_sub(LIST_TOP),
        ..inner
    };
    if rows_area.height == 0 {
        return;
    }

    let cap = rows_area.height as usize;
    let start = app.selected.saturating_sub(cap.saturating_sub(1));
    let mut lines: Vec<Line> = Vec::new();

    for (row, idx) in visible.iter().enumerate().skip(start).take(cap) {
        let e = &app.exchanges[*idx];
        let sel = row == app.selected;
        let url_w = rows_area.width.saturating_sub(7 + 5 + 9) as usize;
        let marker = if sel { "▍" } else { " " };
        lines.push(Line::from(vec![
            Span::styled(
                marker,
                Style::default().fg(if sel { Color::Cyan } else { Color::Reset }),
            ),
            Span::styled(
                format!("{:<6}", e.method_bucket_display()),
                Style::default().fg(method_color(&e.method)).bold(),
            ),
            Span::styled(
                format!("{:<w$}", elide(&display_url(e), url_w), w = url_w),
                if sel {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::Gray)
                },
            ),
            Span::styled(
                format!("{:>4}", status_text(e)),
                status_style(e.status, e.error.is_some()),
            ),
            Span::styled(
                format!("{:>9}", human_size(e.size)),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            if app.exchanges.is_empty() {
                "  waiting for traffic…".to_string()
            } else {
                format!("  nothing matches {}", app.filter_label())
            },
            Style::default().fg(Color::DarkGray).italic(),
        )));
    }
    f.render_widget(Paragraph::new(lines), rows_area);
}

fn status_text(e: &Exchange) -> String {
    match (e.status, &e.error) {
        (Some(s), _) if s > 0 => s.to_string(),
        (_, Some(_)) => "ERR".into(),
        _ => "…".into(),
    }
}

fn display_url(e: &Exchange) -> String {
    let host = e.host();
    let path = e.path();
    // local calls are the common case; showing the host every time wastes width
    if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
        path
    } else {
        format!("{host}{path}")
    }
}

/// Trim from the left so the distinguishing tail of a long path stays visible.
fn elide(s: &str, w: usize) -> String {
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

/// Domain picker. Lists every host seen this session, busiest first, so the
/// third-party noise can be filtered away in one keystroke.
pub fn draw_domain_picker(f: &mut Frame, app: &App, area: Rect) {
    let modal = centered(area, 60, 60);
    f.render_widget(Clear, modal);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(" Filter by domain ").style(Style::default().fg(Color::Cyan).bold()))
        .title_bottom(
            Line::from(" ↑↓ move · enter select · esc cancel ")
                .style(Style::default().fg(Color::DarkGray)),
        );
    let inner = block.inner(modal);
    f.render_widget(block, modal);
    if inner.height == 0 {
        return;
    }

    let rows = domain_rows(app);
    let cap = inner.height as usize;
    let start = app.domain_cursor.saturating_sub(cap.saturating_sub(1));
    let width = inner.width as usize;

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(start)
        .take(cap)
        .map(|(i, (label, count, active))| {
            let sel = i == app.domain_cursor;
            let count_txt = count.map(|n| n.to_string()).unwrap_or_default();
            let name_w = width.saturating_sub(count_txt.len() + 4);
            Line::from(vec![
                Span::styled(
                    if sel { "▍" } else { " " },
                    Style::default().fg(if sel { Color::Cyan } else { Color::Reset }),
                ),
                Span::styled(
                    if *active { "● " } else { "  " },
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!("{:<w$}", elide(label, name_w), w = name_w),
                    if sel {
                        Style::default().fg(Color::White).bold()
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ),
                Span::styled(count_txt, Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    f.render_widget(Paragraph::new(lines), inner);
}

/// Picker rows: "all domains" first, then each host. `None` count = the all row.
/// Shared with the key handler so the cursor and the display cannot disagree.
pub fn domain_rows(app: &App) -> Vec<(String, Option<usize>, bool)> {
    let mut rows: Vec<(String, Option<usize>, bool)> =
        vec![("all domains".into(), None, app.domain.is_none())];
    for (host, n) in app.domains() {
        let active = app.domain.as_deref() == Some(host.as_str());
        rows.push((host, Some(n), active));
    }
    rows
}

/// The complete exchange. Nothing but headers and bodies, per spec.
pub fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let Some(e) = app.selected_exchange() else {
        return;
    };
    let modal = centered(area, 86, 86);
    f.render_widget(Clear, modal);

    let title = format!(" {} {} — {} ", e.method, e.path(), status_text(e));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(title).style(Style::default().fg(Color::Cyan).bold()))
        .title_bottom(
            Line::from(" ↑↓ scroll · esc/enter close ")
                .style(Style::default().fg(Color::DarkGray)),
        );
    let inner = block.inner(modal);
    f.render_widget(block, modal);

    let mut lines = detail_lines(e);
    // scroll
    let max = lines.len().saturating_sub(inner.height as usize);
    let off = app.detail_scroll.min(max);
    if off > 0 {
        lines.drain(..off);
    }

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        inner,
    );
}

/// Built separately from rendering so it can be asserted in tests.
pub fn detail_lines(e: &Exchange) -> Vec<Line<'static>> {
    let head = |s: &str| {
        Line::from(Span::styled(
            s.to_string(),
            Style::default().fg(Color::Cyan).bold(),
        ))
    };
    let kv = |k: &str, v: &str| {
        Line::from(vec![
            Span::styled(format!("  {k}: "), Style::default().fg(Color::DarkGray)),
            Span::styled(v.to_string(), Style::default().fg(Color::Gray)),
        ])
    };

    let mut lines = vec![head("REQUEST HEADERS")];
    if e.req_headers.is_empty() {
        lines.push(kv("(none captured)", ""));
    }
    for (k, v) in &e.req_headers {
        lines.push(kv(k, v));
    }

    lines.push(Line::from(""));
    lines.push(head("REQUEST BODY"));
    match &e.req_body {
        Some(b) if !b.trim().is_empty() => {
            for l in pretty(b).lines() {
                lines.push(Line::from(Span::styled(
                    format!("  {l}"),
                    Style::default().fg(Color::White),
                )));
            }
        }
        _ => lines.push(kv("(empty)", "")),
    }

    lines.push(Line::from(""));
    lines.push(head("RESPONSE HEADERS"));
    if e.res_headers.is_empty() {
        lines.push(kv("(none captured)", ""));
    }
    for (k, v) in &e.res_headers {
        lines.push(kv(k, v));
    }

    lines.push(Line::from(""));
    lines.push(head("RESPONSE BODY"));
    if let Some(from) = e.truncated_from {
        lines.push(Line::from(Span::styled(
            format!("  (truncated — full size {})", human_size(from)),
            Style::default().fg(Color::Yellow),
        )));
    }
    match e.body_text() {
        Some(b) if !b.trim().is_empty() => {
            for l in pretty(&b).lines() {
                lines.push(Line::from(Span::styled(
                    format!("  {l}"),
                    Style::default().fg(Color::White),
                )));
            }
        }
        Some(_) => lines.push(kv("(empty)", "")),
        None => lines.push(kv(
            if e.res_body.is_some() {
                "(binary)"
            } else {
                "(not captured)"
            },
            "",
        )),
    }
    lines
}

/// Pretty-print JSON bodies; leave everything else alone.
fn pretty(s: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| s.to_string()),
        Err(_) => s.to_string(),
    }
}

impl Exchange {
    /// Display form of the method — the real verb, not the tab bucket, so a PUT
    /// still reads "PUT" while filtering under OTHER.
    pub fn method_bucket_display(&self) -> String {
        let m = self.method.to_ascii_uppercase();
        if m.chars().count() > 6 {
            m.chars().take(6).collect()
        } else {
            m
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Headers;

    fn sample() -> Exchange {
        let mut h = Headers::new();
        h.insert("content-type".into(), "application/json".into());
        h.insert("authorization".into(), "Bearer xyz".into());
        let mut rh = Headers::new();
        rh.insert("content-type".into(), "application/json".into());
        Exchange {
            method: "POST".into(),
            url: "http://localhost:8080/api/cart/checkout?x=1".into(),
            status: Some(500),
            req_headers: h,
            res_headers: rh,
            req_body: Some(r#"{"qty":0}"#.into()),
            res_body: Some(br#"{"error":"empty_line_item"}"#.to_vec()),
            size: 27,
            ..Default::default()
        }
    }

    #[test]
    fn detail_shows_all_four_sections_and_nothing_else() {
        let text: String = detail_lines(&sample())
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("REQUEST HEADERS"));
        assert!(text.contains("REQUEST BODY"));
        assert!(text.contains("RESPONSE HEADERS"));
        assert!(text.contains("RESPONSE BODY"));
        assert!(text.contains("authorization"));
        assert!(text.contains("empty_line_item"));
        // spec: the modal shows headers and bodies — no timing/size chrome
        assert!(!text.contains("duration"), "no timing in the modal");
        assert!(!text.contains("TIMING"));
    }

    #[test]
    fn json_bodies_are_pretty_printed() {
        let text: String = detail_lines(&sample())
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("\"error\": \"empty_line_item\""), "pretty JSON");
    }

    #[test]
    fn missing_bodies_are_labelled_not_blank() {
        let e = Exchange {
            method: "GET".into(),
            url: "http://x/y".into(),
            ..Default::default()
        };
        let text: String = detail_lines(&e)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("(empty)"));
        assert!(text.contains("(not captured)"));
    }

    #[test]
    fn truncation_is_disclosed_with_the_real_size() {
        let mut e = sample();
        e.truncated_from = Some(12_279_560);
        let text: String = detail_lines(&e)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("truncated"));
        assert!(text.contains("11.7 MB"));
    }

    #[test]
    fn long_urls_keep_their_tail() {
        let s = elide("/api/v1/organisations/42/members/99/permissions", 20);
        assert_eq!(s.chars().count(), 20);
        assert!(s.starts_with('…'));
        assert!(s.ends_with("permissions"), "tail preserved: {s}");
        assert_eq!(elide("/short", 20), "/short");
        assert_eq!(elide("/x", 0), "");
    }

    #[test]
    fn local_urls_drop_the_host() {
        assert_eq!(display_url(&sample()), "/api/cart/checkout");
        let mut e = sample();
        e.url = "https://api.example.com/v1/things".into();
        assert_eq!(display_url(&e), "api.example.com/v1/things");
    }
}
