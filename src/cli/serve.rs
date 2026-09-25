//! `pulse serve` — the composition root of the always-on server (r3.s3.w1,
//! ADR-0026). Thin by design: the ordered startup steps, the bind policy and
//! the retry loop live in `server::bind` where the tests drive them; this
//! module only parses the flags, opens the migrated DB, resolves the data dir
//! and hands the [`ServeConfig`] over.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Args;

use crate::adapters::store::default_base_dir;
use crate::server::bind::ServeConfig;

/// `pulse serve --bind <ip:port>` — run the server on this host until
/// SIGTERM/SIGINT.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// The address to bind — an IPv4 tailnet address (`100.64.0.0/10`), or a
    /// loopback address under `--dev-loopback`. Port 0 picks an ephemeral port.
    #[arg(long)]
    bind: SocketAddr,
    /// Path to `pulse.db`. Defaults to the platform data path.
    #[arg(long)]
    db: Option<PathBuf>,
    /// The data dir (snapshot/export base). Defaults to the platform path.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Accept a loopback bind (development only — the Mac app talks to a
    /// locally running server without Tailscale in the path).
    #[arg(long, default_value_t = false)]
    dev_loopback: bool,
}

/// Run the server. Errors surface as named non-zero exits (bind policy
/// refusals, exhausted retries, bind failures, serve failures).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] when the DB fails migrate-then-open, the data
/// dir cannot be resolved, or the server start/serve fails.
pub(crate) async fn run_serve(args: &ServeArgs) -> anyhow::Result<()> {
    // ---- Step 1: the migrated DB (the one migrate-then-open every arm uses).
    let db = super::open_db(args.db.as_deref()).await?;
    // ---- Step 2: the data dir (mirror `pulse mcp`).
    let data_dir = match &args.data_dir {
        Some(path) => path.clone(),
        None => default_base_dir().map_err(|e| anyhow::anyhow!("resolve data dir: {e}"))?,
    };
    crate::server::bind::serve(ServeConfig {
        bind: args.bind,
        dev_loopback: args.dev_loopback,
        db,
        data_dir,
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))
}
