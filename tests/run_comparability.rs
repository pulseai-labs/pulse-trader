//! Run comparability (r3.s1.w4) — the integration suite for #142, #212, #198
//! and #199.
//!
//! One engine build, one data snapshot, one cost model: those are the three
//! conditions under which two backtest runs may be compared. The slice makes
//! them all *checkable* rather than assumed:
//!
//! - **#142** — the exchange symbol filters (`SymbolFilters`) the sizer actually
//!   ran under are part of a run's recorded provenance (`BacktestInputs`), are
//!   persisted on every new run (standalone and each walk-forward fold) through
//!   migration `0015`'s four nullable columns, are distinguishable between two
//!   runs of one version, and are shown wherever run provenance is shown.
//! - **A frozen content hash** — the fixture run's `result_content_hash` equals
//!   the value the same run produced before this change (frozen from the
//!   pre-change engine), so persisting the filters provably did not touch the
//!   results the hash covers.
//! - **#198** — one unreadable run row never wedges a read path: the new-run
//!   FR-7 check falls back to "no comparable prior", inputs inheritance falls
//!   back, the Library card shows the latest *readable* stats, the MCP catalog
//!   returns the readable rows, and the coach accept stays fail-closed.
//! - **#199** — an export tool call refuses a non-UTF-8 path with a tool error
//!   instead of panicking inside `serde_json`.
//! - **The cross-build refusal (r1.s2.w3's engine gate)** — a parent run whose
//!   stored `engine_fingerprint` names another build is refused at the accept's
//!   staged check, and nothing is committed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use rmcp::model::CallToolRequestParams;
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use std::time::Duration;
use support::mcp::{FIXTURE_STORE, copy_tree, manifest, spawn_client};
use tempfile::TempDir;

use pulse::{
    AcceptFailureStage, BacktestConfig, BacktestInputs, BacktestRequest, BacktestResult,
    BinanceAdapter, CandleStore, CoachAction, CoachDecisionOutcome, CoachDecisionRequest,
    CoachRequestFingerprint, CoachSessionClaim, CoachingRepository, CoachingSessionId, CreatedBy,
    DesktopState, Disposition, EngineFingerprint, ExchangeAdapter, ExchangeError, FakeClock,
    Hypothesis, InitialCoachOutcome, LlmCallId, MIGRATOR, Mutation, NewVersion, Pair, ParamValue,
    Proposal, SeqIdSource, SessionOutcome, SqliteBacktestRunRepo, SqliteCoachAcceptanceRepo,
    SqliteCoachingRepo, SqliteStrategyRepo, StrategyRepository, SymbolFilters, Timeframe,
    VersionId, WalkForwardRequest, library_overview_core, resolve_default_request,
    run_coach_decision, run_version_backtest, run_walk_forward,
};

use pulse::BacktestRunRepository;

/// A pinned instant, so `created_at` is deterministic everywhere.
const NOW_MS: i64 = 1_756_425_600_000; // 2026-08-29T00:00:00Z

/// A strictly later instant, so a synthesized run row sorts after the real one
/// under `ORDER BY created_at DESC, id DESC`.
const LATER_MS: i64 = 1_756_512_000_000; // 2026-08-30T00:00:00Z

/// The one-and-only request fingerprint the fixture session claims under.
const FINGERPRINT: &str = "aa11bb22cc33dd44ee55ff6600778899aabbccddeeff00112233445566778899";

/// The sweepable leaf every fixture mutation addresses.
const RSI_PERIOD: &str = "entry.lhs.indicator.rsi.period";

/// A fingerprint value that names some OTHER build: valid lowercase hex, but
/// never this binary's `EngineFingerprint::current()` (any sha256 could be that,
/// so a fixed 64-char literal is used and the test asserts the refusal message
/// rather than comparing the hex itself).
const FOREIGN_FP_HEX: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

/// The same minimal, valid DSL the coach suites use — it produces real trades
/// over the fixture, so the parent run is a genuine one.
const MINIMAL_DSL: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold (comparability)",
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

// ---------------------------------------------------------------------------
// The fixture world (the coach_walk_forward_gate.rs shape)
// ---------------------------------------------------------------------------

struct World {
    tmp: TempDir,
    db: pulse::Db,
    store: CandleStore,
    version_id: VersionId,
    /// The run the coaching session claims (real for a plain world, or the
    /// divergent-fingerprint sibling the cross-build case plants).
    session_run_id: pulse::BacktestRunId,
    parent_inputs: BacktestInputs,
    session_id: CoachingSessionId,
}

impl World {
    /// The sqlite file's path — the same db `DesktopState::open` can attach to
    /// (the Library surface's drive pattern, `tests/tauri_library.rs`).
    fn db_path(&self) -> std::path::PathBuf {
        self.tmp.path().join("pulse.db")
    }

    /// The copied fixture store's path — the MCP server's data dir (the
    /// `spawn_client` convention).
    fn store_path(&self) -> std::path::PathBuf {
        self.tmp.path().join("candles")
    }

    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    fn strategies(&self) -> SqliteStrategyRepo<pulse::SystemClock> {
        SqliteStrategyRepo::new(self.pool().clone())
    }

    fn runs(&self) -> SqliteBacktestRunRepo<pulse::SystemClock> {
        SqliteBacktestRunRepo::new(self.pool().clone())
    }

    fn sessions(&self) -> SqliteCoachingRepo<FakeClock> {
        SqliteCoachingRepo::with_deps(self.pool().clone(), FakeClock::at(NOW_MS))
    }

    fn acceptance(&self) -> SqliteCoachAcceptanceRepo<FakeClock, SeqIdSource> {
        SqliteCoachAcceptanceRepo::with_deps(
            self.pool().clone(),
            FakeClock::at(NOW_MS),
            SeqIdSource::with_prefix("minted"),
        )
    }

    async fn table_count(&self, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(self.pool())
            .await
            .unwrap()
    }
}

/// The plain world: a real parent run over the fixture (this build's
/// fingerprint) with a proposed coaching session claiming it.
async fn world() -> World {
    world_with_session_run(SessionRun::Real).await
}

/// The world's backtest request, shared by `world()` and every test that runs a
/// second cold run of the same version.
fn backtest_request(version_id: &VersionId) -> BacktestRequest {
    BacktestRequest {
        version_id: version_id.clone(),
        pair: Pair::new("BTCUSDT"),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: Some(Timeframe::H4),
        config: BacktestConfig::default(),
        snapshots: None,
        window: None,
    }
}

/// Which run the coaching session claims: the real one (the control), a
/// foreign-fingerprint sibling (the cross-build lever), or a corrupt copy
/// (the #198 fail-closed lever).
enum SessionRun {
    Real,
    Foreign(&'static str),
    Corrupt,
}

/// The world with a chosen session run. `Real` keeps the real run; `Foreign`
/// saves a sibling row carrying identical (real) content but a foreign
/// `engine_fingerprint`, at a later instant; `Corrupt` plants a direct-SQL
/// copy of the real row with one damaged money cell, at a later instant —
/// every reader that touches it fail-closes, which is the point.
/// Copy one run's full row — every column the read feeds or renders verbatim,
/// the `0015` filter columns absent — under a new id and a later timestamp.
/// That is exactly the shape a pre-`0015` row has. `net_pnl_override`
/// replaces the copied `net_pnl`: `Some("not-a-decimal")` is the corrupt-row
/// lever (#198) — the cell fails `parse_decimal` on every fail-closed read.
/// `taker_fee_override` replaces the copied cost model, so a test can tell
/// WHICH row an inheritance actually read.
async fn plant_run_copy(
    pool: &SqlitePool,
    from_id: &str,
    new_id: &str,
    net_pnl_override: Option<&str>,
    taker_fee_override: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO backtest_run \
         (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
          engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
          funding_total, slippage_total, expectancy, win_rate, profit_factor, \
          gross_profit, gross_loss, avg_win, avg_loss, max_drawdown, trade_count, \
          wins, losses, breakeven, max_win_streak, max_loss_streak, sharpe, sortino, \
          regime_breakdown, skipped_sub_lot, skipped_sub_notional, \
          skipped_leverage_capped, \
          pair, primary_timeframe, primary_data_version, htf_timeframe, htf_data_version, \
          taker_fee_bps, slippage_bps, funding_config, open_position) \
         SELECT ?2, strategy_version_id, schema_version, \
                '2027-01-01T00:00:00.000Z', engine_fingerprint, engine_target, \
                result_content_hash, starting_equity, COALESCE(?3, net_pnl), fees_total, \
                funding_total, slippage_total, expectancy, win_rate, profit_factor, \
                gross_profit, gross_loss, avg_win, avg_loss, max_drawdown, trade_count, \
                wins, losses, breakeven, max_win_streak, max_loss_streak, sharpe, \
                sortino, regime_breakdown, skipped_sub_lot, skipped_sub_notional, \
                skipped_leverage_capped, \
                pair, primary_timeframe, primary_data_version, htf_timeframe, \
                htf_data_version, COALESCE(?4, taker_fee_bps), slippage_bps, \
                funding_config, open_position \
         FROM backtest_run WHERE id = ?1",
    )
    .bind(from_id)
    .bind(new_id)
    .bind(net_pnl_override)
    .bind(taker_fee_override)
    .execute(pool)
    .await
    .expect("insert the copied run row");
    // The trades ride along: the fed per-trade content must match for the
    // copied row's stored content hash to re-derive.
    sqlx::query(
        "INSERT INTO trade \
         (id, backtest_run_id, seq, direction, qty, entry_price, exit_price, \
          entry_signal_time, entry_fill_time, exit_signal_time, exit_fill_time, \
          fees_total, funding_total, slippage_total, realized_pnl, realized_r, \
          mfe_r, mae_r, exit_reason, source, regime, fills, stop_price) \
         SELECT ?2 || '-' || seq, ?2, seq, direction, qty, entry_price, \
                exit_price, entry_signal_time, entry_fill_time, exit_signal_time, \
                exit_fill_time, fees_total, funding_total, slippage_total, \
                realized_pnl, realized_r, mfe_r, mae_r, exit_reason, source, regime, \
                fills, stop_price \
         FROM trade WHERE backtest_run_id = ?1",
    )
    .bind(from_id)
    .bind(new_id)
    .execute(pool)
    .await
    .expect("copy the trades to the copied run row");
}

async fn world_with_session_run(choice: SessionRun) -> World {
    let tmp = TempDir::new().unwrap();
    let db = pulse::Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .unwrap();
    MIGRATOR.run(db.pool()).await.expect("run the shipped set");

    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategies
        .create_strategy("RSI Oversold", None, &[])
        .await
        .expect("create strategy");
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: MINIMAL_DSL.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");

    // The fixture store is COPIED so these tests own a writable snapshot set.
    let store_dir = tmp.path().join("candles");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);
    let store = CandleStore::with_base_dir(store_dir);
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());

    // A REAL parent run: its persisted inputs name real data versions, which is
    // what lets the accept's snapshot pins resolve.
    let outcome = run_version_backtest(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &backtest_request(&version.id),
    )
    .await
    .expect("the parent backtest runs over the fixture");

    // The session-run lever: `Real` keeps the real run; `Foreign` saves a
    // later sibling with identical content under a foreign fingerprint —
    // `save_run` derives the row's fingerprint from the result, so a re-read
    // re-derives the same content hash (the tamper guard passes) while the
    // stored engine identity differs, exactly the row a build difference
    // produces. `Corrupt` plants a direct-SQL copy of the real row with a
    // damaged `net_pnl` cell — the parse fails-closed on every read.
    let session_run_id = match choice {
        SessionRun::Real => outcome.run.id.clone(),
        SessionRun::Foreign(hex) => {
            let foreign =
                SqliteBacktestRunRepo::with_deps(db.pool().clone(), FakeClock::at(LATER_MS));
            let result = BacktestResult {
                trades: outcome.trades.clone(),
                net_pnl: outcome.run.net_pnl,
                fees_total: outcome.run.fees_total,
                funding_total: outcome.run.funding_total,
                slippage_total: outcome.run.slippage_total,
                regime_breakdown: outcome.run.regime_breakdown,
                skipped_entries: outcome.run.skipped_entries,
                open_position: outcome.run.open_position.clone(),
                engine_fingerprint: EngineFingerprint::from_stored(hex),
                summary: outcome.run.summary.clone(),
                equity_curve: outcome.equity_curve(),
            };
            foreign
                .save_run(
                    &version.id,
                    &outcome.inputs,
                    &result,
                    &outcome.run.summary,
                    outcome.run.starting_equity,
                )
                .await
                .expect("save the foreign-fingerprint sibling run")
        }
        SessionRun::Corrupt => {
            plant_run_copy(
                db.pool(),
                outcome.run.id.as_str(),
                "corrupt-1",
                Some("not-a-decimal"),
                None,
            )
            .await;
            pulse::BacktestRunId::new("corrupt-1")
        }
    };

    seed_llm_call(db.pool()).await;
    let session_id = seed_proposed_session(
        db.pool(),
        &session_run_id,
        &version.id,
        proposed_mutation(21),
    )
    .await;

    World {
        tmp,
        db,
        store,
        version_id: version.id,
        session_run_id,
        parent_inputs: outcome.inputs.clone(),
        session_id,
    }
}

async fn seed_llm_call(pool: &SqlitePool) {
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

async fn seed_proposed_session(
    pool: &SqlitePool,
    run_id: &pulse::BacktestRunId,
    version_id: &VersionId,
    mutation: Mutation,
) -> CoachingSessionId {
    let repo = SqliteCoachingRepo::with_deps(pool.clone(), FakeClock::at(NOW_MS));
    let id = CoachingSessionId::new("sess-1");
    repo.claim_session(CoachSessionClaim {
        session_id: id.clone(),
        backtest_run_id: run_id.clone(),
        strategy_version_id: version_id.clone(),
        request_fingerprint: CoachRequestFingerprint::new(FINGERPRINT).unwrap(),
        created_at: "2026-08-29T00:00:00.000Z".to_owned(),
    })
    .await
    .expect("claim the session");
    repo.finish_session(
        &id,
        InitialCoachOutcome {
            llm_call_id: Some(LlmCallId::new("call-1")),
            outcome: SessionOutcome::Proposed {
                proposal: Proposal {
                    mutation,
                    hypothesis: Hypothesis::new("a slower RSI trades less often").unwrap(),
                    disposition: Disposition::Proposed,
                    accept_failure: None,
                },
            },
        },
    )
    .await
    .expect("settle the claim");
    id
}

fn proposed_mutation(period: u32) -> Mutation {
    Mutation::SetParam {
        path: RSI_PERIOD.to_owned(),
        new_value: ParamValue::Period { value: period },
    }
}

async fn decide(world: &World, action: CoachAction) -> CoachDecisionOutcome {
    run_coach_decision(
        &world.strategies(),
        &world.store,
        &BinanceAdapter::new(),
        &world.runs(),
        &world.acceptance(),
        &world.sessions(),
        CoachDecisionRequest {
            session_id: world.session_id.clone(),
            action,
        },
    )
    .await
    .expect("the decision resolves")
}

// ---------------------------------------------------------------------------
// The frozen content hash (AC-1 (ii)'s oracle)
// ---------------------------------------------------------------------------

/// The fixture run's `result_content_hash`, frozen from the PRE-change engine
/// (captured at the work item's start via the `emit_frozen_content_hash`
/// helper below, before any source edit). #142's persistence work must not
/// move it: the hash covers results, not input provenance. Read by the (ii)
/// case below, which lands as the TDD loop proceeds.
const FROZEN_CONTENT_HASH: &str =
    "b8e91b89b7727eb97c8200ee04373ffc3ef3568b51ab145bbb2e652cb2bb9228";

/// One-off capture helper (the `tests/determinism.rs::emit_content_hash`
/// pattern): runs the fixture backtest through the REAL `run_version_backtest`
/// path and WRITES the persisted run's `result_content_hash` to
/// `target/run-comparability-hash.txt`. Invoked explicitly with
/// `cargo nextest run --test run_comparability emit_frozen_content_hash -- --ignored`.
#[test]
#[ignore = "one-off freeze: writes the pre-change content hash to target/run-comparability-hash.txt"]
fn emit_frozen_content_hash() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let hash = rt.block_on(async {
        let world = world().await;
        world
            .runs()
            .get_run(&world.session_run_id)
            .await
            .expect("read the fixture run back")
            .expect("the fixture run exists")
            .result_content_hash
    });
    let out = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/run-comparability-hash.txt");
    std::fs::write(&out, &hash).expect("write target/run-comparability-hash.txt");
}

// ---------------------------------------------------------------------------
// AC-1 (i): the cross-build refusal
// ---------------------------------------------------------------------------

/// A coaching session whose claimed run was produced by a DIFFERENT engine
/// build is refused at the accept's staged engine check: the outcome is
/// `AcceptFailed` at the backtest stage with the cross-build message, and NO
/// child version or child run is committed.
#[tokio::test]
async fn accept_is_refused_when_the_session_run_was_built_by_another_engine() {
    let world = world_with_session_run(SessionRun::Foreign(FOREIGN_FP_HEX)).await;

    let before_versions = world.table_count("strategy_version").await;
    let before_runs = world.table_count("backtest_run").await;

    let outcome = decide(&world, CoachAction::Accept).await;

    let proposal = match outcome {
        CoachDecisionOutcome::AcceptFailed(proposal) => proposal,
        other => panic!("expected an AcceptFailed outcome, got {other:?}"),
    };
    let failure = proposal
        .accept_failure
        .expect("the refusal is recorded on the proposal");
    assert!(
        matches!(failure.stage, AcceptFailureStage::Backtest),
        "the refusal names the engine stage, got {:?}",
        failure.stage
    );
    assert!(
        failure.message.contains("different engine build"),
        "the refusal message names the cross-build condition, got: {}",
        failure.message
    );

    // The refusal left no child behind.
    assert_eq!(
        world.table_count("strategy_version").await,
        before_versions,
        "no child version is committed by a refused accept"
    );
    assert_eq!(
        world.table_count("backtest_run").await,
        before_runs,
        "no child run is committed by a refused accept"
    );
}

/// The control: the SAME world with the session run carrying THIS build's
/// fingerprint accepts — proving the refusal above is the fingerprint's doing,
/// not the fixtures'.
#[tokio::test]
async fn accept_commits_when_the_session_run_matches_this_build() {
    let world = world().await;
    let outcome = decide(&world, CoachAction::Accept).await;
    assert!(
        matches!(outcome, CoachDecisionOutcome::Accepted(_)),
        "a same-build accept commits, got {outcome:?}"
    );
    // The child exists: one new version, one new run.
    assert_eq!(
        world.table_count("strategy_version").await,
        2,
        "the accept committed a child version"
    );
    assert_eq!(
        world.table_count("backtest_run").await,
        2,
        "the accept committed the child run"
    );
}

// ---------------------------------------------------------------------------
// AC-1 (ii): the filters are persisted and reload equal to what the engine used
// ---------------------------------------------------------------------------

/// An [`ExchangeAdapter`] that answers every pair with one pinned
/// [`SymbolFilters`] value — the seam (iii)'s distinguishability lever, and the
/// proof that the recorded value is whatever the engine was GIVEN, not a
/// constant.
#[derive(Clone)]
struct FixedFiltersExchange(SymbolFilters);

impl ExchangeAdapter for FixedFiltersExchange {
    fn symbol_filters(&self, _pair: &Pair) -> Result<SymbolFilters, ExchangeError> {
        Ok(self.0.clone())
    }
}

/// A standalone run and every walk-forward fold run persist the four filter
/// values the exchange seam returned, and reload equal to what the engine
/// actually ran under (#142).
#[tokio::test]
async fn standalone_and_fold_runs_persist_the_filters_the_engine_ran_under() {
    let world = world().await;
    let expected = BinanceAdapter::new()
        .symbol_filters(&Pair::new("BTCUSDT"))
        .expect("BTCUSDT filters resolve through the port");

    // The standalone run: recorded == resolved.
    let standalone = world
        .runs()
        .get_run(&world.session_run_id)
        .await
        .expect("read the standalone run")
        .expect("the standalone run exists");
    assert_eq!(
        standalone
            .inputs
            .as_ref()
            .and_then(|i| i.symbol_filters.clone()),
        Some(expected.clone()),
        "the standalone run records the resolved filters"
    );

    // A walk-forward over the same version: every fold run records the same
    // filters — they ride the shared prepare step's inputs.
    let wf = WalkForwardRequest {
        version_id: world.version_id.clone(),
        pair: Pair::new("BTCUSDT"),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: None,
        config: BacktestConfig::default(),
        snapshots: None,
        from_ms: None,
        to_ms: None,
        k: Some(2),
        rule: None,
    };
    run_walk_forward(
        &world.strategies(),
        &world.store,
        &BinanceAdapter::new(),
        &world.runs(),
        &wf,
    )
    .await
    .expect("the walk-forward runs over the fixture");

    let summaries = world
        .runs()
        .list_runs_for_version(&world.version_id)
        .await
        .expect("list the version's runs");
    let mut fold_count = 0;
    for summary in &summaries {
        let run = world
            .runs()
            .get_run(&summary.id)
            .await
            .expect("read a fold run")
            .expect("the fold run exists");
        if run.walk_forward.is_none() {
            continue;
        }
        fold_count += 1;
        assert_eq!(
            run.inputs.as_ref().and_then(|i| i.symbol_filters.clone()),
            Some(expected.clone()),
            "fold {} records the resolved filters",
            summary.id.as_str()
        );
    }
    assert_eq!(fold_count, 2, "the walk persisted exactly k fold runs");
}

/// Two cold runs of the same version over the fixture store persist identical
/// `result_content_hash` values, and that hash equals the value the same run
/// produced BEFORE this change (frozen from the pre-change engine): recording
/// the filters did not move the results the hash covers.
#[tokio::test]
async fn the_fixture_run_s_content_hash_is_frozen_at_the_pre_change_value() {
    let world = world().await;
    let second = run_version_backtest(
        &world.strategies(),
        &world.store,
        &BinanceAdapter::new(),
        &world.runs(),
        &backtest_request(&world.version_id),
    )
    .await
    .expect("the second cold run executes");

    let first = world
        .runs()
        .get_run(&world.session_run_id)
        .await
        .expect("read the first cold run")
        .expect("the first cold run exists");

    assert_eq!(
        first.result_content_hash, second.run.result_content_hash,
        "two cold runs of one version are byte-identical"
    );
    assert_eq!(
        second.run.result_content_hash, FROZEN_CONTENT_HASH,
        "the persisted hash equals the pre-change engine's frozen value"
    );
}

/// A pre-`0015` row — the `0006` inputs columns all present, the four filter
/// columns all NULL, inserted directly — reads back with recorded inputs and
/// `symbol_filters: None`: the filters are "not recorded", never guessed.
#[tokio::test]
async fn a_pre_0015_row_reads_with_the_filters_not_recorded() {
    let world = world().await;
    let real_id = world.session_run_id.as_str().to_owned();

    // Copy the real run's row under the pre-0015 column set. The copied
    // content re-derives the same content hash, so the tamper guard passes.
    plant_run_copy(world.pool(), &real_id, "legacy-1", None, None).await;

    let legacy_id = pulse::BacktestRunId::new("legacy-1");
    let legacy = world
        .runs()
        .get_run(&legacy_id)
        .await
        .expect("read the legacy-shaped row")
        .expect("the legacy-shaped row exists");
    assert!(
        legacy.inputs.is_some(),
        "the 0006 provenance reads normally"
    );
    assert_eq!(
        legacy
            .inputs
            .as_ref()
            .and_then(|i| i.symbol_filters.clone()),
        None,
        "a pre-0015 row reads symbol_filters: None — not recorded, never guessed"
    );
}

// ---------------------------------------------------------------------------
// AC-1 (iii): two runs of one version carry distinguishable filters
// ---------------------------------------------------------------------------

/// Two runs of ONE version, run through DIFFERENT exchange seams, persist
/// different filter values — the recorded provenance distinguishes runs that
/// would otherwise read as comparable.
#[tokio::test]
async fn two_runs_of_one_version_carry_distinguishable_filters() {
    let world = world().await;
    // The fixture's engine path already ran under the REAL Binance filters
    // (lot_step 0.001); run the same version again under a coarser seam.
    let coarse = SymbolFilters {
        lot_step: Decimal::new(1, 2),       // 0.01
        min_qty: Decimal::new(5, 3),        // 0.005
        min_notional: Decimal::new(200, 0), // 200
        max_leverage: Decimal::new(10, 0),  // 10
    };
    let second = run_version_backtest(
        &world.strategies(),
        &world.store,
        &FixedFiltersExchange(coarse.clone()),
        &world.runs(),
        &backtest_request(&world.version_id),
    )
    .await
    .expect("the second run executes under the pinned seam");

    let first = world
        .runs()
        .get_run(&world.session_run_id)
        .await
        .expect("read the first run")
        .expect("the first run exists");
    let second_run = world
        .runs()
        .get_run(&second.run.id)
        .await
        .expect("read the second run")
        .expect("the second run exists");

    let binance = BinanceAdapter::new()
        .symbol_filters(&Pair::new("BTCUSDT"))
        .expect("BTCUSDT filters");
    assert_eq!(
        first.inputs.as_ref().and_then(|i| i.symbol_filters.clone()),
        Some(binance),
        "the first run records the real seam's filters"
    );
    assert_eq!(
        second_run
            .inputs
            .as_ref()
            .and_then(|i| i.symbol_filters.clone()),
        Some(coarse),
        "the second run records the pinned seam's filters"
    );
    assert_ne!(
        first.inputs, second_run.inputs,
        "the two runs' recorded inputs are distinguishable"
    );
}

// ---------------------------------------------------------------------------
// AC-1 (iv): one unreadable run row never wedges a read path (#198)
// ---------------------------------------------------------------------------

/// A corrupt LATEST row — the version's newest `backtest_run` row unreadable
/// on the full read — never blocks a NEW run of that version. While a readable
/// prior exists, the FR-7 compare simply uses it (the walk skips the corrupt
/// row); when NOTHING readable remains, the run still persists and its FR-7
/// slot carries the honest "no comparable prior" note (#198).
#[tokio::test]
async fn a_corrupt_latest_row_never_blocks_a_new_run() {
    let world = world().await;

    // 1. Corrupt the latest row at the TRADE level (the summary read never
    //    touches trade cells, so only full reads fail).
    plant_trade_damage(world.pool(), world.session_run_id.as_str()).await;
    assert!(
        world.runs().get_run(&world.session_run_id).await.is_err(),
        "the damaged run must fail-closed on the full read"
    );

    // A new run: the FR-7 compare rides the newest READABLE prior — there is
    // none yet, so the note says so — and the run persists.
    let first = run_version_backtest(
        &world.strategies(),
        &world.store,
        &BinanceAdapter::new(),
        &world.runs(),
        &backtest_request(&world.version_id),
    )
    .await
    .expect("the new run executes despite the unreadable priors");
    let warning = first
        .fingerprint_warning
        .expect("the unreadable priors are disclosed on the run");
    assert!(
        warning.contains("no comparable prior"),
        "the FR-7 note says no comparable prior, got: {warning}"
    );

    // 2. With a READABLE prior present (the run from step 1), the compare
    //    simply uses it: same build, no warning — the corrupt rows cost a
    //    skip, never the comparison.
    let second = run_version_backtest(
        &world.strategies(),
        &world.store,
        &BinanceAdapter::new(),
        &world.runs(),
        &backtest_request(&world.version_id),
    )
    .await
    .expect("the second new run executes");
    assert_eq!(
        second.fingerprint_warning, None,
        "a readable prior is compared normally; the corrupt rows are skipped"
    );

    // And the new runs read back cleanly — persisted, not lost.
    let saved = world
        .runs()
        .get_run(&second.run.id)
        .await
        .expect("read the new run back")
        .expect("the new run exists");
    assert_eq!(
        saved.result_content_hash, second.run.result_content_hash,
        "the new run's stored hash verifies"
    );
}

/// The default-request resolver inherits from the version's latest READABLE
/// run: a trade-damaged latest row (its recorded inputs intact) is skipped in
/// favour of the readable one beneath it, and when NO readable run remains
/// the resolve falls back to the application defaults — the same shape a
/// version with no run gets — instead of erroring (#198).
#[tokio::test]
async fn inputs_inheritance_falls_back_when_the_latest_row_is_unreadable() {
    let world = world().await;

    // The corrupt copy: readable summary, readable INPUTS (a distinctive cost
    // model, 99 bps taker), unreadable TRADES — the full read fails, the
    // inputs columns alone would lie about which run is trustworthy.
    plant_run_copy(
        world.pool(),
        world.session_run_id.as_str(),
        "corrupt-1",
        None,
        Some("99"),
    )
    .await;
    plant_trade_damage(world.pool(), "corrupt-1").await;

    // The readable walk skips corrupt-1 and inherits from the REAL run
    // beneath it: the real run's cost model (4 bps), never the corrupt row's.
    let inherited =
        resolve_default_request(&world.strategies(), &world.runs(), &world.version_id, None)
            .await
            .expect("the resolver inherits from the readable run");
    assert_ne!(
        inherited.config.taker_fee_bps,
        Decimal::new(99, 0),
        "the unreadable row's cost model is not inherited"
    );
    assert_eq!(
        inherited.config.taker_fee_bps, world.parent_inputs.taker_fee_bps,
        "the newest READABLE run's cost model is"
    );

    // When NOTHING readable remains, the resolve falls back to the
    // application defaults — unpinned HEAD, default config.
    plant_trade_damage(world.pool(), world.session_run_id.as_str()).await;
    let fallback =
        resolve_default_request(&world.strategies(), &world.runs(), &world.version_id, None)
            .await
            .expect("the resolver falls back instead of failing");
    assert_eq!(
        fallback.snapshots, None,
        "with no readable run, the defaults run at HEAD, unpinned"
    );
    assert_ne!(
        fallback.config.taker_fee_bps,
        Decimal::new(99, 0),
        "no unreadable row's cost model survives the fallback"
    );
}

/// Damage one run at the TRADE level — an extra trade row whose `realized_pnl`
/// will not parse. The summary read never touches trade cells, so the row
/// survives `list_runs_for_version`'s own D5 skip and every full
/// read of it fails-closed: exactly the wedge the readable-latest walk and
/// the MCP hydrate loop must not turn into a whole-path failure (#198).
async fn plant_trade_damage(pool: &SqlitePool, run_id: &str) {
    sqlx::query(
        "INSERT INTO trade \
         (id, backtest_run_id, seq, direction, qty, entry_price, exit_price, \
          entry_signal_time, entry_fill_time, exit_signal_time, exit_fill_time, \
          fees_total, funding_total, slippage_total, realized_pnl, realized_r, \
          mfe_r, mae_r, exit_reason, source, regime, fills, stop_price) \
         VALUES ('damaged-' || ?1, ?1, 99, 'long', '1', '100', '100', 0, 0, 0, 0, \
                 '0', '0', '0', 'not-a-decimal', '1', '1', '0', 'take_profit', \
                 'backtest', 'ranging', '[]', NULL)",
    )
    .bind(run_id)
    .execute(pool)
    .await
    .expect("insert the damaging trade row");
}

/// The Library card over a version whose LATEST run row is corrupt lists the
/// version with the newest READABLE run's stats — an unreadable row never
/// blanks the Library (#198).
#[tokio::test]
async fn the_library_card_survives_a_corrupt_latest_row() {
    let world = world().await;
    plant_run_copy(
        world.pool(),
        world.session_run_id.as_str(),
        "corrupt-1",
        Some("not-a-decimal"),
        None,
    )
    .await;

    let state = DesktopState::open(&world.db_path())
        .await
        .expect("open the desktop state over the world's db");
    let overview = library_overview_core(&state)
        .await
        .expect("the Library overview reads despite the corrupt latest row");
    let version = overview
        .strategies
        .iter()
        .flat_map(|s| &s.versions)
        .find(|v| v.id == world.version_id.as_str())
        .expect("the version is listed");
    assert!(
        version.stats.is_some(),
        "the version's card shows the newest READABLE run's stats"
    );
}

/// The coach accept stays FAIL-CLOSED against a corrupt parent row: the
/// session's claimed run is unreadable, so the accept refuses at the
/// load-inputs stage rather than replaying unproven inputs (#198's a2).
#[tokio::test]
async fn coach_accept_stays_fail_closed_when_the_parent_run_row_is_corrupt() {
    let world = world_with_session_run(SessionRun::Corrupt).await;

    let before_versions = world.table_count("strategy_version").await;
    let before_runs = world.table_count("backtest_run").await;

    let outcome = decide(&world, CoachAction::Accept).await;
    let proposal = match outcome {
        CoachDecisionOutcome::AcceptFailed(proposal) => proposal,
        other => panic!("expected an AcceptFailed outcome, got {other:?}"),
    };
    let failure = proposal
        .accept_failure
        .expect("the refusal is recorded on the proposal");
    assert!(
        matches!(failure.stage, AcceptFailureStage::LoadInputs),
        "the accept refuses at the load-inputs stage, got {:?}",
        failure.stage
    );

    // No child committed.
    assert_eq!(
        world.table_count("strategy_version").await,
        before_versions,
        "no child version is committed by a refused accept"
    );
    assert_eq!(
        world.table_count("backtest_run").await,
        before_runs,
        "no child run is committed by a refused accept"
    );
}

// ---------------------------------------------------------------------------
// AC-1 (v): the MCP catalog isolates a bad row (#198)
// ---------------------------------------------------------------------------

/// MCP `list_runs` over three runs of one version, one of them unreadable at
/// the full-read level (a damaged trade cell that the summary read never
/// touches): the call SUCCEEDS with exactly the two readable rows — no
/// `tool_error`, unchanged payload shape — and the skipped row is warned
/// about on the server's log channel (the same `eprintln!` policy
/// `list_runs_for_version` applies; visible in the spawned server's stderr,
/// which this harness inherits).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_runs_skips_an_unreadable_row_and_returns_the_readable_ones() {
    let world = world().await;
    // Two more cold runs: three rows of one version, all summary-parseable.
    let second = run_version_backtest(
        &world.strategies(),
        &world.store,
        &BinanceAdapter::new(),
        &world.runs(),
        &backtest_request(&world.version_id),
    )
    .await
    .expect("the second run executes");
    let third = run_version_backtest(
        &world.strategies(),
        &world.store,
        &BinanceAdapter::new(),
        &world.runs(),
        &backtest_request(&world.version_id),
    )
    .await
    .expect("the third run executes");

    // Damage ONE run at the TRADE level: the summary read never parses trade
    // cells, so the row survives `list_runs_for_version`'s own D5 skip and is
    // exactly the wedge the hydrate loop used to turn into a whole-call
    // `tool_error`.
    sqlx::query(
        "INSERT INTO trade \
         (id, backtest_run_id, seq, direction, qty, entry_price, exit_price, \
          entry_signal_time, entry_fill_time, exit_signal_time, exit_fill_time, \
          fees_total, funding_total, slippage_total, realized_pnl, realized_r, \
          mfe_r, mae_r, exit_reason, source, regime, fills, stop_price) \
         VALUES ('bad-1', ?1, 99, 'long', '1', '100', '100', 0, 0, 0, 0, \
                 '0', '0', '0', 'not-a-decimal', '1', '1', '0', 'take_profit', \
                 'backtest', 'ranging', '[]', NULL)",
    )
    .bind(second.run.id.as_str())
    .execute(world.pool())
    .await
    .expect("insert the damaging trade row");
    // And the wedge is real: the full read of that run fail-closes.
    assert!(
        world.runs().get_run(&second.run.id).await.is_err(),
        "the damaged run must fail-closed on the full read"
    );

    // The same stall bounds the (vi) case carries: a catalog call that never
    // answers fails inside the bound, never wedges the suite.
    let client = tokio::time::timeout(
        CHILD_BOUND,
        spawn_client(&world.db_path(), &world.store_path()),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "pulse mcp never finished its handshake within {CHILD_BOUND:?} — \
                 failing on the stall instead of hanging the suite"
        )
    });
    let result = tokio::time::timeout(
        CHILD_BOUND,
        client.call_tool(
            CallToolRequestParams::new("list_runs".to_owned()).with_arguments(
                support::mcp::arguments(&serde_json::json!({
                    "version_id": world.version_id.as_str(),
                })),
            ),
        ),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "list_runs never answered within {CHILD_BOUND:?} — the reply stalled; \
             failing on the stall instead of hanging the suite"
        )
    })
    .expect("the tools/call answers — the catalog call never dies");

    assert_ne!(
        result.is_error,
        Some(true),
        "one unreadable row is not a catalog error"
    );
    let structured = result
        .structured_content
        .expect("the result carries structured content");
    let rows = structured
        .get("runs")
        .and_then(|v| v.as_array())
        .expect("the rows ride under `runs` (shape unchanged)");
    assert_eq!(
        rows.len(),
        2,
        "exactly the two readable rows return; the unreadable one is skipped"
    );
    let listed_ids: Vec<&str> = rows
        .iter()
        .filter_map(|row| row.get("run_id").and_then(|v| v.as_str()))
        .collect();
    assert!(
        listed_ids.contains(&third.run.id.as_str()),
        "the readable rows are present: {listed_ids:?}"
    );
    assert!(
        !listed_ids.contains(&second.run.id.as_str()),
        "the unreadable row is not among them: {listed_ids:?}"
    );
    // Bound 3 — the session close: the same stall rule as the handshake and
    // the call above. A close that never completes fails inside the bound,
    // never hangs the suite.
    bounded_cancel(client).await;
}

/// The other half of that skip (#198's follow-up, r3.s1's round-1 review): a
/// STORE failure is not a corrupt ROW. The catalog call must REPORT the failure
/// — `is_error` naming it — instead of answering with a short list that reads
/// like a version with fewer runs.
#[tokio::test]
async fn list_runs_reports_a_store_failure_instead_of_a_short_catalog() {
    let world = world().await;
    let run = run_version_backtest(
        &world.strategies(),
        &world.store,
        &BinanceAdapter::new(),
        &world.runs(),
        &backtest_request(&world.version_id),
    )
    .await
    .expect("the run executes");
    assert!(
        !run.run.id.as_str().is_empty(),
        "the catalog has a readable row to lose"
    );

    // The store fails while the catalog's own candidate query still answers:
    // the table every FULL read needs is gone, which is the shape that used to
    // be swallowed as "no readable rows".
    sqlx::query("DROP TABLE trade")
        .execute(world.pool())
        .await
        .expect("drop the trades table to simulate a failing store");

    let client = tokio::time::timeout(
        CHILD_BOUND,
        spawn_client(&world.db_path(), &world.store_path()),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "pulse mcp never finished its handshake within {CHILD_BOUND:?} — \
             failing on the stall instead of hanging the suite"
        )
    });

    let result = tokio::time::timeout(
        CHILD_BOUND,
        client.call_tool(
            CallToolRequestParams::new("list_runs".to_owned()).with_arguments(
                support::mcp::arguments(&serde_json::json!({
                    "version_id": world.version_id.as_str(),
                })),
            ),
        ),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "list_runs never answered within {CHILD_BOUND:?} — the reply stalled; \
             failing on the stall instead of hanging the suite"
        )
    })
    .expect("the tools/call answers");

    assert_eq!(
        result.is_error,
        Some(true),
        "a failing store is the catalog's error, never a short list"
    );
    let message = result
        .structured_content
        .as_ref()
        .and_then(|structured| structured.get("message"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned();
    assert!(
        message.contains("trade"),
        "the refusal names the store failure that caused it: {message}"
    );

    bounded_cancel(client).await;
}

// ---------------------------------------------------------------------------
// AC-1 (vi): an export tool call refuses a non-UTF-8 path with a tool error
// ---------------------------------------------------------------------------

/// Every read on the spawned `pulse mcp` child is wall-clock bounded — the
/// binding stall rule this item's first session paid for: its (vi) run wedged
/// ~39 minutes at 0% CPU when the child's reply never arrived and the test's
/// stdout read and the child's stdin read waited on each other's pipes with no
/// bound of any kind. The bound wraps every await that reads the child: each
/// `spawn_client` handshake, each tool call, the (vi) liveness follow-up (the
/// shared `support::mcp::call` carries no bound of its own), and both session
/// closes. A stall is now a FAILED case — the timer fires, the case panics
/// naming the bound, and the test process's exit closes the pipes so the child
/// sees EOF — never a hung suite. The child's stderr stays `Stdio::inherit`
/// (drained concurrently by the runner's capture), so a stall failure still
/// carries the server's last words.
const CHILD_BOUND: Duration = Duration::from_secs(60);

/// The session close inside the stall bound, shared by both MCP-child tests so
/// neither close can regress to an unbounded await (fix round 1). Extracted
/// also to keep each test under the pedantic line-count lint the bounds would
/// otherwise trip. Takes the client by value — rmcp's `cancel` consumes it.
async fn bounded_cancel(client: rmcp::service::RunningService<rmcp::RoleClient, ()>) {
    tokio::time::timeout(CHILD_BOUND, client.cancel())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "cancel never completed within {CHILD_BOUND:?} — the session close \
                 stalled; failing on the stall instead of hanging the suite"
            )
        })
        .expect("cancel the session");
}

/// `export_trades` with a server exports dir whose absolute path carries
/// invalid UTF-8 bytes (built with `OsStr::from_bytes`): the tool call
/// RETURNS a result — `is_error` with a message naming the problem — instead
/// of panicking inside serialization and killing the session (r3.s1.w4,
/// #199). The follow-up call proves the server lived.
///
/// Linux-only by necessity, not by preference: the case needs a directory
/// whose NAME carries invalid UTF-8, and the `macos-latest` leg's APFS (the
/// only nextest leg in CI) refuses such a name at `create_dir_all` with
/// `EILSEQ`. Every export path derives from that canonicalized directory, so
/// the scenario cannot be constructed on macOS at all. The refusal itself —
/// `path_json` turning the path into the tool error — is pinned on EVERY
/// platform by `src/mcp/export.rs`'s unit test, which builds the path without
/// touching a filesystem.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_trades_refuses_a_non_utf8_path_with_a_tool_error() {
    use std::os::unix::ffi::OsStrExt as _;
    let world = world().await;
    let data_dir = world
        .tmp
        .path()
        .join(std::ffi::OsStr::from_bytes(b"exports-\xff-dir"));
    std::fs::create_dir_all(&data_dir).expect("create the non-UTF-8 data dir");

    // Bound 1 — the handshake: a child that never finishes initializing is a
    // stall, and a stall fails the case, never hangs the suite.
    let client = tokio::time::timeout(CHILD_BOUND, spawn_client(&world.db_path(), &data_dir))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "pulse mcp never finished its handshake within {CHILD_BOUND:?} — \
                 failing on the stall instead of hanging the suite"
            )
        });

    // Bound 2 — the tool call: the reply either arrives inside the bound or
    // the case fails naming the bound. (The deadlock this replaces was an
    // unbounded read on exactly this call.)
    let call = client.call_tool(
        CallToolRequestParams::new("export_trades".to_owned()).with_arguments(
            support::mcp::arguments(&serde_json::json!({
                "run_id": world.session_run_id.as_str(),
            })),
        ),
    );
    let result = tokio::time::timeout(CHILD_BOUND, call)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "export_trades never answered within {CHILD_BOUND:?} — the reply \
                 stalled; failing on the stall instead of hanging the suite"
            )
        })
        .expect("the tools/call answers — the server returns, never panics");

    assert_eq!(
        result.is_error,
        Some(true),
        "a non-UTF-8 export path refuses as a tool error"
    );
    let structured = result
        .structured_content
        .expect("the error carries structured content");
    let message = structured
        .get("message")
        .and_then(|v| v.as_str())
        .expect("the error names its message");
    assert!(
        message.contains("not valid UTF-8"),
        "the error names the problem: {message}"
    );

    // Bound 3 — the liveness follow-up: `support::mcp::call` carries no bound
    // of its own, so the timeout wraps it. If the server died the call panics;
    // if it stalls the bound fires — either way the case fails, never hangs.
    tokio::time::timeout(
        CHILD_BOUND,
        support::mcp::call(&client, "list_strategies", serde_json::json!({})),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the liveness follow-up never answered within {CHILD_BOUND:?} — the reply \
             stalled; failing on the stall instead of hanging the suite"
        )
    });
    // Bound 4 — the session close: the same stall rule as the handshake and
    // the calls above. A close that never completes fails inside the bound,
    // never hangs the suite.
    bounded_cancel(client).await;
}
