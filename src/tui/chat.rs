//! Bottom-right: the agent pane — transcript plus the input line.

use super::pane_block;
use crate::app::{App, ChatRole, Pane};
use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let title = if app.spend_usd > 0.0 {
        format!("{} (${:.3})", Pane::Chat.title(), app.spend_usd)
    } else {
        Pane::Chat.title().to_string()
    };
    let block = pane_block(app, Pane::Chat, &title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let input_h = 1;
    let log_h = inner.height.saturating_sub(input_h);

    // --- transcript ---
    if log_h > 0 {
        let mut lines: Vec<Line> = Vec::new();
        for m in &app.chat {
            lines.extend(render_msg(m.role.clone(), &m.text));
        }
        if app.thinking {
            lines.push(Line::from(Span::styled(
                "thinking…",
                Style::default().fg(Color::DarkGray).italic(),
            )));
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "Ask about this session, or /help",
                Style::default().fg(Color::DarkGray).italic(),
            )));
        }

        // pin to the bottom unless scrolled up
        let total = lines.len();
        let cap = log_h as usize;
        let max_off = total.saturating_sub(cap);
        let off = if app.chat_scroll == usize::MAX {
            max_off
        } else {
            app.chat_scroll.min(max_off)
        };
        let view: Vec<Line> = lines.into_iter().skip(off).take(cap).collect();

        f.render_widget(
            Paragraph::new(view).wrap(Wrap { trim: false }),
            Rect {
                height: log_h,
                ..inner
            },
        );
    }

    // --- input line ---
    let focused = app.focus == Pane::Chat && !app.detail_open;
    let caret = if focused { "›" } else { " " };
    let input_area = Rect {
        y: inner.y + log_h,
        height: input_h,
        ..inner
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{caret} "),
                Style::default().fg(if focused { Color::Cyan } else { Color::DarkGray }),
            ),
            Span::styled(
                app.input.buf.clone(),
                Style::default().fg(Color::White),
            ),
        ])),
        input_area,
    );

    // real terminal cursor, so editing feels native
    if focused && !app.thinking {
        let x = input_area.x + 2 + app.input.cursor as u16;
        if x < input_area.x + input_area.width {
            f.set_cursor_position((x, input_area.y));
        }
    }
}

/// Render one message. Split out so the transcript shape is testable.
pub fn render_msg(role: ChatRole, text: &str) -> Vec<Line<'static>> {
    let (prefix, style) = match role {
        ChatRole::User => (
            "you › ",
            Style::default().fg(Color::Cyan).bold(),
        ),
        ChatRole::Agent => ("", Style::default().fg(Color::Gray)),
        ChatRole::System => ("", Style::default().fg(Color::DarkGray).italic()),
    };

    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let mut spans = Vec::new();
        if i == 0 && !prefix.is_empty() {
            spans.push(Span::styled(prefix.to_string(), style));
        }
        spans.push(Span::styled(
            raw.to_string(),
            if role == ChatRole::User {
                Style::default().fg(Color::White)
            } else {
                style
            },
        ));
        out.push(Line::from(spans));
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn user_messages_are_prefixed_once() {
        let out = render_msg(ChatRole::User, "why did checkout fail?");
        assert_eq!(out.len(), 1);
        assert!(flat(&out).starts_with("you › "));
    }

    #[test]
    fn multiline_agent_replies_keep_every_line() {
        // /review emits seven lines; none may be dropped
        let prompt = "Create a feature branch called fix/x.\n\
                      Reproduce the bug using the provided steps.\n\
                      Implement the fix.\n\
                      Write or update tests.\n\
                      Create a pull request with a clear title and description.\n\
                      Reproduction steps and expected behaviour:\n\
                      1. do the thing";
        let out = render_msg(ChatRole::Agent, prompt);
        assert_eq!(out.len(), 7);
        let text = flat(&out);
        assert!(text.contains("Create a feature branch called fix/x."));
        assert!(text.contains("Reproduction steps and expected behaviour:"));
        assert!(!text.contains("you › "), "agent lines are unprefixed");
    }

    #[test]
    fn empty_message_still_produces_a_row() {
        assert_eq!(render_msg(ChatRole::Agent, "").len(), 1);
    }
}
