//! AC-2 — `apply`: a mutation either yields a validated, compiled candidate or a
//! typed [`MutationError`] (r1.s2.w1, ADR-0021).
//!
//! ADR-0021's decision 2: success means the mutated strategy **passed
//! `validate()` and compiled**, using the existing `dsl/validate.rs` and
//! `dsl/compile.rs` — no second validation path. Everything else is a typed,
//! recorded reason rather than a panic or a silence, which is what makes the
//! coach's never-silence guarantee implementable downstream (`w2`/`w3`).
//!
//! This binary asserts the four things AC-2 names:
//!   1. the happy path yields a candidate that validated AND compiles;
//!   2. every `MutationError` variant is exercised — unknown path, type/domain
//!      mismatch, validation failure, and compile failure (the last by direct
//!      construction: it is unreachable through `apply()` by construction, see
//!      `compile_failed_carries_its_context` below);
//!   3. no panic paths — a broad sweep of malformed, empty, out-of-range and
//!      non-parameter paths all return `Err`;
//!   4. the input DSL is never partially mutated — on every failing mutation the
//!      caller's document is byte-identical to what it was.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{
    CandidateDsl, Comparator, Condition, Direction, ExitRule, IndicatorSpec, Mutation,
    MutationError, ParamKind, ParamValue, RiskParams, SchemaVersion, Series, StrategyDsl,
    SweepableValue, ValueSource, apply, compile,
};
use rust_decimal::Decimal;

/// The canonical RSI-oversold strategy (the fixture `validate.rs` and
/// `strategy.rs` both use): long when RSI(14) < 30, 5% stop, 2R take-profit,
/// 1% risk/trade, 3x max leverage.
fn rsi_oversold_strategy() -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "RSI Oversold".to_owned(),
        direction: Direction::Long,
        entry: Condition::Compare {
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
        filters: vec![],
        exits: vec![
            ExitRule::StopLoss {
                distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
            },
            ExitRule::TakeProfit {
                target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
            },
        ],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
            max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
        },
    }
}

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

/// The period leaf of the fixture's entry condition, read back out of a candidate.
fn entry_rsi_period(dsl: &StrategyDsl) -> &SweepableValue<u32> {
    match &dsl.entry {
        Condition::Compare {
            lhs:
                ValueSource::Indicator {
                    series: Series::Primary,
                    spec: IndicatorSpec::Rsi { period },
                },
            ..
        } => period,
        other => panic!("fixture entry is not an RSI compare: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. The happy path: validated AND compiled
// ---------------------------------------------------------------------------

#[test]
fn set_param_yields_a_candidate_that_validated_and_compiles() {
    let dsl = rsi_oversold_strategy();

    let candidate: CandidateDsl = apply(&dsl, &set_period("entry.lhs.indicator.rsi.period", 21))
        .expect("retuning the RSI period to 21 is a valid mutation");

    // The mutation landed on the addressed leaf...
    assert_eq!(
        entry_rsi_period(candidate.dsl()),
        &SweepableValue::Fixed(21),
        "the addressed leaf must carry the new value"
    );
    // ...and nothing else moved.
    let mut expected = rsi_oversold_strategy();
    expected.entry = Condition::Compare {
        lhs: ValueSource::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Rsi {
                period: SweepableValue::Fixed(21),
            },
        },
        op: Comparator::Lt,
        rhs: ValueSource::Constant {
            value: Decimal::new(30, 0),
        },
    };
    assert_eq!(
        candidate.dsl(),
        &expected,
        "a SetParam must change exactly the addressed leaf"
    );

    // The candidate carries the ValidatedDsl that proves it passed validation --
    // `ValidatedDsl` is constructible ONLY via `validate` -- and it compiles.
    assert_eq!(
        candidate.validated().dsl(),
        candidate.dsl(),
        "the carried ValidatedDsl must be the candidate document itself"
    );
    assert!(
        compile(candidate.validated()).is_ok(),
        "a candidate returned by apply() must compile"
    );
}

#[test]
fn a_decimal_leaf_takes_a_threshold_value() {
    let dsl = rsi_oversold_strategy();

    let candidate = apply(
        &dsl,
        &set_threshold("exits[0].distance_pct", Decimal::new(3, 2)),
    )
    .expect("tightening the stop to 3% is a valid mutation");

    match &candidate.dsl().exits[0] {
        ExitRule::StopLoss { distance_pct } => assert_eq!(
            distance_pct,
            &SweepableValue::Fixed(Decimal::new(3, 2)),
            "the stop distance must carry the new value"
        ),
        other => panic!("exits[0] is not a StopLoss: {other:?}"),
    }
}

#[test]
fn a_threshold_is_a_decimal_string_on_the_wire_and_a_bare_float_is_refused() {
    // NFR-2: the DSL admits decimal STRINGS only, and `ParamValue` is the shape
    // the `propose_mutation` tool schema promises ("a decimal STRING, never a
    // float"). `rust_decimal`'s default Deserialize would also accept a bare JSON
    // float, which is the f64 ingress path closed everywhere else — a threshold
    // that arrives as binary 0.03 is not the number that was proposed, and it is
    // not reproducible.
    let as_string = serde_json::to_value(ParamValue::Threshold {
        value: Decimal::new(3, 2),
    })
    .expect("serialize a threshold");
    assert_eq!(
        as_string,
        serde_json::json!({ "type": "Threshold", "value": "0.03" }),
        "a threshold serializes as a decimal string"
    );

    let round_trip: ParamValue = serde_json::from_value(as_string).expect("the string parses");
    assert_eq!(
        round_trip,
        ParamValue::Threshold {
            value: Decimal::new(3, 2)
        }
    );

    let bare_float: Result<ParamValue, _> =
        serde_json::from_value(serde_json::json!({ "type": "Threshold", "value": 0.03 }));
    assert!(
        bare_float.is_err(),
        "a bare JSON float must be refused, got {bare_float:?}"
    );

    // And an argument the shape does not declare is refused too (PR #93's rule):
    // silently ignoring it is how a misunderstanding becomes a wrong mutation.
    let unknown: Result<ParamValue, _> = serde_json::from_value(
        serde_json::json!({ "type": "Period", "value": 21, "unit": "bars" }),
    );
    assert!(
        unknown.is_err(),
        "an unrecognized field must be refused, got {unknown:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Every MutationError variant
// ---------------------------------------------------------------------------

#[test]
fn unknown_path_is_rejected() {
    let dsl = rsi_oversold_strategy();

    let err = apply(&dsl, &set_period("entry.lhs.indicator.rsi.perlod", 21))
        .expect_err("a misspelled path addresses no leaf");

    match err {
        MutationError::UnknownPath { path } => {
            assert_eq!(path, "entry.lhs.indicator.rsi.perlod");
        }
        other => panic!("expected UnknownPath, got {other:?}"),
    }
}

#[test]
fn a_non_parameter_path_is_rejected_as_unknown() {
    let dsl = rsi_oversold_strategy();

    // `name` is a real DSL field, and `exits[0]` a real node -- neither is a
    // sweepable numeric leaf, so neither is addressable by a parameter mutation.
    for path in [
        "name",
        "direction",
        "entry",
        "entry.lhs",
        "exits[0]",
        "risk",
    ] {
        let outcome = apply(&dsl, &set_period(path, 7));
        assert!(
            matches!(outcome, Err(MutationError::UnknownPath { .. })),
            "`{path}` is not a sweepable leaf and must be rejected as UnknownPath, got {outcome:?}"
        );
    }
}

#[test]
fn type_mismatch_is_rejected_in_both_directions() {
    let dsl = rsi_oversold_strategy();

    // A Decimal offered where the leaf is u32 (the spec's own example).
    let err = apply(
        &dsl,
        &set_threshold("entry.lhs.indicator.rsi.period", Decimal::new(215, 1)),
    )
    .expect_err("a threshold value cannot be written into a period leaf");
    match err {
        MutationError::TypeMismatch {
            path,
            expected,
            offered,
        } => {
            assert_eq!(path, "entry.lhs.indicator.rsi.period");
            assert_eq!(expected, ParamKind::Period);
            assert_eq!(offered, ParamKind::Threshold);
        }
        other => panic!("expected TypeMismatch, got {other:?}"),
    }

    // And the mirror: a u32 offered where the leaf is a Decimal.
    let err = apply(&dsl, &set_period("exits[0].distance_pct", 3))
        .expect_err("a period value cannot be written into a threshold leaf");
    match err {
        MutationError::TypeMismatch {
            path,
            expected,
            offered,
        } => {
            assert_eq!(path, "exits[0].distance_pct");
            assert_eq!(expected, ParamKind::Threshold);
            assert_eq!(offered, ParamKind::Period);
        }
        other => panic!("expected TypeMismatch, got {other:?}"),
    }
}

#[test]
fn validation_failure_carries_the_field_errors() {
    let dsl = rsi_oversold_strategy();

    // A period of 0 is addressable and type-correct, and `validate.rs` rejects it
    // (rule 6, FieldRange). The mutation is where that surfaces.
    let err = apply(&dsl, &set_period("entry.lhs.indicator.rsi.period", 0))
        .expect_err("an RSI period of 0 must fail validation");

    match err {
        MutationError::ValidationFailed { path, errors } => {
            assert_eq!(path, "entry.lhs.indicator.rsi.period");
            assert!(
                errors
                    .errors()
                    .iter()
                    .any(|e| e.path == "entry.lhs.indicator.rsi.period"),
                "the FieldErrors must point at the mutated leaf: {:?}",
                errors.errors()
            );
        }
        other => panic!("expected ValidationFailed, got {other:?}"),
    }

    // A cross-field rule the mutated leaf does not itself violate: MACD fast >= slow.
    let mut macd = rsi_oversold_strategy();
    macd.entry = Condition::Compare {
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
    };
    let err = apply(&macd, &set_period("entry.lhs.indicator.macd.fast", 30))
        .expect_err("MACD fast must stay strictly below slow");
    assert!(
        matches!(err, MutationError::ValidationFailed { .. }),
        "a cross-field violation must surface as ValidationFailed, got {err:?}"
    );
}

#[test]
fn compile_failed_carries_its_context() {
    // DEFENSIVE VARIANT, unreachable through `apply()` by construction: the only
    // `CompileError` is `UnexpectedSweep`, and `validate()` rejects every `Sweep`
    // before `compile()` is ever reached -- so no input can drive apply() here.
    // The variant exists because it is the seam's contract if `compile()` ever
    // gains error cases, exactly as `CompileError::UnexpectedSweep` itself is
    // "defensive / should-be-unreachable" in `compile.rs`. Exercised by direct
    // construction, per the orchestrator's dispatch-2 clarification.
    let err = MutationError::CompileFailed {
        path: "entry.lhs.indicator.rsi.period".to_owned(),
        source: pulse::CompileError::UnexpectedSweep {
            field: "entry.lhs.indicator.rsi.period".to_owned(),
        },
    };

    let rendered = err.to_string();
    assert!(
        rendered.contains("entry.lhs.indicator.rsi.period"),
        "a recorded failure reason must name the path it failed on: {rendered}"
    );
    assert!(
        rendered.contains("compile"),
        "a compile failure must say so in its recorded reason: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// 2b. A proposal that would change nothing (PR #128, finding C3)
// ---------------------------------------------------------------------------

#[test]
fn setting_a_period_to_the_value_it_already_holds_is_refused_as_no_change() {
    let dsl = rsi_oversold_strategy();

    // The fixture's entry RSI period is already 14, so this mutation would mint a
    // candidate byte-identical to its parent.
    let err = apply(&dsl, &set_period("entry.lhs.indicator.rsi.period", 14))
        .expect_err("re-offering the value a leaf already holds changes nothing");

    match &err {
        MutationError::NoChange { path } => {
            assert_eq!(path, "entry.lhs.indicator.rsi.period");
        }
        other => panic!("expected NoChange, got {other:?}"),
    }
    assert!(
        err.to_string().contains("entry.lhs.indicator.rsi.period"),
        "a recorded failure reason must name the leaf it declined: {err}"
    );
}

#[test]
fn setting_a_threshold_to_the_value_it_already_holds_is_refused_as_no_change() {
    let dsl = rsi_oversold_strategy();

    // The fixture's stop is 0.05 and this offers 0.050 -- the SAME number at a
    // different scale, so the strategy it would produce is the same strategy.
    let err = apply(
        &dsl,
        &set_threshold("exits[0].distance_pct", Decimal::new(50, 3)),
    )
    .expect_err("a scale-only difference is not a change");

    assert!(
        matches!(&err, MutationError::NoChange { path } if path == "exits[0].distance_pct"),
        "expected NoChange on the stop leaf, got {err:?}"
    );
}

#[test]
fn fixing_a_swept_leaf_at_its_current_value_is_a_real_change() {
    // A `Sweep` leaf pinned to one of its own points is NOT a no-op: it removes
    // the sweep, and `validate()` rejects every `Sweep`, so the pin is the whole
    // reason the candidate compiles at all.
    let mut dsl = rsi_oversold_strategy();
    dsl.entry = Condition::Compare {
        lhs: ValueSource::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Rsi {
                period: SweepableValue::Sweep {
                    start: 10,
                    end: 20,
                    step: 2,
                },
            },
        },
        op: Comparator::Lt,
        rhs: ValueSource::Constant {
            value: Decimal::new(30, 0),
        },
    };

    let candidate = apply(&dsl, &set_period("entry.lhs.indicator.rsi.period", 14))
        .expect("pinning a swept leaf is a real change, not a no-op");

    assert_eq!(
        entry_rsi_period(candidate.dsl()),
        &SweepableValue::Fixed(14),
        "the swept leaf must come back fixed"
    );
}

#[test]
fn a_no_change_error_survives_a_serde_round_trip() {
    // `CoachFailure::InapplicableMutation` persists this error verbatim (w2/w3),
    // so a new variant only reaches the audit trail if it round-trips typed.
    let err = apply(
        &rsi_oversold_strategy(),
        &set_period("entry.lhs.indicator.rsi.period", 14),
    )
    .expect_err("a no-op mutation is inapplicable");

    let json = serde_json::to_string(&err).expect("serialize the typed reason");
    let back: MutationError = serde_json::from_str(&json).expect("read the typed reason back");

    assert_eq!(back, err, "the recorded reason must survive the round trip");
    assert!(
        json.contains("no_change"),
        "the variant tag stays snake_case, like every other: {json}"
    );
}

// ---------------------------------------------------------------------------
// 3. No panic paths
// ---------------------------------------------------------------------------

#[test]
fn malformed_and_out_of_range_paths_return_errors_rather_than_panicking() {
    let dsl = rsi_oversold_strategy();

    let hostile = [
        "",
        ".",
        "..",
        "[0]",
        "entry.",
        "entry..lhs",
        "entry.and[0].lhs.indicator.rsi.period", // entry is not an And
        "exits[99].distance_pct",                // index past the end
        "exits[-1].distance_pct",                // negative index
        "exits[abc].distance_pct",               // non-numeric index
        "exits[0].distance_pct.extra",           // past a leaf
        "filters[0].lhs.indicator.rsi.period",   // empty filters list
        "risk.risk_per_trade_pct.value",
        "ENTRY.LHS.INDICATOR.RSI.PERIOD", // paths are case-sensitive
        "entry.lhs.indicator.rsi.period ", // trailing space
        " entry.lhs.indicator.rsi.period",
    ];

    for path in hostile {
        let outcome = apply(&dsl, &set_period(path, 9));
        assert!(outcome.is_err(), "`{path}` must be rejected, not applied");
    }
}

// ---------------------------------------------------------------------------
// 4. The input DSL is never partially mutated
// ---------------------------------------------------------------------------

#[test]
fn a_failing_mutation_leaves_the_input_untouched() {
    let dsl = rsi_oversold_strategy();
    let before = serde_json::to_string(&dsl).expect("serialize the input strategy");

    let failing = [
        set_period("entry.lhs.indicator.rsi.nope", 21), // unknown path
        set_threshold("entry.lhs.indicator.rsi.period", Decimal::ONE), // type mismatch
        set_period("entry.lhs.indicator.rsi.period", 0), // validation failure
    ];

    for mutation in &failing {
        let outcome = apply(&dsl, mutation);
        assert!(outcome.is_err(), "{mutation:?} was expected to fail");
        assert_eq!(
            serde_json::to_string(&dsl).expect("re-serialize the input strategy"),
            before,
            "the input DSL must be byte-identical after a failed {mutation:?}"
        );
    }

    // A SUCCEEDING mutation must not touch the input either -- it returns a new
    // document (ADR-0010: mutations apply to a version's immutable DSL).
    let candidate = apply(&dsl, &set_period("entry.lhs.indicator.rsi.period", 21))
        .expect("the happy-path mutation applies");
    assert_eq!(
        serde_json::to_string(&dsl).expect("re-serialize the input strategy"),
        before,
        "a successful mutation must leave the input document untouched"
    );
    assert_ne!(
        serde_json::to_string(candidate.dsl()).expect("serialize the candidate"),
        before,
        "the candidate must differ from the input"
    );
}
