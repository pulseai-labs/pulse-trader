//! The Backtest Lab's wire contract (r1.s3.w3) — ring-owned DTOs plus the pure
//! projection that builds them from a [`BacktestOutcome`].
//!
//! **Ring-owned wire types, not `specta` derives on domain types** — the
//! [`LibraryOverview`](super::library::LibraryOverview) pattern. Every crossing type
//! is declared here and built by a pure function, so the domain never grows a
//! serialization concern and the frontend never sees a shape that changed because a
//! domain field moved.
//!
//! **Every analytical and provenance value comes from the read-back.** The outcome
//! this projects was assembled from the saved run, the saved trades and the exact
//! snapshots those inputs name — so what the screen renders is what the database
//! holds, not what memory held a moment before the insert. The single exception is
//! `fingerprint_warning`: the FR-7 comparison must happen *before* the insert (after
//! it the fresh row is its own prior) and has no column, so it is control metadata
//! rather than persisted truth. It is documented as such rather than quietly mixed
//! in.
//!
//! **Decimals cross as exact strings** (NFR-2). The frontend renders them verbatim
//! and does no numeric math, so a rounded or reconstructed figure cannot appear
//! screen-side. `sharpe`/`sortino` stay nullable numbers because they are genuinely
//! `f64`-derived, and `null` is how "not enough trades to compute" is already
//! spelled everywhere else.
//!
//! **`fills` are deliberately absent.** W4 renders no fill-level view, and the inline
//! per-fill JSON is a materially larger payload for data nothing displays.
//!
//! **The projection is fallible, and refuses rather than guesses.** Two things here
//! can fail on a corrupt-but-hash-consistent saved row: an enum token, and narrowing a
//! stored count or schema tag to the wire's `u32`. The first draft fabricated
//! `"unknown"` and clamped to `u32::MAX`; both render a plausible false number for a
//! row the truth-source contract says must refuse. Enum tokens are now exhaustive
//! `match` label functions — infallible by construction and compiler-checked against a
//! new variant — and every narrowing returns a typed post-save failure carrying the
//! saved run id, so the caller can still say which run is affected.
//!
//! **Epoch milliseconds cross as exact decimal strings, not numbers.** Specta refuses
//! to export `i64` at all — it will not risk a silent precision loss in the JS number
//! type — and the two ways round that are a lossy `f64` or an exact string. Every
//! other exact value here is already a string the frontend renders verbatim and
//! converts only for chart geometry, so a string keeps one rule instead of two.
//! `schemaVersion` is a small tag and crosses as `u32`.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::application::backtest::{
    BacktestAppError, BacktestOutcome, Histogram, ReadBackFailure, ReadBackStage,
};
use crate::domain::backtest::{EquityCurve, ExitReason, Regime, RegimeCell, TradeSource};
use crate::domain::{BacktestInputs, BacktestRunId, Direction, FundingConfig, PersistedRun, Trade};

use super::coach::SummaryDto;

/// What the desktop asks for: one persisted strategy version.
///
/// Pair, timeframes and costs are **not** here. r1's Backtest Lab runs the fixed
/// BTCUSDT M15+H4 / default-cost request, and a field the product cannot vary would
/// be a control that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct BacktestRunRequest {
    /// The immutable strategy version to run.
    pub version_id: String,
}

/// One `[lower, upper)` histogram bin and its count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct HistogramBinDto {
    /// Inclusive lower bound in R, exact decimal string.
    pub lower: String,
    /// Exclusive upper bound in R, exact decimal string.
    pub upper: String,
    /// How many trades landed in this bin.
    pub count: u32,
}

/// A pinned excursion histogram, ready to render without further analysis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct HistogramDto {
    /// The shared bin width in R, exact decimal string (`"0.25"`).
    pub bin_width: String,
    /// The finite bins, ascending, covering `[0, 3)`.
    pub bins: Vec<HistogramBinDto>,
    /// Normalized values below `0` — a sign violation, counted rather than hidden.
    pub underflow: u32,
    /// Normalized values at or above `3R`.
    pub overflow: u32,
}

/// One regime's persisted trade count and net P&L.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct RegimeCellDto {
    /// `trending_up` / `trending_down` / `ranging` / `unknown`.
    pub regime: String,
    /// Trades that opened in this regime.
    pub trade_count: u32,
    /// Net P&L across them, exact decimal string.
    pub net_pnl: String,
}

/// One point on the reconstructed equity curve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct EquityPointDto {
    /// Epoch milliseconds, exact integer string.
    pub time_ms: String,
    /// Account equity at that point, exact decimal string.
    pub equity: String,
}

/// One persisted trade, exactly as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct TradeRowDto {
    /// `long` / `short`.
    pub direction: String,
    /// Position size, exact decimal string.
    pub qty: String,
    /// Entry fill price.
    pub entry_price: String,
    /// Exit fill price.
    pub exit_price: String,
    /// Epoch ms the entry signalled.
    pub entry_signal_time: String,
    /// Epoch ms the entry filled.
    pub entry_fill_time: String,
    /// Epoch ms the exit signalled.
    pub exit_signal_time: String,
    /// Epoch ms the exit filled.
    pub exit_fill_time: String,
    /// Taker fees on this trade.
    pub fees_total: String,
    /// Signed funding on this trade.
    pub funding_total: String,
    /// Adverse slippage cost.
    pub slippage_total: String,
    /// Realized P&L.
    pub realized_pnl: String,
    /// Realized R multiple.
    pub realized_r: String,
    /// Maximum favourable excursion, in R — exact, unbinned.
    pub mfe_r: String,
    /// Maximum adverse excursion, in R — exact, unbinned, sign preserved.
    pub mae_r: String,
    /// Why the trade closed.
    pub exit_reason: String,
    /// What opened it.
    pub source: String,
    /// The regime at entry.
    pub regime: String,
}

/// The complete Backtest Lab response.
///
/// `PartialEq` but not `Eq` — `sharpe`/`sortino` are `f64`, the same reason
/// [`PersistedRun`] carries only `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct BacktestRunDto {
    // --- identity -------------------------------------------------------
    /// The freshly minted run id. Every invocation creates a new one.
    pub run_id: String,
    /// The immutable version this run is attributed to.
    pub strategy_version_id: String,
    /// The run-row schema tag.
    pub schema_version: u32,
    /// When the run was recorded (RFC3339 UTC, millisecond precision).
    pub created_at: String,

    // --- provenance (#110) ----------------------------------------------
    /// The pair the run consumed.
    pub pair: String,
    /// The primary timeframe, Binance interval text.
    pub primary_timeframe: String,
    /// The exact immutable primary snapshot identity.
    pub primary_data_version: String,
    /// The HTF timeframe, when the run used one. Paired with the next field.
    pub htf_timeframe: Option<String>,
    /// The exact immutable HTF snapshot identity.
    pub htf_data_version: Option<String>,
    /// First candle `open_time` of the **reloaded** primary snapshot, epoch ms.
    pub first_open_time_ms: String,
    /// Last candle `close_time` of the **reloaded** primary snapshot, epoch ms.
    pub last_close_time_ms: String,
    /// Starting equity, exact decimal string.
    pub starting_equity: String,
    /// Taker fee in basis points.
    pub taker_fee_bps: String,
    /// Slippage in basis points.
    pub slippage_bps: String,
    /// How funding was sourced (`snapshot_rates`).
    pub funding: String,

    // --- engine ---------------------------------------------------------
    /// The recording engine's build fingerprint.
    pub engine_fingerprint: String,
    /// The recording engine's target triple.
    pub engine_target: String,
    /// The stored integrity hash, re-derived and checked on read.
    pub result_content_hash: String,
    /// FR-7 warning from the pre-save comparison against the prior run.
    ///
    /// The one field not read back from storage — the comparison has no column and
    /// must happen before the insert.
    pub fingerprint_warning: Option<String>,

    // --- headline -------------------------------------------------------
    /// Net P&L.
    pub net_pnl: String,
    /// Total taker fees.
    pub fees_total: String,
    /// Total signed funding.
    pub funding_total: String,
    /// Total adverse slippage.
    pub slippage_total: String,
    /// Mean P&L per trade.
    pub expectancy: String,
    /// Fraction of trades that won.
    pub win_rate: String,
    /// Gross profit over gross loss; `null` when there is no loss to divide by.
    pub profit_factor: Option<String>,
    /// Sum of winning P&L.
    pub gross_profit: String,
    /// Sum of losing P&L.
    pub gross_loss: String,
    /// Mean winning trade.
    pub avg_win: String,
    /// Mean losing trade.
    pub avg_loss: String,
    /// Largest peak-to-trough equity drop.
    pub max_drawdown: String,
    /// Number of completed trades.
    pub trade_count: u32,
    /// How many won.
    pub win_count: u32,
    /// How many lost.
    pub loss_count: u32,
    /// Longest winning streak.
    pub max_win_streak: u32,
    /// Longest losing streak.
    pub max_loss_streak: u32,
    /// Sharpe ratio; `null` when undefined for this run.
    pub sharpe: Option<f64>,
    /// Sortino ratio; `null` when undefined for this run.
    pub sortino: Option<f64>,

    // --- skipped entries -------------------------------------------------
    /// Entries skipped below the lot step.
    pub skipped_sub_lot: u32,
    /// Entries skipped below minimum notional.
    pub skipped_sub_notional: u32,
    /// Entries skipped by the leverage cap.
    pub skipped_leverage_capped: u32,

    // --- derived read projections ---------------------------------------
    /// The equity curve, rebuilt from the persisted trades.
    pub equity: Vec<EquityPointDto>,
    /// The four regimes, always in this fixed order.
    pub regimes: Vec<RegimeCellDto>,
    /// The pinned MFE histogram.
    pub mfe: HistogramDto,
    /// The pinned MAE histogram (magnitude).
    pub mae: HistogramDto,
    /// Every persisted trade, in `seq` order, with exact values.
    pub trades: Vec<TradeRowDto>,
}

/// What `compare_child_run` is asked for: one persisted run id (r2.s1.w4 C3).
///
/// Everything else — the run's version, that version's parent, the parent's
/// latest run — is resolved server-side so the screen never reconstructs a
/// lineage it was already shown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CompareChildRunRequest {
    /// The child run to compare against its parent's latest run.
    pub child_run_id: String,
}

/// A child run beside its parent's latest run — the Backtest Lab's
/// before/after for ANY child version, not only a coach accept (r2.s1.w4 C3).
///
/// `before` is always the parent's LATEST persisted run; `after` is the run
/// named in the request. `inputs_differ` is `BacktestInputs` equality over the
/// two persisted tuples — window included — and reads `true` when either side
/// predates recorded inputs, with `inputs_note` saying which.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CompareChildRunDto {
    /// The version the child run belongs to.
    pub child_version_id: String,
    /// The parent version the `before` run belongs to.
    pub parent_version_id: String,
    /// The run named in the request — the `after` half.
    pub child_run_id: String,
    /// The parent's latest persisted run — the `before` half.
    pub parent_run_id: String,
    /// The parent's latest run, in the coach DTO's cell shape.
    pub before: SummaryDto,
    /// The child run, in the same shape.
    pub after: SummaryDto,
    /// `parent.inputs != child.inputs`; `true` when either side is `None`.
    pub inputs_differ: bool,
    /// Why the inputs cannot be proven equal, when a side predates them.
    pub inputs_note: Option<String>,
}

/// Exact decimal text — the same `.normalize()`d form the database stores.
fn dec(value: Decimal) -> String {
    value.normalize().to_string()
}

fn dec_opt(value: Option<Decimal>) -> Option<String> {
    value.map(dec)
}

/// Epoch milliseconds as an exact integer string (see the module docs).
fn ms(value: i64) -> String {
    value.to_string()
}

// --- exhaustive enum labels ------------------------------------------------
//
// Each returns the SAME `snake_case` token the column stores, and each is an
// exhaustive `match` rather than a serde round-trip. Infallible by construction: a
// new variant is a compile error here, not an `"unknown"` on someone's screen.

fn direction_label(direction: Direction) -> &'static str {
    match direction {
        Direction::Long => "long",
        Direction::Short => "short",
    }
}

fn exit_reason_label(reason: ExitReason) -> &'static str {
    match reason {
        ExitReason::StopLoss => "stop_loss",
        ExitReason::TakeProfit => "take_profit",
        ExitReason::Signal => "signal",
        ExitReason::EndOfData => "end_of_data",
    }
}

fn trade_source_label(source: TradeSource) -> &'static str {
    match source {
        TradeSource::Backtest => "backtest",
    }
}

fn regime_label(regime: Regime) -> &'static str {
    match regime {
        Regime::TrendingUp => "trending_up",
        Regime::TrendingDown => "trending_down",
        Regime::Ranging => "ranging",
        Regime::Unknown => "unknown",
    }
}

fn funding_label(funding: FundingConfig) -> &'static str {
    match funding {
        FundingConfig::SnapshotRates => "snapshot_rates",
    }
}

/// A projection failure that still names the saved run.
fn projection_failed(run_id: &BacktestRunId, reason: String) -> BacktestAppError {
    BacktestAppError::SavedButReadBackFailed {
        run_id: run_id.clone(),
        stage: ReadBackStage::Projection,
        failure: ReadBackFailure::Projection(reason),
    }
}

/// Narrow a stored count to the wire's `u32`, refusing rather than clamping.
fn count(run_id: &BacktestRunId, field: &str, value: usize) -> Result<u32, BacktestAppError> {
    u32::try_from(value).map_err(|_| {
        projection_failed(
            run_id,
            format!("stored `{field}` = {value} does not fit the wire's u32"),
        )
    })
}

/// The same for the `i64` schema tag.
fn narrow_i64(run_id: &BacktestRunId, field: &str, value: i64) -> Result<u32, BacktestAppError> {
    u32::try_from(value).map_err(|_| {
        projection_failed(
            run_id,
            format!("stored `{field}` = {value} does not fit the wire's u32"),
        )
    })
}

fn histogram_dto(histogram: &Histogram) -> HistogramDto {
    HistogramDto {
        bin_width: dec(histogram.bin_width),
        bins: histogram
            .bins
            .iter()
            .map(|bin| HistogramBinDto {
                lower: dec(bin.lower),
                upper: dec(bin.upper),
                count: bin.count,
            })
            .collect(),
        underflow: histogram.underflow,
        overflow: histogram.overflow,
    }
}

fn equity_dto(curve: &EquityCurve) -> Vec<EquityPointDto> {
    curve
        .0
        .iter()
        .map(|point| EquityPointDto {
            time_ms: ms(point.time_ms),
            equity: dec(point.equity),
        })
        .collect()
}

fn regime_dto(
    run_id: &BacktestRunId,
    regime: &str,
    cell: RegimeCell,
) -> Result<RegimeCellDto, BacktestAppError> {
    Ok(RegimeCellDto {
        regime: regime.to_owned(),
        trade_count: count(run_id, "regime.trade_count", cell.trade_count)?,
        net_pnl: dec(cell.net_pnl),
    })
}

fn trade_dto(trade: &Trade) -> TradeRowDto {
    TradeRowDto {
        direction: direction_label(trade.direction).to_owned(),
        qty: dec(trade.qty),
        entry_price: dec(trade.entry_price),
        exit_price: dec(trade.exit_price),
        entry_signal_time: ms(trade.entry_signal_time),
        entry_fill_time: ms(trade.entry_fill_time),
        exit_signal_time: ms(trade.exit_signal_time),
        exit_fill_time: ms(trade.exit_fill_time),
        fees_total: dec(trade.fees_total),
        funding_total: dec(trade.funding_total),
        slippage_total: dec(trade.slippage_total),
        realized_pnl: dec(trade.realized_pnl),
        realized_r: dec(trade.realized_r),
        mfe_r: dec(trade.mfe_r),
        mae_r: dec(trade.mae_r),
        exit_reason: exit_reason_label(trade.exit_reason).to_owned(),
        source: trade_source_label(trade.source).to_owned(),
        regime: regime_label(trade.regime).to_owned(),
    }
}

/// Project a completed outcome onto the wire.
///
/// Pure: it reads the outcome and nothing else. Every provenance and analytical
/// value below traces to a persisted column or to the reloaded snapshot the
/// persisted inputs name.
///
/// # Errors
///
/// Returns [`BacktestAppError::SavedButReadBackFailed`] with
/// [`ReadBackStage::Projection`] when a stored count or schema tag does not fit the
/// wire's `u32`. The run is saved, so the error still names it — refusing is the
/// point: a clamped count would render a plausible false number for a row that
/// should not be reported at all.
pub fn backtest_run_dto(outcome: &BacktestOutcome) -> Result<BacktestRunDto, BacktestAppError> {
    let run: &PersistedRun = &outcome.run;
    let summary = &run.summary;
    // Proven `Some` by the use case, which refuses a fresh read-back without inputs
    // — so the outcome carries them non-optionally and this projection is total.
    let inputs: &BacktestInputs = &outcome.inputs;
    let breakdown = &run.regime_breakdown;
    let run_id = &run.id;

    Ok(BacktestRunDto {
        run_id: run.id.as_str().to_owned(),
        strategy_version_id: run.strategy_version_id.as_str().to_owned(),
        schema_version: narrow_i64(run_id, "schema_version", run.schema_version)?,
        created_at: run.created_at.clone(),

        pair: inputs.pair.as_str().to_owned(),
        primary_timeframe: inputs.primary.timeframe.binance_interval().to_owned(),
        primary_data_version: inputs.primary.data_version.as_str().to_owned(),
        htf_timeframe: inputs
            .htf
            .as_ref()
            .map(|htf| htf.timeframe.binance_interval().to_owned()),
        htf_data_version: inputs
            .htf
            .as_ref()
            .map(|htf| htf.data_version.as_str().to_owned()),
        // From the RELOADED snapshot: the truthful range of the data this run
        // consumed, which survives HEAD moving afterwards.
        first_open_time_ms: ms(outcome
            .primary
            .candles
            .first()
            .map_or(0, |candle| candle.open_time)),
        last_close_time_ms: ms(outcome
            .primary
            .candles
            .last()
            .map_or(0, |candle| candle.close_time)),
        starting_equity: dec(run.starting_equity),
        taker_fee_bps: dec(inputs.taker_fee_bps),
        slippage_bps: dec(inputs.slippage_bps),
        funding: funding_label(inputs.funding).to_owned(),

        engine_fingerprint: run.engine_fingerprint.clone(),
        engine_target: run.engine_target.clone(),
        result_content_hash: run.result_content_hash.clone(),
        fingerprint_warning: outcome.fingerprint_warning.clone(),

        net_pnl: dec(run.net_pnl),
        fees_total: dec(run.fees_total),
        funding_total: dec(run.funding_total),
        slippage_total: dec(run.slippage_total),
        expectancy: dec(summary.expectancy),
        win_rate: dec(summary.win_rate),
        profit_factor: dec_opt(summary.profit_factor),
        gross_profit: dec(summary.gross_profit),
        gross_loss: dec(summary.gross_loss),
        avg_win: dec(summary.avg_win),
        avg_loss: dec(summary.avg_loss),
        max_drawdown: dec(summary.max_drawdown),
        trade_count: count(run_id, "trade_count", summary.trade_count)?,
        win_count: count(run_id, "win_count", summary.win_count)?,
        loss_count: count(run_id, "loss_count", summary.loss_count)?,
        max_win_streak: count(run_id, "max_win_streak", summary.max_win_streak)?,
        max_loss_streak: count(run_id, "max_loss_streak", summary.max_loss_streak)?,
        sharpe: summary.sharpe,
        sortino: summary.sortino,

        skipped_sub_lot: count(run_id, "skipped_sub_lot", run.skipped_entries.sub_lot)?,
        skipped_sub_notional: count(
            run_id,
            "skipped_sub_notional",
            run.skipped_entries.sub_notional,
        )?,
        skipped_leverage_capped: count(
            run_id,
            "skipped_leverage_capped",
            run.skipped_entries.leverage_capped,
        )?,

        equity: equity_dto(&outcome.equity_curve()),
        // Fixed order, always four entries: a chart that reorders its categories
        // between runs is unreadable, and an absent regime is a zero, not a gap.
        regimes: vec![
            regime_dto(run_id, "trending_up", breakdown.trending_up())?,
            regime_dto(run_id, "trending_down", breakdown.trending_down())?,
            regime_dto(run_id, "ranging", breakdown.ranging())?,
            regime_dto(run_id, "unknown", breakdown.unknown())?,
        ],
        mfe: histogram_dto(&outcome.mfe),
        mae: histogram_dto(&outcome.mae),
        trades: outcome.trades.iter().map(trade_dto).collect(),
    })
}
