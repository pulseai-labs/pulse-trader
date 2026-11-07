//! r6.s3.w2 — `PreviousPivotHigh` / `PreviousPivotLow` and DSL schema 1.7.0.
//!
//! `PreviousPivotHigh{left, right}` is the high of the confirmed swing high
//! **before** the most recent confirmed one; `PreviousPivotLow` is the mirror on
//! lows. With `PivotHigh`/`PivotLow` they say STR-0003 v2's "of the last two
//! confirmed swing highs, the later is higher" as
//! `PivotHigh > PreviousPivotHigh`. This suite pins, on hand-built candles with
//! `left = right = 3`:
//!
//! 1. The value is `None` until two pivots are confirmed, and the old latest
//!    becomes the previous on the bar the new pivot confirms.
//! 2. Two confirmed pivots at one price are two pivots.
//! 3. No repaint under truncation (ADR-0024's 1.3.0 structure addendum).
//! 4. The `htf` operand reads closed higher-timeframe bars only.
//! 5. `explain_marks` shows both prices and the held state.
//! 6. Schema 1.7.0: a 1.6.0 document loads unchanged, a 1.7.0 document with
//!    both variants round-trips and validates, and a zero span is refused.
//!
//! Every expected value is a pinned literal, labelled by hand against the
//! definition; nothing here recomputes an expectation from the code under test.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use pulse::{
    BinanceAdapter, Candle, CandleSeries, CandleStore, Comparator, CompiledValue, Condition,
    DataVersion, Direction, EvalContext, ExampleMarkDraft, ExampleMarkRepository, ExitRule,
    ExplainRequest, FieldError, IndicatorEngine, IndicatorSpec, MarkSelection, MarkSetup, MarkSide,
    MarkWriter, MarkWriterKind, Migrator, Pair, PriceField, RiskParams, RuleRequest, SchemaVersion,
    Series, SqliteExampleMarkRepo, StrategyDsl, SweepableValue, SystemClock, Timeframe,
    ValidationCode, ValueSource, align, compile, explain_marks, validate,
};
use rust_decimal::Decimal;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Hand-built candles (the `tests/structure_indicators.rs` helpers)
// ---------------------------------------------------------------------------

fn dec(value: i64) -> Decimal {
    Decimal::from(value)
}

fn fixed_u32(value: u32) -> SweepableValue<u32> {
    SweepableValue::Fixed(value)
}

/// One candle of `timeframe` at absolute index `idx`, with the structure
/// suite's funding-stamp policy (a zero rate pays zero).
fn candle_on(timeframe: Timeframe, idx: i64, open: i64, high: i64, low: i64, close: i64) -> Candle {
    let open_time = idx * timeframe.duration_ms();
    Candle {
        open_time,
        close_time: open_time + timeframe.duration_ms() - 1,
        open: dec(open),
        high: dec(high),
        low: dec(low),
        close: dec(close),
        volume: dec(1),
        funding_rate: if open_time % 28_800_000 == 0 {
            Some(Decimal::ZERO)
        } else {
            None
        },
    }
}

/// A bar that carries its `high` and `low` exactly; open and close sit inside.
fn bar_on(timeframe: Timeframe, idx: i64, high: i64, low: i64) -> Candle {
    candle_on(timeframe, idx, high - 1, high, low, high - 1)
}

/// Candles from `(high, low)` rows on `timeframe`.
fn candles_on(timeframe: Timeframe, rows: &[(i64, i64)]) -> Vec<Candle> {
    rows.iter()
        .enumerate()
        .map(|(idx, (high, low))| {
            bar_on(
                timeframe,
                i64::try_from(idx).expect("small index"),
                *high,
                *low,
            )
        })
        .collect()
}

fn series_of(timeframe: Timeframe, candles: Vec<Candle>, version: &str) -> CandleSeries {
    CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe,
        version: DataVersion::new(version),
        candles,
    }
}

/// Three swing highs, 110 @3, 115 @10 and 112 @17, with `left = right = 3`,
/// confirmed 3 bars late: bars 6, 13 and 20. Hand-checked: no other bar is a
/// swing high (every other bar sits on a ramp, and a ramp bar loses either the
/// strict left test or the right test). Each lows column sits two under its
/// high, so the same rows hold two swing lows, 100 @6 and 103 @13, confirmed at
/// bars 9 and 16 (also hand-checked: no third low has its right bars yet).
const THREE_HIGHS: [i64; 21] = [
    100, 102, 104, 110, 106, 104, 102, // pivot @3 = 110
    104, 106, 108, 115, 109, 107, 105, // pivot @10 = 115
    106, 108, 110, 112, 108, 106, 104, // pivot @17 = 112
];

fn three_highs_rows() -> Vec<(i64, i64)> {
    THREE_HIGHS.iter().map(|high| (*high, high - 2)).collect()
}

/// Two swing lows, both 90: @3 and @10, with `left = right = 3`. Confirmed at
/// bars 6 and 13. The highs sit ten over each low.
const TWO_LOWS: [i64; 14] = [
    100, 98, 96, 90, 94, 96, 98, // pivot @3 = 90
    96, 94, 92, 90, 94, 96, 98, // pivot @10 = 90 (the same price)
];

fn two_lows_rows() -> Vec<(i64, i64)> {
    TWO_LOWS.iter().map(|low| (low + 10, *low)).collect()
}

fn previous_high() -> IndicatorSpec {
    IndicatorSpec::PreviousPivotHigh {
        left: fixed_u32(3),
        right: fixed_u32(3),
    }
}

fn previous_low() -> IndicatorSpec {
    IndicatorSpec::PreviousPivotLow {
        left: fixed_u32(3),
        right: fixed_u32(3),
    }
}

fn pivot_high() -> IndicatorSpec {
    IndicatorSpec::PivotHigh {
        left: fixed_u32(3),
        right: fixed_u32(3),
    }
}

fn pivot_low() -> IndicatorSpec {
    IndicatorSpec::PivotLow {
        left: fixed_u32(3),
        right: fixed_u32(3),
    }
}

/// Step ONE indicator over `candles` on the primary series, returning its value
/// per bar (the structure suite's `step`).
fn step(spec: &IndicatorSpec, candles: &[Candle]) -> Vec<Option<Decimal>> {
    let mut engine =
        IndicatorEngine::from_specs(std::slice::from_ref(spec)).expect("engine builds");
    let value = CompiledValue::Indicator {
        series: Series::Primary,
        spec: spec.clone(),
        lag: 0,
    };
    candles
        .iter()
        .map(|candle| {
            engine.step(candle);
            engine.current(&value)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The value
// ---------------------------------------------------------------------------

/// The value is `None` through the first confirmation (bar 6) and becomes the
/// first pivot's price on the bar the second pivot confirms (bar 10 + 3 = 13).
#[test]
fn previous_is_none_until_two_pivots() {
    let candles = candles_on(Timeframe::M15, &three_highs_rows());
    let latest = step(&pivot_high(), &candles);
    let previous = step(&previous_high(), &candles);

    assert_eq!(
        latest[6],
        Some(dec(110)),
        "the first pivot confirms at bar 6"
    );
    for (bar, value) in previous.iter().enumerate().take(13) {
        assert_eq!(*value, None, "bar {bar}: fewer than two pivots confirmed");
    }
    assert_eq!(
        previous[13],
        Some(dec(110)),
        "bar 13: the second pivot confirms and the first becomes the previous"
    );
    assert_eq!(
        latest[13],
        Some(dec(115)),
        "bar 13: the latest is the second"
    );
}

/// Three pivot highs at 110, 115, 112: after the third confirms, `PivotHigh` is
/// 112 and `PreviousPivotHigh` is 115 — and the previous holds through the bars
/// between confirmations.
#[test]
fn previous_takes_the_old_latest() {
    let candles = candles_on(Timeframe::M15, &three_highs_rows());
    let latest = step(&pivot_high(), &candles);
    let previous = step(&previous_high(), &candles);

    assert_eq!(
        latest[20],
        Some(dec(112)),
        "bar 20: the third pivot is latest"
    );
    assert_eq!(
        previous[20],
        Some(dec(115)),
        "bar 20: the second pivot is the previous"
    );
    assert_eq!(previous[19], Some(dec(110)), "bar 19: not yet confirmed");
    assert_eq!(latest[19], Some(dec(115)), "bar 19: not yet confirmed");

    // The low side reads the same rows: swing lows 100 @6 and 103 @13, confirmed
    // at bars 9 and 16.
    let latest_low = step(&pivot_low(), &candles);
    let previous_low = step(&previous_low(), &candles);
    assert_eq!(latest_low[20], Some(dec(103)));
    assert_eq!(previous_low[20], Some(dec(100)));
    assert_eq!(previous_low[15], None, "one swing low confirmed at bar 15");
    assert_eq!(previous_low[16], Some(dec(100)));
}

/// Two pivot lows at 90 and 90 are two pivots: the previous takes the old price
/// even though it equals the new one.
#[test]
fn equal_prices_are_two_pivots() {
    let candles = candles_on(Timeframe::M15, &two_lows_rows());
    let latest = step(&pivot_low(), &candles);
    let previous = step(&previous_low(), &candles);

    assert_eq!(latest[6], Some(dec(90)), "the first low confirms at bar 6");
    assert_eq!(previous[12], None, "bar 12: one pivot");
    assert_eq!(
        previous[13],
        Some(dec(90)),
        "bar 13: the second 90 confirms; the previous is the first 90"
    );
    assert_eq!(latest[13], Some(dec(90)));
}

/// For every prefix of the series, the value at each bar equals the full-series
/// value at that bar (the 1.3.0 addendum's no-repaint test).
#[test]
fn no_repaint_under_truncation() {
    let cases: Vec<(&str, IndicatorSpec, Vec<Candle>)> = vec![
        (
            "previous pivot high",
            previous_high(),
            candles_on(Timeframe::M15, &three_highs_rows()),
        ),
        (
            "previous pivot low",
            previous_low(),
            candles_on(Timeframe::M15, &three_highs_rows()),
        ),
        (
            "previous pivot low, equal prices",
            previous_low(),
            candles_on(Timeframe::M15, &two_lows_rows()),
        ),
    ];
    for (label, spec, candles) in cases {
        let full = step(&spec, &candles);
        assert!(
            full.iter().any(Option::is_some),
            "{label}: the run is not vacuous"
        );
        for cut in 1..=candles.len() {
            let truncated = step(&spec, &candles[..cut]);
            assert_eq!(
                truncated,
                full[..cut],
                "{label}: a value at bar t must not depend on any bar after t (cut at {cut})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The htf operand
// ---------------------------------------------------------------------------

fn long_dsl(entry: Condition) -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "previous pivots htf fixture".to_owned(),
        direction: Direction::Long,
        entry,
        filters: vec![],
        exits: vec![ExitRule::StopLoss {
            distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
        }],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
            max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
        },
        setup: None,
    }
}

/// As an `htf` operand on a 15m primary with a 4h series, the value changes
/// only on the 15m bar where the confirming 4h bar closed.
#[test]
fn htf_previous_pivot_reads_closed_bars() {
    // The 4h series is the three-highs rows: confirmations at 4h bars 6, 13, 20.
    let htf = series_of(
        Timeframe::H4,
        candles_on(Timeframe::H4, &three_highs_rows()),
        "v-previous-pivots-h4",
    );
    // 21 4h bars = 336 15m bars, flat.
    let primary = series_of(
        Timeframe::M15,
        (0..336)
            .map(|idx| candle_on(Timeframe::M15, idx, 100, 101, 99, 100))
            .collect(),
        "v-previous-pivots-m15",
    );

    let entry = Condition::Compare {
        lhs: ValueSource::Price {
            series: Series::Primary,
            field: PriceField::Close,
        },
        op: Comparator::Gt,
        rhs: ValueSource::Indicator {
            series: Series::Htf,
            spec: previous_high(),
        },
    };
    let strategy = compile(&validate(&long_dsl(entry)).expect("validates")).expect("compiles");
    assert!(
        strategy
            .required_htf_indicators()
            .iter()
            .any(|spec| matches!(spec, IndicatorSpec::PreviousPivotHigh { .. })),
        "the previous pivot is an HTF requirement"
    );
    assert!(
        strategy.required_indicators().is_empty(),
        "the primary engine needs no indicator for this strategy"
    );

    // Walk the 15m bars as the backtest does: the 4h engine steps once per newly
    // closed 4h bar, and the value is read on every 15m bar.
    let mut engine = IndicatorEngine::for_series(&strategy, Series::Htf).expect("HTF engine");
    let value = CompiledValue::Indicator {
        series: Series::Htf,
        spec: previous_high(),
        lag: 0,
    };
    let mut stepped_through: Option<i64> = None;
    let mut seen: Vec<Option<Decimal>> = Vec::new();
    for aligned in align(&primary, Some(&htf), None) {
        if let Some(closed) = aligned.htf
            && stepped_through != Some(closed.close_time)
        {
            engine.step(closed);
            stepped_through = Some(closed.close_time);
        }
        seen.push(engine.current(&value));
    }

    // 4h bar 13 closes with the 15m bar 14 * 16 - 1 = 223; bar 20 with 21 * 16 - 1 = 335.
    let mut changes: Vec<(usize, Option<Decimal>)> = Vec::new();
    let mut last: Option<Decimal> = None;
    for (index, value) in seen.iter().enumerate() {
        if *value != last {
            changes.push((index, *value));
            last = *value;
        }
    }
    assert_eq!(
        changes,
        vec![(223, Some(dec(110))), (335, Some(dec(115)))],
        "the value changes only on the 15m bar where the confirming 4h bar closed"
    );
}

// ---------------------------------------------------------------------------
// explain_marks
// ---------------------------------------------------------------------------

/// An inline rule whose only filter is `PivotHigh > PreviousPivotHigh`.
const HIGHER_HIGH_RULE: &str = r#"{
  "schema_version": "1.7.0",
  "name": "r6.s3.w2 explain fixture",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Price", "field": "Close" },
    "op": "Gt",
    "rhs": { "type": "Constant", "value": "1" }
  },
  "filters": [
    {
      "type": "Compare",
      "lhs": {
        "type": "Indicator",
        "spec": { "indicator": "PivotHigh", "left": 3, "right": 3 }
      },
      "op": "Gt",
      "rhs": {
        "type": "Indicator",
        "spec": { "indicator": "PreviousPivotHigh", "left": 3, "right": 3 }
      }
    }
  ],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// `explain_marks` (the `tests/mark_explain.rs` setup, over a store holding the
/// three-highs candles) at a mark after the third pivot: the conjunct's operand
/// labels are `pivot_high:3-3` and `prev_pivot_high:3-3` with values 112 and
/// 115, and the state is `not_held` (the later swing high is lower).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explain_shows_both_pivot_prices() {
    let pair = Pair::new("BTCUSDT");
    let candles = candles_on(Timeframe::M15, &three_highs_rows());
    let version = CandleStore::content_version(&pair, Timeframe::M15, &candles);
    let series = CandleSeries {
        pair: pair.clone(),
        timeframe: Timeframe::M15,
        version: version.clone(),
        candles,
    };

    let tmp_store = TempDir::new().expect("tempdir");
    let store = CandleStore::with_base_dir(tmp_store.path().to_path_buf());
    store.write_snapshot(&series).expect("the snapshot writes");
    store
        .write_head(&pair, Timeframe::M15, &version)
        .expect("the HEAD writes");

    let tmp_db = TempDir::new().expect("tempdir");
    let (_, db) = support::mcp::migrated_db(&tmp_db).await;
    let marks = SqliteExampleMarkRepo::with_deps(db.pool().clone(), SystemClock);
    // The mark is on bar 20 — the bar the third pivot confirms.
    let mark = marks
        .insert(&ExampleMarkDraft {
            pair: pair.clone(),
            timeframe: Timeframe::M15,
            bar_open_ms: 20 * Timeframe::M15.duration_ms(),
            side: MarkSide::Long,
            setup: MarkSetup::parse("previous-pivots").expect("a setup label"),
            note: None,
            writer: MarkWriter {
                kind: MarkWriterKind::Trader,
                name: "w2-app".to_owned(),
            },
            data_version: version.as_str().to_owned(),
        })
        .await
        .expect("the mark inserts");

    let strategies = pulse::SqliteStrategyRepo::new(db.pool().clone());
    let report = explain_marks(
        &marks,
        &store,
        &strategies,
        &BinanceAdapter,
        None,
        ExplainRequest {
            marks: MarkSelection::Ids(vec![mark.id.clone()]),
            rule: RuleRequest::Inline(Box::new(
                serde_json::from_str(HIGHER_HIGH_RULE).expect("the rule literal parses"),
            )),
        },
    )
    .await
    .expect("the rule explains");

    assert_eq!(report.marks_explained, 1, "one mark explained");
    let explained = &report.marks[0];
    assert_eq!(explained.conjuncts.len(), 2, "the entry and the one filter");
    let filter = &explained.conjuncts[1];
    assert_eq!(filter.source, "filter");
    assert_eq!(filter.operands.len(), 2, "a Compare has two operands");
    assert_eq!(filter.operands[0].label, "pivot_high:3-3");
    assert_eq!(filter.operands[0].value.as_deref(), Some("112"));
    assert_eq!(filter.operands[1].label, "prev_pivot_high:3-3");
    assert_eq!(filter.operands[1].value.as_deref(), Some("115"));
    assert_eq!(
        filter.state, "not_held",
        "the later swing high (112) is not above the one before it (115)"
    );
}

// ---------------------------------------------------------------------------
// Schema 1.7.0
// ---------------------------------------------------------------------------

/// A canonical 1.6.0 document: no setup, no previous pivots.
const DOC_1_6_0: &str = r#"{
  "schema_version": "1.6.0",
  "name": "pivot migration fixture",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": {
      "type": "Indicator",
      "series": "primary",
      "spec": { "indicator": "PivotHigh", "left": 3, "right": 3 }
    },
    "op": "Gt",
    "rhs": { "type": "Constant", "value": "1" }
  },
  "filters": [],
  "exits": [{ "type": "StopLoss", "distance_pct": "0.05" }],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

fn compare(lhs: IndicatorSpec, op: Comparator, rhs: IndicatorSpec) -> Condition {
    Condition::Compare {
        lhs: ValueSource::Indicator {
            series: Series::Primary,
            spec: lhs,
        },
        op,
        rhs: ValueSource::Indicator {
            series: Series::Primary,
            spec: rhs,
        },
    }
}

/// A 1.7.0 document using both new variants (one per filter).
fn both_variants() -> StrategyDsl {
    StrategyDsl {
        filters: vec![
            compare(pivot_high(), Comparator::Gt, previous_high()),
            compare(pivot_low(), Comparator::Gt, previous_low()),
        ],
        ..long_dsl(compare(pivot_high(), Comparator::Gt, previous_high()))
    }
}

fn refusals(dsl: &StrategyDsl) -> Vec<FieldError> {
    validate(dsl)
        .expect_err("the document must be refused")
        .into_errors()
}

/// A 1.6.0 document loads unchanged under 1.7.0; a 1.7.0 document with both
/// variants round-trips and validates; `left = 0` is refused with the pivot-span
/// error.
#[test]
fn schema_1_7_round_trip() {
    // (1) A 1.6.0 document loads unchanged: only the version string moves.
    let loaded = Migrator::v1()
        .load(DOC_1_6_0)
        .expect("a 1.6.0 document loads");
    assert_eq!(loaded.from.to_string(), "1.6.0");
    assert!(
        loaded.migrated,
        "the 1.6.0 → 1.7.0 identity step was applied"
    );
    assert_eq!(
        loaded.dsl_original, DOC_1_6_0,
        "dsl_original stays verbatim"
    );
    assert_eq!(loaded.dsl.schema_version, SchemaVersion::CURRENT);
    assert_eq!(loaded.dsl.schema_version.to_string(), "1.7.0");
    let reserialized = serde_json::to_value(&loaded.dsl).expect("re-serializes");
    let mut expected: serde_json::Value =
        serde_json::from_str(DOC_1_6_0).expect("the fixture is valid JSON");
    expected["schema_version"] = serde_json::json!("1.7.0");
    assert_eq!(
        reserialized, expected,
        "the identity step changes nothing but the schema_version string"
    );
    compile(&validate(&loaded.dsl).expect("validates")).expect("compiles");

    // (2) A 1.7.0 document with both variants round-trips and validates.
    let dsl = both_variants();
    validate(&dsl).expect("both variants validate");
    let text = serde_json::to_string(&dsl).expect("serializes");
    assert!(
        text.contains(r#""indicator":"PreviousPivotHigh""#)
            && text.contains(r#""indicator":"PreviousPivotLow""#),
        "both variants are written under their own names: {text}"
    );
    let back: StrategyDsl = serde_json::from_str(&text).expect("deserializes");
    assert_eq!(back, dsl, "serde round-trips value-equal");
    let through_loader = Migrator::v1().load(&text).expect("the loader reads it");
    assert!(
        !through_loader.migrated,
        "a 1.7.0 document needs no migration"
    );
    assert_eq!(
        through_loader.dsl, dsl,
        "the loader round-trips value-equal"
    );

    // (3) `left = 0` is refused with the pivot-span error, on both variants.
    for (spec, label, field) in [
        (
            IndicatorSpec::PreviousPivotHigh {
                left: fixed_u32(0),
                right: fixed_u32(3),
            },
            "PreviousPivotHigh left must be at least 1",
            "previous_pivot_high.left",
        ),
        (
            IndicatorSpec::PreviousPivotLow {
                left: fixed_u32(0),
                right: fixed_u32(3),
            },
            "PreviousPivotLow left must be at least 1",
            "previous_pivot_low.left",
        ),
    ] {
        let dsl = long_dsl(compare(pivot_high(), Comparator::Gt, spec));
        let errors = refusals(&dsl);
        let hit = errors
            .iter()
            .find(|error| error.path.ends_with(field))
            .unwrap_or_else(|| panic!("an error at …{field}; all errors: {errors:?}"));
        assert_eq!(hit.code, ValidationCode::FieldRange, "code at {field}");
        assert_eq!(hit.message, label, "message at {field}");
    }
}
