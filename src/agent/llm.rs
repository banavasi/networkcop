//! The reasoner: `claude -p` on the user's existing subscription, plus the
//! validation layer that decides whether anything it said is allowed through.
//!
//! ADR-0002 measured `--exclude-dynamic-system-prompt-sections --strict-mcp-config`
//! at 41% cheaper per turn ($0.0433 → $0.0255), so both are always on.

use super::prompt::{REFUSAL, SYSTEM};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const TIMEOUT: Duration = Duration::from_secs(180);

/// Which reasoning engine answers free-form questions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// `claude -p` subprocess — the default, no API key needed.
    ClaudeCli { model: String },
    /// The Python LangGraph sidecar under `agent/`, over HTTP.
    Sidecar { url: String },
}

impl Backend {
    pub fn describe(&self) -> String {
        match self {
            Backend::ClaudeCli { model } => format!("claude -p ({model})"),
            Backend::Sidecar { url } => format!("sidecar {url}"),
        }
    }
}

/// What the model is contractually required to return.
#[derive(Debug, Deserialize)]
struct Envelope {
    in_scope: bool,
    #[serde(default)]
    answer: String,
}

pub struct Answer {
    pub text: String,
    pub cost_usd: f64,
    /// True when the guardrail (prompt or validator) blocked the reply.
    pub refused: bool,
}

/// Ask a free-form question about the session.
///
/// `context` is the rendered session digest — the only ground truth the model gets.
pub async fn ask(backend: &Backend, context: &str, question: &str) -> Result<Answer> {
    let payload = format!("{context}\n\nUSER QUESTION: {question}");
    let (raw, cost) = match backend {
        Backend::ClaudeCli { model } => claude_cli(model, &payload).await?,
        Backend::Sidecar { url } => sidecar(url, &payload).await?,
    };
    Ok(validate(&raw, cost))
}

/// The output-validation layer. Anything that is not a well-formed, in-scope
/// envelope becomes the canned refusal — the model's prose is never shown raw.
///
/// This is deliberately unconditional: it runs regardless of which backend
/// produced the text, so swapping the reasoner cannot weaken the guardrail.
pub fn validate(raw: &str, cost_usd: f64) -> Answer {
    let Some(env) = parse_envelope(raw) else {
        return Answer {
            text: REFUSAL.into(),
            cost_usd,
            refused: true,
        };
    };
    if !env.in_scope || env.answer.trim().is_empty() {
        return Answer {
            text: REFUSAL.into(),
            cost_usd,
            refused: true,
        };
    }
    Answer {
        text: env.answer,
        cost_usd,
        refused: false,
    }
}

/// Tolerates fenced JSON and leading chatter; rejects everything else.
fn parse_envelope(raw: &str) -> Option<Envelope> {
    let t = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let slice = match (t.find('{'), t.rfind('}')) {
        (Some(a), Some(b)) if b > a => &t[a..=b],
        _ => return None,
    };
    serde_json::from_str::<Envelope>(slice).ok()
}

async fn claude_cli(model: &str, payload: &str) -> Result<(String, f64)> {
    let mut cmd = tokio::process::Command::new("claude");
    cmd.args([
        "-p",
        "--output-format",
        "json",
        "--model",
        model,
        "--system-prompt",
        SYSTEM,
        // measured 41% cheaper per turn (ADR-0002)
        "--exclude-dynamic-system-prompt-sections",
        "--strict-mcp-config",
        // the agent reasons over the session; it never touches disk or network
        "--disallowed-tools",
        "Bash Edit Write Read Glob Grep WebFetch WebSearch Task NotebookEdit",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true);
    // don't let the child believe it is nested inside another claude session
    for var in ["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SSE_PORT"] {
        cmd.env_remove(var);
    }

    let mut child = cmd
        .spawn()
        .context("spawn `claude` — is the Claude Code CLI on PATH?")?;
    child
        .stdin
        .take()
        .context("claude stdin")?
        .write_all(payload.as_bytes())
        .await?;

    let out = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .context("claude -p timed out")??;
    if !out.status.success() {
        anyhow::bail!("claude -p exited {}", out.status);
    }
    let v: Value = serde_json::from_slice(&out.stdout).context("parse claude -p envelope")?;
    if v["is_error"].as_bool().unwrap_or(false) {
        anyhow::bail!(
            "claude reported an error: {}",
            v["result"].as_str().unwrap_or("unknown")
        );
    }
    Ok((
        v["result"].as_str().unwrap_or_default().to_string(),
        v["total_cost_usd"].as_f64().unwrap_or(0.0),
    ))
}

async fn sidecar(url: &str, payload: &str) -> Result<(String, f64)> {
    let client = reqwest::Client::builder().timeout(TIMEOUT).build()?;
    let resp = client
        .post(format!("{}/ask", url.trim_end_matches('/')))
        .json(&serde_json::json!({ "system": SYSTEM, "input": payload }))
        .send()
        .await
        .with_context(|| format!("POST {url}/ask — is the sidecar running?"))?
        .error_for_status()?;
    let v: Value = resp.json().await?;
    Ok((
        v["result"].as_str().unwrap_or_default().to_string(),
        v["cost_usd"].as_f64().unwrap_or(0.0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_clean_in_scope_envelope() {
        let a = validate(r#"{"in_scope": true, "answer": "POST /x returned 500."}"#, 0.02);
        assert!(!a.refused);
        assert_eq!(a.text, "POST /x returned 500.");
    }

    #[test]
    fn accepts_fenced_json() {
        let a = validate("```json\n{\"in_scope\":true,\"answer\":\"ok\"}\n```", 0.0);
        assert!(!a.refused);
        assert_eq!(a.text, "ok");
    }

    #[test]
    fn out_of_scope_becomes_the_canned_refusal() {
        let a = validate(
            r#"{"in_scope": false, "answer": "Ontology is the study of being."}"#,
            0.0,
        );
        assert!(a.refused);
        assert_eq!(a.text, REFUSAL);
        assert!(!a.text.contains("Ontology"), "model prose must not leak");
    }

    #[test]
    fn unparseable_output_is_refused_not_passed_through() {
        // the /reproduce parse failure observed in the guard spike
        for raw in [
            "```bash\ncurl -X POST http://x\n```",
            "Sure! Here's the answer: 17 * 43 = 731",
            "",
            "{\"in_scope\": true}", // missing answer
            "{\"in_scope\": true, \"answer\": \"   \"}",
        ] {
            let a = validate(raw, 0.0);
            assert!(a.refused, "should refuse: {raw:?}");
            assert_eq!(a.text, REFUSAL);
        }
    }

    #[test]
    fn cost_is_reported_even_when_refused() {
        let a = validate("garbage", 0.0255);
        assert!(a.refused);
        assert!((a.cost_usd - 0.0255).abs() < f64::EPSILON);
    }
}
