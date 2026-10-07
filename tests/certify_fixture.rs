//! r3.s4.w2 — AC-4: the certify fixture (`pulse fixture seed`'s substance).
//!
//! The fixture is a deterministic synthetic BTCUSDT M15 + H4 series (a pinned
//! `SplitMix64` mean-reverting walk) plus a plain dip-buy strategy minted from
//! the generator's own constants. Its whole job (spec §1, E2): the seeded
//! walk-forward on THIS build must be a genuinely passing wf-v1 — six real
//! holding folds of ≥20 trades each — so a fixture-certified promotion is a
//! real gate pass and a foreign-fingerprint refusal (E2) is a real re-cert
//! demand.
//!
//! Case (i) here is the existence proof (the handoff clarification's one
//! allowed GREEN start): it drives the REAL `run_walk_forward` use case with
//! the series pinned through `SnapshotPins` — never HEAD, never a synthetic
//! verdict — on the current build. Cases (ii)–(iv) (generator data versions,
//! seed idempotency, foreign-fingerprint re-seed) are added by the AC-4 loop.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)] // the case bodies assert whole scenarios

mod support;

use pulse::{
    BacktestConfig, BinanceAdapter, CandleSeries, CandleStore, CreatedBy, FoldScheme, NewVersion,
    SnapshotPins, SqliteBacktestRunRepo, SqliteStrategyRepo, StrategyRepository, Timeframe,
    VerdictRule, WalkForwardRequest, fixture_h4_candles, fixture_m15_candles, fixture_pair,
    fixture_strategy_dsl, run_walk_forward,
};
use rust_decimal::Decimal;
use support::mcp::migrated_db;
use tempfile::TempDir;

/// AC-4 case (i): the fixture's own walk-forward is a real wf-v1 pass — six
/// folds, every fold `n >= 20` and holding, the run passing on the CURRENT
/// build's engine fingerprint. The series ride `SnapshotPins`, so the proof
/// never reads HEAD and never fabricates a verdict.
/// Stamp both fixture series into the store (snapshots only, never HEAD),
/// then mint the fixture strategy and its version — the seed's ordered
/// steps 1 and 3 as one reusable piece.
async fn seed_snapshots_and_strategy(
    store: &CandleStore,
    strategies: &SqliteStrategyRepo<pulse::SystemClock>,
) -> (pulse::VersionId, pulse::DataVersion, pulse::DataVersion) {
    let pair = fixture_pair();
    let m15 = fixture_m15_candles();
    let h4 = fixture_h4_candles();
    let m15_version = CandleStore::content_version(&pair, Timeframe::M15, &m15);
    let h4_version = CandleStore::content_version(&pair, Timeframe::H4, &h4);
    store
        .write_snapshot(&CandleSeries {
            pair: pair.clone(),
            timeframe: Timeframe::M15,
            version: m15_version.clone(),
            candles: m15,
        })
        .unwrap();
    store
        .write_snapshot(&CandleSeries {
            pair: pair.clone(),
            timeframe: Timeframe::H4,
            version: h4_version.clone(),
            candles: h4,
        })
        .unwrap();
    let strategy = strategies
        .create_strategy(
            pulse::FIXTURE_STRATEGY_NAME,
            None,
            &[pulse::FIXTURE_STRATEGY_TAG.to_owned()],
        )
        .await
        .unwrap();
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(&fixture_strategy_dsl()).unwrap(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .unwrap();
    (version.id, m15_version, h4_version)
}

/// The pinned fixture walk-forward request (K default, snapshots pinned —
/// never HEAD).
fn fixture_walk_forward_request(
    version_id: pulse::VersionId,
    m15_version: pulse::DataVersion,
    h4_version: pulse::DataVersion,
) -> WalkForwardRequest {
    WalkForwardRequest {
        version_id,
        pair: fixture_pair(),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: Some(Timeframe::H4),
        config: BacktestConfig::default(),
        snapshots: Some(SnapshotPins {
            primary: m15_version,
            htf: Some(h4_version),
            d1: None,
        }),
        from_ms: None,
        to_ms: None,
        k: None,
        rule: None,
    }
}

#[tokio::test]
async fn i_fixture_walk_forward_is_a_real_wf_v1_pass_on_this_build() {
    let tmp = TempDir::new().unwrap();
    let (_path, db) = migrated_db(&tmp).await;
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let store = CandleStore::with_base_dir(tmp.path().join("candles"));

    let (version_id, m15_version, h4_version) =
        seed_snapshots_and_strategy(&store, &strategies).await;
    let outcome = run_walk_forward(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &fixture_walk_forward_request(version_id, m15_version, h4_version),
    )
    .await
    .expect("the fixture walk-forward runs on this build");

    let run = &outcome.run;
    assert_eq!(
        run.rule,
        VerdictRule::WfV1,
        "the fixture is judged by wf-v1, unchanged"
    );
    assert_eq!(
        run.scheme,
        FoldScheme::RollingOos { k: 6 },
        "K stays wf-v1's 6"
    );
    assert_eq!(run.folds.len(), 6);
    assert!(
        run.verdict.pass,
        "the fixture walk-forward must genuinely pass wf-v1; folds: {:?}",
        run.folds
            .iter()
            .map(|f| (f.verdict.n, f.verdict.mean_r, f.verdict.lower_bound))
            .collect::<Vec<_>>()
    );
    for fold in &run.folds {
        assert!(
            fold.verdict.holds,
            "every fold must hold wf-v1 (n >= 20, lower bound > 0); got {:?}",
            fold.verdict
        );
        assert!(
            fold.verdict.n >= 20,
            "every fold must trade at least N_MIN=20 times; got {}",
            fold.verdict.n
        );
    }

    // Evidence for the work-item report (seed, length, strategy, per-fold stats).
    let fold_evidence = run
        .folds
        .iter()
        .map(|f| {
            format!(
                "fold {}: n={} mean_r={:.4} lower_bound={:.6} holds={}",
                f.index, f.verdict.n, f.verdict.mean_r, f.verdict.lower_bound, f.verdict.holds
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    println!(
        "FIXTURE PROOF: pair={} seed=0x{:016X} m15_bars={} h4_bars={} strategy=\"{}\" \
         pass={} pooled_lower_bound={:.6} folds[{}]",
        fixture_pair(),
        pulse::FIXTURE_SEED,
        fixture_m15_candles().len(),
        fixture_h4_candles().len(),
        pulse::FIXTURE_STRATEGY_NAME,
        run.verdict.pass,
        run.verdict.pooled.lower_bound,
        fold_evidence,
    );
}

// ---------------------------------------------------------------------------
// Cases (ii)-(iv): the pinned data versions, the seed's idempotency, and the
// new-build re-certification. (iii)/(iv) drive the REAL `pulse fixture seed`
// binary against a temp db + data dir.
// ---------------------------------------------------------------------------

use std::path::Path;
use std::process::Command;

/// The pinned M15 data version (spec §5: a stable contract, not a value that
/// drifts with the generator's internals).
const PINNED_M15_DATA_VERSION: &str = "0a2c929a27848083";

/// The pinned H4 data version.
const PINNED_H4_DATA_VERSION: &str = "76f15836cc357256";

/// (ii) The generator's data versions equal the pinned constants.
#[tokio::test]
async fn ii_generator_data_versions_are_pinned_constants() {
    let pair = fixture_pair();
    let m15_version = CandleStore::content_version(&pair, Timeframe::M15, &fixture_m15_candles());
    let h4_version = CandleStore::content_version(&pair, Timeframe::H4, &fixture_h4_candles());
    assert_eq!(
        m15_version.as_str(),
        PINNED_M15_DATA_VERSION,
        "M15 data version"
    );
    assert_eq!(
        h4_version.as_str(),
        PINNED_H4_DATA_VERSION,
        "H4 data version"
    );
}

/// Spawn `pulse fixture seed` against a temp db + data dir.
fn run_seed(db: &Path, data_dir: &Path) -> (std::process::ExitStatus, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "fixture",
            "seed",
            "--db",
            &db.to_string_lossy(),
            "--data-dir",
            &data_dir.to_string_lossy(),
        ])
        .output()
        .expect("run pulse fixture seed");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn file_count(dir: &Path) -> usize {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(std::result::Result::ok)
            .map(|e| {
                if e.path().is_dir() {
                    file_count(&e.path())
                } else {
                    1
                }
            })
            .sum(),
        Err(_) => 0,
    }
}

/// One scalar read from the seed db (the row-count probes case (iii) makes).
async fn scalar(pool: &sqlx::SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

/// (iii) `pulse fixture seed` twice on one build: the second run adds no row
/// and no file; BTCUSDT M15/H4 HEAD is byte-identical before and after both
/// runs (absent stays absent); the fixture data versions are
/// `fixture_snapshot` rows; the version reads `certified`.
#[tokio::test]
async fn iii_seed_twice_is_idempotent_and_never_touches_head() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("pulse.db");
    let candles = tmp.path().join("candles");

    let (status, stdout, stderr) = run_seed(&db_path, &candles);
    assert!(
        status.success(),
        "first seed succeeds: {:?}\n{stdout}\n{stderr}",
        status.code()
    );
    assert!(
        stdout.contains("fixture seeded"),
        "first seed reports ids: {stdout}"
    );
    let files_after_first = file_count(&candles);
    assert!(files_after_first > 0, "the seed wrote the snapshot files");

    // HEAD is absent after the first seed — snapshots only, never HEAD.
    let head_path = candles.join("BTCUSDT/M15/HEAD");
    assert!(!head_path.exists(), "M15 HEAD must stay absent");

    // The fixture data versions are fixture_snapshot rows, and the version
    // reads certified (the pointer names a PASSING run).
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path.display()))
        .await
        .unwrap();
    let m15_version =
        CandleStore::content_version(&fixture_pair(), Timeframe::M15, &fixture_m15_candles());
    let h4_version =
        CandleStore::content_version(&fixture_pair(), Timeframe::H4, &fixture_h4_candles());
    let stamped: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM fixture_snapshot WHERE data_version IN (?1, ?2)")
            .bind(m15_version.as_str())
            .bind(h4_version.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        stamped, 2,
        "both fixture data versions are fixture_snapshot rows"
    );
    let certified: i64 = sqlx::query_scalar(
        "SELECT w.pass FROM strategy_version v \
         JOIN walk_forward_run w ON w.id = v.latest_walk_forward_run_id \
         WHERE v.strategy_id = (SELECT id FROM strategy WHERE name = 'FIXTURE certify path')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(certified, 1, "the fixture version's latest run passed");
    let snapshot_rows = scalar(&pool, "SELECT COUNT(*) FROM fixture_snapshot").await;
    let strategies = scalar(
        &pool,
        "SELECT COUNT(*) FROM strategy WHERE name = 'FIXTURE certify path'",
    )
    .await;
    let wf_runs = scalar(&pool, "SELECT COUNT(*) FROM walk_forward_run").await;
    pool.close().await;

    // The SECOND seed: no row, no file, HEAD still absent, ids reported.
    let (status, stdout, stderr) = run_seed(&db_path, &candles);
    assert!(
        status.success(),
        "second seed succeeds: {:?}\n{stdout}\n{stderr}",
        status.code()
    );
    assert!(
        stdout.contains("already seeded on this build"),
        "the second seed reports idempotency: {stdout}"
    );
    assert_eq!(file_count(&candles), files_after_first, "no new file");
    assert!(!head_path.exists(), "HEAD stays absent after both seeds");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path.display()))
        .await
        .unwrap();
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM fixture_snapshot").await,
        snapshot_rows,
        "no new fixture_snapshot row"
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM strategy WHERE name = 'FIXTURE certify path'"
        )
        .await,
        strategies,
        "no second strategy row"
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM walk_forward_run").await,
        wf_runs,
        "no second walk-forward run on the same build"
    );
}

/// (iv) A walk-forward row with a foreign fingerprint makes the next seed add
/// exactly one run — the new build's re-certification.
#[tokio::test]
async fn iv_foreign_fingerprint_run_makes_next_seed_add_exactly_one_run() {
    const FOREIGN: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("pulse.db");
    let candles = tmp.path().join("candles");

    let (status, _, stderr) = run_seed(&db_path, &candles);
    assert!(status.success(), "first seed: {stderr}");

    // The "older build" world: the pointer names a run whose fingerprint is
    // foreign. Immutable rows cannot be edited, so the state is INSERTED raw —
    // one passing parent run (k=2) over two minimal windowed fold runs, each
    // readable by the fail-closed `get_walk_forward_run` (fold rows 0..k, the
    // 0009 window pair, the 0012 lead-in) — and the pointer advances to it by
    // `seq`, exactly what 0014's trigger requires. This is a run an older
    // passing build could have written, with a foreign fingerprint.
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path.display()))
        .await
        .unwrap();
    let version_id: String = sqlx::query_scalar(
        "SELECT v.id FROM strategy_version v JOIN strategy s ON s.id = v.strategy_id \
         WHERE s.name = 'FIXTURE certify path'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    // The parent first — every fold row and fold run references it.
    sqlx::query(
        "INSERT INTO walk_forward_run (id, seq, strategy_version_id, created_at, scheme, rule, k, \
         span_from_ms, span_to_ms, from_defaulted, engine_fingerprint, folds_holding, \
         folds_required, pooled_n, pooled_mean_r, pooled_lower_bound, pass) \
         VALUES ('wf-foreign', 2, ?1, '2026-01-01T00:00:00.000Z', 'rolling-oos/v1', 'wf-v1', 2, \
                 0, 1680000000, 0, ?2, 2, 2, 50, '0.5', 0.1, 1)",
    )
    .bind(&version_id)
    .bind(FOREIGN)
    .execute(&pool)
    .await
    .unwrap();
    for (fold_index, fold_id, from_ms, to_ms) in [
        (0_i64, "wf-foreign-f0", 0_i64, 840_000_000_i64),
        (1_i64, "wf-foreign-f1", 840_000_000_i64, 1_680_000_000_i64),
    ] {
        // The result_content_hash is the re-validate-on-read tamper guard's
        // own derivation over the row (#39), pinned so these rows READ back
        // instead of refusing — the identical columns derive the identical
        // hash for both folds.
        sqlx::query(
            "INSERT INTO backtest_run \
             (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
              engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
              funding_total, slippage_total, expectancy, trade_count, wins, losses, \
              breakeven, max_win_streak, max_loss_streak, skipped_sub_lot, \
              skipped_sub_notional, skipped_leverage_capped, \
              pair, primary_timeframe, primary_data_version, taker_fee_bps, slippage_bps, \
              funding_config, window_from_ms, window_to_ms, window_lead_in_from_ms, \
              walk_forward_run_id, fold_index) \
             VALUES (?1, ?2, 1, '2026-01-01T00:00:00.000Z', ?3, 'tgt', '7a06f9eb344ef8d8396854f60627b1750ec6ff4a16be6ce1fca9682ecb7adde7', '10000', \
                     '0', '0', '0', '0', '0', 25, 20, 5, 0, 3, 1, 0, 0, 0, \
                     'BTCUSDT', '15m', 'aaaa', '4', '1', 'snapshot_rates', \
                     ?4, ?5, 0, 'wf-foreign', ?6)",
        )
        .bind(fold_id)
        .bind(&version_id)
        .bind(FOREIGN)
        .bind(from_ms)
        .bind(to_ms)
        .bind(fold_index)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO walk_forward_fold \
             (walk_forward_run_id, fold_index, window_from_ms, window_to_ms, backtest_run_id, \
              n, mean_r, lower_bound, holds) \
             VALUES ('wf-foreign', ?1, ?2, ?3, ?4, 25, '0.5', 0.1, 1)",
        )
        .bind(fold_index)
        .bind(from_ms)
        .bind(to_ms)
        .bind(fold_id)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "UPDATE strategy_version SET latest_walk_forward_run_id = 'wf-foreign' WHERE id = ?1",
    )
    .bind(&version_id)
    .execute(&pool)
    .await
    .unwrap();
    let runs_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_run")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;

    // The next seed adds EXACTLY one run (this build's re-certification).
    let (status, stdout, stderr) = run_seed(&db_path, &candles);
    assert!(
        status.success(),
        "re-seed on a new build: {:?}\n{stdout}\n{stderr}",
        status.code()
    );
    assert!(
        stdout.contains("new walk-forward certification"),
        "the new build re-certifies: {stdout}"
    );
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path.display()))
        .await
        .unwrap();
    let runs_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_run")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(runs_after, runs_before + 1, "exactly one run added");
    // And the pointer now names THIS build's run — the fixture reads certified
    // on this build again.
    let fingerprint: String = sqlx::query_scalar(
        "SELECT w.engine_fingerprint FROM strategy_version v \
         JOIN walk_forward_run w ON w.id = v.latest_walk_forward_run_id \
         WHERE v.strategy_id = (SELECT id FROM strategy WHERE name = 'FIXTURE certify path')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        fingerprint,
        pulse::EngineFingerprint::current().as_str(),
        "the pointer is back on this build"
    );
}

/// (v) The drift refusal (spec §5's idempotency law): a same-named strategy
/// whose version carries a DIFFERENT document than this build's generator
/// mints refuses the seed with the typed `VersionMismatch` error — the
/// fixture is per-build content, and a drift is surfaced, never silently
/// certified or replaced. The seed's ordered steps 1–2 (the snapshot FILES,
/// then the `fixture_snapshot` stamps) legitimately remain — the refusal
/// fires at step 3, the strategy resolution; no certification, no
/// replacement, no session, and no whole-seed atomic rollback is claimed.
/// The drift: a strategy named exactly like the fixture's, whose only
/// version carries a different document (a different stop distance).
/// Returns the strategy id, the version id and the drifted document text.
async fn create_drifted_fixture_strategy(
    strategies: &SqliteStrategyRepo<pulse::SystemClock>,
) -> (pulse::StrategyId, pulse::VersionId, String) {
    let drifted = strategies
        .create_strategy(
            pulse::FIXTURE_STRATEGY_NAME,
            None,
            &[pulse::FIXTURE_STRATEGY_TAG.to_owned()],
        )
        .await
        .unwrap();
    let mut document = fixture_strategy_dsl();
    document.exits[0] = pulse::ExitRule::StopLoss {
        distance_pct: pulse::SweepableValue::Fixed(Decimal::new(75, 3)),
    };
    let drifted_document = serde_json::to_string(&document).unwrap();
    let drifted_version = strategies
        .create_version(NewVersion {
            strategy_id: drifted.id.clone(),
            parent_version_id: None,
            dsl_json: drifted_document.clone(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .unwrap();
    (drifted.id, drifted_version.id, drifted_document)
}

#[tokio::test]
async fn v_same_named_strategy_with_a_different_document_refuses() {
    let tmp = TempDir::new().unwrap();
    let (db_path, db) = migrated_db(&tmp).await;
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let (drifted_id, drifted_version_id, drifted_document) =
        create_drifted_fixture_strategy(&strategies).await;
    db.pool().close().await;

    // The seed refuses, typed at the CLI edge (ADR-0017).
    let candles = tmp.path().join("candles");
    let (status, stdout, stderr) = run_seed(&db_path, &candles);
    assert!(
        !status.success(),
        "the drifted seed refuses: {:?}\n{stdout}\n{stderr}",
        status.code()
    );
    assert!(
        stderr.contains("different document"),
        "the refusal names the drift: {stderr}"
    );

    // Nothing was certified, nothing stamped, and the drifted strategy and
    // version stand exactly as created — no replacement.
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path.display()))
        .await
        .unwrap();
    let strategy_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM strategy WHERE name = 'FIXTURE certify path'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(strategy_rows, 1, "no second strategy row");
    let version_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version WHERE strategy_id = ?1")
            .bind(drifted_id.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(version_rows, 1, "no second version row");
    let stored: String = sqlx::query_scalar("SELECT dsl FROM strategy_version WHERE id = ?1")
        .bind(drifted_version_id.as_str())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        stored, drifted_document,
        "the drifted document stands, unreplaced"
    );
    let wf_runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_run")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(wf_runs, 0, "the drifted strategy is never certified");
    // Ordered steps 1–2 legitimately stand: both fixture data versions are
    // stamped `fixture_snapshot` rows (the written order stamps BEFORE the
    // strategy resolution where the refusal fires).
    let stamped: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM fixture_snapshot WHERE data_version IN (?1, ?2)")
            .bind(PINNED_M15_DATA_VERSION)
            .bind(PINNED_H4_DATA_VERSION)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stamped, 2, "the ordered stamp step stands on the refusal");
    let all_stamps: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fixture_snapshot")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(all_stamps, 2, "no other stamp was written");
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM paper_session")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 0, "no session exists");
    pool.close().await;
}
