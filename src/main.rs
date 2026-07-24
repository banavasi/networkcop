//! networkcop — launch Chrome, capture everything, ask questions about it.

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use networkcop::agent::{self, llm::Backend, Session};
use networkcop::app::{App, ChatRole, Pane, TAB_ALL};
use networkcop::cdp::{self, Capture, LaunchOpts};
use networkcop::db::{self, ConsoleLine, Db, Write as DbWrite};
use networkcop::tui;
use ratatui::prelude::*;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(
    name = "networkcop",
    version,
    about = "Terminal agent harness for front-end debugging — captures a Chrome session and answers questions strictly from it."
)]
struct Cli {
    /// Port of the local dev server to open (http://localhost:<port>).
    #[arg(default_value = "3000")]
    port: u16,

    /// Full URL to open instead of localhost:<port>.
    #[arg(long)]
    url: Option<String>,

    /// Run Chrome headless.
    #[arg(long)]
    headless: bool,

    /// Session database (default ~/.networkcop/sessions.db).
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    /// Reuse a Chrome profile directory, so authenticated apps capture real traffic.
    #[arg(long)]
    profile: Option<PathBuf>,

    /// Chrome binary to launch.
    #[arg(long, env = "NETWORKCOP_CHROME")]
    chrome: Option<String>,

    /// Port Chrome listens on for DevTools.
    #[arg(long, default_value = "9222")]
    debug_port: u16,

    /// Largest response body stored whole, in bytes.
    #[arg(long, default_value = "2097152")]
    max_body: u64,

    /// Model for the agent pane.
    #[arg(long, default_value = "haiku", env = "NETWORKCOP_MODEL")]
    model: String,

    /// Use the Python LangGraph sidecar instead of the claude CLI.
    #[arg(long, env = "NETWORKCOP_SIDECAR")]
    sidecar: Option<String>,

    /// Where exports are written.
    #[arg(long, default_value = ".")]
    out_dir: PathBuf,

    /// Ask one question, print the answer, exit. No TUI, no browser.
    #[arg(long)]
    ask: Option<String>,

    /// Print the four pane rectangles as JSON and exit.
    #[arg(long)]
    dump_layout: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List recorded sessions.
    Sessions {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // --dump-layout needs no runtime, no browser, no database.
    if cli.dump_layout {
        let l = tui::split(Rect::new(0, 0, 200, 100));
        let j = |r: Rect| {
            serde_json::json!({"x": r.x, "y": r.y, "width": r.width, "height": r.height})
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "overview": j(l.overview),
                "network":  j(l.network),
                "console":  j(l.console),
                "chat":     j(l.chat),
            }))?
        );
        return Ok(());
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    let db_path = cli.db.clone().unwrap_or_else(Db::default_path);

    if let Some(Cmd::Sessions { json }) = &cli.cmd {
        return list_sessions(&db_path, *json);
    }

    let target = cli
        .url
        .clone()
        .unwrap_or_else(|| format!("http://localhost:{}", cli.port));
    let backend = match &cli.sidecar {
        Some(url) => Backend::Sidecar { url: url.clone() },
        None => Backend::ClaudeCli {
            model: cli.model.clone(),
        },
    };

    // One-shot question mode: read the last session, answer, exit.
    if let Some(q) = &cli.ask {
        return ask_once(&db_path, &target, &backend, q, &cli.out_dir).await;
    }

    let db = Db::open(&db_path, &target)?;
    let session_id = db.session_id;

    let (browser, cdp, mut captures) = cdp::launch(&LaunchOpts {
        port: cli.port,
        headless: cli.headless,
        debug_port: cli.debug_port,
        user_data_dir: cli.profile.clone(),
        chrome_binary: cli.chrome.clone(),
        max_body: cli.max_body,
    })
    .await?;

    cdp.navigate(&target).await.ok();

    // --- writer task: batch captures into SQLite ---
    let (writes_tx, mut writes_rx) = mpsc::channel::<DbWrite>(2048);
    let writer = tokio::task::spawn_blocking({
        let db_path = db_path.clone();
        move || -> Result<()> {
            let mut wdb = Db::attach(&db_path, Some(session_id))?;
            let mut batch = Vec::with_capacity(256);
            // block for the next write, then drain whatever else is queued
            while let Some(first) = writes_rx.blocking_recv() {
                batch.push(first);
                while let Ok(w) = writes_rx.try_recv() {
                    batch.push(w);
                    if batch.len() >= 256 {
                        break;
                    }
                }
                wdb.apply(&batch)?;
                batch.clear();
            }
            // drain on shutdown — the session must always reach disk
            while let Ok(w) = writes_rx.try_recv() {
                batch.push(w);
            }
            if !batch.is_empty() {
                wdb.apply(&batch)?;
            }
            wdb.finish()?;
            Ok(())
        }
    });

    // --- terminal ---
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;

    let mut app = App::new(target.clone());
    app.say(
        ChatRole::System,
        format!(
            "networkcop — capturing {target}\nreasoner: {}\ntab/click to move · enter opens a request · /help",
            backend.describe()
        ),
    );

    // agent replies come back on this channel so the UI never blocks
    let (agent_tx, mut agent_rx) = mpsc::channel::<(String, f64)>(8);

    let mut events = EventStream::new();
    let mut layout = tui::Layout4::default();
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    // An external SIGINT/SIGTERM must still reach the flush path — "graceful
    // shutdown always flushes" has to hold for `kill`, not just for `q`.
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let res: Result<()> = loop {
        term.draw(|f| layout = tui::draw(f, &app))?;
        if app.should_quit {
            break Ok(());
        }

        tokio::select! {
            biased;

            Some(ev) = events.next() => {
                match ev {
                    Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                        on_key(&mut app, k, &writes_tx, &agent_tx, &backend,
                               &db_path, session_id, &target, &cli.out_dir).await;
                    }
                    Ok(Event::Mouse(m)) => on_mouse(&mut app, m, &layout),
                    Ok(Event::Resize(_, _)) => {}
                    Err(e) => break Err(e.into()),
                    _ => {}
                }
            }

            Some(cap) = captures.recv() => {
                ingest(&mut app, cap, &writes_tx).await;
            }

            Some((text, cost)) = agent_rx.recv() => {
                app.thinking = false;
                app.spend_usd += cost;
                app.say(ChatRole::Agent, text.clone());
                let _ = writes_tx.send(DbWrite::Chat {
                    ts: Utc::now(), role: "agent".into(), text, cost_usd: cost,
                }).await;
            }

            _ = sigint.recv() => { app.should_quit = true; }
            _ = sigterm.recv() => { app.should_quit = true; }

            _ = ticker.tick() => {}
        }
    };

    // --- shutdown: always flush ---
    disable_raw_mode().ok();
    execute!(term.backend_mut(), LeaveAlternateScreen, DisableMouseCapture).ok();
    term.show_cursor().ok();

    // snapshot the final DOM before Chrome goes away
    if let Ok(html) = cdp.dom_snapshot().await {
        let url = cdp.current_url().await.unwrap_or_else(|_| target.clone());
        let _ = writes_tx
            .send(DbWrite::DomSnapshot {
                ts: Utc::now(),
                url,
                html,
            })
            .await;
    }

    drop(writes_tx);
    drop(cdp);
    browser.shutdown().await;
    match writer.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("warning: session flush failed: {e}"),
        Err(e) => eprintln!("warning: writer task failed: {e}"),
    }
    db.finish().ok();

    println!("session {session_id} saved to {}", db_path.display());
    res
}

/// Translate one capture into UI state plus a durable write.
async fn ingest(app: &mut App, cap: Capture, writes: &mpsc::Sender<DbWrite>) {
    match cap {
        Capture::Request(r) => {
            app.exchanges.push(db::Exchange {
                request_id: r.request_id.clone(),
                ts: Utc::now().to_rfc3339(),
                method: r.method.clone(),
                url: r.url.clone(),
                resource_type: r.resource_type.clone(),
                req_headers: r.headers.clone(),
                req_body: r.post_data.clone(),
                ..Default::default()
            });
            let _ = writes
                .send(DbWrite::Request {
                    request_id: r.request_id,
                    ts: Utc::now(),
                    method: r.method,
                    url: r.url,
                    resource_type: r.resource_type,
                    headers: r.headers,
                    body: r.post_data,
                })
                .await;
        }
        Capture::Response(r) => {
            if let Some(e) = app
                .exchanges
                .iter_mut()
                .rev()
                .find(|e| e.request_id == r.request_id)
            {
                e.status = Some(r.status);
                e.status_text = Some(r.status_text.clone());
                e.res_headers = r.headers.clone();
                e.mime_type = Some(r.mime_type.clone());
                e.duration_ms = r.duration_ms;
                e.from_cache = r.from_cache;
            }
            let _ = writes
                .send(DbWrite::Response {
                    request_id: r.request_id,
                    status: r.status,
                    status_text: r.status_text,
                    headers: r.headers,
                    mime_type: r.mime_type,
                    remote_ip: r.remote_ip,
                    from_cache: r.from_cache,
                    duration_ms: r.duration_ms,
                })
                .await;
        }
        Capture::Body {
            request_id,
            body,
            base64,
            truncated_from,
        } => {
            if let Some(e) = app
                .exchanges
                .iter_mut()
                .rev()
                .find(|e| e.request_id == request_id)
            {
                e.size = truncated_from.unwrap_or(body.len() as u64);
                e.res_body = Some(body.clone());
                e.res_body_b64 = base64;
                e.truncated_from = truncated_from;
            }
            let _ = writes
                .send(DbWrite::Body {
                    request_id,
                    body,
                    base64,
                    truncated_from,
                })
                .await;
        }
        Capture::Failed {
            request_id, error, ..
        } => {
            if let Some(e) = app
                .exchanges
                .iter_mut()
                .rev()
                .find(|e| e.request_id == request_id)
            {
                e.error = Some(error.clone());
            }
            let _ = writes.send(DbWrite::Failed { request_id, error }).await;
        }
        Capture::Console(c) => {
            let line = ConsoleLine {
                ts: Utc::now().to_rfc3339(),
                severity: c.severity.clone(),
                text: c.text.clone(),
                url: c.url.clone(),
                line: c.line,
                source: c.source.clone(),
            };
            app.console.push(line.clone());
            let _ = writes.send(DbWrite::Console(line)).await;
        }
        Capture::Navigated { url, is_main, .. } => {
            if is_main {
                app.navigations.push(db::Navigation {
                    ts: Utc::now().to_rfc3339(),
                    url: url.clone(),
                });
            }
            let _ = writes
                .send(DbWrite::Navigation {
                    ts: Utc::now(),
                    url,
                    is_main,
                })
                .await;
        }
        Capture::Detached(msg) => {
            app.status = msg.clone();
            app.say(ChatRole::System, format!("{msg} — captured data is still queryable."));
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn on_key(
    app: &mut App,
    k: KeyEvent,
    writes: &mpsc::Sender<DbWrite>,
    agent_tx: &mpsc::Sender<(String, f64)>,
    backend: &Backend,
    db_path: &std::path::Path,
    session_id: i64,
    target: &str,
    out_dir: &std::path::Path,
) {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

    // ctrl-c always quits, whatever has focus
    if ctrl && matches!(k.code, KeyCode::Char('c')) {
        app.should_quit = true;
        return;
    }

    // modal owns the keyboard while open
    if app.detail_open {
        match k.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                app.detail_open = false;
                app.detail_scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => app.detail_scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => {
                app.detail_scroll = app.detail_scroll.saturating_sub(1)
            }
            KeyCode::PageDown => app.detail_scroll += 20,
            KeyCode::PageUp => app.detail_scroll = app.detail_scroll.saturating_sub(20),
            _ => {}
        }
        return;
    }

    // the chat pane swallows text input; everything else is a global binding
    if app.focus == Pane::Chat {
        match k.code {
            KeyCode::Enter => {
                let text = app.input.take();
                if !text.trim().is_empty() {
                    submit(app, text, writes, agent_tx, backend, db_path, session_id, target, out_dir)
                        .await;
                }
                return;
            }
            KeyCode::Char(c) if !ctrl => {
                app.input.insert(c);
                return;
            }
            KeyCode::Backspace => {
                app.input.backspace();
                return;
            }
            KeyCode::Delete => {
                app.input.delete();
                return;
            }
            KeyCode::Left => {
                app.input.left();
                return;
            }
            KeyCode::Right => {
                app.input.right();
                return;
            }
            KeyCode::Home => {
                app.input.home();
                return;
            }
            KeyCode::End => {
                app.input.end();
                return;
            }
            KeyCode::Up => {
                app.chat_scroll = app.chat_scroll.saturating_sub(1);
                return;
            }
            KeyCode::Down => {
                if app.chat_scroll != usize::MAX {
                    app.chat_scroll += 1;
                }
                return;
            }
            KeyCode::Esc => {
                app.focus = Pane::Network;
                return;
            }
            _ => {}
        }
    }

    match k.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Tab => app.focus = app.focus.next(),
        KeyCode::BackTab => app.focus = app.focus.prev(),
        KeyCode::Char('i') => app.focus = Pane::Chat,

        KeyCode::Down | KeyCode::Char('j') => match app.focus {
            Pane::Network => app.select_next(),
            Pane::Console => app.console_scroll += 1,
            _ => {}
        },
        KeyCode::Up | KeyCode::Char('k') => match app.focus {
            Pane::Network => app.select_prev(),
            Pane::Console => app.console_scroll = app.console_scroll.saturating_sub(1),
            _ => {}
        },
        KeyCode::Enter if app.focus == Pane::Network => {
            if app.selected_exchange().is_some() {
                app.detail_open = true;
                app.detail_scroll = 0;
            }
        }
        KeyCode::Left | KeyCode::Char('h') if app.focus == Pane::Network => app.cycle_tab(false),
        KeyCode::Right | KeyCode::Char('l') if app.focus == Pane::Network => app.cycle_tab(true),
        KeyCode::Char(c @ '1'..='5') if app.focus == Pane::Network => {
            app.set_tab(c as usize - '1' as usize);
        }
        KeyCode::Esc => {
            app.tab = TAB_ALL;
            app.selected = 0;
        }
        _ => {}
    }
}

fn on_mouse(app: &mut App, m: MouseEvent, layout: &tui::Layout4) {
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if app.detail_open {
                app.detail_open = false;
                return;
            }
            let Some(pane) = tui::pane_at(layout, m.column, m.row) else {
                return;
            };
            app.focus = pane;

            if pane == Pane::Network {
                let inner_y = layout.network.y + 1; // border
                // row 0 of the inner area is the tab strip
                if m.row == inner_y {
                    if let Some(idx) = tab_hit(layout.network.x + 1, m.column) {
                        app.set_tab(idx);
                    }
                } else if m.row > inner_y {
                    let visible = app.visible();
                    let cap = layout.network.height.saturating_sub(3) as usize;
                    let start = app.selected.saturating_sub(cap.saturating_sub(1));
                    let row = (m.row - inner_y - 1) as usize + start;
                    if row < visible.len() {
                        // clicking the selected row opens it
                        if app.selected == row {
                            app.detail_open = true;
                            app.detail_scroll = 0;
                        } else {
                            app.selected = row;
                        }
                    }
                }
            }
        }
        MouseEventKind::ScrollDown => match app.focus {
            Pane::Network => app.select_next(),
            Pane::Console => app.console_scroll += 1,
            Pane::Chat if app.chat_scroll != usize::MAX => app.chat_scroll += 1,
            _ => {}
        },
        MouseEventKind::ScrollUp => match app.focus {
            Pane::Network => app.select_prev(),
            Pane::Console => app.console_scroll = app.console_scroll.saturating_sub(1),
            Pane::Chat => app.chat_scroll = app.chat_scroll.saturating_sub(1),
            _ => {}
        },
        _ => {}
    }
}

/// Which method tab a click at `col` landed on, given the strip starts at `x0`.
/// Layout must match the render in tui/network.rs: `GET | POST | PATCH | …`.
pub fn tab_hit(x0: u16, col: u16) -> Option<usize> {
    let mut cursor = x0;
    for (i, name) in networkcop::app::METHOD_TABS.iter().enumerate() {
        let w = name.len() as u16;
        if col >= cursor && col < cursor + w {
            return Some(i);
        }
        cursor += w + 3; // " | "
    }
    None
}

#[allow(clippy::too_many_arguments)]
async fn submit(
    app: &mut App,
    text: String,
    writes: &mpsc::Sender<DbWrite>,
    agent_tx: &mpsc::Sender<(String, f64)>,
    backend: &Backend,
    db_path: &std::path::Path,
    session_id: i64,
    target: &str,
    out_dir: &std::path::Path,
) {
    app.say(ChatRole::User, text.clone());
    let _ = writes
        .send(DbWrite::Chat {
            ts: Utc::now(),
            role: "user".into(),
            text: text.clone(),
            cost_usd: 0.0,
        })
        .await;
    app.thinking = true;

    // Snapshot the session from what the UI already holds — no DB round-trip,
    // and no risk of reading a half-written batch.
    let session = Session {
        target: target.to_string(),
        exchanges: app.exchanges.clone(),
        console: app.console.clone(),
        navigations: app.navigations.clone(),
    };
    let backend = backend.clone();
    let db_path = db_path.to_path_buf();
    let out_dir = out_dir.to_path_buf();
    let tx = agent_tx.clone();

    tokio::spawn(async move {
        let intent = agent::classify(&text);
        let mut adb = match Db::attach(&db_path, Some(session_id)) {
            Ok(d) => d,
            Err(e) => {
                let _ = tx.send((format!("session unavailable: {e}"), 0.0)).await;
                return;
            }
        };
        match agent::handle(intent, &session, &backend, &mut adb, &out_dir).await {
            Ok(r) => {
                let _ = tx.send((r.text, r.cost_usd)).await;
            }
            Err(e) => {
                let _ = tx.send((format!("that failed: {e}"), 0.0)).await;
            }
        }
    });
}

/// `--ask` — answer from the last recorded session and exit.
async fn ask_once(
    db_path: &std::path::Path,
    target: &str,
    backend: &Backend,
    question: &str,
    out_dir: &std::path::Path,
) -> Result<()> {
    let mut db = Db::attach(db_path, None).with_context(|| {
        format!(
            "no session in {} — run `networkcop <port>` first",
            db_path.display()
        )
    })?;
    let session_id = db.session_id;
    // the recorded target, not whatever the current flags imply
    let recorded = db.target(session_id).unwrap_or_default();
    let target = if recorded.is_empty() { target } else { &recorded };
    let session = Session::load(&db, session_id, target)?;
    let intent = agent::classify(question);
    let reply = agent::handle(intent, &session, backend, &mut db, out_dir).await?;
    println!("{}", reply.text);
    Ok(())
}

fn list_sessions(db_path: &std::path::Path, json: bool) -> Result<()> {
    if !db_path.exists() {
        if json {
            println!("[]");
        } else {
            println!("no sessions recorded yet ({})", db_path.display());
        }
        return Ok(());
    }
    let db = Db::attach(db_path, None)?;
    let rows = db.sessions()?;
    if json {
        let out: Vec<_> = rows
            .iter()
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "started_at": s.started_at,
                    "ended_at": s.ended_at,
                    "target": s.target,
                    "requests": s.requests,
                    "errors": s.errors,
                    "console_errors": s.console_errors,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!(
            "{:<5} {:<26} {:<34} {:>8} {:>7} {:>7}",
            "ID", "STARTED", "TARGET", "REQUESTS", "FAILED", "ERRORS"
        );
        for s in &rows {
            println!(
                "{:<5} {:<26} {:<34} {:>8} {:>7} {:>7}",
                s.id,
                s.started_at.chars().take(25).collect::<String>(),
                s.target.chars().take(33).collect::<String>(),
                s.requests,
                s.errors,
                s.console_errors
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_clicks_map_to_the_rendered_strip() {
        // "GET | POST | PATCH | DELETE | OTHER" starting at column 10
        assert_eq!(tab_hit(10, 10), Some(0)); // G
        assert_eq!(tab_hit(10, 12), Some(0)); // T
        assert_eq!(tab_hit(10, 13), None); // separator
        assert_eq!(tab_hit(10, 16), Some(1)); // POST
        assert_eq!(tab_hit(10, 23), Some(2)); // PATCH
        assert_eq!(tab_hit(10, 31), Some(3)); // DELETE
        assert_eq!(tab_hit(10, 40), Some(4)); // OTHER
        assert_eq!(tab_hit(10, 200), None);
    }
}
