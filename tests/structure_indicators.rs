//! r4.s6.w1 — the structure-indicator acceptance suite (demo line d78).
//!
//! Schema **1.3.0** ships twelve structure and pattern indicator variants: the
//! swing pivots `PivotHigh`/`PivotLow`, the levels `Resistance`/`Support`, the
//! four candle-anatomy values `Body`/`UpperWick`/`LowerWick`/`Range` (all four
//! groups pinned here), and the four pattern variants
//! `DoubleTopNeckline`/`DoubleBottomNeckline`/`TrendlineLows`/`TrendlineHighs`
//! (`tests/pattern_indicators.rs`). This suite pins its eight on a hand-built
//! M15 fixture:
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
    SweepableValue, SymbolFilters, Timeframe, ValueSource, align, compile, run_backtest, validate,
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

/// F4 (r4.s6 PR-362 fix round 1, Codex 4238952609): candidate levels are runs
/// of sorted pivot prices whose highest is within `tolerance_pct` of their
/// lowest, and the runs OVERLAP — a price can be the top of one candidate and
/// the anchor of the next. At 10% tolerance, pivots 100/109/118 and a close of
/// 110 give the candidates `[100, 109]` (104.5) and `[109, 118]` (113.5): the
/// resistance is the lowest candidate above the close, 113.5. The old disjoint
/// sweep consumed `[100, 109]` first, dropped 109 as an anchor, and answered
/// `None`.
#[test]
fn overlapping_runs_form_the_resistance_above_the_close() {
    // Pivot highs 100 (bar 1), 109 (bar 4) and 118 (bar 7) — each a strict
    // local high with no other pivot in between — confirmed at the bar after.
    let rows: [(i64, i64, i64, i64); 9] = [
        (94, 95, 93, 94),
        (94, 100, 94, 96), // pivot high 100
        (96, 97, 95, 96),  // confirms it
        (96, 102, 96, 101),
        (101, 109, 101, 104), // pivot high 109
        (104, 105, 102, 103), // confirms it
        (103, 108, 103, 107),
        (107, 118, 107, 116), // pivot high 118
        (116, 112, 108, 110), // confirms it; the close is 110
    ];

    let spec = IndicatorSpec::Resistance {
        left: fixed_u32(1),
        right: fixed_u32(1),
        tolerance_pct: fixed_dec("0.1"),
        min_touches: fixed_u32(2),
        lookback: fixed_u32(20),
    };

    // The Codex case: close 110 → the run [109, 118] is the lowest candidate
    // above it.
    let candles = candles_from(&rows);
    assert_eq!(
        value_at(&spec, &candles, 8),
        Some(Decimal::from_str("113.5").expect("literal")),
        "109 and 118 are within 10% of each other, so their run is a candidate \
         at mean 113.5 — the lowest one above the close 110"
    );

    // The second case must be one the old disjoint sweep FAILS: with the same
    // pivots and the close at 104 the old sweep answers 104.5 (its [100, 109]
    // group), so that close pins nothing about the overlap fix. Moving the
    // middle pivot to 108 makes the old sweep's only group [100, 108] — mean
    // 104, not strictly above the close — so it answers `None`, while the run
    // [108, 118] is the lowest candidate above 104.
    let rows: [(i64, i64, i64, i64); 9] = [
        (94, 95, 93, 94),
        (94, 100, 94, 96), // pivot high 100
        (96, 97, 95, 96),  // confirms it
        (96, 102, 96, 101),
        (101, 108, 101, 104), // pivot high 108
        (104, 105, 102, 103), // confirms it
        (103, 107, 103, 106),
        (107, 118, 107, 116), // pivot high 118
        (116, 112, 108, 104), // confirms it; the close is 104
    ];
    assert_eq!(
        value_at(&spec, &candles_from(&rows), 8),
        Some(Decimal::from_str("113").expect("literal")),
        "the run [108, 118] is the lowest candidate above the close 104 — the \
         old disjoint sweep answers None here (its [100, 108] group means 104, \
         which is not strictly above the close)"
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

    // (3) The backtest takes its signal on the M15 bar where H4 bar 6 has just
    // closed (M15 index 6*16+15 = 111), and fills on the next bar's open. A
    // level slot counts as warm once its `is_ready()` is true (r4.s6 PR-362 fix
    // round 1, rule (a)), so the first bar a signal may fire on is the level's
    // FIRST bar — H4 bar 6's close, not the bar after it. A no-look-ahead
    // violation would surface the level inside H4 bar 6's own span (M15
    // 96..=110) and fire there, before that H4 bar closed; the assertion below
    // pins the pairing instead: the signal bar's paired H4 bar has closed at or
    // before the signal bar's own close.
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
    // The no-look-ahead rule, asserted on the pairing itself: the signal bar's
    // paired H4 bar has closed at or before the signal bar's own close.
    let paired = align(&primary, Some(&htf), None)
        .into_iter()
        .find(|bar| bar.primary.close_time == trade.entry_signal_time)
        .expect("the signal time resolves to a primary bar")
        .htf
        .expect("the signal bar is paired with a closed H4 bar");
    assert!(
        paired.close_time <= trade.entry_signal_time,
        "no look-ahead: the paired H4 bar must have closed by the signal bar's close"
    );
    assert_eq!(
        trade.entry_signal_time, primary.candles[111].close_time,
        "the signal lands on the M15 bar whose paired H4 bar (6) has just closed"
    );
    assert_eq!(
        trade.entry_fill_time, primary.candles[112].open_time,
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

// ---------------------------------------------------------------------------
// (i) The warm gate on a level slot — the break bar itself (fix round 1)
// ---------------------------------------------------------------------------

/// F3 (r4.s6 PR-362 fix round 1, Codex 4238952612 / close review R1): a
/// level/pattern slot counts as warm once its `is_ready()` is true, so the bar
/// that breaks the ONLY qualifying resistance — the bar whose own level reads
/// `None`, nothing sitting above its close any more — still enters through
/// `CrossesAbove(close, lag(resistance, 1))`, which reads the two EARLIER bars'
/// level values. Under the old all-history-`Some` gate the whole entry stayed
/// cold at that bar and this fixture took zero trades; the fixed engine enters
/// exactly once, on the break bar. The support half mirrors it on
/// `CrossesBelow`.
fn run_primary(compiled_strategy: &CompiledStrategy, primary: &CandleSeries) -> BacktestResult {
    run_backtest(
        compiled_strategy,
        primary,
        None,
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

/// The break fixture's strategy: `crosses_above(close, lag(level, 1))` for a
/// long (`crosses_below` for a short), one percent stop, no filters.
fn break_dsl(direction: Direction, spec: IndicatorSpec) -> StrategyDsl {
    let lagged = ValueSource::Lag {
        value: Box::new(ValueSource::Indicator {
            series: Series::Primary,
            spec,
        }),
        bars: 1,
    };
    let close = ValueSource::Price {
        series: Series::Primary,
        field: PriceField::Close,
    };
    let entry = match direction {
        Direction::Long => Condition::CrossesAbove {
            lhs: close,
            rhs: lagged,
        },
        Direction::Short => Condition::CrossesBelow {
            lhs: close,
            rhs: lagged,
        },
    };
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "level break warm gate".to_owned(),
        direction,
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

fn candles_from(rows: &[(i64, i64, i64, i64)]) -> Vec<Candle> {
    rows.iter()
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

#[test]
fn a_clean_break_of_the_only_resistance_enters_on_the_break_bar() {
    // The zone forms at bars 2/5 (pivot highs 110 and 109, mean 109.5), holds
    // while the close stays below it, and bar 8 closes at 110 — the clean
    // break, with the level reading `None` on that bar itself.
    let rows: [(i64, i64, i64, i64); 12] = [
        (100, 101, 99, 100),
        (100, 102, 100, 101),
        (101, 110, 101, 105), // pivot high 110
        (105, 108, 104, 106), // confirms it
        (106, 107, 105, 106),
        (106, 109, 105, 108), // second pivot high 109
        (108, 108, 106, 107), // confirms it → the level exists, 109.5
        (107, 108, 106, 107), // close below it → resistance 109.5
        (107, 111, 107, 110), // close 110 breaks it → None on this bar
        (110, 112, 109, 111), // fills at this bar's open
        (111, 113, 110, 112),
        (112, 114, 111, 113),
    ];
    let candles = candles_from(&rows);

    let spec = IndicatorSpec::Resistance {
        left: fixed_u32(1),
        right: fixed_u32(1),
        tolerance_pct: fixed_dec(TOLERANCE),
        min_touches: fixed_u32(2),
        lookback: fixed_u32(20),
    };
    let result = run_primary(
        &compiled(&break_dsl(Direction::Long, spec.clone())),
        &m15_series(candles.clone()),
    );

    assert_eq!(
        result.trades.len(),
        1,
        "exactly one entry — the break bar's"
    );
    let trade = &result.trades[0];
    assert_eq!(
        trade.entry_signal_time, candles[8].close_time,
        "the signal lands on the bar whose close breaks the only resistance"
    );
    assert_eq!(
        trade.entry_fill_time, candles[9].open_time,
        "the fill is the next bar's open"
    );

    // The mechanism, streamed independently of the engine's decision: the
    // level holds at bars 6 and 7 and is `None` at bar 8 — the cross reads
    // bars 6 and 7, so the break bar's own `None` must not gate the entry.
    let level = Some(Decimal::from_str("109.5").expect("literal"));
    assert_eq!(
        step(&spec, &candles)[6..=8].to_vec(),
        vec![level, level, None],
        "the level holds, then vanishes on the break bar"
    );
}

#[test]
fn a_clean_break_of_the_only_support_enters_on_the_break_bar() {
    // The mirror: pivot lows 90 (bar 2) and 91 (bar 5), support 90.5, and bar 8
    // closes at 90 — below it, so the level reads `None` on the break bar.
    let rows: [(i64, i64, i64, i64); 12] = [
        (100, 101, 99, 100),
        (100, 100, 98, 99),
        (99, 101, 90, 95), // pivot low 90
        (95, 97, 91, 96),  // confirms it
        (96, 98, 92, 97),
        (97, 98, 91, 95), // second pivot low 91
        (95, 96, 91, 94), // confirms it → the level exists, 90.5
        (94, 95, 93, 93), // close above it → support 90.5
        (93, 94, 90, 90), // close 90 breaks it → None on this bar
        (89, 90, 88, 89), // fills at this bar's open
        (89, 90, 88, 89),
        (89, 90, 89, 89),
    ];
    let candles = candles_from(&rows);

    let spec = IndicatorSpec::Support {
        left: fixed_u32(1),
        right: fixed_u32(1),
        tolerance_pct: fixed_dec(TOLERANCE),
        min_touches: fixed_u32(2),
        lookback: fixed_u32(20),
    };
    let result = run_primary(
        &compiled(&break_dsl(Direction::Short, spec.clone())),
        &m15_series(candles.clone()),
    );

    assert_eq!(
        result.trades.len(),
        1,
        "exactly one entry — the break bar's"
    );
    let trade = &result.trades[0];
    assert_eq!(
        trade.entry_signal_time, candles[8].close_time,
        "the signal lands on the bar whose close breaks the only support"
    );
    assert_eq!(
        trade.entry_fill_time, candles[9].open_time,
        "the fill is the next bar's open"
    );

    let level = Some(Decimal::from_str("90.5").expect("literal"));
    assert_eq!(
        step(&spec, &candles)[6..=8].to_vec(),
        vec![level, level, None],
        "the level holds, then vanishes on the break bar"
    );
}

// ---------------------------------------------------------------------------
// (j) The warm gate's latch and ring depth (fix round 2, rules A + B)
// ---------------------------------------------------------------------------

/// Rule A (r4.s6 PR-362 fix round 2, Codex 4239217701 / CodeRabbit 4239215025):
/// a level slot's readiness is LATCHED — once the window has held `min_touches`
/// pivots it stays ready, so a pivot aging out of `lookback` cannot make the
/// engine cold and freeze every unrelated entry branch. This strategy's entry
/// is `Or(rsi(14) < 30, close > lag(resistance, 1))`: the level window empties
/// at bar 7 (lookback 6) and the RSI branch can only fire around bar 15, so on
/// db4f073 the gate is cold exactly when the RSI branch wants to fire — zero
/// trades. With the latch the RSI branch fires once, long after the level
/// window emptied.
#[test]
fn an_or_branch_still_fires_after_the_level_window_empties() {
    let rows: [(i64, i64, i64, i64); 20] = [
        (100, 100, 99, 100),
        (100, 106, 100, 101), // pivot high 106
        (101, 102, 100, 101), // confirms it
        (101, 103, 100, 102),
        (102, 107, 101, 103), // second pivot high 107 → the level, 106.5
        (103, 103, 101, 102), // confirms it
        (102, 102, 100, 101),
        (101, 101, 99, 100), // the 106 pivot ages out of the 6-bar window
        (100, 101, 99, 100),
        (100, 100, 98, 99),
        (99, 99, 97, 98), // the 107 pivot ages out too: the window is empty
        (98, 98, 95, 96),
        (96, 96, 93, 94),
        (94, 94, 90, 91),
        (91, 91, 87, 88),
        (88, 88, 84, 85), // rsi(14) is now < 30 — the other branch wants in
        (85, 86, 83, 84),
        (84, 85, 83, 84),
        (84, 85, 83, 84),
        (84, 85, 83, 84),
    ];
    let candles = candles_from(&rows);
    let spec = IndicatorSpec::Resistance {
        left: fixed_u32(1),
        right: fixed_u32(1),
        tolerance_pct: fixed_dec(TOLERANCE),
        min_touches: fixed_u32(2),
        lookback: fixed_u32(6),
    };
    let entry = Condition::Or {
        conditions: vec![
            Condition::Compare {
                lhs: ValueSource::Indicator {
                    series: Series::Primary,
                    spec: IndicatorSpec::Rsi {
                        period: fixed_u32(14),
                    },
                },
                op: Comparator::Lt,
                rhs: ValueSource::Constant { value: dec(30) },
            },
            Condition::Compare {
                lhs: ValueSource::Price {
                    series: Series::Primary,
                    field: PriceField::Close,
                },
                op: Comparator::Gt,
                rhs: ValueSource::Lag {
                    value: Box::new(ValueSource::Indicator {
                        series: Series::Primary,
                        spec: spec.clone(),
                    }),
                    bars: 1,
                },
            },
        ],
    };
    let result = run_primary(&compiled(&long_dsl(entry)), &m15_series(candles.clone()));

    assert_eq!(
        result.trades.len(),
        1,
        "the RSI branch fires once the latch keeps the gate open"
    );
    let trade = &result.trades[0];
    let signal_bar = candles
        .iter()
        .position(|candle| candle.close_time == trade.entry_signal_time)
        .expect("the signal time resolves to a fixture bar");
    assert!(
        signal_bar >= 10,
        "the entry fires after the level window emptied (bar {signal_bar}); on \
         db4f073 the gate went cold at bar 7 and no trade ever fired"
    );
}

/// Rule B (r4.s6 PR-362 fix round 2, Codex 4239217700): the level/pattern slots
/// keep the `history.len() == max_lag + 2` ring-depth requirement — F3 relaxed
/// only the all-`Some` rule. `Not(Compare(lag(resistance, 20), …))` turns the
/// unavailable lag into a real entry, so with the ring requirement dropped the
/// gate opens at bar 5 and the `Not` fires there, 16 bars before the requested
/// history exists. With the ring kept, the first warm bar is bar 21.
#[test]
fn a_deep_lag_keeps_the_gate_closed_until_the_ring_holds_it() {
    let mut rows: Vec<(i64, i64, i64, i64)> = vec![
        (100, 100, 99, 100),
        (100, 106, 100, 101), // pivot high 106
        (101, 102, 100, 101), // confirms it
        (101, 103, 100, 102),
        (102, 107, 101, 103), // second pivot high 107 → the level, 106.5
        (103, 103, 101, 102), // confirms it; the level holds from here
    ];
    rows.extend(std::iter::repeat_n((102, 103, 101, 102), 20));
    let candles = candles_from(&rows);

    let spec = IndicatorSpec::Resistance {
        left: fixed_u32(1),
        right: fixed_u32(1),
        tolerance_pct: fixed_dec(TOLERANCE),
        min_touches: fixed_u32(2),
        lookback: fixed_u32(60),
    };
    let entry = Condition::Not {
        condition: Box::new(Condition::Compare {
            lhs: ValueSource::Lag {
                value: Box::new(ValueSource::Indicator {
                    series: Series::Primary,
                    spec: spec.clone(),
                }),
                bars: 20,
            },
            op: Comparator::Gt,
            rhs: ValueSource::Constant { value: dec(0) },
        }),
    };
    let result = run_primary(&compiled(&long_dsl(entry)), &m15_series(candles.clone()));

    assert_eq!(
        result.trades.len(),
        1,
        "the Not fires once, and only once the ring holds the lag"
    );
    let trade = &result.trades[0];
    let signal_bar = candles
        .iter()
        .position(|candle| candle.close_time == trade.entry_signal_time)
        .expect("the signal time resolves to a fixture bar");
    assert!(
        signal_bar >= 21,
        "the gate opens only when the slot's ring holds max_lag + 2 = 22 values \
         (bar 21); on db4f073 it opened at bar 5 and the Not fired there \
         (bar {signal_bar})"
    );
}
