//! r1.s4.w2 — the coach DECISION rail: modify, reject, accept, and the
//! persisted-input re-backtest an accept commits.
//!
//! **Everything here is real.** A `tempfile` `SQLite` migrated by the shipped
//! `MIGRATOR` through `0008`, the real `SqliteStrategyRepo` /
//! `SqliteBacktestRunRepo` / `SqliteCoachingRepo` / `SqliteCoachAcceptanceRepo`, the
//! committed Parquet candle fixture through the real `CandleStore`, the real
//! `apply()` and the real deterministic engine. The only doubles are two
//! INSTRUMENTS, both wrapping real behaviour rather than replacing it: a counting
//! `ExchangeAdapter` (so "the accept never reached the exchange" is an assertion
//! rather than a claim) and a read-back-failing `BacktestRunRepository` decorator
//! over the real repo (the `r1.s3` post-save injection precedent).
//!
//! The parent run every case coaches on is produced by the REAL
//! `run_version_backtest` over the fixture snapshots, so its persisted
//! `BacktestInputs` name real data versions and its summary is a real one — which is
//! what makes "the child was re-run on the parent's exact inputs" checkable rather
//! than asserted.
//!
//! Offline by construction: no provider, no network, no live LLM.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use pulse::{
    AcceptFailureStage, BacktestConfig, BacktestInputs, BacktestRequest, BacktestResult,
    BacktestRunId, BacktestRunRepository, BinanceAdapter, CandleSeriesRepository, CandleStore,
    CoachAction, CoachDecisionError, CoachDecisionOutcome, CoachDecisionRequest,
    CoachRequestFingerprint, CoachSessionClaim, CoachingRepository, CoachingSessionId, CreatedBy,
    DataError, Db, Disposition, ExchangeAdapter, ExchangeError, FakeClock, Hypothesis,
    InitialCoachOutcome, LlmCallId, MIGRATOR, Mutation, NewVersion, Pair, ParamValue, PersistedRun,
    Proposal, ReadBackFailure, RunSummary, SeqIdSource, SessionOutcome, SqliteBacktestRunRepo,
    SqliteCoachAcceptanceRepo, SqliteCoachingRepo, SqliteStrategyRepo, StrategyDsl,
    StrategyRepository, SummaryStats, SymbolFilters, Timeframe, VersionId, run_coach_decision,
    run_version_backtest,
};
mod coach_support;

use rust_decimal::Decimal;
use sqlx::SqlitePool;
use tempfile::TempDir;

/// The committed candle fixture every run reads (mirrors `backtest_provenance.rs`).
const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

/// A pinned instant, so `created_at` is deterministic everywhere.
const NOW_MS: i64 = 1_756_425_600_000; // 2026-08-29T00:00:00Z

/// The one-and-only request fingerprint the fixture session claims under.
const FINGERPRINT: &str = "aa11bb22cc33dd44ee55ff6600778899aabbccddeeff00112233445566778899";

/// The sweepable leaf every fixture mutation addresses.
const RSI_PERIOD: &str = "entry.lhs.indicator.rsi.period";

/// The same minimal, valid DSL the other fixture binaries use — it produces real
/// trades over the fixture, so the parent run is a genuine one.
const MINIMAL_DSL: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold (decision)",
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

fn manifest(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// Recursively copy `from` to `to`, so a test owns a writable candle store.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the store copy");
    for entry in std::fs::read_dir(from).expect("read the fixture store") {
        let entry = entry.expect("a fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("entry type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy a fixture file");
        }
    }
}

// ---------------------------------------------------------------------------
// Instruments — the only two doubles, both over real behaviour
// ---------------------------------------------------------------------------

/// The pinned BTCUSDT filters, plus a COUNT of how many times they were asked for.
///
/// The accept path resolves symbol filters (pinned exchange metadata) and must never
/// reach the exchange for anything else — in particular it never fetches candles. A
/// counter makes "never touched the exchange" an assertion instead of a claim.
#[derive(Debug, Default, Clone)]
struct CountingExchange {
    calls: std::sync::Arc<AtomicUsize>,
}

impl CountingExchange {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ExchangeAdapter for CountingExchange {
    fn symbol_filters(&self, pair: &Pair) -> Result<SymbolFilters, ExchangeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        BinanceAdapter::new().symbol_filters(pair)
    }
}

/// The REAL run repository, with `get_run` refused for every id but one.
///
/// The post-commit read back is the only caller that asks for the freshly minted
/// child run, so refusing everything except the parent id injects a read-back
/// failure at exactly the point `r1.s3`'s saved-but-unreadable precedent covers —
/// through the real port, over the real adapter (the `tauri_backtest.rs` technique).
struct ReadBackFailingRuns<R> {
    inner: R,
    readable: BacktestRunId,
}

impl<R: BacktestRunRepository + Send + Sync> BacktestRunRepository for ReadBackFailingRuns<R> {
    async fn save_run(
        &self,
        strategy_version_id: &VersionId,
        inputs: &BacktestInputs,
        result: &BacktestResult,
        summary: &SummaryStats,
        starting_equity: Decimal,
    ) -> Result<BacktestRunId, DataError> {
        self.inner
            .save_run(
                strategy_version_id,
                inputs,
                result,
                summary,
                starting_equity,
            )
            .await
    }

    async fn get_run(&self, id: &BacktestRunId) -> Result<Option<PersistedRun>, DataError> {
        if id == &self.readable {
            return self.inner.get_run(id).await;
        }
        Err(DataError::Db(format!(
            "injected read-back failure for run `{}`",
            id.as_str()
        )))
    }

    async fn latest_run_for_version(
        &self,
        strategy_version_id: &VersionId,
    ) -> Result<Option<PersistedRun>, DataError> {
        self.inner.latest_run_for_version(strategy_version_id).await
    }

    async fn list_runs_for_version(
        &self,
        strategy_version_id: &VersionId,
    ) -> Result<Vec<RunSummary>, DataError> {
        self.inner.list_runs_for_version(strategy_version_id).await
    }

    async fn get_trades(&self, id: &BacktestRunId) -> Result<Vec<pulse::Trade>, DataError> {
        self.inner.get_trades(id).await
    }
}

// ---------------------------------------------------------------------------
// The fixture world
// ---------------------------------------------------------------------------

/// Everything one decision case needs, all of it real.
struct World {
    _tmp: TempDir,
    db: Db,
    store: CandleStore,
    version_id: VersionId,
    parent_run_id: BacktestRunId,
    parent_summary: SummaryStats,
    parent_inputs: BacktestInputs,
    session_id: CoachingSessionId,
}

impl World {
    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    /// Repoint the parent run's primary snapshot at a GAPPED copy of itself.
    ///
    /// One candle is removed from the middle, so the series stays sorted and
    /// non-empty — the two conditions the accept path used to check — and gains
    /// exactly one spacing gap. The store commits it happily (a gap is reported by
    /// `validate`, not refused by it), which is precisely why the accept path has to
    /// do the refusing itself.
    async fn gap_primary_snapshot(&self) {
        let pair = Pair::new("BTCUSDT");
        let stored = self
            .store
            .load_version(
                &pair,
                Timeframe::M15,
                &self.parent_inputs.primary.data_version,
            )
            .expect("the parent's primary snapshot is in the store");

        let mut candles = stored.series.candles.clone();
        assert!(
            candles.len() > 4,
            "the fixture needs enough bars to punch a hole in"
        );
        let hole = candles.len() / 2;
        candles.remove(hole);

        let gapped = self
            .store
            .commit(&pair, Timeframe::M15, candles)
            .expect("the store commits a gapped series — it reports gaps, it does not refuse them");

        repoint_parent_primary_snapshot(
            self.pool(),
            &self.parent_run_id,
            gapped.series.version.as_str(),
        )
        .await;
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

    /// The proposal as it currently stands in the database.
    async fn proposal(&self) -> Proposal {
        match self
            .sessions()
            .get_session(&self.session_id)
            .await
            .expect("read the session")
            .expect("the session exists")
            .outcome
        {
            SessionOutcome::Proposed { proposal } => proposal,
            other => panic!("expected a proposal turn, got {other:?}"),
        }
    }

    async fn version_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
            .fetch_one(self.pool())
            .await
            .unwrap()
    }

    async fn run_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM backtest_run")
            .fetch_one(self.pool())
            .await
            .unwrap()
    }

    /// "No SETTLED row exists" — the claim-before-I/O-aware form of "nothing was
    /// written" (r1.s4.w1 §8): a pending or open row is expected to be there, and a
    /// bare `COUNT(*) = 0` would fail for the wrong reason.
    async fn settled_proposal_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM coaching_proposals WHERE disposition = 'accepted'")
            .fetch_one(self.pool())
            .await
            .unwrap()
    }
}

/// Build the world: a migrated database, a strategy version, a REAL parent run over
/// the fixture snapshots, a ledger row, and one `proposed` coaching session.
async fn world() -> World {
    world_with_dsl(MINIMAL_DSL).await
}

async fn world_with_dsl(dsl_json: &str) -> World {
    let tmp = TempDir::new().unwrap();
    let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
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
            dsl_json: dsl_json.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");

    // The fixture store is COPIED into the temp dir rather than opened in place: a
    // test that needs an awkward snapshot (a gapped one, below) has to commit it
    // somewhere, and committing into `tests/fixtures/` would edit the repository's
    // own fixture out from under every other test. The copy is 260K and the data is
    // byte-identical, so nothing else about these tests changes.
    let store_dir = tmp.path().join("candles");
    copy_dir(&manifest(FIXTURE_STORE), &store_dir);
    let store = CandleStore::with_base_dir(store_dir);
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());

    // The PARENT run is a real backtest over the fixture, so its persisted inputs
    // name real data versions and its summary is a real one.
    let outcome = run_version_backtest(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &BacktestRequest {
            version_id: version.id.clone(),
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: Some(Timeframe::H4),
            config: BacktestConfig::default(),
            snapshots: None,
            window: None,
        },
    )
    .await
    .expect("the parent backtest runs over the fixture");

    seed_llm_call(db.pool()).await;
    let session_id = seed_proposed_session(
        db.pool(),
        &outcome.run.id,
        &version.id,
        proposed_mutation(21),
    )
    .await;

    World {
        _tmp: tmp,
        db,
        store,
        version_id: version.id,
        parent_run_id: outcome.run.id.clone(),
        parent_summary: outcome.run.summary.clone(),
        parent_inputs: outcome.inputs.clone(),
        session_id,
    }
}

/// The one ledger row every accepted child names (ADR-0010).
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

/// Claim and settle one turn through the REAL repository, so the session row is the
/// one the accept path derives its provenance from.
async fn seed_proposed_session(
    pool: &SqlitePool,
    run_id: &BacktestRunId,
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

/// Drive the decision over the world's real ports.
async fn decide(
    world: &World,
    action: CoachAction,
) -> Result<CoachDecisionOutcome, CoachDecisionError> {
    decide_with_exchange(world, action, &CountingExchange::default()).await
}

async fn decide_with_exchange(
    world: &World,
    action: CoachAction,
    exchange: &CountingExchange,
) -> Result<CoachDecisionOutcome, CoachDecisionError> {
    run_coach_decision(
        &world.strategies(),
        &world.store,
        exchange,
        &world.runs(),
        &world.acceptance(),
        &world.sessions(),
        CoachDecisionRequest {
            session_id: world.session_id.clone(),
            action,
        },
    )
    .await
}

/// The DSL of a persisted version, as stored.
async fn version_dsl(world: &World, id: &VersionId) -> StrategyDsl {
    world
        .strategies()
        .get_version(id)
        .await
        .expect("read the version")
        .expect("the version exists")
        .dsl
}

/// The RSI period the entry condition holds, read straight off the stored document.
fn rsi_period(dsl: &StrategyDsl) -> u32 {
    let json = serde_json::to_value(dsl).unwrap();
    u32::try_from(
        json["entry"]["lhs"]["spec"]["period"]
            .as_u64()
            .expect("the fixture entry addresses an RSI period"),
    )
    .unwrap()
}

// ===========================================================================
// 1. Modify
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_modify_revalidates_stores_the_payload_and_repeats() {
    let world = world().await;

    let first = decide(&world, CoachAction::Modify(proposed_mutation(25)))
        .await
        .expect("the trader's own SetParam applies");
    let CoachDecisionOutcome::Modified(proposal) = first else {
        panic!("expected Modified, got {first:?}");
    };
    assert_eq!(proposal.disposition, Disposition::Modified);
    assert_eq!(proposal.mutation, proposed_mutation(25));

    // A modify is repeatable: `modified -> modified` is a re-edit, not a transition.
    let second = decide(&world, CoachAction::Modify(proposed_mutation(30)))
        .await
        .expect("a second edit replaces the first");
    let CoachDecisionOutcome::Modified(proposal) = second else {
        panic!("expected Modified, got {second:?}");
    };
    assert_eq!(
        proposal.mutation,
        proposed_mutation(30),
        "the LATEST edit is what is stored"
    );

    // It is durable, not just returned.
    assert_eq!(world.proposal().await.mutation, proposed_mutation(30));
    assert_eq!(
        world.version_count().await,
        1,
        "a modify mints no child version"
    );
    assert_eq!(world.run_count().await, 1, "a modify creates no run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inapplicable_modify_writes_nothing() {
    let world = world().await;

    let outcome = decide(
        &world,
        CoachAction::Modify(Mutation::SetParam {
            path: "risk.no_such_knob".to_owned(),
            new_value: ParamValue::Period { value: 9 },
        }),
    )
    .await;

    assert!(
        matches!(
            outcome,
            Err(CoachDecisionError::InapplicableMutation { .. })
        ),
        "an unaddressable locator is a typed refusal, got {outcome:?}"
    );

    let stored = world.proposal().await;
    assert_eq!(
        stored.disposition,
        Disposition::Proposed,
        "a refused modify leaves the proposal exactly as it was"
    );
    assert_eq!(
        stored.mutation,
        proposed_mutation(21),
        "the coach's mutation is still the one an accept would re-apply"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_modify_revalidates_against_the_coached_version_not_a_caller_document() {
    let world = world().await;

    // `NoChange` is a typed inapplicability: the coached version's RSI period is 14,
    // so offering 14 changes nothing — and the only way to know that is to have read
    // THIS version's DSL.
    let outcome = decide(&world, CoachAction::Modify(proposed_mutation(14))).await;

    assert!(
        matches!(
            outcome,
            Err(CoachDecisionError::InapplicableMutation { .. })
        ),
        "a no-op mutation is refused against the coached DSL, got {outcome:?}"
    );
}

// ===========================================================================
// 2. Reject
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reject_is_terminal_and_idempotent() {
    let world = world().await;

    let first = decide(&world, CoachAction::Reject)
        .await
        .expect("reject the proposal");
    let CoachDecisionOutcome::Rejected(proposal) = first else {
        panic!("expected Rejected, got {first:?}");
    };
    assert_eq!(proposal.disposition, Disposition::Rejected);

    // Replaying the rejection is the idempotent no-op the 0008 trigger permits.
    let again = decide(&world, CoachAction::Reject)
        .await
        .expect("replaying a rejection is a no-op");
    assert!(matches!(again, CoachDecisionOutcome::Rejected(_)));

    // And it really is terminal.
    let modify_after = decide(&world, CoachAction::Modify(proposed_mutation(25))).await;
    assert!(
        matches!(
            modify_after,
            Err(CoachDecisionError::NotActionable {
                current: pulse::DispositionKind::Rejected,
                ..
            })
        ),
        "a rejected proposal cannot be edited, got {modify_after:?}"
    );
    let accept_after = decide(&world, CoachAction::Accept).await;
    assert!(
        matches!(
            accept_after,
            Err(CoachDecisionError::NotActionable {
                current: pulse::DispositionKind::Rejected,
                ..
            })
        ),
        "a rejected proposal cannot be accepted, got {accept_after:?}"
    );

    assert_eq!(world.version_count().await, 1, "a reject mints no child");
    assert_eq!(world.run_count().await, 1, "a reject creates no run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejecting_an_accepted_proposal_is_refused() {
    let world = world().await;
    decide(&world, CoachAction::Accept)
        .await
        .expect("accept first");

    let outcome = decide(&world, CoachAction::Reject).await;

    assert!(
        matches!(
            outcome,
            Err(CoachDecisionError::NotActionable {
                current: pulse::DispositionKind::Accepted,
                ..
            })
        ),
        "an accepted proposal is settled, got {outcome:?}"
    );
    assert_eq!(
        world.settled_proposal_count().await,
        1,
        "the refused reject left the accept exactly where it was"
    );
}

// ===========================================================================
// 3. Accept — the committed child
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accept_mints_exactly_one_attributed_child_with_its_run_and_trades() {
    let world = world().await;
    let exchange = CountingExchange::default();

    let outcome = decide_with_exchange(&world, CoachAction::Accept, &exchange)
        .await
        .expect("the accept commits");
    let CoachDecisionOutcome::Accepted(accepted) = outcome else {
        panic!("expected Accepted, got {outcome:?}");
    };

    // Exactly one child, and its provenance is DERIVED from the session row.
    assert_eq!(world.version_count().await, 2, "exactly one child version");
    let (strategy_id, parent, created_by, calls): (String, Option<String>, String, String) =
        sqlx::query_as(
            "SELECT strategy_id, parent_version_id, created_by, creating_llm_call_ids \
             FROM strategy_version WHERE id = ?1",
        )
        .bind(accepted.child_version_id.as_str())
        .fetch_one(world.pool())
        .await
        .expect("the minted child exists");
    assert_eq!(
        parent.as_deref(),
        Some(world.version_id.as_str()),
        "the child is parented on the COACHED version"
    );
    assert_eq!(
        created_by, "\"coach_llm\"",
        "an accepted child is coach-made"
    );
    assert!(
        calls.contains("call-1"),
        "the child names the session's one llm_call, got {calls}"
    );
    assert!(!strategy_id.is_empty());

    // One run, of that child, carrying the prepared trades.
    assert_eq!(world.run_count().await, 2, "exactly one re-backtest run");
    let run_version: String =
        sqlx::query_scalar("SELECT strategy_version_id FROM backtest_run WHERE id = ?1")
            .bind(accepted.accepted_run_id.as_str())
            .fetch_one(world.pool())
            .await
            .expect("the minted run exists");
    assert_eq!(run_version, accepted.child_version_id.as_str());

    let child_trades = world
        .runs()
        .get_trades(&accepted.accepted_run_id)
        .await
        .expect("the child's trades read back");
    assert!(
        !child_trades.is_empty(),
        "the fixture strategy trades, so the child's run has a trade log"
    );

    // The child really is the MUTATED document.
    let child_dsl = version_dsl(&world, &accepted.child_version_id).await;
    assert_eq!(
        rsi_period(&child_dsl),
        21,
        "the child carries apply()'s output"
    );

    // The proposal now names both links and carries no accept failure.
    let proposal = world.proposal().await;
    assert_eq!(
        proposal.disposition,
        Disposition::Accepted {
            child_version_id: accepted.child_version_id.clone(),
            accepted_run_id: accepted.accepted_run_id.clone(),
        }
    );
    assert!(proposal.accept_failure.is_none());

    // The read back succeeded, so both summaries are present.
    assert!(accepted.read_back.is_ok(), "{:?}", accepted.read_back);
    assert!(accepted.after.is_some());

    // Symbol filters are the ONE thing the accept asks the exchange for.
    assert_eq!(
        exchange.calls(),
        1,
        "the accept resolves filters once and fetches nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_child_is_re_backtested_on_the_parents_exact_persisted_inputs() {
    let world = world().await;

    let outcome = decide(&world, CoachAction::Accept)
        .await
        .expect("the accept commits");
    let CoachDecisionOutcome::Accepted(accepted) = outcome else {
        panic!("expected Accepted, got {outcome:?}");
    };

    let child_run = world
        .runs()
        .get_run(&accepted.accepted_run_id)
        .await
        .expect("read the child run")
        .expect("the child run exists");
    let child_inputs = child_run
        .inputs
        .expect("a fresh run records its provenance");
    assert_eq!(
        child_inputs, world.parent_inputs,
        "the child ran on the PARENT run's exact persisted inputs — same pair, same \
         snapshot identities, same costs"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn before_and_after_are_the_two_persisted_run_summaries() {
    let world = world().await;

    let outcome = decide(&world, CoachAction::Accept)
        .await
        .expect("the accept commits");
    let CoachDecisionOutcome::Accepted(accepted) = outcome else {
        panic!("expected Accepted, got {outcome:?}");
    };

    assert_eq!(
        accepted.before, world.parent_summary,
        "`before` is the PARENT run's persisted summary, not a recomputation"
    );
    let child_run = world
        .runs()
        .get_run(&accepted.accepted_run_id)
        .await
        .expect("read the child run")
        .expect("present");
    assert_eq!(
        accepted.after.as_ref(),
        Some(&child_run.summary),
        "`after` is the CHILD run's persisted summary"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepting_twice_returns_the_same_ids_and_mints_nothing_new() {
    let world = world().await;
    // ONE exchange across both calls: the replay must not RE-COMPUTE, and resolving
    // symbol filters is the one thing the compute path always does. A second call
    // here would mean the replay re-applied and re-ran the engine before letting the
    // adapter's own idempotency catch it — which is a different guarantee from
    // "idempotency FIRST" (spec §Accept step 1).
    let exchange = CountingExchange::default();

    let first = decide_with_exchange(&world, CoachAction::Accept, &exchange)
        .await
        .expect("the accept commits");
    let CoachDecisionOutcome::Accepted(first) = first else {
        panic!("expected Accepted");
    };
    let versions = world.version_count().await;
    let runs = world.run_count().await;
    assert_eq!(exchange.calls(), 1, "the first accept computes once");

    let second = decide_with_exchange(&world, CoachAction::Accept, &exchange)
        .await
        .expect("replaying an accept is idempotent");
    let CoachDecisionOutcome::Accepted(second) = second else {
        panic!("expected Accepted, got {second:?}");
    };

    assert_eq!(second.child_version_id, first.child_version_id);
    assert_eq!(second.accepted_run_id, first.accepted_run_id);
    assert_eq!(second.before, first.before);
    assert_eq!(second.after, first.after);
    assert_eq!(world.version_count().await, versions, "no second child");
    assert_eq!(world.run_count().await, runs, "no second run");
    assert_eq!(
        exchange.calls(),
        1,
        "the replay applied, computed and wrote NOTHING — it answered from the \
         already-accepted proposal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accept_after_a_modify_re_applies_the_modified_mutation() {
    let world = world().await;

    decide(&world, CoachAction::Modify(proposed_mutation(30)))
        .await
        .expect("the trader edits the parameter");
    let outcome = decide(&world, CoachAction::Accept)
        .await
        .expect("the accept commits");
    let CoachDecisionOutcome::Accepted(accepted) = outcome else {
        panic!("expected Accepted, got {outcome:?}");
    };

    let child_dsl = version_dsl(&world, &accepted.child_version_id).await;
    assert_eq!(
        rsi_period(&child_dsl),
        30,
        "the accept re-applies the LATEST mutation, not the coach's original 21"
    );
}

// ===========================================================================
// 4. Accept — the recorded failures
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_0006_parent_records_load_inputs_and_mints_no_child() {
    let world = world().await;

    // Blank the parent run's eight provenance columns, exactly as a row written
    // before migration `0006` carries them. `0006`'s completeness trigger is BEFORE
    // INSERT only, and `backtest_run` is UPDATE-immutable, so the legacy shape is
    // reproduced by lifting the immutability trigger for one statement and putting
    // it straight back — the `coach_turn_boundary` precedent.
    make_parent_legacy(world.pool(), &world.parent_run_id).await;

    let outcome = decide(&world, CoachAction::Accept)
        .await
        .expect("a recorded failure is an outcome, not an error");

    assert_accept_failed(&outcome, AcceptFailureStage::LoadInputs);
    assert_eq!(world.version_count().await, 1, "no child was minted");
    assert_eq!(
        world.settled_proposal_count().await,
        0,
        "no settled row exists"
    );
    assert_eq!(
        world.proposal().await.disposition,
        Disposition::Proposed,
        "a failed accept leaves the proposal actionable"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_snapshot_records_load_snapshots_and_never_touches_the_exchange() {
    let world = world().await;
    // A parent run whose persisted inputs name a snapshot the store does not hold.
    // Nothing is re-fetched and no HEAD is consulted: the accept records the fact.
    repoint_parent_primary_snapshot(world.pool(), &world.parent_run_id, "v-does-not-exist").await;

    let exchange = CountingExchange::default();
    let outcome = decide_with_exchange(&world, CoachAction::Accept, &exchange)
        .await
        .expect("a recorded failure is an outcome, not an error");

    assert_accept_failed(&outcome, AcceptFailureStage::LoadSnapshots);
    assert_eq!(
        exchange.calls(),
        0,
        "a missing snapshot is recorded, never re-fetched"
    );
    assert_eq!(world.version_count().await, 1, "no child was minted");
    assert_eq!(
        world.settled_proposal_count().await,
        0,
        "no settled row exists"
    );
}

/// A gapped snapshot is refused here exactly as the standalone path refuses it.
///
/// The engine and the indicator stream assume contiguous bars and neither detects
/// nor fills a hole, so running the child on a gapped series would persist a
/// summary skewed by the hole and then show it beside its parent as the mutation's
/// effect. `backtest.rs` refuses the same series loudly; an accept replays the
/// parent's inputs, so it owes the parent's guards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gapped_snapshot_records_load_snapshots_and_mints_no_child() {
    let world = world().await;
    let exchange = CountingExchange::default();

    // Punch one candle out of the middle of the primary snapshot the parent names.
    // The series stays sorted and non-empty — the two conditions the accept path
    // used to check — and gains exactly one spacing gap.
    world.gap_primary_snapshot().await;

    let outcome = decide_with_exchange(&world, CoachAction::Accept, &exchange)
        .await
        .expect("a recorded failure is an outcome, not an error");

    assert_accept_failed(&outcome, AcceptFailureStage::LoadSnapshots);
    assert_eq!(
        exchange.calls(),
        0,
        "a gapped snapshot is recorded, never re-fetched"
    );
    assert_eq!(world.version_count().await, 1, "no child was minted");
    assert_eq!(
        world.settled_proposal_count().await,
        0,
        "no settled row exists"
    );
}

/// A parent produced by a different engine build refuses the accept.
///
/// The rail shows the two summaries side by side as before/after, so a delta the
/// engine caused would be read as the coach's change. The standalone path compares
/// the same two fingerprints; here the comparison IS the product, so a mismatch
/// refuses rather than warns — and it refuses before the commit, so no child exists
/// to mislead anyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_from_another_engine_build_refuses_the_accept() {
    let world = world().await;
    repoint_parent_engine_fingerprint(
        world.pool(),
        &world.parent_run_id,
        "0000000000000000000000000000000000000000000000000000000000000000",
    )
    .await;

    let outcome = decide(&world, CoachAction::Accept)
        .await
        .expect("a recorded failure is an outcome, not an error");

    assert_accept_failed(&outcome, AcceptFailureStage::Backtest);
    let CoachDecisionOutcome::AcceptFailed(proposal) = &outcome else {
        panic!("expected AcceptFailed, got {outcome:?}");
    };
    let failure = proposal
        .accept_failure
        .as_ref()
        .expect("the refusal is recorded on the proposal");
    assert_eq!(
        failure.subject.as_deref(),
        Some("engine fingerprint"),
        "the refusal names what diverged, not just that something did"
    );
    assert_eq!(world.version_count().await, 1, "no child was minted");
    assert_eq!(
        world.settled_proposal_count().await,
        0,
        "no settled row exists"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compute_failure_records_backtest_and_leaves_the_proposal_actionable() {
    let world = world().await;
    // The engine validates its cost knobs. A parent run whose persisted
    // `taker_fee_bps` is out of range makes the re-run refuse — a COMPUTE failure at
    // exactly the point the shared prepare step runs the engine.
    repoint_parent_taker_fee(world.pool(), &world.parent_run_id, "20000").await;

    let outcome = decide(&world, CoachAction::Accept)
        .await
        .expect("a recorded failure is an outcome, not an error");

    assert_accept_failed(&outcome, AcceptFailureStage::Backtest);
    assert_eq!(world.version_count().await, 1, "no child was minted");
    assert_eq!(
        world.settled_proposal_count().await,
        0,
        "no settled row exists"
    );

    // Still actionable: the trader can edit and try again.
    let modified = decide(&world, CoachAction::Modify(proposed_mutation(25)))
        .await
        .expect("the proposal is still open after a recorded accept failure");
    let CoachDecisionOutcome::Modified(proposal) = modified else {
        panic!("expected Modified, got {modified:?}");
    };
    assert!(
        proposal.accept_failure.is_none(),
        "a valid modify clears the stale accept failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_persist_failure_records_persist_in_a_new_transaction_with_no_child() {
    let world = world().await;
    // Refuse the child INSERT itself, so `commit_acceptance` fails after everything
    // deterministic has already succeeded and its whole transaction rolls back.
    sqlx::query(
        "CREATE TRIGGER _t_r1s4w2_refuse_child BEFORE INSERT ON strategy_version \
         WHEN NEW.created_by = '\"coach_llm\"' \
         BEGIN SELECT RAISE(ABORT, 'strategy_version: injected persist failure'); END",
    )
    .execute(world.pool())
    .await
    .expect("install the persist-failure trigger");

    let outcome = decide(&world, CoachAction::Accept)
        .await
        .expect("a recorded failure is an outcome, not an error");

    assert_accept_failed(&outcome, AcceptFailureStage::Persist);
    assert_eq!(world.version_count().await, 1, "no child exists");
    assert_eq!(world.run_count().await, 1, "and no run");
    assert_eq!(
        world.settled_proposal_count().await,
        0,
        "no settled row exists"
    );
    // The failure was recorded in a NEW transaction — the one the commit used had
    // already rolled back, so a record written on it would not be there at all.
    assert_eq!(
        world.proposal().await.accept_failure.map(|f| f.stage),
        Some(AcceptFailureStage::Persist)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_back_failure_after_the_commit_is_still_an_accepted_outcome() {
    let world = world().await;

    let runs = ReadBackFailingRuns {
        inner: world.runs(),
        readable: world.parent_run_id.clone(),
    };
    let outcome = run_coach_decision(
        &world.strategies(),
        &world.store,
        &CountingExchange::default(),
        &runs,
        &world.acceptance(),
        &world.sessions(),
        CoachDecisionRequest {
            session_id: world.session_id.clone(),
            action: CoachAction::Accept,
        },
    )
    .await
    .expect("a read-back failure is not an accept failure");

    let CoachDecisionOutcome::Accepted(accepted) = outcome else {
        panic!("expected Accepted, got {outcome:?}");
    };
    assert!(
        accepted.after.is_none(),
        "the child run could not be read, so there is no `after` to show"
    );
    assert!(
        matches!(accepted.read_back, Err(ReadBackFailure::Data(_))),
        "the read-back error is carried, not swallowed: {:?}",
        accepted.read_back
    );
    assert_eq!(
        accepted.before, world.parent_summary,
        "`before` still comes from the persisted parent run"
    );

    // The accept SUCCEEDED: both rows are there, and nothing was recorded as a
    // failure on the proposal (`0008` forbids it on an accepted row anyway).
    assert_eq!(world.version_count().await, 2);
    assert_eq!(world.run_count().await, 2);
    let proposal = world.proposal().await;
    assert_eq!(
        proposal.disposition,
        Disposition::Accepted {
            child_version_id: accepted.child_version_id.clone(),
            accepted_run_id: accepted.accepted_run_id.clone(),
        }
    );
    assert!(proposal.accept_failure.is_none());
}

// ===========================================================================
// 5. Determinism and the one writer
// ===========================================================================

/// Two COLD databases accepting the same fixture proposal produce byte-identical
/// child summaries (the r1.s3 two-cold-run determinism pattern).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_cold_databases_produce_byte_identical_child_summaries() {
    let mut rendered = Vec::new();
    let mut hashes = Vec::new();
    for _ in 0..2 {
        let world = world().await;
        let outcome = decide(&world, CoachAction::Accept)
            .await
            .expect("the accept commits");
        let CoachDecisionOutcome::Accepted(accepted) = outcome else {
            panic!("expected Accepted");
        };
        let run = world
            .runs()
            .get_run(&accepted.accepted_run_id)
            .await
            .expect("read the child run")
            .expect("present");
        rendered.push(serde_json::to_string(&run.summary).unwrap());
        hashes.push(run.result_content_hash);
    }

    assert_eq!(
        rendered[0], rendered[1],
        "two cold accepts of the same proposal must render the same summary bytes"
    );
    assert_eq!(
        hashes[0], hashes[1],
        "and the same result content hash — the computation is the shared one"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_disposition_refuses_to_be_a_second_writer_of_accepted_lineage() {
    let world = world().await;

    // Accept properly first, so a real child and run exist to point at.
    let outcome = decide(&world, CoachAction::Accept)
        .await
        .expect("the accept commits");
    let CoachDecisionOutcome::Accepted(accepted) = outcome else {
        panic!("expected Accepted");
    };

    // Even naming the EXACT pair `commit_acceptance` just wrote, the disposition
    // path refuses: it is not a writer of accepted lineage at all.
    let replay = world
        .sessions()
        .record_disposition(
            &world.session_id,
            &Disposition::Accepted {
                child_version_id: accepted.child_version_id.clone(),
                accepted_run_id: accepted.accepted_run_id.clone(),
            },
        )
        .await;
    assert!(
        replay.is_err(),
        "record_disposition must refuse an accepted disposition outright"
    );

    // And the in-memory acceptance adapter mints through the SAME one operation.
    let memory = pulse::InMemoryCoachAcceptanceRepo::new(
        FakeClock::at(NOW_MS),
        SeqIdSource::with_prefix("mem"),
    );
    assert!(
        memory
            .accepted_children()
            .expect("read the in-memory children")
            .is_empty(),
        "nothing writes accepted lineage into the in-memory adapter except \
         commit_acceptance"
    );
}

// ===========================================================================
// 6. Refusals with nothing to record against
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absent_session_and_a_failed_turn_are_typed_refusals() {
    let world = world().await;

    let missing = run_coach_decision(
        &world.strategies(),
        &world.store,
        &CountingExchange::default(),
        &world.runs(),
        &world.acceptance(),
        &world.sessions(),
        CoachDecisionRequest {
            session_id: CoachingSessionId::new("sess-nope"),
            action: CoachAction::Accept,
        },
    )
    .await;
    assert!(
        matches!(missing, Err(CoachDecisionError::SessionNotFound(_))),
        "got {missing:?}"
    );

    // A turn that FAILED has no proposal to decide on.
    let sessions = world.sessions();
    sessions
        .claim_session(CoachSessionClaim {
            session_id: CoachingSessionId::new("sess-failed"),
            backtest_run_id: world.parent_run_id.clone(),
            strategy_version_id: world.version_id.clone(),
            request_fingerprint: CoachRequestFingerprint::new(
                "bb11bb22cc33dd44ee55ff6600778899aabbccddeeff00112233445566778899",
            )
            .unwrap(),
            created_at: "2026-08-29T00:00:00.000Z".to_owned(),
        })
        .await
        .expect("claim");
    sessions
        .finish_session(
            &CoachingSessionId::new("sess-failed"),
            InitialCoachOutcome {
                llm_call_id: None,
                outcome: SessionOutcome::Failed {
                    failure: pulse::CoachFailure::ZeroCalls,
                },
            },
        )
        .await
        .expect("settle as failed");

    let failed = run_coach_decision(
        &world.strategies(),
        &world.store,
        &CountingExchange::default(),
        &world.runs(),
        &world.acceptance(),
        &world.sessions(),
        CoachDecisionRequest {
            session_id: CoachingSessionId::new("sess-failed"),
            action: CoachAction::Reject,
        },
    )
    .await;
    assert!(
        matches!(failed, Err(CoachDecisionError::NoProposal(_))),
        "got {failed:?}"
    );
}

// ---------------------------------------------------------------------------
// Raw-SQL surgery on the immutable parent run
// ---------------------------------------------------------------------------

/// Blank the parent run's `0006` provenance columns — the pre-`0006` legacy shape.
async fn make_parent_legacy(pool: &SqlitePool, run_id: &BacktestRunId) {
    coach_support::with_run_immutability_lifted(
        pool,
        &[&format!(
            "UPDATE backtest_run SET pair = NULL, primary_timeframe = NULL, \
             primary_data_version = NULL, htf_timeframe = NULL, htf_data_version = NULL, \
             taker_fee_bps = NULL, slippage_bps = NULL, funding_config = NULL \
             WHERE id = '{}'",
            run_id.as_str()
        )],
    )
    .await;
}

/// Point the parent run's persisted primary snapshot at a version the store does not
/// hold.
async fn repoint_parent_primary_snapshot(pool: &SqlitePool, run_id: &BacktestRunId, to: &str) {
    coach_support::with_run_immutability_lifted(
        pool,
        &[&format!(
            "UPDATE backtest_run SET primary_data_version = '{to}' WHERE id = '{}'",
            run_id.as_str()
        )],
    )
    .await;
}

/// Put the parent run's persisted taker fee outside the engine's accepted range.
async fn repoint_parent_taker_fee(pool: &SqlitePool, run_id: &BacktestRunId, to: &str) {
    coach_support::with_run_immutability_lifted(
        pool,
        &[&format!(
            "UPDATE backtest_run SET taker_fee_bps = '{to}' WHERE id = '{}'",
            run_id.as_str()
        )],
    )
    .await;
}

/// Stamp the parent run with an engine fingerprint this build did not produce.
///
/// The real shape is an app upgrade between the parent run and the accept; a
/// fabricated hex is the same condition without waiting for one.
async fn repoint_parent_engine_fingerprint(pool: &SqlitePool, run_id: &BacktestRunId, to: &str) {
    coach_support::with_run_immutability_lifted(
        pool,
        &[&format!(
            "UPDATE backtest_run SET engine_fingerprint = '{to}' WHERE id = '{}'",
            run_id.as_str()
        )],
    )
    .await;
}

/// Assert an outcome is a recorded accept failure at `stage`.
fn assert_accept_failed(outcome: &CoachDecisionOutcome, stage: AcceptFailureStage) {
    let CoachDecisionOutcome::AcceptFailed(proposal) = outcome else {
        panic!("expected AcceptFailed({stage:?}), got {outcome:?}");
    };
    let failure = proposal
        .accept_failure
        .as_ref()
        .expect("a recorded accept failure rides the proposal");
    assert_eq!(
        failure.stage, stage,
        "recorded at the wrong stage: {failure}"
    );
    assert!(
        matches!(
            proposal.disposition,
            Disposition::Proposed | Disposition::Modified
        ),
        "a failed accept leaves the proposal open, got {:?}",
        proposal.disposition
    );
}
