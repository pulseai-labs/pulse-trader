//! r2.s3.w4 — AC-1: the coach accept's certification gate at the one seam.
//!
//! A certified parent's child must survive the parent's own walk-forward
//! parameters BEFORE anything commits; an uncertified parent's accept is
//! exactly r1.s4's. The fixture cannot produce a passing verdict (one month of
//! M15 cannot reach `n >= 20` per fold), so the gate's REFUSAL is proven end to
//! end, and the pass path is proven at the repository seam with a synthetic
//! passing draft (AC-1(v)) — `commit_acceptance` persists child + accepted run
//! + walk-forward parent + folds + pointer in one transaction.
//!
//! i.   An uncertified parent's accept runs no gate: no walk-forward rows, a
//!      `walk_forward_run_id: None` outcome, an uncertified child.
//! ii.  A certified parent whose candidate fails `wf-v1` records
//!      `AcceptFailureStage::WalkForward` — message naming scheme, k, span,
//!      rule, fold counts and the pooled lower bound — and persists NO child,
//!      ordinary run, walk-forward parent, fold or fold-run rows.
//! iii. The same holds when the gate's computation errors (a certifying run
//!      whose row cannot be read back fails closed).
//! iv.  `commit_acceptance` with `walk_forward: Some(draft)` mints the child,
//!      writes the walk-forward rows, advances the child's pointer and reports
//!      the run id — the child reads certified on the very next load.
//! v.   Replaying an accepted proposal returns the same ids, and
//!      `walk_forward_run_id` is the child's CURRENT pointer — a later
//!      walk-forward may have advanced it since the accept.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod coach_support;
mod support;

use pulse::{
    AcceptFailureStage, BacktestConfig, BacktestInputs, BacktestRequest, BacktestResult,
    BinanceAdapter, CandleStore, CandleWindow, CoachAcceptanceRepository, CoachAction,
    CoachDecisionOutcome, CoachDecisionRequest, CoachRequestFingerprint, CoachSessionClaim,
    CoachingRepository, CoachingSessionId, CreatedBy, Disposition, EngineFingerprint, EquityCurve,
    FakeClock, FoldScheme, FoldVerdict, Hypothesis, InitialCoachOutcome, LlmCallId, MIGRATOR,
    Mutation, NewVersion, Pair, ParamValue, PreparedBacktest, PreparedCoachAcceptance, Proposal,
    RegimeBreakdown, RunVerdict, SeqIdSource, SessionOutcome, SkippedEntryCounts,
    SqliteBacktestRunRepo, SqliteCoachAcceptanceRepo, SqliteCoachingRepo, SqliteStrategyRepo,
    StrategyRepository, SummaryStats, Timeframe, VerdictRule, VersionId, WalkForwardFoldDraft,
    WalkForwardRunDraft, WalkForwardRunRepository, fold_windows, folds_required,
    run_coach_decision, run_version_backtest,
};
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use support::mcp::{FIXTURE_STORE, copy_tree, manifest, seeded_trade};
use tempfile::TempDir;

/// A pinned instant, so `created_at` is deterministic everywhere.
const NOW_MS: i64 = 1_756_425_600_000; // 2026-08-29T00:00:00Z

/// The certifying run's timestamp — the pointer orders on `seq` alone, so
/// this is a later SAVE, not merely a later instant.
const CERTIFY_MS: i64 = 1_756_512_000_000; // 2026-08-30T00:00:00Z

/// The one-and-only request fingerprint the fixture session claims under.
const FINGERPRINT: &str = "aa11bb22cc33dd44ee55ff6600778899aabbccddeeff00112233445566778899";

/// The sweepable leaf every fixture mutation addresses.
const RSI_PERIOD: &str = "entry.lhs.indicator.rsi.period";

/// The same minimal, valid DSL `coach_decision.rs` uses — it produces real
/// trades over the fixture, so the parent run is a genuine one.
const MINIMAL_DSL: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold (gate)",
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
// The fixture world (the coach_decision.rs shape)
// ---------------------------------------------------------------------------

struct World {
    _tmp: TempDir,
    db: pulse::Db,
    store: CandleStore,
    version_id: VersionId,
    parent_inputs: BacktestInputs,
    session_id: CoachingSessionId,
}

impl World {
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

    async fn table_count(&self, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(self.pool())
            .await
            .unwrap()
    }

    /// The version's persisted certification pair `(certified, pointer)`.
    async fn certification(&self, version: &VersionId) -> (bool, Option<String>) {
        let row: (Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT v.latest_walk_forward_run_id, w.pass \
             FROM strategy_version v \
             LEFT JOIN walk_forward_run w ON w.id = v.latest_walk_forward_run_id \
             WHERE v.id = ?1",
        )
        .bind(version.as_str())
        .fetch_one(self.pool())
        .await
        .expect("the version row reads");
        (row.1.unwrap_or(0) != 0, row.0)
    }
}

async fn world() -> World {
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
    // what lets the gate's snapshot pins resolve.
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
// The synthetic certifying run (AC-1(v)'s lever)
// ---------------------------------------------------------------------------

/// A walk-forward draft whose recorded verdict PASSES — two folds, both
/// holding, over the parent run's own inputs. The fixture cannot produce this
/// verdict honestly, so the draft is built by hand: what `save_walk_forward_run`
/// persists is the recorded verdict, and the certification seam is the pointer
/// plus that `pass` column — which is exactly what a synthetic draft exercises.
fn passing_draft(inputs: &BacktestInputs, span: CandleWindow) -> WalkForwardRunDraft {
    draft_with_fold_verdict(inputs, span, true)
}

/// A synthetic draft whose recorded run verdict AGREES with its own fold
/// verdicts — the coherence the persistence boundary now demands (F5's
/// `validate_draft`: `folds_holding` counts the folds' `holds`, `folds_required`
/// is the scheme's `⌈2K/3⌉`, and `pass` follows both). `folds_hold` picks the
/// two coherent extremes the tests need: every fold holding (the run passes) or
/// none holding (the run fails, so a save de-certifies).
fn draft_with_fold_verdict(
    inputs: &BacktestInputs,
    span: CandleWindow,
    folds_hold: bool,
) -> WalkForwardRunDraft {
    let k = 2_u8;
    let holds = FoldVerdict {
        n: 32,
        mean_r: if folds_hold {
            Decimal::new(45, 2)
        } else {
            Decimal::new(-30, 2)
        },
        lower_bound: if folds_hold { 0.21 } else { -0.4 },
        holds: folds_hold,
    };
    let folds = fold_windows(&span, k)
        .iter()
        .enumerate()
        .map(|(i, window)| {
            let trade = seeded_trade();
            let summary = SummaryStats::from_trades(
                std::slice::from_ref(&trade),
                trade.realized_pnl,
                trade.fees_total,
                trade.funding_total,
                &EquityCurve::default(),
            );
            let mut fold_inputs = inputs.clone();
            fold_inputs.window = Some(window.clone());
            fold_inputs.lead_in_from_ms = Some(window.from_ms);
            WalkForwardFoldDraft {
                index: u8::try_from(i).unwrap(),
                window: window.clone(),
                verdict: holds.clone(),
                inputs: fold_inputs,
                result: BacktestResult {
                    trades: vec![trade.clone()],
                    net_pnl: trade.realized_pnl,
                    fees_total: trade.fees_total,
                    funding_total: trade.funding_total,
                    slippage_total: trade.slippage_total,
                    regime_breakdown: RegimeBreakdown::new(),
                    skipped_entries: SkippedEntryCounts::new(),
                    open_position: None,
                    engine_fingerprint: EngineFingerprint::current(),
                    summary: summary.clone(),
                    equity_curve: EquityCurve::default(),
                },
                summary,
                starting_equity: Decimal::new(10_000, 0),
            }
        })
        .collect();
    let folds_holding = if folds_hold { k } else { 0 };
    let required = folds_required(k);
    // Computed before `holds` moves into `pooled` below.
    let pass = folds_holding >= required && holds.holds;
    WalkForwardRunDraft {
        scheme: FoldScheme::rolling_oos(i64::from(k)).unwrap(),
        rule: VerdictRule::WfV1,
        span,
        from_defaulted: false,
        engine_fingerprint: EngineFingerprint::current().as_str().to_owned(),
        verdict: RunVerdict {
            folds_holding,
            folds_required: required,
            pooled: holds,
            pass,
        },
        folds,
    }
}

/// The walkable span the fixture's M15 snapshot covers, from the candle at
/// `from_idx` to the last candle's close — the same bounds `run_walk_forward`
/// resolves by default. `from_idx` must sit at or past the warm bar of every
/// DSL the test walks (the candidate's own warm point is what the gate's
/// span has to clear).
fn fixture_span_from(from_idx: usize) -> CandleWindow {
    let store = CandleStore::with_base_dir(manifest(FIXTURE_STORE));
    let head = store
        .read_head(&Pair::new("BTCUSDT"), Timeframe::M15)
        .expect("read HEAD")
        .expect("fixture HEAD present");
    let series = store
        .read_snapshot(&Pair::new("BTCUSDT"), Timeframe::M15, &head)
        .expect("read fixture snapshot");
    CandleWindow::new(
        series.candles[from_idx].open_time,
        series.candles.last().unwrap().close_time,
    )
    .expect("the span is ordered")
}

/// The certifying span this suite uses: candle 30 onward, past the warm bar of
/// both the RSI(14) parent and the RSI(21) candidate the accept applies — so
/// the gate's own walk-forward RUNS and its verdict is what decides.
fn fixture_span() -> CandleWindow {
    fixture_span_from(30)
}

/// Certify the parent: one synthetic passing walk-forward, saved through the
/// real repository so the pointer lands exactly as the product moves it.
async fn certify_parent(world: &World) -> pulse::WalkForwardRunId {
    let repo = SqliteBacktestRunRepo::with_deps(world.pool().clone(), FakeClock::at(CERTIFY_MS));
    repo.save_walk_forward_run(
        &world.version_id,
        &passing_draft(&world.parent_inputs, fixture_span()),
    )
    .await
    .expect("the certifying run persists")
}

/// The payload a passed gate hands `commit_acceptance` — the proposal's own
/// mutation as the optimistic lock, the applied RSI(21) child DSL, a prepared
/// backtest over the parent's inputs, and the passed walk-forward draft.
/// Built once per test so the pass path and both rollback pins share it.
fn acceptance_payload(world: &World) -> PreparedCoachAcceptance {
    let mut child_dsl_json: serde_json::Value =
        serde_json::from_str(MINIMAL_DSL).expect("the fixture dsl parses");
    child_dsl_json["entry"]["lhs"]["spec"]["period"] = serde_json::json!(21);
    let child_dsl = pulse::Migrator::v1()
        .load(&child_dsl_json.to_string())
        .expect("the child dsl loads")
        .dsl;

    let trade = seeded_trade();
    let summary = SummaryStats::from_trades(
        std::slice::from_ref(&trade),
        trade.realized_pnl,
        trade.fees_total,
        trade.funding_total,
        &EquityCurve::default(),
    );
    PreparedCoachAcceptance {
        session_id: world.session_id.clone(),
        expected_mutation: proposed_mutation(21),
        // The direct-commit tests run against an uncertified parent — `None` is
        // the pointer the accept read. Cases that certify the parent go through
        // `decide`, which reads the pointer itself.
        expected_certification_pointer: None,
        child_dsl,
        prepared_run: PreparedBacktest {
            inputs: world.parent_inputs.clone(),
            result: BacktestResult {
                trades: vec![trade.clone()],
                net_pnl: trade.realized_pnl,
                fees_total: trade.fees_total,
                funding_total: trade.funding_total,
                slippage_total: trade.slippage_total,
                regime_breakdown: RegimeBreakdown::new(),
                skipped_entries: SkippedEntryCounts::new(),
                open_position: None,
                engine_fingerprint: EngineFingerprint::current(),
                summary: summary.clone(),
                equity_curve: EquityCurve::default(),
            },
            summary,
            starting_equity: Decimal::new(10_000, 0),
        },
        walk_forward: Some(passing_draft(&world.parent_inputs, fixture_span())),
    }
}

/// The five counts an accept writes to — every rollback pin asserts the whole
/// tuple, so nothing can land while the test claims "nothing persisted".
async fn accept_counts(world: &World) -> (i64, i64, i64, i64, i64) {
    (
        world.table_count("strategy_version").await,
        world.table_count("backtest_run").await,
        world.table_count("walk_forward_run").await,
        world.table_count("walk_forward_fold").await,
        world.table_count("trade").await,
    )
}

// ===========================================================================
// AC-1: the gate
// ===========================================================================

/// i. An uncertified parent's accept is exactly r1.s4's: no gate runs, no
/// walk-forward rows exist, the outcome names no certifying run and the child
/// is born uncertified.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uncertified_parents_accept_runs_no_gate() {
    let world = world().await;
    let (certified, pointer) = world.certification(&world.version_id).await;
    assert!(!certified && pointer.is_none(), "the parent is uncertified");

    let outcome = decide(&world, CoachAction::Accept).await;
    let CoachDecisionOutcome::Accepted(accepted) = outcome else {
        panic!("expected Accepted, got {outcome:?}");
    };

    assert_eq!(
        accepted.walk_forward_run_id, None,
        "no gate ran, so no walk-forward run exists to name"
    );
    assert_eq!(
        world.table_count("walk_forward_run").await,
        0,
        "an uncertified accept writes no walk-forward rows"
    );
    let (child_certified, child_pointer) = world.certification(&accepted.child_version_id).await;
    assert!(
        !child_certified && child_pointer.is_none(),
        "a child of an uncertified parent is born uncertified"
    );
}

/// ii. A certified parent whose candidate fails `wf-v1` gets a recorded
/// `WalkForward` failure — and NOTHING persists: no child, no accepted run, no
/// second walk-forward parent, no folds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_gate_records_walk_forward_and_persists_nothing() {
    let world = world().await;
    let certifying = certify_parent(&world).await;
    let (certified, pointer) = world.certification(&world.version_id).await;
    assert!(
        certified && pointer.as_deref() == Some(certifying.as_str()),
        "the parent is certified by the synthetic passing run"
    );

    // The persisted state BEFORE the gate runs — the refusal must leave every
    // one of these untouched.
    let before = accept_counts(&world).await;

    let outcome = decide(&world, CoachAction::Accept).await;
    let CoachDecisionOutcome::AcceptFailed(proposal) = outcome else {
        panic!("the fixture cannot pass wf-v1 — expected AcceptFailed, got {outcome:?}");
    };

    let failure = proposal
        .accept_failure
        .as_ref()
        .expect("the failure is recorded on the still-open proposal");
    assert_eq!(
        failure.stage,
        AcceptFailureStage::WalkForward,
        "the stage is the gate's own vocabulary word"
    );
    assert_eq!(
        failure.subject.as_deref(),
        Some("walk-forward"),
        "the subject names the gate, not a row"
    );
    // AC-1(iii): the detail names the CERTIFYING run's parameters — the scheme
    // and k AND the exact span milliseconds, not a re-resolved or defaulted
    // span.
    let span = fixture_span();
    for needle in [
        "rolling-oos/v1".to_owned(),
        "k=2".to_owned(),
        format!("[{}, {})", span.from_ms, span.to_ms),
        "wf-v1".to_owned(),
        "folds_holding".to_owned(),
        "folds_required".to_owned(),
        "pooled lower bound".to_owned(),
    ] {
        assert!(
            failure.message.contains(&needle),
            "the refusal message names {needle:?}: {}",
            failure.message
        );
    }

    let after = accept_counts(&world).await;
    assert_eq!(
        before, after,
        "a refused gate persists nothing: no child, no runs, no folds"
    );
    assert_eq!(
        proposal.disposition,
        Disposition::Proposed,
        "the proposal is still actionable after a gate refusal"
    );

    // AC-1(ii): a SECOND accept refuses identically and still adds nothing —
    // the recorded failure did not settle or exhaust the proposal.
    let second = decide(&world, CoachAction::Accept).await;
    let CoachDecisionOutcome::AcceptFailed(retried) = second else {
        panic!("the second accept refuses identically, got {second:?}");
    };
    assert_eq!(
        retried.accept_failure.as_ref().map(|f| &f.stage),
        Some(&AcceptFailureStage::WalkForward),
        "the retry records the same stage"
    );
    assert_eq!(
        accept_counts(&world).await,
        before,
        "the second refusal also persists nothing"
    );
    assert_eq!(retried.disposition, Disposition::Proposed);

    // And the parent is still certified — a failed CHILD walk-forward never ran,
    // so nothing moved the parent's pointer.
    let (still_certified, same_pointer) = world.certification(&world.version_id).await;
    assert!(still_certified && same_pointer.as_deref() == Some(certifying.as_str()));
}

/// AC-1(iv). A certified parent whose certifying row cannot be read fails the
/// gate closed: `WalkForward`, recorded, nothing persisted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_certifying_run_fails_the_gate_closed() {
    let world = world().await;
    let certifying = certify_parent(&world).await;

    // Corrupt the certifying run's scheme: the fail-closed read can no longer
    // decode it. The pointer still names it — exactly the state the gate must
    // refuse honestly rather than walk past.
    coach_support::with_trigger_lifted(
        world.pool(),
        "walk_forward_run_no_update",
        &[&format!(
            "UPDATE walk_forward_run SET scheme = 'bogus/v9' WHERE id = '{}'",
            certifying.as_str()
        )],
    )
    .await;

    let outcome = decide(&world, CoachAction::Accept).await;
    let CoachDecisionOutcome::AcceptFailed(proposal) = outcome else {
        panic!("expected AcceptFailed on an unreadable certifying run, got {outcome:?}");
    };
    let failure = proposal.accept_failure.expect("the failure is recorded");
    assert_eq!(failure.stage, AcceptFailureStage::WalkForward);

    assert_eq!(
        world.table_count("strategy_version").await,
        1,
        "no child was minted"
    );
}

/// iv. AC-1(v): `commit_acceptance` with a passing draft commits child + run +
/// walk-forward + folds + pointer in one transaction — and the child reads
/// certified on its very first load.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_passed_gate_persists_the_whole_shape_atomically() {
    let world = world().await;

    let outcome = world
        .acceptance()
        .commit_acceptance(acceptance_payload(&world))
        .await
        .expect("the commit lands");

    let wf_id = outcome
        .walk_forward_run_id
        .as_ref()
        .expect("the committed outcome names the certifying run");

    // The whole shape persisted: one new version, the accepted run, the
    // walk-forward parent, its two folds, and the fold runs' membership.
    assert_eq!(world.table_count("strategy_version").await, 2);
    assert_eq!(world.table_count("walk_forward_run").await, 1);
    assert_eq!(world.table_count("walk_forward_fold").await, 2);
    let fold_runs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM backtest_run WHERE walk_forward_run_id = ?1")
            .bind(wf_id.as_str())
            .fetch_one(world.pool())
            .await
            .unwrap();
    assert_eq!(fold_runs, 2, "both folds joined the ordinary run log");

    // The child is certified — derived, not stored: the pointer names the new
    // run and the joined `pass` reads true.
    let (certified, pointer) = world.certification(&outcome.child_version_id).await;
    assert_eq!(pointer.as_deref(), Some(wf_id.as_str()));
    assert!(certified, "the child reads certified the moment it exists");

    // The proposal settled AFTER every referenced row existed — the links point
    // at real children.
    let proposal = world.proposal().await;
    match proposal.disposition {
        Disposition::Accepted {
            child_version_id,
            accepted_run_id,
        } => {
            assert_eq!(child_version_id, outcome.child_version_id);
            assert_eq!(accepted_run_id, outcome.accepted_run_id);
        }
        other => panic!("the proposal settled accepted, got {other:?}"),
    }
}

/// v. AC-1(v)'s injected mid-transaction failure: a fold draft whose run lands
/// with a NULL window pair is refused by 0013's `walk_forward_fold_windowed`
/// trigger — INSIDE the transaction, after the child row exists — and the whole
/// accept rolls back: no child, no run, no walk-forward rows, no pointer.
///
/// (`inputs.window` and `inputs.lead_in_from_ms` are cleared together: a lead-in
/// with no window trips 0012's pair trigger on the fold's run insert, one
/// statement EARLIER than the fold trigger this test names.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_null_windowed_fold_rolls_the_whole_accept_back() {
    let world = world().await;
    let before = accept_counts(&world).await;

    let mut payload = acceptance_payload(&world);
    let draft = payload.walk_forward.as_mut().expect("the draft is present");
    draft.folds[0].inputs.window = None;
    draft.folds[0].inputs.lead_in_from_ms = None;

    let err = world
        .acceptance()
        .commit_acceptance(payload)
        .await
        .expect_err("0013's fold trigger refuses a windowless fold run");
    assert!(
        err.to_string().contains("windowed backtest_run"),
        "the refusal is the fold trigger's own message: {err}"
    );

    assert_eq!(
        accept_counts(&world).await,
        before,
        "the abort rolls everything back: no child, no runs, no folds"
    );
    assert_eq!(
        world.proposal().await.disposition,
        Disposition::Proposed,
        "the proposal settlement was inside the rolled-back transaction too"
    );
    let (certified, pointer) = world.certification(&world.version_id).await;
    assert!(
        !certified && pointer.is_none(),
        "the parent's certification state is untouched"
    );
}

/// vi. The M8 pin: the pointer UPDATE lives INSIDE the transaction. A test-only
/// trigger aborts `UPDATE OF latest_walk_forward_run_id`, so the commit must
/// fail and leave NOTHING behind — no child, no run, no walk-forward rows.
/// (A fault injected EARLIER cannot distinguish this: under a mutant that moved
/// the UPDATE after `tx.commit()`, an aborted transaction never reaches it —
/// the pointer UPDATE failing is the only shape that sees the difference.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_pointer_update_rolls_the_whole_accept_back() {
    let world = world().await;
    sqlx::query(
        "CREATE TRIGGER w4_test_pointer_update_aborts \
         BEFORE UPDATE OF latest_walk_forward_run_id ON strategy_version \
         BEGIN \
           SELECT RAISE(ABORT, 'w4 test: the pointer update is refused'); \
         END",
    )
    .execute(world.pool())
    .await
    .expect("install the pointer-update abort trigger");
    let before = accept_counts(&world).await;

    let err = world
        .acceptance()
        .commit_acceptance(acceptance_payload(&world))
        .await
        .expect_err("the aborted pointer update fails the commit");
    assert!(
        err.to_string().contains("the pointer update is refused"),
        "the refusal is the test trigger's own message: {err}"
    );

    assert_eq!(
        accept_counts(&world).await,
        before,
        "the pointer write is inside the transaction: its failure rolls back \
         the child, the run, the walk-forward parent and every fold"
    );
    assert_eq!(
        world.proposal().await.disposition,
        Disposition::Proposed,
        "the proposal settlement rolled back with it"
    );

    // `IF EXISTS`: the drop may land on a pooled connection whose WAL snapshot
    // predates the CREATE (#153's stale-snapshot class); the tempdir dies with
    // the test either way, so a no-op drop is harmless.
    sqlx::query("DROP TRIGGER IF EXISTS w4_test_pointer_update_aborts")
        .execute(world.pool())
        .await
        .expect("remove the test trigger");
}

/// Replaying an accepted proposal answers with the child's CURRENT pointer:
/// a later walk-forward that advanced it is what the replay reports, not the
/// run the original accept wrote. (Additional coverage beyond the spec's
/// AC-1(i)–(vi) list — the D5 ruling's pin.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replay_reports_the_childs_current_pointer() {
    let world = world().await;

    // Commit the same passed-gate shape as (iv) through the real adapter.
    let committed = world
        .acceptance()
        .commit_acceptance(acceptance_payload(&world))
        .await
        .expect("the commit lands");
    let original_wf = committed.walk_forward_run_id.clone().unwrap();

    // The replay returns the original accept's ids.
    let first = decide(&world, CoachAction::Accept).await;
    let CoachDecisionOutcome::Accepted(replayed) = first else {
        panic!("an accepted proposal replays, got {first:?}");
    };
    assert_eq!(replayed.child_version_id, committed.child_version_id);
    assert_eq!(replayed.accepted_run_id, committed.accepted_run_id);
    assert_eq!(
        replayed
            .walk_forward_run_id
            .as_ref()
            .map(pulse::WalkForwardRunId::as_str),
        Some(original_wf.as_str()),
    );

    // A LATER walk-forward on the child — this one failing — advances the
    // pointer; the replay must answer with the new run, not the original. The
    // failing draft is COHERENT: its folds do not hold either, because the
    // persistence boundary refuses a run whose recorded verdict its own fold
    // verdicts contradict (F5's `validate_draft`).
    let failing = draft_with_fold_verdict(&world.parent_inputs, fixture_span(), false);
    let later =
        SqliteBacktestRunRepo::with_deps(world.pool().clone(), FakeClock::at(CERTIFY_MS + 60_000))
            .save_walk_forward_run(&committed.child_version_id, &failing)
            .await
            .expect("the later failing run persists");

    let second = decide(&world, CoachAction::Accept).await;
    let CoachDecisionOutcome::Accepted(replayed) = second else {
        panic!("the second replay resolves, got {second:?}");
    };
    assert_eq!(
        replayed
            .walk_forward_run_id
            .as_ref()
            .map(pulse::WalkForwardRunId::as_str),
        Some(later.as_str()),
        "the replay names the child's CURRENT pointer, not the accept's run"
    );
    // And the child is no longer certified — the newer failing run revoked it.
    let (certified, _) = world.certification(&committed.child_version_id).await;
    assert!(
        !certified,
        "a newer failing walk-forward de-certifies the child"
    );
}
