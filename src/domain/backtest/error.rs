//! `BacktestError` — the backtester's domain error taxonomy.
//!
//! Mirrors the [`DataError`](crate::domain::DataError) style: `thiserror`-derived
//! for ergonomic `Display`/`Error`, `serde`-serializable so errors can cross the
//! `Tauri` boundary later, and `#[non_exhaustive]` so the loop (1.03) and CLI
//! (1.04) can extend it **additively** without a breaking rewrite. No library
//! path panics: the crate denies `clippy::unwrap_used` / `expect_used`.
//!
//! Two variants land in this work item (1.01):
//! - [`BacktestError::NoStopLoss`] (G5 / issue #20) — a zero stop-distance
//!   (`entry == stop`) has no risk denominator, so sizing refuses rather than
//!   dividing by zero or inventing a fallback. The loop (1.03) also raises it as
//!   a precondition when a compiled strategy carries no `StopLoss` exit.
//! - [`BacktestError::UnsupportedExit`] (C4) — the fail-fast refusal for an exit
//!   kind the backtester does not model. `TrailingStop` / `TimeStop` were the
//!   examples when this variant landed (1.01); r3.s1.w1 made both REAL exits
//!   (G1/G2), so nothing constructs this variant any more. It stays because the
//!   enum serde round-trips across the `Tauri` boundary.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{Pair, Timeframe};

/// Which input series an input-validation refusal names (r3.s1.w3; `D1`
/// since r3.s2.w4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeriesRole {
    /// The run's primary series.
    Primary,
    /// The supplied higher-timeframe series.
    Htf,
    /// The supplied fixed daily series (r3.s2.w4).
    D1,
}

/// Errors produced by the backtester (domain layer).
///
/// `#[non_exhaustive]` so 1.03/1.04 can add variants additively (the shared file
/// never needs a rewrite). serde round-trips for the later `Tauri` boundary.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[non_exhaustive]
pub enum BacktestError {
    /// Sizing has no risk denominator: the entry and stop prices are equal
    /// (`entry == stop`, a zero stop-distance), or the compiled strategy carries
    /// no `StopLoss` exit at all. No fallback sizing (G5 / #20).
    #[error("cannot size a position without a stop-loss (zero stop distance)")]
    NoStopLoss,

    /// A compiled exit kind this backtester does not model. **No exit kind
    /// constructs this any more**: `TrailingStop` and `TimeStop` were the
    /// examples when it landed (1.01, C4), and r3.s1.w1 made both real exits
    /// (G1/G2). The variant is retained because the enum serde round-trips
    /// across the `Tauri` boundary — a payload written by an older build must
    /// still deserialize.
    #[error("unsupported exit kind for this backtester: {0}")]
    UnsupportedExit(String),

    /// The streaming indicator engine could not be constructed for the compiled
    /// strategy (e.g. a non-fixed or invalid indicator spec). A construction
    /// failure is neither a missing stop nor an unsupported exit — it gets its
    /// own neutral category so the cause is not mislabelled.
    #[error("indicator engine initialization failed: {0}")]
    EngineInit(String),

    /// A short strategy's take-profit geometry resolves to a non-positive price
    /// (`target_r × stop_distance_pct ≥ 1`, so `entry × (1 − target_r ×
    /// stop_distance_pct) ≤ 0`). Such a target can never be reached by positive
    /// market data, so the loop rejects it fail-fast rather than silently
    /// behaving as if no take-profit were set.
    #[error("impossible take-profit geometry: {0}")]
    ImpossibleTakeProfit(String),

    /// The resolved stop leaves untradeable geometry: a non-positive price
    /// (`multiple × ATR ≥ entry` on a long, so `entry − multiple × ATR ≤ 0`)
    /// or a zero stop distance (`stop == entry`, a flat series driving the
    /// frozen ATR to exactly 0). A zero distance would collapse into the
    /// generic `NoStopLoss` and a negative one would size off its absolute
    /// distance while never being fillable — both silently wrong — so the
    /// fill refuses at the seam where the stop is derived, for every stop
    /// kind, mirroring [`BacktestError::ImpossibleTakeProfit`] (r2.s2 round-1
    /// fix F4; the zero-distance leg and the hoist out of the `Atr` arm are
    /// r2.s2 round-3).
    #[error("impossible ATR stop: {0}")]
    ImpossibleStop(String),

    /// The cost/equity configuration is out of range — non-positive starting
    /// equity (the sizing denominator) or a fee/slippage rate outside `[0, 100%)`.
    /// Enforced at the engine boundary so a non-CLI caller cannot feed the
    /// sizing/fill math nonsensical inputs.
    #[error("invalid backtest configuration: {0}")]
    InvalidConfig(String),

    /// The compiled strategy references a `series: "htf"` operand but no
    /// higher-timeframe candle series was supplied (schema 1.1.0, r2.s2.w2).
    /// The engine raises this rather than silently evaluating an `Htf` leaf
    /// against primary data; the application ring checks it first and reports
    /// the missing input field.
    #[error("strategy requires a higher-timeframe candle series (series: \"htf\" operand present)")]
    HtfRequired,

    /// The supplied higher-timeframe series is not strictly higher than the
    /// primary series (`htf.duration_ms() <= primary.duration_ms()`). An equal
    /// or lower interval would advance `Series::Htf` operands on the wrong
    /// cadence while the DSL renders them as the HTF — silently wrong signals.
    /// The request boundary refuses this before any candle I/O (r2.s2 round-1
    /// fix F1); this arm is the engine-level defence for callers that
    /// construct the series directly.
    #[error(
        "higher-timeframe series {htf:?} is not higher than the primary series {primary:?} — \
         `Series::Htf` operands need a strictly longer timeframe"
    )]
    HtfNotHigher {
        /// The primary series' timeframe.
        primary: Timeframe,
        /// The supplied higher-timeframe series' timeframe.
        htf: Timeframe,
    },

    /// The supplied higher-timeframe series is for a different trading pair
    /// than the primary series. `CandleSeries::pair` is public and
    /// `run_backtest` takes the two series independently, so without this
    /// check a direct caller could produce mixed-symbol signals with nothing
    /// red — the engine steps `htf.candles` and routes `Series::Htf` leaves to
    /// it regardless (r2.s2 round-2 fix G1). The application path loads both
    /// series by the request's pair, so this arm is the whole API-seam guard.
    #[error(
        "higher-timeframe series is for a different pair (primary {primary}, htf {htf}) — \
         `Series::Htf` operands must read the same symbol's bars"
    )]
    HtfPairMismatch {
        /// The primary series' trading pair.
        primary: Pair,
        /// The supplied higher-timeframe series' trading pair.
        htf: Pair,
    },

    /// The supplied higher-timeframe series ends more than one HTF interval
    /// before the primary series ends. `align` advances its HTF pointer
    /// forward-only and never clears it, so once the HTF candles are exhausted
    /// every later primary bar would still pair with the FINAL one — `Series::Htf`
    /// operands reading a frozen, stale bar for the rest of the run: silent
    /// wrong trades, not a loud failure. One interval of slack is allowed —
    /// at most one not-yet-closed HTF bar may be pending, the normal live
    /// shape — and an empty HTF series is skipped (legal per r2.s1.w3: `align`
    /// then yields `htf: None` per bar and the paired-bar gate closes entries
    /// outright). The refusal lives in `check_htf_inputs`, the seam that owns
    /// the other HTF input invariants, and applies only when the compiled
    /// strategy consumes the HTF series (`needs_htf`) — a primary-only
    /// strategy is handed the default-resolved H4 snapshot but never reads it,
    /// so a lagging H4 HEAD must not refuse its run (r2.s2 round-5; the
    /// `needs_htf` gate is round-6).
    #[error(
        "higher-timeframe coverage ends at close_time {htf_end}, more than one {htf:?} interval \
         before the primary series ends at {primary_end} — `Series::Htf` operands would read \
         a stale final bar"
    )]
    HtfCoverageShort {
        /// The primary series' last candle `close_time` (epoch ms).
        primary_end: i64,
        /// The supplied higher-timeframe series' last candle `close_time` (epoch ms).
        htf_end: i64,
        /// The supplied higher-timeframe series' timeframe — the interval the
        /// slack is measured in.
        htf: Timeframe,
    },

    /// The compiled strategy references a `series: "d1"` operand but no daily
    /// candle series was supplied (r3.s2.w4). The engine-level defence for
    /// direct callers; the application ring loads the D1 series first and
    /// reports a missing snapshot as the app-level `D1Required` naming the
    /// fetch command.
    #[error("strategy requires a daily candle series (series: \"d1\" operand present)")]
    D1Required,

    /// The supplied daily series is for a different trading pair than the
    /// primary series — the same API-seam guard as
    /// [`BacktestError::HtfPairMismatch`], for the fixed `d1` series
    /// (r3.s2.w4): `Series::D1` operands must never read another symbol's
    /// bars.
    #[error(
        "daily series is for a different pair (primary {primary}, d1 {d1}) — \
         `Series::D1` operands must read the same symbol's bars"
    )]
    D1PairMismatch {
        /// The primary series' trading pair.
        primary: Pair,
        /// The supplied daily series' trading pair.
        d1: Pair,
    },

    /// The supplied daily series ends more than one D1 interval before the
    /// primary series ends — the same forward-only-pointer staleness as
    /// [`BacktestError::HtfCoverageShort`], for the fixed `d1` series
    /// (r3.s2.w4). One interval of slack is allowed (at most one not-yet-closed
    /// daily bar, the normal live shape) and an empty series is skipped; the
    /// refusal applies only when the compiled strategy consumes the daily
    /// series (`needs_d1`).
    #[error(
        "daily coverage ends at close_time {d1_end}, more than one D1 interval before the \
         primary series ends at {primary_end} — `Series::D1` operands would read a stale \
         final bar"
    )]
    D1CoverageShort {
        /// The primary series' last candle `close_time` (epoch ms).
        primary_end: i64,
        /// The supplied daily series' last candle `close_time` (epoch ms).
        d1_end: i64,
    },

    /// A named input series is not strictly ascending by `open_time`, or
    /// repeats an `open_time` (`CandleSeries::validate`'s `Unsorted` /
    /// `Duplicate`). The engine would align, signal and fill on the candles
    /// in whatever order they arrive — wrong money on input it was handed —
    /// so the run refuses before any computation (r3.s1.w3). `at` names the
    /// offending `open_time`: the out-of-order candle's, or the repeated one's.
    #[error(
        "backtest input series {series:?} is not strictly ascending by open_time \
         (offending open_time {at})"
    )]
    SeriesUnsorted {
        /// Which series failed validation.
        series: SeriesRole,
        /// The offending candle's `open_time` (epoch ms) — out of order or
        /// duplicated.
        at: i64,
    },

    /// A named input series has a missing candle: adjacent spacing exceeds one
    /// timeframe duration (`CandleSeries::validate`'s first reported gap). A
    /// gapped series would bar-gate indicators, fills and funding events on
    /// phantom time — the run refuses with the gap's expected and found
    /// `open_time` rather than computing across the hole (r3.s1.w3).
    #[error(
        "backtest input series {series:?} has a gap: expected open_time {expected}, found {found}"
    )]
    SeriesGap {
        /// Which series failed validation.
        series: SeriesRole,
        /// The `open_time` the next candle was expected at (epoch ms).
        expected: i64,
        /// The `open_time` actually found (epoch ms).
        found: i64,
    },

    /// The run's counted span crosses an 8h funding boundary with no funding
    /// stamp on (or before) the candle containing it (`funding_gaps`' first
    /// uncovered segment). The funding fold counts only candles that CARRY a
    /// rate, so a missed event would silently accrue as zero and misstate
    /// every trade it touches — the run refuses with the uncovered segment's
    /// anchors instead (#45, r3.s1.w3). `from` is the previous stamp's
    /// `open_time` (or the counted span's first counted `open_time`); `to` is
    /// the next stamp's (or the last primary candle's `close_time`).
    #[error(
        "funding-order precondition violated: no funding stamp within one interval \
         of the ({from}, {to}] span segment — a missed 8h event would accrue as zero"
    )]
    FundingGap {
        /// The uncovered segment's earlier anchor (epoch ms).
        from: i64,
        /// The uncovered segment's later anchor (epoch ms).
        to: i64,
    },

    /// No funding interval is pinned for the run's pair — the engine refuses
    /// rather than defaulting an ordering precondition it cannot know
    /// (r3.s1.w3; the interval is pinned per-pair on `BinanceAdapter`, the
    /// same home as the symbol filters).
    #[error("no funding interval pinned for pair {pair}")]
    FundingIntervalUnknown {
        /// The pair with no pinned funding interval.
        pair: Pair,
    },

    /// A named input series failed validation with an error variant
    /// `CandleSeries::validate` cannot construct today (it raises only
    /// `Unsorted`/`Duplicate`, both mapped to [`BacktestError::SeriesUnsorted`]
    /// above). A defensive catch-all so a future `DataError` variant cannot
    /// silently pass the input guard — following the `MutationError::
    /// CompileFailed` / `UnexpectedSweep` precedent: documented unreachable,
    /// part of the seam's contract, never reached by a test that bypasses the
    /// production path (r3.s1.w3).
    #[error("backtest input series {series:?} failed validation: {message}")]
    SeriesUnreadable {
        /// Which series failed validation.
        series: SeriesRole,
        /// The underlying validation failure's display.
        message: String,
    },
}
