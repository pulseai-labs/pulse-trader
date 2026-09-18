//! AC-1 — migration `0009_external_agent_window_claim` (r2.s1.w1, ADR-0010 /
//! ADR-0015 / ADR-0018 / ADR-0019 / ADR-0021).
//!
//! Three schema shapes `0008` cannot say, landed in one forward-only migration:
//! a normalized `agent_submission` ledger row for versions an external coding
//! agent wrote (`external_agent` provenance — the audit row a coach version
//! gets from its coaching session and `llm_call`), the optional date window a
//! run consumed (`window_from_ms` / `window_to_ms`, UTC epoch ms, `[from, to)`),
//! and `pulseai-labs/pulse-trader#158`'s one-pending-claim-per-run guarantee,
//! enforced by the system of record rather than a process-local registry.
//!
//! **Why raw SQL.** As with `migration_0008`, the value of `0009` is in the
//! shapes it REFUSES, so the suite drives raw SQL at the schema — the only way
//! to prove a constraint holds against something written around the adapter.
//! The repository-level half of the claim index lives in
//! `tests/coaching_repo.rs` (AC-4); the window's adapter round-trip lives in
//! `tests/backtest_provenance.rs` (AC-3).
//!
//! Offline (`SQLX_OFFLINE=true` + the in-process `MIGRATOR`), `TempDir`-isolated.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{Db, MIGRATOR, undo_to};
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const FINGERPRINT_A: &str = "aa11bb22cc33dd44ee55ff6600778899aabbccddeeff00112233445566778899";
const FINGERPRINT_B: &str = "bb11bb22cc33dd44ee55ff6600778899aabbccddeeff00112233445566778899";

// ---------------------------------------------------------------------------
// helpers — databases at 0008 and at 0009
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

/// Copy the shipped `migrations/` set into `dir`, SKIPPING `0009_*` — the binary
/// that shipped `0008` while this item's contract was still being planned.
fn shipped_set_without_0009(dir: &Path) {
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(&shipped).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("0009_") {
            continue;
        }
        std::fs::copy(&path, dir.join(&name)).unwrap();
    }
}

/// A fresh temp database migrated by the "older" set (everything but `0009`).
async fn db_at_0008() -> (TempDir, PathBuf, Db) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    shipped_set_without_0009(&dir);

    let db_path = tmp.path().join("pulse.db");
    let older = Migrator::new(dir.as_path()).await.unwrap();
    let db = Db::with_path(&db_path).await.unwrap();
    older.run(db.pool()).await.expect("the older set applies");

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

/// A fresh temp database at the full embedded set (0009 included), seeded.
async fn db_at_0009() -> (TempDir, Db) {
    let tmp = TempDir::new().unwrap();
    let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
    MIGRATOR.run(db.pool()).await.expect("run embedded set");
    seed_parents(db.pool()).await;
    (tmp, db)
}

/// The FK parents every new shape needs: one strategy, its versions in the three
/// provenance forms the kind trigger distinguishes, and the runs claims attach
/// to. `created_by` carries the serde-JSON form the adapter writes
/// (`"external_agent"` WITH its quotes) — the `agent_submission_version_kind`
/// trigger compares against exactly that literal.
async fn seed_parents(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO strategy (id, name, tags, archived, created_at) \
         VALUES ('strat-1', 'RSI Oversold', '[]', 0, '2026-08-29T00:00:00.000Z')",
    )
    .execute(pool)
    .await
    .expect("seed strategy");

    for (id, by) in [
        ("ver-ext", "\"external_agent\""),
        ("ver-ext-2", "\"external_agent\""),
        ("ver-coach", "\"coach_llm\""),
        ("ver-human", "\"human\""),
    ] {
        sqlx::query(
            "INSERT INTO strategy_version \
             (id, strategy_id, parent_version_id, dsl_schema_version, dsl, dsl_original, \
              version_hash, created_by, creating_llm_call_ids, created_at) \
             VALUES (?1, 'strat-1', NULL, '1.0.0', '{}', '{}', ?2, ?3, '[]', \
                     '2026-08-29T00:00:00.000Z')",
        )
        .bind(id)
        .bind(format!("hash-{id}"))
        .bind(by)
        .execute(pool)
        .await
        .expect("seed strategy_version");
    }

    for run in ["run-1", "run-2"] {
        seed_run(pool, run, "ver-human").await;
    }
}

/// One `backtest_run` row owned by `version`, with `NULL` window bounds — the
/// shape every pre-0009 row and every unwindowed new run takes. `0006`'s
/// completeness trigger wants the input provenance present.
async fn seed_run(pool: &SqlitePool, run: &str, version: &str) {
    sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
          engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
          funding_total, slippage_total, pair, primary_timeframe, primary_data_version, \
          taker_fee_bps, slippage_bps, funding_config) \
         VALUES (?1, ?2, '1', '2026-08-29T00:00:00.000Z', 'fp-1', 'test-target', \
                 'rch-1', '10000', '0', '0', '0', '0', \
                 'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates')",
    )
    .bind(run)
    .bind(version)
    .execute(pool)
    .await
    .expect("seed backtest_run");
}

// ---------------------------------------------------------------------------
// raw-SQL writers — the way a row written AROUND the adapter gets in
// ---------------------------------------------------------------------------

async fn insert_submission(
    pool: &SqlitePool,
    id: &str,
    version: &str,
    agent_name: &str,
    hypothesis: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO agent_submission (id, version_id, agent_name, hypothesis, created_at) \
         VALUES (?1, ?2, ?3, ?4, '2026-08-29T00:00:00.000Z')",
    )
    .bind(id)
    .bind(version)
    .bind(agent_name)
    .bind(hypothesis)
    .execute(pool)
    .await
    .map(|_| ())
}

async fn insert_run_with_window(
    pool: &SqlitePool,
    run: &str,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
          engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
          funding_total, slippage_total, pair, primary_timeframe, primary_data_version, \
          taker_fee_bps, slippage_bps, funding_config, window_from_ms, window_to_ms) \
         VALUES (?1, 'ver-human', '1', '2026-08-29T00:00:00.000Z', 'fp-1', 'test-target', \
                 'rch-1', '10000', '0', '0', '0', '0', \
                 'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates', ?2, ?3)",
    )
    .bind(run)
    .bind(from_ms)
    .bind(to_ms)
    .execute(pool)
    .await
    .map(|_| ())
}

async fn insert_session(
    pool: &SqlitePool,
    id: &str,
    run: &str,
    outcome: &str,
    failure_kind: Option<&str>,
    failure_detail: Option<&str>,
    fingerprint: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO coaching_sessions \
         (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
          failure_kind, failure_detail, schema_version, request_fingerprint) \
         VALUES (?1, ?2, 'ver-human', '2026-08-29T00:00:00.000Z', NULL, ?3, ?4, ?5, 1, ?6)",
    )
    .bind(id)
    .bind(run)
    .bind(outcome)
    .bind(failure_kind)
    .bind(failure_detail)
    .bind(fingerprint)
    .execute(pool)
    .await
    .map(|_| ())
}

// ===========================================================================
// 1. The migration itself — every pre-0009 row survives, logically unchanged
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0009_preserves_every_pre_0009_row() {
    type RunRow = (String, String);
    type SessionRow = (String, String, String);

    let (_tmp, _path, db) = db_at_0008().await;
    let pool = db.pool();
    seed_parents(pool).await;

    // One pending claim per run — the most pending rows a 0009-conformant db
    // may ever hold — plus a settled one, across two runs.
    insert_session(
        pool,
        "sess-pending-1",
        "run-1",
        "pending",
        None,
        None,
        Some(FINGERPRINT_A),
    )
    .await
    .expect("a pending claim on run-1");
    insert_session(
        pool,
        "sess-failed-2",
        "run-2",
        "failed",
        Some("zero_calls"),
        Some(r#"{"type":"zero_calls"}"#),
        None,
    )
    .await
    .expect("a settled session on run-2");

    let runs_before: Vec<RunRow> =
        sqlx::query_as("SELECT id, strategy_version_id FROM backtest_run ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
    let sessions_before: Vec<SessionRow> =
        sqlx::query_as("SELECT id, backtest_run_id, outcome FROM coaching_sessions ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();

    MIGRATOR.run(pool).await.expect("0009 applies over 0008");
    assert!(
        applied_versions(pool).await.contains(&9),
        "0009 must be applied"
    );

    let runs_after: Vec<RunRow> =
        sqlx::query_as("SELECT id, strategy_version_id FROM backtest_run ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(runs_after, runs_before, "no run is dropped or reparented");
    let sessions_after: Vec<SessionRow> =
        sqlx::query_as("SELECT id, backtest_run_id, outcome FROM coaching_sessions ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(sessions_after, sessions_before, "no session is dropped");

    // Pre-0009 runs carry the whole-snapshot window: NULL/NULL, never a guess.
    let bounds: Vec<(Option<i64>, Option<i64>)> =
        sqlx::query_as("SELECT window_from_ms, window_to_ms FROM backtest_run ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
    assert!(
        bounds.iter().all(|(f, t)| f.is_none() && t.is_none()),
        "every pre-0009 run keeps NULL/NULL window bounds, got {bounds:?}"
    );

    // The new objects exist on the migrated schema.
    for (kind, name) in [
        ("table", "agent_submission"),
        ("trigger", "agent_submission_no_update"),
        ("trigger", "agent_submission_no_delete"),
        ("trigger", "agent_submission_version_kind"),
        ("trigger", "backtest_run_window_pair"),
        ("index", "coaching_sessions_one_pending_per_run"),
    ] {
        assert!(
            object_present(pool, kind, name).await,
            "{kind} {name} must exist after 0009"
        );
    }
}

/// `0008` allowed two pending claims on one run; `0009`'s partial unique index
/// cannot be built over such a table. The migration REFUSES — it does not pick
/// a winner, demote one claim, or drop the loser — so a crash-resumed binary
/// that genuinely double-claimed surfaces the fact at startup instead of
/// laundering it into a clean schema.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0009_refuses_a_db_with_two_pending_claims_on_one_run() {
    let (_tmp, _path, db) = db_at_0008().await;
    let pool = db.pool();
    seed_parents(pool).await;

    insert_session(
        pool,
        "sess-pending-a",
        "run-1",
        "pending",
        None,
        None,
        Some(FINGERPRINT_A),
    )
    .await
    .expect("first pending claim — legal under 0008");
    insert_session(
        pool,
        "sess-pending-b",
        "run-1",
        "pending",
        None,
        None,
        Some(FINGERPRINT_B),
    )
    .await
    .expect("second pending claim — legal under 0008");

    assert!(
        MIGRATOR.run(pool).await.is_err(),
        "the unique index cannot be built over two live claims on one run"
    );

    // The refusal is transactional: 0009 never lands, both claims survive.
    let applied = applied_versions(pool).await;
    assert!(
        !applied.contains(&9),
        "a refused migration leaves no applied 0009 row: {applied:?}"
    );
    assert!(
        !object_present(pool, "table", "agent_submission").await,
        "the ledger table does not half-exist after the refusal"
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM coaching_sessions WHERE backtest_run_id = 'run-1' \
         AND outcome = 'pending'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        pending, 2,
        "both pre-existing claims are left exactly as found"
    );
}

// ===========================================================================
// 2. `agent_submission` — the ledger row an external-agent version carries
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_submission_belongs_only_to_an_external_agent_version() {
    let (_tmp, db) = db_at_0009().await;
    let pool = db.pool();

    insert_submission(
        pool,
        "sub-1",
        "ver-ext",
        "claude-code",
        "RSI 21 holds longer",
    )
    .await
    .expect("a submission on an external_agent version is the normal shape");

    for (label, version) in [
        ("a coach_llm", "ver-coach"),
        ("a human", "ver-human"),
        ("a nonexistent", "ver-missing"),
    ] {
        assert!(
            insert_submission(
                pool,
                &format!("sub-{label}"),
                version,
                "claude-code",
                "a hypothesis"
            )
            .await
            .is_err(),
            "a submission on {label} version must be refused"
        );
    }

    // One submission per version: the UNIQUE on version_id is what makes the
    // ledger a per-version audit row rather than an append log.
    assert!(
        insert_submission(pool, "sub-2", "ver-ext", "claude-code", "again")
            .await
            .is_err(),
        "a second submission on the same version must be refused"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_submission_is_immutable_once_written() {
    let (_tmp, db) = db_at_0009().await;
    let pool = db.pool();
    insert_submission(pool, "sub-1", "ver-ext", "claude-code", "a hypothesis")
        .await
        .expect("seed");

    for (label, sql) in [
        (
            "an update",
            "UPDATE agent_submission SET hypothesis = 'rewritten' WHERE id = 'sub-1'",
        ),
        (
            "a delete",
            "DELETE FROM agent_submission WHERE id = 'sub-1'",
        ),
    ] {
        assert!(
            sqlx::query(sql).execute(pool).await.is_err(),
            "the ledger must refuse {label} — audit rows are append-only"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_name_and_hypothesis_bounds_are_checked_in_schema() {
    let (_tmp, db) = db_at_0009().await;
    let pool = db.pool();

    // Boundaries land; just outside them aborts.
    let name_64 = "a".repeat(64);
    let hyp_2000 = "h".repeat(2000);
    sqlx::query(
        "INSERT INTO agent_submission (id, version_id, agent_name, hypothesis, created_at) \
         VALUES ('sub-max', 'ver-ext', ?1, ?2, '2026-08-29T00:00:00.000Z')",
    )
    .bind(&name_64)
    .bind(&hyp_2000)
    .execute(pool)
    .await
    .expect("the boundary lengths are storable");

    // The negative rows MUST target an `external_agent` version — the kind
    // trigger aborts a `ver-human` insert before the CHECKs are ever
    // evaluated, so aiming them there would prove nothing. `ver-ext-2` keeps
    // `ver-ext`'s UNIQUE(version_id) free for `sub-max` above; every other
    // constraint on these rows is valid, so the length CHECK is the ONLY
    // thing that can refuse them.
    for (label, name, hyp) in [
        ("an empty agent_name", "", "a hypothesis"),
        ("a 65-char agent_name", &"n".repeat(65), "a hypothesis"),
        ("an empty hypothesis", "claude-code", ""),
        ("a 2001-char hypothesis", "claude-code", &"h".repeat(2001)),
    ] {
        let err = sqlx::query(
            "INSERT INTO agent_submission (id, version_id, agent_name, hypothesis, created_at) \
             VALUES (?1, 'ver-ext-2', ?2, ?3, '2026-08-29T00:00:00.000Z')",
        )
        .bind(format!("sub-bad-{label}"))
        .bind(name)
        .bind(hyp)
        .execute(pool)
        .await
        .expect_err("{label} must be refused");
        // …by the length CHECK specifically, not the kind trigger (whose
        // RAISE message names itself) or the UNIQUE.
        let message = match &err {
            sqlx::Error::Database(db) => db.message().to_owned(),
            other => panic!("{label} produced a non-database error: {other}"),
        };
        assert!(
            message.contains("CHECK constraint failed"),
            "{label} must fail the length CHECK, got: {message}"
        );
    }
}

// ===========================================================================
// 3. `backtest_run` window columns — both-or-neither, half-open [from, to)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_window_is_both_bounds_or_neither_and_from_before_to() {
    let (_tmp, db) = db_at_0009().await;
    let pool = db.pool();

    insert_run_with_window(pool, "run-whole", None, None)
        .await
        .expect("NULL/NULL is the whole snapshot");
    insert_run_with_window(
        pool,
        "run-windowed",
        Some(1_700_000_000_000),
        Some(1_700_086_400_000),
    )
    .await
    .expect("from < to is a real window");

    for (label, from, to) in [
        ("from without to", Some(1_700_000_000_000_i64), None),
        ("to without from", None, Some(1_700_086_400_000_i64)),
        (
            "an empty window",
            Some(1_700_000_000_000_i64),
            Some(1_700_000_000_000_i64),
        ),
        (
            "a backwards window",
            Some(1_700_086_400_000_i64),
            Some(1_700_000_000_000_i64),
        ),
    ] {
        assert!(
            insert_run_with_window(pool, &format!("run-{label}"), from, to)
                .await
                .is_err(),
            "{label} must be refused: bounds are both-or-neither and from < to"
        );
    }
}

// ===========================================================================
// 4. `coaching_sessions_one_pending_per_run` — #158, in the system of record
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_pending_claim_per_run_is_enforced_and_partial() {
    let (_tmp, db) = db_at_0009().await;
    let pool = db.pool();

    // The index is partial: it guards the live claim, not the history.
    let sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type='index' \
         AND name='coaching_sessions_one_pending_per_run'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert!(
        sql.contains("pending"),
        "the index must be a partial index over pending rows, got: {sql}"
    );

    insert_session(
        pool,
        "sess-1",
        "run-1",
        "pending",
        None,
        None,
        Some(FINGERPRINT_A),
    )
    .await
    .expect("the first claim owns the run");
    assert!(
        insert_session(
            pool,
            "sess-2",
            "run-1",
            "pending",
            None,
            None,
            Some(FINGERPRINT_B)
        )
        .await
        .is_err(),
        "a second pending claim on the same run must be refused"
    );

    // A different run is unguarded — the claim is per-run, not global.
    insert_session(
        pool,
        "sess-3",
        "run-2",
        "pending",
        None,
        None,
        Some(FINGERPRINT_B),
    )
    .await
    .expect("a pending claim on another run is independent");

    // A settled claim frees the run: settling sess-1 to failed admits a new one.
    sqlx::query(
        "UPDATE coaching_sessions SET outcome='failed', failure_kind='zero_calls', \
         failure_detail='{}' WHERE id='sess-1'",
    )
    .execute(pool)
    .await
    .expect("the first claim settles");
    insert_session(
        pool,
        "sess-4",
        "run-1",
        "pending",
        None,
        None,
        Some(FINGERPRINT_A),
    )
    .await
    .expect("a settled first claim admits a second claim on the run");
}

// ===========================================================================
// 5. Down — exact 0008 shape, and a transactional refusal when it would lie
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn representable_data_survives_down_and_up_again() {
    type SessionRow = (String, String, String);

    let (_tmp, db) = db_at_0009().await;
    let pool = db.pool();

    // Everything seeded so far is 0008-representable — INCLUDING a pending
    // claim: pending is a 0008 shape, and dropping the index loses a guarantee,
    // not a row.
    insert_session(
        pool,
        "sess-pending",
        "run-1",
        "pending",
        None,
        None,
        Some(FINGERPRINT_A),
    )
    .await
    .expect("a pending claim is 0008-representable");
    insert_session(
        pool,
        "sess-failed",
        "run-2",
        "failed",
        Some("zero_calls"),
        Some(r#"{"type":"zero_calls"}"#),
        None,
    )
    .await
    .expect("a settled session");

    let sessions_before: Vec<SessionRow> =
        sqlx::query_as("SELECT id, backtest_run_id, outcome FROM coaching_sessions ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
    let run_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backtest_run")
        .fetch_one(pool)
        .await
        .unwrap();

    undo_to(pool, 8)
        .await
        .expect("0009 down over 0008-representable data");

    for (kind, name) in [
        ("table", "agent_submission"),
        ("trigger", "agent_submission_no_update"),
        ("trigger", "agent_submission_no_delete"),
        ("trigger", "agent_submission_version_kind"),
        ("trigger", "backtest_run_window_pair"),
        ("index", "coaching_sessions_one_pending_per_run"),
    ] {
        assert!(
            !object_present(pool, kind, name).await,
            "{kind} {name} must not outlive the down migration"
        );
    }
    for column in ["window_from_ms", "window_to_ms"] {
        assert!(
            !columns_of(pool, "backtest_run")
                .await
                .contains(&column.to_owned()),
            "backtest_run.{column} must be gone under the 0008 shape"
        );
    }
    let sessions_after: Vec<SessionRow> =
        sqlx::query_as("SELECT id, backtest_run_id, outcome FROM coaching_sessions ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(sessions_after, sessions_before, "no session is dropped");
    let run_count_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backtest_run")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(run_count_after, run_count, "no run is dropped");

    MIGRATOR.run(pool).await.expect("0009 re-applies");
    assert!(applied_versions(pool).await.contains(&9), "0009 is back");
    let sessions_rerun: Vec<SessionRow> =
        sqlx::query_as("SELECT id, backtest_run_id, outcome FROM coaching_sessions ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(sessions_rerun, sessions_before, "and back again");
}

/// A `pending` claim does NOT block the downgrade the way it blocked `0008`'s:
/// under `0008` pending is a native shape, and what the down loses is the index
/// — a guarantee about FUTURE writes, not a record.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_claim_alone_does_not_block_the_downgrade() {
    let (_tmp, db) = db_at_0009().await;
    let pool = db.pool();
    insert_session(
        pool,
        "sess-pending",
        "run-1",
        "pending",
        None,
        None,
        Some(FINGERPRINT_A),
    )
    .await
    .expect("claim");

    undo_to(pool, 8)
        .await
        .expect("a pending claim is 0008-representable; the down must not refuse it");

    let outcome: String =
        sqlx::query_scalar("SELECT outcome FROM coaching_sessions WHERE id = 'sess-pending'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(outcome, "pending", "the live claim survives the downgrade");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lossy_downgrade_is_refused_transactionally() {
    // Each seed is a state 0008 cannot represent. The down migration must refuse
    // the WHOLE downgrade rather than drop the row that carries the state.
    for (label, seed) in [
        ("an agent submission", LossySeed::AgentSubmission),
        ("a windowed run", LossySeed::WindowedRun),
    ] {
        let (_tmp, db) = db_at_0009().await;
        let pool = db.pool();
        seed.apply(pool).await;

        assert!(
            undo_to(pool, 8).await.is_err(),
            "{label} cannot be represented under 0008, so the downgrade must refuse"
        );

        // Transactional: the row that blocked the downgrade is untouched, the
        // new objects are all still there, and 0009 is still applied.
        assert!(
            object_present(pool, "table", "agent_submission").await,
            "{label}: a refused downgrade leaves the 0009 shape in place"
        );
        assert!(
            object_present(pool, "index", "coaching_sessions_one_pending_per_run").await,
            "{label}: a refused downgrade leaves the claim index in place"
        );
        match seed {
            LossySeed::AgentSubmission => {
                let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_submission")
                    .fetch_one(pool)
                    .await
                    .unwrap();
                assert_eq!(n, 1, "{label}: the submission row is not dropped");
            }
            LossySeed::WindowedRun => {
                let bounds: (Option<i64>, Option<i64>) = sqlx::query_as(
                    "SELECT window_from_ms, window_to_ms FROM backtest_run \
                     WHERE id = 'run-windowed'",
                )
                .fetch_one(pool)
                .await
                .unwrap();
                assert_eq!(
                    bounds,
                    (Some(1_700_000_000_000), Some(1_700_086_400_000)),
                    "{label}: the window is not coerced or dropped"
                );
            }
        }
        assert!(
            applied_versions(pool).await.contains(&9),
            "{label}: 0009 is still applied after the refusal"
        );
    }
}

#[derive(Clone, Copy)]
enum LossySeed {
    AgentSubmission,
    WindowedRun,
}

impl LossySeed {
    async fn apply(&self, pool: &SqlitePool) {
        match self {
            Self::AgentSubmission => {
                insert_submission(pool, "sub-1", "ver-ext", "claude-code", "a hypothesis")
                    .await
                    .expect("seed");
            }
            Self::WindowedRun => {
                insert_run_with_window(
                    pool,
                    "run-windowed",
                    Some(1_700_000_000_000),
                    Some(1_700_086_400_000),
                )
                .await
                .expect("seed");
            }
        }
    }
}
