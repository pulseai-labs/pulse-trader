//! AC-3 — migration `0013_walk_forward` (r2.s3.w3, ADR-0018 / ADR-0019 /
//! ADR-0025).
//!
//! Two new tables — `walk_forward_run` (the parent) and `walk_forward_fold`
//! (one row per fold, naming its windowed `backtest_run`) — plus the two
//! membership columns on `backtest_run` (`walk_forward_run_id`, `fold_index`),
//! set together or not at all under the `backtest_run_walk_forward_pair`
//! trigger (the 0009 / 0012 pair-trigger pattern).
//!
//! **Why raw SQL.** As with `0009`/`0012`, the value is in the shapes the
//! schema REFUSES, so the suite drives raw SQL at it — the only way to prove a
//! constraint holds against something written around the adapter. The
//! adapter-level round-trip lives in `tests/walk_forward.rs`.
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
// helpers — databases at 0012 and at 0013
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

/// Copy the shipped `migrations/` set into `dir`, SKIPPING `0013_*` and
/// everything after — the binary that shipped `0012`.
fn shipped_set_without_0013(dir: &Path) {
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(&shipped).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.as_str() >= "0013" {
            continue;
        }
        std::fs::copy(&path, dir.join(&name)).unwrap();
    }
}

/// A fresh temp database migrated by the "older" set (everything but `0013`).
async fn db_at_0012() -> (TempDir, PathBuf, Db) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    shipped_set_without_0013(&dir);

    let db_path = tmp.path().join("pulse.db");
    let older = Migrator::new(dir.as_path()).await.unwrap();
    let db = Db::with_path(&db_path).await.unwrap();
    older.run(db.pool()).await.expect("the older set applies");

    let applied = applied_versions(db.pool()).await;
    assert!(
        !applied.contains(&13),
        "the fixture must NOT have 0013 applied: {applied:?}"
    );
    assert_eq!(
        applied.iter().copied().max(),
        Some(12),
        "the fixture sits at the pre-0013 maximum"
    );
    (tmp, db_path, db)
}

/// A fresh temp database at the full embedded set (0013 included), seeded.
async fn db_at_0013() -> (TempDir, Db) {
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
/// read path's #39 re-validate-on-read guard rebuilds from the stored columns.
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

/// The columns every `backtest_run` row carries, parameterized on the tail
/// (`0012`'s window trio and `0013`'s membership pair).
const RUN_COLUMNS: &str = "\
     (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
      engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
      funding_total, slippage_total, pair, primary_timeframe, primary_data_version, \
      taker_fee_bps, slippage_bps, funding_config, \
      trade_count, wins, losses, breakeven, max_win_streak, max_loss_streak, \
      skipped_sub_lot, skipped_sub_notional, skipped_leverage_capped";

const RUN_VALUES: &str = "'ver-1', '1', '2026-08-29T00:00:00.000Z', 'fp-1', 'test-target', \
     ?2, '10000', '0', '0', '0', '0', \
     'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates', \
     0, 0, 0, 0, 0, 0, 0, 0, 0";

/// One `backtest_run` row owned by `ver-1`, `NULL` on every window/lead-in/
/// membership column — the shape every pre-0013 row and every unwindowed,
/// non-fold new run takes.
async fn seed_run(pool: &SqlitePool, run: &str) {
    sqlx::query(&format!(
        "INSERT INTO backtest_run {RUN_COLUMNS}) VALUES (?1, {RUN_VALUES})"
    ))
    .bind(run)
    .bind(empty_run_hash())
    .execute(pool)
    .await
    .expect("seed backtest_run");
}

/// A windowed run row: the window pair + `window_lead_in_from_ms` set (the
/// shape a fold's `backtest_run` must take for `walk_forward_fold_windowed`
/// to accept it), optionally carrying `0013` membership.
async fn seed_windowed_run(pool: &SqlitePool, run: &str, membership: Option<(&str, i64)>) {
    let (cols, vals): (String, String) = match membership {
        Some((wf, idx)) => (
            ", walk_forward_run_id, fold_index)".to_owned(),
            format!(", '{wf}', {idx}"),
        ),
        None => (")".to_owned(), String::new()),
    };
    sqlx::query(&format!(
        "INSERT INTO backtest_run {RUN_COLUMNS}, \
         window_from_ms, window_to_ms, window_lead_in_from_ms{cols} \
         VALUES (?1, {RUN_VALUES}, \
         1740787200000, 1743379200000, 1735689600000{vals})"
    ))
    .bind(run)
    .bind(empty_run_hash())
    .execute(pool)
    .await
    .expect("seed windowed backtest_run");
}

/// One `walk_forward_run` parent row owned by `ver-1`.
async fn seed_walk_forward_run(pool: &SqlitePool, run: &str) {
    sqlx::query(
        "INSERT INTO walk_forward_run \
         (id, strategy_version_id, created_at, scheme, rule, k, \
          span_from_ms, span_to_ms, from_defaulted, engine_fingerprint, \
          folds_holding, folds_required, pooled_n, pooled_mean_r, \
          pooled_lower_bound, pass) \
         VALUES (?1, 'ver-1', '2026-08-29T00:00:00.000Z', 'rolling-oos/v1', 'wf-v1', 6, \
                 1740787200000, 1743379200000, 1, 'fp-1', \
                 0, 4, 0, '0', 0.0, 0)",
    )
    .bind(run)
    .execute(pool)
    .await
    .expect("seed walk_forward_run");
}

/// One `walk_forward_fold` row under `wf`, naming `backtest_run`.
async fn seed_fold(pool: &SqlitePool, wf: &str, fold_index: i64, backtest_run: &str) {
    sqlx::query(
        "INSERT INTO walk_forward_fold \
         (walk_forward_run_id, fold_index, window_from_ms, window_to_ms, \
          backtest_run_id, n, mean_r, lower_bound, holds) \
         VALUES (?1, ?2, 1740787200000, 1743379200000, ?3, 0, '0', 0.0, 0)",
    )
    .bind(wf)
    .bind(fold_index)
    .bind(backtest_run)
    .execute(pool)
    .await
    .expect("seed walk_forward_fold");
}

// ---------------------------------------------------------------------------
// the migration itself
// ---------------------------------------------------------------------------

/// A fresh database migrates cleanly through 0013 — tables, columns, triggers
/// and the index all land.
#[tokio::test]
async fn a_fresh_database_migrates_through_0013() {
    let tmp = TempDir::new().unwrap();
    let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
    MIGRATOR.run(db.pool()).await.expect("run embedded set");

    let applied = applied_versions(db.pool()).await;
    assert!(applied.contains(&13), "0013 must be applied: {applied:?}");

    for table in ["walk_forward_run", "walk_forward_fold"] {
        assert!(
            object_present(db.pool(), "table", table).await,
            "the {table} table must exist"
        );
    }
    for trigger in [
        "walk_forward_run_no_update",
        "walk_forward_run_no_delete",
        "walk_forward_fold_no_update",
        "walk_forward_fold_no_delete",
        "walk_forward_fold_windowed",
        "backtest_run_walk_forward_pair",
    ] {
        assert!(
            object_present(db.pool(), "trigger", trigger).await,
            "the {trigger} trigger must exist"
        );
    }
    assert!(
        object_present(db.pool(), "index", "idx_backtest_run_walk_forward").await,
        "the membership index must exist"
    );
    let columns = columns_of(db.pool(), "backtest_run").await;
    for col in ["walk_forward_run_id", "fold_index"] {
        assert!(
            columns.contains(&col.to_owned()),
            "backtest_run must carry {col}: {columns:?}"
        );
    }
}

/// A database at 0012 migrates forward to 0013.
#[tokio::test]
async fn a_pre_0013_database_migrates_forward() {
    let (_tmp, _path, db) = db_at_0012().await;
    MIGRATOR.run(db.pool()).await.expect("0013 applies");

    let applied = applied_versions(db.pool()).await;
    assert!(applied.contains(&13), "0013 must be applied: {applied:?}");
    assert!(
        object_present(db.pool(), "table", "walk_forward_run").await,
        "the parent table must exist"
    );
}

// ---------------------------------------------------------------------------
// pre-0013 rows read back honestly
// ---------------------------------------------------------------------------

/// A row written before 0013 reloads with both membership fields `None` — no
/// backfill, no invented provenance (ADR-0018).
#[tokio::test]
async fn a_pre_0013_row_reloads_with_no_membership() {
    let (_tmp, _path, db) = db_at_0012().await;
    seed_parents(db.pool()).await;
    seed_run(db.pool(), "run-pre-0013").await;

    MIGRATOR.run(db.pool()).await.expect("0013 applies");

    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let run = runs
        .get_run(&BacktestRunId::new("run-pre-0013"))
        .await
        .expect("read")
        .expect("the seeded row exists");
    assert_eq!(
        run.walk_forward, None,
        "a pre-0013 row must reload with no walk-forward membership"
    );
}

// ---------------------------------------------------------------------------
// the triggers — the shapes 0013 refuses
// ---------------------------------------------------------------------------

/// `fold_index` without `walk_forward_run_id` is refused — membership is
/// both-or-neither.
#[tokio::test]
async fn fold_index_without_a_walk_forward_run_id_is_refused() {
    let (_tmp, db) = db_at_0013().await;
    let err = sqlx::query(&format!(
        "INSERT INTO backtest_run {RUN_COLUMNS}, fold_index) \
         VALUES ('run-bad', {RUN_VALUES}, 0)"
    ))
    .bind(empty_run_hash())
    .execute(db.pool())
    .await
    .expect_err("a bare fold_index must be refused");
    assert!(
        err.to_string().contains("walk_forward_run_id"),
        "the refusal must name the membership pair: {err}"
    );
}

/// `walk_forward_run_id` without `fold_index` is refused — the other half of
/// the pair.
#[tokio::test]
async fn a_walk_forward_run_id_without_a_fold_index_is_refused() {
    let (_tmp, db) = db_at_0013().await;
    seed_walk_forward_run(db.pool(), "wf-1").await;
    let err = sqlx::query(&format!(
        "INSERT INTO backtest_run {RUN_COLUMNS}, walk_forward_run_id) \
         VALUES ('run-bad', {RUN_VALUES}, 'wf-1')"
    ))
    .bind(empty_run_hash())
    .execute(db.pool())
    .await
    .expect_err("a bare walk_forward_run_id must be refused");
    assert!(
        err.to_string().contains("fold_index"),
        "the refusal must name the membership pair: {err}"
    );
}

/// A fold row pointing at an UNWINDOWED `backtest_run` is refused — a fold IS
/// a counted window, so its run must carry the 0009 window pair.
#[tokio::test]
async fn a_fold_referencing_an_unwindowed_run_is_refused() {
    let (_tmp, db) = db_at_0013().await;
    seed_walk_forward_run(db.pool(), "wf-1").await;
    seed_run(db.pool(), "run-plain").await; // no window pair

    let err = sqlx::query(
        "INSERT INTO walk_forward_fold \
         (walk_forward_run_id, fold_index, window_from_ms, window_to_ms, \
          backtest_run_id, n, mean_r, lower_bound, holds) \
         VALUES ('wf-1', 0, 1740787200000, 1743379200000, 'run-plain', 0, '0', 0.0, 0)",
    )
    .execute(db.pool())
    .await
    .expect_err("a fold over an unwindowed run must be refused");
    assert!(
        err.to_string().contains("windowed"),
        "the refusal must name the windowed-run invariant: {err}"
    );
}

/// `UNIQUE (walk_forward_run_id, fold_index)` — a second fold row at the same
/// index under the same parent is refused.
#[tokio::test]
async fn the_fold_index_unique_constraint_holds() {
    let (_tmp, db) = db_at_0013().await;
    seed_walk_forward_run(db.pool(), "wf-1").await;
    seed_windowed_run(db.pool(), "run-f0", Some(("wf-1", 0))).await;
    seed_windowed_run(db.pool(), "run-f0-dup", Some(("wf-1", 0))).await;
    seed_fold(db.pool(), "wf-1", 0, "run-f0").await;

    let err = sqlx::query(
        "INSERT INTO walk_forward_fold \
         (walk_forward_run_id, fold_index, window_from_ms, window_to_ms, \
          backtest_run_id, n, mean_r, lower_bound, holds) \
         VALUES ('wf-1', 0, 1740787200000, 1743379200000, 'run-f0-dup', 0, '0', 0.0, 0)",
    )
    .execute(db.pool())
    .await
    .expect_err("a duplicate (walk_forward_run_id, fold_index) must be refused");
    assert!(
        err.to_string().contains("UNIQUE"),
        "the refusal must be the UNIQUE constraint: {err}"
    );
}

/// The 0009 window-pair trigger still fires under the new schema.
#[tokio::test]
async fn the_0009_window_pair_trigger_still_fires() {
    let (_tmp, db) = db_at_0013().await;
    let err = sqlx::query(&format!(
        "INSERT INTO backtest_run {RUN_COLUMNS}, window_from_ms) \
         VALUES ('run-bad', {RUN_VALUES}, 1740787200000)"
    ))
    .bind(empty_run_hash())
    .execute(db.pool())
    .await
    .expect_err("a half-present window must still be refused by the 0009 trigger");
    assert!(
        err.to_string().contains("window"),
        "the refusal must name the window pair: {err}"
    );
}

/// The 0012 lead-in pair trigger still fires under the new schema.
#[tokio::test]
async fn the_0012_lead_in_trigger_still_fires() {
    let (_tmp, db) = db_at_0013().await;
    let err = sqlx::query(&format!(
        "INSERT INTO backtest_run {RUN_COLUMNS}, window_lead_in_from_ms) \
         VALUES ('run-bad', {RUN_VALUES}, 1735689600000)"
    ))
    .bind(empty_run_hash())
    .execute(db.pool())
    .await
    .expect_err("a lead-in without a window pair must still be refused");
    assert!(
        err.to_string().contains("window_lead_in"),
        "the refusal must name the lead-in column: {err}"
    );
}

// ---------------------------------------------------------------------------
// the down migration
// ---------------------------------------------------------------------------

/// Undoing to 0012 with no walk-forward state succeeds and restores the exact
/// 0012 shape — tables, columns, triggers and index all gone.
#[tokio::test]
async fn down_migration_restores_0012() {
    let (_tmp, db) = db_at_0013().await;
    seed_run(db.pool(), "run-plain").await;

    undo_to(db.pool(), 12).await.expect("undo to 0012");

    for table in ["walk_forward_run", "walk_forward_fold"] {
        assert!(
            !object_present(db.pool(), "table", table).await,
            "the {table} table must be gone after undo"
        );
    }
    let columns = columns_of(db.pool(), "backtest_run").await;
    for col in ["walk_forward_run_id", "fold_index"] {
        assert!(
            !columns.contains(&col.to_owned()),
            "the {col} column must be gone after undo: {columns:?}"
        );
    }
    assert!(
        !object_present(db.pool(), "trigger", "backtest_run_walk_forward_pair").await,
        "the pair trigger must be gone after undo"
    );
    assert!(
        !object_present(db.pool(), "index", "idx_backtest_run_walk_forward").await,
        "the membership index must be gone after undo"
    );
    // … and the run row itself survives untouched.
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backtest_run WHERE id = 'run-plain'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n, 1, "the ordinary run must survive the downgrade");
}

/// A `walk_forward_run` row has no 0012 representation — the down migration
/// refuses rather than falsify the record (the 0010/0011/0012 guard pattern).
#[tokio::test]
async fn down_migration_refuses_a_walk_forward_run_row() {
    let (_tmp, db) = db_at_0013().await;
    seed_walk_forward_run(db.pool(), "wf-1").await;

    let err = undo_to(db.pool(), 12)
        .await
        .expect_err("a walk_forward_run row cannot downgrade");
    assert!(
        err.to_string().contains("walk_forward_run"),
        "the refusal must name what 0012 cannot say: {err}"
    );
    // The refusal is transactional — the schema is still at 0013.
    assert!(
        object_present(db.pool(), "table", "walk_forward_run").await,
        "a refused down must leave the 0013 schema intact"
    );
}

/// A `walk_forward_fold` row likewise has no 0012 representation.
#[tokio::test]
async fn down_migration_refuses_a_walk_forward_fold_row() {
    let (_tmp, db) = db_at_0013().await;
    seed_walk_forward_run(db.pool(), "wf-1").await;
    seed_windowed_run(db.pool(), "run-f0", Some(("wf-1", 0))).await;
    seed_fold(db.pool(), "wf-1", 0, "run-f0").await;

    let err = undo_to(db.pool(), 12)
        .await
        .expect_err("a walk_forward_fold row cannot downgrade");
    assert!(
        err.to_string().contains("walk_forward_run")
            || err.to_string().contains("walk_forward_fold"),
        "the refusal must name what 0012 cannot say: {err}"
    );
}

/// A `backtest_run` carrying membership has no 0012 representation either —
/// even with the parent and fold tables empty (a run row written directly
/// with both columns set, as the pair trigger allows).
#[tokio::test]
async fn down_migration_refuses_a_membership_bearing_run() {
    let (_tmp, db) = db_at_0013().await;
    seed_walk_forward_run(db.pool(), "wf-1").await;
    seed_windowed_run(db.pool(), "run-f0", Some(("wf-1", 0))).await;

    // No fold row — only the membership columns on the run itself.
    let err = undo_to(db.pool(), 12)
        .await
        .expect_err("a membership-bearing run cannot downgrade");
    assert!(
        err.to_string().contains("walk_forward"),
        "the refusal must name what 0012 cannot say: {err}"
    );
}
