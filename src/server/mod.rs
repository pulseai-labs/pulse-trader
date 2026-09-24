//! The `pulse serve` server ring (r3.s3.w1, ADR-0026): the axum router, the
//! server state, the version/fingerprint handshake (D4) and the scoped-route
//! mount seam w2/w5 build on.
//!
//! **The route surface of THIS item is the handshake and nothing else** — the
//! tests mount their own probe routes through [`mount_scoped`], and w2 adds the
//! command routes later. Every response (refusals and 404s included) carries
//! `X-Pulse-Api-Version: 1`, stamped by the outermost middleware.
//!
//! **Module layout:** `auth` carries the scope vocabulary, token minting and
//! the auth middleware; `log` carries the request-log layer and the injectable
//! sink. The bind policy (`bind`) is wired in by `pulse serve` startup.

pub mod auth;
pub mod bind;
pub mod log;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::Request;
use axum::response::IntoResponse;
use axum::routing::{MethodRouter, get};
use serde::Serialize;

use crate::adapters::db::Db;
use crate::domain::EngineFingerprint;

use auth::RequiredScope;
use log::RequestLog;

/// The API version this server speaks (D4). Bumped on any breaking change to
/// `/api/v1`; the handshake reports it and every response header carries it.
pub const API_VERSION: u32 = 1;

/// The server's shared state: the migrated pool, the snapshot base dir and the
/// injectable log sink. `data_dir` is public surface on purpose: w2's snapshot
/// routes resolve exports under it, and the composition root hands it in.
pub struct ServerState {
    pub(crate) db: Db,
    pub data_dir: PathBuf,
    pub(crate) log: Arc<dyn RequestLog>,
}

impl ServerState {
    /// The production constructor: stderr logging.
    #[must_use]
    pub fn new(db: Db, data_dir: PathBuf) -> Self {
        Self::with_log(db, data_dir, Arc::new(log::StderrLog))
    }

    /// The test seam: supply the log sink so request and startup lines are
    /// captured instead of printed.
    #[must_use]
    pub fn with_log(db: Db, data_dir: PathBuf, log: Arc<dyn RequestLog>) -> Self {
        Self { db, data_dir, log }
    }

    /// Borrow the state's log sink (startup lines go through the same sink as
    /// request lines).
    #[must_use]
    pub fn log(&self) -> &Arc<dyn RequestLog> {
        &self.log
    }
}

/// The handshake body (D4): which API version the server speaks, which binary
/// is running and which engine fingerprint it carries.
#[derive(Serialize)]
struct HandshakeBody {
    api_version: u32,
    binary_version: String,
    engine_fingerprint: String,
    target_triple: String,
}

/// `GET /api/v1/handshake` — accepted for either scope (the auth middleware is
/// mounted with [`RequiredScope::Any`]), so any live token can ask what is
/// running before sending real work.
async fn handshake() -> impl IntoResponse {
    Json(HandshakeBody {
        api_version: API_VERSION,
        binary_version: env!("CARGO_PKG_VERSION").to_owned(),
        engine_fingerprint: EngineFingerprint::current().as_str().to_owned(),
        target_triple: EngineFingerprint::target().to_owned(),
    })
}

/// Build the server router: the handshake under `Any` scope, the request-log
/// layer and the API-version layer wrapped around EVERYTHING (refusals and
/// 404s included — router-level layers wrap the fallback too). Routes mounted
/// afterwards via [`mount_scoped`] carry the same outer pair on themselves
/// (axum layers only wrap routes that exist when `.layer` runs).
pub fn router(state: Arc<ServerState>) -> axum::Router {
    // Each middleware owns its `Arc` (cloned per request inside the closure) —
    // a `move` closure must capture an owned handle, never a borrow.
    let handshake_state = state.clone();
    let handshake = get(handshake).layer(axum::middleware::from_fn(
        move |req: Request, next: axum::middleware::Next| {
            let state = handshake_state.clone();
            async move { auth::require_scope(req, next, state, RequiredScope::Any).await }
        },
    ));
    let log_state = state;
    axum::Router::new()
        .route("/api/v1/handshake", handshake)
        .layer(axum::middleware::from_fn(
            move |req: Request, next: axum::middleware::Next| {
                let state = log_state.clone();
                async move { log::request_log(req, next, state).await }
            },
        ))
        .layer(axum::middleware::from_fn(log::api_version_header))
}

/// Mount one route under a required [`Scope`] — the seam w2 (command routes)
/// and w5 (MCP) use, and the tests use for probe routes. The auth middleware
/// is attached to THIS route only; the router-level layers still wrap it.
///
/// Scope refusal semantics live in `auth`: a valid token of the wrong scope is
/// a 403 `scope_refused` with an audit row; a missing/unknown/revoked token is
/// refused exactly as on any other route.
pub fn mount_scoped(
    router: axum::Router,
    state: &Arc<ServerState>,
    scope: auth::Scope,
    path: &str,
    method_router: MethodRouter,
) -> axum::Router {
    // The middleware must own its handle: clone out of the reference HERE, then
    // `move` the owned `Arc` into the closure (it clones per request). Capturing
    // the `&Arc` itself would tie the middleware to this function's stack frame.
    let auth_state = state.clone();
    let log_state = state.clone();
    // axum layers wrap only the routes that exist when `.layer` runs — routes
    // mounted onto `router` AFTER `router()` returned would escape its
    // router-level request-log and API-version layers. So each scoped mount
    // carries the SAME outer pair itself (version outermost, log, auth
    // innermost), keeping the per-request behaviour identical on every route:
    // exactly one log line, exactly one version header, one auth pass.
    let scoped = method_router
        .layer(axum::middleware::from_fn(
            move |req: Request, next: axum::middleware::Next| {
                let state = auth_state.clone();
                async move { auth::require_scope(req, next, state, scope.into()).await }
            },
        ))
        .layer(axum::middleware::from_fn(
            move |req: Request, next: axum::middleware::Next| {
                let state = log_state.clone();
                async move { log::request_log(req, next, state).await }
            },
        ))
        .layer(axum::middleware::from_fn(log::api_version_header));
    router.route(path, scoped)
}
