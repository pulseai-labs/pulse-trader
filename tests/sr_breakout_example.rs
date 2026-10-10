//! r4.s6.w4 — the initial 4h S/R breakout example (demo line d81).
//!
//! `examples/strategies/sr-breakout-4h.json` is a checked-in, schema-1.3.0,
//! long-only strategy: it enters when the **aligned closed H4 close crosses
//! above the previous H4 bar's `Resistance` level**, filtered by the breaking
//! H4 candle's anatomy (`body > 0.5 × range`) and by a break of the falling
//! trendline through the last two confirmed pivot highs, and exits on a percent
//! stop with a fixed-R take-profit. This suite proves it validates under 1.3.0,
//! backtests deterministically on the committed fixture store, takes at least
//! three trades, and enters **every** one on a bar where its zone break held —
//! checked from an independently streamed `Resistance` series, never from the
//! engine's own decision.
//!
//! ## The zone-break trap (why the entry is not `close > resistance`)
//!
//! `Resistance` at bar t is by definition the lowest qualifying level strictly
//! **above** the close of bar t, so `close > resistance` and a same-bar
//! `CrossesAbove(close, resistance)` can never hold. The example breaks against
//! the **previous** bar's level instead:
//! `CrossesAbove(htf close, lag(htf Resistance{…}, 1))` — the lag reads one H4
//! bar back (the operand's own series), so the cross fires exactly when the
//! level is cleared.
//!
//! ## Calibration (C2 — done before freezing)
//!
//! The parameter grid was swept on this exact fixture (primary = the committed
//! M15 snapshot, 2976 bars; htf = its committed H4 snapshot, 186 closed bars;
//! default costs; the real BTCUSDT filters), with **both filters always in
//! place** — the anatomy threshold fixed at the spec's `body > 0.5 × range`, and
//! no combination allowed to drop or swap a filter:
//!
//! - resistance `(left, right)` ∈ {(1,1),(2,1),(2,2),(3,1),(3,2),(4,2)};
//! - `tolerance_pct` ∈ {"0.001","0.002","0.005","0.01","0.02","0.03","0.05","0.1"};
//! - `min_touches` ∈ {2,3}; `lookback` ∈ {30,50,100,150,200};
//! - trendline-highs `(left, right)` ∈ {(1,1),(2,1),(3,1),(3,2),(4,2)};
//! - exits `stop_loss_pct` ∈ {"0.015","0.02","0.03","0.05"} × `take_profit_r`
//!   ∈ {"1","1.5","2","3"}.
//!
//! 8 172 combinations ran (in passes, before the exits were added); 229 take
//! ≥3 trades with both filters, the best take 8. The chosen set — resistance
//! `(2,1)`, `tolerance_pct "0.01"`, `min_touches 2`, `lookback 150`; trendline
//! highs `(2,1)`; a `1.5%` stop and a `1R` take-profit — takes **5** trades and
//! sits on a measured plateau: `tolerance_pct` 0.01 and 0.02, `lookback` 150
//! and 200, and trendline spans (1,1) and (2,1) all produce the same 5 trades,
//! so the golden is not a single cell's accident. The full sweep and the chosen
//! values are recorded in the item's report.
//!
//! ## Golden regeneration (deliberate change ⇒ reviewed golden diff)
//!
//! The golden constants were captured from the calibrated run. To regenerate
//! after an *intentional* change, run the print probe and copy the emitted
//! `GOLDEN_TRADE_COUNT` / `GOLDEN_NET_PNL` lines into the constants:
//!
//! ```text
//! cargo test --test sr_breakout_example print_golden_for_regeneration -- --ignored --nocapture
//! ```
//!
//! A silent drift fails `golden_backtest_reproduces_frozen_trade_count_and_pnl`
//! loudly; a deliberate change is a one-line, reviewed golden diff.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use pulse::{
    BacktestConfig, BacktestResult, BinanceAdapter, CandleSeries, CandleStore, CompiledStrategy,
    CompiledValue, Condition, EvalContext, ExchangeAdapter, IndicatorEngine, IndicatorSpec,
    Migrator, Pair, PriceField, SchemaVersion, Series, SeriesEnd, StrategyDsl, Timeframe,
    ValueSource, align, compile, run_backtest, validate,
};
use rust_decimal::Decimal;

/// Frozen golden: the exact number of completed trades the example produces on
/// the committed fixture. See the module header for regeneration.
const GOLDEN_TRADE_COUNT: usize = 5;

/// Frozen golden: the exact net P&L (quote currency, already net of
/// fees/funding/slippage) across the run. Exact `Decimal` — a silent engine
/// drift changes this and fails the golden assertion. See the module header for
/// regeneration.
const GOLDEN_NET_PNL: &str = "70.327031281570424226";

/// The checked-in example this suite pins.
fn example_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/strategies/sr-breakout-4h.json")
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

/// The example is a valid schema-1.3.0 document whose entry is the zone-break
/// form, and whose filters are the anatomy gate and the pattern break.
#[test]
fn example_parses_validates_and_carries_the_zone_break_form() {
    let dsl = load_example();
    assert_eq!(
        dsl.schema_version,
        SchemaVersion::CURRENT,
        "the example is authored at the current schema"
    );
    assert_eq!(dsl.schema_version.to_string(), "1.3.0");

    // Entry: h4:close crosses above lag(h4:resistance{…}, 1) — the previous H4
    // bar's level, never the same bar's (the module header's trap).
    let Condition::CrossesAbove { lhs, rhs } = &dsl.entry else {
        panic!("the entry is a CrossesAbove, got {:?}", dsl.entry);
    };
    assert_eq!(
        lhs,
        &ValueSource::Price {
            series: Series::Htf,
            field: PriceField::Close,
        },
        "the entry's left operand is the H4 close"
    );
    let ValueSource::Lag { value, bars } = rhs else {
        panic!("the entry's right operand is a Lag, got {rhs:?}");
    };
    assert_eq!(*bars, 1, "the lag reads exactly the previous H4 bar");
    assert!(
        matches!(
            value.as_ref(),
            ValueSource::Indicator {
                series: Series::Htf,
                spec: IndicatorSpec::Resistance { .. },
            }
        ),
        "the lagged value is the H4 Resistance, got {value:?}"
    );

    // Filter 1: h4:body > (h4:range * 0.5) — the anatomy gate.
    let Condition::Compare { lhs, op, rhs } = &dsl.filters[0] else {
        panic!("filter 0 is a Compare, got {:?}", dsl.filters[0]);
    };
    assert_eq!(*op, pulse::Comparator::Gt);
    assert!(
        matches!(
            lhs,
            ValueSource::Indicator {
                series: Series::Htf,
                spec: IndicatorSpec::Body,
            }
        ),
        "filter 0's left operand is the H4 body, got {lhs:?}"
    );
    assert!(
        matches!(
            rhs,
            ValueSource::Arith {
                op: pulse::ArithOp::Mul,
                ..
            }
        ),
        "filter 0's right operand is the range halved, got {rhs:?}"
    );

    // Filter 2: h4:close > h4:trendline_highs{…} — the pattern break.
    let Condition::Compare { lhs, op, rhs } = &dsl.filters[1] else {
        panic!("filter 1 is a Compare, got {:?}", dsl.filters[1]);
    };
    assert_eq!(*op, pulse::Comparator::Gt);
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
            ValueSource::Indicator {
                series: Series::Htf,
                spec: IndicatorSpec::TrendlineHighs { .. },
            }
        ),
        "filter 1's right operand is the H4 trendline through pivot highs, got {rhs:?}"
    );

    // The compiled strategy reads the structure/pattern vocabulary on the HTF
    // engine — not on the primary one.
    let compiled = compiled_example(&dsl);
    for want in ["Resistance", "TrendlineHighs", "Body", "Range"] {
        assert!(
            compiled
                .required_htf_indicators()
                .iter()
                .any(|spec| { format!("{spec:?}").contains(want) }),
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

/// Non-vacuity (criterion 10 / G5): the example must actually trade — at least
/// three completed trades with **both** filters in place.
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

/// G5, the falsifiable half: for **every** trade, at its `entry_signal_time`
/// the zone-break condition held — the signal bar's paired H4 close is strictly
/// above the level the `Resistance` indicator carried on the previous H4 bar.
///
/// The level series is streamed here, independently of the engine's own entry
/// decision: the same `Resistance` spec (extracted from the example's compiled
/// strategy) is stepped over the H4 candles by a fresh `IndicatorEngine`, and
/// the signal bar's paired H4 candle comes from the public no-look-ahead feed
/// (`align`), not from the backtest engine.
#[test]
fn every_entry_sits_on_a_bar_where_the_zone_break_held() {
    let primary = load_series(Timeframe::M15);
    let htf = load_series(Timeframe::H4);
    let compiled = compiled_example(&load_example());
    let result = run_example(&compiled, &primary, &htf);
    assert!(
        !result.trades.is_empty(),
        "the break check is meaningless on an empty trade list"
    );

    // The example's own Resistance spec, streamed over the H4 candles.
    let spec = compiled
        .required_htf_indicators()
        .iter()
        .find(|spec| matches!(spec, IndicatorSpec::Resistance { .. }))
        .expect("the example reads Resistance on the H4 series")
        .clone();
    let mut engine = IndicatorEngine::from_specs(std::slice::from_ref(&spec))
        .expect("a fresh engine builds from the resistance spec");
    let value = CompiledValue::Indicator {
        series: Series::Htf,
        spec,
        lag: 0,
    };
    let mut levels: Vec<Option<Decimal>> = Vec::with_capacity(htf.candles.len());
    for candle in &htf.candles {
        engine.step(candle);
        levels.push(engine.current(&value));
    }

    // The same no-look-ahead pairing the engine walks: the most-recent H4 bar
    // whose close_time is at or before each primary bar's close_time.
    let aligned = align(&primary, Some(&htf), None);
    for (i, trade) in result.trades.iter().enumerate() {
        let bar = aligned
            .iter()
            .find(|bar| bar.primary.close_time == trade.entry_signal_time)
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
                "trade {i}: the Resistance level exists on the previous H4 bar (bar {})",
                j - 1
            )
        });
        assert!(
            paired.close > level,
            "trade {i}: the zone break must have held at entry_signal_time {} — \
             H4 bar {j} close {} vs the previous bar's resistance {level}",
            trade.entry_signal_time,
            paired.close
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
/// H4 close, and the previous H4 bar's resistance level) — the evidence the
/// item's report carries. Run with `--ignored --nocapture` after a deliberate
/// change; copy the emitted constants up.
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

    let spec = compiled
        .required_htf_indicators()
        .iter()
        .find(|spec| matches!(spec, IndicatorSpec::Resistance { .. }))
        .expect("the example reads Resistance on the H4 series")
        .clone();
    let mut engine =
        IndicatorEngine::from_specs(std::slice::from_ref(&spec)).expect("engine builds");
    let value = CompiledValue::Indicator {
        series: Series::Htf,
        spec,
        lag: 0,
    };
    let mut levels: Vec<Option<Decimal>> = Vec::with_capacity(htf.candles.len());
    for candle in &htf.candles {
        engine.step(candle);
        levels.push(engine.current(&value));
    }
    let aligned = align(&primary, Some(&htf), None);
    for (i, trade) in result.trades.iter().enumerate() {
        let bar = aligned
            .iter()
            .find(|bar| bar.primary.close_time == trade.entry_signal_time)
            .expect("entry_signal_time resolves to a primary bar");
        let paired = bar
            .htf
            .expect("the signal bar is paired with a closed H4 bar");
        let j = htf
            .candles
            .iter()
            .position(|candle| candle.open_time == paired.open_time)
            .expect("paired candle is in the series");
        println!(
            "trade {i}: entry_signal_time={} close={} previous_resistance={:?} exit_reason={:?} realized_pnl={}",
            trade.entry_signal_time,
            paired.close,
            levels[j - 1],
            trade.exit_reason,
            trade.realized_pnl
        );
    }
}
