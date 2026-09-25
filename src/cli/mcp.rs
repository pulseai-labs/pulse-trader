//! `pulse mcp` — serve MCP over stdio (r2.s1.w2), relay it over HTTP
//! (r3.s3.w5), or log the app in (`login`).
//!
//! **Stdout discipline:** this module prints NOTHING to stdout except where a
//! verb owns it — the moment `serve`/relay starts, stdout belongs to the
//! protocol framing, so even startup diagnostics are impossible there
//! (failures return via `anyhow` → stderr at `main`). The boundary script
//! holds the same rule for `src/mcp/**` and `src/application/mcp_read.rs`.
//! `login` is the one deliberate exception: it prints NOTHING on success
//! either (the connection file is the artifact); every diagnostic goes to
//! stderr, and failures exit non-zero.
//!
//! Modes:
//!
//! - `--local` — today's behaviour, unchanged: serve MCP over stdio against
//!   the LOCAL database (`--db`, `--data-dir`, `--agent-name` all apply).
//! - bare `pulse mcp` — the thin relay: stdin's JSON-RPC frames ride HTTP to
//!   the always-on server's `/mcp` (r3.s3.w5). The connection comes from the
//!   app's `mcp-connection.toml`; no login and no `--local` is a non-zero
//!   exit with the reason on stderr.
//! - `pulse mcp login --server <url>` — verify a token against the server
//!   (handshake + one MCP initialize probe) and write the connection file.
//!   The token comes FROM STDIN ONLY — never an argument, never a flag (a
//!   process argument is world-readable via `/proc`).

use std::path::PathBuf;

use crate::adapters::broker::BinanceAdapter;
use crate::adapters::store::{CandleStore, default_base_dir};
use crate::mcp::export::Exports;
use crate::mcp::identity::{AgentIdentity, validate_agent_name};
use crate::mcp::relay::{RelayConfig, run as run_relay};
use crate::mcp::{McpState, serve};

/// `pulse mcp [--local] [--db <path>] [--data-dir <path>] [--agent-name <name>]`
/// | `pulse mcp login --server <url>` (token on stdin).
#[derive(Debug, clap::Args)]
pub struct McpArgs {
    /// The `login` subcommand: verify a token and write the connection file.
    #[command(subcommand)]
    pub command: Option<McpCommand>,
    /// Serve MCP over stdio against the LOCAL database instead of relaying to
    /// the always-on server (the pre-r3.s3.w5 behaviour, unchanged).
    #[arg(long)]
    pub local: bool,
    /// Path to `pulse.db`. Defaults to the platform Application Support path.
    #[arg(long)]
    pub db: Option<PathBuf>,
    /// The application data directory (candle snapshots + exports root).
    /// Defaults to the platform Application Support dir.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// The agent identity to report (1–64 chars of `[A-Za-z0-9._-]`, stored
    /// lowercase). Overrides the MCP `clientInfo.name`.
    #[arg(long)]
    pub agent_name: Option<String>,
}

/// The `pulse mcp` subcommands.
#[derive(Debug, clap::Subcommand)]
pub enum McpCommand {
    /// Verify `--server <url>` with the token read from stdin, then write the
    /// connection file the bare `pulse mcp` relay (and the app) reads back.
    Login {
        /// The server's base URL, e.g. `http://draco-desk:17620`. No default:
        /// an invented endpoint would silently point an agent's writes at the
        /// wrong machine.
        #[arg(long)]
        server: String,
    },
}

/// Validate `--agent-name`, resolve the data dir, open the migrated DB, build
/// [`McpState`] and hand the process to [`serve`]; or relay; or log in.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] — printed to stderr by `main` with a non-zero
/// exit — when the agent name is invalid, the data dir or exports dir cannot
/// be resolved/created, the DB fails migrate-then-open, the connection file is
/// missing or unsafe in relay mode, or the login probes fail.
pub(crate) async fn run_mcp(args: &McpArgs) -> anyhow::Result<()> {
    if let Some(command) = &args.command {
        return match command {
            McpCommand::Login { server } => login(server).await,
        };
    }
    if args.local {
        return serve_local(args).await;
    }
    relay().await
}

/// The pre-r3.s3.w5 stdio serve, byte for byte.
async fn serve_local(args: &McpArgs) -> anyhow::Result<()> {
    // Validate BEFORE opening anything: an invalid --agent-name must exit
    // non-zero before serving (AC-10), with the reason on stderr.
    let agent_name = args
        .agent_name
        .as_deref()
        .map(validate_agent_name)
        .transpose()
        .map_err(|reason| anyhow::anyhow!("invalid --agent-name: {reason}"))?;

    let data_dir = match &args.data_dir {
        Some(dir) => dir.clone(),
        None => default_base_dir().map_err(|e| anyhow::anyhow!("resolve data dir: {e}"))?,
    };
    let exports = Exports::create(&data_dir).map_err(|e| anyhow::anyhow!("exports dir: {e}"))?;
    let db = super::open_db(args.db.as_deref()).await?;

    let state = McpState {
        db,
        candles: CandleStore::with_base_dir(data_dir),
        exports: std::sync::Arc::new(exports),
        identity: AgentIdentity::resolve(agent_name.as_deref(), None),
        // Stateless, cloneable, no I/O at construction — the run_backtest
        // tool's symbol-filter surface (r2.s1.w3).
        exchange: BinanceAdapter::new(),
    };
    serve(state).await
}

/// The relay: bridge stdin's JSON-RPC to the server's `/mcp` over HTTP.
///
/// The connection file is the only configuration: the same file the app's
/// Connect screen writes, so an operator logs in once and both surfaces work.
/// Missing or unsafe → non-zero exit with the reason on stderr.
async fn relay() -> anyhow::Result<()> {
    let connection = crate::client::connection::load_mcp()
        .map_err(|reason| {
            anyhow::anyhow!("pulse mcp: the saved connection is not usable: {reason}")
        })?
        .ok_or_else(|| {
            // The spec's literal (spec ~199): the operator's next action,
            // word for word — pinned by `bare_mcp_without_login_names_the_spec_literal`.
            anyhow::anyhow!("no server login: run 'pulse mcp login --server <url>' or pass --local")
        })?;
    run_relay(
        RelayConfig {
            base: connection.url.trim_end_matches('/').to_owned(),
            token: connection.token,
        },
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
}

/// `pulse mcp login --server <url>`: handshake, probe `/mcp` with one MCP
/// `initialize`, and write the connection file. The token arrives on stdin —
/// read to the first newline, trimmed. Every diagnostic goes to stderr; the
/// ONLY stdout this verb produces is nothing.
///
/// A refused or skewed token exits non-zero and writes NO file — a half-verified
/// connection must never look like a working one.
async fn login(server: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncBufReadExt as _;

    eprintln!("pulse mcp login: reading the token from stdin (one line)…");
    let mut token = String::new();
    tokio::io::BufReader::new(tokio::io::stdin())
        .read_line(&mut token)
        .await
        .map_err(|error| anyhow::anyhow!("read the token from stdin: {error}"))?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        anyhow::bail!("pulse mcp login: the token on stdin was empty");
    }

    // The handshake the app's Connect screen performs — reuse the client core
    // (version pin included) rather than a weaker re-derivation.
    let base = server.trim_end_matches('/');
    eprintln!("pulse mcp login: probing {base}/api/v1/handshake…");
    let (client, outcome) = crate::client::ServerClient::connect(base, &token)
        .await
        .map_err(|error| anyhow::anyhow!("pulse mcp login: {error}"))?;
    if let crate::client::ConnectOutcome::Connected {
        binary_version,
        engine_fingerprint,
    } = &outcome
    {
        eprintln!(
            "pulse mcp login: server answered — pulse {binary_version}, engine \
             {engine_fingerprint}"
        );
    }

    // The MCP probe: one real `initialize` against `/mcp` with THIS token, so
    // `login` proves the token is agent-scoped before it is persisted. The
    // probe rides the streamable-HTTP shape the relay itself will speak.
    eprintln!("pulse mcp login: probing {base}/mcp…");
    let probe = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "pulse-mcp-login", "version": env!("CARGO_PKG_VERSION") }
        }
    });
    let response = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(&probe)
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("pulse mcp login: the /mcp probe failed: {error}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "pulse mcp login: the server refused the /mcp probe (HTTP {status}): {}",
            body.lines().next().unwrap_or_default()
        );
    }

    // Persist the connection the app and the relay share.
    crate::client::connection::store_mcp(&crate::client::connection::ConnectionFile {
        url: base.to_owned(),
        token,
    })
    .map_err(|reason| anyhow::anyhow!("pulse mcp login: {reason}"))?;
    let _ = client; // the probe client's work is done; the file is the artifact
    eprintln!("pulse mcp login: verified and saved — `pulse mcp` now relays to {base}");
    Ok(())
}
