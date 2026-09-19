//! AC-4 — determinism and serde stability of the mutation framework
//! (r1.s2.w1, NFR-2 / ADR-0021).
//!
//! Two properties, both load-bearing for what comes after this item:
//!
//!   1. **Determinism.** The same DSL plus the same mutation yields a
//!      **byte-identical** candidate document, every time. `r1.s4` re-runs
//!      `apply()` at accept (audit C4) and re-backtests the result; if the
//!      candidate's bytes moved between the proposal and the accept, the child
//!      version's `dsl_original` and every fingerprint downstream would move with
//!      it. This is the same no-`f64` discipline `determinism_guard.rs` enforces
//!      over the backtester, applied to the coach's transform.
//!   2. **Serde round-trip.** A [`Mutation`] survives a JSON round-trip
//!      losslessly — `r1.s2.w2` persists it typed and `r1.s4`'s modify path reads
//!      it back, edits its parameters, and re-applies it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{
    Comparator, Condition, Direction, ExitRule, IndicatorSpec, Mutation, ParamValue, RiskParams,
    SchemaVersion, Series, StrategyDsl, SweepableValue, ValueSource, apply,
};
use rust_decimal::Decimal;

/// The canonical RSI-oversold fixture.
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

// ---------------------------------------------------------------------------
// 1. Determinism
// ---------------------------------------------------------------------------

#[test]
fn the_same_mutation_yields_a_byte_identical_candidate() {
    let dsl = rsi_oversold_strategy();
    let mutation = Mutation::SetParam {
        path: "entry.lhs.indicator.rsi.period".to_owned(),
        new_value: ParamValue::Period { value: 21 },
    };

    let first = serde_json::to_string(apply(&dsl, &mutation).expect("the mutation applies").dsl())
        .expect("serialize the first candidate");

    for round in 1..100 {
        let again =
            serde_json::to_string(apply(&dsl, &mutation).expect("the mutation applies").dsl())
                .expect("serialize a later candidate");
        assert_eq!(
            again, first,
            "round {round} produced a different candidate document"
        );
    }
}

#[test]
fn a_decimal_mutation_is_exact_to_the_last_place() {
    let dsl = rsi_oversold_strategy();
    // A value no binary float can hold exactly -- the point of `Decimal` (NFR-2).
    let precise = Decimal::new(123_456_789, 10); // 0.0123456789
    let mutation = Mutation::SetParam {
        path: "exits[0].distance_pct".to_owned(),
        new_value: ParamValue::Threshold { value: precise },
    };

    let candidate = apply(&dsl, &mutation).expect("a 1.23456789% stop is a valid mutation");

    match &candidate.dsl().exits[0] {
        ExitRule::StopLoss { distance_pct } => {
            assert_eq!(
                distance_pct,
                &SweepableValue::Fixed(precise),
                "the written value must be exact, not rounded"
            );
        }
        other => panic!("exits[0] is not a StopLoss: {other:?}"),
    }

    // And it serializes as a STRING, not a float literal -- the serde-with-str
    // discipline that keeps the bytes stable across platforms.
    let json = serde_json::to_string(candidate.dsl()).expect("serialize the candidate");
    assert!(
        json.contains("\"0.0123456789\""),
        "the decimal must round-trip as an exact string: {json}"
    );
}

#[test]
fn two_candidates_from_the_same_inputs_are_equal() {
    let dsl = rsi_oversold_strategy();
    let mutation = Mutation::SetParam {
        path: "risk.risk_per_trade_pct".to_owned(),
        new_value: ParamValue::Threshold {
            value: Decimal::new(2, 2),
        },
    };

    assert_eq!(
        apply(&dsl, &mutation).expect("applies"),
        apply(&dsl, &mutation).expect("applies"),
        "the same inputs must produce equal candidates"
    );
}

#[test]
fn the_mutation_framework_contains_no_f64() {
    // NFR-2: no `f64` reaches a value that ends up in a persisted document or a
    // fingerprint. `determinism_guard.rs` makes this scan over the backtester;
    // this is the same tripwire over the coach's transform.
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/domain/dsl/mutate.rs"
    ))
    .expect("read mutate.rs");

    let offenders: Vec<&str> = source
        .lines()
        .filter(|line| {
            !line.trim_start().starts_with("//") && !line.trim_start().starts_with("//!")
        })
        .filter(|line| line.contains("f64") || line.contains("f32"))
        .collect();

    assert!(
        offenders.is_empty(),
        "mutate.rs must contain no binary float: {offenders:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Serde round-trip
// ---------------------------------------------------------------------------

#[test]
fn a_period_mutation_round_trips_losslessly() {
    let mutation = Mutation::SetParam {
        path: "entry.and[0].not.lhs.indicator.macd.fast".to_owned(),
        new_value: ParamValue::Period { value: 12 },
    };

    let json = serde_json::to_string(&mutation).expect("serialize the mutation");
    let back: Mutation = serde_json::from_str(&json).expect("deserialize the mutation");

    assert_eq!(
        back, mutation,
        "a period mutation must round-trip value-equal"
    );
}

#[test]
fn a_threshold_mutation_round_trips_losslessly() {
    let mutation = Mutation::SetParam {
        path: "exits[0].distance_pct".to_owned(),
        new_value: ParamValue::Threshold {
            value: Decimal::new(123_456_789, 10),
        },
    };

    let json = serde_json::to_string(&mutation).expect("serialize the mutation");
    let back: Mutation = serde_json::from_str(&json).expect("deserialize the mutation");

    assert_eq!(
        back, mutation,
        "a threshold mutation must round-trip value-equal, to the last decimal place"
    );
    assert!(
        json.contains("\"0.0123456789\""),
        "the decimal must persist as an exact string: {json}"
    );
}

#[test]
fn a_round_tripped_mutation_applies_identically() {
    // The property `r1.s4`'s modify-then-accept path actually depends on: what
    // comes back out of the database does what the original would have done.
    let dsl = rsi_oversold_strategy();
    let mutation = Mutation::SetParam {
        path: "entry.lhs.indicator.rsi.period".to_owned(),
        new_value: ParamValue::Period { value: 21 },
    };

    let json = serde_json::to_string(&mutation).expect("serialize the mutation");
    let back: Mutation = serde_json::from_str(&json).expect("deserialize the mutation");

    assert_eq!(
        serde_json::to_string(
            apply(&dsl, &back)
                .expect("the reloaded mutation applies")
                .dsl()
        )
        .expect("serialize the reloaded candidate"),
        serde_json::to_string(apply(&dsl, &mutation).expect("the original applies").dsl())
            .expect("serialize the original candidate"),
        "a reloaded mutation must produce a byte-identical candidate"
    );
}
