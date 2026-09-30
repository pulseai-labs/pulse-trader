//! r3.s1.w1 — every exit kind runs, and each trade says which stop it left at
//! (the `d42` target).
//!
//! `TrailingStop` and `TimeStop` are declared in the DSL, validated, persisted,
//! offered by the composer and advertised in the served schema — and the engine
//! used to refuse both at run time (`UnsupportedExit`). This suite pins the
//! ruled semantics (spine ruling G1) as behaviour, over gap-free, funding-stamped
//! true-M15 hand-built series driven through the real [`run_backtest`] loop:
//!
//! - **(i)** a long trailing-only stop exits at `trail_extreme × (1 − trail_pct)`,
//!   where the extreme is the highest high of CLOSED held bars before the exit
//!   bar — the exit bar's own high is never in its own check, proven by an exit
//!   bar whose high would have moved the level. The reason is `trailing_stop`;
//!   `trade.stop_price` stays the initial stop; realized R is measured against
//!   the initial distance.
//! - **(ii)** the short mirror.
//! - **(iii)** trailing plus a stop loss: a hit before the trail tightens past
//!   the stop is `stop_loss` at the stop-loss level; after, `trailing_stop` at
//!   the trailed level. Sizing uses the stop loss's distance in both.
//! - **(iv)** an open that gaps through the trailing level fills at the open,
//!   labelled `trailing_stop`.
//! - **(v)** the time stop: `max_bars = 3` leaves at the open of the 4th bar
//!   after entry (fill Δ = `3 × 900 000` ms, the M15 spacing); a signal exit on
//!   the same close wins the tie (`signal`); a stop hit inside the window wins
//!   because it is intra-bar and earlier.
//! - **(vi)** refusals: a time-stop-only strategy is still `NoStopLoss`; a
//!   trailing-only strategy runs (the trail stands in for the stop loss).
//! - **(vii)** determinism: two cold runs of a trailing-plus-time strategy
//!   persist an identical `result_content_hash` and identical trades; and a
//!   pre-existing strategy with no trailing or time stop re-runs to the hash
//!   frozen from the base engine.
//! - **(viii)** the composer: a scripted-provider compose emitting
//!   `set_exit_rules` with `stop_loss_pct`, `trailing_pct` and `time_bars`
//!   together persists a version whose exits carry all three, its backtest
//!   runs over the committed fixture store, and its trades' wire labels carry
//!   the new values where they occur.
//!
//! ## The (vii) freeze provenance
//!
//! `BASE_HASH_NO_TRAILING` was captured from the UNMODIFIED base engine —
//! worktree `r3.s1.w1` at spine-branch base `f411c18102323e0121e6aa7a5b06c3f911939a4d`,
//! via this file's `#[ignore]`d `print_base_hash_no_trailing` helper, BEFORE any
//! `src/adapters/backtest/engine.rs` edit (the run path was byte-identical to
//! the spawn state at capture time). Two categories of edit already existed and
//! cannot touch this hash: the compile-enabling `ExitReason` variant / tag /
//! wire-label additions (a no-trailing strategy's trades carry only tags 0–3)
//! and this test file itself. The item must not move the hash: a strategy with
//! no trailing or time stop runs the same engine path as before.
//
// `too_many_lines`: the (viii) compose e2e is one scripted end-to-end on
// purpose (tests/server_routes.rs' convention) — splitting it would scatter
// the compose → persist → run → label chain across helpers.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pulse::{
    BacktestConfig, BacktestError, BacktestRequest, BinanceAdapter, BusError, BusEvent, Candle,
    CandleSeries, CandleStore, CompiledStrategy, ComposeDeps, ComposeWiring, Condition, CreatedBy,
    CredentialSource, DataVersion, Db, Direction, EventSink, ExitReason, ExitRule, FakeClock,
    LlmBackend, LlmConfig, LlmError, LlmProvider, LlmResponse, MIGRATOR, Message, ModelPrice,
    NewVersion, Pair, PriceField, PriceTable, Redactor, RiskParams, RunId, SchemaVersion, Series,
    SeriesEnd, SqliteBacktestRunRepo, SqliteLlmCallRepo, SqliteStrategyRepo, StrategyDsl,
    StrategyRepository, SweepableValue, SymbolFilters, Timeframe, TokenUsage, ToolCall,
    ToolDefinition, ValueSource, VersionId, compile, compose_strategy_core, run_backtest,
    run_version_backtest, validate,
};
use rust_decimal::Decimal;
use serde_json::json;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Hand-built true-M15 series (w3's fixture shape)
// ---------------------------------------------------------------------------

/// A decimal from a plain string — OHLC fixtures read best as literals.
fn dec(s: &str) -> Decimal {
    s.parse::<Decimal>().expect("decimal literal")
}

/// One M15 candle at absolute index `idx` — true 900 000 ms spacing with
/// epoch-aligned boundaries, carrying the default zero-rate funding stamp on
/// every bar whose half-open span contains an 8h boundary (`open_time %
/// 28_800_000 == 0`), exactly as w3's `candle()` helper does. A zero rate pays
/// zero, so no scenario's expected values move.
fn m15(idx: i64, open: &str, high: &str, low: &str, close: &str) -> Candle {
    let open_time = idx * 900_000;
    Candle {
        open_time,
        close_time: open_time + 899_999,
        open: dec(open),
        high: dec(high),
        low: dec(low),
        close: dec(close),
        volume: Decimal::ONE,
        funding_rate: if open_time % 28_800_000 == 0 {
            Some(Decimal::ZERO)
        } else {
            None
        },
    }
}

fn series(candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe: Timeframe::M15,
        version: DataVersion::new("v-exit-kinds"),
        candles,
    }
}

/// The M15 spacing in ms — what a time stop's fill distance is measured in.
const BAR_MS: i64 = 900_000;

// ---------------------------------------------------------------------------
// Strategy construction
// ---------------------------------------------------------------------------

/// `close > 0` — always true from the first evaluated bar on, so entries fire
/// deterministically (signal on bar 1, fill on bar 2's open) with no indicators
/// to warm.
fn price_entry() -> Condition {
    Condition::Compare {
        lhs: ValueSource::Price {
            series: Series::Primary,
            field: PriceField::Close,
        },
        op: crate_order_op(),
        rhs: ValueSource::Constant {
            value: Decimal::ZERO,
        },
    }
}

fn crate_order_op() -> pulse::Comparator {
    pulse::Comparator::Gt
}

fn stop(distance_pct: &str) -> ExitRule {
    ExitRule::StopLoss {
        distance_pct: SweepableValue::Fixed(dec(distance_pct)),
    }
}

fn trail(trail_pct: &str) -> ExitRule {
    ExitRule::TrailingStop {
        trail_pct: SweepableValue::Fixed(dec(trail_pct)),
    }
}

fn time_stop(max_bars: u32) -> ExitRule {
    ExitRule::TimeStop {
        max_bars: SweepableValue::Fixed(max_bars),
    }
}

fn take_profit(target_r: &str) -> ExitRule {
    ExitRule::TakeProfit {
        target_r: SweepableValue::Fixed(dec(target_r)),
    }
}

fn signal_close_above(threshold: &str) -> ExitRule {
    ExitRule::SignalExit {
        condition: Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            },
            op: pulse::Comparator::Gte,
            rhs: ValueSource::Constant {
                value: dec(threshold),
            },
        },
    }
}

fn compiled(direction: Direction, exits: Vec<ExitRule>) -> CompiledStrategy {
    let dsl = StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "exit-kinds fixture".to_owned(),
        direction,
        entry: price_entry(),
        filters: vec![],
        exits,
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(dec("0.01")),
            max_leverage: SweepableValue::Fixed(Decimal::from(3)),
        },
    };
    compile(&validate(&dsl).unwrap()).unwrap()
}

/// Zero-fee, zero-slippage run over `series` from a cold engine.
fn run(
    strategy: &CompiledStrategy,
    series: &CandleSeries,
) -> Result<pulse::BacktestResult, BacktestError> {
    run_backtest(
        strategy,
        series,
        None,
        None,
        &BacktestConfig {
            starting_equity: dec("10000"),
            taker_fee_bps: Decimal::ZERO,
            slippage_bps: Decimal::ZERO,
        },
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
}

// ---------------------------------------------------------------------------
// (vi) refusals
// ---------------------------------------------------------------------------

/// A strategy whose only stop is a `TimeStop` still has no stop loss: the trail
/// stands in for one, a time stop does not (G1). The typed refusal stays
/// `NoStopLoss` — never the retired `UnsupportedExit`.
#[test]
fn time_only_still_refused_no_stop_loss() {
    let primary = series(vec![
        m15(0, "100", "101", "99", "100"),
        m15(1, "100", "102", "99", "101"),
        m15(2, "100", "103", "100", "102"),
    ]);
    let strategy = compiled(Direction::Long, vec![time_stop(5)]);

    let error = run(&strategy, &primary).unwrap_err();
    assert!(
        matches!(error, BacktestError::NoStopLoss),
        "a time-stop-only strategy must refuse NoStopLoss, got {error:?}"
    );
}

/// A trailing-only strategy RUNS — the trail stands in for the stop loss (G1):
/// its initial level is known at fill and sizes the position like a `Pct` stop.
#[test]
fn trailing_only_runs() {
    let primary = series(vec![
        m15(0, "100", "101", "99", "100"),
        m15(1, "100", "102", "99", "101"),
        m15(2, "100", "105", "100", "104"),
    ]);
    let strategy = compiled(Direction::Long, vec![trail("0.02")]);

    let result = run(&strategy, &primary).expect("a trailing-only strategy runs");
    assert!(
        !result.trades.is_empty(),
        "the fixture series must produce at least one trade"
    );
}

// ---------------------------------------------------------------------------
// (i) long trailing-only — the closed-bar level
// ---------------------------------------------------------------------------

/// Rising-then-falling series, trail 2%: the entry fills at bar 2's open (100),
/// the initial stop is `100 × 0.98 = 98.00`. Bar 2's high (105) folds in only
/// AFTER bar 2's check → level `105 × 0.98 = 102.90` for bar 3; bar 3's high
/// (106) folds → level `103.88` for bar 4. Bar 4's low (103) hits `103.88` —
/// and bar 4's OWN high (107) would have moved the level to `104.86` had it
/// folded in before the check, so the fill price itself proves the exit bar's
/// extreme is never in its own check.
#[test]
fn long_trailing_only_exits_at_the_closed_bar_level() {
    let primary = series(vec![
        m15(0, "100", "101", "99", "100"),
        m15(1, "100", "102", "99", "101"),
        m15(2, "100", "105", "100", "104"),
        m15(3, "104", "106", "103", "105"),
        m15(4, "105", "107", "103", "103.5"),
    ]);
    let strategy = compiled(Direction::Long, vec![trail("0.02")]);

    let result = run(&strategy, &primary).expect("runs");
    assert_eq!(result.trades.len(), 1, "one entry, one trail exit");
    let trade = &result.trades[0];
    assert_eq!(
        trade.exit_reason,
        ExitReason::TrailingStop,
        "a trailing-only stop labels every hit trailing_stop"
    );
    assert_eq!(
        trade.exit_price,
        dec("103.88"),
        "the exit fills at the level from CLOSED held bars (extreme 106 × 0.98); \
         the exit bar's own high (107 → 104.86) must not be in its own check"
    );
    assert_eq!(
        trade.stop_price,
        Some(dec("98.00")),
        "trade.stop_price records the INITIAL stop (the sizing basis), not the trailed level"
    );
    assert_eq!(
        trade.realized_r,
        dec("1.94"),
        "realized R is measured against the initial stop distance: \
         (103.88 − 100) / (100 − 98) = 1.94"
    );
    assert_eq!(
        trade.exit_fill_time, primary.candles[4].open_time,
        "an intra-bar stop hit fills on the bar it hits"
    );
}

// ---------------------------------------------------------------------------
// (ii) short trailing-only — the mirror
// ---------------------------------------------------------------------------

/// Falling series, short, trail 2%: entry fills at 100, initial stop
/// `100 × 1.02 = 102.00`. Bar 2's low (95) folds → level `96.90`; bar 3's low
/// (94) folds → `95.88`; bar 4's high (96.2) hits it. Bar 4's OWN low (93)
/// would have moved the level to `94.86` had it folded in first.
#[test]
fn short_trailing_only_mirrors_the_long() {
    let primary = series(vec![
        m15(0, "100", "101", "99", "100"),
        m15(1, "100", "101", "98", "99"),
        m15(2, "100", "100", "95", "96"),
        m15(3, "96", "96.5", "94", "94.5"),
        m15(4, "95", "96.2", "93", "93.5"),
    ]);
    let strategy = compiled(Direction::Short, vec![trail("0.02")]);

    let result = run(&strategy, &primary).expect("runs");
    assert_eq!(result.trades.len(), 1);
    let trade = &result.trades[0];
    assert_eq!(trade.exit_reason, ExitReason::TrailingStop);
    assert_eq!(
        trade.exit_price,
        dec("95.88"),
        "the short's level derives from the lowest low of CLOSED held bars \
         (extreme 94 × 1.02); the exit bar's own low (93 → 94.86) must not fold in first"
    );
    assert_eq!(
        trade.stop_price,
        Some(dec("102.00")),
        "the initial stop is entry × (1 + trail_pct) for a short"
    );
    assert_eq!(
        trade.realized_r,
        dec("2.06"),
        "(100 − 95.88) / (102 − 100) = 2.06, on the initial distance"
    );
}

// ---------------------------------------------------------------------------
// (iii) trailing plus a stop loss — the label transition and the sizing basis
// ---------------------------------------------------------------------------

/// Stop loss 1%, trail 3% (so the trail starts LOOSER than the stop):
/// run A dips through the stop before the trail ever tightens — the hit is
/// `stop_loss` at the stop-loss level; run B rises first (extreme 103 → level
/// `99.91` > the 99.00 stop: tightened), then dips — the hit is
/// `trailing_stop` at the trailed level. Both runs size on the STOP LOSS's
/// 1% distance, so their quantities are identical.
#[test]
fn trailing_with_stop_loss_labels_pre_and_post_tightening() {
    let before_tightening = series(vec![
        m15(0, "100", "101", "99", "100"),
        m15(1, "100", "102", "99", "101"),
        m15(2, "100", "102", "99", "101"),
    ]);
    let after_tightening = series(vec![
        m15(0, "100", "101", "99", "100"),
        m15(1, "100", "102", "99", "101"),
        m15(2, "100", "103", "100", "102"),
        m15(3, "102", "103.5", "99.5", "100"),
    ]);
    let strategy = compiled(Direction::Long, vec![stop("0.01"), trail("0.03")]);
    let pct_only = compiled(Direction::Long, vec![stop("0.01")]);

    let run_a = run(&strategy, &before_tightening).expect("runs");
    assert_eq!(run_a.trades.len(), 1);
    let a = &run_a.trades[0];
    assert_eq!(
        a.exit_reason,
        ExitReason::StopLoss,
        "before the trail tightens past the stop, the stop family's label holds"
    );
    assert_eq!(
        a.exit_price,
        dec("99.00"),
        "a pre-tightening hit fills at the stop-loss level (100 × 0.99)"
    );

    let pct_run = run(&pct_only, &before_tightening).expect("runs");
    assert_eq!(
        a.qty, pct_run.trades[0].qty,
        "sizing uses the stop loss's distance: the trail does not change the size basis"
    );

    let run_b = run(&strategy, &after_tightening).expect("runs");
    assert_eq!(run_b.trades.len(), 1);
    let b = &run_b.trades[0];
    assert_eq!(
        b.exit_reason,
        ExitReason::TrailingStop,
        "once the trail has tightened past the initial stop, the hit is trailing_stop"
    );
    assert_eq!(
        b.exit_price,
        dec("99.91"),
        "a post-tightening hit fills at the trailed level (extreme 103 × 0.97); \
         the exit bar's own high (103.5 → 100.395) must not fold in first"
    );
    assert_eq!(
        b.qty, a.qty,
        "both runs size on the stop loss's 1% distance"
    );
    assert_eq!(
        b.stop_price,
        Some(dec("99.00")),
        "the recorded stop stays the initial (stop-loss) level"
    );
}

// ---------------------------------------------------------------------------
// (iv) a gap through the trailing level
// ---------------------------------------------------------------------------

/// Bar 3 opens BELOW the trailed level (102.90): the position fills at the
/// open — the stop's gap rule — labelled `trailing_stop` (trail-only).
#[test]
fn a_gap_through_the_trailing_level_fills_at_the_open() {
    let primary = series(vec![
        m15(0, "100", "101", "99", "100"),
        m15(1, "100", "102", "99", "101"),
        m15(2, "100", "105", "100", "104"),
        m15(3, "101", "101.5", "100.5", "101"),
    ]);
    let strategy = compiled(Direction::Long, vec![trail("0.02")]);

    let result = run(&strategy, &primary).expect("runs");
    assert_eq!(result.trades.len(), 1);
    let trade = &result.trades[0];
    assert_eq!(trade.exit_reason, ExitReason::TrailingStop);
    assert_eq!(
        trade.exit_price,
        dec("101"),
        "an open through the level fills at the open, never at the unreachable level"
    );
    assert_eq!(
        trade.exit_fill_time, primary.candles[3].open_time,
        "the gap fill lands on the gapped bar's open"
    );
    assert_eq!(
        trade.stop_price,
        Some(dec("98.00")),
        "the recorded stop stays the initial level"
    );
}

// ---------------------------------------------------------------------------
// (v) the time stop
// ---------------------------------------------------------------------------

/// `max_bars = 3`, trailing guard that never tightens into a hit: the entry
/// fills at bar 2's open, `bars_held` reaches 3 at bar 4's close, the pending
/// exit fills at bar 5's open — exactly `3 × 900 000` ms after the entry fill.
#[test]
fn time_stop_leaves_at_the_open_after_max_bars_held() {
    let primary = series(vec![
        m15(0, "100", "101", "99", "100"),
        m15(1, "100", "102", "99", "101"),
        m15(2, "100", "103", "100", "102"),
        m15(3, "102", "103.2", "101.5", "102.5"),
        m15(4, "102.5", "103.4", "102.2", "103"),
        m15(5, "103", "103.5", "102.5", "103.2"),
    ]);
    let strategy = compiled(Direction::Long, vec![trail("0.02"), time_stop(3)]);

    let result = run(&strategy, &primary).expect("runs");
    assert_eq!(result.trades.len(), 1, "no other exit fires first");
    let trade = &result.trades[0];
    assert_eq!(trade.exit_reason, ExitReason::TimeStop);
    assert_eq!(
        trade.exit_fill_time - trade.entry_fill_time,
        3 * BAR_MS,
        "the exit fills 3 held bars (max_bars) after the entry fill, at the next open"
    );
    assert_eq!(
        trade.entry_fill_time, primary.candles[2].open_time,
        "the entry fills at bar 2's open (signal on bar 1's close)"
    );
    assert_eq!(
        trade.exit_fill_time, primary.candles[5].open_time,
        "the time-stop fill is bar 5's open — the 4th bar after entry"
    );
}

/// The same close as the time stop's trigger, a signal exit also true: the
/// signal evaluates FIRST and the label is `signal` (G1's tie rule).
#[test]
fn a_signal_exit_on_the_same_close_wins_the_tie() {
    let primary = series(vec![
        m15(0, "100", "101", "99", "100"),
        m15(1, "100", "102", "99", "101"),
        m15(2, "100", "103", "100", "102"),
        m15(3, "102", "103.2", "101.5", "102.5"),
        m15(4, "102.5", "103.4", "102.2", "103"),
        m15(5, "103", "103.5", "102.5", "103.2"),
    ]);
    // close ≥ 103 first becomes true at bar 4's close — the same close that
    // reaches `bars_held == 3`.
    let strategy = compiled(
        Direction::Long,
        vec![trail("0.02"), time_stop(3), signal_close_above("103")],
    );

    let result = run(&strategy, &primary).expect("runs");
    assert_eq!(result.trades.len(), 1);
    let trade = &result.trades[0];
    assert_eq!(
        trade.exit_reason,
        ExitReason::Signal,
        "a signal exit on the time stop's trigger close keeps the signal label"
    );
    assert_eq!(
        trade.exit_fill_time - trade.entry_fill_time,
        3 * BAR_MS,
        "the tie leaves on the same fill bar the time stop would have used"
    );
}

/// A stop hit inside the time window wins: the stop is intra-bar and earlier
/// than the `max_bars`-th close.
#[test]
fn a_stop_hit_inside_the_time_window_wins() {
    let primary = series(vec![
        m15(0, "100", "101", "99", "100"),
        m15(1, "100", "102", "99", "101"),
        m15(2, "100", "103", "100", "102"),
        m15(3, "102", "102.5", "100", "100.5"),
    ]);
    let strategy = compiled(Direction::Long, vec![trail("0.02"), time_stop(3)]);

    let result = run(&strategy, &primary).expect("runs");
    assert_eq!(result.trades.len(), 1);
    let trade = &result.trades[0];
    assert_eq!(
        trade.exit_reason,
        ExitReason::TrailingStop,
        "bar 3's low (100) breaches the trailed level (100.94) on bar 3 itself — \
         only 2 bars held, before the time stop's trigger"
    );
    assert_eq!(trade.exit_price, dec("100.94"));
    assert_eq!(trade.exit_fill_time, primary.candles[3].open_time);
}

// ---------------------------------------------------------------------------
// (vii) determinism
// ---------------------------------------------------------------------------

/// The freeze of a pre-existing strategy's `result_content_hash`, captured from
/// the base engine — see the module header's "The (vii) freeze provenance".
const BASE_HASH_NO_TRAILING: &str =
    "7539365da9cc020508a76b53b770cfa84facabcd6e96b6664ab58856187022a8";

/// The pre-existing (no trailing, no time stop) strategy the freeze pins:
/// price entry, a 5% stop loss, a 2R take-profit — the exact shape the spine
/// base already ran, over a fixed hand-built series.
fn frozen_strategy() -> CompiledStrategy {
    compiled(Direction::Long, vec![stop("0.05"), take_profit("2")])
}

fn frozen_series() -> CandleSeries {
    series(vec![
        m15(0, "100", "101", "99", "100"),
        m15(1, "100", "102", "99", "101"),
        m15(2, "100", "105", "100", "104"),
        m15(3, "104", "106", "103", "105"),
        m15(4, "105", "107", "103", "103.5"),
        m15(5, "104", "104.5", "102", "102.5"),
        m15(6, "102", "103", "101", "101.5"),
        m15(7, "101", "102", "100", "100.5"),
        m15(8, "100", "101", "99", "100"),
        m15(9, "100", "103", "100", "102"),
        m15(10, "102", "106", "101.5", "105"),
        m15(11, "105", "107", "104.5", "106"),
    ])
}

/// The capture helper for `BASE_HASH_NO_TRAILING` — run with
/// `cargo test --test exit_kinds print_base_hash_no_trailing -- --ignored`
/// against the UNMODIFIED base engine and paste the printed line into the
/// const above.
#[test]
#[ignore = "hash capture helper for the (vii) freeze, not a gate"]
fn print_base_hash_no_trailing() {
    let result = run(&frozen_strategy(), &frozen_series()).expect("runs");
    println!("BASE_HASH_NO_TRAILING={}", result.result_content_hash());
}

/// The freeze holds: a strategy with no trailing or time stop re-runs to the
/// same `result_content_hash` the base engine produced (this item changes no
/// existing run — both new exits were refused before it).
#[test]
fn a_pre_existing_strategy_reruns_to_the_base_hash() {
    let result = run(&frozen_strategy(), &frozen_series()).expect("runs");
    assert_eq!(
        result.result_content_hash(),
        BASE_HASH_NO_TRAILING,
        "the pre-existing strategy's content hash is frozen from the base engine"
    );
    assert!(!result.trades.is_empty(), "the frozen run has trades");
}

/// The persist-and-reload oracle (the `htf_atr_engine.rs` (f) pattern): two
/// cold end-to-end runs of a trailing-plus-time strategy over the committed
/// fixture store persist an identical `result_content_hash` and identical
/// trades.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_cold_runs_persist_identical_results() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");

    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategies
        .create_strategy("trail + time", None, &[])
        .await
        .expect("create strategy");
    let trail_time_dsl = r#"{
      "schema_version": "1.1.0",
      "name": "trail + time",
      "direction": "long",
      "entry": {
        "type": "Compare",
        "lhs": { "type": "Price", "series": "primary", "field": "Close" },
        "op": "Gt",
        "rhs": { "type": "Constant", "value": "0" }
      },
      "filters": [],
      "exits": [
        { "type": "TrailingStop", "trail_pct": "0.02" },
        { "type": "TimeStop", "max_bars": 8 }
      ],
      "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
    }"#;
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: trail_time_dsl.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");

    let store_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store");
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

    // Two cold runs: a fresh store handle each time, the same everything else.
    let store_one = CandleStore::with_base_dir(store_dir.clone());
    let run_one = run_version_backtest(
        &strategies,
        &store_one,
        &BinanceAdapter::new(),
        &runs,
        &request,
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
    )
    .await
    .expect("run two");

    assert_eq!(
        run_one.run.result_content_hash, run_two.run.result_content_hash,
        "two cold runs must persist an identical result_content_hash"
    );
    assert!(
        !run_one.trades.is_empty(),
        "the fixture must produce trades for the oracle to mean anything"
    );
    assert_eq!(
        serde_json::to_string(&run_one.trades).expect("serialize trades one"),
        serde_json::to_string(&run_two.trades).expect("serialize trades two"),
        "two cold runs must persist identical trades"
    );
    assert!(
        run_one.trades.iter().any(|t| matches!(
            t.exit_reason,
            ExitReason::TrailingStop | ExitReason::TimeStop
        )),
        "the fixture run must exercise at least one of the new exits"
    );
}

// ---------------------------------------------------------------------------
// (viii) the composer — scripted provider, in-process (no child process)
// ---------------------------------------------------------------------------

/// A stand-in composer system prompt (the fake provider ignores it).
const TEST_PROMPT: &str = "You are PulseTrader's strategy composer. Build the \
    strategy only by calling builder tools; never emit raw DSL JSON.";

/// A scripted [`LlmProvider`] double (`tests/tauri_compose.rs`'s pattern — the
/// provider is the only faked layer; everything else is real and in-process).
struct FakeComposerProvider {
    scripts: Mutex<VecDeque<LlmResponse>>,
}

impl LlmProvider for FakeComposerProvider {
    fn chat(
        &self,
        _messages: Vec<Message>,
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
    ) -> impl Future<Output = Result<LlmResponse, LlmError>> {
        let next = self.scripts.lock().expect("scripts lock").pop_front();
        std::future::ready(Ok(next.unwrap_or_else(|| LlmResponse {
            content: Some("(script exhausted)".to_owned()),
            tool_calls: Vec::new(),
            usage: test_usage(),
        })))
    }
}

fn test_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 120,
        output_tokens: 48,
    }
}

fn tool_turn(id: &str, name: &str, arguments: serde_json::Value) -> LlmResponse {
    LlmResponse {
        content: None,
        tool_calls: vec![ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments,
        }],
        usage: test_usage(),
    }
}

fn test_prices() -> PriceTable {
    let mut models = HashMap::new();
    models.insert(
        "gpt-oss:120b".to_owned(),
        ModelPrice {
            input_per_mtok: Decimal::from(2),
            output_per_mtok: Decimal::from(8),
        },
    );
    PriceTable::from_config("USD", models)
}

fn llm_config() -> LlmConfig {
    LlmConfig {
        backend: LlmBackend::Ollama,
        model: "gpt-oss:120b".to_owned(),
        temperature: 0.2,
        max_tokens: 1024,
        reasoning_effort: None,
    }
}

/// A collecting [`EventSink`] standing in for the webview's channel.
struct Collector {
    events: Mutex<Vec<BusEvent>>,
}

impl EventSink for Collector {
    fn send_event(&self, event: BusEvent) -> Result<(), BusError> {
        self.events.lock().expect("events lock").push(event);
        Ok(())
    }
}

async fn migrated_db() -> (TempDir, Db) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    (tmp, db)
}

fn compose_deps(
    db: &Db,
    script: Vec<LlmResponse>,
) -> ComposeDeps<
    FakeComposerProvider,
    SqliteLlmCallRepo<FakeClock>,
    SqliteStrategyRepo<pulse::SystemClock>,
    FakeClock,
> {
    let clock = FakeClock::at(1_700_000_000_000);
    ComposeDeps {
        wiring: ComposeWiring {
            provider: FakeComposerProvider {
                scripts: Mutex::new(script.into()),
            },
            llm_repo: SqliteLlmCallRepo::with_deps(db.pool().clone(), clock),
            redactor: Redactor::from_config(vec![]),
            prices: test_prices(),
            clock,
            prompt: TEST_PROMPT.to_owned(),
            key_source: Some(CredentialSource::ConfigDir),
            config: llm_config(),
        },
        strategy_repo: SqliteStrategyRepo::new(db.pool().clone()),
    }
}

/// The compose → run → read-labels end to end, entirely in-process: the
/// scripted provider emits `set_exit_rules` with `stop_loss_pct`,
/// `trailing_pct` and `time_bars` TOGETHER; the persisted version's exits
/// carry all three; its backtest runs; and the trades' wire labels carry the
/// new values where they occur. No child process is spawned anywhere in this
/// test — the bounded-read rule for spawned children is satisfied by not
/// spawning one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn composed_exit_rules_run_and_label_their_exits() {
    let (_tmp, db) = migrated_db().await;
    let script = vec![
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "Trail Time SL", "direction": "long" }),
        ),
        tool_turn(
            "c2",
            "add_entry_signal",
            json!({
                "left": { "source": "price", "price_field": "close" },
                "op": "gt",
                "right": { "source": "constant", "value": "0" }
            }),
        ),
        tool_turn(
            "c3",
            "set_exit_rules",
            json!({ "stop_loss_pct": "0.05", "trailing_pct": "0.02", "time_bars": 8 }),
        ),
        tool_turn(
            "c4",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ),
        tool_turn("c5", "finalize_strategy", json!({})),
    ];
    let deps = compose_deps(&db, script);
    let sink = Collector {
        events: Mutex::new(Vec::new()),
    };
    let outcome = compose_strategy_core(
        &RunId::new(),
        deps,
        "trail 2%, stop 5%, 8 bars",
        &sink,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .await
    .expect("the scripted compose persists a strategy version");
    assert!(!outcome.cancelled);

    let summary = outcome
        .strategy
        .as_ref()
        .expect("a finalized run carries its strategy summary");
    let reader = SqliteStrategyRepo::new(db.pool().clone());
    let version = reader
        .get_version(&VersionId::new(summary.version_id.clone()))
        .await
        .expect("get_version")
        .expect("the summary names a version that persisted");

    // The persisted exits carry all three kinds.
    let exits = &version.dsl.exits;
    assert!(
        exits.iter().any(|e| matches!(
            e,
            ExitRule::StopLoss { distance_pct: SweepableValue::Fixed(d) } if *d == dec("0.05")
        )),
        "the version carries the stop loss: {exits:?}"
    );
    assert!(
        exits.iter().any(|e| matches!(
            e,
            ExitRule::TrailingStop { trail_pct: SweepableValue::Fixed(d) } if *d == dec("0.02")
        )),
        "the version carries the trailing stop: {exits:?}"
    );
    assert!(
        exits.iter().any(|e| matches!(
            e,
            ExitRule::TimeStop {
                max_bars: SweepableValue::Fixed(8)
            }
        )),
        "the version carries the time stop: {exits:?}"
    );

    // The composed version's backtest RUNS (the engine no longer refuses).
    let store = CandleStore::with_base_dir(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store"),
    );
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
    let outcome = run_version_backtest(&reader, &store, &BinanceAdapter::new(), &runs, &request)
        .await
        .expect("the composed version runs");
    assert!(
        !outcome.trades.is_empty(),
        "the fixture produces trades for the composed strategy"
    );

    // Wire labels: every trade's exit reason serializes to its snake_case wire
    // value, and the new values occur where the fixture drives them.
    let labels: Vec<String> = outcome
        .trades
        .iter()
        .map(|t| {
            serde_json::to_string(&t.exit_reason)
                .expect("serialize exit reason")
                .trim_matches('"')
                .to_owned()
        })
        .collect();
    assert!(
        labels.iter().all(|l| matches!(
            l.as_str(),
            "stop_loss" | "take_profit" | "signal" | "end_of_data" | "trailing_stop" | "time_stop"
        )),
        "every wire label is a known exit reason: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "trailing_stop"),
        "the fixture must drive at least one trailing_stop exit: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "time_stop"),
        "the fixture must drive at least one time_stop exit: {labels:?}"
    );
}
