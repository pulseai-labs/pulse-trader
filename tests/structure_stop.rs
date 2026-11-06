//! r6.s2.w4 — the structure stop and the 3R target (ADR-0033 decision 6, STR-0003 v1 rules 6–8).
//!
//! A setup trade's stop sits at the structure of its pullback: the lowest low (long) or highest
//! high (short) from the stop anchor bar, or from the bar after the arm bar, up to the trigger
//! bar, pushed out by `buffer_pct`. Its 1R is the distance from the fill to that stop, and a
//! `TakeProfit` of 3 puts the target at fill ± 3R. When the next bar opens at or past the stop,
//! the entry is skipped and counted, not filled and not a failed run. A stored run keeps the count
//! (migration `0024`).
//!
//! Common scenario: the one of `tests/setup_sequence.rs` — 200 flat 15m bars at 100 from
//! 2025-01-01T00:00Z and 13 flat 4H bars at 100, except that **4H bar 2** is green (its close
//! arrives on 15m bar 47). The long side is `arm: htf close > htf open`, `dip: close < 99`,
//! `trigger: close > 101`, `stop_anchor: close < 99.8`. The exits are `StructureStop` with
//! `buffer_pct` 0.0005 and `TakeProfit` 3. Bars:
//!
//! - bar 48: O 100, H 100, L 98.0, C 100 — a lower low that is neither a dip nor an anchor.
//! - bar 49: O 100, H 100, L 99.6, C 99.7 — the anchor (`close < 99.8`).
//! - bar 50: O 100, H 100, L 98.5, C 98.5 — the dip bar.
//! - bar 55: O 100, H 101.5, L 100, C 101.5 — the trigger bar; the fill is on bar 56's open.
//!
//! What each case proves:
//!
//! - `anchored_long_stop_and_3r_target` — the stop is the anchored window's low less the buffer.
//! - `fallback_window_starts_after_the_arm` — with no anchor the window starts after the arm bar.
//! - `target_is_three_r_from_the_fill` — a `TakeProfit` of 3 sits at fill + 3 × (fill − stop).
//! - `short_mirror_uses_highs` — the short side uses the window's highest high.
//! - `fill_past_the_stop_is_skipped` — a fill at or past the stop skips and counts, no failed run.
//! - `skip_count_feeds_the_hash` — the count is covered by `result_content_hash`.
//! - `persisted_run_keeps_the_skip_count` — a saved run reads back with its count and hash.
//! - `pre_0024_rows_read_zero` — a row with the column NULL reads as 0, hash unchanged.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{
    BacktestConfig, BacktestInputs, BacktestResult, BacktestRunId, BacktestRunRepository, Candle,
    CandleSeries, Comparator, CompiledStrategy, Condition, DataVersion, Db, Direction, ExitReason,
    ExitRule, FundingConfig, MIGRATOR, Pair, PriceField, RiskParams, SchemaVersion, Series,
    SeriesEnd, SetupSide, SetupTrace, SnapshotSelection, SqliteBacktestRunRepo, StrategyDsl,
    SweepableValue, SymbolFilters, Timeframe, ValueSource, VersionId, compile, run_backtest,
    validate,
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
// Hand-built candle series (the helpers of tests/setup_sequence.rs)
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

fn series(timeframe: Timeframe, candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe,
        version: DataVersion::new("v-structure-stop"),
        candles,
    }
}

/// 200 flat 15m bars with `replace` swapped in at their indices.
fn primary_with(replace: &[(i64, Candle)]) -> CandleSeries {
    let candles = (0..200)
        .map(|i| {
            replace
                .iter()
                .find(|(at, _)| *at == i)
                .map_or_else(|| flat_m15(i), |(_, candle)| candle.clone())
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
// The scenario's bars
// ---------------------------------------------------------------------------

/// Bar 48: a lower low that is neither a dip nor an anchor.
fn bar_48() -> (i64, Candle) {
    (48, m15(48, "100", "100", "98.0", "100"))
}

/// Bar 49: the long side's anchor (`close < 99.8`).
fn bar_49() -> (i64, Candle) {
    (49, m15(49, "100", "100", "99.6", "99.7"))
}

/// Bar 50: the long dip bar.
fn dip_50() -> (i64, Candle) {
    (50, m15(50, "100", "100", "98.5", "98.5"))
}

/// Bar 55: the long trigger bar.
fn trigger_55() -> (i64, Candle) {
    (55, m15(55, "100", "101.5", "100", "101.5"))
}

/// The common long scenario's primary series.
fn long_primary(extra: &[(i64, Candle)]) -> CandleSeries {
    let mut bars = vec![bar_48(), bar_49(), dip_50(), trigger_55()];
    bars.extend(extra.iter().cloned());
    primary_with(&bars)
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

/// arm: htf close > htf open; dip: close < 99; trigger: close > 101; optional anchor: close < 99.8.
fn long_side(with_anchor: bool) -> SetupSide {
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
        stop_anchor: with_anchor.then(|| {
            compare(
                price(Series::Primary, PriceField::Close),
                Comparator::Lt,
                constant("99.8"),
            )
        }),
    }
}

/// arm: htf close < htf open; dip: close > 101; trigger: close < 99; anchor: close > 100.2.
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
        stop_anchor: Some(compare(
            price(Series::Primary, PriceField::Close),
            Comparator::Gt,
            constant("100.2"),
        )),
    }
}

/// A setup strategy with `StructureStop 0.0005` and `TakeProfit 3`, `expiry_bars` 96.
fn strategy(
    direction: Direction,
    long: Option<SetupSide>,
    short: Option<SetupSide>,
) -> CompiledStrategy {
    let dsl = StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "structure stop".to_owned(),
        direction,
        entry: compare(
            price(Series::Primary, PriceField::Close),
            Comparator::Gt,
            constant("0"),
        ),
        filters: vec![],
        exits: vec![
            ExitRule::StructureStop {
                buffer_pct: SweepableValue::Fixed(dec("0.0005")),
            },
            ExitRule::TakeProfit {
                target_r: SweepableValue::Fixed(dec("3")),
            },
        ],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(dec("0.01")),
            max_leverage: SweepableValue::Fixed(dec("3")),
        },
        setup: Some(Box::new(pulse::Setup {
            expiry_bars: SweepableValue::Fixed(96),
            long,
            short,
        })),
    };
    compile(&validate(&dsl).expect("fixture validates")).expect("fixture compiles")
}

fn long_strategy(with_anchor: bool) -> CompiledStrategy {
    strategy(Direction::Long, Some(long_side(with_anchor)), None)
}

fn zero_slippage() -> BacktestConfig {
    BacktestConfig {
        starting_equity: dec("10000"),
        taker_fee_bps: dec("0"),
        slippage_bps: dec("0"),
    }
}

fn run(compiled: &CompiledStrategy, primary: &CandleSeries, htf: &CandleSeries) -> BacktestResult {
    run_backtest(
        compiled,
        primary,
        Some(htf),
        None,
        &zero_slippage(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("backtest runs")
}

fn close_of(i: i64) -> i64 {
    START + i * M15_MS + M15_MS - 1
}

// ---------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------

#[test]
fn anchored_long_stop_and_3r_target() {
    let result = run(
        &long_strategy(true),
        &long_primary(&[]),
        &htf_with(&[2], &[]),
    );

    assert_eq!(result.trades.len(), 1, "one setup, one trade");
    let trade = &result.trades[0];
    assert_eq!(trade.direction, Direction::Long);
    assert_eq!(trade.entry_price, dec("100"), "the fill is bar 56's open");
    assert_eq!(
        trade.stop_price,
        Some(dec("98.45075")),
        "98.5 × (1 − 0.0005), the lowest low from the anchor bar through the trigger bar"
    );
    assert_eq!(
        trade.setup,
        Some(SetupTrace {
            arm_time: close_of(47),
            dip_time: close_of(50),
            anchor_time: Some(close_of(49)),
        })
    );
}

#[test]
fn fallback_window_starts_after_the_arm() {
    let result = run(
        &long_strategy(false),
        &long_primary(&[]),
        &htf_with(&[2], &[]),
    );

    assert_eq!(result.trades.len(), 1);
    let trade = &result.trades[0];
    assert_eq!(
        trade.stop_price,
        Some(dec("97.951")),
        "98.0 × (1 − 0.0005), the lowest low from the bar after the arm bar"
    );
    assert_eq!(
        trade.setup.expect("a setup trade has a trace").anchor_time,
        None
    );
}

#[test]
fn target_is_three_r_from_the_fill() {
    let bar_60 = (60, m15(60, "100", "105", "100", "104"));
    let result = run(
        &long_strategy(true),
        &long_primary(&[bar_60]),
        &htf_with(&[2], &[]),
    );

    assert_eq!(result.trades.len(), 1);
    let trade = &result.trades[0];
    assert_eq!(trade.exit_reason, ExitReason::TakeProfit);
    assert_eq!(
        trade.exit_price,
        dec("104.64775"),
        "100 + 3 × (100 − 98.45075)"
    );
    assert_eq!(
        trade.exit_fill_time,
        START + 60 * M15_MS,
        "the target is hit on bar 60"
    );
}

#[test]
fn short_mirror_uses_highs() {
    let primary = primary_with(&[
        (48, m15(48, "100", "102.0", "100", "100")),
        (49, m15(49, "100", "100.4", "100", "100.3")),
        (50, m15(50, "100", "101.5", "100", "101.5")),
        (55, m15(55, "100", "100", "98.5", "98.5")),
        (60, m15(60, "100", "100", "95.0", "96")),
    ]);
    let compiled = strategy(Direction::Short, None, Some(short_side()));
    let result = run(&compiled, &primary, &htf_with(&[], &[2]));

    assert_eq!(result.trades.len(), 1, "one Short trade");
    let trade = &result.trades[0];
    assert_eq!(trade.direction, Direction::Short);
    assert_eq!(
        trade.stop_price,
        Some(dec("101.55075")),
        "101.5 × (1 + 0.0005), the highest high from the anchor bar through the trigger bar"
    );
    assert_eq!(trade.exit_reason, ExitReason::TakeProfit);
    assert_eq!(
        trade.exit_price,
        dec("95.34775"),
        "100 − 3 × (101.55075 − 100)"
    );
}

/// The long scenario with bar 56 (the fill bar) opening at 98.4 — below the 98.45075 stop.
fn skipped_result() -> BacktestResult {
    let bar_56 = (56, m15(56, "98.4", "98.4", "98.4", "98.4"));
    run(
        &long_strategy(true),
        &long_primary(&[bar_56]),
        &htf_with(&[2], &[]),
    )
}

#[test]
fn fill_past_the_stop_is_skipped() {
    let result = skipped_result();

    assert!(
        result.trades.is_empty(),
        "no trade is filled: {:?}",
        result.trades
    );
    assert_eq!(
        result.skipped_entries.stop_beyond_fill, 1,
        "the skip is counted"
    );
    assert_eq!(
        result.skipped_entries.total(),
        1,
        "and it is the only skip of the run"
    );
}

#[test]
fn skip_count_feeds_the_hash() {
    let one = skipped_result();
    assert_eq!(one.skipped_entries.stop_beyond_fill, 1);
    let mut zero = one.clone();
    zero.skipped_entries.stop_beyond_fill = 0;

    assert_ne!(
        one.result_content_hash(),
        zero.result_content_hash(),
        "two results equal except for stop_beyond_fill 1 vs 0 hash differently"
    );
    assert_eq!(
        one.result_content_hash(),
        skipped_result().result_content_hash(),
        "the hash is deterministic"
    );
}

// ---------------------------------------------------------------------------
// Persistence helpers (a migrated temp DB, the `tests/setup_two_sided.rs` pattern)
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
         VALUES ('strat-1', 'Structure stop', '[]', 0, '2026-11-01T00:00:00.000Z')",
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

fn inputs() -> BacktestInputs {
    BacktestInputs {
        pair: Pair::new("BTCUSDT"),
        primary: SnapshotSelection {
            timeframe: Timeframe::M15,
            data_version: DataVersion::new("v-structure-stop"),
        },
        htf: Some(SnapshotSelection {
            timeframe: Timeframe::H4,
            data_version: DataVersion::new("v-structure-stop-htf"),
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

/// The serde token of a unit enum (`long`, `take_profit`, `trending_up`, …), the way the
/// repository writes it.
fn token<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .expect("serialize the token")
        .as_str()
        .expect("a unit enum is a string")
        .to_owned()
}

fn text(value: Decimal) -> String {
    value.normalize().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_run_keeps_the_skip_count() {
    let result = skipped_result();
    assert_eq!(result.skipped_entries.stop_beyond_fill, 1);

    let tmp = TempDir::new().unwrap();
    let db = migrated_db(&tmp).await;
    seed_strategy_and_version(db.pool()).await;
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let id = repo
        .save_run(
            &VersionId::new("ver-1"),
            &inputs(),
            &result,
            &result.summary,
            dec("10000"),
        )
        .await
        .expect("save the run");
    // `get_run` re-derives the content hash from the read-back rows and refuses the read when it
    // differs from the stored one, so a successful read proves the re-derivation.
    let stored = repo
        .get_run(&id)
        .await
        .expect("the stored run reads, so its hash re-derives")
        .expect("the saved run exists");

    assert_eq!(stored.skipped_entries.stop_beyond_fill, 1);
    assert_eq!(stored.skipped_entries, result.skipped_entries);
    assert_eq!(
        stored.result_content_hash,
        result.result_content_hash(),
        "the stored hash is the saved result's hash"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_0024_rows_read_zero() {
    // A run with one trade and no skips, written the way a pre-0024 build wrote it: the raw
    // INSERTs omit `skipped_stop_beyond_fill`, so it holds NULL. Saved rows are immutable, so the
    // row is seeded, not saved and then updated.
    let result = run(
        &long_strategy(true),
        &long_primary(&[]),
        &htf_with(&[2], &[]),
    );
    assert_eq!(result.trades.len(), 1);
    assert_eq!(result.skipped_entries.stop_beyond_fill, 0);
    let stored_hash = result.result_content_hash();

    let tmp = TempDir::new().unwrap();
    let db = migrated_db(&tmp).await;
    let pool = db.pool();
    seed_strategy_and_version(pool).await;
    let regime_breakdown = serde_json::to_string(&result.regime_breakdown).unwrap();
    sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
          engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
          funding_total, slippage_total, trade_count, wins, losses, breakeven, \
          max_win_streak, max_loss_streak, win_rate, expectancy, gross_profit, \
          gross_loss, avg_win, avg_loss, max_drawdown, regime_breakdown, \
          skipped_sub_lot, skipped_sub_notional, skipped_leverage_capped, \
          pair, primary_timeframe, primary_data_version, htf_timeframe, htf_data_version, \
          taker_fee_bps, slippage_bps, funding_config) \
         VALUES ('run-pre-0024', 'ver-1', 1, '2026-11-01T00:00:00.000Z', 'fp', 'tgt', ?1, \
                 '10000', ?2, ?3, ?4, ?5, 1, 0, 1, 0, 0, 1, '0', '0', '0', '0', '0', '0', '0', \
                 ?6, 0, 0, 0, \
                 'BTCUSDT', '15m', 'v-structure-stop', '4h', 'v-structure-stop-htf', \
                 '0', '0', 'snapshot_rates')",
    )
    .bind(&stored_hash)
    .bind(text(result.net_pnl))
    .bind(text(result.fees_total))
    .bind(text(result.funding_total))
    .bind(text(result.slippage_total))
    .bind(&regime_breakdown)
    .execute(pool)
    .await
    .expect("seed the pre-0024 run row");
    for (seq, trade) in result.trades.iter().enumerate() {
        sqlx::query(
            "INSERT INTO trade \
             (id, backtest_run_id, seq, direction, qty, entry_price, exit_price, \
              entry_signal_time, entry_fill_time, exit_signal_time, exit_fill_time, \
              fees_total, funding_total, slippage_total, realized_pnl, realized_r, \
              mfe_r, mae_r, exit_reason, source, regime, fills, stop_price, \
              setup_arm_time, setup_dip_time, setup_anchor_time) \
             VALUES (?1, 'run-pre-0024', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, \
                     ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)",
        )
        .bind(format!("trade-pre-0024-{seq}"))
        .bind(i64::try_from(seq).unwrap())
        .bind(token(&trade.direction))
        .bind(text(trade.qty))
        .bind(text(trade.entry_price))
        .bind(text(trade.exit_price))
        .bind(trade.entry_signal_time)
        .bind(trade.entry_fill_time)
        .bind(trade.exit_signal_time)
        .bind(trade.exit_fill_time)
        .bind(text(trade.fees_total))
        .bind(text(trade.funding_total))
        .bind(text(trade.slippage_total))
        .bind(text(trade.realized_pnl))
        .bind(text(trade.realized_r))
        .bind(text(trade.mfe_r))
        .bind(text(trade.mae_r))
        .bind(token(&trade.exit_reason))
        .bind(token(&trade.source))
        .bind(token(&trade.regime))
        .bind(serde_json::to_string(&trade.fills).unwrap())
        .bind(trade.stop_price.map(text))
        .bind(trade.setup.map(|trace| trace.arm_time))
        .bind(trade.setup.map(|trace| trace.dip_time))
        .bind(trade.setup.and_then(|trace| trace.anchor_time))
        .execute(pool)
        .await
        .expect("seed the pre-0024 trade row");
    }
    let column: Option<i64> =
        sqlx::query_scalar("SELECT skipped_stop_beyond_fill FROM backtest_run WHERE id = ?1")
            .bind("run-pre-0024")
            .fetch_one(pool)
            .await
            .expect("read the raw column");
    assert_eq!(column, None, "the seeded row holds NULL in the new column");

    let repo = SqliteBacktestRunRepo::new(pool.clone());
    let stored = repo
        .get_run(&BacktestRunId::new("run-pre-0024"))
        .await
        .expect("a pre-0024 row reads, so its hash re-derives unchanged")
        .expect("the seeded run exists");
    assert_eq!(stored.skipped_entries.stop_beyond_fill, 0);
    assert_eq!(stored.result_content_hash, stored_hash);
}
