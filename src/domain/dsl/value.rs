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

/// Which candle series a [`ValueSource`] operand reads (r2.s2.w1, schema 1.1.0;
/// `D1` since r3.s2.w4).
///
/// `Primary` is the run's own series; `Htf` is the aligned higher-timeframe
/// series — r2.s2.w2 evaluates it against the higher-timeframe indicator
/// engine stepped on the aligned closed bar (never the primary one). `D1` is
/// the run's fixed daily series (r3.s2.w4): loaded independently of the
/// optional H4 HTF, evaluated on the last closed UTC-midnight bar the same
/// way, and recorded in provenance as `inputs.d1`.
/// Serializes lowercase (`"primary"`/`"htf"`/`"d1"`); deserialization
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
    /// The run's fixed daily series (r3.s2.w4) — independent of [`Series::Htf`],
    /// always UTC-midnight-aligned, read on closed bars only.
    D1,
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

/// A named candlestick pattern (schema **1.4.0**, r5.s1.w1) — the 13 one- and
/// two-bar patterns of TA-Lib's catalog, by TA-Lib's own name with the `CDL`
/// prefix dropped.
///
/// Serialized by **variant name**, like every other enum in this file
/// ([`PriceField`] writes `"Close"`, and the [`IndicatorSpec::CandlePattern`]
/// tag itself writes `"CandlePattern"`): the wire form is
/// `{"indicator":"CandlePattern","pattern":"Engulfing"}`. There is deliberately
/// no `#[serde(rename_all)]` here — the read path's `snake_case` tokens are a
/// separate vocabulary (w3's), and this enum is the document's spelling.
///
/// The four three-bar patterns (`MorningStar`, `EveningStar`,
/// `ThreeWhiteSoldiers`, `ThreeBlackCrows`) land in r5.s1.w2 as additive values
/// inside 1.4.0, on the same candle ring this item builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum CandlePatternName {
    /// `CDLDOJI` — a bar whose body is at most 10% of the recent mean range.
    Doji,
    /// `CDLDRAGONFLYDOJI` — a doji with a long lower shadow and almost no upper.
    DragonflyDoji,
    /// `CDLGRAVESTONEDOJI` — a doji with a long upper shadow and almost no lower.
    GravestoneDoji,
    /// `CDLHAMMER` — a small body with a long lower shadow at or under the prior low.
    Hammer,
    /// `CDLINVERTEDHAMMER` — a small body with a long upper shadow, gapping down.
    InvertedHammer,
    /// `CDLHANGINGMAN` — the hammer's shape at or over the prior high.
    HangingMan,
    /// `CDLSHOOTINGSTAR` — the inverted hammer's shape, gapping up.
    ShootingStar,
    /// `CDLMARUBOZU` — a long body with no shadows; the sign is the bar's colour.
    Marubozu,
    /// `CDLSPINNINGTOP` — a small body with shadows longer than it on both sides.
    SpinningTop,
    /// `CDLENGULFING` — bar t's body covers bar t-1's; ±100, or ±80 when a body
    /// end coincides.
    Engulfing,
    /// `CDLHARAMI` — a long body followed by a short one inside it; ±100, or ±80
    /// when an end touches.
    Harami,
    /// `CDLPIERCING` — a white bar closing above the midpoint of a prior long
    /// black body, opening below its low.
    Piercing,
    /// `CDLDARKCLOUDCOVER` — a black bar closing below the midpoint of a prior
    /// long white body, opening above its high.
    DarkCloudCover,
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
    /// The high of the most recent confirmed **swing high** (schema 1.3.0,
    /// r4.s6.w1). Bar p is a pivot for a `left`/`right` pair when its high is
    /// strictly greater than the `left` highs before it and greater-or-equal to
    /// the `right` highs after it; equality is deliberate, so a run of equal
    /// highs resolves to its FIRST bar. A pivot is confirmed only once bar
    /// `p + right` has closed — a pivot whose right bars are still forming does
    /// not exist for any value — and the value at bar t is the high of the most
    /// recent confirmed pivot, `None` before the first one.
    PivotHigh {
        /// Bars the pivot must exceed to its left (strictly).
        left: SweepableValue<u32>,
        /// Bars the pivot must be at least equal to on its right.
        right: SweepableValue<u32>,
    },
    /// The low of the most recent confirmed **swing low** (schema 1.3.0,
    /// r4.s6.w1) — the mirror of [`IndicatorSpec::PivotHigh`] on lows: strictly
    /// lower than the `left` lows before it, lower-or-equal to the `right` lows
    /// after it, confirmed only once bar `p + right` closes.
    PivotLow {
        /// Bars the pivot must undercut to its left (strictly).
        left: SweepableValue<u32>,
        /// Bars the pivot must be at most equal to on its right.
        right: SweepableValue<u32>,
    },
    /// The nearest **resistance** above the current close, built from confirmed
    /// swing highs (schema 1.3.0, r4.s6.w1). Among the confirmed pivot highs
    /// within the last `lookback` bars, a candidate level is a run of sorted
    /// pivot prices whose highest is within `tolerance_pct` of their lowest
    /// (for each start, the maximal run within the tolerance of its own lowest
    /// price); every run of at least `min_touches` pivots is a candidate, its
    /// level being the mean of its pivots. The value is the LOWEST candidate
    /// level strictly above the current close, `None` when no run qualifies.
    /// `tolerance_pct` is a DECIMAL FRACTION on the `distance_pct` convention
    /// (`0.003` = 0.3%).
    Resistance {
        /// Bars a pivot must exceed to its left (strictly).
        left: SweepableValue<u32>,
        /// Bars a pivot must be at least equal to on its right.
        right: SweepableValue<u32>,
        /// How close two pivots must be to count as one level, as a decimal
        /// fraction (`0.003` = 0.3%) — the `distance_pct` convention.
        tolerance_pct: SweepableValue<Decimal>,
        /// How many touching pivots a level needs to qualify.
        min_touches: SweepableValue<u32>,
        /// How many bars back a confirmed pivot still counts.
        lookback: SweepableValue<u32>,
    },
    /// The nearest **support** below the current close (schema 1.3.0,
    /// r4.s6.w1) — the mirror of [`IndicatorSpec::Resistance`] on confirmed
    /// swing lows: the HIGHEST candidate level strictly below the current
    /// close, `None` when no run qualifies.
    Support {
        /// Bars a pivot must undercut to its left (strictly).
        left: SweepableValue<u32>,
        /// Bars a pivot must be at most equal to on its right.
        right: SweepableValue<u32>,
        /// How close two pivots must be to count as one level, as a decimal
        /// fraction (`0.003` = 0.3%) — the `distance_pct` convention.
        tolerance_pct: SweepableValue<Decimal>,
        /// How many touching pivots a level needs to qualify.
        min_touches: SweepableValue<u32>,
        /// How many bars back a confirmed pivot still counts.
        lookback: SweepableValue<u32>,
    },
    /// The body height of the current bar — `|close − open|` (schema 1.3.0,
    /// r4.s6.w1). Always defined: the first bar already yields a value.
    Body,
    /// The upper wick of the current bar — `high − max(open, close)`
    /// (schema 1.3.0, r4.s6.w1).
    UpperWick,
    /// The lower wick of the current bar — `min(open, close) − low`
    /// (schema 1.3.0, r4.s6.w1).
    LowerWick,
    /// The full range of the current bar — `high − low` (schema 1.3.0,
    /// r4.s6.w1).
    Range,
    /// The neckline of a **double top** (schema 1.3.0, r4.s6.w3). Let A and B
    /// be the last two confirmed pivot highs, A earlier. When they are within
    /// `tolerance_pct` of each other and `B.bar − A.bar ≥ 2`, the value is the
    /// LOWEST LOW of the bars strictly between them; otherwise `None`. The
    /// value stays the same until a newer pivot high is confirmed.
    DoubleTopNeckline {
        /// Bars the pivot must exceed to its left (strictly).
        left: SweepableValue<u32>,
        /// Bars the pivot must be at least equal to on its right.
        right: SweepableValue<u32>,
        /// How close the two tops must be, as a decimal fraction (`0.003` =
        /// 0.3%) — the `distance_pct` convention.
        tolerance_pct: SweepableValue<Decimal>,
    },
    /// The neckline of a **double bottom** (schema 1.3.0, r4.s6.w3) — the
    /// mirror of [`IndicatorSpec::DoubleTopNeckline`] on confirmed pivot lows:
    /// the HIGHEST HIGH of the bars strictly between the last two
    /// within-tolerance pivot lows, `None` otherwise.
    DoubleBottomNeckline {
        /// Bars the pivot must undercut to its left (strictly).
        left: SweepableValue<u32>,
        /// Bars the pivot must be at most equal to on its right.
        right: SweepableValue<u32>,
        /// How close the two bottoms must be, as a decimal fraction (`0.003` =
        /// 0.3%) — the `distance_pct` convention.
        tolerance_pct: SweepableValue<Decimal>,
    },
    /// The **rising line through the last two confirmed pivot lows**, extended
    /// to bar t (schema 1.3.0, r4.s6.w3). Let A and B be the last two confirmed
    /// pivot lows, A earlier. When `B.price > A.price`, the value at bar t is
    /// `B.price + (B.price − A.price) × (t − B.bar) / (B.bar − A.bar)`, the
    /// quotient rounded to 8 decimal places half-even; a pair that does not
    /// rise — or an operation that overflows — gives `None`.
    TrendlineLows {
        /// Bars the pivot must undercut to its left (strictly).
        left: SweepableValue<u32>,
        /// Bars the pivot must be at most equal to on its right.
        right: SweepableValue<u32>,
    },
    /// The **falling line through the last two confirmed pivot highs** (schema
    /// 1.3.0, r4.s6.w3) — the mirror of [`IndicatorSpec::TrendlineLows`] on
    /// highs (`B.price < A.price`), same extension and rounding rule.
    TrendlineHighs {
        /// Bars the pivot must exceed to its left (strictly).
        left: SweepableValue<u32>,
        /// Bars the pivot must be at least equal to on its right.
        right: SweepableValue<u32>,
    },
    /// A named candlestick pattern's own signal (schema **1.4.0**, r5.s1.w1) —
    /// **TA-Lib's integer**: `+100` bullish, `-100` bearish, `0` no pattern,
    /// and `±80` for the partial forms of [`CandlePatternName::Engulfing`] and
    /// [`CandlePatternName::Harami`], where one body end coincides with the
    /// prior body's edge.
    ///
    /// The value is defined on **every** bar from the pattern's lookback on —
    /// it never returns "no value" after warm-up, unlike the level/pattern
    /// variants above. A rule reads it through the existing comparisons:
    /// `Compare(CandlePattern{Engulfing}, Gt, 0)` for "a bullish engulfing
    /// fired", `Eq 100` for "the full bullish form". No boolean type and no
    /// `Condition` kind is added (grill Q1): TA-Lib's convention is a scalar,
    /// and the ADR-0024 addendum records that this variant answers the 1.3.0
    /// addendum's revisit trigger.
    ///
    /// The variant takes no parameter: TA-Lib's default candle settings and
    /// penetrations are fixed in 1.4.0. Making them sweepable is additive and
    /// deliberately out of scope.
    CandlePattern {
        /// Which pattern to evaluate.
        pattern: CandlePatternName,
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
