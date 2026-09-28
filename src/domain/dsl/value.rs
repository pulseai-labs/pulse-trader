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

/// Which candle series a [`ValueSource`] operand reads (r2.s2.w1, schema 1.1.0).
///
/// `Primary` is the run's own series; `Htf` is the aligned higher-timeframe
/// series — r2.s2.w2 evaluates it against the higher-timeframe indicator
/// engine stepped on the aligned closed bar (never the primary one).
/// Serializes lowercase (`"primary"`/`"htf"`); deserialization
/// defaults a missing `series` to `primary` via the `#[serde(default)]` on each
/// operand field, while writes always emit the tag explicitly.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Series {
    /// The run's own candle series.
    #[default]
    Primary,
    /// The aligned higher-timeframe series.
    Htf,
}

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

/// Which output of a MACD an [`IndicatorSpec::Macd`] operand reads (schema
/// 1.2.0, r3.s2 — b2). Serializes `snake_case` (`"line"`/`"signal"`/
/// `"histogram"`); deserialization defaults a missing `output` to `line` via
/// the `#[serde(default)]` on the `Macd` field, while writes always emit the
/// tag explicitly (the `series` precedent). `Line` is the historical behaviour
/// (schema ≤ 1.1.0 exposed only the line).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MacdOutput {
    /// `EMA(fast) − EMA(slow)` — the v1 default.
    #[default]
    Line,
    /// The signal line: the seeded EMA(signal period) of the MACD line.
    Signal,
    /// `line − signal`.
    Histogram,
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
    /// `output` selects which of the three ta-rs outputs the operand reads
    /// (schema 1.2.0, r3.s2 — b2); absent → the historical line.
    Macd {
        /// Fast EMA period.
        fast: SweepableValue<u32>,
        /// Slow EMA period.
        slow: SweepableValue<u32>,
        /// Signal-line EMA period.
        signal: SweepableValue<u32>,
        /// Which output the operand reads; defaults to [`MacdOutput::Line`].
        #[serde(default)]
        output: MacdOutput,
    },
    /// Average True Range over `period` bars (schema 1.1.0; r2.s2.w2 computes
    /// it — Wilder smoothing of the true range).
    Atr {
        /// Lookback period.
        period: SweepableValue<u32>,
    },
    /// The highest `source` value of the **N closed bars before the current
    /// bar**, excluding it (schema 1.2.0, r3.s2 — Q1, the Donchian prior-N
    /// convention). `source` is any price field, defaulting to `High`; writes
    /// always emit it (the `series`/`output` precedent). Warm-up is N+1 bars:
    /// the first value lands on candle index N.
    Highest {
        /// Lookback period (the window size N).
        period: SweepableValue<u32>,
        /// Which price field the window aggregates; absent → `High`.
        #[serde(default = "default_highest_source")]
        source: PriceField,
    },
    /// The lowest `source` value of the **N closed bars before the current
    /// bar**, excluding it (schema 1.2.0, r3.s2 — Q1). `source` defaults to
    /// `Low`; warm-up is N+1 bars, like [`IndicatorSpec::Highest`].
    Lowest {
        /// Lookback period (the window size N).
        period: SweepableValue<u32>,
        /// Which price field the window aggregates; absent → `Low`.
        #[serde(default = "default_lowest_source")]
        source: PriceField,
    },
}

/// `#[serde(default = …)]` target for [`IndicatorSpec::Highest`]'s `source`
/// (Q1: the Donchian convention tracks the highs).
fn default_highest_source() -> PriceField {
    PriceField::High
}

/// `#[serde(default = …)]` target for [`IndicatorSpec::Lowest`]'s `source`.
fn default_lowest_source() -> PriceField {
    PriceField::Low
}

/// The operation of a [`ValueSource::Arith`] node (r3.s2.w3, schema 1.2.0 —
/// Q2). Serializes `snake_case` (`"add"`/`"sub"`/`"mul"`/`"div"`).
///
/// All four operations evaluate on `Decimal` only, through the `checked_*`
/// arithmetic: any overflow gives no value, and `Div` by an exactly-zero
/// operand gives no value (Q2: "division by zero, or any operand without a
/// value, gives no value").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArithOp {
    /// Pointwise sum.
    Add,
    /// Pointwise difference.
    Sub,
    /// Pointwise product.
    Mul,
    /// Pointwise ratio; a zero divisor gives no value.
    Div,
}

#[cfg(test)]
impl proptest::arbitrary::Arbitrary for ArithOp {
    type Parameters = ();
    type Strategy = proptest::strategy::BoxedStrategy<Self>;

    fn arbitrary_with((): Self::Parameters) -> Self::Strategy {
        use proptest::prelude::*;
        prop_oneof![
            Just(Self::Add),
            Just(Self::Sub),
            Just(Self::Mul),
            Just(Self::Div),
        ]
        .boxed()
    }
}

/// Where a scalar value in a [`Condition`](super::Condition) comes from.
///
/// Internally-tagged (`#[serde(tag = "type")]`) with **all struct variants** —
/// see the module docs for why tuple/newtype variants are forbidden.
///
/// r3.s2.w3 (schema 1.2.0, additive inside 1.2.0 — b1) adds the two expression
/// nodes, `Arith` and `Lag`. Both are validated before any strategy persists
/// (depth ≤ 4 over `Arith`+`Lag` nodes, `bars` ∈ 1..=500, no lag of a lag, no
/// mixed-series lag — `validate.rs`), and both **compile by pushing the lag
/// down to the leaves** (`compile.rs`), so evaluation history stays leaf-only.
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
        /// Which series the field reads (absent ⇒ `primary`; writes always
        /// emit it).
        #[serde(default)]
        series: Series,
        /// Which OHLCV field to read.
        field: PriceField,
    },
    /// The output of a technical indicator.
    Indicator {
        /// Which series the indicator runs on (absent ⇒ `primary`; writes
        /// always emit it).
        #[serde(default)]
        series: Series,
        /// The indicator and its parameters.
        spec: IndicatorSpec,
    },
    /// A pointwise arithmetic combination of two operand values (r3.s2.w3,
    /// schema 1.2.0 — Q2). Division by zero, or any operand without a value,
    /// gives no value.
    Arith {
        /// The pointwise operation.
        op: ArithOp,
        /// The left operand.
        lhs: Box<ValueSource>,
        /// The right operand.
        rhs: Box<ValueSource>,
    },
    /// The operand's value `bars` bars back **on the operand's own series**
    /// (r3.s2.w3, schema 1.2.0 — Q2): an `h4:` operand lags in H4 bars.
    /// Validation refuses a lag under a lag (use one `Lag` with a larger
    /// `bars`) and a lag over a mixed-series value.
    Lag {
        /// The value to lag.
        value: Box<ValueSource>,
        /// How many bars back to read, 1..=500.
        bars: u32,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        ArithOp, IndicatorSpec, MacdOutput, PriceField, Series, SweepableValue, ValueSource,
    };
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
            series: Series::Primary,
            field: PriceField::Close,
        };
        assert_eq!(round_trip(&v), v);
    }

    #[test]
    fn indicator_rsi_round_trips() {
        let v = ValueSource::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Rsi {
                period: SweepableValue::Fixed(14),
            },
        };
        assert_eq!(round_trip(&v), v);
    }

    #[test]
    fn indicator_macd_round_trips() {
        let v = ValueSource::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Macd {
                fast: SweepableValue::Fixed(12),
                slow: SweepableValue::Fixed(26),
                signal: SweepableValue::Fixed(9),
                output: MacdOutput::Line,
            },
        };
        assert_eq!(round_trip(&v), v);
    }

    /// schema 1.1.0: an `htf`-tagged operand round-trips, an absent `series`
    /// reads `primary`, and writes always emit the field explicitly.
    #[test]
    fn series_round_trips_and_defaults_primary() {
        let htf = ValueSource::Indicator {
            series: Series::Htf,
            spec: IndicatorSpec::Ema {
                period: SweepableValue::Fixed(200),
            },
        };
        assert_eq!(round_trip(&htf), htf);

        // Absent on the wire ⇒ Primary.
        let read: ValueSource = serde_json::from_str(r#"{"type":"Price","field":"Close"}"#)
            .expect("series-less Price deserializes");
        assert_eq!(
            read,
            ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            }
        );

        // Writes always emit `series` explicitly.
        let json = serde_json::to_string(&read).expect("serialize Price");
        assert!(json.contains("\"series\":\"primary\""), "json was: {json}");
    }

    /// schema 1.1.0: `IndicatorSpec::Atr` round-trips under its `atr` tag.
    #[test]
    fn indicator_atr_round_trips() {
        let v = ValueSource::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Atr {
                period: SweepableValue::Fixed(14),
            },
        };
        assert_eq!(round_trip(&v), v);
        let wire: IndicatorSpec =
            serde_json::from_str(r#"{"indicator":"Atr","period":14}"#).expect("Atr tag parses");
        assert_eq!(
            wire,
            IndicatorSpec::Atr {
                period: SweepableValue::Fixed(14),
            }
        );
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

    /// r3.s2.w3 (schema 1.2.0): an `Arith` node round-trips value-equal, its
    /// `op` token is the lowercase Q2 spelling, and nested operands survive.
    #[test]
    fn arith_round_trips_with_snake_case_op() {
        let v = ValueSource::Arith {
            op: ArithOp::Div,
            lhs: Box::new(ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Atr {
                    period: SweepableValue::Fixed(14),
                },
            }),
            rhs: Box::new(ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            }),
        };
        assert_eq!(round_trip(&v), v);
        let json = serde_json::to_string(&v).expect("serialize Arith");
        assert!(json.contains("\"type\":\"Arith\""), "json was: {json}");
        assert!(json.contains("\"op\":\"div\""), "json was: {json}");
        let wire: ValueSource = serde_json::from_str(&json).expect("deserialize Arith");
        assert_eq!(wire, v);
    }

    /// r3.s2.w3 (schema 1.2.0): a `Lag` node round-trips value-equal and always
    /// writes `bars`.
    #[test]
    fn lag_round_trips_and_writes_bars() {
        let v = ValueSource::Lag {
            value: Box::new(ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            }),
            bars: 5,
        };
        assert_eq!(round_trip(&v), v);
        let json = serde_json::to_string(&v).expect("serialize Lag");
        assert!(json.contains("\"type\":\"Lag\""), "json was: {json}");
        assert!(json.contains("\"bars\":5"), "json was: {json}");
    }
}
