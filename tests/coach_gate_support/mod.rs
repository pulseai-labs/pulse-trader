//! The certified-parent accept harness (r4.s1.w4 extraction).
//!
//! Moved verbatim out of `tests/coach_walk_forward_gate.rs` so the holdout
//! guard's coach-gate case (`tests/holdout_guard.rs`, demo line d63) can drive
//! the same fixture: one strategy version, a REAL parent run over the copied
//! BTCUSDT fixture, a proposed coaching session, and the synthetic
//! passing-walk-forward lever `certify_parent` uses. The gate tests keep their
//! own test bodies and their `acceptance_payload` / `accept_counts` helpers;
//! everything here is shared fixture state.
#![allow(dead_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{
    BacktestConfig, BacktestInputs, BacktestRequest, BacktestResult, BinanceAdapter, CandleStore,
    CandleWindow, CoachAction, CoachDecisionOutcome, CoachDecisionRequest, CoachRequestFingerprint,
    CoachSessionClaim, CoachingRepository, CoachingSessionId, CreatedBy, Disposition,
    EngineFingerprint, EquityCurve, FakeClock, FoldScheme, FoldVerdict, HoldoutFreeze, Hypothesis,
    InitialCoachOutcome, LlmCallId, MIGRATOR, Mutation, NewVersion, Pair, ParamValue, Proposal,
    RegimeBreakdown, RunVerdict, SeqIdSource, SessionOutcome, SkippedEntryCounts,
    SqliteBacktestRunRepo, SqliteCoachAcceptanceRepo, SqliteCoachingRepo, SqliteStrategyRepo,
    StrategyRepository, SummaryStats, Timeframe, VerdictRule, VersionId, WalkForwardFoldDraft,
    WalkForwardRunDraft, WalkForwardRunRepository, fold_windows, run_coach_decision,
    run_version_backtest,
};
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use tempfile::TempDir;

use crate::support::mcp::{FIXTURE_STORE, copy_tree, manifest, seeded_fold_trades};

/// A pinned instant, so `created_at` is deterministic everywhere.
pub const NOW_MS: i64 = 1_756_425_600_000; // 2026-08-29T00:00:00Z

/// The certifying run's timestamp — the pointer orders on `seq` alone, so
/// this is a later SAVE, not merely a later instant.
pub const CERTIFY_MS: i64 = 1_756_512_000_000; // 2026-08-30T00:00:00Z

/// The one-and-only request fingerprint the fixture session claims under.
pub const FINGERPRINT: &str = "aa11bb22cc33dd44ee55ff6600778899aabbccddeeff00112233445566778899";

/// The sweepable leaf every fixture mutation addresses.
pub const RSI_PERIOD: &str = "entry.lhs.indicator.rsi.period";
/// The same minimal, valid DSL `coach_decision.rs` uses — it produces real
/// trades over the fixture, so the parent run is a genuine one.
pub const MINIMAL_DSL: &str = r#"{
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
pub struct World {
    pub _tmp: TempDir,
    pub db: pulse::Db,
    pub store: CandleStore,
    pub version_id: VersionId,
    pub parent_inputs: BacktestInputs,
    pub session_id: CoachingSessionId,
}

impl World {
    pub fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    pub fn strategies(&self) -> SqliteStrategyRepo<pulse::SystemClock> {
        SqliteStrategyRepo::new(self.pool().clone())
    }

    pub fn runs(&self) -> SqliteBacktestRunRepo<pulse::SystemClock> {
        SqliteBacktestRunRepo::new(self.pool().clone())
    }

    pub fn sessions(&self) -> SqliteCoachingRepo<FakeClock> {
        SqliteCoachingRepo::with_deps(self.pool().clone(), FakeClock::at(NOW_MS))
    }

    pub fn acceptance(&self) -> SqliteCoachAcceptanceRepo<FakeClock, SeqIdSource> {
        SqliteCoachAcceptanceRepo::with_deps(
            self.pool().clone(),
            FakeClock::at(NOW_MS),
            SeqIdSource::with_prefix("minted"),
        )
    }

    pub async fn proposal(&self) -> Proposal {
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

    pub async fn table_count(&self, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(self.pool())
            .await
            .unwrap()
    }

    /// The version's persisted certification pair `(certified, pointer)`.
    pub async fn certification(&self, version: &VersionId) -> (bool, Option<String>) {
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
pub async fn world() -> World {
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
        None,
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
pub async fn seed_llm_call(pool: &SqlitePool) {
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
pub async fn seed_proposed_session(
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
pub fn proposed_mutation(period: u32) -> Mutation {
    Mutation::SetParam {
        path: RSI_PERIOD.to_owned(),
        new_value: ParamValue::Period { value: period },
    }
}
pub async fn decide(world: &World, action: CoachAction) -> CoachDecisionOutcome {
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
        None,
    )
    .await
    .expect("the decision resolves")
}
// ---------------------------------------------------------------------------
// The synthetic certifying run (AC-1(v)'s lever)
// ---------------------------------------------------------------------------

/// A walk-forward draft whose recorded verdict PASSES — two folds, both
/// holding, over the parent run's own inputs.
pub fn passing_draft(inputs: &BacktestInputs, span: CandleWindow) -> WalkForwardRunDraft {
    draft_with_fold_verdict(inputs, span, true)
}

/// A synthetic draft whose verdicts are DERIVED from the trades its folds carry
/// (R1): each fold takes twenty trades alternating around a positive (or
/// negative) mean, and the fold and run verdicts come from `FoldVerdict::from_rs`
/// / `RunVerdict::assess` over them — the identical derivation the write gate and
/// the read apply, so this fixture cannot claim a verdict its trades do not
/// support. `folds_hold` picks the two coherent extremes the tests need: every
/// fold holding (the run passes) or none holding (the run fails, so a save
/// de-certifies).
pub fn draft_with_fold_verdict(
    inputs: &BacktestInputs,
    span: CandleWindow,
    folds_hold: bool,
) -> WalkForwardRunDraft {
    let k = 2_u8;
    let (lo, hi) = if folds_hold {
        (Decimal::new(5, 1), Decimal::new(15, 1))
    } else {
        (Decimal::new(-5, 1), Decimal::new(-15, 1))
    };
    let folds: Vec<WalkForwardFoldDraft> = fold_windows(&span, k)
        .iter()
        .enumerate()
        .map(|(i, window)| {
            let trades = seeded_fold_trades(20, lo, hi, i);
            let rs: Vec<Decimal> = trades.iter().map(|t| t.realized_r).collect();
            let verdict = FoldVerdict::from_rs(&rs);
            let net_pnl: Decimal = trades.iter().map(|t| t.realized_pnl).sum();
            let fees_total: Decimal = trades.iter().map(|t| t.fees_total).sum();
            let funding_total: Decimal = trades.iter().map(|t| t.funding_total).sum();
            let slippage_total: Decimal = trades.iter().map(|t| t.slippage_total).sum();
            let summary = SummaryStats::from_trades(
                &trades,
                net_pnl,
                fees_total,
                funding_total,
                &EquityCurve::default(),
            );
            let mut fold_inputs = inputs.clone();
            fold_inputs.window = Some(window.clone());
            fold_inputs.lead_in_from_ms = Some(window.from_ms);
            WalkForwardFoldDraft {
                index: u8::try_from(i).unwrap(),
                window: window.clone(),
                verdict,
                inputs: fold_inputs,
                result: BacktestResult {
                    trades,
                    net_pnl,
                    fees_total,
                    funding_total,
                    slippage_total,
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
    let pooled_rs: Vec<Decimal> = folds
        .iter()
        .flat_map(|f| f.result.trades.iter().map(|t| t.realized_r))
        .collect();
    let fold_verdicts: Vec<FoldVerdict> = folds.iter().map(|f| f.verdict.clone()).collect();
    WalkForwardRunDraft {
        scheme: FoldScheme::rolling_oos(i64::from(k)).unwrap(),
        rule: VerdictRule::WfV1,
        span,
        from_defaulted: false,
        engine_fingerprint: EngineFingerprint::current().as_str().to_owned(),
        verdict: RunVerdict::assess(&fold_verdicts, &pooled_rs),
        folds,
    }
}
/// The walkable span the fixture's M15 snapshot covers, from the candle at
/// `from_idx` to the last candle's close — the same bounds `run_walk_forward`
/// resolves by default. `from_idx` must sit at or past the warm bar of every
/// DSL the test walks (the candidate's own warm point is what the gate's
/// span has to clear).
pub fn fixture_span_from(from_idx: usize) -> CandleWindow {
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
pub fn fixture_span() -> CandleWindow {
    fixture_span_from(30)
}
/// Certify the parent: one synthetic passing walk-forward, saved through the
/// real repository so the pointer lands exactly as the product moves it.
pub async fn certify_parent(world: &World) -> pulse::WalkForwardRunId {
    let repo = SqliteBacktestRunRepo::with_deps(world.pool().clone(), FakeClock::at(CERTIFY_MS));
    repo.save_walk_forward_run(
        &world.version_id,
        &passing_draft(&world.parent_inputs, fixture_span()),
    )
    .await
    .expect("the certifying run persists")
}

/// [`decide`] with the open freeze handed to the accept (r4.s1.w4, Q4): the
/// holdout guard's coach-gate case.
pub async fn decide_with_holdout(
    world: &World,
    action: CoachAction,
    holdout: Option<HoldoutFreeze>,
) -> CoachDecisionOutcome {
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
        holdout,
    )
    .await
    .expect("the decision resolves")
}
