//! The injectable request log and the two response-mapping middlewares
//! (r3.s3.w1 — the gate's no-secret-in-any-log control).
//!
//! **One stderr line per request** through an injectable sink:
//! `pulse serve: <METHOD> <path> <status> <label or -> <elapsed>ms`. The path
//! is logged WITHOUT its query string; no header value is ever logged; the
//! path passes through the domain [`Redactor`] before emission (the
//! reconciliation ruling: reuse the Redactor on anything that echoes input).
//! Startup lines (listening, bind retries, shutdown) go through the same sink.
//! Nothing else is logged.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;

use crate::domain::Redactor;

use super::ServerState;
use super::auth::AuthenticatedLabel;

/// Where log lines go. Production writes to stderr; tests capture.
pub trait RequestLog: Send + Sync {
    /// Write one complete line.
    fn write(&self, line: String);
}

/// The production sink: one line to stderr.
pub struct StderrLog;

impl RequestLog for StderrLog {
    fn write(&self, line: String) {
        eprintln!("{line}");
    }
}

/// The test sink: lines are captured for inspection.
#[derive(Default)]
pub struct CaptureLog {
    lines: Mutex<Vec<String>>,
}

impl CaptureLog {
    /// A snapshot of the captured lines, oldest first.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        self.lock().clone()
    }

    /// Lock the line buffer, recovering from a poisoned mutex (a panicking
    /// test must not take the log down with it).
    fn lock(&self) -> MutexGuard<'_, Vec<String>> {
        self.lines.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl RequestLog for CaptureLog {
    fn write(&self, line: String) {
        self.lock().push(line);
    }
}

/// The request-log middleware (router-level, so the fallback is logged too).
/// The label comes from the response extensions the auth middleware inserted;
/// a refused or fallback request has none and logs `-`.
pub(crate) async fn request_log(req: Request, next: Next, state: Arc<ServerState>) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let raw_path = req.uri().path().to_owned();
    let resp = next.run(req).await;
    let label = resp
        .extensions()
        .get::<AuthenticatedLabel>()
        .map_or_else(|| "-".to_owned(), |l| l.0.clone());
    let status = resp.status().as_u16();
    let elapsed_ms = start.elapsed().as_millis();
    // The path is user-influenced input echoed into a log: scrub it through
    // the pure redaction kernel (no tagged secrets — the structural pass only).
    let path = Redactor::from_config(Vec::new()).redact(&raw_path);
    state.log.write(format!(
        "pulse serve: {method} {path} {status} {label} {elapsed_ms}ms"
    ));
    resp
}

/// The API-version middleware (outermost): EVERY response from this server —
/// refusals and 404s included — carries `X-Pulse-Api-Version: 1` (D4).
pub(crate) async fn api_version_header(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    resp.headers_mut()
        .insert("x-pulse-api-version", HeaderValue::from_static("1"));
    resp
}
