//! The deterministic half of the agent.
//!
//! Every slash command is a plain Rust function over the SQLite session. The model
//! never generates a YAML document, a curl line, or a Jira payload — it is asked
//! only for prose (a bug description), and even that is validated before use.
//!
//! ADR-0002: this split came out of the guard spike, where `/reproduce` broke the
//! JSON envelope by returning a bash block. Commands produce artifacts; artifacts
//! are code's job.

use crate::db::{ConsoleLine, Exchange, Navigation};
use anyhow::{Context, Result};
use serde_yaml_ng as yaml;
use std::collections::BTreeMap;

/// How much of a body to inline into an export or a prompt.
const SAMPLE: usize = 1200;

/// Requests worth reasoning about — the API calls, not the 400 JS modules a dev
/// server serves. The probe measured 554 requests per page load; without this
/// filter every prompt would be mostly noise.
/// Uses the same `is_rest`/`is_ajax`/`is_static` classification the UI filters use
/// (see `db::Exchange`), so "what the REST tab shows" and "what the agent reasons
/// about" cannot drift apart.
pub fn interesting(all: &[Exchange]) -> Vec<&Exchange> {
    all.iter()
        .filter(|e| e.is_error() || ((e.is_rest() || e.is_ajax()) && !e.is_static()))
        .collect()
}

/// The session digest handed to the model. This — and only this — is its world.
pub fn digest(
    target: &str,
    ex: &[Exchange],
    console: &[ConsoleLine],
    navs: &[Navigation],
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "CAPTURED SESSION — target {target}\n{} requests total, {} failed, {} console errors\n\n",
        ex.len(),
        ex.iter().filter(|e| e.is_error()).count(),
        console.iter().filter(|c| c.severity == "error").count()
    ));

    if !navs.is_empty() {
        s.push_str("PAGE NAVIGATIONS (in order):\n");
        for n in navs {
            s.push_str(&format!("  {} {}\n", n.ts, n.url));
        }
        s.push('\n');
    }

    let picks = interesting(ex);
    s.push_str(&format!("REQUESTS ({} shown):\n", picks.len()));
    for e in picks.iter().take(80) {
        s.push_str(&format!(
            "- {} {} → {} ({:.0}ms, {} B)\n",
            e.method,
            e.url,
            e.status
                .map(|c| c.to_string())
                .unwrap_or_else(|| e.error.clone().unwrap_or_else(|| "pending".into())),
            e.duration_ms,
            e.size
        ));
        if let Some(b) = &e.req_body {
            s.push_str(&format!("    request body:  {}\n", clip(b, SAMPLE)));
        }
        if let Some(b) = e.body_text() {
            if !b.trim().is_empty() {
                s.push_str(&format!("    response body: {}\n", clip(&b, SAMPLE)));
            }
        }
    }

    let errs: Vec<&ConsoleLine> = console
        .iter()
        .filter(|c| c.severity == "error" || c.severity == "warn")
        .collect();
    if !errs.is_empty() {
        s.push_str(&format!("\nCONSOLE ({} errors/warnings):\n", errs.len()));
        for c in errs.iter().take(60) {
            s.push_str(&format!("  [{}] {}\n", c.severity, clip(&c.text, 300)));
        }
    }
    s
}

fn clip(s: &str, n: usize) -> String {
    let one_line = s.replace(['\n', '\r'], " ");
    if one_line.chars().count() <= n {
        one_line
    } else {
        one_line.chars().take(n).collect::<String>() + "…(truncated)"
    }
}

// ---------------------------------------------------------------- OpenAPI export

/// A ready-to-import OpenAPI 3.1 document built from what actually happened,
/// with real captured payloads as examples.
pub fn openapi(target: &str, ex: &[Exchange]) -> Result<String> {
    let picks = interesting(ex);
    let mut paths: BTreeMap<String, yaml::Mapping> = BTreeMap::new();
    let mut servers: Vec<String> = Vec::new();

    for e in &picks {
        let base = e
            .url
            .split_once("://")
            .map(|(scheme, rest)| {
                let host = rest.split('/').next().unwrap_or("");
                format!("{scheme}://{host}")
            })
            .unwrap_or_default();
        if !base.is_empty() && !servers.contains(&base) {
            servers.push(base);
        }

        let mut op = yaml::Mapping::new();
        op.insert(
            "summary".into(),
            format!("{} {}", e.method, e.path()).into(),
        );
        op.insert("operationId".into(), operation_id(e).into());

        // query parameters, recovered from the captured URL
        if let Some(q) = e.url.split_once('?').map(|(_, q)| q) {
            let mut params = yaml::Sequence::new();
            for (k, v) in q.split('&').filter_map(|kv| kv.split_once('=')) {
                let mut p = yaml::Mapping::new();
                p.insert("name".into(), k.into());
                p.insert("in".into(), "query".into());
                p.insert("required".into(), false.into());
                let mut sch = yaml::Mapping::new();
                sch.insert("type".into(), "string".into());
                p.insert("schema".into(), sch.into());
                p.insert("example".into(), urldecode(v).into());
                params.push(p.into());
            }
            if !params.is_empty() {
                op.insert("parameters".into(), params.into());
            }
        }

        if let Some(body) = &e.req_body {
            op.insert("requestBody".into(), body_object(body, "application/json"));
        }

        let mut responses = yaml::Mapping::new();
        let code = e.status.unwrap_or(0).to_string();
        let mut resp = yaml::Mapping::new();
        resp.insert(
            "description".into(),
            e.status_text
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "captured response".into())
                .into(),
        );
        if let Some(b) = e.body_text() {
            if !b.trim().is_empty() {
                let mime = e.mime_type.clone().unwrap_or_else(|| "text/plain".into());
                resp.insert("content".into(), content_object(&b, &mime));
            }
        }
        responses.insert(code.into(), resp.into());
        op.insert("responses".into(), responses.into());

        paths
            .entry(e.path())
            .or_default()
            .insert(e.method.to_lowercase().into(), op.into());
    }

    let mut root = yaml::Mapping::new();
    root.insert("openapi".into(), "3.1.0".into());
    let mut info = yaml::Mapping::new();
    info.insert(
        "title".into(),
        format!("Captured session — {target}").into(),
    );
    info.insert("version".into(), "1.0.0".into());
    info.insert(
        "description".into(),
        format!(
            "Generated by networkcop from {} observed requests. Examples are real captured payloads.",
            picks.len()
        )
        .into(),
    );
    root.insert("info".into(), info.into());
    root.insert(
        "servers".into(),
        servers
            .into_iter()
            .map(|u| {
                let mut m = yaml::Mapping::new();
                m.insert("url".into(), u.into());
                yaml::Value::from(m)
            })
            .collect::<yaml::Sequence>()
            .into(),
    );
    let mut path_map = yaml::Mapping::new();
    for (k, v) in paths {
        path_map.insert(k.into(), v.into());
    }
    root.insert("paths".into(), path_map.into());

    yaml::to_string(&yaml::Value::from(root)).context("serialise OpenAPI document")
}

fn operation_id(e: &Exchange) -> String {
    let mut s = e.method.to_lowercase();
    for seg in e.path().split('/').filter(|x| !x.is_empty()) {
        s.push('_');
        s.push_str(&seg.replace(|c: char| !c.is_ascii_alphanumeric(), ""));
    }
    s
}

fn body_object(body: &str, mime: &str) -> yaml::Value {
    let mut rb = yaml::Mapping::new();
    rb.insert("required".into(), true.into());
    rb.insert("content".into(), content_object(body, mime));
    rb.into()
}

fn content_object(body: &str, mime: &str) -> yaml::Value {
    let key = mime.split(';').next().unwrap_or(mime).trim().to_string();
    let mut media = yaml::Mapping::new();
    let mut schema = yaml::Mapping::new();
    // infer just enough shape to be importable
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    schema.insert(
        "type".into(),
        match &parsed {
            Some(serde_json::Value::Array(_)) => "array".into(),
            Some(serde_json::Value::Object(_)) => "object".into(),
            _ => "string".into(),
        },
    );
    media.insert("schema".into(), schema.into());
    media.insert("example".into(), clip(body, 4000).into());
    let mut content = yaml::Mapping::new();
    content.insert(key.into(), media.into());
    content.into()
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(v) => {
                    out.push(v);
                    i += 3;
                }
                Err(_) => {
                    out.push(b[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------- /save-page

/// A named page export: the navigation plus every call that belongs to it.
pub fn save_page(
    name: &str,
    url: &str,
    ex: &[Exchange],
    console: &[ConsoleLine],
) -> Result<String> {
    let mut root = yaml::Mapping::new();
    root.insert("name".into(), name.into());
    root.insert("page".into(), url.into());
    root.insert("captured_at".into(), chrono::Utc::now().to_rfc3339().into());

    let mut calls = yaml::Sequence::new();
    for e in interesting(ex) {
        let mut m = yaml::Mapping::new();
        m.insert("method".into(), e.method.clone().into());
        m.insert("url".into(), e.url.clone().into());
        m.insert(
            "status".into(),
            e.status.map(yaml::Value::from).unwrap_or(yaml::Value::Null),
        );
        m.insert("duration_ms".into(), (e.duration_ms.round() as i64).into());
        m.insert("size_bytes".into(), (e.size as i64).into());
        m.insert("request_headers".into(), headers_map(&e.req_headers));
        if let Some(b) = &e.req_body {
            m.insert("request_body".into(), clip(b, 4000).into());
        }
        m.insert("response_headers".into(), headers_map(&e.res_headers));
        if let Some(b) = e.body_text() {
            m.insert("response_body".into(), clip(&b, 4000).into());
        }
        if let Some(err) = &e.error {
            m.insert("error".into(), err.clone().into());
        }
        calls.push(m.into());
    }
    root.insert("calls".into(), calls.into());

    let mut logs = yaml::Sequence::new();
    for c in console.iter().filter(|c| c.severity != "debug") {
        let mut m = yaml::Mapping::new();
        m.insert("severity".into(), c.severity.clone().into());
        m.insert("text".into(), clip(&c.text, 1000).into());
        logs.push(m.into());
    }
    root.insert("console".into(), logs.into());

    yaml::to_string(&yaml::Value::from(root)).context("serialise page export")
}

fn headers_map(h: &BTreeMap<String, String>) -> yaml::Value {
    let mut m = yaml::Mapping::new();
    for (k, v) in h {
        m.insert(k.clone().into(), v.clone().into());
    }
    m.into()
}

// ---------------------------------------------------------------- /reproduce

/// The first failure worth reproducing, preferring server errors.
pub fn primary_failure(ex: &[Exchange]) -> Option<&Exchange> {
    ex.iter()
        .find(|e| e.status.map(|s| s >= 500).unwrap_or(false))
        .or_else(|| {
            ex.iter()
                .find(|e| e.status.map(|s| s >= 400).unwrap_or(false))
        })
        .or_else(|| ex.iter().find(|e| e.error.is_some()))
}

/// A runnable curl line reproducing one captured request.
pub fn curl_for(e: &Exchange) -> String {
    let mut parts = vec![format!("curl -i -X {}", e.method)];
    for (k, v) in &e.req_headers {
        let lk = k.to_ascii_lowercase();
        // pseudo-headers and hop-by-hop noise are not reproducible
        if lk.starts_with(':') || lk == "host" || lk == "content-length" {
            continue;
        }
        parts.push(format!("  -H {}", shell_quote(&format!("{k}: {v}"))));
    }
    if let Some(b) = &e.req_body {
        parts.push(format!("  --data-raw {}", shell_quote(b)));
    }
    parts.push(format!("  {}", shell_quote(&e.url)));
    parts.join(" \\\n")
}

/// A Playwright script that walks the captured navigations and asserts the
/// failing call fails — a reproduction that runs in CI, not just in prose.
pub fn playwright_for(navs: &[Navigation], failure: Option<&Exchange>) -> String {
    let mut s = String::from(
        "// npm i -D @playwright/test && npx playwright test repro.spec.ts\n\
         import { test, expect } from '@playwright/test';\n\n\
         test('reproduces the captured failure', async ({ page }) => {\n",
    );
    for n in navs.iter().take(8) {
        s.push_str(&format!("  await page.goto({});\n", js_string(&n.url)));
        s.push_str("  await page.waitForLoadState('networkidle');\n");
    }
    match failure {
        Some(f) => {
            s.push_str(&format!(
                "\n  // captured: {} {} → {}\n",
                f.method,
                f.path(),
                f.status.unwrap_or(0)
            ));
            s.push_str(&format!(
                "  const res = await page.request.fetch({}, {{\n    method: {},\n",
                js_string(&f.url),
                js_string(&f.method)
            ));
            if let Some(b) = &f.req_body {
                s.push_str(&format!("    data: {},\n", js_string(b)));
            }
            s.push_str("  });\n");
            s.push_str(&format!(
                "  expect(res.status()).toBe({}); // fails once fixed — flip to the expected code\n",
                f.status.unwrap_or(0)
            ));
        }
        None => s.push_str("\n  // no failing request captured in this session\n"),
    }
    s.push_str("});\n");
    s
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn js_string(s: &str) -> String {
    format!("'{}'", s.replace('\\', r"\\").replace('\'', r"\'"))
}

// ---------------------------------------------------------------- /report (Jira)

pub struct JiraConfig {
    pub base_url: String,
    pub token: String,
    pub email: Option<String>,
    pub project: String,
}

impl JiraConfig {
    /// Present only when both required env vars are set — `/report` degrades to
    /// returning the prompt otherwise, which the spec explicitly permits.
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("JIRA_BASE_URL").ok()?;
        let token = std::env::var("JIRA_API_TOKEN").ok()?;
        Some(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            email: std::env::var("JIRA_EMAIL").ok(),
            project: std::env::var("JIRA_PROJECT").unwrap_or_else(|_| "BUG".into()),
        })
    }
}

/// Create a Jira issue. Returns the issue key.
pub async fn create_jira_issue(
    cfg: &JiraConfig,
    summary: &str,
    description: &str,
) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(45))
        .build()?;
    // Jira Cloud v3 wants Atlassian Document Format, not raw text.
    let body = serde_json::json!({
        "fields": {
            "project": { "key": cfg.project },
            "summary": summary,
            "issuetype": { "name": "Bug" },
            "description": {
                "type": "doc",
                "version": 1,
                "content": description.split("\n\n").map(|para| serde_json::json!({
                    "type": "paragraph",
                    "content": [{ "type": "text", "text": para }]
                })).collect::<Vec<_>>()
            }
        }
    });

    let req = client
        .post(format!("{}/rest/api/3/issue", cfg.base_url))
        .json(&body);
    let req = match &cfg.email {
        Some(email) => req.basic_auth(email, Some(&cfg.token)),
        None => req.bearer_auth(&cfg.token),
    };

    let resp = req.send().await.context("POST to Jira")?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        anyhow::bail!(
            "Jira responded {status}: {}",
            v["errorMessages"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join("; "))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| v.to_string())
        );
    }
    Ok(v["key"].as_str().unwrap_or("UNKNOWN").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Headers;

    fn ex(method: &str, url: &str, status: i64, mime: &str) -> Exchange {
        Exchange {
            method: method.into(),
            url: url.into(),
            status: Some(status),
            mime_type: Some(mime.into()),
            resource_type: Some("XHR".into()),
            ..Default::default()
        }
    }

    #[test]
    fn interesting_keeps_api_calls_and_all_errors_drops_assets() {
        let all = vec![
            ex("GET", "http://x/api/me", 200, "application/json"),
            ex("GET", "http://x/src/main.js", 200, "text/javascript"),
            ex("GET", "http://x/logo.png", 200, "image/png"),
            ex("GET", "http://x/broken.png", 404, "image/png"), // error → kept
        ];
        let picked = interesting(&all);
        let urls: Vec<&str> = picked.iter().map(|e| e.url.as_str()).collect();
        assert!(urls.contains(&"http://x/api/me"));
        assert!(urls.contains(&"http://x/broken.png"), "errors always kept");
        assert!(!urls.contains(&"http://x/src/main.js"), "js dropped");
        assert!(!urls.contains(&"http://x/logo.png"), "images dropped");
    }

    #[test]
    fn openapi_is_valid_yaml_with_captured_examples() {
        let mut e = ex(
            "POST",
            "http://localhost:8080/api/cart?debug=1",
            500,
            "application/json",
        );
        e.req_body = Some(r#"{"qty":0}"#.into());
        e.res_body = Some(br#"{"error":"empty_line_item"}"#.to_vec());
        e.status_text = Some("Internal Server Error".into());
        let doc = openapi("http://localhost:8080", &[e]).unwrap();

        let parsed: yaml::Value = yaml::from_str(&doc).expect("must be parseable YAML");
        assert_eq!(parsed["openapi"].as_str(), Some("3.1.0"));
        assert!(doc.contains("/api/cart"), "path present");
        assert!(doc.contains("empty_line_item"), "real response as example");
        assert!(doc.contains("debug"), "query param recovered");
        assert!(!doc.contains("?debug=1"), "path key must be query-free");
    }

    #[test]
    fn curl_is_shell_safe_and_drops_pseudo_headers() {
        let mut e = ex("POST", "http://x/api/y", 500, "application/json");
        let mut h = Headers::new();
        h.insert(":authority".into(), "x".into());
        h.insert("host".into(), "x".into());
        h.insert("content-length".into(), "9".into());
        h.insert("x-token".into(), "it's-a-secret".into());
        e.req_headers = h;
        e.req_body = Some(r#"{"a":"it's"}"#.into());

        let c = curl_for(&e);
        assert!(!c.contains(":authority"), "pseudo-header dropped");
        assert!(!c.contains("-H 'host"), "host dropped");
        assert!(!c.contains("content-length"), "content-length dropped");
        // an embedded quote must not break out of the shell string
        assert!(c.contains(r"'\''"), "single quotes escaped: {c}");
        assert!(c.contains("-X POST"));
    }

    #[test]
    fn save_page_yaml_round_trips() {
        let mut e = ex("GET", "http://x/api/items", 200, "application/json");
        e.res_body = Some(b"[]".to_vec());
        let out = save_page(
            "checkout",
            "http://x/checkout",
            &[e],
            &[ConsoleLine {
                ts: "t".into(),
                severity: "error".into(),
                text: "boom".into(),
                url: None,
                line: None,
                source: "console".into(),
            }],
        )
        .unwrap();
        let v: yaml::Value = yaml::from_str(&out).expect("parseable");
        assert_eq!(v["name"].as_str(), Some("checkout"));
        assert_eq!(v["calls"].as_sequence().unwrap().len(), 1);
        assert_eq!(v["console"].as_sequence().unwrap().len(), 1);
    }

    #[test]
    fn primary_failure_prefers_5xx_over_4xx() {
        let all = vec![
            ex("GET", "http://x/a", 404, "application/json"),
            ex("GET", "http://x/b", 500, "application/json"),
        ];
        assert_eq!(primary_failure(&all).unwrap().url, "http://x/b");
    }

    #[test]
    fn playwright_escapes_quotes_in_urls() {
        let navs = vec![Navigation {
            ts: "t".into(),
            url: "http://x/it's".into(),
        }];
        let s = playwright_for(&navs, None);
        assert!(s.contains(r"it\'s"), "quote escaped: {s}");
    }
}
