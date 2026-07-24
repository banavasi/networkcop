//! Application state: what is focused, what is filtered, what the chat says.
//!
//! Deliberately free of ratatui types so the state transitions are testable
//! without a terminal.

use crate::db::{ConsoleLine, Exchange, Navigation};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Overview,
    Network,
    Console,
    Chat,
}

impl Pane {
    pub const ALL: [Pane; 4] = [Pane::Overview, Pane::Network, Pane::Console, Pane::Chat];

    pub fn next(self) -> Self {
        match self {
            Pane::Overview => Pane::Network,
            Pane::Network => Pane::Console,
            Pane::Console => Pane::Chat,
            Pane::Chat => Pane::Overview,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Pane::Overview => Pane::Chat,
            Pane::Network => Pane::Overview,
            Pane::Console => Pane::Network,
            Pane::Chat => Pane::Console,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Pane::Overview => "Session overview",
            Pane::Network => "Network",
            Pane::Console => "Console",
            Pane::Chat => "Agent",
        }
    }
}

pub const METHOD_TABS: [&str; 5] = ["GET", "POST", "PATCH", "DELETE", "OTHER"];
/// Index that means "no filter".
pub const TAB_ALL: usize = usize::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Agent,
    System,
}

#[derive(Debug, Clone)]
pub struct ChatMsg {
    pub role: ChatRole,
    pub text: String,
}

/// A single-line input with a cursor. Hand-rolled rather than pulling in a text
/// widget: it is thirty lines and has no version-compat surface.
#[derive(Debug, Default)]
pub struct Input {
    pub buf: String,
    pub cursor: usize,
}

impl Input {
    pub fn insert(&mut self, c: char) {
        let idx = self.byte_at(self.cursor);
        self.buf.insert(idx, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_at(self.cursor - 1);
        let end = self.byte_at(self.cursor);
        self.buf.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.len() {
            return;
        }
        let start = self.byte_at(self.cursor);
        let end = self.byte_at(self.cursor + 1);
        self.buf.replace_range(start..end, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.len();
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }

    pub fn len(&self) -> usize {
        self.buf.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Char index → byte index. Everything else goes through this so multi-byte
    /// input can never panic on a slice boundary.
    fn byte_at(&self, char_idx: usize) -> usize {
        self.buf
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.buf.len())
    }
}

pub struct App {
    pub target: String,
    pub focus: Pane,
    pub exchanges: Vec<Exchange>,
    pub console: Vec<ConsoleLine>,
    pub navigations: Vec<Navigation>,

    /// Which method tab is active; `TAB_ALL` for no filter.
    pub tab: usize,
    pub selected: usize,
    pub console_scroll: usize,
    pub chat_scroll: usize,
    /// Row-level detail modal.
    pub detail_open: bool,
    pub detail_scroll: usize,

    pub chat: Vec<ChatMsg>,
    pub input: Input,
    pub thinking: bool,
    pub spend_usd: f64,
    pub status: String,
    pub should_quit: bool,
}

impl App {
    pub fn new(target: String) -> Self {
        Self {
            target,
            focus: Pane::Network,
            exchanges: Vec::new(),
            console: Vec::new(),
            navigations: Vec::new(),
            tab: TAB_ALL,
            selected: 0,
            console_scroll: 0,
            chat_scroll: 0,
            detail_open: false,
            detail_scroll: 0,
            chat: Vec::new(),
            input: Input::default(),
            thinking: false,
            spend_usd: 0.0,
            status: String::new(),
            should_quit: false,
        }
    }

    /// Indices into `exchanges` that pass the current method filter.
    pub fn visible(&self) -> Vec<usize> {
        self.exchanges
            .iter()
            .enumerate()
            .filter(|(_, e)| self.tab == TAB_ALL || e.method_bucket() == METHOD_TABS[self.tab])
            .map(|(i, _)| i)
            .collect()
    }

    pub fn selected_exchange(&self) -> Option<&Exchange> {
        let v = self.visible();
        v.get(self.selected).and_then(|i| self.exchanges.get(*i))
    }

    pub fn select_next(&mut self) {
        let n = self.visible().len();
        if n > 0 {
            self.selected = (self.selected + 1).min(n - 1);
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Cycle the method tabs, wrapping through "all".
    pub fn cycle_tab(&mut self, forward: bool) {
        self.tab = match (self.tab, forward) {
            (TAB_ALL, true) => 0,
            (TAB_ALL, false) => METHOD_TABS.len() - 1,
            (i, true) if i + 1 >= METHOD_TABS.len() => TAB_ALL,
            (i, true) => i + 1,
            (0, false) => TAB_ALL,
            (i, false) => i - 1,
        };
        self.selected = 0;
    }

    pub fn set_tab(&mut self, idx: usize) {
        self.tab = if self.tab == idx { TAB_ALL } else { idx };
        self.selected = 0;
    }

    pub fn say(&mut self, role: ChatRole, text: impl Into<String>) {
        self.chat.push(ChatMsg {
            role,
            text: text.into(),
        });
        // stick to the bottom as new messages land
        self.chat_scroll = usize::MAX;
    }

    pub fn counters(&self) -> (usize, usize, usize) {
        let failed = self.exchanges.iter().filter(|e| e.is_error()).count();
        let errs = self.console.iter().filter(|c| c.severity == "error").count();
        (self.exchanges.len(), failed, errs)
    }

    pub fn total_bytes(&self) -> u64 {
        self.exchanges.iter().map(|e| e.size).sum()
    }
}

/// Human-readable byte size, right-aligned friendly.
pub fn human_size(n: u64) -> String {
    const K: f64 = 1024.0;
    let n = n as f64;
    if n < K {
        format!("{n:.0} B")
    } else if n < K * K {
        format!("{:.1} kB", n / K)
    } else if n < K * K * K {
        format!("{:.1} MB", n / (K * K))
    } else {
        format!("{:.1} GB", n / (K * K * K))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(method: &str, status: i64) -> Exchange {
        Exchange {
            method: method.into(),
            url: format!("http://x/{}", method.to_lowercase()),
            status: Some(status),
            ..Default::default()
        }
    }

    #[test]
    fn tab_filter_selects_the_right_rows() {
        let mut app = App::new("t".into());
        app.exchanges = vec![
            ex("GET", 200),
            ex("POST", 500),
            ex("PATCH", 200),
            ex("HEAD", 200),
        ];
        assert_eq!(app.visible().len(), 4, "no filter shows everything");

        app.set_tab(1); // POST
        assert_eq!(app.visible(), vec![1]);
        assert_eq!(app.selected_exchange().unwrap().method, "POST");

        app.set_tab(4); // OTHER — HEAD lands here
        assert_eq!(app.visible(), vec![3]);

        app.set_tab(4); // toggling the same tab clears the filter
        assert_eq!(app.tab, TAB_ALL);
        assert_eq!(app.visible().len(), 4);
    }

    #[test]
    fn selection_is_clamped_to_the_filtered_list() {
        let mut app = App::new("t".into());
        app.exchanges = vec![ex("GET", 200), ex("GET", 200), ex("POST", 200)];
        for _ in 0..10 {
            app.select_next();
        }
        assert_eq!(app.selected, 2, "cannot run off the end");
        app.set_tab(1); // POST — one row
        assert_eq!(app.selected, 0, "filter resets selection");
        app.select_next();
        assert_eq!(app.selected, 0, "single row stays put");
        app.select_prev();
        assert_eq!(app.selected, 0, "cannot go negative");
    }

    #[test]
    fn empty_list_never_panics() {
        let mut app = App::new("t".into());
        app.select_next();
        app.select_prev();
        assert!(app.selected_exchange().is_none());
    }

    #[test]
    fn tabs_cycle_through_all() {
        let mut app = App::new("t".into());
        assert_eq!(app.tab, TAB_ALL);
        app.cycle_tab(true);
        assert_eq!(app.tab, 0);
        for _ in 0..4 {
            app.cycle_tab(true);
        }
        assert_eq!(app.tab, 4);
        app.cycle_tab(true);
        assert_eq!(app.tab, TAB_ALL, "wraps back to unfiltered");
        app.cycle_tab(false);
        assert_eq!(app.tab, 4);
    }

    #[test]
    fn pane_focus_cycles_both_ways() {
        let mut p = Pane::Overview;
        for _ in 0..4 {
            p = p.next();
        }
        assert_eq!(p, Pane::Overview, "four panes, full cycle");
        assert_eq!(Pane::Overview.prev(), Pane::Chat);
    }

    #[test]
    fn input_handles_multibyte_without_panicking() {
        let mut i = Input::default();
        for c in "héllo→".chars() {
            i.insert(c);
        }
        assert_eq!(i.len(), 6);
        i.backspace();
        assert_eq!(i.buf, "héllo");
        i.home();
        i.delete();
        assert_eq!(i.buf, "éllo");
        i.end();
        i.insert('!');
        assert_eq!(i.buf, "éllo!");
        assert_eq!(i.take(), "éllo!");
        assert!(i.is_empty());
        assert_eq!(i.cursor, 0);
    }

    #[test]
    fn input_cursor_cannot_escape_the_buffer() {
        let mut i = Input::default();
        i.left();
        i.backspace();
        i.delete();
        assert!(i.buf.is_empty());
        i.insert('a');
        i.right();
        i.right();
        assert_eq!(i.cursor, 1);
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 kB");
        assert_eq!(human_size(12_279_560), "11.7 MB");
    }
}
