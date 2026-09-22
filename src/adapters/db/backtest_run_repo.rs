//! The `SQLite` adapter implementing the [`BacktestRunRepository`] port
//! (VS-1.2.4 work-4.04, FR-6 / FR-7 / NFR-2).
//!
//! This is the ONLY place `query!`/`query_as!` macros for the `backtest_run` /
//! `trade` tables live (`sqlx` is confined to `adapters::db`, mirror
//! `strategy_repo.rs:1-6`); the committed `.sqlx/` offline cache is keyed to the
//! macros in this file (regenerate with `cargo sqlx prepare` under sqlx-cli
//! `=0.8.6`).
//!
//! **Typed projection, never a blob (D1, #68).** `save_run` writes EXPLICIT
//! columns per README C4 + the `schema_version` tag; it NEVER round-trips a
//! serde-serialized [`BacktestResult`]. Read-back is independent of serde
//! field-presence.
//!
//! **Decimal canonicalization mirrors the hash (D2).** Every `Decimal` column is
//! stored via [`feed_decimal_text`] = `.normalize().to_string()` — the SAME
//! canonicalization `result.rs:231-232`'s `feed_decimal` uses — so a reloaded run
//! re-derives the IDENTICAL `result_content_hash`. All money columns are TEXT,
//! never REAL/f64 (NFR-2).
//!
//! **#39 ownership-on-write (D3).** `save_run` asserts the `strategy_version_id`
//! row exists (FK + an explicit `SELECT 1 … WHERE id = ?` guard INSIDE the
//! transaction, mirror `strategy_repo.rs:313-328`'s `set_pinned_version`); an
//! absent / cross-strategy id is a real `Err`, no row persists. The run + all its
//! trades + the read-back run run in ONE transaction (mirror `create_version`).
//!
//! **#39 re-validate-on-read — the hash is TRADE-DEPENDENT (D4).** `get_run`
//! fetches the run's `trade` rows INTERNALLY in the same read and reconstructs the
//! FULL hash input — run totals + `regime_breakdown` + `skipped_entries` + the
//! `seq`-ordered trades — by rebuilding a [`BacktestResult`] and calling
//! [`BacktestResult::result_content_hash`] (which feeds `feed_money_math` +
//! `feed_regime_breakdown` in their frozen `result.rs:161-187` order), then
//! rejects a mismatch with [`DataError::Db`] (mirror `row_to_version`'s re-derive
//! tamper guard). The persistence layer is lossless against the frozen oracle.
//!
//! **Corrupt-isolation scope (D5).** `list_runs_for_version` skips a corrupt
//! summary row with `tracing::warn` (best-effort catalog). `get_run`,
//! `latest_run_for_version`, AND `get_trades` FAIL-CLOSED — any corrupt/
//! un-parseable row → `Err`, never a partial result.
//!
//! **#65 real errors (D9).** Every precondition is a real `Result::Err`
//! ([`DataError::Db`]), NEVER `debug_assert!` (the determinism gate runs
//! `--release`, where `debug_assert!` is compiled out).
//!
//! NO `#[derive(Debug)]` on the repo struct: the `C: Clock` carries no `Debug`
//! bound (mirror `strategy_repo.rs:47-48`).

use chrono::{DateTime, SecondsFormat};
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::adapters::clock::SystemClock;
use crate::domain::backtest::{
    BacktestInputs, BacktestResult, BacktestRunId, CandleWindow, EquityCurve, ExitReason, Fill,
    FundingConfig, OpenPositionMark, PersistedRun, Regime, RegimeBreakdown, RunSummary,
    SnapshotSelection, SummaryStats, Trade, TradeSource, WalkForwardMembership, WalkForwardRunId,
};
use crate::domain::sizing::SkippedEntryCounts;
use crate::domain::strategy::VersionId;
use crate::domain::{
    BacktestRunRepository, Clock, DataError, DataVersion, Direction, EngineFingerprint, Pair,
    Timeframe,
};

/// The run-row schema tag `save_run` writes into every `backtest_run.schema_version`
/// and that every read ASSERTS (D1b, audit C5). v1 reads only v1 and rejects the
/// rest with a real `DataError::Db` — a load-bearing read control point, NOT a
/// ceremonial column. Forward-compat decoding (branch-by-version) is future work.
const RUN_SCHEMA_VERSION: i64 = 1;

/// The `SQLite` [`BacktestRunRepository`](crate::domain::port::BacktestRunRepository)
/// adapter over `pulse.db`.
///
/// Constructed from a [`SqlitePool`] (cloned from `Db::pool()`). Carries an
/// injected [`Clock`] (the `created_at` source, D7).
///
/// No `#[derive(Debug)]`: `C: Clock` carries no `Debug` bound (mirror
/// `SqliteStrategyRepo`).
pub struct SqliteBacktestRunRepo<C: Clock> {
    pub(crate) pool: SqlitePool,
    clock: C,
}

impl SqliteBacktestRunRepo<SystemClock> {
    /// The production constructor: the wall-clock [`SystemClock`].
    #[must_use]
    pub fn new(pool: SqlitePool) -> SqliteBacktestRunRepo<SystemClock> {
        SqliteBacktestRunRepo {
            pool,
            clock: SystemClock,
        }
    }
}

impl<C: Clock> SqliteBacktestRunRepo<C> {
    /// The test/injection seam: supply a [`Clock`] so `created_at` is deterministic
    /// and the `ORDER BY created_at` is `FakeClock`-testable (D7). Mirrors
    /// `SqliteStrategyRepo::with_deps`, but DROPS the `Migrator` field (a run repo
    /// stores no migration-aware DSL).
    #[must_use]
    pub fn with_deps(pool: SqlitePool, clock: C) -> SqliteBacktestRunRepo<C> {
        SqliteBacktestRunRepo { pool, clock }
    }

    /// The current `created_at`, sourced from the injected [`Clock`] (D7),
    /// serialized as an RFC3339 millisecond UTC string for the `TEXT` column.
    /// `pub(crate)` so `walk_forward_run_repo`'s impl mints the same timestamp
    /// shape (r2.s3.w3).
    pub(crate) fn now_rfc3339(&self) -> Result<String, DataError> {
        let now_ms = self.clock.now_ms();
        let dt = DateTime::from_timestamp_millis(now_ms).ok_or_else(|| {
            DataError::Db(format!("clock.now_ms() {now_ms} is out of DateTime range"))
        })?;
        Ok(dt.to_rfc3339_opts(SecondsFormat::Millis, true))
    }
}

/// Canonicalize a `Decimal` for storage EXACTLY as `result.rs:231-232`'s
/// `feed_decimal` does — `.normalize().to_string()` — so a reloaded run re-derives
/// the IDENTICAL `result_content_hash` (D2). `0.10` and `0.1` collapse to one
/// canonical text; `Decimal` has no `-0`/`NaN`/`Inf`, so this is total.
pub(crate) fn decimal_text(value: Decimal) -> String {
    value.normalize().to_string()
}

/// Parse a `Decimal` `TEXT` column back, fail-closed on a malformed value (D5).
pub(crate) fn parse_decimal(column: &str, s: &str) -> Result<Decimal, DataError> {
    s.parse::<Decimal>()
        .map_err(|e| DataError::Db(format!("malformed Decimal in `{column}` = `{s}`: {e}")))
}

/// Parse an optional `Decimal` `TEXT NULL` column (`None` ⇄ SQL NULL).
fn parse_opt_decimal(column: &str, s: Option<&str>) -> Result<Option<Decimal>, DataError> {
    match s {
        None => Ok(None),
        Some(v) => Ok(Some(parse_decimal(column, v)?)),
    }
}

/// Serialize an `Option<f64>` stat (sharpe/sortino) for storage (D2b, audit C10):
/// a FINITE `Some` round-trips via f64 `to_string()`; `None` becomes SQL NULL.
/// NaN/Inf are FORBIDDEN — a non-finite value fails-closed with [`DataError::Db`]
/// rather than persist (they should never arrive non-finite per the C1
/// `None`-when-degenerate contract, but the boundary fails-closed).
fn f64_stat_text(column: &str, value: Option<f64>) -> Result<Option<String>, DataError> {
    match value {
        None => Ok(None),
        Some(x) if x.is_finite() => Ok(Some(x.to_string())),
        Some(x) => Err(DataError::Db(format!(
            "refusing to persist non-finite `{column}` = {x} (NaN/Inf forbidden at the storage boundary, audit C10)"
        ))),
    }
}

/// Parse an `Option<f64>` stat `TEXT NULL` column back (D2b): `Some` round-trips
/// to the identical `f64`, NULL → `None`. A stored non-finite/garbled value
/// fails-closed (defensive — `save_run` never writes one).
fn parse_opt_f64(column: &str, s: Option<&str>) -> Result<Option<f64>, DataError> {
    match s {
        None => Ok(None),
        Some(v) => {
            let parsed = v
                .parse::<f64>()
                .map_err(|e| DataError::Db(format!("malformed f64 in `{column}` = `{v}`: {e}")))?;
            if parsed.is_finite() {
                Ok(Some(parsed))
            } else {
                Err(DataError::Db(format!(
                    "non-finite f64 in `{column}` = `{v}` (corrupt row)"
                )))
            }
        }
    }
}

/// Parse a JSON `TEXT` column into a deserializable value, fail-closed on a
/// malformed payload (the `fills` / `regime_breakdown` inline JSON, D5).
fn parse_json<T: serde::de::DeserializeOwned>(column: &str, s: &str) -> Result<T, DataError> {
    serde_json::from_str(s)
        .map_err(|e| DataError::Db(format!("malformed JSON in `{column}` = `{s}`: {e}")))
}

/// Parse a `Direction` `TEXT` column (`snake_case` JSON token), fail-closed.
fn parse_direction(s: &str) -> Result<Direction, DataError> {
    parse_json("trade.direction", &json_token(s))
}

/// Parse an `ExitReason` `TEXT` column (`snake_case` JSON token), fail-closed.
fn parse_exit_reason(s: &str) -> Result<ExitReason, DataError> {
    parse_json("trade.exit_reason", &json_token(s))
}

/// Parse a `TradeSource` `TEXT` column (`snake_case` JSON token), fail-closed.
fn parse_trade_source(s: &str) -> Result<TradeSource, DataError> {
    parse_json("trade.source", &json_token(s))
}

/// Parse a `Regime` `TEXT` column (`snake_case` JSON token), fail-closed (#49).
fn parse_regime(s: &str) -> Result<Regime, DataError> {
    parse_json("trade.regime", &json_token(s))
}

/// Parse a `Timeframe` `TEXT` column, fail-closed (r1.s3.w2). `Timeframe`'s serde
/// representation IS the Binance interval string (`15m` / `4h`), so the same
/// quote-and-decode trick the enum columns use round-trips it exactly — no second
/// text mapping to keep in sync with `binance_interval()`.
fn parse_timeframe(column: &str, s: &str) -> Result<Timeframe, DataError> {
    parse_json(column, &json_token(s))
}

/// Parse a `FundingConfig` `TEXT` column (`snake_case` token), fail-closed. An
/// unknown discriminant is an error, never a silent default: a run that claims a
/// funding source this binary does not understand is not a run this binary may
/// report on.
fn parse_funding(column: &str, s: &str) -> Result<FundingConfig, DataError> {
    parse_json(column, &json_token(s))
}

/// Wrap a bare `snake_case` enum token in JSON quotes so `serde_json` can decode it
/// (the enums serialize as `"trending_up"` etc.; the column stores the bare token
/// `trending_up`, so we re-quote on read).
fn json_token(s: &str) -> String {
    format!("\"{s}\"")
}

/// The bare `snake_case` token an enum serializes to, for the `TEXT` column (strips
/// the JSON quotes `serde_json::to_string` adds).
fn enum_token<T: serde::Serialize>(value: &T) -> Result<String, DataError> {
    let quoted = serde_json::to_string(value).map_err(|e| DataError::Db(e.to_string()))?;
    Ok(quoted.trim_matches('"').to_owned())
}

impl<C: Clock + Send + Sync> BacktestRunRepository for SqliteBacktestRunRepo<C> {
    // The typed projection writes 32 run columns + N trade rows in one tx; the line
    // count is intrinsic to the explicit-column mapping (D1, NOT a blob). `mfe_r` /
    // `mae_r` are domain field names (similar by construction).
    #[allow(clippy::too_many_lines, clippy::similar_names)]
    async fn save_run(
        &self,
        strategy_version_id: &VersionId,
        inputs: &BacktestInputs,
        result: &BacktestResult,
        summary: &SummaryStats,
        starting_equity: Decimal,
    ) -> Result<BacktestRunId, DataError> {
        let version_id_str = strategy_version_id.as_str().to_owned();
        let run_id = Uuid::new_v4().to_string();
        let created_at = self.now_rfc3339()?;

        // The two version tags are checked BEFORE the transaction opens, so an
        // unsafe one persists nothing at all rather than aborting a partly-built
        // write (see `check_inputs_path_safe`).
        check_inputs_path_safe(inputs)?;

        // INSERT run + ALL trades + read-back in ONE transaction (D3, mirror
        // `create_version`'s begin → insert → commit → read-back).
        // `BEGIN IMMEDIATE` (R4's class, closed here too): the ownership READ
        // below is the transaction's first statement, and a deferred transaction
        // that reads first and writes later can be beaten by a concurrent commit
        // in WAL — the upgrade then fails with `SQLITE_BUSY_SNAPSHOT`, which
        // `busy_timeout` does not retry. Taking the write lock up front makes the
        // concurrent save wait for it instead (the timeout does cover that).
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        // #39 ownership-on-write: the strategy_version_id row MUST exist (FK +
        // explicit SELECT 1 guard INSIDE the tx — mirror `set_pinned_version`'s
        // ownership check). An absent / cross-strategy id is a real Err, no row
        // persists (the tx is dropped/rolled back).
        let owns = sqlx::query!(
            r#"SELECT 1 AS "one!: i64" FROM strategy_version WHERE id = ?1"#,
            version_id_str,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;
        if owns.is_none() {
            return Err(DataError::Db(format!(
                "cannot save run: strategy_version `{version_id_str}` does not exist (#39 ownership-on-write)"
            )));
        }

        insert_run_row(
            &mut tx,
            &run_id,
            &version_id_str,
            &created_at,
            inputs,
            result,
            summary,
            starting_equity,
            None,
        )
        .await?;
        insert_trade_rows(&mut tx, &run_id, &result.trades).await?;

        tx.commit()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        // r1.s3.w3: `save_run` ends HERE, at the commit, and returns the minted id.
        //
        // It used to run `self.get_run(..)` at this point. That read executed AFTER
        // `tx.commit()`, on a different pooled connection — so it was never part of
        // the transaction the doc claimed it was, and, worse, it returned a bare
        // `DataError` that DISCARDED the id of a row that had already committed. A
        // caller could not then say "the run was saved but could not be read", which
        // is exactly the guarantee the desktop command owes its user.
        //
        // Nothing is lost: the #39 re-validate-on-read tamper guard lives in
        // `get_run`, every read path still runs it, and W3's use case calls `get_run`
        // immediately after this returns — with the id in hand.
        Ok(BacktestRunId::new(run_id))
    }

    // The read reconstructs the full typed projection (32 columns) + re-derives the
    // trade-dependent hash; the line count is intrinsic to the explicit mapping (D4).
    #[allow(clippy::too_many_lines)]
    async fn get_run(&self, id: &BacktestRunId) -> Result<Option<PersistedRun>, DataError> {
        let id_str = id.as_str();
        let row = sqlx::query!(
            r#"SELECT
                 id                      AS "id!: String",
                 strategy_version_id     AS "strategy_version_id!: String",
                 schema_version          AS "schema_version!: i64",
                 created_at              AS "created_at!: String",
                 engine_fingerprint      AS "engine_fingerprint!: String",
                 engine_target           AS "engine_target!: String",
                 result_content_hash     AS "result_content_hash!: String",
                 starting_equity         AS "starting_equity!: String",
                 net_pnl                 AS "net_pnl!: String",
                 fees_total              AS "fees_total!: String",
                 funding_total           AS "funding_total!: String",
                 slippage_total          AS "slippage_total!: String",
                 expectancy              AS "expectancy?: String",
                 win_rate                AS "win_rate?: String",
                 profit_factor           AS "profit_factor?: String",
                 gross_profit            AS "gross_profit?: String",
                 gross_loss              AS "gross_loss?: String",
                 avg_win                 AS "avg_win?: String",
                 avg_loss                AS "avg_loss?: String",
                 max_drawdown            AS "max_drawdown?: String",
                 trade_count             AS "trade_count?: i64",
                 wins                    AS "wins?: i64",
                 losses                  AS "losses?: i64",
                 breakeven               AS "breakeven?: i64",
                 max_win_streak          AS "max_win_streak?: i64",
                 max_loss_streak         AS "max_loss_streak?: i64",
                 sharpe                  AS "sharpe?: String",
                 sortino                 AS "sortino?: String",
                 regime_breakdown        AS "regime_breakdown?: String",
                 skipped_sub_lot         AS "skipped_sub_lot?: i64",
                 skipped_sub_notional    AS "skipped_sub_notional?: i64",
                 skipped_leverage_capped AS "skipped_leverage_capped?: i64",
                 pair                    AS "pair?: String",
                 primary_timeframe       AS "primary_timeframe?: String",
                 primary_data_version    AS "primary_data_version?: String",
                 htf_timeframe           AS "htf_timeframe?: String",
                 htf_data_version        AS "htf_data_version?: String",
                 taker_fee_bps           AS "taker_fee_bps?: String",
                 slippage_bps            AS "slippage_bps?: String",
                 funding_config          AS "funding_config?: String",
                 window_from_ms          AS "window_from_ms?: i64",
                 window_to_ms            AS "window_to_ms?: i64",
                 window_lead_in_from_ms  AS "window_lead_in_from_ms?: i64",
                 open_position           AS "open_position?: String",
                 walk_forward_run_id     AS "walk_forward_run_id?: String",
                 fold_index              AS "fold_index?: i64"
               FROM backtest_run WHERE id = ?1"#,
            id_str,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        let Some(r) = row else { return Ok(None) };

        // D1b: the stored schema_version is load-bearing — reject an unsupported tag
        // hard (a single-row read fails-closed; only the list-read rides D5 skip).
        if r.schema_version != RUN_SCHEMA_VERSION {
            return Err(DataError::Db(format!(
                "unsupported backtest_run schema version {}",
                r.schema_version
            )));
        }

        // Fetch this run's trades (seq-ordered) INTERNALLY — fail-closed (D5):
        // get_trades returns Err on any corrupt trade row, never a partial log.
        let trades = self.get_trades(id).await?;

        // Parse the run-row money totals + the regime breakdown + skipped counts
        // (fail-closed on any malformed column, D5).
        let net_pnl = parse_decimal("backtest_run.net_pnl", &r.net_pnl)?;
        let fees_total = parse_decimal("backtest_run.fees_total", &r.fees_total)?;
        let funding_total = parse_decimal("backtest_run.funding_total", &r.funding_total)?;
        let slippage_total = parse_decimal("backtest_run.slippage_total", &r.slippage_total)?;
        let starting_equity = parse_decimal("backtest_run.starting_equity", &r.starting_equity)?;
        let regime_breakdown: RegimeBreakdown = match r.regime_breakdown.as_deref() {
            Some(json) => parse_json("backtest_run.regime_breakdown", json)?,
            None => RegimeBreakdown::default(),
        };
        let skipped_entries = SkippedEntryCounts {
            sub_lot: usize_from("skipped_sub_lot", r.skipped_sub_lot)?,
            sub_notional: usize_from("skipped_sub_notional", r.skipped_sub_notional)?,
            leverage_capped: usize_from("skipped_leverage_capped", r.skipped_leverage_capped)?,
        };
        // r2.s1 G1(b): the window-edge mark parses BEFORE the hash rebuild — the
        // feed covers it when present, so a tampered column fails the #39 guard
        // below; a malformed one fails-closed on the parse (D5).
        let open_position: Option<OpenPositionMark> = match r.open_position.as_deref() {
            Some(json) => Some(parse_json("backtest_run.open_position", json)?),
            None => None,
        };

        rederive_run_hash(
            &r.id,
            &r.result_content_hash,
            &trades,
            net_pnl,
            fees_total,
            funding_total,
            slippage_total,
            &regime_breakdown,
            skipped_entries,
            open_position.clone(),
        )?;

        // Reconstruct the typed SummaryStats projection from its columns.
        let summary = SummaryStats {
            trade_count: usize_from("trade_count", r.trade_count)?,
            win_count: usize_from("wins", r.wins)?,
            loss_count: usize_from("losses", r.losses)?,
            win_rate: parse_decimal(
                "backtest_run.win_rate",
                r.win_rate.as_deref().unwrap_or("0"),
            )?,
            gross_profit: parse_decimal(
                "backtest_run.gross_profit",
                r.gross_profit.as_deref().unwrap_or("0"),
            )?,
            gross_loss: parse_decimal(
                "backtest_run.gross_loss",
                r.gross_loss.as_deref().unwrap_or("0"),
            )?,
            net_pnl,
            profit_factor: parse_opt_decimal(
                "backtest_run.profit_factor",
                r.profit_factor.as_deref(),
            )?,
            avg_win: parse_decimal("backtest_run.avg_win", r.avg_win.as_deref().unwrap_or("0"))?,
            avg_loss: parse_decimal(
                "backtest_run.avg_loss",
                r.avg_loss.as_deref().unwrap_or("0"),
            )?,
            expectancy: parse_decimal(
                "backtest_run.expectancy",
                r.expectancy.as_deref().unwrap_or("0"),
            )?,
            max_drawdown: parse_decimal(
                "backtest_run.max_drawdown",
                r.max_drawdown.as_deref().unwrap_or("0"),
            )?,
            max_win_streak: usize_from("max_win_streak", r.max_win_streak)?,
            max_loss_streak: usize_from("max_loss_streak", r.max_loss_streak)?,
            commission_total: fees_total,
            funding_total,
            sharpe: parse_opt_f64("backtest_run.sharpe", r.sharpe.as_deref())?,
            sortino: parse_opt_f64("backtest_run.sortino", r.sortino.as_deref())?,
        };

        // r1.s3.w2 (#110): rehydrate the input provenance under the four-shape rule
        // (all-NULL legacy, complete-without-HTF, complete-with-HTF, anything else
        // is an error). Decoded AFTER the tamper guard so a corrupt row fails on the
        // hash first, which is the more specific diagnosis.
        let inputs = decode_inputs(
            &r.id,
            r.pair.as_deref(),
            r.primary_timeframe.as_deref(),
            r.primary_data_version.as_deref(),
            r.htf_timeframe.as_deref(),
            r.htf_data_version.as_deref(),
            r.taker_fee_bps.as_deref(),
            r.slippage_bps.as_deref(),
            r.funding_config.as_deref(),
            r.window_from_ms,
            r.window_to_ms,
            r.window_lead_in_from_ms,
        )?;
        // r2.s3.w3: the run's walk-forward membership, same defended read —
        // `0013`'s pair trigger's shape is checked again here.
        let walk_forward =
            decode_walk_forward_membership(&r.id, r.walk_forward_run_id, r.fold_index)?;

        Ok(Some(PersistedRun {
            id: BacktestRunId::new(r.id),
            strategy_version_id: VersionId::new(r.strategy_version_id),
            inputs,
            schema_version: r.schema_version,
            created_at: r.created_at,
            engine_fingerprint: r.engine_fingerprint,
            engine_target: r.engine_target,
            result_content_hash: r.result_content_hash,
            starting_equity,
            net_pnl,
            fees_total,
            funding_total,
            slippage_total,
            summary,
            // r1.s2.w3: surface the two values this function ALREADY decoded above
            // to re-derive `result_content_hash`. A pass-through of persisted
            // columns — no new query, no change to the hash input or its feed
            // order — so the coach's bounded context can read them instead of
            // recomputing anything (ADR-0021 decision 8).
            regime_breakdown,
            skipped_entries,
            open_position,
            walk_forward,
        }))
    }

    async fn latest_run_for_version(
        &self,
        strategy_version_id: &VersionId,
    ) -> Result<Option<PersistedRun>, DataError> {
        let sid = strategy_version_id.as_str();
        // #40 stable ordering: most-recent first, deterministic id tie-break.
        //
        // FOLD RUNS ARE NOT THE VERSION'S LATEST RUN (N1). A walk-forward's K
        // folds are `backtest_run` rows of this version that share ONE
        // `created_at` (the save's single injected-clock instant), so the
        // `id DESC` tie-break would hand an arbitrary fold — a UUIDv4, not a
        // clock — the title of "the version's latest run", and with it the
        // Library KPIs, the parent expectancy delta, and the pins a later run
        // inherits by default. A fold is a measurement of one sub-window, never
        // the version's latest full-span result, so rows carrying
        // `walk_forward_run_id` are excluded here. `list_runs_for_version` still
        // lists them: they ARE runs, and the Lab's fold table reads them as such.
        let row = sqlx::query!(
            r#"SELECT id AS "id!: String"
               FROM backtest_run
               WHERE strategy_version_id = ?1
                 AND walk_forward_run_id IS NULL
               ORDER BY created_at DESC, id DESC
               LIMIT 1"#,
            sid,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        // Reuse get_run's fetch-trades-and-validate path (fail-closed + tamper-check).
        match row {
            None => Ok(None),
            Some(r) => self.get_run(&BacktestRunId::new(r.id)).await,
        }
    }

    async fn list_runs_for_version(
        &self,
        strategy_version_id: &VersionId,
    ) -> Result<Vec<RunSummary>, DataError> {
        let sid = strategy_version_id.as_str();
        // #40 stable ordering for the catalog.
        let rows = sqlx::query!(
            r#"SELECT
                 id                  AS "id!: String",
                 strategy_version_id AS "strategy_version_id!: String",
                 schema_version      AS "schema_version!: i64",
                 created_at          AS "created_at!: String",
                 engine_fingerprint  AS "engine_fingerprint!: String",
                 engine_target       AS "engine_target!: String",
                 result_content_hash AS "result_content_hash!: String",
                 net_pnl             AS "net_pnl!: String",
                 expectancy          AS "expectancy?: String",
                 trade_count         AS "trade_count?: i64"
               FROM backtest_run
               WHERE strategy_version_id = ?1
               ORDER BY created_at, id"#,
            sid,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        // #39 per-row corrupt-isolation — the ONLY method that skips-with-warning
        // (D5): a corrupt summary row is dropped + `tracing::warn`ed, not a
        // whole-list failure (a run catalog is best-effort UX).
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let list_row = ListRunRow {
                id: r.id,
                strategy_version_id: r.strategy_version_id,
                schema_version: r.schema_version,
                created_at: r.created_at,
                engine_fingerprint: r.engine_fingerprint,
                engine_target: r.engine_target,
                result_content_hash: r.result_content_hash,
                net_pnl: r.net_pnl,
                expectancy: r.expectancy,
                trade_count: r.trade_count,
            };
            match row_to_run_summary(&list_row) {
                Ok(summary) => out.push(summary),
                Err(e) => {
                    // Skip-with-warning corrupt-isolation (D5): a corrupt summary row
                    // is dropped + warned, never a whole-list failure (best-effort
                    // catalog UX). The codebase has no `tracing`/`log` dependency (the
                    // spec names `tracing::warn` aspirationally); `eprintln!` is the
                    // established CLI-surface warning channel here (`main.rs:13`,
                    // `cli/mod.rs:267`) and avoids the #41 MSRV/dependency risk. See
                    // report §5.
                    eprintln!(
                        "warning: skipping corrupt backtest_run summary row `{}` in \
                         list_runs_for_version (#39 best-effort catalog): {e}",
                        list_row.id
                    );
                }
            }
        }
        Ok(out)
    }

    async fn get_trades(&self, id: &BacktestRunId) -> Result<Vec<Trade>, DataError> {
        let id_str = id.as_str();
        // Assert the run's schema_version is supported (D1b) — fail-closed even when
        // called directly (e.g. get_run delegates here). A missing run → empty log.
        let schema = sqlx::query!(
            r#"SELECT schema_version AS "schema_version!: i64" FROM backtest_run WHERE id = ?1"#,
            id_str,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;
        if let Some(s) = schema
            && s.schema_version != RUN_SCHEMA_VERSION
        {
            return Err(DataError::Db(format!(
                "unsupported backtest_run schema version {}",
                s.schema_version
            )));
        }

        let rows = sqlx::query!(
            r#"SELECT
                 direction         AS "direction?: String",
                 qty               AS "qty?: String",
                 entry_price       AS "entry_price?: String",
                 exit_price        AS "exit_price?: String",
                 entry_signal_time AS "entry_signal_time?: i64",
                 entry_fill_time   AS "entry_fill_time?: i64",
                 exit_signal_time  AS "exit_signal_time?: i64",
                 exit_fill_time    AS "exit_fill_time?: i64",
                 fees_total        AS "fees_total?: String",
                 funding_total     AS "funding_total?: String",
                 slippage_total    AS "slippage_total?: String",
                 realized_pnl      AS "realized_pnl?: String",
                 realized_r        AS "realized_r?: String",
                 mfe_r             AS "mfe_r?: String",
                 mae_r             AS "mae_r?: String",
                 exit_reason       AS "exit_reason?: String",
                 source            AS "source?: String",
                 regime            AS "regime?: String",
                 fills             AS "fills?: String",
                 stop_price        AS "stop_price?: String"
               FROM trade WHERE backtest_run_id = ?1 ORDER BY seq"#,
            id_str,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        // Fail-closed (D5 / audit C9): a trade log feeds P&L/equity/hash
        // reconstruction, so ANY corrupt/un-parseable trade row is an Err — never a
        // silently-dropped trade (that would corrupt the equity curve + re-derived
        // hash). NO per-row skip here (corrupt-isolation is list_runs_for_version's
        // alone).
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(Trade {
                direction: parse_direction(&require_col("trade.direction", r.direction)?)?,
                qty: parse_decimal("trade.qty", &require_col("trade.qty", r.qty)?)?,
                entry_price: parse_decimal(
                    "trade.entry_price",
                    &require_col("trade.entry_price", r.entry_price)?,
                )?,
                exit_price: parse_decimal(
                    "trade.exit_price",
                    &require_col("trade.exit_price", r.exit_price)?,
                )?,
                entry_signal_time: require_col("trade.entry_signal_time", r.entry_signal_time)?,
                entry_fill_time: require_col("trade.entry_fill_time", r.entry_fill_time)?,
                exit_signal_time: require_col("trade.exit_signal_time", r.exit_signal_time)?,
                exit_fill_time: require_col("trade.exit_fill_time", r.exit_fill_time)?,
                fills: parse_json::<Vec<Fill>>(
                    "trade.fills",
                    &require_col("trade.fills", r.fills)?,
                )?,
                fees_total: parse_decimal(
                    "trade.fees_total",
                    &require_col("trade.fees_total", r.fees_total)?,
                )?,
                funding_total: parse_decimal(
                    "trade.funding_total",
                    &require_col("trade.funding_total", r.funding_total)?,
                )?,
                slippage_total: parse_decimal(
                    "trade.slippage_total",
                    &require_col("trade.slippage_total", r.slippage_total)?,
                )?,
                realized_pnl: parse_decimal(
                    "trade.realized_pnl",
                    &require_col("trade.realized_pnl", r.realized_pnl)?,
                )?,
                realized_r: parse_decimal(
                    "trade.realized_r",
                    &require_col("trade.realized_r", r.realized_r)?,
                )?,
                mfe_r: parse_decimal("trade.mfe_r", &require_col("trade.mfe_r", r.mfe_r)?)?,
                mae_r: parse_decimal("trade.mae_r", &require_col("trade.mae_r", r.mae_r)?)?,
                exit_reason: parse_exit_reason(&require_col("trade.exit_reason", r.exit_reason)?)?,
                source: parse_trade_source(&require_col("trade.source", r.source)?)?,
                regime: parse_regime(&require_col("trade.regime", r.regime)?)?,
                // Migration 0011: NULL (every pre-0011 row) reads back as `None`;
                // a stored value parses fail-closed like every other column.
                stop_price: r
                    .stop_price
                    .map(|s| parse_decimal("trade.stop_price", &s))
                    .transpose()?,
            });
        }
        Ok(out)
    }
}

/// Build a [`RunSummary`] from a catalog row (the `list_runs_for_version`
/// projection). Returns `Err` on a corrupt row OR an unsupported `schema_version`
/// — the caller maps that to a skip-with-warning (D5).
fn row_to_run_summary(r: &ListRunRow) -> Result<RunSummary, DataError> {
    if r.schema_version != RUN_SCHEMA_VERSION {
        return Err(DataError::Db(format!(
            "unsupported backtest_run schema version {}",
            r.schema_version
        )));
    }
    Ok(RunSummary {
        id: BacktestRunId::new(r.id.clone()),
        strategy_version_id: VersionId::new(r.strategy_version_id.clone()),
        schema_version: r.schema_version,
        created_at: r.created_at.clone(),
        engine_fingerprint: r.engine_fingerprint.clone(),
        engine_target: r.engine_target.clone(),
        result_content_hash: r.result_content_hash.clone(),
        net_pnl: parse_decimal("backtest_run.net_pnl", &r.net_pnl)?,
        expectancy: parse_decimal(
            "backtest_run.expectancy",
            r.expectancy.as_deref().unwrap_or("0"),
        )?,
        trade_count: usize_from("trade_count", r.trade_count)?,
    })
}

/// The catalog-row column values consumed by [`row_to_run_summary`] (a struct, not
/// a wide arg list — so the skip-with-warning loop hands one owned reference).
struct ListRunRow {
    id: String,
    strategy_version_id: String,
    schema_version: i64,
    created_at: String,
    engine_fingerprint: String,
    engine_target: String,
    result_content_hash: String,
    net_pnl: String,
    expectancy: Option<String>,
    trade_count: Option<i64>,
}

/// Convert an `Option<i64>` count column to `usize`, fail-closed on NULL or a
/// negative value (a count column should never be NULL/negative — a corrupt row).
/// Rehydrate [`BacktestInputs`] from the eight migration-`0006` columns, fail-closed
/// (r1.s3.w2, #110).
///
/// Exactly four shapes are legal and every other combination is an error:
///
/// 1. **all eight NULL** → `Ok(None)`. A row written before `0006`. It cannot be
///    backfilled truthfully (nothing stored recovers the snapshot identity) and
///    ADR-0018 forbids inventing facts on immutable records, so it reads as an
///    explicit "provenance unavailable".
/// 2. **six required present, both HTF NULL** → `Some`, `htf: None` — a genuine
///    single-timeframe run.
/// 3. **six required present, both HTF present** → `Some`, `htf: Some`.
/// 4. **anything else** → [`DataError::Db`].
///
/// Shape 4 is the point. A half-populated row is not a run with some provenance; it
/// is a row whose provenance cannot be trusted, and a partial projection would let
/// a caller replay against the wrong snapshot while believing it had the right one.
/// The same applies to an unknown timeframe or funding discriminant, an invalid
/// pair, a `data_version` that is not a safe single path component, or a Decimal
/// that will not parse — all refuse rather than degrade. The version check is not
/// cosmetic: W3 hands a decoded tag to `CandleStore::load_version`, which joins it
/// into the snapshot path, so a stored `../../../x` that decoded cleanly would
/// escape the store root.
#[allow(clippy::too_many_arguments)]
fn decode_inputs(
    run_id: &str,
    pair: Option<&str>,
    primary_timeframe: Option<&str>,
    primary_data_version: Option<&str>,
    htf_timeframe: Option<&str>,
    htf_data_version: Option<&str>,
    taker_fee_bps: Option<&str>,
    slippage_bps: Option<&str>,
    funding_config: Option<&str>,
    window_from_ms: Option<i64>,
    window_to_ms: Option<i64>,
    window_lead_in_from_ms: Option<i64>,
) -> Result<Option<BacktestInputs>, DataError> {
    let required = [
        pair,
        primary_timeframe,
        primary_data_version,
        taker_fee_bps,
        slippage_bps,
        funding_config,
    ];
    let present = required.iter().filter(|v| v.is_some()).count();
    let htf_present = [htf_timeframe, htf_data_version]
        .iter()
        .filter(|v| v.is_some())
        .count();

    // Shape 1: the legacy row. ALL eight must be NULL — a row with no required
    // provenance but a stray HTF value is corrupt, not legacy. The `0009` window
    // pair is NULL there too, and is decoded with it rather than counted among
    // the `0006` shapes (a legacy row has no inputs at all).
    if present == 0 && htf_present == 0 {
        if window_from_ms.is_some() || window_to_ms.is_some() || window_lead_in_from_ms.is_some() {
            return Err(DataError::Db(format!(
                "run `{run_id}` carries window bounds but no input provenance: \
                 a row that cannot name its snapshot cannot name a slice of it"
            )));
        }
        return Ok(None);
    }
    if present != required.len() {
        return Err(DataError::Db(format!(
            "run `{run_id}` has partial input provenance ({present}/{} required columns present): \
             a partially-populated row cannot be trusted to name the snapshot it ran against \
             (#110)",
            required.len()
        )));
    }
    if htf_present == 1 {
        return Err(DataError::Db(format!(
            "run `{run_id}` has a half-present HTF selection: htf_timeframe and htf_data_version \
             must both be present or both absent (#110)"
        )));
    }
    let window =
        decode_window_and_lead_in(run_id, window_from_ms, window_to_ms, window_lead_in_from_ms)?;

    let pair = require_col("backtest_run.pair", pair)?;
    let primary_timeframe = require_col("backtest_run.primary_timeframe", primary_timeframe)?;
    let primary_data_version =
        require_col("backtest_run.primary_data_version", primary_data_version)?;
    let taker_fee_bps = require_col("backtest_run.taker_fee_bps", taker_fee_bps)?;
    let slippage_bps = require_col("backtest_run.slippage_bps", slippage_bps)?;
    let funding_config = require_col("backtest_run.funding_config", funding_config)?;

    let htf = match (htf_timeframe, htf_data_version) {
        (Some(tf), Some(version)) => Some(SnapshotSelection {
            timeframe: parse_timeframe("backtest_run.htf_timeframe", tf)?,
            // `parse`, not `new`: a stored tag is untrusted on the way back out.
            data_version: DataVersion::parse(version)?,
        }),
        _ => None,
    };

    Ok(Some(BacktestInputs {
        // `Pair::parse` (not `Pair::new`): the stored symbol is joined verbatim into
        // candle-store paths on replay, so a corrupt value must refuse here.
        pair: Pair::parse(pair.to_owned())?,
        primary: SnapshotSelection {
            timeframe: parse_timeframe("backtest_run.primary_timeframe", primary_timeframe)?,
            data_version: DataVersion::parse(primary_data_version)?,
        },
        htf,
        taker_fee_bps: parse_decimal("backtest_run.taker_fee_bps", taker_fee_bps)?,
        slippage_bps: parse_decimal("backtest_run.slippage_bps", slippage_bps)?,
        funding: parse_funding("backtest_run.funding_config", funding_config)?,
        window,
        lead_in_from_ms: window_lead_in_from_ms,
    }))
}

/// The window/lead-in decode and its two refusals, extracted from
/// `decode_inputs` (#204 — r2.s3.w3 keeps that function under the 80-line cap).
///
/// r2.s1.w1: the window is both bounds or neither, and `from < to` — the same
/// shape `0009`'s pair trigger refuses on INSERT, checked again on the way out
/// (a defended read does not trust the trigger to have seen the write).
/// r2.s3.w2: the lead-in start is only meaningful relative to a counted window
/// — the same shape `0012`'s pair trigger refuses on INSERT.
fn decode_window_and_lead_in(
    run_id: &str,
    window_from_ms: Option<i64>,
    window_to_ms: Option<i64>,
    window_lead_in_from_ms: Option<i64>,
) -> Result<Option<CandleWindow>, DataError> {
    let window = match (window_from_ms, window_to_ms) {
        (None, None) => None,
        (Some(from_ms), Some(to_ms)) => {
            Some(CandleWindow::new(from_ms, to_ms).map_err(|e| {
                DataError::Db(format!("run `{run_id}` stores an invalid window: {e}"))
            })?)
        }
        _ => {
            return Err(DataError::Db(format!(
                "run `{run_id}` has a half-present window: window_from_ms and window_to_ms \
                 must both be present or both absent (r2.s1.w1)"
            )));
        }
    };
    if window.is_none() && window_lead_in_from_ms.is_some() {
        return Err(DataError::Db(format!(
            "run `{run_id}` carries a lead-in start but no window pair: \
             window_lead_in_from_ms requires a complete window (r2.s3.w2)"
        )));
    }
    Ok(window)
}

/// The walk-forward membership decode (r2.s3.w3): `walk_forward_run_id` and
/// `fold_index` are set together or not at all — the same shape `0013`'s pair
/// trigger refuses on INSERT, checked again on the way out.
fn decode_walk_forward_membership(
    run_id: &str,
    walk_forward_run_id: Option<String>,
    fold_index: Option<i64>,
) -> Result<Option<WalkForwardMembership>, DataError> {
    match (walk_forward_run_id, fold_index) {
        (None, None) => Ok(None),
        (Some(wf_id), Some(idx)) => {
            let fold_index = u8::try_from(idx).map_err(|e| {
                DataError::Db(format!(
                    "run `{run_id}` carries an out-of-range fold_index {idx}: {e}"
                ))
            })?;
            Ok(Some(WalkForwardMembership {
                run_id: WalkForwardRunId::new(wf_id),
                fold_index,
            }))
        }
        _ => Err(DataError::Db(format!(
            "run `{run_id}` has a half-present walk-forward membership: \
             walk_forward_run_id and fold_index must both be present or both absent (r2.s3.w3)"
        ))),
    }
}

/// The #39 re-validate-on-read half of `get_run` (D4), extracted so `get_run`
/// stays inside the #204 line cap with the `0013` membership columns added
/// (r2.s3.w3): reconstruct the FULL hash input — run totals +
/// `regime_breakdown` + `skipped_entries` + the seq-ordered trades — by
/// rebuilding a [`BacktestResult`] and re-deriving `result_content_hash` (which
/// feeds `feed_money_math` + `feed_regime_breakdown` in the frozen order). The
/// `summary`/`equity_curve`/`engine_fingerprint` are oracle-EXCLUDED, so
/// defaults here cannot perturb the hash (proven by AC-6). Reject a mismatch.
#[allow(clippy::too_many_arguments)]
fn rederive_run_hash(
    run_id: &str,
    stored_hash: &str,
    trades: &[Trade],
    net_pnl: Decimal,
    fees_total: Decimal,
    funding_total: Decimal,
    slippage_total: Decimal,
    regime_breakdown: &RegimeBreakdown,
    skipped_entries: SkippedEntryCounts,
    open_position: Option<OpenPositionMark>,
) -> Result<(), DataError> {
    let rebuilt = BacktestResult {
        trades: trades.to_vec(),
        net_pnl,
        fees_total,
        funding_total,
        slippage_total,
        regime_breakdown: *regime_breakdown,
        skipped_entries,
        open_position,
        engine_fingerprint: EngineFingerprint::default(),
        summary: SummaryStats::default(),
        equity_curve: EquityCurve::default(),
    };
    let derived = rebuilt.result_content_hash();
    if derived != stored_hash {
        return Err(DataError::Db(format!(
            "result_content_hash mismatch for run `{run_id}`: stored {stored_hash}, derived {derived} \
             (#39 re-validate-on-read tamper guard)"
        )));
    }
    Ok(())
}

fn usize_from(column: &str, value: Option<i64>) -> Result<usize, DataError> {
    let v = value
        .ok_or_else(|| DataError::Db(format!("NULL in count column `{column}` (corrupt row)")))?;
    usize::try_from(v)
        .map_err(|e| DataError::Db(format!("negative/oversized count in `{column}` = {v}: {e}")))
}

/// Require a non-NULL column value, fail-closed on NULL (D5 — a trade column that
/// must be present but is NULL is a corrupt row, never silently defaulted).
fn require_col<T>(column: &str, value: Option<T>) -> Result<T, DataError> {
    value.ok_or_else(|| DataError::Db(format!("NULL in required column `{column}` (corrupt row)")))
}

// ---------------------------------------------------------------------------
// r1.s4.w4 — the crate-private insert mappings, extracted so the coach's accept
// transaction can REUSE them.
//
// `commit_acceptance` has to write a `backtest_run` and its `trade` rows inside the
// same transaction that mints the child version. Copying this mapping there would
// have created a SECOND place that knows how a run becomes columns — and the two
// would drift the first time a column is added, in the direction that matters most
// (money math persisted two ways). These are `pub(crate)`: the mapping stays owned
// by this file, and the accept path borrows it.
// ---------------------------------------------------------------------------

/// The two `DataVersion` tags a run names, checked for path safety BEFORE any
/// transaction opens.
///
/// `DataVersion` is opaque by design but not arbitrary: the adapter joins a tag
/// verbatim into `<base>/candles/<PAIR>/<TF>/<tag>.parquet`, and a consumer hands a
/// decoded tag straight to `load_version`, so `../../../x` would escape the store
/// root. Same rule, same reason, as `Pair::parse`. Called before `begin()` so an
/// unsafe tag persists nothing at all rather than aborting a partly-built write.
///
/// # Errors
///
/// Returns the `DataError` `ensure_path_safe` raises for an unsafe tag.
pub(crate) fn check_inputs_path_safe(inputs: &BacktestInputs) -> Result<(), DataError> {
    inputs.primary.data_version.ensure_path_safe()?;
    if let Some(htf) = inputs.htf.as_ref() {
        htf.data_version.ensure_path_safe()?;
    }
    Ok(())
}

/// Insert one `backtest_run` row on the caller's transaction.
///
/// The caller owns the id, the timestamp and the ownership guard; this owns the
/// COLUMN MAPPING and nothing else. D2 (`Decimal`-as-TEXT via `.normalize()`), D2b
/// (finite-or-NULL sharpe/sortino) and `0006`'s eight input-provenance columns all
/// live here, once.
///
/// # Errors
///
/// Returns [`DataError::Db`] on an overflowing count, a non-finite f64 statistic, a
/// serialization failure, or a store/constraint rejection.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(crate) async fn insert_run_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    run_id: &str,
    version_id_str: &str,
    created_at: &str,
    inputs: &BacktestInputs,
    result: &BacktestResult,
    summary: &SummaryStats,
    starting_equity: Decimal,
    membership: Option<&WalkForwardMembership>,
) -> Result<(), DataError> {
    // Scalar / canonicalized column values (D2 — Decimal-as-TEXT via
    // `.normalize().to_string()`; D2b — sharpe/sortino finite-or-NULL).
    let schema_version = RUN_SCHEMA_VERSION;
    let engine_fingerprint = result.engine_fingerprint.as_str().to_owned();
    let engine_target = EngineFingerprint::target().to_owned();
    let result_content_hash = result.result_content_hash();
    let starting_equity_text = decimal_text(starting_equity);
    let net_pnl_text = decimal_text(result.net_pnl);
    let fees_total_text = decimal_text(result.fees_total);
    let funding_total_text = decimal_text(result.funding_total);
    let slippage_total_text = decimal_text(result.slippage_total);

    let expectancy_text = decimal_text(summary.expectancy);
    let win_rate_text = decimal_text(summary.win_rate);
    let profit_factor_text = summary.profit_factor.map(decimal_text);
    let gross_profit_text = decimal_text(summary.gross_profit);
    let gross_loss_text = decimal_text(summary.gross_loss);
    let avg_win_text = decimal_text(summary.avg_win);
    let avg_loss_text = decimal_text(summary.avg_loss);
    let max_drawdown_text = decimal_text(summary.max_drawdown);
    let trade_count = i64::try_from(summary.trade_count).map_err(|e| {
        DataError::Db(format!(
            "trade_count {} overflows i64: {e}",
            summary.trade_count
        ))
    })?;
    let wins = i64::try_from(summary.win_count)
        .map_err(|e| DataError::Db(format!("win_count overflows i64: {e}")))?;
    let losses = i64::try_from(summary.loss_count)
        .map_err(|e| DataError::Db(format!("loss_count overflows i64: {e}")))?;
    // `breakeven = trade_count - wins - losses` (README C1: wins+losses+breakeven
    // == trade_count). SummaryStats carries no explicit breakeven field.
    let breakeven = trade_count - wins - losses;
    let max_win_streak = i64::try_from(summary.max_win_streak)
        .map_err(|e| DataError::Db(format!("max_win_streak overflows i64: {e}")))?;
    let max_loss_streak = i64::try_from(summary.max_loss_streak)
        .map_err(|e| DataError::Db(format!("max_loss_streak overflows i64: {e}")))?;
    // sharpe/sortino: f64 to_string() finite, or NULL; NaN/Inf fail-closed (D2b).
    let sharpe_text = f64_stat_text("sharpe", summary.sharpe)?;
    let sortino_text = f64_stat_text("sortino", summary.sortino)?;
    // The regime breakdown rides an inline JSON column (round-trips exactly into
    // the hash feed on read — D4b proves this).
    let regime_breakdown_json = serde_json::to_string(&result.regime_breakdown)
        .map_err(|e| DataError::Db(e.to_string()))?;
    let (skipped_sub_lot, skipped_sub_notional, skipped_leverage_capped) =
        skipped_entry_i64s(result)?;

    // r1.s3.w2 (#110) — the eight INPUT provenance columns. Timeframes and the
    // funding discriminant ride their serde tokens (`15m`/`4h`,
    // `snapshot_rates`), matching the `direction`/`regime` precedent; the two
    // bps values ride the same `.normalize()`d Decimal-as-TEXT every other money
    // column uses (NFR-2). The HTF pair is written all-or-nothing — the domain
    // cannot express half a selection, and `0006`'s trigger refuses one.
    //
    // The two version tags are checked BEFORE the transaction opens, so an
    // unsafe one persists nothing at all rather than aborting a partly-built
    // write. `DataVersion` is opaque by design but not arbitrary: the adapter
    // joins a tag verbatim into `<base>/candles/<PAIR>/<TF>/<tag>.parquet`, and
    // W3 will hand a decoded tag straight to `load_version`, so `../../../x`
    // would escape the store root. Same rule, same reason, as `Pair::parse`.
    let pair_text = inputs.pair.as_str().to_owned();
    let primary_timeframe = enum_token(&inputs.primary.timeframe)?;
    let primary_data_version = inputs.primary.data_version.as_str().to_owned();
    let htf_timeframe = inputs
        .htf
        .as_ref()
        .map(|htf| enum_token(&htf.timeframe))
        .transpose()?;
    let htf_data_version = inputs
        .htf
        .as_ref()
        .map(|htf| htf.data_version.as_str().to_owned());
    let taker_fee_bps_text = decimal_text(inputs.taker_fee_bps);
    let slippage_bps_text = decimal_text(inputs.slippage_bps);
    let funding_config = enum_token(&inputs.funding)?;
    // r2.s1.w1 — the window pair is written all-or-nothing: the domain type is
    // `Option<CandleWindow>`, so "half a window" is unrepresentable here, and the
    // `0009` trigger refuses it anyway. Both bounds are UTC epoch-ms INTEGERs.
    let window_from_ms = inputs.window.as_ref().map(|w| w.from_ms);
    let window_to_ms = inputs.window.as_ref().map(|w| w.to_ms);
    // r2.s3.w2: the lead-in start rides the same provenance row — `0012`'s
    // trigger refuses it without a complete window pair, which the
    // application layer already guarantees (lead-in is set only with `window`).
    let window_lead_in_from_ms = inputs.lead_in_from_ms;
    // r2.s3.w3: walk-forward membership rides the same provenance row —
    // `0013`'s pair trigger refuses a half-set pair, which the application
    // layer already guarantees (a fold's run is written only inside
    // `save_walk_forward_run`).
    let walk_forward_run_id = membership.map(|m| m.run_id.as_str().to_owned());
    let fold_index = membership.map(|m| i64::from(m.fold_index));
    // r2.s1 G1(b): the window-edge open-position mark is one JSON column (the
    // `regime_breakdown`/`fills` precedent) — NULL for a run that ended flat
    // or at the snapshot's real last bar. Never a trade row, so the
    // closed-trade statistics stay closed-trade statistics.
    let open_position_json = result
        .open_position
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| DataError::Db(e.to_string()))?;
    sqlx::query!(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
          engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
          funding_total, slippage_total, expectancy, win_rate, profit_factor, \
          gross_profit, gross_loss, avg_win, avg_loss, max_drawdown, trade_count, \
          wins, losses, breakeven, max_win_streak, max_loss_streak, sharpe, sortino, \
          regime_breakdown, skipped_sub_lot, skipped_sub_notional, skipped_leverage_capped, \
          pair, primary_timeframe, primary_data_version, htf_timeframe, htf_data_version, \
          taker_fee_bps, slippage_bps, funding_config, window_from_ms, window_to_ms, \
          window_lead_in_from_ms, open_position, walk_forward_run_id, fold_index) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                 ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, \
                 ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44, ?45, ?46)",
        run_id,
        version_id_str,
        schema_version,
        created_at,
        engine_fingerprint,
        engine_target,
        result_content_hash,
        starting_equity_text,
        net_pnl_text,
        fees_total_text,
        funding_total_text,
        slippage_total_text,
        expectancy_text,
        win_rate_text,
        profit_factor_text,
        gross_profit_text,
        gross_loss_text,
        avg_win_text,
        avg_loss_text,
        max_drawdown_text,
        trade_count,
        wins,
        losses,
        breakeven,
        max_win_streak,
        max_loss_streak,
        sharpe_text,
        sortino_text,
        regime_breakdown_json,
        skipped_sub_lot,
        skipped_sub_notional,
        skipped_leverage_capped,
        pair_text,
        primary_timeframe,
        primary_data_version,
        htf_timeframe,
        htf_data_version,
        taker_fee_bps_text,
        slippage_bps_text,
        funding_config,
        window_from_ms,
        window_to_ms,
        window_lead_in_from_ms,
        open_position_json,
        walk_forward_run_id,
        fold_index,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| DataError::Db(e.to_string()))?;
    Ok(())
}

/// The three `skipped_entries` count columns as `i64` (extracted from
/// `insert_run_row` for #204 — the `0013` membership columns push it past its
/// r2.s3.w2 count otherwise; r2.s3.w3).
fn skipped_entry_i64s(result: &BacktestResult) -> Result<(i64, i64, i64), DataError> {
    let sub_lot = i64::try_from(result.skipped_entries.sub_lot)
        .map_err(|e| DataError::Db(format!("skipped_sub_lot overflows i64: {e}")))?;
    let sub_notional = i64::try_from(result.skipped_entries.sub_notional)
        .map_err(|e| DataError::Db(format!("skipped_sub_notional overflows i64: {e}")))?;
    let leverage_capped = i64::try_from(result.skipped_entries.leverage_capped)
        .map_err(|e| DataError::Db(format!("skipped_leverage_capped overflows i64: {e}")))?;
    Ok((sub_lot, sub_notional, leverage_capped))
}

/// Insert every `trade` row for `run_id`, in `seq` order (0-based chronological),
/// on the caller's transaction.
///
/// # Errors
///
/// Returns [`DataError::Db`] on an overflowing sequence number, a serialization
/// failure, or a store/constraint rejection.
// `mfe_r` / `mae_r` are domain field names and are similar by construction — the
// same allow `save_run` carried before this mapping moved out of it.
#[allow(clippy::similar_names)]
pub(crate) async fn insert_trade_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    run_id: &str,
    trades: &[Trade],
) -> Result<(), DataError> {
    // INSERT every trade in `seq` order (0-based chronological).
    for (seq, trade) in trades.iter().enumerate() {
        let trade_id = Uuid::new_v4().to_string();
        let seq_i64 = i64::try_from(seq)
            .map_err(|e| DataError::Db(format!("trade seq {seq} overflows i64: {e}")))?;
        let direction = enum_token(&trade.direction)?;
        let qty = decimal_text(trade.qty);
        let entry_price = decimal_text(trade.entry_price);
        let exit_price = decimal_text(trade.exit_price);
        let t_fees_total = decimal_text(trade.fees_total);
        let t_funding_total = decimal_text(trade.funding_total);
        let t_slippage_total = decimal_text(trade.slippage_total);
        let realized_pnl = decimal_text(trade.realized_pnl);
        let realized_r = decimal_text(trade.realized_r);
        let mfe_r = decimal_text(trade.mfe_r);
        let mae_r = decimal_text(trade.mae_r);
        let exit_reason = enum_token(&trade.exit_reason)?;
        let source = enum_token(&trade.source)?;
        let regime = enum_token(&trade.regime)?;
        let fills_json =
            serde_json::to_string(&trade.fills).map_err(|e| DataError::Db(e.to_string()))?;
        let entry_signal_time = trade.entry_signal_time;
        let entry_fill_time = trade.entry_fill_time;
        let exit_signal_time = trade.exit_signal_time;
        let exit_fill_time = trade.exit_fill_time;
        // r2.s2.w2 / migration 0011: the recorded per-trade stop. `None` binds
        // NULL — the only value a pre-0011-shaped row can carry.
        let stop_price = trade.stop_price.map(decimal_text);

        sqlx::query!(
            "INSERT INTO trade \
             (id, backtest_run_id, seq, direction, qty, entry_price, exit_price, \
              entry_signal_time, entry_fill_time, exit_signal_time, exit_fill_time, \
              fees_total, funding_total, slippage_total, realized_pnl, realized_r, \
              mfe_r, mae_r, exit_reason, source, regime, fills, stop_price) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
                     ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
            trade_id,
            run_id,
            seq_i64,
            direction,
            qty,
            entry_price,
            exit_price,
            entry_signal_time,
            entry_fill_time,
            exit_signal_time,
            exit_fill_time,
            t_fees_total,
            t_funding_total,
            t_slippage_total,
            realized_pnl,
            realized_r,
            mfe_r,
            mae_r,
            exit_reason,
            source,
            regime,
            fills_json,
            stop_price,
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{RUN_SCHEMA_VERSION, SqliteBacktestRunRepo};
    use crate::adapters::clock::FakeClock;
    use crate::adapters::db::{Db, MIGRATOR};
    use crate::domain::backtest::{
        BacktestInputs, BacktestResult, BacktestRunId, EquityCurve, ExitReason, Fill,
        FundingConfig, OpenPositionMark, Regime, RegimeBreakdown, SnapshotSelection, SummaryStats,
        Trade, TradeSource,
    };
    use crate::domain::sizing::SkippedEntryCounts;
    use crate::domain::strategy::VersionId;
    use crate::domain::{
        BacktestRunRepository, DataVersion, Direction, EngineFingerprint, Pair, Timeframe,
    };
    use rust_decimal::Decimal;
    use sqlx::SqlitePool;
    use tempfile::TempDir;

    /// A `(repo, pool, tempdir)` triple over a fresh migrated tempfile DB with a
    /// deterministic [`FakeClock`] pinned at `now_ms`. Seeds ONE `strategy` + ONE
    /// `strategy_version` (`ver-1`) so `save_run`'s #39 ownership check passes. The
    /// `TempDir` guard keeps the scratch DB alive for the test body.
    async fn repo_at(now_ms: i64) -> (SqliteBacktestRunRepo<FakeClock>, SqlitePool, TempDir) {
        let tmp = TempDir::new().expect("tempdir");
        let db = Db::with_path(&tmp.path().join("pulse.db"))
            .await
            .expect("open db");
        MIGRATOR.run(db.pool()).await.expect("run migrations");
        let pool = db.pool().clone();
        seed_version(&pool, "strat-1", "ver-1").await;
        (
            SqliteBacktestRunRepo::with_deps(pool.clone(), FakeClock::at(now_ms)),
            pool,
            tmp,
        )
    }

    /// Insert a `strategy` + a `strategy_version` row (raw sqlx — these are not the
    /// columns under test) so a run can be saved against `version_id`.
    async fn seed_version(pool: &SqlitePool, strategy_id: &str, version_id: &str) {
        sqlx::query("INSERT INTO strategy (id, name, created_at) VALUES (?1, ?2, ?3)")
            .bind(strategy_id)
            .bind("Test Strategy")
            .bind("2026-06-14T00:00:00.000Z")
            .execute(pool)
            .await
            .expect("insert strategy");
        sqlx::query(
            "INSERT INTO strategy_version \
             (id, strategy_id, dsl_schema_version, dsl, dsl_original, version_hash, created_by, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(version_id)
        .bind(strategy_id)
        .bind("1.0.0")
        .bind("{}")
        .bind("{}")
        .bind("deadbeef")
        .bind("\"human\"")
        .bind("2026-06-14T00:00:00.000Z")
        .execute(pool)
        .await
        .expect("insert strategy_version");
    }

    /// The input provenance every fresh save now carries (r1.s3.w2, #110). These
    /// tests are about the run/trade projection, not about provenance, so they all
    /// use the same complete single-timeframe tuple; `tests/backtest_provenance.rs`
    /// owns the provenance shapes themselves.
    fn sample_inputs() -> BacktestInputs {
        BacktestInputs {
            pair: Pair::new("BTCUSDT"),
            primary: SnapshotSelection {
                timeframe: Timeframe::M15,
                data_version: DataVersion::new("v-primary"),
            },
            htf: None,
            taker_fee_bps: d(4, 0),
            slippage_bps: d(1, 0),
            funding: FundingConfig::SnapshotRates,
            window: None,
            lead_in_from_ms: None,
        }
    }

    fn d(value: i64, scale: u32) -> Decimal {
        Decimal::new(value, scale)
    }

    /// A simple one-trade `Trade` with a single fill.
    fn simple_trade() -> Trade {
        Trade {
            direction: Direction::Long,
            qty: d(5, 1),
            entry_price: d(30_000, 0),
            exit_price: d(33_000, 0),
            entry_signal_time: 1,
            entry_fill_time: 2,
            exit_signal_time: 3,
            exit_fill_time: 4,
            fills: vec![Fill {
                price: d(30_000, 0),
                qty: d(5, 1),
                time_ms: 2,
                fee: d(6, 0),
            }],
            fees_total: d(12, 0),
            funding_total: d(1, 0),
            slippage_total: d(3, 0),
            realized_pnl: d(1_484, 0),
            realized_r: d(2, 0),
            mfe_r: d(25, 1),
            mae_r: d(-5, 1),
            exit_reason: ExitReason::TakeProfit,
            source: TradeSource::Backtest,
            regime: Regime::TrendingUp,
            stop_price: Some(d(28_500, 0)),
        }
    }

    /// Build a `BacktestResult` from a trade log + the run totals, with the derived
    /// (oracle-excluded) `summary`/`equity_curve` attached — exactly as the engine does.
    fn result_from(
        trades: Vec<Trade>,
        regime_breakdown: RegimeBreakdown,
        skipped: SkippedEntryCounts,
    ) -> (BacktestResult, SummaryStats) {
        let net_pnl: Decimal = trades.iter().map(|t| t.realized_pnl).sum();
        let fees_total: Decimal = trades.iter().map(|t| t.fees_total).sum();
        let funding_total: Decimal = trades.iter().map(|t| t.funding_total).sum();
        let slippage_total: Decimal = trades.iter().map(|t| t.slippage_total).sum();
        let starting_equity = d(10_000, 0);
        let equity_curve = EquityCurve::from_trades(0, starting_equity, &trades);
        let summary =
            SummaryStats::from_trades(&trades, net_pnl, fees_total, funding_total, &equity_curve);
        let result = BacktestResult {
            trades,
            net_pnl,
            fees_total,
            funding_total,
            slippage_total,
            regime_breakdown,
            skipped_entries: skipped,
            open_position: None,
            engine_fingerprint: EngineFingerprint::current(),
            summary: summary.clone(),
            equity_curve,
        };
        (result, summary)
    }

    /// The DELIBERATELY NONTRIVIAL fixture (D4b, AC-13): multi-fill `fills`,
    /// trailing-zero Decimals (`0.10` vs `0.1`) that `.normalize()` collapses, ALL
    /// FOUR regime variants across the breakdown, AND a non-empty `skipped_entries`
    /// (all three counters non-zero).
    #[allow(clippy::too_many_lines)]
    fn nontrivial_result() -> (BacktestResult, SummaryStats) {
        // Trade A — multi-fill (open partial + open partial + close), trailing-zero
        // Decimals (qty 0.10, a fee 0.50), regime TrendingUp, realized_pnl negative.
        let trade_a = Trade {
            direction: Direction::Long,
            qty: Decimal::from_str_exact("0.10").unwrap(), // trailing zero → normalizes to 0.1
            entry_price: d(20_000, 0),
            exit_price: d(19_500, 0),
            entry_signal_time: 10,
            entry_fill_time: 11,
            exit_signal_time: 19,
            exit_fill_time: 20,
            fills: vec![
                Fill {
                    price: d(20_000, 0),
                    qty: Decimal::from_str_exact("0.050").unwrap(),
                    time_ms: 11,
                    fee: Decimal::from_str_exact("0.50").unwrap(),
                },
                Fill {
                    price: d(20_010, 0),
                    qty: Decimal::from_str_exact("0.05").unwrap(),
                    time_ms: 12,
                    fee: Decimal::from_str_exact("0.5").unwrap(),
                },
                Fill {
                    price: d(19_500, 0),
                    qty: Decimal::from_str_exact("0.10").unwrap(),
                    time_ms: 20,
                    fee: d(1, 0),
                },
            ],
            fees_total: d(2, 0),
            funding_total: Decimal::from_str_exact("-0.20").unwrap(),
            slippage_total: Decimal::from_str_exact("0.30").unwrap(),
            realized_pnl: d(-50, 0),
            realized_r: d(-1, 0),
            mfe_r: d(5, 1),
            mae_r: d(-12, 1),
            exit_reason: ExitReason::StopLoss,
            source: TradeSource::Backtest,
            regime: Regime::TrendingDown,
            stop_price: Some(Decimal::from_str_exact("19500.00").unwrap()),
        };
        // Trade B — single fill, regime Ranging, positive.
        let trade_b = Trade {
            direction: Direction::Short,
            qty: d(2, 0),
            entry_price: d(100, 0),
            exit_price: d(80, 0),
            entry_signal_time: 30,
            entry_fill_time: 31,
            exit_signal_time: 39,
            exit_fill_time: 40,
            fills: vec![Fill {
                price: d(100, 0),
                qty: d(2, 0),
                time_ms: 31,
                fee: Decimal::from_str_exact("0.10").unwrap(),
            }],
            fees_total: Decimal::from_str_exact("0.20").unwrap(),
            funding_total: d(0, 0),
            slippage_total: d(0, 0),
            realized_pnl: d(40, 0),
            realized_r: d(2, 0),
            mfe_r: d(3, 0),
            mae_r: d(-2, 1),
            exit_reason: ExitReason::TakeProfit,
            source: TradeSource::Backtest,
            regime: Regime::Ranging,
            stop_price: Some(d(110, 0)),
        };
        // Trade C — regime Unknown (opened while warming), break-even.
        let trade_c = Trade {
            direction: Direction::Long,
            qty: d(1, 0),
            entry_price: d(50, 0),
            exit_price: d(50, 0),
            entry_signal_time: 50,
            entry_fill_time: 51,
            exit_signal_time: 59,
            exit_fill_time: 60,
            fills: vec![Fill {
                price: d(50, 0),
                qty: d(1, 0),
                time_ms: 51,
                fee: d(0, 0),
            }],
            fees_total: d(0, 0),
            funding_total: d(0, 0),
            slippage_total: d(0, 0),
            realized_pnl: d(0, 0),
            realized_r: d(0, 0),
            mfe_r: d(0, 0),
            mae_r: d(0, 0),
            exit_reason: ExitReason::Signal,
            source: TradeSource::Backtest,
            regime: Regime::Unknown,
            stop_price: Some(d(45, 0)),
        };
        // Trade D — regime TrendingUp, positive (so all four variants appear).
        let trade_d = Trade {
            direction: Direction::Long,
            qty: d(3, 0),
            entry_price: d(10, 0),
            exit_price: d(15, 0),
            entry_signal_time: 70,
            entry_fill_time: 71,
            exit_signal_time: 79,
            exit_fill_time: 80,
            fills: vec![Fill {
                price: d(10, 0),
                qty: d(3, 0),
                time_ms: 71,
                fee: Decimal::from_str_exact("0.100").unwrap(),
            }],
            fees_total: Decimal::from_str_exact("0.10").unwrap(),
            funding_total: Decimal::from_str_exact("0.05").unwrap(),
            slippage_total: d(0, 0),
            realized_pnl: d(15, 0),
            realized_r: Decimal::from_str_exact("1.5").unwrap(),
            mfe_r: d(2, 0),
            mae_r: d(-1, 1),
            exit_reason: ExitReason::TakeProfit,
            source: TradeSource::Backtest,
            regime: Regime::TrendingUp,
            stop_price: Some(Decimal::from_str_exact("6.66666667").unwrap()),
        };
        let trades = vec![trade_a, trade_b, trade_c, trade_d];
        // All four regime cells populated.
        let mut regime_breakdown = RegimeBreakdown::new();
        for t in &trades {
            regime_breakdown.record(t.regime, t.realized_pnl);
        }
        // All three skip counters non-zero.
        let mut skipped = SkippedEntryCounts::new();
        skipped.sub_lot = 2;
        skipped.sub_notional = 1;
        skipped.leverage_capped = 3;
        result_from(trades, regime_breakdown, skipped)
    }

    // ---- AC-1: save_run round-trips the persisted run + trades ----------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_run_roundtrips_persisted_run_and_trades() {
        let (repo, _pool, _tmp) = repo_at(1_700_000_000_000).await;
        let mut breakdown = RegimeBreakdown::new();
        let trade = simple_trade();
        breakdown.record(trade.regime, trade.realized_pnl);
        let (result, summary) =
            result_from(vec![trade.clone()], breakdown, SkippedEntryCounts::new());

        let id = repo
            .save_run(
                &VersionId::new("ver-1"),
                &sample_inputs(),
                &result,
                &summary,
                d(10_000, 0),
            )
            .await
            .expect("save_run");

        // The run header reads back with the persisted fields (NFR-2: money TEXT).
        let run = repo.get_run(&id).await.expect("get_run").expect("present");
        assert_eq!(run.strategy_version_id, VersionId::new("ver-1"));
        assert_eq!(run.schema_version, RUN_SCHEMA_VERSION);
        assert_eq!(run.net_pnl, result.net_pnl);
        assert_eq!(run.starting_equity, d(10_000, 0));
        assert!(!run.engine_fingerprint.is_empty());
        assert!(!run.engine_target.is_empty());
        assert_eq!(run.result_content_hash, result.result_content_hash());
        assert_eq!(run.summary.trade_count, 1);
        assert_eq!(run.summary.expectancy, summary.expectancy);

        // The trades read back in seq order, trade-for-trade equal.
        let trades = repo.get_trades(&id).await.expect("get_trades");
        assert_eq!(trades, result.trades, "trades round-trip identically");
    }

    // ---- G1(b): the window-edge open-position mark persists + is hash-covered --

    /// A [`OpenPositionMark`] fixture: the shape `run_backtest` writes when a
    /// `SeriesEnd::WindowEdge` run ends holding a position.
    fn a_mark() -> OpenPositionMark {
        OpenPositionMark {
            direction: Direction::Long,
            qty: d(20, 0),
            entry_price: d(100, 0),
            entry_signal_time: 119_999,
            entry_fill_time: 120_000,
            mark_time: 179_999,
            mark_price: d(103, 0),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_window_edge_open_position_mark_round_trips() {
        let (repo, _pool, _tmp) = repo_at(1_700_000_000_000).await;
        let mut breakdown = RegimeBreakdown::new();
        let trade = simple_trade();
        breakdown.record(trade.regime, trade.realized_pnl);
        let (mut result, summary) = result_from(vec![trade], breakdown, SkippedEntryCounts::new());
        result.open_position = Some(a_mark());

        let id = repo
            .save_run(
                &VersionId::new("ver-1"),
                &sample_inputs(),
                &result,
                &summary,
                d(10_000, 0),
            )
            .await
            .expect("save_run");

        let run = repo.get_run(&id).await.expect("get_run").expect("present");
        assert_eq!(
            run.open_position.as_ref(),
            Some(&a_mark()),
            "the persisted mark reads back field-for-field"
        );
        assert_eq!(
            run.summary.trade_count, 1,
            "the mark is not a trade — closed-trade statistics exclude it"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_run_rejects_a_mark_the_stored_hash_does_not_cover() {
        let (repo, pool, _tmp) = repo_at(1_700_000_000_000).await;
        // Empty trade log so every other column can be written honestly by
        // hand: the ONLY difference between what the stored hash covers and
        // what the row carries is the mark.
        let (mut result, _summary) = result_from(
            Vec::new(),
            RegimeBreakdown::default(),
            SkippedEntryCounts::new(),
        );
        result.open_position = Some(a_mark());
        let hash_of_mark_a = result.result_content_hash();

        // A second result identical but for the mark — its hash is what a
        // tamperer would store if the mark column were swapped.
        let mut marked_b = result_from(
            Vec::new(),
            RegimeBreakdown::default(),
            SkippedEntryCounts::new(),
        );
        marked_b.0.open_position = Some(OpenPositionMark {
            mark_price: d(999, 0),
            ..a_mark()
        });
        let hash_of_mark_b = marked_b.0.result_content_hash();
        assert_ne!(
            hash_of_mark_a, hash_of_mark_b,
            "the mark feeds the content hash — different marks hash differently"
        );

        // The immutability trigger blocks UPDATE, so the tamper is seeded as a
        // row honest in every column EXCEPT the pair (stored hash for mark-B,
        // column carrying mark-A): the #39 re-derive must catch exactly that.
        sqlx::query(
            "INSERT INTO backtest_run \
             (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
              engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
              funding_total, slippage_total, trade_count, wins, losses, breakeven, \
              max_win_streak, max_loss_streak, win_rate, expectancy, gross_profit, \
              gross_loss, avg_win, avg_loss, max_drawdown, regime_breakdown, \
              skipped_sub_lot, skipped_sub_notional, skipped_leverage_capped, \
              pair, primary_timeframe, primary_data_version, taker_fee_bps, slippage_bps, \
              funding_config, open_position) \
             VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, '10000', '0', '0', '0', '0', 0, 0, 0, 0, 0, 0, \
                     '0', '0', '0', '0', '0', '0', '0', ?7, 0, 0, 0, \
                     'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates', ?8)",
        )
        .bind("run-lying-mark")
        .bind("ver-1")
        .bind("2026-06-30T00:00:00.000Z")
        .bind(EngineFingerprint::current().as_str())
        .bind(EngineFingerprint::target())
        .bind(&hash_of_mark_b)
        .bind(serde_json::to_string(&RegimeBreakdown::default()).unwrap())
        .bind(serde_json::to_string(&a_mark()).unwrap())
        .execute(&pool)
        .await
        .expect("seed mark-mismatched run");

        let err = repo.get_run(&BacktestRunId::new("run-lying-mark")).await;
        assert!(
            err.is_err(),
            "a mark the stored hash does not cover must fail the #39 re-derive"
        );
    }

    // ---- AC-2: get_run rejects a tampered content hash (#39 re-validate) ------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_run_rejects_tampered_content_hash() {
        let (repo, pool, _tmp) = repo_at(1_700_000_000_000).await;
        let mut breakdown = RegimeBreakdown::new();
        let trade = simple_trade();
        breakdown.record(trade.regime, trade.realized_pnl);
        let (result, summary) = result_from(vec![trade], breakdown, SkippedEntryCounts::new());
        let id = repo
            .save_run(
                &VersionId::new("ver-1"),
                &sample_inputs(),
                &result,
                &summary,
                d(10_000, 0),
            )
            .await
            .expect("save_run");

        // Tamper a TRADE's realized_pnl directly (the hash is trade-dependent), so
        // the re-derived hash no longer matches the stored result_content_hash. We
        // must defeat the immutability trigger: the test corrupts via a sidestep —
        // delete+reinsert is blocked too, so instead corrupt the stored hash column
        // by writing a fresh run row whose stored hash is wrong for its trades.
        // Simpler: directly mutate the stored result_content_hash is blocked by the
        // trigger; instead we seed a SECOND run by hand with a mismatched hash.
        let bad_run = "run-tampered";
        sqlx::query(
            "INSERT INTO backtest_run \
             (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
              engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
              funding_total, slippage_total, trade_count, wins, losses, breakeven, \
              max_win_streak, max_loss_streak, win_rate, expectancy, gross_profit, \
              gross_loss, avg_win, avg_loss, max_drawdown, regime_breakdown, \
              skipped_sub_lot, skipped_sub_notional, skipped_leverage_capped, \
              pair, primary_timeframe, primary_data_version, taker_fee_bps, slippage_bps, \
              funding_config) \
             VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, '10000', '0', '0', '0', '0', 0, 0, 0, 0, 0, 0, \
                     '0', '0', '0', '0', '0', '0', '0', ?7, 0, 0, 0, \
                     'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates')",
        )
        .bind(bad_run)
        .bind("ver-1")
        .bind("2026-06-30T00:00:00.000Z")
        .bind(EngineFingerprint::current().as_str())
        .bind(EngineFingerprint::target())
        // A clearly-wrong hash for an empty trade log.
        .bind("0000000000000000000000000000000000000000000000000000000000000000")
        .bind(serde_json::to_string(&RegimeBreakdown::default()).unwrap())
        .execute(&pool)
        .await
        .expect("seed tampered run");

        let err = repo.get_run(&BacktestRunId::new(bad_run)).await;
        assert!(
            err.is_err(),
            "a stored result_content_hash that does not match the re-derived hash must reject (#39)"
        );

        // The happy path still reads (the genuine run re-derives equal).
        assert!(repo.get_run(&id).await.expect("get_run").is_some());
    }

    // ---- AC-3: save_run rejects an absent / cross-strategy version (#39) ------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_run_rejects_cross_strategy_version() {
        let (repo, pool, _tmp) = repo_at(1_700_000_000_000).await;
        let (result, summary) = result_from(
            vec![simple_trade()],
            RegimeBreakdown::new(),
            SkippedEntryCounts::new(),
        );

        // An absent strategy_version id → real Err, and NO run row persists.
        let err = repo
            .save_run(
                &VersionId::new("ver-does-not-exist"),
                &sample_inputs(),
                &result,
                &summary,
                d(10_000, 0),
            )
            .await;
        assert!(
            err.is_err(),
            "saving against an absent version must reject (#39)"
        );

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backtest_run")
            .fetch_one(&pool)
            .await
            .expect("count runs");
        assert_eq!(count, 0, "a rejected save must persist no run row");
    }

    // ---- AC-4: latest_run_for_version orders by created_at DESC (#40) ---------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latest_run_for_version_orders_by_created_at_desc() {
        let tmp = TempDir::new().unwrap();
        let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
        MIGRATOR.run(db.pool()).await.unwrap();
        let pool = db.pool().clone();
        seed_version(&pool, "strat-1", "ver-1").await;
        let (result, summary) = result_from(
            vec![simple_trade()],
            RegimeBreakdown::new(),
            SkippedEntryCounts::new(),
        );

        // Save three runs at strictly-increasing clock instants.
        let mut latest_id = None;
        for (i, ms) in [1_700_000_000_000_i64, 1_700_000_001_000, 1_700_000_002_000]
            .iter()
            .enumerate()
        {
            let repo = SqliteBacktestRunRepo::with_deps(pool.clone(), FakeClock::at(*ms));
            let id = repo
                .save_run(
                    &VersionId::new("ver-1"),
                    &sample_inputs(),
                    &result,
                    &summary,
                    d(10_000, 0),
                )
                .await
                .unwrap_or_else(|e| panic!("save_run #{i} failed: {e:?}"));
            latest_id = Some(id);
        }

        let repo = SqliteBacktestRunRepo::with_deps(pool.clone(), FakeClock::at(0));
        let latest = repo
            .latest_run_for_version(&VersionId::new("ver-1"))
            .await
            .expect("latest_run")
            .expect("a run exists");
        assert_eq!(
            latest.id,
            latest_id.unwrap(),
            "latest_run_for_version returns the most-recent created_at run (#40)"
        );
        // created_at is the injected-clock instant of the last (latest) save.
        let expected = chrono::DateTime::from_timestamp_millis(1_700_000_002_000)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        assert_eq!(
            latest.created_at, expected,
            "latest run's created_at is the injected-clock instant (D7)"
        );
    }

    // ---- AC-5: list_runs_for_version skips a corrupt summary row (D5) ---------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_runs_for_version_skips_corrupt_row() {
        let (repo, pool, _tmp) = repo_at(1_700_000_000_000).await;
        let (result, summary) = result_from(
            vec![simple_trade()],
            RegimeBreakdown::new(),
            SkippedEntryCounts::new(),
        );

        // One GOOD run via save_run.
        repo.save_run(
            &VersionId::new("ver-1"),
            &sample_inputs(),
            &result,
            &summary,
            d(10_000, 0),
        )
        .await
        .expect("save good run");

        // One CORRUPT summary row: an unsupported schema_version (99) → row_to_run_summary
        // returns Err → skip-with-warning, NOT a whole-list failure.
        sqlx::query(
            "INSERT INTO backtest_run \
             (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
              engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
              funding_total, slippage_total, expectancy, trade_count, \
              pair, primary_timeframe, primary_data_version, taker_fee_bps, slippage_bps, \
              funding_config) \
             VALUES ('run-corrupt', 'ver-1', 99, '2026-06-30T00:00:00.000Z', 'fp', 'tgt', \
                     'hash', '10000', '0', '0', '0', '0', '0', 0, \
                     'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates')",
        )
        .execute(&pool)
        .await
        .expect("seed corrupt run");

        let list = repo
            .list_runs_for_version(&VersionId::new("ver-1"))
            .await
            .expect("list must SUCCEED, skipping the corrupt row (not whole-list fail)");
        assert_eq!(
            list.len(),
            1,
            "the corrupt (schema_version=99) row is skipped; only the good run remains"
        );
    }

    // ---- AC-14: get_trades fails closed on a corrupt trade row (D5 / C9) ------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_trades_fails_closed_on_corrupt_row() {
        let (repo, pool, _tmp) = repo_at(1_700_000_000_000).await;
        let mut breakdown = RegimeBreakdown::new();
        let trade = simple_trade();
        breakdown.record(trade.regime, trade.realized_pnl);
        let (result, summary) = result_from(vec![trade], breakdown, SkippedEntryCounts::new());
        let id = repo
            .save_run(
                &VersionId::new("ver-1"),
                &sample_inputs(),
                &result,
                &summary,
                d(10_000, 0),
            )
            .await
            .expect("save_run");

        // Insert a SECOND, corrupt trade row (a malformed Decimal in realized_pnl).
        // The immutability trigger blocks UPDATE/DELETE, but a fresh INSERT of a
        // corrupt row is allowed — get_trades must then fail-closed (Err), NEVER a
        // partial log (a dropped trade corrupts equity/hash reconstruction).
        sqlx::query(
            "INSERT INTO trade \
             (id, backtest_run_id, seq, direction, qty, entry_price, exit_price, \
              entry_signal_time, entry_fill_time, exit_signal_time, exit_fill_time, \
              fees_total, funding_total, slippage_total, realized_pnl, realized_r, \
              mfe_r, mae_r, exit_reason, source, regime, fills) \
             VALUES ('trade-corrupt', ?1, 1, 'long', '1', '1', '1', 0,0,0,0, '0','0','0', \
                     'NOT_A_DECIMAL', '0', '0', '0', 'take_profit', 'backtest', 'ranging', '[]')",
        )
        .bind(id.as_str())
        .execute(&pool)
        .await
        .expect("seed corrupt trade");

        let err = repo.get_trades(&id).await;
        assert!(
            err.is_err(),
            "a corrupt trade row must fail-closed (Err), never a silently-dropped trade (C9)"
        );

        // get_run inherits the same fail-closed path (it fetches trades internally).
        assert!(
            repo.get_run(&id).await.is_err(),
            "get_run is fail-closed too"
        );
    }

    // ---- AC-15: get_run rejects an unsupported schema_version (D1b) -----------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_run_rejects_unsupported_schema_version() {
        let (repo, pool, _tmp) = repo_at(1_700_000_000_000).await;

        // Seed a run row with a NEWER/unknown schema_version (2). get_run must reject
        // it with DataError::Db("unsupported backtest_run schema version 2"), not
        // silently decode it.
        sqlx::query(
            "INSERT INTO backtest_run \
             (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
              engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
              funding_total, slippage_total, trade_count, wins, losses, breakeven, \
              max_win_streak, max_loss_streak, win_rate, expectancy, gross_profit, \
              gross_loss, avg_win, avg_loss, max_drawdown, regime_breakdown, \
              skipped_sub_lot, skipped_sub_notional, skipped_leverage_capped, \
              pair, primary_timeframe, primary_data_version, taker_fee_bps, slippage_bps, \
              funding_config) \
             VALUES ('run-newschema', 'ver-1', 2, '2026-06-30T00:00:00.000Z', 'fp', 'tgt', \
                     'hash', '10000', '0', '0', '0', '0', 0, 0, 0, 0, 0, 0, '0', '0', '0', '0', \
                     '0', '0', '0', ?1, 0, 0, 0, \
                     'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates')",
        )
        .bind(serde_json::to_string(&RegimeBreakdown::default()).unwrap())
        .execute(&pool)
        .await
        .expect("seed newer-schema run");

        let err = repo.get_run(&BacktestRunId::new("run-newschema")).await;
        match err {
            Err(crate::domain::DataError::Db(msg)) => assert!(
                msg.contains("unsupported backtest_run schema version 2"),
                "must reject the unsupported schema_version; got: {msg}"
            ),
            other => panic!("expected DataError::Db unsupported-schema, got {other:?}"),
        }
    }

    // ---- AC-16: sharpe/sortino persist finite-or-NULL round-trip (D2b) --------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sharpe_sortino_persist_finite_or_null_roundtrip() {
        let (repo, _pool, _tmp) = repo_at(1_700_000_000_000).await;

        // CASE 1: Some(finite) sharpe + sortino → round-trip to the identical f64.
        // Two trades with distinct realized_r give a non-degenerate series.
        let mut t1 = simple_trade();
        t1.realized_r = d(2, 0);
        let mut t2 = simple_trade();
        t2.realized_r = d(-1, 0);
        t2.realized_pnl = d(-10, 0);
        t2.regime = Regime::TrendingDown;
        t2.exit_fill_time = 8;
        let mut breakdown = RegimeBreakdown::new();
        breakdown.record(t1.regime, t1.realized_pnl);
        breakdown.record(t2.regime, t2.realized_pnl);
        let (result, summary) = result_from(vec![t1, t2], breakdown, SkippedEntryCounts::new());
        assert!(summary.sharpe.is_some(), "fixture must yield Some(sharpe)");
        assert!(
            summary.sortino.is_some(),
            "fixture must yield Some(sortino)"
        );

        let id = repo
            .save_run(
                &VersionId::new("ver-1"),
                &sample_inputs(),
                &result,
                &summary,
                d(10_000, 0),
            )
            .await
            .expect("save_run with Some sharpe/sortino");
        let run = repo.get_run(&id).await.expect("get_run").expect("present");
        assert_eq!(
            run.summary.sharpe, summary.sharpe,
            "sharpe round-trips identically"
        );
        assert_eq!(
            run.summary.sortino, summary.sortino,
            "sortino round-trips identically"
        );

        // CASE 2: None sharpe/sortino (a single trade → N<2) → SQL NULL → None.
        let single = simple_trade();
        let mut bd = RegimeBreakdown::new();
        bd.record(single.regime, single.realized_pnl);
        let (result2, summary2) = result_from(vec![single], bd, SkippedEntryCounts::new());
        assert_eq!(summary2.sharpe, None);
        assert_eq!(summary2.sortino, None);
        let id2 = repo
            .save_run(
                &VersionId::new("ver-1"),
                &sample_inputs(),
                &result2,
                &summary2,
                d(10_000, 0),
            )
            .await
            .expect("save_run with None sharpe/sortino");
        let run2 = repo.get_run(&id2).await.expect("get_run").expect("present");
        assert_eq!(
            run2.summary.sharpe, None,
            "None sharpe round-trips to None (NULL)"
        );
        assert_eq!(
            run2.summary.sortino, None,
            "None sortino round-trips to None (NULL)"
        );
    }

    // ---- AC-13 (PRIMARY GATE): nontrivial lossless-reconstruction roundtrip ---

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_then_get_rederives_identical_content_hash_nontrivial() {
        let (repo, _pool, _tmp) = repo_at(1_700_000_000_000).await;
        let (result, summary) = nontrivial_result();

        // The STORED content hash (computed in-memory BEFORE persistence).
        let stored_hash = result.result_content_hash();

        let id = repo
            .save_run(
                &VersionId::new("ver-1"),
                &sample_inputs(),
                &result,
                &summary,
                d(10_000, 0),
            )
            .await
            .expect("save nontrivial run");

        // get_run RE-DERIVES the hash from the read-back rows (the #39 path). If it
        // returns Ok, the re-derived hash matched the stored column → lossless.
        let run = repo
            .get_run(&id)
            .await
            .expect("get_run re-derives + validates the hash")
            .expect("run is present");

        // The persisted hash equals the in-memory STORED hash (multi-fill fills JSON,
        // trailing-zero Decimal .normalize(), all 4 regimes, non-empty skipped all
        // round-trip byte-identically into the feed).
        assert_eq!(
            run.result_content_hash, stored_hash,
            "stored content hash must equal the in-memory hash (D4b lossless roundtrip)"
        );

        // And re-deriving DIRECTLY from the read-back trades reproduces it too.
        let trades = repo.get_trades(&id).await.expect("get_trades");
        assert_eq!(trades, result.trades, "trades round-trip trade-for-trade");
        let rederived = BacktestResult {
            trades,
            net_pnl: result.net_pnl,
            fees_total: result.fees_total,
            funding_total: result.funding_total,
            slippage_total: result.slippage_total,
            regime_breakdown: result.regime_breakdown,
            skipped_entries: result.skipped_entries,
            open_position: result.open_position.clone(),
            engine_fingerprint: EngineFingerprint::current(),
            summary: SummaryStats::default(),
            equity_curve: EquityCurve::default(),
        }
        .result_content_hash();
        assert_eq!(
            rederived, stored_hash,
            "re-deriving from the read-back trades reproduces the stored hash byte-for-byte"
        );
    }

    // ---- AC-6: persisted run fields do NOT change result_content_hash ---------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persisted_run_fields_do_not_change_result_content_hash() {
        // The frozen-invariant exclusion guard (#69 / README C8): the read-side
        // reconstruction defaults the oracle-EXCLUDED fields (summary / equity_curve
        // / engine_fingerprint), so they cannot perturb the re-derived hash. This
        // mirrors `content_hash_excludes_fingerprint`: a result differing ONLY in
        // those fields hashes identically. Proven directly over the nontrivial fixture
        // (no DB needed — this guards the reconstruction shape get_run relies on).
        let (result, _summary) = nontrivial_result();
        let base = result.result_content_hash();

        // Perturb ONLY the persisted/derived run fields the read RECONSTRUCTS with
        // defaults — summary, equity_curve, engine_fingerprint. The hash must not move.
        let mut perturbed = result.clone();
        perturbed.summary = SummaryStats {
            trade_count: 12_345,
            sharpe: Some(9.9),
            sortino: Some(-3.3),
            expectancy: Decimal::new(777, 0),
            ..SummaryStats::default()
        };
        perturbed.equity_curve = EquityCurve::default();
        perturbed.engine_fingerprint = EngineFingerprint::default();
        assert_eq!(
            base,
            perturbed.result_content_hash(),
            "summary / equity_curve / engine_fingerprint are oracle-EXCLUDED — \
             persisting/deriving them does NOT change result_content_hash (#69 / C8)"
        );
    }
}
