//! r6.s1.w1 — the schema-1.5.0 acceptance suite (AC-3).
//!
//! Schema **1.5.0** is additive (ADR-0024 and its Cipher addendum): a new
//! indicator variant, `IndicatorSpec::Sma { period, source }`, and two new
//! arithmetic operations, `ArithOp::Max` and `ArithOp::Min`. Old strategies
//! still deserialize, so the bump is a minor one with an identity migration.
//! This suite pins the facts that make the bump honest:
//!
//! 1. **The served schema names the current version** (1.6.0 since r6.s2.w1) and lists the
//!    loadable spellings.
//! 2. **The wire shapes are the spec's**, including `Sma`'s defaulted `source`.
//! 3. **A 1.4.0 document migrates by identity**, byte-verbatim in
//!    `dsl_original`, with `schema_version` the only difference.
//! 4. **A 1.5.0 strategy using the new variants** validates, renders, compiles
//!    and round-trips.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use pulse::{
    ArithOp, CandleStore, Comparator, CompiledValue, Condition, Direction, EvalContext, ExitRule,
    IndicatorEngine, IndicatorSpec, Migrator, Pair, PriceField, RiskParams, SchemaVersion, Series,
    StrategyDsl, SweepableValue, Timeframe, ValueSource, compile, render, validate,
};
use rust_decimal::Decimal;

fn dec(value: &str) -> Decimal {
    Decimal::from_str_exact(value).expect("a decimal literal")
}

fn sma(period: u32, source: PriceField) -> IndicatorSpec {
    IndicatorSpec::Sma {
        period: SweepableValue::Fixed(period),
        source,
    }
}

fn indicator(spec: IndicatorSpec) -> ValueSource {
    ValueSource::Indicator {
        series: Series::Primary,
        spec,
    }
}

fn constant(value: &str) -> ValueSource {
    ValueSource::Constant { value: dec(value) }
}

/// The skeleton every strategy here shares: only `entry` varies.
fn strategy_with_entry(name: &str, entry: Condition) -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: name.to_owned(),
        direction: Direction::Long,
        entry,
        filters: vec![],
        exits: vec![ExitRule::StopLoss {
            distance_pct: SweepableValue::Fixed(dec("0.05")),
        }],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(dec("0.01")),
            max_leverage: SweepableValue::Fixed(Decimal::from(3)),
        },
        setup: None,
    }
}

// ---------------------------------------------------------------------------
// 1. The served schema
// ---------------------------------------------------------------------------

/// The served schema names 1.6.0 last and lists exactly the seven loadable
/// spellings.
#[test]
fn the_served_version_enum_is_the_loadable_set_and_names_1_5_0() {
    let schema = serde_json::to_value(schemars::schema_for!(StrategyDsl)).expect("serializes");
    assert_eq!(
        schema["properties"]["schema_version"]["enum"],
        serde_json::json!([
            "1.0.0", "1.1.0", "1.2.0", "1.3.0", "1.4.0", "1.5.0", "1.6.0"
        ]),
        "the served enum lists every loadable spelling and nothing else"
    );
    assert_eq!(SchemaVersion::CURRENT.to_string(), "1.6.0");
    assert_eq!(pulse::DSL_SCHEMA_VERSION, "1.6.0");
}

/// The served schema advertises the `Sma` variant, tagged by its `indicator`
/// name.
#[test]
fn the_served_schema_advertises_the_sma_variant() {
    let doc = serde_json::to_value(schemars::schema_for!(StrategyDsl)).expect("serializes");
    let members = doc["$defs"]["IndicatorSpec"]["oneOf"]
        .as_array()
        .expect("IndicatorSpec is a tagged enum with oneOf members");
    assert!(
        members
            .iter()
            .any(|m| m["properties"]["indicator"]["const"] == serde_json::json!("Sma")),
        "the served schema advertises Sma; members were: {}",
        serde_json::to_string(members).unwrap_or_default()
    );
}

// ---------------------------------------------------------------------------
// 2. The wire shape
// ---------------------------------------------------------------------------

/// `Sma` serializes with its source written out, and an absent `source`
/// deserializes to close (writes always emit it).
#[test]
fn the_sma_wire_shape_is_exact_and_source_defaults_to_close() {
    let spec = sma(50, PriceField::Close);
    let json = serde_json::to_string(&spec).expect("serialize");
    assert_eq!(
        json, r#"{"indicator":"Sma","period":50,"source":"Close"}"#,
        "writes always emit the source"
    );
    let back: IndicatorSpec = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, spec);

    let absent: IndicatorSpec =
        serde_json::from_str(r#"{"indicator":"Sma","period":50}"#).expect("source is optional");
    assert_eq!(absent, spec, "an absent source reads as close");

    let high = sma(20, PriceField::High);
    let json = serde_json::to_string(&high).expect("serialize");
    assert_eq!(json, r#"{"indicator":"Sma","period":20,"source":"High"}"#);
    let back: IndicatorSpec = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, high);
}

// ---------------------------------------------------------------------------
// 3. The identity migration
// ---------------------------------------------------------------------------

/// A canonical 1.4.0 document — the wire form the engine wrote before this
/// bump, every defaulted field written explicitly.
const DOC_1_4_0: &str = r#"{
  "schema_version": "1.4.0",
  "name": "ema cross migration fixture",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": {
      "type": "Indicator",
      "series": "primary",
      "spec": { "indicator": "Ema", "period": 20 }
    },
    "op": "Gt",
    "rhs": {
      "type": "Indicator",
      "series": "primary",
      "spec": { "indicator": "Ema", "period": 50 }
    }
  },
  "filters": [],
  "exits": [{ "type": "StopLoss", "distance_pct": "0.05" }],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// A 1.4.0 strategy loads unchanged under 1.6.0: `from` is the authored
/// version, `dsl_original` stays byte-verbatim, the typed document is
/// `CURRENT`, and the re-serialization differs only in the `schema_version`
/// string.
#[test]
fn a_1_4_0_document_loads_unchanged_under_1_5_0() {
    let loaded = Migrator::v1()
        .load(DOC_1_4_0)
        .expect("a 1.4.0 document loads");

    assert_eq!(
        loaded.from.to_string(),
        "1.4.0",
        "`from` is the authored version"
    );
    assert!(
        loaded.migrated,
        "the 1.4.0 → 1.6.0 identity chain was applied"
    );
    assert_eq!(
        loaded.dsl_original, DOC_1_4_0,
        "dsl_original stays byte-verbatim"
    );
    assert_eq!(loaded.dsl.schema_version, SchemaVersion::CURRENT);
    assert_eq!(loaded.dsl.schema_version.to_string(), "1.6.0");

    let reserialized = serde_json::to_value(&loaded.dsl).expect("re-serializes");
    let mut expected: serde_json::Value =
        serde_json::from_str(DOC_1_4_0).expect("the fixture is valid JSON");
    expected["schema_version"] = serde_json::json!("1.6.0");
    assert_eq!(
        reserialized, expected,
        "the identity step changes nothing but the schema_version string"
    );
    compile(&validate(&loaded.dsl).expect("validates")).expect("compiles");
}

/// The whole registered chain is identity, end to end: a 1.0.0 document reaches
/// 1.6.0 with its meaning intact.
#[test]
fn the_whole_chain_migrates_identity_to_1_5_0() {
    let doc = DOC_1_4_0.replace("\"1.4.0\"", "\"1.0.0\"");
    let loaded = Migrator::v1().load(&doc).expect("a 1.0.0 document loads");
    assert_eq!(loaded.from.to_string(), "1.0.0");
    assert_eq!(loaded.dsl.schema_version.to_string(), "1.6.0");
    compile(&validate(&loaded.dsl).expect("validates")).expect("compiles");
}

/// A future minor is still refused, naming the new pair.
#[test]
fn a_future_minor_is_refused() {
    let doc = DOC_1_4_0.replace("\"1.4.0\"", "\"1.7.0\"");
    let err = Migrator::v1()
        .load(&doc)
        .expect_err("a future version must refuse");
    let pulse::LoadError::FutureVersion { found, current } = err else {
        panic!("expected FutureVersion, got {err:?}");
    };
    assert_eq!(found.to_string(), "1.7.0");
    assert_eq!(current.to_string(), "1.6.0");
}

// ---------------------------------------------------------------------------
// 4. A 1.5.0 strategy using the new variants
// ---------------------------------------------------------------------------

/// `close > sma(50, close)` — the simplest use of the new indicator.
fn close_above_sma_strategy() -> StrategyDsl {
    strategy_with_entry(
        "close above sma 1.5.0",
        Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            },
            op: Comparator::Gt,
            rhs: indicator(sma(50, PriceField::Close)),
        },
    )
}

/// A 1.5.0 strategy with `Sma` validates, renders, compiles and survives a
/// serialize → load round trip as the same typed document.
#[test]
fn a_sma_strategy_validates_renders_compiles_and_round_trips() {
    let dsl = close_above_sma_strategy();
    let validated = validate(&dsl).expect("the strategy validates");
    compile(&validated).expect("the strategy compiles");

    assert_eq!(render::strategy(&dsl).entry, "close > sma(50,close)");

    let json = serde_json::to_string(&dsl).expect("serializes");
    let loaded = Migrator::v1().load(&json).expect("loads");
    assert!(!loaded.migrated, "a CURRENT document needs no migration");
    assert_eq!(loaded.dsl, dsl, "the round trip is value-equal");
}

/// `render::indicator` spells `Sma` as the period then the field, lowercase
/// with no spaces.
#[test]
fn sma_renders_in_the_house_style() {
    assert_eq!(
        render::indicator(&sma(50, PriceField::Close)),
        "sma(50,close)"
    );
    assert_eq!(
        render::indicator(&sma(20, PriceField::High)),
        "sma(20,high)"
    );
    assert_eq!(
        render::indicator(&sma(9, PriceField::Volume)),
        "sma(9,volume)"
    );
}

/// A zero SMA period is refused with a path naming the leaf — the same rule as
/// every other period.
#[test]
fn a_zero_sma_period_is_refused_by_validation() {
    let dsl = strategy_with_entry(
        "sma zero",
        Condition::Compare {
            lhs: indicator(sma(0, PriceField::Close)),
            op: Comparator::Gt,
            rhs: constant("1"),
        },
    );
    let errors = validate(&dsl).expect_err("a zero period must be refused");
    let text = format!("{errors:?}");
    assert!(
        text.contains("sma.period"),
        "the error names the SMA period leaf: {text}"
    );
}

// ---------------------------------------------------------------------------
// 5. `max` / `min` arithmetic
// ---------------------------------------------------------------------------

fn arith(op: ArithOp, lhs: ValueSource, rhs: ValueSource) -> ValueSource {
    ValueSource::Arith {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

fn ema(period: u32) -> ValueSource {
    indicator(IndicatorSpec::Ema {
        period: SweepableValue::Fixed(period),
    })
}

/// The Cipher ribbon's band top: the highest of eight EMAs of close, as a
/// depth-3 tree of `max` nodes.
fn band_top() -> ValueSource {
    let m = arith;
    m(
        ArithOp::Max,
        m(
            ArithOp::Max,
            m(ArithOp::Max, ema(5), ema(11)),
            m(ArithOp::Max, ema(15), ema(18)),
        ),
        m(
            ArithOp::Max,
            m(ArithOp::Max, ema(21), ema(24)),
            m(ArithOp::Max, ema(28), ema(34)),
        ),
    )
}

/// `max` and `min` serialize `snake_case`, round-trip, and the served schema
/// lists all six operations.
#[test]
fn the_max_and_min_wire_shape_is_exact_and_the_schema_lists_six_ops() {
    for (op, word) in [(ArithOp::Max, "max"), (ArithOp::Min, "min")] {
        let json = serde_json::to_string(&op).expect("serialize");
        assert_eq!(json, format!("\"{word}\""), "{op:?} serializes snake_case");
        let back: ArithOp = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, op);
    }

    let doc = serde_json::to_value(schemars::schema_for!(StrategyDsl)).expect("serializes");
    let mut ops: Vec<String> = doc["$defs"]["ArithOp"]["oneOf"]
        .as_array()
        .expect("ArithOp is an enum of const strings")
        .iter()
        .map(|m| m["const"].as_str().expect("a const string").to_owned())
        .collect();
    ops.sort();
    assert_eq!(ops, ["add", "div", "max", "min", "mul", "sub"]);
}

/// A 1.5.0 strategy with `Sma` and `max` validates, compiles, renders and
/// round-trips as the same typed document.
#[test]
fn a_sma_and_max_strategy_round_trips() {
    let dsl = strategy_with_entry(
        "close above the higher sma",
        Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            },
            op: Comparator::Gt,
            rhs: arith(
                ArithOp::Max,
                indicator(sma(50, PriceField::Close)),
                indicator(sma(20, PriceField::High)),
            ),
        },
    );
    compile(&validate(&dsl).expect("validates")).expect("compiles");
    assert_eq!(
        render::strategy(&dsl).entry,
        "close > max(sma(50,close),sma(20,high))"
    );

    let json = serde_json::to_string(&dsl).expect("serializes");
    let loaded = Migrator::v1().load(&json).expect("loads");
    assert!(!loaded.migrated);
    assert_eq!(loaded.dsl, dsl, "the round trip is value-equal");
}

/// `min` renders as a call too, and nests inside infix arithmetic.
#[test]
fn max_and_min_render_as_calls() {
    let min = arith(ArithOp::Min, ema(5), ema(11));
    assert_eq!(render::value(&min), "min(ema(5),ema(11))");
    let nested = arith(
        ArithOp::Add,
        arith(ArithOp::Max, ema(5), constant("2")),
        constant("1"),
    );
    assert_eq!(render::value(&nested), "(max(ema(5),2) + 1)");
}

/// The band top (depth 3) sits inside `MAX_EXPRESSION_DEPTH = 4`: it
/// validates and compiles; a fifth level is still refused.
#[test]
fn the_ribbon_band_top_validates_and_compiles_inside_the_depth_limit() {
    let dsl = strategy_with_entry(
        "above the band top",
        Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            },
            op: Comparator::Gt,
            rhs: band_top(),
        },
    );
    compile(&validate(&dsl).expect("the band top validates")).expect("compiles");

    // One more `max` around it makes depth 4 — still legal; two more is not.
    let four = arith(ArithOp::Max, band_top(), constant("0"));
    let five = arith(ArithOp::Max, four.clone(), constant("0"));
    let with = |rhs| {
        strategy_with_entry(
            "deep",
            Condition::Compare {
                lhs: constant("1"),
                op: Comparator::Gt,
                rhs,
            },
        )
    };
    validate(&with(four)).expect("depth 4 is the limit and is allowed");
    validate(&with(five)).expect_err("depth 5 is refused");
}

/// End to end on the SOLUSDT fixture: the engine's band top equals the largest
/// of the eight EMAs on every bar where all eight have a value.
#[test]
fn the_band_top_equals_the_largest_of_the_eight_emas_on_real_candles() {
    let base =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/solusdt-cipher-store");
    let store = CandleStore::with_base_dir(base);
    let pair = Pair::new("SOLUSDT");
    let head = store
        .read_head(&pair, Timeframe::H4)
        .expect("read HEAD")
        .expect("the fixture has a 4h HEAD");
    let series = store
        .read_snapshot(&pair, Timeframe::H4, &head)
        .expect("read");

    let periods = [5u32, 11, 15, 18, 21, 24, 28, 34];
    // Compile the band top through the real compiler: a throwaway strategy,
    // then read its entry's right-hand side.
    let dsl = strategy_with_entry(
        "band top",
        Condition::Compare {
            lhs: constant("0"),
            op: Comparator::Lt,
            rhs: band_top(),
        },
    );
    let compiled = compile(&validate(&dsl).expect("validates")).expect("compiles");
    let pulse::CompiledCondition::Compare { rhs: band_top, .. } = compiled.entry() else {
        panic!("the entry compiles to a comparison");
    };
    let mut engine =
        IndicatorEngine::from_specs(compiled.required_indicators()).expect("build engine");

    let operand = |period: u32| CompiledValue::Indicator {
        series: Series::Primary,
        spec: IndicatorSpec::Ema {
            period: SweepableValue::Fixed(period),
        },
        lag: 0,
    };
    let mut compared = 0usize;
    for candle in &series.candles {
        engine.step(candle);
        let emas: Vec<_> = periods
            .iter()
            .map(|&p| engine.current(&operand(p)))
            .collect();
        let got = engine.current(band_top);
        if emas.iter().all(Option::is_some) {
            let want = emas.iter().flatten().copied().max();
            assert_eq!(
                got, want,
                "bar {}: band top = the largest EMA",
                candle.open_time
            );
            compared += 1;
        } else {
            assert_eq!(
                got, None,
                "bar {}: no value while an EMA warms up",
                candle.open_time
            );
        }
    }
    assert!(compared > 500, "the check ran on real bars ({compared})");
}

// ---------------------------------------------------------------------------
// 6. `WaveTrend` (r6.s1.w2) — joins under the same 1.5.0
// ---------------------------------------------------------------------------

fn wavetrend(channel: u32, average: u32) -> IndicatorSpec {
    IndicatorSpec::WaveTrend {
        channel: SweepableValue::Fixed(channel),
        average: SweepableValue::Fixed(average),
    }
}

/// `WaveTrend` is an additive variant under the same 1.5.0: the served version
/// enum is unchanged, the wire shape is exact, and both lengths are required.
#[test]
fn the_wavetrend_wire_shape_is_exact_and_both_lengths_are_required() {
    let spec = wavetrend(9, 12);
    let json = serde_json::to_string(&spec).expect("serialize");
    assert_eq!(
        json,
        r#"{"indicator":"WaveTrend","channel":9,"average":12}"#
    );
    let back: IndicatorSpec = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, spec);

    for partial in [
        r#"{"indicator":"WaveTrend","channel":9}"#,
        r#"{"indicator":"WaveTrend","average":12}"#,
    ] {
        assert!(
            serde_json::from_str::<IndicatorSpec>(partial).is_err(),
            "{partial}: no length has a default"
        );
    }

    let schema = serde_json::to_value(schemars::schema_for!(StrategyDsl)).expect("serializes");
    assert_eq!(
        schema["properties"]["schema_version"]["enum"],
        serde_json::json!([
            "1.0.0", "1.1.0", "1.2.0", "1.3.0", "1.4.0", "1.5.0", "1.6.0"
        ]),
        "WaveTrend adds no schema version"
    );
}

/// A 1.5.0 strategy with `WaveTrend` on both series validates, renders,
/// compiles and survives a serialize → load round trip.
#[test]
fn a_wavetrend_strategy_validates_renders_compiles_and_round_trips() {
    let htf = ValueSource::Indicator {
        series: Series::Htf,
        spec: wavetrend(9, 12),
    };
    let dsl = strategy_with_entry(
        "pullback under way",
        Condition::Compare {
            lhs: htf,
            op: Comparator::Lt,
            rhs: constant("0"),
        },
    );
    let validated = validate(&dsl).expect("the strategy validates");
    compile(&validated).expect("the strategy compiles");

    assert_eq!(render::indicator(&wavetrend(9, 12)), "wavetrend(9,12)");
    assert_eq!(render::strategy(&dsl).entry, "h4:wavetrend(9,12) < 0");

    let json = serde_json::to_string(&dsl).expect("serializes");
    let loaded = Migrator::v1().load(&json).expect("loads");
    assert!(!loaded.migrated, "a CURRENT document needs no migration");
    assert_eq!(loaded.dsl, dsl, "the round trip is value-equal");
}

/// A zero length is refused with a path naming that leaf, one error per zero.
#[test]
fn a_zero_wavetrend_length_is_refused_by_validation() {
    for (channel, average, leaf) in [(0, 12, "wavetrend.channel"), (9, 0, "wavetrend.average")] {
        let dsl = strategy_with_entry(
            "wavetrend zero",
            Condition::Compare {
                lhs: indicator(wavetrend(channel, average)),
                op: Comparator::Lt,
                rhs: constant("0"),
            },
        );
        let errors = validate(&dsl).expect_err("a zero length must be refused");
        let text = format!("{errors:?}");
        assert!(text.contains(leaf), "the error names {leaf}: {text}");
    }
}
