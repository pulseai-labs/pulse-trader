//! Deterministic backtest event loop adapter.

mod engine;
// VS-1.2.2 work-2.03: the stateful regime detector (EMA50/200 + ADX14),
// composing the VS-1.1.3 `Ema`/`Adx` adapters independently of the strategy's
// `IndicatorEngine`. Pure value types + classification live in
// `domain::backtest::regime`.
mod regime;

pub use engine::{BacktestConfig, first_fully_warm_bar_ms, run_backtest};
pub use regime::RegimeDetector;
