//! AC-1 — migration `0008_coaching_lifecycle` and the coach lifecycle storage
//! contract it makes representable (r1.s4.w4, ADR-0010 / ADR-0018 / ADR-0019 /
//! ADR-0021).
//!
//! Merged `0005` cannot say four things the coach rail needs to be able to say:
//! that a session id was CLAIMED before the provider call, that a turn failed
//! because the advice was structural or the parent run's inputs were missing, that
//! an accept was ATTEMPTED and failed without minting a child, and that an accepted
//! proposal names both its child version and the re-backtest run of that child.
//! `0008` is the forward-only migration that adds exactly those, and this binary is
//! its proof.
//!
//! **Why this file is not "one happy-path migrate".** The value of `0008` is in the
//! shapes it REFUSES, so most of what follows drives raw SQL at the schema — the
//! only way to prove a constraint holds against something written around the
//! adapter. The repository half then proves the same contract through the real
//! `SqliteCoachingRepo` / `SqliteCoachAcceptanceRepo`, so a mutation to production
//! repository behaviour reds this binary too, not only a mutation to the SQL.
//!
//! Offline (`SQLX_OFFLINE=true` + the in-process `MIGRATOR`), `TempDir`-isolated,
//! deterministic through a `FakeClock` and a `SeqIdSource` — the suite never
//! touches the real `pulse.db`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use chrono::{DateTime, SecondsFormat};
use pulse::{
    AcceptFailureStage, BacktestInputs, BacktestResult, BacktestRunId, CoachAcceptFailure,
    CoachAcceptanceRepository, CoachFailure, CoachRequestFingerprint, CoachSessionClaim,
    CoachSessionClaimResult, CoachingRepository, CoachingSession, CoachingSessionId, Comparator,
    Condition, DataVersion, Db, Direction, Disposition, EquityCurve, ExitReason, ExitRule,
    FakeClock, Fill, FundingConfig, Hypothesis, IndicatorSpec, InitialCoachOutcome, LlmCallId,
    MIGRATOR, Mutation, Pair, ParamValue, PreparedBacktest, PreparedCoachAcceptance, Proposal,
    Regime, RegimeBreakdown, RiskParams, SchemaVersion, SeqIdSource, Series, SessionOutcome,
    SkippedEntryCounts, SnapshotSelection, SqliteCoachAcceptanceRepo, SqliteCoachingRepo,
    StrategyDsl, SummaryStats, SweepableValue, Timeframe, Trade, TradeSource, ValueSource,
    VersionId, undo_to,
};
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const NOW_MS: i64 = 1_756_425_600_000; // 2026-08-29T00:00:00Z
const FINGERPRINT_A: &str = "aa11bb22cc33dd44ee55ff6600778899aabbccddeeff00112233445566778899";
const FINGERPRINT_B: &str = "bb11bb22cc33dd44ee55ff6600778899aabbccddeeff00112233445566778899";

// ---------------------------------------------------------------------------
// helpers — databases at 0007 and at 0008
// ---------------------------------------------------------------------------

/// The RFC3339-millis rendering of [`NOW_MS`].
fn now_rfc3339() -> String {
    DateTime::from_timestamp_millis(NOW_MS)
        .expect("clock in range")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

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

/// Copy the shipped `migrations/` set into `dir`, SKIPPING `0008_*` and later
/// — the binary that shipped `0007` while the coach rail was still being
/// planned. (`0009`'s pending-claim index only has meaning once `0008` makes
/// `pending` a legal outcome; unskipped it would also lift the fixture's
/// maximum off the `0007` pin.)
fn shipped_set_without_0008(dir: &Path) {
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(&shipped).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.as_str() >= "0008" {
            continue;
        }
        std::fs::copy(&path, dir.join(&name)).unwrap();
    }
}

/// A fresh temp database migrated by the "older" set (everything but `0008`).
async fn db_at_0007() -> (TempDir, PathBuf, Db) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    shipped_set_without_0008(&dir);

    let db_path = tmp.path().join("pulse.db");
    let older = Migrator::new(dir.as_path()).await.unwrap();
    let db = Db::with_path(&db_path).await.unwrap();
    older.run(db.pool()).await.expect("the older set applies");

    let applied = applied_versions(db.pool()).await;
    assert!(
        !applied.contains(&8),
        "the fixture must NOT have 0008 applied: {applied:?}"
    );
    assert_eq!(
        applied.iter().copied().max(),
        Some(7),
        "the fixture sits at the pre-0008 maximum"
    );
    (tmp, db_path, db)
}

/// A fresh temp database at the full embedded set (0008 included).
async fn db_at_0008() -> (TempDir, Db) {
    let tmp = TempDir::new().unwrap();
    let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
    MIGRATOR.run(db.pool()).await.expect("run embedded set");
    seed_parents(db.pool()).await;
    (tmp, db)
}

/// The FK parents the coaching tables need: one coached strategy, its version
/// tree, the runs those versions produced, and a ledger row.
///
/// The tree is the shape the accept path has to reason about: `ver-1` is the
/// coached version, `ver-2`/`ver-3` are its real children, `ver-root` has no
/// parent, `ver-sibling` descends from `ver-root`, and `ver-foreign` names `ver-1`
/// as parent while belonging to a different strategy. Each wrong shape is an
/// individually legal row and jointly a false provenance claim.
async fn seed_parents(pool: &SqlitePool) {
    for (id, name) in [("strat-1", "RSI Oversold"), ("strat-2", "Another")] {
        sqlx::query(
            "INSERT INTO strategy (id, name, tags, archived, created_at) \
             VALUES (?1, ?2, '[]', 0, '2026-08-29T00:00:00.000Z')",
        )
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .expect("seed strategy");
    }

    for (id, strategy, parent, hash, by) in [
        ("ver-1", "strat-1", None, "hash-1", "human"),
        ("ver-2", "strat-1", Some("ver-1"), "hash-2", "coach_llm"),
        ("ver-3", "strat-1", Some("ver-1"), "hash-3", "coach_llm"),
        ("ver-root", "strat-1", None, "hash-root", "human"),
        (
            "ver-sibling",
            "strat-1",
            Some("ver-root"),
            "hash-sib",
            "coach_llm",
        ),
        (
            "ver-foreign",
            "strat-2",
            Some("ver-1"),
            "hash-foreign",
            "coach_llm",
        ),
    ] {
        sqlx::query(
            "INSERT INTO strategy_version \
             (id, strategy_id, parent_version_id, dsl_schema_version, dsl, dsl_original, \
              version_hash, created_by, creating_llm_call_ids, created_at) \
             VALUES (?1, ?2, ?3, '1.0.0', '{}', '{}', ?4, ?5, '[]', \
                     '2026-08-29T00:00:00.000Z')",
        )
        .bind(id)
        .bind(strategy)
        .bind(parent)
        .bind(hash)
        .bind(by)
        .execute(pool)
        .await
        .expect("seed strategy_version");
    }

    // One run per version, so "the accepted run belongs to the accepted child"
    // is a constraint under test rather than an accident of the fixture.
    for (run, version) in [
        ("run-1", "ver-1"),
        ("run-2", "ver-1"),
        ("run-child-2", "ver-2"),
        ("run-child-3", "ver-3"),
        ("run-root", "ver-root"),
        ("run-sibling", "ver-sibling"),
        ("run-foreign", "ver-foreign"),
    ] {
        seed_run(pool, run, version).await;
    }

    sqlx::query(
        "INSERT INTO llm_call \
         (id, backend, model, prompt_messages, completion, input_tokens, output_tokens, cost, \
          cost_currency, created_at, created_by, schema_version) \
         VALUES ('call-1', 'ollama', 'glm-5.3-flash', '[]', NULL, 1, 1, '0', 'CNY', \
                 '2026-08-29T00:00:00.000Z', 'coach_llm', 1)",
    )
    .execute(pool)
    .await
    .expect("seed llm_call");
}

/// One `backtest_run` row owned by `version`. `0006`'s completeness trigger wants
/// the input provenance present, so the tuple is a plain complete one.
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

#[allow(clippy::too_many_arguments)]
async fn insert_session(
    pool: &SqlitePool,
    id: &str,
    outcome: &str,
    llm_call: Option<&str>,
    failure_kind: Option<&str>,
    failure_detail: Option<&str>,
    fingerprint: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO coaching_sessions \
         (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
          failure_kind, failure_detail, schema_version, request_fingerprint) \
         VALUES (?1, 'run-1', 'ver-1', '2026-08-29T00:00:00.000Z', ?2, ?3, ?4, ?5, 1, ?6)",
    )
    .bind(id)
    .bind(llm_call)
    .bind(outcome)
    .bind(failure_kind)
    .bind(failure_detail)
    .bind(fingerprint)
    .execute(pool)
    .await
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
async fn insert_proposal(
    pool: &SqlitePool,
    id: &str,
    session_id: &str,
    disposition: &str,
    child: Option<&str>,
    run: Option<&str>,
    stage: Option<&str>,
    detail: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO coaching_proposals \
         (id, session_id, mutation, hypothesis, disposition, child_version_id, \
          accepted_run_id, accept_failure_stage, accept_failure_detail) \
         VALUES (?1, ?2, '{\"type\":\"set_param\"}', 'a slower RSI', ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(id)
    .bind(session_id)
    .bind(disposition)
    .bind(child)
    .bind(run)
    .bind(stage)
    .bind(detail)
    .execute(pool)
    .await
    .map(|_| ())
}

/// A `proposed` session plus its open proposal — the state every disposition test
/// starts from.
async fn seed_open_proposal(pool: &SqlitePool, session: &str, proposal: &str) {
    insert_session(pool, session, "proposed", Some("call-1"), None, None, None)
        .await
        .expect("seed proposed session");
    insert_proposal(pool, proposal, session, "proposed", None, None, None, None)
        .await
        .expect("seed open proposal");
}

// ===========================================================================
// 1. The migration itself — every `0005` row survives, logically unchanged
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0008_preserves_every_0005_row() {
    let (_tmp, _path, db) = db_at_0007().await;
    let pool = db.pool();
    seed_parents(pool).await;

    // Three `0005` rows across the whole vocabulary it could write: a proposal
    // turn, a failed turn carrying both failure fields, and a proposal that was
    // rejected. (`0005` could also store an accepted proposal; that shape is the
    // migration pre-flight's subject and has its own test below.)
    insert_session_0005(
        pool,
        "sess-proposed",
        "proposed",
        Some("call-1"),
        None,
        None,
    )
    .await;
    insert_session_0005(
        pool,
        "sess-failed",
        "failed",
        None,
        Some("context_overflow"),
        Some(r#"{"type":"context_overflow","detail":"too big"}"#),
    )
    .await;
    insert_session_0005(
        pool,
        "sess-rejected",
        "proposed",
        Some("call-1"),
        None,
        None,
    )
    .await;
    insert_proposal_0005(pool, "prop-1", "sess-proposed", "proposed", None).await;
    insert_proposal_0005(pool, "prop-2", "sess-rejected", "rejected", None).await;

    let before = read_sessions(pool).await;
    let proposals_before = read_proposals(pool).await;

    MIGRATOR.run(pool).await.expect("0008 applies over 0007");

    assert!(
        applied_versions(pool).await.contains(&8),
        "0008 must be applied"
    );
    assert_eq!(
        read_sessions(pool).await,
        before,
        "every 0005 session column survives the rebuild unchanged"
    );
    assert_eq!(
        read_proposals(pool).await,
        proposals_before,
        "every 0005 proposal column survives the rebuild unchanged"
    );

    // The index is recreated on the REBUILT table, not left pointing at the old one.
    assert!(
        object_present(pool, "index", "idx_coaching_sessions_run").await,
        "the run index must exist after the rebuild"
    );
    let indexed_table: String = sqlx::query_scalar(
        "SELECT tbl_name FROM sqlite_master WHERE type='index' AND name='idx_coaching_sessions_run'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        indexed_table, "coaching_sessions",
        "the run index must index the rebuilt table"
    );

    // No scaffolding survives the migration.
    for leftover in [
        "coaching_sessions_0005",
        "coaching_proposals_0005",
        "_0008_preflight",
    ] {
        assert!(
            !object_present(pool, "table", leftover).await,
            "{leftover} must not outlive the migration"
        );
    }

    // A legacy terminal row keeps a NULL fingerprint and stays readable.
    let fp: Option<String> = sqlx::query_scalar(
        "SELECT request_fingerprint FROM coaching_sessions WHERE id = 'sess-proposed'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert!(
        fp.is_none(),
        "a copied 0005 row records no request fingerprint, got {fp:?}"
    );
}

/// A `0005`-shaped session insert (no `request_fingerprint` column yet).
async fn insert_session_0005(
    pool: &SqlitePool,
    id: &str,
    outcome: &str,
    llm_call: Option<&str>,
    failure_kind: Option<&str>,
    failure_detail: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO coaching_sessions \
         (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
          failure_kind, failure_detail, schema_version) \
         VALUES (?1, 'run-1', 'ver-1', '2026-08-29T00:00:00.000Z', ?2, ?3, ?4, ?5, 1)",
    )
    .bind(id)
    .bind(llm_call)
    .bind(outcome)
    .bind(failure_kind)
    .bind(failure_detail)
    .execute(pool)
    .await
    .expect("seed a 0005 session");
}

/// A `0005`-shaped proposal insert (no run / accept-failure columns yet).
async fn insert_proposal_0005(
    pool: &SqlitePool,
    id: &str,
    session_id: &str,
    disposition: &str,
    child: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO coaching_proposals \
         (id, session_id, mutation, hypothesis, disposition, child_version_id) \
         VALUES (?1, ?2, '{\"type\":\"set_param\"}', 'a slower RSI', ?3, ?4)",
    )
    .bind(id)
    .bind(session_id)
    .bind(disposition)
    .bind(child)
    .execute(pool)
    .await
    .expect("seed a 0005 proposal");
}

type SessionRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    i64,
);

async fn read_sessions(pool: &SqlitePool) -> Vec<SessionRow> {
    sqlx::query_as(
        "SELECT id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
         failure_kind, failure_detail, schema_version FROM coaching_sessions ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

type ProposalRow = (String, String, String, String, String, Option<String>);

async fn read_proposals(pool: &SqlitePool) -> Vec<ProposalRow> {
    sqlx::query_as(
        "SELECT id, session_id, mutation, hypothesis, disposition, child_version_id \
         FROM coaching_proposals ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0008_adds_the_lifecycle_columns() {
    let (_tmp, db) = db_at_0008().await;

    assert!(
        columns_of(db.pool(), "coaching_sessions")
            .await
            .contains(&"request_fingerprint".to_owned()),
        "coaching_sessions must carry request_fingerprint"
    );
    for column in [
        "accepted_run_id",
        "accept_failure_stage",
        "accept_failure_detail",
    ] {
        assert!(
            columns_of(db.pool(), "coaching_proposals")
                .await
                .contains(&column.to_owned()),
            "coaching_proposals must carry {column}"
        );
    }

    // Audit C4 still holds over the NEW columns: a mutation's validity is
    // established by `apply()` at use time and is never a stored property.
    for table in ["coaching_sessions", "coaching_proposals"] {
        for column in columns_of(db.pool(), table).await {
            assert!(
                !column.to_lowercase().contains("valid"),
                "{table}.{column} looks like persisted validity, which audit C4 forbids"
            );
        }
    }
}

/// The dormant `0005` shape `0008` refuses to guess at: an accepted proposal that
/// names a child and cannot name a run, because the column did not exist. `r1.s2`
/// never wrote one; the migration VERIFIES that rather than trusting it, and fails
/// rather than inventing a run link.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0008_refuses_a_dormant_accepted_row_rather_than_inventing_a_run() {
    let (_tmp, _path, db) = db_at_0007().await;
    let pool = db.pool();
    seed_parents(pool).await;

    insert_session_0005(
        pool,
        "sess-accepted",
        "proposed",
        Some("call-1"),
        None,
        None,
    )
    .await;
    insert_proposal_0005(pool, "prop-1", "sess-accepted", "accepted", Some("ver-2")).await;

    let err = MIGRATOR
        .run(pool)
        .await
        .expect_err("0008 must refuse a dormant accepted row");
    let text = err.to_string();
    assert!(
        text.contains("0008"),
        "the refusal must name the migration, got: {text}"
    );

    // The pre-flight refused BEFORE tightening anything: the 0005 row is untouched
    // and the new columns never appeared.
    assert!(
        !columns_of(pool, "coaching_proposals")
            .await
            .contains(&"accepted_run_id".to_owned()),
        "a refused migration leaves the old shape in place"
    );
    let child: Option<String> =
        sqlx::query_scalar("SELECT child_version_id FROM coaching_proposals WHERE id = 'prop-1'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        child.as_deref(),
        Some("ver-2"),
        "the dormant row is left exactly as it was"
    );
}

// ===========================================================================
// 2. `coaching_sessions` — the claim, the vocabulary, the transition
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_session_must_carry_a_non_empty_request_fingerprint() {
    let (_tmp, db) = db_at_0008().await;
    let pool = db.pool();

    insert_session(pool, "ok", "pending", None, None, None, Some(FINGERPRINT_A))
        .await
        .expect("a pending claim with a fingerprint is the normal shape");

    for (label, fingerprint) in [
        ("absent", None),
        ("empty", Some("")),
        ("blank", Some("  \t")),
    ] {
        assert!(
            insert_session(
                pool,
                &format!("bad-{label}"),
                "pending",
                None,
                None,
                None,
                fingerprint
            )
            .await
            .is_err(),
            "a pending claim with an {label} request fingerprint must be refused"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_session_carries_no_call_no_failure_and_no_proposal() {
    let (_tmp, db) = db_at_0008().await;
    let pool = db.pool();

    assert!(
        insert_session(
            pool,
            "with-call",
            "pending",
            Some("call-1"),
            None,
            None,
            Some(FINGERPRINT_A)
        )
        .await
        .is_err(),
        "a pending claim precedes the provider call, so it names no ledger row"
    );
    assert!(
        insert_session(
            pool,
            "with-failure",
            "pending",
            None,
            Some("zero_calls"),
            Some(r#"{"type":"zero_calls"}"#),
            Some(FINGERPRINT_A)
        )
        .await
        .is_err(),
        "a pending claim has no outcome yet, so it carries no failure"
    );

    insert_session(
        pool,
        "sess-pending",
        "pending",
        None,
        None,
        None,
        Some(FINGERPRINT_A),
    )
    .await
    .expect("seed a pending claim");
    assert!(
        insert_proposal(
            pool,
            "prop-1",
            "sess-pending",
            "proposed",
            None,
            None,
            None,
            None
        )
        .await
        .is_err(),
        "a proposal may be attached only to a session that reached `proposed`"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_failure_fields_are_all_or_nothing_across_the_widened_vocabulary() {
    let (_tmp, db) = db_at_0008().await;
    let pool = db.pool();

    // The three tags `0008` adds are storable...
    for kind in [
        "inapplicable_advice",
        "missing_backtest_inputs",
        "interrupted",
    ] {
        insert_session(
            pool,
            kind,
            "failed",
            None,
            Some(kind),
            Some(&format!(r#"{{"type":"{kind}"}}"#)),
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("`{kind}` must be a storable failure tag, got {e}"));
    }
    // ...and the enumeration is still closed.
    assert!(
        insert_session(
            pool,
            "invented",
            "failed",
            None,
            Some("the_model_was_rude"),
            Some("{}"),
            None
        )
        .await
        .is_err(),
        "the failure taxonomy is enumerated in-schema; a typo cannot become a state"
    );

    // Half a failure is not a failure.
    assert!(
        insert_session(
            pool,
            "half-1",
            "failed",
            None,
            Some("zero_calls"),
            None,
            None
        )
        .await
        .is_err(),
        "a failed turn carries BOTH its kind and its detail"
    );
    assert!(
        insert_session(pool, "half-2", "failed", None, None, Some("{}"), None)
            .await
            .is_err(),
        "a failed turn carries BOTH its kind and its detail"
    );
    // And a proposed turn carries neither.
    assert!(
        insert_session(
            pool,
            "proposed-with-failure",
            "proposed",
            Some("call-1"),
            Some("zero_calls"),
            Some("{}"),
            None
        )
        .await
        .is_err(),
        "a proposed turn carries no failure field"
    );
    assert!(
        insert_session(
            pool,
            "invented-outcome",
            "abandoned",
            None,
            None,
            None,
            None
        )
        .await
        .is_err(),
        "the outcome vocabulary is enumerated in-schema"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_session_settles_once_and_then_is_immutable() {
    let (_tmp, db) = db_at_0008().await;
    let pool = db.pool();
    insert_session(
        pool,
        "sess-1",
        "pending",
        None,
        None,
        None,
        Some(FINGERPRINT_A),
    )
    .await
    .expect("claim");

    // Pending → proposed is the one move, and it may attach the ledger row.
    sqlx::query(
        "UPDATE coaching_sessions SET outcome='proposed', llm_call_id='call-1' WHERE id='sess-1'",
    )
    .execute(pool)
    .await
    .expect("a pending claim settles to proposed");

    // A settled row cannot change outcome, identity or fingerprint.
    for (label, sql) in [
        (
            "outcome",
            "UPDATE coaching_sessions SET outcome='failed', failure_kind='zero_calls', \
             failure_detail='{}' WHERE id='sess-1'",
        ),
        (
            "run identity",
            "UPDATE coaching_sessions SET backtest_run_id='run-2' WHERE id='sess-1'",
        ),
        (
            "version identity",
            "UPDATE coaching_sessions SET strategy_version_id='ver-2' WHERE id='sess-1'",
        ),
        (
            "created_at",
            "UPDATE coaching_sessions SET created_at='2030-01-01T00:00:00.000Z' WHERE id='sess-1'",
        ),
        (
            "request fingerprint",
            "UPDATE coaching_sessions SET request_fingerprint=NULL WHERE id='sess-1'",
        ),
    ] {
        assert!(
            sqlx::query(sql).execute(pool).await.is_err(),
            "a settled session's {label} is immutable"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_session_cannot_settle_to_pending_and_a_proposal_blocks_a_failure() {
    let (_tmp, db) = db_at_0008().await;
    let pool = db.pool();

    insert_session(
        pool,
        "sess-1",
        "pending",
        None,
        None,
        None,
        Some(FINGERPRINT_A),
    )
    .await
    .expect("claim");
    assert!(
        sqlx::query("UPDATE coaching_sessions SET outcome='pending' WHERE id='sess-1'")
            .execute(pool)
            .await
            .is_err(),
        "a claim settles to proposed or failed, never back to pending"
    );

    seed_open_proposal(pool, "sess-2", "prop-2").await;
    assert!(
        sqlx::query(
            "UPDATE coaching_sessions SET outcome='failed', failure_kind='zero_calls', \
             failure_detail='{}' WHERE id='sess-2'"
        )
        .execute(pool)
        .await
        .is_err(),
        "a session carrying a proposal cannot be recorded as failed"
    );
}

// ===========================================================================
// 3. `coaching_proposals` — accepted child + run, typed accept failure, matrix
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_proposal_names_both_its_child_and_its_run() {
    let (_tmp, db) = db_at_0008().await;
    let pool = db.pool();
    insert_session(pool, "s1", "proposed", Some("call-1"), None, None, None)
        .await
        .expect("session");

    // Both present is the only accepted shape.
    insert_proposal(
        pool,
        "p-ok",
        "s1",
        "accepted",
        Some("ver-2"),
        Some("run-child-2"),
        None,
        None,
    )
    .await
    .expect("an accepted proposal with its child and run");

    for (label, child, run) in [
        ("no run", Some("ver-2"), None),
        ("no child", None, Some("run-child-2")),
        ("neither", None, None),
    ] {
        insert_session(
            pool,
            &format!("s-{label}"),
            "proposed",
            Some("call-1"),
            None,
            None,
            None,
        )
        .await
        .expect("session");
        assert!(
            insert_proposal(
                pool,
                &format!("p-{label}"),
                &format!("s-{label}"),
                "accepted",
                child,
                run,
                None,
                None
            )
            .await
            .is_err(),
            "an accepted proposal with {label} must be refused"
        );
    }

    // And nothing else may name either.
    for disposition in ["proposed", "rejected", "modified"] {
        insert_session(
            pool,
            &format!("s-{disposition}"),
            "proposed",
            Some("call-1"),
            None,
            None,
            None,
        )
        .await
        .expect("session");
        assert!(
            insert_proposal(
                pool,
                &format!("p-{disposition}"),
                &format!("s-{disposition}"),
                disposition,
                Some("ver-2"),
                Some("run-child-2"),
                None,
                None
            )
            .await
            .is_err(),
            "a `{disposition}` proposal may not name a child version or a run"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_run_must_belong_to_the_accepted_child_version() {
    let (_tmp, db) = db_at_0008().await;
    let pool = db.pool();
    insert_session(pool, "s1", "proposed", Some("call-1"), None, None, None)
        .await
        .expect("session");

    // `run-1` is a real run and belongs to `ver-1`, not to the child `ver-2`.
    assert!(
        insert_proposal(
            pool,
            "p1",
            "s1",
            "accepted",
            Some("ver-2"),
            Some("run-1"),
            None,
            None
        )
        .await
        .is_err(),
        "the accepted run must be the re-backtest OF the accepted child"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_child_must_descend_from_the_coached_version_in_the_same_strategy() {
    let (_tmp, db) = db_at_0008().await;
    let pool = db.pool();

    for (label, child, run) in [
        ("root", "ver-root", "run-root"),
        ("parented elsewhere", "ver-sibling", "run-sibling"),
        ("another strategy", "ver-foreign", "run-foreign"),
    ] {
        let session = format!("s-{label}");
        insert_session(pool, &session, "proposed", Some("call-1"), None, None, None)
            .await
            .expect("session");
        assert!(
            insert_proposal(
                pool,
                &format!("p-{label}"),
                &session,
                "accepted",
                Some(child),
                Some(run),
                None,
                None
            )
            .await
            .is_err(),
            "a child that is {label} records a lineage that never happened"
        );
    }

    // The honest shape still lands.
    insert_session(pool, "s-ok", "proposed", Some("call-1"), None, None, None)
        .await
        .expect("session");
    insert_proposal(
        pool,
        "p-ok",
        "s-ok",
        "accepted",
        Some("ver-2"),
        Some("run-child-2"),
        None,
        None,
    )
    .await
    .expect("a direct child of the coached version, with its own run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accept_failure_fields_are_paired_enumerated_and_open_state_only() {
    let (_tmp, db) = db_at_0008().await;
    let pool = db.pool();

    for disposition in ["proposed", "modified"] {
        let session = format!("s-{disposition}");
        insert_session(pool, &session, "proposed", Some("call-1"), None, None, None)
            .await
            .expect("session");
        insert_proposal(
            pool,
            &format!("p-{disposition}"),
            &session,
            disposition,
            None,
            None,
            Some("backtest"),
            Some(r#"{"stage":"backtest","message":"no candles"}"#),
        )
        .await
        .unwrap_or_else(|e| panic!("a `{disposition}` proposal may record an accept failure: {e}"));
    }

    insert_session(pool, "s-half", "proposed", Some("call-1"), None, None, None)
        .await
        .expect("session");
    for (label, stage, detail) in [
        ("stage without detail", Some("apply"), None),
        ("detail without stage", None, Some("{}")),
    ] {
        assert!(
            insert_proposal(
                pool,
                &format!("p-{label}"),
                "s-half",
                "proposed",
                None,
                None,
                stage,
                detail
            )
            .await
            .is_err(),
            "an accept failure with {label} is half a record"
        );
    }
    assert!(
        insert_proposal(
            pool,
            "p-read-back",
            "s-half",
            "proposed",
            None,
            None,
            Some("read_back"),
            Some("{}")
        )
        .await
        .is_err(),
        "`read_back` is not an accept-failure stage: once child and run are \
         committed the accept SUCCEEDED"
    );

    // Neither terminal state may carry one.
    insert_session(pool, "s-acc", "proposed", Some("call-1"), None, None, None)
        .await
        .expect("session");
    assert!(
        insert_proposal(
            pool,
            "p-acc",
            "s-acc",
            "accepted",
            Some("ver-2"),
            Some("run-child-2"),
            Some("persist"),
            Some("{}")
        )
        .await
        .is_err(),
        "an accepted proposal cannot also carry an accept failure"
    );
    insert_session(pool, "s-rej", "proposed", Some("call-1"), None, None, None)
        .await
        .expect("session");
    assert!(
        insert_proposal(
            pool,
            "p-rej",
            "s-rej",
            "rejected",
            None,
            None,
            Some("apply"),
            Some("{}")
        )
        .await
        .is_err(),
        "a rejected proposal carries no child, no run and no accept failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_disposition_transition_matrix_is_pinned_by_a_trigger() {
    let (_tmp, db) = db_at_0008().await;
    let pool = db.pool();

    // proposed → modified → accepted is the working path.
    seed_open_proposal(pool, "s1", "p1").await;
    sqlx::query("UPDATE coaching_proposals SET disposition='modified' WHERE id='p1'")
        .execute(pool)
        .await
        .expect("proposed → modified");
    sqlx::query("UPDATE coaching_proposals SET disposition='modified' WHERE id='p1'")
        .execute(pool)
        .await
        .expect("modified → modified is a re-edit, not a transition");
    sqlx::query(
        "UPDATE coaching_proposals SET disposition='accepted', child_version_id='ver-2', \
         accepted_run_id='run-child-2' WHERE id='p1'",
    )
    .execute(pool)
    .await
    .expect("modified → accepted");

    // Accepted is terminal: the IDENTICAL rewrite is the idempotent no-op...
    sqlx::query(
        "UPDATE coaching_proposals SET disposition='accepted', child_version_id='ver-2', \
         accepted_run_id='run-child-2' WHERE id='p1'",
    )
    .execute(pool)
    .await
    .expect("replaying the identical accept is a no-op");
    // ...and everything else is refused, including a BACKWARD move that a
    // column-presence check alone would admit.
    for (label, sql) in [
        (
            "a second child",
            "UPDATE coaching_proposals SET child_version_id='ver-3', \
             accepted_run_id='run-child-3' WHERE id='p1'",
        ),
        (
            "a re-rejection",
            "UPDATE coaching_proposals SET disposition='rejected', child_version_id=NULL, \
             accepted_run_id=NULL WHERE id='p1'",
        ),
        (
            "a return to modified",
            "UPDATE coaching_proposals SET disposition='modified', child_version_id=NULL, \
             accepted_run_id=NULL WHERE id='p1'",
        ),
    ] {
        assert!(
            sqlx::query(sql).execute(pool).await.is_err(),
            "an accepted proposal must refuse {label}"
        );
    }

    // Rejected is terminal the same way, and nothing returns to `proposed`.
    seed_open_proposal(pool, "s2", "p2").await;
    sqlx::query("UPDATE coaching_proposals SET disposition='rejected' WHERE id='p2'")
        .execute(pool)
        .await
        .expect("proposed → rejected");
    sqlx::query("UPDATE coaching_proposals SET disposition='rejected' WHERE id='p2'")
        .execute(pool)
        .await
        .expect("the identical rejection replays");
    assert!(
        sqlx::query("UPDATE coaching_proposals SET disposition='modified' WHERE id='p2'")
            .execute(pool)
            .await
            .is_err(),
        "a rejected proposal is settled"
    );

    seed_open_proposal(pool, "s3", "p3").await;
    sqlx::query("UPDATE coaching_proposals SET disposition='modified' WHERE id='p3'")
        .execute(pool)
        .await
        .expect("proposed → modified");
    assert!(
        sqlx::query("UPDATE coaching_proposals SET disposition='proposed' WHERE id='p3'")
            .execute(pool)
            .await
            .is_err(),
        "nothing returns to `proposed`"
    );
}

/// `pulseai-labs/pulse-trader#149` — the `accepted_run_id` CHECK, bound.
///
/// The lineage triggers fire only `WHEN NEW.disposition = 'accepted'`, so a run
/// link smuggled onto a still-OPEN proposal passes every one of them. What refuses
/// it is `CHECK ((disposition = 'accepted') = (accepted_run_id IS NOT NULL))`, and
/// until now nothing exercised that half from the open side. An open proposal
/// carrying a run link is a proposal that says a re-backtest happened for a decision
/// nobody made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_proposal_cannot_be_given_an_accepted_run_id() {
    let (_tmp, db) = db_at_0008().await;
    let pool = db.pool();

    for (label, state) in [("proposed", "proposed"), ("modified", "modified")] {
        let session = format!("s-open-{state}");
        let proposal = format!("p-open-{state}");
        seed_open_proposal(pool, &session, &proposal).await;
        if state == "modified" {
            sqlx::query("UPDATE coaching_proposals SET disposition='modified' WHERE id=?1")
                .bind(&proposal)
                .execute(pool)
                .await
                .expect("proposed → modified");
        }

        // `run-child-2` is a real run of a real child of the coached version, so
        // every FK and every lineage trigger would be satisfied if the disposition
        // said `accepted`. The only thing standing between this row and a false
        // "this proposal has a re-backtest" claim is the CHECK.
        assert!(
            sqlx::query("UPDATE coaching_proposals SET accepted_run_id='run-child-2' WHERE id=?1")
                .bind(&proposal)
                .execute(pool)
                .await
                .is_err(),
            "a `{label}` proposal must not name an accepted run"
        );

        // And the refusal wrote NOTHING. A statement that errors has still run, so
        // "the UPDATE returned an error" and "the row is unchanged" are two separate
        // facts; reading the link back is what makes the second one checkable rather
        // than assumed.
        let read_back: Option<String> =
            sqlx::query_scalar("SELECT accepted_run_id FROM coaching_proposals WHERE id=?1")
                .bind(&proposal)
                .fetch_one(pool)
                .await
                .expect("read the proposal back");
        assert!(
            read_back.is_none(),
            "the refused UPDATE left no run link on the `{label}` proposal"
        );
    }
}

/// `pulseai-labs/pulse-trader#149` — ALL EIGHT illegal moves, enumerated.
///
/// The state machine has four states. Twelve of the sixteen ordered pairs are moves
/// (the other four are identity rewrites), five of those twelve are legal, and the
/// remaining seven are refused — plus the eighth illegal move an identity rewrite
/// can still express: re-accepting with a DIFFERENT child and run. Enumerating them
/// in one table is what makes "the matrix is pinned" a checkable statement rather
/// than a sample.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_eight_illegal_disposition_transitions_are_refused() {
    let (_tmp, db) = db_at_0008().await;
    let pool = db.pool();

    // (label, starting state, the illegal UPDATE's SET clause)
    let illegal: [(&str, &str, &str); 8] = [
        (
            "modified → proposed",
            "modified",
            "disposition='proposed', child_version_id=NULL, accepted_run_id=NULL",
        ),
        (
            "accepted → proposed",
            "accepted",
            "disposition='proposed', child_version_id=NULL, accepted_run_id=NULL",
        ),
        (
            "accepted → modified",
            "accepted",
            "disposition='modified', child_version_id=NULL, accepted_run_id=NULL",
        ),
        (
            "accepted → rejected",
            "accepted",
            "disposition='rejected', child_version_id=NULL, accepted_run_id=NULL",
        ),
        (
            "accepted → a SECOND child",
            "accepted",
            "child_version_id='ver-3', accepted_run_id='run-child-3'",
        ),
        (
            "rejected → proposed",
            "rejected",
            "disposition='proposed', child_version_id=NULL, accepted_run_id=NULL",
        ),
        (
            "rejected → modified",
            "rejected",
            "disposition='modified', child_version_id=NULL, accepted_run_id=NULL",
        ),
        (
            "rejected → accepted",
            "rejected",
            "disposition='accepted', child_version_id='ver-2', accepted_run_id='run-child-2'",
        ),
    ];

    for (index, (label, from, set_clause)) in illegal.iter().enumerate() {
        let session = format!("s-matrix-{index}");
        let proposal = format!("p-matrix-{index}");
        seed_in_state(pool, &session, &proposal, from).await;

        let outcome = sqlx::query(&format!(
            "UPDATE coaching_proposals SET {set_clause} WHERE id='{proposal}'"
        ))
        .execute(pool)
        .await;
        assert!(outcome.is_err(), "`{label}` must be refused");

        // The refused write changed nothing.
        let still: String =
            sqlx::query_scalar("SELECT disposition FROM coaching_proposals WHERE id=?1")
                .bind(&proposal)
                .fetch_one(pool)
                .await
                .expect("read the proposal back");
        assert_eq!(&still, from, "`{label}` left the row where it was");
    }

    // The five LEGAL moves, so the table above is a statement about the matrix
    // rather than a claim that every UPDATE fails.
    for (index, (from, set_clause)) in [
        ("proposed", "disposition='modified'"),
        ("proposed", "disposition='rejected'"),
        (
            "proposed",
            "disposition='accepted', child_version_id='ver-2', accepted_run_id='run-child-2'",
        ),
        ("modified", "disposition='rejected'"),
        (
            "modified",
            "disposition='accepted', child_version_id='ver-2', accepted_run_id='run-child-2'",
        ),
    ]
    .iter()
    .enumerate()
    {
        let session = format!("s-legal-{index}");
        let proposal = format!("p-legal-{index}");
        seed_in_state(pool, &session, &proposal, from).await;
        sqlx::query(&format!(
            "UPDATE coaching_proposals SET {set_clause} WHERE id='{proposal}'"
        ))
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("`{from}` → `{set_clause}` must be legal, got {e}"));
    }
}

/// Seed one proposal already in `state`, reaching it only through LEGAL moves.
async fn seed_in_state(pool: &SqlitePool, session: &str, proposal: &str, state: &str) {
    seed_open_proposal(pool, session, proposal).await;
    match state {
        "proposed" => {}
        "modified" => {
            sqlx::query("UPDATE coaching_proposals SET disposition='modified' WHERE id=?1")
                .bind(proposal)
                .execute(pool)
                .await
                .expect("proposed → modified");
        }
        "rejected" => {
            sqlx::query("UPDATE coaching_proposals SET disposition='rejected' WHERE id=?1")
                .bind(proposal)
                .execute(pool)
                .await
                .expect("proposed → rejected");
        }
        "accepted" => {
            sqlx::query(
                "UPDATE coaching_proposals SET disposition='accepted', \
                 child_version_id='ver-2', accepted_run_id='run-child-2' WHERE id=?1",
            )
            .bind(proposal)
            .execute(pool)
            .await
            .expect("proposed → accepted");
        }
        other => panic!("no such disposition `{other}`"),
    }
}

// ===========================================================================
// 4. Down — exact `0005` shape, and a transactional refusal when it would lie
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn representable_data_survives_down_and_up_again() {
    let (_tmp, db) = db_at_0008().await;
    let pool = db.pool();

    insert_session(pool, "sess-1", "proposed", Some("call-1"), None, None, None)
        .await
        .expect("a 0005-representable proposal turn");
    insert_session(
        pool,
        "sess-2",
        "failed",
        None,
        Some("context_overflow"),
        Some(r#"{"type":"context_overflow","detail":"too big"}"#),
        None,
    )
    .await
    .expect("a 0005-representable failure");
    insert_proposal(pool, "prop-1", "sess-1", "proposed", None, None, None, None)
        .await
        .expect("an open proposal");

    let sessions = read_sessions(pool).await;
    let proposals = read_proposals(pool).await;

    undo_to(pool, 7).await.expect("0008 down over 0005 data");
    assert!(
        !columns_of(pool, "coaching_sessions")
            .await
            .contains(&"request_fingerprint".to_owned()),
        "the down migration reconstructs the exact 0005 shape"
    );
    assert!(
        !columns_of(pool, "coaching_proposals")
            .await
            .contains(&"accepted_run_id".to_owned()),
        "the down migration reconstructs the exact 0005 shape"
    );
    assert_eq!(read_sessions(pool).await, sessions, "no row is dropped");
    assert_eq!(read_proposals(pool).await, proposals, "no row is dropped");
    assert!(
        object_present(pool, "index", "idx_coaching_sessions_run").await,
        "0005's index comes back with 0005's shape"
    );

    MIGRATOR.run(pool).await.expect("0008 re-applies");
    assert_eq!(read_sessions(pool).await, sessions, "and back again");
    assert_eq!(read_proposals(pool).await, proposals, "and back again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lossy_downgrade_is_refused_transactionally() {
    // Each case is a state `0005` cannot represent. The down migration must refuse
    // the WHOLE downgrade rather than coerce the state into an old tag or drop the
    // row that carries it.
    for (label, seed) in [
        ("a pending claim", LossySeed::Pending),
        ("a new failure tag", LossySeed::NewFailureTag),
        ("an accepted run link", LossySeed::AcceptedRun),
        ("an accept failure", LossySeed::AcceptFailure),
    ] {
        let (_tmp, db) = db_at_0008().await;
        let pool = db.pool();
        seed.apply(pool).await;
        let sessions_before = read_sessions(pool).await;
        let proposals_before = read_proposals(pool).await;

        assert!(
            undo_to(pool, 7).await.is_err(),
            "{label} cannot be represented under 0005, so the downgrade must refuse"
        );

        // Transactional: nothing was dropped and nothing was coerced into an old
        // tag on the way to the refusal.
        assert!(
            columns_of(pool, "coaching_sessions")
                .await
                .contains(&"request_fingerprint".to_owned()),
            "{label}: a refused downgrade leaves the 0008 shape in place"
        );
        assert_eq!(
            read_sessions(pool).await,
            sessions_before,
            "{label}: a refused downgrade drops no session"
        );
        assert_eq!(
            read_proposals(pool).await,
            proposals_before,
            "{label}: a refused downgrade drops no proposal"
        );
        assert!(
            applied_versions(pool).await.contains(&8),
            "{label}: 0008 is still applied after the refusal"
        );
    }
}

enum LossySeed {
    Pending,
    NewFailureTag,
    AcceptedRun,
    AcceptFailure,
}

impl LossySeed {
    async fn apply(&self, pool: &SqlitePool) {
        match self {
            Self::Pending => {
                insert_session(
                    pool,
                    "sess-pending",
                    "pending",
                    None,
                    None,
                    None,
                    Some(FINGERPRINT_A),
                )
                .await
                .expect("seed");
            }
            Self::NewFailureTag => {
                insert_session(
                    pool,
                    "sess-advice",
                    "failed",
                    None,
                    Some("inapplicable_advice"),
                    Some(
                        r#"{"type":"inapplicable_advice","intent":"add an ADX filter","evidence":"ranging losses"}"#,
                    ),
                    None,
                )
                .await
                .expect("seed");
            }
            Self::AcceptedRun => {
                insert_session(pool, "s1", "proposed", Some("call-1"), None, None, None)
                    .await
                    .expect("seed");
                insert_proposal(
                    pool,
                    "p1",
                    "s1",
                    "accepted",
                    Some("ver-2"),
                    Some("run-child-2"),
                    None,
                    None,
                )
                .await
                .expect("seed");
            }
            Self::AcceptFailure => {
                insert_session(pool, "s1", "proposed", Some("call-1"), None, None, None)
                    .await
                    .expect("seed");
                insert_proposal(
                    pool,
                    "p1",
                    "s1",
                    "proposed",
                    None,
                    None,
                    Some("compile"),
                    Some(r#"{"stage":"compile","message":"no"}"#),
                )
                .await
                .expect("seed");
            }
        }
    }
}

// ===========================================================================
// 5. The repository lifecycle, through the real adapters
// ===========================================================================

fn fingerprint(hex: &str) -> CoachRequestFingerprint {
    CoachRequestFingerprint::new(hex).expect("a non-empty digest")
}

fn claim(session: &str, run: &str, version: &str, fp: &str) -> CoachSessionClaim {
    CoachSessionClaim {
        session_id: CoachingSessionId::new(session),
        backtest_run_id: BacktestRunId::new(run),
        strategy_version_id: VersionId::new(version),
        request_fingerprint: fingerprint(fp),
        created_at: now_rfc3339(),
    }
}

fn a_proposal() -> Proposal {
    Proposal {
        mutation: Mutation::SetParam {
            path: "entry.lhs.spec.period".to_owned(),
            new_value: ParamValue::Period { value: 21 },
        },
        hypothesis: Hypothesis::new("a slower RSI trades less and holds longer")
            .expect("non-empty"),
        disposition: Disposition::Proposed,
        accept_failure: None,
    }
}

async fn coaching_repo() -> (SqliteCoachingRepo<FakeClock>, SqlitePool, TempDir) {
    let (tmp, db) = db_at_0008().await;
    let pool = db.pool().clone();
    (
        SqliteCoachingRepo::with_deps(pool.clone(), FakeClock::at(NOW_MS)),
        pool,
        tmp,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_reserves_the_session_before_the_provider_call() {
    let (repo, pool, _tmp) = coaching_repo().await;
    let c = claim("sess-1", "run-1", "ver-1", FINGERPRINT_A);

    assert!(
        matches!(
            repo.claim_session(c.clone()).await.expect("claim"),
            CoachSessionClaimResult::Claimed
        ),
        "the first claim owns the one provider attempt"
    );

    // The row is COMMITTED before any network I/O — it is readable now.
    let outcome: String =
        sqlx::query_scalar("SELECT outcome FROM coaching_sessions WHERE id = 'sess-1'")
            .fetch_one(&pool)
            .await
            .expect("the claim committed");
    assert_eq!(outcome, "pending", "a claim lands as `pending`");

    // A repeat of the SAME claim is still pending; the repository returns the row
    // unchanged and does not judge whether the claim is live.
    match repo.claim_session(c.clone()).await.expect("re-claim") {
        CoachSessionClaimResult::ExistingPending(session) => {
            assert_eq!(session.id, CoachingSessionId::new("sess-1"));
            assert!(matches!(session.outcome, SessionOutcome::Pending));
            assert!(
                session.llm_call_id.is_none(),
                "a pending row names no ledger call"
            );
        }
        other => panic!("expected ExistingPending, got {other:?}"),
    }

    // Finalizing settles it once, and a later claim is the idempotent hit.
    let settled = repo
        .finish_session(
            &CoachingSessionId::new("sess-1"),
            InitialCoachOutcome {
                llm_call_id: Some(LlmCallId::new("call-1")),
                outcome: SessionOutcome::Proposed {
                    proposal: a_proposal(),
                },
            },
        )
        .await
        .expect("finish");
    assert!(matches!(settled.outcome, SessionOutcome::Proposed { .. }));

    match repo.claim_session(c).await.expect("claim after finish") {
        CoachSessionClaimResult::Existing(session) => match session.outcome {
            SessionOutcome::Proposed { .. } => {}
            other => panic!("expected the settled proposal back, got {other:?}"),
        },
        other => panic!("expected Existing, got {other:?}"),
    }

    // And it cannot settle twice.
    assert!(
        repo.finish_session(
            &CoachingSessionId::new("sess-1"),
            InitialCoachOutcome {
                llm_call_id: None,
                outcome: SessionOutcome::Failed {
                    failure: CoachFailure::ZeroCalls
                },
            }
        )
        .await
        .is_err(),
        "a settled session is terminal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reusing_a_session_id_with_different_provenance_is_an_error_not_an_idempotent_hit() {
    let (repo, _pool, _tmp) = coaching_repo().await;
    repo.claim_session(claim("sess-1", "run-1", "ver-1", FINGERPRINT_A))
        .await
        .expect("claim");

    for (label, other) in [
        (
            "a different run",
            claim("sess-1", "run-2", "ver-1", FINGERPRINT_A),
        ),
        (
            "a different version",
            claim("sess-1", "run-1", "ver-2", FINGERPRINT_A),
        ),
        (
            "a different fingerprint",
            claim("sess-1", "run-1", "ver-1", FINGERPRINT_B),
        ),
    ] {
        assert!(
            repo.claim_session(other).await.is_err(),
            "reusing the session id with {label} is an error, never an idempotent hit"
        );
    }
}

/// A claim left by an earlier process lifetime is finalized as `interrupted`
/// WITHOUT another provider call — the honest record of a turn nothing finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_claim_is_finalized_as_interrupted() {
    let (repo, _pool, _tmp) = coaching_repo().await;
    let id = CoachingSessionId::new("sess-1");
    repo.claim_session(claim("sess-1", "run-1", "ver-1", FINGERPRINT_A))
        .await
        .expect("claim");

    let settled = repo
        .finish_session(
            &id,
            InitialCoachOutcome {
                llm_call_id: None,
                outcome: SessionOutcome::Failed {
                    failure: CoachFailure::Interrupted {
                        detail: "claimed by a process that did not finish the turn".to_owned(),
                    },
                },
            },
        )
        .await
        .expect("finalize a stale claim");

    match settled.outcome {
        SessionOutcome::Failed {
            failure: CoachFailure::Interrupted { .. },
        } => {}
        other => panic!("expected a typed `interrupted` failure, got {other:?}"),
    }
    let back = repo.get_session(&id).await.expect("read").expect("present");
    assert_eq!(back.outcome, settled.outcome, "and it reads back typed");
}

/// `save_session` survives Round 1 as the initial-write path only. It never
/// writes an already-settled disposition, and W1 retires the production bypass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_session_accepts_initial_shapes_only() {
    let (repo, _pool, _tmp) = coaching_repo().await;

    let mut already_accepted = a_proposal();
    already_accepted.disposition = Disposition::Accepted {
        child_version_id: VersionId::new("ver-2"),
        accepted_run_id: BacktestRunId::new("run-child-2"),
    };
    assert!(
        repo.save_session(&CoachingSession {
            id: CoachingSessionId::new("sess-bad"),
            backtest_run_id: BacktestRunId::new("run-1"),
            strategy_version_id: VersionId::new("ver-1"),
            created_at: now_rfc3339(),
            llm_call_id: Some(LlmCallId::new("call-1")),
            outcome: SessionOutcome::Proposed {
                proposal: already_accepted,
            },
        })
        .await
        .is_err(),
        "save_session writes an INITIAL turn, never an already-settled proposal"
    );

    assert!(
        repo.save_session(&CoachingSession {
            id: CoachingSessionId::new("sess-pending"),
            backtest_run_id: BacktestRunId::new("run-1"),
            strategy_version_id: VersionId::new("ver-1"),
            created_at: now_rfc3339(),
            llm_call_id: None,
            outcome: SessionOutcome::Pending,
        })
        .await
        .is_err(),
        "a claim is made by claim_session, not by save_session"
    );
}

// ---------------------------------------------------------------------------
// 5b. Accept outcome persistence
// ---------------------------------------------------------------------------

fn acceptance_repo(pool: &SqlitePool) -> SqliteCoachAcceptanceRepo<FakeClock, SeqIdSource> {
    SqliteCoachAcceptanceRepo::with_deps(
        pool.clone(),
        FakeClock::at(NOW_MS),
        SeqIdSource::with_prefix("minted"),
    )
}

fn child_dsl() -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "RSI Oversold".to_owned(),
        direction: Direction::Long,
        entry: Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Rsi {
                    period: SweepableValue::Fixed(21),
                },
            },
            op: Comparator::Lt,
            rhs: ValueSource::Constant {
                value: Decimal::new(30, 0),
            },
        },
        filters: vec![],
        exits: vec![
            ExitRule::StopLoss {
                distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
            },
            ExitRule::TakeProfit {
                target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
            },
        ],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
            max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
        },
    }
}

fn prepared_backtest() -> PreparedBacktest {
    let trades = vec![Trade {
        direction: Direction::Long,
        qty: Decimal::new(5, 1),
        entry_price: Decimal::new(30_000, 0),
        exit_price: Decimal::new(33_000, 0),
        entry_signal_time: 1,
        entry_fill_time: 2,
        exit_signal_time: 3,
        exit_fill_time: 4,
        fills: vec![Fill {
            price: Decimal::new(30_000, 0),
            qty: Decimal::new(5, 1),
            time_ms: 2,
            fee: Decimal::new(6, 0),
        }],
        fees_total: Decimal::new(12, 0),
        funding_total: Decimal::new(1, 0),
        slippage_total: Decimal::new(3, 0),
        realized_pnl: Decimal::new(1_484, 0),
        realized_r: Decimal::new(2, 0),
        mfe_r: Decimal::new(25, 1),
        mae_r: Decimal::new(-5, 1),
        exit_reason: ExitReason::TakeProfit,
        source: TradeSource::Backtest,
        regime: Regime::TrendingUp,
        stop_price: Some(Decimal::new(28_500, 0)),
    }];
    let starting_equity = Decimal::new(10_000, 0);
    let net_pnl: Decimal = trades.iter().map(|t| t.realized_pnl).sum();
    let fees_total: Decimal = trades.iter().map(|t| t.fees_total).sum();
    let funding_total: Decimal = trades.iter().map(|t| t.funding_total).sum();
    let slippage_total: Decimal = trades.iter().map(|t| t.slippage_total).sum();
    let equity_curve = EquityCurve::from_trades(0, starting_equity, &trades);
    let summary =
        SummaryStats::from_trades(&trades, net_pnl, fees_total, funding_total, &equity_curve);
    let result = BacktestResult {
        trades,
        net_pnl,
        fees_total,
        funding_total,
        slippage_total,
        regime_breakdown: RegimeBreakdown::default(),
        skipped_entries: SkippedEntryCounts::default(),
        open_position: None,
        engine_fingerprint: pulse::EngineFingerprint::current(),
        summary: summary.clone(),
        equity_curve,
    };
    PreparedBacktest {
        inputs: BacktestInputs {
            pair: Pair::new("BTCUSDT"),
            primary: SnapshotSelection {
                timeframe: Timeframe::M15,
                data_version: DataVersion::new("v-primary"),
            },
            htf: None,
            taker_fee_bps: Decimal::new(4, 0),
            slippage_bps: Decimal::new(1, 0),
            funding: FundingConfig::SnapshotRates,
            window: None,
            lead_in_from_ms: None,
        },
        result,
        summary,
        starting_equity,
    }
}

fn prepared(session: &str) -> PreparedCoachAcceptance {
    PreparedCoachAcceptance {
        session_id: CoachingSessionId::new(session),
        // The accept's optimistic lock: the fixture proposal's own mutation, so the
        // guard passes for every case that is not testing the guard itself.
        expected_mutation: a_proposal().mutation,
        child_dsl: child_dsl(),
        prepared_run: prepared_backtest(),
        walk_forward: None,
    }
}

/// Seed a proposed session with an open proposal, through the real repository.
async fn proposed_turn(repo: &SqliteCoachingRepo<FakeClock>, id: &str) -> CoachingSessionId {
    repo.claim_session(claim(id, "run-1", "ver-1", FINGERPRINT_A))
        .await
        .expect("claim");
    repo.finish_session(
        &CoachingSessionId::new(id),
        InitialCoachOutcome {
            llm_call_id: Some(LlmCallId::new("call-1")),
            outcome: SessionOutcome::Proposed {
                proposal: a_proposal(),
            },
        },
    )
    .await
    .expect("finish");
    CoachingSessionId::new(id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_accept_is_recorded_and_creates_no_child() {
    let (repo, pool, _tmp) = coaching_repo().await;
    let id = proposed_turn(&repo, "sess-1").await;
    let accepts = acceptance_repo(&pool);

    let proposal = accepts
        .record_accept_failure(
            &id,
            CoachAcceptFailure {
                stage: AcceptFailureStage::Backtest,
                message: "the parent run's primary snapshot is gone".to_owned(),
                subject: Some("v-primary".to_owned()),
            },
        )
        .await
        .expect("record the typed accept failure");

    assert_eq!(
        proposal.disposition,
        Disposition::Proposed,
        "recording a failure leaves the proposal open"
    );
    let failure = proposal
        .accept_failure
        .expect("the latest accept outcome rides the proposal projection");
    assert_eq!(failure.stage, AcceptFailureStage::Backtest);
    assert_eq!(failure.subject.as_deref(), Some("v-primary"));

    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(versions, 6, "a failed accept mints no child version");

    // It reads back through the ordinary session read, typed.
    let back = repo.get_session(&id).await.expect("read").expect("present");
    match back.outcome {
        SessionOutcome::Proposed { proposal } => {
            assert_eq!(
                proposal.accept_failure.map(|f| f.stage),
                Some(AcceptFailureStage::Backtest)
            );
        }
        other => panic!("expected the proposal back, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successful_accept_writes_child_run_and_links_in_one_transaction() {
    let (repo, pool, _tmp) = coaching_repo().await;
    let id = proposed_turn(&repo, "sess-1").await;
    let accepts = acceptance_repo(&pool);

    // A stale failure from an earlier attempt is cleared by the accept itself.
    accepts
        .record_accept_failure(
            &id,
            CoachAcceptFailure {
                stage: AcceptFailureStage::LoadSnapshots,
                message: "transient".to_owned(),
                subject: None,
            },
        )
        .await
        .expect("an earlier failed attempt");

    let outcome = accepts
        .commit_acceptance(prepared("sess-1"))
        .await
        .expect("commit the acceptance");

    // Identity is MINTED by the adapter, never supplied by the caller.
    let (child_strategy, child_parent, child_by, child_calls): (
        String,
        Option<String>,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT strategy_id, parent_version_id, created_by, creating_llm_call_ids \
         FROM strategy_version WHERE id = ?1",
    )
    .bind(outcome.child_version_id.as_str())
    .fetch_one(&pool)
    .await
    .expect("the minted child exists");
    assert_eq!(child_strategy, "strat-1", "the child stays in the strategy");
    assert_eq!(
        child_parent.as_deref(),
        Some("ver-1"),
        "provenance is derived from the claimed session row, not from the caller"
    );
    assert_eq!(child_by, "\"coach_llm\"", "an accepted child is coach-made");
    assert!(
        child_calls.contains("call-1"),
        "the creating call comes from the session, got {child_calls}"
    );

    // The run is written against the MINTED child, and the trades against the run.
    let run_version: String =
        sqlx::query_scalar("SELECT strategy_version_id FROM backtest_run WHERE id = ?1")
            .bind(outcome.accepted_run_id.as_str())
            .fetch_one(&pool)
            .await
            .expect("the minted run exists");
    assert_eq!(run_version, outcome.child_version_id.as_str());
    let trades: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trade WHERE backtest_run_id = ?1")
        .bind(outcome.accepted_run_id.as_str())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(trades, 1, "the prepared trades ride the minted run");

    // The proposal now names both, carries no stale failure, and is terminal.
    let back = repo.get_session(&id).await.expect("read").expect("present");
    match back.outcome {
        SessionOutcome::Proposed { proposal } => {
            assert_eq!(
                proposal.disposition,
                Disposition::Accepted {
                    child_version_id: outcome.child_version_id.clone(),
                    accepted_run_id: outcome.accepted_run_id.clone(),
                }
            );
            assert!(
                proposal.accept_failure.is_none(),
                "a successful accept clears the stale failure inside the transaction"
            );
        }
        other => panic!("expected the proposal back, got {other:?}"),
    }

    // Replaying the accept returns the EXACT existing pair and inserts nothing.
    let versions_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    let replay = accepts
        .commit_acceptance(prepared("sess-1"))
        .await
        .expect("replaying an accept is idempotent");
    assert_eq!(replay, outcome, "the same child and run come back");
    let versions_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        versions_before, versions_after,
        "an idempotent accept inserts nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accept_against_a_session_that_never_proposed_writes_nothing() {
    let (repo, pool, _tmp) = coaching_repo().await;
    let accepts = acceptance_repo(&pool);

    // A failed turn has no proposal to accept.
    repo.claim_session(claim("sess-failed", "run-1", "ver-1", FINGERPRINT_A))
        .await
        .expect("claim");
    repo.finish_session(
        &CoachingSessionId::new("sess-failed"),
        InitialCoachOutcome {
            llm_call_id: None,
            outcome: SessionOutcome::Failed {
                failure: CoachFailure::MissingBacktestInputs {
                    detail: "the parent run records no primary snapshot".to_owned(),
                },
            },
        },
    )
    .await
    .expect("finish");

    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        accepts
            .commit_acceptance(prepared("sess-failed"))
            .await
            .is_err(),
        "a failed turn has no proposal to accept"
    );
    assert!(
        accepts
            .record_accept_failure(
                &CoachingSessionId::new("sess-failed"),
                CoachAcceptFailure {
                    stage: AcceptFailureStage::Apply,
                    message: "nothing to apply".to_owned(),
                    subject: None,
                }
            )
            .await
            .is_err(),
        "and no accept failure to record against it"
    );
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, after, "a refused accept rolls the whole write back");
}

/// `pulseai-labs/pulse-trader#149` — a failure AFTER the child insert rolls back the
/// child, its run, its trades and the proposal's links TOGETHER.
///
/// `commit_acceptance` writes four kinds of row in one transaction, and the ones
/// that matter most are written LAST: the run belongs to the just-minted child, the
/// trades belong to the just-minted run. A fault in the trade insert is therefore
/// the case where a non-atomic implementation would leave the worst residue — a
/// child version with no run, which is exactly the shape release exit criterion 4
/// forbids.
///
/// The injected fault is a ROW-level one: a trade whose `mfe_r` trips a constraint
/// on the `trade` table, installed for this test. It fires after the child and the
/// run are already inserted on the open transaction.
///
/// The proof is **"no settled row exists"**, not `COUNT(*) = 0`: the session and its
/// open proposal are supposed to still be there, and a bare count would pass for the
/// wrong reason (r1.s4.w1 §8).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fault_after_the_child_insert_rolls_the_whole_accept_back() {
    let (repo, pool, _tmp) = coaching_repo().await;
    let id = proposed_turn(&repo, "sess-rollback").await;
    let accepts = acceptance_repo(&pool);

    sqlx::query(
        "CREATE TRIGGER _t_149_trade_guard BEFORE INSERT ON trade \
         WHEN NEW.mfe_r = '99999' \
         BEGIN SELECT RAISE(ABORT, 'trade: injected constraint violation'); END",
    )
    .execute(&pool)
    .await
    .expect("install the trade constraint");

    let versions_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    let runs_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backtest_run")
        .fetch_one(&pool)
        .await
        .unwrap();

    let mut acceptance = prepared("sess-rollback");
    for trade in &mut acceptance.prepared_run.result.trades {
        trade.mfe_r = Decimal::new(99_999, 0);
    }

    let outcome = accepts.commit_acceptance(acceptance).await;
    assert!(
        outcome.is_err(),
        "a refused trade insert must fail the whole accept"
    );

    // Child, run and trades are all gone together — not one of them survived the
    // other's rollback.
    let versions_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    let runs_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backtest_run")
        .fetch_one(&pool)
        .await
        .unwrap();
    let trades_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trade")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(versions_after, versions_before, "no child version survived");
    assert_eq!(runs_after, runs_before, "no run survived");
    assert_eq!(trades_after, 0, "no trade survived");

    // NO SETTLED ROW EXISTS. The session and its proposal are still there — that is
    // the point — so the assertion is about the DISPOSITION, not about absence.
    let settled: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM coaching_proposals \
         WHERE disposition = 'accepted' OR child_version_id IS NOT NULL \
            OR accepted_run_id IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(settled, 0, "no accepted row and no dangling link exists");

    let open = repo
        .get_session(&id)
        .await
        .expect("read")
        .expect("the session is still there");
    match open.outcome {
        SessionOutcome::Proposed { proposal } => {
            assert_eq!(
                proposal.disposition,
                Disposition::Proposed,
                "the proposal is exactly as open as it was"
            );
            assert!(
                proposal.accept_failure.is_none(),
                "commit_acceptance records no failure — that is `record_accept_failure`'s \
                 job, in its own transaction"
            );
        }
        other => panic!("expected the proposal back, got {other:?}"),
    }
}
