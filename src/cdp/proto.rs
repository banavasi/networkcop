//! The slice of the Chrome DevTools Protocol networkcop actually consumes.
//!
//! CDP is enormous and mostly irrelevant here. Rather than model it, we deserialise
//! the handful of event payloads we act on and keep the raw `Value` for anything
//! else, so an unexpected field is never a parse error.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// Headers arrive as a JSON object; BTreeMap keeps them stably ordered for display
/// and for the YAML exports, which would otherwise churn between runs.
pub type Headers = BTreeMap<String, String>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestWillBeSent {
    pub request_id: String,
    pub loader_id: String,
    pub request: Request,
    #[serde(default)]
    pub timestamp: f64,
    #[serde(default)]
    pub wall_time: f64,
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub redirect_response: Option<Response>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub url: String,
    pub method: String,
    #[serde(default)]
    pub headers: Headers,
    #[serde(default)]
    pub post_data: Option<String>,
    #[serde(default)]
    pub has_post_data: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseReceived {
    pub request_id: String,
    pub response: Response,
    #[serde(default)]
    pub timestamp: f64,
    #[serde(default)]
    pub r#type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub url: String,
    pub status: i64,
    #[serde(default)]
    pub status_text: String,
    #[serde(default)]
    pub headers: Headers,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub remote_ip_address: Option<String>,
    #[serde(default)]
    pub from_disk_cache: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadingFinished {
    pub request_id: String,
    #[serde(default)]
    pub timestamp: f64,
    #[serde(default)]
    pub encoded_data_length: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadingFailed {
    pub request_id: String,
    #[serde(default)]
    pub timestamp: f64,
    #[serde(default)]
    pub error_text: String,
    #[serde(default)]
    pub canceled: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetResponseBody {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub base64_encoded: bool,
}

/// `Log.entryAdded`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub timestamp: f64,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub line_number: Option<i64>,
    #[serde(default)]
    pub source: Option<String>,
}

/// `Runtime.consoleAPICalled` — console.log/warn/error from page script.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleApiCalled {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub args: Vec<RemoteObject>,
    #[serde(default)]
    pub timestamp: f64,
}

/// `Runtime.exceptionThrown` — the uncaught errors that matter most when debugging.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExceptionThrown {
    #[serde(default)]
    pub timestamp: f64,
    pub exception_details: ExceptionDetails,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExceptionDetails {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub exception: Option<RemoteObject>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub line_number: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteObject {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub unserializable_value: Option<String>,
}

impl RemoteObject {
    /// Best-effort rendering of a console argument as the user would see it.
    pub fn render(&self) -> String {
        if let Some(d) = &self.description {
            return d.clone();
        }
        if let Some(u) = &self.unserializable_value {
            return u.clone();
        }
        match &self.value {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => self.r#type.clone(),
        }
    }
}

/// `Page.frameNavigated`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameNavigated {
    pub frame: Frame,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Frame {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub url: String,
}

/// Map a CDP log level / console API type onto our four severities.
pub fn severity_of(level: &str) -> &'static str {
    match level {
        "error" | "assert" | "critical" => "error",
        "warning" | "warn" => "warn",
        "debug" | "verbose" | "trace" => "debug",
        _ => "info",
    }
}
