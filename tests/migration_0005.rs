//! AC-2 — migration `0005_coaching`: the reserved out-of-order number, the
//! up/down round trip, and the constraints that hold the coaching schema honest
//! (r1.s2.w2, ADR-0021 / grill L2 / audit C3-C4).
//!
//! `0005` is the FIRST REAL EXERCISE of the reserved-number scheme. Release
//! planning allocated `0005` to `r1.s2` and `0006` to `r1.s3` while `r1.s1`
//! shipped `0007`, so this migration arrives at a database whose maximum applied
//! version is already higher than its own. `src/adapters/db/migrate.rs` compares
//! applied version SETS rather than maxima for exactly this reason (PR #115);
//! until now that support was proved against a synthetic probe migration, and this
//! binary proves it against the real one. `r1.s3`'s `0006` has since landed the
//! same way — `tests/backtest_provenance.rs` is its equivalent proof — so the
//! reserved-number window is now closed and the shipped set is contiguous.
//!
//! Offline (`SQLX_OFFLINE=true` + the in-process `MIGRATOR`), `TempDir`-isolated —
//! the suite never touches the real `pulse.db`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{Db, MIGRATOR, undo_to};
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// helpers
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

/// Assert every version in `versions` is present in the applied set — the
/// "rides along in the same run" checks this binary shares, named once so a new
/// migration in that run is one more number here rather than another four-line
/// assertion block inside the test.
fn assert_all_rode_along(applied: &BTreeSet<i64>, versions: &[i64]) {
    for version in versions {
        assert!(
            applied.contains(version),
            "{version:04} rides along in the same run: {applied:?}"
        );
    }
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

/// Copy the shipped `migrations/` set into `dir`, SKIPPING `0005_*` and
/// everything from `0008` on — the "older binary" that shipped `0007` while
/// `0005` was still reserved.
///
/// r1.s4.w4 added `0008` to the skip list, and it is not bookkeeping: `0008`
/// REBUILDS the two tables `0005` creates, so a set holding `0008` without `0005`
/// is not an older binary at all — it is an impossible one, and it fails with "no
/// such table: `coaching_proposals`". r2.s1.w1 adds `0009` for the same reason —
/// its pending-claim index targets `coaching_sessions`, a `0005` table — and G1
/// adds `0010`, which would lift the fixture's maximum off its pin. r2.s2.w2
/// adds `0011` (`trade.stop_price`), the same reason, and r2.s3.w2 adds `0012`
/// (`backtest_run.window_lead_in_from_ms`). The binary being simulated here
/// predates all six.
fn shipped_set_without_0005(dir: &Path) {
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(&shipped).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("0005_") || name.as_str() >= "0008" {
            continue;
        }
        std::fs::copy(&path, dir.join(&name)).unwrap();
    }
}

/// A fresh temp database migrated by the "older" set (everything but `0005`),
/// returning the guard, the db path, and the open `Db`.
async fn db_at_0007_without_0005() -> (TempDir, PathBuf, Db) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    shipped_set_without_0005(&dir);

    let db_path = tmp.path().join("pulse.db");
    let older = Migrator::new(dir.as_path()).await.unwrap();
    let db = Db::with_path(&db_path).await.unwrap();
    older.run(db.pool()).await.expect("the older set applies");

    let applied = applied_versions(db.pool()).await;
    assert!(
        !applied.contains(&5),
        "the fixture must NOT have 0005 applied: {applied:?}"
    );
    assert_eq!(
        applied.iter().copied().max(),
        Some(7),
        "the fixture's maximum applied version is 7, above 0005's own"
    );
    (tmp, db_path, db)
}

/// Insert the FK parents a coaching session needs: a strategy, a version, a run,
/// and an `llm_call`. Raw SQL rather than the repos — this binary is about the
/// schema, and the repo round-trip is AC-3's.
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
         (id, strategy_id, dsl_schema_version, dsl, dsl_original, version_hash, created_by, \
          creating_llm_call_ids, created_at) \
         VALUES ('ver-1', 'strat-1', '1.0.0', '{}', '{}', 'hash-1', 'human', '[]', \
                 '2026-08-29T00:00:00.000Z')",
    )
    .execute(pool)
    .await
    .expect("seed strategy_version");

    // A second version, so an `accepted` disposition has a child to point at.
    //
    // r1.s4.w4: it is now a REAL CHILD of `ver-1` — `0008`'s lineage trigger asks
    // that an accepted child descend from the version the session coached, and a
    // root version standing in for a child was only ever legal because nothing
    // checked. The fixture had to become true rather than merely present.
    sqlx::query(
        "INSERT INTO strategy_version \
         (id, strategy_id, parent_version_id, dsl_schema_version, dsl, dsl_original, \
          version_hash, created_by, creating_llm_call_ids, created_at) \
         VALUES ('ver-2', 'strat-1', 'ver-1', '1.0.0', '{}', '{}', 'hash-2', 'coach_llm', '[]', \
                 '2026-08-29T00:00:01.000Z')",
    )
    .execute(pool)
    .await
    .expect("seed child strategy_version");

    // r1.s3.w2: `0006`'s BEFORE INSERT completeness trigger requires every fresh row
    // to name its input provenance. This binary is about `0005`'s schema, so the
    // tuple is a plain complete one — the trigger only asks that it be present and
    // internally consistent.
    sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, engine_target, \
          result_content_hash, starting_equity, net_pnl, fees_total, funding_total, slippage_total, \
          pair, primary_timeframe, primary_data_version, taker_fee_bps, slippage_bps, \
          funding_config) \
         VALUES ('run-1', 'ver-1', '1', '2026-08-29T00:00:00.000Z', 'fp-1', 'test-target', \
                 'rch-1', '10000', '0', '0', '0', '0', \
                 'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates')",
    )
    .execute(pool)
    .await
    .expect("seed backtest_run");

    // r1.s4.w4: a run OF the child version. `0008` requires an accepted proposal to
    // name the re-backtest of its child, so the legal pairing below needs one.
    sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, engine_target, \
          result_content_hash, starting_equity, net_pnl, fees_total, funding_total, slippage_total, \
          pair, primary_timeframe, primary_data_version, taker_fee_bps, slippage_bps, \
          funding_config) \
         VALUES ('run-child-2', 'ver-2', '1', '2026-08-29T00:00:01.000Z', 'fp-1', 'test-target', \
                 'rch-2', '10000', '0', '0', '0', '0', \
                 'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates')",
    )
    .execute(pool)
    .await
    .expect("seed the child's backtest_run");

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

/// Insert a `proposed`-outcome coaching session.
async fn insert_proposed_session(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO coaching_sessions \
         (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
          failure_kind, failure_detail, schema_version) \
         VALUES (?1, 'run-1', 'ver-1', '2026-08-29T00:00:00.000Z', 'call-1', 'proposed', \
                 NULL, NULL, 1)",
    )
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Insert a proposal row, returning the raw SQL outcome so a test can assert a
/// constraint rejection.
async fn insert_proposal(
    pool: &SqlitePool,
    id: &str,
    session_id: &str,
    hypothesis: &str,
    disposition: &str,
    child_version_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO coaching_proposals \
         (id, session_id, mutation, hypothesis, disposition, child_version_id) \
         VALUES (?1, ?2, '{\"type\":\"set_param\"}', ?3, ?4, ?5)",
    )
    .bind(id)
    .bind(session_id)
    .bind(hypothesis)
    .bind(disposition)
    .bind(child_version_id)
    .execute(pool)
    .await
    .map(|_| ())
}

// ---------------------------------------------------------------------------
// 1. The reserved out-of-order number
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0005_applies_to_a_database_already_at_0007() {
    let (_tmp, _path, db) = db_at_0007_without_0005().await;

    // A row written BEFORE 0005 exists — it must survive the migration and read
    // back with no prompt version, rather than becoming unreadable (the same
    // backward-compatibility contract 0007 recorded for `key_source`).
    seed_parents(db.pool()).await;

    // Now the binary that ships the reserved 0005 opens the same database.
    MIGRATOR
        .run(db.pool())
        .await
        .expect("the reserved 0005 must apply to a db already holding 0007");

    let applied = applied_versions(db.pool()).await;
    assert!(
        applied.contains(&5),
        "0005 must be applied, not skipped: {applied:?}"
    );
    // The subject of this test is that `0005` APPLIED even though it sorts BELOW
    // the maximum the database already held — the reserved-number property. The
    // maximum itself moved to 14 across r1.s4.w4, r2.s1.w1, G1, r2.s2.w2,
    // r2.s3.w2, r2.s3.w3 and r2.s3.w4 because the same run also applies `0008`,
    // `0009`, `0010`, `0011`, `0012`, `0013` and `0014`, which the fixture
    // withheld; asserting
    // `Some(7)` here would now be asserting that they did not run, which is a
    // different (and false) claim.
    assert_all_rode_along(&applied, &[8, 9, 10, 11, 12, 13, 14]);
    assert_eq!(
        applied.iter().copied().max(),
        Some(14),
        "0005 is recorded at its own version, below the new maximum 0014 sets"
    );

    assert!(
        object_present(db.pool(), "table", "coaching_sessions").await,
        "coaching_sessions must exist after 0005"
    );
    assert!(
        object_present(db.pool(), "table", "coaching_proposals").await,
        "coaching_proposals must exist after 0005"
    );
    assert!(
        object_present(db.pool(), "index", "idx_coaching_sessions_run").await,
        "the run index must exist after 0005"
    );

    // `llm_call` gained a NULLABLE `prompt_version`, and the pre-existing row
    // reads back NULL — "no prompt version recorded", not an error.
    assert!(
        columns_of(db.pool(), "llm_call")
            .await
            .contains(&"prompt_version".to_owned()),
        "llm_call must carry prompt_version after 0005"
    );
    let prompt_version: Option<String> =
        sqlx::query_scalar("SELECT prompt_version FROM llm_call WHERE id = 'call-1'")
            .fetch_one(db.pool())
            .await
            .expect("read prompt_version");
    assert!(
        prompt_version.is_none(),
        "a row written before 0005 reads back with no prompt version, got {prompt_version:?}"
    );

    // And the column really is nullable for NEW rows too (composer rows stay NULL).
    sqlx::query(
        "INSERT INTO llm_call \
         (id, backend, model, prompt_messages, completion, input_tokens, output_tokens, cost, \
          cost_currency, created_at, created_by, schema_version) \
         VALUES ('call-2', 'ollama', 'glm-5.3-flash', '[]', NULL, 1, 1, '0', 'CNY', \
                 '2026-08-29T00:00:02.000Z', 'composer_llm', 1)",
    )
    .execute(db.pool())
    .await
    .expect("a composer row needs no prompt_version");
}

// ---------------------------------------------------------------------------
// 2. Up / down round trip
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0005_up_down_round_trips() {
    let tmp = TempDir::new().unwrap();
    let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();

    // Up.
    MIGRATOR.run(db.pool()).await.expect("run embedded set");
    assert!(object_present(db.pool(), "table", "coaching_sessions").await);
    assert!(object_present(db.pool(), "table", "coaching_proposals").await);
    assert!(
        columns_of(db.pool(), "llm_call")
            .await
            .contains(&"prompt_version".to_owned())
    );

    // Down to 0004 — reverts 0007 and then 0005 (descending order).
    undo_to(db.pool(), 4).await.expect("undo to 4");
    assert!(
        !object_present(db.pool(), "table", "coaching_sessions").await,
        "coaching_sessions is gone after the undo"
    );
    assert!(
        !object_present(db.pool(), "table", "coaching_proposals").await,
        "coaching_proposals is gone after the undo"
    );
    assert!(
        !object_present(db.pool(), "index", "idx_coaching_sessions_run").await,
        "the run index is gone after the undo"
    );
    assert!(
        !columns_of(db.pool(), "llm_call")
            .await
            .contains(&"prompt_version".to_owned()),
        "prompt_version is gone after the undo"
    );

    // Re-run: everything comes back.
    MIGRATOR.run(db.pool()).await.expect("re-run embedded set");
    assert!(object_present(db.pool(), "table", "coaching_sessions").await);
    assert!(object_present(db.pool(), "table", "coaching_proposals").await);
    assert!(
        columns_of(db.pool(), "llm_call")
            .await
            .contains(&"prompt_version".to_owned())
    );
    assert!(applied_versions(db.pool()).await.contains(&5));
}

// ---------------------------------------------------------------------------
// 3. Audit C4 — no persisted validity, anywhere
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_coaching_schema_stores_no_validity() {
    let tmp = TempDir::new().unwrap();
    let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
    MIGRATOR.run(db.pool()).await.unwrap();

    // A mutation's validity is established by `apply()` at use time and is never a
    // stored property (ADR-0021 decision 3). A column that caches the answer is the
    // specific mistake this asserts against.
    for table in ["coaching_sessions", "coaching_proposals"] {
        for column in columns_of(db.pool(), table).await {
            assert!(
                !column.to_lowercase().contains("valid"),
                "{table}.{column} looks like persisted validity, which audit C4 forbids"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. The constraints
// ---------------------------------------------------------------------------

/// A migrated temp db with the FK parents seeded.
async fn seeded_db() -> (TempDir, Db) {
    let tmp = TempDir::new().unwrap();
    let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
    MIGRATOR.run(db.pool()).await.unwrap();
    seed_parents(db.pool()).await;
    (tmp, db)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_most_one_proposal_per_session() {
    let (_tmp, db) = seeded_db().await;
    insert_proposed_session(db.pool(), "sess-1")
        .await
        .expect("seed session");

    insert_proposal(
        db.pool(),
        "prop-1",
        "sess-1",
        "a slower RSI",
        "proposed",
        None,
    )
    .await
    .expect("the first proposal is accepted");

    let second = insert_proposal(
        db.pool(),
        "prop-2",
        "sess-1",
        "a second opinion",
        "proposed",
        None,
    )
    .await;
    assert!(
        second.is_err(),
        "a session may carry at most one proposal — the accept idempotency key"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_disposition_vocabulary_is_enforced() {
    let (_tmp, db) = seeded_db().await;
    insert_proposed_session(db.pool(), "sess-1")
        .await
        .expect("seed session");

    let bogus = insert_proposal(db.pool(), "prop-1", "sess-1", "why", "abandoned", None).await;
    assert!(
        bogus.is_err(),
        "a disposition outside the enumerated set must be rejected"
    );

    for disposition in ["proposed", "rejected", "modified"] {
        let (_tmp, db) = seeded_db().await;
        insert_proposed_session(db.pool(), "sess-1")
            .await
            .expect("seed session");
        insert_proposal(db.pool(), "prop-1", "sess-1", "why", disposition, None)
            .await
            .unwrap_or_else(|e| panic!("`{disposition}` must be accepted: {e}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_version_exists_exactly_when_accepted() {
    let (_tmp, db) = seeded_db().await;
    insert_proposed_session(db.pool(), "sess-1")
        .await
        .expect("seed session");

    // Accepted WITHOUT a child version: no state where an accepted proposal lacks
    // its child (r1.s4's consistency model).
    let orphan_accept =
        insert_proposal(db.pool(), "prop-1", "sess-1", "why", "accepted", None).await;
    assert!(
        orphan_accept.is_err(),
        "an accepted proposal must name its child version"
    );

    // A child version on a proposal that was NOT accepted.
    let stray_child = insert_proposal(
        db.pool(),
        "prop-1",
        "sess-1",
        "why",
        "rejected",
        Some("ver-2"),
    )
    .await;
    assert!(
        stray_child.is_err(),
        "only an accepted proposal may name a child version"
    );

    // The legal pairing. r1.s4.w4 made it a TRIPLE rather than a pair: `0008`
    // requires an accepted proposal to name its child AND that child's
    // re-backtest, so "accepted + child, no run" — the dormant `0005` shape — is
    // now itself refused. `tests/migration_0008.rs` covers that half; here the
    // point is that the legal shape still lands.
    insert_accepted_proposal(db.pool(), "prop-1", "sess-1", "ver-2", "run-child-2")
        .await
        .expect("accepted + child version + its run is the legal shape");
}

/// Insert an ACCEPTED proposal naming both `0008` links (r1.s4.w4).
///
/// Separate from [`insert_proposal`] rather than a seventh parameter on it: every
/// other call site names no run at all, and threading a `None` through ten of them
/// would obscure the one place the accepted shape is the subject.
async fn insert_accepted_proposal(
    pool: &SqlitePool,
    id: &str,
    session_id: &str,
    child_version_id: &str,
    accepted_run_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO coaching_proposals \
         (id, session_id, mutation, hypothesis, disposition, child_version_id, accepted_run_id) \
         VALUES (?1, ?2, '{\"type\":\"set_param\"}', 'why', 'accepted', ?3, ?4)",
    )
    .bind(id)
    .bind(session_id)
    .bind(child_version_id)
    .bind(accepted_run_id)
    .execute(pool)
    .await
    .map(|_| ())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hypothesis_may_not_be_blank_at_the_sql_layer() {
    let (_tmp, db) = seeded_db().await;
    insert_proposed_session(db.pool(), "sess-1")
        .await
        .expect("seed session");

    // SQLite's one-argument `trim()` strips SPACES ONLY, so the tab / newline /
    // carriage-return cases are the ones a naive CHECK lets through — and they are
    // the ones that hurt: the row inserts, and then `Hypothesis::new` (Rust's
    // whitespace-wide `str::trim`) refuses it at READ time, so the session is
    // written once and unreadable forever after.
    for blank in ["", "   ", "\t", "\n", "\r\n", " \t\n ", "\u{b}\u{c}"] {
        let outcome = insert_proposal(db.pool(), "prop-1", "sess-1", blank, "proposed", None).await;
        assert!(
            outcome.is_err(),
            "a blank hypothesis ({blank:?}) must be rejected by the schema too"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hypothesis_check_rejects_every_scalar_rust_calls_whitespace() {
    let (_tmp, db) = seeded_db().await;
    insert_proposed_session(db.pool(), "sess-1")
        .await
        .expect("seed session");

    // The expectation is DERIVED from the toolchain rather than restated as a list:
    // whatever `Hypothesis::new`'s `str::trim` calls whitespace is exactly what this
    // column must refuse. A scalar the CHECK admits and the domain rejects is a row
    // that inserts once and that no typed read can ever return.
    let whitespace: Vec<char> = (0..=0x0010_FFFF)
        .filter_map(char::from_u32)
        .filter(|c| c.is_whitespace())
        .collect();

    // Guard the harness itself: an empty or truncated set would make every
    // assertion below pass while testing nothing.
    assert!(
        whitespace.len() >= 25,
        "the derived whitespace set looks truncated ({} scalars)",
        whitespace.len()
    );
    for probe in ['\u{00a0}', '\u{3000}'] {
        assert!(
            whitespace.contains(&probe),
            "U+{:04X} must be in the derived set",
            probe as u32
        );
    }

    for c in &whitespace {
        let only = c.to_string();
        let outcome =
            insert_proposal(db.pool(), "prop-ws", "sess-1", &only, "proposed", None).await;
        assert!(
            outcome.is_err(),
            "a hypothesis of U+{:04X} alone must be rejected by the schema",
            *c as u32
        );
    }

    // And a string mixing every one of them is just as blank.
    let mixed: String = whitespace.iter().collect();
    let outcome = insert_proposal(db.pool(), "prop-ws", "sess-1", &mixed, "proposed", None).await;
    assert!(
        outcome.is_err(),
        "a hypothesis of nothing but whitespace must be rejected however it is spelled"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hypothesis_carrying_non_whitespace_unicode_is_stored() {
    let (_tmp, db) = seeded_db().await;
    insert_proposed_session(db.pool(), "sess-1")
        .await
        .expect("seed session");

    // The CHECK trims whitespace; it does not ban non-ASCII. `U+200B` is the
    // boundary case in the other direction — a zero-width SPACE that Unicode does
    // NOT give the `White_Space` property, so `str::trim` keeps it and so must the
    // column.
    for text in [
        "RSI(14) は速すぎる",
        "\u{200b}",
        "\u{00a0}the stop is inside the noise\u{00a0}",
        "expectancy → 0.03R",
    ] {
        insert_proposal(db.pool(), "prop-ok", "sess-1", text, "proposed", None)
            .await
            .unwrap_or_else(|e| panic!("a hypothesis of {text:?} must be storable: {e}"));

        // `coaching_proposals.session_id` is UNIQUE, so clear the row before the
        // next sample rather than seeding a session per case.
        sqlx::query("DELETE FROM coaching_proposals WHERE id = 'prop-ok'")
            .execute(db.pool())
            .await
            .expect("clear the proposal row");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_session_carries_its_reason_and_a_proposed_one_does_not() {
    let (_tmp, db) = seeded_db().await;

    // `failed` with no reason — the never-silence guarantee at the SQL layer.
    let silent_failure = sqlx::query(
        "INSERT INTO coaching_sessions \
         (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
          failure_kind, failure_detail, schema_version) \
         VALUES ('sess-x', 'run-1', 'ver-1', '2026-08-29T00:00:00.000Z', NULL, 'failed', \
                 NULL, NULL, 1)",
    )
    .execute(db.pool())
    .await;
    assert!(
        silent_failure.is_err(),
        "a failed session with no recorded reason is exactly the silence this forbids"
    );

    // `proposed` carrying a failure reason — the other half of the iff.
    let confused = sqlx::query(
        "INSERT INTO coaching_sessions \
         (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
          failure_kind, failure_detail, schema_version) \
         VALUES ('sess-y', 'run-1', 'ver-1', '2026-08-29T00:00:00.000Z', 'call-1', 'proposed', \
                 'zero_calls', '{}', 1)",
    )
    .execute(db.pool())
    .await;
    assert!(
        confused.is_err(),
        "a proposed session must not also carry a failure reason"
    );

    // An unknown failure kind is not a state.
    let bogus_kind = sqlx::query(
        "INSERT INTO coaching_sessions \
         (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
          failure_kind, failure_detail, schema_version) \
         VALUES ('sess-z', 'run-1', 'ver-1', '2026-08-29T00:00:00.000Z', NULL, 'failed', \
                 'the_model_was_grumpy', '{}', 1)",
    )
    .execute(db.pool())
    .await;
    assert!(
        bogus_kind.is_err(),
        "a failure kind outside the L3 taxonomy must be rejected"
    );

    // A pre-call failure records NULL llm_call_id (audit C3) and is legal.
    sqlx::query(
        "INSERT INTO coaching_sessions \
         (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
          failure_kind, failure_detail, schema_version) \
         VALUES ('sess-ok', 'run-1', 'ver-1', '2026-08-29T00:00:00.000Z', NULL, 'failed', \
                 'context_overflow', '{\"type\":\"context_overflow\"}', 1)",
    )
    .execute(db.pool())
    .await
    .expect("a pre-call failure with no llm_call row is the audit-C3 shape");
}

// ---------------------------------------------------------------------------
// r1.s2.w4 — the widened failure-kind vocabulary
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_failure_kind_check_admits_transport_failure_and_still_rejects_the_unknown() {
    let (_tmp, db) = seeded_db().await;

    // The seventh kind (operator ruling 2026-08-29): a provider transport fault is
    // a recorded outcome, so the schema has to admit it. `llm_call_id` is NULL —
    // no usable exchange means no priced ledger row (audit C3).
    sqlx::query(
        "INSERT INTO coaching_sessions \
         (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
          failure_kind, failure_detail, schema_version) \
         VALUES ('sess-transport', 'run-1', 'ver-1', '2026-08-29T00:00:00.000Z', NULL, 'failed', \
                 'transport_failure', '{\"type\":\"transport_failure\"}', 1)",
    )
    .execute(db.pool())
    .await
    .expect("the widened CHECK must admit `transport_failure`");

    // Widening the enum must not have turned it into a free-text column: an
    // unknown kind is still not a state.
    let bogus = sqlx::query(
        "INSERT INTO coaching_sessions \
         (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
          failure_kind, failure_detail, schema_version) \
         VALUES ('sess-bogus', 'run-1', 'ver-1', '2026-08-29T00:00:00.000Z', NULL, 'failed', \
                 'the_wifi_was_down', '{}', 1)",
    )
    .execute(db.pool())
    .await;
    assert!(
        bogus.is_err(),
        "a kind outside the taxonomy must still be rejected"
    );

    // And the seventh kind obeys the same never-silence iff as the other six: a
    // `failed` row must carry BOTH its kind and its detail.
    let silent = sqlx::query(
        "INSERT INTO coaching_sessions \
         (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
          failure_kind, failure_detail, schema_version) \
         VALUES ('sess-silent', 'run-1', 'ver-1', '2026-08-29T00:00:00.000Z', NULL, 'failed', \
                 'transport_failure', NULL, 1)",
    )
    .execute(db.pool())
    .await;
    assert!(
        silent.is_err(),
        "a transport failure with no recorded reason is still silence"
    );
}
