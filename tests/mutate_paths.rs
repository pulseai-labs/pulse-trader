//! AC-3 — the parameter path scheme: total over the tunable surface, and spoken
//! in `validate.rs`'s locator grammar (r1.s2.w1, ADR-0021 decision 4 / audit C6).
//!
//! Two claims, both asserted here against ONE representative strategy that
//! exercises every leaf-bearing shape the DSL has — all five indicators, nested
//! `And`/`Or`/`Not`, a primary + an `htf` filter, every exit kind validation
//! permits at once (`StopLoss` and `AtrStop` are one exclusive family — the
//! fixture carries `AtrStop`, the schema-1.1.0 member), and the risk params:
//!
//!   1. **Totality.** Every sweepable numeric leaf is addressable, and the set of
//!      addressable paths is exactly the set of leaves — no leaf is unreachable
//!      and no path addresses something that is not a leaf.
//!   2. **One address language.** A path that `validate.rs` prints in a
//!      `FieldError` is a path `apply` accepts, character for character. That is
//!      audit C6's whole point: a coach failure and a validation failure can be
//!      shown against the same field.
//!
//! Anything outside that surface — a typo, a structural node, a non-parameter
//! field, an index past the end — is a **typed inapplicability**
//! ([`MutationError::UnknownPath`]), never a partial write.
// `too_many_lines`: the representative fixture is one long literal on purpose --
// it is the surface every assertion here is measured against (the `validate.rs`
// test-module precedent).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use pulse::{
    Comparator, Condition, Direction, ExitRule, IndicatorSpec, Mutation, MutationError, ParamValue,
    PriceField, RiskParams, SchemaVersion, Series, StrategyDsl, SweepableValue, ValueSource, apply,
    sweepable_paths, validate,
};
use rust_decimal::Decimal;

/// A representative strategy: every indicator, every exit kind, a filter, and a
/// nested `And`/`Or`/`Not` entry tree — and it validates.
fn representative_strategy() -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "Representative".to_owned(),
        direction: Direction::Long,
        entry: Condition::And {
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
                Condition::Not {
                    condition: Box::new(Condition::Compare {
                        lhs: ValueSource::Indicator {
                            series: Series::Primary,
                            spec: IndicatorSpec::Adx {
                                period: SweepableValue::Fixed(20),
                            },
                        },
                        op: Comparator::Lt,
                        rhs: ValueSource::Constant {
                            value: Decimal::new(20, 0),
                        },
                    }),
                },
                Condition::Or {
                    conditions: vec![
                        Condition::CrossesAbove {
                            lhs: ValueSource::Indicator {
                                series: Series::Primary,
                                spec: IndicatorSpec::Ema {
                                    period: SweepableValue::Fixed(9),
                                },
                            },
                            rhs: ValueSource::Price {
                                series: Series::Primary,
                                field: PriceField::Close,
                            },
                        },
                        Condition::Compare {
                            lhs: ValueSource::Indicator {
                                series: Series::Primary,
                                spec: IndicatorSpec::Macd {
                                    fast: SweepableValue::Fixed(12),
                                    slow: SweepableValue::Fixed(26),
                                    signal: SweepableValue::Fixed(9),
                                },
                            },
                            op: Comparator::Gt,
                            rhs: ValueSource::Constant {
                                value: Decimal::ZERO,
                            },
                        },
                    ],
                },
                // schema 1.1.0: an `Atr` operand (a new indicator leaf).
                Condition::Compare {
                    lhs: ValueSource::Indicator {
                        series: Series::Primary,
                        spec: IndicatorSpec::Atr {
                            period: SweepableValue::Fixed(14),
                        },
                    },
                    op: Comparator::Gt,
                    rhs: ValueSource::Constant {
                        value: Decimal::ZERO,
                    },
                },
            ],
        },
        filters: vec![
            Condition::Compare {
                lhs: ValueSource::Indicator {
                    series: Series::Primary,
                    spec: IndicatorSpec::Ema {
                        period: SweepableValue::Fixed(50),
                    },
                },
                op: Comparator::Gt,
                rhs: ValueSource::Price {
                    series: Series::Primary,
                    field: PriceField::Close,
                },
            },
            // schema 1.1.0: an `htf`-tagged `Ema` filter — `series` is grammar,
            // not a leaf; the Ema's period is.
            Condition::Compare {
                lhs: ValueSource::Indicator {
                    series: Series::Htf,
                    spec: IndicatorSpec::Ema {
                        period: SweepableValue::Fixed(200),
                    },
                },
                op: Comparator::Gt,
                rhs: ValueSource::Constant {
                    value: Decimal::new(100, 0),
                },
            },
        ],
        exits: vec![
            // schema 1.1.0: `AtrStop` occupies the stop family (`StopLoss` and
            // `AtrStop` are exclusive — only one may appear).
            ExitRule::AtrStop {
                period: SweepableValue::Fixed(14),
                multiple: SweepableValue::Fixed(Decimal::new(2, 0)),
            },
            ExitRule::TakeProfit {
                target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
            },
            ExitRule::TrailingStop {
                trail_pct: SweepableValue::Fixed(Decimal::new(3, 2)),
            },
            ExitRule::TimeStop {
                max_bars: SweepableValue::Fixed(48),
            },
            ExitRule::SignalExit {
                condition: Condition::Compare {
                    lhs: ValueSource::Indicator {
                        series: Series::Primary,
                        spec: IndicatorSpec::Rsi {
                            period: SweepableValue::Fixed(14),
                        },
                    },
                    op: Comparator::Gt,
                    rhs: ValueSource::Constant {
                        value: Decimal::new(70, 0),
                    },
                },
            },
        ],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
            max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
        },
    }
}

/// The same representative strategy minus the `htf` filter.
///
/// `apply()` compiles its candidate, and `series: "htf"` is the schema-1.1.0
/// compile gate ([`CompileError::HtfUnsupported`]) — so every apply-driven
/// claim below runs against this all-primary variant; the `htf` filter still
/// proves its addressing claim (a `series` tag is not a leaf, the indicator's
/// period is) through `sweepable_paths` on the full fixture.
fn representative_strategy_primary_only() -> StrategyDsl {
    let mut dsl = representative_strategy();
    dsl.filters.truncate(1);
    dsl
}

/// Every sweepable leaf of [`representative_strategy`], in traversal order.
/// Hand-derived from the fixture: if a leaf is added to the fixture and the
/// traversal does not reach it (or reaches it by another name), this list is what
/// catches it.
const EXPECTED_PATHS: &[&str] = &[
    "entry.and[0].lhs.indicator.rsi.period",
    "entry.and[1].not.lhs.indicator.adx.period",
    "entry.and[2].or[0].lhs.indicator.ema.period",
    "entry.and[2].or[1].lhs.indicator.macd.fast",
    "entry.and[2].or[1].lhs.indicator.macd.slow",
    "entry.and[2].or[1].lhs.indicator.macd.signal",
    "entry.and[3].lhs.indicator.atr.period",
    "filters[0].lhs.indicator.ema.period",
    "filters[1].lhs.indicator.ema.period",
    "exits[0].period",
    "exits[0].multiple",
    "exits[1].target_r",
    "exits[2].trail_pct",
    "exits[3].max_bars",
    "exits[4].condition.lhs.indicator.rsi.period",
    "risk.risk_per_trade_pct",
    "risk.max_leverage",
];

fn set_period(path: &str, value: u32) -> Mutation {
    Mutation::SetParam {
        path: path.to_owned(),
        new_value: ParamValue::Period { value },
    }
}

fn set_threshold(path: &str, value: Decimal) -> Mutation {
    Mutation::SetParam {
        path: path.to_owned(),
        new_value: ParamValue::Threshold { value },
    }
}

// ---------------------------------------------------------------------------
// 1. Totality over the tunable surface
// ---------------------------------------------------------------------------

#[test]
fn the_fixture_validates() {
    // The claims below are about a VALID strategy's tunable surface; if the
    // fixture stopped validating, every other assertion here would be vacuous.
    assert!(
        validate(&representative_strategy()).is_ok(),
        "the representative strategy must validate"
    );
}

#[test]
fn every_sweepable_leaf_is_addressable() {
    let paths = sweepable_paths(&representative_strategy());

    assert_eq!(
        paths,
        EXPECTED_PATHS
            .iter()
            .map(|p| (*p).to_owned())
            .collect::<Vec<_>>(),
        "the addressable set must be exactly the strategy's sweepable leaves, in traversal order"
    );
}

#[test]
fn addressable_paths_are_unique() {
    let paths = sweepable_paths(&representative_strategy());
    let mut sorted = paths.clone();
    sorted.sort();
    sorted.dedup();

    assert_eq!(
        sorted.len(),
        paths.len(),
        "two leaves sharing a path would make a mutation ambiguous: {paths:?}"
    );
}

#[test]
fn every_addressable_path_has_exactly_one_leaf_kind() {
    let dsl = representative_strategy();

    for path in sweepable_paths(&dsl) {
        let as_period = apply(&dsl, &set_period(&path, 10));
        let as_threshold = apply(&dsl, &set_threshold(&path, Decimal::new(5, 1)));

        // Neither attempt may claim the path does not exist...
        for outcome in [&as_period, &as_threshold] {
            assert!(
                !matches!(outcome, Err(MutationError::UnknownPath { .. })),
                "`{path}` came from sweepable_paths and must be addressable, got {outcome:?}"
            );
        }

        // ...and exactly one of the two kinds must be the leaf's own kind. (The
        // matching kind may still fail validation -- e.g. setting MACD `slow` to
        // 10 puts it below `fast` -- which is a domain answer, not an addressing
        // one.)
        let mismatches = [&as_period, &as_threshold]
            .iter()
            .filter(|o| matches!(o, Err(MutationError::TypeMismatch { .. })))
            .count();
        assert_eq!(
            mismatches, 1,
            "`{path}` must accept exactly one of the two numeric kinds; period={as_period:?} threshold={as_threshold:?}"
        );
    }
}

#[test]
fn a_type_correct_mutation_on_every_period_leaf_applies() {
    // `representative_strategy_primary_only`: `apply` compiles its candidate,
    // and the full fixture's `htf` filter hits the schema-1.1.0 compile gate.
    let dsl = representative_strategy_primary_only();

    // Every period leaf except MACD's, which is cross-field constrained
    // (`fast < slow`) and therefore not independently settable to one value.
    let independent_periods = [
        "entry.and[0].lhs.indicator.rsi.period",
        "entry.and[1].not.lhs.indicator.adx.period",
        "entry.and[2].or[0].lhs.indicator.ema.period",
        "entry.and[3].lhs.indicator.atr.period",
        "filters[0].lhs.indicator.ema.period",
        "exits[0].period",
        "exits[3].max_bars",
        "exits[4].condition.lhs.indicator.rsi.period",
    ];

    for path in independent_periods {
        let candidate = apply(&dsl, &set_period(path, 11))
            .unwrap_or_else(|e| panic!("`{path}` must accept a period of 11, got {e:?}"));
        assert!(
            sweepable_paths(candidate.dsl()).contains(&path.to_owned()),
            "the mutated candidate must still expose `{path}`"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. One address language (audit C6)
// ---------------------------------------------------------------------------

#[test]
fn a_validate_field_error_path_is_an_addressable_mutation_path() {
    // `representative_strategy_primary_only`: `apply` compiles its candidate —
    // the full fixture's `htf` filter hits the compile gate.
    // Break one leaf so `validate` has something to point at, and take the path
    // from ITS mouth rather than writing it out again here.
    let mut broken = representative_strategy_primary_only();
    broken.entry = Condition::And {
        conditions: match broken.entry {
            Condition::And { mut conditions } => {
                conditions[0] = Condition::Compare {
                    lhs: ValueSource::Indicator {
                        series: Series::Primary,
                        spec: IndicatorSpec::Rsi {
                            period: SweepableValue::Fixed(0),
                        },
                    },
                    op: Comparator::Lt,
                    rhs: ValueSource::Constant {
                        value: Decimal::new(30, 0),
                    },
                };
                conditions
            }
            other => panic!("fixture entry is not an And: {other:?}"),
        },
    };

    let errors = validate(&broken).expect_err("an RSI period of 0 must fail validation");
    let field_error = errors
        .errors()
        .iter()
        .find(|e| e.path.ends_with("rsi.period"))
        .expect("validate must report the zero RSI period");

    // The grammar claim: validate's locator IS a mutation path.
    let healthy = representative_strategy_primary_only();
    assert!(
        sweepable_paths(&healthy).contains(&field_error.path),
        "validate reported `{}`, which mutation paths must also address: {:?}",
        field_error.path,
        sweepable_paths(&healthy)
    );
    apply(&broken, &set_period(&field_error.path, 14)).unwrap_or_else(|e| {
        panic!(
            "`{}` must be repairable by a mutation, got {e:?}",
            field_error.path
        )
    });
}

// ---------------------------------------------------------------------------
// 3. Everything else is a typed inapplicability
// ---------------------------------------------------------------------------

#[test]
fn unknown_and_non_parameter_paths_are_typed_inapplicability() {
    let dsl = representative_strategy();

    let outside_the_surface = [
        // Non-parameter fields of the document.
        "name",
        "direction",
        "schema_version",
        // Structural nodes rather than leaves.
        "entry",
        "entry.and[0]",
        "entry.and[0].lhs",
        "entry.and[0].lhs.indicator",
        "entry.and[0].lhs.indicator.rsi",
        "exits",
        "exits[0]",
        // `exits[0]` is an AtrStop — the old stop family's leaf name is a
        // near-miss, not an address.
        "exits[0].distance_pct",
        "risk",
        // A leaf of the wrong indicator at a real address.
        "entry.and[0].lhs.indicator.ema.period",
        // The rhs operands, which are Constant/Price and carry no leaf.
        "entry.and[0].rhs.indicator.rsi.period",
        "entry.and[2].or[0].rhs.indicator.ema.period",
        // Indices past the end.
        "filters[2].lhs.indicator.ema.period",
        "exits[5].distance_pct",
        "entry.and[3].lhs.indicator.rsi.period",
        "entry.and[2].or[2].lhs.indicator.ema.period",
        // Near-misses on a real path.
        "entry.and[0].lhs.indicator.rsi.periods",
        "entry.and[0].not.lhs.indicator.rsi.period",
        "risk.risk_per_trade",
    ];

    for path in outside_the_surface {
        let outcome = apply(&dsl, &set_period(path, 10));
        assert!(
            matches!(outcome, Err(MutationError::UnknownPath { .. })),
            "`{path}` is outside the tunable surface and must be a typed inapplicability, got {outcome:?}"
        );
    }
}
