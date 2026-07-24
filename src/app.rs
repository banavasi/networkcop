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

/// What kind of traffic a row is. Filters on this axis are independent of method
/// and domain — all three AND together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    All,
    /// Issued by page script: fetch/XHR. Includes analytics beacons.
    Ajax,
    /// Looks like an API endpoint, however it was issued.
    Rest,
    /// Top-level page loads.
    Doc,
    /// Scripts, styles, images, fonts, media.
    Static,
}

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::All, Kind::Ajax, Kind::Rest, Kind::Doc, Kind::Static];

    pub fn label(self) -> &'static str {
        match self {
            Kind::All => "ALL",
            Kind::Ajax => "AJAX",
            Kind::Rest => "REST",
            Kind::Doc => "DOC",
            Kind::Static => "STATIC",
        }
    }

    pub fn matches(self, e: &Exchange) -> bool {
        match self {
            Kind::All => true,
            Kind::Ajax => e.is_ajax(),
            Kind::Rest => e.is_rest(),
            Kind::Doc => e.is_document(),
            Kind::Static => e.is_static(),
        }
    }

    pub fn next(self) -> Self {
        let i = Kind::ALL.iter().position(|k| *k == self).unwrap_or(0);
        Kind::ALL[(i + 1) % Kind::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let i = Kind::ALL.iter().position(|k| *k == self).unwrap_or(0);
        Kind::ALL[(i + Kind::ALL.len() - 1) % Kind::ALL.len()]
    }
}

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
    /// Traffic-kind filter.
    pub kind: Kind,
    /// Host filter; `None` shows every domain.
    pub domain: Option<String>,
    /// Domain picker modal.
    pub domain_picker: bool,
    pub domain_cursor: usize,
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
            kind: Kind::All,
            domain: None,
            domain_picker: false,
            domain_cursor: 0,
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

    /// Indices into `exchanges` passing every active filter. Method, kind and
    /// domain are independent axes and AND together.
    pub fn visible(&self) -> Vec<usize> {
        self.exchanges
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                (self.tab == TAB_ALL || e.method_bucket() == METHOD_TABS[self.tab])
                    && self.kind.matches(e)
                    && self
                        .domain
                        .as_deref()
                        .map(|d| e.host() == d)
                        .unwrap_or(true)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Hosts seen this session, busiest first. Drives the domain picker.
    ///
    /// Counts are unfiltered on purpose: the picker should show what is available
    /// to switch to, not what survives the filters already applied.
    pub fn domains(&self) -> Vec<(String, usize)> {
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for e in &self.exchanges {
            let h = e.host();
            if !h.is_empty() {
                *counts.entry(h).or_default() += 1;
            }
        }
        let mut v: Vec<(String, usize)> = counts
            .into_iter()
            .map(|(h, n)| (h.to_string(), n))
            .collect();
        // busiest first, then alphabetical so the order is stable between draws
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    pub fn cycle_kind(&mut self, forward: bool) {
        self.kind = if forward {
            self.kind.next()
        } else {
            self.kind.prev()
        };
        self.selected = 0;
    }

    pub fn set_kind(&mut self, k: Kind) {
        // clicking the active kind clears it, like the method tabs
        self.kind = if self.kind == k { Kind::All } else { k };
        self.selected = 0;
    }

    pub fn set_domain(&mut self, d: Option<String>) {
        self.domain = d;
        self.selected = 0;
    }

    /// Everything back to unfiltered — what `esc` does.
    pub fn clear_filters(&mut self) {
        self.tab = TAB_ALL;
        self.kind = Kind::All;
        self.domain = None;
        self.selected = 0;
    }

    pub fn filters_active(&self) -> bool {
        self.tab != TAB_ALL || self.kind != Kind::All || self.domain.is_some()
    }

    /// Compact description of the active filters, for the pane title.
    pub fn filter_label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.tab != TAB_ALL {
            parts.push(METHOD_TABS[self.tab].to_string());
        }
        if self.kind != Kind::All {
            parts.push(self.kind.label().to_string());
        }
        if let Some(d) = &self.domain {
            parts.push(d.clone());
        }
        parts.join(" · ")
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

    fn full(method: &str, url: &str, rtype: &str, mime: &str) -> Exchange {
        Exchange {
            method: method.into(),
            url: url.into(),
            status: Some(200),
            resource_type: Some(rtype.into()),
            mime_type: Some(mime.into()),
            ..Default::default()
        }
    }

    #[test]
    fn ajax_and_rest_are_different_questions() {
        // an analytics beacon: AJAX (page script issued it) but not REST
        let beacon = full("POST", "https://a.co/g/collect?v=2", "XHR", "text/plain");
        assert!(beacon.is_ajax());
        assert!(!beacon.is_rest(), "a beacon is not an API endpoint");

        // a service-worker API call: REST but not XHR/Fetch
        let sw = full("GET", "https://x.co/api/v1/me", "Other", "application/json");
        assert!(!sw.is_ajax());
        assert!(sw.is_rest(), "API shape counts however it was issued");

        // the common case: both
        let both = full("POST", "http://localhost:8080/api/cart", "Fetch", "application/json");
        assert!(both.is_ajax() && both.is_rest());

        // an XHR that pulled a template is neither REST nor static
        let tpl = full("GET", "http://localhost:8080/views/cart.html", "XHR", "text/html");
        assert!(tpl.is_ajax());
        assert!(!tpl.is_rest());
    }

    #[test]
    fn static_and_document_are_excluded_from_rest() {
        assert!(full("GET", "http://x/main.js", "Script", "text/javascript").is_static());
        assert!(full("GET", "http://x/logo.png", "Image", "image/png").is_static());
        assert!(full("GET", "http://x/a.css", "Stylesheet", "text/css").is_static());
        assert!(full("GET", "http://x/", "Document", "text/html").is_document());
        // a JS file served from an /api/ path must not count as REST
        let odd = full("GET", "http://x/api/widget.js", "Script", "text/javascript");
        assert!(!odd.is_rest(), "static wins over path shape");
    }

    #[test]
    fn version_prefixed_paths_count_as_rest() {
        assert!(full("GET", "http://x/v1/users", "Other", "application/json").is_rest());
        assert!(full("GET", "http://x/v22/users", "Other", "application/json").is_rest());
        // not a version segment
        assert!(!full("GET", "http://x/video/clip", "Other", "text/plain").is_rest());
        assert!(!full("GET", "http://x/vendor/lib", "Other", "text/plain").is_rest());
    }

    #[test]
    fn the_three_filter_axes_and_together() {
        let mut app = App::new("t".into());
        app.exchanges = vec![
            full("GET", "http://localhost:8080/api/me", "XHR", "application/json"),
            full("POST", "http://localhost:8080/api/cart", "XHR", "application/json"),
            full("POST", "https://analytics.co/collect", "XHR", "text/plain"),
            full("GET", "http://localhost:8080/main.js", "Script", "text/javascript"),
        ];
        assert_eq!(app.visible().len(), 4);

        app.set_kind(Kind::Rest);
        assert_eq!(app.visible().len(), 2, "beacon and script drop out");

        app.set_domain(Some("localhost:8080".into()));
        assert_eq!(app.visible().len(), 2);

        app.set_tab(1); // POST
        assert_eq!(app.visible(), vec![1], "method ∧ kind ∧ domain");

        app.set_domain(Some("analytics.co".into()));
        assert!(app.visible().is_empty(), "no POST+REST on that host");

        app.clear_filters();
        assert_eq!(app.visible().len(), 4);
        assert!(!app.filters_active());
    }

    #[test]
    fn domains_are_listed_busiest_first_and_stably() {
        let mut app = App::new("t".into());
        app.exchanges = vec![
            full("GET", "http://localhost:8080/a", "XHR", "application/json"),
            full("GET", "http://localhost:8080/b", "XHR", "application/json"),
            full("GET", "https://zzz.co/x", "XHR", "application/json"),
            full("GET", "https://aaa.co/x", "XHR", "application/json"),
        ];
        let d = app.domains();
        assert_eq!(d[0], ("localhost:8080".to_string(), 2));
        // ties break alphabetically, so the picker does not reshuffle between draws
        assert_eq!(d[1].0, "aaa.co");
        assert_eq!(d[2].0, "zzz.co");
    }

    #[test]
    fn domain_counts_ignore_other_filters() {
        let mut app = App::new("t".into());
        app.exchanges = vec![
            full("GET", "http://localhost:8080/main.js", "Script", "text/javascript"),
            full("GET", "https://cdn.co/lib.js", "Script", "text/javascript"),
        ];
        app.set_kind(Kind::Rest); // hides everything
        assert!(app.visible().is_empty());
        assert_eq!(app.domains().len(), 2, "picker still offers both hosts");
    }

    #[test]
    fn kind_cycles_both_ways_and_toggles_off() {
        let mut app = App::new("t".into());
        assert_eq!(app.kind, Kind::All);
        app.cycle_kind(true);
        assert_eq!(app.kind, Kind::Ajax);
        app.cycle_kind(false);
        assert_eq!(app.kind, Kind::All);
        app.cycle_kind(false);
        assert_eq!(app.kind, Kind::Static, "wraps backwards");

        app.set_kind(Kind::Rest);
        assert_eq!(app.kind, Kind::Rest);
        app.set_kind(Kind::Rest);
        assert_eq!(app.kind, Kind::All, "clicking the active kind clears it");
    }

    #[test]
    fn filter_label_reads_naturally() {
        let mut app = App::new("t".into());
        assert_eq!(app.filter_label(), "");
        app.set_tab(1);
        app.set_kind(Kind::Rest);
        app.set_domain(Some("api.x.co".into()));
        assert_eq!(app.filter_label(), "POST · REST · api.x.co");
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 kB");
        assert_eq!(human_size(12_279_560), "11.7 MB");
    }
}
