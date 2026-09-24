//! `pulse token` — the operator's local token administration (r3.s3.w1, D5).
//!
//! **Stdout discipline:** `token issue` prints the token ONCE, on stdout, as
//! the only stdout line (so a script can capture it); everything else —
//! success notes and every error — goes to stderr. `token list` prints one
//! line per token (label, scope, `created_at`, `revoked_at` or `active`) and
//! NEVER prints a token or its hash.
//!
//! These commands are the one sanctioned second writer to `pulse.db` beside a
//! running server (WAL permits it; ADR-0026): a token cannot be issued over an
//! API that needs a token.

use std::path::PathBuf;

use clap::Subcommand;

use crate::adapters::db::SqliteClientTokenRepo;
use crate::domain::strategy::AgentName;
use crate::server::auth::{Scope, hash_token, mint_token};

/// `pulse token {issue,revoke,list}` — operator administration on the host.
#[derive(Debug, Subcommand)]
pub enum TokenAction {
    /// Mint a token and print it once on stdout.
    Issue {
        /// The scope the token carries (`app` or `agent`).
        #[arg(long)]
        scope: Scope,
        /// The unique label (1–64 chars of `[A-Za-z0-9._-]`, lowercased).
        #[arg(long)]
        label: String,
        /// Path to `pulse.db`. Defaults to the platform data path.
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Revoke the token with this label.
    Revoke {
        /// The label of the token to revoke.
        #[arg(long)]
        label: String,
        /// Path to `pulse.db`. Defaults to the platform data path.
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// List tokens (never a token or a hash).
    List {
        /// Path to `pulse.db`. Defaults to the platform data path.
        #[arg(long)]
        db: Option<PathBuf>,
    },
}

/// `pulse token ...` argument root.
#[derive(Debug, clap::Args)]
pub struct TokenArgs {
    /// The subcommand to run.
    #[command(subcommand)]
    pub action: TokenAction,
}

/// Run one `pulse token` action against the migrated database.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] — printed to stderr by `main` with a non-zero
/// exit — when the label is invalid, the DB fails migrate-then-open, or the
/// store refuses the action (duplicate label, unknown label, already revoked).
pub(crate) async fn run_token(args: &TokenArgs) -> anyhow::Result<()> {
    match &args.action {
        TokenAction::Issue { scope, label, db } => {
            let name =
                AgentName::parse(label).map_err(|e| anyhow::anyhow!("invalid --label: {e}"))?;
            let db_handle = super::open_db(db.as_deref()).await?;
            let repo = SqliteClientTokenRepo::new(db_handle.pool().clone());
            let token = mint_token();
            let hash = hash_token(&token);
            repo.issue(name.as_str(), scope.as_str(), &hash, "cli:token-issue")
                .await
                .map_err(|e| anyhow::anyhow!("token issue refused: {e}"))?;
            // The one stdout line — the only thing a capturing script sees.
            println!("{token}");
        }
        TokenAction::Revoke { label, db } => {
            let name =
                AgentName::parse(label).map_err(|e| anyhow::anyhow!("invalid --label: {e}"))?;
            let db_handle = super::open_db(db.as_deref()).await?;
            let repo = SqliteClientTokenRepo::new(db_handle.pool().clone());
            repo.revoke(name.as_str())
                .await
                .map_err(|e| anyhow::anyhow!("token revoke refused: {e}"))?;
            eprintln!("pulse token: revoked {}", name.as_str());
        }
        TokenAction::List { db } => {
            let db_handle = super::open_db(db.as_deref()).await?;
            let repo = SqliteClientTokenRepo::new(db_handle.pool().clone());
            let rows = repo
                .list()
                .await
                .map_err(|e| anyhow::anyhow!("token list failed: {e}"))?;
            for row in rows {
                let state = row
                    .revoked_at
                    .as_deref()
                    .map_or_else(|| "active".to_owned(), |ts| format!("revoked:{ts}"));
                println!("{} {} {} {}", row.label, row.scope, row.created_at, state);
            }
        }
    }
    Ok(())
}
