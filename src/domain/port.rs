//! Domain ports — the hexagonal seams the outer rings implement.
//!
//! - [`MarketDataSource`] — historical/incremental candle data (WI-02 onward).
//! - [`CandleSeriesRepository`] — the candle-snapshot persistence port (r1.s3.w1,
//!   #112); `CandleStore` (`adapters/store`) implements it, and the fetch,
//!   indicator and backtest use cases consume it generically. Synchronous, unlike
//!   its siblings, because its only implementor is. It closes ADR-0015's one
//!   named hexagonal exception.
//! - [`StrategyRepository`] — the strategy-tree persistence port (VS-1.1.4,
//!   FR-4 / FR-11); the `sqlx` adapter (1.03) implements it, the CLI (1.05) and
//!   agent layer consume it generically (`<R: StrategyRepository>`).
//! - [`ExchangeAdapter`] — the exchange-metadata port (VS-1.2.2, FR-5 / NFR-3);
//!   `BinanceAdapter` (`adapters/broker`) implements it, the sizer consumes the
//!   returned [`SymbolFilters`](crate::domain::sizing::SymbolFilters). Minimal v1
//!   surface (NO order/account/balance methods); returns a dedicated
//!   [`ExchangeError`](crate::domain::exchange::ExchangeError), not
//!   `BacktestError` (audit C5).
//!
//! Hexagonal inbound-of-outer ports (NFR-9): adapters (`BinanceDataSource` in
//! WI-02, a `Parquet`-replay source later) implement them; the engine consumes
//! them generically, never as `dyn`. The domain stays free of `tokio`/`reqwest`;
//! only the trait shapes live here.
//!
//! **`Send` futures (audit C3):** the methods return `impl Future<..> + Send`
//! rather than bare `async fn`, so the returned futures are guaranteed `Send`
//! and adapter calls can be `spawn`ed on tokio's multi-thread runtime. Stating
//! the bound explicitly also sidesteps the `async_fn_in_trait` lint — no
//! `#[allow(..)]` is needed.

use std::future::Future;

use crate::domain::backtest::SummaryStats;
use crate::domain::backtest::{
    BacktestInputs, BacktestResult, BacktestRunId, PersistedRun, RunSummary, Trade, WalkForwardRun,
    WalkForwardRunDraft, WalkForwardRunId,
};
use crate::domain::candle::Candle;
use crate::domain::coaching::{
    AcceptedCoachOutcome, CoachAcceptFailure, CoachSessionClaim, CoachSessionClaimResult,
    CoachTurnProjection, CoachingSession, CoachingSessionId, Disposition, InitialCoachOutcome,
    PreparedCoachAcceptance, Proposal,
};
use crate::domain::dsl::Mutation;
use crate::domain::error::DataError;
use crate::domain::exchange::ExchangeError;
use crate::domain::llm::{LlmConfig, LlmError, LlmResponse, Message, ToolDefinition};
use crate::domain::llm_call::{AttributedCall, AttributedCallError, LlmCall, LlmCallId};
use crate::domain::pair::Pair;
use crate::domain::series::{CandleSeries, StoredCandleSeries};
use crate::domain::sizing::SymbolFilters;
use crate::domain::strategy::{
    AgentSubmission, NewAgentSubmission, NewVersion, Strategy, StrategyId, StrategyVersion,
    VersionId,
};
use crate::domain::timeframe::Timeframe;
use crate::domain::version::DataVersion;
use rust_decimal::Decimal;

/// The exchange-metadata port (VS-1.2.2, FR-5 / NFR-3) — audit C3 / C5.
///
/// Supplies a symbol's [`SymbolFilters`] (lot step / min qty / min notional /
/// exchange max-leverage) so the shared
/// [`compute_position_size`](crate::domain::sizing::compute_position_size) can
/// apply exchange constraints. **Minimal v1 surface** — NO order / cancel /
/// account / balance methods (those land with live execution in v3). Returns the
/// **dedicated** [`ExchangeError`] (NOT [`DataError`] / `BacktestError`) because
/// the port serves live execution too (audit C5).
///
/// Synchronous: v1's only implementor (`BinanceAdapter`) returns **pinned
/// consts** with no I/O. A future networked filter-fetch adapter can wrap a cache
/// behind the same sync surface, or this method gains an async sibling additively.
pub trait ExchangeAdapter {
    /// Return the [`SymbolFilters`] for `pair`.
    ///
    /// # Errors
    ///
    /// Returns [`ExchangeError::UnknownSymbol`] when the adapter has no filters
    /// pinned for `pair` (v1's `BinanceAdapter` only knows `BTCUSDT`).
    fn symbol_filters(&self, pair: &Pair) -> Result<SymbolFilters, ExchangeError>;
}

/// A source of historical and incremental candle data.
///
/// All methods return `Send` futures so callers may `spawn` them across threads.
pub trait MarketDataSource {
    /// Fetch a closed historical range `[start_ms, end_ms)` for `(pair, tf)`.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the underlying source fails (I/O, parse) or the
    /// returned series is structurally invalid.
    fn fetch_historical(
        &self,
        pair: &Pair,
        tf: Timeframe,
        start_ms: i64,
        end_ms: i64,
    ) -> impl Future<Output = Result<CandleSeries, DataError>> + Send;

    /// Fetch candles newer than `since_ms` for `(pair, tf)`.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the underlying source fails (I/O, parse).
    fn fetch_incremental(
        &self,
        pair: &Pair,
        tf: Timeframe,
        since_ms: i64,
    ) -> impl Future<Output = Result<Vec<Candle>, DataError>> + Send;
}

/// The candle-snapshot persistence port (r1.s3.w1, #112).
///
/// ADR-0015 named exactly one hexagonal exception: candle storage had no port, so
/// the fetch, indicator and backtest use cases imported and constructed the
/// concrete `CandleStore` adapter. This trait closes that exception. The Parquet
/// adapter implements it; the use cases consume it generically
/// (`<R: CandleSeriesRepository>`), never as `dyn`, and only `src/cli/mod.rs` (the
/// composition root) still chooses a concrete implementation.
///
/// **Synchronous, unlike the sibling ports.** The only implementor is the existing
/// synchronous Parquet adapter, and the debug CLI already drives it that way;
/// stating `async` here would buy nothing and force every consumer into a runtime
/// it does not need. Offloading the blocking read from a Tauri command is `r1.s3`'s
/// `w3` problem, not this port's.
///
/// **Deep, not shallow (ADR-0012).** Three semantic operations, and deliberately
/// no content-version, path, encode, raw HEAD-write, provenance-decoder, existence,
/// latest-version or temp-file method. Those are adapter internals — re-exposing
/// them here would recreate `CandleStore` as a trait and close nothing.
pub trait CandleSeriesRepository {
    /// Load the current snapshot for `(pair, timeframe)` — the `HEAD` pointer and
    /// the exact snapshot it names, resolved as ONE caller operation.
    ///
    /// `Ok(None)` means no `HEAD` exists yet (a first run, before anything was
    /// committed). A `HEAD` that names an absent or unreadable snapshot is an
    /// **error**, never `Ok(None)`: "nothing here yet" and "the pointer is broken"
    /// are different facts and a caller must not confuse them.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] when `HEAD` is unreadable or malformed, or when the
    /// snapshot it names is absent or cannot be decoded.
    fn load_head(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
    ) -> Result<Option<StoredCandleSeries>, DataError>;

    /// Load precisely the immutable snapshot identified by `version`.
    ///
    /// It never falls back to `HEAD`: an unknown version is an error, because a
    /// caller asking for an exact identity (a replayed run, a provenance check)
    /// is worse served by silently different data than by a refusal.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] when no snapshot exists for `version` or it cannot be
    /// decoded.
    fn load_version(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
        version: &DataVersion,
    ) -> Result<StoredCandleSeries, DataError>;

    /// Publish `candles` as the current snapshot for `(pair, timeframe)`.
    ///
    /// There is **no version parameter**: the repository derives the canonical
    /// identity (ADR-0009's content hash) from the content itself, constructs the
    /// series and validates it. A caller-supplied identity that disagrees with the
    /// content is therefore unrepresentable rather than merely rejected.
    ///
    /// For a non-empty commit the immutable snapshot is written/reconciled
    /// **first** and `HEAD` advanced **second** (ADR-0018's crash-safe ordering), and
    /// `storage_location` is `Some(..)`. A failure to advance `HEAD` returns an
    /// error and may leave an already-valid orphan snapshot behind — the existing
    /// contract, preserved.
    ///
    /// For **zero** candles nothing is written, `HEAD` is left exactly where it was,
    /// and the derived identity comes back with `storage_location: None`.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] on a structurally invalid series, a same-identity
    /// different-content collision, or any storage failure.
    fn commit(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
        candles: Vec<Candle>,
    ) -> Result<StoredCandleSeries, DataError>;
}

/// The strategy-tree persistence port (VS-1.1.4, FR-4 / FR-11).
///
/// The `sqlx` adapter (1.03) implements it over `pulse.db`; the CLI (1.05) and
/// agent layer consume it generically (`<R: StrategyRepository>`), never as
/// `dyn`. Same `Send`-future style as [`MarketDataSource`] (audit C3) so a
/// repository call can be `spawn`ed on tokio's multi-thread runtime.
///
/// **Versions are create + read only (FR-4 immutability).** There is
/// deliberately NO `update_version` / `delete_version` method — a version's
/// shape is structurally immutable in this API (the DB triggers in 1.01 are the
/// second guard). The FR-11 "compare" op is NOT a port method: it is the pure
/// [`diff_versions`](crate::domain::strategy::diff_versions) fn over two
/// `get_version` results.
///
/// `get_strategy` / `get_version` return `Option<_>` (an absent id is `Ok(None)`,
/// not an error). Ids/`version_hash`/timestamps are the adapter's to mint — this
/// trait only declares the contract every sibling reads through.
pub trait StrategyRepository {
    /// Create a new strategy and return the freshly-built record (with the
    /// adapter's generated id + timestamp).
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the underlying store fails.
    fn create_strategy(
        &self,
        name: &str,
        owner: Option<&str>,
        tags: &[String],
    ) -> impl Future<Output = Result<Strategy, DataError>> + Send;

    /// Fetch a strategy by id (`Ok(None)` if no such row).
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the underlying store fails.
    fn get_strategy(
        &self,
        id: &StrategyId,
    ) -> impl Future<Output = Result<Option<Strategy>, DataError>> + Send;

    /// List strategies (FR-11 browse); `include_archived` toggles archived rows.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the underlying store fails.
    fn list_strategies(
        &self,
        include_archived: bool,
    ) -> impl Future<Output = Result<Vec<Strategy>, DataError>> + Send;

    /// Rename a strategy, returning the updated record.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the underlying store fails or the id is absent.
    fn rename_strategy(
        &self,
        id: &StrategyId,
        new_name: &str,
    ) -> impl Future<Output = Result<Strategy, DataError>> + Send;

    /// Set a strategy's tags (FR-11 tag), returning the updated record.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the underlying store fails or the id is absent.
    fn set_tags(
        &self,
        id: &StrategyId,
        tags: &[String],
    ) -> impl Future<Output = Result<Strategy, DataError>> + Send;

    /// Set (or clear) a strategy's pinned version (FR-11 pin). 1.03 validates
    /// `version ∈ strategy`; the signature only declares it.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the underlying store fails, the id is absent, or
    /// the version does not belong to the strategy.
    fn set_pinned_version(
        &self,
        id: &StrategyId,
        version_id: Option<&VersionId>,
    ) -> impl Future<Output = Result<Strategy, DataError>> + Send;

    /// Archive or un-archive a strategy (FR-11 archive), returning the record.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the underlying store fails or the id is absent.
    fn archive_strategy(
        &self,
        id: &StrategyId,
        archived: bool,
    ) -> impl Future<Output = Result<Strategy, DataError>> + Send;

    /// Create an immutable version from a [`NewVersion`] request (FR-11 clone =
    /// parent set; FR-4 immutable). The adapter mints the id/`version_hash`/
    /// `created_at` and routes `dsl_json` through the `Migrator`.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the underlying store fails or the DSL is invalid.
    fn create_version(
        &self,
        request: NewVersion,
    ) -> impl Future<Output = Result<StrategyVersion, DataError>> + Send;

    /// Fetch a version by id (`Ok(None)` if no such row).
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the underlying store fails.
    fn get_version(
        &self,
        id: &VersionId,
    ) -> impl Future<Output = Result<Option<StrategyVersion>, DataError>> + Send;

    /// List all versions of a strategy.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the underlying store fails.
    fn list_versions(
        &self,
        strategy_id: &StrategyId,
    ) -> impl Future<Output = Result<Vec<StrategyVersion>, DataError>> + Send;

    /// Browse the strategy's version subtree, parent-ordered (FR-11 browse).
    /// 1.03 owns the topological ordering.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the underlying store fails.
    fn version_tree(
        &self,
        strategy_id: &StrategyId,
    ) -> impl Future<Output = Result<Vec<StrategyVersion>, DataError>> + Send;

    /// Create an external-agent version and its `agent_submission` audit row in
    /// ONE transaction (r2.s1.w1): either both rows exist afterwards or neither
    /// does. The submission is the audit trail an agent version carries where a
    /// coach version carries a coaching session and an `LlmCall`.
    ///
    /// The implementation refuses `request.created_by !=
    /// CreatedBy::ExternalAgent` and a non-empty `creating_llm_call_ids`
    /// **before touching the database** — an agent version names no `LlmCall`
    /// rows because the agent's cost is external to the app.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] on a refused provenance shape, if the underlying
    /// store fails, or if the DSL is invalid.
    fn create_agent_version(
        &self,
        request: NewVersion,
        submission: NewAgentSubmission,
    ) -> impl Future<Output = Result<(StrategyVersion, AgentSubmission), DataError>> + Send;

    /// Create a NEW `strategy` row, its root `external_agent` version, and the
    /// `agent_submission` audit row in ONE `BEGIN IMMEDIATE` transaction
    /// (r2.s1.w3): either all three rows exist afterwards or none does.
    ///
    /// The name-uniqueness refusal is part of the atomic write — the `strategy`
    /// name is re-read under the `BEGIN IMMEDIATE` write lock — so two
    /// concurrent root submits can never both pass the way the `list_strategies`
    /// pre-read followed by `create_strategy` + `create_agent_version` let them
    /// (two writers both pass the pre-read; a failed version write orphans the
    /// strategy). `strategy.name` carries no schema-level UNIQUE — same-named
    /// human strategies are legal and may already exist — so the constraint is
    /// enforced here, at the only write boundary that promises it.
    ///
    /// `parent_version_id` / `created_by` / `creating_llm_call_ids` are absent
    /// by construction: a root version has no parent, writes as
    /// [`CreatedBy::ExternalAgent`], and names no `LlmCall` rows — the same
    /// refusals [`create_agent_version`](Self::create_agent_version) validates,
    /// made structural. Agent roots are bare strategies: `owner` is `NULL` and
    /// `tags` is `[]` (they are human-library metadata).
    ///
    /// # Errors
    ///
    /// - [`DataError::StrategyNameTaken`] when a `strategy` row already carries
    ///   `strategy_name` — decided under the write lock.
    /// - [`DataError`] on a store failure or an invalid DSL (the same
    ///   load → validate → canonicalize pipeline `create_agent_version` runs).
    fn create_agent_strategy_version(
        &self,
        strategy_name: &str,
        dsl_json: String,
        submission: NewAgentSubmission,
    ) -> impl Future<Output = Result<(Strategy, StrategyVersion, AgentSubmission), DataError>> + Send;

    /// Fetch the `agent_submission` audit row for a version (`Ok(None)` when the
    /// version has none — e.g. a human- or coach-authored version).
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the underlying store fails.
    fn get_agent_submission(
        &self,
        version_id: &VersionId,
    ) -> impl Future<Output = Result<Option<AgentSubmission>, DataError>> + Send;
}

/// The persisted backtest-run system-of-record port (VS-1.2.4 work-4.04, FR-6 /
/// FR-7 / NFR-2).
///
/// The `sqlx` adapter
/// ([`SqliteBacktestRunRepo`](crate::adapters::db::SqliteBacktestRunRepo))
/// implements it over `pulse.db`; the CLI (4.05) consumes it generically
/// (`<R: BacktestRunRepository>`), never as `dyn`. Same `Send`-future style as
/// [`StrategyRepository`] (audit C3) so a repository call can be `spawn`ed on
/// tokio's multi-thread runtime.
///
/// **Runs are create + read only.** A persisted [`BacktestRun`](PersistedRun) is
/// append-only live-capital provenance (FR-6) — there is deliberately NO
/// `update_run` / `delete_run` method; immutability is structural in this API and
/// enforced by the migration-`0003` `BEFORE UPDATE` / `BEFORE DELETE` triggers.
///
/// **#39 integrity guarantees:**
/// - `save_run` is **ownership-on-write** (the `strategy_version_id` must exist).
/// - `get_run` is **re-validate-on-read**: because `result_content_hash` is
///   trade-dependent, it fetches the run's `trade` rows internally and re-derives
///   the full hash input, rejecting a mismatch (tamper guard).
/// - **Corrupt-isolation is scoped to `list_runs_for_version` ONLY** (a catalog
///   is best-effort UX — a corrupt summary row is skipped-with-warning).
///   `get_run`, `latest_run_for_version`, and `get_trades` **fail-closed**: any
///   corrupt/un-parseable row is an `Err`, never a partial result (a trade log
///   feeds P&L/equity/hash reconstruction).
pub trait BacktestRunRepository {
    /// Persist a finished run + all its trades (FR-6, #39 ownership-on-write).
    ///
    /// Asserts the `strategy_version_id` row exists (FK + an explicit `SELECT 1`
    /// guard INSIDE the transaction), then INSERTs the run + every trade in that
    /// ONE transaction and commits; stores the `result_content_hash`. The
    /// `created_at` is sourced from the injected `Clock`.
    ///
    /// **It returns the minted id and reads nothing back (r1.s3.w3).** A read after
    /// the commit is not part of the transaction — it runs on another connection —
    /// and an implementation that failed there could only report a bare error,
    /// discarding the id of a row that already exists. A caller that wants the
    /// persisted projection calls [`get_run`](Self::get_run) itself, with the id in
    /// hand, and can therefore say "saved, but unreadable" rather than "failed".
    ///
    /// **`inputs` is required, not optional (r1.s3.w2, #110).** A fresh run must
    /// name the pair, the exact primary and optional HTF snapshot identities, and
    /// the cost/funding configuration it actually ran with. The `Option` on
    /// [`PersistedRun::inputs`] is a READ-side accommodation for rows written
    /// before migration `0006`, never a write-side choice. Requiring the value —
    /// no `Option` in this signature — is what makes "a fresh run with no
    /// provenance" unrepresentable at the port, before the database trigger has to
    /// catch it.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] if the `strategy_version_id` is absent /
    /// cross-strategy (no row persists), if a `sharpe`/`sortino` value is
    /// non-finite (fail-closed at the boundary), or the store fails.
    fn save_run(
        &self,
        strategy_version_id: &VersionId,
        inputs: &BacktestInputs,
        result: &BacktestResult,
        summary: &SummaryStats,
        starting_equity: Decimal,
    ) -> impl Future<Output = Result<BacktestRunId, DataError>> + Send;

    /// Fetch one persisted run by id (`Ok(None)` if no such row), **#39
    /// re-validate-on-read**.
    ///
    /// Fetches the run's `trade` rows internally in the same read and reconstructs
    /// the full hash input (run totals + `regime_breakdown` + `skipped_entries` +
    /// the `seq`-ordered trades) to re-derive `result_content_hash`, rejecting a
    /// mismatch (tamper guard). Fail-closed on any corrupt/un-parseable row.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] on a re-derived-hash mismatch, an unsupported
    /// stored `schema_version`, a malformed/corrupt column, or a store failure.
    fn get_run(
        &self,
        id: &BacktestRunId,
    ) -> impl Future<Output = Result<Option<PersistedRun>, DataError>> + Send;

    /// The most-recent run for a version (`ORDER BY created_at DESC, id DESC LIMIT
    /// 1`, #40 stable) — the FR-7 prior-run lookup. Reuses `get_run`'s
    /// fetch-trades-and-validate path (so it is fail-closed + tamper-checked).
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] on the same conditions as [`get_run`](Self::get_run).
    fn latest_run_for_version(
        &self,
        strategy_version_id: &VersionId,
    ) -> impl Future<Output = Result<Option<PersistedRun>, DataError>> + Send;

    /// List the run catalog for a version (`ORDER BY created_at, id`, #40 stable).
    ///
    /// **The ONLY method with #39 per-row corrupt-isolation** — a corrupt summary
    /// row is skipped-with-warning (`tracing::warn`), not a whole-list failure,
    /// because a run catalog is best-effort UX.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] only on a store-level failure (the query itself
    /// failing), never for a single un-parseable summary row (that is skipped).
    fn list_runs_for_version(
        &self,
        strategy_version_id: &VersionId,
    ) -> impl Future<Output = Result<Vec<RunSummary>, DataError>> + Send;

    /// Fetch a run's full trade log (`ORDER BY seq`, #40 stable). **Fail-closed**
    /// (audit C9): a trade log feeds P&L/equity/hash reconstruction, so a
    /// corrupt/missing trade row is an `Err`, never silently skipped.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] on any corrupt/un-parseable trade row, an
    /// unsupported stored `schema_version`, or a store failure.
    fn get_trades(
        &self,
        id: &BacktestRunId,
    ) -> impl Future<Output = Result<Vec<Trade>, DataError>> + Send;
}

/// The walk-forward run port (r2.s3.w3 — `rolling-oos/v1` + `wf-v1`, ADR-0025).
///
/// Implemented by the same `SqliteBacktestRunRepo` adapter: a walk-forward run
/// is a parent row whose K folds are **ordinary persisted windowed
/// `backtest_run` rows** — they stay visible through
/// [`BacktestRunRepository::list_runs_for_version`] /
/// [`get_run`](BacktestRunRepository::get_run) (L8) and carry their membership
/// ([`PersistedRun::walk_forward`]). This port adds only the parent + fold-row
/// surface; the run-log surface is unchanged.
///
/// **Walk-forward runs are create + read only**, like every run row (the
/// `0013` `BEFORE UPDATE` / `BEFORE DELETE` triggers enforce it).
pub trait WalkForwardRunRepository {
    /// Persist one walk-forward run **in one transaction** (a9): the
    /// ownership check [`save_run`](BacktestRunRepository::save_run) makes, the
    /// `walk_forward_run` row, then per fold the `backtest_run` + `trade` rows
    /// (with `walk_forward_run_id` + `fold_index` set) and the
    /// `walk_forward_fold` row — any failure rolls everything back; nothing
    /// partial persists.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] if the `strategy_version_id` is absent, a fold
    /// draft violates the schema (a non-windowed run, a duplicated
    /// `fold_index`), or the store fails.
    fn save_walk_forward_run(
        &self,
        strategy_version_id: &VersionId,
        draft: &WalkForwardRunDraft,
    ) -> impl Future<Output = Result<WalkForwardRunId, DataError>> + Send;

    /// Fetch one persisted walk-forward run by id (`Ok(None)` if no such row),
    /// with its folds ordered by `fold_index`. **Fail-closed** like
    /// [`get_run`](BacktestRunRepository::get_run): a fold whose `backtest_run`
    /// is missing or whose columns are corrupt is an `Err`, never a partial
    /// read.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] on a corrupt/un-parseable row or a store
    /// failure.
    fn get_walk_forward_run(
        &self,
        id: &WalkForwardRunId,
    ) -> impl Future<Output = Result<Option<WalkForwardRun>, DataError>> + Send;
}

/// `PulseTrader`'s own LLM chat port (VS-1.3.1 work-1.01, FR-23 / FR-24, README
/// C1).
///
/// The **established** port style — `impl Future<Output = ...> + Send`, consumed
/// generically (`<P: LlmProvider>`), **never `dyn`** — mirrors
/// [`MarketDataSource`] / [`StrategyRepository`] / [`BacktestRunRepository`]. This
/// is deliberately NOT `PulseHive`'s `#[async_trait]` + `dyn`-safe trait: the
/// shapes are `PulseTrader`-OWNED so `PulseHive`'s evolving 2.x API cannot ripple
/// inward (ADR-0012). The
/// [`LlmBackend`](crate::domain::llm::LlmBackend) on
/// [`LlmConfig`](crate::domain::llm::LlmConfig) IS FR-23's "config flag" — the
/// composition root (1.05) `match`es on it and constructs the concrete provider;
/// adding a backend is a new adapter + match arm, no domain refactor.
///
/// **Non-streaming ONLY (v1):** the cost-logged path needs `usage`, and only
/// `chat()` carries it (ADR-0012); streaming is a v1.5 GUI concern. The port takes
/// `tools` **additively** (VS-1.3.2 work-2.01, FR-3): a borrowed
/// `&[ToolDefinition]` slice the composer advertises so the model may return
/// `tool_calls`. An **empty slice** reproduces the VS-1.3.1 no-tools behavior
/// exactly (a response still carries `tool_calls` for forward-compat).
pub trait LlmProvider {
    /// Run one non-streaming chat completion, advertising `tools` to the model.
    ///
    /// `tools` is a borrowed slice of [`ToolDefinition`]s the model may call; pass
    /// `&[]` for the no-tools flow (identical to the VS-1.3.1 behavior).
    ///
    /// # Errors
    ///
    /// Returns [`LlmError`] if the provider / transport fails
    /// ([`LlmError::Provider`]) or the request / config is invalid
    /// ([`LlmError::Config`]).
    fn chat(
        &self,
        messages: Vec<Message>,
        tools: &[ToolDefinition],
        config: &LlmConfig,
    ) -> impl Future<Output = Result<LlmResponse, LlmError>> + Send;
}

/// `PulseTrader`'s append-only [`LlmCall`] ledger persistence port (VS-1.3.1
/// work-1.02, FR-24, README C6).
///
/// The **established** repository style — `impl Future<Output = ...> + Send`,
/// consumed generically (`<R: LlmCallRepository>`), **never `dyn`** — mirrors
/// [`BacktestRunRepository`] / [`StrategyRepository`] (audit C3), so a repository
/// call can be `spawn`ed on tokio's multi-thread runtime. The `SqliteLlmCallRepo`
/// (`adapters::db`) implements it over `pulse.db`; the 1.04 decorator writes
/// through it and the 1.05 demo reads a row back.
///
/// **Calls are create + read only.** A persisted [`LlmCall`] is an append-only
/// verbatim ledger record (FR-24) — there is deliberately NO `update_call` /
/// `delete_call` method; immutability is structural in this API and enforced by the
/// migration-`0004` `BEFORE UPDATE` / `BEFORE DELETE` triggers.
///
/// **Errors are [`DataError::Db`]**, the shared persistence error (like the other
/// repos) — NOT [`LlmError`], which is the *provider*-port error only.
pub trait LlmCallRepository {
    /// Persist one [`LlmCall`] ledger row (append-only, FR-24).
    ///
    /// The stored `created_at` is sourced from the adapter's injected `Clock`
    /// (deterministic under test); every other field is persisted verbatim.
    /// Returns the persisted [`LlmCallId`].
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] if the store fails.
    fn save_call(
        &self,
        call: &LlmCall,
    ) -> impl Future<Output = Result<LlmCallId, DataError>> + Send;

    /// Fetch one persisted call by id (`Ok(None)` if no such row).
    ///
    /// **Fail-closed** (mirror [`BacktestRunRepository::get_run`]): an unsupported
    /// stored `schema_version` or a corrupt/un-parseable column is an `Err`, never a
    /// silent partial.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] on an unsupported stored `schema_version`, a
    /// malformed column, or a store failure.
    fn get_call(
        &self,
        id: &LlmCallId,
    ) -> impl Future<Output = Result<Option<LlmCall>, DataError>> + Send;
}

/// The coaching session persistence port (r1.s2.w2, ADR-0021 / audit C3).
///
/// The `sqlx` adapter
/// ([`SqliteCoachingRepo`](crate::adapters::db::SqliteCoachingRepo)) implements it
/// over `pulse.db`; `w3`'s coach turn and `r1.s4`'s rail consume it generically
/// (`<R: CoachingRepository>`), never as `dyn`. Same `Send`-future style as
/// [`StrategyRepository`] (audit C3) so a repository call can be `spawn`ed.
///
/// **The session row IS the audit trail.** Every coaching outcome persists — a
/// proposal or a typed failure — so "never silence" is a storage guarantee rather
/// than a convention. `llm_call_id` is `None` when no ledger row was correlated to
/// that turn, which does not prove the provider was never called: a timeout or a
/// transport fault is an attempt that can leave no priced row behind.
///
/// **This port persists; it does not decide.** The disposition state machine lives
/// in [`Proposal::transition`](crate::domain::Proposal::transition), and
/// [`record_disposition`](CoachingRepository::record_disposition) writes the
/// disposition it is given. The `0005` `CHECK` constraints are the second guard
/// (no accepted proposal without its child version, and no child version on
/// anything else); the legality of `rejected → accepted` is the caller's to
/// enforce through the domain, which is where `r1.s4`'s rail already goes.
///
/// **No `validated` anywhere** (audit C4): a stored mutation's applicability is
/// re-established by [`apply`](crate::domain::apply) at use time, so there is
/// deliberately no method here that reads or writes such a fact.
pub trait CoachingRepository {
    /// **Reserve** a session id before the provider call (r1.s4.w4).
    ///
    /// This is the operation that makes a coach turn recoverable. It commits a
    /// `pending` row keyed by the claim's opaque
    /// [`CoachRequestFingerprint`](crate::domain::CoachRequestFingerprint) and
    /// returns — **no write transaction is held across the network call** — so a
    /// crash mid-turn leaves a claim to finalize instead of a silence to explain.
    ///
    /// The three [`CoachSessionClaimResult`]s are semantic, not mechanical.
    /// `Claimed` means this call owns the one provider attempt. `Existing` means the
    /// same request already settled and is the idempotent answer. `ExistingPending`
    /// means the same request is still open — and the repository deliberately does
    /// NOT judge whether that claim is live, because it cannot see the process that
    /// made it. `w1`'s process-local single-flight owner decides: reattach a live
    /// call, refuse a duplicate, or finalize a claim left by an earlier process
    /// lifetime through [`finish_session`](CoachingRepository::finish_session) as a
    /// typed `interrupted` — never with a second provider call.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] when the session id is reused with a DIFFERENT
    /// run, version or fingerprint — that is a collision, never an idempotent hit —
    /// and on any store failure.
    fn claim_session(
        &self,
        claim: CoachSessionClaim,
    ) -> impl Future<Output = Result<CoachSessionClaimResult, DataError>> + Send;

    /// Settle a claimed session, exactly once (r1.s4.w4).
    ///
    /// It moves the one claimed `pending` row to a single initial `Proposed` or
    /// `Failed` outcome — `interrupted` included — and attaches the ledger
    /// correlation learned during the turn. It cannot settle an already-terminal
    /// row, cannot attach a second proposal, and is not a route around
    /// [`record_disposition`](CoachingRepository::record_disposition) for a later
    /// disposition: the returned session is the initial record, not a settled one.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] when there is no pending claim under that id (an
    /// absent session, or one that already settled), when the outcome is
    /// [`SessionOutcome::Pending`](crate::domain::SessionOutcome::Pending), when it
    /// carries an already-dispositioned proposal, or on a store failure.
    fn finish_session(
        &self,
        session_id: &CoachingSessionId,
        outcome: InitialCoachOutcome,
    ) -> impl Future<Output = Result<CoachingSession, DataError>> + Send;

    /// Persist one coach turn — the session row, plus its proposal row when the
    /// turn produced one.
    ///
    /// **Round-1 survivor (r1.s4.w4).** Production turn creation moves to
    /// `claim_session` + `finish_session`; this remains for the callers and tests
    /// that still write an initial turn in one act, and it is the one path that may
    /// still insert a terminal row with no request fingerprint. It accepts INITIAL
    /// proposed/failed shapes only — never a claim, and never an
    /// already-modified/accepted/rejected proposal — and `w1` retires the
    /// production bypass.
    ///
    /// The stored `created_at` is sourced from the adapter's injected `Clock`
    /// (deterministic under test); every other field is persisted verbatim.
    /// Returns the persisted [`CoachingSessionId`].
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] if the store fails — including the re-save of a
    /// session id that already exists, which the primary key and the
    /// one-proposal-per-session `UNIQUE` refuse rather than silently double.
    fn save_session(
        &self,
        session: &CoachingSession,
    ) -> impl Future<Output = Result<CoachingSessionId, DataError>> + Send;

    /// Fetch one recorded turn by id (`Ok(None)` if no such row).
    ///
    /// **Fail-closed** (mirror [`LlmCallRepository::get_call`]): an unsupported
    /// stored `schema_version` or a corrupt/un-parseable column is an `Err`, never
    /// a silent partial — a coaching record that reads back wrong is worse than one
    /// that refuses to read.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] on an unsupported stored `schema_version`, a
    /// malformed column, or a store failure.
    fn get_session(
        &self,
        id: &CoachingSessionId,
    ) -> impl Future<Output = Result<Option<CoachingSession>, DataError>> + Send;

    /// Every recorded turn for one backtest run, oldest first — successes and
    /// failures alike.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] on a malformed row or a store failure.
    fn list_sessions_for_run(
        &self,
        run_id: &BacktestRunId,
    ) -> impl Future<Output = Result<Vec<CoachingSession>, DataError>> + Send;

    /// Record a proposal's disposition, keyed by its session id — the accept
    /// idempotency key (`r1.s4`'s consistency model keys one child version per
    /// proposal by session id).
    ///
    /// Dormant in `r1.s2`: `w2` writes only the `Proposed` state, and `r1.s4`'s
    /// rail is what drives the rest.
    ///
    /// **It settles a proposal; it does not edit one.** Since r1.s4.w2 the one
    /// writable target is [`Disposition::Rejected`] — the two refusals below say why
    /// the others are not — and the write is CONDITIONAL on the proposal still being
    /// open (`proposed` or `modified`), which is the state machine in
    /// [`Proposal::transition`](crate::domain::Proposal) enforced where the row
    /// actually changes. Replaying the identical rejection is a no-op.
    ///
    /// [`Disposition::Modified`] is refused here on purpose: a modify replaces the
    /// proposal's stored mutation, and this operation writes only the disposition
    /// columns — recording it would leave a row that says "edited" while carrying
    /// the un-edited mutation. [`record_modification`](Self::record_modification)
    /// is the operation that writes both in one statement.
    ///
    /// **[`Disposition::Accepted`] is refused here too, from r1.s4.w2.**
    /// [`CoachAcceptanceRepository::commit_acceptance`] is the ONE writer of
    /// accepted lineage: it mints the child version and its run inside the same
    /// transaction that settles the proposal, so an accepted row can never name a
    /// child that does not exist or a run that is not that child's. This operation
    /// can only be handed ids some other code minted, which is a second writer of
    /// the same fact — and the moment two writers disagree the version tree stops
    /// being evidence. So the one target it still writes is
    /// [`Disposition::Rejected`].
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] when the session has no proposal to disposition
    /// (an absent session, or a turn that failed), when the requested transition is
    /// not legal from the proposal's current state, when the target is anything but
    /// [`Disposition::Rejected`], or when the store rejects the write.
    fn record_disposition(
        &self,
        id: &CoachingSessionId,
        disposition: &Disposition,
    ) -> impl Future<Output = Result<(), DataError>> + Send;

    /// Record the trader's EDITED mutation and move the proposal to `modified`
    /// (r1.s4.w2, ADR-0021).
    ///
    /// One statement writes both, which is the whole reason this exists next to
    /// [`record_disposition`](Self::record_disposition) rather than inside it: that
    /// operation writes the disposition columns only, so recording `modified`
    /// through it would leave a row saying "edited" while carrying the un-edited
    /// mutation, with no way to tell from the row which value the trader meant.
    ///
    /// **It edits; it does not settle.** The proposal stays open — a modify may
    /// repeat, and an accept afterwards re-applies the LATEST stored mutation. Any
    /// stale accept failure is cleared in the same statement, because a failure
    /// recorded against the previous mutation is not a statement about this one.
    ///
    /// The mutation's applicability is the CALLER's to establish by calling
    /// [`apply`](crate::domain::apply) first (audit C4: validity is use-time and is
    /// never stored). This port persists what it is given.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] when the session has no OPEN proposal to edit — an
    /// absent session, a turn that failed, or a proposal already accepted or
    /// rejected — or on a store failure.
    fn record_modification(
        &self,
        id: &CoachingSessionId,
        mutation: &Mutation,
    ) -> impl Future<Output = Result<Proposal, DataError>> + Send;
}

/// The coach ACCEPT persistence port (r1.s4.w4, ADR-0010 / ADR-0019 / ADR-0021).
///
/// A product-owned seam, separate from [`CoachingRepository`] because it answers a
/// different question: that port records what a TURN produced, this one records
/// what a DECISION did. Two implementations ship — the real `SQLite` adapter and a
/// deterministic in-memory test adapter — and both are consumed generically
/// (`<A: CoachAcceptanceRepository>`), never as `dyn`.
///
/// **Two semantic operations, not transaction steps.** There is deliberately no
/// `begin`/`insert_child`/`insert_run`/`commit` on this trait. Exposing the steps
/// would put the atomicity guarantee in the caller's hands, and the whole point of
/// the seam is that "an accept produced a child, its run and its links, or it
/// produced none of them" is the adapter's promise.
///
/// **Nothing here computes.** Validation, snapshot loading and the deterministic
/// backtest all happen before either method is called, so no write transaction is
/// ever open across network or CPU work.
pub trait CoachAcceptanceRepository {
    /// Record a typed failure of one accept attempt.
    ///
    /// Leaves the disposition where it was — `proposed` or `modified` — and stores
    /// no child and no run: a failed accept mints nothing. The stored failure is
    /// the LATEST accept outcome on the mutable proposal projection, so a second
    /// failed attempt replaces the first rather than appending to it.
    ///
    /// Returns the proposal as it now stands, carrying the failure.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] when the session has no proposal to fail (an
    /// absent session, or a turn that failed), when the proposal is already settled
    /// — `0008` refuses an accept failure on an accepted or rejected row, because
    /// neither is an attempt that can still fail — or on a store failure.
    fn record_accept_failure(
        &self,
        session_id: &CoachingSessionId,
        failure: CoachAcceptFailure,
    ) -> impl Future<Output = Result<Proposal, DataError>> + Send;

    /// Commit one accept: child version, its run and trades, and the proposal's
    /// links — **in a single transaction, or not at all**.
    ///
    /// Inside that transaction the adapter checks the session is `proposed` and has
    /// exactly one attributable `llm_call_id`; checks the proposal is
    /// `proposed`/`modified` with no accepted child or run and still carries the
    /// mutation the accept was computed from; re-reads the parent version's
    /// `latest_walk_forward_run_id` and refuses when it moved since the accept
    /// loaded the parent (the second optimistic lock — the certification gate
    /// computed against the run it named); MINTS the child `VersionId`, the
    /// [`BacktestRunId`] and `created_at` from its injected id/clock sources; and
    /// DERIVES the strategy id, the parent version id, `CreatedBy::CoachLlm` and
    /// the creating call id from the claimed session row.
    /// [`PreparedCoachAcceptance`] carries no identity precisely so the caller
    /// cannot supply provenance that disagrees with the session.
    ///
    /// Replaying an accept for an already-accepted session returns the EXISTING
    /// exact child and run and inserts nothing — the session id is the accept
    /// idempotency key, so a client that lost the response can always retry.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] when the session is not a proposed turn, when it
    /// names no single attributable call, when the proposal is rejected or already
    /// accepted with a different child, or when any part of the write fails — in
    /// which case the whole transaction rolls back and **no child version exists**.
    fn commit_acceptance(
        &self,
        acceptance: PreparedCoachAcceptance,
    ) -> impl Future<Output = Result<AcceptedCoachOutcome, DataError>> + Send;
}

// ---------------------------------------------------------------------------
// The coach turn's two ports (r1.s4.w1)
// ---------------------------------------------------------------------------

/// The projection port: one run id in, one consistent projection out.
///
/// The consistency is structural, not conventional. The application ring asks for a
/// run by id and receives the run, its complete ordered trade set and THE VERSION
/// THAT RUN NAMES together, read under one snapshot — so a caller cannot pair a run
/// with a version it was not produced against, cannot substitute another run's
/// trades, and cannot truncate the set. It has no such input. That is `#132`'s first
/// false audit row made unconstructible.
///
/// `pub(crate)`: the sealed coach turn is a crate-internal use case, and nothing
/// outside this crate implements or names the port (ADR-0015).
pub(crate) trait CoachTurnSource {
    /// Load the projection for `run_id`, or `Ok(None)` when no such run exists.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] when the run, its trades or its version cannot be read
    /// — including the fail-closed integrity and schema checks the repositories
    /// already apply.
    fn load_coach_turn(
        &self,
        run_id: &BacktestRunId,
    ) -> impl Future<Output = Result<Option<CoachTurnProjection>, DataError>> + Send;
}

/// The provider port the sealed turn speaks to: it returns the response and the
/// ledger row TOGETHER, so a session can never name a row another turn produced.
///
/// The production implementation composes the redacting/logging decorator and the
/// capture buffer inside ONE adapter built by the composition root — the module
/// never receives a provider and a capture handle separately, which is the pairing
/// obligation `#132` showed a caller can get wrong.
///
/// `pub(crate)` for the same reason as [`CoachTurnSource`].
pub(crate) trait AttributedCoachProvider {
    /// Make the one call.
    ///
    /// # Errors
    ///
    /// Returns [`AttributedCallError`] for a transport or local fault, and for a
    /// usable response this adapter cannot attribute to exactly one ledger row.
    fn attributed_chat(
        &self,
        messages: Vec<Message>,
        tools: &[ToolDefinition],
        config: &LlmConfig,
    ) -> impl Future<Output = Result<AttributedCall, AttributedCallError>> + Send;

    /// The ledger row the LAST attempt correlated, when the call itself never
    /// returned — the timeout path, where the future is cancelled mid-flight.
    ///
    /// Zero is legitimate here and only here: a timeout can strike before the
    /// decorator writes, and recording `NULL` for a call that was billed would be as
    /// dishonest as inventing an id. Several is legitimate nowhere.
    ///
    /// # Errors
    ///
    /// Returns [`AttributedCallError::LedgerRowsAmbiguous`] when the cancelled
    /// attempt correlated more than one row.
    fn attempted_call_id(&self) -> Result<Option<LlmCallId>, AttributedCallError>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::MarketDataSource;
    use crate::domain::candle::Candle;
    use crate::domain::error::DataError;
    use crate::domain::pair::Pair;
    use crate::domain::series::CandleSeries;
    use crate::domain::timeframe::Timeframe;
    use crate::domain::version::DataVersion;
    use std::future::Future;

    /// A trivial generic fake implementing the port with no I/O.
    struct FakeSource;

    impl MarketDataSource for FakeSource {
        fn fetch_historical(
            &self,
            pair: &Pair,
            tf: Timeframe,
            _start_ms: i64,
            _end_ms: i64,
        ) -> impl Future<Output = Result<CandleSeries, DataError>> {
            std::future::ready(Ok(CandleSeries {
                pair: pair.clone(),
                timeframe: tf,
                version: DataVersion::new("fake"),
                candles: Vec::new(),
            }))
        }

        fn fetch_incremental(
            &self,
            _pair: &Pair,
            _tf: Timeframe,
            _since_ms: i64,
        ) -> impl Future<Output = Result<Vec<Candle>, DataError>> {
            std::future::ready(Ok(Vec::new()))
        }
    }

    // Generic consumption (`<S: MarketDataSource>`) proves the port is used by
    // bound, not as `dyn`, and that the future is `Send` (required by `spawn`).
    async fn fetch_via<S: MarketDataSource>(source: S) -> Result<CandleSeries, DataError> {
        let pair = Pair::new("BTCUSDT");
        source
            .fetch_historical(&pair, Timeframe::H4, 0, 14_400_000)
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fetch_future_is_send_spawnable_on_multi_thread_runtime() {
        // If the port's future were not `Send`, this `spawn` would not compile.
        let handle = tokio::spawn(async { fetch_via(FakeSource).await });
        let series = handle
            .await
            .expect("spawned task joins")
            .expect("fetch succeeds");
        assert_eq!(series.timeframe, Timeframe::H4);
        assert!(series.candles.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fetch_incremental_future_is_send_spawnable() {
        let handle = tokio::spawn(async {
            let pair = Pair::new("BTCUSDT");
            FakeSource.fetch_incremental(&pair, Timeframe::M15, 0).await
        });
        let candles = handle
            .await
            .expect("spawned task joins")
            .expect("fetch succeeds");
        assert!(candles.is_empty());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod repository_tests {
    use super::StrategyRepository;
    use crate::domain::error::DataError;
    use crate::domain::strategy::{
        AgentSubmission, AgentSubmissionId, CreatedBy, NewAgentSubmission, NewVersion, Strategy,
        StrategyId, StrategyVersion, VersionId,
    };
    use std::collections::HashMap;
    use std::sync::Mutex;

    use crate::domain::dsl::{
        Comparator, Condition, Direction, ExitRule, IndicatorSpec, RiskParams, SchemaVersion,
        Series, StrategyDsl, SweepableValue, ValueSource,
    };
    use chrono::{TimeZone, Utc};
    use rust_decimal::Decimal;
    use std::future::Future;

    /// A canonical `1.0.0` typed DSL the fake embeds in created versions.
    fn canonical_dsl() -> StrategyDsl {
        StrategyDsl {
            schema_version: SchemaVersion::CURRENT,
            name: "RSI Oversold".to_owned(),
            direction: Direction::Long,
            entry: Condition::Compare {
                lhs: ValueSource::Indicator {
                    series: Series::Primary,
                    spec: IndicatorSpec::Rsi {
                        period: SweepableValue::Fixed(14),
                    },
                },
                op: Comparator::Lt,
                rhs: ValueSource::Constant {
                    value: Decimal::new(30, 0),
                },
            },
            filters: vec![],
            exits: vec![ExitRule::TakeProfit {
                target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
            }],
            risk: RiskParams {
                risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
                max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
            },
        }
    }

    /// An in-memory, zero-I/O fake implementing `StrategyRepository`. It locks
    /// the trait shape (create + read only for versions — no update/delete
    /// method to implement) and proves the futures are `Send`-spawnable. The
    /// `Mutex`-guarded maps keep the fake `Send + Sync` so its futures are `Send`
    /// even while borrowing `&self`.
    #[derive(Default)]
    struct FakeRepo {
        strategies: Mutex<HashMap<String, Strategy>>,
        versions: Mutex<HashMap<String, StrategyVersion>>,
        submissions: Mutex<HashMap<String, AgentSubmission>>,
        seq: Mutex<u64>,
    }

    impl FakeRepo {
        fn next_id(&self, prefix: &str) -> String {
            let mut seq = self.seq.lock().expect("seq lock");
            *seq += 1;
            format!("{prefix}-{seq}")
        }
    }

    impl StrategyRepository for FakeRepo {
        fn create_strategy(
            &self,
            name: &str,
            owner: Option<&str>,
            tags: &[String],
        ) -> impl Future<Output = Result<Strategy, DataError>> {
            let strat = Strategy {
                id: StrategyId::new(self.next_id("strat")),
                name: name.to_owned(),
                tags: tags.to_vec(),
                owner: owner.map(ToOwned::to_owned),
                pinned_version_id: None,
                archived: false,
                created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            };
            self.strategies
                .lock()
                .expect("strategies lock")
                .insert(strat.id.as_str().to_owned(), strat.clone());
            std::future::ready(Ok(strat))
        }

        fn get_strategy(
            &self,
            id: &StrategyId,
        ) -> impl Future<Output = Result<Option<Strategy>, DataError>> {
            std::future::ready(Ok(self
                .strategies
                .lock()
                .expect("strategies lock")
                .get(id.as_str())
                .cloned()))
        }

        fn list_strategies(
            &self,
            include_archived: bool,
        ) -> impl Future<Output = Result<Vec<Strategy>, DataError>> {
            std::future::ready(Ok(self
                .strategies
                .lock()
                .expect("strategies lock")
                .values()
                .filter(|s| include_archived || !s.archived)
                .cloned()
                .collect()))
        }

        fn rename_strategy(
            &self,
            id: &StrategyId,
            new_name: &str,
        ) -> impl Future<Output = Result<Strategy, DataError>> {
            std::future::ready((|| {
                let mut map = self.strategies.lock().expect("strategies lock");
                let strat = map
                    .get_mut(id.as_str())
                    .ok_or_else(|| DataError::Io("no such strategy".to_owned()))?;
                strat.name = new_name.to_owned();
                Ok(strat.clone())
            })())
        }

        fn set_tags(
            &self,
            id: &StrategyId,
            tags: &[String],
        ) -> impl Future<Output = Result<Strategy, DataError>> {
            std::future::ready((|| {
                let mut map = self.strategies.lock().expect("strategies lock");
                let strat = map
                    .get_mut(id.as_str())
                    .ok_or_else(|| DataError::Io("no such strategy".to_owned()))?;
                strat.tags = tags.to_vec();
                Ok(strat.clone())
            })())
        }

        fn set_pinned_version(
            &self,
            id: &StrategyId,
            version_id: Option<&VersionId>,
        ) -> impl Future<Output = Result<Strategy, DataError>> {
            std::future::ready((|| {
                let mut map = self.strategies.lock().expect("strategies lock");
                let strat = map
                    .get_mut(id.as_str())
                    .ok_or_else(|| DataError::Io("no such strategy".to_owned()))?;
                strat.pinned_version_id = version_id.cloned();
                Ok(strat.clone())
            })())
        }

        fn archive_strategy(
            &self,
            id: &StrategyId,
            archived: bool,
        ) -> impl Future<Output = Result<Strategy, DataError>> {
            std::future::ready((|| {
                let mut map = self.strategies.lock().expect("strategies lock");
                let strat = map
                    .get_mut(id.as_str())
                    .ok_or_else(|| DataError::Io("no such strategy".to_owned()))?;
                strat.archived = archived;
                Ok(strat.clone())
            })())
        }

        fn create_version(
            &self,
            request: NewVersion,
        ) -> impl Future<Output = Result<StrategyVersion, DataError>> {
            let version = StrategyVersion {
                id: VersionId::new(self.next_id("ver")),
                strategy_id: request.strategy_id,
                parent_version_id: request.parent_version_id,
                dsl_schema_version: SchemaVersion::CURRENT,
                dsl: canonical_dsl(),
                dsl_original: request.dsl_json,
                version_hash: "deadbeef".to_owned(),
                created_by: request.created_by,
                creating_llm_call_ids: request.creating_llm_call_ids,
                created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
                latest_walk_forward_run_id: None,
                certified: false,
            };
            self.versions
                .lock()
                .expect("versions lock")
                .insert(version.id.as_str().to_owned(), version.clone());
            std::future::ready(Ok(version))
        }

        fn get_version(
            &self,
            id: &VersionId,
        ) -> impl Future<Output = Result<Option<StrategyVersion>, DataError>> {
            std::future::ready(Ok(self
                .versions
                .lock()
                .expect("versions lock")
                .get(id.as_str())
                .cloned()))
        }

        fn list_versions(
            &self,
            strategy_id: &StrategyId,
        ) -> impl Future<Output = Result<Vec<StrategyVersion>, DataError>> {
            std::future::ready(Ok(self
                .versions
                .lock()
                .expect("versions lock")
                .values()
                .filter(|v| &v.strategy_id == strategy_id)
                .cloned()
                .collect()))
        }

        async fn version_tree(
            &self,
            strategy_id: &StrategyId,
        ) -> Result<Vec<StrategyVersion>, DataError> {
            self.list_versions(strategy_id).await
        }

        async fn create_agent_version(
            &self,
            request: NewVersion,
            submission: NewAgentSubmission,
        ) -> Result<(StrategyVersion, AgentSubmission), DataError> {
            if request.created_by != CreatedBy::ExternalAgent {
                return Err(DataError::Db(
                    "create_agent_version requires created_by = external_agent".to_owned(),
                ));
            }
            if !request.creating_llm_call_ids.is_empty() {
                return Err(DataError::Db(
                    "an external-agent version names no LlmCall rows".to_owned(),
                ));
            }
            let version = self.create_version(request).await?;
            let submission = AgentSubmission {
                id: AgentSubmissionId::new(self.next_id("sub")),
                version_id: version.id.clone(),
                agent_name: submission.agent_name,
                hypothesis: submission.hypothesis,
                created_at: version.created_at,
            };
            self.submissions
                .lock()
                .expect("submissions lock")
                .insert(version.id.as_str().to_owned(), submission.clone());
            Ok((version, submission))
        }

        async fn create_agent_strategy_version(
            &self,
            strategy_name: &str,
            dsl_json: String,
            submission: NewAgentSubmission,
        ) -> Result<(Strategy, StrategyVersion, AgentSubmission), DataError> {
            // The fake's `Mutex` plays the adapter's `BEGIN IMMEDIATE` write
            // lock: the name check and the strategy insert happen under one
            // hold, so the fake honours the same collision-free contract.
            let strat = {
                let mut strategies = self.strategies.lock().expect("strategies lock");
                if strategies.values().any(|s| s.name == strategy_name) {
                    return Err(DataError::StrategyNameTaken {
                        name: strategy_name.to_owned(),
                    });
                }
                let strat = Strategy {
                    id: StrategyId::new(self.next_id("strat")),
                    name: strategy_name.to_owned(),
                    tags: vec![],
                    owner: None,
                    pinned_version_id: None,
                    archived: false,
                    created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
                };
                strategies.insert(strat.id.as_str().to_owned(), strat.clone());
                strat
            };
            let (version, submission) = self
                .create_agent_version(
                    NewVersion {
                        strategy_id: strat.id.clone(),
                        parent_version_id: None,
                        dsl_json,
                        created_by: CreatedBy::ExternalAgent,
                        creating_llm_call_ids: vec![],
                    },
                    submission,
                )
                .await?;
            Ok((strat, version, submission))
        }

        fn get_agent_submission(
            &self,
            version_id: &VersionId,
        ) -> impl Future<Output = Result<Option<AgentSubmission>, DataError>> {
            std::future::ready(Ok(self
                .submissions
                .lock()
                .expect("submissions lock")
                .get(version_id.as_str())
                .cloned()))
        }
    }

    /// Generic consumption (`<R: StrategyRepository>`) proves the port is used by
    /// bound, not as `dyn`, and that the create→read futures are `Send` (required
    /// by `spawn`). Round-trips a strategy then a version.
    async fn roundtrip_via<R: StrategyRepository>(
        repo: R,
    ) -> Result<(Strategy, StrategyVersion), DataError> {
        let created = repo
            .create_strategy("Demo", Some("alice"), &["btc".to_owned()])
            .await?;
        let fetched = repo
            .get_strategy(&created.id)
            .await?
            .expect("created strategy is fetchable");

        let version = repo
            .create_version(NewVersion {
                strategy_id: fetched.id.clone(),
                parent_version_id: None,
                dsl_json: r#"{"schema_version":"1.0.0"}"#.to_owned(),
                created_by: CreatedBy::Human,
                creating_llm_call_ids: vec![],
            })
            .await?;
        let fetched_version = repo
            .get_version(&version.id)
            .await?
            .expect("created version is fetchable");

        Ok((fetched, fetched_version))
    }

    /// AC-17 (FR-11): the port double is `Send`-spawnable and create→read
    /// round-trips for both a strategy and a version. The ABSENCE of any
    /// `update_version`/`delete_version` method is demonstrated by `FakeRepo`
    /// having none to implement (FR-4 immutability is structural in the API).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repository_port_double_create_read_only() {
        // If the port's futures were not `Send`, this `spawn` would not compile.
        let handle = tokio::spawn(async { roundtrip_via(FakeRepo::default()).await });
        let (strat, version) = handle
            .await
            .expect("spawned task joins")
            .expect("round-trip succeeds");

        assert_eq!(strat.name, "Demo");
        assert_eq!(strat.owner.as_deref(), Some("alice"));
        assert_eq!(version.strategy_id, strat.id);
        assert_eq!(version.created_by, CreatedBy::Human);
        // The verbatim source bytes survived the create→read round-trip.
        assert_eq!(version.dsl_original, r#"{"schema_version":"1.0.0"}"#);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod backtest_run_repository_tests {
    use super::BacktestRunRepository;
    use crate::domain::backtest::{
        BacktestInputs, BacktestResult, BacktestRunId, FundingConfig, PersistedRun, RunSummary,
        SnapshotSelection, SummaryStats, Trade,
    };
    use crate::domain::error::DataError;
    use crate::domain::pair::Pair;
    use crate::domain::strategy::VersionId;
    use crate::domain::timeframe::Timeframe;
    use crate::domain::version::DataVersion;
    use rust_decimal::Decimal;
    use std::collections::HashMap;
    use std::future::Future;
    use std::sync::Mutex;

    /// An in-memory, zero-I/O fake implementing `BacktestRunRepository`. It locks
    /// the trait shape (create + read only — no `update_run`/`delete_run` to
    /// implement) and proves the futures are `Send`-spawnable. The `Mutex`-guarded
    /// map keeps the fake `Send + Sync` so its futures are `Send` even while
    /// borrowing `&self` (mirror `FakeRepo`).
    #[derive(Default)]
    struct FakeRunRepo {
        runs: Mutex<HashMap<String, (PersistedRun, Vec<Trade>)>>,
        seq: Mutex<u64>,
    }

    impl FakeRunRepo {
        fn next_id(&self) -> String {
            let mut seq = self.seq.lock().expect("seq lock");
            *seq += 1;
            format!("run-{seq}")
        }
    }

    impl BacktestRunRepository for FakeRunRepo {
        fn save_run(
            &self,
            strategy_version_id: &VersionId,
            inputs: &BacktestInputs,
            result: &BacktestResult,
            summary: &SummaryStats,
            starting_equity: Decimal,
        ) -> impl Future<Output = Result<BacktestRunId, DataError>> {
            let id = BacktestRunId::new(self.next_id());
            let persisted = PersistedRun {
                id: id.clone(),
                strategy_version_id: strategy_version_id.clone(),
                // The fake stores what it was given: `save_run` takes inputs by
                // value, so a fresh run with no provenance is unrepresentable here
                // too, not merely rejected by the database.
                inputs: Some(inputs.clone()),
                schema_version: 1,
                created_at: "2026-06-30T00:00:00.000Z".to_owned(),
                engine_fingerprint: result.engine_fingerprint.as_str().to_owned(),
                engine_target: "test-target".to_owned(),
                result_content_hash: result.result_content_hash(),
                starting_equity,
                net_pnl: result.net_pnl,
                fees_total: result.fees_total,
                funding_total: result.funding_total,
                slippage_total: result.slippage_total,
                summary: summary.clone(),
                regime_breakdown: result.regime_breakdown,
                skipped_entries: result.skipped_entries,
                open_position: result.open_position.clone(),
                // The fake's `save_run` is the standalone path — it never
                // persists a run as a walk-forward fold.
                walk_forward: None,
            };
            self.runs
                .lock()
                .expect("runs lock")
                .insert(id.as_str().to_owned(), (persisted, result.trades.clone()));
            std::future::ready(Ok(id))
        }

        fn get_run(
            &self,
            id: &BacktestRunId,
        ) -> impl Future<Output = Result<Option<PersistedRun>, DataError>> {
            std::future::ready(Ok(self
                .runs
                .lock()
                .expect("runs lock")
                .get(id.as_str())
                .map(|(run, _)| run.clone())))
        }

        fn latest_run_for_version(
            &self,
            strategy_version_id: &VersionId,
        ) -> impl Future<Output = Result<Option<PersistedRun>, DataError>> {
            std::future::ready(Ok(self
                .runs
                .lock()
                .expect("runs lock")
                .values()
                .filter(|(run, _)| &run.strategy_version_id == strategy_version_id)
                .map(|(run, _)| run.clone())
                .last()))
        }

        fn list_runs_for_version(
            &self,
            strategy_version_id: &VersionId,
        ) -> impl Future<Output = Result<Vec<RunSummary>, DataError>> {
            std::future::ready(Ok(self
                .runs
                .lock()
                .expect("runs lock")
                .values()
                .filter(|(run, _)| &run.strategy_version_id == strategy_version_id)
                .map(|(run, _)| RunSummary {
                    id: run.id.clone(),
                    strategy_version_id: run.strategy_version_id.clone(),
                    schema_version: run.schema_version,
                    created_at: run.created_at.clone(),
                    engine_fingerprint: run.engine_fingerprint.clone(),
                    engine_target: run.engine_target.clone(),
                    result_content_hash: run.result_content_hash.clone(),
                    net_pnl: run.net_pnl,
                    expectancy: run.summary.expectancy,
                    trade_count: run.summary.trade_count,
                })
                .collect()))
        }

        fn get_trades(
            &self,
            id: &BacktestRunId,
        ) -> impl Future<Output = Result<Vec<Trade>, DataError>> {
            std::future::ready(Ok(self
                .runs
                .lock()
                .expect("runs lock")
                .get(id.as_str())
                .map(|(_, trades)| trades.clone())
                .unwrap_or_default()))
        }
    }

    /// Generic consumption (`<R: BacktestRunRepository>`) proves the port is used by
    /// bound, not as `dyn`, and that the save→read futures are `Send` (required by
    /// `spawn`). Round-trips a saved run.
    async fn roundtrip_via<R: BacktestRunRepository>(
        repo: R,
    ) -> Result<(PersistedRun, Vec<Trade>), DataError> {
        let version_id = VersionId::new("ver-1");
        let result = BacktestResult {
            trades: Vec::new(),
            net_pnl: Decimal::ZERO,
            fees_total: Decimal::ZERO,
            funding_total: Decimal::ZERO,
            slippage_total: Decimal::ZERO,
            regime_breakdown: crate::domain::backtest::RegimeBreakdown::default(),
            skipped_entries: crate::domain::sizing::SkippedEntryCounts::default(),
            open_position: None,
            engine_fingerprint: crate::domain::EngineFingerprint::current(),
            summary: SummaryStats::default(),
            equity_curve: crate::domain::backtest::EquityCurve::default(),
        };
        let inputs = BacktestInputs {
            pair: Pair::new("BTCUSDT"),
            primary: SnapshotSelection {
                timeframe: Timeframe::M15,
                data_version: DataVersion::new("v-primary"),
            },
            htf: None,
            taker_fee_bps: Decimal::new(4, 0),
            slippage_bps: Decimal::new(1, 0),
            funding: FundingConfig::SnapshotRates,
            window: None,
            lead_in_from_ms: None,
        };
        let id = repo
            .save_run(
                &version_id,
                &inputs,
                &result,
                &SummaryStats::default(),
                Decimal::ZERO,
            )
            .await?;
        let fetched = repo.get_run(&id).await?.expect("saved run is fetchable");
        let trades = repo.get_trades(&id).await?;
        Ok((fetched, trades))
    }

    /// AC-7 (D8): the run-repo port double is `Send`-spawnable and consumed
    /// generically (`<R: BacktestRunRepository>`, never `dyn`). The ABSENCE of any
    /// `update_run`/`delete_run` method is demonstrated by `FakeRunRepo` having none
    /// to implement (FR-6 immutability is structural in the API).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backtest_run_repo_port_double_is_send_spawnable() {
        // If the port's futures were not `Send`, this `spawn` would not compile.
        let handle = tokio::spawn(async { roundtrip_via(FakeRunRepo::default()).await });
        let (run, trades) = handle
            .await
            .expect("spawned task joins")
            .expect("round-trip succeeds");

        assert_eq!(run.strategy_version_id, VersionId::new("ver-1"));
        assert_eq!(run.schema_version, 1);
        assert!(trades.is_empty());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod llm_provider_tests {
    use super::LlmProvider;
    use crate::domain::llm::{
        LlmBackend, LlmConfig, LlmError, LlmResponse, Message, TokenUsage, ToolDefinition,
    };
    use std::future::Future;

    /// An in-memory, zero-I/O fake implementing [`LlmProvider`]. It echoes the
    /// last user message back as the completion and reports fixed token usage,
    /// proving the port's future is `Send` (spawnable) and consumed generically
    /// (`<P: LlmProvider>`, never `dyn`) — mirrors `FakeSource` / `FakeRepo` /
    /// `FakeRunRepo`.
    struct FakeProvider;

    impl LlmProvider for FakeProvider {
        fn chat(
            &self,
            messages: Vec<Message>,
            _tools: &[ToolDefinition],
            _config: &LlmConfig,
        ) -> impl Future<Output = Result<LlmResponse, LlmError>> {
            let content = messages.iter().rev().find_map(|m| match m {
                Message::User { content } => Some(content.clone()),
                _ => None,
            });
            std::future::ready(Ok(LlmResponse {
                content,
                tool_calls: Vec::new(),
                usage: TokenUsage {
                    input_tokens: 7,
                    output_tokens: 2,
                },
            }))
        }
    }

    /// Generic consumption (`<P: LlmProvider>`) proves the port is used by bound,
    /// not as `dyn`, and that the returned future is `Send` (required by `spawn`).
    async fn chat_via<P: LlmProvider>(provider: P) -> Result<LlmResponse, LlmError> {
        let config = LlmConfig {
            backend: LlmBackend::Ollama,
            model: "gpt-oss:120b".to_owned(),
            temperature: 0.5,
            max_tokens: 128,
            reasoning_effort: None,
        };
        provider
            .chat(vec![Message::user("ping")], &[], &config)
            .await
    }

    /// AC-9: the [`LlmProvider`] future is `Send`-spawnable on the multi-thread
    /// runtime (mirror `fetch_future_is_send_spawnable_on_multi_thread_runtime`).
    /// If the port's future were not `Send`, this `spawn` would not compile.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn llm_provider_future_is_send() {
        let handle = tokio::spawn(async { chat_via(FakeProvider).await });
        let response = handle
            .await
            .expect("spawned task joins")
            .expect("chat succeeds");
        assert_eq!(response.content.as_deref(), Some("ping"));
        assert_eq!(response.usage.input_tokens, 7);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod llm_call_repository_tests {
    use super::LlmCallRepository;
    use crate::domain::error::DataError;
    use crate::domain::llm::{LlmBackend, Message};
    use crate::domain::llm_call::{LlmCall, LlmCallId};
    use crate::domain::strategy::CreatedBy;
    use chrono::{TimeZone, Utc};
    use rust_decimal::Decimal;
    use std::collections::HashMap;
    use std::future::Future;
    use std::sync::Mutex;

    /// An in-memory, zero-I/O fake implementing [`LlmCallRepository`]. It locks the
    /// trait shape (create + read only — no `update_call`/`delete_call` to
    /// implement, so FR-24 immutability is structural in the API) and proves the
    /// futures are `Send`-spawnable. The `Mutex`-guarded map keeps the fake
    /// `Send + Sync` so its futures are `Send` while borrowing `&self` (mirror
    /// `FakeRunRepo`).
    #[derive(Default)]
    struct FakeLlmCallRepo {
        calls: Mutex<HashMap<String, LlmCall>>,
    }

    impl LlmCallRepository for FakeLlmCallRepo {
        fn save_call(&self, call: &LlmCall) -> impl Future<Output = Result<LlmCallId, DataError>> {
            self.calls
                .lock()
                .expect("calls lock")
                .insert(call.id.as_str().to_owned(), call.clone());
            std::future::ready(Ok(call.id.clone()))
        }

        fn get_call(
            &self,
            id: &LlmCallId,
        ) -> impl Future<Output = Result<Option<LlmCall>, DataError>> {
            std::future::ready(Ok(self
                .calls
                .lock()
                .expect("calls lock")
                .get(id.as_str())
                .cloned()))
        }
    }

    fn sample_call() -> LlmCall {
        LlmCall {
            id: LlmCallId::new("call-1"),
            backend: LlmBackend::Ollama,
            model: "gpt-oss:120b".to_owned(),
            prompt_messages: vec![Message::system("be terse"), Message::user("hi")],
            completion: Some("hello".to_owned()),
            input_tokens: 7,
            output_tokens: 2,
            cost: Decimal::new(1234, 4),
            cost_currency: "CNY".to_owned(),
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            created_by: CreatedBy::ComposerLlm,
            key_source: None,
            prompt_version: None,
        }
    }

    /// Generic consumption (`<R: LlmCallRepository>`) proves the port is used by
    /// bound, not as `dyn`, and that the save→read futures are `Send` (required by
    /// `spawn`). Round-trips a saved call.
    async fn roundtrip_via<R: LlmCallRepository>(repo: R) -> Result<Option<LlmCall>, DataError> {
        let call = sample_call();
        let id = repo.save_call(&call).await?;
        repo.get_call(&id).await
    }

    /// The call-repo port double is `Send`-spawnable and consumed generically
    /// (`<R: LlmCallRepository>`, never `dyn`). The ABSENCE of any
    /// `update_call`/`delete_call` method is demonstrated by `FakeLlmCallRepo`
    /// having none to implement (FR-24 immutability is structural in the API).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn llm_call_repo_port_double_is_send_spawnable() {
        // If the port's futures were not `Send`, this `spawn` would not compile.
        let handle = tokio::spawn(async { roundtrip_via(FakeLlmCallRepo::default()).await });
        let fetched = handle
            .await
            .expect("spawned task joins")
            .expect("round-trip succeeds")
            .expect("saved call is fetchable");

        assert_eq!(fetched.id, LlmCallId::new("call-1"));
        assert_eq!(fetched.backend, LlmBackend::Ollama);
        assert_eq!(fetched.cost_currency, "CNY");
        assert_eq!(fetched.cost, Decimal::new(1234, 4));
    }
}
