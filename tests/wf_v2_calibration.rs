//! r4.s1.w3 — AC-1 (demo line d61): wf-v2's calibration on planted-edge data,
//! and the guarantee that every persisted wf-v1 verdict still re-derives
//! unchanged under wf-v1.
//!
//! The calibration works at the RULE level, on seeded synthetic `realized_r`
//! series — no engine, no fixture store, no database: K = 6 folds of 100 trades
//! each, normally distributed R (σ = 1.2R) from a small in-test deterministic
//! PRNG (splitmix64 seeded per series, Box–Muller to a standard normal; no new
//! crate). Over the seeds 1..=20, fixed in advance and never selected:
//!
//! - the planted **+0.25R** series passes wf-v2 on at least 19 of 20 seeds;
//! - the **zero-edge** series passes on at most 4 of 20;
//! - and the exact counts the seeds give are pinned, so a change to the rule or
//!   the PRNG fails this test rather than silently re-calibrating.
//!
//! The second half is the wf-v1 byte-identity guard: one REAL walk-forward over
//! the committed one-month fixture, every persisted fold verdict and the run
//! verdict re-derived from the fold runs' own trades (the save gate's
//! arithmetic), and the same trades held up against wf-v2 in memory — never
//! written back. A persisted wf-v1 run is re-derived under wf-v1 and is never
//! re-assessed under wf-v2 in storage.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{
    BacktestConfig, BacktestRunRepository, BinanceAdapter, CandleStore, CreatedBy, Db, FoldScheme,
    FoldVerdict, K_DEFAULT, Migrator, NewVersion, Pair, RunVerdict, SqliteBacktestRunRepo,
    SqliteStrategyRepo, StrategyDsl, StrategyRepository, Timeframe, VerdictRule, VersionId,
    WalkForwardRequest, WalkForwardRunRepository, run_walk_forward,
};
use rust_decimal::Decimal;
use support::mcp::{FIXTURE_STORE, copy_tree, manifest, migrated_db};
use support::rng::{SplitMix64, normal_decimal};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// The seeded series — the shared splitmix64 + Box–Muller stream (support::rng)
// ---------------------------------------------------------------------------

/// The calibration's fold shape: K = 6 folds of 100 trades.
const CALIBRATION_K: u8 = 6;
const FOLD_TRADES: usize = 100;
/// The planted volatility, σ = 1.2R.
const SIGMA_R: f64 = 1.2;

/// One seed's series: [`CALIBRATION_K`] folds of [`FOLD_TRADES`] trades drawn
/// `N(mean_r, σ²)` from the SHARED generator (`support::rng`, the same stream
/// the measurement harness re-derives the ADR's tables with).
fn seeded_folds(seed: u64, mean_r: f64) -> Vec<Vec<Decimal>> {
    let mut rng = SplitMix64::new(seed);
    (0..CALIBRATION_K)
        .map(|_| {
            (0..FOLD_TRADES)
                .map(|_| normal_decimal(&mut rng, mean_r, SIGMA_R))
                .collect()
        })
        .collect()
}

/// One series' wf-v2 verdict — the same fold assessment and pooling the app
/// path performs (`assess_fold` per fold, `assess_run` over the fold verdicts
/// and the pooled series).
fn wf_v2_verdict(folds: &[Vec<Decimal>]) -> RunVerdict {
    let fold_verdicts: Vec<FoldVerdict> = folds
        .iter()
        .map(|rs| VerdictRule::WfV2.assess_fold(rs))
        .collect();
    let pooled: Vec<Decimal> = folds.iter().flatten().copied().collect();
    VerdictRule::WfV2.assess_run(&fold_verdicts, &pooled)
}

fn passes(seed: u64, mean_r: f64) -> bool {
    wf_v2_verdict(&seeded_folds(seed, mean_r)).pass
}

// ---------------------------------------------------------------------------
// The calibration: seeds 1..=20, planted +0.25R vs zero edge
// ---------------------------------------------------------------------------

/// d61, first half. The planted series must pass almost always and the
/// zero-edge series almost never — and the counts are pinned, not merely
/// bounded: a rule or PRNG change that moved them would fail here.
#[test]
fn wf_v2_calibration_planted_edge_passes_and_zero_edge_fails() {
    let planted: Vec<bool> = (1..=20).map(|s| passes(s, 0.25)).collect();
    let zero: Vec<bool> = (1..=20).map(|s| passes(s, 0.0)).collect();
    let planted_count = planted.iter().filter(|p| **p).count();
    let zero_count = zero.iter().filter(|p| **p).count();
    let passing_seeds = |flags: &[bool]| {
        flags
            .iter()
            .enumerate()
            .filter(|(_, p)| **p)
            .map(|(i, _)| (i + 1).to_string())
            .collect::<Vec<_>>()
            .join(" ")
    };

    assert!(
        planted_count >= 19,
        "the planted +0.25R series must pass wf-v2 on at least 19 of 20 seeds; got \
         {planted_count}: seeds [{}] pass",
        passing_seeds(&planted)
    );
    assert!(
        zero_count <= 4,
        "the zero-edge series must pass wf-v2 on at most 4 of 20 seeds; got {zero_count}: \
         seeds [{}] pass",
        passing_seeds(&zero)
    );
    assert_eq!(
        (planted_count, zero_count),
        (PINNED_PLANTED_PASSES, PINNED_ZERO_EDGE_PASSES),
        "the exact counts the seeds give are pinned (rule or PRNG change?)"
    );
}

/// The exact seed counts measured on first green (2026-10-08, this item) — the
/// pin the assertion above compares against.
const PINNED_PLANTED_PASSES: usize = 20;
const PINNED_ZERO_EDGE_PASSES: usize = 0;

// ---------------------------------------------------------------------------
// The wf-v1 byte-identity guard: a real persisted run, re-derived, never
// re-assessed under wf-v2 in storage
// ---------------------------------------------------------------------------

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
    World {
        strategies: SqliteStrategyRepo::new(db.pool().clone()),
        runs: SqliteBacktestRunRepo::new(db.pool().clone()),
        store: CandleStore::with_base_dir(store_dir),
        _tmp: tmp,
        db,
    }
}

fn oracle_dsl() -> StrategyDsl {
    let json =
        std::fs::read_to_string(manifest("tests/fixtures/strategies/rsi-oversold-long.json"))
            .expect("the oracle fixture reads");
    Migrator::v1().load(&json).expect("dsl loads").dsl
}

async fn make_version(world: &World) -> VersionId {
    let strategy = world
        .strategies
        .create_strategy("wf-v2 calibration", None, &[])
        .await
        .expect("create strategy");
    world
        .strategies
        .create_version(NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(&oracle_dsl()).expect("dsl serializes"),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version")
        .id
}

/// A default wf-v1 walk-forward over the committed one-month fixture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wf_v2_does_not_change_persisted_wf_v1_verdicts() {
    let world = world().await;
    let version = make_version(&world).await;
    let outcome = run_walk_forward(
        &world.strategies,
        &world.store,
        &BinanceAdapter::new(),
        &world.runs,
        &WalkForwardRequest {
            version_id: version.clone(),
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: None,
            config: BacktestConfig::default(),
            snapshots: None,
            from_ms: None,
            to_ms: None,
            k: Some(i32::from(K_DEFAULT)),
            rule: None,
        },
        None,
    )
    .await
    .expect("the wf-v1 walk-forward completes over the fixture");

    assert_eq!(outcome.run.rule, VerdictRule::WfV1);
    assert_eq!(outcome.run.scheme, FoldScheme::RollingOos { k: 6 });

    // Every persisted fold verdict re-derives unchanged under wf-v1 from the
    // fold run's own trades — the derivation the save gate applies.
    let mut derived_folds = Vec::new();
    let mut pooled_rs: Vec<Decimal> = Vec::new();
    for fold in &outcome.run.folds {
        let trades = world
            .runs
            .get_trades(&fold.backtest_run_id)
            .await
            .expect("fold trades read");
        let rs: Vec<Decimal> = trades.iter().map(|t| t.realized_r).collect();
        let derived = FoldVerdict::from_rs(&rs);
        assert_eq!(
            fold.verdict, derived,
            "fold {}'s persisted wf-v1 verdict is the one its trades derive",
            fold.index
        );
        // The same trades under wf-v2: computed in memory, never written.
        let v2 = VerdictRule::WfV2.assess_fold(&rs);
        assert_eq!(v2.n, derived.n);
        assert_eq!(v2.mean_r, derived.mean_r);
        assert_eq!(v2.lower_bound.to_bits(), derived.lower_bound.to_bits());
        derived_folds.push(derived);
        pooled_rs.extend(rs);
    }
    assert_eq!(
        outcome.run.verdict,
        RunVerdict::assess(&derived_folds, &pooled_rs),
        "the run verdict is the folds' wf-v1 verdicts pooled"
    );

    // Nothing was written by the in-memory wf-v2 assessment: one run row, still
    // `wf-v1`, still the same verdict on the read path.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_run")
        .fetch_one(world.db.pool())
        .await
        .expect("count walk-forward runs");
    assert_eq!(count, 1, "one persisted run, as saved");
    let reread = world
        .runs
        .get_walk_forward_run(&outcome.run.id)
        .await
        .expect("read back")
        .expect("the run exists");
    assert_eq!(reread.rule, VerdictRule::WfV1);
    assert_eq!(reread.rule.name(), "wf-v1");
    assert_eq!(reread.verdict, outcome.run.verdict);
}
