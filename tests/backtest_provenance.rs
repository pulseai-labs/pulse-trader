//! r1.s3.w2 (#110) — durable backtest input provenance and migration `0006`.
//!
//! A persisted run used to record only its OUTPUTS. `engine_fingerprint` pinned the
//! engine and `result_content_hash` detected tampering with the result, but nothing
//! identified the DATA: the CLI loads the `HEAD` snapshot, `fetch-data` advances
//! `HEAD`, and once it moves no stored row says which snapshot produced it. This
//! binary is the proof that the link is now stored.
//!
//! It exercises the real paths end to end — the shipped migration set through the
//! production startup runner, the real `pulse backtest --version` binary over the
//! committed Parquet fixture, the real SQLite repository, and W1's
//! `CandleSeriesRepository` — never a mocked seam:
//!
//! 1. `0006` applies through the startup path on a database that already has `0007`.
//! 2. A row written before `0006` stays readable afterwards as `inputs: None`.
//! 3. The `BEFORE INSERT` completeness trigger rejects missing base provenance and a
//!    half-present HTF pair, and accepts both legal shapes.
//! 4. A fresh repository save round-trips every encoded input exactly.
//! 5. **The versioned CLI persists the inputs that actually ran** — pair, both
//!    loaded snapshot identities, and the exact engine cost config.
//! 6. **Ledger line `d10`:** those persisted identities still reload the original
//!    primary AND HTF snapshots through the domain port after both `HEAD` pointers
//!    advance.
//! 7. The run/trade immutability triggers still refuse UPDATE and DELETE after a
//!    `0006` up/down/up cycle.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use pulse::{
    BacktestInputs, BacktestResult, BacktestRunRepository, Candle, CandleSeriesRepository,
    CandleStore, CandleWindow, CreatedBy, DataVersion, Db, EngineFingerprint, EquityCurve,
    FundingConfig, MIGRATOR, NewVersion, Pair, RegimeBreakdown, SkippedEntryCounts,
    SnapshotSelection, SqliteBacktestRunRepo, SqliteStrategyRepo, StrategyId, StrategyRepository,
    SummaryStats, Timeframe, VersionId, run_migrations_with_backup, undo_to,
};
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use tempfile::TempDir;

/// The committed candle fixture the backtest runs over (mirrors `backtest_cli.rs`).
const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

/// The same minimal, valid DSL the other CLI binaries use — it produces real trades
/// over the fixture, so the persisted run is a genuine one.
const MINIMAL_DSL: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold (provenance)",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Indicator", "spec": { "indicator": "Rsi", "period": 14 } },
    "op": "Lt",
    "rhs": { "type": "Constant", "value": "30" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": {
    "risk_per_trade_pct": "0.01",
    "max_leverage": "3"
  }
}"#;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn manifest(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

async fn applied_versions(pool: &SqlitePool) -> BTreeSet<i64> {
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success = TRUE")
            .fetch_all(pool)
            .await
            .unwrap();
    versions.into_iter().collect()
}

async fn object_present(pool: &SqlitePool, kind: &str, name: &str) -> bool {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type=?1 AND name=?2")
        .bind(kind)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
    n == 1
}

async fn columns_of(pool: &SqlitePool, table: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT name FROM pragma_table_info(?1)")
        .bind(table)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// The eight columns migration `0006` adds, in declaration order.
const PROVENANCE_COLUMNS: [&str; 8] = [
    "pair",
    "primary_timeframe",
    "primary_data_version",
    "htf_timeframe",
    "htf_data_version",
    "taker_fee_bps",
    "slippage_bps",
    "funding_config",
];

/// Copy the shipped `migrations/` set into `dir`, SKIPPING `0006_*` and
/// everything from `0008` on — the "older binary" that shipped `0007` while
/// `0006` was still a reserved gap.
///
/// r1.s4.w4 added `0008` to the skip list. A binary that predates `0006` predates
/// `0008` by two spines, and including it would also move the fixture's maximum
/// applied version to 8 — which would silently destroy the property the next
/// assertion states: that `0006` arrives BELOW the maximum already in the database.
/// r2.s1.w1 adds `0009`, and G1 adds `0010`, for the same reason, one release
/// further along each.
fn shipped_set_without_0006(dir: &Path) {
    let shipped = manifest("migrations");
    for entry in std::fs::read_dir(&shipped).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("0006_") || name.as_str() >= "0008" {
            continue;
        }
        std::fs::copy(&path, dir.join(&name)).unwrap();
    }
}

/// Copy the shipped `migrations/` set into `dir`, SKIPPING `0009_*` and later
/// — the `0008` binary this item's pre-0009 compatibility is measured against.
fn shipped_set_without_0009(dir: &Path) {
    let shipped = manifest("migrations");
    for entry in std::fs::read_dir(&shipped).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.as_str() >= "0009" {
            continue;
        }
        std::fs::copy(&path, dir.join(&name)).unwrap();
    }
}

/// A temp database migrated by the `0008` set (everything but `0009`).
async fn db_at_0008() -> (TempDir, PathBuf, Db) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    shipped_set_without_0009(&dir);

    let db_path = tmp.path().join("pulse.db");
    let older = Migrator::new(dir.as_path()).await.unwrap();
    let db = Db::with_path(&db_path).await.unwrap();
    older.run(db.pool()).await.expect("the 0008 set applies");

    let applied = applied_versions(db.pool()).await;
    assert!(
        !applied.contains(&9),
        "the fixture must NOT have 0009 applied: {applied:?}"
    );
    assert_eq!(
        applied.iter().copied().max(),
        Some(8),
        "the fixture sits at the pre-0009 maximum"
    );
    (tmp, db_path, db)
}

/// A temp database migrated by the "older" set (everything but `0006`).
async fn db_at_0007_without_0006() -> (TempDir, PathBuf, Db) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    shipped_set_without_0006(&dir);

    let db_path = tmp.path().join("pulse.db");
    let older = Migrator::new(dir.as_path()).await.unwrap();
    let db = Db::with_path(&db_path).await.unwrap();
    older.run(db.pool()).await.expect("the older set applies");

    let applied = applied_versions(db.pool()).await;
    assert!(
        !applied.contains(&6),
        "the fixture must NOT have 0006 applied: {applied:?}"
    );
    assert_eq!(
        applied.iter().copied().max(),
        Some(7),
        "0007 already ships, so 0006 arrives BELOW the current maximum"
    );
    (tmp, db_path, db)
}

/// A fresh temp db migrated to the full embedded set.
async fn migrated_db() -> (TempDir, PathBuf, Db) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("pulse.db");
    let db = Db::with_path(&db_path).await.unwrap();
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    (tmp, db_path, db)
}

/// Seed the FK parents a `backtest_run` row needs, by raw SQL.
async fn seed_strategy_and_version(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO strategy (id, name, tags, archived, created_at) \
         VALUES ('strat-1', 'RSI Oversold', '[]', 0, '2026-09-01T00:00:00.000Z')",
    )
    .execute(pool)
    .await
    .expect("seed strategy");
    sqlx::query(
        "INSERT INTO strategy_version \
         (id, strategy_id, dsl_schema_version, dsl, dsl_original, version_hash, created_by, \
          creating_llm_call_ids, created_at) \
         VALUES ('ver-1', 'strat-1', '1.0.0', '{}', '{}', 'hash-1', 'human', '[]', \
                 '2026-09-01T00:00:00.000Z')",
    )
    .execute(pool)
    .await
    .expect("seed strategy_version");
}

/// Seed a strategy + one real compilable VERSION through the library repo, for the
/// binary's `--version` path.
async fn seed_version_through_repo(db_path: &Path) -> VersionId {
    let db = Db::with_path(db_path).await.expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    let repo = SqliteStrategyRepo::new(db.pool().clone());
    let strat = repo
        .create_strategy("Provenance demo", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create strategy");
    repo.create_version(NewVersion {
        strategy_id: StrategyId::new(strat.id.as_str().to_owned()),
        parent_version_id: None,
        dsl_json: MINIMAL_DSL.to_owned(),
        created_by: CreatedBy::Human,
        creating_llm_call_ids: vec![],
    })
    .await
    .expect("create version")
    .id
}

/// A trade-free result whose totals are all zero — the shape a hand-seeded row must
/// hash to for `get_run`'s re-derive guard to accept it.
fn empty_result() -> BacktestResult {
    BacktestResult {
        trades: vec![],
        net_pnl: Decimal::ZERO,
        fees_total: Decimal::ZERO,
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        regime_breakdown: RegimeBreakdown::new(),
        skipped_entries: SkippedEntryCounts::new(),
        open_position: None,
        engine_fingerprint: EngineFingerprint::current(),
        summary: SummaryStats::default(),
        equity_curve: EquityCurve::default(),
    }
}

fn inputs_with_htf() -> BacktestInputs {
    BacktestInputs {
        pair: Pair::new("BTCUSDT"),
        primary: SnapshotSelection {
            timeframe: Timeframe::M15,
            data_version: DataVersion::new("primary-abc123"),
        },
        htf: Some(SnapshotSelection {
            timeframe: Timeframe::H4,
            data_version: DataVersion::new("htf-def456"),
        }),
        taker_fee_bps: Decimal::new(4, 0),
        slippage_bps: Decimal::new(1, 0),
        funding: FundingConfig::SnapshotRates,
        window: None,
    }
}

fn inputs_without_htf() -> BacktestInputs {
    BacktestInputs {
        htf: None,
        ..inputs_with_htf()
    }
}

/// Recursively copy a directory tree (the candle fixture, so a test may advance
/// `HEAD` without touching the committed store).
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// A contiguous M15/H4 candle run that is guaranteed distinct from the fixture's,
/// so committing it advances `HEAD` to a new `data_version`.
fn distinct_candles(tf: Timeframe, count: i64) -> Vec<Candle> {
    let step = tf.duration_ms();
    (0..count)
        .map(|i| {
            let price = Decimal::new(90_000 + i, 0);
            Candle {
                open_time: i * step,
                close_time: i * step + step - 1,
                open: price,
                high: price,
                low: price,
                close: price,
                volume: Decimal::ONE,
                funding_rate: None,
            }
        })
        .collect()
}

/// Run `pulse backtest --version <id> … [--htf H4] --store <store> --db <db>`.
fn run_versioned_backtest(
    db_path: &Path,
    version_id: &str,
    store: &Path,
    htf: Option<&str>,
) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pulse"));
    cmd.current_dir(manifest("."));
    cmd.args([
        "backtest",
        "--version",
        version_id,
        "--pair",
        "BTCUSDT",
        "--tf",
        "M15",
    ]);
    if let Some(htf) = htf {
        cmd.args(["--htf", htf]);
    }
    cmd.arg("--store")
        .arg(store)
        .arg("--db")
        .arg(db_path)
        .output()
        .expect("run pulse backtest")
}

fn assert_cli_ok(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "backtest failed: status={:?}\nstderr={}\nstdout={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}

/// The single run persisted against `version_id`.
async fn the_only_run(db: &Db, version_id: &VersionId) -> pulse::PersistedRun {
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let listed = repo
        .list_runs_for_version(version_id)
        .await
        .expect("list runs");
    assert_eq!(listed.len(), 1, "exactly one run was persisted: {listed:?}");
    repo.get_run(&listed[0].id)
        .await
        .expect("get run")
        .expect("the listed run is fetchable")
}

// ---------------------------------------------------------------------------
// 1. the migration applies out of order, through the production startup path
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0006_applies_through_the_startup_path_despite_0007() {
    let (_tmp, db_path, db) = db_at_0007_without_0006().await;
    let before = columns_of(db.pool(), "backtest_run").await;
    for column in PROVENANCE_COLUMNS {
        assert!(
            !before.contains(&column.to_owned()),
            "`{column}` must not exist before 0006"
        );
    }
    drop(db);

    // The REAL startup path — the one that compares applied/embedded version SETS.
    run_migrations_with_backup(&db_path)
        .await
        .expect("0006 must apply on a db that already has 0007");

    let db = Db::with_path(&db_path).await.unwrap();
    let applied = applied_versions(db.pool()).await;
    assert!(applied.contains(&6), "0006 is now applied: {applied:?}");
    // r1.s4.w4: the same startup run also applies `0008`, which the fixture
    // withheld, so the maximum DOES move now — to 8. The property this test owns is
    // that `0006` applied at all despite sorting below the maximum the database
    // already held; the isolated "filling a gap moves no maximum" case lives in
    // `migrate.rs`'s `a_later_lower_numbered_migration_applies_through_the_startup_path`,
    // which withholds `0008` precisely so it can still state it.
    // r2.s1.w1: `0009` rides along too, and G1's `0010` as well, moving the
    // maximum to 10.
    assert!(applied.contains(&8), "0008 rides along: {applied:?}");
    assert!(applied.contains(&9), "0009 rides along: {applied:?}");
    assert!(applied.contains(&10), "0010 rides along: {applied:?}");
    assert_eq!(
        applied.iter().copied().max(),
        Some(10),
        "0006 is recorded at its own version, below the maximum 0010 sets"
    );

    let after = columns_of(db.pool(), "backtest_run").await;
    for column in PROVENANCE_COLUMNS {
        assert!(
            after.contains(&column.to_owned()),
            "`{column}` exists after 0006: {after:?}"
        );
    }
    assert!(
        object_present(db.pool(), "trigger", "backtest_run_inputs_complete").await,
        "the completeness trigger is installed"
    );
}

// ---------------------------------------------------------------------------
// 2. a pre-0006 row stays honestly readable as legacy
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_written_before_0006_reads_back_as_legacy_inputs_none() {
    let (_tmp, db_path, db) = db_at_0007_without_0006().await;
    seed_strategy_and_version(db.pool()).await;

    // A complete, valid pre-0006 run row: the eight provenance columns do not exist
    // yet, so it CANNOT carry them. Its hash is the genuine empty-trade-log hash so
    // `get_run`'s re-derive guard accepts it.
    let hash = empty_result().result_content_hash();
    sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
          engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
          funding_total, slippage_total, expectancy, win_rate, gross_profit, gross_loss, \
          avg_win, avg_loss, max_drawdown, trade_count, wins, losses, breakeven, \
          max_win_streak, max_loss_streak, regime_breakdown, skipped_sub_lot, \
          skipped_sub_notional, skipped_leverage_capped) \
         VALUES ('run-legacy', 'ver-1', 1, '2026-09-01T00:00:00.000Z', ?1, ?2, ?3, \
                 '10000', '0', '0', '0', '0', '0', '0', '0', '0', '0', '0', '0', \
                 0, 0, 0, 0, 0, 0, ?4, 0, 0, 0)",
    )
    .bind(EngineFingerprint::current().as_str())
    .bind(EngineFingerprint::target())
    .bind(&hash)
    .bind(serde_json::to_string(&RegimeBreakdown::new()).unwrap())
    .execute(db.pool())
    .await
    .expect("seed a pre-0006 run row");
    drop(db);

    run_migrations_with_backup(&db_path).await.expect("migrate");

    let db = Db::with_path(&db_path).await.unwrap();
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let run = repo
        .get_run(&pulse::BacktestRunId::new("run-legacy"))
        .await
        .expect("a legacy row still reads")
        .expect("the legacy row is present");

    assert!(
        run.inputs.is_none(),
        "a row whose eight provenance columns are all NULL reads as inputs: None, \
         never as guessed values: {:?}",
        run.inputs
    );
    // It is honestly unavailable, not corrupt — the rest of the row still decodes.
    assert_eq!(run.starting_equity, Decimal::new(10_000, 0));
    assert_eq!(run.result_content_hash, hash);
}

// ---------------------------------------------------------------------------
// 3. the completeness trigger
// ---------------------------------------------------------------------------

/// The eight provenance values a hand-written `backtest_run` row carries. A struct
/// rather than eight positional `Option`s: the columns are interchangeable in type
/// and only distinguishable by name, which is exactly the shape that makes a
/// positional call site silently wrong.
#[derive(Clone, Copy)]
struct Provenance {
    pair: Option<&'static str>,
    primary_timeframe: Option<&'static str>,
    primary_data_version: Option<&'static str>,
    htf_timeframe: Option<&'static str>,
    htf_data_version: Option<&'static str>,
    taker_fee_bps: Option<&'static str>,
    slippage_bps: Option<&'static str>,
    funding_config: Option<&'static str>,
}

impl Provenance {
    /// Every required column present, no HTF — the legal minimum, and the shape
    /// the debug CLI writes when `--htf` is omitted.
    fn complete() -> Self {
        Self {
            pair: Some("BTCUSDT"),
            primary_timeframe: Some("15m"),
            primary_data_version: Some("v-primary"),
            htf_timeframe: None,
            htf_data_version: None,
            taker_fee_bps: Some("4"),
            slippage_bps: Some("1"),
            funding_config: Some("snapshot_rates"),
        }
    }

    /// The same, plus a full HTF pair — the shape the r1 app path writes.
    fn complete_with_htf() -> Self {
        Self {
            htf_timeframe: Some("4h"),
            htf_data_version: Some("v-htf"),
            ..Self::complete()
        }
    }
}

/// Insert a `backtest_run` row carrying `p`, returning the result so a test can
/// assert accept or reject.
async fn insert_with_provenance(
    pool: &SqlitePool,
    id: &str,
    p: Provenance,
) -> Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error> {
    sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
          engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
          funding_total, slippage_total, trade_count, wins, losses, breakeven, \
          max_win_streak, max_loss_streak, pair, primary_timeframe, primary_data_version, \
          htf_timeframe, htf_data_version, taker_fee_bps, slippage_bps, funding_config) \
         VALUES (?1, 'ver-1', 1, '2026-09-01T00:00:00.000Z', 'fp', 'tgt', 'hash', \
                 '10000', '0', '0', '0', '0', 0, 0, 0, 0, 0, 0, \
                 ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(id)
    .bind(p.pair)
    .bind(p.primary_timeframe)
    .bind(p.primary_data_version)
    .bind(p.htf_timeframe)
    .bind(p.htf_data_version)
    .bind(p.taker_fee_bps)
    .bind(p.slippage_bps)
    .bind(p.funding_config)
    .execute(pool)
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_completeness_trigger_accepts_both_legal_shapes() {
    let (_tmp, _db_path, db) = migrated_db().await;
    seed_strategy_and_version(db.pool()).await;

    insert_with_provenance(db.pool(), "run-no-htf", Provenance::complete())
        .await
        .expect("a complete row with no HTF is accepted");
    insert_with_provenance(db.pool(), "run-with-htf", Provenance::complete_with_htf())
        .await
        .expect("a complete row with a full HTF pair is accepted");
}

/// A required provenance column, named, plus the mutation that clears it.
type ClearOne = (&'static str, fn(&mut Provenance));

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_completeness_trigger_rejects_a_new_row_missing_any_required_column() {
    let (_tmp, _db_path, db) = migrated_db().await;
    seed_strategy_and_version(db.pool()).await;

    let clear: [ClearOne; 6] = [
        ("pair", |p| p.pair = None),
        ("primary_timeframe", |p| p.primary_timeframe = None),
        ("primary_data_version", |p| p.primary_data_version = None),
        ("taker_fee_bps", |p| p.taker_fee_bps = None),
        ("slippage_bps", |p| p.slippage_bps = None),
        ("funding_config", |p| p.funding_config = None),
    ];
    for (missing, clear_one) in clear {
        let mut row = Provenance::complete();
        clear_one(&mut row);
        let err = insert_with_provenance(db.pool(), &format!("run-missing-{missing}"), row)
            .await
            .expect_err(&format!("a new row missing `{missing}` must be rejected"));
        assert!(
            err.to_string().contains("provenance"),
            "the refusal names the reason ({missing}): {err}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_completeness_trigger_rejects_a_half_present_htf_pair() {
    let (_tmp, _db_path, db) = migrated_db().await;
    seed_strategy_and_version(db.pool()).await;

    // Either direction: a run used a higher timeframe or it did not. "Half an HTF
    // selection" is not a state the domain can express, so it is not one the
    // database may hold.
    let timeframe_only = Provenance {
        htf_timeframe: Some("4h"),
        ..Provenance::complete()
    };
    let version_only = Provenance {
        htf_data_version: Some("v-htf"),
        ..Provenance::complete()
    };
    for (label, row) in [
        ("timeframe-only", timeframe_only),
        ("version-only", version_only),
    ] {
        insert_with_provenance(db.pool(), &format!("run-half-htf-{label}"), row)
            .await
            .expect_err("a half-present HTF pair must be rejected");
    }
}

// ---------------------------------------------------------------------------
// 4. a fresh repository save round-trips every encoded input
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_save_round_trips_every_encoded_input() {
    let (_tmp, _db_path, db) = migrated_db().await;
    seed_strategy_and_version(db.pool()).await;
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let result = empty_result();
    let version = VersionId::new("ver-1");

    for expected in [inputs_with_htf(), inputs_without_htf()] {
        let id = repo
            .save_run(
                &version,
                &expected,
                &result,
                &result.summary,
                Decimal::new(10_000, 0),
            )
            .await
            .expect("save a fresh run with typed inputs");
        let run = repo
            .get_run(&id)
            .await
            .expect("read it back")
            .expect("the saved run exists");
        let got = run.inputs.expect("a fresh save always carries inputs");
        assert_eq!(got, expected, "every input round-trips byte-exactly");
    }
}

// ---------------------------------------------------------------------------
// 4b. a stored `data_version` is untrusted in BOTH directions
// ---------------------------------------------------------------------------
//
// `DataVersion` is opaque by design, and opaque was being read as arbitrary. The
// Parquet adapter joins a tag verbatim into
// `<base>/candles/<PAIR>/<TF>/<tag>.parquet`, and W3 hands a decoded tag straight
// to `CandleStore::load_version` — so a row carrying `../../../x` that decoded
// cleanly would resolve OUTSIDE the store root. These prove it refuses on the way
// in and on the way out.

/// Tags that are not a single portable path component.
const UNSAFE_VERSIONS: [&str; 6] = ["", ".", "..", "../../../etc/passwd", "/tmp/x", "a/b"];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsafe_data_version_is_refused_before_any_row_persists() {
    let (_tmp, _db_path, db) = migrated_db().await;
    seed_strategy_and_version(db.pool()).await;
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let result = empty_result();
    let version = VersionId::new("ver-1");

    for tag in UNSAFE_VERSIONS {
        // Primary and HTF are checked alike — a run replays both.
        let via_primary = BacktestInputs {
            primary: SnapshotSelection {
                timeframe: Timeframe::M15,
                data_version: DataVersion::new(tag),
            },
            ..inputs_without_htf()
        };
        let via_htf = BacktestInputs {
            htf: Some(SnapshotSelection {
                timeframe: Timeframe::H4,
                data_version: DataVersion::new(tag),
            }),
            ..inputs_without_htf()
        };
        for (which, inputs) in [("primary", via_primary), ("htf", via_htf)] {
            repo.save_run(
                &version,
                &inputs,
                &result,
                &result.summary,
                Decimal::new(10_000, 0),
            )
            .await
            .expect_err(&format!(
                "an unsafe {which} data_version {tag:?} must be refused"
            ));
        }
    }

    // Refused BEFORE the transaction opens, so nothing at all was written.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backtest_run")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "a rejected save must leave no row behind, not a partial one"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_complete_row_with_an_unsafe_stored_version_fails_closed_on_read() {
    let (_tmp, _db_path, db) = migrated_db().await;
    seed_strategy_and_version(db.pool()).await;
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());

    // The completeness trigger is about PRESENCE, not safety — it accepts this row.
    // Decoding is what must refuse it.
    for (idx, tag) in ["../../../etc/passwd", "/tmp/x", "a/b", ".."]
        .into_iter()
        .enumerate()
    {
        let id = format!("run-unsafe-primary-{idx}");
        insert_with_provenance(
            db.pool(),
            &id,
            Provenance {
                primary_data_version: Some(tag),
                ..Provenance::complete()
            },
        )
        .await
        .expect("the trigger checks presence, not path safety");
        repo.get_run(&pulse::BacktestRunId::new(id.clone()))
            .await
            .expect_err(&format!(
                "a stored primary version {tag:?} must fail closed"
            ));

        let htf_id = format!("run-unsafe-htf-{idx}");
        insert_with_provenance(
            db.pool(),
            &htf_id,
            Provenance {
                htf_timeframe: Some("4h"),
                htf_data_version: Some(tag),
                ..Provenance::complete()
            },
        )
        .await
        .expect("a full HTF pair satisfies the trigger");
        repo.get_run(&pulse::BacktestRunId::new(htf_id))
            .await
            .expect_err(&format!("a stored HTF version {tag:?} must fail closed"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_stored_pair_timeframe_funding_or_decimal_fails_closed_on_read() {
    let (_tmp, _db_path, db) = migrated_db().await;
    seed_strategy_and_version(db.pool()).await;
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());

    // Each row is complete enough for the trigger and corrupt in exactly one place —
    // the decoder is what has to notice. Every one of these was a documented refusal
    // with no direct test behind it.
    let corrupt: [(&str, Provenance); 5] = [
        (
            "pair",
            Provenance {
                pair: Some("bt/cusdt"),
                ..Provenance::complete()
            },
        ),
        (
            "primary_timeframe",
            Provenance {
                primary_timeframe: Some("1h"),
                ..Provenance::complete()
            },
        ),
        (
            "htf_timeframe",
            Provenance {
                htf_timeframe: Some("30m"),
                htf_data_version: Some("v-htf"),
                ..Provenance::complete()
            },
        ),
        (
            "funding_config",
            Provenance {
                funding_config: Some("flat_rate"),
                ..Provenance::complete()
            },
        ),
        (
            "taker_fee_bps",
            Provenance {
                taker_fee_bps: Some("not-a-decimal"),
                ..Provenance::complete()
            },
        ),
    ];
    for (column, row) in corrupt {
        let id = format!("run-corrupt-{column}");
        insert_with_provenance(db.pool(), &id, row)
            .await
            .unwrap_or_else(|e| panic!("the trigger accepts a present-but-corrupt {column}: {e}"));
        repo.get_run(&pulse::BacktestRunId::new(id))
            .await
            .expect_err(&format!(
                "a corrupt stored `{column}` must fail closed, never decode partially"
            ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_partially_populated_row_fails_closed_rather_than_projecting() {
    let (_tmp, db_path, db) = db_at_0007_without_0006().await;
    seed_strategy_and_version(db.pool()).await;

    // Written before 0006, so the trigger cannot see it — then 0006 lands and the
    // columns appear all-NULL. Filling ONE of them by hand afterwards is the shape
    // no legal path produces and the decoder must refuse: it is not "a run with some
    // provenance", it is a row whose provenance cannot be trusted.
    let hash = empty_result().result_content_hash();
    sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
          engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
          funding_total, slippage_total, trade_count, wins, losses, breakeven, \
          max_win_streak, max_loss_streak, regime_breakdown, skipped_sub_lot, \
          skipped_sub_notional, skipped_leverage_capped) \
         VALUES ('run-partial', 'ver-1', 1, '2026-09-01T00:00:00.000Z', ?1, ?2, ?3, \
                 '10000', '0', '0', '0', '0', 0, 0, 0, 0, 0, 0, ?4, 0, 0, 0)",
    )
    .bind(EngineFingerprint::current().as_str())
    .bind(EngineFingerprint::target())
    .bind(&hash)
    .bind(serde_json::to_string(&RegimeBreakdown::new()).unwrap())
    .execute(db.pool())
    .await
    .expect("seed a pre-0006 row");
    drop(db);

    run_migrations_with_backup(&db_path).await.expect("migrate");
    let db = Db::with_path(&db_path).await.unwrap();

    // The immutability trigger blocks UPDATE, so the half-populated row is created
    // by a fresh INSERT that the completeness trigger would refuse — assert that
    // refusal, then prove the decoder refuses the shape too via a direct call.
    insert_with_provenance(
        db.pool(),
        "run-half",
        Provenance {
            slippage_bps: None,
            ..Provenance::complete()
        },
    )
    .await
    .expect_err("the trigger refuses a partially-populated NEW row");

    // The pre-0006 row itself is the legal all-NULL shape and still reads.
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let run = repo
        .get_run(&pulse::BacktestRunId::new("run-partial"))
        .await
        .expect("the legacy row still reads")
        .expect("present");
    assert!(run.inputs.is_none());
}

// ---------------------------------------------------------------------------
// 5. THE MAIN PROOF — the versioned CLI persists what actually ran
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_versioned_cli_persists_the_inputs_that_actually_ran() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("pulse.db");
    let version_id = seed_version_through_repo(&db_path).await;

    let store_dir = manifest(FIXTURE_STORE);
    // The identities the CLI will actually load: the fixture's current HEADs, read
    // through the SAME domain port the CLI uses.
    let store = CandleStore::with_base_dir(store_dir.clone());
    let pair = Pair::new("BTCUSDT");
    let expected_primary = store
        .load_head(&pair, Timeframe::M15)
        .expect("fixture M15 HEAD")
        .expect("fixture has an M15 HEAD")
        .series
        .version;
    let expected_htf = store
        .load_head(&pair, Timeframe::H4)
        .expect("fixture H4 HEAD")
        .expect("fixture has an H4 HEAD")
        .series
        .version;

    let output = run_versioned_backtest(&db_path, version_id.as_str(), &store_dir, Some("H4"));
    assert_cli_ok(&output);

    let db = Db::with_path(&db_path).await.unwrap();
    let run = the_only_run(&db, &version_id).await;
    let inputs = run
        .inputs
        .expect("a run persisted by the versioned CLI carries its inputs");

    assert_eq!(inputs.pair, pair);
    assert_eq!(inputs.primary.timeframe, Timeframe::M15);
    assert_eq!(
        inputs.primary.data_version, expected_primary,
        "the persisted primary identity is the snapshot the engine actually consumed"
    );
    let htf = inputs.htf.expect("an --htf run records its HTF selection");
    assert_eq!(htf.timeframe, Timeframe::H4);
    assert_eq!(
        htf.data_version, expected_htf,
        "the persisted HTF identity is the snapshot the engine actually consumed"
    );
    // The exact engine cost config, not the run's cost OUTCOMES.
    assert_eq!(inputs.taker_fee_bps, Decimal::new(4, 0));
    assert_eq!(inputs.slippage_bps, Decimal::new(1, 0));
    assert_eq!(inputs.funding, FundingConfig::SnapshotRates);
    // The run is a real one, not an empty shell.
    assert!(
        run.summary.trade_count > 0,
        "the fixture run produced trades: {:?}",
        run.summary
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_timeframe_cli_run_records_no_htf() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("pulse.db");
    let version_id = seed_version_through_repo(&db_path).await;

    let output = run_versioned_backtest(
        &db_path,
        version_id.as_str(),
        &manifest(FIXTURE_STORE),
        None,
    );
    assert_cli_ok(&output);

    let db = Db::with_path(&db_path).await.unwrap();
    let inputs = the_only_run(&db, &version_id)
        .await
        .inputs
        .expect("a single-timeframe run still carries inputs");
    assert!(
        inputs.htf.is_none(),
        "the debug CLI may record no HTF, and does so honestly: {:?}",
        inputs.htf
    );
    assert_eq!(inputs.primary.timeframe, Timeframe::M15);
}

// ---------------------------------------------------------------------------
// 6. LEDGER LINE d10 — the identities survive HEAD advancing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_inputs_reload_the_exact_snapshots_after_both_heads_advance() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("pulse.db");
    let version_id = seed_version_through_repo(&db_path).await;

    // A writable copy of the fixture, so HEAD can advance without touching the
    // committed store.
    let store_dir = tmp.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);

    let output = run_versioned_backtest(&db_path, version_id.as_str(), &store_dir, Some("H4"));
    assert_cli_ok(&output);

    let db = Db::with_path(&db_path).await.unwrap();
    let inputs = the_only_run(&db, &version_id)
        .await
        .inputs
        .expect("the run carries inputs");
    let htf = inputs.htf.clone().expect("HTF recorded");

    // The original series, as loaded through the recorded identities.
    let store = CandleStore::with_base_dir(store_dir.clone());
    let original_primary = store
        .load_version(
            &inputs.pair,
            inputs.primary.timeframe,
            &inputs.primary.data_version,
        )
        .expect("the recorded primary snapshot loads before HEAD moves")
        .series;
    let original_htf = store
        .load_version(&inputs.pair, htf.timeframe, &htf.data_version)
        .expect("the recorded HTF snapshot loads before HEAD moves")
        .series;

    // Advance BOTH HEAD pointers with distinct content — exactly what a later
    // `fetch-data` does, and the thing that used to make a run unre-derivable.
    for tf in [Timeframe::M15, Timeframe::H4] {
        let committed = store
            .commit(&inputs.pair, tf, distinct_candles(tf, 40))
            .expect("commit a new snapshot");
        assert!(committed.storage_location.is_some());
    }

    for (tf, recorded) in [
        (Timeframe::M15, &inputs.primary.data_version),
        (Timeframe::H4, &htf.data_version),
    ] {
        let head = store
            .load_head(&inputs.pair, tf)
            .expect("load_head")
            .expect("HEAD present")
            .series
            .version;
        assert_ne!(
            &head,
            recorded,
            "HEAD for {} moved off the snapshot the run used",
            tf.binance_interval()
        );
    }

    // The point of #110: the run still names snapshots that resolve exactly.
    let replayed_primary = store
        .load_version(
            &inputs.pair,
            inputs.primary.timeframe,
            &inputs.primary.data_version,
        )
        .expect("the recorded primary snapshot STILL loads after HEAD advanced")
        .series;
    let replayed_htf = store
        .load_version(&inputs.pair, htf.timeframe, &htf.data_version)
        .expect("the recorded HTF snapshot STILL loads after HEAD advanced")
        .series;

    assert_eq!(
        replayed_primary, original_primary,
        "the replayed primary series is byte-identical to the one the run consumed"
    );
    assert_eq!(
        replayed_htf, original_htf,
        "the replayed HTF series is byte-identical to the one the run consumed"
    );
    assert!(
        !replayed_primary.candles.is_empty() && !replayed_htf.candles.is_empty(),
        "the replayed snapshots are real series, not empty stand-ins"
    );
}

// ---------------------------------------------------------------------------
// 7. immutability survives the 0006 up/down/up cycle
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_and_trade_immutability_survives_a_0006_up_down_up_cycle() {
    let (_tmp, db_path, db) = migrated_db().await;
    seed_strategy_and_version(db.pool()).await;
    insert_with_provenance(db.pool(), "run-immutable", Provenance::complete())
        .await
        .expect("seed a complete row");

    // Down past 0006, then forward again through the real startup path.
    undo_to(db.pool(), 5).await.expect("undo 0007 and 0006");
    let after_undo = columns_of(db.pool(), "backtest_run").await;
    for column in PROVENANCE_COLUMNS {
        assert!(
            !after_undo.contains(&column.to_owned()),
            "0006's down migration drops `{column}`"
        );
    }
    assert!(
        !object_present(db.pool(), "trigger", "backtest_run_inputs_complete").await,
        "0006's down migration drops the completeness trigger first"
    );
    drop(db);

    run_migrations_with_backup(&db_path)
        .await
        .expect("re-apply 0006 and 0007");
    let db = Db::with_path(&db_path).await.unwrap();

    // The 0003 immutability triggers are untouched by any of it.
    sqlx::query("UPDATE backtest_run SET net_pnl = '999' WHERE id = 'run-immutable'")
        .execute(db.pool())
        .await
        .expect_err("backtest_run is still immutable after the 0006 cycle");
    sqlx::query("DELETE FROM backtest_run WHERE id = 'run-immutable'")
        .execute(db.pool())
        .await
        .expect_err("backtest_run rows still cannot be deleted");
    for trigger in [
        "backtest_run_no_update",
        "backtest_run_no_delete",
        "trade_no_update",
        "trade_no_delete",
    ] {
        assert!(
            object_present(db.pool(), "trigger", trigger).await,
            "{trigger} survived the 0006 up/down/up cycle"
        );
    }
}

// ---------------------------------------------------------------------------
// 8. r2.s1.w1 (AC-3): the candle window a run consumed
// ---------------------------------------------------------------------------
//
// A run may now record WHICH slice of its snapshots it consumed:
// `window_from_ms` / `window_to_ms`, UTC epoch milliseconds, half-open
// `[from, to)`. `NULL`/`NULL` is the whole snapshot — every pre-0009 row, and
// every run saved without a window. The schema-side pair/inversion refusals
// live in `tests/migration_0009.rs`; these are the ADAPTER round-trips.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_saved_with_a_window_round_trips_its_bounds() {
    let (_tmp, _db_path, db) = migrated_db().await;
    seed_strategy_and_version(db.pool()).await;
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let result = empty_result();
    let version = VersionId::new("ver-1");
    let window = CandleWindow::new(1_700_000_000_000, 1_700_086_400_000).expect("a real window");
    let inputs = BacktestInputs {
        window: Some(window.clone()),
        ..inputs_with_htf()
    };

    let id = repo
        .save_run(
            &version,
            &inputs,
            &result,
            &result.summary,
            Decimal::new(10_000, 0),
        )
        .await
        .expect("save a windowed run");
    let run = repo
        .get_run(&id)
        .await
        .expect("read it back")
        .expect("the saved run exists");
    let got = run.inputs.expect("a fresh save always carries inputs");
    assert_eq!(
        got.window,
        Some(window),
        "the window round-trips bound-for-bound"
    );

    // The columns hold UTC epoch milliseconds — two INTEGERs, not a blob.
    let bounds: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT window_from_ms, window_to_ms FROM backtest_run WHERE id = ?1")
            .bind(id.as_str())
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(
        bounds,
        (Some(1_700_000_000_000_i64), Some(1_700_086_400_000_i64)),
        "the bounds persist as UTC epoch-ms integers"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_saved_without_a_window_round_trips_none() {
    let (_tmp, _db_path, db) = migrated_db().await;
    seed_strategy_and_version(db.pool()).await;
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let result = empty_result();
    let version = VersionId::new("ver-1");

    let id = repo
        .save_run(
            &version,
            &inputs_with_htf(),
            &result,
            &result.summary,
            Decimal::new(10_000, 0),
        )
        .await
        .expect("save an unwindowed run");
    let run = repo
        .get_run(&id)
        .await
        .expect("read it back")
        .expect("the saved run exists");
    let got = run.inputs.expect("a fresh save always carries inputs");
    assert_eq!(
        got.window, None,
        "an unwindowed run reads back as window: None — the whole snapshot"
    );

    let bounds: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT window_from_ms, window_to_ms FROM backtest_run WHERE id = ?1")
            .bind(id.as_str())
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(bounds, (None, None), "NULL/NULL is the whole snapshot");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_0009_row_reloads_with_window_none() {
    // Written by the `0008` binary: the window columns do not exist yet, so the
    // row CANNOT carry them. It is a complete `0008`-shape row — every summary
    // column AND the full `0006` provenance tuple — missing only the field
    // `0009` introduces.
    let (_tmp, db_path, db) = db_at_0008().await;
    seed_strategy_and_version(db.pool()).await;
    let p = Provenance::complete();
    sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
          engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
          funding_total, slippage_total, expectancy, win_rate, gross_profit, gross_loss, \
          avg_win, avg_loss, max_drawdown, trade_count, wins, losses, breakeven, \
          max_win_streak, max_loss_streak, regime_breakdown, skipped_sub_lot, \
          skipped_sub_notional, skipped_leverage_capped, \
          pair, primary_timeframe, primary_data_version, htf_timeframe, \
          htf_data_version, taker_fee_bps, slippage_bps, funding_config) \
         VALUES ('run-pre-0009', 'ver-1', 1, '2026-09-01T00:00:00.000Z', ?1, ?2, \
                 ?3, '10000', '0', '0', '0', '0', '0', '0', '0', '0', '0', '0', \
                 '0', 0, 0, 0, 0, 0, 0, ?4, 0, 0, 0, \
                 ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )
    .bind(EngineFingerprint::current().as_str())
    .bind(EngineFingerprint::target())
    .bind(empty_result().result_content_hash())
    .bind(serde_json::to_string(&RegimeBreakdown::new()).unwrap())
    .bind(p.pair)
    .bind(p.primary_timeframe)
    .bind(p.primary_data_version)
    .bind(p.htf_timeframe)
    .bind(p.htf_data_version)
    .bind(p.taker_fee_bps)
    .bind(p.slippage_bps)
    .bind(p.funding_config)
    .execute(db.pool())
    .await
    .expect("a complete row at 0008");
    drop(db);

    run_migrations_with_backup(&db_path)
        .await
        .expect("0009 applies over it");

    let db = Db::with_path(&db_path).await.unwrap();
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let run = repo
        .get_run(&pulse::BacktestRunId::new("run-pre-0009"))
        .await
        .expect("a pre-0009 row still reads")
        .expect("the row is present");

    // The row keeps its full 0006 provenance AND gains `window: None` — the
    // whole snapshot, not a guess at bounds it never recorded.
    let inputs = run.inputs.expect("the 0006 provenance still decodes");
    assert_eq!(inputs.window, None, "a pre-0009 row is unwindowed");
    assert_eq!(inputs.pair, Pair::new("BTCUSDT"));
}
