//! What the copy keys put on the clipboard.
//!
//! These are paste targets, not display text: the error report in particular is
//! shaped to be dropped straight into a coding agent or a bug report and be
//! self-sufficient — request, response, console, and the page it happened on.

use crate::agent::tools::curl_for;
use crate::app::human_size;
use crate::db::{ConsoleLine, Exchange};

/// Full exchange: request headers + body, response headers + body.
pub fn exchange(e: &Exchange) -> String {
    let mut s = String::new();
    s.push_str(&format!("{} {}\n", e.method, e.url));
    s.push_str(&status_line(e));
    if let Some(p) = &e.page_url {
        s.push_str(&format!("Page: {p}\n"));
    }
    s.push('\n');
    s.push_str(&request(e));
    s.push('\n');
    s.push_str(&response(e));
    s
}

/// Request side only.
pub fn request(e: &Exchange) -> String {
    let mut s = format!("--- REQUEST ---\n{} {}\n", e.method, e.url);
    for (k, v) in &e.req_headers {
        s.push_str(&format!("{k}: {v}\n"));
    }
    match &e.req_body {
        Some(b) if !b.trim().is_empty() => {
            s.push('\n');
            s.push_str(&pretty(b));
            s.push('\n');
        }
        _ => s.push_str("\n(no request body)\n"),
    }
    s
}

/// Response side only.
pub fn response(e: &Exchange) -> String {
    let mut s = String::from("--- RESPONSE ---\n");
    s.push_str(&status_line(e));
    for (k, v) in &e.res_headers {
        s.push_str(&format!("{k}: {v}\n"));
    }
    if let Some(from) = e.truncated_from {
        s.push_str(&format!(
            "\n(body truncated for storage; full size {})\n",
            human_size(from)
        ));
    }
    match e.body_text() {
        Some(b) if !b.trim().is_empty() => {
            s.push('\n');
            s.push_str(&pretty(&b));
            s.push('\n');
        }
        Some(_) => s.push_str("\n(empty response body)\n"),
        None if e.res_body.is_some() => s.push_str("\n(binary response body)\n"),
        None => s.push_str("\n(response body not captured)\n"),
    }
    s
}

/// A runnable curl reproducing this request.
pub fn as_curl(e: &Exchange) -> String {
    curl_for(e)
}

/// Everything needed to act on a failure, in one paste.
///
/// Deliberately includes the console lines and the page: a 500 without the
/// JavaScript error it triggered is half a bug report.
pub fn error_report(e: &Exchange, console: &[ConsoleLine], target: &str) -> String {
    let mut s = String::from("# Captured failure\n\n");
    s.push_str(&format!("Session target: {target}\n"));
    if let Some(p) = &e.page_url {
        s.push_str(&format!("Page:           {p}\n"));
    }
    s.push_str(&format!("Request:        {} {}\n", e.method, e.url));
    s.push_str(&format!("Result:         {}", status_line(e)));
    if e.duration_ms > 0.0 {
        s.push_str(&format!("Duration:       {:.0}ms\n", e.duration_ms));
    }
    s.push_str(&format!("Size:           {}\n\n", human_size(e.size)));

    s.push_str(&request(e));
    s.push('\n');
    s.push_str(&response(e));

    // console output from the same page, errors and warnings only
    let relevant: Vec<&ConsoleLine> = console
        .iter()
        .filter(|c| c.severity == "error" || c.severity == "warn")
        .collect();
    s.push_str("\n--- CONSOLE ---\n");
    if relevant.is_empty() {
        s.push_str("(no errors or warnings captured)\n");
    } else {
        for c in relevant.iter().take(40) {
            s.push_str(&format!("[{}] {}\n", c.severity, c.text));
        }
        if relevant.len() > 40 {
            s.push_str(&format!("… and {} more\n", relevant.len() - 40));
        }
    }

    s.push_str("\n--- REPRODUCE ---\n");
    s.push_str(&curl_for(e));
    s.push('\n');
    s
}

/// Every console error, for pasting a whole page's failures at once.
pub fn console_errors(console: &[ConsoleLine]) -> String {
    let errs: Vec<&ConsoleLine> = console
        .iter()
        .filter(|c| c.severity == "error" || c.severity == "warn")
        .collect();
    if errs.is_empty() {
        return "(no console errors or warnings captured)\n".into();
    }
    let mut s = format!("# Console — {} errors/warnings\n\n", errs.len());
    for c in errs {
        s.push_str(&format!("[{}] {}", c.severity, c.text));
        if let (Some(u), Some(l)) = (&c.url, c.line) {
            s.push_str(&format!("\n    at {u}:{l}"));
        }
        s.push('\n');
    }
    s
}

fn status_line(e: &Exchange) -> String {
    match (e.status, &e.error) {
        (_, Some(err)) => format!("FAILED: {err}\n"),
        (Some(s), _) => format!("HTTP {s} {}\n", e.status_text.clone().unwrap_or_default()),
        (None, None) => "(no response captured)\n".into(),
    }
}

fn pretty(s: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| s.to_string()),
        Err(_) => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Headers;

    fn failing() -> Exchange {
        let mut rh = Headers::new();
        rh.insert("content-type".into(), "application/json".into());
        rh.insert("authorization".into(), "Bearer tok".into());
        let mut sh = Headers::new();
        sh.insert("content-type".into(), "application/json".into());
        Exchange {
            method: "POST".into(),
            url: "http://localhost:8080/api/cart/checkout".into(),
            status: Some(500),
            status_text: Some("Internal Server Error".into()),
            req_headers: rh,
            res_headers: sh,
            req_body: Some(r#"{"items":[{"sku":"A-12","qty":0}]}"#.into()),
            res_body: Some(br#"{"error":"empty_line_item"}"#.to_vec()),
            duration_ms: 2100.0,
            size: 27,
            page_url: Some("http://localhost:8080/checkout".into()),
            ..Default::default()
        }
    }

    fn console() -> Vec<ConsoleLine> {
        vec![
            ConsoleLine {
                ts: "t".into(),
                severity: "error".into(),
                text: "TypeError: t.total is undefined".into(),
                url: Some("http://localhost:8080/app.js".into()),
                line: Some(42),
                source: "exception".into(),
            },
            ConsoleLine {
                ts: "t".into(),
                severity: "debug".into(),
                text: "noise".into(),
                url: None,
                line: None,
                source: "console".into(),
            },
        ]
    }

    #[test]
    fn error_report_is_self_sufficient() {
        let r = error_report(&failing(), &console(), "http://localhost:8080");
        // the failing call
        assert!(r.contains("POST http://localhost:8080/api/cart/checkout"));
        assert!(r.contains("HTTP 500"));
        // the page it happened on — a 500 without this is half a report
        assert!(r.contains("/checkout"));
        // both payloads
        assert!(r.contains("empty_line_item"));
        assert!(
            r.contains(r#""sku": "A-12""#),
            "request body, pretty-printed"
        );
        // the JS error it triggered
        assert!(r.contains("TypeError: t.total is undefined"));
        // and a way to re-run it
        assert!(r.contains("curl -i -X POST"));
        // debug noise stays out
        assert!(!r.contains("noise"));
    }

    #[test]
    fn request_and_response_are_separable() {
        let e = failing();
        let req = request(&e);
        assert!(req.contains("authorization: Bearer tok"));
        assert!(req.contains("A-12"));
        assert!(
            !req.contains("empty_line_item"),
            "response must not leak in"
        );

        let res = response(&e);
        assert!(res.contains("HTTP 500"));
        assert!(res.contains("empty_line_item"));
        assert!(!res.contains("Bearer tok"), "request must not leak in");
    }

    #[test]
    fn json_bodies_are_pretty_printed_for_pasting() {
        let r = exchange(&failing());
        assert!(r.contains("\"error\": \"empty_line_item\""));
    }

    #[test]
    fn missing_pieces_are_labelled_not_silently_absent() {
        let bare = Exchange {
            method: "GET".into(),
            url: "http://x/y".into(),
            ..Default::default()
        };
        let r = exchange(&bare);
        assert!(r.contains("(no request body)"));
        assert!(r.contains("(response body not captured)"));
        assert!(r.contains("(no response captured)"));

        let empty_console = error_report(&bare, &[], "t");
        assert!(empty_console.contains("(no errors or warnings captured)"));
    }

    #[test]
    fn network_failures_report_the_error_not_a_status() {
        let mut e = failing();
        e.status = None;
        e.status_text = None;
        e.error = Some("net::ERR_CONNECTION_REFUSED".into());
        let r = error_report(&e, &[], "t");
        assert!(r.contains("FAILED: net::ERR_CONNECTION_REFUSED"));
    }

    #[test]
    fn truncation_is_disclosed_in_the_paste() {
        let mut e = failing();
        e.truncated_from = Some(12_279_560);
        assert!(response(&e).contains("truncated"));
        assert!(response(&e).contains("11.7 MB"));
    }

    #[test]
    fn console_copy_includes_location_and_skips_debug() {
        let c = console_errors(&console());
        assert!(c.contains("TypeError"));
        assert!(c.contains("app.js:42"));
        assert!(!c.contains("noise"));
        assert_eq!(
            console_errors(&[]).trim(),
            "(no console errors or warnings captured)"
        );
    }
}
