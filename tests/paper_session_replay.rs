//! r3.s4.w2 — AC-3: the replay state machine and the atomic batch.
//!
//! State IS the log replayed: `PaperSessionState::replay` folds a scripted
//! log into the same state applying it event by event produces, a prefix
//! replays to the state at that point, and every `ReplayError` case — a
//! `seq` gap or repeat, a non-increasing `bar_processed` `open_time`, any
//! event after `stop`, an `engine_upgraded` whose `old` is not the current
//! epoch — is refused. `engine_upgraded` opens an epoch (E3).
//!
//! The atomic batch (audit #4, as the operator corrected it): an `append_bar`
//! batch carrying bar rows AND a first `stop` (inserted) AND a second `stop`
//! (refused by the schema trigger mid-batch) leaves ZERO of the batch's
//! `paper_event` rows and ZERO of its `paper_bar` rows, while the
//! pre-existing log still reads back and replays.
//!
//! The per-session `MAX(seq)+1` mint for events and bars is asserted here
//! too (disclosed from AC-2: it is the write path's property).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{
    BarRef, Candle, Db, EngineFingerprint, FakeClock, Graduation, NonEmptyLabel, NonEmptyReason,
    Pair, PaperEvent, PaperSession, PaperSessionId, PaperSessionRepository, PaperSessionState,
    PaperSessionStatus, PaperSide, ReplayError, SqlitePaperSessionRepo, Timeframe, VersionId,
};
use rust_decimal::Decimal;
use support::mcp::migrated_db;
use tempfile::TempDir;

const FINGERPRINT_ONE: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const FINGERPRINT_TWO: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const AT: &str = "2026-01-01T00:00:00.000Z";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn session() -> PaperSession {
    PaperSession {
        id: PaperSessionId::new("sess-1".to_owned()),
        seq: 1,
        strategy_version_id: VersionId::new("ver-1"),
        created_at: AT.to_owned(),
        pair: Pair::new("BTCUSDT"),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: None,
        uses_d1: false,
        starting_equity: Decimal::from(10_000),
        taker_fee_bps: Decimal::from(4),
        slippage_bps: Decimal::from(1),
        engine_fingerprint: EngineFingerprint::from_stored(FINGERPRINT_ONE.to_owned()),
        graduation: Graduation::Override {
            reason: NonEmptyReason::try_new("test session").unwrap(),
            at: AT.to_owned(),
        },
        fixture: false,
        min_trades: 20,
        promoted_by: NonEmptyLabel::try_new("operator-token").unwrap(),
    }
}

fn bar(seq: i64, open_time: i64) -> PaperEvent {
    PaperEvent::BarProcessed {
        seq,
        at: AT.to_owned(),
        bars: vec![BarRef {
            timeframe: Timeframe::M15,
            open_time,
        }],
    }
}

fn stop(seq: i64) -> PaperEvent {
    PaperEvent::Stop {
        seq,
        at: AT.to_owned(),
        actor: pulse::StopActor::Token {
            label: NonEmptyLabel::try_new("operator-token").unwrap(),
        },
    }
}

/// The scripted log: a bar, an entry fill, funding, another bar, the exit
/// fill (stop-loss), a data event, an engine upgrade, and the stop.
fn scripted_log() -> Vec<PaperEvent> {
    vec![
        bar(1, 1_000),
        PaperEvent::Fill {
            seq: 2,
            at: AT.to_owned(),
            side: PaperSide::Long,
            qty: Decimal::from(1),
            price: Decimal::from(60_000),
            exit_reason: None,
        },
        PaperEvent::Funding {
            seq: 3,
            at: AT.to_owned(),
            rate: Decimal::new(1, 5),
            amount: Decimal::new(-5, 1),
        },
        bar(4, 2_000),
        PaperEvent::Fill {
            seq: 5,
            at: AT.to_owned(),
            side: PaperSide::Long,
            qty: Decimal::from(1),
            price: Decimal::from(60_100),
            exit_reason: Some(pulse::ExitReason::StopLoss),
        },
        PaperEvent::DataEvent {
            seq: 6,
            at: AT.to_owned(),
            summary: "m15 feed hiccup".to_owned(),
        },
        PaperEvent::EngineUpgraded {
            seq: 7,
            at: AT.to_owned(),
            old: EngineFingerprint::from_stored(FINGERPRINT_ONE.to_owned()),
            new: EngineFingerprint::from_stored(FINGERPRINT_TWO.to_owned()),
        },
        stop(8),
    ]
}

fn fold(session: &PaperSession, log: &[PaperEvent]) -> PaperSessionState {
    let mut state = PaperSessionState::initial(session);
    for event in log {
        state.apply(event).expect("the scripted prefix applies");
    }
    state
}

// ---------------------------------------------------------------------------
// Replay == apply; prefix == state at that point
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replay_equals_event_by_event_apply() {
    let session = session();
    let log = scripted_log();

    let replayed = PaperSessionState::replay(&session, &log).expect("the scripted log replays");
    assert_eq!(replayed, fold(&session, &log), "replay IS the fold");

    // The scripted log's full shape, asserted once for readability.
    assert_eq!(replayed.status, PaperSessionStatus::Stopped);
    assert_eq!(
        replayed.epochs,
        vec![
            EngineFingerprint::from_stored(FINGERPRINT_ONE.to_owned()),
            EngineFingerprint::from_stored(FINGERPRINT_TWO.to_owned()),
        ]
    );
    assert_eq!(replayed.last_bar_open_time, Some(2_000));
    assert_eq!(replayed.closed_trades.len(), 1);
    let trade = &replayed.closed_trades[0];
    assert_eq!(trade.entry_price, Some(Decimal::from(60_000)));
    assert_eq!(trade.exit_price, Decimal::from(60_100));
    assert_eq!(trade.exit_reason, pulse::ExitReason::StopLoss);
    assert!(
        replayed.open_position.is_none(),
        "the exit closed the position"
    );
    assert_eq!(replayed.funding_total, Decimal::new(-5, 1));
    assert_eq!(replayed.data_event_count, 1);
}

#[tokio::test]
async fn prefix_replay_equals_state_at_that_point() {
    let session = session();
    let log = scripted_log();
    for k in 1..=log.len() {
        let prefix = PaperSessionState::replay(&session, &log[..k])
            .unwrap_or_else(|e| panic!("prefix {k} replays: {e}"));
        assert_eq!(
            prefix,
            fold(&session, &log[..k]),
            "prefix {k} is the state at {k}"
        );
    }
}

// ---------------------------------------------------------------------------
// Every ReplayError case
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_replay_refusal_fires() {
    let session = session();

    // A seq gap: 1 then 3.
    let err = PaperSessionState::replay(&session, &[bar(1, 1_000), bar(3, 2_000)])
        .expect_err("a seq gap refuses");
    assert_eq!(
        err,
        ReplayError::SeqOrder {
            expected: 2,
            found: 3
        }
    );

    // A seq repeat: 1 then 1.
    let err = PaperSessionState::replay(&session, &[bar(1, 1_000), bar(1, 2_000)])
        .expect_err("a seq repeat refuses");
    assert_eq!(
        err,
        ReplayError::SeqOrder {
            expected: 2,
            found: 1
        }
    );

    // A non-increasing bar_processed open_time: the same bar, then an older one.
    let err = PaperSessionState::replay(&session, &[bar(1, 1_000), bar(2, 1_000)])
        .expect_err("a repeated bar open_time refuses");
    assert_eq!(
        err,
        ReplayError::BarOpenTimeReversed {
            last: 1_000,
            found: 1_000
        }
    );
    let err = PaperSessionState::replay(&session, &[bar(1, 2_000), bar(2, 1_000)])
        .expect_err("an older bar open_time refuses");
    assert_eq!(
        err,
        ReplayError::BarOpenTimeReversed {
            last: 2_000,
            found: 1_000
        }
    );

    // Any event after stop.
    let err = PaperSessionState::replay(&session, &[stop(1), bar(2, 1_000)])
        .expect_err("an event after stop refuses");
    assert_eq!(err, ReplayError::EventAfterStop);

    // An engine_upgraded whose `old` is not the current epoch.
    let err = PaperSessionState::replay(
        &session,
        &[PaperEvent::EngineUpgraded {
            seq: 1,
            at: AT.to_owned(),
            old: EngineFingerprint::from_stored(FINGERPRINT_TWO.to_owned()),
            new: EngineFingerprint::from_stored(FINGERPRINT_ONE.to_owned()),
        }],
    )
    .expect_err("an epoch mismatch refuses");
    assert_eq!(
        err,
        ReplayError::EpochMismatch {
            current: EngineFingerprint::from_stored(FINGERPRINT_ONE.to_owned()),
            found_old: EngineFingerprint::from_stored(FINGERPRINT_TWO.to_owned()),
        }
    );
}

/// `engine_upgraded` opens an epoch; the session keeps running.
#[tokio::test]
async fn engine_upgrade_opens_an_epoch() {
    let session = session();
    let state = PaperSessionState::replay(
        &session,
        &[PaperEvent::EngineUpgraded {
            seq: 1,
            at: AT.to_owned(),
            old: EngineFingerprint::from_stored(FINGERPRINT_ONE.to_owned()),
            new: EngineFingerprint::from_stored(FINGERPRINT_TWO.to_owned()),
        }],
    )
    .expect("the upgrade applies");
    assert_eq!(state.status, PaperSessionStatus::Running);
    assert_eq!(state.epochs.len(), 2);
    assert_eq!(
        state.epochs[1],
        EngineFingerprint::from_stored(FINGERPRINT_TWO.to_owned())
    );
}

// ---------------------------------------------------------------------------
// The repository: minted seqs, and the atomic mid-batch rollback
// ---------------------------------------------------------------------------

struct World {
    _tmp: TempDir,
    db: Db,
    paper: SqlitePaperSessionRepo<FakeClock>,
}

async fn world_with_session() -> World {
    let tmp = TempDir::new().unwrap();
    let (_path, db) = migrated_db(&tmp).await;
    let pool = db.pool().clone();
    // The strategy/version/run rows the session's FKs + CHECK need.
    sqlx::query("INSERT INTO strategy (id, name, created_at) VALUES ('st-1', 'replay', '2026-01-01T00:00:00.000Z')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO strategy_version (id, strategy_id, dsl_schema_version, dsl, dsl_original, \
         version_hash, created_by, creating_llm_call_ids, created_at) \
         VALUES ('ver-1', 'st-1', '1.2.0', '{}', '{}', 'hash', 'human', '[]', '2026-01-01T00:00:00.000Z')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO walk_forward_run (id, seq, strategy_version_id, created_at, scheme, rule, k, \
         span_from_ms, span_to_ms, from_defaulted, engine_fingerprint, folds_holding, \
         folds_required, pooled_n, pooled_mean_r, pooled_lower_bound, pass) \
         VALUES ('run-1', 1, 'ver-1', '2026-01-01T00:00:00.000Z', 'rolling-oos/v1', 'wf-v1', 6, \
                 0, 1, 0, 'fp', 6, 4, 100, '0.5', 0.1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO paper_session \
         (id, seq, strategy_version_id, created_at, pair, primary_timeframe, \
          htf_timeframe, uses_d1, starting_equity, taker_fee_bps, slippage_bps, \
          engine_fingerprint, graduation, walk_forward_run_id, override_reason, \
          override_at, certified_data_versions, fixture, min_trades, promoted_by) \
         VALUES ('sess-1', 1, 'ver-1', '2026-01-01T00:00:00.000Z', 'BTCUSDT', '15m', NULL, 0, \
                 '10000', '4', '1', 'fp', 'certified', 'run-1', NULL, NULL, \
                 '[{\"timeframe\":\"15m\",\"data_version\":\"a\"}]', 1, 20, 'operator-token')",
    )
    .execute(&pool)
    .await
    .unwrap();
    World {
        _tmp: tmp,
        db,
        paper: SqlitePaperSessionRepo::with_clock(pool, FakeClock::at(1_767_225_600_000)),
    }
}

fn candle(open_time: i64) -> Candle {
    Candle {
        open_time,
        close_time: open_time + 899_999,
        open: Decimal::from(60_000),
        high: Decimal::from(60_100),
        low: Decimal::from(59_900),
        close: Decimal::from(60_050),
        volume: Decimal::from(100),
        funding_rate: None,
    }
}

async fn count(pool: &sqlx::SqlitePool, table: &str, session_filter: bool) -> i64 {
    let sql = format!(
        "SELECT COUNT(*) FROM {table}{}",
        if session_filter {
            " WHERE session_id = 'sess-1'"
        } else {
            ""
        }
    );
    sqlx::query_scalar(&sql).fetch_one(pool).await.unwrap()
}

/// Happy path: bars + events persist in one batch; `seq` mints `MAX+1` per
/// session on both tables; the returned events carry the minted sequences.
#[tokio::test]
async fn append_bar_persists_and_mints_per_session() {
    let world = world_with_session().await;
    let session_id = PaperSessionId::new("sess-1".to_owned());

    let first = world
        .paper
        .append_bar(
            &session_id,
            &[(Timeframe::M15, candle(1_000), true)],
            &[bar(0, 1_000)],
        )
        .await
        .expect("the first batch persists");
    assert_eq!(
        first.iter().map(PaperEvent::seq).collect::<Vec<_>>(),
        vec![1],
        "the batch's events are re-keyed to the minted sequences"
    );

    // A second batch on the SAME session continues the per-session sequence…
    world
        .paper
        .append_bar(
            &session_id,
            &[(Timeframe::M15, candle(2_000), false)],
            &[bar(0, 2_000)],
        )
        .await
        .expect("the second batch persists");
    // …and a third carries the stop (nothing may follow it).
    world
        .paper
        .append_bar(&session_id, &[], &[stop(0)])
        .await
        .expect("the stop persists");
    let seqs: Vec<i64> =
        sqlx::query_scalar("SELECT seq FROM paper_event WHERE session_id = 'sess-1' ORDER BY seq")
            .fetch_all(world.db.pool())
            .await
            .unwrap();
    assert_eq!(seqs, vec![1, 2, 3], "events mint MAX+1 per session");
    let bar_seqs: Vec<i64> =
        sqlx::query_scalar("SELECT seq FROM paper_bar WHERE session_id = 'sess-1' ORDER BY seq")
            .fetch_all(world.db.pool())
            .await
            .unwrap();
    assert_eq!(bar_seqs, vec![1, 2], "bars mint MAX+1 per session");

    // A SECOND session's sequence starts from its own 1.
    sqlx::query(
        "INSERT INTO paper_session \
         (id, seq, strategy_version_id, created_at, pair, primary_timeframe, \
          htf_timeframe, uses_d1, starting_equity, taker_fee_bps, slippage_bps, \
          engine_fingerprint, graduation, walk_forward_run_id, override_reason, \
          override_at, certified_data_versions, fixture, min_trades, promoted_by) \
         VALUES ('sess-2', 2, 'ver-1', '2026-01-01T00:00:00.000Z', 'BTCUSDT', '15m', NULL, 0, \
                 '10000', '4', '1', 'fp', 'certified', 'run-1', NULL, NULL, \
                 '[{\"timeframe\":\"15m\",\"data_version\":\"a\"}]', 1, 20, 'operator-token')",
    )
    .execute(world.db.pool())
    .await
    .unwrap();
    world
        .paper
        .append_bar(
            &PaperSessionId::new("sess-2".to_owned()),
            &[(Timeframe::M15, candle(1_000), false)],
            &[bar(0, 1_000)],
        )
        .await
        .expect("the other session's batch persists");
    let other: i64 = sqlx::query_scalar("SELECT seq FROM paper_event WHERE session_id = 'sess-2'")
        .fetch_one(world.db.pool())
        .await
        .unwrap();
    assert_eq!(other, 1, "the mint is per session, not global");

    // The stored payloads agree with the minted columns (decode round-trip),
    // and the whole log — ending in its stop — replays to `stopped`.
    let log = world
        .paper
        .events(&session_id)
        .await
        .expect("the log reads back");
    assert_eq!(log.iter().map(PaperEvent::seq).collect::<Vec<_>>(), seqs);
    let state = PaperSessionState::replay(&session(), &log).expect("the stored log replays");
    assert_eq!(
        state.status,
        PaperSessionStatus::Stopped,
        "the stop is in the log"
    );
}

/// The atomic mid-batch proof (audit #4, as corrected): a batch whose bar
/// rows AND first `stop` event inserted, whose SECOND `stop` the trigger
/// refuses, rolls back BOTH tables — and the pre-existing log still reads
/// back and replays.
#[tokio::test]
async fn mid_batch_refusal_rolls_back_bars_and_events() {
    let world = world_with_session().await;
    let session_id = PaperSessionId::new("sess-1".to_owned());

    // The pre-existing log: one bar_processed.
    world
        .paper
        .append_bar(
            &session_id,
            &[(Timeframe::M15, candle(1_000), true)],
            &[bar(0, 1_000)],
        )
        .await
        .expect("the pre-existing batch persists");

    // The failing batch: a bar row, a first stop (fine), a second stop
    // (refused — the session already has a stop). The refusal fires MID-batch,
    // after the bar row and the first event have inserted.
    let err = world
        .paper
        .append_bar(
            &session_id,
            &[(Timeframe::M15, candle(2_000), false)],
            &[stop(0), stop(0)],
        )
        .await
        .expect_err("the second stop aborts the batch");
    assert!(
        format!("{err}").contains("stopped"),
        "the refusal names the read-only law: {err}"
    );

    // ZERO rows from the failed batch, on BOTH tables; the pre-existing rows
    // survive untouched.
    assert_eq!(
        count(world.db.pool(), "paper_event", true).await,
        1,
        "only the pre-existing event remains"
    );
    let batch_bar: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM paper_bar WHERE session_id = 'sess-1' AND open_time = 2000",
    )
    .fetch_one(world.db.pool())
    .await
    .unwrap();
    assert_eq!(batch_bar, 0, "the batch's bar row rolled back");
    let preexisting_bar: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM paper_bar WHERE session_id = 'sess-1' AND open_time = 1000",
    )
    .fetch_one(world.db.pool())
    .await
    .unwrap();
    assert_eq!(preexisting_bar, 1, "the pre-existing bar row is untouched");

    // The pre-existing log reads back and replays.
    let log = world
        .paper
        .events(&session_id)
        .await
        .expect("the log reads back");
    assert_eq!(log.len(), 1);
    let state = PaperSessionState::replay(&session(), &log).expect("the log replays");
    assert_eq!(state.status, PaperSessionStatus::Running);
    assert_eq!(state.last_bar_open_time, Some(1_000));
}

/// A4, as the coordinator confirmed: once a `stop` exists the session is
/// read-only — an `append_bar` carrying BAR ROWS and an EMPTY event batch
/// must refuse too. (The event half was already walled by the schema
/// trigger; the bar rows alone used to slip past it.)
#[tokio::test]
async fn stopped_session_refuses_bar_appends_even_with_empty_events() {
    let world = world_with_session().await;
    let session_id = PaperSessionId::new("sess-1".to_owned());

    // One pre-stop bar, then the stop.
    world
        .paper
        .append_bar(
            &session_id,
            &[(Timeframe::M15, candle(1_000), true)],
            &[bar(0, 1_000)],
        )
        .await
        .expect("the pre-stop batch persists");
    world
        .paper
        .append_bar(&session_id, &[], &[stop(0)])
        .await
        .expect("the stop persists");

    // Bars with NO events on the stopped session: refused, nothing written.
    let err = world
        .paper
        .append_bar(&session_id, &[(Timeframe::M15, candle(2_000), false)], &[])
        .await
        .expect_err("a stopped session is read-only even for bare bars");
    assert!(
        format!("{err}").contains("stopped"),
        "the refusal names the read-only law: {err}"
    );
    assert_eq!(
        count(world.db.pool(), "paper_bar", true).await,
        1,
        "only the pre-stop bar remains"
    );
    assert_eq!(
        count(world.db.pool(), "paper_event", true).await,
        2,
        "the log is untouched (the bar_processed and the stop)"
    );
}
