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
//! - [`BacktestError::UnsupportedExit`] (C4) — `TrailingStop` / `TimeStop` exits
//!   are not modelled this slice; 1.03 fail-fast rejects them with this variant
//!   rather than silently mis-pricing them.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::Timeframe;

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

    /// A compiled exit kind this slice does not model was encountered —
    /// `TrailingStop` or `TimeStop`. 1.03 rejects it fail-fast rather than
    /// mis-pricing it (C4).
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

    /// An ATR-derived stop resolved to a non-positive price (`multiple × ATR ≥
    /// entry` on a long, so `entry − multiple × ATR ≤ 0`). A zero stop would
    /// collapse into the generic `NoStopLoss` and a negative one would size off
    /// its absolute distance while never being fillable — both silently wrong —
    /// so the loop refuses at the seam where the stop is derived, mirroring
    /// [`BacktestError::ImpossibleTakeProfit`] (r2.s2 round-1 fix F4).
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
}
