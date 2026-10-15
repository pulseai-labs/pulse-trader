//! r5.s1.w1 and r5.s1.w2 — the candle-pattern acceptance suite (demo line d99).
//!
//! Schema **1.4.0** adds one `IndicatorSpec::CandlePattern { pattern }` variant
//! carrying TA-Lib's own integer (`+100` / `-100` / `0`, and `±80` for the
//! partial Engulfing and Harami forms), plus the spine's 17 named patterns —
//! w1's 13 one- and two-bar patterns and w2's four three-bar ones. This suite
//! pins that against TA-Lib's own output on the committed BTCUSDT fixture bars:
//!
//! - **(a) Reference parity.** For each pattern, every bar of both reference
//!   CSVs and of the three synthetic sequences must equal TA-Lib's integer
//!   exactly, and rows before the pattern's lookback must read `None` — the
//!   CSV's `0` there is TA-Lib's padding, NOT "no pattern". No tolerance, no
//!   skipped row, and the exception list is **empty** (a mismatch stops the
//!   item; see the spec's Constraints).
//! - **(b) The occurrence floor.** Each pattern must read non-zero at least
//!   three times across the CSVs it is read from, so no pattern can pass
//!   vacuously (grill Q3).
//! - **(c) No repaint.** For every pattern and every prefix of the 4h series —
//!   and for a fixed sample of prefixes of the 15m series — the values on the
//!   prefix equal the full series' values over the overlap, so a value at bar t
//!   is independent of every bar after t.
//! - **(d) Determinism.** Streaming the 15m series twice yields a
//!   byte-identical `Vec<Option<Decimal>>` (NFR-2).
//! - **(e) Through the engine.** A `CandlePattern` spec built by
//!   `IndicatorEngine::from_specs` yields exactly the direct stream on the
//!   primary series, and a strategy carrying the variant on `series: Htf`
//!   builds, warms and yields values from its higher-timeframe engine. This
//!   tests the dispatch, not only the struct.
//!
//! The expected values are TA-Lib's, never recomputed from the implementation
//! under test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::str::FromStr;

use pulse::{
    CandlePatternName, CandleSeries, Comparator, Condition, Direction, EvalContext, ExitRule,
    IndicatorEngine, IndicatorSpec, RiskParams, Series, StrategyDsl, SweepableValue, Timeframe,
    ValueSource, compile, validate,
};
use rust_decimal::Decimal;

mod support;

use support::candle_patterns_support as cp;

// ---------------------------------------------------------------------------
// The catalog
// ---------------------------------------------------------------------------

/// The 17 patterns across both work items of the spine, in catalog order, each
/// with its TA-Lib function name and its lookback `L` (definitions §5, used
/// EXACTLY as listed). w1 shipped the first 13; w2 added the four three-bar
/// patterns, which sit on w1's ring and under the same one `CandlePattern`
/// variant.
const CATALOG: [(CandlePatternName, &str, u64); 17] = [
    (CandlePatternName::Doji, "CDLDOJI", 10),
    (CandlePatternName::DragonflyDoji, "CDLDRAGONFLYDOJI", 10),
    (CandlePatternName::GravestoneDoji, "CDLGRAVESTONEDOJI", 10),
    (CandlePatternName::Hammer, "CDLHAMMER", 11),
    (CandlePatternName::InvertedHammer, "CDLINVERTEDHAMMER", 11),
    (CandlePatternName::HangingMan, "CDLHANGINGMAN", 11),
    (CandlePatternName::ShootingStar, "CDLSHOOTINGSTAR", 11),
    (CandlePatternName::Marubozu, "CDLMARUBOZU", 10),
    (CandlePatternName::SpinningTop, "CDLSPINNINGTOP", 10),
    (CandlePatternName::Engulfing, "CDLENGULFING", 2),
    (CandlePatternName::Harami, "CDLHARAMI", 11),
    (CandlePatternName::Piercing, "CDLPIERCING", 11),
    (CandlePatternName::DarkCloudCover, "CDLDARKCLOUDCOVER", 11),
    // r5.s1.w2: the three-bar patterns. ThreeBlackCrows' `L` is 13, one bar
    // more than its conditions need — TA-Lib's declared formula, used as listed.
    (CandlePatternName::MorningStar, "CDLMORNINGSTAR", 12),
    (CandlePatternName::EveningStar, "CDLEVENINGSTAR", 12),
    (
        CandlePatternName::ThreeWhiteSoldiers,
        "CDL3WHITESOLDIERS",
        12,
    ),
    (CandlePatternName::ThreeBlackCrows, "CDL3BLACKCROWS", 13),
];

/// Which CSVs each pattern is read from. `Piercing`, `DarkCloudCover` and
/// `ThreeBlackCrows` never fire on the fixture bars (crypto bars open at the
/// prior close, so the strict real-body gap tests the definitions state reject
/// every bar), so each is read from its own synthetic sequence as well.
#[derive(Clone, Copy)]
struct Source {
    /// The file name under `tests/fixtures/candle-patterns/`.
    file: &'static str,
    /// Whether every catalogued pattern is compared on this file, or only one.
    only: Option<CandlePatternName>,
}

const SOURCES: [Source; 5] = [
    Source {
        file: "talib-reference-15m.csv",
        only: None,
    },
    Source {
        file: "talib-reference-4h.csv",
        only: None,
    },
    Source {
        file: "talib-synthetic-CDLPIERCING.csv",
        only: Some(CandlePatternName::Piercing),
    },
    Source {
        file: "talib-synthetic-CDLDARKCLOUDCOVER.csv",
        only: Some(CandlePatternName::DarkCloudCover),
    },
    Source {
        file: "talib-synthetic-CDL3BLACKCROWS.csv",
        only: Some(CandlePatternName::ThreeBlackCrows),
    },
];

/// The patterns a source is compared on.
fn patterns_for(source: Source) -> Vec<(CandlePatternName, &'static str, u64)> {
    CATALOG
        .iter()
        .filter(|(pattern, _, _)| source.only.is_none_or(|only| only == *pattern))
        .copied()
        .collect()
}

// ---------------------------------------------------------------------------
// (a) Reference parity
// ---------------------------------------------------------------------------

/// The parity contract, per pattern and per source: rows `i < L` are `None`,
/// rows `i >= L` are `Some` and equal TA-Lib's integer exactly.
///
/// A mismatch prints pattern, file, row, `open_time`, expected and actual, then
/// names every mismatch (capped) in the panic message. **The exception list is
/// empty**: a mismatch is a stop, never a tolerance and never a skipped row.
#[test]
fn every_pattern_matches_talib_exactly_on_every_reference_bar() {
    let mut mismatches: Vec<String> = Vec::new();
    let mut compared: BTreeMap<&str, usize> = BTreeMap::new();

    for source in SOURCES {
        let table = cp::read_csv(source.file);
        for (pattern, column, lookback) in patterns_for(source) {
            let values = cp::stream(&table.candles, pattern);
            assert_eq!(
                values.len(),
                table.candles.len(),
                "{pattern:?}: one value per fed bar"
            );
            let expected = table
                .columns
                .get(column)
                .unwrap_or_else(|| panic!("{}: no {column} column", source.file));

            for (row, value) in values.iter().enumerate() {
                let index = row as u64;
                if index < lookback {
                    if value.is_some() {
                        mismatches.push(format!(
                            "NOT-READY pattern={column} series={} row={row} \
                             open_time={} expected=None actual={}",
                            source.file,
                            table.open_times[row],
                            fmt(*value),
                        ));
                    }
                    continue;
                }
                let want = Decimal::from(expected[row]);
                if *value != Some(want) {
                    mismatches.push(format!(
                        "MISMATCH pattern={column} series={} row={row} \
                         open_time={} expected={} actual={}",
                        source.file,
                        table.open_times[row],
                        want,
                        fmt(*value),
                    ));
                }
            }
            *compared.entry(source.file).or_default() += table.candles.len();
        }
    }

    assert!(
        mismatches.is_empty(),
        "TA-Lib parity failed: {} mismatch(es) across {:?}; no tolerance is applied and \
         no row is skipped (a real disagreement with the definitions is a stop, not a \
         test edit). First {}:\n{}",
        mismatches.len(),
        compared,
        mismatches.len().min(20),
        head(&mismatches, 20),
    );
}

/// A value as the parity line prints it.
fn fmt(value: Option<Decimal>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "None".to_owned(),
    }
}

/// The first `n` lines, joined.
fn head(lines: &[String], n: usize) -> String {
    let mut out = String::new();
    for line in lines.iter().take(n) {
        let _ = writeln!(out, "  {line}");
    }
    out
}

// ---------------------------------------------------------------------------
// (b) The occurrence floor
// ---------------------------------------------------------------------------

/// Each pattern must read non-zero at least three times across its sources — the
/// floor grill Q3 sets so a pattern can never pass the parity suite vacuously.
/// The count is measured across each pattern's whole source set, because a
/// pattern rare on 4h (`InvertedHammer` never fires there) is common on 15m.
#[test]
fn every_pattern_occurs_at_least_three_times() {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();

    for source in SOURCES {
        let table = cp::read_csv(source.file);
        for (pattern, column, _) in patterns_for(source) {
            let _ = column;
            let non_zero = cp::stream(&table.candles, pattern)
                .into_iter()
                .filter(|value| value.is_some_and(|v| !v.is_zero()))
                .count();
            *counts.entry(catalog_name(pattern)).or_default() += non_zero;
        }
    }

    assert_eq!(counts.len(), CATALOG.len(), "every pattern is measured");
    for (pattern, _, _) in CATALOG {
        let count = counts[catalog_name(pattern)];
        assert!(
            count >= 3,
            "{pattern:?} fires only {count} time(s) across its reference files; the floor \
             is 3, so this pattern could pass vacuously (grill Q3)"
        );
    }
}

/// The catalog key for a pattern — its TA-Lib function name, so the count map
/// reads in the reference's own vocabulary.
fn catalog_name(pattern: CandlePatternName) -> &'static str {
    CATALOG
        .iter()
        .find(|(candidate, _, _)| *candidate == pattern)
        .map(|(_, column, _)| *column)
        .expect("the pattern is in the catalog")
}

// ---------------------------------------------------------------------------
// (c) No repaint
// ---------------------------------------------------------------------------

/// Every prefix of the 4h series, and a fixed sample of prefixes of the 15m
/// series, must reproduce the full series' values over the overlap.
///
/// 4h is 186 bars, so every cut point is cheap and exhaustive. The 15m series
/// is 2,976 bars; the sampled cuts are a fixed stride plus the extremes, so a
/// failure is reproducible, and they include the bars around every lookback
/// boundary.
#[test]
fn no_pattern_repaints_under_truncation() {
    for (file, cuts) in [
        ("talib-reference-4h.csv", CutSet::All),
        ("talib-reference-15m.csv", CutSet::Sampled),
    ] {
        let table = cp::read_csv(file);
        for (pattern, column, _) in CATALOG {
            let full = cp::stream(&table.candles, pattern);
            let mut checked = 0_usize;
            for cut in cuts.of(table.candles.len()) {
                let prefix = cp::stream(&table.candles[..cut], pattern);
                assert_eq!(
                    prefix.as_slice(),
                    &full[..cut],
                    "{column} on {file}: the values on the first {cut} bars must not \
                     depend on any later bar (no repaint)"
                );
                checked += 1;
            }
            assert!(checked > 0, "{column} on {file}: at least one cut point");
        }
    }
}

/// Which cut points a series is checked at.
#[derive(Clone, Copy)]
enum CutSet {
    /// Every prefix from 1 bar to the full series.
    All,
    /// A fixed stride, plus the ends and the lookback neighbourhoods.
    Sampled,
}

impl CutSet {
    /// The cut points for a series of `len` bars.
    fn of(self, len: usize) -> Vec<usize> {
        match self {
            CutSet::All => (1..=len).collect(),
            CutSet::Sampled => {
                let mut cuts: Vec<usize> = (1..=len).step_by(97).collect();
                for extra in [1, 2, 9, 10, 11, 12, 13, 14, len.saturating_sub(1), len] {
                    if extra > 0 && extra <= len {
                        cuts.push(extra);
                    }
                }
                cuts.sort_unstable();
                cuts.dedup();
                cuts
            }
        }
    }
}

// ---------------------------------------------------------------------------
// (d) Determinism
// ---------------------------------------------------------------------------

/// NFR-2: streaming the 15m series twice yields a byte-identical
/// `Vec<Option<Decimal>>` for every pattern.
#[test]
fn every_pattern_streams_twice_byte_identically() {
    let table = cp::read_csv("talib-reference-15m.csv");
    for (pattern, column, _) in CATALOG {
        assert_eq!(
            cp::stream(&table.candles, pattern),
            cp::stream(&table.candles, pattern),
            "{column}: repeated runs must yield an identical Vec<Option<Decimal>>"
        );
    }
}

// ---------------------------------------------------------------------------
// (e) Through the engine
// ---------------------------------------------------------------------------

/// The engine's dispatch, not only the struct: a `CandlePattern` spec built by
/// `IndicatorEngine::from_specs` must yield exactly the direct stream on the
/// primary series.
#[test]
fn the_engine_dispatches_candle_pattern_exactly_as_the_direct_stream() {
    let table = cp::read_csv("talib-reference-15m.csv");
    for (pattern, column, _) in CATALOG {
        let spec = IndicatorSpec::CandlePattern { pattern };
        let mut engine =
            IndicatorEngine::from_specs(std::slice::from_ref(&spec)).expect("engine builds");
        let value = pulse::CompiledValue::Indicator {
            series: Series::Primary,
            spec,
            lag: 0,
        };
        let through_engine: Vec<Option<Decimal>> = table
            .candles
            .iter()
            .map(|candle| {
                engine.step(candle);
                engine.current(&value)
            })
            .collect();
        assert_eq!(
            through_engine,
            cp::stream(&table.candles, pattern),
            "{column}: the engine's value at every bar must equal the direct stream"
        );
    }
}

/// The `htf` operand (grill G1): a strategy carrying `CandlePattern` on
/// `series: Htf` compiles, builds its higher-timeframe engine, and yields the
/// same values the direct stream gives on the H4 series. No new code is needed
/// for the closed-H4-bar rule — the indicator is an ordinary `Indicator`.
#[test]
fn the_pattern_works_on_the_htf_operand() {
    let table = cp::read_csv("talib-reference-4h.csv");
    let spec = IndicatorSpec::CandlePattern {
        pattern: CandlePatternName::Engulfing,
    };
    let dsl = StrategyDsl {
        schema_version: pulse::SchemaVersion::CURRENT,
        name: "htf candle pattern".to_owned(),
        direction: Direction::Long,
        entry: Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Htf,
                spec: spec.clone(),
            },
            op: Comparator::Gt,
            rhs: ValueSource::Constant {
                value: Decimal::ZERO,
            },
        },
        filters: vec![],
        exits: vec![ExitRule::StopLoss {
            distance_pct: SweepableValue::Fixed(Decimal::from_str("0.05").unwrap()),
        }],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(Decimal::from_str("0.01").unwrap()),
            max_leverage: SweepableValue::Fixed(Decimal::from(3)),
        },
    };
    let strategy =
        compile(&validate(&dsl).expect("the strategy validates")).expect("the strategy compiles");
    assert!(
        strategy.needs_htf(),
        "the strategy consumes the higher timeframe"
    );
    assert!(
        strategy
            .required_htf_indicators()
            .iter()
            .any(|required| matches!(required, IndicatorSpec::CandlePattern { .. })),
        "the candle pattern is an HTF requirement"
    );

    let mut engine =
        IndicatorEngine::for_series(&strategy, Series::Htf).expect("HTF engine builds");
    let value = pulse::CompiledValue::Indicator {
        series: Series::Htf,
        spec,
        lag: 0,
    };
    let through_engine: Vec<Option<Decimal>> = table
        .candles
        .iter()
        .map(|candle| {
            engine.step(candle);
            engine.current(&value)
        })
        .collect();
    assert_eq!(
        through_engine,
        cp::stream(&table.candles, CandlePatternName::Engulfing),
        "the H4 engine's engulfing values must equal the direct stream"
    );
    assert!(
        engine.is_warm(),
        "the HTF engine warms on the 4h series (a plain lookback, never a level's carve-out)"
    );
}

/// The engine's warm gate treats the variant as an ordinary warm-up: the first
/// bar a slot can carry a value is the pattern's first ready bar plus one (the
/// #36 rule that a slot needs a current AND a previous value), never earlier.
#[test]
fn the_engine_warms_at_the_plain_lookback() {
    let table = cp::read_csv("talib-reference-15m.csv");
    let spec = IndicatorSpec::CandlePattern {
        pattern: CandlePatternName::Engulfing,
    };
    let mut engine = IndicatorEngine::from_specs(&[spec]).expect("engine builds");
    // Engulfing's L is 2, so its first value lands on index 2 and the engine is
    // warm one bar later (index 3), when a current and a previous both exist.
    for (index, candle) in table.candles.iter().enumerate() {
        engine.step(candle);
        if index >= 3 {
            assert!(
                engine.is_warm(),
                "the engine must be warm from index 3 onwards (bar {index})"
            );
        }
    }
}

/// The fixture store and the CSVs describe the same bars. This is the check
/// that makes the parity suite meaningful: TA-Lib's output is only our answer
/// key if the candles it ran on are the candles we stream.
#[test]
fn the_fixture_store_bars_are_the_csv_bars() {
    for (timeframe, file) in [
        (Timeframe::M15, "talib-reference-15m.csv"),
        (Timeframe::H4, "talib-reference-4h.csv"),
    ] {
        let table = cp::read_csv(file);
        let stored = cp::load_fixture_series(timeframe);
        assert_eq!(
            stored.candles.len(),
            table.candles.len(),
            "{file}: the fixture store and the CSV must carry the same bar count"
        );
        for (index, (stored, csv)) in stored.candles.iter().zip(&table.candles).enumerate() {
            assert_eq!(
                (
                    stored.open_time,
                    stored.open,
                    stored.high,
                    stored.low,
                    stored.close
                ),
                (csv.open_time, csv.open, csv.high, csv.low, csv.close),
                "{file}: bar {index} must be identical in the store and the CSV"
            );
        }
    }
}

/// A sanity pin on the crate surface this suite compiles against: the
/// `CandlePatternName` enum serializes by variant name (no `rename_all`), so
/// the wire form is the spec's `{"indicator":"CandlePattern","pattern":"Engulfing"}`.
/// `tests/dsl_schema_1_4.rs` carries the full round-trip; this keeps the
/// spelling visible where the parity suite reads it.
#[test]
fn the_pattern_name_spelling_is_the_variant_name() {
    let json = serde_json::to_string(&IndicatorSpec::CandlePattern {
        pattern: CandlePatternName::DarkCloudCover,
    })
    .expect("serialize");
    assert_eq!(
        json,
        r#"{"indicator":"CandlePattern","pattern":"DarkCloudCover"}"#
    );
}

// ---------------------------------------------------------------------------
// The fixture series loader
// ---------------------------------------------------------------------------

/// A strategy-carried candle-pattern operand over the primary series — the form
/// the spec names for a rule (`Compare(CandlePattern{Engulfing}, Gt, 0)`).
#[test]
fn a_rule_reads_the_pattern_through_compare() {
    let table = cp::read_csv("talib-reference-15m.csv");
    let spec = IndicatorSpec::CandlePattern {
        pattern: CandlePatternName::Engulfing,
    };
    let dsl = StrategyDsl {
        schema_version: pulse::SchemaVersion::CURRENT,
        name: "candle pattern compare".to_owned(),
        direction: Direction::Long,
        entry: Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Primary,
                spec: spec.clone(),
            },
            op: Comparator::Gt,
            rhs: ValueSource::Constant {
                value: Decimal::ZERO,
            },
        },
        filters: vec![],
        exits: vec![ExitRule::StopLoss {
            distance_pct: SweepableValue::Fixed(Decimal::from_str("0.05").unwrap()),
        }],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(Decimal::from_str("0.01").unwrap()),
            max_leverage: SweepableValue::Fixed(Decimal::from(3)),
        },
    };
    let strategy = compile(&validate(&dsl).expect("validates")).expect("compiles");
    let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");
    let value = pulse::CompiledValue::Indicator {
        series: Series::Primary,
        spec,
        lag: 0,
    };
    // `Gt 0` reads "a bullish engulfing fired", which the `±80` partial form also
    // satisfies — the distinction the amended grill Q1 keeps.
    let expected = cp::stream(&table.candles, CandlePatternName::Engulfing);
    let mut fired = 0_usize;
    for (index, candle) in table.candles.iter().enumerate() {
        engine.step(candle);
        let read = engine.current(&value);
        assert_eq!(
            read, expected[index],
            "bar {index}: engine vs direct stream"
        );
        if read.is_some_and(|v| v > Decimal::ZERO) {
            fired += 1;
        }
    }
    assert!(fired >= 3, "the engulfing rule fires on at least 3 bars");
}

/// The direct stream and the engine must agree on the fixture store's own 15m
/// series too — the store's bars are the ones the backtest consumes, and this
/// ties the CSV-parsed series to them.
#[test]
fn the_stored_series_streams_to_the_same_values() {
    let series: CandleSeries = cp::load_fixture_series(Timeframe::M15);
    let table = cp::read_csv("talib-reference-15m.csv");
    for (pattern, column, _) in CATALOG {
        assert_eq!(
            cp::stream(&series.candles, pattern),
            cp::stream(&table.candles, pattern),
            "{column}: the store and the CSV must stream identically"
        );
    }
}

/// The suite reads the CSVs from one directory; a missing file is a fixture
/// error, never a silently skipped pattern.
#[test]
fn every_reference_file_is_present() {
    for source in SOURCES {
        let path = cp::fixture_path(source.file);
        assert!(
            path.is_file(),
            "missing reference fixture: {}",
            path.display()
        );
    }
}
