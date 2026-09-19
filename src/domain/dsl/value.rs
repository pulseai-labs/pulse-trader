//! `ValueSource` — where a scalar value in a [`Condition`](super::Condition)
//! comes from, plus its supporting leaf types ([`PriceField`],
//! [`IndicatorSpec`]).
//!
//! `ValueSource` is an internally-tagged enum (`#[serde(tag = "type")]`) whose
//! variants are **all struct variants** (named fields). This is mandatory:
//! serde cannot serialize an internally-tagged *newtype/tuple* variant wrapping
//! a sequence, scalar, or enum — it errors at runtime. Struct variants serialize
//! cleanly as `{"type":"Constant","value":"30"}` (MASTER-SPEC §7.4).
//!
//! No evaluation and no indicator math live here — [`IndicatorSpec`] is a
//! *reference* type only; ta-rs wiring is VS-1.1.3.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::sweepable::SweepableValue;

/// A field of the current candle (OHLCV). Serialized via its variant name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum PriceField {
    /// Opening price of the candle.
    Open,
    /// Highest price of the candle.
    High,
    /// Lowest price of the candle.
    Low,
    /// Closing price of the candle.
    Close,
    /// Traded volume of the candle.
    Volume,
}

/// A typed reference to a technical indicator and its parameters.
///
/// Internally-tagged (`#[serde(tag = "indicator")]`) with **all struct
/// variants**. The v1 catalog mirrors each indicator's real parameter shape
/// (confirmed additively against the `ta` crate in VS-1.1.3). Typed (not
/// stringly-typed) for compile-time exhaustiveness — the DSL is the contract
/// VS-1.1.3 implements against.
///
/// **Additive-variant rule (load-bearing for 2.05):** appending a *new*
/// indicator variant is a serde-backward-compatible change (old strategies still
/// deserialize) → a **minor** `schema_version` bump. Only renaming, removing, or
/// reshaping a shipped variant is breaking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "indicator")]
pub enum IndicatorSpec {
    /// Relative Strength Index over `period` bars.
    Rsi {
        /// Lookback period.
        period: SweepableValue<u32>,
    },
    /// Exponential Moving Average over `period` bars.
    Ema {
        /// Lookback period.
        period: SweepableValue<u32>,
    },
    /// Average Directional Index over `period` bars.
    Adx {
        /// Lookback period.
        period: SweepableValue<u32>,
    },
    /// Moving Average Convergence Divergence with fast/slow/signal periods.
    Macd {
        /// Fast EMA period.
        fast: SweepableValue<u32>,
        /// Slow EMA period.
        slow: SweepableValue<u32>,
        /// Signal-line EMA period.
        signal: SweepableValue<u32>,
    },
}

/// Where a scalar value in a [`Condition`](super::Condition) comes from.
///
/// Internally-tagged (`#[serde(tag = "type")]`) with **all struct variants** —
/// see the module docs for why tuple/newtype variants are forbidden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum ValueSource {
    /// A literal constant in value-space.
    Constant {
        /// The constant value (a `Decimal`, never `f64`).
        value: Decimal,
    },
    /// A field of the current candle.
    Price {
        /// Which OHLCV field to read.
        field: PriceField,
    },
    /// The output of a technical indicator.
    Indicator {
        /// The indicator and its parameters.
        spec: IndicatorSpec,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{IndicatorSpec, PriceField, SweepableValue, ValueSource};
    use rust_decimal::Decimal;

    fn round_trip(v: &ValueSource) -> ValueSource {
        let json = serde_json::to_string(v).expect("serialize ValueSource");
        serde_json::from_str(&json).expect("deserialize ValueSource")
    }

    #[test]
    fn constant_round_trips() {
        let v = ValueSource::Constant {
            value: Decimal::new(30, 0),
        };
        assert_eq!(round_trip(&v), v);
    }

    #[test]
    fn price_round_trips() {
        let v = ValueSource::Price {
            field: PriceField::Close,
        };
        assert_eq!(round_trip(&v), v);
    }

    #[test]
    fn indicator_rsi_round_trips() {
        let v = ValueSource::Indicator {
            spec: IndicatorSpec::Rsi {
                period: SweepableValue::Fixed(14),
            },
        };
        assert_eq!(round_trip(&v), v);
    }

    #[test]
    fn indicator_macd_round_trips() {
        let v = ValueSource::Indicator {
            spec: IndicatorSpec::Macd {
                fast: SweepableValue::Fixed(12),
                slow: SweepableValue::Fixed(26),
                signal: SweepableValue::Fixed(9),
            },
        };
        assert_eq!(round_trip(&v), v);
    }

    #[test]
    fn constant_serializes_with_struct_tag() {
        // The internal tag + struct-variant shape: {"type":"Constant","value":"30"}.
        let v = ValueSource::Constant {
            value: Decimal::new(30, 0),
        };
        let json = serde_json::to_string(&v).expect("serialize Constant");
        assert!(json.contains("\"type\":\"Constant\""), "json was: {json}");
        assert!(json.contains("\"value\":\"30\""), "json was: {json}");
    }
}
