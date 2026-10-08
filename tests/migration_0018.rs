//! r3.s4.w2 — AC-2: the `0018_paper_sessions` schema law.
//!
//! The paper session's durability lives in the SCHEMA first: rows are
//! immutable by trigger (`paper_session`) or append-only (`paper_event`,
//! `paper_bar`, `fixture_snapshot`), a stopped session's log is read-only
//! (A4), `(session_id, seq)` and `(session_id, timeframe, open_time)` are
//! unique, the graduation is a sum whose variants name exactly their own
//! columns, and `promoted_by` is never blank (audit #6). Everything here
//! drives raw SQL against a real migrated database, the way a hostile or
//! buggy writer would — plus the one refusal the schema cannot express (an
//! unlabeled `stop` actor), which the DOMAIN type refuses at decode.
//!
//! The down migration is the 0013/0017 pattern: it REFUSES (transactionally,
//! naming the blocking table) while any of the four tables holds a row, and
//! round-trips cleanly when they are all empty.
//!
//! Disclosed scope note: the per-session `MAX(seq)+1` MINT for
//! `paper_event`/`paper_bar` is a property of the repository's write path
//! (`append_bar`, AC-3), not of the schema — this file pins the schema-level
//! uniqueness the mint relies on, and AC-3's replay suite asserts the minted
//! sequences themselves.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{Db, MIGRATOR, PaperEvent, undo_to};
use support::mcp::migrated_db;
use tempfile::TempDir;

/// A valid `paper_session` row's INSERT, parameterized on the pieces the
/// CHECKs discriminate.
const SESSION_INSERT: &str = "INSERT INTO paper_session \
     (id, seq, strategy_version_id, created_at, pair, primary_timeframe, \
      htf_timeframe, uses_d1, starting_equity, taker_fee_bps, slippage_bps, \
      engine_fingerprint, graduation, walk_forward_run_id, override_reason, \
      override_at, certified_data_versions, fixture, min_trades, promoted_by) \
     VALUES (?1, ?2, ?3, '2026-01-01T00:00:00.000Z', 'BTCUSDT', '15m', NULL, 0, \
             '10000', '4', '1', 'fp', ?4, ?5, ?6, ?7, ?8, 0, 20, 'operator-token')";

/// Insert a valid certified session (`?1`=id, `?2`=version).
async fn seed_session(pool: &sqlx::SqlitePool, id: &str, version_id: &str, run_id: &str) {
    sqlx::query(SESSION_INSERT)
        .bind(id)
        .bind(1_i64)
        .bind(version_id)
        .bind("certified")
        .bind(run_id)
        .bind(None::<String>)
        .bind(None::<String>)
        .bind(r#"[{"timeframe":"15m","data_version":"aaaa"}]"#)
        .execute(pool)
        .await
        .unwrap();
}

async fn world() -> (TempDir, sqlx::SqlitePool) {
    let tmp = TempDir::new().unwrap();
    let (_path, db) = migrated_db(&tmp).await;
    (tmp, db.pool().clone())
}

/// A minimal `strategy` + `strategy_version` pair to satisfy the FKs.
async fn seed_version(pool: &sqlx::SqlitePool, strategy_name: &str, version_id: &str) {
    sqlx::query(
        "INSERT INTO strategy (id, name, created_at) VALUES (?1, ?2, '2026-01-01T00:00:00.000Z')",
    )
    .bind(format!("st-{strategy_name}"))
    .bind(strategy_name)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO strategy_version (id, strategy_id, dsl_schema_version, dsl, dsl_original, \
         version_hash, created_by, creating_llm_call_ids, created_at) \
         VALUES (?1, ?2, '1.2.0', '{}', '{}', 'hash', 'human', '[]', '2026-01-01T00:00:00.000Z')",
    )
    .bind(version_id)
    .bind(format!("st-{strategy_name}"))
    .execute(pool)
    .await
    .unwrap();
}

/// A walk-forward run row for certified sessions to name.
async fn seed_walk_forward_run(pool: &sqlx::SqlitePool, run_id: &str, version_id: &str) {
    sqlx::query(
        "INSERT INTO walk_forward_run (id, seq, strategy_version_id, created_at, scheme, rule, k, \
         span_from_ms, span_to_ms, from_defaulted, engine_fingerprint, folds_holding, \
         folds_required, pooled_n, pooled_mean_r, pooled_lower_bound, pass) \
         VALUES (?1, 1, ?2, '2026-01-01T00:00:00.000Z', 'rolling-oos/v1', 'wf-v1', 6, \
                 0, 1, 0, 'fp', 6, 4, 100, '0.5', 0.1, 1)",
    )
    .bind(run_id)
    .bind(version_id)
    .execute(pool)
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// 1. Immutable / append-only by trigger, on all four tables.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_and_delete_abort_on_all_four_tables() {
    let (_tmp, pool) = world().await;
    seed_version(&pool, "immutable", "ver-1").await;
    seed_walk_forward_run(&pool, "run-1", "ver-1").await;
    seed_session(&pool, "sess-1", "ver-1", "run-1").await;
    sqlx::query(
        "INSERT INTO paper_event (session_id, seq, at, kind, payload) \
         VALUES ('sess-1', 1, '2026-01-01T00:00:01.000Z', 'order', '{}')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO paper_bar (session_id, timeframe, seq, open_time, close_time, open, high, \
         low, close, volume, funding_rate, lead_in) \
         VALUES ('sess-1', '15m', 1, 1000, 1999, '100', '101', '99', '100', '5', NULL, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO fixture_snapshot (pair, timeframe, data_version, created_at) \
         VALUES ('BTCUSDT', '15m', 'aaaa', '2026-01-01T00:00:00.000Z')",
    )
    .execute(&pool)
    .await
    .unwrap();

    for (table, update, delete) in [
        (
            "paper_session",
            "UPDATE paper_session SET promoted_by = 'x' WHERE id = 'sess-1'",
            "DELETE FROM paper_session WHERE id = 'sess-1'",
        ),
        (
            "paper_event",
            "UPDATE paper_event SET at = 'x' WHERE session_id = 'sess-1'",
            "DELETE FROM paper_event WHERE session_id = 'sess-1'",
        ),
        (
            "paper_bar",
            "UPDATE paper_bar SET close = 'x' WHERE session_id = 'sess-1'",
            "DELETE FROM paper_bar WHERE session_id = 'sess-1'",
        ),
        (
            "fixture_snapshot",
            "UPDATE fixture_snapshot SET created_at = 'x' WHERE data_version = 'aaaa'",
            "DELETE FROM fixture_snapshot WHERE data_version = 'aaaa'",
        ),
    ] {
        let err = sqlx::query(update)
            .execute(&pool)
            .await
            .expect_err(&format!("{table} update aborts"));
        assert!(
            format!("{err}").contains("immutable") || format!("{err}").contains("append-only"),
            "{table} update refusal names the law: {err}"
        );
        let err = sqlx::query(delete)
            .execute(&pool)
            .await
            .expect_err(&format!("{table} delete aborts"));
        assert!(
            format!("{err}").contains("immutable") || format!("{err}").contains("append-only"),
            "{table} delete refusal names the law: {err}"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. A stopped session's log is read-only (A4).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn insert_after_stop_aborts() {
    let (_tmp, pool) = world().await;
    seed_version(&pool, "stopped", "ver-1").await;
    seed_walk_forward_run(&pool, "run-1", "ver-1").await;
    seed_session(&pool, "sess-1", "ver-1", "run-1").await;
    let stop_payload = r#"{"type":"stop","seq":1,"at":"2026-01-01T00:00:02.000Z","actor":{"token":{"label":"operator-token"}}}"#;
    sqlx::query(
        "INSERT INTO paper_event (session_id, seq, at, kind, payload) \
         VALUES ('sess-1', 1, '2026-01-01T00:00:02.000Z', 'stop', ?1)",
    )
    .bind(stop_payload)
    .execute(&pool)
    .await
    .unwrap();

    let err = sqlx::query(
        "INSERT INTO paper_event (session_id, seq, at, kind, payload) \
         VALUES ('sess-1', 2, '2026-01-01T00:00:03.000Z', 'order', '{}')",
    )
    .execute(&pool)
    .await
    .expect_err("nothing appends after stop");
    assert!(
        format!("{err}").contains("stopped"),
        "the refusal names the read-only law: {err}"
    );
}

/// The read-only law covers the RECORDED BARS too (A4, coordinator-confirmed):
/// once a `stop` exists, even a raw `paper_bar` insert aborts — the same wall
/// the event log has, so no write path (the sanctioned `append_bar` least of
/// all) can add a bar to a stopped session.
#[tokio::test]
async fn insert_bar_after_stop_aborts() {
    let (_tmp, pool) = world().await;
    seed_version(&pool, "stopped", "ver-1").await;
    seed_walk_forward_run(&pool, "run-1", "ver-1").await;
    seed_session(&pool, "sess-1", "ver-1", "run-1").await;
    let stop_payload = r#"{"type":"stop","seq":1,"at":"2026-01-01T00:00:02.000Z","actor":{"token":{"label":"operator-token"}}}"#;
    sqlx::query(
        "INSERT INTO paper_event (session_id, seq, at, kind, payload) \
         VALUES ('sess-1', 1, '2026-01-01T00:00:02.000Z', 'stop', ?1)",
    )
    .bind(stop_payload)
    .execute(&pool)
    .await
    .unwrap();

    let err = sqlx::query(
        "INSERT INTO paper_bar (session_id, timeframe, seq, open_time, close_time, open, high, \
         low, close, volume, funding_rate, lead_in) \
         VALUES ('sess-1', '15m', 1, 1000, 1999, '100', '101', '99', '100', '5', NULL, 1)",
    )
    .execute(&pool)
    .await
    .expect_err("no bar lands on a stopped session");
    assert!(
        format!("{err}").contains("stopped"),
        "the refusal names the read-only law: {err}"
    );
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM paper_bar WHERE session_id = 'sess-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining, 0, "the refused bar wrote nothing");
}

// ---------------------------------------------------------------------------
// 3. Per-session uniqueness the mint relies on; `shadow_checked` is a kind.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn uniqueness_and_kind_vocabulary() {
    let (_tmp, pool) = world().await;
    seed_version(&pool, "unique", "ver-1").await;
    seed_walk_forward_run(&pool, "run-1", "ver-1").await;
    seed_session(&pool, "sess-1", "ver-1", "run-1").await;

    sqlx::query(
        "INSERT INTO paper_event (session_id, seq, at, kind, payload) \
         VALUES ('sess-1', 1, '2026-01-01T00:00:01.000Z', 'shadow_checked', '{}')",
    )
    .execute(&pool)
    .await
    .unwrap();
    // (session_id, seq) is unique — a second event at the same seq refuses…
    let err = sqlx::query(
        "INSERT INTO paper_event (session_id, seq, at, kind, payload) \
         VALUES ('sess-1', 1, '2026-01-01T00:00:02.000Z', 'order', '{}')",
    )
    .execute(&pool)
    .await
    .expect_err("duplicate (session_id, seq) refuses");
    assert!(format!("{err}").contains("UNIQUE"), "{err}");
    // …while the SAME seq on ANOTHER session is fine (per-session, not global).
    sqlx::query(SESSION_INSERT)
        .bind("sess-2")
        .bind(2_i64) // `paper_session.seq` is globally unique; the per-session law is on the CHILD tables
        .bind("ver-1")
        .bind("certified")
        .bind("run-1")
        .bind(None::<String>)
        .bind(None::<String>)
        .bind(r#"[{"timeframe":"15m","data_version":"aaaa"}]"#)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO paper_event (session_id, seq, at, kind, payload) \
         VALUES ('sess-2', 1, '2026-01-01T00:00:01.000Z', 'order', '{}')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // (session_id, timeframe, open_time) is unique — the same bar twice refuses.
    for _ in 0..2 {
        let result = sqlx::query(
            "INSERT INTO paper_bar (session_id, timeframe, seq, open_time, close_time, open, \
             high, low, close, volume, funding_rate, lead_in) \
             VALUES ('sess-1', '15m', 1, 1000, 1999, '100', '101', '99', '100', '5', NULL, 1)",
        )
        .execute(&pool)
        .await;
        if let Err(err) = result {
            assert!(format!("{err}").contains("UNIQUE"), "{err}");
        }
    }
    let bar_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM paper_bar WHERE session_id = 'sess-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        bar_count, 1,
        "the duplicate bar row was refused, not upserted"
    );

    // An unknown kind refuses.
    let err = sqlx::query(
        "INSERT INTO paper_event (session_id, seq, at, kind, payload) \
         VALUES ('sess-1', 9, '2026-01-01T00:00:09.000Z', 'vibes', '{}')",
    )
    .execute(&pool)
    .await
    .expect_err("an unknown kind refuses");
    assert!(format!("{err}").contains("CHECK"), "{err}");
}

// ---------------------------------------------------------------------------
// 4. The graduation CHECKs hold.
// ---------------------------------------------------------------------------

/// One malformed `paper_session` insert; the graduation CHECK must refuse it.
async fn refused_graduation_row(
    pool: &sqlx::SqlitePool,
    id: &str,
    graduation: &str,
    run_id: Option<&str>,
    override_reason: Option<&str>,
    override_at: Option<&str>,
    versions: &str,
) {
    let err = sqlx::query(SESSION_INSERT)
        .bind(id)
        .bind(1_i64)
        .bind("ver-1")
        .bind(graduation)
        .bind(run_id)
        .bind(override_reason)
        .bind(override_at)
        .bind(versions)
        .execute(pool)
        .await
        .expect_err("a malformed graduation row refuses");
    assert!(format!("{err}").contains("CHECK"), "{id}: {err}");
}

#[tokio::test]
async fn graduation_checks_hold() {
    let (_tmp, pool) = world().await;
    seed_version(&pool, "graduation", "ver-1").await;
    seed_walk_forward_run(&pool, "run-1", "ver-1").await;
    let at = "2026-01-01T00:00:00.000Z";
    let some_versions = r#"[{"timeframe":"15m","data_version":"a"}]"#;

    // Every malformed shape refuses. `certified` demands a run id, a
    // non-empty version list and no override fields; `override` demands no
    // run id, a non-empty reason, an instant, and no versions.
    for (graduation, name, run_id, reason, override_at, versions) in [
        ("certified", "no run id", None, None, None, some_versions),
        (
            "certified",
            "empty versions",
            Some("run-1"),
            None,
            None,
            "[]",
        ),
        (
            "certified",
            "carries a reason",
            Some("run-1"),
            Some("because"),
            None,
            some_versions,
        ),
        (
            "override",
            "with run id",
            Some("run-1"),
            Some("because"),
            Some(at),
            "[]",
        ),
        ("override", "without reason", None, None, Some(at), "[]"),
        (
            "override",
            "blank reason",
            None,
            Some("   "),
            Some(at),
            "[]",
        ),
        ("override", "without at", None, Some("because"), None, "[]"),
        (
            "override",
            "with versions",
            None,
            Some("because"),
            Some(at),
            some_versions,
        ),
    ] {
        refused_graduation_row(
            &pool,
            &format!("sess-{graduation}-{name}"),
            graduation,
            run_id,
            reason,
            override_at,
            versions,
        )
        .await;
    }

    // The two well-formed shapes accept.
    seed_session(&pool, "sess-ok-certified", "ver-1", "run-1").await;
    sqlx::query(SESSION_INSERT)
        .bind("sess-ok-override")
        .bind(2_i64)
        .bind("ver-1")
        .bind("override")
        .bind(None::<String>)
        .bind("because")
        .bind(at)
        .bind("[]")
        .execute(&pool)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 5. `promoted_by` is never blank (audit #6).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn promoted_by_check_refuses_blank() {
    let (_tmp, pool) = world().await;
    seed_version(&pool, "promoted-by", "ver-1").await;
    seed_walk_forward_run(&pool, "run-1", "ver-1").await;
    for blank in ["", "   ", "\t\n"] {
        let err = sqlx::query(
            "INSERT INTO paper_session \
             (id, seq, strategy_version_id, created_at, pair, primary_timeframe, \
              htf_timeframe, uses_d1, starting_equity, taker_fee_bps, slippage_bps, \
              engine_fingerprint, graduation, walk_forward_run_id, override_reason, \
              override_at, certified_data_versions, fixture, min_trades, promoted_by) \
             VALUES ('s', 1, ?1, '2026-01-01T00:00:00.000Z', 'BTCUSDT', '15m', NULL, 0, \
                     '10000', '4', '1', 'fp', 'certified', 'run-1', NULL, NULL, \
                     '[{\"timeframe\":\"15m\",\"data_version\":\"a\"}]', 0, 20, ?2)",
        )
        .bind("ver-1")
        .bind(blank)
        .execute(&pool)
        .await
        .expect_err("a blank promoted_by refuses");
        assert!(format!("{err}").contains("promoted_by"), "{blank:?}: {err}");
    }
}

// ---------------------------------------------------------------------------
// 6. The one refusal the schema cannot express: an unlabeled `stop` actor is
//    refused by the DOMAIN type at decode (audit #6).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unlabeled_stop_refused_by_domain_type() {
    // An empty label fails NonEmptyText's serde try_from…
    let empty =
        r#"{"type":"stop","seq":1,"at":"2026-01-01T00:00:00.000Z","actor":{"token":{"label":""}}}"#;
    assert!(PaperEvent::decode("stop", empty).is_err());
    // …a whitespace label too…
    let blank = r#"{"type":"stop","seq":1,"at":"2026-01-01T00:00:00.000Z","actor":{"token":{"label":"   "}}}"#;
    assert!(PaperEvent::decode("stop", blank).is_err());
    // …and a stop_all sweep without its issuer.
    let no_issuer = r#"{"type":"stop","seq":1,"at":"2026-01-01T00:00:00.000Z","actor":{"stop_all":{"issuer":""}}}"#;
    assert!(PaperEvent::decode("stop", no_issuer).is_err());

    // A LABELED stop decodes, and its round-trip keeps the label.
    let labeled = r#"{"type":"stop","seq":1,"at":"2026-01-01T00:00:00.000Z","actor":{"token":{"label":"operator-token"}}}"#;
    let event = PaperEvent::decode("stop", labeled).expect("a labeled stop decodes");
    assert_eq!(event.kind(), "stop");
    assert_eq!(event.seq(), 1);

    // A payload whose internal tag disagrees with the row's kind refuses.
    let err = PaperEvent::decode("order", labeled).expect_err("a lying row refuses");
    assert!(matches!(
        err,
        pulse::PaperEventDecodeError::KindMismatch { .. }
    ));
}

// ---------------------------------------------------------------------------
// 7. The down migration: refuses with rows present, round-trips empty.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn down_refuses_with_rows_present() {
    let (tmp, pool) = world().await;
    seed_version(&pool, "down-guard", "ver-1").await;
    seed_walk_forward_run(&pool, "run-1", "ver-1").await;
    seed_session(&pool, "sess-1", "ver-1", "run-1").await;

    drop(pool);
    let path = tmp.path().join("pulse.db");
    let db = Db::with_path(&path).await.unwrap();
    let err = undo_to(db.pool(), 17)
        .await
        .expect_err("0018 down refuses over a session row");
    let message = format!("{err}");
    assert!(
        message.contains("0018") && message.contains("refus"),
        "the refusal names the migration and the state: {message}"
    );
    // The refusal is transactional: the schema is still at 18 with the row.
    let max: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(max, 18, "the refused down left the schema at 0018");
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM paper_session")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(sessions, 1, "the refused down left the row in place");
}

#[tokio::test]
async fn down_round_trips_when_empty() {
    let (tmp, pool) = world().await;
    drop(pool);
    let path = tmp.path().join("pulse.db");
    let db = Db::with_path(&path).await.unwrap();

    // Empty: the down restores 0017 exactly.
    undo_to(db.pool(), 17)
        .await
        .expect("empty 0018 downgrades losslessly");
    for object in [
        "paper_session",
        "paper_event",
        "paper_bar",
        "fixture_snapshot",
    ] {
        let present: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        )
        .bind(object)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(present, 0, "{object} is gone after the down");
    }

    // And the re-run brings it back.
    MIGRATOR
        .run(db.pool())
        .await
        .expect("re-run to embedded max");
    let max: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(max, 20, "the re-run restores the embedded max");
    let present: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'paper_session'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(present, 1, "paper_session is back");
}
