//! r5.s1.w1 and r5.s1.w2 — the schema-1.4.0 acceptance suite (AC-2).
//!
//! Schema **1.4.0** is additive: one new `IndicatorSpec` variant,
//! `CandlePattern { pattern }`, whose value is TA-Lib's own integer per named
//! candlestick pattern. This suite pins the four facts that make the bump
//! honest:
//!
//! 1. **The served schema names 1.4.0 and its pattern values.** The
//!    `pulse://dsl/schema` document carries `schema_version: "1.4.0"`, the
//!    served version enum lists the five loadable spellings, and the
//!    `CandlePattern` variant's `pattern` enum lists the 17 names.
//! 2. **The wire shape is the spec's, verbatim.** One exact-JSON round-trip
//!    test: `{"indicator":"CandlePattern","pattern":"Engulfing"}`, spelled by
//!    variant name (no `rename_all`), for a multi-word name and a one-word name
//!    alike.
//! 3. **A 1.3.0 document migrates by identity.** It loads, reports its authored
//!    `from`, keeps `dsl_original` byte-verbatim, and re-serializes with the
//!    `schema_version` string as the only difference.
//! 4. **A 1.4.0 strategy using the new variant validates, renders, compiles and
//!    backtests deterministically** on the committed fixture store, on both the
//!    `primary` and the `htf` operand, and two runs give the same
//!    `result_content_hash`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use pulse::{
    BacktestConfig, BinanceAdapter, CandlePatternName, CandleSeries, CandleStore, Comparator,
    Condition, Direction, ExchangeAdapter, ExitRule, IndicatorSpec, Migrator, Pair, RiskParams,
    SchemaVersion, Series, SeriesEnd, StrategyDsl, SweepableValue, Timeframe, ValueSource, compile,
    render, run_backtest, validate,
};
use rust_decimal::Decimal;

/// The 17 pattern names the spine ships, in the enum's own spelling: w1's 13
/// and w2's four three-bar patterns, all inside schema 1.4.0.
const PATTERNS: [CandlePatternName; 17] = [
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

// ---------------------------------------------------------------------------
// 1. The served schema
// ---------------------------------------------------------------------------

/// The served schema (the `StrategyDsl` JSON Schema the `pulse://dsl/schema`
/// resource carries) names 1.4.0 and lists the 17 pattern names on the
/// `CandlePattern` spec.
#[test]
fn the_served_dsl_schema_names_1_4_0_and_the_pattern_values() {
    let doc = serde_json::to_value(schemars::schema_for!(StrategyDsl)).expect("schema serializes");
    assert_eq!(
        doc["properties"]["schema_version"]["enum"]
            .as_array()
            .expect("the version enum")
            .last()
            .and_then(|v| v.as_str()),
        Some("1.4.0"),
        "the served schema names the version this engine writes"
    );

    // schemars v1 renders a unit-variant enum as `oneOf` of `const` strings.
    let name_members = doc["$defs"]["CandlePatternName"]["oneOf"]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "the served schema must carry a CandlePatternName enum; got {}",
                serde_json::to_string_pretty(&doc["$defs"]["CandlePatternName"])
                    .unwrap_or_default()
            )
        });
    let mut names: Vec<String> = name_members
        .iter()
        .map(|member| {
            member["const"]
                .as_str()
                .unwrap_or_else(|| panic!("a pattern name is a const string: {member}"))
                .to_owned()
        })
        .collect();
    names.sort();
    let mut expected: Vec<String> = PATTERNS.iter().map(|p| format!("{p:?}")).collect();
    expected.sort();
    assert_eq!(
        names, expected,
        "the served schema lists exactly the spine's 17 pattern names"
    );

    // The variant itself is advertised, tagged by its `indicator` name.
    let indicator_enum = doc["$defs"]["IndicatorSpec"]["oneOf"]
        .as_array()
        .expect("IndicatorSpec is a tagged enum with oneOf members");
    assert!(
        indicator_enum.iter().any(|member| {
            member["properties"]["indicator"]["const"] == serde_json::json!("CandlePattern")
                || member["properties"]["indicator"]["enum"]
                    .as_array()
                    .is_some_and(|values| values.contains(&serde_json::json!("CandlePattern")))
        }),
        "the served schema advertises the CandlePattern variant; members were: {}",
        serde_json::to_string(indicator_enum).unwrap_or_default()
    );
}

/// The served version enum is the loader's accepted set: the four earlier
/// versions plus `CURRENT`.
#[test]
fn the_served_version_enum_is_the_loadable_set() {
    let schema =
        serde_json::to_value(schemars::schema_for!(StrategyDsl)).expect("schema serializes");
    assert_eq!(
        schema["properties"]["schema_version"]["enum"],
        serde_json::json!(["1.0.0", "1.1.0", "1.2.0", "1.3.0", "1.4.0"]),
        "the served enum lists every loadable spelling and nothing else"
    );
}

// ---------------------------------------------------------------------------
// 2. The wire shape
// ---------------------------------------------------------------------------

/// The spec's exact JSON, byte for byte, and its round-trip: an `Engulfing`
/// operand serializes as `{"indicator":"CandlePattern","pattern":"Engulfing"}`
/// and deserializes back to the same value.
#[test]
fn the_candle_pattern_wire_shape_is_exact_and_round_trips() {
    let spec = IndicatorSpec::CandlePattern {
        pattern: CandlePatternName::Engulfing,
    };
    let json = serde_json::to_string(&spec).expect("serialize");
    assert_eq!(
        json, r#"{"indicator":"CandlePattern","pattern":"Engulfing"}"#,
        "the wire form is the spec's, verbatim"
    );
    let back: IndicatorSpec = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, spec, "the round-trip is value-equal");

    // A multi-word name keeps its variant spelling (no snake_case rename).
    let multi = IndicatorSpec::CandlePattern {
        pattern: CandlePatternName::DarkCloudCover,
    };
    let json = serde_json::to_string(&multi).expect("serialize");
    assert_eq!(
        json,
        r#"{"indicator":"CandlePattern","pattern":"DarkCloudCover"}"#
    );
    let back: IndicatorSpec = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, multi);

    // Every name round-trips by its own spelling, so the enum's spelling and
    // the wire's cannot drift apart.
    for pattern in PATTERNS {
        let spec = IndicatorSpec::CandlePattern { pattern };
        let json = serde_json::to_string(&spec).expect("serialize");
        let expected = format!(r#"{{"indicator":"CandlePattern","pattern":"{pattern:?}"}}"#);
        assert_eq!(json, expected, "{pattern:?} serializes by variant name");
        let back: IndicatorSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, spec, "{pattern:?} round-trips");
    }

    // The snake_case spelling is NOT accepted: it is a different vocabulary
    // (the read-path tokens are w3's), and silently accepting it would make two
    // spellings mean one thing.
    let snake: Result<IndicatorSpec, _> =
        serde_json::from_str(r#"{"indicator":"CandlePattern","pattern":"dark_cloud_cover"}"#);
    assert!(
        snake.is_err(),
        "the snake_case spelling must not be accepted on the wire"
    );
}

// ---------------------------------------------------------------------------
// 3. The identity migration
// ---------------------------------------------------------------------------

/// A canonical 1.3.0 document — every defaulted field written explicitly, the
/// wire form the engine wrote before this bump.
const DOC_1_3_0: &str = r#"{
  "schema_version": "1.3.0",
  "name": "candle pattern migration fixture",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": {
      "type": "Indicator",
      "series": "primary",
      "spec": {
        "indicator": "CandlePattern",
        "pattern": "Engulfing"
      }
    },
    "op": "Gt",
    "rhs": { "type": "Constant", "value": "0" }
  },
  "filters": [],
  "exits": [{ "type": "StopLoss", "distance_pct": "0.05" }],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// A 1.3.0 document loads by identity: `from` is the authored version,
/// `dsl_original` stays byte-verbatim, the typed document is `CURRENT`, and the
/// re-serialization differs from the input only in the `schema_version` string.
#[test]
fn a_1_3_0_document_migrates_by_identity() {
    let loaded = Migrator::v1()
        .load(DOC_1_3_0)
        .expect("a 1.3.0 document loads");

    assert_eq!(
        loaded.from.to_string(),
        "1.3.0",
        "`from` is the authored version"
    );
    assert!(
        loaded.migrated,
        "the 1.3.0 → 1.4.0 identity step was applied"
    );
    assert_eq!(
        loaded.dsl_original, DOC_1_3_0,
        "dsl_original stays byte-verbatim"
    );
    assert_eq!(
        loaded.dsl.schema_version,
        SchemaVersion::CURRENT,
        "the typed document is CURRENT"
    );
    assert_eq!(
        loaded.dsl.schema_version.to_string(),
        "1.4.0",
        "CURRENT is 1.4.0"
    );

    let reserialized = serde_json::to_value(&loaded.dsl).expect("re-serializes");
    let mut expected: serde_json::Value =
        serde_json::from_str(DOC_1_3_0).expect("the fixture is valid JSON");
    expected["schema_version"] = serde_json::json!("1.4.0");
    assert_eq!(
        reserialized, expected,
        "the identity step changes nothing but the schema_version string"
    );
}

/// The whole registered chain is identity, end to end: a 1.0.0 document reaches
/// 1.4.0 with its meaning intact (the same compiled strategy, hash-for-hash).
#[test]
fn the_whole_chain_migrates_identity_to_1_4_0() {
    let doc = DOC_1_3_0.replace("\"1.3.0\"", "\"1.0.0\"");
    let loaded = Migrator::v1().load(&doc).expect("a 1.0.0 document loads");
    assert_eq!(loaded.from.to_string(), "1.0.0");
    assert_eq!(loaded.dsl.schema_version.to_string(), "1.4.0");
    compile(&validate(&loaded.dsl).expect("validates")).expect("compiles");
}

/// A future minor is still refused with the message naming the new pair.
#[test]
fn a_future_minor_is_refused() {
    let doc = DOC_1_3_0.replace("\"1.3.0\"", "\"1.5.0\"");
    let err = Migrator::v1()
        .load(&doc)
        .expect_err("a future version must refuse");
    let pulse::LoadError::FutureVersion { found, current } = err else {
        panic!("expected FutureVersion, got {err:?}");
    };
    assert_eq!(found.to_string(), "1.5.0");
    assert_eq!(current.to_string(), "1.4.0");
}

// ---------------------------------------------------------------------------
// 4. The end-to-end 1.4.0 strategy
// ---------------------------------------------------------------------------

/// The strategy the journey needs: an `Engulfing` gate on the primary series
/// and the full bullish form on the `htf` operand, beside the existing
/// comparisons. It must validate, render, compile and backtest.
fn candle_pattern_strategy() -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "candle pattern 1.4.0".to_owned(),
        direction: Direction::Long,
        // A bullish engulfing fired on the primary series (`Gt 0` — the `±80`
        // partial form counts, which is the amended grill Q1's point).
        entry: Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Htf,
                spec: IndicatorSpec::CandlePattern {
                    pattern: CandlePatternName::Engulfing,
                },
            },
            op: Comparator::Eq,
            rhs: ValueSource::Constant {
                value: Decimal::from(100),
            },
        },
        filters: vec![Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::CandlePattern {
                    pattern: CandlePatternName::Hammer,
                },
            },
            op: Comparator::Gt,
            rhs: ValueSource::Constant {
                value: Decimal::ZERO,
            },
        }],
        exits: vec![ExitRule::StopLoss {
            distance_pct: SweepableValue::Fixed(dec("0.05")),
        }],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(dec("0.01")),
            max_leverage: SweepableValue::Fixed(Decimal::from(3)),
        },
    }
}

fn dec(value: &str) -> Decimal {
    Decimal::from_str_exact(value).expect("a decimal literal")
}

/// A committed fixture series (no network, no DB).
fn load_series(timeframe: Timeframe) -> CandleSeries {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store");
    let store = CandleStore::with_base_dir(base);
    let pair = Pair::new("BTCUSDT");
    let head = store
        .read_head(&pair, timeframe)
        .expect("read HEAD")
        .expect("HEAD present");
    store
        .read_snapshot(&pair, timeframe, &head)
        .expect("read snapshot")
}

/// Every pattern's trader-words rendering is pinned, one line per name, so the
/// formatter's table cannot drift from the enum (r5.s1.w1 review: mutating any
/// single word must turn this test red).
#[test]
fn every_pattern_renders_in_its_trader_words() {
    for (pattern, word) in [
        (CandlePatternName::Doji, "doji"),
        (CandlePatternName::DragonflyDoji, "dragonfly doji"),
        (CandlePatternName::GravestoneDoji, "gravestone doji"),
        (CandlePatternName::Hammer, "hammer"),
        (CandlePatternName::InvertedHammer, "inverted hammer"),
        (CandlePatternName::HangingMan, "hanging man"),
        (CandlePatternName::ShootingStar, "shooting star"),
        (CandlePatternName::Marubozu, "marubozu"),
        (CandlePatternName::SpinningTop, "spinning top"),
        (CandlePatternName::Engulfing, "engulfing"),
        (CandlePatternName::Harami, "harami"),
        (CandlePatternName::Piercing, "piercing"),
        (CandlePatternName::DarkCloudCover, "dark cloud cover"),
        // r5.s1.w2: the three-bar patterns render in the same trader words.
        (CandlePatternName::MorningStar, "morning star"),
        (CandlePatternName::EveningStar, "evening star"),
        (
            CandlePatternName::ThreeWhiteSoldiers,
            "three white soldiers",
        ),
        (CandlePatternName::ThreeBlackCrows, "three black crows"),
    ] {
        assert_eq!(
            render::indicator(&IndicatorSpec::CandlePattern { pattern }),
            word,
            "{pattern:?} renders as its trader words"
        );
    }

    // The htf-prefixed form the summary line builds on top of it.
    let dsl = candle_pattern_strategy();
    let rendered = render::strategy(&dsl);
    assert_eq!(rendered.entry, "h4:engulfing = 100");
    assert_eq!(rendered.filters, vec!["hammer > 0".to_owned()]);
}

/// **r5.s1.w2.** A 1.4.0 strategy on `Compare(CandlePattern{MorningStar}, Gt, 0)`
/// — the three-bar form the spec's Approach §6 names — validates, renders in
/// trader words, compiles and backtests deterministically on the fixture store.
#[test]
fn a_morning_star_strategy_validates_renders_and_backtests() {
    let dsl = StrategyDsl {
        name: "morning star 1.4.0".to_owned(),
        entry: Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::CandlePattern {
                    pattern: CandlePatternName::MorningStar,
                },
            },
            op: Comparator::Gt,
            rhs: ValueSource::Constant {
                value: Decimal::ZERO,
            },
        },
        ..candle_pattern_strategy()
    };
    let validated = validate(&dsl).expect("the strategy validates");
    let compiled = compile(&validated).expect("the strategy compiles");

    let rendered = render::strategy(&dsl);
    assert_eq!(rendered.entry, "morning star > 0");

    let primary = load_series(Timeframe::M15);
    let htf = load_series(Timeframe::H4);
    let first = run(&compiled, &primary, &htf);
    let second = run(&compiled, &primary, &htf);
    assert_eq!(
        first.result_content_hash(),
        second.result_content_hash(),
        "two runs of the morning-star strategy must hash identically"
    );
}

/// A refusal that names a candle-pattern operand names it through the crate's
/// own `spec_name` convention — `CandlePattern(DarkCloudCover)`, the same shape
/// `PivotHigh(1/2)` and `Body` already use — so a validation message never
/// prints a bare `Debug` of the spec.
#[test]
fn a_refusal_message_names_the_pattern_as_the_crate_names_it() {
    // A provably-impossible comparison: the same operand on both sides of a
    // strict comparison is equal on every bar, so it can never be above itself
    // — and the message names both sides through `describe`/`spec_name`.
    let pattern = ValueSource::Indicator {
        series: Series::Primary,
        spec: IndicatorSpec::CandlePattern {
            pattern: CandlePatternName::DarkCloudCover,
        },
    };
    let dsl = StrategyDsl {
        entry: Condition::Compare {
            lhs: pattern.clone(),
            op: Comparator::Gt,
            rhs: pattern,
        },
        ..candle_pattern_strategy()
    };
    let errors = validate(&dsl).expect_err("a value cannot be above itself");
    let messages = format!("{errors:?}");
    assert!(
        messages.contains("the CandlePattern(DarkCloudCover) value can never be above itself"),
        "the refusal names the pattern through `spec_name`, got: {messages}"
    );
}

/// The 1.4.0 strategy validates, renders the pattern in trader words, compiles,
/// and backtests on the fixture store.
#[test]
fn a_1_4_0_candle_pattern_strategy_validates_renders_and_backtests() {
    let dsl = candle_pattern_strategy();
    let validated = validate(&dsl).expect("the strategy validates");
    let compiled = compile(&validated).expect("the strategy compiles");
    assert!(
        compiled.needs_htf(),
        "the strategy reads the higher timeframe"
    );
    assert!(
        compiled
            .required_htf_indicators()
            .iter()
            .any(|spec| matches!(spec, IndicatorSpec::CandlePattern { .. })),
        "the engulfing gate is an HTF requirement"
    );
    assert!(
        compiled
            .required_indicators()
            .iter()
            .any(|spec| matches!(spec, IndicatorSpec::CandlePattern { .. })),
        "the hammer filter is a primary requirement"
    );

    // Rendered in trader words, never as the wire token.
    let rendered = render::strategy(&dsl);
    assert_eq!(
        rendered.entry, "h4:engulfing = 100",
        "the entry renders the pattern and the full-form test"
    );
    assert_eq!(
        rendered.filters,
        vec!["hammer > 0".to_owned()],
        "the filter renders the pattern"
    );

    let result = run(
        &compiled,
        &load_series(Timeframe::M15),
        &load_series(Timeframe::H4),
    );
    assert!(
        !result.result_content_hash().is_empty(),
        "the backtest produces a content hash"
    );
}

/// Two runs of the 1.4.0 strategy over the fixture give the same
/// `result_content_hash` and the same trade log — the determinism the risk
/// posture requires of a new indicator feeding the engine.
#[test]
fn a_1_4_0_backtest_is_deterministic() {
    let compiled =
        compile(&validate(&candle_pattern_strategy()).expect("validates")).expect("compiles");
    let primary = load_series(Timeframe::M15);
    let htf = load_series(Timeframe::H4);
    let first = run(&compiled, &primary, &htf);
    let second = run(&compiled, &primary, &htf);

    assert_eq!(
        first.result_content_hash(),
        second.result_content_hash(),
        "two runs of one strategy must hash identically"
    );
    assert_eq!(
        serde_json::to_string(&first.trades).expect("trades serialize"),
        serde_json::to_string(&second.trades).expect("trades serialize"),
        "the two trade logs must be byte-identical"
    );
}

/// The pre-bump goldens are unmoved by this bump: the committed 1.0.0 fixture
/// strategy's `result_content_hash` on the fixture store still equals the
/// frozen `b8e91b89…` pin, and its trade log is unchanged.
///
/// That is the whole "no golden changes" claim, asserted from this suite's own
/// side: a new variant no existing document names cannot move a number.
#[test]
fn the_frozen_pre_bump_golden_is_unmoved() {
    const FROZEN: &str = "b8e91b89b7727eb97c8200ee04373ffc3ef3568b51ab145bbb2e652cb2bb9228";
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/strategies/rsi-oversold-long.json");
    let json = std::fs::read_to_string(&path).expect("read the fixture strategy");
    let loaded = Migrator::v1().load(&json).expect("loads");
    assert_eq!(
        loaded.from.to_string(),
        "1.0.0",
        "the fixture is authored at 1.0.0"
    );
    let compiled = compile(&validate(&loaded.dsl).expect("validates")).expect("compiles");
    let result = run(
        &compiled,
        &load_series(Timeframe::M15),
        &load_series(Timeframe::H4),
    );
    assert_eq!(
        result.result_content_hash(),
        FROZEN,
        "the frozen pre-bump result_content_hash must not move"
    );
}

/// One run over the fixture, through the real BTCUSDT filters and the default
/// cost model — the recipe the other fixture suites use.
fn run(
    compiled: &pulse::CompiledStrategy,
    primary: &CandleSeries,
    htf: &CandleSeries,
) -> pulse::BacktestResult {
    let filters = BinanceAdapter::new()
        .symbol_filters(&Pair::new("BTCUSDT"))
        .expect("BTCUSDT filters resolve through the port");
    run_backtest(
        compiled,
        primary,
        Some(htf),
        None,
        &BacktestConfig::default(),
        &filters,
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("the backtest runs over the fixture")
}
