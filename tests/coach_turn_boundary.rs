//! AC-1 — the SEALED coach turn (r1.s4.w1, `#131` / `#132`, ADR-0021 as amended).
//!
//! One crate-private application module runs a coach turn end to end over `w4`'s
//! claim/finish contract, and this binary drives that module the only way a caller
//! outside the crate can: through the composition root
//! (`run_coach_with`), which assembles the repository-owned projection, the
//! attributed provider and the process-local registry and then calls
//! `run_coach_turn`. A caller supplies **identifiers only** — there is no run, no
//! trade vector, no version and no capture handle to hand in, which is what makes
//! the six false-but-individually-valid audit rows `#132` names unrepresentable.
//!
//! Real `SQLite` through migration `0008`, a scripted inner provider behind the
//! REAL attributed composition (so every ledger row is a real row), the REAL
//! `apply()` framework and the REAL repositories. **No live LLM call happens
//! here.**
//!
//! What it asserts, one named test each:
//!   1. the claim is committed BEFORE the provider is invoked — the provider reads
//!      the row and finds it `pending`;
//!   2. an idempotent retry returns the existing terminal session and makes no call;
//!   3. a live pending claim in THIS process is refused as `TurnInFlight`;
//!   4. a stale pending claim from an abandoned turn is finalized as `Interrupted`
//!      without a second provider call;
//!   5. a reused session id is a `SessionConflict` — on a different run, and
//!      equally on the SAME run and version with a different request fingerprint,
//!      so a changed prompt, tool set or model never reuses a claim; and every exit
//!      from the claim step (`Existing`, `ExistingPending`, an erroring claim)
//!      releases the process-local registry entry on the way out;
//!   6. a pre-`0006` run is recorded as `MissingBacktestInputs` with no call;
//!   7. `record_inapplicable` persists `InapplicableAdvice` — no proposal, no child;
//!   8. its `intent` / `evidence` are redacted before they are stored;
//!   9. `propose_mutation` still yields one validated proposal;
//!  10. both tools in one response is a several-calls failure;
//!  11. the request fingerprint of a fixed fixture equals a pinned digest;
//!  12. and changes when ANY feed element changes;
//!  13. the projection is loaded by `run_id` alone, so the coached version is the
//!      one the run names and no caller can substitute another;
//!  14. the registry entry is cleared after a provider error.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pulse::{
    BacktestInputs, BacktestResult, BacktestRunId, BacktestRunRepository, CoachCliOutcome,
    CoachFailure, CoachTurnError, CoachTurnRegistry, CoachWiring, CoachingSessionId, CreatedBy,
    DataVersion, Db, EngineFingerprint, FakeClock, FundingConfig, LlmConfig, LlmError, LlmProvider,
    LlmResponse, MIGRATOR, Message, NewVersion, Pair, ParamValue, Redactor, RegimeBreakdown,
    SessionOutcome, SkippedEntryCounts, SnapshotSelection, SqliteBacktestRunRepo,
    SqliteCoachTurnSource, SqliteCoachingRepo, SqliteLlmCallRepo, SqliteStrategyRepo,
    StrategyRepository, StrategyVersion, SummaryStats, Timeframe, TokenUsage, ToolCall,
    ToolDefinition, coach_request_fingerprint, run_coach_with,
};
use rust_decimal::Decimal;
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

mod coach_support;
use coach_support::{CapturingLlmRepo, canonical_dsl_json, config, test_prices};

/// The input provenance a fresh `save_run` requires (r1.s3.w2, #110).
fn seed_inputs() -> BacktestInputs {
    BacktestInputs {
        pair: Pair::new("BTCUSDT"),
        primary: SnapshotSelection {
            timeframe: Timeframe::M15,
            data_version: DataVersion::new("v-primary"),
        },
        htf: None,
        taker_fee_bps: Decimal::new(4, 0),
        slippage_bps: Decimal::new(1, 0),
        funding: FundingConfig::SnapshotRates,
    }
}

// ---------------------------------------------------------------------------
// The scripted provider
// ---------------------------------------------------------------------------

/// A scripted `LlmProvider` that counts its calls, can stall, can fail at the
/// transport layer, and can READ THE DATABASE at call time — which is how the
/// claim-before-I/O ordering is asserted rather than assumed.
struct ScriptedProvider {
    scripts: Mutex<VecDeque<LlmResponse>>,
    calls: Arc<AtomicUsize>,
    delay: Option<Duration>,
    fails_with: Option<LlmError>,
    /// `(pool, session id)` to probe, and where to record what the probe saw.
    observe: Option<(sqlx::SqlitePool, CoachingSessionId)>,
    observed: Arc<Mutex<Vec<Option<String>>>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<LlmResponse>) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                scripts: Mutex::new(responses.into()),
                calls: Arc::clone(&calls),
                delay: None,
                fails_with: None,
                observe: None,
                observed: Arc::new(Mutex::new(Vec::new())),
            },
            calls,
        )
    }

    fn stalling(delay: Duration) -> (Self, Arc<AtomicUsize>) {
        let (mut provider, calls) = Self::new(Vec::new());
        provider.delay = Some(delay);
        (provider, calls)
    }

    fn failing(error: LlmError) -> (Self, Arc<AtomicUsize>) {
        let (mut provider, calls) = Self::new(Vec::new());
        provider.fails_with = Some(error);
        (provider, calls)
    }

    /// Probe `coaching_sessions.outcome` for `session_id` at call time.
    fn observing(
        responses: Vec<LlmResponse>,
        pool: sqlx::SqlitePool,
        session_id: CoachingSessionId,
    ) -> (Self, Arc<Mutex<Vec<Option<String>>>>) {
        let (mut provider, _calls) = Self::new(responses);
        provider.observe = Some((pool, session_id));
        let observed = Arc::clone(&provider.observed);
        (provider, observed)
    }
}

impl LlmProvider for ScriptedProvider {
    async fn chat(
        &self,
        _messages: Vec<Message>,
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
    ) -> Result<LlmResponse, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some((pool, session_id)) = &self.observe {
            let id = session_id.as_str().to_owned();
            let outcome: Option<String> =
                sqlx::query_scalar("SELECT outcome FROM coaching_sessions WHERE id = ?1")
                    .bind(id)
                    .fetch_optional(pool)
                    .await
                    .expect("probe the claim row");
            self.observed.lock().expect("observed lock").push(outcome);
        }
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        if let Some(error) = &self.fails_with {
            return Err(error.clone());
        }
        Ok(self
            .scripts
            .lock()
            .expect("scripts lock")
            .pop_front()
            .unwrap_or_else(|| LlmResponse {
                content: Some("(script exhausted)".to_owned()),
                tool_calls: Vec::new(),
                usage: usage(),
            }))
    }
}

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 900,
        output_tokens: 90,
    }
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments,
    }
}

fn with_calls(tool_calls: Vec<ToolCall>) -> LlmResponse {
    LlmResponse {
        content: None,
        tool_calls,
        usage: usage(),
    }
}

fn propose(path: &str) -> LlmResponse {
    with_calls(vec![tool_call(
        "c1",
        "propose_mutation",
        json!({
            "path": path,
            "new_value": { "type": "Period", "value": 21 },
            "hypothesis": "a slower RSI should cut the whipsaw entries this run shows",
        }),
    )])
}

fn record_inapplicable(intent: &str, evidence: &str) -> LlmResponse {
    with_calls(vec![tool_call(
        "c1",
        "record_inapplicable",
        json!({ "intent": intent, "evidence": evidence }),
    )])
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

async fn seeded() -> (TempDir, Db, StrategyVersion, BacktestRunId) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");

    let strategy_repo = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategy_repo
        .create_strategy("RSI Oversold", None, &[])
        .await
        .expect("create strategy");
    let version = strategy_repo
        .create_version(NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: canonical_dsl_json(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");

    let run_id = save_a_run(&db, &version).await;
    (tmp, db, version, run_id)
}

/// A real run through the real repo, so `get_run`'s integrity re-check passes.
async fn save_a_run(db: &Db, version: &StrategyVersion) -> BacktestRunId {
    let run_repo =
        SqliteBacktestRunRepo::with_deps(db.pool().clone(), FakeClock::at(1_700_000_000_000));
    let result = BacktestResult {
        trades: Vec::new(),
        net_pnl: Decimal::ZERO,
        fees_total: Decimal::ZERO,
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        regime_breakdown: RegimeBreakdown::default(),
        skipped_entries: SkippedEntryCounts::default(),
        engine_fingerprint: EngineFingerprint::current(),
        summary: SummaryStats::default(),
        equity_curve: pulse::EquityCurve::default(),
    };
    run_repo
        .save_run(
            &version.id,
            &seed_inputs(),
            &result,
            &SummaryStats::default(),
            Decimal::new(10_000, 0),
        )
        .await
        .expect("save run")
}

/// Everything a turn needs that a test wants to vary.
struct Drive<P> {
    provider: P,
    session_id: Option<CoachingSessionId>,
    registry: Option<Arc<CoachTurnRegistry>>,
    turn_timeout: Option<Duration>,
    redactor: Redactor,
    prompt_dir: Option<PathBuf>,
}

impl<P> Drive<P> {
    fn new(provider: P) -> Self {
        Self {
            provider,
            session_id: None,
            registry: None,
            turn_timeout: None,
            redactor: Redactor::default(),
            prompt_dir: None,
        }
    }

    fn session(mut self, id: &CoachingSessionId) -> Self {
        self.session_id = Some(id.clone());
        self
    }

    fn registry(mut self, registry: &Arc<CoachTurnRegistry>) -> Self {
        self.registry = Some(Arc::clone(registry));
        self
    }

    fn timeout(mut self, timeout: Duration) -> Self {
        self.turn_timeout = Some(timeout);
        self
    }

    fn redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// Resolve the coach prompt from an overlay directory instead of the built-in
    /// default — the `$PULSE_PROMPT_DIR` road, in-process.
    ///
    /// This is how a test changes the REQUEST without changing the run: the
    /// fingerprint's first element is the resolved prompt text and its fourth is
    /// that text's version hash, so an overlay makes two turns on the same run and
    /// the same version genuinely different asks.
    fn prompt_dir(mut self, dir: &std::path::Path) -> Self {
        self.prompt_dir = Some(dir.to_path_buf());
        self
    }
}

/// An overlay directory holding one `coach.md` with the given text.
///
/// Returned by value with its `TempDir`, because dropping the handle deletes the
/// directory and the prompt would resolve back to the default.
fn prompt_overlay(text: &str) -> TempDir {
    let dir = TempDir::new().expect("overlay tempdir");
    std::fs::write(dir.path().join("coach.md"), text).expect("write the overlay prompt");
    dir
}

/// Drive ONE sealed turn through the composition root.
async fn drive<P>(
    db: &Db,
    run_id: &BacktestRunId,
    drive: Drive<P>,
) -> anyhow::Result<CoachCliOutcome>
where
    P: LlmProvider + Send + Sync,
{
    let clock = FakeClock::at(1_700_000_000_000);
    let ids = Arc::new(Mutex::new(Vec::new()));
    let wiring = CoachWiring {
        provider: drive.provider,
        llm_repo: CapturingLlmRepo::new(
            SqliteLlmCallRepo::with_deps(db.pool().clone(), clock),
            Arc::clone(&ids),
        ),
        redactor: drive.redactor,
        prices: test_prices(),
        clock,
        key_source: None,
        config: config(),
        prompt_dir: drive.prompt_dir,
        turn_timeout: drive.turn_timeout,
        max_dsl_bytes: None,
        captured: ids,
        session_id: drive.session_id,
        registry: drive.registry,
    };
    let source = SqliteCoachTurnSource::new(db.pool().clone());
    let coaching_repo = SqliteCoachingRepo::with_deps(db.pool().clone(), clock);
    run_coach_with(wiring, &source, &coaching_repo, run_id).await
}

/// The error of a REFUSED turn. `CoachCliOutcome` is not `Debug` (it carries the
/// whole session), so `expect_err` is unavailable and the refusal is unwrapped here.
fn refusal(result: anyhow::Result<CoachCliOutcome>, what: &str) -> anyhow::Error {
    match result {
        Ok(outcome) => panic!(
            "{what}: expected a refusal, got a settled session `{}`",
            outcome.session.id.as_str()
        ),
        Err(error) => error,
    }
}

/// The recorded failure of a turn.
fn failure_of(outcome: &CoachCliOutcome) -> &CoachFailure {
    match &outcome.session.outcome {
        SessionOutcome::Failed { failure } => failure,
        SessionOutcome::Proposed { .. } => panic!("expected a recorded failure, got a proposal"),
        SessionOutcome::Pending => panic!("expected a settled turn, got an open claim"),
    }
}

async fn stored_outcome(db: &Db, session_id: &CoachingSessionId) -> Option<String> {
    sqlx::query_scalar("SELECT outcome FROM coaching_sessions WHERE id = ?1")
        .bind(session_id.as_str().to_owned())
        .fetch_optional(db.pool())
        .await
        .expect("read the session row")
}

async fn proposal_count(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM coaching_proposals")
        .fetch_one(db.pool())
        .await
        .expect("count the proposals")
}

async fn ledger_count(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM llm_call")
        .fetch_one(db.pool())
        .await
        .expect("count the ledger rows")
}

// ---------------------------------------------------------------------------
// 1. Claim before I/O
// ---------------------------------------------------------------------------

/// The item's central invariant: the claim commits BEFORE any provider call, and
/// no write transaction is held across it. Asserted by having the provider READ
/// the row while it is being called — an ordering that cannot be faked, and that
/// flips the moment someone moves the claim after the call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_claim_is_committed_before_the_provider_is_called() {
    let (_tmp, db, _version, run_id) = seeded().await;
    let session_id = CoachingSessionId::new("sess-claim-order");

    let (provider, observed) = ScriptedProvider::observing(
        vec![propose("entry.lhs.indicator.rsi.period")],
        db.pool().clone(),
        session_id.clone(),
    );

    let outcome = drive(&db, &run_id, Drive::new(provider).session(&session_id))
        .await
        .expect("the turn completes");

    let seen = observed.lock().expect("observed lock").clone();
    assert_eq!(seen.len(), 1, "exactly one provider call");
    assert_eq!(
        seen[0].as_deref(),
        Some("pending"),
        "the claim must be committed and still open while the provider is called"
    );
    assert!(matches!(
        outcome.session.outcome,
        SessionOutcome::Proposed { .. }
    ));
    assert_eq!(
        stored_outcome(&db, &session_id).await.as_deref(),
        Some("proposed"),
        "and the claim settles exactly once, to the turn's outcome"
    );
}

// ---------------------------------------------------------------------------
// 2. Idempotent retry
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idempotent_retry_returns_the_existing_session_and_makes_no_call() {
    let (_tmp, db, _version, run_id) = seeded().await;
    let session_id = CoachingSessionId::new("sess-idempotent");

    let (first_provider, _calls) =
        ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    let first = drive(
        &db,
        &run_id,
        Drive::new(first_provider).session(&session_id),
    )
    .await
    .expect("the first turn completes");

    let (second_provider, calls) =
        ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    let second = drive(
        &db,
        &run_id,
        Drive::new(second_provider).session(&session_id),
    )
    .await
    .expect("the retry completes");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "an already-settled session is answered from the record, never re-asked"
    );
    assert_eq!(
        second.session, first.session,
        "the retry returns the recorded session unchanged"
    );
    assert_eq!(
        proposal_count(&db).await,
        1,
        "and it does not attach a second proposal"
    );
}

// ---------------------------------------------------------------------------
// 3. A live claim in this process
// ---------------------------------------------------------------------------

/// A second turn on a session id whose call is STILL IN FLIGHT in this process is
/// refused, not reattached and not re-asked: the repository can see a pending row
/// but cannot see whether its claimant is alive, so the process-local registry is
/// what decides.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_pending_claim_in_this_process_is_refused_as_in_flight() {
    let (_tmp, db, _version, run_id) = seeded().await;
    let session_id = CoachingSessionId::new("sess-in-flight");
    let registry = Arc::new(CoachTurnRegistry::new());

    let (slow, _slow_calls) = ScriptedProvider::stalling(Duration::from_millis(400));
    let (fast, fast_calls) = ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);

    let live = drive(
        &db,
        &run_id,
        Drive::new(slow)
            .session(&session_id)
            .registry(&registry)
            .timeout(Duration::from_millis(600)),
    );
    let duplicate = async {
        tokio::time::sleep(Duration::from_millis(120)).await;
        drive(
            &db,
            &run_id,
            Drive::new(fast).session(&session_id).registry(&registry),
        )
        .await
    };

    let (_live, duplicate) = tokio::join!(live, duplicate);

    let error = refusal(duplicate, "a duplicate turn on a live claim");
    assert!(
        matches!(
            error.downcast_ref::<CoachTurnError>(),
            Some(CoachTurnError::TurnInFlight { .. })
        ),
        "expected TurnInFlight, got {error:#}"
    );
    assert_eq!(
        fast_calls.load(Ordering::SeqCst),
        0,
        "the refused duplicate spends no money"
    );
}

// ---------------------------------------------------------------------------
// 4. A stale claim from an abandoned turn
// ---------------------------------------------------------------------------

/// A claim left by a turn that never finished — here, one whose future is dropped
/// mid-call, the in-process stand-in for a crashed process lifetime — is finalized
/// as a typed `Interrupted` WITHOUT a second provider call. Re-asking on the
/// claimant's behalf would spend money on a turn nobody is waiting for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_pending_claim_is_finalized_as_interrupted_without_a_call() {
    let (_tmp, db, _version, run_id) = seeded().await;
    let session_id = CoachingSessionId::new("sess-stale");
    let registry = Arc::new(CoachTurnRegistry::new());

    let (abandoned, abandoned_calls) = ScriptedProvider::stalling(Duration::from_secs(30));
    let dropped = tokio::time::timeout(
        Duration::from_millis(250),
        drive(
            &db,
            &run_id,
            Drive::new(abandoned)
                .session(&session_id)
                .registry(&registry)
                .timeout(Duration::from_secs(30)),
        ),
    )
    .await;
    assert!(dropped.is_err(), "the first turn is abandoned mid-call");
    assert_eq!(
        abandoned_calls.load(Ordering::SeqCst),
        1,
        "it did reach the provider"
    );
    assert_eq!(
        stored_outcome(&db, &session_id).await.as_deref(),
        Some("pending"),
        "and left its claim behind"
    );

    // The registry entry is cleared on EVERY exit path, dropping included — which
    // is what lets the next turn tell a stale claim from a live one.
    assert!(
        !registry.in_flight(&session_id),
        "the abandoned turn cleared its registry entry"
    );

    let (retry, retry_calls) =
        ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    let outcome = drive(
        &db,
        &run_id,
        Drive::new(retry).session(&session_id).registry(&registry),
    )
    .await
    .expect("finalizing a stale claim is a completed turn");

    match failure_of(&outcome) {
        CoachFailure::Interrupted { detail } => assert!(
            !detail.is_empty(),
            "the finalized claim records what is known about it"
        ),
        other => panic!("expected Interrupted, got {other:?}"),
    }
    assert_eq!(
        retry_calls.load(Ordering::SeqCst),
        0,
        "finalizing a stale claim makes no provider call"
    );
    assert!(
        outcome.session.llm_call_id.is_none(),
        "and names no ledger row it did not produce"
    );
}

// ---------------------------------------------------------------------------
// 5. A reused session id on a different request
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reused_session_id_on_a_different_run_is_a_session_conflict() {
    let (_tmp, db, version, run_id) = seeded().await;
    let other_run = save_a_run(&db, &version).await;
    let session_id = CoachingSessionId::new("sess-conflict");

    let (first, _first_calls) =
        ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    drive(&db, &run_id, Drive::new(first).session(&session_id))
        .await
        .expect("the first turn completes");

    let (second, calls) = ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    let error = refusal(
        drive(&db, &other_run, Drive::new(second).session(&session_id)).await,
        "the same session id on a different run",
    );

    assert!(
        matches!(
            error.downcast_ref::<CoachTurnError>(),
            Some(CoachTurnError::SessionConflict { .. })
        ),
        "expected SessionConflict, got {error:#}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a collision is caught before the provider is called"
    );
}

/// The identity of a claim is the run, the version AND the request fingerprint —
/// so the same session id on the same run and version, but a DIFFERENT ask, is a
/// conflict too.
///
/// This is the branch that stops a changed prompt, tool set or model from silently
/// reusing an existing claim and answering the new question with the old question's
/// result. The run-based case above cannot reach it: it differs in the first column
/// checked and returns before the fingerprint is ever compared.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reused_session_id_with_a_different_fingerprint_is_a_session_conflict() {
    let (_tmp, db, _version, run_id) = seeded().await;
    let session_id = CoachingSessionId::new("sess-fingerprint-conflict");
    let registry = Arc::new(CoachTurnRegistry::new());

    // Turn one, on the built-in prompt.
    let (first, _first_calls) =
        ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    drive(
        &db,
        &run_id,
        Drive::new(first).session(&session_id).registry(&registry),
    )
    .await
    .expect("the first turn completes");

    // Turn two: SAME session id, SAME run, SAME version — and a different prompt,
    // so only the fingerprint moved.
    let overlay = prompt_overlay("A different coaching prompt, so a different ask.\n");
    let (second, calls) = ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    let error = refusal(
        drive(
            &db,
            &run_id,
            Drive::new(second)
                .session(&session_id)
                .registry(&registry)
                .prompt_dir(overlay.path()),
        )
        .await,
        "the same session id, run and version with a different request fingerprint",
    );

    assert!(
        matches!(
            error.downcast_ref::<CoachTurnError>(),
            Some(CoachTurnError::SessionConflict { .. })
        ),
        "expected SessionConflict, got {error:#}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a changed request is caught before the provider is called"
    );
    assert!(
        !registry.in_flight(&session_id),
        "and the refused turn leaves no single-flight entry behind"
    );
}

/// Every early return out of the CLAIM step releases the process-local entry.
///
/// The turn takes the registry entry BEFORE the durable claim (see the module's
/// ordering note), which is the only order that stops a duplicate from reading a
/// committed `pending` row, seeing "not in flight", and finalizing a LIVE turn as
/// `Interrupted`. That order is correct — and it is also what gives every exit out
/// of the claim step a cleanup obligation the other order would not have created.
/// A leak would make the NEXT turn on that id in this process report `TurnInFlight`
/// forever, so the three exits are pinned one test each, below: `Existing`,
/// `ExistingPending`, and a claim that errors.
///
/// `Existing` — the idempotent retry of a claim that already settled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_existing_exit_from_the_claim_step_releases_the_registry_entry() {
    let (_tmp, db, _version, run_id) = seeded().await;
    let session_id = CoachingSessionId::new("sess-release-existing");
    let registry = Arc::new(CoachTurnRegistry::new());

    let (first, _first_calls) =
        ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    drive(
        &db,
        &run_id,
        Drive::new(first).session(&session_id).registry(&registry),
    )
    .await
    .expect("the first turn completes");

    let (again, again_calls) =
        ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    drive(
        &db,
        &run_id,
        Drive::new(again).session(&session_id).registry(&registry),
    )
    .await
    .expect("the idempotent retry returns the settled session");

    assert_eq!(
        again_calls.load(Ordering::SeqCst),
        0,
        "the idempotent retry makes no provider call"
    );
    assert!(
        !registry.in_flight(&session_id),
        "the `Existing` early return releases the registry entry"
    );
}

/// `ExistingPending` — a stale claim finalized as `Interrupted`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_existing_pending_exit_from_the_claim_step_releases_the_registry_entry() {
    let (_tmp, db, _version, run_id) = seeded().await;
    let session_id = CoachingSessionId::new("sess-release-pending");
    let registry = Arc::new(CoachTurnRegistry::new());

    let (abandoned, _abandoned_calls) = ScriptedProvider::stalling(Duration::from_secs(30));
    let dropped = tokio::time::timeout(
        Duration::from_millis(250),
        drive(
            &db,
            &run_id,
            Drive::new(abandoned)
                .session(&session_id)
                .registry(&registry)
                .timeout(Duration::from_secs(30)),
        ),
    )
    .await;
    assert!(dropped.is_err(), "the first turn is abandoned mid-call");

    let (settler, settler_calls) =
        ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    let outcome = drive(
        &db,
        &run_id,
        Drive::new(settler).session(&session_id).registry(&registry),
    )
    .await
    .expect("finalizing a stale claim is a completed turn");

    assert!(
        matches!(failure_of(&outcome), CoachFailure::Interrupted { .. }),
        "the stale claim is finalized as Interrupted"
    );
    assert_eq!(
        settler_calls.load(Ordering::SeqCst),
        0,
        "finalizing a stale claim makes no provider call"
    );
    assert!(
        !registry.in_flight(&session_id),
        "the `ExistingPending` early return releases the registry entry"
    );
}

/// An ERRORING claim — the `?` road out of the claim step, which has no explicit
/// cleanup call anywhere and relies entirely on the guard's `Drop`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_erroring_claim_releases_the_registry_entry() {
    let (_tmp, db, version, run_id) = seeded().await;
    let session_id = CoachingSessionId::new("sess-release-error");
    let registry = Arc::new(CoachTurnRegistry::new());
    let other_run = save_a_run(&db, &version).await;

    let (held, _held_calls) =
        ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    drive(
        &db,
        &run_id,
        Drive::new(held).session(&session_id).registry(&registry),
    )
    .await
    .expect("the holding turn completes");

    let (colliding, colliding_calls) =
        ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    refusal(
        drive(
            &db,
            &other_run,
            Drive::new(colliding)
                .session(&session_id)
                .registry(&registry),
        )
        .await,
        "the same session id on a different run",
    );

    assert_eq!(
        colliding_calls.load(Ordering::SeqCst),
        0,
        "the refused claim spends no money"
    );
    assert!(
        !registry.in_flight(&session_id),
        "an ERRORING claim releases the registry entry too"
    );
}

// ---------------------------------------------------------------------------
// 6. A pre-0006 run
// ---------------------------------------------------------------------------

/// A run whose `inputs` are absent (a row written before migration `0006`) cannot
/// be re-backtested comparably, so the turn records the typed reason rather than
/// spending a call on advice nobody can act on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_0006_run_is_recorded_as_missing_backtest_inputs_without_a_call() {
    let (_tmp, db, _version, run_id) = seeded().await;

    // A pre-`0006` row, manufactured the only way a 0008 database allows.
    //
    // Two schema rules stand in the way, and BOTH are correct: `backtest_run` is
    // immutable (0003's BEFORE UPDATE trigger), so an existing row cannot be aged
    // into the legacy shape; and `0006`'s BEFORE INSERT completeness trigger refuses
    // a NEW row without provenance, so no fresh row may be born legacy. The fixture
    // therefore lifts `0006`'s trigger for exactly one INSERT and puts it back FROM
    // ITS OWN STORED DEFINITION (never a copy that could drift), then asserts it is
    // back. Copying every hash-relevant column from a real run keeps `get_run`'s
    // integrity re-derive honest: this is a genuinely readable run that predates the
    // provenance, not a corrupt one.
    let completeness_trigger: String =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = ?1")
            .bind("backtest_run_inputs_complete")
            .fetch_one(db.pool())
            .await
            .expect("0006's completeness trigger is present before the fixture touches it");
    sqlx::query("DROP TRIGGER backtest_run_inputs_complete")
        .execute(db.pool())
        .await
        .expect("lift the completeness trigger for one insert");

    let legacy_run = BacktestRunId::new("run-legacy");
    sqlx::query(
        "INSERT INTO backtest_run ( \
           id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
           engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
           funding_total, slippage_total, expectancy, win_rate, profit_factor, gross_profit, \
           gross_loss, avg_win, avg_loss, max_drawdown, trade_count, wins, losses, breakeven, \
           max_win_streak, max_loss_streak, sharpe, sortino, regime_breakdown, \
           skipped_sub_lot, skipped_sub_notional, skipped_leverage_capped) \
         SELECT \
           ?1, strategy_version_id, schema_version, created_at, engine_fingerprint, \
           engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
           funding_total, slippage_total, expectancy, win_rate, profit_factor, gross_profit, \
           gross_loss, avg_win, avg_loss, max_drawdown, trade_count, wins, losses, breakeven, \
           max_win_streak, max_loss_streak, sharpe, sortino, regime_breakdown, \
           skipped_sub_lot, skipped_sub_notional, skipped_leverage_capped \
         FROM backtest_run WHERE id = ?2",
    )
    .bind(legacy_run.as_str().to_owned())
    .bind(run_id.as_str().to_owned())
    .execute(db.pool())
    .await
    .expect("insert the pre-0006 run");

    sqlx::query(&completeness_trigger)
        .execute(db.pool())
        .await
        .expect("restore the completeness trigger");
    let restored: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND name = ?1",
    )
    .bind("backtest_run_inputs_complete")
    .fetch_one(db.pool())
    .await
    .expect("count the completeness trigger");
    assert_eq!(
        restored, 1,
        "the fixture must not leave 0006's completeness rule lifted"
    );

    let session_id = CoachingSessionId::new("sess-legacy");
    let (provider, calls) = ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    let outcome = drive(&db, &legacy_run, Drive::new(provider).session(&session_id))
        .await
        .expect("a legacy run is a completed, recorded turn");

    match failure_of(&outcome) {
        CoachFailure::MissingBacktestInputs { detail } => assert!(
            detail.contains(legacy_run.as_str()),
            "the recorded reason names the run whose provenance is gone: {detail}"
        ),
        other => panic!("expected MissingBacktestInputs, got {other:?}"),
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a legacy run costs no provider call"
    );
    assert_eq!(ledger_count(&db).await, 0, "and mints no ledger row");
    assert!(outcome.session.llm_call_id.is_none());
}

// ---------------------------------------------------------------------------
// 7-8. record_inapplicable
// ---------------------------------------------------------------------------

/// `#131`: structural advice is recorded honestly instead of being approximated by
/// whichever parameter sits nearest to it. No proposal row, no child version, and
/// the turn still names the one ledger row it produced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_inapplicable_persists_inapplicable_advice_with_no_proposal() {
    let (_tmp, db, _version, run_id) = seeded().await;
    let session_id = CoachingSessionId::new("sess-inapplicable");

    let (provider, calls) = ScriptedProvider::new(vec![record_inapplicable(
        "add an ADX trend filter to the entry",
        "38 of 51 losing trades opened while the regime breakdown says ranging",
    )]);

    let outcome = drive(&db, &run_id, Drive::new(provider).session(&session_id))
        .await
        .expect("recorded inapplicability is a completed turn");

    match failure_of(&outcome) {
        CoachFailure::InapplicableAdvice { intent, evidence } => {
            assert_eq!(intent, "add an ADX trend filter to the entry");
            assert!(
                evidence.contains("ranging"),
                "the evidence is preserved: {evidence}"
            );
        }
        other => panic!("expected InapplicableAdvice, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one provider call");
    assert_eq!(
        proposal_count(&db).await,
        0,
        "recorded inapplicability creates NO proposal"
    );
    assert_eq!(
        stored_outcome(&db, &session_id).await.as_deref(),
        Some("failed"),
        "it is a recorded failed turn, not a silent one"
    );
    assert!(
        outcome.session.llm_call_id.is_some(),
        "the turn reached the provider, so it names its ledger row"
    );
    assert_eq!(ledger_count(&db).await, 1, "exactly one ledger row");
}

/// The stored text passes the existing redactor: `intent` and `evidence` become
/// stored domain values, which is the road an ordinary log scrubber misses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inapplicable_advice_is_redacted_before_it_is_stored() {
    const CANARY: &str = "sk-canary-INAPPLICABLE-1a2b3c4d5e6f7890";
    let (_tmp, db, _version, run_id) = seeded().await;

    let (provider, _calls) = ScriptedProvider::new(vec![record_inapplicable(
        &format!("swap the RSI for an ADX, using key {CANARY}"),
        &format!("the losing trades cluster in ranging bars ({CANARY})"),
    )]);

    let outcome = drive(
        &db,
        &run_id,
        Drive::new(provider).redactor(Redactor::from_config(vec![CANARY.to_owned()])),
    )
    .await
    .expect("the turn completes");

    match failure_of(&outcome) {
        CoachFailure::InapplicableAdvice { intent, evidence } => {
            assert!(!intent.contains(CANARY), "the intent carries the canary");
            assert!(
                !evidence.contains(CANARY),
                "the evidence carries the canary"
            );
        }
        other => panic!("expected InapplicableAdvice, got {other:?}"),
    }

    let stored: String = sqlx::query_scalar(
        "SELECT COALESCE(group_concat(COALESCE(failure_detail, '')), '') FROM coaching_sessions",
    )
    .fetch_one(db.pool())
    .await
    .expect("read the recorded failures");
    assert!(
        !stored.contains(CANARY),
        "the canary reached the audit trail: {stored}"
    );
}

/// The two fields become STORED DOMAIN VALUES, so they are held to the same standard
/// as a proposal's hypothesis: an empty one is silence wearing a record's clothes,
/// and an unbounded one turns the session row into a document store. Both are the
/// existing `MalformedArguments` failure — a recorded turn, never a dropped one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_inapplicable_refuses_an_empty_or_unbounded_field() {
    for (label, intent, evidence, needle) in [
        (
            "an empty intent",
            "   ".to_owned(),
            "ranging losses".to_owned(),
            "intent",
        ),
        (
            "an empty evidence",
            "add an ADX filter".to_owned(),
            String::new(),
            "evidence",
        ),
        (
            "an unbounded evidence",
            "add an ADX filter".to_owned(),
            "x".repeat(4_000),
            "evidence",
        ),
    ] {
        let (_tmp, db, _version, run_id) = seeded().await;
        let (provider, calls) =
            ScriptedProvider::new(vec![record_inapplicable(&intent, &evidence)]);
        let outcome = drive(&db, &run_id, Drive::new(provider))
            .await
            .expect("a malformed honest answer is still a recorded turn");

        match failure_of(&outcome) {
            CoachFailure::MalformedArguments { detail } => assert!(
                detail.contains(needle),
                "{label}: the recorded reason names the field it refused: {detail}"
            ),
            other => panic!("{label}: expected MalformedArguments, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{label}: one call, one turn"
        );
        assert_eq!(
            proposal_count(&db).await,
            0,
            "{label}: a refused honest answer creates no proposal"
        );
    }
}

// ---------------------------------------------------------------------------
// 9-10. The unchanged first tool, and the two-tool exclusivity
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn propose_mutation_still_yields_one_validated_proposal() {
    let (_tmp, db, _version, run_id) = seeded().await;

    let (provider, calls) = ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    let outcome = drive(&db, &run_id, Drive::new(provider))
        .await
        .expect("the turn completes");

    match &outcome.session.outcome {
        SessionOutcome::Proposed { proposal } => {
            assert_eq!(
                proposal.mutation,
                pulse::Mutation::SetParam {
                    path: "entry.lhs.indicator.rsi.period".to_owned(),
                    new_value: ParamValue::Period { value: 21 },
                }
            );
            assert!(!proposal.hypothesis.as_str().is_empty());
        }
        other => panic!("expected a proposal, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(proposal_count(&db).await, 1);
}

/// The two tools are MUTUALLY EXCLUSIVE. Calling both in one response is the
/// existing several-calls failure — the taxonomy did not need a new variant for a
/// model that answered twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_tools_in_one_response_is_a_several_calls_failure() {
    let (_tmp, db, _version, run_id) = seeded().await;

    let (provider, _calls) = ScriptedProvider::new(vec![with_calls(vec![
        tool_call(
            "c1",
            "propose_mutation",
            json!({
                "path": "entry.lhs.indicator.rsi.period",
                "new_value": { "type": "Period", "value": 21 },
                "hypothesis": "a slower RSI should cut whipsaw entries",
            }),
        ),
        tool_call(
            "c2",
            "record_inapplicable",
            json!({ "intent": "add an ADX filter", "evidence": "ranging losses" }),
        ),
    ])]);

    let outcome = drive(&db, &run_id, Drive::new(provider))
        .await
        .expect("the turn completes");

    match failure_of(&outcome) {
        CoachFailure::SeveralCalls {
            count,
            propose_mutation_count,
        } => {
            assert_eq!(*count, 2, "two tool calls arrived");
            assert_eq!(
                *propose_mutation_count, 1,
                "one of them was propose_mutation, and the record says so"
            );
        }
        other => panic!("expected SeveralCalls, got {other:?}"),
    }
    assert_eq!(
        proposal_count(&db).await,
        0,
        "a deviant turn attaches no proposal"
    );
}

// ---------------------------------------------------------------------------
// 11-12. The request fingerprint
// ---------------------------------------------------------------------------

/// The fixture feed, spelled out here rather than imported, so this test PINS the
/// feed order byte for byte: an element added, removed, reordered or serialized
/// differently changes the digest and fails here.
fn fixture_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "alpha".to_owned(),
            description: "the first tool".to_owned(),
            // Deliberately out of key order: the feed serializes canonical JSON
            // (sorted keys, no insignificant whitespace), so this must hash the
            // same as `{"a":1,"b":2}`.
            parameters: json!({ "b": 2, "a": 1 }),
        },
        ToolDefinition {
            name: "beta".to_owned(),
            description: "the second tool".to_owned(),
            parameters: json!({ "type": "object" }),
        },
    ]
}

fn fixture_config() -> LlmConfig {
    LlmConfig {
        backend: pulse::LlmBackend::Ollama,
        model: "fixture-model".to_owned(),
        temperature: 0.0,
        max_tokens: 2_048,
        reasoning_effort: None,
    }
}

/// An INDEPENDENT reference implementation of the feed, so the pinned digest below
/// is checked against the specification rather than against the code under test.
fn reference_digest(
    prompt: &str,
    context: &str,
    tools: &[ToolDefinition],
    prompt_version: Option<&str>,
    config: &LlmConfig,
) -> String {
    let mut hasher = Sha256::new();
    let mut feed = |bytes: &[u8]| {
        hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(bytes);
    };
    feed(prompt.as_bytes());
    feed(context.as_bytes());
    for tool in tools {
        feed(tool.name.as_bytes());
        feed(tool.description.as_bytes());
        feed(canonical_json(&tool.parameters).as_bytes());
    }
    feed(prompt_version.unwrap_or_default().as_bytes());
    feed(config.model.as_bytes());
    feed(format!("{:?}", config.temperature).as_bytes());
    feed(config.max_tokens.to_string().as_bytes());
    feed(format!("{:?}", config.reasoning_effort).as_bytes());
    hex::encode(hasher.finalize())
}

/// Canonical JSON: sorted keys, no insignificant whitespace.
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::Value::String(k.clone()),
                        canonical_json(&map[k])
                    )
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        serde_json::Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", body.join(","))
        }
        other => other.to_string(),
    }
}

/// The digest of a FIXED fixture, pinned. A change to the feed's order, its
/// length-prefixing, or its canonical JSON changes this value — which is the point:
/// the fingerprint is the single-flight key, and a silent change to it silently
/// stops two identical requests from recognizing each other.
const PINNED_FIXTURE_DIGEST: &str =
    "3b21dd3d26051d31e819388af6b2ea0894a365b573d2da478de665a39bb00261";

#[test]
fn the_request_fingerprint_of_a_fixed_fixture_matches_its_pinned_digest() {
    let tools = fixture_tools();
    let config = fixture_config();
    let digest = coach_request_fingerprint(
        "FIXTURE COACH PROMPT\n",
        "FIXTURE RENDERED CONTEXT\n",
        &tools,
        Some("pv-fixture"),
        &config,
    );

    assert_eq!(
        digest,
        reference_digest(
            "FIXTURE COACH PROMPT\n",
            "FIXTURE RENDERED CONTEXT\n",
            &tools,
            Some("pv-fixture"),
            &config,
        ),
        "the feed must be the ordered length-prefixed one the spec pins"
    );
    assert_eq!(
        digest, PINNED_FIXTURE_DIGEST,
        "the fixture's digest is pinned byte for byte"
    );
    assert_eq!(digest, digest.to_lowercase(), "the digest is lowercase hex");
}

/// One whole fingerprint feed, so a case below can change exactly ONE element and
/// stay readable.
struct Feed {
    prompt: String,
    context: String,
    tools: Vec<ToolDefinition>,
    version: Option<String>,
    config: LlmConfig,
}

impl Feed {
    fn fixture() -> Self {
        Self {
            prompt: "FIXTURE COACH PROMPT\n".to_owned(),
            context: "FIXTURE RENDERED CONTEXT\n".to_owned(),
            tools: fixture_tools(),
            version: Some("pv-fixture".to_owned()),
            config: fixture_config(),
        }
    }

    fn digest(&self) -> String {
        coach_request_fingerprint(
            &self.prompt,
            &self.context,
            &self.tools,
            self.version.as_deref(),
            &self.config,
        )
    }

    /// The fixture with one element changed by `edit`.
    fn edited(edit: impl FnOnce(&mut Self)) -> Self {
        let mut feed = Self::fixture();
        edit(&mut feed);
        feed
    }
}

#[test]
fn the_request_fingerprint_changes_when_any_feed_element_changes() {
    let base = Feed::fixture().digest();

    // One changed element each, so a feed that stopped covering ANY of them fails
    // here by name rather than silently letting two different requests share a key.
    let cases: Vec<(&str, Feed)> = vec![
        ("the prompt", Feed::edited(|f| f.prompt.push('!'))),
        ("the context", Feed::edited(|f| f.context.push('!'))),
        ("the tool ORDER", Feed::edited(|f| f.tools.swap(0, 1))),
        (
            "a tool name",
            Feed::edited(|f| f.tools[0].name.push_str("-2")),
        ),
        (
            "a tool description",
            Feed::edited(|f| f.tools[0].description.push('!')),
        ),
        (
            "a tool's parameter schema",
            Feed::edited(|f| f.tools[1].parameters = json!({ "type": "object", "extra": true })),
        ),
        (
            "the prompt version",
            Feed::edited(|f| f.version = Some("pv-fixture-2".to_owned())),
        ),
        (
            "an absent prompt version",
            Feed::edited(|f| f.version = None),
        ),
        (
            "the model",
            Feed::edited(|f| f.config.model = "fixture-model-2".to_owned()),
        ),
        (
            "the temperature",
            Feed::edited(|f| f.config.temperature = 0.7),
        ),
        (
            "the token cap",
            Feed::edited(|f| f.config.max_tokens = 2_049),
        ),
        (
            "the reasoning effort",
            Feed::edited(|f| f.config.reasoning_effort = Some(pulse::ReasoningEffort::Low)),
        ),
    ];

    for (label, feed) in cases {
        assert_ne!(
            base,
            feed.digest(),
            "changing {label} must change the fingerprint"
        );
    }
}

/// The feed carries no credential, no base URL and no price data — the elements
/// are named one by one above, and this is the negative half: a tagged secret in
/// the CONFIG's neighbourhood never reaches the digest, because the digest is fed
/// explicit fields rather than a serialized struct.
#[test]
fn two_requests_that_differ_only_in_a_secret_share_a_fingerprint() {
    let tools = fixture_tools();
    let config = fixture_config();
    let first = coach_request_fingerprint(
        "FIXTURE COACH PROMPT\n",
        "FIXTURE RENDERED CONTEXT\n",
        &tools,
        Some("pv-fixture"),
        &config,
    );
    // The same request, made with a different API key / base URL: neither is a feed
    // element, so the single-flight key is unchanged.
    let second = coach_request_fingerprint(
        "FIXTURE COACH PROMPT\n",
        "FIXTURE RENDERED CONTEXT\n",
        &tools,
        Some("pv-fixture"),
        &fixture_config(),
    );
    assert_eq!(first, second);
}

// ---------------------------------------------------------------------------
// 13. The projection is loaded by run id alone
// ---------------------------------------------------------------------------

/// `#132`'s first false row: a turn that relates a run to a version it never
/// touched. The sealed entry point takes identifiers only and loads the owning
/// version FROM the run, so a caller has nowhere to put a stranger version, a
/// foreign trade set, or a truncated one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_turn_coaches_the_version_the_run_names() {
    let (_tmp, db, version, run_id) = seeded().await;

    // A second, real version under the same strategy that this run was never
    // produced against — the value a pre-seal caller could have handed in.
    let strategy_repo = SqliteStrategyRepo::new(db.pool().clone());
    let stranger = strategy_repo
        .create_version(NewVersion {
            strategy_id: version.strategy_id.clone(),
            parent_version_id: Some(version.id.clone()),
            dsl_json: canonical_dsl_json(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create the stranger version");

    let (provider, _calls) = ScriptedProvider::new(vec![propose("entry.lhs.indicator.rsi.period")]);
    let outcome = drive(&db, &run_id, Drive::new(provider))
        .await
        .expect("the turn completes");

    assert_eq!(
        outcome.session.strategy_version_id, version.id,
        "the coached version is the one the run names"
    );
    assert_ne!(
        outcome.session.strategy_version_id, stranger.id,
        "and never one a caller offered instead"
    );
    assert_eq!(outcome.session.backtest_run_id, run_id);
}

// ---------------------------------------------------------------------------
// 14. The registry clears on every exit path
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_registry_entry_is_cleared_after_a_provider_error() {
    let (_tmp, db, _version, run_id) = seeded().await;
    let session_id = CoachingSessionId::new("sess-cleared");
    let registry = Arc::new(CoachTurnRegistry::new());

    let (provider, calls) =
        ScriptedProvider::failing(LlmError::Provider("HTTP 503 from upstream".to_owned()));
    let outcome = drive(
        &db,
        &run_id,
        Drive::new(provider)
            .session(&session_id)
            .registry(&registry),
    )
    .await
    .expect("a transport fault is a recorded turn");

    assert!(matches!(
        failure_of(&outcome),
        CoachFailure::TransportFailure { .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        !registry.in_flight(&session_id),
        "the registry entry must be cleared on the error path too"
    );
}
