//! The agent harness: Observe → Reason → Act → Persist.
//!
//! `Observe`  read the session out of SQLite and build the digest
//! `Reason`   classify the input; only free-form questions reach the model
//! `Act`      run the deterministic tool, or validate the model's envelope
//! `Persist`  write the turn back to the session so it survives a restart
//!
//! Slash commands skip the model for their *artifact* and use it only for prose.

pub mod llm;
pub mod prompt;
pub mod tools;

use crate::db::{ConsoleLine, Db, Exchange, Navigation, Write as DbWrite};
use anyhow::Result;
use chrono::Utc;
use llm::Backend;
use prompt::{fix_prompt, slugify, REFUSAL};
use std::path::PathBuf;

/// What the user typed, once classified.
#[derive(Debug, PartialEq)]
pub enum Intent {
    Review,
    Report,
    SavePage(String),
    Reproduce(String),
    Export(Option<String>),
    Annotate(String),
    Help,
    Ask(String),
}

/// Parse the chat input. Unknown slash commands are NOT sent to the model —
/// a typo'd command must not become a free-form prompt.
pub fn classify(input: &str) -> Intent {
    let t = input.trim();
    if !t.starts_with('/') {
        return Intent::Ask(t.to_string());
    }
    let (cmd, rest) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
    let rest = rest.trim().to_string();
    match cmd {
        "/review" => Intent::Review,
        "/report" => Intent::Report,
        "/save-page" => Intent::SavePage(if rest.is_empty() { "page".into() } else { rest }),
        "/reproduce" => Intent::Reproduce(rest),
        "/export" | "/openapi" => Intent::Export(if rest.is_empty() { None } else { Some(rest) }),
        "/note" | "/annotate" => Intent::Annotate(rest),
        "/help" | "/?" => Intent::Help,
        _ => Intent::Help,
    }
}

pub struct Session {
    pub target: String,
    pub exchanges: Vec<Exchange>,
    pub console: Vec<ConsoleLine>,
    pub navigations: Vec<Navigation>,
    /// Set when these are a filtered slice rather than the whole session, e.g.
    /// "POST · REST · /checkout". Carried into the digest so the model describes
    /// what it was actually shown instead of asserting session-wide facts, and
    /// into command output so an export says what it covers.
    pub filter: Option<String>,
    /// How many exchanges exist unfiltered — for honest "8 of 561" reporting.
    pub total_exchanges: usize,
}

impl Session {
    pub fn load(db: &Db, session_id: i64, target: &str) -> Result<Self> {
        let exchanges = db.exchanges(session_id)?;
        let total_exchanges = exchanges.len();
        Ok(Self {
            target: target.to_string(),
            exchanges,
            console: db.console(session_id)?,
            navigations: db.navigations(session_id)?,
            filter: None,
            total_exchanges,
        })
    }

    pub fn digest(&self) -> String {
        tools::digest(
            &self.target,
            &self.exchanges,
            &self.console,
            &self.navigations,
            self.filter.as_deref(),
            self.total_exchanges,
        )
    }

    /// " — 8 of 561 requests matching POST · REST · /checkout", appended to
    /// command output so an export never silently covers less than assumed.
    pub fn scope_note(&self) -> String {
        match &self.filter {
            Some(f) => format!(
                " — {} of {} requests matching {f}",
                self.exchanges.len(),
                self.total_exchanges
            ),
            None => String::new(),
        }
    }

    /// Nothing captured yet — every command should say so rather than
    /// confabulate over an empty session.
    pub fn is_empty(&self) -> bool {
        self.exchanges.is_empty() && self.console.is_empty()
    }
}

pub struct AgentReply {
    pub text: String,
    pub cost_usd: f64,
    /// Files this turn wrote, to report back to the user.
    pub wrote: Vec<PathBuf>,
}

impl AgentReply {
    fn msg(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cost_usd: 0.0,
            wrote: Vec::new(),
        }
    }
}

pub const HELP: &str = "networkcop agent — I only know this session.\n\
     Ask anything about the captured requests, responses, bodies, console, or navigations.\n\
     \n\
     /review              analyse the session and emit a ready-to-paste fix prompt\n\
     /report              file the bug to Jira (needs JIRA_BASE_URL + JIRA_API_TOKEN), then print the prompt\n\
     /save-page <name>    export this page and its calls to <name>.yaml\n\
     /reproduce <desc>    minimal curl + Playwright reproduction, plus the fix prompt\n\
     /export [file]       OpenAPI 3.1 collection of the session\n\
     /note <text>         annotate the session\n\
     /help                this list";

/// Run one turn. `out_dir` is where exports land.
pub async fn handle(
    intent: Intent,
    session: &Session,
    backend: &Backend,
    db: &mut Db,
    out_dir: &std::path::Path,
) -> Result<AgentReply> {
    let reply = match intent {
        Intent::Help => AgentReply::msg(HELP),

        Intent::Annotate(note) if note.is_empty() => {
            AgentReply::msg("Usage: /note <what you observed>")
        }
        Intent::Annotate(note) => {
            db.apply(&[DbWrite::Annotation {
                ts: Utc::now(),
                request_id: None,
                note: note.clone(),
            }])?;
            AgentReply::msg(format!("Noted: {note}"))
        }

        Intent::Export(name) => {
            if session.exchanges.is_empty() {
                AgentReply::msg("Nothing captured yet — load a page first.")
            } else {
                let doc = tools::openapi(&session.target, &session.exchanges)?;
                let path = out_dir.join(name.unwrap_or_else(|| "session-openapi.yaml".into()));
                std::fs::write(&path, &doc)?;
                AgentReply {
                    text: format!(
                        "Wrote {} — OpenAPI 3.1, {} operations, examples are real captured payloads.{}",
                        path.display(),
                        tools::interesting(&session.exchanges).len(),
                        session.scope_note()
                    ),
                    cost_usd: 0.0,
                    wrote: vec![path],
                }
            }
        }

        Intent::SavePage(name) => {
            // With a page filter active every exchange belongs to that page, so
            // take the page from the data rather than from "wherever the browser
            // ended up" — otherwise saving a filtered page names the wrong URL.
            let url = session
                .exchanges
                .iter()
                .find_map(|e| e.page_url.clone())
                .or_else(|| session.navigations.last().map(|n| n.url.clone()))
                .unwrap_or_else(|| session.target.clone());
            let doc = tools::save_page(&name, &url, &session.exchanges, &session.console)?;
            let path = out_dir.join(format!("{}.yaml", slugify(&name)));
            std::fs::write(&path, &doc)?;
            AgentReply {
                text: format!(
                    "Wrote {} — page {url} and its calls.{}",
                    path.display(),
                    session.scope_note()
                ),
                cost_usd: 0.0,
                wrote: vec![path],
            }
        }

        Intent::Reproduce(desc) => {
            if session.is_empty() {
                AgentReply::msg("Nothing captured yet — nothing to reproduce.")
            } else {
                let failure = tools::primary_failure(&session.exchanges);
                let (bug, cost) = describe_bug(session, backend, &desc).await;
                let mut out = String::new();
                if let Some(f) = failure {
                    out.push_str("Minimal reproduction (curl):\n\n");
                    out.push_str(&tools::curl_for(f));
                    out.push_str("\n\n");
                }
                out.push_str("Playwright:\n\n");
                out.push_str(&tools::playwright_for(&session.navigations, failure));
                out.push_str("\n\n");
                out.push_str(&fix_prompt(&slugify(&bug_title(&bug, &desc)), &bug));
                AgentReply {
                    text: out,
                    cost_usd: cost,
                    wrote: Vec::new(),
                }
            }
        }

        Intent::Review => {
            if session.is_empty() {
                AgentReply::msg("Nothing captured yet — load a page first.")
            } else {
                let (bug, cost) = describe_bug(session, backend, "").await;
                AgentReply {
                    text: format!(
                        "{}{}\n\n{}",
                        session_summary(session),
                        session.scope_note(),
                        fix_prompt(&slugify(&bug_title(&bug, "")), &bug)
                    ),
                    cost_usd: cost,
                    wrote: Vec::new(),
                }
            }
        }

        Intent::Report => {
            if session.is_empty() {
                AgentReply::msg("Nothing captured yet — nothing to report.")
            } else {
                let (bug, cost) = describe_bug(session, backend, "").await;
                let title = bug_title(&bug, "");
                let fix = fix_prompt(&slugify(&title), &bug);
                let filed = match tools::JiraConfig::from_env() {
                    None => {
                        "JIRA_BASE_URL / JIRA_API_TOKEN not set — ticket not filed.".to_string()
                    }
                    Some(cfg) => {
                        let body = format!("{bug}\n\n{fix}");
                        match tools::create_jira_issue(&cfg, &title, &body).await {
                            Ok(key) => format!("Filed {key} at {}/browse/{key}", cfg.base_url),
                            Err(e) => format!("Jira filing failed: {e}"),
                        }
                    }
                };
                AgentReply {
                    text: format!("{filed}\n\n{fix}"),
                    cost_usd: cost,
                    wrote: Vec::new(),
                }
            }
        }

        Intent::Ask(q) if q.is_empty() => AgentReply::msg(HELP),
        Intent::Ask(q) => {
            if session.is_empty() {
                AgentReply::msg(
                    "Nothing captured yet — load a page and I'll have something to work with.",
                )
            } else {
                let a = llm::ask(backend, &session.digest(), &q)
                    .await
                    .unwrap_or_else(|e| llm::Answer {
                        text: format!("{REFUSAL}\n(reasoner unavailable: {e})"),
                        cost_usd: 0.0,
                        refused: true,
                    });
                AgentReply {
                    text: a.text,
                    cost_usd: a.cost_usd,
                    wrote: Vec::new(),
                }
            }
        }
    };

    Ok(reply)
}

/// Deterministic session facts — always true, never model-generated.
fn session_summary(s: &Session) -> String {
    let failed: Vec<&Exchange> = s.exchanges.iter().filter(|e| e.is_error()).collect();
    let errs = s.console.iter().filter(|c| c.severity == "error").count();
    let slowest = s
        .exchanges
        .iter()
        .max_by(|a, b| a.duration_ms.total_cmp(&b.duration_ms));
    let mut out = format!(
        "Session: {} requests, {} failed, {} console errors.",
        s.exchanges.len(),
        failed.len(),
        errs
    );
    if let Some(sl) = slowest {
        if sl.duration_ms > 0.0 {
            out.push_str(&format!(
                "\nSlowest: {} {} at {:.0}ms.",
                sl.method,
                sl.path(),
                sl.duration_ms
            ));
        }
    }
    for f in failed.iter().take(5) {
        out.push_str(&format!(
            "\n  {} {} → {}",
            f.method,
            f.path(),
            f.status
                .map(|c| c.to_string())
                .unwrap_or_else(|| f.error.clone().unwrap_or_default())
        ));
    }
    out
}

/// Ask the model for the reproduction prose. Falls back to a deterministic
/// description built from the session when the reasoner is unavailable or refuses,
/// so `/review` always produces a usable prompt.
async fn describe_bug(session: &Session, backend: &Backend, hint: &str) -> (String, f64) {
    let ask = if hint.is_empty() {
        "Identify the single most likely bug in this session. Reply with numbered \
         reproduction steps followed by a line starting 'Expected:' describing correct \
         behaviour. Cite the exact requests, status codes and body strings involved."
            .to_string()
    } else {
        format!(
            "The user reports: {hint}\nWrite numbered reproduction steps for this, grounded \
             in the captured session, followed by a line starting 'Expected:'. Cite exact \
             requests, status codes and body strings."
        )
    };
    match llm::ask(backend, &session.digest(), &ask).await {
        Ok(a) if !a.refused && !a.text.trim().is_empty() => (a.text, a.cost_usd),
        Ok(a) => (fallback_repro(session, hint), a.cost_usd),
        Err(_) => (fallback_repro(session, hint), 0.0),
    }
}

/// Reproduction steps assembled from the session alone. No model involved.
fn fallback_repro(s: &Session, hint: &str) -> String {
    let mut out = String::new();
    let mut n = 1;
    if !hint.is_empty() {
        out.push_str(&format!("Reported: {hint}\n"));
    }
    for nav in s.navigations.iter().take(5) {
        out.push_str(&format!("{n}. Open {}\n", nav.url));
        n += 1;
    }
    match tools::primary_failure(&s.exchanges) {
        Some(f) => {
            out.push_str(&format!(
                "{n}. Trigger {} {} — it returns {}",
                f.method,
                f.path(),
                f.status
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| f.error.clone().unwrap_or_default())
            ));
            if let Some(b) = f.body_text() {
                let b = b.trim();
                if !b.is_empty() {
                    out.push_str(&format!(" with body {}", &b[..b.len().min(200)]));
                }
            }
            out.push('\n');
            out.push_str(&format!(
                "Expected: {} {} succeeds, or fails with a specific, handled error.\n",
                f.method,
                f.path()
            ));
        }
        None => {
            let err = s.console.iter().find(|c| c.severity == "error");
            match err {
                Some(c) => {
                    out.push_str(&format!("{n}. Observe the console error: {}\n", c.text));
                    out.push_str("Expected: no console errors during this flow.\n");
                }
                None => out.push_str("Expected: no failures observed in this session.\n"),
            }
        }
    }
    out
}

/// A short title for the branch slug and the Jira summary.
fn bug_title(bug: &str, hint: &str) -> String {
    if !hint.is_empty() {
        return hint.to_string();
    }
    bug.lines()
        .find(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with("Expected:")
        })
        .map(|l| l.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ' '))
        .unwrap_or("session issue")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_commands_and_free_text() {
        assert_eq!(
            classify("why did checkout fail?"),
            Intent::Ask("why did checkout fail?".into())
        );
        assert_eq!(classify("/review"), Intent::Review);
        assert_eq!(classify("  /review  "), Intent::Review);
        assert_eq!(
            classify("/save-page checkout"),
            Intent::SavePage("checkout".into())
        );
        assert_eq!(classify("/save-page"), Intent::SavePage("page".into()));
        assert_eq!(
            classify("/reproduce the 500"),
            Intent::Reproduce("the 500".into())
        );
        assert_eq!(classify("/export"), Intent::Export(None));
        assert_eq!(
            classify("/export api.yaml"),
            Intent::Export(Some("api.yaml".into()))
        );
    }

    #[test]
    fn unknown_slash_command_never_becomes_a_prompt() {
        // a typo must not silently turn into a paid model call
        assert_eq!(classify("/revieww"), Intent::Help);
        assert_eq!(classify("/rm -rf /"), Intent::Help);
        assert_eq!(classify("/"), Intent::Help);
    }

    fn session_with_failure() -> Session {
        let mut e = Exchange {
            method: "POST".into(),
            url: "http://localhost:8080/api/cart/checkout".into(),
            status: Some(500),
            duration_ms: 2100.0,
            ..Default::default()
        };
        e.res_body = Some(br#"{"error":"empty_line_item"}"#.to_vec());
        Session {
            target: "http://localhost:8080".into(),
            filter: None,
            total_exchanges: 1,
            exchanges: vec![e],
            console: vec![ConsoleLine {
                ts: "t".into(),
                severity: "error".into(),
                text: "TypeError: t.total is undefined".into(),
                url: None,
                line: None,
                source: "console".into(),
                page_url: None,
            }],
            navigations: vec![Navigation {
                ts: "t".into(),
                url: "http://localhost:8080/checkout".into(),
            }],
        }
    }

    #[test]
    fn fallback_reproduction_needs_no_model() {
        let s = session_with_failure();
        let r = fallback_repro(&s, "");
        assert!(r.contains("Open http://localhost:8080/checkout"));
        assert!(r.contains("POST /api/cart/checkout"));
        assert!(r.contains("500"));
        assert!(r.contains("empty_line_item"));
        assert!(r.contains("Expected:"));
    }

    #[test]
    fn summary_reports_only_measured_facts() {
        let s = session_with_failure();
        let out = session_summary(&s);
        assert!(out.contains("1 requests, 1 failed, 1 console errors"));
        assert!(out.contains("2100ms"));
    }

    #[test]
    fn a_filtered_session_tells_the_model_it_is_a_slice() {
        let mut s = session_with_failure();
        s.filter = Some("REST · /checkout".into());
        s.total_exchanges = 561;

        let d = s.digest();
        assert!(d.contains("FILTERED"), "must be flagged: {d}");
        assert!(d.contains("REST · /checkout"), "names the filter");
        assert!(d.contains("of 561"), "gives the denominator");
        // and instructs against generalising
        assert!(d.to_lowercase().contains("filtered view"));
    }

    #[test]
    fn an_unfiltered_session_says_nothing_about_filters() {
        let s = session_with_failure();
        let d = s.digest();
        assert!(!d.contains("FILTERED"));
        assert_eq!(s.scope_note(), "", "no note when nothing is filtered");
    }

    #[test]
    fn scope_note_states_coverage_honestly() {
        let mut s = session_with_failure();
        s.filter = Some("POST · /checkout".into());
        s.total_exchanges = 561;
        let n = s.scope_note();
        assert!(n.contains("1 of 561"));
        assert!(n.contains("POST · /checkout"));
    }

    #[tokio::test]
    async fn export_covers_only_the_filtered_slice_and_says_so() {
        let dir = std::env::temp_dir().join(format!("nc-export-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("s.db");
        let mut db = Db::open(&db_path, "t").unwrap();

        let mut s = session_with_failure();
        s.filter = Some("REST · /checkout".into());
        s.total_exchanges = 561;

        let r = handle(
            Intent::Export(Some("api.yaml".into())),
            &s,
            &Backend::ClaudeCli {
                model: "haiku".into(),
            },
            &mut db,
            &dir,
        )
        .await
        .unwrap();

        assert!(r.text.contains("1 of 561"), "reports coverage: {}", r.text);
        let doc = std::fs::read_to_string(dir.join("api.yaml")).unwrap();
        assert!(doc.contains("/api/cart/checkout"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn save_page_names_the_filtered_page_not_the_last_navigation() {
        let dir = std::env::temp_dir().join(format!("nc-save-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("s.db"), "t").unwrap();

        let mut s = session_with_failure();
        // the browser has since moved on, but the filtered data is /checkout
        s.navigations.push(Navigation {
            ts: "t".into(),
            url: "http://localhost:8080/somewhere-else".into(),
        });
        s.exchanges[0].page_url = Some("http://localhost:8080/checkout".into());
        s.filter = Some("/checkout".into());

        let r = handle(
            Intent::SavePage("cart".into()),
            &s,
            &Backend::ClaudeCli {
                model: "haiku".into(),
            },
            &mut db,
            &dir,
        )
        .await
        .unwrap();

        assert!(r.text.contains("/checkout"), "{}", r.text);
        assert!(
            !r.text.contains("somewhere-else"),
            "must not name where the browser drifted to: {}",
            r.text
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bug_title_skips_expected_line() {
        let t = bug_title("1. Do the thing\nExpected: it works", "");
        assert_eq!(t, "Do the thing");
        assert_eq!(bug_title("anything", "user hint"), "user hint");
    }
}
