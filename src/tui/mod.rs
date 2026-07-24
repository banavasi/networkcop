//! The four-pane layout and its widgets.
//!
//! Layout is fixed by spec: top row split 50/50, bottom row 25% of the height,
//! console bottom-left and agent bottom-right.

mod chat;
mod console;
pub mod network;
mod overview;

use crate::app::{App, Pane};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders};

/// Where each pane landed on the last draw. Also what `--dump-layout` prints,
/// so pane geometry is machine-checkable rather than eyeballed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Layout4 {
    pub overview: Rect,
    pub network: Rect,
    pub console: Rect,
    pub chat: Rect,
}

/// Split a frame into the mandated geometry.
pub fn split(area: Rect) -> Layout4 {
    let rows =
        Layout::vertical([Constraint::Percentage(75), Constraint::Percentage(25)]).split(area);
    let top =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[0]);
    let bottom =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[1]);
    Layout4 {
        overview: top[0],
        network: top[1],
        console: bottom[0],
        chat: bottom[1],
    }
}

pub fn draw(f: &mut Frame, app: &App) -> Layout4 {
    let l = split(f.area());
    overview::draw(f, app, l.overview);
    network::draw(f, app, l.network);
    console::draw(f, app, l.console);
    chat::draw(f, app, l.chat);
    if app.domain_picker {
        network::draw_domain_picker(f, app, f.area());
    } else if app.detail_open {
        network::draw_detail(f, app, f.area());
    }
    l
}

/// Bordered block, highlighted when the pane owns the keyboard.
pub fn pane_block(app: &App, pane: Pane, title: &str) -> Block<'static> {
    let focused = app.focus == pane && !app.detail_open;
    let border = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let title_style = if focused {
        Style::default().fg(Color::Cyan).bold()
    } else {
        Style::default().fg(Color::Gray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(border)
        .title(Line::from(format!(" {title} ")).style(title_style))
}

/// Colour a status code the way a network panel does.
pub fn status_style(status: Option<i64>, errored: bool) -> Style {
    if errored {
        return Style::default().fg(Color::Red);
    }
    match status {
        Some(s) if s >= 500 => Style::default().fg(Color::Red).bold(),
        Some(s) if s >= 400 => Style::default().fg(Color::Yellow),
        Some(s) if s >= 300 => Style::default().fg(Color::Cyan),
        Some(s) if s >= 200 => Style::default().fg(Color::Green),
        Some(_) => Style::default().fg(Color::Gray),
        None => Style::default().fg(Color::DarkGray),
    }
}

pub fn method_color(method: &str) -> Color {
    match method.to_ascii_uppercase().as_str() {
        "GET" => Color::Green,
        "POST" => Color::Yellow,
        "PATCH" | "PUT" => Color::Cyan,
        "DELETE" => Color::Red,
        _ => Color::Gray,
    }
}

pub fn severity_color(sev: &str) -> Color {
    match sev {
        "error" => Color::Red,
        "warn" => Color::Yellow,
        "debug" => Color::DarkGray,
        _ => Color::Gray,
    }
}

/// Centred rectangle for the modal.
pub fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - pct_y) / 2),
        Constraint::Percentage(pct_y),
        Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(v[1])[1]
}

/// Which pane contains a click.
pub fn pane_at(l: &Layout4, x: u16, y: u16) -> Option<Pane> {
    let hit = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
    if hit(l.overview) {
        Some(Pane::Overview)
    } else if hit(l.network) {
        Some(Pane::Network)
    } else if hit(l.console) {
        Some(Pane::Console)
    } else if hit(l.chat) {
        Some(Pane::Chat)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_the_specified_geometry() {
        let area = Rect::new(0, 0, 200, 100);
        let l = split(area);

        // bottom row is 25% of the height
        assert_eq!(l.console.height, 25);
        assert_eq!(l.chat.height, 25);
        assert_eq!(l.overview.height, 75);

        // top row splits 50/50
        assert_eq!(l.overview.width, 100);
        assert_eq!(l.network.width, 100);
        assert_eq!(l.overview.x, 0);
        assert_eq!(l.network.x, 100);

        // console bottom-left, chat bottom-right
        assert_eq!(l.console.x, 0);
        assert_eq!(l.chat.x, 100);
        assert_eq!(l.console.y, 75);
    }

    #[test]
    fn every_pane_is_clickable_and_regions_do_not_overlap() {
        let l = split(Rect::new(0, 0, 200, 100));
        assert_eq!(pane_at(&l, 10, 10), Some(Pane::Overview));
        assert_eq!(pane_at(&l, 150, 10), Some(Pane::Network));
        assert_eq!(pane_at(&l, 10, 90), Some(Pane::Console));
        assert_eq!(pane_at(&l, 150, 90), Some(Pane::Chat));
        // boundaries belong to exactly one pane
        assert_eq!(pane_at(&l, 99, 74), Some(Pane::Overview));
        assert_eq!(pane_at(&l, 100, 75), Some(Pane::Chat));
    }

    #[test]
    fn tiny_terminals_do_not_produce_zero_width_panes() {
        for (w, h) in [(40u16, 12u16), (20, 8), (80, 24)] {
            let l = split(Rect::new(0, 0, w, h));
            for r in [l.overview, l.network, l.console, l.chat] {
                assert!(r.width > 0, "{w}x{h} produced a zero-width pane");
            }
        }
    }

    #[test]
    fn status_colours_follow_http_classes() {
        assert_eq!(status_style(Some(200), false).fg, Some(Color::Green));
        assert_eq!(status_style(Some(301), false).fg, Some(Color::Cyan));
        assert_eq!(status_style(Some(404), false).fg, Some(Color::Yellow));
        assert_eq!(status_style(Some(500), false).fg, Some(Color::Red));
        assert_eq!(
            status_style(Some(200), true).fg,
            Some(Color::Red),
            "net failure wins"
        );
    }

    #[test]
    fn centered_modal_stays_inside_the_frame() {
        let area = Rect::new(0, 0, 100, 50);
        let m = centered(area, 80, 80);
        assert!(m.x + m.width <= area.width);
        assert!(m.y + m.height <= area.height);
        assert!(m.width > 0 && m.height > 0);
    }
}
