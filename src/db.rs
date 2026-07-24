//! SQLite persistence — the session memory.
//!
//! Everything the agent can ever know lives here. It is the only thing the agent
//! reads, which is what makes the guardrail enforceable rather than aspirational.
//!
//! Runs on its own blocking task fed by a bounded channel (ADR-0002): the capture
//! path measured 554 requests from a single page load, so writes are batched in a
//! transaction rather than committed one at a time.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS sessions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at  TEXT NOT NULL,
    ended_at    TEXT,
    target      TEXT NOT NULL,
    label       TEXT
);

CREATE TABLE IF NOT EXISTS requests (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id    INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    request_id    TEXT NOT NULL,
    ts            TEXT NOT NULL,
    method        TEXT NOT NULL,
    url           TEXT NOT NULL,
    resource_type TEXT,
    req_headers   TEXT NOT NULL DEFAULT '{}',
    req_body      TEXT,
    status        INTEGER,
    status_text   TEXT,
    res_headers   TEXT,
    res_body      BLOB,
    res_body_b64  INTEGER NOT NULL DEFAULT 0,
    truncated_from INTEGER,
    mime_type     TEXT,
    size          INTEGER NOT NULL DEFAULT 0,
    duration_ms   REAL NOT NULL DEFAULT 0,
    from_cache    INTEGER NOT NULL DEFAULT 0,
    remote_ip     TEXT,
    error         TEXT,
    page_url      TEXT,
    UNIQUE(session_id, request_id)
);
CREATE INDEX IF NOT EXISTS idx_requests_session ON requests(session_id);
CREATE INDEX IF NOT EXISTS idx_requests_status  ON requests(session_id, status);
CREATE INDEX IF NOT EXISTS idx_requests_method  ON requests(session_id, method);
CREATE INDEX IF NOT EXISTS idx_requests_page    ON requests(session_id, page_url);

CREATE TABLE IF NOT EXISTS console (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ts         TEXT NOT NULL,
    severity   TEXT NOT NULL,
    text       TEXT NOT NULL,
    url        TEXT,
    line       INTEGER,
    source     TEXT,
    page_url   TEXT
);
CREATE INDEX IF NOT EXISTS idx_console_session ON console(session_id);

CREATE TABLE IF NOT EXISTS navigations (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ts         TEXT NOT NULL,
    url        TEXT NOT NULL,
    is_main    INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_nav_session ON navigations(session_id);

CREATE TABLE IF NOT EXISTS dom_snapshots (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ts         TEXT NOT NULL,
    url        TEXT NOT NULL,
    html       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS annotations (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ts         TEXT NOT NULL,
    request_id TEXT,
    note       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS chat (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ts         TEXT NOT NULL,
    role       TEXT NOT NULL,
    text       TEXT NOT NULL,
    cost_usd   REAL NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_chat_session ON chat(session_id);
"#;

pub type Headers = BTreeMap<String, String>;

/// Additive migrations for databases written by an older networkcop.
///
/// `CREATE TABLE IF NOT EXISTS` will not add a column to a table that already
/// exists, so a session recorded before page tracking would fail every query
/// mentioning `page_url`. Adding it is safe and idempotent.
fn migrate(conn: &Connection) -> Result<()> {
    for table in ["requests", "console"] {
        let mut have: Vec<String> = Vec::new();
        {
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
            for c in rows {
                have.push(c?);
            }
        }
        if !have.is_empty() && !have.iter().any(|c| c == "page_url") {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN page_url TEXT"))?;
        }
    }
    Ok(())
}

/// One captured exchange, as the TUI and agent see it.
#[derive(Debug, Clone, Default)]
pub struct Exchange {
    pub id: i64,
    pub request_id: String,
    pub ts: String,
    pub method: String,
    pub url: String,
    pub resource_type: Option<String>,
    pub req_headers: Headers,
    pub req_body: Option<String>,
    pub status: Option<i64>,
    pub status_text: Option<String>,
    pub res_headers: Headers,
    pub res_body: Option<Vec<u8>>,
    pub res_body_b64: bool,
    pub truncated_from: Option<u64>,
    pub mime_type: Option<String>,
    pub size: u64,
    pub duration_ms: f64,
    pub from_cache: bool,
    pub error: Option<String>,
    /// Main-frame URL in effect when this request was issued — the page it
    /// belongs to. `None` for anything captured before the first navigation.
    pub page_url: Option<String>,
}

impl Exchange {
    pub fn host(&self) -> &str {
        self.url
            .split("://")
            .nth(1)
            .and_then(|r| r.split('/').next())
            .unwrap_or("")
    }

    /// Path without query — what the request list and the OpenAPI export show.
    pub fn path(&self) -> String {
        let after = match self.url.split_once("://") {
            Some((_, r)) => r,
            None => &self.url,
        };
        match after.find('/') {
            Some(i) => after[i..].split('?').next().unwrap_or("/").to_string(),
            None => "/".into(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.error.is_some() || self.status.map(|s| s >= 400).unwrap_or(false)
    }

    /// Body as text when it plausibly is text; `None` for binary.
    pub fn body_text(&self) -> Option<String> {
        let raw = self.res_body.as_ref()?;
        if self.res_body_b64 {
            return None;
        }
        String::from_utf8(raw.clone()).ok()
    }

    pub fn method_bucket(&self) -> &'static str {
        match self.method.to_ascii_uppercase().as_str() {
            "GET" => "GET",
            "POST" => "POST",
            "PATCH" => "PATCH",
            "DELETE" => "DELETE",
            _ => "OTHER",
        }
    }

    fn resource_is(&self, want: &[&str]) -> bool {
        self.resource_type
            .as_deref()
            .map(|t| want.iter().any(|w| t.eq_ignore_ascii_case(w)))
            .unwrap_or(false)
    }

    fn mime(&self) -> &str {
        self.mime_type.as_deref().unwrap_or("")
    }

    /// Issued by page script at runtime — `fetch()` or `XMLHttpRequest`.
    ///
    /// This answers "what did the JavaScript actually ask for", which includes
    /// analytics beacons and telemetry that are not REST at all.
    pub fn is_ajax(&self) -> bool {
        self.resource_is(&["XHR", "Fetch"])
    }

    /// Looks like an API endpoint.
    ///
    /// Deliberately distinct from [`is_ajax`]: this answers "which endpoints were
    /// hit", and catches API calls made outside XHR (a service worker, a redirect,
    /// a document-level JSON GET) while excluding an XHR that fetched a template.
    /// The two overlap heavily but neither contains the other.
    pub fn is_rest(&self) -> bool {
        if self.is_static() || self.is_document() {
            return false;
        }
        let path = self.path();
        let looks_like_api = path.contains("/api/")
            || path.starts_with("/api")
            || path.contains("/graphql")
            || path.contains("/rest/")
            // /v1/… /v2/… version-prefixed endpoints
            || path.split('/').any(|seg| {
                let mut c = seg.chars();
                c.next() == Some('v')
                    && c.clone().next().is_some_and(|d| d.is_ascii_digit())
                    && c.all(|d| d.is_ascii_digit())
            });
        let json_shaped = self.mime().contains("json");
        looks_like_api || (json_shaped && self.is_ajax())
    }

    /// A top-level page load.
    pub fn is_document(&self) -> bool {
        self.resource_is(&["Document"]) || self.mime().starts_with("text/html")
    }

    /// Path of the page this request belongs to, for grouping and filtering.
    pub fn page_path(&self) -> String {
        match &self.page_url {
            Some(u) => path_of(u),
            None => "(before first navigation)".into(),
        }
    }

    /// Scripts, styles, images, fonts, media — the bulk of a dev-server page load.
    pub fn is_static(&self) -> bool {
        if self.resource_is(&["Script", "Stylesheet", "Image", "Font", "Media"]) {
            return true;
        }
        let m = self.mime();
        m.starts_with("image/")
            || m.starts_with("font/")
            || m.starts_with("video/")
            || m.starts_with("audio/")
            || m.contains("javascript")
            || m.contains("css")
    }
}

/// `http://host/a/b?c` → `/a/b`. Shared by page grouping and the request list.
pub fn path_of(url: &str) -> String {
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

#[derive(Debug, Clone)]
pub struct ConsoleLine {
    pub ts: String,
    pub severity: String,
    pub text: String,
    pub url: Option<String>,
    pub line: Option<i64>,
    pub source: String,
    /// Page in effect when this was logged, so a page filter narrows the console
    /// in step with the request list.
    pub page_url: Option<String>,
}

impl ConsoleLine {
    pub fn page_path(&self) -> String {
        match &self.page_url {
            Some(u) => path_of(u),
            None => "(before first navigation)".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Navigation {
    pub ts: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: i64,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub target: String,
    pub requests: i64,
    pub errors: i64,
    pub console_errors: i64,
}

/// Writes that the capture pipeline hands to the writer task.
#[derive(Debug)]
pub enum Write {
    Request {
        request_id: String,
        ts: DateTime<Utc>,
        method: String,
        url: String,
        resource_type: Option<String>,
        headers: Headers,
        body: Option<String>,
        page_url: Option<String>,
    },
    Response {
        request_id: String,
        status: i64,
        status_text: String,
        headers: Headers,
        mime_type: String,
        remote_ip: Option<String>,
        from_cache: bool,
        duration_ms: f64,
    },
    Body {
        request_id: String,
        body: Vec<u8>,
        base64: bool,
        truncated_from: Option<u64>,
    },
    Failed {
        request_id: String,
        error: String,
    },
    Console(ConsoleLine),
    Navigation {
        ts: DateTime<Utc>,
        url: String,
        is_main: bool,
    },
    DomSnapshot {
        ts: DateTime<Utc>,
        url: String,
        html: String,
    },
    Annotation {
        ts: DateTime<Utc>,
        request_id: Option<String>,
        note: String,
    },
    Chat {
        ts: DateTime<Utc>,
        role: String,
        text: String,
        cost_usd: f64,
    },
}

pub struct Db {
    conn: Connection,
    pub session_id: i64,
}

impl Db {
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".networkcop")
            .join("sessions.db")
    }

    /// Open (creating parents) and start a new session row.
    pub fn open(path: &Path, target: &str) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch(SCHEMA).context("apply schema")?;
        migrate(&conn)?;
        conn.execute(
            "INSERT INTO sessions (started_at, target) VALUES (?1, ?2)",
            params![Utc::now().to_rfc3339(), target],
        )?;
        let session_id = conn.last_insert_rowid();
        Ok(Self { conn, session_id })
    }

    /// Open read-only-ish against an existing session (for `sessions`/`ask` subcommands).
    pub fn attach(path: &Path, session_id: Option<i64>) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        let session_id = match session_id {
            Some(id) => id,
            None => conn
                .query_row("SELECT MAX(id) FROM sessions", [], |r| {
                    r.get::<_, Option<i64>>(0)
                })
                .optional()?
                .flatten()
                .context("no sessions recorded yet")?,
        };
        Ok(Self { conn, session_id })
    }

    /// The URL this session was recorded against. `--ask` must use this rather
    /// than whatever the current CLI flags imply, or exports get mislabelled.
    pub fn target(&self, session_id: i64) -> Result<String> {
        Ok(self
            .conn
            .query_row(
                "SELECT target FROM sessions WHERE id = ?1",
                [session_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_default())
    }

    pub fn finish(&self) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), self.session_id],
        )?;
        Ok(())
    }

    /// Apply a batch inside one transaction — the hot path.
    pub fn apply(&mut self, batch: &[Write]) -> Result<()> {
        let sid = self.session_id;
        let tx = self.conn.transaction()?;
        for w in batch {
            match w {
                Write::Request {
                    request_id,
                    ts,
                    method,
                    url,
                    resource_type,
                    headers,
                    body,
                    page_url,
                } => {
                    tx.execute(
                        "INSERT OR IGNORE INTO requests
                           (session_id, request_id, ts, method, url, resource_type, req_headers, req_body, page_url)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                        params![
                            sid,
                            request_id,
                            ts.to_rfc3339(),
                            method,
                            url,
                            resource_type,
                            serde_json::to_string(headers)?,
                            body,
                            page_url
                        ],
                    )?;
                }
                Write::Response {
                    request_id,
                    status,
                    status_text,
                    headers,
                    mime_type,
                    remote_ip,
                    from_cache,
                    duration_ms,
                } => {
                    tx.execute(
                        "UPDATE requests SET status=?1, status_text=?2, res_headers=?3,
                            mime_type=?4, remote_ip=?5, from_cache=?6, duration_ms=?7
                         WHERE session_id=?8 AND request_id=?9",
                        params![
                            status,
                            status_text,
                            serde_json::to_string(headers)?,
                            mime_type,
                            remote_ip,
                            *from_cache as i64,
                            duration_ms,
                            sid,
                            request_id
                        ],
                    )?;
                }
                Write::Body {
                    request_id,
                    body,
                    base64,
                    truncated_from,
                } => {
                    // rusqlite has no ToSql for u64 — narrow at the boundary
                    let trunc: Option<i64> = truncated_from.map(|v| v as i64);
                    let size = trunc.unwrap_or(body.len() as i64);
                    tx.execute(
                        "UPDATE requests SET res_body=?1, res_body_b64=?2, truncated_from=?3, size=?4
                         WHERE session_id=?5 AND request_id=?6",
                        params![body, *base64 as i64, trunc, size, sid, request_id],
                    )?;
                }
                Write::Failed { request_id, error } => {
                    tx.execute(
                        "UPDATE requests SET error=?1 WHERE session_id=?2 AND request_id=?3",
                        params![error, sid, request_id],
                    )?;
                }
                Write::Console(c) => {
                    tx.execute(
                        "INSERT INTO console (session_id, ts, severity, text, url, line, source, page_url)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                        params![
                            sid, c.ts, c.severity, c.text, c.url, c.line, c.source, c.page_url
                        ],
                    )?;
                }
                Write::Navigation { ts, url, is_main } => {
                    tx.execute(
                        "INSERT INTO navigations (session_id, ts, url, is_main) VALUES (?1,?2,?3,?4)",
                        params![sid, ts.to_rfc3339(), url, *is_main as i64],
                    )?;
                }
                Write::DomSnapshot { ts, url, html } => {
                    tx.execute(
                        "INSERT INTO dom_snapshots (session_id, ts, url, html) VALUES (?1,?2,?3,?4)",
                        params![sid, ts.to_rfc3339(), url, html],
                    )?;
                }
                Write::Annotation {
                    ts,
                    request_id,
                    note,
                } => {
                    tx.execute(
                        "INSERT INTO annotations (session_id, ts, request_id, note) VALUES (?1,?2,?3,?4)",
                        params![sid, ts.to_rfc3339(), request_id, note],
                    )?;
                }
                Write::Chat {
                    ts,
                    role,
                    text,
                    cost_usd,
                } => {
                    tx.execute(
                        "INSERT INTO chat (session_id, ts, role, text, cost_usd) VALUES (?1,?2,?3,?4,?5)",
                        params![sid, ts.to_rfc3339(), role, text, cost_usd],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn exchanges(&self, session_id: i64) -> Result<Vec<Exchange>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, request_id, ts, method, url, resource_type, req_headers, req_body,
                    status, status_text, res_headers, res_body, res_body_b64, truncated_from,
                    mime_type, size, duration_ms, from_cache, error, page_url
             FROM requests WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([session_id], |r| {
            let req_headers: String = r.get(6)?;
            let res_headers: Option<String> = r.get(10)?;
            Ok(Exchange {
                id: r.get(0)?,
                request_id: r.get(1)?,
                ts: r.get(2)?,
                method: r.get(3)?,
                url: r.get(4)?,
                resource_type: r.get(5)?,
                req_headers: serde_json::from_str(&req_headers).unwrap_or_default(),
                req_body: r.get(7)?,
                status: r.get(8)?,
                status_text: r.get(9)?,
                res_headers: res_headers
                    .and_then(|h| serde_json::from_str(&h).ok())
                    .unwrap_or_default(),
                res_body: r.get(11)?,
                res_body_b64: r.get::<_, i64>(12)? != 0,
                truncated_from: r.get::<_, Option<i64>>(13)?.map(|v| v as u64),
                mime_type: r.get(14)?,
                size: r.get::<_, i64>(15)?.max(0) as u64,
                duration_ms: r.get(16)?,
                from_cache: r.get::<_, i64>(17)? != 0,
                error: r.get(18)?,
                page_url: r.get(19)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn console(&self, session_id: i64) -> Result<Vec<ConsoleLine>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, severity, text, url, line, source, page_url FROM console
             WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([session_id], |r| {
            Ok(ConsoleLine {
                ts: r.get(0)?,
                severity: r.get(1)?,
                text: r.get(2)?,
                url: r.get(3)?,
                line: r.get(4)?,
                source: r.get(5)?,
                page_url: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn navigations(&self, session_id: i64) -> Result<Vec<Navigation>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, url FROM navigations WHERE session_id = ?1 AND is_main = 1 ORDER BY id",
        )?;
        let rows = stmt.query_map([session_id], |r| {
            Ok(Navigation {
                ts: r.get(0)?,
                url: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Prior chat turns, so the agent pane survives a restart.
    pub fn chat_history(&self, session_id: i64) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT role, text FROM chat WHERE session_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map([session_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Session index — this is what `networkcop sessions` prints, and it exists
    /// because the `sqlite3` CLI is not assumed to be installed (ADR-0002).
    pub fn sessions(&self) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.started_at, s.ended_at, s.target,
                    (SELECT COUNT(*) FROM requests r WHERE r.session_id = s.id),
                    (SELECT COUNT(*) FROM requests r WHERE r.session_id = s.id
                       AND (r.status >= 400 OR r.error IS NOT NULL)),
                    (SELECT COUNT(*) FROM console c WHERE c.session_id = s.id
                       AND c.severity = 'error')
             FROM sessions s ORDER BY s.id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SessionRow {
                id: r.get(0)?,
                started_at: r.get(1)?,
                ended_at: r.get(2)?,
                target: r.get(3)?,
                requests: r.get(4)?,
                errors: r.get(5)?,
                console_errors: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!(
            "networkcop-test-{}-{}.db",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn round_trips_a_request_response_and_body() {
        let path = tmp();
        let mut db = Db::open(&path, "http://localhost:8080").unwrap();
        let mut h = Headers::new();
        h.insert("content-type".into(), "application/json".into());

        db.apply(&[
            Write::Request {
                request_id: "R1".into(),
                ts: Utc::now(),
                method: "POST".into(),
                url: "http://localhost:8080/api/cart/checkout?a=1".into(),
                resource_type: Some("XHR".into()),
                headers: h.clone(),
                body: Some(r#"{"qty":0}"#.into()),
                page_url: Some("http://localhost:8080/cart".into()),
            },
            Write::Response {
                request_id: "R1".into(),
                status: 500,
                status_text: "Internal Server Error".into(),
                headers: h,
                mime_type: "application/json".into(),
                remote_ip: None,
                from_cache: false,
                duration_ms: 2100.0,
            },
            Write::Body {
                request_id: "R1".into(),
                body: br#"{"error":"empty_line_item"}"#.to_vec(),
                base64: false,
                truncated_from: None,
            },
        ])
        .unwrap();

        let ex = db.exchanges(db.session_id).unwrap();
        assert_eq!(ex.len(), 1);
        let e = &ex[0];
        assert_eq!(e.method, "POST");
        assert_eq!(e.status, Some(500));
        assert!(e.is_error());
        assert_eq!(e.path(), "/api/cart/checkout", "query must be stripped");
        assert_eq!(e.host(), "localhost:8080");
        assert_eq!(e.method_bucket(), "POST");
        assert_eq!(e.body_text().unwrap(), r#"{"error":"empty_line_item"}"#);
        assert_eq!(e.size, 27);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn survives_reopen_and_indexes_sessions() {
        let path = tmp();
        {
            let mut db = Db::open(&path, "http://localhost:3000").unwrap();
            db.apply(&[Write::Console(ConsoleLine {
                ts: Utc::now().to_rfc3339(),
                severity: "error".into(),
                text: "TypeError: t.total is undefined".into(),
                url: None,
                line: None,
                source: "exception".into(),
                page_url: None,
            })])
            .unwrap();
            db.finish().unwrap();
        }
        // reopen — memory must survive the process
        let db = Db::attach(&path, None).unwrap();
        let lines = db.console(db.session_id).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].severity, "error");

        let sessions = db.sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].console_errors, 1);
        assert!(sessions[0].ended_at.is_some());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn truncated_body_keeps_true_size() {
        let path = tmp();
        let mut db = Db::open(&path, "t").unwrap();
        db.apply(&[
            Write::Request {
                request_id: "R2".into(),
                ts: Utc::now(),
                method: "GET".into(),
                url: "http://x/video.mp4".into(),
                resource_type: None,
                headers: Headers::new(),
                body: None,
                page_url: None,
            },
            Write::Body {
                request_id: "R2".into(),
                body: vec![b'x'; 128],
                base64: true,
                truncated_from: Some(12_279_560),
            },
        ])
        .unwrap();
        let e = &db.exchanges(db.session_id).unwrap()[0];
        assert_eq!(e.size, 12_279_560, "reports real size, not stored size");
        assert_eq!(e.truncated_from, Some(12_279_560));
        assert!(e.body_text().is_none(), "base64 bodies are not text");
        let _ = std::fs::remove_file(&path);
    }
}
