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
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod prop_tests {
    use super::{Comparator, Condition};
    use crate::domain::dsl::sweepable::SweepableValue;
    use crate::domain::dsl::value::{IndicatorSpec, PriceField, Series, ValueSource};
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

    fn arb_indicator_spec() -> impl Strategy<Value = IndicatorSpec> {
        prop_oneof![
            arb_sweepable_u32().prop_map(|period| IndicatorSpec::Rsi { period }),
            arb_sweepable_u32().prop_map(|period| IndicatorSpec::Ema { period }),
            arb_sweepable_u32().prop_map(|period| IndicatorSpec::Adx { period }),
            arb_sweepable_u32().prop_map(|period| IndicatorSpec::Atr { period }),
            (
                arb_sweepable_u32(),
                arb_sweepable_u32(),
                arb_sweepable_u32()
            )
                .prop_map(|(fast, slow, signal)| IndicatorSpec::Macd {
                    fast,
                    slow,
                    signal
                }),
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

    fn arb_value_source() -> impl Strategy<Value = ValueSource> {
        prop_oneof![
            arb_decimal().prop_map(|value| ValueSource::Constant { value }),
            (arb_series(), arb_price_field())
                .prop_map(|(series, field)| ValueSource::Price { series, field }),
            (arb_series(), arb_indicator_spec())
                .prop_map(|(series, spec)| ValueSource::Indicator { series, spec }),
        ]
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
    /// variants (`Compare`/`CrossesAbove`/`CrossesBelow`); `prop_recursive` caps
    /// depth/size so it can't build pathological trees (spec §3 constraint a).
    fn arb_condition() -> impl Strategy<Value = Condition> {
        let leaf = prop_oneof![
            (arb_value_source(), arb_comparator(), arb_value_source())
                .prop_map(|(lhs, op, rhs)| Condition::Compare { lhs, op, rhs }),
            (arb_value_source(), arb_value_source())
                .prop_map(|(lhs, rhs)| Condition::CrossesAbove { lhs, rhs }),
            (arb_value_source(), arb_value_source())
                .prop_map(|(lhs, rhs)| Condition::CrossesBelow { lhs, rhs }),
        ];
        // depth ≤ 4, ≤ 32 total nodes, ≤ 4 children per collection.
        leaf.prop_recursive(4, 32, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 1..=4)
                    .prop_map(|conditions| Condition::And { conditions }),
                prop::collection::vec(inner.clone(), 1..=4)
                    .prop_map(|conditions| Condition::Or { conditions }),
                inner.prop_map(|c| Condition::Not {
                    condition: Box::new(c),
                }),
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
}
