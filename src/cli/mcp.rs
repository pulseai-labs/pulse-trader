//! `pulse mcp` — serve MCP over stdio (r2.s1.w2).
//!
//! **Stdout discipline:** this module prints NOTHING — the moment `serve`
//! starts, stdout belongs to rmcp's protocol framing, so even startup
//! diagnostics are impossible here (failures return via `anyhow` → stderr at
//! `main`). The boundary script holds the same rule for `src/mcp/**` and
//! `src/application/mcp_read.rs`.
//!
//! `--data-dir` is the app-data-dir override: the candle store roots at it and
//! exports land in `<data dir>/exports/<pid>-<start-unix-ms>/`.

use std::path::PathBuf;

use crate::adapters::store::{CandleStore, default_base_dir};
use crate::mcp::export::Exports;
use crate::mcp::identity::{AgentIdentity, validate_agent_name};
use crate::mcp::{McpState, serve};

/// `pulse mcp [--db <path>] [--data-dir <path>] [--agent-name <name>]`.
#[derive(Debug, clap::Args)]
pub struct McpArgs {
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

/// Validate `--agent-name`, resolve the data dir, open the migrated DB, build
/// [`McpState`] and hand the process to [`serve`].
///
/// # Errors
///
/// Returns an [`anyhow::Error`] — printed to stderr by `main` with a non-zero
/// exit — when the agent name is invalid, the data dir or exports dir cannot
/// be resolved/created, or the DB fails migrate-then-open.
pub(crate) async fn run_mcp(args: &McpArgs) -> anyhow::Result<()> {
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
        exports,
        identity: AgentIdentity::resolve(agent_name.as_deref(), None),
    };
    serve(state).await
}
