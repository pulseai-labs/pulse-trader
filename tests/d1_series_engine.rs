//! r3.s2.w4 — the fixed `d1` series engine semantics (ledger line d51).
//!
//! Schema 1.2.0 gained a third series: `Series::D1`, the run's **daily**
//! series — loaded independently of the optional H4 HTF, evaluated on the
//! **last closed** UTC-midnight bar, and recorded in run provenance as
//! `inputs.d1`. Every case is hand-built M15 + H4 + D1 candle sets driven
//! through the real [`run_backtest`] loop, plus application-ring refusals, a
//! determinism oracle, and one frozen-base warm-point pin.
//!
//! What each numbered case proves:
//!
//! - **(i)** a `d1` operand reads the **last closed** D1 candle — the candle
//!   whose `close_time` is ≤ the primary bar's `close_time`. The five
//!   threshold runs partition the fixture's days and pin every
//!   day-boundary pairing exactly (95/191/287/383/479); a run over a
//!   threshold only the still-forming day-5 bar could satisfy proves the
//!   forming bar is never visible; `lag(d1:close, 1)` is D1-bar-relative;
//!   and an EMA(3) equality that only holds while day 2 is the paired bar
//!   proves the D1 engine steps every closed D1 candle exactly once (with
//!   days 0–1 fed as lead-in, never re-stepped).
//! - **(ii)** beside H4, in the shape the spec names: `h4:ema(3) rising (1
//!   bar)` AND `d1:close > d1:ema(3)` — the rising half is w3's `Rising`
//!   (which compiles to a lag leaf on the H4 engine) running beside a live
//!   D1 engine. The H4-only twin signals at the first fully-warm bar
//!   (M15[63]), the gated run at M15[383] (the first pairing where the D1
//!   half holds; both halves are recomputed in the test at that bar). Each
//!   engine routes its own series; the gate blocks, not corrupts, the H4
//!   side.
//! - **(iii)** refusals through `run_version_backtest` on a scratch store:
//!   a `d1` strategy with no D1 snapshot refuses `D1Required` naming
//!   `pulse fetch-data --tf D1`; a request selecting D1 as the HTF refuses
//!   `HtfIsD1` before any I/O (the store has no D1 to load, so a load
//!   attempt would surface as a missing-snapshot error instead); a
//!   no-`d1` strategy on a D1-less store runs and records
//!   `inputs.d1 = None`.
//! - **(iv)** provenance: a run over a D1-bearing store persists
//!   `inputs.d1` naming the exact D1 version; after a newer D1 snapshot
//!   advances HEAD, the pinned version still reloads the original candles.
//!   (The fold-row leg lives in
//!   `tests/walk_forward.rs::every_fold_run_records_the_d1_selection`; the
//!   app-layer `read_back` leg in
//!   `tests/backtest_provenance.rs::read_back_reloads_the_pinned_d1_snapshot_after_head_advances`.)
//! - **(v)** two cold runs of the beside-H4 strategy over fresh stores
//!   persist an identical `result_content_hash`.
//! - **(vi)** a primary/H4-only strategy's first fully-warm bar is
//!   **unchanged from the untouched base (`2ad4dac`)**: `56_700_000` —
//!   captured by a temporary test at the base, before any edit (the same
//!   discipline as w1's frozen capture) — so adding the `d1` series moves
//!   nothing for a strategy that does not use it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use pulse::{
    BacktestAppError, BacktestConfig, BacktestRequest, BacktestRunRepository, BinanceAdapter,
    Candle, CandleSeries, CandleSeriesRepository, CandleStore, Comparator, CompiledStrategy,
    Condition, CreatedBy, DataVersion, Db, Direction, ExitReason, ExitRule, IndicatorSpec,
    MIGRATOR, NewVersion, Pair, PriceField, RiskParams, SchemaVersion, Series, SeriesEnd,
    SqliteBacktestRunRepo, SqliteStrategyRepo, StrategyDsl, StrategyRepository, SweepableValue,
    SymbolFilters, Timeframe, ValueSource, compile, first_fully_warm_bar_ms, run_backtest,
    run_version_backtest, validate,
};
use rust_decimal::Decimal;
use tempfile::TempDir;

fn dec(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

// ---------------------------------------------------------------------------
// Hand-built candle series — 500 flat M15 bars (5 days + a 20-bar stub of the
// 6th), 125 rising H4 bars, 6 D1 bars whose day-5 bar never closes inside the
// primary range.
// ---------------------------------------------------------------------------

/// One flat M15 candle at absolute index `i` (close 100), zero-rate funding
/// stamps on every 8h boundary — the repo's real-shaped-series rule.
fn m15(i: i64) -> Candle {
    let open_time = i * Timeframe::M15.duration_ms();
    Candle {
        open_time,
        close_time: open_time + Timeframe::M15.duration_ms() - 1,
        open: dec(100, 0),
        high: dec(100, 0),
        low: dec(100, 0),
        close: dec(100, 0),
        volume: dec(1, 0),
        funding_rate: if open_time % 28_800_000 == 0 {
            Some(Decimal::ZERO)
        } else {
            None
        },
    }
}

/// One rising H4 candle at absolute index `j` (OHLC = `100 + j`), same stamp
/// rule: the H4 engine's EMA lags the close on every bar from `j = 1`.
fn h4(j: i64) -> Candle {
    let open_time = j * Timeframe::H4.duration_ms();
    Candle {
        open_time,
        close_time: open_time + Timeframe::H4.duration_ms() - 1,
        open: dec(100 + j, 0),
        high: dec(100 + j, 0),
        low: dec(100 + j, 0),
        close: dec(100 + j, 0),
        volume: dec(1, 0),
        funding_rate: if open_time % 28_800_000 == 0 {
            Some(Decimal::ZERO)
        } else {
            None
        },
    }
}

/// One D1 candle for day `j` (flat OHLC at `close`), `open_time = j × 24h`.
/// Every D1 open is an 8h boundary, so every bar carries the zero-rate stamp.
/// Day 5 (`close 91`) closes at `518_399_999` — past the primary's last
/// `close_time` (`449_999_999`) — so it is permanently still-forming here.
fn d1(j: i64, close: i64) -> Candle {
    let open_time = j * Timeframe::D1.duration_ms();
    Candle {
        open_time,
        close_time: open_time + Timeframe::D1.duration_ms() - 1,
        open: dec(close, 0),
        high: dec(close, 0),
        low: dec(close, 0),
        close: dec(close, 0),
        volume: dec(1, 0),
        funding_rate: Some(Decimal::ZERO),
    }
}

/// The six D1 closes: three rising days, one falling day, one recovering day,
/// and the never-closed day 5.
const D1_CLOSES: [i64; 6] = [100, 101, 102, 103, 90, 91];

fn m15_fixture() -> Vec<Candle> {
    (0..500).map(m15).collect()
}

fn h4_fixture() -> Vec<Candle> {
    (0..125).map(h4).collect()
}

fn d1_fixture() -> Vec<Candle> {
    D1_CLOSES
        .iter()
        .enumerate()
        .map(|(j, close)| d1(i64::try_from(j).expect("six days fit an i64"), *close))
        .collect()
}

fn series(timeframe: Timeframe, candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe,
        version: DataVersion::new("v-d1-series"),
        candles,
    }
}

fn config() -> BacktestConfig {
    BacktestConfig::default()
}

fn run(
    compiled: &CompiledStrategy,
    primary: &CandleSeries,
    htf: Option<&CandleSeries>,
    d1_series: Option<&CandleSeries>,
    series_end: SeriesEnd,
) -> pulse::BacktestResult {
    run_backtest(
        compiled,
        primary,
        htf,
        d1_series,
        &config(),
        &SymbolFilters::unconstrained(),
        series_end,
        None,
    )
    .expect("backtest runs")
}

// ---------------------------------------------------------------------------
// DSL builders
// ---------------------------------------------------------------------------

fn htf_price(field: PriceField) -> ValueSource {
    ValueSource::Price {
        series: Series::Htf,
        field,
    }
}

fn d1_price(field: PriceField) -> ValueSource {
    ValueSource::Price {
        series: Series::D1,
        field,
    }
}

fn htf_ema(period: u32) -> ValueSource {
    ValueSource::Indicator {
        series: Series::Htf,
        spec: IndicatorSpec::Ema {
            period: SweepableValue::Fixed(period),
        },
    }
}

fn d1_ema(period: u32) -> ValueSource {
    ValueSource::Indicator {
        series: Series::D1,
        spec: IndicatorSpec::Ema {
            period: SweepableValue::Fixed(period),
        },
    }
}

fn d1_lagged_close(bars: u32) -> ValueSource {
    ValueSource::Lag {
        value: Box::new(d1_price(PriceField::Close)),
        bars,
    }
}

fn constant(mantissa: i64, scale: u32) -> ValueSource {
    ValueSource::Constant {
        value: dec(mantissa, scale),
    }
}

fn compare(lhs: ValueSource, op: Comparator, rhs: ValueSource) -> Condition {
    Condition::Compare { lhs, op, rhs }
}

fn stop_loss() -> ExitRule {
    ExitRule::StopLoss {
        distance_pct: SweepableValue::Fixed(dec(5, 2)),
    }
}

fn dsl(entry: Condition, exits: Vec<ExitRule>, direction: Direction) -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "d1 series fixture".to_owned(),
        direction,
        entry,
        filters: vec![],
        exits,
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(dec(1, 2)),
            max_leverage: SweepableValue::Fixed(dec(3, 0)),
        },
    }
}

fn compiled(strategy: &StrategyDsl) -> CompiledStrategy {
    compile(&validate(strategy).expect("fixture validates")).expect("fixture compiles")
}

/// Independent seeded-EMA oracle — `ema[0] = close[0]`, then
/// `ema[t] = k·close[t] + (1−k)·ema[t−1]` with `k = 2/(period+1)`, matching the
/// adapter's ta-rs recursion (NOT the engine under test).
fn oracle_ema(closes: &[f64], period: u32) -> Decimal {
    use pulse::f64_to_decimal_rounded;
    let k = 2.0 / (f64::from(period) + 1.0);
    let mut ema = closes[0];
    for &close in &closes[1..] {
        ema = k * close + (1.0 - k) * ema;
    }
    f64_to_decimal_rounded(ema).expect("oracle ema to Decimal")
}

/// The first M15 index whose `close_time` reaches `cutoff` — the first bar a
/// D1 candle closing at `cutoff` may lawfully pair with.
fn first_index_reaching(candles: &[Candle], cutoff: i64) -> usize {
    candles
        .iter()
        .position(|c| c.close_time >= cutoff)
        .expect("the fixture reaches the cutoff")
}

// ---------------------------------------------------------------------------
// (i) a d1 operand reads only the LAST CLOSED D1 candle
// ---------------------------------------------------------------------------

/// The shared three-series fixture. Day boundaries are exact in M15 indices:
/// the day-`d` D1 bar closes at M15 index `96(d+1) − 1`'s `close_time`, so the
/// five pairing boundaries are 95, 191, 287, 383 and 479.
fn fixture() -> (CandleSeries, CandleSeries, CandleSeries) {
    (
        series(Timeframe::M15, m15_fixture()),
        series(Timeframe::H4, h4_fixture()),
        series(Timeframe::D1, d1_fixture()),
    )
}

#[test]
fn d1_close_reads_only_the_closed_daily_bar() {
    let (primary, htf_series, d1_series) = fixture();

    // One run per threshold: `d1:close > K` first fires on the FIRST bar
    // pairing a day whose close clears K — pinning each day-boundary exactly.
    // (close 100, 101, 102, 103, 90, 91; day 5 never pairs.)
    let cases: &[(i64, usize)] = &[
        (99, 95),   // day 0: the first pairing at all
        (100, 191), // day-0 close == threshold is not `>`; day 1 (101) first clears
        (101, 287), // day 2
        (102, 383), // day 3
    ];
    for &(threshold, expected_index) in cases {
        let strategy = compiled(&dsl(
            compare(
                d1_price(PriceField::Close),
                Comparator::Gt,
                constant(threshold, 0),
            ),
            vec![stop_loss()],
            Direction::Long,
        ));
        let result = run(
            &strategy,
            &primary,
            Some(&htf_series),
            Some(&d1_series),
            SeriesEnd::SnapshotEnd,
        );
        assert_eq!(
            result.trades.len(),
            1,
            "threshold {threshold}: exactly one entry (flat prices never stop out)"
        );
        assert_eq!(
            result.trades[0].entry_signal_time, primary.candles[expected_index].close_time,
            "threshold {threshold}: the signal must land on the first bar pairing the \
             closed day candle that clears it — firing earlier reads a forming day bar"
        );
        assert_eq!(
            result.trades[0].entry_fill_time,
            primary.candles[expected_index + 1].open_time,
            "threshold {threshold}: fill at the next bar's open"
        );
    }

    // A threshold neither closed day clears (day-4 close 90, day-5 forming
    // 91) never fires — no pairing, no leak, no trade.
    let never = compiled(&dsl(
        compare(
            d1_price(PriceField::Close),
            Comparator::Gt,
            constant(103, 0),
        ),
        vec![stop_loss()],
        Direction::Long,
    ));
    let result = run(
        &never,
        &primary,
        Some(&htf_series),
        Some(&d1_series),
        SeriesEnd::SnapshotEnd,
    );
    assert!(
        result.trades.is_empty(),
        "no closed day clears 103, and the forming day-5 bar (91) must not either: {:?}",
        result.trades
    );
}

/// A signal exit only the still-forming day-5 bar (close 91) can trigger never
/// fires: the entry parks a position during day 4's pairing window (closed
/// 90), the exit watches for `d1:close > 90`, and the run must end `EndOfData` —
/// a forming-bar leak (pairing day 5 by `open_time` from index 480 on) would
/// signal `Signal` almost immediately.
#[test]
fn forming_daily_bar_is_never_visible() {
    let (primary, htf_series, d1_series) = fixture();
    // Entry: `d1:close < 90.5` — false for every closed day except day 4's
    // 90, so the single entry pins day 4's pairing start (index 479).
    let strategy = compiled(&dsl(
        compare(
            d1_price(PriceField::Close),
            Comparator::Lt,
            constant(9050, 2),
        ),
        vec![
            stop_loss(),
            ExitRule::SignalExit {
                condition: compare(d1_price(PriceField::Close), Comparator::Gt, constant(90, 0)),
            },
        ],
        Direction::Long,
    ));
    let result = run(
        &strategy,
        &primary,
        Some(&htf_series),
        Some(&d1_series),
        SeriesEnd::SnapshotEnd,
    );
    assert_eq!(result.trades.len(), 1);
    assert_eq!(
        result.trades[0].entry_signal_time, primary.candles[479].close_time,
        "day 4 (closed 90) first pairs at M15[479]"
    );
    assert_eq!(
        result.trades[0].exit_reason,
        ExitReason::EndOfData,
        "the `d1:close > 90` exit must never fire: only the forming day-5 bar \
         (91) clears it, and a forming bar must not be visible"
    );
}

/// `lag(d1:close, 1)` reads the PREVIOUS closed day — it first reaches
/// `101 > 100.5` on the first day-2 pairing bar (287), never on day-1 pairing
/// (where the lag reads day 0's 100) and never on day-0 pairing (where the lag
/// has no prior bar at all).
#[test]
fn lag_on_d1_is_daily_bar_relative() {
    let (primary, htf_series, d1_series) = fixture();
    let strategy = compiled(&dsl(
        compare(d1_lagged_close(1), Comparator::Gt, constant(100, 1)),
        vec![stop_loss()],
        Direction::Long,
    ));
    let result = run(
        &strategy,
        &primary,
        Some(&htf_series),
        Some(&d1_series),
        SeriesEnd::SnapshotEnd,
    );
    assert_eq!(result.trades.len(), 1);
    assert_eq!(
        result.trades[0].entry_signal_time, primary.candles[287].close_time,
        "the lagged read must first clear 100.5 on the day-2 pairing bar, where \
         the lag resolves to day 1's closed 101"
    );
}

/// `d1:ema(3)` equals the oracle value of the FOUR closed days (`102.125`)
/// exactly over day 3's pairing window, and the three-day value (`101.25`) —
/// the state a skipped fourth step would leave readable — matches nowhere.
/// Together they prove the D1 engine stepped days 0–3 in order, once each
/// (a skip, a re-step or an out-of-order step perturbs the recursion).
///
/// Why day 3 and not day 2, when the three-day EMA `101.25` already exists
/// then: the ENTRY GATE is not the indicator alone. w3's per-engine ring rule
/// keeps `max_lag + 2` values per slot, so a lag-0 read still has a
/// "yesterday" — the D1 engine is `is_warm` only once TWO values have been
/// produced (day 2's and day 3's), which is the same lead-in the H4 engine
/// needs. The value is therefore *readable* from the day-3 pairing bar (383).
#[test]
fn d1_engine_steps_every_closed_daily_candle_exactly_once() {
    let (primary, htf_series, d1_series) = fixture();
    let expected = oracle_ema(&[100.0, 101.0, 102.0, 103.0], 3);
    assert_eq!(
        expected,
        dec(102_125, 3),
        "hand-check: EMA(3) over four days"
    );
    let strategy = compiled(&dsl(
        compare(
            d1_ema(3),
            Comparator::Eq,
            ValueSource::Constant { value: expected },
        ),
        vec![stop_loss()],
        Direction::Long,
    ));
    let result = run(
        &strategy,
        &primary,
        Some(&htf_series),
        Some(&d1_series),
        SeriesEnd::SnapshotEnd,
    );
    assert_eq!(
        result.trades.len(),
        1,
        "the equality holds exactly over day 3's pairing window"
    );
    assert_eq!(
        result.trades[0].entry_signal_time, primary.candles[383].close_time,
        "the EMA reaches 102.125 when the CLOSED day-3 candle (103) is stepped \
         — earlier means day 3 was fed before its close; never means lead-in \
         days were skipped or day 3 was double-stepped"
    );

    // The three-day state, readable only if day 3 was never stepped: it must
    // match no bar at all (the run's D1 EMA is 102.125 from day 3 onward).
    let stale = compiled(&dsl(
        compare(
            d1_ema(3),
            Comparator::Eq,
            ValueSource::Constant {
                value: oracle_ema(&[100.0, 101.0, 102.0], 3),
            },
        ),
        vec![stop_loss()],
        Direction::Long,
    ));
    let result = run(
        &stale,
        &primary,
        Some(&htf_series),
        Some(&d1_series),
        SeriesEnd::SnapshotEnd,
    );
    assert!(
        result.trades.is_empty(),
        "a D1 EMA still sitting on the three-day value would mean day 3's \
         closed candle never stepped the D1 engine: {:?}",
        result.trades
    );
}

// ---------------------------------------------------------------------------
// (ii) the d1 series works BESIDE the H4 HTF series
// ---------------------------------------------------------------------------

/// The case (ii)/(vi) strategy: `h4:close > h4:ema(3)` — H4-only, no `d1`
/// operand. Its first fully-warm bar is the frozen base constant of case (vi).
///
/// The H4 fixture rises (`close = 100 + j`), so the EMA lags BELOW the close
/// and this comparison holds on every bar from the first closed-H4 pairing:
/// the gate is the warm-up, not the arithmetic.
fn h4_only() -> CompiledStrategy {
    compiled(&dsl(
        compare(htf_price(PriceField::Close), Comparator::Gt, htf_ema(3)),
        vec![stop_loss()],
        Direction::Long,
    ))
}

/// The spec's case (ii) shape, with the `d1` gate `AND`ed in:
/// `h4:ema(3) rising (1 bar)` AND `d1:close > d1:ema(3)`. The rising half is
/// w3's `Rising` — it compiles to exactly `ema(3) > lag(ema(3), 1)`, a lag
/// leaf on the H4 engine — so the gated run exercises the lag-leaf path on
/// one series beside a live D1 engine, which the plain `close > ema` form
/// never read.
fn h4_and_d1_gated() -> CompiledStrategy {
    compiled(&dsl(
        Condition::And {
            conditions: vec![
                Condition::Rising {
                    value: Box::new(htf_ema(3)),
                    bars: 1,
                },
                compare(d1_price(PriceField::Close), Comparator::Gt, d1_ema(3)),
            ],
        },
        vec![stop_loss()],
        Direction::Long,
    ))
}

#[test]
// The case-(ii) recomputation feeds `oracle_ema`'s f64 slice from the
// fixtures' small integer closes (≤ 123) — a magnitude where the cast cannot
// lose a bit, and `f64::from(u16::try_from(…))` would be noise.
#[allow(clippy::cast_precision_loss)]
fn d1_gate_blocks_until_the_daily_condition_holds_beside_h4() {
    let (primary, htf_series, d1_series) = fixture();

    // Twin: H4-only. `h4:close > h4:ema(3)` holds on every bar from the first
    // closed-H4 pairing, so the twin's first entry is the FIRST FULLY-WARM BAR
    // — taken from the engine's own warm oracle rather than hand-counted, and
    // pinned as a base constant by case (vi).
    let warm = first_fully_warm_bar_ms(&h4_only(), &primary, Some(&htf_series), None)
        .expect("the twin warms on the fixture");
    let warm_index = primary
        .candles
        .iter()
        .position(|c| c.open_time == warm)
        .expect("the warm bar is one of the fixture's bars");
    let twin = run(
        &h4_only(),
        &primary,
        Some(&htf_series),
        None,
        SeriesEnd::SnapshotEnd,
    );
    assert_eq!(
        twin.trades.len(),
        1,
        "the twin enters once a closed H4 pairs"
    );
    assert_eq!(
        twin.trades[0].entry_signal_time, primary.candles[warm_index].close_time,
        "the twin signals on the first fully-warm bar — the H4 gate needs the \
         H4 engine's ring, not just its first value"
    );

    // Gated: `d1:close > d1:ema(3)` needs the D1 engine warm — two produced
    // values (days 2 and 3) because the rings keep `max_lag + 2` — so the
    // first lawful signal is the day-3 pairing bar: day 0 closes at
    // primary[95], and the three further day boundaries land on 191, 287 and
    // 383. The rising H4 half holds from the second closed-H4 pairing
    // onward, so the D1 half is what sets the bar. Derived from the candle
    // boundary, never hard-coded.
    let gated = run(
        &h4_and_d1_gated(),
        &primary,
        Some(&htf_series),
        Some(&d1_series),
        SeriesEnd::SnapshotEnd,
    );
    assert_eq!(gated.trades.len(), 1, "the gated run enters once both hold");
    let day0_close_time = d1_fixture()[0].close_time;
    let bars_per_day = usize::try_from(Timeframe::D1.duration_ms() / Timeframe::M15.duration_ms())
        .expect("a day is 96 M15 bars");
    let expected_index = first_index_reaching(&primary.candles, day0_close_time) + 3 * bars_per_day;
    assert_eq!(
        expected_index, 383,
        "the day-3 pairing bar: the first bar the D1 gate can lawfully read"
    );

    // Case (ii)'s "enters only on bars where both hold, recomputed in the
    // test": at the signal bar the paired H4 EMA(3) is rising against its own
    // previous closed value, and the paired D1 close clears its own EMA(3) —
    // both by the oracle, never the engine under test.
    let bars_per_h4 = usize::try_from(Timeframe::H4.duration_ms() / Timeframe::M15.duration_ms())
        .expect("an H4 bar is 16 M15 bars");
    let paired_h4 = expected_index / bars_per_h4;
    let h4_closes: Vec<f64> = (0..=paired_h4).map(|j| (100 + j) as f64).collect();
    assert!(
        oracle_ema(&h4_closes, 3) > oracle_ema(&h4_closes[..h4_closes.len() - 1], 3),
        "the rising half must hold at the gated bar (recomputed at H4 index {paired_h4})"
    );
    let d1_closes: Vec<f64> = D1_CLOSES[..=3].iter().map(|&c| c as f64).collect();
    assert!(
        dec(D1_CLOSES[3], 0) > oracle_ema(&d1_closes, 3),
        "the d1 half must hold at the gated bar (recomputed: day 3's close 103 \
         against its four-day EMA(3))"
    );

    assert_eq!(
        gated.trades[0].entry_signal_time, primary.candles[expected_index].close_time,
        "the d1 gate must block the twin's first entry and first fire where the \
         day-3 condition holds (recomputed index {expected_index})"
    );
    assert_eq!(
        gated.trades[0].entry_fill_time,
        primary.candles[expected_index + 1].open_time
    );
    // The gate only REMOVES entries: the gated H4 half (rising) is true from
    // the second closed-H4 pairing onward, so the move from the twin's warm
    // bar to the day-3 pairing bar is the D1 gate's doing alone.
    assert!(
        gated.trades[0].entry_signal_time > twin.trades[0].entry_signal_time,
        "the d1 gate may only delay, never move earlier"
    );
}

// ---------------------------------------------------------------------------
// (iii) application-ring refusals on a scratch store
// ---------------------------------------------------------------------------

/// The `d1`-operand DSL the versioned runs use (schema 1.2.0 JSON — the
/// `series: "d1"` leaf forces `needs_d1()`).
const D1_OPERAND_DSL: &str = r#"{
  "schema_version": "1.2.0",
  "name": "d1 gate",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Price", "series": "d1", "field": "Close" },
    "op": "Gt",
    "rhs": { "type": "Constant", "value": "0" }
  },
  "filters": [],
  "exits": [ { "type": "StopLoss", "distance_pct": "0.05" } ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// The beside-H4 strategy as persisted JSON — the case (ii) shape the
/// determinism case runs: `h4:ema(3) rising (1 bar)` AND
/// `d1:close > d1:ema(3)`, the same shape the compiled builder above gates.
const H4_AND_D1_DSL: &str = r#"{
  "schema_version": "1.2.0",
  "name": "h4 and d1",
  "direction": "long",
  "entry": {
    "type": "And",
    "conditions": [
      {
        "type": "Rising",
        "value": { "type": "Indicator", "series": "htf",
                   "spec": { "indicator": "Ema", "period": 3 } },
        "bars": 1
      },
      {
        "type": "Compare",
        "lhs": { "type": "Price", "series": "d1", "field": "Close" },
        "op": "Gt",
        "rhs": { "type": "Indicator", "series": "d1",
                 "spec": { "indicator": "Ema", "period": 3 } }
      }
    ]
  },
  "filters": [],
  "exits": [ { "type": "StopLoss", "distance_pct": "0.05" } ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// A primary-only DSL — no `series` field anywhere, so neither `needs_htf()`
/// nor `needs_d1()` holds.
const PRIMARY_ONLY_DSL: &str = r#"{
  "schema_version": "1.2.0",
  "name": "primary only",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Price", "field": "Close" },
    "op": "Gt",
    "rhs": { "type": "Constant", "value": "0" }
  },
  "filters": [],
  "exits": [ { "type": "StopLoss", "distance_pct": "0.05" } ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// Seed `store_dir` with the fixture's M15 and H4 series, plus the D1 series
/// only when `with_d1`. Returns the derived identities.
fn seed_store(
    store_dir: PathBuf,
    with_d1: bool,
) -> (DataVersion, DataVersion, Option<DataVersion>) {
    let store = CandleStore::with_base_dir(store_dir);
    let pair = Pair::new("BTCUSDT");
    let m15_version = store
        .commit(&pair, Timeframe::M15, m15_fixture())
        .expect("commit m15 snapshot")
        .series
        .version;
    let h4_version = store
        .commit(&pair, Timeframe::H4, h4_fixture())
        .expect("commit h4 snapshot")
        .series
        .version;
    let d1_version = with_d1.then(|| {
        store
            .commit(&pair, Timeframe::D1, d1_fixture())
            .expect("commit d1 snapshot")
            .series
            .version
    });
    (m15_version, h4_version, d1_version)
}

/// One strategy version whose DSL is `dsl_json`, through the real repos.
async fn seed_version(db: &Db, name: &str, dsl_json: &str) -> pulse::VersionId {
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategies
        .create_strategy(name, None, &[])
        .await
        .expect("create strategy");
    strategies
        .create_version(NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: dsl_json.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version")
        .id
}

/// The single run persisted for `version_id`, decoded.
async fn the_only_run(db: &Db, version_id: &pulse::VersionId) -> pulse::PersistedRun {
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let listed = repo
        .list_runs_for_version(version_id)
        .await
        .expect("list runs");
    assert_eq!(listed.len(), 1, "exactly one run was persisted: {listed:?}");
    repo.get_run(&listed[0].id)
        .await
        .expect("get run")
        .expect("the listed run is fetchable")
}

/// (iii-a) a `d1` strategy over a store with NO D1 snapshot refuses
/// `D1Required`, naming the fetch command — before the engine ever sees the
/// request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_d1_snapshot_refuses_d1_required() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    let version = seed_version(&db, "d1-gate", D1_OPERAND_DSL).await;

    let store_dir = tmp.path().join("store");
    seed_store(store_dir.clone(), false);
    let store = CandleStore::with_base_dir(store_dir);
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let err = run_version_backtest(
        &SqliteStrategyRepo::new(db.pool().clone()),
        &store,
        &BinanceAdapter::new(),
        &runs,
        &BacktestRequest {
            version_id: version.clone(),
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: None,
            config: BacktestConfig::default(),
            snapshots: None,
            window: None,
        },
    )
    .await
    .expect_err("a d1 strategy without a D1 snapshot must refuse");

    match &err {
        BacktestAppError::D1Required { field, pair } => {
            assert_eq!(*field, "inputs.d1");
            assert_eq!(pair.as_str(), "BTCUSDT");
        }
        other => panic!("expected D1Required, got {other:?}"),
    }
    let rendered = err.to_string();
    // The spec's shorthand AND the runnable command — the pair is positional,
    // so the bare `--tf D1` form alone would not actually run.
    assert!(
        rendered.contains("pulse fetch-data --tf D1"),
        "the refusal must name the fetch command; was: {rendered}"
    );
    assert!(
        rendered.contains("pulse fetch-data BTCUSDT --tf D1"),
        "the refusal must name the runnable command for the pair; was: {rendered}"
    );
    assert!(
        !rendered.contains("  "),
        "the message must not carry a whitespace artefact; was: {rendered}"
    );
}

/// (iii-b) a request selecting D1 as the HTF refuses `HtfIsD1` at the request
/// boundary — the store has no D1 snapshot at all, so any load attempt would
/// have surfaced as a missing-snapshot error instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d1_as_the_htf_refuses_before_any_io() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    let version = seed_version(&db, "primary-only", PRIMARY_ONLY_DSL).await;

    let store_dir = tmp.path().join("store");
    seed_store(store_dir.clone(), false);
    let store = CandleStore::with_base_dir(store_dir);
    let err = run_version_backtest(
        &SqliteStrategyRepo::new(db.pool().clone()),
        &store,
        &BinanceAdapter::new(),
        &SqliteBacktestRunRepo::new(db.pool().clone()),
        &BacktestRequest {
            version_id: version,
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: Some(Timeframe::D1),
            config: BacktestConfig::default(),
            snapshots: None,
            window: None,
        },
    )
    .await
    .expect_err("a D1 HTF selection must refuse");

    match &err {
        BacktestAppError::HtfIsD1 { field } => {
            assert_eq!(*field, "inputs.htf");
            assert!(
                err.to_string().contains("d1"),
                "the refusal must name the d1 series; was: {err}"
            );
        }
        other => panic!("expected HtfIsD1, got {other:?}"),
    }
}

/// (iii-c) + (vi's recording half): a no-`d1` strategy over a D1-less store
/// runs, and records `inputs.d1 = None` — nothing loaded, nothing recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strategy_without_d1_records_no_d1_selection() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    let version = seed_version(&db, "primary-only", PRIMARY_ONLY_DSL).await;

    let store_dir = tmp.path().join("store");
    seed_store(store_dir.clone(), false);
    let store = CandleStore::with_base_dir(store_dir);
    let outcome = run_version_backtest(
        &SqliteStrategyRepo::new(db.pool().clone()),
        &store,
        &BinanceAdapter::new(),
        &SqliteBacktestRunRepo::new(db.pool().clone()),
        &BacktestRequest {
            version_id: version.clone(),
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: None,
            config: BacktestConfig::default(),
            snapshots: None,
            window: None,
        },
    )
    .await
    .expect("a primary-only strategy runs without any D1 snapshot");
    assert!(
        !outcome.trades.is_empty(),
        "the run must produce its trades"
    );

    let persisted = the_only_run(&db, &version).await;
    let inputs = persisted
        .inputs
        .expect("a fresh versioned run always carries inputs");
    assert!(
        inputs.d1.is_none(),
        "nothing D1 was loaded, so nothing D1 may be recorded: {:?}",
        inputs.d1
    );
}

// ---------------------------------------------------------------------------
// (iv) provenance — the D1 selection is recorded and pins its snapshot
// ---------------------------------------------------------------------------

/// A d1 run persists `inputs.d1` naming the exact D1 version; after a newer D1
/// snapshot advances HEAD, the pinned identity still reloads the original
/// candles through the domain port.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d1_run_records_and_pins_its_snapshot() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    let version = seed_version(&db, "d1-gate", D1_OPERAND_DSL).await;

    let store_dir = tmp.path().join("store");
    let (_, _, d1_version) = seed_store(store_dir.clone(), true);
    let d1_version = d1_version.expect("the seeded store carries a D1 snapshot");
    let store = CandleStore::with_base_dir(store_dir);

    run_version_backtest(
        &SqliteStrategyRepo::new(db.pool().clone()),
        &store,
        &BinanceAdapter::new(),
        &SqliteBacktestRunRepo::new(db.pool().clone()),
        &BacktestRequest {
            version_id: version.clone(),
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: None,
            config: BacktestConfig::default(),
            snapshots: None,
            window: None,
        },
    )
    .await
    .expect("the d1 run completes over the seeded store");

    let persisted = the_only_run(&db, &version).await;
    let inputs = persisted
        .inputs
        .expect("a fresh versioned run always carries inputs");
    let recorded = inputs.d1.expect("a d1 run records its D1 selection");
    assert_eq!(recorded.timeframe, Timeframe::D1);
    assert_eq!(
        recorded.data_version, d1_version,
        "the recorded D1 identity is the snapshot the engine actually consumed"
    );

    // HEAD advances: a newer D1 snapshot exists. The recorded identity still
    // reloads the ORIGINAL candles (ADR-0009 immutable snapshots).
    let pair = Pair::new("BTCUSDT");
    store
        .commit(&pair, Timeframe::D1, {
            let mut candles = d1_fixture();
            candles[2].close = dec(999, 0);
            candles
        })
        .expect("commit a newer d1 snapshot");

    let reloaded = store
        .load_version(&pair, Timeframe::D1, &recorded.data_version)
        .expect("the pinned D1 version still loads")
        .series;
    assert_eq!(reloaded.version, recorded.data_version);
    let original = d1_fixture();
    let reloaded_closes: Vec<_> = reloaded.candles.iter().map(|c| c.close).collect();
    let original_closes: Vec<_> = original.iter().map(|c| c.close).collect();
    assert_eq!(
        reloaded_closes, original_closes,
        "the pinned version's bytes are the day the run saw, not HEAD's"
    );
}

// ---------------------------------------------------------------------------
// (v) determinism — two cold runs, one hash
// ---------------------------------------------------------------------------

/// Two cold runs of the beside-H4 strategy over fresh stores persist an
/// identical `result_content_hash` — the repeat-twice oracle, now across a
/// three-series alignment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_cold_runs_persist_identical_content_hashes() {
    // "Cold" is per-run state, not a second runtime: each iteration gets its
    // own database, its own store and its own run id, and the two runs share
    // nothing but this process. (A nested runtime would panic — the engine is
    // already async here; the oracle is the hash, not the threading.)
    let mut hashes = Vec::with_capacity(2);
    for _ in 0..2 {
        let tmp = TempDir::new().expect("tempdir");
        let db = Db::with_path(&tmp.path().join("pulse.db"))
            .await
            .expect("open db");
        MIGRATOR.run(db.pool()).await.expect("run migrations");
        let version = seed_version(&db, "h4-and-d1", H4_AND_D1_DSL).await;
        let store_dir = tmp.path().join("store");
        seed_store(store_dir.clone(), true);
        let store = CandleStore::with_base_dir(store_dir);
        run_version_backtest(
            &SqliteStrategyRepo::new(db.pool().clone()),
            &store,
            &BinanceAdapter::new(),
            &SqliteBacktestRunRepo::new(db.pool().clone()),
            &BacktestRequest {
                version_id: version.clone(),
                pair: Pair::new("BTCUSDT"),
                primary_timeframe: Timeframe::M15,
                htf_timeframe: Some(Timeframe::H4),
                config: BacktestConfig::default(),
                snapshots: None,
                window: None,
            },
        )
        .await
        .expect("the gated run completes");
        let persisted = the_only_run(&db, &version).await;
        hashes.push(persisted.result_content_hash);
    }
    assert_eq!(
        hashes[0], hashes[1],
        "two cold three-series runs must persist the same content hash"
    );
}

// ---------------------------------------------------------------------------
// (vi) no warm-point movement for strategies that do not use d1
// ---------------------------------------------------------------------------

/// The H4-only strategy's first fully-warm bar on this fixture is the FROZEN
/// base constant: `56_700_000`, captured at the untouched base `2ad4dac` by a
/// temporary throwaway test run BEFORE any edit landed (w1's frozen-capture
/// discipline). A value computed after the change would prove nothing; this
/// one proves the `d1` series left the primary/H4 warm point alone.
#[test]
fn h4_only_first_fully_warm_bar_is_unchanged_from_base() {
    let (primary, htf_series, d1_series) = fixture();
    let compiled = h4_only();
    let before = first_fully_warm_bar_ms(&compiled, &primary, Some(&htf_series), None)
        .expect("the h4-only strategy warms on the fixture");
    assert_eq!(
        before, 56_700_000,
        "the base constant moved — the d1 series changed primary/H4 warm-up"
    );
    // And handing the run a D1 series it does not use changes nothing.
    let with_d1 = first_fully_warm_bar_ms(&compiled, &primary, Some(&htf_series), Some(&d1_series))
        .expect("warm with d1 present");
    assert_eq!(
        with_d1, before,
        "an unused d1 series must not move the warm point"
    );
}
