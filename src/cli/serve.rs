//! `pulse serve` — the composition root of the always-on server (r3.s3.w1,
//! ADR-0026). Thin by design: the ordered startup steps, the bind policy and
//! the retry loop live in `server::bind` where the tests drive them; this
//! module parses the flags, checks the data-dir role marker (`--role`,
//! r4.s2.w3) BEFORE the database is opened — an existing marker before the
//! instance lock, an absent one published only with the lock held (PR-354 fix
//! N1) — takes the database's instance lock on the resolved path (PR-354 fix
//! C3a — before the migrate-then-open, so an import/restore cannot swap the
//! file in between), opens the migrated DB, resolves the data dir and hands the
//! [`ServeConfig`] (the lock's guard with it) over.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use clap::Args;

use crate::adapters::db::default_db_path;
use crate::adapters::db::instance_lock::InstanceLock;
use crate::adapters::store::default_base_dir;
use crate::application::paper_control::DEFAULT_PAPER_REPLY_TIMEOUT_MS;
use crate::server::bind::{DEFAULT_POLL_GRACE_MS, ServeConfig, ServeError};
use crate::server::role::{self, ServerRole};
use crate::server::start_limit::StartLimit;

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
    /// Bound repeated starts, `<N>/<SECONDS>` (e.g. `3/900`). launchd has no
    /// start limit of its own, so the Mini's `LaunchAgent` passes this: when
    /// this start makes more than N starts inside the window, the server writes
    /// `<data dir>/serve-start-limit` and exits 0 — `KeepAlive { SuccessfulExit
    /// = false }` then stops relaunching — before binding. Off by default (the
    /// systemd unit bounds its own starts and passes nothing).
    #[arg(long, value_name = "N/SECONDS")]
    start_limit: Option<String>,
    /// The role this data dir is for, `prod` or `qa` (r4.s2.w3, C5). With
    /// `--role`, the database must sit inside `--data-dir`, and
    /// `<data dir>/server-role` is checked BEFORE the database is opened — an
    /// existing marker before the instance lock, an absent one published only
    /// with the lock held (PR-354 fix N1): the same role continues, the other
    /// is refused by name (never starting on the other role's data), and an
    /// unmarked dir is marked with this role. Without `--role` nothing is
    /// checked or written.
    #[arg(long, value_name = "prod|qa")]
    role: Option<String>,
}

/// Run the server. Errors surface as named non-zero exits (bind policy
/// refusals, exhausted retries, bind failures, serve failures).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] when the DB fails migrate-then-open, the data
/// dir cannot be resolved, or the server start/serve fails.
pub(crate) async fn run_serve(args: &ServeArgs) -> anyhow::Result<()> {
    // ---- Step 0: the start bound's grammar (r4.s2.w1, G5). A malformed
    // `--start-limit` is a startup refusal, before the DB is opened.
    let start_limit = match &args.start_limit {
        Some(value) => Some(
            StartLimit::from_str(value)
                .map_err(|error| anyhow::anyhow!("--start-limit: {error}"))?,
        ),
        None => None,
    };
    // ---- Step 0b (r4.s2.w3, C5; PR-354 fix N1): the EXISTING role marker,
    // read BEFORE the instance lock. A refused start must touch NOTHING at all
    // — no database file created or migrated, no instance lock, no start-log
    // entry, no marker change — so a QA server started with prod's `--db` by
    // mistake never opens prod's database. The two paths read here are the same
    // ones `open_db` and the server below resolve.
    let role = match &args.role {
        Some(raw) => {
            Some(ServerRole::parse(raw).map_err(|error| anyhow::anyhow!("--role: {error}"))?)
        }
        None => None,
    };
    let data_dir = resolved_data_dir(args)?;
    let db_path = resolved_db_path(args)?;
    if let Some(role) = role {
        role::check_before_lock(&data_dir, &db_path, role)
            .map_err(|error| anyhow::anyhow!("pulse serve: refusing to serve: {error}"))?;
    }
    // ---- Step 0c (PR-354 fix C3a): the database's instance lock, held for this
    // process's lifetime. It is taken HERE — on the resolved db path, BEFORE
    // `open_db` — because an import/restore can otherwise swap the database file
    // in between the migrate-then-open and the later acquire: the server would
    // keep serving the unlinked inode while the import installs a new file under
    // the same name. A second server refuses this one by name before anything
    // touches the database (the instance lock is `pulse serve`'s alone in this
    // PR; the import/restore enforcement was split out to issue #355).
    let instance_lock = InstanceLock::acquire(&db_path).map_err(|error| {
        anyhow::Error::new(ServeError::InstanceLockHeld {
            reason: error.to_string(),
        })
    })?;
    // ---- Step 0d (PR-354 fix N1): an ABSENT role marker is published only
    // HERE, with the instance lock in hand and still before `open_db`. The old
    // order published it in step 0b, so a `--role qa` start that then lost the
    // lock above — another server already on that database — had already
    // written `server-role=qa` and durably relabelled a live database's dir
    // before it exited. A lock refusal now writes no marker; the refusal path
    // above still writes nothing either.
    if let Some(role) = role {
        role::publish_absent_marker(&data_dir, role)
            .map_err(|error| anyhow::anyhow!("pulse serve: refusing to serve: {error}"))?;
    }
    // ---- Step 1: the migrated DB (the one migrate-then-open every arm uses).
    let db = super::open_db(args.db.as_deref()).await?;
    crate::server::bind::serve(ServeConfig {
        bind: args.bind,
        dev_loopback: args.dev_loopback,
        db,
        data_dir,
        poll_grace_ms: DEFAULT_POLL_GRACE_MS,
        paper_reply_timeout_ms: DEFAULT_PAPER_REPLY_TIMEOUT_MS,
        start_limit,
        role,
        instance_lock,
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// The database `pulse serve` opens: `--db`, or the platform default — the same
/// path the role check reads, resolved once.
fn resolved_db_path(args: &ServeArgs) -> anyhow::Result<PathBuf> {
    match &args.db {
        Some(path) => Ok(path.clone()),
        None => default_db_path().map_err(|e| anyhow::anyhow!("resolve db path: {e}")),
    }
}

/// The data dir the server runs on (mirror `pulse mcp`): `--data-dir`, or the
/// platform default.
fn resolved_data_dir(args: &ServeArgs) -> anyhow::Result<PathBuf> {
    match &args.data_dir {
        Some(path) => Ok(path.clone()),
        None => default_base_dir().map_err(|e| anyhow::anyhow!("resolve data dir: {e}")),
    }
}
