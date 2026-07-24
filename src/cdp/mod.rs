//! Chrome launch + the CDP WebSocket client.
//!
//! Shape: one task owns the socket. Callers never touch it directly — they send
//! `Call`s down a channel and await a oneshot reply, which keeps request/response
//! correlation in one place. Captured events go out as `Capture` values.
//!
//! ADR-0002: response bodies are fetched EAGERLY at `Network.loadingFinished`.
//! The probe measured 0/554 bodies still retrievable after a navigation, so there
//! is no lazy path to fall back on.

pub mod proto;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use proto::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Semaphore};

/// How many body fetches may be in flight at once. One 12 MB body should not
/// stall the several hundred requests queued behind it.
const BODY_CONCURRENCY: usize = 16;

/// Everything the capture layer emits. The TUI and the DB writer both consume these.
#[derive(Debug, Clone)]
pub enum Capture {
    Request(Box<CapturedRequest>),
    Response(Box<CapturedResponse>),
    Body {
        request_id: String,
        body: Vec<u8>,
        base64: bool,
        truncated_from: Option<u64>,
    },
    Failed {
        request_id: String,
        error: String,
        canceled: bool,
    },
    Console(Box<CapturedConsole>),
    Navigated {
        url: String,
        frame_id: String,
        is_main: bool,
    },
    /// Chrome went away. The TUI shows this rather than silently freezing.
    Detached(String),
}

#[derive(Debug, Clone)]
pub struct CapturedRequest {
    pub request_id: String,
    pub method: String,
    pub url: String,
    pub headers: Headers,
    pub post_data: Option<String>,
    pub resource_type: Option<String>,
    pub wall_time: f64,
}

#[derive(Debug, Clone)]
pub struct CapturedResponse {
    pub request_id: String,
    pub status: i64,
    pub status_text: String,
    pub headers: Headers,
    pub mime_type: String,
    pub remote_ip: Option<String>,
    pub from_cache: bool,
    pub encoded_length: u64,
    pub duration_ms: f64,
}

#[derive(Debug, Clone)]
pub struct CapturedConsole {
    pub severity: String,
    pub text: String,
    pub url: Option<String>,
    pub line: Option<i64>,
    pub source: String,
}

/// A method call awaiting its reply.
struct Call {
    method: String,
    params: Value,
    reply: oneshot::Sender<Result<Value>>,
}

/// Handle to the live CDP connection.
#[derive(Clone)]
pub struct Cdp {
    calls: mpsc::Sender<Call>,
}

impl Cdp {
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        self.calls
            .send(Call {
                method: method.to_string(),
                params,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("cdp connection closed"))?;
        rx.await.map_err(|_| anyhow::anyhow!("cdp call dropped"))?
    }

    pub async fn navigate(&self, url: &str) -> Result<()> {
        self.call("Page.navigate", json!({ "url": url })).await?;
        Ok(())
    }

    /// Full-page HTML at this instant — the DOM snapshot stored alongside the session.
    pub async fn dom_snapshot(&self) -> Result<String> {
        let v = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": "document.documentElement.outerHTML",
                    "returnByValue": true
                }),
            )
            .await?;
        Ok(v["result"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    pub async fn current_url(&self) -> Result<String> {
        let v = self
            .call(
                "Runtime.evaluate",
                json!({ "expression": "location.href", "returnByValue": true }),
            )
            .await?;
        Ok(v["result"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }
}

/// A launched Chrome we are responsible for killing.
pub struct Browser {
    child: tokio::process::Child,
    profile: Option<PathBuf>,
}

impl Browser {
    pub async fn shutdown(mut self) {
        let _ = self.child.kill().await;
        if let Some(p) = self.profile.take() {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

pub struct LaunchOpts {
    pub port: u16,
    pub headless: bool,
    pub debug_port: u16,
    /// Reuse a real Chrome profile so authenticated apps capture real traffic
    /// (a fresh profile has no cookies — the session is then all login page).
    pub user_data_dir: Option<PathBuf>,
    pub chrome_binary: Option<String>,
    pub max_body: u64,
}

/// Launch Chrome, attach to its page target, enable the domains we consume, and
/// start pumping captures.
pub async fn launch(opts: &LaunchOpts) -> Result<(Browser, Cdp, mpsc::Receiver<Capture>)> {
    let (profile, ephemeral) = match &opts.user_data_dir {
        Some(p) => (p.clone(), false),
        None => (
            std::env::temp_dir().join(format!("networkcop-{}", std::process::id())),
            true,
        ),
    };

    let binary = opts.chrome_binary.clone().unwrap_or_else(default_chrome);
    let mut cmd = tokio::process::Command::new(&binary);
    cmd.arg(format!("--remote-debugging-port={}", opts.debug_port))
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-backgrounding-occluded-windows",
            "--disable-renderer-backgrounding",
            "--disable-features=Translate,MediaRouter,OptimizationHints",
        ]);
    if opts.headless {
        cmd.args(["--headless=new", "--disable-gpu"]);
    }
    cmd.arg("about:blank")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {binary} — is Chrome installed?"))?;
    let browser = Browser {
        child,
        profile: ephemeral.then_some(profile),
    };

    let ws_url = match wait_for_target(opts.debug_port).await {
        Ok(u) => u,
        Err(e) => {
            browser.shutdown().await;
            return Err(e);
        }
    };

    let (cdp, captures) = connect(&ws_url, opts.max_body).await?;
    Ok((browser, cdp, captures))
}

fn default_chrome() -> String {
    for c in [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "brave-browser",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ] {
        if c.starts_with('/') {
            if std::path::Path::new(c).exists() {
                return c.into();
            }
        } else if which(c) {
            return c.into();
        }
    }
    "google-chrome".into()
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p).any(|dir| {
                let f = dir.join(bin);
                f.is_file()
            })
        })
        .unwrap_or(false)
}

async fn wait_for_target(debug_port: u16) -> Result<String> {
    let client = reqwest::Client::new();
    for _ in 0..80 {
        if let Ok(resp) = client
            .get(format!("http://127.0.0.1:{debug_port}/json/list"))
            .send()
            .await
        {
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
    bail!("Chrome's debug endpoint never came up on port {debug_port}")
}

/// Own the socket in one task; everything else talks to it over channels.
async fn connect(ws_url: &str, max_body: u64) -> Result<(Cdp, mpsc::Receiver<Capture>)> {
    let (socket, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .context("connect to the Chrome DevTools websocket")?;
    let (mut sink, mut stream) = socket.split();

    let (call_tx, mut call_rx) = mpsc::channel::<Call>(64);
    // Bounded: a chatty page should slow capture, never balloon memory (ADR-0002).
    let (cap_tx, cap_rx) = mpsc::channel::<Capture>(1024);
    // Internal loop-back so the body-fetch tasks can issue calls too.
    let cdp = Cdp {
        calls: call_tx.clone(),
    };

    for method in [
        "Network.enable",
        "Page.enable",
        "Runtime.enable",
        "Log.enable",
    ] {
        sink.send(json!({"id": 0, "method": method}).to_string().into())
            .await
            .context("enable CDP domains")?;
    }

    let pending: Arc<tokio::sync::Mutex<HashMap<i64, oneshot::Sender<Result<Value>>>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    // --- writer task: serialise every outbound call ---
    {
        let pending = pending.clone();
        tokio::spawn(async move {
            let mut next_id: i64 = 1000;
            while let Some(call) = call_rx.recv().await {
                next_id += 1;
                let id = next_id;
                pending.lock().await.insert(id, call.reply);
                let msg = json!({"id": id, "method": call.method, "params": call.params});
                if sink.send(msg.to_string().into()).await.is_err() {
                    if let Some(tx) = pending.lock().await.remove(&id) {
                        let _ = tx.send(Err(anyhow::anyhow!("cdp socket closed")));
                    }
                    break;
                }
            }
        });
    }

    // --- reader task: dispatch replies and translate events into captures ---
    {
        let cdp = cdp.clone();
        let cap_tx = cap_tx.clone();
        let pending = pending.clone();
        let body_slots = Arc::new(Semaphore::new(BODY_CONCURRENCY));

        tokio::spawn(async move {
            // requestId -> (start timestamp, url) so we can compute a duration
            let mut started: HashMap<String, (f64, String)> = HashMap::new();
            let mut main_frame: Option<String> = None;

            while let Some(Ok(raw)) = stream.next().await {
                let Ok(text) = raw.into_text() else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };

                // a reply to one of our calls
                if let Some(id) = v.get("id").and_then(|x| x.as_i64()) {
                    if let Some(tx) = pending.lock().await.remove(&id) {
                        let out = match v.get("error") {
                            Some(e) => Err(anyhow::anyhow!(
                                "{}",
                                e["message"].as_str().unwrap_or("cdp error")
                            )),
                            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(out);
                    }
                    continue;
                }

                let Some(method) = v.get("method").and_then(|m| m.as_str()) else {
                    continue;
                };
                let params = v.get("params").cloned().unwrap_or(Value::Null);

                match method {
                    "Network.requestWillBeSent" => {
                        let Ok(e) = serde_json::from_value::<RequestWillBeSent>(params) else {
                            continue;
                        };
                        started.insert(e.request_id.clone(), (e.timestamp, e.request.url.clone()));
                        let cap = CapturedRequest {
                            request_id: e.request_id,
                            method: e.request.method,
                            url: e.request.url,
                            headers: e.request.headers,
                            post_data: e.request.post_data,
                            resource_type: e.r#type,
                            wall_time: e.wall_time,
                        };
                        if cap_tx.send(Capture::Request(Box::new(cap))).await.is_err() {
                            break;
                        }
                    }
                    "Network.responseReceived" => {
                        let Ok(e) = serde_json::from_value::<ResponseReceived>(params) else {
                            continue;
                        };
                        let dur = started
                            .get(&e.request_id)
                            .map(|(t0, _)| (e.timestamp - t0) * 1000.0)
                            .unwrap_or(0.0);
                        let cap = CapturedResponse {
                            request_id: e.request_id,
                            status: e.response.status,
                            status_text: e.response.status_text,
                            headers: e.response.headers,
                            mime_type: e.response.mime_type,
                            remote_ip: e.response.remote_ip_address,
                            from_cache: e.response.from_disk_cache.unwrap_or(false),
                            encoded_length: 0,
                            duration_ms: dur.max(0.0),
                        };
                        if cap_tx.send(Capture::Response(Box::new(cap))).await.is_err() {
                            break;
                        }
                    }
                    "Network.loadingFinished" => {
                        let Ok(e) = serde_json::from_value::<LoadingFinished>(params) else {
                            continue;
                        };
                        // EAGER body fetch — this is the only moment it is available.
                        let cdp = cdp.clone();
                        let cap_tx = cap_tx.clone();
                        let slots = body_slots.clone();
                        let rid = e.request_id.clone();
                        let encoded = e.encoded_data_length.max(0.0) as u64;
                        tokio::spawn(async move {
                            let _permit = match slots.acquire().await {
                                Ok(p) => p,
                                Err(_) => return,
                            };
                            let res = cdp
                                .call(
                                    "Network.getResponseBody",
                                    json!({ "requestId": rid.clone() }),
                                )
                                .await;
                            let Ok(v) = res else { return };
                            let Ok(b) = serde_json::from_value::<GetResponseBody>(v) else {
                                return;
                            };
                            let full = b.body.len() as u64;
                            let (bytes, truncated_from) = if full > max_body {
                                (
                                    b.body.as_bytes()[..max_body as usize].to_vec(),
                                    Some(encoded.max(full)),
                                )
                            } else {
                                (b.body.into_bytes(), None)
                            };
                            let _ = cap_tx
                                .send(Capture::Body {
                                    request_id: rid,
                                    body: bytes,
                                    base64: b.base64_encoded,
                                    truncated_from,
                                })
                                .await;
                        });
                    }
                    "Network.loadingFailed" => {
                        let Ok(e) = serde_json::from_value::<LoadingFailed>(params) else {
                            continue;
                        };
                        if cap_tx
                            .send(Capture::Failed {
                                request_id: e.request_id,
                                error: e.error_text,
                                canceled: e.canceled.unwrap_or(false),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    "Log.entryAdded" => {
                        let Ok(e) = serde_json::from_value::<LogEntry>(params["entry"].clone())
                        else {
                            continue;
                        };
                        let cap = CapturedConsole {
                            severity: severity_of(&e.level).to_string(),
                            text: e.text,
                            url: e.url,
                            line: e.line_number,
                            source: e.source.unwrap_or_else(|| "log".into()),
                        };
                        if cap_tx.send(Capture::Console(Box::new(cap))).await.is_err() {
                            break;
                        }
                    }
                    "Runtime.consoleAPICalled" => {
                        let Ok(e) = serde_json::from_value::<ConsoleApiCalled>(params) else {
                            continue;
                        };
                        let text = e
                            .args
                            .iter()
                            .map(|a| a.render())
                            .collect::<Vec<_>>()
                            .join(" ");
                        let cap = CapturedConsole {
                            severity: severity_of(&e.r#type).to_string(),
                            text,
                            url: None,
                            line: None,
                            source: "console".into(),
                        };
                        if cap_tx.send(Capture::Console(Box::new(cap))).await.is_err() {
                            break;
                        }
                    }
                    "Runtime.exceptionThrown" => {
                        let Ok(e) = serde_json::from_value::<ExceptionThrown>(params) else {
                            continue;
                        };
                        let d = e.exception_details;
                        let text = d
                            .exception
                            .as_ref()
                            .map(|x| x.render())
                            .filter(|s| !s.is_empty())
                            .unwrap_or(d.text);
                        let cap = CapturedConsole {
                            severity: "error".into(),
                            text,
                            url: d.url,
                            line: d.line_number,
                            source: "exception".into(),
                        };
                        if cap_tx.send(Capture::Console(Box::new(cap))).await.is_err() {
                            break;
                        }
                    }
                    // SPA routing: history.pushState/replaceState never fires
                    // frameNavigated, so without this the page filter would be
                    // dead on every client-routed app.
                    "Page.navigatedWithinDocument" => {
                        let url = params["url"].as_str().unwrap_or_default().to_string();
                        let frame_id = params["frameId"].as_str().unwrap_or_default().to_string();
                        // only the main frame changes "the page"
                        let is_main = main_frame.as_deref().map(|m| m == frame_id).unwrap_or(true);
                        if url.is_empty() {
                            continue;
                        }
                        if cap_tx
                            .send(Capture::Navigated {
                                url,
                                frame_id,
                                is_main,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    "Page.frameNavigated" => {
                        let Ok(e) = serde_json::from_value::<FrameNavigated>(params) else {
                            continue;
                        };
                        let is_main = e.frame.parent_id.is_none();
                        if is_main {
                            main_frame = Some(e.frame.id.clone());
                        }
                        if cap_tx
                            .send(Capture::Navigated {
                                url: e.frame.url,
                                frame_id: e.frame.id,
                                is_main,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let _ = cap_tx
                .send(Capture::Detached("Chrome disconnected".into()))
                .await;
        });
    }

    Ok((cdp, cap_rx))
}
