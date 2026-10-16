//! The chart read contract (r5.s2.w1, PDR-0036) — the **app's** one view of what
//! the Chart tab draws: a window of candles for a pair and timeframe (capped and
//! cut at an open freeze's holdout start) and, when a backtest run is named, that
//! run's trades and one overlay series per S/R, pivot, candle-pattern and EMA
//! operand its version's rule uses on that series.
//!
//! **The engine computes; the tab draws (PDR-0002).** Every overlay value here
//! comes from stepping the existing [`IndicatorEngine`] exactly as MCP
//! `export_indicators` does — `from_specs`, the holdout cut *before* the first
//! step, one `current(...)` read per bar — so an overlay's values equal what an
//! agent sees for the same token, pair, timeframe and data version (SPINE G2).
//! No indicator, level, pivot or pattern maths lives in this module, and none
//! lives in TypeScript.
//!
//! **Wire shape.** camelCase; times cross as millisecond integer strings,
//! decimals as exact strings (specta refuses `i64`/`u64`; NFR-2) and counts as
//! `u32`. The overlay list is one internally tagged enum on `kind` (`level`,
//! `line`, `pivot`, `pattern` — the `streak_card.rs` precedent); every overlay
//! carries the `label` an agent can feed `export_indicators`.
//!
//! **Additive by design (ADR-0020).** A NEW plain route, a NEW bus command and a
//! NEW client method; not one existing Tauri signature changes.
//!
//! **Refusals.** An unknown `runId` is a 422 `not_found`; a `pair` or `timeframe`
//! the run does not use is a 422 naming the field; a bounded window holding more
//! than [`CHART_BAR_CAP`] candles is a 422 naming the cap and the count — never a
//! silent truncation (SPINE Q3/G4).

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::adapters::indicators::engine::IndicatorEngine;
use crate::adapters::store::CandleStore;
use crate::application::mcp_read::indicator_label;
use crate::domain::strategy::VersionId;
use crate::domain::{
    BacktestRunId, BacktestRunRepository, Candle, CandleSeries, CandleSeriesRepository,
    CandleWindow, CompiledValue, DataVersion, EvalContext, HoldoutFreeze, IndicatorSpec, Pair,
    Series, StoredCandleSeries, StrategyRepository, SweepableValue, Timeframe, Trade, compile,
    validate,
};
use crate::tauri::backtest::{dec, direction_label, exit_reason_label, ms};

use super::commands::DesktopState;
use super::error::{BusError, BusErrorCode};

/// How many candles one chart read may return (SPINE Q3/G4).
pub const CHART_BAR_CAP: u32 = 20_000;

/// What `chart_data` is asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ChartDataRequest {
    /// The pair to read (e.g. `BTCUSDT`).
    pub pair: String,
    /// The `Binance` interval text (`15m` / `4h` / `1d`).
    pub timeframe: String,
    /// The window's start, epoch ms as an exact integer string. `null` uses the
    /// run's own start, or the data's when no run is named.
    pub from_ms: Option<String>,
    /// The window's end, epoch ms as an exact integer string. `null` uses the
    /// run's own end, or the latest bar when no run is named.
    pub to_ms: Option<String>,
    /// The backtest run whose trades and overlays to draw. `null` reads candles
    /// only — the pair/timeframe case `r5.s3` marks trades on with no run.
    pub run_id: Option<String>,
}

/// One candle, exactly as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ChartCandleDto {
    /// Bar open time, epoch ms as an exact integer string.
    pub t: String,
    /// Open price, exact decimal string.
    pub o: String,
    /// High price, exact decimal string.
    pub h: String,
    /// Low price, exact decimal string.
    pub l: String,
    /// Close price, exact decimal string.
    pub c: String,
    /// Traded base-asset volume, exact decimal string.
    pub v: String,
}

/// The note that an open freeze cut this read (PDR-0007 / ADR-0028): the holdout
/// start and how many candles the cut removed **inside the resolved window**.
///
/// The resolved window is an explicit `fromMs`/`toMs`, else the run's own recorded
/// window, else — for a read naming no run and no bounds — the whole pinned
/// snapshot. It is NOT the capped slice a default pair read hands back: the count
/// answers "how many of this window's bars did the fence withhold", which is why a
/// read with no window of its own can report bars the returned slice never held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ChartHoldoutDto {
    /// The open freeze's holdout start, epoch ms as an exact integer string.
    pub holdout_start_ms: String,
    /// How many candles the cut removed inside the resolved window.
    pub bars_withheld: u32,
}

/// One pivot event: the confirmation bar, the peak `right` bars before it, and
/// the confirmed price.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ChartPivotEventDto {
    /// The bar where the engine's held pivot value changed — its confirmation.
    pub confirm_time: String,
    /// The peak bar, `right` bars before `confirm_time` (SPINE G3).
    pub peak_time: String,
    /// The confirmed pivot's price, exact decimal string.
    pub price: String,
}

/// One candle-pattern event: the bar it fired on and its signed value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ChartPatternEventDto {
    /// The bar the pattern fired on, epoch ms as an exact integer string.
    pub time: String,
    /// `100` / `-100` (full form) or `80` / `-80` (partial), exact decimal string.
    pub value: String,
}

/// One overlay series, tagged on `kind` (Q4). The `label` is always the token
/// `export_indicators` accepts for the spec its values came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ChartOverlayDto {
    /// An S/R level: one value per window bar, the level as it stood at that bar,
    /// so the chart draws it stepped and never from a later bar (G3).
    Level {
        /// The engine's token for the level's spec.
        label: String,
        /// One per window bar; `null` where the engine reads no value.
        values: Vec<Option<String>>,
    },
    /// An EMA line: one value per window bar, `null` while the engine warms up.
    Line {
        /// The engine's token for the EMA's spec.
        label: String,
        /// One per window bar; `null` while warming up.
        values: Vec<Option<String>>,
    },
    /// A pivot: one event per confirmation.
    Pivot {
        /// The engine's token for the pivot's spec.
        label: String,
        /// The window's confirmations, in bar order.
        events: Vec<ChartPivotEventDto>,
    },
    /// A named candle pattern: one event per bar whose value is non-zero.
    Pattern {
        /// The engine's token for the pattern's spec.
        label: String,
        /// The window's firings, in bar order.
        events: Vec<ChartPatternEventDto>,
    },
}

/// One of the run's stored trades, as the chart marks it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ChartTradeDto {
    /// `long` / `short`.
    pub side: String,
    /// Entry fill time, epoch ms as an exact integer string.
    pub entry_time: String,
    /// Entry fill price, exact decimal string.
    pub entry_price: String,
    /// Exit fill time, epoch ms as an exact integer string; `null` while open.
    pub exit_time: Option<String>,
    /// Exit fill price, exact decimal string; `null` while open.
    pub exit_price: Option<String>,
    /// The stored exit reason (`stop_loss`, `take_profit`, …); `null` while open.
    pub exit_reason: Option<String>,
    /// Realized R multiple, exact decimal string.
    pub realized_r: String,
}

/// The run layer — `null` when the read named no run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ChartRunDto {
    /// The run the layer describes.
    pub run_id: String,
    /// The immutable strategy version whose rule the overlays come from.
    pub version_id: String,
    /// `primary` / `htf` — the series the overlays were computed on (Q2).
    pub series: String,
    /// Every stored trade whose entry or exit falls in the window and which ends
    /// before an open freeze's holdout start (Q5).
    ///
    /// A trade that reaches the fence is withheld WHOLE, even when it entered
    /// before the holdout start: its exit price, exit reason and realized R were
    /// produced inside the withheld period, and the cut withholds those bars.
    pub trades: Vec<ChartTradeDto>,
    /// One overlay per drawn spec of the rule on this series.
    pub overlays: Vec<ChartOverlayDto>,
    /// The labels of the rule's other specs on this series — what the tab says it
    /// does not draw (Q4; PDR-0011).
    pub not_drawn: Vec<String>,
}

/// The chart read's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ChartDataDto {
    /// The pair these candles belong to.
    pub pair: String,
    /// The `Binance` interval text.
    pub timeframe: String,
    /// The snapshot identity the candles came from — what an agent passes as
    /// `data_version` to compare an overlay against `export_indicators` (G2).
    pub data_version: String,
    /// The window's candles, ascending.
    pub candles: Vec<ChartCandleDto>,
    /// The holdout cut, or `null` while no freeze is open.
    pub holdout: Option<ChartHoldoutDto>,
    /// The run layer, or `null` when the read named no run.
    pub run: Option<ChartRunDto>,
}

/// `chart_data`'s transport-free core: one request in, the window (and, when a
/// run is named, its trades and overlays) out.
///
/// # Errors
///
/// Returns a [`BusError`] with [`BusErrorCode::Validation`] for an unreadable
/// field, a bounded window over [`CHART_BAR_CAP`] candles, a version whose stored
/// strategy does not validate or compile, or a `pair`/`timeframe` the run does
/// not use; [`BusErrorCode::NotFound`] for a run id that names nothing;
/// [`BusErrorCode::Data`] when a snapshot cannot be read.
pub async fn chart_data_core(
    state: &DesktopState,
    request: ChartDataRequest,
) -> Result<ChartDataDto, BusError> {
    let pair = Pair::parse(request.pair.clone()).map_err(|e| field("pair", e.to_string()))?;
    let timeframe = parse_timeframe(&request.timeframe).map_err(|e| field("timeframe", e))?;
    let from_ms = parse_window_ms(request.from_ms.as_deref(), "fromMs")?;
    let to_ms = parse_window_ms(request.to_ms.as_deref(), "toMs")?;
    // The open freeze, read ONCE and BEFORE any candle is loaded or stepped:
    // while one is open this read stops at the holdout start (Q5). A read failure
    // is an error, never a silent `None`.
    let holdout = state.open_holdout().await?;
    let store = state.candles();

    let run = match request.run_id.as_deref() {
        Some(run_id) => Some(resolve_run(state, run_id, &pair, timeframe).await?),
        None => None,
    };
    // A bounded read — an explicit window, or a run's own span — is refused over
    // the cap; the plain pair read takes the latest bars up to it (Q3/G4).
    let bounded = run.is_some() || from_ms.is_some() || to_ms.is_some();
    let specs = run.as_ref().map_or_else(Vec::new, |run| run.specs.clone());
    let version = run.as_ref().map(|run| run.data_version.clone());
    let span = run
        .as_ref()
        .map(|run| (run.primary_timeframe, run.primary_data_version.clone()));
    // A windowed run consumed one slice of its pinned snapshot (a walk-forward fold,
    // a windowed backtest, a freeze-clamped run), and that slice is the window a
    // read with no bounds defaults to — the same `[first_open_time_ms,
    // last_close_time_ms]` the run DTO reports, and the reason a small window inside
    // an over-cap snapshot is a request this read can answer.
    let window = run.as_ref().and_then(|run| run.window.clone());

    let read = {
        let store = store.clone();
        let pair = pair.clone();
        match tokio::task::spawn_blocking(move || {
            read_window(
                &store,
                &pair,
                timeframe,
                version.as_ref(),
                span.as_ref().map(|(tf, version)| (*tf, version)),
                window,
                holdout,
                (from_ms, to_ms),
                bounded,
                &specs,
            )
        })
        .await
        {
            Ok(result) => result?,
            Err(e) => {
                return Err(BusError::new(
                    BusErrorCode::Internal,
                    format!("chart read worker failed: {e}"),
                ));
            }
        }
    };

    let trades = match &run {
        Some(run) => {
            let trades = state
                .backtest_run_repo()
                .get_trades(&BacktestRunId::new(&run.run_id))
                .await?;
            trades
                .iter()
                .filter(|trade| trade_in_window(trade, read.from, read.to))
                .filter(|trade| trade_clears_holdout(trade, holdout))
                .map(trade_dto)
                .collect()
        }
        None => Vec::new(),
    };

    Ok(ChartDataDto {
        pair: pair.to_string(),
        timeframe: timeframe.binance_interval().to_owned(),
        data_version: read.data_version.as_str().to_owned(),
        candles: read.candles,
        holdout: holdout.map(|freeze| ChartHoldoutDto {
            holdout_start_ms: ms(freeze.holdout_start_ms),
            bars_withheld: count(read.withheld_in_window),
        }),
        run: run.map(|run| ChartRunDto {
            run_id: run.run_id,
            version_id: run.version_id.as_str().to_owned(),
            series: series_label(run.series).to_owned(),
            trades,
            overlays: read.overlays,
            not_drawn: read.not_drawn,
        }),
    })
}

/// The run layer's resolvable facts: which series, which snapshot, which specs.
struct ResolvedRun {
    /// The run the layer describes.
    run_id: String,
    /// The immutable version whose rule the overlays come from.
    version_id: VersionId,
    /// Which of the run's two series this read draws.
    series: Series,
    /// That series' recorded snapshot identity.
    data_version: DataVersion,
    /// The run's primary timeframe — the run's own window is its primary span.
    primary_timeframe: Timeframe,
    /// The run's primary snapshot identity.
    primary_data_version: DataVersion,
    /// The candle slice the run recorded (`inputs.window`), or `None` for a run
    /// that consumed its whole pinned snapshot. Both legs are read back through
    /// this same slice, so it is the window a read with no bounds defaults to.
    window: Option<CandleWindow>,
    /// The rule's specs on the drawn series, in reference order.
    specs: Vec<IndicatorSpec>,
}

/// Resolve one named run against the requested pair and timeframe (Q1/Q2/Q4):
/// the pair must be the run's, the timeframe one of its two, and the specs are
/// the compiled rule's own for that series.
async fn resolve_run(
    state: &DesktopState,
    run_id: &str,
    pair: &Pair,
    timeframe: Timeframe,
) -> Result<ResolvedRun, BusError> {
    let runs = state.backtest_run_repo();
    let run = runs
        .get_run(&BacktestRunId::new(run_id))
        .await?
        .ok_or_else(|| {
            BusError::new(BusErrorCode::NotFound, format!("no backtest run {run_id}"))
        })?;
    let inputs = run.inputs.clone().ok_or_else(|| {
        BusError::new(
            BusErrorCode::Validation,
            format!("run {run_id} records no provenance, so its series cannot be read"),
        )
    })?;
    if inputs.pair != *pair {
        return Err(field(
            "pair",
            format!("run {run_id} traded {}, not {pair}", inputs.pair),
        ));
    }
    // The primary wins when a run recorded the same timeframe twice (a row only a
    // pre-fix binary could leave).
    let (series, data_version) = if inputs.primary.timeframe == timeframe {
        (Series::Primary, inputs.primary.data_version.clone())
    } else if let Some(htf) = inputs.htf.as_ref().filter(|htf| htf.timeframe == timeframe) {
        (Series::Htf, htf.data_version.clone())
    } else {
        let also = inputs
            .htf
            .as_ref()
            .map(|htf| format!(" and {}", htf.timeframe.binance_interval()))
            .unwrap_or_default();
        return Err(field(
            "timeframe",
            format!(
                "run {run_id} ran {}{also}, not {}",
                inputs.primary.timeframe.binance_interval(),
                timeframe.binance_interval()
            ),
        ));
    };
    let specs = compile_specs(state, &run.strategy_version_id, series).await?;
    Ok(ResolvedRun {
        run_id: run_id.to_owned(),
        version_id: run.strategy_version_id.clone(),
        series,
        data_version,
        primary_timeframe: inputs.primary.timeframe,
        primary_data_version: inputs.primary.data_version.clone(),
        window: inputs.window.clone(),
        specs,
    })
}

/// The rule's specs on one series, from the version's own compiled document
/// (ADR-0024: read only — no DSL and no indicator change).
async fn compile_specs(
    state: &DesktopState,
    version_id: &VersionId,
    series: Series,
) -> Result<Vec<IndicatorSpec>, BusError> {
    let version = state
        .strategy_repo()
        .get_version(version_id)
        .await?
        .ok_or_else(|| {
            BusError::new(
                BusErrorCode::NotFound,
                format!("no strategy version {}", version_id.as_str()),
            )
        })?;
    let validated = validate(&version.dsl).map_err(|e| {
        BusError::new(
            BusErrorCode::Validation,
            format!("the run's strategy does not validate: {e}"),
        )
    })?;
    let compiled = compile(&validated).map_err(|e| {
        BusError::new(
            BusErrorCode::Validation,
            format!("the run's strategy does not compile: {e}"),
        )
    })?;
    Ok(match series {
        Series::Primary => compiled.required_indicators().to_vec(),
        Series::Htf => compiled.required_htf_indicators().to_vec(),
        // The daily series is never drawn (Q4 / out of scope), and no run records
        // it as its primary or HTF timeframe.
        Series::D1 => Vec::new(),
    })
}

/// What one blocking read computed.
struct BlockingRead {
    /// The snapshot the candles came from.
    data_version: DataVersion,
    /// The resolved window's bounds, after the run's span or the series' own
    /// extent filled a missing bound.
    from: i64,
    to: i64,
    /// The window's candles, ascending.
    candles: Vec<ChartCandleDto>,
    /// How many candles the holdout cut removed inside the window.
    withheld_in_window: usize,
    /// One overlay per drawn spec, in reference order.
    overlays: Vec<ChartOverlayDto>,
    /// The labels of the rule's other specs on this series.
    not_drawn: Vec<String>,
}

/// How far a read may draw its series' bars, by `close_time`. `Nothing` is the HTF
/// leg's honest empty answer when the window holds no primary bar the run consumed —
/// distinct from a primary-leg or run-less read, which is simply unrestricted.
enum DrawEdge {
    Unrestricted,
    ClosedBy(i64),
    Nothing,
}

impl DrawEdge {
    fn admits(&self, close_time: i64) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::ClosedBy(close) => close_time <= *close,
            Self::Nothing => false,
        }
    }
}

/// The whole synchronous read — load, cut, slice and step. The cut happens BEFORE
/// the engine's first step and before any window is taken, so no holdout candle
/// reaches either (Q5); the engine then steps the WHOLE cut series from its first
/// candle, so warm-up matches `export_indicators` bar for bar (G2), and only
/// afterwards is the window kept.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // one read in one place; the caller is the core
fn read_window(
    store: &CandleStore,
    pair: &Pair,
    timeframe: Timeframe,
    version: Option<&DataVersion>,
    span: Option<(Timeframe, &DataVersion)>,
    window: Option<CandleWindow>,
    holdout: Option<HoldoutFreeze>,
    requested: (Option<i64>, Option<i64>),
    bounded: bool,
    specs: &[IndicatorSpec],
) -> Result<BlockingRead, BusError> {
    let mut series = load(store, pair, timeframe, version)?.series;
    let data_version = series.version.clone();

    // The run's own consumed bars, when it recorded a window: half-open on opens while
    // this read's `to` is inclusive, so the last consumed bar opens at `to_ms - 1` —
    // the same reading the default bounds below use. Taken before the bounds match
    // moves `window`.
    let consumed = window.as_ref().map(|held| (held.from_ms, held.to_ms - 1));

    // The bounds: an explicit window; else the run's OWN recorded window; else the
    // run's primary span (the run DTO's own definition of the run's range); else the
    // series' own extent.
    let (from, to) = match requested {
        (Some(from), Some(to)) => (from, to),
        (from_ms, to_ms) => {
            let (span_from, span_to) = match window {
                // A persisted window is half-open on the candles' `open_time`
                // (`[from_ms, to_ms)`, `CandleWindow`'s own contract) while this
                // read's `to` is inclusive: the window's last bar is the one
                // opening at `to_ms - 1`. The bars this keeps are exactly the
                // slice the run's read-back answers from, on both legs.
                Some(window) => (window.from_ms, window.to_ms - 1),
                None => match span {
                    Some((span_timeframe, span_version))
                        if span_timeframe == timeframe && Some(span_version) == version =>
                    {
                        span_of(&series)
                    }
                    Some((span_timeframe, span_version)) => {
                        span_of(&load(store, pair, span_timeframe, Some(span_version))?.series)
                    }
                    None => span_of(&series),
                },
            };
            (from_ms.unwrap_or(span_from), to_ms.unwrap_or(span_to))
        }
    };

    // The cap (Q3/G4): a window the caller asked for is refused, never truncated.
    if bounded {
        let held = series
            .candles
            .iter()
            .filter(|candle| (from..=to).contains(&candle.open_time))
            .count();
        if held > CHART_BAR_CAP as usize {
            return Err(field(
                "fromMs",
                format!(
                    "the window holds {held} candles, more than the {CHART_BAR_CAP} bar cap; \
                     narrow fromMs/toMs"
                ),
            ));
        }
    }

    // The holdout cut, counted inside the window before the rows go (Q5).
    let withheld_in_window = match holdout {
        Some(freeze) => series
            .candles
            .iter()
            .filter(|candle| {
                candle.open_time >= freeze.holdout_start_ms
                    && (from..=to).contains(&candle.open_time)
            })
            .count(),
        None => 0,
    };
    if let Some(freeze) = holdout {
        series
            .candles
            .retain(|candle| candle.open_time < freeze.holdout_start_ms);
    }

    // The bars this read may DRAW. A run's HTF leg stops where the run's own feed
    // stopped: `feed.rs` exposes an HTF candle only once it has closed at or before a
    // primary bar's close (G3), and a fold's last primary bar commonly sits inside the
    // HTF candle that carries it — so an open-time-only window would draw that
    // still-forming candle, its complete OHLC and its overlays, values the run never
    // saw. The endpoint is the close of the last primary bar consumed inside BOTH this
    // request's bounds and the run's own `inputs.window`, never the raw `to`: bounds
    // reaching past the run's end expose nothing later, and a window holding no consumed
    // primary bar draws no HTF bar at all. The primary leg, and a read with no run, are
    // unrestricted.
    let edge = match span {
        Some((primary_timeframe, primary_version)) if primary_timeframe != timeframe => {
            let (consumed_from, consumed_to) = consumed.unwrap_or((i64::MIN, i64::MAX));
            let primary = load(store, pair, primary_timeframe, Some(primary_version))?.series;
            match primary.candles.iter().rev().find(|bar| {
                bar.open_time <= to.min(consumed_to) && bar.open_time >= from.max(consumed_from)
            }) {
                Some(bar) => DrawEdge::ClosedBy(bar.close_time),
                None => DrawEdge::Nothing,
            }
        }
        _ => DrawEdge::Unrestricted,
    };

    // The window's bars, as indices into the cut series. An unbounded read keeps
    // the latest bars up to the cap.
    let start = if bounded {
        0
    } else {
        series.candles.len().saturating_sub(CHART_BAR_CAP as usize)
    };
    let window: Vec<usize> = (start..series.candles.len())
        .filter(|&index| {
            let candle = &series.candles[index];
            (from..=to).contains(&candle.open_time) && edge.admits(candle.close_time)
        })
        .collect();

    // The rule's specs: the four drawn kinds get an engine slot; every other spec
    // on this series is a label the tab says it does not draw (Q4).
    let mut drawn: Vec<IndicatorSpec> = Vec::new();
    let mut not_drawn: Vec<String> = Vec::new();
    for spec in specs {
        if drawn_kind(spec).is_some() {
            drawn.push(spec.clone());
        } else {
            not_drawn.push(indicator_label(spec));
        }
    }

    let per_bar = engine_pass(&drawn, &series)?;
    Ok(BlockingRead {
        data_version,
        from,
        to,
        candles: window
            .iter()
            .map(|&index| candle_dto(&series.candles[index]))
            .collect(),
        withheld_in_window,
        overlays: overlay_dtos(&drawn, &per_bar, &series.candles, &window),
        not_drawn,
    })
}

/// One `current(...)` read per bar per spec, over the WHOLE cut series — the
/// `export_indicators` loop, so every overlay value equals what an agent sees for
/// the same token, timeframe and data version (G2).
fn engine_pass(
    specs: &[IndicatorSpec],
    series: &CandleSeries,
) -> Result<Vec<Vec<Option<Decimal>>>, BusError> {
    let mut engine = IndicatorEngine::from_specs(specs).map_err(|e| {
        BusError::new(
            BusErrorCode::Validation,
            format!("the rule's indicators cannot run over this series: {e}"),
        )
    })?;
    let mut per_bar: Vec<Vec<Option<Decimal>>> =
        vec![Vec::with_capacity(series.candles.len()); specs.len()];
    for candle in &series.candles {
        engine.step(candle);
        for (slot, spec) in specs.iter().enumerate() {
            per_bar[slot].push(engine.current(&CompiledValue::Indicator {
                series: Series::Primary,
                spec: spec.clone(),
                lag: 0,
            }));
        }
    }
    Ok(per_bar)
}

/// Which drawn family a spec belongs to, with a pivot's `right` span (the bars
/// between a pivot's peak and its confirmation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drawn {
    /// `support:` / `resistance:` — a per-bar level.
    Level,
    /// `ema:` — a per-bar line.
    Line,
    /// `pivot_high:` / `pivot_low:` — events on confirmation bars.
    Pivot(u32),
    /// `cdl:` — events on the bars a pattern fired.
    Pattern,
}

/// The drawn family of a spec, or `None` for everything the tab does not draw. A
/// swept period has no engine value and no token (validation rejects every sweep
/// before this point), so it is not drawn.
fn drawn_kind(spec: &IndicatorSpec) -> Option<Drawn> {
    match spec {
        IndicatorSpec::Support { .. } | IndicatorSpec::Resistance { .. } => Some(Drawn::Level),
        IndicatorSpec::Ema { .. } => Some(Drawn::Line),
        IndicatorSpec::PivotHigh { right, .. } | IndicatorSpec::PivotLow { right, .. } => {
            fixed_period(right).map(Drawn::Pivot)
        }
        IndicatorSpec::CandlePattern { .. } => Some(Drawn::Pattern),
        _ => None,
    }
}

/// One overlay DTO per drawn spec, from that spec's per-bar engine values.
fn overlay_dtos(
    specs: &[IndicatorSpec],
    per_bar: &[Vec<Option<Decimal>>],
    candles: &[Candle],
    window: &[usize],
) -> Vec<ChartOverlayDto> {
    let mut overlays = Vec::with_capacity(specs.len());
    for (slot, spec) in specs.iter().enumerate() {
        let values = per_bar.get(slot).map_or(&[][..], Vec::as_slice);
        let label = indicator_label(spec);
        let series: Vec<Option<String>> = window
            .iter()
            .map(|&index| values.get(index).copied().flatten().map(dec))
            .collect();
        match drawn_kind(spec) {
            Some(Drawn::Level) => overlays.push(ChartOverlayDto::Level {
                label,
                values: series,
            }),
            Some(Drawn::Line) => overlays.push(ChartOverlayDto::Line {
                label,
                values: series,
            }),
            Some(Drawn::Pivot(right)) => overlays.push(ChartOverlayDto::Pivot {
                label,
                events: pivot_events(values, candles, window, right),
            }),
            Some(Drawn::Pattern) => overlays.push(ChartOverlayDto::Pattern {
                label,
                events: pattern_events(values, candles, window),
            }),
            None => {}
        }
    }
    overlays
}

/// A pivot's confirmations inside the window: one event on each bar where the
/// engine's held value changes, priced, with the peak bar `right` bars earlier
/// (G3). A confirmation that repeats the held price leaves no observable change
/// in the engine's public per-bar series, so it emits no event — recorded as this
/// item's known limit.
fn pivot_events(
    values: &[Option<Decimal>],
    candles: &[Candle],
    window: &[usize],
    right: u32,
) -> Vec<ChartPivotEventDto> {
    let back = usize::try_from(right).unwrap_or(usize::MAX);
    let mut events = Vec::new();
    for &index in window {
        let Some(value) = values.get(index).copied().flatten() else {
            continue;
        };
        let previous = index
            .checked_sub(1)
            .and_then(|earlier| values.get(earlier).copied().flatten());
        if previous == Some(value) {
            continue;
        }
        let (Some(confirm), Some(peak)) =
            (candles.get(index), candles.get(index.saturating_sub(back)))
        else {
            continue;
        };
        events.push(ChartPivotEventDto {
            confirm_time: ms(confirm.open_time),
            peak_time: ms(peak.open_time),
            price: dec(value),
        });
    }
    events
}

/// One event per window bar whose pattern value is non-zero (`±100` / `±80`).
fn pattern_events(
    values: &[Option<Decimal>],
    candles: &[Candle],
    window: &[usize],
) -> Vec<ChartPatternEventDto> {
    let mut events = Vec::new();
    for &index in window {
        let Some(value) = values.get(index).copied().flatten() else {
            continue;
        };
        if value.is_zero() {
            continue;
        }
        let Some(candle) = candles.get(index) else {
            continue;
        };
        events.push(ChartPatternEventDto {
            time: ms(candle.open_time),
            value: dec(value),
        });
    }
    events
}

/// One stored candle as the chart draws it.
fn candle_dto(candle: &Candle) -> ChartCandleDto {
    ChartCandleDto {
        t: ms(candle.open_time),
        o: dec(candle.open),
        h: dec(candle.high),
        l: dec(candle.low),
        c: dec(candle.close),
        v: dec(candle.volume),
    }
}

/// Whether a stored trade's entry OR exit falls in the window — the spec's own
/// filter: either end of a trade draws it.
fn trade_in_window(trade: &Trade, from: i64, to: i64) -> bool {
    (from..=to).contains(&trade.entry_fill_time) || (from..=to).contains(&trade.exit_fill_time)
}

/// Whether a stored trade clears the open freeze (Q5): BOTH of its fills before
/// the holdout start. A trade that entered before the fence and exited inside it
/// carries an outcome produced in the withheld period — its exit price, its exit
/// reason and its realized R — so the cut withholds it whole, exactly as it
/// withholds the candles the position ran through. `None` (no freeze open, or the
/// certification exemption) clears every trade.
fn trade_clears_holdout(trade: &Trade, holdout: Option<HoldoutFreeze>) -> bool {
    holdout.is_none_or(|freeze| {
        trade.entry_fill_time < freeze.holdout_start_ms
            && trade.exit_fill_time < freeze.holdout_start_ms
    })
}

/// One stored trade as the chart marks it — the run's values, unchanged. A stored
/// trade is closed by construction (an open position is the run's own
/// `open_position`, never a trade row), so the exit fields are present; they cross
/// as `Option` because the wire shape the tab draws allows an open one.
fn trade_dto(trade: &Trade) -> ChartTradeDto {
    ChartTradeDto {
        side: direction_label(trade.direction).to_owned(),
        entry_time: ms(trade.entry_fill_time),
        entry_price: dec(trade.entry_price),
        exit_time: Some(ms(trade.exit_fill_time)),
        exit_price: Some(dec(trade.exit_price)),
        exit_reason: Some(exit_reason_label(trade.exit_reason).to_owned()),
        realized_r: dec(trade.realized_r),
    }
}

/// Load the pinned snapshot when one is named, else HEAD.
fn load(
    store: &CandleStore,
    pair: &Pair,
    timeframe: Timeframe,
    version: Option<&DataVersion>,
) -> Result<StoredCandleSeries, BusError> {
    match version {
        Some(version) => store
            .load_version(pair, timeframe, version)
            .map_err(|e| BusError::new(BusErrorCode::Data, e.to_string())),
        None => store.load_head(pair, timeframe)?.ok_or_else(|| {
            BusError::new(
                BusErrorCode::Validation,
                format!(
                    "no HEAD snapshot for {pair} {}",
                    timeframe.binance_interval()
                ),
            )
        }),
    }
}

/// A series' own `[first open, last close]`; an empty series spans the whole line.
fn span_of(series: &CandleSeries) -> (i64, i64) {
    (
        series
            .candles
            .first()
            .map_or(i64::MIN, |candle| candle.open_time),
        series
            .candles
            .last()
            .map_or(i64::MAX, |candle| candle.close_time),
    )
}

/// The wire word for a series.
fn series_label(series: Series) -> &'static str {
    match series {
        Series::Primary => "primary",
        Series::Htf => "htf",
        Series::D1 => "d1",
    }
}

/// The `Fixed` period of a spec leaf, or `None` for an (unreachable) sweep.
fn fixed_period(value: &SweepableValue<u32>) -> Option<u32> {
    match value {
        SweepableValue::Fixed(period) => Some(*period),
        SweepableValue::Sweep { .. } => None,
    }
}

/// The one token grammar this read and the CLI share: the `Binance` intervals.
fn parse_timeframe(raw: &str) -> Result<Timeframe, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "15m" => Ok(Timeframe::M15),
        "4h" => Ok(Timeframe::H4),
        "1d" => Ok(Timeframe::D1),
        other => Err(format!(
            "unknown timeframe {other:?} (expected 15m, 4h, or 1d)"
        )),
    }
}

/// A window bound: an exact millisecond integer as text, like every time on this
/// wire.
fn parse_window_ms(raw: Option<&str>, field_name: &str) -> Result<Option<i64>, BusError> {
    raw.map(|text| {
        text.trim().parse::<i64>().map_err(|e| {
            field(
                field_name,
                format!("{field_name} must be an epoch-millisecond integer: {e}"),
            )
        })
    })
    .transpose()
}

/// A 422 naming the field it refused.
fn field(field_name: &str, message: impl std::fmt::Display) -> BusError {
    BusError::new(BusErrorCode::Validation, format!("{field_name}: {message}"))
}

/// A count as the wire's `u32` — specta refuses 64-bit integers.
fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
