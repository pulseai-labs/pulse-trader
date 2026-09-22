//! r2.s3.w2 — AC-1: full-history lead-in for windowed runs.
//!
//! A windowed run loads both series as `[snapshot_start, to)`: every candle
//! before `from` is **lead-in** — both indicator engines step it, the HTF
//! engine steps each closed H4 bar — while entries, fills, exits, funding,
//! equity marks and `PnL` count only on bars with `open_time >= from`. The
//! ruling-1 shape ("`window` slices the series to `[from, to)`; indicators warm
//! up inside the window") is what this suite supersedes.
//!
//! The seven properties, over the committed one-month BTCUSDT fixture
//! (M15 primary + H4 higher timeframe):
//!
//!   i. An M15 EMA(50) strategy windowed 60 bars in fires on the same first
//!      bars the unwindowed run fires on in that slice — the warm gate is
//!      already true at `from`.
//!  ii. An `htf` EMA(20) strategy windowed 100 H4 bars in fires inside the
//!      window, where the ruling-1 slice could not warm the operand.
//! iii. No trade fills before `from`; none fills at the first counted bar's
//!      open (a lead-in signal cannot produce a fill — a4).
//!  iv. A window covering the whole snapshot persists the same
//!      `result_content_hash` as the unwindowed run.
//!   v. `from` = snapshot start records `lead_in_from_ms == from_ms`.
//!  vi. Two cold windowed runs over the same inputs are byte-identical.
//! vii. For a window at which the unwindowed run is flat at both ends, the
//!      windowed run's trades equal the unwindowed run's trades whose entry
//!      fill is `>= from` and exit `< to` — selected from the fixture, found
//!      asserted.
//!
//! Engine-granularity pins (ruling (g): no entry evaluation, pending entry,
//! fill, exit, funding accrual, equity mark, excursion update or regime tally
//! on a lead-in bar) are the `engine_*` tests at the bottom.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{
    BacktestConfig, BacktestOutcome, BacktestRequest, BinanceAdapter, CandleSeries, CandleStore,
    CandleWindow, Comparator, CompiledStrategy, Condition, CreatedBy, Db, Direction,
    ExchangeAdapter, ExitRule, IndicatorSpec, Migrator, NewVersion, Pair, PriceField, RiskParams,
    SchemaVersion, Series, SeriesEnd, SqliteBacktestRunRepo, SqliteStrategyRepo, StrategyDsl,
    StrategyRepository, SweepableValue, Timeframe, Trade, ValueSource, VersionId, compile,
    run_backtest, run_version_backtest, validate,
};
use rust_decimal::Decimal;
use serde_json::json;
use support::mcp::{FIXTURE_STORE, MINIMAL_DSL, copy_tree, manifest, migrated_db};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn fixed(v: i64, scale: u32) -> SweepableValue<Decimal> {
    SweepableValue::Fixed(Decimal::new(v, scale))
}

/// M15 EMA(50): entry `ema(50) < close` — 50 bars of primary history to warm.
fn ema50_dsl() -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion {
            major: 1,
            minor: 0,
            patch: 0,
        },
        name: "EMA50 Long (lead-in)".to_owned(),
        direction: Direction::Long,
        entry: Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Ema {
                    period: SweepableValue::Fixed(50),
                },
            },
            op: Comparator::Lt,
            rhs: ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            },
        },
        filters: vec![],
        exits: vec![
            ExitRule::StopLoss {
                distance_pct: fixed(5, 2),
            },
            ExitRule::TakeProfit {
                target_r: fixed(2, 0),
            },
        ],
        risk: RiskParams {
            risk_per_trade_pct: fixed(1, 2),
            max_leverage: fixed(3, 0),
        },
    }
}

/// `htf` EMA(20): entry `htf.ema(20) < close` — 20 closed H4 bars to warm.
fn htf_ema20_dsl() -> StrategyDsl {
    let mut dsl = ema50_dsl();
    dsl.schema_version = SchemaVersion::CURRENT;
    "HTF EMA20 Long (lead-in)".clone_into(&mut dsl.name);
    dsl.entry = Condition::Compare {
        lhs: ValueSource::Indicator {
            series: Series::Htf,
            spec: IndicatorSpec::Ema {
                period: SweepableValue::Fixed(20),
            },
        },
        op: Comparator::Lt,
        rhs: ValueSource::Price {
            series: Series::Primary,
            field: PriceField::Close,
        },
    };
    dsl
}

/// Always-true entry (`close > 0`), no indicators — warm on the very first bar
/// it is evaluated, so any fill before `from` can only come from a lead-in
/// bar being counted. Used by the `engine_*` ruling-(g) pins below.
fn always_entry_dsl() -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "Always Long (lead-in gate)".to_owned(),
        direction: Direction::Long,
        entry: Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            },
            op: Comparator::Gt,
            rhs: ValueSource::Constant {
                value: Decimal::ZERO,
            },
        },
        filters: vec![],
        exits: vec![ExitRule::StopLoss {
            distance_pct: fixed(5, 2),
        }],
        risk: RiskParams {
            risk_per_trade_pct: fixed(1, 2),
            max_leverage: fixed(3, 0),
        },
    }
}

fn compile_dsl(dsl: &StrategyDsl) -> CompiledStrategy {
    let json = serde_json::to_string(dsl).expect("dsl serializes");
    let loaded = Migrator::v1().load(&json).expect("dsl loads");
    let validated = validate(&loaded.dsl).expect("dsl validates");
    compile(&validated).expect("dsl compiles")
}

// ---------------------------------------------------------------------------
// Fixture + world
// ---------------------------------------------------------------------------

/// The committed store's series, read in place (read-only — window selection
/// and the ruling-1 emulation need the real candles).
fn fixture_series(tf: Timeframe) -> CandleSeries {
    let store = CandleStore::with_base_dir(manifest(FIXTURE_STORE));
    let head = store
        .read_head(&Pair::new("BTCUSDT"), tf)
        .expect("read HEAD")
        .expect("fixture HEAD present");
    store
        .read_snapshot(&Pair::new("BTCUSDT"), tf, &head)
        .expect("read fixture snapshot")
}

/// A migrated db + a COPY of the fixture store + the two repos
/// `run_version_backtest` needs. The store is copied by convention: nothing
/// here commits to it, but the fixture is shared and the copy keeps that
/// invariant obvious.
struct World {
    _tmp: TempDir,
    #[allow(dead_code)]
    db: Db,
    strategies: SqliteStrategyRepo<pulse::SystemClock>,
    store: CandleStore,
    runs: SqliteBacktestRunRepo<pulse::SystemClock>,
}

async fn world() -> World {
    let tmp = TempDir::new().unwrap();
    let (_path, db) = migrated_db(&tmp).await;
    let store_dir = tmp.path().join("candles");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    World {
        _tmp: tmp,
        db,
        strategies,
        store: CandleStore::with_base_dir(store_dir),
        runs,
    }
}

async fn make_version(world: &World, dsl: &StrategyDsl) -> VersionId {
    let strategy = world
        .strategies
        .create_strategy("lead-in", None, &[])
        .await
        .expect("create strategy");
    world
        .strategies
        .create_version(NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(dsl).expect("dsl serializes"),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version")
        .id
}

async fn run(
    world: &World,
    version: &VersionId,
    htf: bool,
    window: Option<CandleWindow>,
) -> BacktestOutcome {
    run_version_backtest(
        &world.strategies,
        &world.store,
        &BinanceAdapter::new(),
        &world.runs,
        &BacktestRequest {
            version_id: version.clone(),
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: htf.then_some(Timeframe::H4),
            config: BacktestConfig::default(),
            snapshots: None,
            window,
        },
    )
    .await
    .expect("the run completes over the fixture")
}

/// RFC 3339 (ms precision — the `created_at` column's convention) of an
/// epoch-ms candle bound.
fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .expect("a real candle ms")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// ---------------------------------------------------------------------------
// (i) the warm gate is already true at `from`
// ---------------------------------------------------------------------------

/// Windowed deep into the snapshot, the EMA(50) run's in-window entry signals
/// are exactly the unwindowed run's in-window entry signals: the engine
/// arrives at `from` with the same indicator state, not a cold EMA(50)
/// re-warming on the slice.
///
/// `from` must be a bar where the UNWINDOWED run is flat — otherwise its
/// still-open position suppresses signals the windowed run (always flat at
/// `from`) is free to take, and the difference is position state, not
/// warm-up. `from` = the candle whose open is the first trade's exit fill:
/// the position closed at that bar's open, so both runs stand flat AND warm
/// there — the windowed run having warmed on the same bars as lead-in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_gate_is_already_true_at_from() {
    let world = world().await;
    let version = make_version(&world, &ema50_dsl()).await;

    let m15 = fixture_series(Timeframe::M15);
    let unwindowed = run(&world, &version, false, None).await;
    assert!(
        !unwindowed.trades.is_empty(),
        "the fixture must produce trades for the probe to mean anything"
    );

    // The first candle whose open is the first trade's exit fill: the
    // unwindowed run is provably flat from this bar on (the exit filled at
    // its open). Well past the EMA(50) cold span, so both runs are warm.
    let first_exit = unwindowed.trades[0].exit_fill_time;
    let from = m15
        .candles
        .iter()
        .find(|c| c.open_time >= first_exit)
        .expect("the exit fill bar exists in the series")
        .open_time;
    let window = CandleWindow::new(from, m15.candles.last().unwrap().open_time + 1).unwrap();

    let windowed = run(&world, &version, false, Some(window.clone())).await;

    // Signals in the counted slice = signals on bars with open_time >= from —
    // `entry_signal_time` is the signal bar's close_time; a lead-in bar's
    // close is < from, so `>= from` selects exactly the counted bars.
    let unwindowed_signals: Vec<i64> = unwindowed
        .trades
        .iter()
        .filter(|t| t.entry_signal_time >= window.from_ms)
        .map(|t| t.entry_signal_time)
        .collect();
    let windowed_signals: Vec<i64> = windowed
        .trades
        .iter()
        .map(|t| t.entry_signal_time)
        .collect();

    assert!(
        !unwindowed_signals.is_empty(),
        "the fixture must produce in-window signals for the probe to mean anything"
    );
    assert_eq!(
        windowed_signals, unwindowed_signals,
        "flat and identically warm at `from`, the windowed run must fire on the \
         same bars the unwindowed run fires on — a cold re-warm shifts the \
         whole sequence"
    );
}

// ---------------------------------------------------------------------------
// (ii) the HTF engine warms on lead-in
// ---------------------------------------------------------------------------

/// `htf` EMA(20) with `from` 100+ H4 bars in: the lead-in run signals inside
/// the cold span (the first 20 closed H4 bars after `from`), where ruling 1 —
/// both series sliced to `[from, to)` — cannot warm the operand at all.
///
/// The window chosen here is SHORTER than the cold span (a single fixture
/// trade's span), so the ruling-1 emulation fires nothing in the whole window —
/// not merely nothing early.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn htf_engine_warms_on_lead_in() {
    let world = world().await;
    let version = make_version(&world, &htf_ema20_dsl()).await;

    let m15 = fixture_series(Timeframe::M15);
    let h4 = fixture_series(Timeframe::H4);
    assert!(h4.candles.len() > 130, "fixture needs >130 H4 bars");

    let unwindowed = run(&world, &version, true, None).await;

    // Select the window from the fixture: the first unwindowed trade whose
    // signal bar sits >= 100 H4 bars in. `from` is the H4 open aligned to that
    // signal's span start so the signal lands inside the cold span — the first
    // 20 closed H4 bars after `from`, over which ruling-1's htf EMA(20) can
    // never warm. Asserting a candidate was found is part of the claim.
    let mut selected: Option<(i64, i64)> = None;
    for t in &unwindowed.trades {
        // H4 index of the signal bar (its open is the close minus a bar).
        let signal_bar_open = t.entry_signal_time - Timeframe::M15.duration_ms() + 1;
        let signal_h4 = h4
            .candles
            .iter()
            .rposition(|c| c.open_time <= signal_bar_open)
            .expect("the signal bar sits in some H4 bar");
        if signal_h4 < 100 || signal_h4 + 19 >= h4.candles.len() {
            continue;
        }
        // from = open of the H4 bar containing the signal — >= 100 H4 bars in —
        // to = open of H4 bar (signal_h4 + 19): the window covers exactly the
        // first 19 cold H4 bars, strictly shorter than the 20-bar cold span.
        let from = h4.candles[signal_h4].open_time;
        let to = h4.candles[signal_h4 + 19].open_time;
        // The signal bar is counted (open >= from) and its fill bar is counted
        // (open = signal_open + 15m < to): the probe is observable in-window.
        if t.entry_fill_time < to {
            selected = Some((from, to));
            break;
        }
    }
    let (from, to) = selected.expect("the fixture must yield a trade signalling >= 100 H4 bars in");
    let window = CandleWindow::new(from, to).unwrap();

    let windowed = run(&world, &version, true, Some(window.clone())).await;

    // The whole window sits inside the cold span, so ANY in-window evidence —
    // a closed trade or the still-open position's mark — is a signal that could
    // only exist because the htf engine arrived at `from` warm.
    let in_window_signal = |signal_time: i64| signal_time >= from && signal_time < to;
    let fired = windowed
        .trades
        .iter()
        .any(|t| in_window_signal(t.entry_signal_time))
        || windowed
            .run
            .open_position
            .as_ref()
            .is_some_and(|m| in_window_signal(m.entry_signal_time));
    assert!(
        fired,
        "the lead-in run must produce an in-window signal — the htf EMA(20) is \
         warm at `from`; ruling-1 cannot warm it in-window"
    );

    // Ruling-1 emulation: both series sliced to `[from, to)` — the shape every
    // windowed run took before this item. EMA(20) cannot warm on fewer than 20
    // closed H4 bars, so the emulated run fires nothing in the whole window.
    let compiled = compile_dsl(&htf_ema20_dsl());
    let filters = BinanceAdapter::new()
        .symbol_filters(&Pair::new("BTCUSDT"))
        .expect("filters resolve");
    let emulated = run_backtest(
        &compiled,
        &m15.windowed(&window),
        Some(&h4.windowed(&window)),
        &BacktestConfig::default(),
        &filters,
        SeriesEnd::WindowEdge,
        None,
    )
    .expect("the emulation runs");
    assert!(
        emulated.trades.is_empty() && emulated.open_position.is_none(),
        "the ruling-1 emulation must fire nothing — the window is shorter than \
         the htf EMA(20) cold span"
    );
}

// ---------------------------------------------------------------------------
// (iii) nothing counts before `from` (a4)
// ---------------------------------------------------------------------------

/// Every fill and signal in a windowed run belongs to counted bars: no entry
/// fill before `from`, and none at the first counted bar's open — a lead-in
/// signal cannot produce a fill, so the earliest legal fill is the SECOND
/// counted bar's open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_fills_or_signals_before_from() {
    let world = world().await;
    let version = make_version(&world, &ema50_dsl()).await;

    let m15 = fixture_series(Timeframe::M15);
    let from = m15.candles[500].open_time;
    let to = m15.candles[2000].open_time;
    let window = CandleWindow::new(from, to).unwrap();
    let first_counted_open = m15
        .candles
        .iter()
        .find(|c| c.open_time >= from)
        .unwrap()
        .open_time;

    let windowed = run(&world, &version, false, Some(window)).await;
    assert!(!windowed.trades.is_empty(), "fixture must trade in-window");
    for t in &windowed.trades {
        assert!(
            t.entry_signal_time >= from && t.entry_fill_time >= from,
            "trade {t:?} has a pre-from signal or fill"
        );
        assert!(
            t.entry_fill_time > first_counted_open,
            "a fill at the first counted open would mean a lead-in signal produced it"
        );
        assert!(
            t.exit_fill_time < to,
            "a trade must close inside the window"
        );
    }
    if let Some(mark) = &windowed.run.open_position {
        assert!(
            mark.entry_signal_time >= from && mark.entry_fill_time >= from,
            "an open-position mark must also originate on counted bars"
        );
    }
}

// ---------------------------------------------------------------------------
// (iv) a whole-snapshot window is the unwindowed run
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whole_snapshot_window_matches_unwindowed() {
    let world = world().await;
    let version = make_version(&world, &ema50_dsl()).await;

    let m15 = fixture_series(Timeframe::M15);
    let window = CandleWindow::new(
        m15.candles.first().unwrap().open_time,
        m15.candles.last().unwrap().open_time + 1,
    )
    .unwrap();

    let unwindowed = run(&world, &version, false, None).await;
    let windowed = run(&world, &version, false, Some(window)).await;

    assert_eq!(
        windowed.run.result_content_hash, unwindowed.run.result_content_hash,
        "a window covering the snapshot must produce the unwindowed result bytes"
    );
    assert_eq!(windowed.trades, unwindowed.trades);
}

// ---------------------------------------------------------------------------
// (v) the lead-in start is recorded
// ---------------------------------------------------------------------------

/// `from` = the snapshot's first candle: the run still records `lead_in_from` —
/// the `open_time` of the first candle the engine consumed — equal to `from_ms`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lead_in_from_is_recorded() {
    let world = world().await;
    let version = make_version(&world, &ema50_dsl()).await;

    let m15 = fixture_series(Timeframe::M15);
    let first_open = m15.candles.first().unwrap().open_time;
    let window = CandleWindow::new(first_open, m15.candles.last().unwrap().open_time + 1).unwrap();

    let outcome = run(&world, &version, false, Some(window.clone())).await;

    let inputs = serde_json::to_value(&outcome.inputs).expect("inputs serialize");
    let lead_in = inputs
        .as_object()
        .expect("inputs is an object")
        .get("lead_in_from")
        .unwrap_or_else(|| panic!("inputs must record lead_in_from: {inputs}"));
    assert_eq!(
        *lead_in,
        json!(rfc3339(first_open)),
        "from == snapshot start records lead_in_from == from"
    );
    // The typed field agrees with the wire spelling.
    assert_eq!(outcome.inputs.lead_in_from_ms, Some(first_open));
    assert_eq!(outcome.inputs.window, Some(window));
}

// ---------------------------------------------------------------------------
// (vi) determinism
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_cold_windowed_runs_are_byte_identical() {
    let world = world().await;
    let version = make_version(&world, &ema50_dsl()).await;

    let m15 = fixture_series(Timeframe::M15);
    let window = CandleWindow::new(m15.candles[60].open_time, m15.candles[2000].open_time).unwrap();

    let a = run(&world, &version, false, Some(window.clone())).await;
    let b = run(&world, &version, false, Some(window)).await;

    assert_eq!(a.run.result_content_hash, b.run.result_content_hash);
    assert_eq!(a.trades, b.trades);
}

// ---------------------------------------------------------------------------
// (vii) windowed trades equal the unwindowed run's in-window trades
// ---------------------------------------------------------------------------

/// Select a window at which the unwindowed run is FLAT at both `from` and `to`,
/// starting before the first trade (so the equity base — and therefore qty —
/// is identical on both sides). The windowed run's trade log must then equal
/// the unwindowed run's trades with entry fill `>= from` and exit `< to`,
/// field for field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windowed_trades_equal_unwindowed_in_window_subset() {
    let world = world().await;
    // The RSI strategy trades sparsely enough that flat gaps between trades
    // exist to choose `to` in.
    let dsl: StrategyDsl = serde_json::from_str(MINIMAL_DSL).expect("MINIMAL_DSL parses");
    let version = make_version(&world, &dsl).await;

    let m15 = fixture_series(Timeframe::M15);
    let unwindowed = run(&world, &version, false, None).await;
    assert!(
        unwindowed.trades.len() >= 2,
        "the fixture must produce multiple trades to window around"
    );

    // Flat-at-t predicate: no trade holds a position across instant `t` — no
    // trade with entry_fill <= t < exit_fill (exit_fill == t fills at the bar
    // opening at t: the position was open through that bar).
    let flat_at = |trades: &[Trade], t: i64| {
        !trades
            .iter()
            .any(|x| x.entry_fill_time <= t && x.exit_fill_time > t)
    };

    // `from` before the first trade's signal bar — the engine counts the bar
    // that produced the signal, so `from` must be <= that bar's open.
    let first_signal_bar_open =
        unwindowed.trades[0].entry_signal_time - Timeframe::M15.duration_ms() + 1;
    let from = m15
        .candles
        .iter()
        .rev()
        .find(|c| c.open_time <= first_signal_bar_open)
        .expect("a candle at or before the first signal bar")
        .open_time;
    assert!(flat_at(&unwindowed.trades, from), "flat at `from`");

    // `to` = the first candle open after trades[k]'s exit-fill bar, for the
    // smallest k leaving a flat gap before the next entry.
    let mut selected: Option<(usize, i64)> = None;
    for (k, trade) in unwindowed.trades.iter().enumerate() {
        let after_exit = m15
            .candles
            .iter()
            .find(|c| c.open_time > trade.exit_fill_time)
            .map(|c| c.open_time);
        if let Some(t) = after_exit
            && flat_at(&unwindowed.trades, t)
        {
            selected = Some((k, t));
            break;
        }
    }
    let (k, to) = selected.expect("a flat window end exists in the fixture");
    let window = CandleWindow::new(from, to).unwrap();

    let windowed = run(&world, &version, false, Some(window)).await;
    let expected: Vec<Trade> = unwindowed
        .trades
        .iter()
        .filter(|t| t.entry_fill_time >= from && t.exit_fill_time < to)
        .cloned()
        .collect();
    assert_eq!(
        expected.len(),
        k + 1,
        "the flat window must contain exactly trades 0..={k}"
    );
    assert_eq!(
        windowed.trades, expected,
        "with the same equity base, the windowed run's trades are the \
         unwindowed run's in-window trades, field for field"
    );
    assert!(
        windowed.run.open_position.is_none(),
        "flat at `to` on the unwindowed run must mean flat at `to` on the windowed run"
    );
}

// ---------------------------------------------------------------------------
// ruling-(g): engine-granularity lead-in gates
// ---------------------------------------------------------------------------

/// A direct `run_backtest` over the FULL series with `count_from_ms` mid-way —
/// no application slice involved, so every assertion below isolates the
/// engine's own gate.
fn engine_run(
    dsl: &StrategyDsl,
    count_from_ms: Option<i64>,
    series_end: SeriesEnd,
) -> pulse::BacktestResult {
    let m15 = fixture_series(Timeframe::M15);
    let compiled = compile_dsl(dsl);
    let filters = BinanceAdapter::new()
        .symbol_filters(&Pair::new("BTCUSDT"))
        .expect("filters resolve");
    run_backtest(
        &compiled,
        &m15,
        None,
        &BacktestConfig::default(),
        &filters,
        series_end,
        count_from_ms,
    )
    .expect("the engine run completes")
}

/// (g) the first eligible candle COUNTS: an always-true entry signals on the
/// very first counted bar (there are no indicators to warm, so the signal is
/// immediate) and fills at the SECOND counted bar's open — proof both that the
/// first `open_time >= from` bar is evaluated and that no pending entry made on
/// a lead-in bar can fill at `from`'s open.
#[test]
fn engine_counts_the_first_eligible_bar_and_nothing_earlier() {
    let m15 = fixture_series(Timeframe::M15);
    let from = m15.candles[500].open_time;

    let windowed = engine_run(&always_entry_dsl(), Some(from), SeriesEnd::SnapshotEnd);
    assert!(
        !windowed.trades.is_empty(),
        "the always-entry run must trade once counting starts"
    );
    let first = &windowed.trades[0];
    assert_eq!(
        first.entry_signal_time, m15.candles[500].close_time,
        "the first counted bar signals immediately — its close is the signal"
    );
    assert_eq!(
        first.entry_fill_time, m15.candles[501].open_time,
        "the fill lands on the SECOND counted bar's open — a lead-in signal \
         would have filled at `from`'s open"
    );
    assert!(
        windowed
            .trades
            .iter()
            .all(|t| t.entry_signal_time >= from && t.entry_fill_time >= from),
        "no signal or fill references a lead-in bar"
    );

    // Control: unbounded counting signals on bar index 1 (the `index > 0`
    // gate), so the boundary is `from`, not "the second bar of the input".
    let unwindowed = engine_run(&always_entry_dsl(), None, SeriesEnd::SnapshotEnd);
    assert_eq!(
        unwindowed.trades[0].entry_signal_time, m15.candles[1].close_time,
        "the unwindowed control fires one bar later than index 0"
    );
}

/// (g) lead-in bars leave no mark on the run's per-bar outputs: the equity
/// curve opens at the first COUNTED candle's open — not the snapshot's — and
/// no curve point predates `from`.
#[test]
fn engine_equity_curve_opens_at_the_first_counted_candle() {
    let m15 = fixture_series(Timeframe::M15);
    let from = m15.candles[500].open_time;

    let windowed = engine_run(&always_entry_dsl(), Some(from), SeriesEnd::SnapshotEnd);
    assert_eq!(
        windowed.equity_curve.0.first().map(|p| p.time_ms),
        Some(from),
        "the leading equity point is the first counted candle, not the snapshot's first"
    );
    assert!(
        windowed.equity_curve.0.iter().all(|p| p.time_ms >= from),
        "no equity mark exists on a lead-in bar"
    );

    let unwindowed = engine_run(&always_entry_dsl(), None, SeriesEnd::SnapshotEnd);
    assert_eq!(
        unwindowed.equity_curve.0.first().map(|p| p.time_ms),
        Some(m15.candles[0].open_time),
        "the unwindowed control opens at the snapshot's first candle"
    );
}

/// (g) a position held when the counted span ends is the strategy's open
/// position — `WindowEdge` never fabricates a close — and the run-level tallies
/// (`regime_breakdown`, `skipped_entries`, the funding total) cover exactly the
/// counted trades: a lead-in bar can add nothing to any of them.
#[test]
fn engine_window_edge_marks_and_aggregates_cover_only_counted_trades() {
    let m15 = fixture_series(Timeframe::M15);

    // A `count_from` three bars from the end: the first counted bar signals,
    // the second fills, and the position is necessarily still open at the
    // last — which `WindowEdge` must MARK, never close as a fabricated trade.
    // (`WindowEdge` on the full series is the engine-side shape a
    // `[start, to)` slice presents.)
    let tail_from = m15.candles[m15.candles.len() - 3].open_time;
    let tail = engine_run(&always_entry_dsl(), Some(tail_from), SeriesEnd::WindowEdge);
    let mark = tail
        .open_position
        .as_ref()
        .expect("a strategy still holding at a window edge is marked, not closed");
    assert!(
        mark.entry_fill_time >= tail_from && mark.entry_signal_time >= tail_from,
        "the mark's position opened on counted bars only"
    );
    assert!(
        tail.trades.is_empty(),
        "two counted bars leave no room for a closed trade — none is invented"
    );

    // A mid-series `from` with thousands of counted bars behind it: the
    // run-level tallies cover exactly the counted trades — a lead-in bar
    // contributing a regime, an excursion, or a funding accrual would break
    // one of these equalities.
    let from = m15.candles[500].open_time;
    let windowed = engine_run(&always_entry_dsl(), Some(from), SeriesEnd::SnapshotEnd);
    assert!(
        !windowed.trades.is_empty(),
        "the mid-series windowed run must trade"
    );
    let mut breakdown = pulse::RegimeBreakdown::new();
    for trade in &windowed.trades {
        breakdown.record(trade.regime, trade.realized_pnl);
    }
    assert_eq!(
        windowed.regime_breakdown, breakdown,
        "the regime tally covers exactly the counted trades"
    );
    assert_eq!(
        windowed.funding_total,
        windowed
            .trades
            .iter()
            .map(|t| t.funding_total)
            .sum::<Decimal>(),
        "the run's funding total is only what counted trades accrued"
    );
}
