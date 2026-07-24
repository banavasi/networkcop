//! Phase 1 spike — the load-bearing question for the whole capture design:
//! does Chrome still hand over a response body after the page has navigated away?
//!
//! Launches Chrome on a debug port, attaches to the page target, records every
//! response, then fetches each body TWICE: once right at `Network.loadingFinished`,
//! and once again after a subsequent navigation. Prints a table and a verdict.
//!
//! Throwaway. Deleted once the answer lands in docs/adr/0002.
//!
//!   cargo run --example probe -- 8080

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

#[derive(Debug)]
struct Row {
    method: String,
    url: String,
    status: i64,
    eager_len: Option<usize>,
    eager_err: Option<String>,
    late_len: Option<usize>,
    late_err: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let port: u16 = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "8080".into())
        .parse()
        .context("usage: probe <port>")?;
    let target = format!("http://localhost:{port}/");

    let profile = std::env::temp_dir().join(format!("networkcop-probe-{}", std::process::id()));
    let debug_port = 9333u16;

    println!("launching chrome  → {target}");
    let mut chrome = tokio::process::Command::new("google-chrome")
        .args([
            &format!("--remote-debugging-port={debug_port}"),
            &format!("--user-data-dir={}", profile.display()),
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-features=Translate,MediaRouter",
            "about:blank",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn google-chrome")?;

    let ws_url = wait_for_target(debug_port).await?;
    println!("cdp target        → {ws_url}\n");

    let (mut sock, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .context("connect to CDP websocket")?;

    let mut id = 0i64;
    let mut next = || {
        id += 1;
        id
    };

    // enable the domains we care about
    for method in ["Network.enable", "Page.enable", "Runtime.enable", "Log.enable"] {
        let msg = json!({"id": next(), "method": method});
        sock.send(msg.to_string().into()).await?;
    }

    // requestId -> (method, url)
    let mut reqs: HashMap<String, (String, String)> = HashMap::new();
    // requestId -> status
    let mut status: HashMap<String, i64> = HashMap::new();
    let mut rows: Vec<(String, Row)> = Vec::new();
    // in-flight getResponseBody calls: callId -> requestId
    let mut pending: HashMap<i64, String> = HashMap::new();

    // navigate
    let nav_id = next();
    sock.send(
        json!({"id": nav_id, "method": "Page.navigate", "params": {"url": target}})
            .to_string()
            .into(),
    )
    .await?;

    // ---- pass 1: capture, fetching each body eagerly at loadingFinished ----
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(Some(Ok(msg))) = tokio::time::timeout(remaining, sock.next()).await else {
            break;
        };
        let Ok(v): Result<Value, _> = serde_json::from_str(msg.to_text().unwrap_or("{}")) else {
            continue;
        };

        // reply to one of our getResponseBody calls
        if let Some(call_id) = v.get("id").and_then(|x| x.as_i64()) {
            if let Some(rid) = pending.remove(&call_id) {
                let (len, err) = read_body_reply(&v);
                if let Some(slot) = rows.iter_mut().find(|(k, _)| *k == rid) {
                    slot.1.eager_len = len;
                    slot.1.eager_err = err;
                }
            }
            continue;
        }

        match v.get("method").and_then(|m| m.as_str()) {
            Some("Network.requestWillBeSent") => {
                let p = &v["params"];
                let rid = p["requestId"].as_str().unwrap_or_default().to_string();
                let method = p["request"]["method"].as_str().unwrap_or("?").to_string();
                let url = p["request"]["url"].as_str().unwrap_or("?").to_string();
                reqs.insert(rid, (method, url));
            }
            Some("Network.responseReceived") => {
                let p = &v["params"];
                let rid = p["requestId"].as_str().unwrap_or_default().to_string();
                status.insert(rid, p["response"]["status"].as_i64().unwrap_or(0));
            }
            Some("Network.loadingFinished") => {
                let rid = v["params"]["requestId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let Some((method, url)) = reqs.get(&rid).cloned() else {
                    continue;
                };
                if url.starts_with("data:") {
                    continue;
                }
                rows.push((
                    rid.clone(),
                    Row {
                        method,
                        url,
                        status: status.get(&rid).copied().unwrap_or(0),
                        eager_len: None,
                        eager_err: None,
                        late_len: None,
                        late_err: None,
                    },
                ));
                let call = next();
                pending.insert(call, rid.clone());
                sock.send(
                    json!({"id": call, "method": "Network.getResponseBody",
                           "params": {"requestId": rid}})
                    .to_string()
                    .into(),
                )
                .await?;
            }
            _ => {}
        }
    }

    println!("captured {} responses; navigating away…\n", rows.len());

    // ---- pass 2: navigate elsewhere, then re-fetch every body ----
    let nav2 = next();
    sock.send(
        json!({"id": nav2, "method": "Page.navigate",
               "params": {"url": "about:blank"}})
        .to_string()
        .into(),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    pending.clear();
    for (rid, _) in rows.iter() {
        let call = next();
        pending.insert(call, rid.clone());
        sock.send(
            json!({"id": call, "method": "Network.getResponseBody",
                   "params": {"requestId": rid}})
            .to_string()
            .into(),
        )
        .await?;
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while !pending.is_empty() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(Some(Ok(msg))) = tokio::time::timeout(remaining, sock.next()).await else {
            break;
        };
        let Ok(v): Result<Value, _> = serde_json::from_str(msg.to_text().unwrap_or("{}")) else {
            continue;
        };
        let Some(call_id) = v.get("id").and_then(|x| x.as_i64()) else {
            continue;
        };
        let Some(rid) = pending.remove(&call_id) else {
            continue;
        };
        let (len, err) = read_body_reply(&v);
        if let Some(slot) = rows.iter_mut().find(|(k, _)| *k == rid) {
            slot.1.late_len = len;
            slot.1.late_err = err;
        }
    }

    // ---- report ----
    println!(
        "{:<7} {:<44} {:>5} {:>11} {:>13}",
        "METHOD", "URL", "STAT", "EAGER", "AFTER_NAV"
    );
    println!("{}", "-".repeat(84));
    let (mut eager_ok, mut late_ok) = (0usize, 0usize);
    for (_, r) in rows.iter() {
        let eager = match (r.eager_len, &r.eager_err) {
            (Some(n), _) => {
                eager_ok += 1;
                format!("{n} B")
            }
            (None, Some(e)) => truncate(e, 11),
            _ => "—".into(),
        };
        let late = match (r.late_len, &r.late_err) {
            (Some(n), _) => {
                late_ok += 1;
                format!("{n} B")
            }
            (None, Some(e)) => truncate(e, 13),
            _ => "—".into(),
        };
        println!(
            "{:<7} {:<44} {:>5} {:>11} {:>13}",
            r.method,
            truncate(&r.url, 44),
            r.status,
            eager,
            late
        );
    }

    let total = rows.len();
    println!("\n{}", "=".repeat(84));
    println!("bodies at loadingFinished : {eager_ok}/{total}");
    println!("bodies after navigation   : {late_ok}/{total}");
    let verdict = if total == 0 {
        "INCONCLUSIVE — no responses captured"
    } else if late_ok == 0 {
        "body_after_nav: EVICTED — must fetch eagerly at loadingFinished"
    } else if late_ok < eager_ok {
        "body_after_nav: PARTIAL — some evicted; fetch eagerly to be safe"
    } else {
        "body_after_nav: OK — lazy fetch on modal open is safe"
    };
    println!("VERDICT → {verdict}");

    let _ = chrome.kill().await;
    let _ = std::fs::remove_dir_all(&profile);
    Ok(())
}

/// `{"result":{"body":"…","base64Encoded":false}}` or `{"error":{"message":"…"}}`
fn read_body_reply(v: &Value) -> (Option<usize>, Option<String>) {
    if let Some(err) = v.get("error").and_then(|e| e["message"].as_str()) {
        return (None, Some(err.to_string()));
    }
    match v.get("result").and_then(|r| r["body"].as_str()) {
        Some(b) => (Some(b.len()), None),
        None => (None, Some("no body field".into())),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

/// Poll /json/version until Chrome's debug endpoint answers, then find the page target.
async fn wait_for_target(debug_port: u16) -> Result<String> {
    let client = reqwest::Client::new();
    for _ in 0..60 {
        let list = client
            .get(format!("http://127.0.0.1:{debug_port}/json/list"))
            .send()
            .await;
        if let Ok(resp) = list {
            if let Ok(targets) = resp.json::<Vec<Value>>().await {
                if let Some(t) = targets
                    .iter()
                    .find(|t| t["type"] == "page" && t["webSocketDebuggerUrl"].is_string())
                {
                    return Ok(t["webSocketDebuggerUrl"].as_str().unwrap().to_string());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    bail!("chrome debug endpoint never came up on port {debug_port}")
}
