//! r5.s1.w3 — the 4h support-rejection example (demo line d100).
//!
//! `examples/strategies/support-rejection-4h.json` is a checked-in, schema-1.4.0,
//! long-only strategy: it enters when a **closed H4 bar reaches a 4h support
//! level and closes back above it**, and its filters require the same bar to
//! carry a **named bullish candlestick pattern** (its value `> 0`). This suite
//! proves the example validates under 1.4.0, backtests deterministically on the
//! committed fixture store, takes at least three trades, and enters **every**
//! one on a bar where both the support touch and the named pattern held —
//! checked from independently streamed `Support` and `CandlePattern` series,
//! never from the engine's own decision.
//!
//! ## The support-touch trap (why the entry reads a lagged level)
//!
//! `Support` at bar t is by definition the **highest** qualifying level strictly
//! **below** the close of bar t, so `close > Support` always holds when a level
//! exists and a same-bar touch test against it says nothing. The example
//! therefore compares the bar's low with the level **as of the previous H4
//! bar**: `low ≤ lag(Support{…}, 1) × (1 + k)` **and** `close > lag(Support{…},
//! 1)`. The second condition is not redundant — without it a bar that sliced
//! through the level and closed below it would still pass the first, and that is
//! a breakdown, not a rejection.
//!
//! **Considered and not taken:** a same-bar pierce test, `low ≤ Support{…}` on
//! the bar itself, is also correct and is *not* vacuous (the level is derived
//! from the close while the test reads the low). It was not taken because the
//! item keeps one doctrine with r4.s6's `lag(…, 1)` zone form, and because the
//! lagged level is the one a trader marks on the chart before the bar opens
//! (operator ruling, 2026-10-15).
//!
//! ## Calibration (done before freezing; both conditions in every run)
//!
//! The grid was swept on this exact fixture (primary = the committed M15
//! snapshot, 2976 bars; htf = its committed H4 snapshot, 186 closed bars;
//! default costs; the real BTCUSDT filters), with **both** conditions always in
//! place — the lagged support touch (the entry plus filter 0) and the named
//! `Hammer` pattern (filter 1) — and no run was allowed to drop either:
//!
//! - **Pass A** (432 runs): the six bullish patterns that fire on the H4
//!   fixture × {htf, primary} × span {(1,1),(2,1),(2,2),(3,1)} × `tolerance_pct`
//!   {"0.003","0.01","0.03"} × `min_touches` 2 × `lookback` 100 × `m`
//!   {"1","1.002","1.01"}; exits fixed at stop 0.02 / tp 2.
//! - **Pass B** (5 760 runs): the four htf candidates that cleared the floor in
//!   pass A — `hammer`, `engulfing`, `harami`, `spinning_top` — × span (6, adding
//!   (3,2), (4,2)) × `tolerance_pct` {"0.002","0.003","0.005","0.01","0.02",
//!   "0.05"} × `min_touches` {2,3} × `lookback` {50,100,150,200} × `m`
//!   {"1","1.001","1.002","1.005","1.01"}; exits fixed.
//! - **Pass C** (56 runs): the chosen pattern's 14 qualifying cells × the four
//!   exit pairs swept — stop `0.015` / tp `1`, stop `0.015` / tp `2`, stop
//!   `0.02` / tp `2`, stop `0.03` / tp `2`.
//! - **Pass D**: the plateau was read off pass B's full grid — the ±1
//!   neighbours of the chosen cell are already swept there — not re-run.
//!
//! The selection rule, fixed before the sweep: the earliest pattern in the
//! purity order `Hammer` → `DragonflyDoji` → `Engulfing` → `Harami` →
//! `Marubozu` → `SpinningTop` whose region holds a plateau (≥3 one-step
//! neighbours with the same trade count) wins; then the widest plateau, then
//! the higher trade count (margin over the floor), then the more conservative
//! exits. `Hammer` won: **4 trades** with stop `0.015` / `1R`, and its 14
//! qualifying cells all produce the *same* 4 trades (one identical net P&L), so
//! the golden is not a single cell's accident. For the record, the wider
//! regions the rule passed over: `spinning_top` 618 qualifying cells (max 11
//! trades), `engulfing` 360 (max 9), `harami` 115 (max 7) — `Hammer` is the
//! operator's own rejection candle and the tightest reading of "a rejection at
//! support". The chosen cell is the widest plateau: `Support{left 1, right 1,
//! tolerance_pct "0.05", min_touches 2, lookback 50}` on `htf`, touch band
//! `m = 1.005`, `StopLoss 0.015`, `TakeProfit 1R`, risk `0.01` / leverage `3`.
//!
//! ## Golden regeneration (deliberate change ⇒ reviewed golden diff)
//!
//! The golden constants were captured from the calibrated run. To regenerate
//! after an *intentional* change, run the print probe and copy the emitted
//! `GOLDEN_TRADE_COUNT` / `GOLDEN_NET_PNL` lines into the constants:
//!
//! ```text
//! cargo test --test candle_pattern_example print_golden_for_regeneration -- --ignored --nocapture
//! ```
//!
//! A silent drift fails `golden_backtest_reproduces_frozen_trade_count_and_pnl`
//! loudly; a deliberate change is a one-line, reviewed golden diff.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::path::PathBuf;

use pulse::{
    BacktestConfig, BacktestResult, BinanceAdapter, CandlePatternName, CandleSeries, CandleStore,
    Comparator, CompiledStrategy, CompiledValue, Condition, EvalContext, ExchangeAdapter,
    IndicatorEngine, IndicatorSpec, Migrator, Pair, PriceField, SchemaVersion, Series, SeriesEnd,
    StrategyDsl, Timeframe, ValueSource, align, compile, run_backtest, validate,
};
use rust_decimal::Decimal;

/// Frozen golden: the exact number of completed trades the example produces on
/// the committed fixture. See the module header for regeneration.
const GOLDEN_TRADE_COUNT: usize = 4;

/// Frozen golden: the exact net P&L (quote currency, already net of
/// fees/funding/slippage) across the run. Exact `Decimal` — a silent engine
/// drift changes this and fails the golden assertion. See the module header for
/// regeneration.
const GOLDEN_NET_PNL: &str = "-27.091967573400553580";

/// The checked-in example this suite pins.
fn example_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/strategies/support-rejection-4h.json")
}

/// Load the example through the full read path the engine consumes:
/// `Migrator::v1().load` (version-safe) -> `validate` -> `compile`.
fn load_example() -> StrategyDsl {
    let json = std::fs::read_to_string(example_path()).expect("read the checked-in example json");
    Migrator::v1()
        .load(&json)
        .expect("the example loads through Migrator::v1")
        .dsl
}

fn compiled_example(dsl: &StrategyDsl) -> CompiledStrategy {
    compile(&validate(dsl).expect("the example validates")).expect("the example compiles")
}

/// Load one committed fixture series (no network / LLM / SQLite):
/// `with_base_dir` -> `read_head` -> `read_snapshot`.
fn load_series(timeframe: Timeframe) -> CandleSeries {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store");
    let store = CandleStore::with_base_dir(base);
    let pair = Pair::new("BTCUSDT");
    let head = store
        .read_head(&pair, timeframe)
        .expect("read HEAD")
        .expect("HEAD present in the fixture store");
    store
        .read_snapshot(&pair, timeframe, &head)
        .expect("read snapshot")
}

/// One golden run: the example over the fixture's M15 primary + H4 htf series,
/// through the real BTCUSDT filters and the default cost model.
fn run_example(
    compiled: &CompiledStrategy,
    primary: &CandleSeries,
    htf: &CandleSeries,
) -> BacktestResult {
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
    .expect("backtest runs over the fixture")
}

/// The support-touch multiplier `(1 + k)` the example's entry carries, read
/// from the document itself so the independent check cannot drift from the
/// file it checks.
fn touch_multiplier(dsl: &StrategyDsl) -> Decimal {
    let Condition::Compare { rhs, .. } = &dsl.entry else {
        panic!("the entry is a Compare, got {:?}", dsl.entry);
    };
    let ValueSource::Arith { rhs, .. } = rhs else {
        panic!("the entry's right operand is an Arith, got {rhs:?}");
    };
    let ValueSource::Constant { value } = rhs.as_ref() else {
        panic!("the touch multiplier is a constant, got {rhs:?}");
    };
    *value
}

/// The named pattern the example gates on, and the series it is read on.
fn example_pattern(dsl: &StrategyDsl) -> (CandlePatternName, Series) {
    for condition in &dsl.filters {
        if let Condition::Compare {
            lhs:
                ValueSource::Indicator {
                    series,
                    spec: IndicatorSpec::CandlePattern { pattern },
                },
            ..
        } = condition
        {
            return (*pattern, *series);
        }
    }
    panic!(
        "the example gates on a named candle pattern: {:?}",
        dsl.filters
    );
}

/// The example is a valid document at schema 1.4.0 whose entry is the lagged
/// support-touch form (the trap) and whose filters are the close-back-above and
/// the named bullish pattern.
#[test]
fn example_parses_validates_and_carries_the_support_touch_form() {
    let dsl = load_example();
    assert_eq!(
        dsl.schema_version,
        SchemaVersion::CURRENT,
        "the example loads to the current schema"
    );
    assert_eq!(
        dsl.schema_version.to_string(),
        "1.4.0",
        "the example is authored at schema 1.4.0"
    );

    // Entry: htf:low <= lag(htf:support{…}, 1) × (1 + k) — the level the
    // PREVIOUS H4 bar saw, never the same bar's (the module header's trap).
    let Condition::Compare { lhs, op, rhs } = &dsl.entry else {
        panic!("the entry is a Compare, got {:?}", dsl.entry);
    };
    assert_eq!(*op, Comparator::Lte);
    assert_eq!(
        lhs,
        &ValueSource::Price {
            series: Series::Htf,
            field: PriceField::Low,
        },
        "the entry's left operand is the H4 low"
    );
    let ValueSource::Arith { op, lhs, .. } = rhs else {
        panic!("the entry's right operand is an Arith, got {rhs:?}");
    };
    assert_eq!(*op, pulse::ArithOp::Mul);
    let ValueSource::Lag { value, bars } = lhs.as_ref() else {
        panic!("the touch band multiplies a Lag, got {lhs:?}");
    };
    assert_eq!(*bars, 1, "the lag reads exactly the previous H4 bar");
    assert!(
        matches!(
            value.as_ref(),
            ValueSource::Indicator {
                series: Series::Htf,
                spec: IndicatorSpec::Support { .. },
            }
        ),
        "the lagged value is the H4 Support, got {value:?}"
    );

    // Filter 0: htf:close > lag(htf:support{…}, 1) — the close back above the
    // zone the previous H4 bar saw.
    let Condition::Compare { lhs, op, rhs } = &dsl.filters[0] else {
        panic!("filter 0 is a Compare, got {:?}", dsl.filters[0]);
    };
    assert_eq!(*op, Comparator::Gt);
    assert_eq!(
        lhs,
        &ValueSource::Price {
            series: Series::Htf,
            field: PriceField::Close,
        },
    );
    assert!(
        matches!(
            rhs,
            ValueSource::Lag { value, bars: 1 }
                if matches!(
                    value.as_ref(),
                    ValueSource::Indicator {
                        series: Series::Htf,
                        spec: IndicatorSpec::Support { .. },
                    }
                )
        ),
        "filter 0 compares the H4 close with the previous bar's Support, got {rhs:?}"
    );

    // Filter 1: the named candle pattern fired (`> 0`).
    let Condition::Compare { lhs, op, rhs } = &dsl.filters[1] else {
        panic!("filter 1 is a Compare, got {:?}", dsl.filters[1]);
    };
    assert_eq!(*op, Comparator::Gt);
    assert!(
        matches!(
            lhs,
            ValueSource::Indicator {
                spec: IndicatorSpec::CandlePattern { .. },
                ..
            }
        ),
        "filter 1's left operand is the named candle pattern, got {lhs:?}"
    );
    assert_eq!(
        rhs,
        &ValueSource::Constant {
            value: Decimal::ZERO
        },
        "the pattern is read as `> 0` (any bullish form, including ±80's +80)"
    );

    // The compiled strategy streams both on the HTF engine.
    let compiled = compiled_example(&dsl);
    for want in ["Support", "CandlePattern"] {
        assert!(
            compiled
                .required_htf_indicators()
                .iter()
                .any(|spec| format!("{spec:?}").contains(want)),
            "the HTF requirement set must contain {want}: {:?}",
            compiled.required_htf_indicators()
        );
    }
}

/// The regression anchor: the example on the fixture reproduces the **frozen
/// golden** trade count and net P&L exactly. A silent engine drift fails this
/// loudly; a deliberate change is a reviewed golden diff (see the module
/// header's regeneration note).
#[test]
fn golden_backtest_reproduces_frozen_trade_count_and_pnl() {
    let primary = load_series(Timeframe::M15);
    let htf = load_series(Timeframe::H4);
    let compiled = compiled_example(&load_example());
    let result = run_example(&compiled, &primary, &htf);

    assert_eq!(
        result.trades.len(),
        GOLDEN_TRADE_COUNT,
        "golden trade_count drifted: expected {GOLDEN_TRADE_COUNT}, got {} \
         (regenerate the golden only on a deliberate change)",
        result.trades.len()
    );
    let expected_net_pnl = Decimal::from_str_exact(GOLDEN_NET_PNL).expect("GOLDEN_NET_PNL parses");
    assert_eq!(
        result.net_pnl, expected_net_pnl,
        "golden net P&L drifted: expected {expected_net_pnl}, got {}",
        result.net_pnl
    );
}

/// Non-vacuity: the example must actually trade — at least three completed
/// trades with **both** conditions in place.
#[test]
fn golden_run_is_non_vacuous_at_least_three_trades() {
    let primary = load_series(Timeframe::M15);
    let htf = load_series(Timeframe::H4);
    let compiled = compiled_example(&load_example());
    let result = run_example(&compiled, &primary, &htf);

    assert!(
        result.trades.len() >= 3,
        "expected at least 3 completed trades (non-vacuous example), got {}",
        result.trades.len()
    );
}

/// The falsifiable half: for **every** trade, at its `entry_signal_time` the
/// support touch held (the signal bar's paired H4 low reached the level the
/// previous H4 bar carried, and the H4 close finished above it) **and** the
/// named pattern read `> 0`.
///
/// Both series are streamed here, independently of the engine's own entry
/// decision: the `Support` and `CandlePattern` specs are extracted from the
/// example's own compiled strategy and stepped by a fresh `IndicatorEngine`,
/// and the signal bar's paired H4 candle comes from the public no-look-ahead
/// feed (`align`), not from the backtest engine.
#[test]
fn every_entry_sits_on_a_bar_where_the_pattern_held_and_the_support_was_touched() {
    let primary = load_series(Timeframe::M15);
    let htf = load_series(Timeframe::H4);
    let dsl = load_example();
    let compiled = compiled_example(&dsl);
    let result = run_example(&compiled, &primary, &htf);
    assert!(
        !result.trades.is_empty(),
        "the touch/pattern check is meaningless on an empty trade list"
    );

    let multiplier = touch_multiplier(&dsl);
    let (pattern, pattern_series) = example_pattern(&dsl);

    // The example's own Support spec, streamed over the H4 candles.
    let support = compiled
        .required_htf_indicators()
        .iter()
        .find(|spec| matches!(spec, IndicatorSpec::Support { .. }))
        .expect("the example reads Support on the H4 series")
        .clone();
    let mut engine = IndicatorEngine::from_specs(std::slice::from_ref(&support))
        .expect("a fresh engine builds from the support spec");
    let value = CompiledValue::Indicator {
        series: Series::Htf,
        spec: support,
        lag: 0,
    };
    let mut levels: Vec<Option<Decimal>> = Vec::with_capacity(htf.candles.len());
    for candle in &htf.candles {
        engine.step(candle);
        levels.push(engine.current(&value));
    }

    // The example's own pattern spec, streamed over the series it is read on.
    let pattern_spec = IndicatorSpec::CandlePattern { pattern };
    let mut pattern_engine = IndicatorEngine::from_specs(std::slice::from_ref(&pattern_spec))
        .expect("a fresh engine builds from the pattern spec");
    let pattern_value = CompiledValue::Indicator {
        series: pattern_series,
        spec: pattern_spec,
        lag: 0,
    };
    let pattern_series_data = if pattern_series == Series::Htf {
        &htf
    } else {
        &primary
    };
    let mut patterns: Vec<Option<Decimal>> = Vec::with_capacity(pattern_series_data.candles.len());
    for candle in &pattern_series_data.candles {
        pattern_engine.step(candle);
        patterns.push(pattern_engine.current(&pattern_value));
    }

    // The same no-look-ahead pairing the engine walks: the most-recent H4 bar
    // whose close_time is at or before each primary bar's close_time.
    let aligned = align(&primary, Some(&htf), None);
    for (i, trade) in result.trades.iter().enumerate() {
        let (primary_index, bar) = aligned
            .iter()
            .enumerate()
            .find(|(_, bar)| bar.primary.close_time == trade.entry_signal_time)
            .unwrap_or_else(|| {
                panic!(
                    "trade {i}: entry_signal_time {} resolves to a primary bar",
                    trade.entry_signal_time
                )
            });
        let paired = bar
            .htf
            .unwrap_or_else(|| panic!("trade {i}: the signal bar is paired with a closed H4 bar"));
        let j = htf
            .candles
            .iter()
            .position(|candle| candle.open_time == paired.open_time)
            .expect("the paired H4 candle is in the series");
        assert!(
            j >= 1,
            "trade {i}: the signal bar's H4 bar {j} has a previous bar"
        );
        let level = levels[j - 1].unwrap_or_else(|| {
            panic!(
                "trade {i}: the Support level exists on the previous H4 bar (bar {})",
                j - 1
            )
        });
        assert!(
            paired.low <= level * multiplier,
            "trade {i}: the support touch must have held at entry_signal_time {} — \
             H4 bar {j} low {} vs the previous bar's support {level} × {multiplier}",
            trade.entry_signal_time,
            paired.low
        );
        assert!(
            paired.close > level,
            "trade {i}: the close-back-above must have held at entry_signal_time {} — \
             H4 bar {j} close {} vs the previous bar's support {level}",
            trade.entry_signal_time,
            paired.close
        );

        let at = if pattern_series == Series::Htf {
            j
        } else {
            primary_index
        };
        let pattern_read = patterns[at].unwrap_or_else(|| {
            panic!("trade {i}: the {pattern:?} pattern is past its warm-up on the signal bar")
        });
        assert!(
            pattern_read > Decimal::ZERO,
            "trade {i}: the named pattern must have held at entry_signal_time {} — \
             {pattern:?} read {pattern_read}",
            trade.entry_signal_time
        );
    }
}

/// The example is deterministic: two runs produce byte-identical trade lists.
#[test]
fn two_runs_give_byte_identical_trade_lists() {
    let primary = load_series(Timeframe::M15);
    let htf = load_series(Timeframe::H4);
    let compiled = compiled_example(&load_example());
    let first = run_example(&compiled, &primary, &htf);
    let second = run_example(&compiled, &primary, &htf);

    assert_eq!(
        first.trades, second.trades,
        "two runs over the same fixture produce the same trades"
    );
    assert_eq!(
        serde_json::to_vec(&first.trades).expect("trades serialize"),
        serde_json::to_vec(&second.trades).expect("trades serialize"),
        "the two trade lists are byte-identical"
    );
}

/// Regeneration probe (ignored by default): prints the golden trade count and
/// exact net P&L, plus one row per trade (its `entry_signal_time`, the paired
/// H4 candle's low and close, the previous H4 bar's support level, the pattern
/// value and the trade's own P&L) — the evidence the item's report carries. Run
/// with `--ignored --nocapture` after a deliberate change; copy the emitted
/// constants up.
#[test]
#[ignore = "regeneration probe; run with --ignored --nocapture to refresh the golden"]
fn print_golden_for_regeneration() {
    let primary = load_series(Timeframe::M15);
    let htf = load_series(Timeframe::H4);
    let dsl = load_example();
    let compiled = compiled_example(&dsl);
    let result = run_example(&compiled, &primary, &htf);

    println!("GOLDEN_TRADE_COUNT = {}", result.trades.len());
    println!("GOLDEN_NET_PNL = \"{}\"", result.net_pnl);

    let (pattern, pattern_series) = example_pattern(&dsl);
    let support = compiled
        .required_htf_indicators()
        .iter()
        .find(|spec| matches!(spec, IndicatorSpec::Support { .. }))
        .expect("the example reads Support on the H4 series")
        .clone();
    let mut engine =
        IndicatorEngine::from_specs(std::slice::from_ref(&support)).expect("engine builds");
    let value = CompiledValue::Indicator {
        series: Series::Htf,
        spec: support,
        lag: 0,
    };
    let mut levels: Vec<Option<Decimal>> = Vec::with_capacity(htf.candles.len());
    for candle in &htf.candles {
        engine.step(candle);
        levels.push(engine.current(&value));
    }

    let pattern_spec = IndicatorSpec::CandlePattern { pattern };
    let mut pattern_engine =
        IndicatorEngine::from_specs(std::slice::from_ref(&pattern_spec)).expect("engine builds");
    let pattern_value = CompiledValue::Indicator {
        series: pattern_series,
        spec: pattern_spec,
        lag: 0,
    };
    let pattern_data = if pattern_series == Series::Htf {
        &htf
    } else {
        &primary
    };
    let mut patterns: Vec<Option<Decimal>> = Vec::with_capacity(pattern_data.candles.len());
    for candle in &pattern_data.candles {
        pattern_engine.step(candle);
        patterns.push(pattern_engine.current(&pattern_value));
    }

    let aligned = align(&primary, Some(&htf), None);
    for (i, trade) in result.trades.iter().enumerate() {
        let (primary_index, bar) = aligned
            .iter()
            .enumerate()
            .find(|(_, bar)| bar.primary.close_time == trade.entry_signal_time)
            .expect("entry_signal_time resolves to a primary bar");
        let paired = bar
            .htf
            .expect("the signal bar is paired with a closed H4 bar");
        let j = htf
            .candles
            .iter()
            .position(|candle| candle.open_time == paired.open_time)
            .expect("paired candle is in the series");
        let at = if pattern_series == Series::Htf {
            j
        } else {
            primary_index
        };
        println!(
            "trade {i}: entry_signal_time={} h4_low={} h4_close={} previous_support={:?} \
             pattern={pattern:?}:{:?} exit_reason={:?} realized_pnl={}",
            trade.entry_signal_time,
            paired.low,
            paired.close,
            levels[j - 1],
            patterns[at],
            trade.exit_reason,
            trade.realized_pnl
        );
    }
}
