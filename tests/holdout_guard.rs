//! r4.s1.w4 — AC-1 (demo line d63), AC-2 and AC-4: the freeze record, its two
//! operator commands, the one holdout guard on every entry point, and #327's
//! effective-window echo.
//!
//! The world: a migrated temp `pulse.db`, the seeded version tree, a copied
//! BTCUSDT fixture store (January 2025, M15 + H4) and a real parent run — so
//! every run surface resolves its snapshots the way the product does. The
//! holdout start used throughout is `2025-01-16T00:00:00Z`, inside the
//! fixture's range, so an explicit window into it is testable and a defaulted
//! window visibly clamps.
//!
//! Coverage:
//! - the two commands and the read verb (AC-2), including every named refusal
//!   and the table's immutability (no delete, no second close, no edit, one
//!   open row, a new holdout after the last close);
//! - every Q4 entry point refuses an explicit window into the holdout by name
//!   (MCP `run_backtest` / `run_walk_forward`, the app's backtest and
//!   walk-forward cores, the coach's certification gate, the CLI);
//! - a defaulted window is clamped to the holdout start and the result says so;
//! - an agent export stops at the holdout start;
//! - with no freeze open nothing changes; after `close-freeze` the guard lifts;
//! - the exemptions run unguarded (the certify-fixture seed).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod coach_gate_support;
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use pulse::{
    AcceptFailureStage, BacktestRunId, BacktestRunRepository, CandleSeriesRepository, CandleStore,
    CoachAction, CoachDecisionOutcome, Db, DesktopState, FakeClock, FreezeStoreError,
    HoldoutFreeze, OpenFreezeRequest, Pair, SqliteBacktestRunRepo, SqliteCertificationFreezeRepo,
    Timeframe, VersionId, WalkForwardRunRequest, run_backtest_version_core,
    run_walk_forward_version_core,
};
use serde_json::{Value, json};
use support::mcp::{
    FIXTURE_STORE, arguments, call, call_err, copy_tree, manifest, migrated_db, seed_real_run,
    seed_versions, spawn_client,
};
use tempfile::TempDir;

/// The holdout start every guard test freezes at: inside the fixture's January
/// 2025 BTCUSDT range (`2025-01-16T00:00:00Z`).
const HOLDOUT_MS: i64 = 1_736_985_600_000;

/// The instant the test freeze is opened at (deterministic, injected).
const OPENED_MS: i64 = 1_760_000_000_000;

/// The instant a test close writes (deterministic, injected; after `OPENED_MS`).
const CLOSED_MS: i64 = 1_760_000_600_000;

/// A holdout start AFTER the fixture's whole range (`2026-01-01T00:00:00Z`) —
/// the "the snapshot already ends before the holdout start" case: the guard
/// must not clamp and the echo must say so.
const AFTER_FIXTURE_MS: i64 = 1_767_225_600_000;

/// The fixture's first M15 candle `open_time` (`2025-01-01T00:00:00Z`).
const FIXTURE_FIRST_OPEN_MS: i64 = 1_735_689_600_000;

// ---------------------------------------------------------------------------
// The world
// ---------------------------------------------------------------------------

struct World {
    _tmp: TempDir,
    db: Db,
    db_path: PathBuf,
    store_dir: PathBuf,
    child: VersionId,
}

async fn world() -> World {
    let tmp = TempDir::new().unwrap();
    let (db_path, db) = migrated_db(&tmp).await;
    let (parent, child) = seed_versions(&db).await;
    let store_dir = tmp.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);
    // One REAL parent run: its recorded inputs are what the child's default
    // resolution inherits (pair, timeframes, snapshot pins).
    seed_real_run(&db, &store_dir, &parent).await;
    World {
        _tmp: tmp,
        db,
        db_path,
        store_dir,
        child,
    }
}

impl World {
    fn store(&self) -> CandleStore {
        CandleStore::with_base_dir(self.store_dir.clone())
    }

    fn runs(&self) -> SqliteBacktestRunRepo<pulse::SystemClock> {
        SqliteBacktestRunRepo::new(self.db.pool().clone())
    }

    /// Open the freeze through the store, with the injected clock.
    async fn open_freeze(&self, holdout_start_ms: i64) {
        freeze_repo(&self.db, OPENED_MS)
            .open(&open_request(holdout_start_ms))
            .await
            .expect("open the freeze");
    }

    async fn client(&self) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        spawn_client(&self.db_path, &self.store_dir).await
    }
}

/// A `SqliteCertificationFreezeRepo` over `db` with a fixed clock.
fn freeze_repo(db: &Db, at_ms: i64) -> SqliteCertificationFreezeRepo<FakeClock> {
    SqliteCertificationFreezeRepo::with_deps(db.pool().clone(), FakeClock::at(at_ms))
}

fn open_request(holdout_start_ms: i64) -> OpenFreezeRequest {
    OpenFreezeRequest {
        holdout_start_ms,
        h: 12,
        alpha: "0.0463".to_owned(),
        holdout_test: "C1: z = z(1 - 0.05/H)".to_owned(),
    }
}

/// Run `pulse certify <args> --db <db_path>` and return the output.
fn certify(db_path: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pulse"))
        .arg("certify")
        .args(args)
        .arg("--db")
        .arg(db_path)
        .output()
        .expect("run pulse certify")
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// One explicit window that reaches into the holdout (both bounds are inside
/// the fixture's range, the `to` past the holdout start).
fn window_into_holdout() -> Value {
    json!({
        "from": "2025-01-10T00:00:00Z",
        "to": "2025-01-20T00:00:00Z",
    })
}

/// One explicit window that clears the holdout (`to` is exclusive, so ending
/// exactly at the holdout start is legal).
fn window_clearing_holdout() -> Value {
    json!({
        "from": "2025-01-10T00:00:00Z",
        "to": "2025-01-16T00:00:00Z",
    })
}

/// The persisted run's recorded counted window, read back through the ordinary
/// run log.
async fn recorded_window(world: &World, run_id: &str) -> (i64, i64) {
    let run = world
        .runs()
        .get_run(&BacktestRunId::new(run_id))
        .await
        .expect("read the run")
        .expect("the run exists");
    let window = run
        .inputs
        .expect("a fresh run carries its inputs")
        .window
        .expect("a guarded run records its counted window");
    (window.from_ms, window.to_ms)
}

/// The runs persisted for a version — the "no run is saved" probe.
async fn run_count(world: &World, version: &VersionId) -> usize {
    world
        .runs()
        .list_runs_for_version(version)
        .await
        .expect("list the version's runs")
        .len()
}

// ---------------------------------------------------------------------------
// AC-2: the freeze commands, their refusals, and the table's immutability
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeze_command_opens_a_record_and_refuses_a_second_open() {
    let world = world().await;
    let opened = certify(
        &world.db_path,
        &[
            "freeze",
            "--holdout-start",
            "2025-01-16",
            "--h",
            "12",
            "--alpha",
            "0.0463",
            "--test",
            "C1: z = z(1 - 0.05/H)",
        ],
    );
    assert!(opened.status.success(), "stderr: {}", stderr_of(&opened));
    let printed = stdout_of(&opened);
    for expected in [
        "holdout_start: 2025-01-16T00:00:00Z",
        "h: 12",
        "alpha: 0.0463",
        "holdout_test: C1: z = z(1 - 0.05/H)",
        "closed_at: -",
    ] {
        assert!(
            printed.contains(expected),
            "the printed record carries {expected:?}: {printed}"
        );
    }

    let again = certify(
        &world.db_path,
        &[
            "freeze",
            "--holdout-start",
            "2025-02-01",
            "--h",
            "12",
            "--alpha",
            "0.0463",
            "--test",
            "C1",
        ],
    );
    assert!(!again.status.success(), "a second open must be refused");
    assert!(
        stderr_of(&again).contains("already open"),
        "the refusal names the open freeze: {}",
        stderr_of(&again)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeze_command_refuses_h_outside_the_budget() {
    let world = world().await;
    for h in ["13", "0"] {
        let out = certify(
            &world.db_path,
            &[
                "freeze",
                "--holdout-start",
                "2025-01-16",
                "--h",
                h,
                "--alpha",
                "0.0463",
                "--test",
                "C1",
            ],
        );
        assert!(!out.status.success(), "--h {h} must be refused");
        assert!(
            stderr_of(&out).contains("1..=12"),
            "the refusal names the budget: {}",
            stderr_of(&out)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_freeze_command_refuses_when_none_is_open_and_status_reads_none() {
    let world = world().await;
    let status = certify(&world.db_path, &["status"]);
    assert!(status.status.success());
    assert_eq!(stdout_of(&status).trim(), "no open freeze");

    let close = certify(&world.db_path, &["close-freeze"]);
    assert!(!close.status.success(), "closing nothing must be refused");
    assert!(
        stderr_of(&close).contains("no freeze is open"),
        "the refusal says what is missing: {}",
        stderr_of(&close)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_freeze_command_closes_once_and_a_second_close_is_refused() {
    let world = world().await;
    assert!(
        certify(
            &world.db_path,
            &[
                "freeze",
                "--holdout-start",
                "2025-01-16",
                "--h",
                "12",
                "--alpha",
                "0.0463",
                "--test",
                "C1",
            ],
        )
        .status
        .success()
    );

    let closed = certify(&world.db_path, &["close-freeze"]);
    assert!(closed.status.success(), "stderr: {}", stderr_of(&closed));
    assert!(
        stdout_of(&closed).contains("closed_at: 2"),
        "the closed record prints its close: {}",
        stdout_of(&closed)
    );

    let again = certify(&world.db_path, &["close-freeze"]);
    assert!(!again.status.success(), "a second close must be refused");
    assert!(stderr_of(&again).contains("no freeze is open"));

    // And the read verb reports no open freeze again.
    let status = certify(&world.db_path, &["status"]);
    assert_eq!(stdout_of(&status).trim(), "no open freeze");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeze_store_refuses_a_reused_holdout_and_a_second_open() {
    let world = world().await;
    let repo = freeze_repo(&world.db, OPENED_MS);
    repo.open(&open_request(HOLDOUT_MS))
        .await
        .expect("the first freeze opens");

    // Law 1, named: at most one open.
    let err = repo
        .open(&open_request(HOLDOUT_MS + 1))
        .await
        .expect_err("a second open must be refused");
    assert!(
        matches!(err, FreezeStoreError::FreezeOpen { .. }),
        "got {err:?}"
    );

    repo.close().await.expect("the freeze closes");

    // Law 2, named: a spent holdout is never reused (F1).
    let err = repo
        .open(&open_request(HOLDOUT_MS))
        .await
        .expect_err("a reused holdout must be refused");
    assert!(
        matches!(err, FreezeStoreError::HoldoutStartNotAfterLastClose { .. }),
        "got {err:?}"
    );

    // Strictly later is legal, and the store refuses H outside the budget.
    repo.open(&open_request(CLOSED_MS + 1))
        .await
        .expect("a strictly later holdout opens");
    let err = repo
        .open(&open_request(CLOSED_MS + 2))
        .await
        .expect_err("a second open must be refused");
    assert!(matches!(err, FreezeStoreError::FreezeOpen { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeze_table_refuses_delete_a_second_close_and_an_edit() {
    let world = world().await;
    world.open_freeze(HOLDOUT_MS).await;

    // No DELETE.
    let err = sqlx::query("DELETE FROM certification_freeze")
        .execute(world.db.pool())
        .await
        .expect_err("a freeze row is never deleted");
    assert!(
        err.to_string().contains("never deleted"),
        "the refusal says why: {err}"
    );

    // One close is legal.
    sqlx::query("UPDATE certification_freeze SET closed_at_ms = ?1")
        .bind(CLOSED_MS)
        .execute(world.db.pool())
        .await
        .expect("the first close writes");

    // A second close is not.
    let err = sqlx::query("UPDATE certification_freeze SET closed_at_ms = ?1")
        .bind(CLOSED_MS + 1)
        .execute(world.db.pool())
        .await
        .expect_err("a second close is refused");
    assert!(
        err.to_string()
            .contains("immutable except a first closed_at_ms"),
        "the refusal says why: {err}"
    );

    // Nor is any edit, on the frozen parameters or the holdout start.
    for statement in [
        "UPDATE certification_freeze SET alpha = '0.9'",
        "UPDATE certification_freeze SET holdout_start_ms = holdout_start_ms + 1",
        "UPDATE certification_freeze SET h = 6",
    ] {
        let err = sqlx::query(statement)
            .execute(world.db.pool())
            .await
            .expect_err("an edit is refused");
        assert!(
            err.to_string().contains("immutable"),
            "the refusal says why: {err}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeze_table_refuses_a_second_open_row() {
    let world = world().await;
    world.open_freeze(HOLDOUT_MS).await;
    let err = sqlx::query(
        "INSERT INTO certification_freeze \
         (id, holdout_start_ms, h, alpha, holdout_test, opened_at_ms, closed_at_ms) \
         VALUES ('raw-2', ?1, 12, '0.0463', 'C1', ?2, NULL)",
    )
    .bind(HOLDOUT_MS + 1)
    .bind(OPENED_MS + 1)
    .execute(world.db.pool())
    .await
    .expect_err("a second open row is refused at the schema");
    assert!(
        err.to_string().contains("at most one may be open"),
        "the refusal says why: {err}"
    );
}

// ---------------------------------------------------------------------------
// AC-1: the guard on every entry point
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_refuses_mcp_run_backtest_into_the_holdout_by_name() {
    let world = world().await;
    world.open_freeze(HOLDOUT_MS).await;
    let client = world.client().await;

    let mut args = arguments(&window_into_holdout());
    args.insert("version_id".to_owned(), json!(world.child.as_str()));
    let err = call_err(&client, "run_backtest", Value::Object(args)).await;
    assert_eq!(err["field"], "to", "the offending bound is named: {err}");
    let message = err["message"].as_str().expect("a message");
    assert!(message.contains("BTCUSDT"), "cites the pair: {message}");
    assert!(
        message.contains("2025-01-16"),
        "cites the holdout start: {message}"
    );
    assert_eq!(
        run_count(&world, &world.child).await,
        0,
        "no run is saved on a refusal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_refuses_mcp_run_walk_forward_into_the_holdout_by_name() {
    let world = world().await;
    world.open_freeze(HOLDOUT_MS).await;
    let client = world.client().await;

    // An explicit `to` past the holdout start.
    let err = call_err(
        &client,
        "run_walk_forward",
        json!({"version_id": world.child.as_str(), "to": "2025-01-20T00:00:00Z"}),
    )
    .await;
    assert_eq!(err["field"], "to", "{err}");
    assert!(err["message"].as_str().unwrap().contains("2025-01-16"));

    // An explicit `from` at the holdout start.
    let err = call_err(
        &client,
        "run_walk_forward",
        json!({"version_id": world.child.as_str(), "from": "2025-01-16T00:00:00Z"}),
    )
    .await;
    assert_eq!(err["field"], "from", "{err}");
    assert!(err["message"].as_str().unwrap().contains("BTCUSDT"));

    // The walk-forward log stays empty for the child.
    let wf_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_run WHERE strategy_version_id = ?1")
            .bind(world.child.as_str())
            .fetch_one(world.db.pool())
            .await
            .expect("count the version's walk-forward rows");
    assert_eq!(wf_rows, 0, "no walk-forward row is saved");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_refuses_the_app_walk_forward_into_the_holdout_by_name() {
    let world = world().await;
    world.open_freeze(HOLDOUT_MS).await;
    let state = DesktopState::open_with_store(&world.db_path, world.store())
        .await
        .expect("open state");

    let err = run_walk_forward_version_core(
        &state,
        WalkForwardRunRequest {
            version_id: world.child.as_str().to_owned(),
            from: Some("2025-01-10T00:00:00Z".to_owned()),
            to: Some("2025-01-20T00:00:00Z".to_owned()),
            k: None,
        },
    )
    .await
    .expect_err("the app walk-forward refuses the holdout window");
    let message = err.to_string();
    assert!(
        message.contains("BTCUSDT") && message.contains("2025-01-16"),
        "the refusal cites the pair and the holdout start: {message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_refuses_the_coach_gate_span_into_the_holdout_by_name() {
    // The gate walks the parent's certifying span, which here ends at the
    // fixture's last close — past the holdout start.
    let gate_world = coach_gate_support::world().await;
    coach_gate_support::certify_parent(&gate_world).await;

    let outcome = coach_gate_support::decide_with_holdout(
        &gate_world,
        CoachAction::Accept,
        Some(HoldoutFreeze {
            holdout_start_ms: HOLDOUT_MS,
        }),
    )
    .await;
    match outcome {
        CoachDecisionOutcome::AcceptFailed(proposal) => {
            let failure = proposal.accept_failure.expect("the failure is recorded");
            assert_eq!(failure.stage, AcceptFailureStage::WalkForward);
            assert!(
                failure.message.contains("holdout"),
                "the recorded failure names the holdout: {}",
                failure.message
            );
            assert!(
                failure.message.contains("BTCUSDT") && failure.message.contains("2025-01-16"),
                "it cites the pair and the holdout start: {}",
                failure.message
            );
        }
        other => panic!("expected a recorded accept failure, got {other:?}"),
    }

    // And the same accept RUNS with no freeze open (the guard is the only
    // difference), proving the refusal came from the guard and not the fixture.
    let gate_world = coach_gate_support::world().await;
    coach_gate_support::certify_parent(&gate_world).await;
    let outcome =
        coach_gate_support::decide_with_holdout(&gate_world, CoachAction::Accept, None).await;
    assert!(
        !matches!(outcome, CoachDecisionOutcome::Accepted(_)),
        "the gate's own verdict decides the accept (the fixture's span fails it): {outcome:?}"
    );
    match outcome {
        CoachDecisionOutcome::AcceptFailed(proposal) => {
            let failure = proposal.accept_failure.expect("the failure is recorded");
            assert_eq!(failure.stage, AcceptFailureStage::WalkForward);
            assert!(
                !failure.message.contains("holdout"),
                "without a freeze the gate runs its own walk-forward: {}",
                failure.message
            );
        }
        other => panic!("expected a recorded accept failure, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_clamps_the_cli_backtest_default_window() {
    let world = world().await;
    world.open_freeze(HOLDOUT_MS).await;
    let out = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "backtest",
            "--version",
            world.child.as_str(),
            "--pair",
            "BTCUSDT",
            "--tf",
            "M15",
            "--htf",
            "H4",
            "--store",
            &world.store_dir.to_string_lossy(),
            "--db",
            &world.db_path.to_string_lossy(),
            "--json",
        ])
        .output()
        .expect("run pulse backtest --version");
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));

    let runs = world
        .runs()
        .list_runs_for_version(&world.child)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1, "the clamped run persisted");
    let (from, to) = recorded_window(&world, runs[0].id.as_str()).await;
    assert_eq!(to, HOLDOUT_MS, "the defaulted window stops at the holdout");
    assert_eq!(
        from, FIXTURE_FIRST_OPEN_MS,
        "the clamped window counts from the snapshot's first candle"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_clamps_the_app_backtest_default_window() {
    let world = world().await;
    world.open_freeze(HOLDOUT_MS).await;
    let state = DesktopState::open_with_store(&world.db_path, world.store())
        .await
        .expect("open state");
    let dto = run_backtest_version_core(
        &state,
        pulse::BacktestRunRequest {
            version_id: world.child.as_str().to_owned(),
        },
    )
    .await
    .expect("the app backtest runs clamped");

    let (from, to) = recorded_window(&world, &dto.run_id).await;
    assert_eq!(to, HOLDOUT_MS, "the defaulted window stops at the holdout");
    assert_eq!(from, FIXTURE_FIRST_OPEN_MS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_lifts_after_close_freeze() {
    let world = world().await;
    world.open_freeze(HOLDOUT_MS).await;
    let client = world.client().await;

    let mut args = arguments(&window_into_holdout());
    args.insert("version_id".to_owned(), json!(world.child.as_str()));
    let _ = call_err(&client, "run_backtest", Value::Object(args.clone())).await;

    freeze_repo(&world.db, CLOSED_MS)
        .close()
        .await
        .expect("the freeze closes");

    let out = call(&client, "run_backtest", Value::Object(args)).await;
    assert!(
        out["run_id"].is_string(),
        "the same window runs once the freeze is closed: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_freeze_open_leaves_every_surface_unchanged() {
    let world = world().await;
    let client = world.client().await;

    let mut args = arguments(&window_into_holdout());
    args.insert("version_id".to_owned(), json!(world.child.as_str()));
    let out = call(&client, "run_backtest", Value::Object(args)).await;
    assert!(out["run_id"].is_string(), "{out}");
}

// ---------------------------------------------------------------------------
// AC-1: the export cut
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_exports_stop_at_the_holdout_start() {
    let world = world().await;
    world.open_freeze(HOLDOUT_MS).await;
    let client = world.client().await;

    let total = world
        .store()
        .load_head(&Pair::new("BTCUSDT"), Timeframe::M15)
        .expect("load HEAD")
        .expect("the fixture has a HEAD")
        .series
        .candles
        .len();

    // export_candles (CSV): cut, and the result says so.
    let out = call(
        &client,
        "export_candles",
        json!({"pair": "BTCUSDT", "timeframe": "15m"}),
    )
    .await;
    let rows = usize::try_from(out["rows"].as_u64().unwrap()).unwrap();
    let withheld = usize::try_from(out["holdout"]["rows_withheld"].as_u64().unwrap()).unwrap();
    assert_eq!(
        rows + withheld,
        total,
        "every candle is either kept or withheld"
    );
    assert!(withheld > 0, "the fixture's range reaches past the holdout");
    assert_eq!(out["holdout"]["holdout_start"], "2025-01-16T00:00:00.000Z");
    let csv = std::fs::read_to_string(out["path"].as_str().unwrap()).expect("read the export");
    let last_open: i64 = csv
        .lines()
        .last()
        .expect("a data row")
        .split('\t')
        .next()
        .expect("the open_time column")
        .parse()
        .expect("open_time is epoch ms");
    assert!(
        last_open < HOLDOUT_MS,
        "the last exported candle opens before the holdout start: {last_open}"
    );

    // export_indicators: the same cut.
    let out = call(
        &client,
        "export_indicators",
        json!({"pair": "BTCUSDT", "timeframe": "15m", "indicators": ["rsi:14"]}),
    )
    .await;
    let rows = usize::try_from(out["rows"].as_u64().unwrap()).unwrap();
    let withheld = usize::try_from(out["holdout"]["rows_withheld"].as_u64().unwrap()).unwrap();
    assert_eq!(rows + withheld, total);

    // The parquet byte copy cannot be cut, so it is refused while a freeze is
    // open (the accepted known limit) — CSV is the cut surface.
    let err = call_err(
        &client,
        "export_candles",
        json!({"pair": "BTCUSDT", "timeframe": "15m", "format": "parquet"}),
    )
    .await;
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("cannot be cut at the holdout start"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// AC-1: the exemptions run unguarded
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exemptions_run_unguarded() {
    let world = world().await;
    world.open_freeze(HOLDOUT_MS).await;

    // The certify-fixture seed (grill Q4, G8) walks its own synthetic
    // snapshots with `holdout: None` — a freeze must not refuse it.
    let data_dir = world.store_dir.join("fixture-synthetic");
    let out = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "fixture",
            "seed",
            "--db",
            &world.db_path.to_string_lossy(),
            "--data-dir",
            &data_dir.to_string_lossy(),
        ])
        .output()
        .expect("run pulse fixture seed");
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
}

// ---------------------------------------------------------------------------
// AC-4 (#327): the effective-window echo
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_backtest_echoes_the_effective_window() {
    let world = world().await;
    let client = world.client().await;

    // An explicit window: echoed verbatim, both bounds explicit, not clamped —
    // and `to` is exclusive (the request ends at 2025-01-20, the run counts
    // through the candle opening 2025-01-19T23:45).
    let mut args = arguments(&json!({
        "from": "2025-01-10T00:00:00Z",
        "to": "2025-01-20T00:00:00Z",
    }));
    args.insert("version_id".to_owned(), json!(world.child.as_str()));
    let out = call(&client, "run_backtest", Value::Object(args)).await;
    let window = &out["effective_window"];
    assert_eq!(window["from"], "2025-01-10T00:00:00.000Z");
    assert_eq!(window["to"], "2025-01-20T00:00:00.000Z");
    assert_eq!(window["from_defaulted"], false);
    assert_eq!(window["to_defaulted"], false);
    assert_eq!(window["to_clamped"], false);

    // No window at all: the whole snapshot, both bounds defaulted, no clamp.
    let out = call(
        &client,
        "run_backtest",
        json!({"version_id": world.child.as_str()}),
    )
    .await;
    let window = &out["effective_window"];
    assert_eq!(window["from"], "2025-01-01T00:00:00.000Z");
    assert_eq!(window["to"], "2025-01-31T23:59:59.999Z");
    assert_eq!(window["from_defaulted"], true);
    assert_eq!(window["to_defaulted"], true);
    assert_eq!(window["to_clamped"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_walk_forward_echoes_the_effective_window() {
    let world = world().await;
    let client = world.client().await;

    let out = call(
        &client,
        "run_walk_forward",
        json!({
            "version_id": world.child.as_str(),
            "from": "2025-01-10T00:00:00Z",
            "to": "2025-01-20T00:00:00Z",
            "k": 2,
        }),
    )
    .await;
    let window = &out["effective_window"];
    assert_eq!(window["from"], "2025-01-10T00:00:00.000Z");
    assert_eq!(window["to"], "2025-01-20T00:00:00.000Z");
    assert_eq!(window["from_defaulted"], false);
    assert_eq!(window["to_defaulted"], false);
    assert_eq!(window["to_clamped"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clamped_defaults_echo_the_effective_window() {
    let world = world().await;
    world.open_freeze(HOLDOUT_MS).await;
    let client = world.client().await;

    // run_backtest: the defaulted whole-snapshot window is clamped.
    let out = call(
        &client,
        "run_backtest",
        json!({"version_id": world.child.as_str()}),
    )
    .await;
    let window = &out["effective_window"];
    assert_eq!(window["from"], "2025-01-01T00:00:00.000Z");
    assert_eq!(window["to"], "2025-01-16T00:00:00.000Z");
    assert_eq!(window["from_defaulted"], true);
    assert_eq!(window["to_defaulted"], true);
    assert_eq!(window["to_clamped"], true);

    // run_walk_forward: the defaulted `to` is clamped; `from` stays the first
    // fully-warm bar.
    let out = call(
        &client,
        "run_walk_forward",
        json!({"version_id": world.child.as_str(), "k": 2}),
    )
    .await;
    let window = &out["effective_window"];
    assert_eq!(window["to"], "2025-01-16T00:00:00.000Z");
    assert_eq!(window["from_defaulted"], true);
    assert_eq!(window["to_defaulted"], true);
    assert_eq!(window["to_clamped"], true);
    assert_eq!(out["span"]["from_defaulted"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_snapshot_inside_the_search_span_echoes_no_clamp() {
    // The freeze opens AFTER the fixture's whole range: nothing reaches the
    // holdout, so the guard must not move the defaulted bound and the echo must
    // say `clamped: false` with the snapshot's own bounds (the amended reading).
    let world = world().await;
    world.open_freeze(AFTER_FIXTURE_MS).await;
    let client = world.client().await;

    let out = call(
        &client,
        "run_backtest",
        json!({"version_id": world.child.as_str()}),
    )
    .await;
    let window = &out["effective_window"];
    assert_eq!(window["to"], "2025-01-31T23:59:59.999Z");
    assert_eq!(window["to_defaulted"], true);
    assert_eq!(window["to_clamped"], false);

    let out = call(
        &client,
        "run_walk_forward",
        json!({"version_id": world.child.as_str(), "k": 2}),
    )
    .await;
    let window = &out["effective_window"];
    assert_eq!(window["to"], "2025-01-31T23:59:59.999Z");
    assert_eq!(window["to_defaulted"], true);
    assert_eq!(window["to_clamped"], false);
}

/// One window that clears the holdout runs with a freeze open — the guard's
/// pass arm (the `to` is exclusive, so ending exactly at the holdout start is
/// legal).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_ending_at_the_holdout_start_runs() {
    let world = world().await;
    world.open_freeze(HOLDOUT_MS).await;
    let client = world.client().await;

    let mut args = arguments(&window_clearing_holdout());
    args.insert("version_id".to_owned(), json!(world.child.as_str()));
    let out = call(&client, "run_backtest", Value::Object(args)).await;
    assert!(out["run_id"].is_string(), "{out}");
    let (_, to) = recorded_window(&world, out["run_id"].as_str().unwrap()).await;
    assert_eq!(to, HOLDOUT_MS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_run_reads_withhold_holdout_results() {
    let world = world().await;
    let client = world.client().await;
    let mut args = arguments(&window_into_holdout());
    args.insert("version_id".to_owned(), json!(world.child.as_str()));
    let unsafe_run = call(&client, "run_backtest", Value::Object(args)).await;
    let mut args = arguments(&window_clearing_holdout());
    args.insert("version_id".to_owned(), json!(world.child.as_str()));
    let safe_run = call(&client, "run_backtest", Value::Object(args)).await;
    let wf = call(
        &client,
        "run_walk_forward",
        json!({
            "version_id": world.child.as_str(), "k": 2,
            "from": "2025-01-10T00:00:00Z", "to": "2025-01-20T00:00:00Z"
        }),
    )
    .await;
    let full_run = call(
        &client,
        "run_backtest",
        json!({"version_id": world.child.as_str()}),
    )
    .await;
    let before = call(&client, "get_run", json!({"run_id": safe_run["run_id"]})).await;
    world.open_freeze(HOLDOUT_MS).await;
    for name in ["get_run", "export_trades"] {
        let err = call_err(&client, name, json!({"run_id": unsafe_run["run_id"]})).await;
        assert_eq!(err["field"], "run_id");
        assert!(err["message"].as_str().unwrap().contains("BTCUSDT"));
        assert!(err["message"].as_str().unwrap().contains("2025-01-16"));
    }
    let err = call_err(
        &client,
        "get_walk_forward_run",
        json!({"walk_forward_run_id": wf["walk_forward_run_id"]}),
    )
    .await;
    assert_eq!(err["field"], "run_id");
    let list = call(
        &client,
        "list_runs",
        json!({"version_id": world.child.as_str()}),
    )
    .await;
    call_err(&client, "get_run", json!({"run_id": full_run["run_id"]})).await;
    assert_eq!(list["withheld_for_holdout"], 3);
    assert_eq!(list["runs"].as_array().unwrap().len(), 2);
    assert_eq!(
        call(&client, "get_run", json!({"run_id": safe_run["run_id"]})).await,
        before
    );
    freeze_repo(&world.db, CLOSED_MS).close().await.unwrap();
    call(&client, "get_run", json!({"run_id": unsafe_run["run_id"]})).await;
    let list = call(
        &client,
        "list_runs",
        json!({"version_id": world.child.as_str()}),
    )
    .await;
    assert!(list.get("withheld_for_holdout").is_none());
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coach_refuses_parent_window_before_snapshot_load() {
    let gate_world = coach_gate_support::world_with_window(Some(
        pulse::CandleWindow::new(FIXTURE_FIRST_OPEN_MS, HOLDOUT_MS + 900_000).unwrap(),
    ))
    .await;
    let before = gate_world.table_count("backtest_run").await;
    // A missing pinned snapshot would fail at LoadSnapshots if preparation ran.
    let missing = TempDir::new().unwrap();
    let missing_store = pulse::CandleStore::with_base_dir(missing.path().join("missing"));
    let outcome = pulse::run_coach_decision(
        &gate_world.strategies(),
        &missing_store,
        &pulse::BinanceAdapter::new(),
        &gate_world.runs(),
        &gate_world.acceptance(),
        &gate_world.sessions(),
        pulse::CoachDecisionRequest {
            session_id: gate_world.session_id.clone(),
            action: CoachAction::Accept,
        },
        Some(HoldoutFreeze {
            holdout_start_ms: HOLDOUT_MS,
        }),
    )
    .await
    .unwrap();
    match outcome {
        CoachDecisionOutcome::AcceptFailed(proposal) => {
            let failure = proposal.accept_failure.unwrap();
            assert_eq!(failure.stage, AcceptFailureStage::WalkForward);
            assert!(failure.message.contains("BTCUSDT"));
            assert!(failure.message.contains("2025-01-16"));
        }
        other => panic!("expected staged refusal, got {other:?}"),
    }
    assert_eq!(gate_world.table_count("backtest_run").await, before);
    assert_eq!(gate_world.table_count("strategy_version").await, 1);
}
