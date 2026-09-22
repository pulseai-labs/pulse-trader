//! AC-2 — migration `0012_window_lead_in` (r2.s3.w2, ADR-0018 / ADR-0019).
//!
//! One nullable column on `backtest_run` — `window_lead_in_from_ms`, the
//! `open_time` of the first candle the engine consumed for a windowed run (the
//! snapshot's first candle under full-history lead-in, equal to
//! `window_from_ms` when nothing precedes `from`). `NULL` for every row
//! persisted before `0012`, for every unwindowed run, and — enforced by a
//! CHECK-style trigger mirroring `0009`'s `backtest_run_window_pair` — for any
//! row whose window pair is not complete.
//!
//! **Why raw SQL.** As with `0009`, the value is in the shapes the schema
//! REFUSES, so the suite drives raw SQL at it — the only way to prove a
//! constraint holds against something written around the adapter. The
//! adapter-level round-trip lives in `tests/backtest_provenance.rs`.
//!
//! Offline (`SQLX_OFFLINE=true` + the in-process `MIGRATOR`), `TempDir`-isolated.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{
    BacktestResult, BacktestRunId, BacktestRunRepository, Db, EngineFingerprint, EquityCurve,
    MIGRATOR, RegimeBreakdown, SkippedEntryCounts, SqliteBacktestRunRepo, SummaryStats, undo_to,
};
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// helpers — databases at 0011 and at 0012
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

/// Whether a named object exists in `sqlite_master`.
async fn object_present(pool: &SqlitePool, kind: &str, name: &str) -> bool {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type=?1 AND name=?2")
        .bind(kind)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
    n == 1
}

/// The column names of `table`, via `pragma_table_info`.
async fn columns_of(pool: &SqlitePool, table: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT name FROM pragma_table_info(?1)")
        .bind(table)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Copy the shipped `migrations/` set into `dir`, SKIPPING `0012_*` and
/// everything after — the binary that shipped `0011`.
fn shipped_set_without_0012(dir: &Path) {
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(&shipped).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.as_str() >= "0012" {
            continue;
        }
        std::fs::copy(&path, dir.join(&name)).unwrap();
    }
}

/// A fresh temp database migrated by the "older" set (everything but `0012`).
async fn db_at_0011() -> (TempDir, PathBuf, Db) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    shipped_set_without_0012(&dir);

    let db_path = tmp.path().join("pulse.db");
    let older = Migrator::new(dir.as_path()).await.unwrap();
    let db = Db::with_path(&db_path).await.unwrap();
    older.run(db.pool()).await.expect("the older set applies");

    let applied = applied_versions(db.pool()).await;
    assert!(
        !applied.contains(&12),
        "the fixture must NOT have 0012 applied: {applied:?}"
    );
    assert_eq!(
        applied.iter().copied().max(),
        Some(11),
        "the fixture sits at the pre-0012 maximum"
    );
    (tmp, db_path, db)
}

/// A fresh temp database at the full embedded set (0012 included), seeded.
async fn db_at_0012() -> (TempDir, Db) {
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
         VALUES ('ver-1', 'strat-1', NULL, '1.0.0', '{}', '{}', 'hash-ver-1', '\"human\"', '[]', \
                 '2026-08-29T00:00:00.000Z')",
    )
    .execute(pool)
    .await
    .expect("seed strategy_version");
}

/// The `result_content_hash` of a no-trade, zero-totals run — the value the
/// read path's #39 re-validate-on-read guard rebuilds from the stored columns
/// (empty trade log, `0` money totals, empty regime/skipped). Seeding any
/// other string would make the row read as tampered, not merely legacy.
fn empty_run_hash() -> String {
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

/// One `backtest_run` row owned by `ver-1`, `NULL` on every window/lead-in
/// column — the shape every pre-0012 row and every unwindowed new run takes.
/// `0006`'s completeness trigger wants the input provenance present; the
/// read path's `usize_from` counts and the tamper-guard hash must be real.
async fn seed_run(pool: &SqlitePool, run: &str) {
    sqlx::query(
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
                 0, 0, 0, 0, 0, 0, 0, 0, 0)",
    )
    .bind(run)
    .bind(empty_run_hash())
    .execute(pool)
    .await
    .expect("seed backtest_run");
}

/// A windowed run row: the window pair set, plus `window_lead_in_from_ms`.
async fn seed_windowed_run(pool: &SqlitePool, run: &str, lead_in_ms: Option<i64>) {
    sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
          engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
          funding_total, slippage_total, pair, primary_timeframe, primary_data_version, \
          taker_fee_bps, slippage_bps, funding_config, \
          trade_count, wins, losses, breakeven, max_win_streak, max_loss_streak, \
          skipped_sub_lot, skipped_sub_notional, skipped_leverage_capped, \
          window_from_ms, window_to_ms, window_lead_in_from_ms) \
         VALUES (?1, 'ver-1', '1', '2026-08-29T00:00:00.000Z', 'fp-1', 'test-target', \
                 ?2, '10000', '0', '0', '0', '0', \
                 'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates', \
                 0, 0, 0, 0, 0, 0, 0, 0, 0, \
                 1740787200000, 1743379200000, ?3)",
    )
    .bind(run)
    .bind(empty_run_hash())
    .bind(lead_in_ms)
    .execute(pool)
    .await
    .expect("seed windowed backtest_run");
}

// ---------------------------------------------------------------------------
// the migration itself
// ---------------------------------------------------------------------------

/// A fresh database migrates cleanly through 0012 — column and trigger land.
#[tokio::test]
async fn a_fresh_database_migrates_through_0012() {
    let tmp = TempDir::new().unwrap();
    let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
    MIGRATOR.run(db.pool()).await.expect("run embedded set");

    let applied = applied_versions(db.pool()).await;
    assert!(applied.contains(&12), "0012 must be applied: {applied:?}");

    let columns = columns_of(db.pool(), "backtest_run").await;
    assert!(
        columns.contains(&"window_lead_in_from_ms".to_owned()),
        "backtest_run must carry window_lead_in_from_ms: {columns:?}"
    );
    assert!(
        object_present(db.pool(), "trigger", "backtest_run_window_lead_in_pair").await,
        "the lead-in pair trigger must exist"
    );
}

/// A database at 0011 migrates forward to 0012.
#[tokio::test]
async fn a_pre_0012_database_migrates_forward() {
    let (_tmp, _path, db) = db_at_0011().await;
    MIGRATOR.run(db.pool()).await.expect("0012 applies");

    let applied = applied_versions(db.pool()).await;
    assert!(applied.contains(&12), "0012 must be applied: {applied:?}");
    let columns = columns_of(db.pool(), "backtest_run").await;
    assert!(columns.contains(&"window_lead_in_from_ms".to_owned()));
}

// ---------------------------------------------------------------------------
// pre-0012 rows read back honestly
// ---------------------------------------------------------------------------

/// A row written before 0012 keeps `lead_in_from_ms = NULL` and reloads with
/// `None` — no backfill, no invented provenance (ADR-0018).
#[tokio::test]
async fn a_pre_0012_row_reloads_with_no_lead_in() {
    let (_tmp, _path, db) = db_at_0011().await;
    seed_parents(db.pool()).await;
    seed_run(db.pool(), "run-pre-0012").await;

    MIGRATOR.run(db.pool()).await.expect("0012 applies");

    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let run = runs
        .get_run(&BacktestRunId::new("run-pre-0012"))
        .await
        .expect("read")
        .expect("the seeded row exists");
    let inputs = serde_json::to_value(run.inputs.expect("a post-0006 row has inputs"))
        .expect("inputs serialize");
    assert!(
        inputs["lead_in_from"].is_null(),
        "a pre-0012 row must reload with lead_in_from null (absent key indexes as null — \
         so assert the key is present AND null): {inputs}"
    );
    assert!(
        inputs
            .as_object()
            .expect("inputs is an object")
            .contains_key("lead_in_from"),
        "the field must exist on the wire even when null: {inputs}"
    );
}

/// A pre-0012 WINDOWED row — the pair set, the lead-in column NULL — reloads
/// with `lead_in_from_ms = None` (the lead-in its run consumed is unknowable:
/// recorded only from 0012 on).
#[tokio::test]
async fn a_pre_0012_windowed_row_reloads_with_no_lead_in() {
    let (_tmp, _path, db) = db_at_0011().await;
    seed_parents(db.pool()).await;
    sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
          engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
          funding_total, slippage_total, pair, primary_timeframe, primary_data_version, \
          taker_fee_bps, slippage_bps, funding_config, \
          trade_count, wins, losses, breakeven, max_win_streak, max_loss_streak, \
          skipped_sub_lot, skipped_sub_notional, skipped_leverage_capped, \
          window_from_ms, window_to_ms) \
         VALUES ('run-windowed', 'ver-1', '1', '2026-08-29T00:00:00.000Z', 'fp-1', 'test-target', \
                 ?1, '10000', '0', '0', '0', '0', \
                 'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates', \
                 0, 0, 0, 0, 0, 0, 0, 0, 0, \
                 1740787200000, 1743379200000)",
    )
    .bind(empty_run_hash())
    .execute(db.pool())
    .await
    .expect("seed pre-0012 windowed run");

    MIGRATOR.run(db.pool()).await.expect("0012 applies");

    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let run = runs
        .get_run(&BacktestRunId::new("run-windowed"))
        .await
        .expect("read")
        .expect("the seeded row exists");
    let inputs = serde_json::to_value(run.inputs.expect("a post-0006 row has inputs"))
        .expect("inputs serialize");
    assert!(
        inputs["lead_in_from"].is_null()
            && inputs
                .as_object()
                .expect("inputs is an object")
                .contains_key("lead_in_from"),
        "a pre-0012 windowed row reloads with lead_in_from present and null: {inputs}"
    );
    assert_ne!(
        inputs["window"],
        serde_json::Value::Null,
        "the window itself still decodes"
    );
}

// ---------------------------------------------------------------------------
// the trigger — the shapes 0012 refuses
// ---------------------------------------------------------------------------

/// A non-NULL lead-in on a row whose window pair is NULL is refused: the lead-in
/// is only meaningful relative to a counted window.
#[tokio::test]
async fn a_lead_in_without_a_window_is_refused() {
    let (_tmp, db) = db_at_0012().await;
    let err = sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
          engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
          funding_total, slippage_total, pair, primary_timeframe, primary_data_version, \
          taker_fee_bps, slippage_bps, funding_config, window_lead_in_from_ms) \
         VALUES ('run-bad', 'ver-1', '1', '2026-08-29T00:00:00.000Z', 'fp-1', 'test-target', \
                 'rch-1', '10000', '0', '0', '0', '0', \
                 'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates', 1735689600000)",
    )
    .execute(db.pool())
    .await
    .expect_err("a lead-in without a window pair must be refused");
    assert!(
        err.to_string().contains("window_lead_in"),
        "the refusal must name the column: {err}"
    );
}

/// A lead-in on a HALF-present window is refused too — the trigger guards the
/// pair, not just the fully-NULL case.
#[tokio::test]
async fn a_lead_in_with_a_half_window_is_refused() {
    let (_tmp, db) = db_at_0012().await;
    let err = sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
          engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
          funding_total, slippage_total, pair, primary_timeframe, primary_data_version, \
          taker_fee_bps, slippage_bps, funding_config, \
          window_from_ms, window_lead_in_from_ms) \
         VALUES ('run-bad', 'ver-1', '1', '2026-08-29T00:00:00.000Z', 'fp-1', 'test-target', \
                 'rch-1', '10000', '0', '0', '0', '0', \
                 'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates', \
                 1740787200000, 1735689600000)",
    )
    .execute(db.pool())
    .await
    .expect_err("a lead-in on a half-present window must be refused");
    assert!(
        err.to_string().contains("window"),
        "the refusal must name the window invariant: {err}"
    );
}

/// The accepted shape: a complete window pair carrying its lead-in start.
#[tokio::test]
async fn a_windowed_row_with_lead_in_is_accepted() {
    let (_tmp, db) = db_at_0012().await;
    seed_windowed_run(db.pool(), "run-windowed", Some(1_735_689_600_000)).await;

    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let run = runs
        .get_run(&BacktestRunId::new("run-windowed"))
        .await
        .expect("read")
        .expect("the seeded row exists");
    let inputs = serde_json::to_value(run.inputs.expect("a post-0006 row has inputs"))
        .expect("inputs serialize");
    assert_eq!(
        inputs["lead_in_from"],
        serde_json::json!("2025-01-01T00:00:00.000Z"),
        "the lead-in start round-trips as RFC 3339"
    );
}

/// The 0009 pair trigger still fires under the new schema: a half-present
/// window with no lead-in remains refused.
#[tokio::test]
async fn the_0009_window_pair_trigger_still_fires() {
    let (_tmp, db) = db_at_0012().await;
    let err = sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
          engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
          funding_total, slippage_total, pair, primary_timeframe, primary_data_version, \
          taker_fee_bps, slippage_bps, funding_config, window_from_ms) \
         VALUES ('run-bad', 'ver-1', '1', '2026-08-29T00:00:00.000Z', 'fp-1', 'test-target', \
                 'rch-1', '10000', '0', '0', '0', '0', \
                 'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates', 1740787200000)",
    )
    .execute(db.pool())
    .await
    .expect_err("a half-present window must still be refused by the 0009 trigger");
    assert!(
        err.to_string().contains("window"),
        "the refusal must name the window pair: {err}"
    );
}

// ---------------------------------------------------------------------------
// the down migration
// ---------------------------------------------------------------------------

/// Undoing to 0011 over all-NULL lead-ins succeeds and restores the exact 0011
/// shape — the column is gone, the trigger with it.
#[tokio::test]
async fn down_migration_restores_0011() {
    let (_tmp, db) = db_at_0012().await;
    seed_run(db.pool(), "run-plain").await;

    undo_to(db.pool(), 11).await.expect("undo to 0011");

    let columns = columns_of(db.pool(), "backtest_run").await;
    assert!(
        !columns.contains(&"window_lead_in_from_ms".to_owned()),
        "the column must be gone after undo: {columns:?}"
    );
    assert!(
        !object_present(db.pool(), "trigger", "backtest_run_window_lead_in_pair").await,
        "the trigger must be gone after undo"
    );
}

/// A row carrying a lead-in has no 0011 representation — the down migration
/// refuses rather than falsify the record (the 0010/0011 guard pattern).
#[tokio::test]
async fn down_migration_refuses_a_row_with_lead_in() {
    let (_tmp, db) = db_at_0012().await;
    seed_windowed_run(db.pool(), "run-windowed", Some(1_735_689_600_000)).await;

    let err = undo_to(db.pool(), 11)
        .await
        .expect_err("a run carrying a lead-in cannot downgrade");
    assert!(
        err.to_string().contains("lead-in") || err.to_string().contains("window_lead_in"),
        "the refusal must name what 0011 cannot say: {err}"
    );
}
