//! `Direction` and `RiskParams` — the trade-side and risk vocabulary of a
//! strategy.
//!
//! [`Direction`] is a unit enum serialized lowercase (matching the `Timeframe`
//! serde-rename style). v1 is single-direction per strategy; "both" is out of
//! scope. [`RiskParams`] is the minimal sizing-input set (grill branch 3):
//! `risk_per_trade_pct` (with a stop distance, determines position size per the
//! NFR-3 identity downstream) and `max_leverage` (a sweepable cap that changes
//! backtest results). No sizing-model enum (one v1 model), no max-positions (v1
//! single position), and explicitly OUT: drawdown/kill-switch (v3), fee/slippage
//! (cost-model config), account size (runtime). Position-sizing **math** is the
//! backtester's job (Sprint 1.2), not this grammar.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::sweepable::SweepableValue;

/// The trade side a strategy takes. v1 is single-direction per strategy.
///
/// Serialized lowercase (`"long"` / `"short"`) to match the `Timeframe`
/// serde-rename style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// A long (buy) strategy.
    Long,
    /// A short (sell) strategy.
    Short,
}

/// The risk / sizing inputs of a strategy.
///
/// A plain (untagged) struct. Minimal by design — the two fields that change
/// backtest results and are sweepable. Position-sizing math is the backtester's
/// (Sprint 1.2); these are declarative inputs only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RiskParams {
    /// Fraction of account equity risked per trade as a **decimal fraction**
    /// (`0.01` = 1%), NOT a percentage-point number. With a `StopLoss` distance
    /// this determines position size (the NFR-3 sizing identity, computed
    /// downstream).
    pub risk_per_trade_pct: SweepableValue<Decimal>,
    /// Maximum leverage cap (a plain multiplier, e.g. `3` = 3x). In the DSL
    /// because it changes backtest results and is sweepable.
    pub max_leverage: SweepableValue<Decimal>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Direction, RiskParams};
    use crate::domain::dsl::sweepable::SweepableValue;
    use rust_decimal::Decimal;

    #[test]
    fn direction_serializes_lowercase() {
        let json = serde_json::to_string(&Direction::Long).expect("serialize Long");
        assert_eq!(json, "\"long\"");
        let back: Direction = serde_json::from_str(&json).expect("deserialize Long");
        assert_eq!(back, Direction::Long);

        let json = serde_json::to_string(&Direction::Short).expect("serialize Short");
        assert_eq!(json, "\"short\"");
    }

    #[test]
    fn risk_params_round_trips() {
        let r = RiskParams {
            // 0.01 = 1% (decimal-fraction convention).
            risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
            max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
        };
        let json = serde_json::to_string(&r).expect("serialize RiskParams");
        let back: RiskParams = serde_json::from_str(&json).expect("deserialize RiskParams");
        assert_eq!(back, r);
    }
}
