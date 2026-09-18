//! `ExitRule` — the exit vocabulary of a strategy. Any rule in a strategy's
//! `exits` list that triggers closes the position.
//!
//! Internally-tagged (`#[serde(tag = "type")]`) with **all struct variants** —
//! the 2.01 serde invariant: serde cannot serialize an internally-tagged
//! tuple/newtype variant. Stops are **percent-based** and the take-profit target
//! is an **R-multiple** of the stop distance (grill branch 1 — R:R is the
//! project's native risk vocabulary).
//!
//! No ATR-based stops in v1 (ATR isn't in the 2.01 indicator catalog). No
//! evaluation or money math here — these are declarative grammar only; the
//! backtester computes `1R = entry_price × distance_pct` (Sprint 1.2). Semantic
//! rules (≥1 exit; no duplicate exclusive exits; a `TakeProfit` requires a
//! `StopLoss`) are **2.03's**, recorded in the slice README contract.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::condition::Condition;
use super::sweepable::SweepableValue;

/// A single exit rule. A strategy's `exits` list may hold several; any that
/// triggers closes the position.
///
/// Internally-tagged (`#[serde(tag = "type")]`) with **all struct variants** —
/// see the module docs for why tuple/newtype variants are forbidden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum ExitRule {
    /// A fixed stop-loss a percentage distance from entry. This stop distance
    /// feeds position-sizing (NFR-3) and **defines 1R** for the strategy.
    StopLoss {
        /// Stop distance from entry as a **decimal fraction** (`0.05` = 5%), NOT
        /// a percentage-point number.
        distance_pct: SweepableValue<Decimal>,
    },
    /// A take-profit target expressed as an **R-multiple** of the stop distance.
    TakeProfit {
        /// Target as a plain R-multiple (`2.0` = 2R), NOT a fraction. Undefined
        /// without a [`StopLoss`](ExitRule::StopLoss) in the same strategy
        /// (enforced in 2.03).
        target_r: SweepableValue<Decimal>,
    },
    /// A trailing stop a percentage distance behind the favourable extreme.
    TrailingStop {
        /// Trail distance as a **decimal fraction** (`0.05` = 5%), NOT a
        /// percentage-point number.
        trail_pct: SweepableValue<Decimal>,
    },
    /// A time-based exit after a maximum number of bars in the trade.
    TimeStop {
        /// Maximum number of bars to hold before closing.
        max_bars: SweepableValue<u32>,
    },
    /// Close when a [`Condition`] becomes true (e.g. `RSI > 70`).
    SignalExit {
        /// The condition that, when true, closes the position.
        condition: Condition,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::ExitRule;
    use crate::domain::dsl::condition::{Comparator, Condition};
    use crate::domain::dsl::sweepable::SweepableValue;
    use crate::domain::dsl::value::{IndicatorSpec, ValueSource};
    use rust_decimal::Decimal;

    fn round_trip(e: &ExitRule) -> ExitRule {
        let json = serde_json::to_string(e).expect("serialize ExitRule");
        serde_json::from_str(&json).expect("deserialize ExitRule")
    }

    #[test]
    fn stop_loss_round_trips() {
        let e = ExitRule::StopLoss {
            // 0.05 = 5% (decimal-fraction convention).
            distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
        };
        assert_eq!(round_trip(&e), e);
    }

    #[test]
    fn take_profit_round_trips() {
        let e = ExitRule::TakeProfit {
            // 2.0 = 2R (plain R-multiple).
            target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
        };
        assert_eq!(round_trip(&e), e);
    }

    #[test]
    fn trailing_stop_round_trips() {
        let e = ExitRule::TrailingStop {
            trail_pct: SweepableValue::Fixed(Decimal::new(3, 2)),
        };
        assert_eq!(round_trip(&e), e);
    }

    #[test]
    fn time_stop_round_trips() {
        let e = ExitRule::TimeStop {
            max_bars: SweepableValue::Fixed(48),
        };
        assert_eq!(round_trip(&e), e);
    }

    #[test]
    fn signal_exit_round_trips() {
        let e = ExitRule::SignalExit {
            condition: Condition::Compare {
                lhs: ValueSource::Indicator {
                    spec: IndicatorSpec::Rsi {
                        period: SweepableValue::Fixed(14),
                    },
                },
                op: Comparator::Gt,
                rhs: ValueSource::Constant {
                    value: Decimal::new(70, 0),
                },
            },
        };
        assert_eq!(round_trip(&e), e);
    }

    #[test]
    fn exit_rule_serializes_with_struct_tag() {
        let e = ExitRule::StopLoss {
            distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
        };
        let json = serde_json::to_string(&e).expect("serialize StopLoss");
        assert!(json.contains("\"type\":\"StopLoss\""), "json was: {json}");
    }

    #[test]
    fn unknown_exit_type_is_err() {
        let bad: Result<ExitRule, _> = serde_json::from_str(r#"{"type":"Bogus"}"#);
        assert!(bad.is_err(), "unknown ExitRule type must be rejected");
    }
}
