//! r6.s2.w3 — two-sided setups on one position slot, and the setup trace persisted (ADR-0033).
//!
//! One strategy holds a long side and a short side. Both step on every bar and share one
//! position slot: a fired side enters only when no position and no pending entry exists, and the
//! position takes the direction of the side that fired (PDR-0045, STR-0003). A stored run keeps
//! each trade's setup trace (migration `0023`) and re-derives its content hash on read.
//!
//! Common scenario: the one of `tests/setup_sequence.rs` — 200 flat 15m bars at 100 from
//! 2025-01-01T00:00Z and 13 flat 4H bars at 100, except that **4H bar 2** is green (its close
//! arrives on 15m bar 47) and **4H bar 5** is red (O 101, H 101, L 100, C 100; its close arrives
//! on 15m bar 95). The strategy has both sides — `long` as in w2, and
//! `short: { arm: htf close < htf open, dip: close > 101, trigger: close < 99 }` — with
//! `direction` long, `StopLoss 0.05` and `TakeProfit 2`.
//!
//! What each case proves:
//!
//! - `both_sides_trade_one_at_a_time` — the long enters and exits on a time stop, then the short
//!   enters; each trade names its own arm and dip.
//! - `a_fire_on_a_busy_slot_enters_nothing` — a short fire while the long is open enters nothing.
//! - `short_tp_check_covers_a_two_sided_strategy` — the plan-time short take-profit check runs for
//!   the short side although `direction` is long.
//! - `persisted_run_keeps_the_trace` — a saved run reads back with every trace, and its content
//!   hash re-derives.
//! - `pre_0023_rows_read_without_a_trace` — a run whose trades carry no trace stores NULL columns
//!   and reads back with `setup: None` and an unchanged hash.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{
    BacktestConfig, BacktestError, BacktestInputs, BacktestResult, BacktestRunRepository, Candle,
    CandleSeries, Comparator, CompiledStrategy, Condition, DataVersion, Db, Direction,
    EngineSession, ExitReason, ExitRule, FundingConfig, MIGRATOR, Pair, PriceField, RiskParams,
    SchemaVersion, Series, SeriesEnd, SessionTimeframes, SetupEvent, SetupPhase, SetupSide,
    SetupTrace, SetupView, SnapshotSelection, SqliteBacktestRunRepo, StrategyDsl, SweepableValue,
    SymbolFilters, Timeframe, ValueSource, VersionId, compile, run_backtest, step_series, validate,
};
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use tempfile::TempDir;

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

/// A strategy with the given setup sides: `StopLoss` `stop`, `TakeProfit` `take_profit_r`, an
/// optional `TimeStop`, `expiry_bars` 96. `direction` is the strategy's own; a setup's sides
/// override it per entry.
fn strategy(
    direction: Direction,
    stop: &str,
    take_profit_r: &str,
    time_stop: Option<u32>,
    long: Option<SetupSide>,
    short: Option<SetupSide>,
) -> CompiledStrategy {
    let mut exits = vec![
        ExitRule::StopLoss {
            distance_pct: SweepableValue::Fixed(dec(stop)),
        },
        ExitRule::TakeProfit {
            target_r: SweepableValue::Fixed(dec(take_profit_r)),
        },
    ];
    if let Some(max_bars) = time_stop {
        exits.push(ExitRule::TimeStop {
            max_bars: SweepableValue::Fixed(max_bars),
        });
    }
    let setup = (long.is_some() || short.is_some()).then(|| {
        Box::new(pulse::Setup {
            expiry_bars: SweepableValue::Fixed(96),
            long,
            short,
        })
    });
    let dsl = StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "setup two sided".to_owned(),
        direction,
        entry: compare(
            price(Series::Primary, PriceField::Close),
            Comparator::Gt,
            constant("0"),
        ),
        filters: vec![],
        exits,
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(dec("0.01")),
            max_leverage: SweepableValue::Fixed(dec("3")),
        },
        setup,
    };
    compile(&validate(&dsl).expect("fixture validates")).expect("fixture compiles")
}

/// The two-sided strategy of the common scenario; `time_stop` is the optional `TimeStop`.
fn two_sided(time_stop: Option<u32>) -> CompiledStrategy {
    strategy(
        Direction::Long,
        "0.05",
        "2",
        time_stop,
        Some(long_side()),
        Some(short_side()),
    )
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
// Persistence helpers (a migrated temp DB, the `tests/support/mcp.rs` pattern)
// ---------------------------------------------------------------------------

async fn migrated_db(tmp: &TempDir) -> Db {
    let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    db
}

/// Seed the FK parents a `backtest_run` row needs, by raw SQL.
async fn seed_strategy_and_version(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO strategy (id, name, tags, archived, created_at) \
         VALUES ('strat-1', 'Two sided', '[]', 0, '2026-11-01T00:00:00.000Z')",
    )
    .execute(pool)
    .await
    .expect("seed strategy");
    sqlx::query(
        "INSERT INTO strategy_version \
         (id, strategy_id, dsl_schema_version, dsl, dsl_original, version_hash, created_by, \
          creating_llm_call_ids, created_at) \
         VALUES ('ver-1', 'strat-1', '1.6.0', '{}', '{}', 'hash-1', 'human', '[]', \
                 '2026-11-01T00:00:00.000Z')",
    )
    .execute(pool)
    .await
    .expect("seed strategy_version");
}

fn inputs(with_htf: bool) -> BacktestInputs {
    BacktestInputs {
        pair: Pair::new("BTCUSDT"),
        primary: SnapshotSelection {
            timeframe: Timeframe::M15,
            data_version: DataVersion::new("v-setup-two-sided"),
        },
        htf: with_htf.then(|| SnapshotSelection {
            timeframe: Timeframe::H4,
            data_version: DataVersion::new("v-setup-two-sided-htf"),
        }),
        d1: None,
        taker_fee_bps: Decimal::ZERO,
        slippage_bps: Decimal::ZERO,
        funding: FundingConfig::SnapshotRates,
        symbol_filters: None,
        window: None,
        lead_in_from_ms: None,
    }
}

/// Save `result` through the repository and read it back: the stored trades, plus the stored
/// row's content hash (`get_run` re-derives that hash from the read-back rows and refuses the
/// read when it differs, so a successful read proves the re-derivation).
async fn save_and_read(
    result: &BacktestResult,
    with_htf: bool,
) -> (Vec<pulse::Trade>, String, TempDir) {
    let tmp = TempDir::new().unwrap();
    let db = migrated_db(&tmp).await;
    seed_strategy_and_version(db.pool()).await;
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let id = repo
        .save_run(
            &VersionId::new("ver-1"),
            &inputs(with_htf),
            result,
            &result.summary,
            dec("10000"),
        )
        .await
        .expect("save the run");
    let stored = repo
        .get_run(&id)
        .await
        .expect("the stored run reads, so its hash re-derives")
        .expect("the saved run exists");
    let trades = repo.get_trades(&id).await.expect("read the trades");
    (trades, stored.result_content_hash, tmp)
}

// ---------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------

/// 4H bar 2 green (arm on 15m bar 47), long dip 50 and trigger 55; 4H bar 5 red (arm on bar 95),
/// short dip bar (O 100 H 101.5 L 100 C 101.5) at 97 and trigger bar (O 100 H 100 L 98.5 C 98.5)
/// at 100.
fn both_sides_scenario() -> (CandleSeries, CandleSeries) {
    let primary = series(
        Timeframe::M15,
        (0..200)
            .map(|i| match i {
                50 => dip_bar(i),
                55 => trigger_bar(i),
                97 => m15(i, "100", "101.5", "100", "101.5"),
                100 => m15(i, "100", "100", "98.5", "98.5"),
                _ => flat_m15(i),
            })
            .collect(),
    );
    (primary, htf_with(&[2], &[5]))
}

#[test]
fn both_sides_trade_one_at_a_time() {
    let (primary, htf) = both_sides_scenario();
    // The long fills on bar 56 (held bar 1), so a 10-bar time stop schedules its exit at bar 65's
    // close and closes it at bar 66's open — before the short side fires at bar 100.
    let result = run(&two_sided(Some(10)), &primary, &htf, None);

    assert_eq!(
        result.trades.len(),
        2,
        "one trade per side: {:?}",
        result.trades
    );
    let long = &result.trades[0];
    assert_eq!(long.direction, Direction::Long);
    assert_eq!(long.entry_signal_time, close_of(55));
    assert_eq!(long.entry_fill_time, open_of(56));
    assert_eq!(long.exit_reason, ExitReason::TimeStop);
    assert_eq!(long.exit_signal_time, close_of(65));
    assert_eq!(long.exit_fill_time, open_of(66));
    assert_eq!(
        long.setup,
        Some(SetupTrace {
            arm_time: close_of(47),
            dip_time: close_of(50),
            anchor_time: None,
        })
    );

    let short = &result.trades[1];
    assert_eq!(short.direction, Direction::Short);
    assert_eq!(short.entry_signal_time, close_of(100));
    assert_eq!(short.entry_fill_time, open_of(101));
    assert!(
        short.stop_price.expect("a fresh trade records its stop") > short.entry_price,
        "a short's stop sits above its entry"
    );
    assert_eq!(
        short.setup,
        Some(SetupTrace {
            arm_time: close_of(95),
            dip_time: close_of(97),
            anchor_time: None,
        })
    );
}

#[test]
fn a_fire_on_a_busy_slot_enters_nothing() {
    let (primary, htf) = both_sides_scenario();
    // No time stop: the long (stop 95, take-profit 110) is still open at bar 100.
    let compiled = two_sided(None);
    let result = run(&compiled, &primary, &htf, None);

    assert_eq!(result.trades.len(), 1, "the short fire enters nothing");
    assert_eq!(result.trades[0].direction, Direction::Long);
    assert_eq!(result.trades[0].exit_reason, ExitReason::EndOfData);

    let seen = views(&compiled, &primary, &htf);
    let short = seen[100].short.expect("the short side is declared");
    assert_eq!(short.event, Some(SetupEvent::Triggered));
    assert_eq!(short.phase, SetupPhase::Idle);
}

#[test]
fn short_tp_check_covers_a_two_sided_strategy() {
    let (primary, htf) = both_sides_scenario();
    // 2 × 0.5 ≥ 1 on the short side, although `direction` is long.
    let compiled = strategy(
        Direction::Long,
        "0.5",
        "2",
        None,
        Some(long_side()),
        Some(short_side()),
    );
    let outcome = run_backtest(
        &compiled,
        &primary,
        Some(&htf),
        None,
        &zero_slippage(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    );
    assert!(
        matches!(outcome, Err(BacktestError::ImpossibleTakeProfit(_))),
        "a short side that cannot take profit is refused: {outcome:?}"
    );

    // The same exits on a long-only setup run: only the declared sides are checked.
    let long_only = strategy(Direction::Long, "0.5", "2", None, Some(long_side()), None);
    run_backtest(
        &long_only,
        &primary,
        Some(&htf),
        None,
        &zero_slippage(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("a long-only setup has no short take-profit to refuse");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_run_keeps_the_trace() {
    let (primary, htf) = both_sides_scenario();
    let result = run(&two_sided(Some(10)), &primary, &htf, None);
    assert_eq!(result.trades.len(), 2);
    assert!(
        result.trades.iter().all(|trade| trade.setup.is_some()),
        "both trades carry a trace"
    );

    let (read, stored_hash, _tmp) = save_and_read(&result, true).await;
    assert_eq!(
        read, result.trades,
        "the read trades equal the saved trades"
    );
    assert_eq!(
        stored_hash,
        result.result_content_hash(),
        "the stored row's re-derived hash equals the saved result's hash"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_0023_rows_read_without_a_trace() {
    // A strategy without a setup: every trade carries `setup: None`, so the repository writes the
    // three trace columns NULL — the shape of every row written before `0023`.
    let compiled = strategy(Direction::Long, "0.05", "2", None, None, None);
    let primary = primary_with(&[]);
    let result = run_backtest(
        &compiled,
        &primary,
        None,
        None,
        &zero_slippage(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("backtest runs");
    assert!(!result.trades.is_empty(), "the entry gate opens a trade");
    assert!(result.trades.iter().all(|trade| trade.setup.is_none()));

    let (read, stored_hash, _tmp) = save_and_read(&result, false).await;
    assert!(read.iter().all(|trade| trade.setup.is_none()));
    assert_eq!(read, result.trades, "the trades read back unchanged");
    assert_eq!(
        stored_hash,
        result.result_content_hash(),
        "the run's re-derived hash is unchanged from its stored hash"
    );
}
