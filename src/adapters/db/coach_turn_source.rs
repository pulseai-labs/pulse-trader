//! The `SQLite` [`CoachTurnSource`] adapter (r1.s4.w1, `#132`) — one run id in, one
//! consistent coach-turn projection out.
//!
//! **What this exists to make impossible.** Before the seal, a caller assembled the
//! turn's inputs itself: a run from one call, its trades from another, a version
//! from a third. Every one of those was an opportunity to hand the coach a version
//! the run was never produced against, another run's trades, or a truncated set —
//! and the resulting audit row would carry two individually valid foreign keys
//! asserting a relationship that never existed. This adapter takes a `run_id` and
//! nothing else, and derives the rest, so the fragments have nowhere to come from.
//!
//! **Every projection is holdout-guarded before any trade or prompt read.**
//! Both coach entry points use this source; a persisted result reaching the open
//! freeze is refused by the shared application guard before any provider call.
//!
//! **It adds no SQL.** The three reads are the EXISTING repository methods, with
//! their fail-closed integrity re-check (`backtest_run.result_content_hash`), their
//! `schema_version` read-rejects and their `Migrator`-driven DSL read path intact.
//! Re-implementing them here to fold them into a single explicit read transaction
//! would put a second copy of those rules in the tree, which is the more expensive
//! mistake: the property this projection actually owes is that the three values are
//! CONSISTENT WITH EACH OTHER — the version is the one the run names, the trades are
//! that run's complete ordered set — and that is established by the key each read is
//! made with, not by the isolation level. On a WAL database with an append-only run
//! row and an immutable version row, there is no writer that could interleave to
//! make them disagree.
//!
//! **A pre-`0006` run is a projection, not an error** (`CoachTurnProjection::Legacy`):
//! its provenance columns are all NULL, so no child of it could be re-backtested on
//! the same data, and the honest answer is a recorded
//! `CoachFailure::MissingBacktestInputs` rather than a load failure the rail would
//! have to interpret.

use sqlx::SqlitePool;

use crate::domain::{BacktestRunId, BacktestRunRepository, DataError, StrategyRepository};
use crate::domain::{CoachTurnProjection, CoachTurnSource, ProjectedRun};

use super::{SqliteBacktestRunRepo, SqliteCertificationFreezeRepo, SqliteStrategyRepo};
use crate::adapters::store::CandleStore;
use crate::application::holdout::guard_persisted_run;

/// The `SQLite` coach-turn projection over `pulse.db`.
///
/// Read-only, and therefore clock-free: nothing here mints a timestamp, so there is
/// no `with_deps` seam to inject one through. The turn's one caller-supplied
/// timestamp — the claim's `created_at` — is the application module's, from the
/// clock the composition root injects there.
pub struct SqliteCoachTurnSource {
    pool: SqlitePool,
    candles: Option<CandleStore>,
}

impl SqliteCoachTurnSource {
    /// Build the projection over a pool (cloned from `Db::pool()`).
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            candles: None,
        }
    }
    /// Use the entry point's candle store for pinned, unwindowed run spans.
    /// The CLI default is resolved lazily, only when a freeze is open.
    #[must_use]
    pub fn with_candles(mut self, candles: CandleStore) -> Self {
        self.candles = Some(candles);
        self
    }
}

impl CoachTurnSource for SqliteCoachTurnSource {
    async fn load_coach_turn(
        &self,
        run_id: &BacktestRunId,
    ) -> Result<Option<CoachTurnProjection>, DataError> {
        let runs = SqliteBacktestRunRepo::new(self.pool.clone());
        let strategies = SqliteStrategyRepo::new(self.pool.clone());

        let Some(run) = runs.get_run(run_id).await? else {
            return Ok(None);
        };
        // The sealed projection is the common boundary for desktop and CLI
        // turns. Read the open freeze once and refuse before loading trades,
        // rendering context, claiming a turn, or calling a provider.
        let holdout = SqliteCertificationFreezeRepo::new(self.pool.clone())
            .open_freeze()
            .await?
            .map(|freeze| freeze.holdout());
        if holdout.is_some() {
            let candles = match &self.candles {
                Some(candles) => candles.clone(),
                None => CandleStore::with_default_base_dir()?,
            };
            guard_persisted_run(candles, holdout, &run)
                .await
                .map_err(|error| DataError::Db(error.to_string()))?;
        }
        // The COMPLETE ordered trade set of THIS run — the repo orders by `seq`, and
        // there is no argument here through which a caller could ask for fewer.
        let trades = runs.get_trades(run_id).await?;
        // The version the RUN names. An absent one is a corrupt row, not an empty
        // projection: the FK guarantees it exists, so its absence must be loud.
        let version = strategies
            .get_version(&run.strategy_version_id)
            .await?
            .ok_or_else(|| {
                DataError::Db(format!(
                    "backtest run `{}` names strategy version `{}`, which is absent",
                    run_id.as_str(),
                    run.strategy_version_id.as_str()
                ))
            })?;

        let legacy = run.inputs.is_none();
        let projected = ProjectedRun {
            run,
            trades,
            version,
        };
        Ok(Some(if legacy {
            CoachTurnProjection::Legacy(projected)
        } else {
            CoachTurnProjection::Coachable(projected)
        }))
    }
}
