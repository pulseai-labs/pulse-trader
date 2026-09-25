//! MCP over HTTP (r3.s3.w5, AC-2): the SAME `PulseMcp` handler the stdio
//! transport serves, mounted on w1's router at `/mcp` under
//! [`crate::server::auth::Scope::Agent`].
//!
//! rmcp 3.4's `StreamableHttpService` is a tower service over `http-body`, so
//! it mounts through axum's `any_service` with no adapter crate in between —
//! and this file is the ONLY place outside the transport module that names
//! `rmcp::transport`, keeping `scripts/check-mcp-boundary.sh`'s seam rule
//! intact (src/server and src/cli stay rmcp-free; they consume the axum
//! [`MethodRouter`] this returns).
//!
//! Built over the server's own state (the spec's "the same pool, the candle
//! store at `data_dir`, exports under `data_dir`"): the factory clones the
//! server's pool handle per session, roots a store at the server's data dir,
//! and shares the ONE per-process exports directory. Identity follows the
//! spec's rule — the authenticated token's LABEL is the agent's final
//! identity — delivered as an axum extension by the auth middleware and read
//! in `PulseMcp::initialize` (see `mod.rs`).

use std::sync::Arc;

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, session::local::LocalSessionManager, tower::StreamableHttpService,
};

use super::{McpState, PulseMcp, export::Exports, identity::AgentIdentity};
use crate::adapters::broker::BinanceAdapter;
use crate::adapters::db::Db;
use crate::adapters::store::CandleStore;

/// The handles the `/mcp` mount needs from the server: the pool and the data
/// dir — the same two the server itself was built over.
pub(crate) struct McpHttpDeps {
    /// The server's own pool (the one-pool rule).
    pub(crate) db: Db,
    /// The server's data dir: the candle store's root and the exports parent.
    pub(crate) data_dir: std::path::PathBuf,
}

/// Build the `/mcp` route: an agent-scoped [`StreamableHttpService`] over a
/// per-session `PulseMcp` factory.
///
/// One shared exports directory is created HERE (per server process), not per
/// session — a session exports through the same durable directory the stdio
/// transport's single session would have.
///
/// # Errors
///
/// Returns the data error when the exports directory cannot be created — the
/// mount fails at startup, exactly like every other composition-root
/// failure, rather than failing per request.
pub(crate) fn mcp_http_router(
    deps: &McpHttpDeps,
) -> Result<axum::routing::MethodRouter, crate::domain::DataError> {
    let exports = Arc::new(Exports::create(&deps.data_dir)?);
    let db = deps.db.clone();
    let data_dir = deps.data_dir.clone();
    let factory = move || {
        Ok(PulseMcp::new(McpState {
            db: db.clone(),
            candles: CandleStore::with_base_dir(data_dir.clone()),
            exports: Arc::clone(&exports),
            // Upgraded at `initialize` from the authenticated token's label
            // (the request extension the auth middleware inserts); until then
            // the session is unidentified and `initialize` is required — the
            // protocol enforces that ordering for us.
            identity: AgentIdentity::resolve(None, None),
            exchange: BinanceAdapter::new(),
        }))
    };

    // The bind policy (D6) is the Host control: the server never accepts
    // connections off the tailnet, so the tower default's loopback-only
    // `allowed_hosts` — which would refuse the operator's tailnet hostname in
    // the `Host` header — is disabled to match the OUTER router's posture
    // (plain routes carry no Host validation either). One control, stated
    // once, at the bind.
    let config = StreamableHttpServerConfig::default().disable_allowed_hosts();
    // `LocalSessionManager` is `Default` (rmcp marks it `#[non_exhaustive]`
    // with no `new`).
    let service =
        StreamableHttpService::new(factory, Arc::new(LocalSessionManager::default()), config);
    Ok(axum::routing::any_service(service))
}
