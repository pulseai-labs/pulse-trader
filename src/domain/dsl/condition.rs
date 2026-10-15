//! `Condition` — the boolean predicate tree of the strategy DSL, plus its
//! [`Comparator`].
//!
//! `Condition` is an internally-tagged enum (`#[serde(tag = "type")]`) whose
//! variants are **all struct variants** (named fields). This is mandatory:
//! serde cannot serialize an internally-tagged *newtype/tuple* variant wrapping
//! a `Vec`, scalar, or enum — it errors at runtime. So `And` carries
//! `{ conditions: Vec<Condition> }`, not `And(Vec<Condition>)`. `Box`/`Vec`
//! carry the recursion.
//!
//! `CrossesAbove`/`CrossesBelow` are grammar primitives here (pure data); their
//! *stateful* evaluation (needs the prior bar; first bar → false) is defined in
//! the compiler/evaluator (2.04), NOT here. This item only fixes the data shape
//! and guarantees serde round-trip — no evaluation, no semantic validation
//! (that is 2.03).

use serde::{Deserialize, Serialize};

use super::value::ValueSource;

/// A scalar comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum Comparator {
    /// Greater than.
    Gt,
    /// Greater than or equal.
    Gte,
    /// Less than.
    Lt,
    /// Less than or equal.
    Lte,
    /// Equal (exact `Decimal` equality).
    Eq,
}

/// A boolean predicate over [`ValueSource`]s — the entry-signal and filter type
/// the rest of the DSL composes.
///
/// Internally-tagged (`#[serde(tag = "type")]`) with **all struct variants** —
/// see the module docs for why tuple/newtype variants are forbidden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum Condition {
    /// Compare two values with a [`Comparator`].
    Compare {
        /// Left-hand operand.
        lhs: ValueSource,
        /// The comparison operator.
        op: Comparator,
        /// Right-hand operand.
        rhs: ValueSource,
    },
    /// `lhs` crosses above `rhs` (stateful; evaluated in 2.04).
    CrossesAbove {
        /// Left-hand operand.
        lhs: ValueSource,
        /// Right-hand operand.
        rhs: ValueSource,
    },
    /// `lhs` crosses below `rhs` (stateful; evaluated in 2.04).
    CrossesBelow {
        /// Left-hand operand.
        lhs: ValueSource,
        /// Right-hand operand.
        rhs: ValueSource,
    },
    /// Logical conjunction over a list of conditions.
    And {
        /// The conjoined conditions.
        conditions: Vec<Condition>,
    },
    /// Logical disjunction over a list of conditions.
    Or {
        /// The disjoined conditions.
        conditions: Vec<Condition>,
    },
    /// Logical negation of a single condition.
    Not {
        /// The negated condition.
        condition: Box<Condition>,
    },
    /// `value` is strictly greater than its own value `bars` bars back **on the
    /// value's own series** (r3.s2.w3, schema 1.2.0 — Q2). Compiles to exactly
    /// `Compare{value, Gt, Lag{value, bars}}`; validation refuses a Rising over
    /// a value containing a `Lag` (the compiled form would be a lag of a lag)
    /// and over a mixed-series value.
    Rising {
        /// The value that must be rising.
        value: Box<ValueSource>,
        /// How many bars back to compare, 1..=500. Absent on the wire ⇒ 1;
        /// writes always emit it.
        #[serde(default = "default_expression_bars")]
        bars: u32,
    },
    /// `value` is strictly less than its own value `bars` bars back on the
    /// value's own series — the mirror of [`Condition::Rising`]; compiles to
    /// exactly `Compare{value, Lt, Lag{value, bars}}`.
    Falling {
        /// The value that must be falling.
        value: Box<ValueSource>,
        /// How many bars back to compare, 1..=500. Absent on the wire ⇒ 1;
        /// writes always emit it.
        #[serde(default = "default_expression_bars")]
        bars: u32,
    },
}

/// The `bars` default for [`Condition::Rising`]/[`Condition::Falling`] (Q2:
/// default 1). Writes always emit the field explicitly; this only serves a
/// document that omits it.
fn default_expression_bars() -> u32 {
    1
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Comparator, Condition};
    use crate::domain::dsl::sweepable::SweepableValue;
    use crate::domain::dsl::value::{IndicatorSpec, Series, ValueSource};
    use rust_decimal::Decimal;

    fn round_trip(c: &Condition) -> Condition {
        let json = serde_json::to_string(c).expect("serialize Condition");
        serde_json::from_str(&json).expect("deserialize Condition")
    }

    /// AC-5: a real `RSI(14) < 30` predicate round-trips value-equal. This is
    /// the demo-1 sample predicate.
    #[test]
    fn rsi_oversold_condition_round_trips() {
        let cond = Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Rsi {
                    period: SweepableValue::Fixed(14),
                },
            },
            op: Comparator::Lt,
            rhs: ValueSource::Constant {
                value: Decimal::new(30, 0),
            },
        };
        assert_eq!(round_trip(&cond), cond);
    }

    /// AC-7: an `And { conditions: [Compare…, Compare…] }` round-trips
    /// value-equal. This is the exact shape that fails at runtime if any variant
    /// were tuple/newtype rather than a struct variant (catches the
    /// internally-tagged-enum trap behaviorally).
    #[test]
    fn and_condition_round_trips() {
        let cond = Condition::And {
            conditions: vec![
                Condition::Compare {
                    lhs: ValueSource::Indicator {
                        series: Series::Primary,
                        spec: IndicatorSpec::Rsi {
                            period: SweepableValue::Fixed(14),
                        },
                    },
                    op: Comparator::Lt,
                    rhs: ValueSource::Constant {
                        value: Decimal::new(30, 0),
                    },
                },
                Condition::Compare {
                    lhs: ValueSource::Indicator {
                        series: Series::Primary,
                        spec: IndicatorSpec::Ema {
                            period: SweepableValue::Fixed(50),
                        },
                    },
                    op: Comparator::Gt,
                    rhs: ValueSource::Constant {
                        value: Decimal::new(100, 0),
                    },
                },
            ],
        };
        // Serialize succeeds (would error at runtime for a tuple/newtype variant
        // under #[serde(tag = ...)]) AND round-trips value-equal.
        assert_eq!(round_trip(&cond), cond);
    }

    /// AC-8: a `Decimal` with non-zero scale (`30.00`) round-trips
    /// **value-equal**. Pins the round-trip contract to value-equality, NOT
    /// byte-canonical JSON.
    #[test]
    fn decimal_scale_survives_round_trip() {
        // Decimal::new(3000, 2) == 30.00 (mantissa 3000, scale 2).
        let scaled = Decimal::new(3000, 2);
        assert_eq!(scaled.scale(), 2, "fixture must carry a non-zero scale");
        let cond = Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Primary,
                field: crate::domain::dsl::value::PriceField::Close,
            },
            op: Comparator::Gte,
            rhs: ValueSource::Constant { value: scaled },
        };
        // Value-equality holds (Decimal Eq compares value AND scale, so the
        // serde-with-str representation must preserve the "30.00" text).
        assert_eq!(round_trip(&cond), cond);
    }

    /// AC-9: serde rejects an unknown variant tag AND a malformed
    /// `SweepableValue` (a `Sweep` object missing `step`). Locks the
    /// structural-rejection boundary (serde rejects malformed; 2.03 rejects
    /// semantically-invalid).
    #[test]
    fn rejects_unknown_variant_and_malformed_sweepable() {
        // Unknown variant tag → Err.
        let unknown: Result<Condition, _> = serde_json::from_str(r#"{"type":"Bogus"}"#);
        assert!(unknown.is_err(), "unknown variant tag must be rejected");

        // A Sweep object missing `step`, nested in an otherwise-valid Indicator
        // condition → Err (untagged SweepableValue cannot match Fixed (object,
        // not bare value) NOR Sweep (missing `step`)).
        let malformed = r#"{
            "type":"Compare",
            "lhs":{"type":"Indicator","spec":{
                "indicator":"Rsi","period":{"start":5,"end":20}
            }},
            "op":"Lt",
            "rhs":{"type":"Constant","value":"30"}
        }"#;
        let bad: Result<Condition, _> = serde_json::from_str(malformed);
        assert!(bad.is_err(), "malformed SweepableValue must be rejected");
    }

    /// r3.s2.w3 (schema 1.2.0): a `Rising` condition round-trips value-equal,
    /// writes always emit `bars`, and an omitted `bars` reads as 1 (Q2's
    /// default). `Falling` mirrors.
    #[test]
    fn rising_and_falling_round_trip_and_default_bars_to_one() {
        use crate::domain::dsl::value::PriceField;
        let rising = Condition::Rising {
            value: Box::new(ValueSource::Indicator {
                series: Series::Htf,
                spec: IndicatorSpec::Ema {
                    period: SweepableValue::Fixed(200),
                },
            }),
            bars: 1,
        };
        assert_eq!(round_trip(&rising), rising);
        let json = serde_json::to_string(&rising).expect("serialize Rising");
        assert!(json.contains("\"type\":\"Rising\""), "json was: {json}");
        assert!(json.contains("\"bars\":1"), "writes always emit bars");

        let omitted: Condition =
            serde_json::from_str(r#"{"type":"Rising","value":{"type":"Price","field":"Close"}}"#)
                .expect("bars-less Rising deserializes");
        assert_eq!(
            omitted,
            Condition::Rising {
                value: Box::new(ValueSource::Price {
                    series: Series::Primary,
                    field: PriceField::Close,
                }),
                bars: 1,
            },
            "an omitted bars reads as the Q2 default 1"
        );

        let falling = Condition::Falling {
            value: Box::new(ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            }),
            bars: 3,
        };
        assert_eq!(round_trip(&falling), falling);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod prop_tests {
    use super::{Comparator, Condition};
    use crate::domain::dsl::sweepable::SweepableValue;
    use crate::domain::dsl::value::{
        ArithOp, CandlePatternName, IndicatorSpec, MacdOutput, PriceField, Series, ValueSource,
    };
    use proptest::prelude::*;
    use rust_decimal::Decimal;

    /// Generate a `Decimal` from an integer mantissa + scale (NOT `f64`) so the
    /// round-trip is exact (architect-critic C6, spec §3 proptest constraint b).
    fn arb_decimal() -> impl Strategy<Value = Decimal> {
        (any::<i64>(), 0u32..=8u32).prop_map(|(m, s)| Decimal::new(m, s))
    }

    fn arb_sweepable_u32() -> impl Strategy<Value = SweepableValue<u32>> {
        prop_oneof![
            any::<u32>().prop_map(SweepableValue::Fixed),
            (any::<u32>(), any::<u32>(), any::<u32>())
                .prop_map(|(start, end, step)| SweepableValue::Sweep { start, end, step }),
        ]
    }

    /// r4.s6.w1: the decimal leaf the structure level variants carry
    /// (`tolerance_pct`), in both its `Fixed` and `Sweep` shapes — the same
    /// generator `strategy.rs` uses for its sweepable decimals.
    fn arb_sweepable_decimal() -> impl Strategy<Value = SweepableValue<Decimal>> {
        prop_oneof![
            arb_decimal().prop_map(SweepableValue::Fixed),
            (arb_decimal(), arb_decimal(), arb_decimal())
                .prop_map(|(start, end, step)| SweepableValue::Sweep { start, end, step }),
        ]
    }

    /// Every `prop_oneof` arm below is `.boxed()` — see
    /// `strategy.rs`'s `arb_indicator_spec` for the #283 rationale: the
    /// union's value tree holds a `Box<dyn ValueTree>` instead of the arm's
    /// full monomorphized tree, so nested `new_tree`/drop-glue frames stay
    /// small on pathological seeds. Distribution, weights and
    /// `prop_recursive(4, 32, 4)` bounds are unchanged.
    fn arb_indicator_spec() -> impl Strategy<Value = IndicatorSpec> {
        prop_oneof![
            arb_sweepable_u32()
                .prop_map(|period| IndicatorSpec::Rsi { period })
                .boxed(),
            arb_sweepable_u32()
                .prop_map(|period| IndicatorSpec::Ema { period })
                .boxed(),
            arb_sweepable_u32()
                .prop_map(|period| IndicatorSpec::Adx { period })
                .boxed(),
            arb_sweepable_u32()
                .prop_map(|period| IndicatorSpec::Atr { period })
                .boxed(),
            (
                arb_sweepable_u32(),
                arb_sweepable_u32(),
                arb_sweepable_u32(),
                prop_oneof![
                    Just(MacdOutput::Line),
                    Just(MacdOutput::Signal),
                    Just(MacdOutput::Histogram)
                ]
                .boxed(),
            )
                .prop_map(|(fast, slow, signal, output)| IndicatorSpec::Macd {
                    fast,
                    slow,
                    signal,
                    output
                })
                .boxed(),
            // r3.s2.w2 (Q1): the rolling extremes — any period, any source.
            (arb_sweepable_u32(), arb_price_field())
                .prop_map(|(period, source)| IndicatorSpec::Highest { period, source })
                .boxed(),
            (arb_sweepable_u32(), arb_price_field())
                .prop_map(|(period, source)| IndicatorSpec::Lowest { period, source })
                .boxed(),
            // r4.s6.w1 (schema 1.3.0) and r4.s6.w3: the structure and pattern
            // families — pivots, levels, candle anatomy, necklines and
            // trendlines — as one arm (their generator has its own function, so
            // this one stays readable as the catalog grows).
            arb_structure_indicator_spec(),
        ]
    }

    /// The structure (r4.s6.w1) and pattern (r4.s6.w3) variants as one arm of
    /// the spec generator: any span for the pivots and trendlines, any span plus
    /// any decimal tolerance for the levels and necklines, and the four
    /// parameterless anatomy variants.
    fn arb_structure_indicator_spec() -> impl Strategy<Value = IndicatorSpec> {
        prop_oneof![
            (arb_sweepable_u32(), arb_sweepable_u32())
                .prop_map(|(left, right)| IndicatorSpec::PivotHigh { left, right })
                .boxed(),
            (arb_sweepable_u32(), arb_sweepable_u32())
                .prop_map(|(left, right)| IndicatorSpec::PivotLow { left, right })
                .boxed(),
            (
                arb_sweepable_u32(),
                arb_sweepable_u32(),
                arb_sweepable_decimal(),
                arb_sweepable_u32(),
                arb_sweepable_u32(),
            )
                .prop_map(|(left, right, tolerance_pct, min_touches, lookback)| {
                    IndicatorSpec::Resistance {
                        left,
                        right,
                        tolerance_pct,
                        min_touches,
                        lookback,
                    }
                },)
                .boxed(),
            (
                arb_sweepable_u32(),
                arb_sweepable_u32(),
                arb_sweepable_decimal(),
                arb_sweepable_u32(),
                arb_sweepable_u32(),
            )
                .prop_map(|(left, right, tolerance_pct, min_touches, lookback)| {
                    IndicatorSpec::Support {
                        left,
                        right,
                        tolerance_pct,
                        min_touches,
                        lookback,
                    }
                },)
                .boxed(),
            Just(IndicatorSpec::Body).boxed(),
            Just(IndicatorSpec::UpperWick).boxed(),
            Just(IndicatorSpec::LowerWick).boxed(),
            Just(IndicatorSpec::Range).boxed(),
            // r4.s6.w3 (schema 1.3.0): the pattern indicators — any span for
            // the trendlines, any span plus a tolerance for the necklines.
            (arb_sweepable_u32(), arb_sweepable_u32())
                .prop_map(|(left, right)| IndicatorSpec::TrendlineLows { left, right })
                .boxed(),
            (arb_sweepable_u32(), arb_sweepable_u32())
                .prop_map(|(left, right)| IndicatorSpec::TrendlineHighs { left, right })
                .boxed(),
            (
                arb_sweepable_u32(),
                arb_sweepable_u32(),
                arb_sweepable_decimal(),
            )
                .prop_map(|(left, right, tolerance_pct)| {
                    IndicatorSpec::DoubleTopNeckline {
                        left,
                        right,
                        tolerance_pct,
                    }
                })
                .boxed(),
            (
                arb_sweepable_u32(),
                arb_sweepable_u32(),
                arb_sweepable_decimal(),
            )
                .prop_map(|(left, right, tolerance_pct)| {
                    IndicatorSpec::DoubleBottomNeckline {
                        left,
                        right,
                        tolerance_pct,
                    }
                })
                .boxed(),
            // r5.s1.w1 (schema 1.4.0): the named candle patterns, in their own
            // generator so this one stays readable as the catalog grows (the
            // comment above the structure arm's own split).
            arb_candle_pattern_indicator_spec(),
        ]
    }

    /// The schema-1.4.0 named candle patterns (r5.s1.w1; the four three-bar
    /// patterns added by r5.s1.w2): parameterless like the anatomy variants, so
    /// one `Just` arm per pattern — the spine's full 17. The coverage probe
    /// above asserts this list is complete, so a dropped arm is a red, not a
    /// silent shrink.
    fn arb_candle_pattern_indicator_spec() -> impl Strategy<Value = IndicatorSpec> {
        prop_oneof![
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::Doji,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::DragonflyDoji,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::GravestoneDoji,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::Hammer,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::InvertedHammer,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::HangingMan,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::ShootingStar,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::Marubozu,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::SpinningTop,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::Engulfing,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::Harami,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::Piercing,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::DarkCloudCover,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::MorningStar,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::EveningStar,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::ThreeWhiteSoldiers,
            })
            .boxed(),
            Just(IndicatorSpec::CandlePattern {
                pattern: CandlePatternName::ThreeBlackCrows,
            })
            .boxed(),
        ]
    }

    fn arb_price_field() -> impl Strategy<Value = PriceField> {
        prop_oneof![
            Just(PriceField::Open),
            Just(PriceField::High),
            Just(PriceField::Low),
            Just(PriceField::Close),
            Just(PriceField::Volume),
        ]
    }

    /// schema 1.1.0: the `series` tag covers both values so the round-trip
    /// property exercises `htf` operands too (compile gating is not grammar).
    fn arb_series() -> impl Strategy<Value = Series> {
        prop_oneof![Just(Series::Primary), Just(Series::Htf)]
    }

    /// r3.s2.w3: lag depths the round-trip property exercises — the full legal
    /// range 1..=500 (deep lags are plain data to serde).
    fn arb_bars() -> impl Strategy<Value = u32> {
        1u32..=500
    }

    /// r3.s2.w3 (schema 1.2.0): the value grammar gains `Arith`/`Lag` through a
    /// bounded `prop_recursive` of depth ≤ 2 with boxed arms (`.boxed()`) — the
    /// recursion bound #283 coordination pins, so the generator can never be
    /// the reason the merged default-stack proptest re-run overflows. The leaf
    /// arms keep the boxing the rolling-extremes round added.
    fn arb_value_source() -> BoxedStrategy<ValueSource> {
        let leaf = prop_oneof![
            arb_decimal()
                .prop_map(|value| ValueSource::Constant { value })
                .boxed(),
            (arb_series(), arb_price_field())
                .prop_map(|(series, field)| ValueSource::Price { series, field })
                .boxed(),
            (arb_series(), arb_indicator_spec())
                .prop_map(|(series, spec)| ValueSource::Indicator { series, spec })
                .boxed(),
        ];
        leaf.prop_recursive(2, 8, 2, |inner| {
            prop_oneof![
                (any::<ArithOp>(), inner.clone(), inner.clone()).prop_map(|(op, lhs, rhs)| {
                    ValueSource::Arith {
                        op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    }
                }),
                (inner, arb_bars()).prop_map(|(value, bars)| ValueSource::Lag {
                    value: Box::new(value),
                    bars
                }),
            ]
        })
        .boxed()
    }

    fn arb_comparator() -> impl Strategy<Value = Comparator> {
        prop_oneof![
            Just(Comparator::Gt),
            Just(Comparator::Gte),
            Just(Comparator::Lt),
            Just(Comparator::Lte),
            Just(Comparator::Eq),
        ]
    }

    /// A bounded recursive `Condition` generator. Leaf cases are the non-recursive
    /// variants (`Compare`/`CrossesAbove`/`CrossesBelow` — plus r3.s2.w3's
    /// `Rising`/`Falling` leaves); `prop_recursive` caps depth/size so it can't
    /// build pathological trees (spec §3 constraint a).
    fn arb_condition() -> impl Strategy<Value = Condition> {
        let leaf = prop_oneof![
            (arb_value_source(), arb_comparator(), arb_value_source())
                .prop_map(|(lhs, op, rhs)| Condition::Compare { lhs, op, rhs })
                .boxed(),
            (arb_value_source(), arb_value_source())
                .prop_map(|(lhs, rhs)| Condition::CrossesAbove { lhs, rhs })
                .boxed(),
            (arb_value_source(), arb_value_source())
                .prop_map(|(lhs, rhs)| Condition::CrossesBelow { lhs, rhs })
                .boxed(),
            // r3.s2.w3: the expression conditions join the leaves, boxed like
            // their siblings.
            (arb_value_source(), arb_bars())
                .prop_map(|(value, bars)| Condition::Rising {
                    value: Box::new(value),
                    bars
                })
                .boxed(),
            (arb_value_source(), arb_bars())
                .prop_map(|(value, bars)| Condition::Falling {
                    value: Box::new(value),
                    bars
                })
                .boxed(),
        ];
        // depth ≤ 4, ≤ 32 total nodes, ≤ 4 children per collection.
        leaf.prop_recursive(4, 32, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 1..=4)
                    .prop_map(|conditions| Condition::And { conditions })
                    .boxed(),
                prop::collection::vec(inner.clone(), 1..=4)
                    .prop_map(|conditions| Condition::Or { conditions })
                    .boxed(),
                inner
                    .prop_map(|c| Condition::Not {
                        condition: Box::new(c),
                    })
                    .boxed(),
            ]
        })
    }

    proptest! {
        /// AC-6: `deserialize(serialize(x)) == x` over arbitrary `Condition`
        /// trees. The whole-grammar round-trip property; reused by 2.02.
        #[test]
        fn prop_condition_round_trip(cond in arb_condition()) {
            let json = serde_json::to_string(&cond).expect("serialize arbitrary Condition");
            let back: Condition =
                serde_json::from_str(&json).expect("deserialize arbitrary Condition");
            prop_assert_eq!(back, cond);
        }
    }

    /// r5.s1.w1, extended r5.s1.w2: the generator really does produce every
    /// schema-1.4.0 candle pattern, so the round-trip property above exercises
    /// all 17 and a dropped arm cannot silently shrink coverage. The probe draws
    /// a bounded sample and asserts set equality both ways — an arm for a value
    /// the catalog does not have would fail too.
    ///
    /// The expected set is enumerated here rather than derived from the enum,
    /// so extending the enum without extending this list is itself a failure:
    /// the four three-bar patterns of r5.s1.w2 are listed below.
    ///
    /// Names are keyed by their `Debug` spelling: `CandlePatternName` deliberately
    /// derives no `Ord` (nothing in production needs one), and a test must not
    /// widen a shipped type's trait surface for its own convenience.
    #[test]
    fn the_generator_covers_every_candle_pattern() {
        use proptest::strategy::ValueTree;
        use std::collections::BTreeSet;

        const CATALOG: [CandlePatternName; 17] = [
            CandlePatternName::Doji,
            CandlePatternName::DragonflyDoji,
            CandlePatternName::GravestoneDoji,
            CandlePatternName::Hammer,
            CandlePatternName::InvertedHammer,
            CandlePatternName::HangingMan,
            CandlePatternName::ShootingStar,
            CandlePatternName::Marubozu,
            CandlePatternName::SpinningTop,
            CandlePatternName::Engulfing,
            CandlePatternName::Harami,
            CandlePatternName::Piercing,
            CandlePatternName::DarkCloudCover,
            CandlePatternName::MorningStar,
            CandlePatternName::EveningStar,
            CandlePatternName::ThreeWhiteSoldiers,
            CandlePatternName::ThreeBlackCrows,
        ];

        let strategy = arb_structure_indicator_spec();
        let mut runner = proptest::test_runner::TestRunner::deterministic();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for _ in 0..4_000 {
            let spec = strategy
                .new_tree(&mut runner)
                .expect("the generator builds a tree")
                .current();
            if let IndicatorSpec::CandlePattern { pattern } = spec {
                let _ = seen.insert(format!("{pattern:?}"));
            }
        }

        let want: BTreeSet<String> = CATALOG.iter().map(|p| format!("{p:?}")).collect();
        assert_eq!(
            seen,
            want,
            "the generator must produce exactly the spine's 17 candle patterns; \
             missing: {:?}; unexpected: {:?}",
            want.difference(&seen).collect::<Vec<_>>(),
            seen.difference(&want).collect::<Vec<_>>(),
        );
    }
}
