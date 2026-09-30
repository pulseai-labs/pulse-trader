//! r3.s2.w4 — AC-2: migration `0017_run_d1_provenance` (ADR-0010 / ADR-0018 /
//! ADR-0019, the `0015` precedent).
//!
//! `0017` adds ONE nullable column, `backtest_run.d1_data_version` — the fixed
//! daily-series slot's analogue of `0006`'s `htf_*` pair. Its value is the
//! content-hash tag of the D1 snapshot a `d1`-bearing strategy actually read, and
//! the timeframe is implicitly `1d`, so there is no `d1_timeframe` column to pair
//! it with and no half-present state for a trigger to refuse.
//!
//! **Why raw SQL.** As with `0011`/`0013`/`0014`, the value is in the states the
//! schema holds and the states the DOWN migration refuses: a legacy row that must
//! read `None` rather than a guess, and a recorded daily version that `0016`
//! cannot hold. The adapter-level round trip lives in `tests/backtest_provenance.rs`.
//!
//! Offline (`SQLX_OFFLINE=true` + the in-process `MIGRATOR`), `TempDir`-isolated.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{BacktestRunId, BacktestRunRepository, Db, MIGRATOR, SqliteBacktestRunRepo, undo_to};
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// helpers — databases at 0016 and at 0017
// ---------------------------------------------------------------------------

/// Every successfully-applied migration version.
async fn applied_versions(pool: &SqlitePool) -> BTreeSet<i64> {
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success = TRUE")
            .fetch_all(pool)
            .await
            .unwrap();
    versions.into_iter().collect()
}

async fn applied_max(pool: &SqlitePool) -> i64 {
    applied_versions(pool).await.into_iter().max().unwrap_or(0)
}

/// The column names of `table`, via `pragma_table_info`.
async fn columns_of(pool: &SqlitePool, table: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT name FROM pragma_table_info(?1)")
        .bind(table)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Whether `backtest_run.d1_data_version` is nullable — the whole point of the
/// column (a pre-`0017` row has no daily snapshot to name, and ADR-0018 forbids
/// inventing one).
async fn d1_column_is_nullable(pool: &SqlitePool) -> Option<bool> {
    let notnull: i64 = sqlx::query_scalar(
        "SELECT \"notnull\" FROM pragma_table_info('backtest_run') WHERE name = 'd1_data_version'",
    )
    .fetch_optional(pool)
    .await
    .unwrap()?;
    Some(notnull == 0)
}

/// Copy the shipped `migrations/` set into `dir`, SKIPPING `0017_*` — the
/// binary that shipped `0016`.
fn shipped_set_without_0017(dir: &Path) {
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(&shipped).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.as_str() >= "0017" {
            continue;
        }
        std::fs::copy(&path, dir.join(&name)).unwrap();
    }
}

/// A fresh temp database migrated by the "older" set (everything but `0017`).
async fn db_at_0016() -> (TempDir, PathBuf, Db) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    shipped_set_without_0017(&dir);

    let db_path = tmp.path().join("pulse.db");
    let older = Migrator::new(dir.as_path()).await.unwrap();
    let db = Db::with_path(&db_path).await.unwrap();
    older.run(db.pool()).await.expect("the older set applies");

    let applied = applied_versions(db.pool()).await;
    assert!(
        !applied.contains(&17),
        "the fixture must NOT have 0017 applied: {applied:?}"
    );
    assert_eq!(
        applied.iter().copied().max(),
        Some(16),
        "the fixture sits at the pre-0017 maximum"
    );
    (tmp, db_path, db)
}

/// A fresh temp database at the full embedded set (0017 included), with the FK
/// parents a `backtest_run` row needs.
async fn db_at_0017() -> (TempDir, Db) {
    let tmp = TempDir::new().unwrap();
    let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
    MIGRATOR.run(db.pool()).await.expect("run embedded set");
    seed_parents(db.pool()).await;
    (tmp, db)
}

/// The FK parents a `backtest_run` row needs: one strategy, one version.
async fn seed_parents(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO strategy (id, name, tags, archived, created_at) \
         VALUES ('strat-1', 'RSI Oversold', '[]', 0, '2026-08-29T00:00:00.000Z')",
    )
    .execute(pool)
    .await
    .expect("seed strategy");

    sqlx::query(
        "INSERT INTO strategy_version \
         (id, strategy_id, parent_version_id, dsl_schema_version, dsl, dsl_original, \
          version_hash, created_by, creating_llm_call_ids, created_at) \
         VALUES ('ver-1', 'strat-1', NULL, '1.0.0', '{}', '{}', 'hash-1', '\"human\"', '[]', \
                 '2026-08-29T00:00:00.000Z')",
    )
    .execute(pool)
    .await
    .expect("seed version");
}

/// The columns every `backtest_run` row carries — the `0006` provenance set,
/// `0012`'s window trio and `0013`'s membership pair (NULL here: nobody's fold).
///
/// `d1_data_version: None` writes the **0016-shaped** statement, deliberately:
/// that is exactly the legacy row this suite needs to build on a pre-`0017`
/// database (where the column does not exist yet) and to prove reads back as
/// `None` afterwards. `Some(tag)` writes the `0017` shape, which only a migrated
/// database accepts.
async fn seed_run(pool: &SqlitePool, run: &str, d1_data_version: Option<&str>) {
    let sql = match d1_data_version {
        None => {
            "INSERT INTO backtest_run \
             (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
              engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
              funding_total, slippage_total, pair, primary_timeframe, primary_data_version, \
              taker_fee_bps, slippage_bps, funding_config, \
              trade_count, wins, losses, breakeven, max_win_streak, max_loss_streak, \
              skipped_sub_lot, skipped_sub_notional, skipped_leverage_capped) \
             VALUES (?1, 'ver-1', '1', '2026-08-29T00:00:00.000Z', 'fp-1', 'test-target', \
                     ?2, '10000', '0', '0', '0', '0', \
                     'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates', \
                     0, 0, 0, 0, 0, 0, 0, 0, 0)"
        }
        Some(_) => {
            "INSERT INTO backtest_run \
             (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
              engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
              funding_total, slippage_total, pair, primary_timeframe, primary_data_version, \
              taker_fee_bps, slippage_bps, funding_config, \
              trade_count, wins, losses, breakeven, max_win_streak, max_loss_streak, \
              skipped_sub_lot, skipped_sub_notional, skipped_leverage_capped, d1_data_version) \
             VALUES (?1, 'ver-1', '1', '2026-08-29T00:00:00.000Z', 'fp-1', 'test-target', \
                     ?2, '10000', '0', '0', '0', '0', \
                     'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates', \
                     0, 0, 0, 0, 0, 0, 0, 0, 0, ?3)"
        }
    };
    let mut query = sqlx::query(sql).bind(run).bind(empty_run_hash());
    if let Some(tag) = d1_data_version {
        query = query.bind(tag);
    }
    query.execute(pool).await.expect("seed backtest_run");
}

/// The `#39` content hash of a no-trade, zero-totals run — the value the read
/// path's re-validate-on-read guard rebuilds from the stored columns.
fn empty_run_hash() -> String {
    use pulse::{
        BacktestResult, EngineFingerprint, EquityCurve, RegimeBreakdown, SkippedEntryCounts,
        SummaryStats,
    };
    use rust_decimal::Decimal;

    BacktestResult {
        trades: vec![],
        net_pnl: Decimal::ZERO,
        fees_total: Decimal::ZERO,
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        regime_breakdown: RegimeBreakdown::new(),
        skipped_entries: SkippedEntryCounts::new(),
        open_position: None,
        engine_fingerprint: EngineFingerprint::default(),
        summary: SummaryStats::default(),
        equity_curve: EquityCurve::default(),
    }
    .result_content_hash()
}

/// One `d1_data_version` cell, raw.
async fn stored_d1(pool: &SqlitePool, run: &str) -> Option<String> {
    sqlx::query_scalar("SELECT d1_data_version FROM backtest_run WHERE id = ?1")
        .bind(run)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// up
// ---------------------------------------------------------------------------

/// A fresh database migrates straight through `0017`: the column exists, is
/// nullable, and the table's other columns are untouched.
#[tokio::test]
async fn a_fresh_database_migrates_through_0017() {
    let (_tmp, db) = db_at_0017().await;
    assert_eq!(
        applied_max(db.pool()).await,
        17,
        "the embedded set ends at 0017"
    );

    let columns = columns_of(db.pool(), "backtest_run").await;
    assert!(
        columns.iter().any(|c| c == "d1_data_version"),
        "0017 adds the daily-series column: {columns:?}"
    );
    assert!(
        !columns.iter().any(|c| c == "d1_timeframe"),
        "the timeframe is implicitly 1d — no column may claim otherwise: {columns:?}"
    );
    assert_eq!(
        d1_column_is_nullable(db.pool()).await,
        Some(true),
        "the column is nullable by design (ADR-0018: a legacy row reads None, never a guess)"
    );
    // 0006's HTF pair is still there and still paired — 0017 is additive.
    assert!(columns.iter().any(|c| c == "htf_timeframe"));
    assert!(columns.iter().any(|c| c == "htf_data_version"));
}

/// A database that predates `0017`, already populated, migrates forward: every
/// row survives, the new column reads NULL, and the repo decodes that as
/// `inputs.d1: None` — the legacy row is not an error and not a guess.
#[tokio::test]
async fn a_pre_0017_database_migrates_forward_keeping_every_row() {
    let (_tmp, db_path, db) = db_at_0016().await;
    seed_parents(db.pool()).await;
    seed_run(db.pool(), "run-legacy", None).await;
    assert!(
        !columns_of(db.pool(), "backtest_run")
            .await
            .iter()
            .any(|c| c == "d1_data_version"),
        "the fixture starts without the column"
    );

    // The up path: the embedded set applies on top of the older one.
    MIGRATOR.run(db.pool()).await.expect("0017 applies");
    assert_eq!(applied_max(db.pool()).await, 17, "now at 0017");

    assert_eq!(
        stored_d1(db.pool(), "run-legacy").await,
        None,
        "a row written before 0017 has no daily snapshot to name — NULL, not a guess"
    );

    // …and the adapter reads it back as `None` rather than refusing (#110's
    // legacy-row shape, extended to the new column).
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let run = repo
        .get_run(&BacktestRunId::new("run-legacy"))
        .await
        .expect("the legacy row reads")
        .expect("the legacy row exists");
    let inputs = run
        .inputs
        .expect("a 0006-shaped row carries its provenance");
    assert_eq!(inputs.d1, None, "a pre-0017 row decodes `inputs.d1: None`");
    // The rest of the provenance is intact — 0017 changed nothing else.
    assert_eq!(inputs.primary.data_version.as_str(), "v-primary");
    assert_eq!(inputs.htf, None);

    drop(db);
    assert!(db_path.exists(), "the database file is still there");
}

/// A row that names a daily version survives the same forward migration with
/// its tag intact — the column is a record, not a default.
#[tokio::test]
async fn a_daily_row_keeps_its_recorded_version_across_the_up() {
    let (_tmp, _db_path, db) = db_at_0016().await;
    MIGRATOR.run(db.pool()).await.expect("run embedded set");
    seed_parents(db.pool()).await;
    seed_run(db.pool(), "run-d1", Some("btcusdt-1d-v9")).await;

    // The decode reconstructs the fixed `1d` timeframe from the tag alone.
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let run = repo
        .get_run(&BacktestRunId::new("run-d1"))
        .await
        .expect("the daily row reads")
        .expect("the daily row exists");
    let d1 = run
        .inputs
        .expect("provenance")
        .d1
        .expect("the daily selection is recorded");
    assert_eq!(d1.data_version.as_str(), "btcusdt-1d-v9");
    assert_eq!(d1.timeframe, pulse::Timeframe::D1);
}

// ---------------------------------------------------------------------------
// down
// ---------------------------------------------------------------------------

/// The down REFUSES while any row holds a recorded daily version: `0016` has no
/// column for it, and dropping it would destroy the only record of which daily
/// snapshot the run read (the `0011`/`0015` rule).
#[tokio::test]
async fn the_down_refuses_a_recorded_d1_snapshot() {
    let (_tmp, db) = db_at_0017().await;
    seed_run(db.pool(), "run-d1", Some("btcusdt-1d-v9")).await;

    let err = undo_to(db.pool(), 16)
        .await
        .expect_err("a recorded daily version must refuse the 0017 down");
    assert!(
        err.to_string().contains("0017"),
        "the refusal names the migration: {err}"
    );
    assert_eq!(
        applied_max(db.pool()).await,
        17,
        "the refused down is transactional: the database stays at 0017"
    );
    assert_eq!(
        stored_d1(db.pool(), "run-d1").await.as_deref(),
        Some("btcusdt-1d-v9"),
        "the refused down leaves the record intact"
    );
}

/// With nothing `0016` cannot hold, the down restores it exactly — the column is
/// gone, every row survives — and the up re-applies: a clean round trip.
#[tokio::test]
async fn the_down_restores_the_0016_shape() {
    let (_tmp, db) = db_at_0017().await;
    seed_run(db.pool(), "run-legacy", None).await;

    undo_to(db.pool(), 16).await.expect("the clean down runs");
    assert_eq!(applied_max(db.pool()).await, 16, "back at the 0016 max");
    assert!(
        !columns_of(db.pool(), "backtest_run")
            .await
            .iter()
            .any(|c| c == "d1_data_version"),
        "the column is gone"
    );
    // The row itself is untouched: 0017 owns one column and nothing else.
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backtest_run WHERE id = 'run-legacy'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1, "the down drops a column, never a row");

    MIGRATOR.run(db.pool()).await.expect("the up re-applies");
    assert_eq!(
        applied_max(db.pool()).await,
        17,
        "round trip closes at 0017"
    );
    assert_eq!(
        stored_d1(db.pool(), "run-legacy").await,
        None,
        "the re-applied column is NULL for the row that never named a daily snapshot"
    );
}
