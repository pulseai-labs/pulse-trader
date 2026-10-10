//! r4.s6.w1 — the structure-indicator acceptance suite (demo line d78).
//!
//! Schema **1.3.0** adds eight price-level indicator variants: the swing pivots
//! `PivotHigh`/`PivotLow`, the levels `Resistance`/`Support`, and the four
//! candle-anatomy values `Body`/`UpperWick`/`LowerWick`/`Range`. This suite
//! pins their behaviour on a hand-built M15 fixture:
//!
//! - **(a) Pivots.** The confirmed swing highs and lows land exactly on the
//!   hand-labelled bars, and a pivot's value first appears `right` bars after
//!   its own bar — never before.
//! - **(b) Edge rules.** A tie (equal extremes) resolves to the FIRST bar of
//!   the run, and the first `left` bars of a series can never host a pivot.
//! - **(c) Levels.** A resistance touched twice holds at the mean of its
//!   touches while the close stays below it, and goes to `None` once the close
//!   breaks above it; a support mirrors it below the close; the `lookback`
//!   window ages a pivot out and the level disappears with it.
//! - **(d) Anatomy.** `Body`/`UpperWick`/`LowerWick`/`Range` are the current
//!   bar's own arithmetic, defined from the first bar.
//! - **(e) No repaint.** For EVERY variant and EVERY cut point k, the values on
//!   `series[..k]` equal the first k values on the full series — the property
//!   that makes a value at bar t independent of every bar after t.
//! - **(f) Determinism.** Each variant streamed twice over the same series
//!   yields byte-identical `Vec<Option<Decimal>>` (the NFR-2 pattern).
//! - **(g) The `htf` operand.** A strategy whose `Resistance` reads
//!   `series: htf` compiles, routes to the higher-timeframe engine, and takes
//!   its signal on the M15 bar where the paired closed H4 bar first yields the
//!   level — never on a forming H4 bar.
//! - **(h) Migration.** A 1.2.0 document loads through the identity step and
//!   re-serializes as 1.3.0, unchanged otherwise.
//!
//! The fixture and every expected value are pinned literals, labelled by hand
//! against the definitions in SPINE.md's primitive table; nothing in this file
//! recomputes an expectation from the implementation under test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::str::FromStr;

use pulse::{
    BacktestConfig, BacktestResult, Candle, CandleSeries, Comparator, CompiledStrategy,
    CompiledValue, Condition, DataVersion, Direction, EvalContext, ExitRule, IndicatorEngine,
    IndicatorSpec, Migrator, Pair, PriceField, RiskParams, SchemaVersion, Series, StrategyDsl,
    SweepableValue, SymbolFilters, Timeframe, ValueSource, compile, run_backtest, validate,
};
use rust_decimal::Decimal;

// ---------------------------------------------------------------------------
// The fixture — 68 M15 candles (open, high, low, close)
// ---------------------------------------------------------------------------
//
// A hand-built path with the structure the assertions name:
//   * swing high @6 (113) and @16 (114)  → the resistance zone (mean 113.5)
//   * swing low  @27 (90) and @40 (89)   → the support zone (mean 89.5)
//   * swing high @57 (126) and @64 (127) → the late zone (mean 126.5)
//   * a doji with an upper wick @5, a longer lower wick @37, a longer upper
//     wick @45
// Bar 45's high and bar 37's low are the only hand-made wicks; both are
// deliberately too small to create a pivot of their own.
const FIXTURE: [(i64, i64, i64, i64); 68] = [
    (100, 101, 99, 100),
    (100, 103, 99, 102),
    (102, 105, 101, 104),
    (104, 107, 103, 106),
    (106, 109, 105, 108),
    (110, 112, 109, 110),
    (110, 113, 109, 112),
    (112, 113, 109, 110),
    (110, 111, 107, 108),
    (108, 109, 105, 106),
    (106, 107, 103, 104),
    (104, 105, 101, 102),
    (102, 105, 101, 104),
    (104, 107, 103, 106),
    (106, 109, 105, 108),
    (108, 111, 107, 110),
    (110, 114, 109, 113),
    (113, 114, 110, 111),
    (111, 112, 108, 109),
    (109, 110, 106, 107),
    (107, 108, 104, 105),
    (105, 106, 102, 103),
    (103, 104, 100, 101),
    (101, 102, 98, 99),
    (99, 100, 96, 97),
    (97, 98, 94, 95),
    (95, 96, 92, 93),
    (93, 94, 90, 91),
    (91, 94, 90, 93),
    (93, 96, 92, 95),
    (95, 98, 94, 97),
    (97, 100, 96, 99),
    (99, 102, 98, 101),
    (101, 104, 100, 103),
    (103, 104, 100, 101),
    (101, 102, 98, 99),
    (99, 100, 96, 97),
    (97, 98, 92, 95),
    (95, 96, 92, 93),
    (93, 94, 91, 92),
    (92, 93, 89, 90),
    (90, 94, 89, 93),
    (93, 96, 92, 95),
    (95, 98, 94, 97),
    (97, 100, 96, 99),
    (99, 103, 98, 101),
    (101, 104, 100, 103),
    (103, 106, 102, 105),
    (105, 108, 104, 107),
    (107, 110, 106, 109),
    (109, 112, 108, 111),
    (111, 114, 110, 113),
    (113, 116, 112, 115),
    (115, 118, 114, 117),
    (117, 120, 116, 119),
    (119, 122, 118, 121),
    (121, 124, 120, 123),
    (123, 126, 122, 125),
    (125, 126, 122, 123),
    (123, 124, 120, 121),
    (121, 122, 118, 119),
    (119, 121, 118, 120),
    (120, 123, 119, 122),
    (122, 125, 121, 124),
    (124, 127, 123, 126),
    (126, 127, 123, 124),
    (124, 125, 121, 122),
    (122, 123, 119, 120),
];

/// The fixture's bars as candles: one M15 bar per row, absolute index `idx`.
/// Every bar whose half-open span contains an 8h boundary carries the default
/// zero-rate funding stamp, exactly like `htf_atr_engine.rs`'s helper — a zero
/// rate pays zero, so no assertion moves.
fn candle(idx: i64, open: i64, high: i64, low: i64, close: i64) -> Candle {
    let open_time = idx * Timeframe::M15.duration_ms();
    Candle {
        open_time,
        close_time: open_time + Timeframe::M15.duration_ms() - 1,
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

fn dec(value: i64) -> Decimal {
    Decimal::from(value)
}

fn fixture_candles() -> Vec<Candle> {
    FIXTURE
        .iter()
        .enumerate()
        .map(|(idx, (open, high, low, close))| {
            candle(
                i64::try_from(idx).expect("small index"),
                *open,
                *high,
                *low,
                *close,
            )
        })
        .collect()
}

fn m15_series(candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe: Timeframe::M15,
        version: DataVersion::new("v-structure-indicators"),
        candles,
    }
}

// ---------------------------------------------------------------------------
// Spec builders
// ---------------------------------------------------------------------------

fn fixed_u32(value: u32) -> SweepableValue<u32> {
    SweepableValue::Fixed(value)
}

fn fixed_dec(value: &str) -> SweepableValue<Decimal> {
    SweepableValue::Fixed(Decimal::from_str(value).expect("decimal literal"))
}

fn pivot_high(left: u32, right: u32) -> IndicatorSpec {
    IndicatorSpec::PivotHigh {
        left: fixed_u32(left),
        right: fixed_u32(right),
    }
}

fn pivot_low(left: u32, right: u32) -> IndicatorSpec {
    IndicatorSpec::PivotLow {
        left: fixed_u32(left),
        right: fixed_u32(right),
    }
}

fn resistance(tolerance_pct: &str, lookback: u32) -> IndicatorSpec {
    IndicatorSpec::Resistance {
        left: fixed_u32(2),
        right: fixed_u32(2),
        tolerance_pct: fixed_dec(tolerance_pct),
        min_touches: fixed_u32(2),
        lookback: fixed_u32(lookback),
    }
}

fn support(tolerance_pct: &str, lookback: u32) -> IndicatorSpec {
    IndicatorSpec::Support {
        left: fixed_u32(2),
        right: fixed_u32(2),
        tolerance_pct: fixed_dec(tolerance_pct),
        min_touches: fixed_u32(2),
        lookback: fixed_u32(lookback),
    }
}

/// The tolerance the fixture is built around: 5% groups `113`/`114` and
/// `126`/`127` (and `89`/`90`) but keeps the `104` pivot out of both zones.
const TOLERANCE: &str = "0.05";
/// The window that keeps a zone alive across the whole fixture.
const LOOKBACK: u32 = 60;
/// The narrow window that ages the `113` pivot out while the `114` one lives.
const NARROW_LOOKBACK: u32 = 20;

/// Every variant this item adds, for the property cases.
fn all_structure_specs() -> Vec<(&'static str, IndicatorSpec)> {
    vec![
        ("pivot high(2/2)", pivot_high(2, 2)),
        ("pivot low(2/2)", pivot_low(2, 2)),
        (
            "resistance(2/2, 2+, 5%, 60)",
            resistance(TOLERANCE, LOOKBACK),
        ),
        ("support(2/2, 2+, 5%, 60)", support(TOLERANCE, LOOKBACK)),
        ("body", IndicatorSpec::Body),
        ("upper wick", IndicatorSpec::UpperWick),
        ("lower wick", IndicatorSpec::LowerWick),
        ("range", IndicatorSpec::Range),
    ]
}

/// Step ONE indicator over `candles` on the primary series, returning its value
/// per bar (the `rolling_extremes.rs` pattern).
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
// (a) Pivots — the confirmed bars, and the `right`-bar confirmation delay
// ---------------------------------------------------------------------------

#[test]
fn pivot_highs_and_lows_land_on_the_hand_labelled_bars() {
    let candles = fixture_candles();
    let high = pivot_high(2, 2);
    let low = pivot_low(2, 2);

    // Confirmed swing highs: (bar, price, first visible at bar).
    for (bar, price, first) in [
        (6usize, 113, 8usize),
        (16, 114, 18),
        (33, 104, 35),
        (57, 126, 59),
        (64, 127, 66),
    ] {
        assert_eq!(
            value_at(&high, &candles, bar + 1),
            value_at(&high, &candles, first - 1),
            "bar {bar}: the pivot is not visible before its right bars close"
        );
        assert_eq!(
            value_at(&high, &candles, first),
            Some(dec(price)),
            "bar {first}: the pivot @{bar} ({price}) is confirmed exactly right bars later"
        );
    }

    // The value at each bar is the most recent confirmed pivot.
    for (bar, expected) in [
        (0usize, None),
        (7, None),
        (8, Some(113)),
        (17, Some(113)),
        (18, Some(114)),
        (34, Some(114)),
        (35, Some(104)),
        (58, Some(104)),
        (59, Some(126)),
        (65, Some(126)),
        (66, Some(127)),
        (67, Some(127)),
    ] {
        assert_eq!(
            value_at(&high, &candles, bar),
            expected.map(dec),
            "bar {bar}: pivot high is the most recent confirmed swing high"
        );
    }

    // Confirmed swing lows: (bar, price, first visible at bar).
    for (bar, price, first) in [
        (11usize, 101, 13usize),
        (27, 90, 29),
        (40, 89, 42),
        (60, 118, 62),
    ] {
        assert_eq!(
            value_at(&low, &candles, first),
            Some(dec(price)),
            "bar {first}: the pivot low @{bar} ({price}) is confirmed right bars later"
        );
    }
    for (bar, expected) in [
        (12usize, None),
        (13, Some(101)),
        (28, Some(101)),
        (29, Some(90)),
        (41, Some(90)),
        (42, Some(89)),
        (61, Some(89)),
        (62, Some(118)),
        (67, Some(118)),
    ] {
        assert_eq!(
            value_at(&low, &candles, bar),
            expected.map(dec),
            "bar {bar}: pivot low is the most recent confirmed swing low"
        );
    }
}

// ---------------------------------------------------------------------------
// (b) Edge rules — ties, and the first `left` bars
// ---------------------------------------------------------------------------

#[test]
fn a_tie_resolves_to_the_first_bar_and_the_first_left_bars_cannot_pivot() {
    // A plateau of equal highs: 10, 12, 12, 12, 10. Strictly greater on the
    // left means only the FIRST bar of the run can be the pivot.
    let plateau: Vec<Candle> = [10, 12, 12, 12, 10]
        .iter()
        .enumerate()
        .map(|(idx, high)| candle(i64::try_from(idx).expect("small"), 10, *high, 9, 10))
        .collect();
    let plateau_high = pivot_high(1, 1);
    let values = step(&plateau_high, &plateau);
    assert_eq!(
        values,
        vec![None, None, Some(dec(12)), Some(dec(12)), Some(dec(12))],
        "the equal-high run resolves to its FIRST bar, confirmed one bar later"
    );
    // With a wider right side the tie reading does not change: the first bar
    // still wins, because every later equal bar fails the strict-left test —
    // and the confirmation simply lands `right` bars later.
    let wide = pivot_high(1, 2);
    assert_eq!(
        step(&wide, &plateau),
        vec![None, None, None, Some(dec(12)), Some(dec(12))],
        "the tie reading is stable across the right span (confirmation at bar 3)"
    );

    // The first `left` bars cannot host a pivot: the earliest possible pivot bar
    // is index `left`, so the earliest possible value is bar `left + right`.
    let ramp: Vec<Candle> = [10, 12, 13, 12, 10]
        .iter()
        .enumerate()
        .map(|(idx, high)| candle(i64::try_from(idx).expect("small"), 10, *high, 9, 10))
        .collect();
    assert_eq!(
        step(&pivot_high(2, 2), &ramp),
        vec![None, None, None, None, Some(dec(13))],
        "bar 2 is the earliest pivot (two bars before it), confirmed at bar 4"
    );
}

// ---------------------------------------------------------------------------
// (c) Levels — held twice, then broken; the window rule; the support mirror
// ---------------------------------------------------------------------------

#[test]
fn resistance_holds_twice_then_breaks() {
    let candles = fixture_candles();
    let spec = resistance(TOLERANCE, LOOKBACK);

    // Before the second touch confirms, there is no level at all.
    for bar in [0usize, 8, 17] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            None,
            "bar {bar}: no level yet"
        );
    }
    // Two touches (113 @6 and 114 @16) → the level is their mean, and it holds
    // while the close stays below it.
    for bar in [18usize, 27, 40, 50, 51] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            Some(Decimal::from_str("113.5").expect("literal")),
            "bar {bar}: the resistance is the mean of its two touches"
        );
    }
    // Bar 52 closes at 115 — above the level — so nothing sits above the close
    // any more and the resistance is `None`. This is the break.
    assert!(
        FIXTURE[52].3 > 113,
        "the fixture breaks the level at bar 52"
    );
    for bar in [52usize, 57, 59, 65] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            None,
            "bar {bar}: the broken level is below the close, so no resistance is above it"
        );
    }
    // The late zone (126 @57 and 127 @64) becomes the new resistance once its
    // second touch confirms.
    for bar in [66usize, 67] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            Some(Decimal::from_str("126.5").expect("literal")),
            "bar {bar}: the next zone above the close is the new resistance"
        );
    }
}

#[test]
fn the_lookback_window_ages_a_pivot_out_of_its_group() {
    let candles = fixture_candles();
    let narrow = resistance(TOLERANCE, NARROW_LOOKBACK);

    // With a 20-bar window the 113 pivot @6 leaves the window at bar 26
    // (t - 6 >= 20), so the group is down to its single 114 touch and no level
    // qualifies — while the wide-window spec still reads 113.5 at that bar.
    assert_eq!(
        value_at(&narrow, &candles, 25),
        Some(Decimal::from_str("113.5").expect("literal")),
        "bar 25: both touches are still inside the 20-bar window"
    );
    assert_eq!(
        value_at(&narrow, &candles, 26),
        None,
        "bar 26: the 113 touch has aged out, so the group no longer qualifies"
    );
    assert_eq!(
        value_at(&resistance(TOLERANCE, LOOKBACK), &candles, 26),
        Some(Decimal::from_str("113.5").expect("literal")),
        "bar 26: the same bar under the wide window still holds the level"
    );
}

#[test]
fn support_mirrors_the_resistance_below_the_close() {
    let candles = fixture_candles();
    let spec = support(TOLERANCE, LOOKBACK);

    for bar in [0usize, 29, 41] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            None,
            "bar {bar}: no level yet"
        );
    }
    // Two touches (90 @27 and 89 @40) → their mean, below the close.
    for bar in [42usize, 51, 61, 67] {
        assert_eq!(
            value_at(&spec, &candles, bar),
            Some(Decimal::from_str("89.5").expect("literal")),
            "bar {bar}: the support is the mean of its two touches"
        );
    }
    // A support is the HIGHEST qualifying level BELOW the close — the single
    // touches at 101 (@11) and 118 (@60) never qualify.
    assert_eq!(
        value_at(&spec, &candles, 62),
        Some(Decimal::from_str("89.5").expect("literal")),
        "bar 62: a single-touch pivot is not a level"
    );
}

// ---------------------------------------------------------------------------
// (d) Anatomy
// ---------------------------------------------------------------------------

#[test]
fn candle_anatomy_is_the_current_bars_own_arithmetic() {
    let candles = fixture_candles();
    // (bar, body, upper wick, lower wick, range) — hand-read off the fixture.
    for (bar, body, upper, lower, range) in [
        (0usize, 0i64, 1i64, 1i64, 2i64), // open == close
        (5, 0, 2, 1, 3),                  // the doji with an upper wick
        (6, 2, 1, 1, 4),
        (37, 2, 1, 3, 6), // the longer lower wick
        (45, 2, 2, 1, 5), // the longer upper wick
        (67, 2, 1, 1, 4),
    ] {
        assert_eq!(
            value_at(&IndicatorSpec::Body, &candles, bar),
            Some(dec(body)),
            "bar {bar}: body"
        );
        assert_eq!(
            value_at(&IndicatorSpec::UpperWick, &candles, bar),
            Some(dec(upper)),
            "bar {bar}: upper wick"
        );
        assert_eq!(
            value_at(&IndicatorSpec::LowerWick, &candles, bar),
            Some(dec(lower)),
            "bar {bar}: lower wick"
        );
        assert_eq!(
            value_at(&IndicatorSpec::Range, &candles, bar),
            Some(dec(range)),
            "bar {bar}: range"
        );
    }
    // Every anatomy variant is defined on the very first bar (unlike the
    // pivots and the levels).
    for (label, spec) in [
        ("body", IndicatorSpec::Body),
        ("upper wick", IndicatorSpec::UpperWick),
        ("lower wick", IndicatorSpec::LowerWick),
        ("range", IndicatorSpec::Range),
    ] {
        assert!(
            step(&spec, &candles)[0].is_some(),
            "{label}: the first bar already has anatomy"
        );
    }
}

// ---------------------------------------------------------------------------
// (e) No repaint — prefix equality at every cut point
// ---------------------------------------------------------------------------

#[test]
fn no_structure_indicator_repaints_under_truncation() {
    let candles = fixture_candles();
    for (label, spec) in all_structure_specs() {
        let full = step(&spec, &candles);
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
// (f) Determinism — two cold runs, byte-identical
// ---------------------------------------------------------------------------

#[test]
fn every_structure_indicator_streams_byte_identically_twice() {
    let candles = fixture_candles();
    for (label, spec) in all_structure_specs() {
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
// (g) The `htf` operand — compiles, and steps on the closed H4 bar
// ---------------------------------------------------------------------------

/// One H4 candle at absolute index `j`, with the same funding-stamp policy as
/// the M15 helper.
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

/// The H4 fixture: a swing high at bar 2 (120) and bar 5 (121) — the zone whose
/// mean is 120.5 — with H4 closes that stay below it.
const H4_FIXTURE: [(i64, i64, i64, i64); 10] = [
    (108, 110, 107, 109),
    (109, 115, 108, 114),
    (114, 120, 113, 119),
    (119, 119, 116, 117),
    (117, 118, 115, 116),
    (116, 121, 115, 120),
    (120, 120, 117, 118),
    (118, 119, 116, 117),
    (117, 118, 115, 116),
    (116, 117, 114, 115),
];

fn htf_resistance() -> IndicatorSpec {
    IndicatorSpec::Resistance {
        left: fixed_u32(1),
        right: fixed_u32(1),
        tolerance_pct: fixed_dec(TOLERANCE),
        min_touches: fixed_u32(2),
        lookback: fixed_u32(20),
    }
}

fn long_dsl(entry: Condition) -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "structure htf fixture".to_owned(),
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
fn a_resistance_on_the_htf_series_compiles_and_steps_on_the_closed_h4_bar() {
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
    // the primary one. The break compares the PRIMARY close with the H4 level.
    let entry = Condition::Compare {
        lhs: ValueSource::Price {
            series: Series::Primary,
            field: PriceField::Close,
        },
        op: Comparator::Gt,
        rhs: ValueSource::Indicator {
            series: Series::Htf,
            spec: htf_resistance(),
        },
    };
    let strategy = compiled(&long_dsl(entry));
    assert!(
        strategy
            .required_htf_indicators()
            .iter()
            .any(|spec| matches!(spec, IndicatorSpec::Resistance { .. })),
        "the resistance is an HTF requirement"
    );
    assert!(
        strategy.required_indicators().is_empty(),
        "the primary engine needs no indicator for this strategy"
    );
    assert!(
        strategy.needs_htf(),
        "the strategy consumes the higher timeframe"
    );

    // (2) Stepped on the H4 series, the level first exists on H4 bar 6 — the
    // bar that confirms the second swing high — and never before.
    let mut htf_engine =
        IndicatorEngine::for_series(&strategy, Series::Htf).expect("HTF engine builds");
    let value = CompiledValue::Indicator {
        series: Series::Htf,
        spec: htf_resistance(),
        lag: 0,
    };
    let mut htf_values = Vec::new();
    for candle in &htf_candles {
        htf_engine.step(candle);
        htf_values.push(htf_engine.current(&value));
    }
    let level = Some(Decimal::from_str("120.5").expect("literal"));
    assert_eq!(
        htf_values,
        vec![
            None, None, None, None, None, None, level, level, level, level
        ],
        "the H4 level is confirmed exactly when the second swing high's right bar closes"
    );

    // (3) The backtest takes its signal on the M15 bar where H4 bar 7 has just
    // closed (M15 index 7*16+15 = 127), and fills on the next bar's open. Two
    // rules put it there, and the assertion pins both:
    //   * the level first exists at H4 bar 6 (M15 index 111), and
    //   * the engine's warm gate needs a PREVIOUS value as well as a current
    //     one (#36), so the first bar a signal may fire on is the one after the
    //     level's first bar — H4 bar 7's close.
    // A no-look-ahead violation would surface the level inside H4 bar 6's own
    // span (M15 96..=110) and fire there, before that H4 bar closed.
    let primary = m15_series(
        (0..144)
            .map(|i| {
                let close = 100 + i;
                candle(i, close - 1, close + 1, close - 1, close)
            })
            .collect(),
    );
    let htf = CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe: Timeframe::H4,
        version: DataVersion::new("v-structure-indicators-h4"),
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

// ---------------------------------------------------------------------------
// (h) Migration — a 1.2.0 document loads and re-serializes as 1.3.0
// ---------------------------------------------------------------------------

/// A canonical 1.2.0 document (every defaulted field written explicitly, the
/// wire form the engine writes).
const DOC_1_2_0: &str = r#"{
  "schema_version": "1.2.0",
  "name": "structure migration fixture",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": {
      "type": "Indicator",
      "series": "primary",
      "spec": { "indicator": "Rsi", "period": 14 }
    },
    "op": "Lt",
    "rhs": { "type": "Constant", "value": "30" }
  },
  "filters": [],
  "exits": [{ "type": "StopLoss", "distance_pct": "0.05" }],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

#[test]
fn a_1_2_0_document_loads_and_reserializes_as_1_3_0_unchanged_otherwise() {
    let loaded = Migrator::v1()
        .load(DOC_1_2_0)
        .expect("a 1.2.0 document loads");

    assert_eq!(
        loaded.from.to_string(),
        "1.2.0",
        "`from` is the authored version"
    );
    assert!(
        loaded.migrated,
        "the 1.2.0 → 1.3.0 identity step was applied"
    );
    assert_eq!(
        loaded.dsl_original, DOC_1_2_0,
        "dsl_original stays byte-verbatim"
    );
    assert_eq!(
        loaded.dsl.schema_version,
        SchemaVersion::CURRENT,
        "the typed document is CURRENT"
    );
    assert_eq!(
        loaded.dsl.schema_version.to_string(),
        "1.3.0",
        "CURRENT is 1.3.0"
    );

    // Re-serialized, the document differs from the input ONLY in its version.
    let reserialized = serde_json::to_value(&loaded.dsl).expect("re-serializes");
    let mut expected: serde_json::Value =
        serde_json::from_str(DOC_1_2_0).expect("the fixture is valid JSON");
    expected["schema_version"] = serde_json::json!("1.3.0");
    assert_eq!(
        reserialized, expected,
        "the identity step changes nothing but the schema_version string"
    );
}
