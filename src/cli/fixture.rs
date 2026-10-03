//! `pulse fixture` — the certify fixture's operator verb (r3.s4.w2, E4).
//!
//! `pulse fixture seed` stamps the deterministic synthetic BTCUSDT M15 + H4
//! series into the candle store (snapshots only — never HEAD), their
//! `fixture_snapshot` rows, the plainly-named `fixture`-tagged strategy and
//! its version, and ONE real walk-forward certification per engine
//! fingerprint. Idempotent per build: a second seed adds no row and no file
//! and reports the existing ids. The fixture is what keeps a
//! fixture-certified promotion a real gate pass and its E2 refusal a real
//! re-cert demand.
//!
//! The `--db` / `--data-dir` overrides mirror every other store-touching
//! verb; the dispatch arm opens the db migrate-then-open via `open_migrated`.

use std::path::PathBuf;

use clap::Subcommand;

use crate::adapters::db::paper_session_repo::SqlitePaperSessionRepo;
use crate::adapters::db::{SqliteBacktestRunRepo, SqliteStrategyRepo, open_migrated, paths};
use crate::adapters::store::CandleStore;

/// The `fixture` command group.
#[derive(Debug, clap::Args)]
pub struct FixtureArgs {
    /// The fixture subcommand to run.
    #[command(subcommand)]
    pub command: FixtureCommand,
}

/// The fixture verbs.
#[derive(Debug, Subcommand)]
pub enum FixtureCommand {
    /// Stamp the certify fixture: snapshots + `fixture_snapshot` rows + the
    /// fixture strategy/version + one walk-forward certification per build.
    Seed {
        /// `pulse.db` path override (defaults to the platform db).
        #[arg(long, global = true)]
        db: Option<PathBuf>,
        /// The candle-snapshot directory override (defaults to the platform
        /// data dir).
        #[arg(long, global = true)]
        data_dir: Option<PathBuf>,
    },
}

/// The dispatch arm: open the db, run the seed, print the outcome ids.
///
/// # Errors
///
/// Anyhow-wrapped store/seed failures, typed at this edge (ADR-0017): the
/// version-drift refusal prints its own operator-facing message.
pub async fn run_seed(
    db_override: Option<&std::path::Path>,
    data_dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    let path = match db_override {
        Some(p) => p.to_path_buf(),
        None => paths::default_db_path().map_err(|e| anyhow::anyhow!("resolve db path: {e}"))?,
    };
    let db = open_migrated(&path)
        .await
        .map_err(|e| anyhow::anyhow!("open db: {e}"))?;
    let data = match data_dir {
        Some(dir) => dir,
        None => paths::default_data_dir().map_err(|e| anyhow::anyhow!("resolve data dir: {e}"))?,
    };
    let store = CandleStore::with_base_dir(data);
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let paper = SqlitePaperSessionRepo::new(db.pool().clone(), store.clone());

    let exchange = crate::adapters::broker::BinanceAdapter::new();
    let outcome = crate::application::fixture::seed(&strategies, &store, &runs, &paper, &exchange)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let strategy_id = outcome.strategy_id.as_str();
    let version_id = outcome.version_id.as_str();
    let run_id = outcome.walk_forward_run_id.as_str();
    if outcome.already_seeded && !outcome.added_run {
        println!(
            "fixture already seeded on this build (no row, no file changed): \
             strategy={strategy_id} version={version_id} run={run_id}"
        );
    } else if outcome.added_run {
        println!(
            "fixture seeded: strategy={strategy_id} version={version_id} run={run_id} \
             (new walk-forward certification)"
        );
    } else {
        println!("fixture seeded: strategy={strategy_id} version={version_id} run={run_id}");
    }
    Ok(())
}
