//! The `loss_context` use case (r6.s3.w4, PDR-0046 decision 4, STR-0003): where a stored
//! run's losses fall.
//!
//! Nothing per-trade stores volatility or 4H structure (`trade` table:
//! `migrations/0003_backtest_runs.up.sql` plus `stop_price`). So the context is rebuilt
//! at read time from the run's own pinned candle snapshots, the way the chart read
//! (ADR-0030) and `explain_marks` (ADR-0031) load them — but by the timeframe and
//! `data_version` the run's persisted inputs name, never by the current HEAD. No
//! migration, and old runs work too.
//!
//! The read describes; it decides nothing and writes nothing. It adds no field to
//! `BacktestResult` or `RegimeBreakdown`, so no stored hash moves.
//!
//! The pivot span and the ATR period are **STR-0003's** constants, named below: the
//! 4H swing-trend rule Cipher v2 arms on reads `PivotHigh`/`PivotLow` with
//! `left = right = 3`, and the volatility context is `ATR(14)` on the primary series.

use std::collections::BTreeSet;

use rust_decimal::Decimal;
use serde::Serialize;

use crate::adapters::backtest::{BarReading, SwingPivots, bar_readings, swing_pivots_at};
use crate::domain::{
    CandleSeries, CandleSeriesRepository, DataError, Direction, HoldoutFreeze, LossContextReport,
    PersistedRun, SnapshotSelection, Trade, TradeContext, TrendSide, loss_context,
};

/// STR-0003's ATR period for the volatility context: `ATR(14)` on the primary series.
pub const ATR_PERIOD: u32 = 14;

/// STR-0003's swing pivot span for the trend context: `(left, right) = (3, 3)` on the
/// higher timeframe — the span Cipher v2's arm rule reads.
pub const PIVOT_SPAN: (u32, u32) = (3, 3);

const MS_PER_HOUR: i64 = 3_600_000;
const MS_PER_DAY: i64 = 24 * MS_PER_HOUR;

/// The 4H swing trend at a primary close time, before it is read against a side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwingTrend {
    /// The latest confirmed swing high and swing low are both above the ones before.
    Up,
    /// The latest confirmed swing high and swing low are both below the ones before.
    Down,
    /// Four swings are confirmed and they do not trend.
    Ranging,
    /// A swing is not yet confirmed, or there is no higher timeframe.
    Unknown,
}

impl SwingTrend {
    /// The trend relative to a trade's side: `With` is up for a long or down for a
    /// short, `Against` the reverse.
    #[must_use]
    pub fn relative_to(self, direction: Direction) -> TrendSide {
        match (self, direction) {
            (Self::Up, Direction::Long) | (Self::Down, Direction::Short) => TrendSide::With,
            (Self::Up, Direction::Short) | (Self::Down, Direction::Long) => TrendSide::Against,
            (Self::Ranging, _) => TrendSide::Ranging,
            (Self::Unknown, _) => TrendSide::Unknown,
        }
    }

    /// `Up` when `PivotHigh > PreviousPivotHigh && PivotLow > PreviousPivotLow`, `Down`
    /// when both are `<`, `Ranging` otherwise, `Unknown` when any of the four is `None`.
    fn of(pivots: &SwingPivots) -> Self {
        let (Some(high), Some(previous_high), Some(low), Some(previous_low)) = (
            pivots.high,
            pivots.previous_high,
            pivots.low,
            pivots.previous_low,
        ) else {
            return Self::Unknown;
        };
        if high > previous_high && low > previous_low {
            Self::Up
        } else if high < previous_high && low < previous_low {
            Self::Down
        } else {
            Self::Ranging
        }
    }
}

/// The 4H swing trend at a primary close time: the four swings of [`PIVOT_SPAN`] over
/// every `htf` candle whose `close_time` is at or before `at_close_ms`.
#[must_use]
pub fn swing_trend_at(htf: &CandleSeries, at_close_ms: i64) -> SwingTrend {
    // The span is a constant the engine always accepts; an engine that refused it
    // would read no swing, which is `Unknown` — never a guessed trend.
    swing_pivots_at(htf, at_close_ms, PIVOT_SPAN)
        .map_or(SwingTrend::Unknown, |pivots| SwingTrend::of(&pivots))
}

/// The read's typed refusal.
#[derive(Debug, thiserror::Error)]
pub enum LossContextError {
    /// A candle-series read failed.
    #[error(transparent)]
    Data(#[from] DataError),
    /// The run's recorded inputs name no snapshot to read: a run saved before
    /// migration `0006` records no inputs at all.
    #[error("{path}: {message}")]
    MissingSnapshot {
        /// The run member the refusal attaches to.
        path: &'static str,
        /// The correctable description.
        message: String,
    },
    /// An indicator engine could not be built.
    #[error("{0}")]
    Engine(String),
}

/// One run's losses by context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LossContextRead {
    /// The run the read describes.
    pub run_id: String,
    /// The higher timeframe the trend was read on, when the run recorded one.
    pub htf_timeframe: Option<String>,
    /// Trades whose entry signal bar is not in the loaded primary series: each reads
    /// `Unknown` in the trend and volatility groupings.
    pub unmatched_signal_bars: usize,
    /// The groupings.
    pub report: LossContextReport,
}

/// One recorded snapshot, cut at the holdout start before any engine steps.
fn load_recorded<C>(
    candles: &C,
    run: &PersistedRun,
    selection: &SnapshotSelection,
    freeze: Option<HoldoutFreeze>,
) -> Result<CandleSeries, LossContextError>
where
    C: CandleSeriesRepository,
{
    let inputs = run.inputs.as_ref().ok_or_else(|| missing_inputs(run))?;
    let mut series = candles
        .load_version(&inputs.pair, selection.timeframe, &selection.data_version)?
        .series;
    if let Some(freeze) = freeze {
        series
            .candles
            .retain(|candle| candle.open_time < freeze.holdout_start_ms);
    }
    Ok(series)
}

fn missing_inputs(run: &PersistedRun) -> LossContextError {
    LossContextError::MissingSnapshot {
        path: "inputs",
        message: format!(
            "run {} recorded no inputs (it was saved before migration 0006), so the snapshots \
             its trades were taken on cannot be named",
            run.id.as_str()
        ),
    }
}

/// The UTC hour (0–23) of an epoch-millisecond instant.
fn hour_utc(ms: i64) -> u8 {
    u8::try_from(ms.rem_euclid(MS_PER_DAY) / MS_PER_HOUR).unwrap_or(0)
}

/// Rebuild each trade's context at its entry signal bar and fold the run.
///
/// 1. Load the run's recorded primary snapshot and, when it recorded one, its higher
///    timeframe, each by the timeframe and `data_version` its persisted inputs name,
///    cut at the holdout start. Nothing is hard-coded to 15m or 4h.
/// 2. Step `ATR(`[`ATR_PERIOD`]`)` over the primary candles and the four swing pivots of
///    [`PIVOT_SPAN`] over the HTF candles, each HTF candle fed only once its `close_time`
///    is at or before the primary bar's close (ADR-0024).
/// 3. At the primary bar whose `close_time == entry_signal_time`: `atr_ratio` is ATR
///    divided by that bar's close; the trend is read from the four swings as
///    [`SwingTrend`] says, then against the trade's side. A trade whose signal bar is
///    not in the loaded series reads `Unknown` and has no ratio; the read counts it.
///
/// # Errors
///
/// [`LossContextError`] for a store error, a run with no recorded inputs, or an engine
/// that cannot be built.
pub fn loss_context_for_run<C>(
    candles: &C,
    freeze: Option<HoldoutFreeze>,
    run: &PersistedRun,
    trades: &[Trade],
) -> Result<LossContextRead, LossContextError>
where
    C: CandleSeriesRepository,
{
    let inputs = run.inputs.as_ref().ok_or_else(|| missing_inputs(run))?;
    let primary = load_recorded(candles, run, &inputs.primary, freeze)?;
    let htf = inputs
        .htf
        .as_ref()
        .map(|selection| load_recorded(candles, run, selection, freeze))
        .transpose()?;

    let wanted: BTreeSet<i64> = trades.iter().map(|trade| trade.entry_signal_time).collect();
    let readings = bar_readings(&primary, htf.as_ref(), &wanted, ATR_PERIOD, PIVOT_SPAN)
        .map_err(LossContextError::Engine)?;

    let mut unmatched_signal_bars = 0_usize;
    let contexts: Vec<TradeContext> = trades
        .iter()
        .map(|trade| {
            let reading: Option<&BarReading> = readings.get(&trade.entry_signal_time);
            if reading.is_none() {
                unmatched_signal_bars += 1;
            }
            TradeContext {
                trend: reading
                    .and_then(|reading| reading.pivots.as_ref())
                    .map_or(SwingTrend::Unknown, SwingTrend::of)
                    .relative_to(trade.direction),
                atr_ratio: reading.and_then(|reading| ratio(reading.atr?, reading.close)),
                hour_utc: hour_utc(trade.entry_fill_time),
            }
        })
        .collect();

    // The two slices are built from one another, so their lengths always agree; a
    // mismatch would be a bug here, and it surfaces as the fold's own typed error.
    let report = loss_context(trades, &contexts)
        .map_err(|error| LossContextError::Engine(error.to_string()))?;
    Ok(LossContextRead {
        run_id: run.id.as_str().to_owned(),
        htf_timeframe: inputs
            .htf
            .as_ref()
            .map(|selection| selection.timeframe.binance_interval().to_owned()),
        unmatched_signal_bars,
        report,
    })
}

/// `atr ÷ close`, or `None` for a zero close.
fn ratio(atr: Decimal, close: Decimal) -> Option<Decimal> {
    atr.checked_div(close)
}
