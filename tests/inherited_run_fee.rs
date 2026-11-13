//! #555 — an inherited run request takes the CURRENT default taker fee.
//!
//! `resolve_default_request` inherits the parent's (or the version's own) latest
//! run inputs. Before 0.1.17 it copied the fee too, so a lineage whose run was
//! recorded at 4 bps kept running at 4 bps after #527 made the default 5 bps
//! (PDR-0052). The fee is now the one input an inherited request does NOT
//! inherit; every other recorded input still is.
//!
//! The suite runs the real MCP tools (`run_backtest`, `run_walk_forward`) over
//! a lineage whose only run was recorded at 4 bps: the child with no run, and
//! the version with its own run. It reads the persisted runs back and compares
//! their recorded inputs with the 4 bps run's, field by field.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{
    BacktestConfig, BacktestInputs, BacktestRequest, BacktestRunId, BacktestRunRepository,
    BinanceAdapter, CandleStore, Pair, SqliteBacktestRunRepo, SqliteStrategyRepo, Timeframe,
    VersionId, run_version_backtest,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use support::mcp::{
    FIXTURE_STORE, Fixture, call, copy_tree, manifest, migrated_db, seed_versions, spawn_client,
};
use tempfile::TempDir;

/// The fee the lineage's only run was recorded at — the pre-#527 default.
fn recorded_fee() -> Decimal {
    Decimal::new(4, 0)
}

/// The version tree plus ONE real run on `parent`, recorded at 4 bps. Returns
/// the fixture, the run's recorded inputs and a running MCP client.
async fn lineage_at_four_bps() -> (
    Fixture,
    BacktestInputs,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
) {
    let tmp_db = TempDir::new().unwrap();
    let (db_path, db) = migrated_db(&tmp_db).await;
    let (parent, child) = seed_versions(&db).await;

    let tmp_store = TempDir::new().unwrap();
    let store_dir = tmp_store.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);

    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let outcome = run_version_backtest(
        &strategies,
        &CandleStore::with_base_dir(store_dir.clone()),
        &BinanceAdapter::new(),
        &runs,
        &BacktestRequest {
            version_id: parent.clone(),
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: Some(Timeframe::H4),
            config: BacktestConfig {
                taker_fee_bps: recorded_fee(),
                ..BacktestConfig::default()
            },
            snapshots: None,
            window: None,
        },
        None,
    )
    .await
    .expect("the 4 bps run completes over the fixture");
    let recorded = run_inputs(&runs, &outcome.run.id).await;
    assert_eq!(
        recorded.taker_fee_bps,
        recorded_fee(),
        "the fixture's only run was recorded at 4 bps"
    );

    let client = spawn_client(&db_path, &store_dir).await;
    (
        Fixture {
            _tmp_db: tmp_db,
            _tmp_store: tmp_store,
            db,
            db_path,
            store_dir,
            seed: (parent, child, outcome.run.id),
        },
        recorded,
        client,
    )
}

/// One persisted run's recorded inputs, read back from the store.
async fn run_inputs(
    runs: &SqliteBacktestRunRepo<pulse::SystemClock>,
    id: &BacktestRunId,
) -> BacktestInputs {
    runs.get_run(id)
        .await
        .expect("read the run back")
        .expect("the run exists")
        .inputs
        .expect("a fresh run records its inputs")
}

/// The run id a `run_backtest` call answers with.
fn run_id_of(result: &Value) -> BacktestRunId {
    BacktestRunId::new(result["run_id"].as_str().expect("run_id on the wire"))
}

/// Every recorded input except the fee equals the 4 bps run's, and the fee is
/// the current default. `window` and `lead_in_from_ms` are the run's own (a
/// walk-forward fold is windowed), so they are not compared.
fn assert_only_the_fee_moved(got: &BacktestInputs, recorded: &BacktestInputs, what: &str) {
    assert_eq!(
        got.taker_fee_bps,
        BacktestConfig::default().taker_fee_bps,
        "{what}: the inherited request charges the current default fee"
    );
    assert_eq!(
        got.taker_fee_bps,
        Decimal::new(5, 0),
        "{what}: that is 5 bps"
    );
    assert_eq!(got.pair, recorded.pair, "{what}: pair inherited");
    assert_eq!(
        got.primary, recorded.primary,
        "{what}: primary snapshot inherited"
    );
    assert_eq!(got.htf, recorded.htf, "{what}: htf snapshot inherited");
    assert_eq!(got.d1, recorded.d1, "{what}: d1 snapshot inherited");
    assert_eq!(
        got.slippage_bps, recorded.slippage_bps,
        "{what}: slippage inherited"
    );
    assert_eq!(got.funding, recorded.funding, "{what}: funding inherited");
}

/// `run_backtest` on `version`: the persisted run's inputs.
async fn backtest_inputs(
    fixture: &Fixture,
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    version: &VersionId,
) -> BacktestInputs {
    let result = call(
        client,
        "run_backtest",
        json!({ "version_id": version.as_str() }),
    )
    .await;
    let runs = SqliteBacktestRunRepo::new(fixture.db.pool().clone());
    run_inputs(&runs, &run_id_of(&result)).await
}

/// `run_walk_forward` (k 2) on `version`: every fold run's inputs.
async fn walk_forward_fold_inputs(
    fixture: &Fixture,
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    version: &VersionId,
) -> Vec<BacktestInputs> {
    let detail = call(
        client,
        "run_walk_forward",
        json!({ "version_id": version.as_str(), "k": 2 }),
    )
    .await;
    let runs = SqliteBacktestRunRepo::new(fixture.db.pool().clone());
    let mut out = Vec::new();
    for fold in detail["folds"].as_array().expect("folds") {
        let id = BacktestRunId::new(fold["backtest_run_id"].as_str().expect("fold run id"));
        out.push(run_inputs(&runs, &id).await);
    }
    assert_eq!(out.len(), 2, "k 2 persists two fold runs");
    out
}

/// (a) A child whose PARENT has a 4 bps run: the default backtest runs at 5 bps.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_childs_backtest_over_a_four_bps_parent_runs_at_five() {
    let (fixture, recorded, client) = lineage_at_four_bps().await;
    let (_parent, child, _run) = &fixture.seed;

    let got = backtest_inputs(&fixture, &client, child).await;
    assert_only_the_fee_moved(&got, &recorded, "child backtest");

    client.cancel().await.expect("cancel session");
}

/// (a) The same child's default walk-forward: every fold runs at 5 bps.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_childs_walk_forward_over_a_four_bps_parent_runs_at_five() {
    let (fixture, recorded, client) = lineage_at_four_bps().await;
    let (_parent, child, _run) = &fixture.seed;

    for folds in walk_forward_fold_inputs(&fixture, &client, child).await {
        assert_only_the_fee_moved(&folds, &recorded, "child walk-forward fold");
    }

    client.cancel().await.expect("cancel session");
}

/// (b) A version with its OWN 4 bps run (no parent in play): the default
/// backtest runs at 5 bps.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_versions_backtest_over_its_own_four_bps_run_runs_at_five() {
    let (fixture, recorded, client) = lineage_at_four_bps().await;
    let (parent, _child, _run) = &fixture.seed;

    let got = backtest_inputs(&fixture, &client, parent).await;
    assert_only_the_fee_moved(&got, &recorded, "own-run backtest");

    client.cancel().await.expect("cancel session");
}

/// (b) The same version's default walk-forward: every fold runs at 5 bps.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_versions_walk_forward_over_its_own_four_bps_run_runs_at_five() {
    let (fixture, recorded, client) = lineage_at_four_bps().await;
    let (parent, _child, _run) = &fixture.seed;

    for folds in walk_forward_fold_inputs(&fixture, &client, parent).await {
        assert_only_the_fee_moved(&folds, &recorded, "own-run walk-forward fold");
    }

    client.cancel().await.expect("cancel session");
}
