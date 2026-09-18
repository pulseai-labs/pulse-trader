//! `src/mcp/` — the `pulse mcp` delivery ring (r2.s1.w2).
//!
//! An MCP server over stdio: stdout carries ONLY protocol frames, diagnostics
//! go to stderr, and the ring names no secrets/LLM adapter so the credential
//! gate holds by construction (`scripts/check-mcp-boundary.sh` scans for it).
//! The seven tools are all read-only; `w3` appends the two write tools to
//! `tools.rs` and `w4` adds desktop parity.
//!
//! The import boundary (enforced by the script): `crate::domain`,
//! `crate::application`, `crate::adapters::{db, store, indicators, backtest}`
//! and `rmcp` only — never `adapters::secrets`, `adapters::llm`, `agent`, or
//! `cli`.

pub(crate) mod export;
pub(crate) mod identity;
pub(crate) mod resources;
pub(crate) mod tools;

use std::sync::{Mutex, MutexGuard, PoisonError};

use rmcp::model::{
    Implementation, InitializeRequestParams, InitializeResult, ListResourcesResult,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities,
};
use rmcp::service::RequestContext;
use rmcp::transport::stdio;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt, tool_handler};

use crate::adapters::db::Db;
use crate::adapters::store::CandleStore;

use export::Exports;
use identity::AgentIdentity;

/// Everything `serve` needs, resolved by the CLI composition root.
///
/// `identity` is the startup-resolved [`AgentIdentity`] — final when its
/// source is `Flag`; otherwise the handler upgrades it once at `initialize`
/// from `clientInfo.name` (the precedence table lives in `identity.rs`).
pub(crate) struct McpState {
    /// The migrated `pulse.db` pool handle.
    pub(crate) db: Db,
    /// The candle store rooted at the app data dir.
    pub(crate) candles: CandleStore,
    /// The per-process exports directory handle (`<data dir>/exports/<pid>-<start>/`).
    pub(crate) exports: Exports,
    /// The flag-resolved identity (`Flag` or the not-yet-upgraded `Unknown`).
    pub(crate) identity: AgentIdentity,
}

/// The `pulse mcp` server handler: the tool router lives in `tools.rs`, the
/// resource surface in `resources.rs`.
pub(crate) struct PulseMcp {
    state: McpState,
    /// The session identity — upgraded once at `initialize` when no flag was
    /// given. `Mutex`, not the handler: `ServerHandler` methods take `&self`.
    identity: Mutex<AgentIdentity>,
    tool_router: rmcp::handler::server::tool::ToolRouter<Self>,
}

impl PulseMcp {
    fn new(state: McpState) -> Self {
        Self {
            identity: Mutex::new(state.identity.clone()),
            state,
            tool_router: Self::tool_router(),
        }
    }

    fn identity_lock(&self) -> MutexGuard<'_, AgentIdentity> {
        self.identity.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[tool_handler(router = self.tool_router)]
// The ServerHandler trait requires `async fn` signatures; the handshake and
// resource bodies are legitimately synchronous, so the impl-level allow is
// scoped to exactly that macro-required lint.
#[allow(clippy::unused_async_trait_impl)]
impl ServerHandler for PulseMcp {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("pulse", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "Read-only access to the PulseTrader strategy library, backtest runs, \
             candle snapshots and indicator series, plus the pulse://dsl/schema \
             grammar. Write tools land in a later work item.",
        )
    }

    /// Capture `clientInfo.name` into the resolved identity — the handshake
    /// half of the precedence table — before the default negotiation runs.
    /// A flag-resolved identity is final and is never overwritten.
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        {
            let mut identity = self.identity_lock();
            if identity.source != identity::AgentNameSource::Flag {
                *identity = AgentIdentity::resolve(None, Some(&request.client_info.name));
            }
            let resolved = identity.clone();
            drop(identity);
            eprintln!(
                "pulse mcp: agent identity '{}' (source: {:?})",
                resolved.name, resolved.source
            );
        }
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    /// The one resource this server advertises: `pulse://dsl/schema`.
    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let resource = Resource::new(resources::DSL_SCHEMA_URI, resources::DSL_SCHEMA_NAME)
            .with_description(
                "The PulseTrader strategy DSL grammar as JSON Schema, plus the conventions an agent needs before composing a variant.",
            )
            .with_mime_type(resources::DSL_SCHEMA_MIME);
        Ok(ListResourcesResult {
            resources: vec![resource],
            ..ListResourcesResult::default()
        })
    }

    /// Read `pulse://dsl/schema`; any other URI is a protocol-level error.
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        if request.uri != resources::DSL_SCHEMA_URI {
            return Err(McpError::invalid_params(
                format!("unknown resource {}", request.uri),
                None,
            ));
        }
        let mut contents =
            ResourceContents::text(resources::dsl_schema_json(), resources::DSL_SCHEMA_URI);
        if let ResourceContents::TextResourceContents { mime_type, .. } = &mut contents {
            *mime_type = Some(resources::DSL_SCHEMA_MIME.to_owned());
        }
        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            vec![contents],
        )))
    }
}

/// Serve MCP over stdio until the peer closes stdin or the transport fails.
///
/// Stdout is reserved for protocol frames — the only `write` this process
/// performs on it is rmcp's own framing. Diagnostics (`identity`, startup
/// failures) go to stderr from `cli::mcp` and `identity.rs`.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] when the transport handshake or the serving
/// task fails.
pub(crate) async fn serve(state: McpState) -> anyhow::Result<()> {
    let service = PulseMcp::new(state)
        .serve(stdio())
        .await
        .map_err(|e| anyhow::anyhow!("mcp transport init: {e}"))?;
    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("mcp serve task: {e}"))?;
    Ok(())
}
