//! r6.s2.w1 — the schema-1.6.0 acceptance suite (AC-1).
//!
//! Schema **1.6.0** is additive (ADR-0024, ADR-0033): a new optional
//! `StrategyDsl.setup` and a new `ExitRule::StructureStop`. A document without
//! a setup deserializes to its default, so the bump is a minor one with an
//! identity migration. This suite pins the facts that make the bump honest:
//!
//! 1. **A 1.5.0 document loads unchanged** and serializes byte-identically
//!    (no `setup` key).
//! 2. **A 1.6.0 two-sided document round-trips** through serde and through
//!    `Migrator::v1().load`.
//! 3. **Each shape rule S1–S6 is refused** with its code, path and message.
//! 4. **A setup compiles**: it needs the higher timeframe and registers every
//!    side indicator.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{
    Comparator, Condition, Direction, ExitRule, FieldError, IndicatorSpec, Migrator, PriceField,
    RiskParams, SchemaVersion, Series, Setup, SetupSide, StrategyDsl, SweepableValue,
    ValidationCode, ValueSource, compile, validate,
};
use rust_decimal::Decimal;

fn dec(value: &str) -> Decimal {
    Decimal::from_str_exact(value).expect("a decimal literal")
}

fn price(field: PriceField) -> ValueSource {
    ValueSource::Price {
        series: Series::Primary,
        field,
    }
}

fn sma(period: u32) -> ValueSource {
    ValueSource::Indicator {
        series: Series::Primary,
        spec: IndicatorSpec::Sma {
            period: SweepableValue::Fixed(period),
            source: PriceField::Close,
        },
    }
}

fn htf_close() -> ValueSource {
    ValueSource::Price {
        series: Series::Htf,
        field: PriceField::Close,
    }
}

fn compare(lhs: ValueSource, op: Comparator, rhs: ValueSource) -> Condition {
    Condition::Compare { lhs, op, rhs }
}

fn constant(value: &str) -> ValueSource {
    ValueSource::Constant { value: dec(value) }
}

/// The long side: arm on the higher-timeframe close, dip under the SMA 50,
/// trigger on a close back above the open, with a stop anchor.
fn long_side() -> SetupSide {
    SetupSide {
        arm: compare(htf_close(), Comparator::Gt, constant("1")),
        dip: compare(price(PriceField::Close), Comparator::Lt, sma(50)),
        trigger: compare(
            price(PriceField::Close),
            Comparator::Gt,
            price(PriceField::Open),
        ),
        stop_anchor: Some(compare(
            price(PriceField::Low),
            Comparator::Lt,
            price(PriceField::Open),
        )),
    }
}

/// The short side, written in full (nothing is mirrored).
fn short_side() -> SetupSide {
    SetupSide {
        arm: compare(htf_close(), Comparator::Lt, constant("1000000")),
        dip: compare(price(PriceField::Close), Comparator::Gt, sma(20)),
        trigger: compare(
            price(PriceField::Close),
            Comparator::Lt,
            price(PriceField::Open),
        ),
        stop_anchor: None,
    }
}

fn structure_stop(buffer: &str) -> ExitRule {
    ExitRule::StructureStop {
        buffer_pct: SweepableValue::Fixed(dec(buffer)),
    }
}

/// A valid two-sided setup strategy with a structure stop and a take profit.
fn two_sided() -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "two sided setup".to_owned(),
        direction: Direction::Long,
        entry: compare(price(PriceField::Close), Comparator::Gt, constant("0")),
        filters: vec![],
        exits: vec![
            structure_stop("0.0005"),
            ExitRule::TakeProfit {
                target_r: SweepableValue::Fixed(dec("2")),
            },
        ],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(dec("0.01")),
            max_leverage: SweepableValue::Fixed(Decimal::from(3)),
        },
        setup: Some(Box::new(Setup {
            expiry_bars: SweepableValue::Fixed(16),
            long: Some(long_side()),
            short: Some(short_side()),
        })),
    }
}

/// A valid long-only setup strategy.
fn long_only() -> StrategyDsl {
    let mut dsl = two_sided();
    dsl.setup = Some(Box::new(Setup {
        expiry_bars: SweepableValue::Fixed(16),
        long: Some(long_side()),
        short: None,
    }));
    dsl
}

/// The errors a document is refused with.
fn refusal(dsl: &StrategyDsl) -> Vec<FieldError> {
    validate(dsl)
        .expect_err("the document must be refused")
        .into_errors()
}

/// Assert that exactly one error carries `path`, with `code` and the exact
/// `message`.
fn assert_refused(dsl: &StrategyDsl, path: &str, code: ValidationCode, message: &str) {
    let errors = refusal(dsl);
    let hits: Vec<&FieldError> = errors.iter().filter(|e| e.path == path).collect();
    assert_eq!(hits.len(), 1, "one error at {path}; all errors: {errors:?}");
    assert_eq!(hits[0].code, code, "code at {path}");
    assert_eq!(hits[0].message, message, "message at {path}");
}

// ---------------------------------------------------------------------------
// 1. A 1.5.0 document loads unchanged
// ---------------------------------------------------------------------------

/// A canonical 1.5.0 document: no setup, every defaulted field written.
const DOC_1_5_0: &str = r#"{
  "schema_version": "1.5.0",
  "name": "sma cross migration fixture",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": {
      "type": "Indicator",
      "series": "primary",
      "spec": { "indicator": "Sma", "period": 20, "source": "Close" }
    },
    "op": "Gt",
    "rhs": {
      "type": "Indicator",
      "series": "primary",
      "spec": { "indicator": "Sma", "period": 50, "source": "Close" }
    }
  },
  "filters": [],
  "exits": [{ "type": "StopLoss", "distance_pct": "0.05" }],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// A 1.5.0 strategy loads unchanged under 1.7.0 and serializes with no
/// `setup` key: the only difference is the `schema_version` string.
#[test]
fn a_1_5_0_document_loads_unchanged_under_1_6_0() {
    let loaded = Migrator::v1()
        .load(DOC_1_5_0)
        .expect("a 1.5.0 document loads");

    assert_eq!(loaded.from.to_string(), "1.5.0");
    assert!(
        loaded.migrated,
        "the 1.5.0 → 1.6.0 identity step was applied"
    );
    assert_eq!(
        loaded.dsl_original, DOC_1_5_0,
        "dsl_original stays byte-verbatim"
    );
    assert_eq!(loaded.dsl.schema_version, SchemaVersion::CURRENT);
    assert_eq!(loaded.dsl.schema_version.to_string(), "1.7.0");
    assert_eq!(loaded.dsl.setup, None, "an absent setup reads as none");

    let reserialized = serde_json::to_value(&loaded.dsl).expect("re-serializes");
    let mut expected: serde_json::Value =
        serde_json::from_str(DOC_1_5_0).expect("the fixture is valid JSON");
    expected["schema_version"] = serde_json::json!("1.7.0");
    assert_eq!(
        reserialized, expected,
        "the identity step changes nothing but the schema_version string"
    );
    compile(&validate(&loaded.dsl).expect("validates")).expect("compiles");
}

/// A strategy with no setup serializes without a `setup` key, so every
/// earlier document keeps its canonical bytes.
#[test]
fn a_strategy_without_a_setup_serializes_byte_identically() {
    let loaded = Migrator::v1().load(DOC_1_5_0).expect("loads");
    let now = serde_json::to_string(&loaded.dsl).expect("serializes");
    assert!(!now.contains("setup"), "no setup key: {now}");

    let mut before = loaded.dsl.clone();
    before.schema_version = "1.5.0".parse().expect("a version string");
    let before = serde_json::to_string(&before).expect("serializes");
    assert_eq!(
        now.replace("1.7.0", "1.5.0"),
        before,
        "the bytes differ only in the version string"
    );
}

/// The whole registered chain is identity, end to end.
#[test]
fn the_whole_chain_migrates_identity_to_1_6_0() {
    let doc = DOC_1_5_0
        .replace("\"1.5.0\"", "\"1.0.0\"")
        .replace(", \"source\": \"Close\"", "");
    // A 1.0.0 document cannot name Sma; use an EMA entry instead.
    let doc = doc.replace("\"Sma\"", "\"Ema\"");
    let loaded = Migrator::v1().load(&doc).expect("a 1.0.0 document loads");
    assert_eq!(loaded.from.to_string(), "1.0.0");
    assert_eq!(loaded.dsl.schema_version.to_string(), "1.7.0");
    compile(&validate(&loaded.dsl).expect("validates")).expect("compiles");
}

/// A future minor is still refused, naming the new pair.
#[test]
fn a_future_minor_is_refused() {
    let doc = DOC_1_5_0.replace("\"1.5.0\"", "\"1.8.0\"");
    let err = Migrator::v1()
        .load(&doc)
        .expect_err("a future version must refuse");
    let pulse::LoadError::FutureVersion { found, current } = err else {
        panic!("expected FutureVersion, got {err:?}");
    };
    assert_eq!(found.to_string(), "1.8.0");
    assert_eq!(current.to_string(), "1.7.0");
}

// ---------------------------------------------------------------------------
// 2. A two-sided document round-trips
// ---------------------------------------------------------------------------

/// A two-sided setup with a structure stop round-trips value-equal through
/// serde and through the version-safe loader.
#[test]
fn a_two_sided_document_round_trips() {
    let dsl = two_sided();
    validate(&dsl).expect("the two-sided document validates");

    let json = serde_json::to_string(&dsl).expect("serializes");
    let back: StrategyDsl = serde_json::from_str(&json).expect("deserializes");
    assert_eq!(back, dsl, "serde round trip is value-equal");

    let loaded = Migrator::v1().load(&json).expect("loads");
    assert!(!loaded.migrated, "a CURRENT document needs no migration");
    assert_eq!(loaded.dsl, dsl, "the loader round trip is value-equal");
    assert_eq!(loaded.dsl_original, json, "dsl_original stays verbatim");
}

/// The wire shape of the new variant and fields.
#[test]
fn the_setup_wire_shape_is_exact() {
    let json = serde_json::to_value(two_sided()).expect("serializes");
    assert_eq!(
        json["exits"][0],
        serde_json::json!({"type": "StructureStop", "buffer_pct": "0.0005"})
    );
    assert_eq!(json["setup"]["expiry_bars"], serde_json::json!(16));
    assert!(json["setup"]["long"]["stop_anchor"].is_object());
    assert!(
        json["setup"]["short"].get("stop_anchor").is_none(),
        "an absent stop_anchor is omitted"
    );
    let one = serde_json::to_value(long_only()).expect("serializes");
    assert!(
        one["setup"].get("short").is_none(),
        "an absent side is omitted"
    );
}

// ---------------------------------------------------------------------------
// 3. S1–S6
// ---------------------------------------------------------------------------

/// S1: a setup has at least one side.
#[test]
fn s1_a_setup_with_no_side_is_refused() {
    let mut dsl = two_sided();
    dsl.setup = Some(Box::new(Setup {
        expiry_bars: SweepableValue::Fixed(16),
        long: None,
        short: None,
    }));
    assert_refused(
        &dsl,
        "setup",
        ValidationCode::InvalidSetup,
        "a setup must declare a long side, a short side, or both",
    );
}

/// S2: a one-sided setup's direction must be its side.
#[test]
fn s2_a_one_sided_setup_must_match_its_direction() {
    let mut dsl = long_only();
    dsl.direction = Direction::Short;
    assert_refused(
        &dsl,
        "direction",
        ValidationCode::InvalidSetup,
        "a one-sided setup's direction must be its side (long)",
    );

    let mut dsl = long_only();
    dsl.setup = Some(Box::new(Setup {
        expiry_bars: SweepableValue::Fixed(16),
        long: None,
        short: Some(short_side()),
    }));
    assert_refused(
        &dsl,
        "direction",
        ValidationCode::InvalidSetup,
        "a one-sided setup's direction must be its side (short)",
    );
}

/// S3: a two-sided setup's direction is long.
#[test]
fn s3_a_two_sided_setup_must_have_direction_long() {
    let mut dsl = two_sided();
    dsl.direction = Direction::Short;
    assert_refused(
        &dsl,
        "direction",
        ValidationCode::InvalidSetup,
        "a two-sided setup's direction must be long; each trade takes the side that triggered",
    );
}

/// S4: a structure stop needs a setup.
#[test]
fn s4_a_structure_stop_without_a_setup_is_refused() {
    let mut dsl = two_sided();
    dsl.setup = None;
    assert_refused(
        &dsl,
        "exits[0]",
        ValidationCode::InvalidSetup,
        "a StructureStop exit requires a setup",
    );
}

/// S5: `expiry_bars` lies in 1..=2000.
#[test]
fn s5_expiry_bars_out_of_range_is_refused() {
    for bad in [0u32, 2001] {
        let mut dsl = long_only();
        if let Some(setup) = dsl.setup.as_mut() {
            setup.expiry_bars = SweepableValue::Fixed(bad);
        }
        assert_refused(
            &dsl,
            "setup.expiry_bars",
            ValidationCode::FieldRange,
            "setup expiry_bars must be between 1 and 2000",
        );
    }
    for good in [1u32, 2000] {
        let mut dsl = long_only();
        if let Some(setup) = dsl.setup.as_mut() {
            setup.expiry_bars = SweepableValue::Fixed(good);
        }
        validate(&dsl).expect("the bound itself is valid");
    }
}

/// S6: `buffer_pct` lies in [0, 0.05].
#[test]
fn s6_buffer_pct_out_of_range_is_refused() {
    for bad in ["-0.0001", "0.0501"] {
        let mut dsl = long_only();
        dsl.exits[0] = structure_stop(bad);
        assert_refused(
            &dsl,
            "exits[0].buffer_pct",
            ValidationCode::FieldRange,
            "structure-stop buffer_pct must be between 0 and 0.05",
        );
    }
    for good in ["0", "0.05"] {
        let mut dsl = long_only();
        dsl.exits[0] = structure_stop(good);
        validate(&dsl).expect("the bound itself is valid");
    }
}

/// S7: neither numeric field may be a sweep.
#[test]
fn s7_a_sweep_is_refused_on_both_numeric_fields() {
    let mut dsl = long_only();
    if let Some(setup) = dsl.setup.as_mut() {
        setup.expiry_bars = SweepableValue::Sweep {
            start: 4,
            end: 16,
            step: 4,
        };
    }
    let errors = refusal(&dsl);
    assert!(
        errors
            .iter()
            .any(|e| e.path == "setup.expiry_bars" && e.code == ValidationCode::SweepUnsupported),
        "{errors:?}"
    );

    let mut dsl = long_only();
    dsl.exits[0] = ExitRule::StructureStop {
        buffer_pct: SweepableValue::Sweep {
            start: dec("0"),
            end: dec("0.01"),
            step: dec("0.005"),
        },
    };
    let errors = refusal(&dsl);
    assert!(
        errors
            .iter()
            .any(|e| e.path == "exits[0].buffer_pct" && e.code == ValidationCode::SweepUnsupported),
        "{errors:?}"
    );
}

/// A side condition is validated like any condition, at its own path.
#[test]
fn a_bad_side_condition_is_refused_at_its_path() {
    let mut dsl = long_only();
    if let Some(side) = dsl.setup.as_mut().and_then(|s| s.long.as_mut()) {
        side.dip = Condition::And { conditions: vec![] };
        side.stop_anchor = Some(Condition::Or { conditions: vec![] });
    }
    let errors = refusal(&dsl);
    assert!(
        errors
            .iter()
            .any(|e| e.path == "setup.long.dip.and" && e.code == ValidationCode::EmptyConjunction),
        "{errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| e.path == "setup.long.stop_anchor.or"
                && e.code == ValidationCode::EmptyConjunction),
        "{errors:?}"
    );
}

/// The stop-family rules count a `StructureStop`: it satisfies a take
/// profit's need for a stop, and it is exclusive with `StopLoss`.
#[test]
fn a_structure_stop_joins_the_stop_family() {
    let mut dsl = long_only();
    dsl.exits.push(ExitRule::StopLoss {
        distance_pct: SweepableValue::Fixed(dec("0.05")),
    });
    let errors = refusal(&dsl);
    assert!(
        errors
            .iter()
            .any(|e| e.code == ValidationCode::DuplicateExit
                && e.message.contains("StopLoss, AtrStop or StructureStop")),
        "{errors:?}"
    );

    let mut dsl = long_only();
    dsl.exits = vec![ExitRule::TakeProfit {
        target_r: SweepableValue::Fixed(dec("2")),
    }];
    let errors = refusal(&dsl);
    assert!(
        errors.iter().any(|e| e.code == ValidationCode::TakeProfitWithoutStop
            && e.message
                == "a TakeProfit exit requires a StopLoss, AtrStop or StructureStop in the same strategy (R is undefined without a stop)"),
        "{errors:?}"
    );
}

/// A document may write `null` for a setup, side or stop anchor: it reads as
/// absent (the write-path walk is unit-tested in `validate.rs`).
#[test]
fn null_reads_as_absent() {
    let mut raw = serde_json::to_value(two_sided()).expect("serializes");
    raw["setup"]["short"] = serde_json::Value::Null;
    raw["setup"]["long"]["stop_anchor"] = serde_json::Value::Null;
    let parsed: StrategyDsl = serde_json::from_value(raw.clone()).expect("null reads as absent");
    assert_eq!(parsed.setup.as_ref().and_then(|s| s.short.as_ref()), None);
    let long = parsed.setup.as_ref().and_then(|s| s.long.as_ref());
    assert_eq!(long.map(|side| side.stop_anchor.clone()), Some(None));

    let mut raw = serde_json::to_value(long_only()).expect("serializes");
    raw["setup"] = serde_json::Value::Null;
    let parsed: StrategyDsl = serde_json::from_value(raw).expect("a null setup reads as absent");
    assert_eq!(parsed.setup, None);
}

// ---------------------------------------------------------------------------
// 4. Compile
// ---------------------------------------------------------------------------

/// A setup compiles: it needs the higher timeframe, and every side indicator
/// is registered (an `Sma{50}` used only in `setup.long.dip` is required).
#[test]
fn a_setup_compiles_and_registers_every_side_indicator() {
    let dsl = long_only();
    let compiled = compile(&validate(&dsl).expect("validates")).expect("compiles");

    assert!(compiled.needs_htf(), "a setup needs the higher timeframe");
    let sma50 = IndicatorSpec::Sma {
        period: SweepableValue::Fixed(50),
        source: PriceField::Close,
    };
    assert!(
        compiled.required_indicators().contains(&sma50),
        "Sma(50) is used only in setup.long.dip: {:?}",
        compiled.required_indicators()
    );
    let setup = compiled.setup().expect("the compiled setup");
    assert_eq!(setup.expiry_bars, 16);
    assert!(setup.long.is_some());
    assert!(setup.short.is_none());
}

/// A strategy without a setup does not need the higher timeframe.
#[test]
fn a_strategy_without_a_setup_does_not_need_the_higher_timeframe() {
    let loaded = Migrator::v1().load(DOC_1_5_0).expect("loads");
    let compiled = compile(&validate(&loaded.dsl).expect("validates")).expect("compiles");
    assert!(!compiled.needs_htf());
    assert!(compiled.setup().is_none());
}
