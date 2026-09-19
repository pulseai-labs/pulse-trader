//! `backtest` (pure domain) — the money-math + value-type foundation of the
//! deterministic backtester (VS-1.2.1, FR-5 / FR-6, BACKLOG-4).
//!
//! This tree is **zero-I/O, `Decimal`-only** (NFR-2): the immutable trade-record
//! types ([`Trade`] / [`Fill`] / [`ExitReason`] / [`TradeSource`]), the run
//! aggregate ([`BacktestResult`]), the error taxonomy ([`BacktestError`]), the
//! pure cost / P&L / collision / sizing primitives the event loop (1.03)
//! composes, and the MTF-aligned, no-look-ahead candle [`feed`] (work-1.02) over
//! already-loaded [`CandleSeries`](crate::domain::CandleSeries). The concrete
//! loop + `IndicatorEngine` orchestration lives in `adapters::backtest` (gate-2
//! amendment S1); nothing here iterates candles, reads indicators, or persists.

// work-1.01: pure money-math + entities.
mod collision;
mod cost;
mod error;
mod result;
mod trade;
// work-1.02: the MTF-aligned, no-look-ahead candle feed.
mod feed;
// VS-1.2.2 work-2.03: the pure regime value types + classification (EMA50/200 +
// ADX14). The stateful detector that holds the indicator adapters lives in
// `adapters::backtest::regime`; this is the zero-I/O, `Decimal`-only half.
mod regime;
// VS-1.2.4 work-4.01: the derived read-only SummaryStats + equity curve (FR-6 /
// NFR-2). Pure Decimal/usize folds over the already-final trade log + run totals;
// oracle-excluded (README C3) so the frozen baseline stays frozen by construction.
mod stats;
// VS-1.2.4 work-4.04: the persisted backtest-run projection types (FR-6 / FR-7 /
// NFR-2). `BacktestRunId`/`PersistedRun`/`RunSummary` are the typed read-back
// projections of a persisted run (explicit columns per README C4, NOT a
// `BacktestResult` blob — #68 / D1). `SqliteBacktestRunRepo` writes/reads them.
mod run;

// Re-exports kept at the `domain::backtest` surface so `domain/mod.rs` + `lib.rs`
// can curate them onto the crate's public API (an un-re-exported public domain
// type is a `dead_code` BUILD error under `deny(warnings)`).
pub use collision::{IntraBarExit, resolve_intra_bar_exit};
pub use cost::{Side, apply_slippage, funding_payment, realized_pnl, realized_r, taker_fee};
pub use error::BacktestError;
pub use feed::{AlignedBar, align};
pub use regime::{ADX_TREND_THRESHOLD, Regime, RegimeBreakdown, RegimeCell, classify};
pub use result::BacktestResult;
pub use run::{
    BacktestInputs, BacktestRunId, CandleWindow, CandleWindowError, FundingConfig,
    OpenPositionMark, PersistedRun, RunSummary, SeriesEnd, SnapshotSelection,
};
pub use stats::{EquityCurve, EquityPoint, SummaryStats};
pub use trade::{ExitReason, Fill, Trade, TradeSource};
