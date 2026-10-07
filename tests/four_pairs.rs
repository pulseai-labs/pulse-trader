//! r4.s1.w2 — four pairs (spine demo line d62): ETHUSDT, SOLUSDT and XRPUSDT
//! backtest and walk forward exactly as BTCUSDT does, and every stored run
//! records the pair's pinned USD-M symbol filters.
//!
//! The snapshots are the certify fixture's deterministic synthetic series
//! (`application::fixture`: 16,800 M15 bars plus the H4 aggregation), committed
//! into a temporary store under each pair — the same generator
//! `tests/certify_fixture.rs` rides, so the four runs are comparable and no
//! network is touched.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{
    BacktestConfig, BacktestRequest, BacktestRunRepository, BinanceAdapter, CandleSeriesRepository,
    CandleStore, CreatedBy, Db, MIGRATOR, NewVersion, Pair, SqliteBacktestRunRepo,
    SqliteStrategyRepo, StrategyRepository, SymbolFilters, Timeframe, VersionId,
    WalkForwardRequest, WalkForwardRunRepository, fixture_h4_candles, fixture_m15_candles,
    fixture_strategy_dsl, run_version_backtest, run_walk_forward,
};
use rust_decimal::Decimal;
use tempfile::TempDir;

fn dec(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

/// `(pair, pinned filters)` — the pinned USD-M constants every stored run must
/// record. ETHUSDT/SOLUSDT/XRPUSDT are pinned from the dated `exchangeInfo`
/// (2026-10-07) + the published leverage-bracket table read beside it; BTCUSDT
/// keeps its byte-identical legacy pin (`min_notional` 100, `max_leverage` 125).
fn pinned_cases() -> Vec<(&'static str, SymbolFilters)> {
    vec![
        (
            "ETHUSDT",
            SymbolFilters {
                lot_step: dec(1, 3),       // 0.001
                min_qty: dec(1, 3),        // 0.001
                min_notional: dec(20, 0),  // 20
                max_leverage: dec(150, 0), // 150
            },
        ),
        (
            "SOLUSDT",
            SymbolFilters {
                lot_step: dec(1, 2),       // 0.01
                min_qty: dec(1, 2),        // 0.01
                min_notional: dec(5, 0),   // 5
                max_leverage: dec(100, 0), // 100
            },
        ),
        (
            "XRPUSDT",
            SymbolFilters {
                lot_step: dec(1, 1),       // 0.1
                min_qty: dec(1, 1),        // 0.1
                min_notional: dec(5, 0),   // 5
                max_leverage: dec(100, 0), // 100
            },
        ),
        (
            "BTCUSDT",
            SymbolFilters {
                lot_step: dec(1, 3),       // 0.001
                min_qty: dec(1, 3),        // 0.001
                min_notional: dec(100, 0), // 100 (BTCUSDT's pinned value, unchanged)
                max_leverage: dec(125, 0), // 125
            },
        ),
    ]
}

/// One pair's backtest and `k = 2` walk-forward over that pair's own snapshots,
/// through the same application use cases MCP, the CLI and the app call: both
/// stored rows must name the pair and its pinned filters.
async fn run_one_pair<S, R>(
    strategies: &S,
    runs: &R,
    store: &CandleStore,
    version_id: &VersionId,
    name: &str,
    expected_filters: &SymbolFilters,
) where
    S: StrategyRepository,
    R: BacktestRunRepository + WalkForwardRunRepository,
{
    let pair = Pair::new(name);

    // ---- backtest --------------------------------------------------------
    let request = BacktestRequest {
        version_id: version_id.clone(),
        pair: pair.clone(),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: Some(Timeframe::H4),
        config: BacktestConfig::default(),
        snapshots: None,
        window: None,
    };
    let outcome = run_version_backtest(strategies, store, &BinanceAdapter::new(), runs, &request)
        .await
        .unwrap_or_else(|e| panic!("{name}: a backtest must run exactly as BTCUSDT does: {e}"));
    assert!(
        !outcome.trades.is_empty(),
        "{name}: the fixture strategy trades on the fixture series"
    );
    let stored = runs
        .get_run(&outcome.run.id)
        .await
        .expect("read the saved run back")
        .expect("the saved run exists");
    let inputs = stored
        .inputs
        .as_ref()
        .expect("a fresh run records its inputs");
    assert_eq!(inputs.pair, pair, "{name}: the stored run names the pair");
    assert_eq!(
        inputs.symbol_filters,
        Some(expected_filters.clone()),
        "{name}: the stored run records the pair's pinned filters"
    );

    // ---- walk-forward ----------------------------------------------------
    let wf = run_walk_forward(
        strategies,
        store,
        &BinanceAdapter::new(),
        runs,
        &WalkForwardRequest {
            version_id: version_id.clone(),
            pair: pair.clone(),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: Some(Timeframe::H4),
            config: BacktestConfig::default(),
            snapshots: None,
            from_ms: None,
            to_ms: None,
            k: Some(2),
            rule: None,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("{name}: a walk-forward must run exactly as BTCUSDT does: {e}"));
    assert_eq!(wf.fold_summaries.len(), 2, "{name}: k=2 cuts two folds");
    let fold = runs
        .get_run(&wf.fold_summaries[0].id)
        .await
        .expect("read the fold run back")
        .expect("the fold run exists");
    let fold_inputs = fold.inputs.as_ref().expect("a fold run records its inputs");
    assert_eq!(
        fold_inputs.pair, pair,
        "{name}: the fold run names the pair"
    );
    assert_eq!(
        fold_inputs.symbol_filters,
        Some(expected_filters.clone()),
        "{name}: the fold run records the pair's pinned filters"
    );
}

/// The four-pair demo line: one backtest and one walk-forward per pair (BTCUSDT
/// riding the same path as the three new pairs), over per-pair synthetic
/// snapshots, with the pinned filters asserted off the persisted rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn four_pairs_backtest_and_walk_forward_record_their_pinned_filters() {
    let tmp_db = TempDir::new().expect("tempdir for the db");
    let db = Db::with_path(&tmp_db.path().join("pulse.db"))
        .await
        .expect("open the temp db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());

    // One version carrying the certify fixture's own strategy document — the
    // generator and the strategy are minted from the same constants, so the
    // fixture series gives it a real (synthetic) edge.
    let strategy = strategies
        .create_strategy("four pairs", Some("four-pairs-test"), &[])
        .await
        .expect("create the strategy");
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: serde_json::to_string(&fixture_strategy_dsl()).expect("serialize the DSL"),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create the version");

    // The synthetic snapshots, committed per pair (content-addressed).
    let tmp_store = TempDir::new().expect("tempdir for the store");
    let store = CandleStore::with_base_dir(tmp_store.path().join("store"));
    let m15 = fixture_m15_candles();
    let h4 = fixture_h4_candles();
    for (name, _) in pinned_cases() {
        let pair = Pair::new(name);
        store
            .commit(&pair, Timeframe::M15, m15.clone())
            .unwrap_or_else(|e| panic!("{name}: commit M15: {e}"));
        store
            .commit(&pair, Timeframe::H4, h4.clone())
            .unwrap_or_else(|e| panic!("{name}: commit H4: {e}"));
    }

    for (name, expected_filters) in pinned_cases() {
        run_one_pair(
            &strategies,
            &runs,
            &store,
            &version.id,
            name,
            &expected_filters,
        )
        .await;
    }
}
