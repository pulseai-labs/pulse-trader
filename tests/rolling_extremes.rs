//! The rolling-extremes acceptance suite (r3.s2.w2 AC-4).
//!
//! Five cases, per the spec:
//!
//! - **(i) The convention** — on a hand-built M15 series, `highest(high, 3)`
//!   at bar k is the max high of the 3 closed bars BEFORE k (inclusive
//!   prior-N window `high[k-3..=k-1]`), a bar making a new extreme does not
//!   raise its own value, `lowest` mirrors, and the first value lands at bar
//!   index N (warm-up N+1).
//! - **(ii) The breakout** — `close > highest(high, 20)` backtests on the
//!   committed 1-month fixture; every trade's entry bar has a close above the
//!   prior-20-bars max recomputed in-test from the fixture candles (d47's
//!   check in miniature); at least one trade.
//! - **(iii) Any series** — `h4:highest(high, 5)` read on the primary bars is
//!   the max high of the 5 closed H4 candles before the last closed one,
//!   recomputed in-test; no look-ahead.
//! - **(iv) Determinism** — two cold runs of (ii), each on a fresh store and
//!   a fresh compile, persist identical `result_content_hash`es.
//! - **(v) Source** — `highest(close, 10)` differs from `highest(high, 10)`
//!   on the fixture and compiles to two engine slots.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use pulse::{
    BacktestConfig, Candle, CandleSeries, CandleStore, Comparator, CompiledStrategy, Condition,
    DataVersion, Direction, EvalContext, ExitRule, IndicatorEngine, IndicatorSpec, Pair,
    PriceField, RiskParams, Series, StrategyDsl, SweepableValue, SymbolFilters, Timeframe,
    ValueSource, compile, run_backtest, validate,
};
use rust_decimal::Decimal;

// ---------------------------------------------------------------------------
// Hand-built candle series (the `htf_atr_engine.rs` helper pattern)
// ---------------------------------------------------------------------------

fn dec(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

/// One M15 candle at absolute index `idx` with the given high/low/close
/// (open = close). Every bar whose span contains an 8h boundary carries the
/// default zero-rate funding stamp, exactly like `htf_atr_engine.rs`'s
/// `flat_m15_from` — a zero rate pays zero, so no assertion moves.
fn m15(idx: i64, high: i64, low: i64, close: i64) -> Candle {
    let open_time = idx * Timeframe::M15.duration_ms();
    Candle {
        open_time,
        close_time: open_time + Timeframe::M15.duration_ms() - 1,
        open: dec(close, 0),
        high: dec(high, 0),
        low: dec(low, 0),
        close: dec(close, 0),
        volume: dec(1, 0),
        funding_rate: if open_time % 28_800_000 == 0 {
            Some(Decimal::ZERO)
        } else {
            None
        },
    }
}

/// One H4 candle at absolute index `j` — same boundary-stamp policy.
fn h4(j: i64, _open: i64, high: i64, low: i64, close: i64) -> Candle {
    let open_time = j * Timeframe::H4.duration_ms();
    Candle {
        open_time,
        close_time: open_time + Timeframe::H4.duration_ms() - 1,
        open: dec(close, 0),
        high: dec(high, 0),
        low: dec(low, 0),
        close: dec(close, 0),
        volume: dec(1, 0),
        funding_rate: if open_time % 28_800_000 == 0 {
            Some(Decimal::ZERO)
        } else {
            None
        },
    }
}

fn m15_series(candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe: Timeframe::M15,
        version: DataVersion::new("v-rolling-extremes"),
        candles,
    }
}

fn h4_series(candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe: Timeframe::H4,
        version: DataVersion::new("v-rolling-extremes-h4"),
        candles,
    }
}

// ---------------------------------------------------------------------------
// Strategy builders
// ---------------------------------------------------------------------------

fn fixed(v: u32) -> SweepableValue<u32> {
    SweepableValue::Fixed(v)
}

fn price(series: Series, field: PriceField) -> ValueSource {
    ValueSource::Price { series, field }
}

fn highest(series: Series, period: u32, source: PriceField) -> ValueSource {
    ValueSource::Indicator {
        series,
        spec: IndicatorSpec::Highest {
            period: fixed(period),
            source,
        },
    }
}

fn compare(lhs: ValueSource, op: Comparator, rhs: ValueSource) -> Condition {
    Condition::Compare { lhs, op, rhs }
}

/// A long strategy with the given entry and the canonical stop/TP exits.
fn long_dsl(entry: Condition) -> StrategyDsl {
    StrategyDsl {
        schema_version: pulse::SchemaVersion::CURRENT,
        name: "rolling extremes fixture".to_owned(),
        direction: Direction::Long,
        entry,
        filters: vec![],
        exits: vec![
            ExitRule::StopLoss {
                distance_pct: SweepableValue::Fixed(dec(5, 2)),
            },
            ExitRule::TakeProfit {
                target_r: SweepableValue::Fixed(dec(2, 0)),
            },
        ],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(dec(1, 2)),
            max_leverage: SweepableValue::Fixed(dec(3, 0)),
        },
    }
}

fn compiled(dsl: &StrategyDsl) -> CompiledStrategy {
    compile(&validate(dsl).expect("fixture validates")).expect("fixture compiles")
}

/// Zero-slippage config so fills sit exactly on candle opens.
fn config() -> BacktestConfig {
    BacktestConfig {
        starting_equity: dec(10_000, 0),
        taker_fee_bps: dec(0, 0),
        slippage_bps: dec(0, 0),
    }
}

fn run(
    compiled_strategy: &CompiledStrategy,
    primary: &CandleSeries,
    htf: Option<&CandleSeries>,
) -> pulse::BacktestResult {
    run_backtest(
        compiled_strategy,
        primary,
        htf,
        &config(),
        &SymbolFilters::unconstrained(),
        pulse::SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("backtest runs")
}

/// The committed 1-month BTCUSDT M15 fixture (the `backtest_fixture.rs` load
/// recipe) — a FRESH store every call, so (iv)'s two runs are truly cold.
fn load_primary_fresh() -> CandleSeries {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store");
    let store = CandleStore::with_base_dir(base);
    let pair = Pair::new("BTCUSDT");
    let head = store
        .read_head(&pair, Timeframe::M15)
        .expect("read M15 HEAD")
        .expect("M15 HEAD present in fixture store");
    store
        .read_snapshot(&pair, Timeframe::M15, &head)
        .expect("read M15 snapshot")
}

// ---------------------------------------------------------------------------
// (i) The convention — inclusive prior-N window, self-bar excluded
// ---------------------------------------------------------------------------

/// Hand-built M15 highs with a deliberate shape: bar 4's own high (15) is a
/// new extreme that must NOT raise bar 4's own value; bars rise and fall so
/// the window maximum moves.
const CONVENTION_HIGHS: [i64; 10] = [10, 12, 11, 14, 15, 9, 13, 8, 16, 12];
const CONVENTION_LOWS: [i64; 10] = [8, 9, 7, 10, 11, 5, 9, 4, 12, 8];

#[test]
fn highest_and_lowest_follow_the_prior_n_convention() {
    let period = 3u32;
    let candles: Vec<Candle> = CONVENTION_HIGHS
        .iter()
        .zip(CONVENTION_LOWS)
        .enumerate()
        .map(|(idx, (high, low))| {
            let idx = i64::try_from(idx).expect("small index");
            m15(idx, *high, low, *high - 1)
        })
        .collect();

    // Highest(high, 3): first Some at bar index 3 (warm-up N+1); value at bar
    // k is max(high[k-3..=k-1]) — the 3 closed bars BEFORE k, excluding k.
    let mut engine = IndicatorEngine::from_specs(&[IndicatorSpec::Highest {
        period: fixed(period),
        source: PriceField::High,
    }])
    .expect("engine builds");
    for k in 0..candles.len() {
        engine.step(&candles[k]);
        let got = engine.current(&pulse::CompiledValue::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Highest {
                period: fixed(period),
                source: PriceField::High,
            },
            lag: 0,
        });
        if k < period as usize {
            assert_eq!(got, None, "bar {k}: inside the N+1 warm-up → None");
            continue;
        }
        let expected = CONVENTION_HIGHS[k - period as usize..k]
            .iter()
            .max()
            .expect("window is non-empty");
        assert_eq!(
            got,
            Some(dec(*expected, 0)),
            "bar {k}: highest(high,3) is the max of the prior 3 highs"
        );
    }

    // A bar whose own high is a new extreme does not raise its own value:
    // step through bar 4 only — its high (15) tops everything before it, yet
    // its own value is the max of bars 1..=3 (12, 11, 14 → 14), not 15.
    let mut engine = IndicatorEngine::from_specs(&[IndicatorSpec::Highest {
        period: fixed(period),
        source: PriceField::High,
    }])
    .expect("engine builds");
    for candle in &candles[..=4] {
        engine.step(candle);
    }
    let at_new_extreme = engine.current(&pulse::CompiledValue::Indicator {
        series: Series::Primary,
        spec: IndicatorSpec::Highest {
            period: fixed(period),
            source: PriceField::High,
        },
        lag: 0,
    });
    assert_eq!(
        at_new_extreme,
        Some(dec(14, 0)),
        "bar 4's own new-extreme high must not raise its own value"
    );

    // Lowest(low, 3) mirrors: value at bar k is min(low[k-3..=k-1]).
    let mut engine = IndicatorEngine::from_specs(&[IndicatorSpec::Lowest {
        period: fixed(period),
        source: PriceField::Low,
    }])
    .expect("engine builds");
    for k in 0..candles.len() {
        engine.step(&candles[k]);
        let got = engine.current(&pulse::CompiledValue::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Lowest {
                period: fixed(period),
                source: PriceField::Low,
            },
            lag: 0,
        });
        if k < period as usize {
            assert_eq!(got, None, "bar {k}: inside the N+1 warm-up → None");
            continue;
        }
        let expected = CONVENTION_LOWS[k - period as usize..k]
            .iter()
            .min()
            .expect("window is non-empty");
        assert_eq!(
            got,
            Some(dec(*expected, 0)),
            "bar {k}: lowest(low,3) is the min of the prior 3 lows"
        );
    }
}

// ---------------------------------------------------------------------------
// (ii) The breakout over the committed 1-month fixture
// ---------------------------------------------------------------------------

/// The committed-fixture breakout run: `close > highest(high, 20)`.
fn breakout_run() -> (pulse::BacktestResult, Vec<Candle>) {
    let primary = load_primary_fresh();
    let strategy = compiled(&long_dsl(compare(
        price(Series::Primary, PriceField::Close),
        Comparator::Gt,
        highest(Series::Primary, 20, PriceField::High),
    )));
    let result = run(&strategy, &primary, None);
    (result, primary.candles)
}

#[test]
fn breakout_backtests_on_the_committed_fixture_and_every_entry_beats_the_prior_20_bar_high() {
    let (result, candles) = breakout_run();
    assert!(
        !result.trades.is_empty(),
        "the breakout strategy produces at least one trade on the fixture"
    );

    for (trade_idx, trade) in result.trades.iter().enumerate() {
        // The entry SIGNAL bar is the bar whose close_time is the signal time
        // (the fill happens on the next bar's open).
        let signal_idx = candles
            .iter()
            .position(|c| c.close_time == trade.entry_signal_time)
            .unwrap_or_else(|| panic!("trade {trade_idx}: signal bar found in the fixture"));
        assert!(
            signal_idx >= 21,
            "trade {trade_idx}: no entry before the engine is warm (bar {signal_idx})"
        );
        let signal_bar = &candles[signal_idx];
        let prior_20_max = candles[signal_idx - 20..signal_idx]
            .iter()
            .map(|c| c.high)
            .max()
            .expect("20 prior bars exist");
        assert!(
            signal_bar.close > prior_20_max,
            "trade {trade_idx}: entry bar close {} must exceed the prior-20-bars max high {}",
            signal_bar.close,
            prior_20_max
        );
    }
}

// ---------------------------------------------------------------------------
// (iii) Any series — h4:highest(high, 5) with no look-ahead
// ---------------------------------------------------------------------------

/// H4 highs FALL (101 → 93) while the primary closes RISE, so the recomputed
/// prior-5 window max (101, then 100) is crossed at the first bar where the
/// strategy is warm — and a look-ahead read (the forming H4 bar, whose highs
/// sit lower) would fire ~15 bars earlier. The closes then fall so the 5%
/// stop completes the trade inside the run.
fn no_lookahead_fixture() -> (CandleSeries, CandleSeries) {
    let h4_candles: Vec<Candle> = (0..9)
        .map(|j| {
            let high = 101 - j;
            h4(j, 100, high, 99, high - 2)
        })
        .collect();
    // 144 M15 bars span the 9 H4 bars (h4[8] closes at M15 index 143).
    // Closes rise +1 per H4 span (i / 12) to a peak of 110 at bar 127, then
    // fall so the stop completes the trade inside the run.
    let m15_candles: Vec<Candle> = (0..144)
        .map(|i| {
            let close = if i <= 127 {
                100 + (i / 12)
            } else {
                110 - (i - 127)
            };
            m15(i, close + 1, close - 1, close)
        })
        .collect();
    (m15_series(m15_candles), h4_series(h4_candles))
}

#[test]
fn h4_highest_read_on_primary_bars_is_the_prior_5_closed_h4_bars_max_without_lookahead() {
    let (primary, htf_series) = no_lookahead_fixture();
    let strategy = compiled(&long_dsl(compare(
        price(Series::Primary, PriceField::Close),
        Comparator::Gt,
        highest(Series::Htf, 5, PriceField::High),
    )));
    let result = run(&strategy, &primary, Some(&htf_series));
    assert!(
        !result.trades.is_empty(),
        "the h4 breakout fires at least once"
    );

    // The recomputation: for primary bar t, the paired last-CLOSED H4 index j
    // is the last H4 whose close_time <= the bar's close_time; the slot value
    // is max(high[j-5..=j-1]) (the 5 closed H4 candles BEFORE the last closed
    // one — Q1 excludes the current bar) and None until j >= 5. The strategy
    // is warm only once the slot has a CURRENT and a PREVIOUS value (the
    // #36 warm gate), so the first lawful bar is the first t where j(t) >= 5
    // AND j(t-1) >= 5 AND close beats the recomputed max.
    let first = &result.trades[0];
    let signal_idx = primary
        .candles
        .iter()
        .position(|c| c.close_time == first.entry_signal_time)
        .expect("signal bar found in the primary series");

    // The FIRST trade must be the FIRST lawful bar: walk the primary series
    // and find the earliest bar whose close beats the recomputed prior-5 max.
    let paired_j = |t: usize| -> Option<usize> {
        let count = htf_series
            .candles
            .iter()
            .filter(|h| h.close_time <= primary.candles[t].close_time)
            .count();
        // Highest(5) has a VALUE from the 6th closed H4 candle (the window
        // holds the 5 before the last closed one), but the #36 warm gate
        // needs a CURRENT and a PREVIOUS slot value — two value-bearing H4
        // arrivals — so an entry is lawful only from the 7th closed H4
        // candle (j >= 6).
        if count >= 7 { Some(count - 1) } else { None }
    };
    let prior_5_max = |j: usize| -> Decimal {
        htf_series.candles[j - 5..j]
            .iter()
            .map(|h| h.high)
            .max()
            .expect("window non-empty")
    };
    let mut expected_first: Option<usize> = None;
    for t in 0..primary.candles.len() {
        let Some(j) = paired_j(t) else { continue };
        if primary.candles[t].close > prior_5_max(j) {
            expected_first = Some(t);
            break;
        }
    }
    let expected_first = expected_first.expect("the fixture produces a lawful entry bar");
    assert_eq!(
        signal_idx, expected_first,
        "the first entry lands exactly on the first warm bar whose close beats \
         the recomputed prior-5 closed-H4 max — no earlier (look-ahead) bar, \
         no later"
    );

    // And the value the strategy compared against at that bar: recompute.
    let j = paired_j(signal_idx).expect("signal bar is past the warm-up");
    let prior_5_max_at_signal = prior_5_max(j);
    assert!(
        primary.candles[signal_idx].close > prior_5_max_at_signal,
        "the signal bar's close beats the prior-5 closed-H4 max \
         {prior_5_max_at_signal}"
    );
}

// ---------------------------------------------------------------------------
// (iv) Determinism — two cold runs persist identical content hashes
// ---------------------------------------------------------------------------

#[test]
fn two_cold_runs_persist_identical_result_content_hashes() {
    let (first, _) = breakout_run();
    let (second, _) = breakout_run();
    assert_eq!(
        first.result_content_hash(),
        second.result_content_hash(),
        "two cold runs of the breakout persist identical result_content_hashes"
    );
}

// ---------------------------------------------------------------------------
// (v) Source — highest(close, 10) vs highest(high, 10)
// ---------------------------------------------------------------------------

#[test]
fn highest_close_and_highest_high_differ_and_compile_to_two_slots() {
    let close_spec = IndicatorSpec::Highest {
        period: fixed(10),
        source: PriceField::Close,
    };
    let high_spec = IndicatorSpec::Highest {
        period: fixed(10),
        source: PriceField::High,
    };
    assert_ne!(
        close_spec, high_spec,
        "the two specs are distinct values (from_specs dedups by equality)"
    );

    // Two engine slots.
    let engine = IndicatorEngine::from_specs(&[close_spec.clone(), high_spec.clone()])
        .expect("engine builds");
    assert_eq!(
        engine.indicator_count(),
        2,
        "highest(close,10) and highest(high,10) are two engine slots"
    );

    // And they differ on the fixture: highs sit strictly above closes on
    // most BTCUSDT bars, so the two windows diverge on those bars — assert a
    // real divergence count rather than every bar (a bar that closes at its
    // own high can legitimately make the two window maxima coincide).
    let candles = load_primary_fresh().candles;
    let mut close_engine =
        IndicatorEngine::from_specs(std::slice::from_ref(&close_spec)).expect("engine builds");
    let mut high_engine =
        IndicatorEngine::from_specs(std::slice::from_ref(&high_spec)).expect("engine builds");
    let mut compared = 0usize;
    let mut diverged = 0usize;
    for candle in &candles {
        close_engine.step(candle);
        high_engine.step(candle);
        let close_value = close_engine.current(&pulse::CompiledValue::Indicator {
            series: Series::Primary,
            spec: close_spec.clone(),
            lag: 0,
        });
        let high_value = high_engine.current(&pulse::CompiledValue::Indicator {
            series: Series::Primary,
            spec: high_spec.clone(),
            lag: 0,
        });
        if let (Some(c), Some(h)) = (close_value, high_value) {
            compared += 1;
            if c != h {
                diverged += 1;
            }
        }
    }
    assert!(
        compared > 1_000,
        "both slots are warm across the fixture ({compared} compared bars)"
    );
    assert!(
        diverged > 1_000,
        "highest(close,10) and highest(high,10) read different values on the \
         fixture ({diverged}/{compared} bars diverge)"
    );
}
