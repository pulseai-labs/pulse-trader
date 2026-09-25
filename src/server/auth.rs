//! The scope vocabulary, token minting/hashing and the auth middleware
//! (r3.s3.w1, D5 — the Network listener and client auth gate's token control).
//!
//! **Scopes do not nest.** An `agent` token is refused on an `app` route and
//! the reverse; the handshake accepts either ([`RequiredScope::Any`]).
//!
//! **The token format** (D5): the prefix `pt_` followed by the unpadded
//! base64url encoding of 32 bytes from the OS CSPRNG (43 characters). Only the
//! SHA-256 hex of the full token string is ever stored.
//!
//! **The refusal table** (D5, exactly):
//!
//! | condition | status | body `code` | audit `event`/reason |
//! |---|---|---|---|
//! | no `Authorization`, or not `Bearer` | 401 | `token_missing` | `refused`/`missing` |
//! | hash not found | 401 | `token_refused` | `refused`/`unknown` |
//! | token revoked | 401 | `token_refused` | `refused`/`revoked` |
//! | wrong scope | 403 | `scope_refused` | `refused`/`scope` |
//!
//! Every refusal body is JSON `{code, message}` whose message names the reason
//! in words and NEVER echoes the presented value; every refusal writes exactly
//! one `refused` audit row whose `route` is the method + path (never a query
//! string, never a header value — and the path passes the same structural
//! [`Redactor`] pass the request log runs, since a path is user-influenced
//! input) and whose `peer` is the remote IP. A request
//! that authenticates carries the token's label into the request AND response
//! extensions, so the log line and later handlers can name the client.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::adapters::db::{ClientToken, SqliteClientTokenRepo};
use crate::domain::Redactor;

use super::ServerState;

/// The bearer-token prefix (D5).
pub const TOKEN_PREFIX: &str = "pt_";

/// The scope of a token or a route (D5): `app` is the human surface, `agent`
/// is for MCP clients. Scopes do NOT nest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Scope {
    /// The human surface (the Mac app).
    App,
    /// MCP clients (w5 mounts `/mcp` under it).
    Agent,
}

impl Scope {
    /// The stored/checked spelling of the scope.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::App => "app",
            Scope::Agent => "agent",
        }
    }
}

/// The requirement a mounted route carries. `Any` is the handshake's
/// "either scope" mount; it still requires a valid, non-revoked token.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RequiredScope {
    /// Only an `app` token passes.
    App,
    /// Only an `agent` token passes.
    Agent,
    /// Either scope passes.
    Any,
}

impl From<Scope> for RequiredScope {
    fn from(scope: Scope) -> Self {
        match scope {
            Scope::App => RequiredScope::App,
            Scope::Agent => RequiredScope::Agent,
        }
    }
}

/// The accepted client's label, inserted into the request AND response
/// extensions by the auth middleware — the log line and later handlers name
/// the client from here.
#[derive(Debug, Clone)]
pub struct AuthenticatedLabel(pub String);

/// Mint one bearer token: `pt_` + unpadded base64url of 32 OS-CSPRNG bytes
/// (46 characters total, 43 after the prefix).
///
/// # Panics
///
/// Only if the operating system's secure entropy source fails, which is not a
/// recoverable condition for a security-critical secret.
#[must_use]
pub fn mint_token() -> String {
    let mut bytes = [0u8; 32];
    if let Err(err) = getrandom::fill(&mut bytes) {
        panic!("OS CSPRNG failed; cannot mint a token: {err}");
    }
    format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// The SHA-256 hex (lowercase) of the full token string — the ONLY form of a
/// token that is ever stored.
#[must_use]
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Extract the bearer token from the `Authorization` header. `Err(())` means
/// the header is missing, not `Bearer`, or carries no value — all one refusal
/// reason (`missing`, per the table).
fn bearer_token(headers: &axum::http::HeaderMap) -> Result<String, ()> {
    let value = headers.get(AUTHORIZATION).ok_or(())?;
    let value = value.to_str().map_err(|_| ())?;
    let rest = value.strip_prefix("Bearer ").ok_or(())?;
    if rest.is_empty() {
        return Err(());
    }
    Ok(rest.to_owned())
}

/// The refusal message for one reason: named in words, never echoing the
/// presented value.
fn refusal_message(reason: &str) -> &'static str {
    match reason {
        "missing" => "an Authorization header with a Bearer token is required",
        "unknown" => "the presented token is not recognised",
        "revoked" => "the presented token has been revoked",
        "scope" => "the token's scope does not cover this route",
        _ => "the request was refused",
    }
}

/// The auth middleware for one mounted route. See the module docs for the
/// refusal table; a request that authenticates carries its label onward.
pub(crate) async fn require_scope(
    req: Request,
    next: Next,
    state: Arc<ServerState>,
    required: RequiredScope,
) -> Response {
    // Capture what the audit row needs BEFORE the request is consumed.
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string());
    // The path is user-influenced input that lands in the refusal audit row and
    // in the lookup-failure log line: it goes through the SAME structural
    // redaction pass the request log runs (log.rs) before either is written.
    let route = format!(
        "{method} {}",
        Redactor::from_config(Vec::new()).redact(&path)
    );

    let repo = SqliteClientTokenRepo::new(state.db.pool().clone());

    let Ok(token) = bearer_token(req.headers()) else {
        return refuse(
            &state,
            &route,
            peer.as_deref(),
            None,
            &Refusal {
                status: StatusCode::UNAUTHORIZED,
                code: "token_missing",
                reason: "missing",
            },
        )
        .await;
    };

    let found = match repo.find_by_hash(&hash_token(&token)).await {
        Ok(found) => found,
        Err(err) => {
            // A database failure is not a client refusal: no audit row (the
            // failure is on our side), a bare 500, nothing echoed.
            state
                .log
                .write(format!("pulse serve: auth lookup failed on {route}: {err}"));
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let Some(row) = found else {
        return refuse(
            &state,
            &route,
            peer.as_deref(),
            None,
            &Refusal {
                status: StatusCode::UNAUTHORIZED,
                code: "token_refused",
                reason: "unknown",
            },
        )
        .await;
    };
    if row.revoked_at.is_some() {
        return refuse(
            &state,
            &route,
            peer.as_deref(),
            Some(&row),
            &Refusal {
                status: StatusCode::UNAUTHORIZED,
                code: "token_refused",
                reason: "revoked",
            },
        )
        .await;
    }
    let scope_ok = match required {
        RequiredScope::Any => true,
        RequiredScope::App => row.scope == Scope::App.as_str(),
        RequiredScope::Agent => row.scope == Scope::Agent.as_str(),
    };
    if !scope_ok {
        return refuse(
            &state,
            &route,
            peer.as_deref(),
            Some(&row),
            &Refusal {
                status: StatusCode::FORBIDDEN,
                code: "scope_refused",
                reason: "scope",
            },
        )
        .await;
    }

    let label = AuthenticatedLabel(row.label.clone());
    let mut req = req;
    req.extensions_mut().insert(label.clone());
    let mut resp = next.run(req).await;
    resp.extensions_mut().insert(label);
    resp
}

/// One refusal's fixed shape: status, body `code`, audit `reason`.
struct Refusal {
    status: StatusCode,
    code: &'static str,
    reason: &'static str,
}

/// Write one `refused` audit row (best-effort) and build the refusal response.
/// The message names the reason in words and never echoes the presented value.
async fn refuse(
    state: &Arc<ServerState>,
    route: &str,
    peer: Option<&str>,
    row: Option<&ClientToken>,
    refusal: &Refusal,
) -> Response {
    let repo = SqliteClientTokenRepo::new(state.db.pool().clone());
    let audit = repo
        .audit_append(
            "refused",
            row.map(|r| r.id.as_str()),
            row.map(|r| r.label.as_str()),
            Some(refusal.reason),
            Some(route),
            peer,
        )
        .await;
    if let Err(err) = audit {
        // The refusal stands even when its audit row could not be written;
        // the failure is named on the log sink, never shown to the client.
        state.log.write(format!(
            "pulse serve: audit write failed for {route}: {err}"
        ));
    }
    let body = json!({
        "code": refusal.code,
        "message": refusal_message(refusal.reason),
    });
    (refusal.status, Json(body)).into_response()
}
