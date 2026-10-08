//! r3.s2.w3 — value expressions: arithmetic/ratios, lag N, rising/falling, with
//! lag-aware warm-up (ledger line `d50`).
//!
//! Schema 1.2.0 stays additive (b1) and gains three constructs (Q2): an
//! `Arith{op, lhs, rhs}` node (add/sub/mul/div, `Decimal` only, any no-value or
//! divide-by-zero operand gives no value), a `Lag{value, bars}` node (own-series
//! bars, 1..=500, no lag-of-lag, no mixed-series lag), and `Rising`/`Falling`
//! conditions (strict, `bars` 1..=500 default 1, compiled to exactly
//! `Compare{value, Gt|Lt, Lag{value, bars}}`). Warm-up follows the deepest lag
//! actually used (b4): `is_warm` additionally requires each slot's ring to hold
//! `max_lag + 2` values, and a strategy with no lag has **exactly** the warm
//! point it had at `85052e8`.
//!
//! What each lettered case proves:
//!
//! - **(i)** `atr(14) / close` equals the ratio computed in the test, as a
//!   `Decimal`, exactly; `add`/`sub`/`mul` likewise; division by a zero-valued
//!   operand and an unwarmed operand give no value — the leaf compares false,
//!   and `Not` over it is true only behind the warm gate, as today.
//! - **(ii)** `lag(close, 3)` reads the close 3 primary bars back; `lag(h4:ema(5), 1)`
//!   reads the EMA of the **previous closed H4 candle** (the HTF engine's ring
//!   advances once per closed H4 bar — never an M15-shifted read).
//! - **(iii)** `h4:ema(20) rising (1 bar)` and its hand-written
//!   `Compare{h4:ema(20), Gt, Lag{h4:ema(20), 1}}` twin produce identical
//!   results and identical `result_content_hash`es (the hash covers results,
//!   not the document); `Falling` likewise with `Lt`.
//! - **(iv)** entering on `ema(10) > lag(ema(10), 5)` has its first fully-warm
//!   bar exactly 5 primary bars after one entering on `ema(10) > 0`; the no-lag
//!   strategy's warm bar is the pre-change warm bar (the `85052e8` definition,
//!   reconstructed with the public no-lag API); walk-forward's defaulted `from`
//!   moves by the same 5 bars.
//! - **(v)** depth 5 is refused with `FieldRange` at the exceeding node's path;
//!   `bars` 0 and 501 are refused with `FieldRange`; lag-of-a-lag, a
//!   mixed-series lag, and Rising over a lag are each refused with
//!   `InvalidExpression`; an MCP submit of the goal's ratio filter succeeds and
//!   the served schema lists `Arith`, `Lag`, `Rising` and `Falling`.
//! - **(vi)** two cold end-to-end runs of a strategy using all three constructs,
//!   each on a fresh store, persist identical `result_content_hash`es.
//! - **product safety:** the deepest legal expression shape — an `Arith` chain
//!   at depth 4 inside nested `And`/`Not` conditions — deserializes and
//!   validates on a 2 MiB thread (the #283 fallback: w2's
//!   `deep_nesting_stack.rs` is not on this item's base).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::thread;

use pulse::{
    ArithOp, BacktestConfig, BacktestRequest, BacktestResult, BinanceAdapter, Candle, CandleSeries,
    CandleStore, Comparator, CompiledStrategy, CompiledValue, Condition, CreatedBy, DataVersion,
    Db, Direction, EvalContext, ExitRule, IndicatorEngine, IndicatorSpec, MIGRATOR, NewVersion,
    Pair, PriceField, RiskParams, SchemaVersion, Series, SeriesEnd, SqliteBacktestRunRepo,
    SqliteStrategyRepo, StrategyDsl, StrategyRepository, SweepableValue, SymbolFilters,
    SystemClock, Timeframe, ValidationCode, ValueSource, VersionId, WalkForwardRequest, compile,
    first_fully_warm_bar_ms, run_backtest, run_version_backtest, run_walk_forward, validate,
};
use rmcp::model::{ReadResourceRequestParams, ResourceContents};
use rust_decimal::Decimal;
use serde_json::json;
use support::mcp::{
    FIXTURE_STORE, call, copy_tree, manifest, migrated_db, seed_real_run, seed_versions,
    spawn_client,
};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Hand-built fixtures
// ---------------------------------------------------------------------------

fn dec(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

/// One M15 candle at absolute index `i` closing at `close` (`open = close`,
/// `high = close + 2`, `low = close − 2` — the OHLC invariant holds). Every bar
/// whose half-open span contains an 8h boundary carries the default zero-rate
/// funding stamp, the same boundary-stamp policy
/// `tests/htf_atr_engine.rs::flat_m15_from` pins.
fn m15(i: i64, close: i64) -> Candle {
    let open_time = i * Timeframe::M15.duration_ms();
    Candle {
        open_time,
        close_time: open_time + Timeframe::M15.duration_ms() - 1,
        open: dec(close, 0),
        high: dec(close + 2, 0),
        low: dec(close - 2, 0),
        close: dec(close, 0),
        volume: dec(1, 0),
        funding_rate: if open_time % 28_800_000 == 0 {
            Some(Decimal::ZERO)
        } else {
            None
        },
    }
}

/// A flat M15 candle: `open = close = 100`, `high = 101`, `low = 99`.
fn m15_flat(i: i64) -> Candle {
    m15(i, 100)
}

/// One H4 candle at absolute index `j` closing at `close` (`open = close`,
/// `high = close + 1`, `low = close − 1`), with the same zero-rate stamping on
/// 8h boundaries (absolute index `j` even).
fn h4(j: i64, close: i64) -> Candle {
    let open_time = j * Timeframe::H4.duration_ms();
    Candle {
        open_time,
        close_time: open_time + Timeframe::H4.duration_ms() - 1,
        open: dec(close, 0),
        high: dec(close + 1, 0),
        low: dec(close - 1, 0),
        close: dec(close, 0),
        volume: dec(1, 0),
        funding_rate: if open_time % 28_800_000 == 0 {
            Some(Decimal::ZERO)
        } else {
            None
        },
    }
}

fn series(timeframe: Timeframe, candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe,
        version: DataVersion::new("v-value-expr"),
        candles,
    }
}

/// The oscillating H4 close pattern: an initial ramp so EMA(20) rises, then a
/// repeating cycle so it rises AND falls — Rising and Falling both fire.
const H4_PATTERN: [i64; 8] = [100, 102, 104, 106, 108, 106, 104, 102];

fn htf_fixture(bars: i64) -> CandleSeries {
    let pattern_len = i64::try_from(H4_PATTERN.len()).expect("the pattern length fits i64");
    series(
        Timeframe::H4,
        (0..bars)
            .map(|j| {
                let index =
                    usize::try_from(j.rem_euclid(pattern_len)).expect("a non-negative index");
                h4(j, H4_PATTERN[index])
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Document builders (the grammar shapes the tests author)
// ---------------------------------------------------------------------------

fn constant(mantissa: i64) -> ValueSource {
    ValueSource::Constant {
        value: dec(mantissa, 0),
    }
}

fn price(series: Series, field: PriceField) -> ValueSource {
    ValueSource::Price { series, field }
}

fn ind(series: Series, period: u32) -> ValueSource {
    ValueSource::Indicator {
        series,
        spec: IndicatorSpec::Ema {
            period: SweepableValue::Fixed(period),
        },
    }
}

fn atr_operand(period: u32) -> ValueSource {
    ValueSource::Indicator {
        series: Series::Primary,
        spec: IndicatorSpec::Atr {
            period: SweepableValue::Fixed(period),
        },
    }
}

fn arith(op: ArithOp, lhs: ValueSource, rhs: ValueSource) -> ValueSource {
    ValueSource::Arith {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

fn lag(value: ValueSource, bars: u32) -> ValueSource {
    ValueSource::Lag {
        value: Box::new(value),
        bars,
    }
}

fn cmp(lhs: ValueSource, op: Comparator, rhs: ValueSource) -> Condition {
    Condition::Compare { lhs, op, rhs }
}

fn rising(value: ValueSource, bars: u32) -> Condition {
    Condition::Rising {
        value: Box::new(value),
        bars,
    }
}

fn falling(value: ValueSource, bars: u32) -> Condition {
    Condition::Falling {
        value: Box::new(value),
        bars,
    }
}

fn stop_loss() -> ExitRule {
    ExitRule::StopLoss {
        distance_pct: SweepableValue::Fixed(dec(5, 2)),
    }
}

fn take_profit() -> ExitRule {
    ExitRule::TakeProfit {
        target_r: SweepableValue::Fixed(dec(2, 0)),
    }
}

fn signal_exit(condition: Condition) -> ExitRule {
    ExitRule::SignalExit { condition }
}

fn base_dsl(entry: Condition, exits: Vec<ExitRule>) -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "value expressions fixture".to_owned(),
        direction: Direction::Long,
        entry,
        filters: vec![],
        exits,
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(dec(1, 2)),
            max_leverage: SweepableValue::Fixed(dec(3, 0)),
        },
    }
}

fn compiled(dsl: &StrategyDsl) -> CompiledStrategy {
    compile(&validate(dsl).expect("fixture validates")).expect("fixture compiles")
}

// Compiled-value builders — the engine-level reads below construct
// `CompiledValue` trees directly (the compiled shape of a lag-free document
// value; lags ride on the leaves exactly as compile pushes them down).
fn cv_close(lag: u32) -> CompiledValue {
    CompiledValue::Price {
        series: Series::Primary,
        field: PriceField::Close,
        lag,
    }
}

fn cv_const(mantissa: i64) -> CompiledValue {
    CompiledValue::Const(dec(mantissa, 0))
}

fn cv_ind(series: Series, period: u32, lag: u32) -> CompiledValue {
    CompiledValue::Indicator {
        series,
        spec: IndicatorSpec::Ema {
            period: SweepableValue::Fixed(period),
        },
        lag,
    }
}

fn cv_atr(period: u32, lag: u32) -> CompiledValue {
    CompiledValue::Indicator {
        series: Series::Primary,
        spec: IndicatorSpec::Atr {
            period: SweepableValue::Fixed(period),
        },
        lag,
    }
}

fn cv_arith(op: ArithOp, lhs: CompiledValue, rhs: CompiledValue) -> CompiledValue {
    CompiledValue::Arith {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

/// Zero-slippage config so fills equal raw candle opens (the
/// `htf_atr_engine::zero_slippage` precedent).
fn zero_slippage() -> BacktestConfig {
    BacktestConfig {
        starting_equity: dec(10_000, 0),
        taker_fee_bps: dec(0, 0),
        slippage_bps: dec(0, 0),
    }
}

fn run(
    compiled: &CompiledStrategy,
    primary: &CandleSeries,
    htf: Option<&CandleSeries>,
) -> BacktestResult {
    run_backtest(
        compiled,
        primary,
        htf,
        None,
        &zero_slippage(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("the run completes")
}

// ---------------------------------------------------------------------------
// (i) arithmetic — exact Decimal math, no-value semantics
// ---------------------------------------------------------------------------

/// (i) `atr(14) / close` equals the ratio computed in the test, as a `Decimal`,
/// exactly — and `add`/`sub`/`mul` chain the same exact way. The operand values
/// are read off the same engine, so the assertion isolates the `Arith`
/// composition: the node performs the same `Decimal` operation the test's
/// literal expression performs, never an `f64` rounding.
#[test]
fn arith_ratio_and_operands_are_exact_decimals() {
    // 16 rising M15 bars: closes 100..115 — ATR(14) is warm by the last bar.
    let primary = series(Timeframe::M15, (0..16).map(|i| m15(i, 100 + i)).collect());
    let strategy = compiled(&base_dsl(
        cmp(atr_operand(14), Comparator::Lt, constant(0)),
        vec![stop_loss()],
    ));
    let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");
    for candle in &primary.candles {
        engine.step(candle);
    }

    let atr_now = engine.current(&cv_atr(14, 0)).expect("ATR(14) is warm");
    let close_now = engine.current(&cv_close(0)).expect("the candle exists");

    // The goal's ratio: atr(14) / close — exact Decimal division.
    let ratio = cv_arith(ArithOp::Div, cv_atr(14, 0), cv_close(0));
    assert_eq!(
        engine.current(&ratio),
        Some(atr_now / close_now),
        "atr(14) / close must equal the Decimal ratio computed in the test"
    );

    // add/sub/mul over a price and constants: ((close − 90) × 3) + 1.
    let composed = cv_arith(
        ArithOp::Add,
        cv_arith(
            ArithOp::Mul,
            cv_arith(ArithOp::Sub, cv_close(0), cv_const(90)),
            cv_const(3),
        ),
        cv_const(1),
    );
    let expected = (close_now - dec(90, 0)) * dec(3, 0) + dec(1, 0);
    assert_eq!(
        engine.current(&composed),
        Some(expected),
        "the composed expression must equal the test's Decimal arithmetic"
    );

    // Pure division exactness: close / 4 lands on a terminating decimal.
    let quarter = cv_arith(ArithOp::Div, cv_close(0), cv_const(4));
    assert_eq!(
        engine.current(&quarter),
        Some(close_now / dec(4, 0)),
        "division must be the exact Decimal operation"
    );
}

/// (i) Division by a zero-valued operand and an unwarmed operand give **no
/// value**: the comparison leaf evaluates false, and `Not` over it composes to
/// true — the today semantics (safe behind the engine's entry warm gate).
#[test]
fn no_value_operands_compare_false_and_not_composes_true() {
    let primary = series(Timeframe::M15, (0..6).map(m15_flat).collect());
    let strategy = compiled(&base_dsl(
        cmp(
            price(Series::Primary, PriceField::Close),
            Comparator::Gt,
            constant(0),
        ),
        vec![stop_loss()],
    ));
    let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");
    for candle in &primary.candles {
        engine.step(candle);
    }

    // Division by an exactly-zero operand: no value.
    let div_by_zero = cv_arith(ArithOp::Div, cv_close(0), cv_const(0));
    assert_eq!(
        engine.current(&div_by_zero),
        None,
        "close / 0 must produce no value"
    );

    // The comparison over it is false; `Not` over the false leaf is true — the
    // #16 composition, unchanged: entries stay gated by the engine's warm gate.
    let compare_zero = compiled(&base_dsl(
        cmp(
            arith(
                ArithOp::Div,
                price(Series::Primary, PriceField::Close),
                constant(0),
            ),
            Comparator::Gt,
            constant(0),
        ),
        vec![stop_loss()],
    ));
    assert!(
        !compare_zero.entry().eval(&engine),
        "a comparison over a no-value operand must be false"
    );
    let not_version = compiled(&base_dsl(
        Condition::Not {
            condition: Box::new(cmp(
                arith(
                    ArithOp::Div,
                    price(Series::Primary, PriceField::Close),
                    constant(0),
                ),
                Comparator::Gt,
                constant(0),
            )),
        },
        vec![stop_loss()],
    ));
    assert!(
        not_version.entry().eval(&engine),
        "Not over a false leaf is true, exactly as today"
    );

    // An unwarmed operand: ATR(200) never produces a value over 6 bars.
    let unwarmed = cv_arith(
        ArithOp::Div,
        CompiledValue::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Atr {
                period: SweepableValue::Fixed(200),
            },
            lag: 0,
        },
        cv_close(0),
    );
    assert_eq!(
        engine.current(&unwarmed),
        None,
        "an expression over an unwarmed operand must produce no value"
    );
    let unwarmed_compare = compiled(&base_dsl(
        cmp(
            arith(
                ArithOp::Div,
                atr_operand(200),
                price(Series::Primary, PriceField::Close),
            ),
            Comparator::Lt,
            constant(1),
        ),
        vec![stop_loss()],
    ));
    assert!(
        !unwarmed_compare.entry().eval(&engine),
        "a comparison over an unwarmed operand must be false"
    );
}

// ---------------------------------------------------------------------------
// (ii) lag on its own series
// ---------------------------------------------------------------------------

/// (ii) `lag(close, 3)` reads the close 3 primary bars back; `previous` shifts
/// to 4; a lag deeper than the recorded history gives no value.
#[test]
fn lag_close_reads_primary_bars_back() {
    // 6 bars, closes 100..105; the last close (105) is "today".
    let primary = series(Timeframe::M15, (0..6).map(|i| m15(i, 100 + i)).collect());
    let strategy = compiled(&base_dsl(
        cmp(
            price(Series::Primary, PriceField::Close),
            Comparator::Gt,
            lag(price(Series::Primary, PriceField::Close), 3),
        ),
        vec![stop_loss()],
    ));
    let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");
    for candle in &primary.candles {
        engine.step(candle);
    }

    assert_eq!(engine.current(&cv_close(0)), Some(dec(105, 0)));
    assert_eq!(
        engine.current(&cv_close(3)),
        Some(dec(102, 0)),
        "lag(close, 3) must read the close 3 primary bars back"
    );
    assert_eq!(
        engine.previous(&cv_close(3)),
        Some(dec(101, 0)),
        "previous of a lag-3 leaf shifts one bar deeper"
    );
    // Only 6 candles were stepped: a lag-6 read has no recorded bar yet.
    assert_eq!(engine.current(&cv_close(6)), None);
}

/// (ii) `lag(h4:ema(5), 1)` reads the EMA of the **previous closed H4 candle** —
/// the HTF engine's ring advances once per closed H4 bar (Q2's own-series rule),
/// never one M15 bar back.
#[test]
fn lag_htf_ema_reads_previous_closed_h4_candle() {
    let strategy = compiled(&base_dsl(
        cmp(lag(ind(Series::Htf, 5), 1), Comparator::Gt, constant(0)),
        vec![stop_loss()],
    ));
    // The lag routes to the Htf engine; the primary series carries neither an
    // indicator nor a lagged Price leaf, so the primary engine warms at once.
    let primary_engine = IndicatorEngine::new(&strategy).expect("primary engine");
    assert!(
        primary_engine.is_warm(),
        "no primary indicator and no primary lag: the primary engine is warm immediately"
    );

    let mut engine =
        IndicatorEngine::for_series(&strategy, Series::Htf).expect("htf engine builds");
    let ema_today = cv_ind(Series::Htf, 5, 0);
    let ema_lag1 = cv_ind(Series::Htf, 5, 1);
    // Step six closed H4 candles; the fixture's closes move every bar.
    for j in 0..6 {
        engine.step(&h4(j, 100 + (j % 3) * 10));
    }
    let lag1 = engine.current(&ema_lag1).expect("lag 1 is within the ring");
    let direct_previous = engine
        .previous(&ema_today)
        .expect("a previous value exists");
    assert_eq!(
        lag1, direct_previous,
        "lag(h4:ema(5), 1) must equal the previous closed-H4 EMA read"
    );
    assert_ne!(
        engine.current(&ema_today),
        Some(lag1),
        "the fixture must move the EMA across H4 bars for this proof to bite"
    );
}

// ---------------------------------------------------------------------------
// (iii) Rising / Falling equivalence with the hand-written twin
// ---------------------------------------------------------------------------

/// The rising strategy and its hand-written twin (the exact compiled form Q2
/// declares): entry `h4:ema(20) rising (1 bar)` ⇔
/// `Compare{h4:ema(20), Gt, Lag{h4:ema(20), 1}}`, signal exit `falling` ⇔
/// `Compare{…, Lt, Lag{…, 1}}`. Identical fixtures must produce identical
/// results and identical `result_content_hash`es.
#[test]
fn rising_and_falling_equal_their_hand_written_twins() {
    let primary = series(Timeframe::M15, (0..640).map(m15_flat).collect());
    let htf = htf_fixture(40);
    let ema_htf = ind(Series::Htf, 20);

    // Rising entry + falling signal exit, and the twin pair.
    let rising_dsl = base_dsl(
        rising(ema_htf.clone(), 1),
        vec![
            stop_loss(),
            take_profit(),
            signal_exit(falling(ema_htf.clone(), 1)),
        ],
    );
    let rising_twin_dsl = base_dsl(
        cmp(ema_htf.clone(), Comparator::Gt, lag(ema_htf.clone(), 1)),
        vec![
            stop_loss(),
            take_profit(),
            signal_exit(cmp(
                ema_htf.clone(),
                Comparator::Lt,
                lag(ema_htf.clone(), 1),
            )),
        ],
    );
    let rising_run = run(&compiled(&rising_dsl), &primary, Some(&htf));
    let rising_twin_run = run(&compiled(&rising_twin_dsl), &primary, Some(&htf));
    assert_eq!(
        rising_run, rising_twin_run,
        "Rising must behave exactly like its hand-written Compare+Lag twin"
    );
    assert_eq!(
        rising_run.result_content_hash(),
        rising_twin_run.result_content_hash(),
        "the content hashes must be identical"
    );
    assert!(
        !rising_run.trades.is_empty(),
        "the fixture must trade for the equivalence to be meaningful"
    );

    // Falling entry + rising signal exit, and the Lt/Gt twin pair.
    let falling_dsl = base_dsl(
        falling(ema_htf.clone(), 2),
        vec![
            stop_loss(),
            take_profit(),
            signal_exit(rising(ema_htf.clone(), 2)),
        ],
    );
    let falling_twin_dsl = base_dsl(
        cmp(ema_htf.clone(), Comparator::Lt, lag(ema_htf.clone(), 2)),
        vec![
            stop_loss(),
            take_profit(),
            signal_exit(cmp(ema_htf, Comparator::Gt, lag(ind(Series::Htf, 20), 2))),
        ],
    );
    let falling_run = run(&compiled(&falling_dsl), &primary, Some(&htf));
    let falling_twin_run = run(&compiled(&falling_twin_dsl), &primary, Some(&htf));
    assert_eq!(
        falling_run, falling_twin_run,
        "Falling must behave exactly like its hand-written Compare+Lag twin"
    );
    assert_eq!(
        falling_run.result_content_hash(),
        falling_twin_run.result_content_hash(),
    );
}

// ---------------------------------------------------------------------------
// (iv) lag-aware warm-up (b4)
// ---------------------------------------------------------------------------

/// (iv) A strategy entering on `ema(10) > lag(ema(10), 5)` has its first fully
/// warm bar exactly 5 primary bars after one entering on `ema(10) > 0`; the
/// no-lag strategy's warm bar is the pre-change (`85052e8`) warm bar — the bar
/// where `current` and `previous` first both exist and the indicator is ready,
/// which the no-lag `is_warm` of a `from_specs` engine still computes today.
#[test]
fn lag_five_shifts_the_first_warm_bar_by_exactly_five_bars() {
    let primary = series(
        Timeframe::M15,
        (0..60).map(|i| m15(i, 100 + i % 7)).collect(),
    );
    let ema10 = ind(Series::Primary, 10);
    let lag5 = compiled(&base_dsl(
        cmp(ema10.clone(), Comparator::Gt, lag(ema10.clone(), 5)),
        vec![stop_loss()],
    ));
    let no_lag = compiled(&base_dsl(
        cmp(ema10, Comparator::Gt, constant(0)),
        vec![stop_loss()],
    ));

    let lag5_warm =
        first_fully_warm_bar_ms(&lag5, &primary, None, None).expect("lag strategy warms");
    let no_lag_warm =
        first_fully_warm_bar_ms(&no_lag, &primary, None, None).expect("no-lag strategy warms");
    assert_eq!(
        lag5_warm - no_lag_warm,
        5 * Timeframe::M15.duration_ms(),
        "the deepest lag must move the first warm bar by exactly that many bars"
    );

    // The no-lag warm point is the 85052e8 warm point: reconstruct the legacy
    // definition with the public no-lag API (a from_specs engine carries no
    // lags, so its `is_warm` IS the pre-change check) and require the same bar.
    let mut engine = IndicatorEngine::from_specs(&[IndicatorSpec::Ema {
        period: SweepableValue::Fixed(10),
    }])
    .expect("engine builds");
    let legacy_warm = primary
        .candles
        .iter()
        .enumerate()
        .find(|(idx, candle)| {
            engine.step(candle);
            *idx > 0 && engine.is_warm()
        })
        .map(|(_, candle)| candle.open_time)
        .expect("the indicator warms over the fixture");
    assert_eq!(
        no_lag_warm, legacy_warm,
        "a strategy with no lag must keep the exact warm point it had at 85052e8"
    );
}

/// (iv) Walk-forward's defaulted `from` IS `first_fully_warm_bar_ms`
/// (`resolve_counted_span`), so the lag-5 entry's default span starts exactly 5
/// primary bars later than the no-lag entry's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walk_forward_default_from_moves_with_the_lag() {
    let tmp = TempDir::new().expect("tempdir");
    let (_db_path, db) = migrated_db(&tmp).await;
    let store_dir = tmp.path().join("candles");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let store = CandleStore::with_base_dir(store_dir);

    let ema10 = ind(Series::Primary, 10);
    let lag5_version = make_wf_version(
        &strategies,
        "wf lag5",
        cmp(ema10.clone(), Comparator::Gt, lag(ema10.clone(), 5)),
    )
    .await;
    let no_lag_version = make_wf_version(
        &strategies,
        "wf no-lag",
        cmp(ema10, Comparator::Gt, constant(0)),
    )
    .await;

    let request = |version: VersionId| WalkForwardRequest {
        version_id: version,
        pair: Pair::new("BTCUSDT"),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: None,
        config: BacktestConfig::default(),
        snapshots: None,
        from_ms: None,
        to_ms: None,
        k: None,
    };

    let lag5_outcome = run_walk_forward(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &request(lag5_version),
        None,
    )
    .await
    .expect("lag walk-forward completes");
    let no_lag_outcome = run_walk_forward(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &request(no_lag_version),
        None,
    )
    .await
    .expect("no-lag walk-forward completes");

    assert!(lag5_outcome.run.from_defaulted && no_lag_outcome.run.from_defaulted);
    assert_eq!(
        lag5_outcome.run.span.from_ms - no_lag_outcome.run.span.from_ms,
        5 * Timeframe::M15.duration_ms(),
        "the defaulted walk-forward `from` must move by exactly the lag"
    );
}

async fn make_wf_version(
    strategies: &SqliteStrategyRepo<SystemClock>,
    name: &str,
    entry: Condition,
) -> VersionId {
    let strategy = strategies
        .create_strategy(name, None, &[])
        .await
        .expect("create strategy");
    strategies
        .create_version(NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(&base_dsl(entry, vec![stop_loss()]))
                .expect("dsl serializes"),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version")
        .id
}

// ---------------------------------------------------------------------------
// (v) validation
// ---------------------------------------------------------------------------

/// (v) Depth 5 — five `Arith` nodes along one root-to-leaf path — is refused
/// with `FieldRange` at the node that exceeds the cap; four are legal.
#[test]
fn depth_five_is_refused_with_field_range_at_the_exceeding_node() {
    // Five nested Arith nodes on the lhs; the innermost is the fifth.
    let mut value = price(Series::Primary, PriceField::Close);
    for _ in 0..5 {
        value = arith(ArithOp::Add, value, constant(1));
    }
    let dsl = base_dsl(cmp(value, Comparator::Gt, constant(0)), vec![stop_loss()]);
    let errors = validate(&dsl).expect_err("depth 5 must be refused");
    let depth_error = errors
        .errors()
        .iter()
        .find(|e| e.code == ValidationCode::FieldRange)
        .expect("a FieldRange error is reported");
    assert_eq!(
        depth_error.path, "entry.lhs.arith.lhs.arith.lhs.arith.lhs.arith.lhs.arith",
        "the refusal names the node that exceeds the cap, got {}",
        depth_error.path
    );

    // Four nodes are legal: the same chain, one link shorter, validates.
    let mut value = price(Series::Primary, PriceField::Close);
    for _ in 0..4 {
        value = arith(ArithOp::Add, value, constant(1));
    }
    let dsl = base_dsl(cmp(value, Comparator::Gt, constant(0)), vec![stop_loss()]);
    validate(&dsl).expect("depth 4 is the legal maximum");

    // r3.s2 round-1 fix (C3): the cap is judged PER PATH. A legal four-node
    // left branch (root + three `lhs` nodes) must not make a compound right
    // branch read as a fifth-level node — before the fix the depth the left
    // walk reached leaked into the right walk and refused this document.
    let mut deep_lhs = price(Series::Primary, PriceField::Close);
    for _ in 0..3 {
        deep_lhs = arith(ArithOp::Add, deep_lhs, constant(1));
    }
    let value = arith(
        ArithOp::Mul,
        deep_lhs,
        arith(
            ArithOp::Sub,
            price(Series::Primary, PriceField::Close),
            constant(2),
        ),
    );
    let dsl = base_dsl(cmp(value, Comparator::Gt, constant(0)), vec![stop_loss()]);
    let errors = validate(&dsl);
    assert!(
        errors.is_ok(),
        "a four-node lhs path with a compound rhs is legal, got {:?}",
        errors.err().map(|e| e.errors().to_vec())
    );
}

/// (v) `bars` outside 1..=500 is refused with `FieldRange`, on `Lag` and on
/// `Rising`/`Falling`; the boundaries 1 and 500 are legal.
#[test]
fn bars_out_of_range_is_refused_with_field_range() {
    for bars in [0u32, 501] {
        let dsl = base_dsl(
            cmp(
                price(Series::Primary, PriceField::Close),
                Comparator::Gt,
                lag(price(Series::Primary, PriceField::Close), bars),
            ),
            vec![stop_loss()],
        );
        let errors = validate(&dsl).expect_err("out-of-range bars must be refused");
        assert!(
            errors
                .errors()
                .iter()
                .any(|e| e.code == ValidationCode::FieldRange && e.path == "entry.rhs.lag.bars"),
            "bars {bars} must be refused with FieldRange at entry.rhs.lag.bars, got {:?}",
            errors.errors()
        );

        let dsl = base_dsl(
            rising(price(Series::Primary, PriceField::Close), bars),
            vec![stop_loss()],
        );
        let errors = validate(&dsl).expect_err("out-of-range rising bars must be refused");
        assert!(
            errors
                .errors()
                .iter()
                .any(|e| e.code == ValidationCode::FieldRange && e.path == "entry.rising.bars"),
            "rising bars {bars} must be refused with FieldRange at entry.rising.bars"
        );

        let dsl = base_dsl(
            falling(price(Series::Primary, PriceField::Close), bars),
            vec![stop_loss()],
        );
        let errors = validate(&dsl).expect_err("out-of-range falling bars must be refused");
        assert!(
            errors
                .errors()
                .iter()
                .any(|e| e.code == ValidationCode::FieldRange && e.path == "entry.falling.bars"),
            "falling bars {bars} must be refused with FieldRange at entry.falling.bars"
        );
    }
    for bars in [1u32, 500] {
        let dsl = base_dsl(
            cmp(
                price(Series::Primary, PriceField::Close),
                Comparator::Gt,
                lag(price(Series::Primary, PriceField::Close), bars),
            ),
            vec![stop_loss()],
        );
        validate(&dsl).expect("in-range bars validate");
    }
}

/// (v) Lag of a lag, a mixed-series lag, and Rising/Falling over a lag or a
/// mixed-series value are each refused with `InvalidExpression`; a `Constant`
/// is series-neutral and never makes a value mixed.
#[test]
fn invalid_expressions_are_refused() {
    // Lag of a lag.
    let dsl = base_dsl(
        cmp(
            price(Series::Primary, PriceField::Close),
            Comparator::Gt,
            lag(lag(price(Series::Primary, PriceField::Close), 2), 3),
        ),
        vec![stop_loss()],
    );
    let errors = validate(&dsl).expect_err("lag of a lag must be refused");
    assert!(
        errors
            .errors()
            .iter()
            .any(|e| e.code == ValidationCode::InvalidExpression),
        "lag of a lag is InvalidExpression, got {:?}",
        errors.errors()
    );

    // A mixed-series lag: an expression with leaves on both series, lagged.
    let dsl = base_dsl(
        cmp(
            price(Series::Primary, PriceField::Close),
            Comparator::Gt,
            lag(
                arith(
                    ArithOp::Sub,
                    price(Series::Primary, PriceField::Close),
                    price(Series::Htf, PriceField::Close),
                ),
                2,
            ),
        ),
        vec![stop_loss()],
    );
    let errors = validate(&dsl).expect_err("a mixed-series lag must be refused");
    assert!(
        errors
            .errors()
            .iter()
            .any(|e| e.code == ValidationCode::InvalidExpression),
        "a mixed-series lag is InvalidExpression"
    );

    // Rising over a lag — the compiled form would be a lag of a lag.
    let dsl = base_dsl(
        rising(lag(price(Series::Primary, PriceField::Close), 1), 1),
        vec![stop_loss()],
    );
    let errors = validate(&dsl).expect_err("Rising over a lag must be refused");
    assert!(
        errors
            .errors()
            .iter()
            .any(|e| e.code == ValidationCode::InvalidExpression),
        "Rising over a lag is InvalidExpression"
    );

    // Falling over a mixed-series value.
    let dsl = base_dsl(
        falling(
            arith(
                ArithOp::Add,
                price(Series::Primary, PriceField::Close),
                price(Series::Htf, PriceField::Close),
            ),
            1,
        ),
        vec![stop_loss()],
    );
    let errors = validate(&dsl).expect_err("Falling over a mixed-series value must be refused");
    assert!(
        errors
            .errors()
            .iter()
            .any(|e| e.code == ValidationCode::InvalidExpression),
        "Falling over a mixed-series value is InvalidExpression"
    );

    // A constant is series-neutral: lag over close + constant is legal.
    let dsl = base_dsl(
        cmp(
            price(Series::Primary, PriceField::Close),
            Comparator::Gt,
            lag(
                arith(
                    ArithOp::Add,
                    price(Series::Primary, PriceField::Close),
                    constant(5),
                ),
                2,
            ),
        ),
        vec![stop_loss()],
    );
    validate(&dsl).expect("a constant never makes a value mixed");
}

/// (v) r3.s2 round-1 fix (C5): a `Lag` in EITHER `Arith` branch makes the whole
/// value lagged, so `Rising`/`Falling` over it is refused wherever the lag sits
/// — the merge used to drop `rhs.under_lag`, the document validated, and
/// `push_lag`'s `lag.max(bars)` (safe only because no accepted document
/// overlaps a lag with a lag) then read the bar at the outer lag instead of the
/// composed one: silently different signals than the document states.
#[test]
fn slope_over_a_lag_in_either_branch_is_refused() {
    // r3.s2 round-1 fix (C5): the lag sits in the RIGHT branch of the arith —
    // still a lag under the slope, and still refused. Before the fix the merge
    // dropped `rhs.under_lag`, the document validated, and `push_lag`'s
    // `lag.max(bars)` read the bar at the OUTER lag instead of the composed one.
    let dsl = base_dsl(
        rising(
            arith(
                ArithOp::Sub,
                price(Series::Primary, PriceField::Close),
                lag(price(Series::Primary, PriceField::High), 1),
            ),
            2,
        ),
        vec![stop_loss()],
    );
    let errors = validate(&dsl).expect_err("Rising over a lag in the right branch must be refused");
    assert!(
        errors
            .errors()
            .iter()
            .any(|e| e.code == ValidationCode::InvalidExpression),
        "Rising over a rhs-only lag is InvalidExpression, got {:?}",
        errors.errors()
    );

    // The same shape under `Falling` (the twin arm).
    let dsl = base_dsl(
        falling(
            arith(
                ArithOp::Add,
                lag(price(Series::Primary, PriceField::Low), 3),
                price(Series::Primary, PriceField::Close),
            ),
            1,
        ),
        vec![stop_loss()],
    );
    let errors = validate(&dsl).expect_err("Falling over a lag in the left branch must be refused");
    assert!(
        errors
            .errors()
            .iter()
            .any(|e| e.code == ValidationCode::InvalidExpression),
        "Falling over a lhs-only lag is InvalidExpression"
    );

    // A lag in NEITHER branch still validates: the rule is about lags, not
    // about arithmetic under a slope.
    let dsl = base_dsl(
        rising(
            arith(
                ArithOp::Sub,
                price(Series::Primary, PriceField::Close),
                price(Series::Primary, PriceField::High),
            ),
            2,
        ),
        vec![stop_loss()],
    );
    validate(&dsl).expect("a slope over a lag-free arith is legal");
}

/// (v) r3.s2 round-1 fix (C4): a `Rising`/`Falling` over a value with NO series
/// operand is the same constant on both sides of its compiled strict compare,
/// so it can never hold — refused as `ImpossibleCondition`, the constant-compare
/// rule reached through the slope's own compiled form (r3.s1.w2 rule 9, whose
/// sibling shape `CrossesAbove{Constant, Constant}` is refused as
/// `DegenerateCross`). A slope over any series operand stays legal.
#[test]
fn slope_over_a_constant_is_impossible() {
    for condition in [
        rising(constant(1), 1),
        falling(arith(ArithOp::Mul, constant(2), constant(3)), 5),
        // A lag over a constant is still a constant (the push-down drops it).
        rising(lag(constant(7), 4), 1),
    ] {
        let dsl = base_dsl(condition, vec![stop_loss()]);
        let errors = validate(&dsl).expect_err("a slope over a constant must be refused");
        assert!(
            errors
                .errors()
                .iter()
                .any(|e| e.code == ValidationCode::ImpossibleCondition),
            "a constant slope is ImpossibleCondition, got {:?}",
            errors.errors()
        );
    }

    // The message names the constant self-comparison, mirroring the
    // constant-compare refusal.
    let dsl = base_dsl(rising(constant(1), 1), vec![stop_loss()]);
    let errors = validate(&dsl).expect_err("a slope over a constant must be refused");
    assert!(
        errors
            .errors()
            .iter()
            .any(|e| e.message.contains("the constant comparison")
                && e.message.contains("never true")),
        "the message mirrors the constant-compare refusal, got {:?}",
        errors.errors()
    );

    // A slope over a series operand is satisfiable and stays legal.
    let dsl = base_dsl(
        rising(price(Series::Primary, PriceField::Close), 1),
        vec![stop_loss()],
    );
    validate(&dsl).expect("a slope over a price is legal");
}

/// (v) An MCP submit of the goal's ratio filter — `atr(14) / close < 0.02` as a
/// filter — succeeds, and the served `pulse://dsl/schema` lists all four new
/// constructs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_submit_of_the_ratio_filter_succeeds_and_schema_lists_the_constructs() {
    let tmp_db = TempDir::new().expect("tempdir");
    let (db_path, db) = migrated_db(&tmp_db).await;
    let (parent, _child) = seed_versions(&db).await;
    let tmp_store = TempDir::new().expect("tempdir");
    let store_dir = tmp_store.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);
    let _run = seed_real_run(&db, &store_dir, &parent).await;
    let client = spawn_client(&db_path, &store_dir).await;

    let result = call(
        &client,
        "submit_strategy_version",
        json!({
            "parent_version_id": parent.as_str().to_owned(),
            "dsl": {
                "schema_version": "1.2.0",
                "name": "atr ratio filter",
                "direction": "long",
                "entry": {
                    "type": "Compare",
                    "lhs": { "type": "Indicator", "spec": { "indicator": "Rsi", "period": 14 } },
                    "op": "Lt",
                    "rhs": { "type": "Constant", "value": "30" }
                },
                "filters": [
                    {
                        "type": "Compare",
                        "lhs": {
                            "type": "Arith",
                            "op": "div",
                            "lhs": { "type": "Indicator", "spec": { "indicator": "Atr", "period": 14 } },
                            "rhs": { "type": "Price", "field": "Close" }
                        },
                        "op": "Lt",
                        "rhs": { "type": "Constant", "value": "0.02" }
                    }
                ],
                "exits": [
                    { "type": "StopLoss", "distance_pct": "0.05" },
                    { "type": "TakeProfit", "target_r": "2.0" }
                ],
                "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
            },
            "hypothesis": "the goal's volatility-normalised filter submits through the served schema",
        }),
    )
    .await;
    assert!(
        result["version_id"].as_str().is_some(),
        "the ratio-filter submit must succeed, got: {result}"
    );

    // The served schema lists the four constructs.
    let schema = client
        .read_resource(ReadResourceRequestParams::new("pulse://dsl/schema"))
        .await
        .expect("resources/read pulse://dsl/schema");
    let body = match schema.contents.first() {
        Some(ResourceContents::TextResourceContents { text, .. }) => text.clone(),
        other => panic!("dsl_schema must be text contents, got {other:?}"),
    };
    for tag in ["Arith", "Lag", "Rising", "Falling"] {
        assert!(
            body.contains(tag),
            "the served schema must list {tag}, got: {body}"
        );
    }
    client.cancel().await.expect("cancel session");
}

// ---------------------------------------------------------------------------
// (vi) determinism — two cold runs over all three constructs
// ---------------------------------------------------------------------------

/// (vi) A strategy using all three constructs — a lagged EMA entry, the goal's
/// ratio filter, and a Falling signal exit — persists an identical
/// `result_content_hash` across two cold end-to-end runs on fresh stores (the
/// `two_cold_htf_atr_runs_persist_identical_hashes` oracle, extended to the new
/// evaluation surface).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_cold_value_expression_runs_persist_identical_hashes() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");

    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategies
        .create_strategy("value-expressions", None, &[])
        .await
        .expect("create strategy");
    let dsl_json = all_three_constructs_doc();
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json,
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");

    let store_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/btcusdt-1m-store");
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let request = BacktestRequest {
        version_id: version.id.clone(),
        pair: Pair::new("BTCUSDT"),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: None,
        config: BacktestConfig::default(),
        snapshots: None,
        window: None,
    };

    let store_one = CandleStore::with_base_dir(store_dir.clone());
    let run_one = run_version_backtest(
        &strategies,
        &store_one,
        &BinanceAdapter::new(),
        &runs,
        &request,
        None,
    )
    .await
    .expect("run one");
    let store_two = CandleStore::with_base_dir(store_dir);
    let run_two = run_version_backtest(
        &strategies,
        &store_two,
        &BinanceAdapter::new(),
        &runs,
        &request,
        None,
    )
    .await
    .expect("run two");

    assert_eq!(
        run_one.run.result_content_hash, run_two.run.result_content_hash,
        "two cold runs must persist identical result_content_hash"
    );
    assert!(
        !run_one.trades.is_empty(),
        "the all-three-constructs strategy must produce trades"
    );
}

/// The (vi) document: entry `ema(10) > lag(ema(10), 3)`, the goal's ratio
/// filter `atr(14) / close < 0.05`, and a `Falling (2 bars)` signal exit — all
/// three new constructs in one executable strategy.
fn all_three_constructs_doc() -> String {
    json!({
        "schema_version": "1.2.0",
        "name": "all three constructs",
        "direction": "long",
        "entry": {
            "type": "Compare",
            "lhs": { "type": "Indicator", "spec": { "indicator": "Ema", "period": 10 } },
            "op": "Gt",
            "rhs": {
                "type": "Lag",
                "value": { "type": "Indicator", "spec": { "indicator": "Ema", "period": 10 } },
                "bars": 3
            }
        },
        "filters": [
            {
                "type": "Compare",
                "lhs": {
                    "type": "Arith",
                    "op": "div",
                    "lhs": { "type": "Indicator", "spec": { "indicator": "Atr", "period": 14 } },
                    "rhs": { "type": "Price", "field": "Close" }
                },
                "op": "Lt",
                "rhs": { "type": "Constant", "value": "0.05" }
            }
        ],
        "exits": [
            { "type": "StopLoss", "distance_pct": "0.05" },
            {
                "type": "SignalExit",
                "condition": {
                    "type": "Falling",
                    "value": { "type": "Price", "field": "Close" },
                    "bars": 2
                }
            }
        ],
        "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Product safety — the deepest legal expression on a 2 MiB thread (#283 fallback)
// ---------------------------------------------------------------------------

/// `tests/deep_nesting_stack.rs` is w2's file and is NOT on this item's base, so
/// per spec §6 the case lives here: an `Arith` chain at depth 4 (the legal
/// maximum) inside nested `And`/`Not` conditions deserializes and validates on a
/// thread with a 2 MiB stack.
#[test]
fn deepest_legal_expression_deserializes_and_validates_on_2mib_thread() {
    // Depth-4 Arith chain: Arith(Arith(Arith(Arith(close, 1), 1), 1), 1).
    let mut chain = json!({ "type": "Price", "field": "Close" });
    for _ in 0..4 {
        chain = json!({
            "type": "Arith",
            "op": "add",
            "lhs": chain,
            "rhs": { "type": "Constant", "value": "1" }
        });
    }
    // Nested And/Not conditions carrying the chain as the entry compare.
    let document = json!({
        "schema_version": "1.2.0",
        "name": "deep but legal",
        "direction": "long",
        "entry": { "type": "And", "conditions": [
            { "type": "Not", "condition": { "type": "And", "conditions": [
                { "type": "Not", "condition": { "type": "And", "conditions": [
                    { "type": "Not", "condition": {
                        "type": "Compare",
                        "lhs": chain,
                        "op": "Gt",
                        "rhs": { "type": "Constant", "value": "0" }
                    } }
                ] } }
            ] } }
        ] },
        "filters": [],
        "exits": [
            { "type": "StopLoss", "distance_pct": "0.05" }
        ],
        "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
    });

    let handle = thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let parsed: StrategyDsl =
                serde_json::from_value(document).expect("deserializes on a 2 MiB thread");
            validate(&parsed).expect("validates on a 2 MiB thread");
        })
        .expect("spawn the 2 MiB thread");
    handle
        .join()
        .expect("the deep document survives a 2 MiB stack");
}
