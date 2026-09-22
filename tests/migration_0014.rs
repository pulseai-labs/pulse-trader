//! r2.s3.w4 — AC-2: migration `0014_certification` (ADR-0018 / ADR-0019 /
//! ADR-0025).
//!
//! `0014` adds `strategy_version.latest_walk_forward_run_id` — the pointer that
//! IS certification — narrows `strategy_version_no_update` to admit exactly
//! that one mutable cell, installs `strategy_version_certification_owner` (the
//! pointer must name a walk-forward run OF THIS VERSION and may only advance in
//! `(created_at, id)` order), and rebuilds `coaching_proposals` so
//! `accept_failure_stage` can carry `'walk_forward'`.
//!
//! **Why raw SQL.** As with `0009`/`0012`/`0013`, the value is in the shapes the
//! schema REFUSES — a pointer to another version's run, a backward move, an
//! update that smuggles a second column's change beside the pointer — so the
//! suite drives raw SQL at it. The adapter-level round-trip lives in
//! `tests/coach_walk_forward_gate.rs` and `tests/strategy_certification.rs`.
//!
//! Offline (`SQLX_OFFLINE=true` + the in-process `MIGRATOR`), `TempDir`-isolated.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{
    BacktestResult, Db, EngineFingerprint, EquityCurve, MIGRATOR, RegimeBreakdown,
    SkippedEntryCounts, SummaryStats, undo_to,
};
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// helpers — databases at 0013 and at 0014
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

/// Copy the shipped `migrations/` set into `dir`, SKIPPING `0014_*` — the
/// binary that shipped `0013`.
fn shipped_set_without_0014(dir: &Path) {
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(&shipped).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.as_str() >= "0014" {
            continue;
        }
        std::fs::copy(&path, dir.join(&name)).unwrap();
    }
}

/// A fresh temp database migrated by the "older" set (everything but `0014`).
async fn db_at_0013() -> (TempDir, PathBuf, Db) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    shipped_set_without_0014(&dir);

    let db_path = tmp.path().join("pulse.db");
    let older = Migrator::new(dir.as_path()).await.unwrap();
    let db = Db::with_path(&db_path).await.unwrap();
    older.run(db.pool()).await.expect("the older set applies");

    let applied = applied_versions(db.pool()).await;
    assert!(
        !applied.contains(&14),
        "the fixture must NOT have 0014 applied: {applied:?}"
    );
    assert_eq!(
        applied.iter().copied().max(),
        Some(13),
        "the fixture sits at the pre-0014 maximum"
    );
    (tmp, db_path, db)
}

/// A fresh temp database at the full embedded set (0014 included), seeded.
async fn db_at_0014() -> (TempDir, Db) {
    let tmp = TempDir::new().unwrap();
    let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
    MIGRATOR.run(db.pool()).await.expect("run embedded set");
    seed_parents(db.pool()).await;
    (tmp, db)
}

/// The FK parents a `walk_forward_run` row needs: one strategy, two versions.
async fn seed_parents(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO strategy (id, name, tags, archived, created_at) \
         VALUES ('strat-1', 'RSI Oversold', '[]', 0, '2026-08-29T00:00:00.000Z')",
    )
    .execute(pool)
    .await
    .expect("seed strategy");

    for version in ["ver-1", "ver-2"] {
        sqlx::query(
            "INSERT INTO strategy_version \
             (id, strategy_id, parent_version_id, dsl_schema_version, dsl, dsl_original, \
              version_hash, created_by, creating_llm_call_ids, created_at) \
             VALUES (?1, 'strat-1', NULL, '1.0.0', ?3, ?3, ?2, '\"human\"', '[]', \
                     '2026-08-29T00:00:00.000Z')",
        )
        .bind(version)
        .bind(format!("hash-{version}"))
        .bind(MINIMAL_DSL)
        .execute(pool)
        .await
        .expect("seed strategy_version");
    }
}

/// A real, loadable DSL document — `get_version` re-validates `dsl_original` on
/// read, so the seeded row cannot carry `'{}'`.
const MINIMAL_DSL: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold (0014)",
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
    { "type": "TakeProfit", "target_r": "2" }
  ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// One `walk_forward_run` parent row owned by `version`, at `created_at`,
/// with `pass` as given.
async fn seed_walk_forward_run(
    pool: &SqlitePool,
    run: &str,
    version: &str,
    created_at: &str,
    pass: i64,
) {
    sqlx::query(
        "INSERT INTO walk_forward_run \
         (id, strategy_version_id, created_at, scheme, rule, k, \
          span_from_ms, span_to_ms, from_defaulted, engine_fingerprint, \
          folds_holding, folds_required, pooled_n, pooled_mean_r, \
          pooled_lower_bound, pass) \
         VALUES (?1, ?2, ?3, 'rolling-oos/v1', 'wf-v1', 6, \
                 1740787200000, 1743379200000, 1, 'fp-1', \
                 4, 4, 24, '0.5', 0.21, ?4)",
    )
    .bind(run)
    .bind(version)
    .bind(created_at)
    .bind(pass)
    .execute(pool)
    .await
    .expect("seed walk_forward_run");
}

/// The version's `(pointer, certifying pass)` pair, straight off the row.
async fn pointer(pool: &SqlitePool, version: &str) -> (Option<String>, Option<i64>) {
    sqlx::query_as(
        "SELECT v.latest_walk_forward_run_id, w.pass \
         FROM strategy_version v \
         LEFT JOIN walk_forward_run w ON w.id = v.latest_walk_forward_run_id \
         WHERE v.id = ?1",
    )
    .bind(version)
    .fetch_one(pool)
    .await
    .expect("the version row reads")
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

/// The columns every `backtest_run` row carries, plus `0012`'s window trio and
/// `0013`'s membership pair (NULL here — this run is nobody's fold).
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

/// A `proposed` coaching session + open proposal, the minimum a
/// `coaching_proposals` row needs (its `session_must_be_proposed` trigger reads
/// the session's outcome). The session names a real run — `backtest_run_id` is
/// NOT NULL and FK'd.
async fn seed_proposal(pool: &SqlitePool, proposal: &str, session: &str) {
    seed_run(pool, &format!("run-{session}")).await;
    sqlx::query(
        "INSERT INTO coaching_sessions \
         (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
          failure_kind, failure_detail, schema_version, request_fingerprint) \
         VALUES (?1, ?2, 'ver-1', '2026-08-29T00:00:00.000Z', NULL, 'proposed', \
                 NULL, NULL, 1, \
                 'aa11bb22cc33dd44ee55ff6600778899aabbccddeeff00112233445566778899')",
    )
    .bind(session)
    .bind(format!("run-{session}"))
    .execute(pool)
    .await
    .expect("seed coaching_session");

    sqlx::query(
        "INSERT INTO coaching_proposals \
         (id, session_id, mutation, hypothesis, disposition, child_version_id, \
          accepted_run_id, accept_failure_stage, accept_failure_detail) \
         VALUES (?1, ?2, '{\"type\":\"set_param\"}', 'a slower RSI', 'proposed', \
                 NULL, NULL, NULL, NULL)",
    )
    .bind(proposal)
    .bind(session)
    .execute(pool)
    .await
    .expect("seed coaching_proposal");
}

/// The whole proposal row, ordered — for the byte-for-byte rebuild comparison.
#[allow(clippy::type_complexity)]
async fn proposal_row(pool: &SqlitePool, id: &str) -> Vec<Option<String>> {
    let row: (
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT id, session_id, mutation, hypothesis, disposition, child_version_id, \
                accepted_run_id, accept_failure_stage, accept_failure_detail \
         FROM coaching_proposals WHERE id = ?1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("the proposal reads");
    vec![
        Some(row.0),
        Some(row.1),
        Some(row.2),
        Some(row.3),
        Some(row.4),
        row.5,
        row.6,
        row.7,
        row.8,
    ]
}

// ---------------------------------------------------------------------------
// the migration itself
// ---------------------------------------------------------------------------

/// A fresh database migrates cleanly through 0014 — the column, both
/// certification triggers, the rebuilt proposals table and all five recreated
/// coaching triggers land.
#[tokio::test]
async fn a_fresh_database_migrates_through_0014() {
    let tmp = TempDir::new().unwrap();
    let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
    MIGRATOR.run(db.pool()).await.expect("run embedded set");

    let applied = applied_versions(db.pool()).await;
    assert!(applied.contains(&14), "0014 must be applied: {applied:?}");

    let columns = columns_of(db.pool(), "strategy_version").await;
    assert!(
        columns.contains(&"latest_walk_forward_run_id".to_owned()),
        "strategy_version must carry the pointer: {columns:?}"
    );
    for trigger in [
        "strategy_version_no_update",
        "strategy_version_certification_owner",
        "coaching_sessions_lifecycle",
        "coaching_proposals_session_must_be_proposed",
        "coaching_proposals_accept_lineage_insert",
        "coaching_proposals_accept_lineage_update",
        "coaching_proposals_transition",
    ] {
        assert!(
            object_present(db.pool(), "trigger", trigger).await,
            "the {trigger} trigger must exist"
        );
    }

    // The indexes the schema relies on, by name: 0008's sessions index, 0009's
    // one-pending-per-run unique index, and the autoindex the rebuilt
    // `coaching_proposals` UNIQUE(session_id) must carry over.
    for index in [
        "idx_coaching_sessions_run",
        "coaching_sessions_one_pending_per_run",
        "sqlite_autoindex_coaching_proposals_1",
    ] {
        assert!(
            object_present(db.pool(), "index", index).await,
            "the {index} index must exist by name"
        );
    }

    // The rebuild landed the widened vocabulary — 'walk_forward' is a legal
    // accept-failure stage now — while every OTHER unknown word is still
    // refused by the rebuilt CHECK.
    seed_parents(db.pool()).await;
    seed_proposal(db.pool(), "prop-1", "sess-1").await;
    sqlx::query(
        "UPDATE coaching_proposals \
         SET accept_failure_stage = 'walk_forward', accept_failure_detail = 'gate refused' \
         WHERE id = 'prop-1'",
    )
    .execute(db.pool())
    .await
    .expect("'walk_forward' is a legal accept_failure_stage under 0014");

    let err = sqlx::query(
        "UPDATE coaching_proposals \
         SET accept_failure_stage = 'bogus', accept_failure_detail = 'x' \
         WHERE id = 'prop-1'",
    )
    .execute(db.pool())
    .await
    .expect_err("a stage outside the widened vocabulary is still refused");
    assert!(
        err.to_string().contains("accept_failure_stage"),
        "the refusal names the CHECKed column: {err}"
    );
}

/// A database at 0013 migrates forward to 0014, and its existing
/// `coaching_proposals` rows survive the rebuild byte-for-byte.
#[tokio::test]
async fn a_pre_0014_database_migrates_forward_preserving_every_proposal() {
    let (_tmp, _path, db) = db_at_0013().await;
    seed_parents(db.pool()).await;
    seed_proposal(db.pool(), "prop-1", "sess-1").await;
    // A second proposal already carrying a recorded 0008-stage failure — the
    // shape the rebuild must carry across untouched.
    seed_proposal(db.pool(), "prop-2", "sess-2").await;
    sqlx::query(
        "UPDATE coaching_proposals \
         SET accept_failure_stage = 'backtest', accept_failure_detail = 'no trades' \
         WHERE id = 'prop-2'",
    )
    .execute(db.pool())
    .await
    .expect("record a 0008-stage failure pre-rebuild");

    let before_1 = proposal_row(db.pool(), "prop-1").await;
    let before_2 = proposal_row(db.pool(), "prop-2").await;

    MIGRATOR.run(db.pool()).await.expect("0014 applies");

    let applied = applied_versions(db.pool()).await;
    assert!(applied.contains(&14), "0014 must be applied: {applied:?}");
    assert_eq!(
        proposal_row(db.pool(), "prop-1").await,
        before_1,
        "prop-1 is byte-for-byte identical across the rebuild"
    );
    assert_eq!(
        proposal_row(db.pool(), "prop-2").await,
        before_2,
        "prop-2 (with a recorded failure) is byte-for-byte identical"
    );

    // And under 0014 the pointer column reads NULL on pre-existing versions.
    let (ptr, pass) = pointer(db.pool(), "ver-1").await;
    assert!(
        ptr.is_none() && pass.is_none(),
        "pre-0014 rows are uncertified"
    );
}

/// A `strategy_version` row written before 0014 reads back uncertified — NULL
/// pointer, no joined `pass` — through the same LEFT JOIN shape the read path
/// runs. (The repository-level `get_version`/`list_versions` derived-`certified`
/// assertions live in `tests/strategy_certification.rs` — a hand-seeded row
/// cannot survive `get_version`'s hash/DSL re-validation, so the seed here is
/// raw SQL and the assertion reads the columns the adapter projects.)
#[tokio::test]
async fn a_pre_0014_version_reads_uncertified() {
    let (_tmp, _path, db) = db_at_0013().await;
    seed_parents(db.pool()).await;

    MIGRATOR.run(db.pool()).await.expect("0014 applies");

    for version in ["ver-1", "ver-2"] {
        let (ptr, pass) = pointer(db.pool(), version).await;
        assert!(
            ptr.is_none() && pass.is_none(),
            "{version}: no pointer, no certifying run — derived certified is false"
        );
    }
}

// ---------------------------------------------------------------------------
// the pointer's law — ownership and chronology
// ---------------------------------------------------------------------------

/// The pointer may only name a walk-forward run OF THIS VERSION — another
/// version's run is refused even when the run exists and passed.
#[tokio::test]
async fn the_pointer_cannot_borrow_another_versions_run() {
    let (_tmp, db) = db_at_0014().await;
    seed_walk_forward_run(
        db.pool(),
        "wf-other",
        "ver-2",
        "2026-08-29T01:00:00.000Z",
        1,
    )
    .await;

    let err = sqlx::query(
        "UPDATE strategy_version SET latest_walk_forward_run_id = 'wf-other' WHERE id = 'ver-1'",
    )
    .execute(db.pool())
    .await
    .expect_err("another version's run must be refused");
    assert!(
        err.to_string()
            .contains("must name a walk-forward run of this version"),
        "the refusal is the ownership rule's: {err}"
    );
    let (ptr, _) = pointer(db.pool(), "ver-1").await;
    assert!(ptr.is_none(), "the refused write left the pointer NULL");
}

/// Once set the pointer only advances in `(created_at, id)` order — a newer
/// run lands, an older-or-equal one is refused, and NULL is a backward move
/// (the product de-certifies by writing a NEWER failing run, never by
/// clearing).
#[tokio::test]
async fn the_pointer_only_advances() {
    let (_tmp, db) = db_at_0014().await;
    seed_walk_forward_run(db.pool(), "wf-old", "ver-1", "2026-08-29T01:00:00.000Z", 1).await;
    seed_walk_forward_run(db.pool(), "wf-new", "ver-1", "2026-08-29T02:00:00.000Z", 0).await;
    // Same timestamp as wf-new but an id that sorts BEFORE it — the (created_at,
    // id) tiebreak makes this a backward move, not a lateral one.
    seed_walk_forward_run(db.pool(), "wf-aaa", "ver-1", "2026-08-29T02:00:00.000Z", 1).await;

    // First set: NULL -> a run of this version lands.
    sqlx::query(
        "UPDATE strategy_version SET latest_walk_forward_run_id = 'wf-new' WHERE id = 'ver-1'",
    )
    .execute(db.pool())
    .await
    .expect("the first pointer set lands");

    // Backward in (created_at, id): same instant, lower id — refused.
    let err = sqlx::query(
        "UPDATE strategy_version SET latest_walk_forward_run_id = 'wf-aaa' WHERE id = 'ver-1'",
    )
    .execute(db.pool())
    .await
    .expect_err("an id that sorts backward at the same instant is refused");
    assert!(
        err.to_string().contains("only advances"),
        "the refusal is the chronology rule's: {err}"
    );

    // Backward in time: the older run — refused.
    let err = sqlx::query(
        "UPDATE strategy_version SET latest_walk_forward_run_id = 'wf-old' WHERE id = 'ver-1'",
    )
    .execute(db.pool())
    .await
    .expect_err("an older run is refused");
    assert!(err.to_string().contains("only advances"), "{err}");

    // Clearing is a backward move too — refused, not a quiet de-certify.
    let err = sqlx::query(
        "UPDATE strategy_version SET latest_walk_forward_run_id = NULL WHERE id = 'ver-1'",
    )
    .execute(db.pool())
    .await
    .expect_err("NULL over a set pointer is refused");
    assert!(err.to_string().contains("only advances"), "{err}");

    // A forward move — including to a FAILING run — is the legal write. This is
    // the de-certification path: pointer to the newer failure.
    seed_walk_forward_run(db.pool(), "wf-fail", "ver-1", "2026-08-29T03:00:00.000Z", 0).await;
    sqlx::query(
        "UPDATE strategy_version SET latest_walk_forward_run_id = 'wf-fail' WHERE id = 'ver-1'",
    )
    .execute(db.pool())
    .await
    .expect("a newer failing run advances the pointer");
    let (ptr, pass) = pointer(db.pool(), "ver-1").await;
    assert_eq!(ptr.as_deref(), Some("wf-fail"));
    assert_eq!(pass, Some(0), "the joined pass reads false — de-certified");
}

/// The narrowed immutability trigger still refuses every 0001 column — and an
/// UPDATE that sets the pointer AND smuggles a second column is refused whole.
#[tokio::test]
async fn the_narrowed_immutability_trigger_still_refuses_the_other_columns() {
    let (_tmp, db) = db_at_0014().await;
    seed_walk_forward_run(db.pool(), "wf-1", "ver-1", "2026-08-29T01:00:00.000Z", 1).await;

    let err = sqlx::query("UPDATE strategy_version SET dsl = '{\"x\":1}' WHERE id = 'ver-1'")
        .execute(db.pool())
        .await
        .expect_err("a 0001 column is still immutable");
    assert!(
        err.to_string()
            .contains("strategy_version is immutable except latest_walk_forward_run_id"),
        "the refusal is the narrowed trigger's own message: {err}"
    );

    // Pointer + smuggled column in one UPDATE: refused, and the pointer stays
    // NULL — the smuggle does not ride a legal write in.
    let err = sqlx::query(
        "UPDATE strategy_version \
         SET latest_walk_forward_run_id = 'wf-1', dsl = '{\"x\":1}' WHERE id = 'ver-1'",
    )
    .execute(db.pool())
    .await
    .expect_err("the pointer does not admit a second column's change");
    assert!(err.to_string().contains("immutable except"), "{err}");
    let (ptr, _) = pointer(db.pool(), "ver-1").await;
    assert!(ptr.is_none(), "the refused write changed nothing");

    // The one legal write, alone: the pointer and nothing else.
    sqlx::query(
        "UPDATE strategy_version SET latest_walk_forward_run_id = 'wf-1' WHERE id = 'ver-1'",
    )
    .execute(db.pool())
    .await
    .expect("the pointer alone updates");
}

// ---------------------------------------------------------------------------
// the down — truthful or not at all
// ---------------------------------------------------------------------------

/// The down REFUSES while any pointer is set — a set `latest_walk_forward_run_id`
/// is certification state 0013 cannot represent, and dropping it would falsify
/// the record (the 0010/0011/0012/0013 refusal pattern).
#[tokio::test]
async fn the_down_refuses_a_set_pointer() {
    let (_tmp, db) = db_at_0014().await;
    seed_walk_forward_run(db.pool(), "wf-1", "ver-1", "2026-08-29T01:00:00.000Z", 1).await;
    sqlx::query(
        "UPDATE strategy_version SET latest_walk_forward_run_id = 'wf-1' WHERE id = 'ver-1'",
    )
    .execute(db.pool())
    .await
    .expect("set the pointer");

    let err = undo_to(db.pool(), 13)
        .await
        .expect_err("a set pointer must refuse the 0014 down");
    assert!(
        err.to_string().contains("0014"),
        "the refusal names the migration: {err}"
    );
    assert_eq!(
        applied_max(db.pool()).await,
        14,
        "the refusal is transactional — the database stays at 0014"
    );
}

/// The down REFUSES while any proposal records `walk_forward` — a stage the
/// 0008 vocabulary cannot store.
#[tokio::test]
async fn the_down_refuses_a_recorded_walk_forward_stage() {
    let (_tmp, db) = db_at_0014().await;
    seed_proposal(db.pool(), "prop-1", "sess-1").await;
    sqlx::query(
        "UPDATE coaching_proposals \
         SET accept_failure_stage = 'walk_forward', accept_failure_detail = 'gate refused' \
         WHERE id = 'prop-1'",
    )
    .execute(db.pool())
    .await
    .expect("record the new stage");

    let err = undo_to(db.pool(), 13)
        .await
        .expect_err("a recorded walk_forward stage must refuse the down");
    assert!(err.to_string().contains("0014"), "{err}");
    assert_eq!(applied_max(db.pool()).await, 14);
}

/// With nothing the 0013 shape cannot hold, the down restores it exactly: the
/// blanket immutability trigger with its 0001 message, no pointer column, and
/// `coaching_proposals` back to the 0008 vocabulary — a `walk_forward` stage is
/// refused again. Then the up re-applies: a clean round trip.
#[tokio::test]
async fn the_down_restores_the_0013_shape() {
    let (_tmp, db) = db_at_0014().await;
    seed_proposal(db.pool(), "prop-1", "sess-1").await;
    let before = proposal_row(db.pool(), "prop-1").await;

    undo_to(db.pool(), 13).await.expect("the clean down runs");
    assert_eq!(applied_max(db.pool()).await, 13, "back at the 0013 max");

    let columns = columns_of(db.pool(), "strategy_version").await;
    assert!(
        !columns.contains(&"latest_walk_forward_run_id".to_owned()),
        "the pointer column is gone: {columns:?}"
    );
    assert!(
        !object_present(db.pool(), "trigger", "strategy_version_certification_owner").await,
        "the certification trigger is gone"
    );

    // 0001's blanket trigger is back — its ORIGINAL message proves it.
    let err = sqlx::query("UPDATE strategy_version SET dsl = '{\"x\":1}' WHERE id = 'ver-1'")
        .execute(db.pool())
        .await
        .expect_err("the blanket trigger refuses any update");
    assert!(
        err.to_string().contains("strategy_version is immutable"),
        "the 0001 message is back verbatim: {err}"
    );

    // The 0008 vocabulary is back — 'walk_forward' is refused again.
    let err = sqlx::query(
        "UPDATE coaching_proposals \
         SET accept_failure_stage = 'walk_forward', accept_failure_detail = 'x' \
         WHERE id = 'prop-1'",
    )
    .execute(db.pool())
    .await
    .expect_err("the 0008 CHECK refuses the new word");
    assert!(
        err.to_string().contains("accept_failure_stage"),
        "the refusal names the narrowed column: {err}"
    );

    // The proposal row itself is byte-for-byte what the 0014 rebuild carried.
    assert_eq!(proposal_row(db.pool(), "prop-1").await, before);

    // And the triggers the rebuild moved are all back.
    for trigger in [
        "coaching_sessions_lifecycle",
        "coaching_proposals_session_must_be_proposed",
        "coaching_proposals_accept_lineage_insert",
        "coaching_proposals_accept_lineage_update",
        "coaching_proposals_transition",
    ] {
        assert!(
            object_present(db.pool(), "trigger", trigger).await,
            "{trigger} is recreated by the down"
        );
    }

    // The round trip closes: 0014 re-applies on top of the restored 0013.
    MIGRATOR.run(db.pool()).await.expect("re-run to 0014");
    assert_eq!(applied_max(db.pool()).await, 14);
    assert_eq!(proposal_row(db.pool(), "prop-1").await, before);
}
