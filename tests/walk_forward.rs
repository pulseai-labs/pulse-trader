//! r2.s3.w3 — AC-1: walk-forward as a run kind over the one-month fixture.
//!
//! `rolling-oos/v1` cuts the counted span `[from, to)` into K equal-length
//! contiguous half-open folds (the last absorbs the remainder); every fold runs
//! as an ordinary persisted windowed backtest with full-history lead-in (w2);
//! the walk-forward run, its K `walk_forward_fold` rows and its K fold
//! `backtest_run` rows persist in one transaction; `wf-v1` judges each fold
//! (`n >= 20` and the expectancy lower bound in R strictly above zero) and the
//! run (`ceil(2K/3)` folds hold and the pooled lower bound is above zero).
//!
//! The ten properties the spec's AC-1 lists, over the committed one-month
//! BTCUSDT M15 fixture (the canonical `rsi-oversold-long` oracle):
//!
//!   i. A default request (K=6, no `from`/`to`) persists one `walk_forward_run`,
//!      six `walk_forward_fold` rows and six `backtest_run` rows carrying
//!      `walk_forward_run_id` + `fold_index` 0..5; the fold windows are
//!      contiguous, equal in length except the remainder in the last, and
//!      union exactly to `[span.from, span.to)`; every fold run is windowed
//!      with `lead_in_from_ms` at the snapshot's first candle; the run names
//!      `rolling-oos/v1` and `wf-v1`.
//!  ii. Every persisted fold verdict equals `FoldVerdict` recomputed from that
//!      fold run's trades, and the run verdict equals `RunVerdict` recomputed
//!      from the folds (the pass path itself is AC-2's synthetic job: the
//!      fixture's real per-fold counts are too small to hold).
//! iii. `span.from_ms` equals `first_fully_warm_bar_ms` over the same snapshot
//!      and `from_defaulted` is true; a second run handed that value
//!      explicitly is fold-for-fold byte-identical with `from_defaulted`
//!      false.
//!  iv. An explicit `from` one primary bar earlier refuses as `FromBeforeWarm`
//!      naming the earliest allowed.
//!   v. K=1 and K=13 refuse as `KOutOfRange`.
//!  vi. A request whose last fold holds no counted candle refuses BEFORE any
//!      fold runs and leaves every table's row count unchanged (a9).
//! vii. Two cold runs persist identical per-fold `result_content_hash` values
//!      and identical fold and run verdicts (a10).
//!viii. The fold runs are listed by `list_runs_for_version` and read by
//!      `get_run` with window, lead-in and walk-forward membership in their
//!      provenance (L8).
//!  ix. `get_walk_forward_run` round-trips every field.
//!   x. One fold's `lower_bound` recomputed 100x from the same trades is
//!      bit-identical (`f64::to_bits`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod coach_support;
mod support;

use pulse::{
    BacktestConfig, BacktestRequest, BacktestRunRepository, BinanceAdapter, CandleSeries,
    CandleStore, CandleWindow, CompiledStrategy, Db, FakeClock, FoldScheme, FoldVerdict, Migrator,
    Pair, RunVerdict, SqliteBacktestRunRepo, SqliteStrategyRepo, StrategyDsl, StrategyRepository,
    Timeframe, VersionId, WalkForwardAppError, WalkForwardError, WalkForwardMembership,
    WalkForwardOutcome, WalkForwardRequest, WalkForwardRunRepository, compile,
    first_fully_warm_bar_ms, fold_windows, run_version_backtest, run_walk_forward, validate,
};
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use support::mcp::{FIXTURE_STORE, copy_tree, manifest, migrated_db, seeded_walk_forward_draft};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixture + world (the windowed_lead_in shape)
// ---------------------------------------------------------------------------

/// M7b pin (verifier correction): the oracle's first fully-warm bar as a
/// hard-coded golden — measured once by stepping the fixture (see report §6),
/// never recomputed by the function under test. `rsi-oversold-long` is RSI(14)
/// on primary M15: the entry gate warms on the first bar whose RSI value
/// exists — the 15th candle (index 14) of the 2976-candle snapshot.
const ORACLE_FIRST_WARM_MS: i64 = 1_735_702_200_000;

/// The canonical oracle: `tests/fixtures/strategies/rsi-oversold-long.json` —
/// RSI(14) on M15, warm after ~15 primary bars, ~6 trades over the month.
fn oracle_dsl() -> StrategyDsl {
    let json =
        std::fs::read_to_string(manifest("tests/fixtures/strategies/rsi-oversold-long.json"))
            .expect("rsi fixture reads");
    Migrator::v1().load(&json).expect("dsl loads").dsl
}

fn compile_dsl(dsl: &StrategyDsl) -> CompiledStrategy {
    let json = serde_json::to_string(dsl).expect("dsl serializes");
    let loaded = Migrator::v1().load(&json).expect("dsl loads");
    let validated = validate(&loaded.dsl).expect("dsl validates");
    compile(&validated).expect("dsl compiles")
}

/// The committed store's series, read in place.
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

struct World {
    _tmp: TempDir,
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
        .create_strategy("walk-forward", None, &[])
        .await
        .expect("create strategy");
    world
        .strategies
        .create_version(pulse::NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(dsl).expect("dsl serializes"),
            created_by: pulse::CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version")
        .id
}

fn request(version: &VersionId) -> WalkForwardRequest {
    WalkForwardRequest {
        version_id: version.clone(),
        pair: Pair::new("BTCUSDT"),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: None,
        config: BacktestConfig::default(),
        snapshots: None,
        from_ms: None,
        to_ms: None,
        k: None,
    }
}

async fn run(world: &World, request: &WalkForwardRequest) -> WalkForwardOutcome {
    run_walk_forward(
        &world.strategies,
        &world.store,
        &BinanceAdapter::new(),
        &world.runs,
        request,
    )
    .await
    .expect("the walk-forward completes over the fixture")
}

// ---------------------------------------------------------------------------
// Row-count + row-shape probes (raw SQL — the tables are the artifact)
// ---------------------------------------------------------------------------

async fn table_count(pool: &SqlitePool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn counts(pool: &SqlitePool) -> (i64, i64, i64, i64) {
    (
        table_count(pool, "walk_forward_run").await,
        table_count(pool, "walk_forward_fold").await,
        table_count(pool, "backtest_run").await,
        table_count(pool, "trade").await,
    )
}

/// The `(walk_forward_run_id, fold_index)` pairs persisted on `backtest_run`.
async fn fold_memberships(pool: &SqlitePool) -> Vec<(String, i64)> {
    sqlx::query_as(
        "SELECT walk_forward_run_id, fold_index FROM backtest_run \
         WHERE walk_forward_run_id IS NOT NULL ORDER BY fold_index",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn fold_windows_persisted(pool: &SqlitePool) -> Vec<(i64, i64, i64)> {
    sqlx::query_as(
        "SELECT fold_index, window_from_ms, window_to_ms FROM walk_forward_fold \
         ORDER BY fold_index",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// (i) the default request persists the whole shape
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_request_persists_run_folds_and_windowed_fold_runs() {
    let world = world().await;
    let version = make_version(&world, &oracle_dsl()).await;
    let m15 = fixture_series(Timeframe::M15);

    let outcome = run(&world, &request(&version)).await;
    let wf = &outcome.run;

    // The run names the scheme and the rule.
    assert_eq!(
        wf.scheme,
        FoldScheme::RollingOos { k: 6 },
        "the default request runs rolling-oos with K=6"
    );
    assert_eq!(wf.scheme.name(), "rolling-oos/v1");
    assert_eq!(wf.rule.name(), "wf-v1");
    assert!(wf.from_defaulted, "a request without `from` defaults it");

    // One parent row, six fold rows, six member runs — all in one transaction.
    let (wf_runs, wf_folds, runs, _trades) = counts(world.db.pool()).await;
    assert_eq!(wf_runs, 1, "one walk_forward_run row");
    assert_eq!(wf_folds, 6, "six walk_forward_fold rows");
    assert_eq!(runs, 6, "six backtest_run rows (the folds)");
    let memberships = fold_memberships(world.db.pool()).await;
    assert_eq!(memberships.len(), 6);
    for (i, (run_id, fold_index)) in memberships.iter().enumerate() {
        assert_eq!(run_id, wf.id.as_str(), "fold {i} names the parent");
        assert_eq!(*fold_index, i64::try_from(i).unwrap(), "fold_index {i}");
    }

    // The fold windows are contiguous, equal-length but the last's remainder,
    // and union exactly to the span.
    assert_eq!(wf.folds.len(), 6);
    let step = (wf.span.to_ms - wf.span.from_ms) / 6;
    for (i, fold) in wf.folds.iter().enumerate() {
        assert_eq!(fold.index, u8::try_from(i).unwrap());
        assert_eq!(
            fold.window.from_ms,
            wf.span.from_ms + i64::try_from(i).unwrap() * step
        );
        if i < 5 {
            assert_eq!(fold.window.to_ms, fold.window.from_ms + step);
            assert_eq!(fold.window.to_ms, wf.folds[i + 1].window.from_ms);
        } else {
            assert_eq!(fold.window.to_ms, wf.span.to_ms);
        }
    }
    assert_eq!(wf.folds[0].window.from_ms, wf.span.from_ms);
    assert_eq!(wf.folds[5].window.to_ms, wf.span.to_ms);
    let persisted = fold_windows_persisted(world.db.pool()).await;
    for (i, fold) in wf.folds.iter().enumerate() {
        assert_eq!(
            persisted[i],
            (
                i64::try_from(i).unwrap(),
                fold.window.from_ms,
                fold.window.to_ms
            ),
            "the persisted fold row carries the same bounds"
        );
    }

    // Every fold is an ordinary windowed run with full-history lead-in.
    let snapshot_start = m15.candles.first().unwrap().open_time;
    for fold in &wf.folds {
        let persisted_run = world
            .runs
            .get_run(&fold.backtest_run_id)
            .await
            .expect("fold run reads")
            .expect("fold run exists");
        let inputs = persisted_run.inputs.expect("a fresh run carries inputs");
        assert_eq!(
            inputs.window.as_ref(),
            Some(&fold.window),
            "fold {}'s run is windowed on its own bounds",
            fold.index
        );
        assert_eq!(
            inputs.lead_in_from_ms,
            Some(snapshot_start),
            "fold {}'s lead-in starts at the snapshot's first candle",
            fold.index
        );
    }
    assert_eq!(outcome.fold_summaries.len(), 6);
}

// ---------------------------------------------------------------------------
// (ii) persisted verdicts equal the recomputed ones
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_verdicts_equal_recomputed_from_trades() {
    let world = world().await;
    let version = make_version(&world, &oracle_dsl()).await;

    let outcome = run(&world, &request(&version)).await;
    let wf = &outcome.run;

    let mut fold_verdicts = Vec::new();
    let mut pooled_rs: Vec<Decimal> = Vec::new();
    for fold in &wf.folds {
        let trades = world
            .runs
            .get_trades(&fold.backtest_run_id)
            .await
            .expect("fold trades read");
        let rs: Vec<Decimal> = trades.iter().map(|t| t.realized_r).collect();
        let recomputed = FoldVerdict::from_rs(&rs);
        assert_eq!(
            fold.verdict, recomputed,
            "fold {}'s persisted verdict is the recomputed one",
            fold.index
        );
        fold_verdicts.push(recomputed);
        pooled_rs.extend(rs);
    }
    assert_eq!(
        wf.verdict,
        RunVerdict::assess(&fold_verdicts, &pooled_rs),
        "the run verdict is the folds' verdicts pooled"
    );
    assert_eq!(
        wf.verdict.folds_required, 4,
        "wf-v1 requires ceil(2*6/3) = 4 holding folds"
    );
}

// ---------------------------------------------------------------------------
// (iii) the default `from` is the first fully-warm bar
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn span_from_defaults_to_first_warm_bar_and_explicit_from_is_identical() {
    let world = world().await;
    let dsl = oracle_dsl();
    let version = make_version(&world, &dsl).await;
    let m15 = fixture_series(Timeframe::M15);

    let compiled = compile_dsl(&dsl);
    let warm = first_fully_warm_bar_ms(&compiled, &m15, None)
        .expect("the fixture's RSI(14) warms inside a month");
    assert_eq!(
        warm, ORACLE_FIRST_WARM_MS,
        "the default `from` is pinned independently of the function (M7b)"
    );

    let defaulted = run(&world, &request(&version)).await;
    assert_eq!(defaulted.run.span.from_ms, warm);
    assert_eq!(defaulted.run.span.from_ms, ORACLE_FIRST_WARM_MS);
    assert!(defaulted.run.from_defaulted);
    // `to` defaults to the snapshot's last candle's close_time (ruling (c)).
    assert_eq!(
        defaulted.run.span.to_ms,
        m15.candles.last().unwrap().close_time
    );

    // The same `from` given explicitly: fold-for-fold identical, not defaulted.
    let mut explicit_req = request(&version);
    explicit_req.from_ms = Some(warm);
    let explicit = run(&world, &explicit_req).await;
    assert!(!explicit.run.from_defaulted);
    assert_eq!(explicit.run.span, defaulted.run.span);
    for (e, d) in explicit.run.folds.iter().zip(&defaulted.run.folds) {
        assert_eq!(e.window, d.window);
        let erun = world
            .runs
            .get_run(&e.backtest_run_id)
            .await
            .unwrap()
            .unwrap();
        let drun = world
            .runs
            .get_run(&d.backtest_run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            erun.result_content_hash, drun.result_content_hash,
            "explicit-from fold {} is byte-identical",
            e.index
        );
        assert_eq!(e.verdict, d.verdict);
    }
    assert_eq!(explicit.run.verdict, defaulted.run.verdict);
}

/// M7b pin: the first fully-warm bar is a hard-coded golden measured once —
/// not recomputed by `first_fully_warm_bar_ms` — and the bar immediately
/// before it is NOT warm: the fixture truncated to `[first_candle, golden)`
/// never reaches the entry gate (the flip), asserted through the public API
/// alone (`CandleSeries::windowed` + `first_fully_warm_bar_ms`).
#[test]
fn first_warm_bar_is_the_pinned_fixture_golden() {
    let m15 = fixture_series(Timeframe::M15);
    let compiled = compile_dsl(&oracle_dsl());

    assert_eq!(
        first_fully_warm_bar_ms(&compiled, &m15, None),
        Some(ORACLE_FIRST_WARM_MS)
    );
    let golden_idx = m15
        .candles
        .iter()
        .position(|c| c.open_time == ORACLE_FIRST_WARM_MS)
        .expect("the golden is a real fixture bar");
    assert_eq!(
        golden_idx, 14,
        "RSI(14) first computes on the 15th candle of the fixture"
    );

    let before =
        m15.windowed(&CandleWindow::new(m15.candles[0].open_time, ORACLE_FIRST_WARM_MS).unwrap());
    assert_eq!(before.candles.len(), golden_idx);
    assert_eq!(
        first_fully_warm_bar_ms(&compiled, &before, None),
        None,
        "no bar before the golden is warm"
    );
}

// ---------------------------------------------------------------------------
// (iv) an explicit `from` before the first warm bar refuses
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn from_before_warm_is_refused_naming_the_earliest_allowed() {
    let world = world().await;
    let dsl = oracle_dsl();
    let version = make_version(&world, &dsl).await;
    let m15 = fixture_series(Timeframe::M15);
    let compiled = compile_dsl(&dsl);
    let warm = first_fully_warm_bar_ms(&compiled, &m15, None).unwrap();

    let mut req = request(&version);
    req.from_ms = Some(warm - 15 * 60_000); // one M15 bar earlier
    let err = run_walk_forward(
        &world.strategies,
        &world.store,
        &BinanceAdapter::new(),
        &world.runs,
        &req,
    )
    .await
    .expect_err("from before the first warm bar must refuse");
    assert_eq!(
        err,
        WalkForwardAppError::FromBeforeWarm {
            field: "from",
            from_ms: warm - 15 * 60_000,
            earliest_allowed_ms: warm,
        }
    );
}

// ---------------------------------------------------------------------------
// (v) K bounds refuse
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn k_out_of_range_is_refused() {
    let world = world().await;
    let version = make_version(&world, &oracle_dsl()).await;

    for k in [1_i32, 13, -1, 256] {
        let mut req = request(&version);
        req.k = Some(k);
        let err = run_walk_forward(
            &world.strategies,
            &world.store,
            &BinanceAdapter::new(),
            &world.runs,
            &req,
        )
        .await
        .expect_err("K outside 2..=12 must refuse");
        assert_eq!(
            err,
            WalkForwardAppError::Domain(WalkForwardError::KOutOfRange {
                k: i64::from(k),
                min: 2,
                max: 12
            }),
            "K={k}"
        );
    }
}

// ---------------------------------------------------------------------------
// (vi) an empty last fold refuses before anything persists
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fold_with_no_counted_candle_refuses_before_any_persist() {
    let world = world().await;
    let dsl = oracle_dsl();
    let version = make_version(&world, &dsl).await;
    let m15 = fixture_series(Timeframe::M15);
    let compiled = compile_dsl(&dsl);
    let warm = first_fully_warm_bar_ms(&compiled, &m15, None).unwrap();
    let last_close = m15.candles.last().unwrap().close_time;

    // A `to` twice the natural span: folds 3..5 start beyond the snapshot's
    // last candle — the first empty fold (index 3) must refuse.
    let mut req = request(&version);
    req.to_ms = Some(last_close + (last_close - warm));
    let before = counts(world.db.pool()).await;
    let err = run_walk_forward(
        &world.strategies,
        &world.store,
        &BinanceAdapter::new(),
        &world.runs,
        &req,
    )
    .await
    .expect_err("a fold with no counted candle must refuse");
    match err {
        WalkForwardAppError::FoldEmpty { fold_index, .. } => {
            assert_eq!(fold_index, 3, "the first empty fold names itself");
        }
        other => panic!("expected FoldEmpty, got {other}"),
    }
    assert_eq!(
        counts(world.db.pool()).await,
        before,
        "a refused request writes nothing (a9)"
    );
}

// ---------------------------------------------------------------------------
// (vii) two cold runs are byte-identical fold by fold
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_cold_runs_persist_identical_fold_hashes_and_verdicts() {
    let world = world().await;
    let version = make_version(&world, &oracle_dsl()).await;

    let first = run(&world, &request(&version)).await;
    let second = run(&world, &request(&version)).await;

    assert_ne!(first.run.id, second.run.id, "two distinct parent runs");
    assert_eq!(first.run.verdict, second.run.verdict, "run verdicts equal");
    for (a, b) in first.run.folds.iter().zip(&second.run.folds) {
        let arun = world
            .runs
            .get_run(&a.backtest_run_id)
            .await
            .unwrap()
            .unwrap();
        let brun = world
            .runs
            .get_run(&b.backtest_run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            arun.result_content_hash, brun.result_content_hash,
            "fold {} is byte-identical across cold runs",
            a.index
        );
        assert_eq!(a.verdict, b.verdict, "fold {} verdict rows equal", a.index);
    }
}

// ---------------------------------------------------------------------------
// (viii) fold runs stay visible through the ordinary run surfaces
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fold_runs_are_listed_and_read_with_walk_forward_membership() {
    let world = world().await;
    let version = make_version(&world, &oracle_dsl()).await;

    let outcome = run(&world, &request(&version)).await;
    let wf = &outcome.run;

    let listed = world
        .runs
        .list_runs_for_version(&version)
        .await
        .expect("list runs");
    assert_eq!(listed.len(), 6, "the six fold runs are the run catalog");
    let listed_ids: Vec<&str> = listed.iter().map(|r| r.id.as_str()).collect();
    for fold in &wf.folds {
        assert!(
            listed_ids.contains(&fold.backtest_run_id.as_str()),
            "fold {} is listed",
            fold.index
        );
        let run = world
            .runs
            .get_run(&fold.backtest_run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            run.walk_forward,
            Some(WalkForwardMembership {
                run_id: wf.id.clone(),
                fold_index: fold.index,
            }),
            "fold {}'s provenance names its walk-forward parent",
            fold.index
        );
    }
}

// ---------------------------------------------------------------------------
// (ix) get_walk_forward_run round-trips
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_walk_forward_run_round_trips_every_field() {
    let world = world().await;
    let version = make_version(&world, &oracle_dsl()).await;

    let outcome = run(&world, &request(&version)).await;
    let read = world
        .runs
        .get_walk_forward_run(&outcome.run.id)
        .await
        .expect("walk-forward run reads")
        .expect("the saved walk-forward run exists");

    assert_eq!(read, outcome.run, "every field round-trips");
}

// ---------------------------------------------------------------------------
// (x) the lower bound is bit-identical across recomputation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fold_lower_bound_is_bit_identical_across_100_recomputations() {
    let world = world().await;
    let version = make_version(&world, &oracle_dsl()).await;

    let outcome = run(&world, &request(&version)).await;
    // A fold with at least one trade — the fixture produces them.
    let fold = outcome
        .run
        .folds
        .iter()
        .find(|f| f.verdict.n > 0)
        .expect("the fixture produces at least one traded fold");
    let trades = world
        .runs
        .get_trades(&fold.backtest_run_id)
        .await
        .expect("fold trades read");
    let rs: Vec<Decimal> = trades.iter().map(|t| t.realized_r).collect();

    let first = FoldVerdict::from_rs(&rs);
    assert_eq!(first, fold.verdict, "recomputed == persisted");
    for _ in 0..100 {
        let again = FoldVerdict::from_rs(&rs);
        assert_eq!(
            again.lower_bound.to_bits(),
            first.lower_bound.to_bits(),
            "the lower bound is bit-identical across recomputation"
        );
    }
}

// ---------------------------------------------------------------------------
// `fold_windows` sanity on the real span (the exhaustive sweep is AC-2's)
// ---------------------------------------------------------------------------

#[test]
fn fold_windows_cover_the_span_contiguously() {
    let span = CandleWindow::new(1_735_689_600_000, 1_738_367_999_999).unwrap();
    let folds = fold_windows(&span, 6);
    assert_eq!(folds.len(), 6);
    assert_eq!(folds[0].from_ms, span.from_ms);
    assert_eq!(folds[5].to_ms, span.to_ms);
    for pair in folds.windows(2) {
        assert_eq!(pair[0].to_ms, pair[1].from_ms, "contiguous");
    }
    let step = (span.to_ms - span.from_ms) / 6;
    for fold in &folds[..5] {
        assert_eq!(fold.to_ms - fold.from_ms, step, "equal-length folds");
    }
    assert!(
        folds[5].to_ms - folds[5].from_ms >= step,
        "the last fold absorbs the remainder"
    );
}

// ---------------------------------------------------------------------------
// The fold read is fail-closed about the run it points at (F11, review fix)
// ---------------------------------------------------------------------------

/// F11: the fold read is DECODABILITY-aware, not existence-aware. A fold row
/// whose referenced `backtest_run` is present but unreadable is an `Err` — the
/// port's contract ("missing or corrupt is an `Err`, never a partial read") —
/// where an existence probe returned the fold anyway and handed the caller a run
/// id they could not follow anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fold_whose_run_does_not_decode_is_refused() {
    let world = world().await;
    let version = make_version(&world, &oracle_dsl()).await;
    let outcome = run(&world, &request(&version)).await;

    // The read is healthy while every fold's run decodes — so the refusal below
    // is the corruption's doing, not this read's default answer.
    world
        .runs
        .get_walk_forward_run(&outcome.run.id)
        .await
        .expect("a healthy walk-forward reads")
        .expect("the run exists");

    // Corrupt ONE fold's run with a schema tag `get_run` refuses (D1b).
    let victim = outcome.run.folds[2].backtest_run_id.clone();
    coach_support::with_run_immutability_lifted(
        world.db.pool(),
        &[&format!(
            "UPDATE backtest_run SET schema_version = 99 WHERE id = '{}'",
            victim.as_str()
        )],
    )
    .await;

    let err = world
        .runs
        .get_walk_forward_run(&outcome.run.id)
        .await
        .expect_err("a fold whose run does not decode must refuse");
    assert!(
        err.to_string().contains("corrupt backtest_run")
            && err.to_string().contains(victim.as_str()),
        "the refusal names the fold's run: {err}"
    );

    // And the two reads agree — the row the fold points at is exactly the row
    // `get_run` refuses, which is what the fold read now honours.
    assert!(
        world.runs.get_run(&victim).await.is_err(),
        "the corrupted run does not decode"
    );
}

/// MINOR (droid): a LOST fold row is a short read, not a smaller run. The write
/// guarantees `k` fold rows indexed `0..k`, so a read whose row set does not
/// match the scheme it declares refuses rather than answering `Ok` with a tally
/// that describes fewer folds than the run claims.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_whose_fold_rows_were_lost_is_refused() {
    let world = world().await;
    let version = make_version(&world, &oracle_dsl()).await;
    let outcome = run(&world, &request(&version)).await;

    // Lift 0013's fold immutability rule and drop ONE fold row.
    coach_support::with_trigger_lifted(
        world.db.pool(),
        "walk_forward_fold_no_delete",
        &[&format!(
            "DELETE FROM walk_forward_fold WHERE walk_forward_run_id = '{}' AND fold_index = 2",
            outcome.run.id.as_str()
        )],
    )
    .await;

    let err = world
        .runs
        .get_walk_forward_run(&outcome.run.id)
        .await
        .expect_err("a run missing a fold row must refuse");
    assert!(
        err.to_string()
            .contains("scheme k=6 but 5 fold row(s) are recorded"),
        "the refusal names the scheme and the row set it found: {err}"
    );
}

// ---------------------------------------------------------------------------
// The version's latest run is not a fold (N1, review fix)
// ---------------------------------------------------------------------------

/// N1: completing a walk-forward does not make one of its folds the version's
/// "latest run".
///
/// The K folds of one walk-forward are `backtest_run` rows of the version that
/// share ONE `created_at`, so `ORDER BY created_at DESC, id DESC` used to hand
/// an arbitrary UUIDv4-selected fold that title — and with it the Library KPIs,
/// the parent expectancy delta, and the pins a later run inherits by default.
/// The clocks here are pinned so the pre-fix behaviour fails rather than
/// sometimes passes: the ordinary run lands at `ORDINARY_MS` and every fold at
/// the strictly later `FOLD_MS`, which is exactly the ordering that made a fold
/// win before the fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_walk_forward_does_not_displace_the_versions_latest_run() {
    const ORDINARY_MS: i64 = 1_756_425_600_000; // 2025-08-29T00:00:00Z
    const FOLD_MS: i64 = ORDINARY_MS + 300_000;

    let world = world().await;
    let version = make_version(&world, &oracle_dsl()).await;

    // A REAL ordinary run of this version, on its own injected clock.
    let early =
        SqliteBacktestRunRepo::with_deps(world.db.pool().clone(), FakeClock::at(ORDINARY_MS));
    let ordinary = run_version_backtest(
        &world.strategies,
        &world.store,
        &BinanceAdapter::new(),
        &early,
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
    .expect("the ordinary run persists");
    assert_eq!(
        ordinary.run.created_at, "2025-08-29T00:00:00.000Z",
        "the ordinary run's instant is the injected clock's (D7)"
    );

    // Then a walk-forward — whose folds are saved strictly later, all six at
    // ONE instant.
    let late = SqliteBacktestRunRepo::with_deps(world.db.pool().clone(), FakeClock::at(FOLD_MS));
    run_walk_forward(
        &world.strategies,
        &world.store,
        &BinanceAdapter::new(),
        &late,
        &request(&version),
    )
    .await
    .expect("the walk-forward completes over the fixture");

    let folds: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, created_at FROM backtest_run WHERE walk_forward_run_id IS NOT NULL \
         ORDER BY id",
    )
    .fetch_all(world.db.pool())
    .await
    .expect("read the fold rows");
    assert_eq!(folds.len(), 6, "K=6 folds persist as ordinary runs");
    assert!(
        folds
            .iter()
            .all(|(_, at)| *at == "2025-08-29T00:05:00.000Z"),
        "every fold of one walk-forward shares one created_at"
    );

    // The latest run is still the ordinary one: a fold never displaces it.
    let latest = world
        .runs
        .latest_run_for_version(&version)
        .await
        .expect("the latest-run read does not fail")
        .expect("the version has a run");
    assert_eq!(
        latest.id, ordinary.run.id,
        "the version's latest run is its ordinary run, not a fold"
    );

    // And the fold runs are still ordinary runs on the catalog read (L8) — the
    // exclusion is scoped to the latest-run read, not to reading folds.
    let catalog = world
        .runs
        .list_runs_for_version(&version)
        .await
        .expect("the catalog reads");
    assert_eq!(catalog.len(), 7, "one ordinary run + six folds");
}

// ---------------------------------------------------------------------------
// Concurrent saves both succeed (R4, review fix)
// ---------------------------------------------------------------------------

/// R4: two walk-forward saves racing on one database both SUCCEED.
///
/// In a DEFERRED transaction each save's first statement is the ownership
/// `SELECT`, so two connections can take read snapshots before either writes;
/// after one commits, the other cannot upgrade its stale WAL snapshot and fails
/// with `SQLITE_BUSY_SNAPSHOT` — which `busy_timeout` does not retry, because it
/// covers a held lock and not a moved snapshot. The save then loses an otherwise
/// valid experiment to a scheduling accident, which is reachable for different
/// versions through desktop windows and for any concurrent MCP calls. The
/// transaction now opens `BEGIN IMMEDIATE`, so a concurrent save WAITS for the
/// write lock (which the timeout does cover).
///
/// Eight versions, so the saves contend on the database rather than on one
/// version's certification pointer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_walk_forward_saves_all_succeed() {
    let world = world().await;
    let mut versions = Vec::new();
    for _ in 0..8 {
        versions.push(make_version(&world, &oracle_dsl()).await);
    }

    let mut handles = Vec::new();
    for version in versions {
        let pool = world.db.pool().clone();
        handles.push(tokio::spawn(async move {
            SqliteBacktestRunRepo::new(pool)
                .save_walk_forward_run(&version, &seeded_walk_forward_draft(true))
                .await
        }));
    }
    for handle in handles {
        let saved = handle.await.expect("the save task does not panic");
        assert!(
            saved.is_ok(),
            "every concurrent save succeeds: {:?}",
            saved.err()
        );
    }

    // And all eight persisted, each with its own folds.
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_run")
        .fetch_one(world.db.pool())
        .await
        .expect("count walk-forward runs");
    assert_eq!(runs, 8, "one run row per save");
    let folds: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_fold")
        .fetch_one(world.db.pool())
        .await
        .expect("count fold rows");
    assert_eq!(folds, 16, "two folds per run");
}
