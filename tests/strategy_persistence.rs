//! End-to-end demo integration tests for VS-1.1.4 work-1.05 (auto #1).
//!
//! Realizes the slice's first demo criterion — "a Strategy + immutable
//! `StrategyVersion` is created and reloaded BYTE-IDENTICALLY" (NFR-2) — plus the
//! FR-4 immutability half (a raw `UPDATE`/`DELETE` against `strategy_version` is
//! aborted by the DB trigger). The load-bearing byte-identity + immutability
//! assertions drive the **library path** (the repo over a `TempDir` `Db`), so the
//! test can assert exact bytes + reach a raw `sqlx` tamper; a single smoke test
//! drives the **binary** to prove the clap→dispatch→repo wiring end-to-end.
//!
//! Offline (`SQLX_OFFLINE=true` + committed `.sqlx/` + in-process `MIGRATOR`),
//! `TempDir`-isolated (never the real Application Support dir).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use pulse::{
    AgentHypothesis, AgentName, Comparator, Condition, CreatedBy, DataError, Db, Direction,
    ExitRule, IndicatorSpec, MIGRATOR, NewAgentSubmission, NewVersion, RiskParams, SchemaVersion,
    Series, SqliteStrategyRepo, StrategyDsl, StrategyRepository, SweepableValue, ValueSource,
    VersionId,
};
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use tempfile::TempDir;

/// A `(repo, pool, tempdir)` triple over a fresh migrated tempfile `pulse.db`.
/// The integration test opens its OWN `Db` + pool (the repo's pool is private) —
/// mirrors `strategy_repo.rs`'s in-crate `repo()` helper, but through the public
/// `pulse::{Db, MIGRATOR, SqliteStrategyRepo}` surface (§4a-7).
async fn repo() -> (SqliteStrategyRepo<pulse::SystemClock>, SqlitePool, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db at tempfile path");
    MIGRATOR.run(db.pool()).await.expect("run 0001_init");
    let pool = db.pool().clone();
    (SqliteStrategyRepo::new(pool.clone()), pool, tmp)
}

/// The canonical `1.0.0` RSI-oversold strategy — VALID per `validate()` (it has a
/// `StopLoss`, §4a-5). Built via the typed `StrategyDsl` so the shape is
/// guaranteed schema-current; `create_version` runs `validate()` after
/// `Migrator::load`, so a parseable-but-invalid DSL would be REJECTED. Mirrors
/// `strategy_repo.rs::canonical_dsl()`.
fn canonical_dsl() -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "RSI Oversold".to_owned(),
        direction: Direction::Long,
        entry: Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Rsi {
                    period: SweepableValue::Fixed(14),
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

/// The canonical DSL serialized to a JSON string (the `--dsl <file>` / `dsl_json`
/// contents).
fn canonical_json() -> String {
    serde_json::to_string(&canonical_dsl()).expect("serialize canonical dsl")
}

/// Create a strategy + one version over a fresh repo, returning the source bytes
/// and the created version id for the byte-identity / tamper assertions.
async fn seed_one_version(repo: &SqliteStrategyRepo<pulse::SystemClock>) -> (String, VersionId) {
    let s = repo
        .create_strategy("Demo", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create strategy");
    let dsl_json = canonical_json();
    let created = repo
        .create_version(NewVersion {
            strategy_id: s.id.clone(),
            parent_version_id: None,
            dsl_json: dsl_json.clone(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");
    (dsl_json, created.id)
}

// ---- AC-7 (NFR-2 / auto #1): byte-identical reload through the repo ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_is_byte_identical_through_repo() {
    let (repo, _pool, _tmp) = repo().await;
    let s = repo
        .create_strategy("ByteId", Some("alice"), &["scalp".to_owned()])
        .await
        .unwrap();
    let dsl_json = canonical_json();
    let created = repo
        .create_version(NewVersion {
            strategy_id: s.id.clone(),
            parent_version_id: None,
            dsl_json: dsl_json.clone(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .unwrap();

    let fetched = repo.get_version(&created.id).await.unwrap().unwrap();

    // (a) NFR-2: the verbatim source bytes survive create→read BYTE-FOR-BYTE.
    assert_eq!(
        fetched.dsl_original, dsl_json,
        "dsl_original must round-trip byte-identical"
    );

    // (b) NFR-2: the stored version_hash re-derives equal (the read defense in
    // `row_to_version` already rejects a mismatch — a successful get_version IS
    // the re-derivation proof; assert the field is the 64-char SHA-256 hex).
    assert_eq!(
        fetched.version_hash.len(),
        64,
        "version_hash is SHA-256 hex"
    );
    assert!(
        fetched.version_hash.chars().all(|c| c.is_ascii_hexdigit()),
        "version_hash is lowercase hex"
    );

    // (c) the migrated `.dsl` round-trips through Migrator::load to the canonical
    // typed document.
    assert_eq!(
        fetched.dsl,
        canonical_dsl(),
        "loaded dsl is the canonical doc"
    );

    // (d) exact field equality on the reloaded Strategy + StrategyVersion.
    let reloaded_strategy = repo.get_strategy(&s.id).await.unwrap().unwrap();
    assert_eq!(reloaded_strategy, s, "Strategy reloads field-identical");
    assert_eq!(fetched.strategy_id, s.id);
    assert_eq!(fetched.parent_version_id, None);
    assert_eq!(fetched.dsl_schema_version, SchemaVersion::CURRENT);
    assert_eq!(fetched.created_by, CreatedBy::Human);
    assert!(
        fetched.creating_llm_call_ids.is_empty(),
        "FR-11: no LLM ⇒ empty creating_llm_call_ids"
    );
}

// ---- AC-8 (FR-4): a raw UPDATE on strategy_version is aborted by the trigger -

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_update_on_strategy_version_aborts() {
    let (repo, pool, _tmp) = repo().await;
    let (_dsl, vid) = seed_one_version(&repo).await;
    let id = vid.as_str().to_owned();

    // A RAW UPDATE bypassing the repo (which has no update_version) must be
    // aborted by the BEFORE UPDATE trigger (FR-4 end-to-end immutability proof).
    let err = sqlx::query("UPDATE strategy_version SET dsl = 'tampered' WHERE id = ?1")
        .bind(&id)
        .execute(&pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))
        .expect_err("raw UPDATE on an immutable row must fail");
    match err {
        DataError::Db(msg) => assert!(
            msg.contains("strategy_version is immutable"),
            "trigger ABORT message must surface; got: {msg}"
        ),
        other => panic!("expected DataError::Db, got {other:?}"),
    }

    // The row is UNCHANGED (the abort rolled the statement back).
    let after = repo.get_version(&vid).await.unwrap().unwrap();
    assert_eq!(
        after.dsl,
        canonical_dsl(),
        "row unchanged after aborted UPDATE"
    );
}

// ---- AC-9 (FR-4): a raw DELETE on strategy_version is aborted by the trigger -

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_delete_on_strategy_version_aborts() {
    let (repo, pool, _tmp) = repo().await;
    let (_dsl, vid) = seed_one_version(&repo).await;
    let id = vid.as_str().to_owned();

    // SQLite needs a SEPARATE BEFORE DELETE trigger — this pins it is wired.
    let err = sqlx::query("DELETE FROM strategy_version WHERE id = ?1")
        .bind(&id)
        .execute(&pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))
        .expect_err("raw DELETE on an immutable row must fail");
    match err {
        DataError::Db(msg) => assert!(
            msg.contains("strategy_version is immutable"),
            "trigger ABORT message must surface; got: {msg}"
        ),
        other => panic!("expected DataError::Db, got {other:?}"),
    }

    // The row still exists (the abort rolled the DELETE back).
    let still = repo.get_version(&vid).await.unwrap();
    assert!(still.is_some(), "row still present after aborted DELETE");
}

// ---- r2.s1.w1 (AC-2): the external-agent provenance pair --------------------
//
// `create_agent_version` writes TWO rows in one `BEGIN IMMEDIATE` transaction —
// the immutable `strategy_version` (created_by `external_agent`, no LLM-call
// provenance) and its `agent_submission` audit row — or neither. A coach
// version's audit trail is its coaching session and its `LlmCall`; the external
// agent's cost is external to the app, so the submission is the ONLY durable
// link between the version and the agent + hypothesis that produced it.

/// A valid `NewAgentSubmission` — `AgentName`/`Hypothesis` are validated
/// newtypes, so this is the only construction path.
fn a_submission() -> NewAgentSubmission {
    NewAgentSubmission {
        agent_name: AgentName::parse("Claude-Code").expect("a valid agent name"),
        hypothesis: AgentHypothesis::parse("RSI(21) oversold holds longer than RSI(14)")
            .expect("a valid hypothesis"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_external_agent_version_round_trips_with_its_submission() {
    let (repo, _pool, _tmp) = repo().await;
    let s = repo
        .create_strategy("AgentTree", Some("agent"), &[])
        .await
        .unwrap();
    let parent = repo
        .create_version(NewVersion {
            strategy_id: s.id.clone(),
            parent_version_id: None,
            dsl_json: canonical_json(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .unwrap();

    let (version, submission) = repo
        .create_agent_version(
            NewVersion {
                strategy_id: s.id.clone(),
                parent_version_id: Some(parent.id.clone()),
                dsl_json: canonical_json(),
                created_by: CreatedBy::ExternalAgent,
                creating_llm_call_ids: vec![],
            },
            a_submission(),
        )
        .await
        .expect("the version + submission pair commits");

    // The version carries the sixth provenance and NO ledger ids — the agent's
    // call is external, so there is nothing truthful to put there.
    assert_eq!(version.created_by, CreatedBy::ExternalAgent);
    assert!(
        version.creating_llm_call_ids.is_empty(),
        "an external_agent version names no LlmCall rows"
    );
    assert_eq!(version.parent_version_id, Some(parent.id));

    // The submission is the normalized audit row, keyed to the minted version.
    assert_eq!(submission.version_id, version.id);
    assert_eq!(
        submission.agent_name.as_str(),
        "claude-code",
        "AgentName normalizes to lowercase"
    );

    // The version round-trips through the EXISTING read path, hash defense and
    // all — `create_agent_version` reuses `insert_version_row`, so `get_version`
    // is the proof the stored row is a real version.
    let fetched = repo
        .get_version(&version.id)
        .await
        .unwrap()
        .expect("the agent version reads back");
    assert_eq!(fetched, version, "the version round-trips byte-identical");

    let back = repo
        .get_agent_submission(&version.id)
        .await
        .unwrap()
        .expect("the submission reads back");
    assert_eq!(
        back, submission,
        "the submission round-trips field-identical"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_agent_version_refuses_a_non_agent_provenance_and_llm_call_ids() {
    let (repo, pool, _tmp) = repo().await;
    let s = repo.create_strategy("Refusals", None, &[]).await.unwrap();
    let versions_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .unwrap();

    // A coach-composed request is NOT an agent submission: refusing pre-DB keeps
    // a misattributed version out of the audit trail.
    assert!(
        repo.create_agent_version(
            NewVersion {
                strategy_id: s.id.clone(),
                parent_version_id: None,
                dsl_json: canonical_json(),
                created_by: CreatedBy::CoachLlm,
                creating_llm_call_ids: vec![],
            },
            a_submission(),
        )
        .await
        .is_err(),
        "created_by != external_agent must be refused before the database"
    );

    // An agent submission writes no LlmCall — its cost is external — so a
    // request carrying call ids claims a provenance the ledger does not hold.
    assert!(
        repo.create_agent_version(
            NewVersion {
                strategy_id: s.id.clone(),
                parent_version_id: None,
                dsl_json: canonical_json(),
                created_by: CreatedBy::ExternalAgent,
                creating_llm_call_ids: vec!["call-1".to_owned()],
            },
            a_submission(),
        )
        .await
        .is_err(),
        "a non-empty creating_llm_call_ids must be refused before the database"
    );

    let versions_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        versions_after, versions_before,
        "a refused call writes no version"
    );
    let submissions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_submission")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(submissions, 0, "a refused call writes no submission");
}

/// Atomicity: when the transaction fails, BOTH halves roll back.
///
/// The reachable in-transaction failure through the typed port is the
/// VERSION-side insert (here, an `strategy_id` FK violation): a submission-side
/// CHECK failure is *unrepresentable* through `NewAgentSubmission` — the domain
/// newtypes are strictly tighter than the schema CHECKs (`agent_name`'s
/// charset/length and `hypothesis`'s bounds are enforced at construction), so
/// the schema CHECKs exist only against writes that bypass the domain. Those
/// raw-SQL refusals are proven in `tests/migration_0009.rs`; here the property
/// under test is that a failed insert leaves NO row on either side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_agent_version_is_atomic_when_the_version_insert_fails() {
    let (repo, pool, _tmp) = repo().await;

    assert!(
        repo.create_agent_version(
            NewVersion {
                strategy_id: pulse::StrategyId::new("strat-missing"),
                parent_version_id: None,
                dsl_json: canonical_json(),
                created_by: CreatedBy::ExternalAgent,
                creating_llm_call_ids: vec![],
            },
            a_submission(),
        )
        .await
        .is_err(),
        "the version insert must fail on the strategy FK"
    );

    // The one transaction rolled back: no version AND no submission.
    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(versions, 0, "the failed insert left no version row");
    let submissions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_submission")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(submissions, 0, "the failed insert left no submission row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_version_writes_no_agent_submission() {
    let (repo, pool, _tmp) = repo().await;
    let (_dsl, vid) = seed_one_version(&repo).await;

    let submissions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_submission")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        submissions, 0,
        "the ordinary create_version path never touches the ledger"
    );
    assert!(
        repo.get_agent_submission(&vid).await.unwrap().is_none(),
        "a human-authored version has no submission to read back"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_agent_submission_returns_none_for_a_coach_version() {
    let (repo, _pool, _tmp) = repo().await;
    let s = repo.create_strategy("CoachTree", None, &[]).await.unwrap();
    let coach_child = repo
        .create_version(NewVersion {
            strategy_id: s.id.clone(),
            parent_version_id: None,
            dsl_json: canonical_json(),
            created_by: CreatedBy::CoachLlm,
            creating_llm_call_ids: vec![],
        })
        .await
        .unwrap();

    assert!(
        repo.get_agent_submission(&coach_child.id)
            .await
            .unwrap()
            .is_none(),
        "a coach version's audit trail is its coaching session, not this ledger"
    );
}

// ---- r2.s1.w3 (F3): the root submit is ONE atomic write ----------------------
//
// `create_agent_strategy_version` commits the `strategy` row, its root
// `external_agent` version and the `agent_submission` in a single
// `BEGIN IMMEDIATE` transaction, with the name-uniqueness check inside the
// same lock. The old `list_strategies` + `create_strategy` +
// `create_agent_version` sequence was a check-then-act race (two concurrent
// submits both pass the pre-read) AND could orphan a `strategy` row when the
// version write failed.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_root_writes_strategy_version_and_submission_as_one_commit() {
    let (repo, pool, _tmp) = repo().await;

    let (strategy, version, submission) = repo
        .create_agent_strategy_version("AgentRoot", canonical_json(), a_submission())
        .await
        .expect("the triple commit succeeds");

    // The three rows agree on their relationships.
    assert_eq!(strategy.name, "AgentRoot");
    assert!(
        strategy.owner.is_none() && strategy.tags.is_empty(),
        "an agent root is a bare strategy — owner/tags are human-library metadata"
    );
    assert_eq!(version.strategy_id, strategy.id);
    assert_eq!(
        version.parent_version_id, None,
        "a root version has no parent"
    );
    assert_eq!(version.created_by, CreatedBy::ExternalAgent);
    assert!(version.creating_llm_call_ids.is_empty());
    assert_eq!(submission.version_id, version.id);

    // Read-back through the defended paths agrees.
    let fetched = repo
        .get_version(&version.id)
        .await
        .unwrap()
        .expect("the root version reads back");
    assert_eq!(fetched, version, "the version round-trips byte-identical");
    assert_eq!(
        repo.get_agent_submission(&version.id)
            .await
            .unwrap()
            .expect("the submission reads back"),
        submission
    );

    // Exactly the three rows exist — nothing half-written.
    let strategies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy")
        .fetch_one(&pool)
        .await
        .unwrap();
    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    let submissions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_submission")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((strategies, versions, submissions), (1, 1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_agent_strategy_version_refuses_a_taken_name_and_writes_nothing() {
    let (repo, pool, _tmp) = repo().await;
    // A same-named HUMAN strategy is legal data — and it is exactly what a
    // root submit must not clobber.
    repo.create_strategy("Taken", Some("alice"), &[])
        .await
        .unwrap();

    let err = repo
        .create_agent_strategy_version("Taken", canonical_json(), a_submission())
        .await
        .expect_err("a taken name is refused under the write lock");
    match err {
        DataError::StrategyNameTaken { name } => {
            assert_eq!(name, "Taken", "the refusal names the colliding name");
        }
        other => panic!("expected DataError::StrategyNameTaken, got {other:?}"),
    }

    // The refusal rolled back cleanly: the human strategy is the ONLY row.
    let strategies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(strategies, 1, "no second strategy row was written");
    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(versions, 0, "no orphan version row");
    let submissions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_submission")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(submissions, 0, "no orphan submission row");
}

/// THE race regression: N concurrent root submits on one name — exactly one
/// commits. `BEGIN IMMEDIATE` serializes the writers; every loser's in-lock
/// re-check sees the winner's committed `strategy` row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_name_root_writes_commit_exactly_one() {
    const RACERS: usize = 6;

    let (repo, pool, _tmp) = repo().await;
    let repo = std::sync::Arc::new(repo);
    let mut handles = Vec::with_capacity(RACERS);
    for _ in 0..RACERS {
        let repo = repo.clone();
        handles.push(tokio::spawn(async move {
            repo.create_agent_strategy_version("Racer", canonical_json(), a_submission())
                .await
        }));
    }

    let mut committed = 0usize;
    let mut refused = 0usize;
    for handle in handles {
        match handle.await.expect("racer joins") {
            Ok(_) => committed += 1,
            Err(DataError::StrategyNameTaken { name }) => {
                assert_eq!(name, "Racer");
                refused += 1;
            }
            Err(other) => panic!("a racer failed with neither commit nor refusal: {other:?}"),
        }
    }
    assert_eq!(committed, 1, "exactly one racer commits the name");
    assert_eq!(
        refused,
        RACERS - 1,
        "every loser is refused, not duplicated"
    );

    // The committed state is one consistent triple — never two same-named
    // strategies and never a strategy without its root version.
    let named: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy WHERE name = 'Racer'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(named, 1, "one strategy row carries the name");
    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(versions, 1, "exactly the winner's root version");
    let submissions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_submission")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(submissions, 1, "exactly the winner's submission");
}

/// The document pipeline runs BEFORE the transaction: an invalid DSL leaves
/// the store untouched (no strategy, no version, no submission).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_agent_strategy_version_refuses_an_invalid_document_before_the_store() {
    let (repo, pool, _tmp) = repo().await;

    assert!(
        repo.create_agent_strategy_version(
            "BadDoc",
            "{\"schema_version\":\"1.0.0\"}".to_owned(),
            a_submission(),
        )
        .await
        .is_err(),
        "a document that will not load/validate is refused"
    );

    for table in ["strategy", "strategy_version", "agent_submission"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "a refused document wrote no {table} row");
    }
}

// ---- G8/T23: a post-commit read fault must never report a committed write
// ---- as failed --------------------------------------------------------------
//
// Pre-G8, `create_*` committed, then ran its defended read-back through the
// pool — a transient pool fault after the commit made the caller see `Err` for
// a write that had already landed, and a retry double-wrote (there is no
// idempotency key). Now the read-back runs on the transaction's own
// connection, before commit: a read failure rolls back, so `Err` always means
// nothing was written.
//
// The fault is REAL rather than mocked into the repo: a one-connection pool
// that discards every released connection and refuses every connect after the
// first — under the old shape the post-commit `get_version` would hit a dead
// pool on a committed write.

/// A `(repo, db_path)` pair over a migrated tempfile where the pool opens ONE
/// connection ever — after it is released (post-commit) every acquire fails.
async fn dead_after_first_release_repo(
    tmp: &TempDir,
) -> (SqliteStrategyRepo<pulse::SystemClock>, std::path::PathBuf) {
    let db_path = tmp.path().join("pulse.db");
    {
        let db = Db::with_path(&db_path)
            .await
            .expect("open db at tempfile path");
        MIGRATOR.run(db.pool()).await.expect("migrate");
    }

    let connects = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connects_in_hook = connects.clone();
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .after_connect(move |_conn, _meta| {
            let connects = connects_in_hook.clone();
            Box::pin(async move {
                if connects.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    Ok(())
                } else {
                    Err(sqlx::Error::Io(std::io::Error::other(
                        "injected connect fault",
                    )))
                }
            })
        })
        // `Ok(false)` discards the tx's connection the instant it returns to
        // the pool — the post-commit read would need a fresh connect.
        .after_release(|_conn, _meta| Box::pin(async move { Ok(false) }))
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&db_path)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .foreign_keys(true),
        )
        .await
        .expect("adversarial pool");
    (SqliteStrategyRepo::new(pool), db_path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_post_commit_read_fault_never_reports_a_committed_write_as_failed() {
    let tmp = TempDir::new().expect("tempdir");

    // Root path: strategy + version + submission in one tx; the read-back
    // shares the tx connection, so the dead post-commit pool is irrelevant.
    let (root_repo, db_path) = dead_after_first_release_repo(&tmp).await;
    let (strategy, version, submission) = root_repo
        .create_agent_strategy_version("G8 Root", canonical_json(), a_submission())
        .await
        .expect("the write verified and committed before the pool went dead");

    // Child path: version + submission under the root — same guarantee.
    let (child_repo, _) = dead_after_first_release_repo(&tmp).await;
    let (child_version, child_submission) = child_repo
        .create_agent_version(
            NewVersion {
                strategy_id: strategy.id.clone(),
                parent_version_id: Some(version.id.clone()),
                dsl_json: canonical_json(),
                created_by: CreatedBy::ExternalAgent,
                creating_llm_call_ids: vec![],
            },
            a_submission(),
        )
        .await
        .expect("the child write verified and committed before the pool went dead");

    // The rows really landed — proven through a fresh, healthy pool.
    let check_db = Db::with_path(&db_path).await.expect("reopen db");
    let check = SqliteStrategyRepo::new(check_db.pool().clone());
    for vid in [&version.id, &child_version.id] {
        assert!(check.get_version(vid).await.unwrap().is_some());
        assert!(check.get_agent_submission(vid).await.unwrap().is_some());
    }
    assert_eq!(submission.version_id, version.id);
    assert_eq!(child_submission.version_id, child_version.id);
}

// ---- binary smoke test (AC-5): clap→dispatch→repo wiring end-to-end ----------

#[test]
fn binary_create_then_show_over_tempdb() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("pulse.db");

    // `strategy create demo --db <tempdb>` exits 0 and echoes a UUID-shaped id.
    let create = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "strategy",
            "create",
            "demo",
            "--db",
            db_path.to_str().expect("utf8 db path"),
        ])
        .output()
        .expect("run pulse strategy create");
    assert!(
        create.status.success(),
        "create status={:?}\nstderr={}",
        create.status.code(),
        String::from_utf8_lossy(&create.stderr)
    );
    let id = String::from_utf8(create.stdout)
        .expect("stdout utf8")
        .trim()
        .to_owned();
    // A UUID v4 is 36 hyphenated chars (8-4-4-4-12).
    assert_eq!(id.len(), 36, "create echoes a UUID-shaped id, got {id:?}");
    assert_eq!(id.matches('-').count(), 4, "UUID has 4 hyphens, got {id:?}");

    // A follow-up `strategy show <id> --db <tempdb>` exits 0 and prints it.
    let show = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "strategy",
            "show",
            &id,
            "--db",
            db_path.to_str().expect("utf8 db path"),
        ])
        .output()
        .expect("run pulse strategy show");
    assert!(
        show.status.success(),
        "show status={:?}\nstderr={}",
        show.status.code(),
        String::from_utf8_lossy(&show.stderr)
    );
    let stdout = String::from_utf8(show.stdout).expect("stdout utf8");
    assert!(
        stdout.contains(&id),
        "show output must name the strategy id {id:?}; got: {stdout}"
    );
    assert!(
        stdout.contains("demo"),
        "show output must name the strategy; got: {stdout}"
    );
}
