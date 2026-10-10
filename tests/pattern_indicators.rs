//! r4.s6.w3 — the pattern-indicator acceptance suite (demo line d80).
//!
//! Schema **1.3.0** gains four price-level pattern variants on top of w1's
//! confirmed-pivot tracker: the necklines `DoubleTopNeckline` /
//! `DoubleBottomNeckline` and the lines `TrendlineLows` / `TrendlineHighs`.
//! This suite pins their behaviour on small hand-built fixtures:
//!
//! - **(a) Double top.** The last two confirmed pivot highs within
//!   `tolerance_pct` give the LOWEST LOW strictly between them; the value holds
//!   until a newer pivot high is confirmed, and a pair outside tolerance gives
//!   `None`.
//! - **(b) Double bottom.** The mirror: the HIGHEST HIGH strictly between two
//!   within-tolerance pivot lows.
//! - **(c) Trendlines.** A rising pair of pivot lows gives
//!   `B.price + (B.price − A.price) × (t − B.bar) / (B.bar − A.bar)` at bar t,
//!   rounded to 8 decimal places half-even; a pair that does not point the
//!   right way (falling lows, rising highs) gives `None`; the high mirror is
//!   the falling pair of pivot highs.
//! - **(d) No repaint.** For every variant and EVERY cut point k, the values on
//!   `series[..k]` equal the first k values on the full series — the property
//!   that makes a value at bar t independent of every bar after t.
//! - **(e) Determinism.** Each variant streamed twice over the same series
//!   yields byte-identical `Vec<Option<Decimal>>` (the NFR-2 pattern).
//! - **(f) The `htf` operand.** A strategy whose `DoubleTopNeckline` reads
//!   `series: htf` compiles, routes to the higher-timeframe engine, and takes
//!   its signal on the M15 bar where the paired closed H4 bar first carries a
//!   level — never on a forming H4 bar.
//!
//! The fixtures and every expected value are pinned literals, labelled by hand
//! against the definitions in SPINE.md's primitive table and this item's spec
//! ("Exact definitions"); nothing in this file recomputes an expectation from
//! the implementation under test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::str::FromStr;

use pulse::{
    BacktestConfig, BacktestResult, Candle, CandleSeries, Comparator, CompiledStrategy,
    CompiledValue, Condition, DataVersion, Direction, EvalContext, ExitRule, IndicatorEngine,
    IndicatorSpec, Pair, PriceField, RiskParams, Series, StrategyDsl, SweepableValue,
    SymbolFilters, Timeframe, ValueSource, compile, run_backtest, validate,
};
use rust_decimal::Decimal;

// ---------------------------------------------------------------------------
// Fixtures — (high, low) per bar; only the extremes drive the pivots, and the
// pattern variants read no other price, so open = low and close = high.
// ---------------------------------------------------------------------------

/// 11 bars: the pivot highs are `(2, 113)`, `(5, 114)` and `(8, 130)`; the lows
/// strictly between the first pair are `90` (bar 3) and `92` (bar 4), so the
/// first pair's neckline is `90` — and the second pair (114 vs 130) is far
/// outside the 5% tolerance.
const DOUBLE_TOP: [(i64, i64); 11] = [
    (105, 95),
    (100, 93),
    (113, 100),
    (110, 90),
    (112, 92),
    (114, 95),
    (110, 93),
    (120, 100),
    (130, 105),
    (125, 108),
    (120, 106),
];

/// The mirror, 11 bars: pivot lows `(2, 90)`, `(5, 91)` and `(8, 70)`; the
/// highs strictly between the first pair are `110` (bar 3) and `108` (bar 4),
/// so the first pair's neckline is `110`.
const DOUBLE_BOTTOM: [(i64, i64); 11] = [
    (105, 95),
    (108, 100),
    (100, 90),
    (110, 93),
    (108, 92),
    (105, 91),
    (108, 93),
    (100, 80),
    (95, 70),
    (98, 75),
    (99, 78),
];

/// 10 bars: the pivot lows are `(2, 90)` and `(5, 100)` — rising — so the line
/// is `100 + 10 × (t − 5) / 3`.
const TRENDLINE_LOWS: [(i64, i64); 10] = [
    (115, 110),
    (105, 100),
    (95, 90),
    (100, 95),
    (110, 105),
    (105, 100),
    (115, 110),
    (120, 115),
    (125, 120),
    (130, 125),
];

/// The mirror, 10 bars: the pivot highs are `(2, 110)` and `(5, 100)` — falling
/// — so the line is `100 − 10 × (t − 5) / 3`.
const TRENDLINE_HIGHS: [(i64, i64); 10] = [
    (90, 85),
    (100, 95),
    (110, 105),
    (105, 100),
    (95, 90),
    (100, 95),
    (90, 85),
    (85, 80),
    (80, 75),
    (75, 70),
];

/// 9 bars whose pivot lows are `(2, 110)` and `(6, 105)` — FALLING, so
/// `TrendlineLows` has nothing to draw.
const FALLING_LOWS: [(i64, i64); 9] = [
    (125, 120),
    (120, 115),
    (115, 110),
    (120, 115),
    (125, 120),
    (115, 110),
    (110, 105),
    (115, 110),
    (120, 115),
];

/// 9 bars whose pivot highs are `(2, 90)` and `(6, 95)` — RISING, so
/// `TrendlineHighs` has nothing to draw.
const RISING_HIGHS: [(i64, i64); 9] = [
    (80, 75),
    (85, 80),
    (90, 85),
    (85, 80),
    (80, 75),
    (90, 85),
    (95, 90),
    (90, 85),
    (85, 80),
];

/// 30 bars with two full swings: pivot highs `(4, 110)`, `(13, 115)`,
/// `(21, 112)` and pivot lows `(8, 96)`, `(17, 99)`, `(24, 101)`. The last two
/// highs FALL and the last two lows RISE, so all four variants produce a value
/// (and several confirmations) — what the truncation and two-run properties
/// need.
const ZIGZAG: [(i64, i64); 30] = [
    (100, 96),
    (102, 98),
    (105, 100),
    (107, 102),
    (110, 104),
    (108, 102),
    (104, 99),
    (100, 97),
    (97, 96),
    (101, 98),
    (104, 101),
    (108, 104),
    (112, 107),
    (115, 109),
    (112, 106),
    (108, 102),
    (104, 100),
    (102, 99),
    (104, 101),
    (107, 103),
    (110, 105),
    (112, 107),
    (110, 104),
    (107, 103),
    (104, 101),
    (106, 102),
    (108, 104),
    (110, 106),
    (111, 107),
    (112, 108),
];

/// The H4 fixture of the `htf` case — 10 H4 bars (open, high, low, close):
/// pivot highs `(2, 120)` and `(5, 121)` (1 ≤ 120 × 5% apart) with the lows
/// strictly between them `116` (bar 3) and `115` (bar 4), so the neckline is
/// `115`, first carried by H4 bar 6. No later H4 high becomes a pivot.
const H4_FIXTURE: [(i64, i64, i64, i64); 10] = [
    (108, 110, 107, 109),
    (109, 115, 105, 114),
    (114, 120, 113, 119),
    (117, 118, 116, 117),
    (117, 119, 115, 118),
    (118, 121, 115, 120),
    (120, 120, 118, 119),
    (119, 118, 116, 117),
    (117, 119, 115, 118),
    (118, 120, 116, 119),
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn dec(value: i64) -> Decimal {
    Decimal::from(value)
}

fn dec8(value: &str) -> Decimal {
    Decimal::from_str(value).expect("decimal literal")
}

/// One M15 bar at absolute index `idx` from a `(high, low)` row.
fn hl_candle(idx: usize, high: i64, low: i64) -> Candle {
    let idx = i64::try_from(idx).expect("small index");
    let open_time = idx * Timeframe::M15.duration_ms();
    Candle {
        open_time,
        close_time: open_time + Timeframe::M15.duration_ms() - 1,
        open: dec(low),
        high: dec(high),
        low: dec(low),
        close: dec(high),
        volume: dec(1),
        funding_rate: if open_time % 28_800_000 == 0 {
            Some(Decimal::ZERO)
        } else {
            None
        },
    }
}

/// A `(high, low)` fixture as M15 candles, index-ordered.
fn hl_candles(rows: &[(i64, i64)]) -> Vec<Candle> {
    rows.iter()
        .enumerate()
        .map(|(idx, (high, low))| hl_candle(idx, *high, *low))
        .collect()
}

/// One H4 candle at absolute index `j`, same funding-stamp policy as the M15
/// helper.
fn h4_candle(j: i64, open: i64, high: i64, low: i64, close: i64) -> Candle {
    let open_time = j * Timeframe::H4.duration_ms();
    Candle {
        open_time,
        close_time: open_time + Timeframe::H4.duration_ms() - 1,
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

fn m15_series(candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe: Timeframe::M15,
        version: DataVersion::new("v-pattern-indicators"),
        candles,
    }
}

fn fixed_u32(value: u32) -> SweepableValue<u32> {
    SweepableValue::Fixed(value)
}

fn fixed_dec(value: &str) -> SweepableValue<Decimal> {
    SweepableValue::Fixed(dec8(value))
}

fn double_top(left: u32, right: u32, tolerance_pct: &str) -> IndicatorSpec {
    IndicatorSpec::DoubleTopNeckline {
        left: fixed_u32(left),
        right: fixed_u32(right),
        tolerance_pct: fixed_dec(tolerance_pct),
    }
}

fn double_bottom(left: u32, right: u32, tolerance_pct: &str) -> IndicatorSpec {
    IndicatorSpec::DoubleBottomNeckline {
        left: fixed_u32(left),
        right: fixed_u32(right),
        tolerance_pct: fixed_dec(tolerance_pct),
    }
}

fn trendline_lows(left: u32, right: u32) -> IndicatorSpec {
    IndicatorSpec::TrendlineLows {
        left: fixed_u32(left),
        right: fixed_u32(right),
    }
}

fn trendline_highs(left: u32, right: u32) -> IndicatorSpec {
    IndicatorSpec::TrendlineHighs {
        left: fixed_u32(left),
        right: fixed_u32(right),
    }
}

/// Every variant this item adds, for the property cases.
fn all_pattern_specs() -> Vec<(&'static str, IndicatorSpec)> {
    vec![
        ("double top neckline(1/1, 5%)", double_top(1, 1, "0.05")),
        (
            "double bottom neckline(1/1, 5%)",
            double_bottom(1, 1, "0.05"),
        ),
        ("trendline lows(1/1)", trendline_lows(1, 1)),
        ("trendline highs(1/1)", trendline_highs(1, 1)),
    ]
}

/// Step ONE indicator over `candles` on the primary series, returning its value
/// per bar (the `structure_indicators.rs` pattern).
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

/// One value at one bar: the assertion shape the hand-labelled cases use.
fn value_at(spec: &IndicatorSpec, candles: &[Candle], bar: usize) -> Option<Decimal> {
    step(spec, &candles[..=bar])[bar]
}

// ---------------------------------------------------------------------------
// (a) Double top — the lowest low strictly between two within-tolerance tops
// ---------------------------------------------------------------------------

#[test]
fn double_top_neckline_is_the_lowest_low_between_the_tops() {
    let candles = hl_candles(&DOUBLE_TOP);
    let spec = double_top(1, 1, "0.05");

    // No pair before the second pivot high @5 confirms (at bar 5 + right = 6).
    for bar in [0usize, 2, 3, 4, 5] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            None,
            "bar {bar}: the neckline needs two confirmed pivot highs"
        );
    }
    // The 113 @2 / 114 @5 pair is within 5%; the lowest low strictly between
    // them is 90 @3 — and the value HOLDS until a newer pivot high confirms.
    for bar in [6usize, 7, 8] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            Some(dec(90)),
            "bar {bar}: the value is the lowest low between the tops and holds"
        );
    }
    // The 114 / 130 pair is 16 apart, far outside 114 × 5% = 5.7 → None.
    for bar in [9usize, 10] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            None,
            "bar {bar}: a pair outside tolerance has no neckline"
        );
    }
}

#[test]
fn a_tighter_tolerance_gives_the_same_pair_no_neckline() {
    let candles = hl_candles(&DOUBLE_TOP);
    // 114 − 113 = 1, but 113 × 0.1% = 0.113 — the pair is outside this
    // tolerance, so every bar is None.
    let tight = double_top(1, 1, "0.001");
    assert_eq!(
        step(&tight, &candles),
        vec![None; candles.len()],
        "the tolerance leaf decides whether a pair is a neckline"
    );
}

// ---------------------------------------------------------------------------
// (b) Double bottom — the mirror: the highest high between two bottoms
// ---------------------------------------------------------------------------

#[test]
fn double_bottom_neckline_is_the_highest_high_between_the_bottoms() {
    let candles = hl_candles(&DOUBLE_BOTTOM);
    let spec = double_bottom(1, 1, "0.05");

    for bar in [0usize, 2, 3, 5] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            None,
            "bar {bar}: the neckline needs two confirmed pivot lows"
        );
    }
    // The 90 @2 / 91 @5 pair is within 5%; the highest high strictly between
    // them is 110 @3.
    for bar in [6usize, 7, 8] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            Some(dec(110)),
            "bar {bar}: the value is the highest high between the bottoms and holds"
        );
    }
    // 91 vs 70 is far outside 91 × 5% → None.
    for bar in [9usize, 10] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            None,
            "bar {bar}: a pair outside tolerance has no neckline"
        );
    }
}

// ---------------------------------------------------------------------------
// (c) Trendlines — the line through the last two pivots, 8-dp half-even
// ---------------------------------------------------------------------------

#[test]
fn rising_trendline_through_pivot_lows_is_hand_computed() {
    let candles = hl_candles(&TRENDLINE_LOWS);
    let spec = trendline_lows(1, 1);

    // Pivots (2, 90) and (5, 100), confirmed at bars 3 and 6: the line
    // 100 + 10 × (t − 5) / 3 exists from bar 6.
    for bar in [0usize, 2, 3, 5] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            None,
            "bar {bar}: the line needs two confirmed pivot lows"
        );
    }
    // t = 6: 10/3 = 3.333…; t = 7: 20/3 = 6.666…; t = 9: 40/3 = 13.333… —
    // each rounded to 8 decimal places, half-even, then added to 100.
    for (bar, expected) in [
        (6usize, "103.33333333"),
        (7, "106.66666667"),
        (8, "110"),
        (9, "113.33333333"),
    ] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            Some(dec8(expected)),
            "bar {bar}: the line through the pivot lows extended to the bar"
        );
    }
}

#[test]
fn falling_trendline_through_pivot_highs_is_hand_computed() {
    let candles = hl_candles(&TRENDLINE_HIGHS);
    let spec = trendline_highs(1, 1);

    for bar in [0usize, 2, 3, 5] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            None,
            "bar {bar}: the line needs two confirmed pivot highs"
        );
    }
    // Pivots (2, 110) and (5, 100): the line 100 − 10 × (t − 5) / 3.
    for (bar, expected) in [
        (6usize, "96.66666667"),
        (7, "93.33333333"),
        (8, "90"),
        (9, "86.66666667"),
    ] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            Some(dec8(expected)),
            "bar {bar}: the line through the pivot highs extended to the bar"
        );
    }
}

#[test]
fn a_pair_that_does_not_point_the_right_way_gives_no_line() {
    // Falling pivot lows (110 @2, 105 @6) — `TrendlineLows` refuses.
    let falling = hl_candles(&FALLING_LOWS);
    assert_eq!(
        value_at(&trendline_lows(1, 1), &falling, 7),
        None,
        "a falling pair of pivot lows is not a rising support line"
    );
    assert_eq!(
        step(&trendline_lows(1, 1), &falling),
        vec![None; falling.len()],
        "and no bar of the falling series carries a value"
    );

    // Rising pivot highs (90 @2, 95 @6) — `TrendlineHighs` refuses.
    let rising = hl_candles(&RISING_HIGHS);
    assert_eq!(
        value_at(&trendline_highs(1, 1), &rising, 7),
        None,
        "a rising pair of pivot highs is not a falling resistance line"
    );
    assert_eq!(
        step(&trendline_highs(1, 1), &rising),
        vec![None; rising.len()],
        "and no bar of the rising series carries a value"
    );
}

// ---------------------------------------------------------------------------
// (d) No repaint — prefix equality at every cut point
// ---------------------------------------------------------------------------

#[test]
fn no_pattern_indicator_repaints_under_truncation() {
    let candles = hl_candles(&ZIGZAG);
    for (label, spec) in all_pattern_specs() {
        let full = step(&spec, &candles);
        assert!(
            full.iter().any(Option::is_some),
            "{label}: the property is not vacuous — the zig-zag carries a value"
        );
        for cut in 1..=candles.len() {
            let truncated = step(&spec, &candles[..cut]);
            assert_eq!(
                truncated,
                full[..cut],
                "{label}: a value at bar t must not depend on any bar after t \
                 (cut at {cut} differs)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (e) Determinism — two cold runs, byte-identical
// ---------------------------------------------------------------------------

#[test]
fn every_pattern_indicator_streams_byte_identically_twice() {
    let candles = hl_candles(&ZIGZAG);
    for (label, spec) in all_pattern_specs() {
        let first = step(&spec, &candles);
        let second = step(&spec, &candles);
        assert_eq!(
            first, second,
            "{label}: two runs over the same series yield the identical Vec<Option<Decimal>>"
        );
        assert!(
            first.iter().any(Option::is_some),
            "{label}: the run is not vacuous (at least one value exists)"
        );
    }
}

// ---------------------------------------------------------------------------
// (f) The `htf` operand — compiles, and steps on the closed H4 bar
// ---------------------------------------------------------------------------

fn htf_neckline() -> IndicatorSpec {
    double_top(1, 1, "0.05")
}

fn long_dsl(entry: Condition) -> StrategyDsl {
    StrategyDsl {
        schema_version: pulse::SchemaVersion::CURRENT,
        name: "pattern htf fixture".to_owned(),
        direction: Direction::Long,
        entry,
        filters: vec![],
        exits: vec![ExitRule::StopLoss {
            distance_pct: fixed_dec("0.05"),
        }],
        risk: RiskParams {
            risk_per_trade_pct: fixed_dec("0.01"),
            max_leverage: fixed_dec("3"),
        },
    }
}

fn compiled(dsl: &StrategyDsl) -> CompiledStrategy {
    compile(&validate(dsl).expect("fixture validates")).expect("fixture compiles")
}

fn run(
    compiled_strategy: &CompiledStrategy,
    primary: &CandleSeries,
    htf: &CandleSeries,
) -> BacktestResult {
    run_backtest(
        compiled_strategy,
        primary,
        Some(htf),
        None,
        &BacktestConfig {
            starting_equity: dec(10_000),
            taker_fee_bps: dec(0),
            slippage_bps: dec(0),
        },
        &SymbolFilters::unconstrained(),
        pulse::SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("backtest runs")
}

#[test]
fn a_double_top_neckline_on_the_htf_series_compiles_and_steps_on_the_closed_h4_bar() {
    let htf_candles: Vec<Candle> = H4_FIXTURE
        .iter()
        .enumerate()
        .map(|(j, (open, high, low, close))| {
            h4_candle(
                i64::try_from(j).expect("small index"),
                *open,
                *high,
                *low,
                *close,
            )
        })
        .collect();

    // (1) The strategy compiles, and the spec is routed to the HTF engine — not
    // the primary one.
    let entry = Condition::Compare {
        lhs: ValueSource::Price {
            series: Series::Primary,
            field: PriceField::Close,
        },
        op: Comparator::Gt,
        rhs: ValueSource::Indicator {
            series: Series::Htf,
            spec: htf_neckline(),
        },
    };
    let strategy = compiled(&long_dsl(entry));
    assert!(
        strategy
            .required_htf_indicators()
            .iter()
            .any(|spec| matches!(spec, IndicatorSpec::DoubleTopNeckline { .. })),
        "the neckline is an HTF requirement"
    );
    assert!(
        strategy.required_indicators().is_empty(),
        "the primary engine needs no indicator for this strategy"
    );
    assert!(
        strategy.needs_htf(),
        "the strategy consumes the higher timeframe"
    );

    // (2) Stepped on the H4 series, the neckline first exists on H4 bar 6 — the
    // bar that confirms the second swing high (5 + right) — and never before.
    let mut htf_engine =
        IndicatorEngine::for_series(&strategy, Series::Htf).expect("HTF engine builds");
    let value = CompiledValue::Indicator {
        series: Series::Htf,
        spec: htf_neckline(),
        lag: 0,
    };
    let mut htf_values = Vec::new();
    for candle in &htf_candles {
        htf_engine.step(candle);
        htf_values.push(htf_engine.current(&value));
    }
    let neckline = Some(dec(115));
    assert_eq!(
        htf_values,
        vec![
            None, None, None, None, None, None, neckline, neckline, neckline, neckline
        ],
        "the H4 neckline is confirmed exactly when the second swing high's right bar closes"
    );

    // (3) The backtest takes its signal on the M15 bar where H4 bar 7 has just
    // closed (M15 index 7 × 16 + 15 = 127), and fills on the next bar's open:
    // the level first exists at H4 bar 6 (M15 index 111), and the engine's warm
    // gate needs a PREVIOUS value as well as a current one (#36), so the first
    // bar a signal may fire on is the one after the level's first bar — H4 bar
    // 7's close. A no-look-ahead violation would surface the level inside H4
    // bar 6's own span (M15 96..=110) and fire there, before that H4 bar closed.
    let primary = m15_series(
        (0..144)
            .map(|i| {
                let close = 100 + i;
                let open_time = i * Timeframe::M15.duration_ms();
                Candle {
                    open_time,
                    close_time: open_time + Timeframe::M15.duration_ms() - 1,
                    open: dec(close - 1),
                    high: dec(close + 1),
                    low: dec(close - 1),
                    close: dec(close),
                    volume: dec(1),
                    funding_rate: if open_time % 28_800_000 == 0 {
                        Some(Decimal::ZERO)
                    } else {
                        None
                    },
                }
            })
            .collect(),
    );
    let htf = CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe: Timeframe::H4,
        version: DataVersion::new("v-pattern-indicators-h4"),
        candles: htf_candles,
    };
    let result = run(&strategy, &primary, &htf);
    assert_eq!(result.trades.len(), 1, "exactly one entry");
    let trade = &result.trades[0];
    assert_eq!(
        trade.entry_signal_time, primary.candles[127].close_time,
        "the signal lands on the M15 bar whose paired H4 bar (7) has just closed"
    );
    assert_eq!(
        trade.entry_fill_time, primary.candles[128].open_time,
        "the fill is the next M15 bar's open"
    );
}
