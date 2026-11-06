//! Deterministic backtest event loop adapter.

mod engine;
// VS-1.2.2 work-2.03: the stateful regime detector (EMA50/200 + ADX14),
// composing the VS-1.1.3 `Ema`/`Adx` adapters independently of the strategy's
// `IndicatorEngine`. Pure value types + classification live in
// `domain::backtest::regime`.
mod regime;
pub mod session;
// r6.s2.w2 (ADR-0033): the one-sided setup state machine.
mod setup;

pub use engine::{
    BacktestConfig, FundingGapWindow, first_fully_warm_bar_ms, funding_gaps, run_backtest,
};
pub use regime::RegimeDetector;
pub use session::{
    EngineSession, EvalProbe, SessionTimeframes, SetupView, indicator_values_at, step_series,
};
pub use setup::{SetupEvent, SetupFire, SetupPhase, SideState};
