//! r6.s2.w2 — the one-sided setup state machine in the engine (ADR-0033).
//!
//! A setup side arms on the 15m bar where a 4H bar closed and its `arm` holds,
//! waits for its `dip`, fires on its `trigger` (with the shared entry gate), and
//! lapses `expiry_bars` bars after the arm bar. Every case is a hand-built 15m +
//! 4H candle pair driven through the real [`run_backtest`] loop or a stepped
//! [`EngineSession`].
//!
//! Common scenario: 200 flat 15m bars at 100 from 2025-01-01T00:00Z (bar *i*
//! opens at *i* × 15 min) and 13 flat 4H bars at 100 from the same instant,
//! except that **4H bar 2** (08:00–12:00) is green: O 100, H 101, L 100, C 101.
//! Its close arrives on **15m bar 47**. A *dip bar* is O 100, H 100, L 98.5,
//! C 98.5; a *trigger bar* is O 100, H 101.5, L 100, C 101.5.
//!
//! What each case proves:
//!
//! - `arms_on_the_4h_close_bar_only` — the arm is read on the bar a 4H bar
//!   closed on, and only there; the trace carries the arm, dip and trigger times.
//! - `the_arm_bar_is_not_a_dip` — a dip on the arm bar does not count.
//! - `expiry_bars_bounds_the_trigger` — a trigger on arm + `expiry_bars` fires;
//!   one bar later the side has lapsed.
//! - `rearm_restarts_the_side` — a new 4H close re-arms and drops the dip.
//! - `trigger_in_a_position_uses_up_the_setup` — a fire in a position enters
//!   nothing and restarts nothing (PDR-0045).
//! - `fold_boundary_keeps_an_armed_setup` — lead-in bars advance the state, so a
//!   fold that starts after the arm enters the same trade as a full run.
//! - `repeat_runs_and_stepping_agree` — determinism, and the fold equals the step.
//! - `short_side_alone` — the short side mirrors the long side.
//! - `trace_feeds_the_hash` — the trace is covered by `result_content_hash`.
//! - `two_sided_is_refused_until_w3` — two sides on one slot wait for w3.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{
    BacktestConfig, BacktestError, BacktestResult, Candle, CandleSeries, Comparator,
    CompiledStrategy, Condition, DataVersion, Direction, EngineSession, ExitReason, ExitRule, Pair,
    PriceField, RiskParams, SchemaVersion, Series, SeriesEnd, SessionTimeframes, SetupEvent,
    SetupPhase, SetupSide, SetupTrace, SetupView, StrategyDsl, SweepableValue, SymbolFilters,
    Timeframe, ValueSource, compile, run_backtest, step_series, validate,
};
use rust_decimal::Decimal;

/// 2025-01-01T00:00:00Z in epoch milliseconds (a multiple of the 8h funding interval).
const START: i64 = 1_735_689_600_000;
const M15_MS: i64 = 900_000;
const H4_MS: i64 = 14_400_000;

fn dec(text: &str) -> Decimal {
    text.parse::<Decimal>().expect("decimal literal")
}

// ---------------------------------------------------------------------------
// Hand-built candle series (the helpers of tests/htf_atr_engine.rs, shifted to
// 2025-01-01; the zero-rate funding stamps on 8h boundaries are kept)
// ---------------------------------------------------------------------------

fn funding_stamp(open_time: i64) -> Option<Decimal> {
    if open_time % 28_800_000 == 0 {
        Some(Decimal::ZERO)
    } else {
        None
    }
}

/// One 4H candle at absolute index `j`.
fn h4(j: i64, open: &str, high: &str, low: &str, close: &str) -> Candle {
    let open_time = START + j * H4_MS;
    Candle {
        open_time,
        close_time: open_time + H4_MS - 1,
        open: dec(open),
        high: dec(high),
        low: dec(low),
        close: dec(close),
        volume: dec("1"),
        funding_rate: funding_stamp(open_time),
    }
}

/// One 15m candle at absolute index `i`.
fn m15(i: i64, open: &str, high: &str, low: &str, close: &str) -> Candle {
    let open_time = START + i * M15_MS;
    Candle {
        open_time,
        close_time: open_time + M15_MS - 1,
        open: dec(open),
        high: dec(high),
        low: dec(low),
        close: dec(close),
        volume: dec("1"),
        funding_rate: funding_stamp(open_time),
    }
}

fn flat_m15(i: i64) -> Candle {
    m15(i, "100", "100", "100", "100")
}

/// A dip bar: O 100, H 100, L 98.5, C 98.5.
fn dip_bar(i: i64) -> Candle {
    m15(i, "100", "100", "98.5", "98.5")
}

/// A trigger bar: O 100, H 101.5, L 100, C 101.5.
fn trigger_bar(i: i64) -> Candle {
    m15(i, "100", "101.5", "100", "101.5")
}

fn series(timeframe: Timeframe, candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe,
        version: DataVersion::new("v-setup-sequence"),
        candles,
    }
}

/// Builds the 15m candle at an absolute index.
type BarMaker = fn(i64) -> Candle;

/// 200 flat 15m bars with `replace` swapped in at their indices.
fn primary_with(replace: &[(i64, BarMaker)]) -> CandleSeries {
    let candles = (0..200)
        .map(|i| {
            replace
                .iter()
                .find(|(at, _)| *at == i)
                .map_or_else(|| flat_m15(i), |(_, make)| make(i))
        })
        .collect();
    series(Timeframe::M15, candles)
}

/// 13 flat 4H bars at 100 with the `green` bars O 100, H 101, L 100, C 101 and
/// the `red` bars O 101, H 101, L 100, C 100.
fn htf_with(green: &[i64], red: &[i64]) -> CandleSeries {
    let candles = (0..13)
        .map(|j| {
            if green.contains(&j) {
                h4(j, "100", "101", "100", "101")
            } else if red.contains(&j) {
                h4(j, "101", "101", "100", "100")
            } else {
                h4(j, "100", "100", "100", "100")
            }
        })
        .collect();
    series(Timeframe::H4, candles)
}

// ---------------------------------------------------------------------------
// Strategy builders
// ---------------------------------------------------------------------------

fn constant(text: &str) -> ValueSource {
    ValueSource::Constant { value: dec(text) }
}

fn price(series: Series, field: PriceField) -> ValueSource {
    ValueSource::Price { series, field }
}

fn compare(lhs: ValueSource, op: Comparator, rhs: ValueSource) -> Condition {
    Condition::Compare { lhs, op, rhs }
}

/// arm: htf close > htf open; dip: close < 99; trigger: close > 101.
fn long_side() -> SetupSide {
    SetupSide {
        arm: compare(
            price(Series::Htf, PriceField::Close),
            Comparator::Gt,
            price(Series::Htf, PriceField::Open),
        ),
        dip: compare(
            price(Series::Primary, PriceField::Close),
            Comparator::Lt,
            constant("99"),
        ),
        trigger: compare(
            price(Series::Primary, PriceField::Close),
            Comparator::Gt,
            constant("101"),
        ),
        stop_anchor: None,
    }
}

/// arm: htf close < htf open; dip: close > 101; trigger: close < 99.
fn short_side() -> SetupSide {
    SetupSide {
        arm: compare(
            price(Series::Htf, PriceField::Close),
            Comparator::Lt,
            price(Series::Htf, PriceField::Open),
        ),
        dip: compare(
            price(Series::Primary, PriceField::Close),
            Comparator::Gt,
            constant("101"),
        ),
        trigger: compare(
            price(Series::Primary, PriceField::Close),
            Comparator::Lt,
            constant("99"),
        ),
        stop_anchor: None,
    }
}

fn strategy(
    direction: Direction,
    take_profit_r: &str,
    expiry_bars: u32,
    long: Option<SetupSide>,
    short: Option<SetupSide>,
) -> CompiledStrategy {
    let dsl = StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "setup sequence".to_owned(),
        direction,
        entry: compare(
            price(Series::Primary, PriceField::Close),
            Comparator::Gt,
            constant("0"),
        ),
        filters: vec![],
        exits: vec![
            ExitRule::StopLoss {
                distance_pct: SweepableValue::Fixed(dec("0.05")),
            },
            ExitRule::TakeProfit {
                target_r: SweepableValue::Fixed(dec(take_profit_r)),
            },
        ],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(dec("0.01")),
            max_leverage: SweepableValue::Fixed(dec("3")),
        },
        setup: Some(Box::new(pulse::Setup {
            expiry_bars: SweepableValue::Fixed(expiry_bars),
            long,
            short,
        })),
    };
    compile(&validate(&dsl).expect("fixture validates")).expect("fixture compiles")
}

/// The common long strategy: stop 5%, take-profit 2R, `expiry_bars` 96.
fn long_strategy() -> CompiledStrategy {
    strategy(Direction::Long, "2", 96, Some(long_side()), None)
}

fn zero_slippage() -> BacktestConfig {
    BacktestConfig {
        starting_equity: dec("10000"),
        taker_fee_bps: dec("0"),
        slippage_bps: dec("0"),
    }
}

fn run(
    compiled: &CompiledStrategy,
    primary: &CandleSeries,
    htf: &CandleSeries,
    count_from_ms: Option<i64>,
) -> BacktestResult {
    run_backtest(
        compiled,
        primary,
        Some(htf),
        None,
        &zero_slippage(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        count_from_ms,
    )
    .expect("backtest runs")
}

fn session(compiled: &CompiledStrategy, count_from_ms: Option<i64>) -> EngineSession {
    EngineSession::new(
        compiled,
        &Pair::new("BTCUSDT"),
        SessionTimeframes {
            primary: Timeframe::M15,
            htf: Some(Timeframe::H4),
            d1: None,
        },
        zero_slippage(),
        SymbolFilters::unconstrained(),
        count_from_ms,
    )
    .expect("session constructs")
}

/// Step a session over the whole scenario and return the `setup_view()` read
/// after every bar, indexed by bar.
fn views(
    compiled: &CompiledStrategy,
    primary: &CandleSeries,
    htf: &CandleSeries,
) -> Vec<SetupView> {
    let mut session = session(compiled, None);
    let mut seen: Vec<SetupView> = Vec::new();
    step_series(&mut session, primary, Some(htf), None, |_, probe| {
        seen.push(probe.setup_view().expect("a setup strategy has a view"));
    })
    .expect("steps");
    seen
}

fn close_of(i: i64) -> i64 {
    START + i * M15_MS + M15_MS - 1
}

fn open_of(i: i64) -> i64 {
    START + i * M15_MS
}

// ---------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------

#[test]
fn arms_on_the_4h_close_bar_only() {
    let primary = primary_with(&[(50, dip_bar), (55, trigger_bar)]);
    let htf = htf_with(&[2], &[]);
    let result = run(&long_strategy(), &primary, &htf, None);

    assert_eq!(result.trades.len(), 1, "one setup, one trade");
    let trade = &result.trades[0];
    assert_eq!(trade.direction, Direction::Long);
    assert_eq!(
        trade.setup,
        Some(SetupTrace {
            arm_time: close_of(47),
            dip_time: close_of(50),
            anchor_time: None,
        })
    );
    assert_eq!(trade.entry_signal_time, close_of(55), "the trigger bar");
    assert_eq!(trade.entry_fill_time, open_of(56), "fills at the next open");
    assert_eq!(trade.exit_reason, ExitReason::EndOfData);

    // The 4H condition holds on bars 48..=54 too (the paired bar is still bar 2)
    // but no 4H bar closed there: the arm time stays at bar 47's close.
    let seen = views(&long_strategy(), &primary, &htf);
    let long = |i: usize| seen[i].long.expect("the long side is declared");
    assert_eq!(long(46).phase, SetupPhase::Idle);
    assert_eq!(long(47).phase, SetupPhase::Armed);
    assert_eq!(long(47).event, Some(SetupEvent::Armed));
    for i in 48..=54 {
        assert_eq!(long(i).arm_time, close_of(47), "bar {i} keeps the arm time");
    }
    assert_eq!(long(50).phase, SetupPhase::Dipped);
    assert_eq!(long(50).event, Some(SetupEvent::Dipped));
    assert_eq!(long(55).event, Some(SetupEvent::Triggered));
    assert_eq!(long(55).phase, SetupPhase::Idle);
    assert!(seen[0].short.is_none(), "the short side is not declared");
}

#[test]
fn the_arm_bar_is_not_a_dip() {
    let primary = primary_with(&[(47, dip_bar), (49, trigger_bar)]);
    let htf = htf_with(&[2], &[]);
    let result = run(&long_strategy(), &primary, &htf, None);
    assert!(
        result.trades.is_empty(),
        "a dip on the arm bar is not a dip: {:?}",
        result.trades
    );
}

#[test]
fn expiry_bars_bounds_the_trigger() {
    let htf = htf_with(&[2], &[]);
    let compiled = strategy(Direction::Long, "2", 8, Some(long_side()), None);

    // Trigger on arm + 8 fires.
    let on_limit = primary_with(&[(50, dip_bar), (55, trigger_bar)]);
    assert_eq!(run(&compiled, &on_limit, &htf, None).trades.len(), 1);

    // Trigger on arm + 9 does not: the side lapsed on that bar.
    let past_limit = primary_with(&[(50, dip_bar), (56, trigger_bar)]);
    assert!(run(&compiled, &past_limit, &htf, None).trades.is_empty());
    let seen = views(&compiled, &past_limit, &htf);
    let long = seen[56].long.expect("declared");
    assert_eq!(long.phase, SetupPhase::Idle);
    assert_eq!(long.event, Some(SetupEvent::Expired));
}

#[test]
fn rearm_restarts_the_side() {
    // 4H bar 3 is green too: its close arrives on bar 63.
    let htf = htf_with(&[2, 3], &[]);

    // A: the dip at 50 is dropped by the re-arm at 63, so the trigger at 66 has no dip.
    let case_a = primary_with(&[(50, dip_bar), (66, trigger_bar)]);
    assert!(run(&long_strategy(), &case_a, &htf, None).trades.is_empty());

    // B: a second dip at 64 restores it.
    let case_b = primary_with(&[(50, dip_bar), (64, dip_bar), (66, trigger_bar)]);
    let result = run(&long_strategy(), &case_b, &htf, None);
    assert_eq!(result.trades.len(), 1);
    assert_eq!(
        result.trades[0].setup,
        Some(SetupTrace {
            arm_time: close_of(63),
            dip_time: close_of(64),
            anchor_time: None,
        })
    );
    assert_eq!(result.trades[0].entry_signal_time, close_of(66));
}

#[test]
fn trigger_in_a_position_uses_up_the_setup() {
    // 4H bar 4 is green too (close on bar 79). TakeProfit 10 keeps the first trade open.
    let htf = htf_with(&[2, 3, 4], &[]);
    let compiled = strategy(Direction::Long, "10", 96, Some(long_side()), None);
    let primary = primary_with(&[
        (50, dip_bar),
        (55, trigger_bar),
        (81, dip_bar),
        (85, trigger_bar),
    ]);
    let result = run(&compiled, &primary, &htf, None);
    assert_eq!(result.trades.len(), 1, "the second fire enters nothing");
    assert_eq!(result.trades[0].exit_reason, ExitReason::EndOfData);
    assert_eq!(result.trades[0].entry_signal_time, close_of(55));

    let seen = views(&compiled, &primary, &htf);
    let long = seen[85].long.expect("declared");
    assert_eq!(long.event, Some(SetupEvent::Triggered));
    assert_eq!(long.phase, SetupPhase::Idle);
}

#[test]
fn fold_boundary_keeps_an_armed_setup() {
    let primary = primary_with(&[(50, dip_bar), (55, trigger_bar)]);
    let htf = htf_with(&[2], &[]);
    let full = run(&long_strategy(), &primary, &htf, None);
    // The fold starts at bar 52: the arm (47) and the dip (50) are lead-in bars.
    let windowed = run(&long_strategy(), &primary, &htf, Some(open_of(52)));
    assert_eq!(full.trades.len(), 1);
    assert_eq!(
        windowed.trades, full.trades,
        "the windowed run enters the same trade as the full run"
    );
}

#[test]
fn repeat_runs_and_stepping_agree() {
    let primary = primary_with(&[(50, dip_bar), (55, trigger_bar)]);
    let htf = htf_with(&[2], &[]);
    let compiled = long_strategy();
    let first = run(&compiled, &primary, &htf, None);
    let second = run(&compiled, &primary, &htf, None);
    assert_eq!(first.result_content_hash(), second.result_content_hash());

    let mut stepped = session(&compiled, None);
    step_series(&mut stepped, &primary, Some(&htf), None, |_, _| {}).expect("steps");
    let finished = stepped
        .finish(SeriesEnd::SnapshotEnd, primary.candles.last())
        .expect("finishes");
    assert_eq!(finished.result_content_hash(), first.result_content_hash());
    assert_eq!(finished.trades, first.trades);
}

#[test]
fn short_side_alone() {
    let htf = htf_with(&[], &[2]);
    let compiled = strategy(Direction::Short, "2", 96, None, Some(short_side()));
    let primary = series(
        Timeframe::M15,
        (0..200)
            .map(|i| match i {
                // dip bar: O 100 H 101.5 L 100 C 101.5
                50 => m15(i, "100", "101.5", "100", "101.5"),
                // trigger bar: O 100 H 100 L 98.5 C 98.5
                55 => m15(i, "100", "100", "98.5", "98.5"),
                _ => flat_m15(i),
            })
            .collect(),
    );
    let result = run(&compiled, &primary, &htf, None);
    assert_eq!(result.trades.len(), 1);
    let trade = &result.trades[0];
    assert_eq!(trade.direction, Direction::Short);
    assert!(
        trade.stop_price.expect("a fresh trade records its stop") > trade.entry_price,
        "a short's stop sits above its entry"
    );
    assert_eq!(
        trade.setup,
        Some(SetupTrace {
            arm_time: close_of(47),
            dip_time: close_of(50),
            anchor_time: None,
        })
    );
    assert_eq!(trade.entry_signal_time, close_of(55));
    assert_eq!(trade.entry_fill_time, open_of(56));
}

#[test]
fn trace_feeds_the_hash() {
    let primary = primary_with(&[(50, dip_bar), (55, trigger_bar)]);
    let htf = htf_with(&[2], &[]);
    let original = run(&long_strategy(), &primary, &htf, None);
    let mut changed = original.clone();
    let trace = changed.trades[0]
        .setup
        .as_mut()
        .expect("the trade carries its trace");
    trace.dip_time += M15_MS;
    assert_ne!(
        original.result_content_hash(),
        changed.result_content_hash(),
        "the dip time is covered by the result hash"
    );
}

#[test]
fn two_sided_is_refused_until_w3() {
    let compiled = strategy(
        Direction::Long,
        "2",
        96,
        Some(long_side()),
        Some(short_side()),
    );
    let outcome = EngineSession::new(
        &compiled,
        &Pair::new("BTCUSDT"),
        SessionTimeframes {
            primary: Timeframe::M15,
            htf: Some(Timeframe::H4),
            d1: None,
        },
        zero_slippage(),
        SymbolFilters::unconstrained(),
        None,
    );
    let Err(BacktestError::UnsupportedExit(text)) = outcome else {
        panic!("a two-sided setup is refused until r6.s2.w3");
    };
    assert!(text.contains("two-sided"), "{text}");
}
