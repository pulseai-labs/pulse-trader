//! r6.s3.w1 — R is net of fees and funding (#457, ADR-0028 amendment, PDR-0046
//! decision 2).
//!
//! A closed trade records `realized_r = realized_pnl ÷ (qty × |entry − initial
//! stop|)`: its net P&L in units of its initial risk. Three properties:
//!
//! 1. `only_r_moves_on_the_fixture` — over the committed BTCUSDT fixture the
//!    recorded R is exactly the net P&L over the initial risk, and its gap to
//!    the gross price-move R is exactly the costs over the initial risk.
//! 2. `issue_457_long_hold` — a long held across several positive-funding
//!    stamps and stopped out reads below −1R, and R and USDT agree in scale.
//! 3. `walk_forward_reads_net_r` — a walk-forward over the fixture reads each
//!    fold's R from the trades' net `realized_r`, and the repository's
//!    `derive_fold` check accepts the persisted folds.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::PathBuf;

use pulse::{
    BacktestConfig, BacktestResult, BacktestRunRepository, BinanceAdapter, Candle, CandleSeries,
    CandleStore, Comparator, CompiledStrategy, Condition, DataVersion, Db, Direction,
    ExchangeAdapter, ExitReason, ExitRule, FoldScheme, FoldVerdict, Migrator, Pair, PriceField,
    RiskParams, SchemaVersion, Series, SeriesEnd, SqliteBacktestRunRepo, SqliteStrategyRepo,
    StrategyDsl, StrategyRepository, SweepableValue, SymbolFilters, Timeframe, Trade, ValueSource,
    WalkForwardOutcome, WalkForwardRequest, WalkForwardRunRepository, compile, realized_r,
    run_backtest, run_walk_forward, validate,
};
use rust_decimal::Decimal;
use support::mcp::{FIXTURE_STORE, copy_tree, manifest, migrated_db};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixture: the strategy `tests/backtest_fixture.rs` runs
// ---------------------------------------------------------------------------

fn load_primary() -> CandleSeries {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store");
    let store = CandleStore::with_base_dir(base);
    let pair = Pair::new("BTCUSDT");
    let head = store
        .read_head(&pair, Timeframe::M15)
        .expect("read M15 HEAD")
        .expect("M15 HEAD present in fixture store");
    store
        .read_snapshot(&pair, Timeframe::M15, &head)
        .expect("read M15 snapshot")
}

fn oracle_dsl() -> StrategyDsl {
    let json =
        std::fs::read_to_string(manifest("tests/fixtures/strategies/rsi-oversold-long.json"))
            .expect("read strategy fixture json");
    Migrator::v1().load(&json).expect("load strategy").dsl
}

fn compile_dsl(dsl: &StrategyDsl) -> CompiledStrategy {
    let json = serde_json::to_string(dsl).expect("dsl serializes");
    let loaded = Migrator::v1().load(&json).expect("dsl loads");
    let validated = validate(&loaded.dsl).expect("dsl validates");
    compile(&validated).expect("dsl compiles")
}

fn run_fixture(primary: &CandleSeries) -> BacktestResult {
    let compiled = compile_dsl(&oracle_dsl());
    let filters = BinanceAdapter::new()
        .symbol_filters(&Pair::new("BTCUSDT"))
        .expect("BTCUSDT filters resolve through the port");
    run_backtest(
        &compiled,
        primary,
        None,
        None,
        &BacktestConfig::default(),
        &filters,
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("backtest runs over the fixture")
}

/// The trade's initial risk in USDT: `qty × |entry − initial stop|`.
fn initial_risk(trade: &Trade) -> Decimal {
    let stop = trade.stop_price.expect("a trade records its initial stop");
    trade.qty * (trade.entry_price - stop).abs()
}

// ---------------------------------------------------------------------------
// 1. Only R moves on the fixture
// ---------------------------------------------------------------------------

#[test]
fn only_r_moves_on_the_fixture() {
    let primary = load_primary();
    let result = run_fixture(&primary);
    assert!(
        !result.trades.is_empty(),
        "the fixture produces trades (non-vacuous)"
    );
    let mut costed = 0;
    for (i, trade) in result.trades.iter().enumerate() {
        let risk = initial_risk(trade);
        assert!(
            risk > Decimal::ZERO,
            "trade {i} has a positive initial risk"
        );
        assert_eq!(
            trade.realized_r,
            trade.realized_pnl / risk,
            "trade {i}: realized_r is realized_pnl over the initial risk"
        );
        let gross_r = realized_r(
            trade.entry_price,
            trade.exit_price,
            trade.stop_price.expect("initial stop recorded"),
            trade.direction,
        )
        .expect("a stop distance exists");
        assert_eq!(
            trade.realized_r - gross_r,
            (trade.funding_total - trade.fees_total) / risk,
            "trade {i}: the gap to the gross R is the costs and nothing else"
        );
        if trade.fees_total > Decimal::ZERO {
            costed += 1;
        }
    }
    assert_eq!(costed, result.trades.len(), "every fixture trade paid fees");
}

// ---------------------------------------------------------------------------
// 2. Issue #457: a long that sat through funding
// ---------------------------------------------------------------------------

fn dec_m(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

fn hand_candle(i: i64, open: i64, high: i64, low: i64, close: i64) -> Candle {
    let t = i * Timeframe::M15.duration_ms();
    Candle {
        open_time: t,
        close_time: t + Timeframe::M15.duration_ms() - 1,
        open: dec_m(open, 0),
        high: dec_m(high, 0),
        low: dec_m(low, 0),
        close: dec_m(close, 0),
        volume: dec_m(1, 0),
        // A positive rate on every 8h stamp: a long pays it.
        funding_rate: if t % 28_800_000 == 0 {
            Some(dec_m(1, 3))
        } else {
            None
        },
    }
}

fn always_long_with_five_percent_stop() -> CompiledStrategy {
    let dsl = StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "net-r issue 457".to_owned(),
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
            distance_pct: SweepableValue::Fixed(dec_m(5, 2)),
        }],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(dec_m(1, 2)),
            max_leverage: SweepableValue::Fixed(dec_m(3, 0)),
        },
        setup: None,
    };
    compile(&validate(&dsl).expect("fixture validates")).expect("fixture compiles")
}

#[test]
fn issue_457_long_hold() {
    // Flat at 100 for 100 bars (stamps at bars 32, 64 and 96 fall inside the
    // hold), then one bar that trades down through the 95 stop. The series
    // ends on that bar, so the run holds exactly one trade.
    let mut candles: Vec<Candle> = (0..100)
        .map(|i| hand_candle(i, 100, 100, 100, 100))
        .collect();
    candles.push(hand_candle(100, 100, 100, 90, 90));
    let primary = CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe: Timeframe::M15,
        version: DataVersion::new("v-net-r-issue-457"),
        candles,
    };
    let config = BacktestConfig {
        starting_equity: dec_m(10_000, 0),
        taker_fee_bps: dec_m(4, 0),
        slippage_bps: dec_m(0, 0),
    };

    let result = run_backtest(
        &always_long_with_five_percent_stop(),
        &primary,
        None,
        None,
        &config,
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("the hand-built run completes");

    assert_eq!(result.trades.len(), 1, "one trade");
    let trade = &result.trades[0];
    assert_eq!(trade.exit_reason, ExitReason::StopLoss, "stopped out");
    assert!(
        trade.funding_total < Decimal::ZERO,
        "the long paid funding across the hold: {}",
        trade.funding_total
    );
    assert!(
        trade.realized_r < Decimal::NEGATIVE_ONE,
        "a stop-out that paid funding and fees reads below -1R, got {}",
        trade.realized_r
    );
    assert_eq!(
        trade.realized_r * initial_risk(trade),
        trade.realized_pnl,
        "R and USDT agree in sign and scale"
    );
}

// ---------------------------------------------------------------------------
// 3. The walk-forward reads net R
// ---------------------------------------------------------------------------

struct World {
    _tmp: TempDir,
    _db: Db,
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
        _db: db,
        strategies,
        store: CandleStore::with_base_dir(store_dir),
        runs,
    }
}

async fn run_wf(world: &World) -> WalkForwardOutcome {
    let strategy = world
        .strategies
        .create_strategy("net-r", None, &[])
        .await
        .expect("create strategy");
    let version = world
        .strategies
        .create_version(pulse::NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(&oracle_dsl()).expect("dsl serializes"),
            created_by: pulse::CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version")
        .id;
    let request = WalkForwardRequest {
        version_id: version,
        pair: Pair::new("BTCUSDT"),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: None,
        config: BacktestConfig::default(),
        snapshots: None,
        from_ms: None,
        to_ms: None,
        k: None,
        rule: None,
    };
    // The repository's `derive_fold` re-derives every fold verdict from the
    // trades' stored R on this write: a mismatch refuses the run here.
    run_walk_forward(
        &world.strategies,
        &world.store,
        &BinanceAdapter::new(),
        &world.runs,
        &request,
        None,
    )
    .await
    .expect("the walk-forward completes over the fixture")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walk_forward_reads_net_r() {
    let world = world().await;
    let outcome = run_wf(&world).await;
    let wf = &outcome.run;
    assert_eq!(wf.scheme, FoldScheme::RollingOos { k: 6 });

    let mut trade_count = 0;
    for fold in &wf.folds {
        let trades = world
            .runs
            .get_trades(&fold.backtest_run_id)
            .await
            .expect("fold trades read");
        for trade in &trades {
            assert_eq!(
                trade.realized_r,
                trade.realized_pnl / initial_risk(trade),
                "fold {}: a stored trade carries its net R",
                fold.index
            );
        }
        let rs: Vec<Decimal> = trades.iter().map(|t| t.realized_r).collect();
        assert_eq!(
            fold.verdict,
            FoldVerdict::from_rs(&rs),
            "fold {}: the verdict reads the trades' net R",
            fold.index
        );
        trade_count += trades.len();
    }
    assert!(
        trade_count > 0,
        "the fixture trades inside the span (non-vacuous)"
    );

    // `derive_fold` accepted the persisted folds on write; the read re-derives
    // them again and returns the same run.
    let read = world
        .runs
        .get_walk_forward_run(&wf.id)
        .await
        .expect("walk-forward run reads")
        .expect("the saved run exists");
    assert_eq!(&read, wf, "the persisted folds read back unchanged");
}
