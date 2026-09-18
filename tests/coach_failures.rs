//! AC-2 / demo line `d7` — every deviant coach turn ends as a typed recorded
//! failure, never silence (r1.s2.w3, grill L3 / ADR-0021 decision 6).
//!
//! The half of the capability sentence that is easy to skip and expensive to get
//! wrong: *zero calls, several calls, malformed arguments, an inapplicable
//! mutation, a timeout, context overflow, a transport fault — each ends as a
//! recorded failed session.* A coach that quietly returns nothing when the model
//! misbehaves is the failure mode this spine exists to remove, so each of the
//! seven is driven here through the REAL turn against a scripted provider and
//! asserted to leave a row behind.
//!
//! And the counterpart: the two faults that are NOT coaching outcomes — a local
//! fault on the call path, and a session that cannot be written — surface as
//! errors with nothing (or everything) recorded, never as a false reason in the
//! audit trail.
//!
//! **No live LLM call happens here.** This binary is a demo-ledger line (`d7`)
//! re-run at every future spine close.
//!
//! Also asserted, once per case: **at most one provider call per turn** — no
//! retries and no nudges, unlike the composer.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pulse::{
    BacktestInputs, BacktestResult, BacktestRunId, BacktestRunRepository, CoachCliOutcome,
    CoachFailure, CoachSessionClaim, CoachSessionClaimResult, CoachTurnError, CoachWiring,
    CoachingRepository, CoachingSession, CoachingSessionId, CreatedBy, DataError, DataVersion, Db,
    Disposition, EngineFingerprint, FakeClock, FundingConfig, InitialCoachOutcome, LlmCallCapture,
    LlmCallId, LlmConfig, LlmError, LlmProvider, LlmResponse, MIGRATOR, Message, Mutation,
    MutationError, NewVersion, Pair, Proposal, Redactor, RegimeBreakdown, SessionOutcome,
    SkippedEntryCounts, SnapshotSelection, SqliteBacktestRunRepo, SqliteCoachTurnSource,
    SqliteCoachingRepo, SqliteLlmCallRepo, SqliteStrategyRepo, StrategyDsl, StrategyRepository,
    SummaryStats, Timeframe, TokenUsage, ToolCall, ToolDefinition, run_coach_with,
};

/// The input provenance a fresh `save_run` now requires (r1.s3.w2, #110). These
/// tests are about coach/library behaviour, not provenance, so the tuple is a
/// plain complete single-timeframe one; `tests/backtest_provenance.rs` owns the
/// provenance shapes themselves.
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
        window: None,
    }
}
use rust_decimal::Decimal;
use serde_json::json;
use tempfile::TempDir;

mod coach_support;
use coach_support::{CapturingLlmRepo, canonical_dsl_json, capture, config, test_prices};

/// A scripted provider that counts its calls and can stall past a turn timeout.
struct ScriptedProvider {
    scripts: Mutex<VecDeque<LlmResponse>>,
    calls: Arc<AtomicUsize>,
    delay: Option<Duration>,
    /// When set, the call fails at the transport layer instead of answering.
    fails_with: Option<LlmError>,
    /// Ids to push into the shared capture buffer during the call — the capturing
    /// decorator's side effect, under a test's control (PR #128, finding G1).
    pushes: Vec<LlmCallId>,
    /// The buffer `pushes` are written to.
    capture: Option<LlmCallCapture>,
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
                pushes: Vec::new(),
                capture: None,
            },
            calls,
        )
    }

    /// A provider that answers AND mints `pushes` ledger ids into `capture` — the
    /// capturing decorator's side effect, made scriptable so a turn can be driven
    /// into the zero-id and several-id cases (PR #128, finding G1).
    fn pushing(
        responses: Vec<LlmResponse>,
        capture: LlmCallCapture,
        pushes: Vec<LlmCallId>,
    ) -> (Self, Arc<AtomicUsize>) {
        let (mut provider, calls) = Self::new(responses);
        provider.pushes = pushes;
        provider.capture = Some(capture);
        (provider, calls)
    }

    fn stalling(delay: Duration) -> (Self, Arc<AtomicUsize>) {
        let (mut provider, calls) = Self::new(vec![]);
        provider.delay = Some(delay);
        (provider, calls)
    }

    /// A provider that fails at the transport layer — an HTTP 5xx, a refused
    /// connection, a malformed envelope. The call happens; no usable response
    /// comes back.
    fn failing(error: LlmError) -> (Self, Arc<AtomicUsize>) {
        let (mut provider, calls) = Self::new(vec![]);
        provider.fails_with = Some(error);
        (provider, calls)
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
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        if let Some(capture) = &self.capture
            && let Ok(mut ids) = capture.lock()
        {
            ids.extend(self.pushes.iter().cloned());
        }
        if let Some(error) = &self.fails_with {
            return Err(error.clone());
        }
        Ok(self
            .scripts
            .lock()
            .expect("scripts lock")
            .pop_front()
            .unwrap_or_else(|| text_only("(script exhausted)")))
    }
}

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 500,
        output_tokens: 50,
    }
}

/// A response with no tool call at all.
fn text_only(text: &str) -> LlmResponse {
    LlmResponse {
        content: Some(text.to_owned()),
        tool_calls: Vec::new(),
        usage: usage(),
    }
}

fn call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
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

fn good_args(path: &str) -> serde_json::Value {
    json!({
        "path": path,
        "new_value": { "type": "Period", "value": 21 },
        "hypothesis": "a slower RSI should cut whipsaw entries",
    })
}

/// A migrated temp DB with a persisted version + run to coach against.
async fn seeded() -> (TempDir, Db, BacktestRunId) {
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

    let clock = FakeClock::at(1_700_000_000_000);
    let run_repo = SqliteBacktestRunRepo::with_deps(db.pool().clone(), clock);
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
    let run_id = run_repo
        .save_run(
            &version.id,
            &seed_inputs(),
            &result,
            &SummaryStats::default(),
            Decimal::new(10_000, 0),
        )
        .await
        .expect("save run");
    (tmp, db, run_id)
}

/// Run one turn over `provider`, with optional timeout / DSL-budget overrides.
async fn turn_with(
    db: &Db,
    run_id: &BacktestRunId,
    provider: ScriptedProvider,
    turn_timeout: Option<Duration>,
    max_dsl_bytes: Option<usize>,
) -> CoachCliOutcome {
    turn_with_redactor(
        db,
        run_id,
        provider,
        turn_timeout,
        max_dsl_bytes,
        Redactor::default(),
        None,
    )
    .await
}

/// The same, with an explicit redactor — the transport case tags a canary into it.
async fn turn_with_redactor(
    db: &Db,
    run_id: &BacktestRunId,
    provider: ScriptedProvider,
    turn_timeout: Option<Duration>,
    max_dsl_bytes: Option<usize>,
    redactor: Redactor,
    prompt_dir: Option<PathBuf>,
) -> CoachCliOutcome {
    try_turn(
        db,
        run_id,
        provider,
        turn_timeout,
        max_dsl_bytes,
        redactor,
        prompt_dir,
        Captures::wired(),
    )
    .await
    .expect("a deviant turn is still a completed turn")
}

/// The two ends of the capture pairing: the buffer the LEDGER REPO writes minted ids
/// into, and the buffer the ATTRIBUTED PROVIDER reads them back from.
struct Captures {
    ledger: LlmCallCapture,
    provider: LlmCallCapture,
}

impl Captures {
    /// Correctly wired: one buffer, both ends — what the composition root builds.
    fn wired() -> Self {
        let shared = capture();
        Self {
            ledger: Arc::clone(&shared),
            provider: shared,
        }
    }

    /// Mis-wired: the provider reads a buffer nothing writes to, so a call that
    /// happened and was billed correlates no row.
    fn orphaned() -> Self {
        Self {
            ledger: capture(),
            provider: capture(),
        }
    }

    /// Correctly wired to a buffer the TEST also holds — so something else can write
    /// into it and the turn sees more ids than its one call produced.
    fn shared_with(buffer: &LlmCallCapture) -> Self {
        Self {
            ledger: Arc::clone(buffer),
            provider: Arc::clone(buffer),
        }
    }
}

/// The refusal of a turn that must not complete. `CoachCliOutcome` is not `Debug`
/// (it carries the whole session), so `expect_err` is unavailable here.
trait ExpectRefused {
    fn expect_err_refusal(self, what: &str) -> anyhow::Error;
}
impl ExpectRefused for anyhow::Result<CoachCliOutcome> {
    fn expect_err_refusal(self, what: &str) -> anyhow::Error {
        match self {
            Ok(outcome) => panic!(
                "{what}: expected a refusal, got a settled session `{}`",
                outcome.session.id.as_str()
            ),
            Err(error) => error,
        }
    }
}

/// The same, returning the `Result` — the wiring-fault cases below are about what
/// does NOT get recorded, so they need the `Err`.
///
/// `captures` is the r1.s4.w1 seam for those cases. `CoachWiring.captured` is the
/// buffer the ATTRIBUTED PROVIDER reads, and the capturing ledger repo writes to its
/// own; wiring them to DIFFERENT buffers is precisely the mis-pairing `#132` showed a
/// caller can perform, and it is now reproducible without a `Coach` constructor.
#[allow(clippy::too_many_arguments)]
async fn try_turn(
    db: &Db,
    run_id: &BacktestRunId,
    provider: ScriptedProvider,
    turn_timeout: Option<Duration>,
    max_dsl_bytes: Option<usize>,
    redactor: Redactor,
    prompt_dir: Option<PathBuf>,
    captures: Captures,
) -> anyhow::Result<CoachCliOutcome> {
    let clock = FakeClock::at(1_700_000_000_000);
    let wiring = CoachWiring {
        provider,
        llm_repo: CapturingLlmRepo::new(
            SqliteLlmCallRepo::with_deps(db.pool().clone(), clock),
            captures.ledger,
        ),
        redactor,
        prices: test_prices(),
        clock,
        key_source: None,
        config: config(),
        prompt_dir,
        turn_timeout,
        max_dsl_bytes,
        captured: captures.provider,
        session_id: None,
        registry: None,
    };
    let source = SqliteCoachTurnSource::new(db.pool().clone());
    let coaching_repo = SqliteCoachingRepo::with_deps(db.pool().clone(), clock);
    run_coach_with(wiring, &source, &coaching_repo, run_id).await
}

/// How many coaching sessions are on disk — the never-silence counter.
async fn session_count(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM coaching_sessions")
        .fetch_one(db.pool())
        .await
        .expect("count the coaching sessions")
}

/// How many SETTLED sessions are on disk (r1.s4.w1).
///
/// The counter that matters changed shape when the turn started CLAIMING a session
/// before it calls: a wiring or local fault now leaves a `pending` claim behind — by
/// design, because that claim is what a later turn finalizes as `Interrupted`
/// instead of leaving the crash silent. What must still be zero is a SETTLED row: a
/// recorded outcome for a turn that never produced one is the false record, and the
/// pending claim is not one.
async fn settled_session_count(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM coaching_sessions WHERE outcome <> 'pending'")
        .fetch_one(db.pool())
        .await
        .expect("count the settled coaching sessions")
}

/// The stored `outcome` of the one session row, when there is exactly one.
async fn only_session_outcome(db: &Db) -> String {
    sqlx::query_scalar("SELECT outcome FROM coaching_sessions")
        .fetch_one(db.pool())
        .await
        .expect("read the one session row's outcome")
}

/// The recorded failure of a turn — panicking if it produced a proposal instead.
fn failure_of(outcome: &CoachCliOutcome) -> &CoachFailure {
    match &outcome.outcome_ref() {
        SessionOutcome::Failed { failure } => failure,
        SessionOutcome::Proposed { .. } => panic!("expected a recorded failure, got a proposal"),
        SessionOutcome::Pending => panic!("expected a settled turn, got an open claim"),
    }
}

/// Assert the session really is on disk — never silence means a ROW, not a return
/// value.
async fn assert_persisted(db: &Db, outcome: &CoachCliOutcome) {
    let repo = SqliteCoachingRepo::new(db.pool().clone());
    let stored = repo
        .get_session(&outcome.session.id)
        .await
        .expect("get_session")
        .expect("the failed turn left a session row behind");
    assert_eq!(&stored, &outcome.session, "the row is the returned session");
}

trait OutcomeRef {
    fn outcome_ref(&self) -> &SessionOutcome;
}
impl OutcomeRef for CoachCliOutcome {
    fn outcome_ref(&self) -> &SessionOutcome {
        &self.session.outcome
    }
}

// ---------------------------------------------------------------------------
// The seven typed failures
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_with_no_tool_call_is_recorded_as_zero_calls() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, calls) =
        ScriptedProvider::new(vec![text_only("I think you should try RSI 21.")]);

    let outcome = turn_with(&db, &run_id, provider, None, None).await;

    assert!(matches!(failure_of(&outcome), CoachFailure::ZeroCalls));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one provider call");
    assert!(
        outcome.session.llm_call_id.is_some(),
        "the turn reached the provider, so it names its ledger row"
    );
    assert_persisted(&db, &outcome).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_with_several_tool_calls_is_recorded_as_several_calls() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, calls) = ScriptedProvider::new(vec![with_calls(vec![
        call(
            "c1",
            "propose_mutation",
            good_args("entry.lhs.indicator.rsi.period"),
        ),
        call("c2", "propose_mutation", good_args("exits[0].distance_pct")),
    ])]);

    let outcome = turn_with(&db, &run_id, provider, None, None).await;

    match failure_of(&outcome) {
        CoachFailure::SeveralCalls {
            count,
            propose_mutation_count,
        } => {
            assert_eq!(*count, 2);
            assert_eq!(
                *propose_mutation_count, 2,
                "both calls were propose_mutation, and the record says so"
            );
        }
        other => panic!("expected SeveralCalls, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "still ONE provider call");
    assert_persisted(&db, &outcome).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_calls_counts_a_foreign_named_call_separately() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, _calls) = ScriptedProvider::new(vec![with_calls(vec![
        call(
            "c1",
            "propose_mutation",
            good_args("entry.lhs.indicator.rsi.period"),
        ),
        call("c2", "finalize_strategy", json!({})),
    ])]);

    let outcome = turn_with(&db, &run_id, provider, None, None).await;

    // One proposal plus one reach for a tool the coach does not have is a
    // DIFFERENT mistake from proposing twice; the recorded reason has to say which.
    match failure_of(&outcome) {
        CoachFailure::SeveralCalls {
            count,
            propose_mutation_count,
        } => {
            assert_eq!(*count, 2, "two tool calls arrived");
            assert_eq!(
                *propose_mutation_count, 1,
                "only one of them was propose_mutation"
            );
        }
        other => panic!("expected SeveralCalls, got {other:?}"),
    }
    let rendered = failure_of(&outcome).to_string();
    assert!(
        rendered.contains("1 of them propose_mutation"),
        "the recorded reason must not claim two propose_mutation calls: {rendered}"
    );
    assert_persisted(&db, &outcome).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unparseable_arguments_are_recorded_as_malformed() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, calls) = ScriptedProvider::new(vec![with_calls(vec![call(
        "c1",
        "propose_mutation",
        // `new_value` is not the tagged shape the tool declares.
        json!({ "path": "entry.lhs.indicator.rsi.period", "new_value": 21, "hypothesis": "faster" }),
    )])]);

    let outcome = turn_with(&db, &run_id, provider, None, None).await;

    match failure_of(&outcome) {
        CoachFailure::MalformedArguments { detail } => assert!(
            detail.contains("propose_mutation"),
            "the recorded reason names what failed: {detail}"
        ),
        other => panic!("expected MalformedArguments, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_persisted(&db, &outcome).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bare_float_threshold_is_recorded_as_malformed() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, _calls) = ScriptedProvider::new(vec![with_calls(vec![call(
        "c1",
        "propose_mutation",
        json!({
            "path": "exits[0].distance_pct",
            // The tool schema promises "a decimal STRING, never a float".
            "new_value": { "type": "Threshold", "value": 0.03 },
            "hypothesis": "a tighter stop should raise expectancy",
        }),
    )])]);

    let outcome = turn_with(&db, &run_id, provider, None, None).await;

    // NFR-2: the f64 ingress path is closed. Accepting the float would write a
    // binary-rounded threshold into the strategy — a number the coach did not
    // propose and the trader cannot reproduce.
    match failure_of(&outcome) {
        CoachFailure::MalformedArguments { detail } => assert!(
            detail.contains("propose_mutation"),
            "the recorded reason names what failed: {detail}"
        ),
        other => panic!("expected MalformedArguments, got {other:?}"),
    }
    assert_persisted(&db, &outcome).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unrecognized_argument_is_recorded_as_malformed() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, _calls) = ScriptedProvider::new(vec![with_calls(vec![call(
        "c1",
        "propose_mutation",
        json!({
            "path": "entry.lhs.indicator.rsi.period",
            "new_value": { "type": "Period", "value": 21 },
            "hypothesis": "a slower RSI should cut whipsaw entries",
            // Not a field of the tool. Accepting it silently is how a model's
            // misunderstanding of the surface becomes a proposal that does
            // something other than what it described (PR #93's rule).
            "confidence": "high",
        }),
    )])]);

    let outcome = turn_with(&db, &run_id, provider, None, None).await;

    match failure_of(&outcome) {
        CoachFailure::MalformedArguments { detail } => assert!(
            detail.contains("confidence"),
            "the recorded reason names the argument it did not recognize: {detail}"
        ),
        other => panic!("expected MalformedArguments, got {other:?}"),
    }
    assert_persisted(&db, &outcome).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_hypothesis_is_recorded_as_malformed() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, _calls) = ScriptedProvider::new(vec![with_calls(vec![call(
        "c1",
        "propose_mutation",
        json!({
            "path": "entry.lhs.indicator.rsi.period",
            "new_value": { "type": "Period", "value": 21 },
            "hypothesis": "   ",
        }),
    )])]);

    let outcome = turn_with(&db, &run_id, provider, None, None).await;

    match failure_of(&outcome) {
        CoachFailure::MalformedArguments { detail } => assert!(
            detail.contains("hypothesis"),
            "a mutation without a stated hypothesis is not a proposal: {detail}"
        ),
        other => panic!("expected MalformedArguments, got {other:?}"),
    }
    assert_persisted(&db, &outcome).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mutation_that_does_not_apply_is_recorded_inapplicable() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, _calls) = ScriptedProvider::new(vec![with_calls(vec![call(
        "c1",
        "propose_mutation",
        json!({
            // A path that addresses nothing in this strategy — the real `apply()`
            // decides, not a lookalike check here.
            "path": "entry.lhs.indicator.ema.period",
            "new_value": { "type": "Period", "value": 21 },
            "hypothesis": "an EMA would be steadier",
        }),
    )])]);

    let outcome = turn_with(&db, &run_id, provider, None, None).await;

    match failure_of(&outcome) {
        CoachFailure::InapplicableMutation { mutation, error } => {
            assert!(
                matches!(error, MutationError::UnknownPath { .. }),
                "the w1 MutationError is carried verbatim: {error:?}"
            );
            assert!(
                format!("{mutation:?}").contains("ema.period"),
                "the rejected mutation is recorded too, so the turn is reconstructable"
            );
        }
        other => panic!("expected InapplicableMutation, got {other:?}"),
    }
    assert_persisted(&db, &outcome).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_that_outlives_the_guard_is_recorded_as_a_timeout() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, calls) = ScriptedProvider::stalling(Duration::from_secs(30));

    // A test-shortened guard: the mechanism is the composer's `tokio::time::timeout`
    // (audit C5); only the value moves.
    let outcome = turn_with(
        &db,
        &run_id,
        provider,
        Some(Duration::from_millis(20)),
        None,
    )
    .await;

    match failure_of(&outcome) {
        // The MEASURED wait, not the configured budget: at least the guard, and
        // nowhere near the provider's own 30s stall.
        CoachFailure::ProviderTimeout { elapsed_ms } => {
            assert!(
                (20..5_000).contains(elapsed_ms),
                "the recorded elapsed_ms must be the measured wait past the 20ms guard, got {elapsed_ms}"
            );
        }
        other => panic!("expected ProviderTimeout, got {other:?}"),
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the timed-out call is not retried"
    );
    assert_persisted(&db, &outcome).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_dsl_is_recorded_as_context_overflow_before_any_call() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, calls) = ScriptedProvider::new(vec![with_calls(vec![call(
        "c1",
        "propose_mutation",
        good_args("entry.lhs.indicator.rsi.period"),
    )])]);

    // A budget the canonical strategy cannot fit under.
    let outcome = turn_with(&db, &run_id, provider, None, Some(10)).await;

    match failure_of(&outcome) {
        CoachFailure::ContextOverflow { detail } => assert!(
            detail.contains("budget"),
            "the recorded reason states the measured overflow: {detail}"
        ),
        other => panic!("expected ContextOverflow, got {other:?}"),
    }

    // The point of a PRE-call check: it costs nothing.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "context overflow is detected before the provider is called"
    );
    assert!(
        outcome.session.llm_call_id.is_none(),
        "a pre-call failure records no ledger row (audit C3)"
    );
    assert_persisted(&db, &outcome).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dsl_budget_measures_the_rendered_form_not_the_compact_one() {
    let (_tmp, db, run_id) = seeded().await;
    let dsl: StrategyDsl =
        serde_json::from_str(&canonical_dsl_json()).expect("the canonical fixture parses");
    let compact = serde_json::to_string(&dsl)
        .expect("serialize compactly")
        .len();

    let (provider, calls) = ScriptedProvider::new(vec![with_calls(vec![call(
        "c1",
        "propose_mutation",
        good_args("entry.lhs.indicator.rsi.period"),
    )])]);

    // A budget the COMPACT serialization fits exactly. The turn sends PRETTY JSON,
    // which is strictly larger, so a check measuring the compact form would pass
    // this document and then send a message that does not fit — a pre-call refusal
    // that refuses the wrong things.
    let outcome = turn_with(&db, &run_id, provider, None, Some(compact)).await;

    match failure_of(&outcome) {
        CoachFailure::ContextOverflow { detail } => assert!(
            detail.contains(&compact.to_string()),
            "the recorded reason states the budget it measured against: {detail}"
        ),
        other => panic!("expected ContextOverflow, got {other:?}"),
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "still a pre-call refusal, so it still costs nothing"
    );
    assert_persisted(&db, &outcome).await;
}

/// The DSL budget bounds the one variable-length CONTEXT field. It cannot see the
/// resolved system prompt, which an operator's `$PULSE_PROMPT_DIR/coach.md`
/// overlay owns and can make arbitrarily large — so before PR #128 (finding C1) a
/// huge overlay sailed past the pre-call check and was sent anyway. The refusal
/// belongs where every other pre-call refusal is: before the call, recorded, free.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_system_prompt_is_recorded_as_context_overflow_before_any_call() {
    let (_tmp, db, run_id) = seeded().await;

    // ~184 KiB of overlay, well past the whole-turn ceiling, over a canonical DSL
    // that fits its own sub-budget comfortably — so ONLY the whole-turn check can
    // refuse this turn.
    let overlay_dir = TempDir::new().expect("tempdir");
    std::fs::write(
        overlay_dir.path().join("coach.md"),
        "OVERSIZED COACH PROMPT\n".repeat(8_000),
    )
    .expect("write the oversized overlay");

    let (provider, calls) = ScriptedProvider::new(vec![with_calls(vec![call(
        "c1",
        "propose_mutation",
        good_args("entry.lhs.indicator.rsi.period"),
    )])]);

    let outcome = turn_with_redactor(
        &db,
        &run_id,
        provider,
        None,
        None,
        Redactor::default(),
        Some(overlay_dir.path().to_path_buf()),
    )
    .await;

    match failure_of(&outcome) {
        CoachFailure::ContextOverflow { detail } => assert!(
            detail.contains("budget"),
            "the recorded reason states the budget it measured against: {detail}"
        ),
        other => panic!("expected ContextOverflow, got {other:?}"),
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "an oversized prompt must be refused BEFORE the provider is called"
    );
    assert!(
        outcome.session.llm_call_id.is_none(),
        "a pre-call failure records no ledger row (audit C3)"
    );
    assert_persisted(&db, &outcome).await;
}

/// A run and a version that do not belong together used to be a CALLER fault the
/// turn had to CHECK for (PR #128, finding F3): `Coach::run_turn` took both, so a
/// direct caller could prompt on one version's DSL about another version's result
/// and persist a session whose two foreign keys were individually valid and jointly
/// false.
///
/// **r1.s4.w1 removed the fault instead of re-checking it** (`#132`). The sealed
/// turn takes identifiers, and the projection loads the version FROM the run — there
/// is no argument through which the wrong version could arrive, and
/// `CoachTurnError::RunVersionMismatch` no longer exists because the state it named
/// is unconstructible. This case therefore keeps its subject and changes its
/// question: it seeds the very version a pre-seal caller would have handed in, and
/// asserts the recorded row names the run's OWN version, in the database, not just
/// in the returned value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_records_the_version_the_run_names_and_never_a_strangers() {
    let (_tmp, db, run_id) = seeded().await;
    let clock = FakeClock::at(1_700_000_000_000);
    let run_repo = SqliteBacktestRunRepo::with_deps(db.pool().clone(), clock);
    let strategy_repo = SqliteStrategyRepo::new(db.pool().clone());

    let run = run_repo
        .get_run(&run_id)
        .await
        .expect("load the run")
        .expect("the seeded run exists");
    let owner = strategy_repo
        .get_version(&run.strategy_version_id)
        .await
        .expect("load the run's version")
        .expect("the run's version exists");

    // A second version under the same strategy that this run was never produced
    // against — a real row, and the wrong one.
    let stranger = strategy_repo
        .create_version(NewVersion {
            strategy_id: owner.strategy_id.clone(),
            parent_version_id: Some(owner.id.clone()),
            dsl_json: canonical_dsl_json(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create the stranger version");

    let (provider, calls) = ScriptedProvider::new(vec![with_calls(vec![call(
        "c1",
        "propose_mutation",
        good_args("entry.lhs.indicator.rsi.period"),
    )])]);
    let outcome = turn_with(&db, &run_id, provider, None, None).await;

    assert_eq!(calls.load(Ordering::SeqCst), 1, "one call, one turn");
    assert_eq!(
        outcome.session.strategy_version_id, owner.id,
        "the recorded session names the version the RUN names"
    );
    assert_ne!(
        outcome.session.strategy_version_id, stranger.id,
        "and never the stranger a caller could once have offered"
    );

    // In the row, not just in the return value: the audit trail is the artifact.
    let stored: String =
        sqlx::query_scalar("SELECT strategy_version_id FROM coaching_sessions WHERE id = ?1")
            .bind(outcome.session.id.as_str().to_owned())
            .fetch_one(db.pool())
            .await
            .expect("read the session row");
    assert_eq!(stored, run.strategy_version_id.as_str());
}

/// A turn that reached the provider MUST name the ledger row that call minted: NULL
/// there would claim a correlation that does not exist, and audit C3 reads a NULL on
/// a POST-call session as exactly that.
///
/// **The mis-wiring is reproduced through the composition root now** (r1.s4.w1). The
/// pairing used to be `Coach::new`'s two independent arguments; it is now
/// `CoachWiring.captured` versus the buffer the capturing ledger repo writes to, and
/// handing the attributed provider a DIFFERENT buffer is the same fault in the one
/// place it can still be made. The turn refuses rather than writing NULL.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successful_turn_that_captured_no_ledger_row_is_a_local_fault() {
    let (_tmp, db, run_id) = seeded().await;

    let (provider, calls) = ScriptedProvider::new(vec![with_calls(vec![call(
        "c1",
        "propose_mutation",
        good_args("entry.lhs.indicator.rsi.period"),
    )])]);

    // The provider reads a buffer nothing writes to: the ledger decorator still mints
    // its row, into the OTHER buffer, so this turn sees zero ids for a call that
    // happened and was billed.
    let error = try_turn(
        &db,
        &run_id,
        provider,
        None,
        None,
        Redactor::default(),
        None,
        Captures::orphaned(),
    )
    .await
    .expect_err_refusal("a response with no captured ledger row");

    assert!(
        matches!(
            error.downcast_ref::<CoachTurnError>(),
            Some(CoachTurnError::LedgerRowMissing)
        ),
        "expected LedgerRowMissing, got {error:#}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "this is a POST-call fault — the call did happen"
    );
    // r1.s4.w1: the claim exists (it is what a later turn finalizes as
    // `Interrupted`), and it is still PENDING. What a wiring fault must never leave
    // is a SETTLED row asserting an outcome this turn never produced.
    assert_eq!(
        settled_session_count(&db).await,
        0,
        "a wiring fault settles nothing"
    );
    assert_eq!(
        only_session_outcome(&db).await,
        "pending",
        "it leaves the claim open rather than recording a false outcome"
    );
}

/// Several ids for one turn means the buffer is shared or looping, and there is no
/// honest way to pick one. Taking `.last()` — what the code did before PR #128
/// (finding G1) — is a guess that can name another turn's row.
///
/// Driven through the real composition (r1.s4.w1): the scripted provider pushes ONE
/// impostor id into the same buffer the ledger decorator writes its genuine row to,
/// which is exactly the "the buffer is shared with something else" shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ambiguous_capture_is_refused_rather_than_guessed() {
    let (_tmp, db, run_id) = seeded().await;

    let shared: LlmCallCapture = capture();
    let (provider, calls) = ScriptedProvider::pushing(
        vec![with_calls(vec![call(
            "c1",
            "propose_mutation",
            good_args("entry.lhs.indicator.rsi.period"),
        )])],
        Arc::clone(&shared),
        vec![LlmCallId::new("call-impostor".to_owned())],
    );

    let error = try_turn(
        &db,
        &run_id,
        provider,
        None,
        None,
        Redactor::default(),
        None,
        Captures::shared_with(&shared),
    )
    .await
    .expect_err_refusal("two ids for one turn");

    match error.downcast_ref::<CoachTurnError>() {
        Some(CoachTurnError::LedgerRowsAmbiguous { seen }) => assert_eq!(
            *seen, 2,
            "the impostor and the decorator's genuine row are both in the buffer"
        ),
        _ => panic!("expected LedgerRowsAmbiguous, got {error:#}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "still one call per turn");
    assert_eq!(
        settled_session_count(&db).await,
        0,
        "an unresolvable correlation settles nothing"
    );
}

// ---------------------------------------------------------------------------
// The taxonomy, as a set
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_tool_is_recorded_rather_than_ignored() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, _calls) = ScriptedProvider::new(vec![with_calls(vec![call(
        "c1",
        "finalize_strategy",
        json!({}),
    )])]);

    let outcome = turn_with(&db, &run_id, provider, None, None).await;

    match failure_of(&outcome) {
        CoachFailure::MalformedArguments { detail } => assert!(
            detail.contains("finalize_strategy"),
            "the recorded reason names the tool that was called: {detail}"
        ),
        other => panic!("expected MalformedArguments, got {other:?}"),
    }
    assert_persisted(&db, &outcome).await;
}

// ---------------------------------------------------------------------------
// The seventh variant (r1.s2.w4, operator ruling 2026-08-29)
// ---------------------------------------------------------------------------

/// A canary that the provider's own error text echoes back — an error body can
/// quote the request that produced it, so this road needs the same
/// scrub-before-record discipline `classify()` applies to tool arguments.
const TRANSPORT_CANARY: &str = "sk-canary-TRANSPORT-4b3a2c1d9e8f7061";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transport_failure_is_recorded_rather_than_returned() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, calls) = ScriptedProvider::failing(LlmError::Provider(format!(
        "HTTP 503 from upstream while sending key {TRANSPORT_CANARY}"
    )));

    let outcome = turn_with_redactor(
        &db,
        &run_id,
        provider,
        None,
        None,
        Redactor::from_config(vec![TRANSPORT_CANARY.to_owned()]),
        None,
    )
    .await;

    // The hole this item closes: before w4 this path returned an error and left
    // NO row behind, so a provider outage was the one silent coach turn.
    match failure_of(&outcome) {
        CoachFailure::TransportFailure { detail } => {
            assert!(
                detail.contains("503"),
                "the provider's own error text is preserved: {detail}"
            );
            assert!(
                !detail.contains(TRANSPORT_CANARY),
                "the preserved error text must be scrubbed: {detail}"
            );
        }
        other => panic!("expected TransportFailure, got {other:?}"),
    }

    // One call was attempted, and it is not retried (grill L3).
    assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one attempt");

    // The call WAS billed — under #169's R1 the decorator writes the row for an
    // errored attempt too, carrying the scrubbed detail in `completion` and the
    // tokens that are visible (zero; a transport error exposes no usage).
    let llm_call_id = outcome
        .session
        .llm_call_id
        .clone()
        .expect("a billed transport fault names its ledger row");
    let (stored_detail, input_tokens, output_tokens): (Option<String>, i64, i64) = sqlx::query_as(
        "SELECT completion, input_tokens, output_tokens FROM llm_call WHERE id = ?1",
    )
    .bind(llm_call_id.as_str())
    .fetch_one(db.pool())
    .await
    .expect("read the errored call's row");
    let stored_detail = stored_detail.expect("the errored call stores its detail");
    assert!(
        stored_detail.contains("503"),
        "the row keeps the provider's error text: {stored_detail}"
    );
    assert!(
        !stored_detail.contains(TRANSPORT_CANARY),
        "the row's detail is scrubbed: {stored_detail}"
    );
    assert_eq!(
        (input_tokens, output_tokens),
        (0, 0),
        "a transport error exposes no usage — visible tokens only"
    );

    assert_persisted(&db, &outcome).await;
}

/// Drive one turn against an EXPLICIT coaching repo, returning the error text
/// rather than panicking — the two cases below are about what does NOT get
/// recorded, so they need the `Err`.
async fn try_turn_against<K>(
    db: &Db,
    run_id: &BacktestRunId,
    provider: ScriptedProvider,
    turn_timeout: Option<Duration>,
    coaching_repo: &K,
) -> Result<(), String>
where
    K: CoachingRepository + Send + Sync,
{
    let clock = FakeClock::at(1_700_000_000_000);
    let ids = Arc::new(Mutex::new(Vec::new()));
    let wiring = CoachWiring {
        provider,
        llm_repo: CapturingLlmRepo::new(
            SqliteLlmCallRepo::with_deps(db.pool().clone(), clock),
            Arc::clone(&ids),
        ),
        redactor: Redactor::default(),
        prices: test_prices(),
        clock,
        key_source: None,
        config: config(),
        prompt_dir: None,
        turn_timeout,
        max_dsl_bytes: None,
        captured: ids,
        session_id: None,
        registry: None,
    };
    let source = SqliteCoachTurnSource::new(db.pool().clone());
    run_coach_with(wiring, &source, coaching_repo, run_id)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e:#}"))
}

// ---------------------------------------------------------------------------
// What is NOT a coaching outcome (PR #128, findings 5 and 6)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_fault_is_surfaced_rather_than_recorded_as_a_transport_failure() {
    let (_tmp, db, run_id) = seeded().await;
    // An unpriced model and a failed ledger insert both arrive here as
    // `LlmError::Config` / `LlmError::Local` — this process faulting on the call
    // path, not the provider failing.
    let (provider, _calls) = ScriptedProvider::failing(LlmError::Config(
        "unknown model `glm-9.9` in the price table".to_owned(),
    ));

    let coaching_repo =
        SqliteCoachingRepo::with_deps(db.pool().clone(), FakeClock::at(1_700_000_000_000));
    let error = try_turn_against(&db, &run_id, provider, None, &coaching_repo)
        .await
        .expect_err("a local fault must surface, not be recorded");
    assert!(
        error.contains("glm-9.9"),
        "the fault is preserved at the edge: {error}"
    );

    // And nothing was RECORDED. `TransportFailure` says "the coach's provider call
    // failed"; recording that for a price-table miss would put a false reason in
    // the one record an auditor trusts.
    //
    // r1.s4.w1: the claim the turn committed before calling is still there, still
    // `pending`. That is not a recorded outcome — it is the reservation whose whole
    // purpose is to survive a turn that ends badly, and a later turn finalizes it as
    // `Interrupted`. What must be zero is SETTLED rows.
    assert_eq!(
        settled_session_count(&db).await,
        0,
        "a local fault settles no coaching session"
    );
    assert_eq!(
        session_count(&db).await,
        1,
        "it does leave its claim behind — that is what makes the crash recoverable"
    );
    assert_eq!(only_session_outcome(&db).await, "pending");
}

/// A coaching repo that refuses every write, CLAIM INCLUDED — the unwritable-store
/// fixture. A claim that quietly succeeded here would let a turn believe it had
/// reserved a session id on a database that cannot hold one.
struct RefusingCoachingRepo;

impl CoachingRepository for RefusingCoachingRepo {
    fn claim_session(
        &self,
        _claim: CoachSessionClaim,
    ) -> impl Future<Output = Result<CoachSessionClaimResult, DataError>> {
        std::future::ready(Err(DataError::Db(
            "coaching_sessions is unwritable".to_owned(),
        )))
    }

    fn finish_session(
        &self,
        _session_id: &CoachingSessionId,
        _outcome: InitialCoachOutcome,
    ) -> impl Future<Output = Result<CoachingSession, DataError>> {
        std::future::ready(Err(DataError::Db(
            "coaching_sessions is unwritable".to_owned(),
        )))
    }

    fn save_session(
        &self,
        _session: &CoachingSession,
    ) -> impl Future<Output = Result<CoachingSessionId, DataError>> {
        std::future::ready(Err(DataError::Db(
            "coaching_sessions is unwritable".to_owned(),
        )))
    }

    fn get_session(
        &self,
        _id: &CoachingSessionId,
    ) -> impl Future<Output = Result<Option<CoachingSession>, DataError>> {
        std::future::ready(Ok(None))
    }

    fn list_sessions_for_run(
        &self,
        _run_id: &BacktestRunId,
    ) -> impl Future<Output = Result<Vec<CoachingSession>, DataError>> {
        std::future::ready(Ok(Vec::new()))
    }

    fn record_disposition(
        &self,
        _id: &CoachingSessionId,
        _disposition: &Disposition,
    ) -> impl Future<Output = Result<(), DataError>> {
        std::future::ready(Ok(()))
    }

    fn record_modification(
        &self,
        _id: &CoachingSessionId,
        _mutation: &Mutation,
    ) -> impl Future<Output = Result<Proposal, DataError>> {
        std::future::ready(Err(DataError::Db(
            "coaching_proposals is unwritable".to_owned(),
        )))
    }
}

/// A store that ACCEPTS the claim and cannot SETTLE it — the double-fault fixture
/// after r1.s4.w1.
///
/// The double fault is "the turn deviated AND the deviation could not be recorded",
/// and reaching it now requires getting past the claim: claim-before-I/O means a
/// store that refuses everything fails before the provider is ever called, which is
/// a different (and also tested) incident. This fixture is the one that still
/// produces the original one.
struct UnsettleableCoachingRepo;

impl CoachingRepository for UnsettleableCoachingRepo {
    fn claim_session(
        &self,
        _claim: CoachSessionClaim,
    ) -> impl Future<Output = Result<CoachSessionClaimResult, DataError>> {
        std::future::ready(Ok(CoachSessionClaimResult::Claimed))
    }

    fn finish_session(
        &self,
        _session_id: &CoachingSessionId,
        _outcome: InitialCoachOutcome,
    ) -> impl Future<Output = Result<CoachingSession, DataError>> {
        std::future::ready(Err(DataError::Db(
            "coaching_sessions is unwritable".to_owned(),
        )))
    }

    fn save_session(
        &self,
        _session: &CoachingSession,
    ) -> impl Future<Output = Result<CoachingSessionId, DataError>> {
        std::future::ready(Err(DataError::Db(
            "coaching_sessions is unwritable".to_owned(),
        )))
    }

    fn get_session(
        &self,
        _id: &CoachingSessionId,
    ) -> impl Future<Output = Result<Option<CoachingSession>, DataError>> {
        std::future::ready(Ok(None))
    }

    fn list_sessions_for_run(
        &self,
        _run_id: &BacktestRunId,
    ) -> impl Future<Output = Result<Vec<CoachingSession>, DataError>> {
        std::future::ready(Ok(Vec::new()))
    }

    fn record_disposition(
        &self,
        _id: &CoachingSessionId,
        _disposition: &Disposition,
    ) -> impl Future<Output = Result<(), DataError>> {
        std::future::ready(Ok(()))
    }

    fn record_modification(
        &self,
        _id: &CoachingSessionId,
        _mutation: &Mutation,
    ) -> impl Future<Output = Result<Proposal, DataError>> {
        std::future::ready(Err(DataError::Db(
            "coaching_proposals is unwritable".to_owned(),
        )))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_that_cannot_be_recorded_reports_both_halves() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, calls) = ScriptedProvider::stalling(Duration::from_secs(30));

    let error = try_turn_against(
        &db,
        &run_id,
        provider,
        Some(Duration::from_millis(20)),
        &UnsettleableCoachingRepo,
    )
    .await
    .expect_err("an unsettleable session is fatal");

    // The double fault: the write error alone would leave the operator knowing
    // that something could not be written and nothing about what the turn did.
    // The turn's reason exists ONLY in that frame — dropping it loses the incident.
    assert!(
        error.contains("unwritable"),
        "the write error is reported: {error}"
    );
    assert!(
        error.contains("did not answer"),
        "the original failure travels with it: {error}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the turn did reach the provider — that is what makes this the DOUBLE fault"
    );
}

/// r1.s4.w1: a store that cannot even take the CLAIM fails before the provider is
/// called, and says so.
///
/// The counterpart to the double fault above, and the reason the two fixtures are
/// different: claim-before-I/O turns "the store is unwritable" into a fault that
/// costs nothing, and an operator reading it needs to know no call was billed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_that_cannot_be_written_is_fatal_before_any_call() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, calls) = ScriptedProvider::new(vec![with_calls(vec![call(
        "c1",
        "propose_mutation",
        good_args("entry.lhs.indicator.rsi.period"),
    )])]);

    let error = try_turn_against(&db, &run_id, provider, None, &RefusingCoachingRepo)
        .await
        .expect_err("a session id that cannot be claimed is fatal");

    assert!(
        error.contains("unwritable"),
        "the store's reason is preserved at the edge: {error}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the claim precedes the call, so an unwritable store costs nothing"
    );
}
