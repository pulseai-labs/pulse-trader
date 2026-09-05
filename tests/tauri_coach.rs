//! r1.s4.w3 — the desktop coach rail's backend half, over the two real command
//! cores.
//!
//! This is the spine's `d13` backend evidence. It drives `coach_turn_core` and
//! `coach_decide_core` — the transport-free cores behind the `coach_turn` and
//! `coach_decide` bus commands — over a REAL `DesktopState` on a migrated temp
//! database, the committed BTCUSDT Parquet fixture, the real SQLite repositories,
//! the real `apply()` framework, the real acceptance adapter and the real engine.
//! **The only double is the provider**, scripted behind the same redacting-ledger
//! decorator and attributed adapter production uses (`tests/coach_turn.rs`'s
//! pattern). No live LLM, no network, no Keychain.
//!
//! What it proves, and why each is here rather than assumed:
//!
//!  1. **One turn, one durable session, one ledger row** — and the DTO carries the
//!     ledger row's own cost and prompt version, never a recomputed one.
//!  2. **The same session id is idempotent** — no second provider call.
//!  3. **A live duplicate is `Busy`** — the single-flight latch, not a second call.
//!  4. **An abandoned claim settles as `interrupted`** with its named recovery.
//!  5. **A pre-`0006` run is `MissingBacktestInputs`** with its recovery, no call.
//!  6. **`record_inapplicable` is `InapplicableAdvice`** with no proposal.
//!  7. **Modify / reject / accept each return the durable state**, accept mints
//!     exactly one child and one run, twice is the same two ids, and a
//!     saved-but-unreadable accept carries both ids with no `after`.
//!  8. **The `#141` latch** refuses an overlapping backtest and releases on success,
//!     on `BusError` and on a panic.
//!  9. **Every DTO decimal is a string**, and **the credential appears nowhere** in
//!     a DTO, an error or a persisted row (the canary).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pulse::{
    BacktestRunId, BacktestRunRequest, CoachActionDto, CoachDecisionRequestDto,
    CoachRequestFingerprint, CoachSessionClaim, CoachTurnDeps, CoachTurnRequestDto,
    CoachingRepository, CoachingSessionId, CreatedBy, CredentialSource, Db, DesktopState,
    Disposition, Hypothesis, InitialCoachOutcome, LlmBackend, LlmCallId, LlmCallRepository,
    LlmConfig, LlmError, LlmProvider, LlmResponse, Message, ModelPrice, Mutation, NewVersion,
    OperationKey, ParamValue, PriceTable, Proposal, Redactor, SessionOutcome, SqliteCoachingRepo,
    SqliteStrategyRepo, StrategyRepository, SystemClock, TokenUsage, ToolCall, ToolDefinition,
    VersionId, coach_decide_core, coach_turn_core, run_backtest_version_core,
};
mod coach_support;

use rust_decimal::Decimal;
use serde_json::json;
use tempfile::TempDir;

/// The committed candle fixture every run reads.
const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

/// An API-key-shaped literal that must never reach a DTO, an error or a stored
/// row. NOT a real key.
const FAKE_KEY: &str = "sk-COACHRAIL1234abcd5678efgh9012ijkl";

/// The sweepable leaf every fixture mutation addresses.
const RSI_PERIOD: &str = "entry.lhs.indicator.rsi.period";

/// The same minimal, valid DSL the other coach binaries use — it produces real
/// trades over the fixture, so the parent run is a genuine one.
const MINIMAL_DSL: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold (rail)",
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
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// The scripted provider — the ONE double
// ---------------------------------------------------------------------------

/// A scripted [`LlmProvider`]: hands back queued responses and counts calls.
struct ScriptedProvider {
    scripts: Mutex<VecDeque<LlmResponse>>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedProvider {
    fn new(responses: Vec<LlmResponse>) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                scripts: Mutex::new(responses.into()),
                calls: Arc::clone(&calls),
            },
            calls,
        )
    }
}

impl LlmProvider for ScriptedProvider {
    fn chat(
        &self,
        _messages: Vec<Message>,
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
    ) -> impl Future<Output = Result<LlmResponse, LlmError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self.scripts.lock().expect("scripts lock").pop_front();
        std::future::ready(Ok(next.unwrap_or_else(|| LlmResponse {
            content: Some("(script exhausted)".to_owned()),
            tool_calls: Vec::new(),
            usage: usage(),
        })))
    }
}

/// A provider that signals it was entered and then never answers — the live turn
/// a duplicate must be refused against, with no timing assumption anywhere.
struct HangingProvider {
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl LlmProvider for HangingProvider {
    fn chat(
        &self,
        _messages: Vec<Message>,
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
    ) -> impl Future<Output = Result<LlmResponse, LlmError>> {
        if let Some(tx) = self.entered.lock().expect("entered lock").take() {
            let _ = tx.send(());
        }
        std::future::pending()
    }
}

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 1_000,
        output_tokens: 200,
    }
}

/// One `propose_mutation` tool call — the happy script.
fn propose_call(path: &str, new_value: &serde_json::Value, hypothesis: &str) -> LlmResponse {
    LlmResponse {
        content: None,
        tool_calls: vec![ToolCall {
            id: "call-1".to_owned(),
            name: "propose_mutation".to_owned(),
            arguments: json!({
                "path": path,
                "new_value": new_value.clone(),
                "hypothesis": hypothesis,
            }),
        }],
        usage: usage(),
    }
}

/// One `record_inapplicable` tool call — structural advice, recorded (#131).
fn inapplicable_call(intent: &str, evidence: &str) -> LlmResponse {
    LlmResponse {
        content: None,
        tool_calls: vec![ToolCall {
            id: "call-1".to_owned(),
            name: "record_inapplicable".to_owned(),
            arguments: json!({ "intent": intent, "evidence": evidence }),
        }],
        usage: usage(),
    }
}

fn test_prices() -> PriceTable {
    let mut models = HashMap::new();
    models.insert(
        "glm-5.3-flash".to_owned(),
        ModelPrice {
            input_per_mtok: Decimal::new(1, 0),
            output_per_mtok: Decimal::new(2, 0),
        },
    );
    PriceTable::from_config("CNY", models)
}

fn config() -> LlmConfig {
    LlmConfig {
        backend: LlmBackend::Ollama,
        model: "glm-5.3-flash".to_owned(),
        temperature: 0.0,
        max_tokens: 2_048,
    }
}

/// The deps the desktop command builds, with the provider injected — the same
/// shape the wrapper composes from the resolved credential, minus the credential.
fn deps<P>(provider: P) -> CoachTurnDeps<P> {
    CoachTurnDeps {
        provider,
        prices: test_prices(),
        // The canary: the redactor is built from the key exactly as the command
        // builds it, so anything echoing the key back is scrubbed at every store.
        redactor: Redactor::from_config(vec![FAKE_KEY.to_owned()]),
        key_source: Some(CredentialSource::Env),
        config: config(),
        prompt: "You are PulseTrader's coach.".to_owned(),
        prompt_version: Some("promptver-abc123".to_owned()),
        turn_timeout: None,
        max_dsl_bytes: None,
    }
}

/// The same deps after a release changed the prompt — which moves the request
/// fingerprint, exactly as a new model or a new tool definition would.
fn deps_after_a_prompt_change<P>(provider: P) -> CoachTurnDeps<P> {
    CoachTurnDeps {
        prompt: "You are PulseTrader's coach. (reworded in a later release)".to_owned(),
        prompt_version: Some("promptver-def456".to_owned()),
        ..deps(provider)
    }
}

// ---------------------------------------------------------------------------
// The fixture world — everything but the provider is real
// ---------------------------------------------------------------------------

struct World {
    _tmp: TempDir,
    db_path: PathBuf,
    store_root: PathBuf,
    version_id: VersionId,
    parent_run_id: BacktestRunId,
}

impl World {
    async fn state(&self) -> DesktopState {
        DesktopState::open_with_store(
            &self.db_path,
            pulse::CandleStore::with_base_dir(self.store_root.clone()),
        )
        .await
        .expect("open desktop state over the temp db + fixture store")
    }

    async fn db(&self) -> Db {
        Db::with_path(&self.db_path).await.expect("open db")
    }

    async fn count(&self, sql: &str) -> i64 {
        let db = self.db().await;
        sqlx::query_scalar(sql).fetch_one(db.pool()).await.unwrap()
    }

    async fn sessions(&self) -> SqliteCoachingRepo<SystemClock> {
        SqliteCoachingRepo::new(self.db().await.pool().clone())
    }

    /// Seed a settled `proposed` session over the real repository, so the decision
    /// path derives its provenance from a row production could have written.
    async fn seed_proposed_session(&self, session_id: &str, period: u32) -> CoachingSessionId {
        let db = self.db().await;
        sqlx::query(
            "INSERT INTO llm_call \
             (id, backend, model, prompt_messages, completion, input_tokens, output_tokens, \
              cost, cost_currency, created_at, created_by, schema_version, prompt_version) \
             VALUES ('call-seed', 'ollama', 'glm-5.3-flash', '[]', NULL, 1, 1, '0.5', 'CNY', \
                     '2026-09-05T00:00:00.000Z', 'coach_llm', 1, 'promptver-seed')",
        )
        .execute(db.pool())
        .await
        .ok();

        let repo = SqliteCoachingRepo::new(db.pool().clone());
        let id = CoachingSessionId::new(session_id.to_owned());
        repo.claim_session(CoachSessionClaim {
            session_id: id.clone(),
            backtest_run_id: self.parent_run_id.clone(),
            strategy_version_id: self.version_id.clone(),
            request_fingerprint: CoachRequestFingerprint::new(
                "aa11bb22cc33dd44ee55ff6600778899aabbccddeeff00112233445566778899",
            )
            .unwrap(),
            created_at: "2026-09-05T00:00:00.000Z".to_owned(),
        })
        .await
        .expect("claim the session");
        repo.finish_session(
            &id,
            InitialCoachOutcome {
                llm_call_id: Some(LlmCallId::new("call-seed")),
                outcome: SessionOutcome::Proposed {
                    proposal: Proposal {
                        mutation: Mutation::SetParam {
                            path: RSI_PERIOD.to_owned(),
                            new_value: ParamValue::Period { value: period },
                        },
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
}

/// A migrated temp database, a persisted version, and a REAL parent backtest run
/// produced by the desktop's own command core.
async fn world() -> World {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("pulse.db");
    let store_root = tmp.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_root);

    let mut world = World {
        _tmp: tmp,
        db_path,
        store_root,
        version_id: VersionId::new(String::new()),
        parent_run_id: BacktestRunId::new(String::new()),
    };

    let state = world.state().await;
    let strategies: SqliteStrategyRepo<SystemClock> = state.strategy_repo();
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
    world.version_id = version.id.clone();

    // The parent run is produced by the REAL desktop command, so its persisted
    // inputs name real data versions and its summary is a real one.
    let run = run_backtest_version_core(
        &state,
        BacktestRunRequest {
            version_id: version.id.as_str().to_owned(),
        },
    )
    .await
    .expect("the parent backtest runs over the fixture");
    world.parent_run_id = BacktestRunId::new(run.run_id.clone());
    world
}

fn turn_request(session_id: &str, run_id: &BacktestRunId) -> CoachTurnRequestDto {
    CoachTurnRequestDto {
        session_id: session_id.to_owned(),
        run_id: run_id.as_str().to_owned(),
    }
}

fn decision_request(
    session_id: &CoachingSessionId,
    action: CoachActionDto,
) -> CoachDecisionRequestDto {
    CoachDecisionRequestDto {
        session_id: session_id.as_str().to_owned(),
        action,
    }
}

// ---------------------------------------------------------------------------
// 1. one turn, one session, one ledger row — with the LEDGER's cost and version
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fresh_session_id_records_one_proposed_turn_carrying_the_ledger_cost_and_prompt_version()
{
    let world = world().await;
    let state = world.state().await;
    let (provider, calls) = ScriptedProvider::new(vec![propose_call(
        RSI_PERIOD,
        &json!({ "type": "Period", "value": 21 }),
        "a slower RSI trades less often",
    )]);

    let dto = coach_turn_core(
        &state,
        deps(provider),
        turn_request("sess-fresh", &world.parent_run_id),
    )
    .await
    .expect("the turn records a session");

    assert_eq!(dto.session_id, "sess-fresh");
    assert_eq!(dto.run_id, world.parent_run_id.as_str());
    assert_eq!(dto.version_id, world.version_id.as_str());
    assert_eq!(dto.outcome, "proposed");
    assert!(dto.failure.is_none(), "a proposed turn records no failure");
    let proposal = dto
        .proposal
        .as_ref()
        .expect("a proposed turn has a proposal");
    assert_eq!(proposal.mutation.path, RSI_PERIOD);
    assert_eq!(proposal.mutation.new_value, "21");
    assert_eq!(proposal.hypothesis, "a slower RSI trades less often");
    assert_eq!(proposal.disposition, "proposed");
    assert!(proposal.child_version_id.is_none());
    assert!(proposal.accepted_run_id.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly ONE provider call");

    // Exactly one ledger row, and the DTO's cost/version are ITS values.
    assert_eq!(world.count("SELECT COUNT(*) FROM llm_call").await, 1);
    let call_id = dto
        .llm_call_id
        .clone()
        .expect("the turn names its ledger row");
    let row = state
        .llm_call_repo()
        .get_call(&LlmCallId::new(call_id))
        .await
        .expect("read the ledger row")
        .expect("the named row exists");
    let cost = dto
        .cost
        .as_ref()
        .expect("a turn with a call carries a cost");
    assert_eq!(cost.amount, row.cost.normalize().to_string());
    assert_eq!(cost.currency, row.cost_currency);
    assert_eq!(dto.prompt_version, row.prompt_version);
    assert_eq!(
        dto.prompt_version.as_deref(),
        Some("promptver-abc123"),
        "the stamped version is the one the composition root resolved"
    );

    // Exactly one durable session row.
    assert_eq!(
        world.count("SELECT COUNT(*) FROM coaching_sessions").await,
        1
    );
}

// ---------------------------------------------------------------------------
// 2. the same id is idempotent — no second provider call
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_same_session_id_returns_the_same_session_without_a_second_call() {
    let world = world().await;
    let state = world.state().await;
    let (provider, calls) = ScriptedProvider::new(vec![propose_call(
        RSI_PERIOD,
        &json!({ "type": "Period", "value": 21 }),
        "a slower RSI trades less often",
    )]);

    let first = coach_turn_core(
        &state,
        deps(provider),
        turn_request("sess-idem", &world.parent_run_id),
    )
    .await
    .expect("first turn");

    let (provider2, calls2) = ScriptedProvider::new(vec![propose_call(
        RSI_PERIOD,
        &json!({ "type": "Period", "value": 99 }),
        "a DIFFERENT answer nobody may see",
    )]);
    let second = coach_turn_core(
        &state,
        deps(provider2),
        turn_request("sess-idem", &world.parent_run_id),
    )
    .await
    .expect("the reload returns the durable session");

    assert_eq!(first, second, "the reload is the SAME durable session");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        calls2.load(Ordering::SeqCst),
        0,
        "a settled session must not bill a second call"
    );
    assert_eq!(world.count("SELECT COUNT(*) FROM llm_call").await, 1);
}

// ---------------------------------------------------------------------------
// 3. a live duplicate is refused with Busy — no second call, no second row
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_turn_for_a_live_session_is_refused_as_busy() {
    let world = world().await;
    let state = world.state().await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let hanging = HangingProvider {
        entered: Mutex::new(Some(entered_tx)),
    };

    let first = coach_turn_core(
        &state,
        deps(hanging),
        turn_request("sess-live", &world.parent_run_id),
    );
    tokio::pin!(first);

    // Drive the first turn until it is INSIDE the provider call, deterministically.
    tokio::select! {
        _ = &mut first => panic!("the hanging provider must not complete the turn"),
        entered = entered_rx => entered.expect("the provider was entered"),
    }

    let (provider2, calls2) = ScriptedProvider::new(vec![propose_call(
        RSI_PERIOD,
        &json!({ "type": "Period", "value": 21 }),
        "a second answer that must never be asked for",
    )]);
    let refused = coach_turn_core(
        &state,
        deps(provider2),
        turn_request("sess-live", &world.parent_run_id),
    )
    .await
    .expect_err("a live duplicate is refused");

    assert_eq!(refused.code, pulse::BusErrorCode::Busy);
    assert!(
        refused.message.contains("sess-live"),
        "the refusal names the contested key: {}",
        refused.message
    );
    assert_eq!(
        calls2.load(Ordering::SeqCst),
        0,
        "a refused duplicate never reaches the provider"
    );
}

// ---------------------------------------------------------------------------
// 4. an abandoned claim settles as `interrupted`, with its named recovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_abandoned_claim_is_settled_as_interrupted_with_the_new_session_recovery() {
    let world = world().await;
    let state = world.state().await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let hanging = HangingProvider {
        entered: Mutex::new(Some(entered_tx)),
    };

    {
        let first = coach_turn_core(
            &state,
            deps(hanging),
            turn_request("sess-stale", &world.parent_run_id),
        );
        tokio::pin!(first);
        tokio::select! {
            _ = &mut first => panic!("the hanging provider must not complete the turn"),
            entered = entered_rx => entered.expect("the provider was entered"),
        }
        // The future is dropped here — the claim is committed and unfinished, and
        // both the registry entry and the operation latch release on drop.
    }

    // Old enough that no live call can still hold it — adoption is gated on age,
    // because both single-flight registries are process-local and a young pending
    // row may be a turn running in another process.
    age_pending_claim(world.db().await.pool(), "sess-stale", 10).await;

    let (provider2, calls2) = ScriptedProvider::new(vec![propose_call(
        RSI_PERIOD,
        &json!({ "type": "Period", "value": 21 }),
        "no call may be made on an abandoned claimant's behalf",
    )]);
    let dto = coach_turn_core(
        &state,
        deps(provider2),
        turn_request("sess-stale", &world.parent_run_id),
    )
    .await
    .expect("the stale claim settles");

    assert_eq!(dto.outcome, "failed");
    let failure = dto
        .failure
        .as_ref()
        .expect("a failed turn states its reason");
    assert_eq!(failure.kind, "interrupted");
    assert_eq!(failure.recovery, "start a new coaching session");
    assert!(
        dto.proposal.is_none(),
        "an interrupted turn proposes nothing"
    );
    assert_eq!(
        calls2.load(Ordering::SeqCst),
        0,
        "finalizing an abandoned claim spends no money"
    );
}

/// Back-date a `pending` claim so a reload may adopt it.
///
/// Adoption is gated on AGE — a turn cannot outlive its own wall-clock guard, so a
/// claim older than that guard plus a margin is held by nothing. A row just written
/// is indistinguishable from a live call in another process, and refusing it is the
/// safe direction, so a test about the ABANDONED path has to make the row old.
async fn age_pending_claim(pool: &sqlx::SqlitePool, session: &str, minutes: i64) {
    let when = (chrono::Utc::now() - chrono::Duration::minutes(minutes))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    // `0008` pins a recorded session's identity, `created_at` included, so the
    // guard is lifted for this one statement and put straight back.
    coach_support::with_trigger_lifted(
        pool,
        "coaching_sessions_lifecycle",
        &[&format!(
            "UPDATE coaching_sessions SET created_at = '{when}' WHERE id = '{session}'"
        )],
    )
    .await;
}

/// An abandoned claim settles even when the prompt or config has moved on.
///
/// Adoption used to re-claim under the old id through the ordinary turn path, which
/// recomputes the request fingerprint from CURRENT settings; `claim_session` refuses
/// a reused id whose fingerprint moved. So any release that reworded the prompt,
/// changed the model or added a tool left the crash-era row pending, every later ask
/// selected that same row and failed the same way, and coaching for the run was
/// blocked for good.
///
/// An abandoned claim is settled directly instead. The fingerprint answers "is this
/// the same request?", which is the wrong question about a claim already proven
/// abandoned by age — what is being recorded is that a turn ended without an answer.
#[tokio::test]
async fn an_abandoned_claim_settles_even_after_the_prompt_changed() {
    let world = world().await;
    let state = world.state().await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let hanging = HangingProvider {
        entered: Mutex::new(Some(entered_tx)),
    };

    {
        let first = coach_turn_core(
            &state,
            deps(hanging),
            turn_request("sess-before-release", &world.parent_run_id),
        );
        tokio::pin!(first);
        tokio::select! {
            _ = &mut first => panic!("the hanging provider must not complete the turn"),
            entered = entered_rx => entered.expect("the provider was entered"),
        }
    }
    age_pending_claim(world.db().await.pool(), "sess-before-release", 10).await;

    // The release lands: a different prompt, so a different fingerprint.
    let (provider2, calls2) = ScriptedProvider::new(vec![propose_call(
        RSI_PERIOD,
        &json!({ "type": "Period", "value": 21 }),
        "no call may be made on an abandoned claimant's behalf",
    )]);
    let dto = coach_turn_core(
        &state,
        deps_after_a_prompt_change(provider2),
        turn_request("sess-after-release", &world.parent_run_id),
    )
    .await
    .expect("the abandoned claim settles despite the changed fingerprint");

    assert_eq!(dto.outcome, "failed");
    assert_eq!(dto.session_id, "sess-before-release");
    assert_eq!(
        dto.failure.as_ref().expect("a reason").kind,
        "interrupted",
        "settled as interrupted, not refused as a session conflict"
    );
    assert_eq!(calls2.load(Ordering::SeqCst), 0, "settling spends no money");

    // AND the run is not blocked: the next ask starts a fresh turn under its own id.
    let (provider3, calls3) = ScriptedProvider::new(vec![propose_call(
        RSI_PERIOD,
        &json!({ "type": "Period", "value": 21 }),
        "a slower RSI trades less often on this chop",
    )]);
    let next = coach_turn_core(
        &state,
        deps_after_a_prompt_change(provider3),
        turn_request("sess-fresh", &world.parent_run_id),
    )
    .await
    .expect("the run is workable again");
    assert_eq!(next.outcome, "proposed");
    assert_eq!(next.session_id, "sess-fresh");
    assert_eq!(calls3.load(Ordering::SeqCst), 1, "and it really asked");
}

/// A YOUNG pending claim is never adopted — it may be a turn running elsewhere.
///
/// Both single-flight registries are process-local, so a second process sees an
/// adopted id as free and `run_coach_turn` reads its `ExistingPending` as stale,
/// settling a live billed call as `interrupted` and breaking it when it tries to
/// settle its own row. Age is the only cross-process evidence available without a
/// durable lease, and a fresh row carries none of it: the reload defers instead.
#[tokio::test]
async fn a_young_pending_claim_is_not_adopted_and_the_reload_defers_to_it() {
    let world = world().await;
    let state = world.state().await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let hanging = HangingProvider {
        entered: Mutex::new(Some(entered_tx)),
    };

    {
        let first = coach_turn_core(
            &state,
            deps(hanging),
            turn_request("sess-live-elsewhere", &world.parent_run_id),
        );
        tokio::pin!(first);
        tokio::select! {
            _ = &mut first => panic!("the hanging provider must not complete the turn"),
            entered = entered_rx => entered.expect("the provider was entered"),
        }
        // Dropped: this process's registry entry and latch are released, so only
        // the row's AGE stands between the reload and stealing the claim. It is
        // NOT back-dated here — that is the whole point.
    }

    let (provider2, calls2) = ScriptedProvider::new(vec![propose_call(
        RSI_PERIOD,
        &json!({ "type": "Period", "value": 21 }),
        "a live turn elsewhere must not be stolen",
    )]);
    let error = coach_turn_core(
        &state,
        deps(provider2),
        turn_request("sess-reload", &world.parent_run_id),
    )
    .await
    .expect_err("a young pending claim is deferred to, never adopted");

    assert_eq!(error.code, pulse::BusErrorCode::Busy);
    assert!(
        error.message.contains("sess-live-elsewhere"),
        "the refusal names the claim it is deferring to: {}",
        error.message
    );
    assert_eq!(
        calls2.load(Ordering::SeqCst),
        0,
        "deferring to a live claim spends no money"
    );

    // The claim is untouched — still pending, not settled as interrupted.
    let sessions = world.sessions().await;
    let recorded = sessions
        .get_session(&CoachingSessionId::new("sess-live-elsewhere"))
        .await
        .expect("read the claim")
        .expect("it is still there");
    assert!(
        matches!(recorded.outcome, SessionOutcome::Pending),
        "a live claim is left alone, not finalized out from under its owner"
    );
}

/// The post-RESTART shape: a fresh session id still settles the abandoned claim.
///
/// The test above reuses the stale id explicitly, which is what a running app can
/// do because it still holds it. After a restart it does not: the operation store
/// that held it is gone and the rail mints a new id. Nothing then reached the
/// pending row, so W1's `interrupted` recovery — written, tested at the unit level,
/// and named in the DTO — could never fire in production.
///
/// A reload adopts this run's unfinished claim, so the recovery is reachable by the
/// road a trader actually travels.
#[tokio::test]
async fn a_reload_under_a_fresh_session_id_still_settles_this_runs_abandoned_claim() {
    let world = world().await;
    let state = world.state().await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let hanging = HangingProvider {
        entered: Mutex::new(Some(entered_tx)),
    };

    {
        let first = coach_turn_core(
            &state,
            deps(hanging),
            turn_request("sess-before-restart", &world.parent_run_id),
        );
        tokio::pin!(first);
        tokio::select! {
            _ = &mut first => panic!("the hanging provider must not complete the turn"),
            entered = entered_rx => entered.expect("the provider was entered"),
        }
        // Dropped: the claim is committed and unfinished, as a killed process
        // leaves it.
    }

    age_pending_claim(world.db().await.pool(), "sess-before-restart", 10).await;

    // The rail after a restart: a NEW id, because nothing remembers the old one.
    let (provider2, calls2) = ScriptedProvider::new(vec![propose_call(
        RSI_PERIOD,
        &json!({ "type": "Period", "value": 21 }),
        "no call may be made on an abandoned claimant's behalf",
    )]);
    let dto = coach_turn_core(
        &state,
        deps(provider2),
        turn_request("sess-after-restart", &world.parent_run_id),
    )
    .await
    .expect("the reload settles the stale claim");

    assert_eq!(dto.outcome, "failed");
    assert_eq!(
        dto.session_id, "sess-before-restart",
        "the reload adopted this run's unfinished claim rather than opening a second one"
    );
    let failure = dto
        .failure
        .as_ref()
        .expect("a failed turn states its reason");
    assert_eq!(failure.kind, "interrupted");
    assert_eq!(failure.recovery, "start a new coaching session");
    assert_eq!(
        calls2.load(Ordering::SeqCst),
        0,
        "settling an abandoned claim spends no money"
    );

    // And exactly ONE session exists for the run: the reload settled the claim
    // rather than leaving it pending beside a second one.
    let sessions = world.sessions().await;
    let recorded = sessions
        .list_sessions_for_run(&world.parent_run_id)
        .await
        .expect("read the run's turns");
    assert_eq!(recorded.len(), 1, "no second session was opened");
}

// ---------------------------------------------------------------------------
// 5. a pre-0006 run is MissingBacktestInputs, with its recovery and no call
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_run_without_input_provenance_is_recorded_as_missing_inputs_with_no_call() {
    let world = world().await;
    let state = world.state().await;

    // Strip the parent run's recorded provenance — the all-NULL shape a row
    // written before migration `0006` has, which `decode_inputs` reads as legacy.
    let db = world.db().await;
    let blank = format!(
        "UPDATE backtest_run SET pair = NULL, primary_timeframe = NULL, \
         primary_data_version = NULL, htf_timeframe = NULL, htf_data_version = NULL, \
         taker_fee_bps = NULL, slippage_bps = NULL, funding_config = NULL \
         WHERE id = '{}'",
        world.parent_run_id.as_str()
    );
    coach_support::with_run_immutability_lifted(db.pool(), &[blank.as_str()]).await;

    let (provider, calls) = ScriptedProvider::new(vec![propose_call(
        RSI_PERIOD,
        &json!({ "type": "Period", "value": 21 }),
        "no call may be made for a run with no inputs",
    )]);
    let dto = coach_turn_core(
        &state,
        deps(provider),
        turn_request("sess-legacy", &world.parent_run_id),
    )
    .await
    .expect("the legacy run records a failure rather than refusing");

    assert_eq!(dto.outcome, "failed");
    let failure = dto
        .failure
        .as_ref()
        .expect("a failed turn states its reason");
    assert_eq!(failure.kind, "missing_backtest_inputs");
    assert_eq!(
        failure.recovery,
        "run this version again, then ask the coach"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "a legacy run is free");
    assert_eq!(world.count("SELECT COUNT(*) FROM llm_call").await, 0);
}

// ---------------------------------------------------------------------------
// 6. record_inapplicable is InapplicableAdvice, and proposes nothing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn structural_advice_is_recorded_as_inapplicable_advice_with_no_proposal() {
    let world = world().await;
    let state = world.state().await;
    let (provider, calls) = ScriptedProvider::new(vec![inapplicable_call(
        "add a volume filter to the entry",
        "most losses opened on thin bars",
    )]);

    let dto = coach_turn_core(
        &state,
        deps(provider),
        turn_request("sess-inapplicable", &world.parent_run_id),
    )
    .await
    .expect("structural advice is recorded");

    assert_eq!(dto.outcome, "failed");
    assert!(dto.proposal.is_none(), "no proposal, and no approximation");
    let failure = dto
        .failure
        .as_ref()
        .expect("a failed turn states its reason");
    assert_eq!(failure.kind, "inapplicable_advice");
    assert_eq!(
        failure.recovery,
        "the coach's advice was structural; ask again on another run or edit the strategy"
    );
    assert!(
        failure.detail.contains("volume filter"),
        "the recorded detail carries the coach's own words: {}",
        failure.detail
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// 7. the decision rail: modify, reject, accept
// ---------------------------------------------------------------------------

#[tokio::test]
async fn modify_stores_the_traders_own_value_and_returns_the_durable_state() {
    let world = world().await;
    let state = world.state().await;
    let session = world.seed_proposed_session("sess-modify", 21).await;

    let dto = coach_decide_core(
        &state,
        decision_request(
            &session,
            CoachActionDto::Modify {
                path: RSI_PERIOD.to_owned(),
                new_value: "9".to_owned(),
            },
        ),
    )
    .await
    .expect("the modify is applied");

    let proposal = dto.session.proposal.as_ref().expect("still one proposal");
    assert_eq!(proposal.disposition, "modified");
    assert_eq!(
        proposal.mutation.new_value, "9",
        "the stored modification replaces the shown value"
    );
    assert!(dto.accepted.is_none(), "a modify mints nothing");

    // The durable state, re-read through the repository rather than trusted.
    let stored = world
        .sessions()
        .await
        .get_session(&session)
        .await
        .unwrap()
        .unwrap();
    match stored.outcome {
        SessionOutcome::Proposed { proposal } => match proposal.mutation {
            Mutation::SetParam { new_value, .. } => {
                assert_eq!(new_value, ParamValue::Period { value: 9 });
            }
        },
        other => panic!("expected a proposal, got {other:?}"),
    }
}

/// A modify edits THIS proposal's value; it does not re-target the mutation.
///
/// The new value is parsed against the kind of the leaf the PROPOSAL names, so a
/// caller-supplied different path would have its value parsed against the wrong
/// leaf's type and stored under a path nothing validated it for.
#[tokio::test]
async fn modify_refuses_a_path_that_is_not_the_proposals_own() {
    let world = world().await;
    let state = world.state().await;
    let session = world.seed_proposed_session("sess-modify-path", 21).await;

    let error = coach_decide_core(
        &state,
        decision_request(
            &session,
            CoachActionDto::Modify {
                path: "exits.0.distance_pct".to_owned(),
                new_value: "9".to_owned(),
            },
        ),
    )
    .await
    .expect_err("a modify may not re-target the mutation");
    assert!(
        error.message.contains(RSI_PERIOD),
        "the refusal names the leaf this proposal actually changes: {}",
        error.message
    );

    // Nothing was written: the proposal still carries its original value.
    let stored = world
        .sessions()
        .await
        .get_session(&session)
        .await
        .unwrap()
        .unwrap();
    match stored.outcome {
        SessionOutcome::Proposed { proposal } => match proposal.mutation {
            Mutation::SetParam { path, new_value } => {
                assert_eq!(path, RSI_PERIOD);
                assert_eq!(new_value, ParamValue::Period { value: 21 });
            }
        },
        other => panic!("expected an untouched proposal, got {other:?}"),
    }
}

#[tokio::test]
async fn reject_is_terminal_and_mints_neither_a_child_nor_a_run() {
    let world = world().await;
    let state = world.state().await;
    let session = world.seed_proposed_session("sess-reject", 21).await;
    let versions_before = world.count("SELECT COUNT(*) FROM strategy_version").await;
    let runs_before = world.count("SELECT COUNT(*) FROM backtest_run").await;

    let dto = coach_decide_core(&state, decision_request(&session, CoachActionDto::Reject))
        .await
        .expect("the reject is recorded");

    let proposal = dto
        .session
        .proposal
        .as_ref()
        .expect("the proposal survives");
    assert_eq!(proposal.disposition, "rejected");
    assert!(dto.accepted.is_none());
    assert_eq!(
        world.count("SELECT COUNT(*) FROM strategy_version").await,
        versions_before
    );
    assert_eq!(
        world.count("SELECT COUNT(*) FROM backtest_run").await,
        runs_before
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accept_mints_exactly_one_child_and_one_run_and_reports_both_summaries() {
    let world = world().await;
    let state = world.state().await;
    let session = world.seed_proposed_session("sess-accept", 21).await;
    let versions_before = world.count("SELECT COUNT(*) FROM strategy_version").await;
    let runs_before = world.count("SELECT COUNT(*) FROM backtest_run").await;

    let dto = coach_decide_core(&state, decision_request(&session, CoachActionDto::Accept))
        .await
        .expect("the accept commits");

    let accepted = dto.accepted.as_ref().expect("an accept reports its child");
    assert_eq!(
        world.count("SELECT COUNT(*) FROM strategy_version").await,
        versions_before + 1,
        "exactly one child version"
    );
    assert_eq!(
        world.count("SELECT COUNT(*) FROM backtest_run").await,
        runs_before + 1,
        "exactly one re-backtest run"
    );

    let proposal = dto
        .session
        .proposal
        .as_ref()
        .expect("the proposal survives");
    assert_eq!(proposal.disposition, "accepted");
    assert_eq!(
        proposal.child_version_id.as_deref(),
        Some(accepted.child_version_id.as_str())
    );
    assert_eq!(
        proposal.accepted_run_id.as_deref(),
        Some(accepted.accepted_run_id.as_str())
    );

    // before/after are the two PERSISTED summaries, read back from the rows.
    let after = accepted.after.as_ref().expect("the child run read back");
    let db = world.db().await;
    let parent_expectancy: String =
        sqlx::query_scalar("SELECT expectancy FROM backtest_run WHERE id = ?1")
            .bind(world.parent_run_id.as_str())
            .fetch_one(db.pool())
            .await
            .unwrap();
    let child_expectancy: String =
        sqlx::query_scalar("SELECT expectancy FROM backtest_run WHERE id = ?1")
            .bind(accepted.accepted_run_id.as_str())
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(
        accepted.before.expectancy,
        Decimal::from_str_exact(&parent_expectancy)
            .unwrap()
            .normalize()
            .to_string(),
        "`before` is the parent's persisted expectancy"
    );
    assert_eq!(
        after.expectancy,
        Decimal::from_str_exact(&child_expectancy)
            .unwrap()
            .normalize()
            .to_string(),
        "`after` is the child's persisted expectancy"
    );
    assert!(
        matches!(accepted.read_back, pulse::ReadBackDto::Ok(_)),
        "a readable child run reports ok"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepting_twice_returns_the_same_two_ids_and_mints_nothing_further() {
    let world = world().await;
    let state = world.state().await;
    let session = world.seed_proposed_session("sess-twice", 21).await;

    let first = coach_decide_core(&state, decision_request(&session, CoachActionDto::Accept))
        .await
        .expect("the accept commits");
    let versions_after_first = world.count("SELECT COUNT(*) FROM strategy_version").await;
    let runs_after_first = world.count("SELECT COUNT(*) FROM backtest_run").await;

    let second = coach_decide_core(&state, decision_request(&session, CoachActionDto::Accept))
        .await
        .expect("the replay answers from the record");

    let a = first.accepted.as_ref().unwrap();
    let b = second.accepted.as_ref().unwrap();
    assert_eq!(a.child_version_id, b.child_version_id);
    assert_eq!(a.accepted_run_id, b.accepted_run_id);
    assert_eq!(
        world.count("SELECT COUNT(*) FROM strategy_version").await,
        versions_after_first
    );
    assert_eq!(
        world.count("SELECT COUNT(*) FROM backtest_run").await,
        runs_after_first
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saved_but_unreadable_accept_carries_both_ids_and_no_after() {
    let world = world().await;
    let state = world.state().await;
    let session = world.seed_proposed_session("sess-unreadable", 21).await;

    // Every run row inserted from here on reads back with an unsupported schema
    // tag — the r1.s3 saved-but-unreadable injection, at the STORAGE layer, so the
    // real repository refuses a real read of a row that really committed. The
    // immutability guard stays lifted for the accept because the breaker rewrites
    // the row it just inserted; everything the accept itself does is unchanged.
    let db = world.db().await;
    let mut conn = db.pool().acquire().await.expect("one pooled connection");
    sqlx::query("DROP TRIGGER backtest_run_no_update")
        .execute(&mut *conn)
        .await
        .expect("lift the immutability guard");
    sqlx::query(
        "CREATE TRIGGER break_child_read_back AFTER INSERT ON backtest_run \
         BEGIN UPDATE backtest_run SET schema_version = 99 WHERE id = NEW.id; END",
    )
    .execute(&mut *conn)
    .await
    .expect("install the read-back breaker");

    let dto = coach_decide_core(&state, decision_request(&session, CoachActionDto::Accept))
        .await
        .expect("a read-back failure is not an accept failure");

    let accepted = dto
        .accepted
        .as_ref()
        .expect("the accept SUCCEEDED — both ids are real");
    assert!(!accepted.child_version_id.is_empty());
    assert!(!accepted.accepted_run_id.is_empty());
    assert!(
        accepted.after.is_none(),
        "the child run could not be read back, so there is no `after` to show"
    );
    match &accepted.read_back {
        pulse::ReadBackDto::Failed { failure } => {
            assert!(!failure.is_empty(), "the read-back failure states itself");
        }
        pulse::ReadBackDto::Ok(_) => panic!("the read back was broken on purpose"),
    }
}

// ---------------------------------------------------------------------------
// 8. #141 — the single-flight latch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_overlapping_backtest_for_one_version_is_refused_and_starts_no_second_run() {
    let world = world().await;
    let state = world.state().await;
    let runs_before = world.count("SELECT COUNT(*) FROM backtest_run").await;

    // Hold the key exactly as an in-flight run holds it.
    let held = state
        .begin_operation(OperationKey::Backtest(world.version_id.clone()))
        .expect("the key is free");

    let refused = run_backtest_version_core(
        &state,
        BacktestRunRequest {
            version_id: world.version_id.as_str().to_owned(),
        },
    )
    .await
    .expect_err("an overlapping run for the same version is refused");
    assert_eq!(refused.code, pulse::BusErrorCode::Busy);
    assert!(
        refused.message.contains(world.version_id.as_str()),
        "the refusal names the key it is about: {}",
        refused.message
    );
    assert_eq!(
        world.count("SELECT COUNT(*) FROM backtest_run").await,
        runs_before,
        "a refused invocation never starts a second engine run"
    );

    // Released, the very same request succeeds — the latch refuses overlap, not the
    // operation.
    drop(held);
    run_backtest_version_core(
        &state,
        BacktestRunRequest {
            version_id: world.version_id.as_str().to_owned(),
        },
    )
    .await
    .expect("the key was released");
    assert_eq!(
        world.count("SELECT COUNT(*) FROM backtest_run").await,
        runs_before + 1
    );
}

#[tokio::test]
async fn the_latch_is_released_after_success_after_a_bus_error_and_after_a_panic() {
    let world = world().await;
    let state = world.state().await;
    let key = OperationKey::Backtest(world.version_id.clone());

    // Success.
    run_backtest_version_core(
        &state,
        BacktestRunRequest {
            version_id: world.version_id.as_str().to_owned(),
        },
    )
    .await
    .expect("a real run");
    assert!(
        !state.operation_in_flight(&key),
        "the latch releases on the success path"
    );

    // A BusError: no such version.
    let missing = OperationKey::Backtest(VersionId::new("no-such-version"));
    run_backtest_version_core(
        &state,
        BacktestRunRequest {
            version_id: "no-such-version".to_owned(),
        },
    )
    .await
    .expect_err("an absent version fails");
    assert!(
        !state.operation_in_flight(&missing),
        "the latch releases on the error path"
    );

    // A panic on the path that holds the latch — RAII, not a manual release.
    let panic_key = OperationKey::Coach(CoachingSessionId::new("sess-panic"));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = state.begin_operation(panic_key.clone()).expect("free");
        panic!("a fault inside the core");
    }));
    assert!(result.is_err(), "the panic was raised");
    assert!(
        !state.operation_in_flight(&panic_key),
        "the latch releases on an unwinding panic"
    );
}

// ---------------------------------------------------------------------------
// 9. the wire contract: every decimal a string, and no credential anywhere
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_decimal_the_rail_shows_crosses_as_an_exact_string() {
    let world = world().await;
    let state = world.state().await;
    let session = world.seed_proposed_session("sess-strings", 21).await;

    let dto = coach_decide_core(&state, decision_request(&session, CoachActionDto::Accept))
        .await
        .expect("the accept commits");
    let value = serde_json::to_value(&dto).expect("the decision DTO serializes");

    let accepted = &value["accepted"];
    for half in ["before", "after"] {
        let summary = &accepted[half];
        for field in [
            "netPnl",
            "expectancy",
            "winRate",
            "grossProfit",
            "grossLoss",
            "avgWin",
            "avgLoss",
            "maxDrawdown",
            "commissionTotal",
            "fundingTotal",
        ] {
            assert!(
                summary[field].is_string(),
                "{half}.{field} must cross as an exact string, got {}",
                summary[field]
            );
        }
    }

    // The session half too: the cost is a string in its own currency.
    let turn = serde_json::to_value(&dto.session).expect("the session DTO serializes");
    if !turn["cost"].is_null() {
        assert!(turn["cost"]["amount"].is_string());
        assert!(turn["cost"]["currency"].is_string());
    }
    assert!(
        value["accepted"]["before"]["tradeCount"].is_number(),
        "counts stay numbers; only money and ratios are strings"
    );
}

#[tokio::test]
async fn the_credential_never_reaches_a_dto_an_error_or_a_persisted_row() {
    let world = world().await;
    let state = world.state().await;
    // The provider echoes the key back in every field it controls.
    let (provider, _calls) = ScriptedProvider::new(vec![propose_call(
        RSI_PERIOD,
        &json!({ "type": "Period", "value": 21 }),
        &format!("the key {FAKE_KEY} must not survive this turn"),
    )]);

    let dto = coach_turn_core(
        &state,
        deps(provider),
        turn_request("sess-canary", &world.parent_run_id),
    )
    .await
    .expect("the turn records a session");

    let serialized = serde_json::to_string(&dto).expect("the DTO serializes");
    assert!(
        !serialized.contains(FAKE_KEY),
        "the credential reached the wire DTO: {serialized}"
    );

    // And nowhere in the persisted ledger row either.
    let call_id = dto.llm_call_id.clone().expect("the turn names its row");
    let row = state
        .llm_call_repo()
        .get_call(&LlmCallId::new(call_id))
        .await
        .unwrap()
        .unwrap();
    let stored = serde_json::to_string(&row).expect("the ledger row serializes");
    assert!(
        !stored.contains(FAKE_KEY),
        "the credential reached the persisted ledger row"
    );

    // A refusal's message is text a screen renders — it may not carry one either.
    let refused = coach_decide_core(
        &state,
        decision_request(
            &CoachingSessionId::new("no-such-session"),
            CoachActionDto::Accept,
        ),
    )
    .await
    .expect_err("no such session");
    assert!(!refused.message.contains(FAKE_KEY));
}

// ---------------------------------------------------------------------------
// The live coach wiring, both surfaces (#164, PR #165 review R6/R7)
// ---------------------------------------------------------------------------

mod source_scan;

/// The two coach composition sites, as `(file, the fn that opens the site, the
/// call that closes it)`.
///
/// Both wrappers resolve a live credential and construct a live transport, so
/// neither is reachable from an offline test — and they are exactly where the two
/// surfaces drifted: until #164 the desktop built `compose_config` (the composer's
/// 4 096-token cap and 0.2 temperature) while `pulse coach` sent 0.0 and the CLI
/// reasoning constant, and until PR #165's review round each built its provider
/// inline, where swapping `single_attempt` for `new` would have passed every gate.
const COACH_SITES: [(&str, &str, &str); 2] = [
    (
        "src/cli/coach.rs",
        "pub async fn run_coach(",
        "run_coach_with(",
    ),
    (
        "src/tauri/commands.rs",
        "pub async fn coach_turn(",
        "coach_turn_core(",
    ),
];

/// Read the wiring block of one coach site: the wrapper's body up to the call that
/// hands off to its transport-free core.
fn coach_site_wiring(relative: &str, opens: &str, closes: &str) -> String {
    let code = source_scan::blank_comments(&source_scan::read_source(relative));
    let start = code
        .find(opens)
        .unwrap_or_else(|| panic!("{relative} no longer declares `{opens}`"));
    let body = &code[start..];
    let end = body
        .find(closes)
        .unwrap_or_else(|| panic!("{relative}'s `{opens}` no longer reaches `{closes}`"));
    body[..end].to_owned()
}

/// BOTH coach sites build the SHARED coach transport and the SHARED coach config.
///
/// A source scan because neither site can be reached offline, and because the
/// property is about which constructor is named, not about what a value ends up
/// being: an inline `OpenAiCompatProvider::new(...)` here would still compile, still
/// pass every other test, and quietly restore the retrying, 60-second, 4 096-token
/// posture that #164 is about.
#[test]
fn both_coach_sites_build_the_shared_transport_and_config() {
    for (relative, opens, closes) in COACH_SITES {
        let wiring = coach_site_wiring(relative, opens, closes);

        assert!(
            wiring.contains("coach_provider("),
            "{relative}'s `{opens}` must build its transport through the shared \
             `coach_transport::coach_provider` so both coach surfaces send the same \
             retry, timeout and endpoint posture"
        );
        assert!(
            wiring.contains("coach_config("),
            "{relative}'s `{opens}` must build the shared `coach_config` so both \
             coach surfaces send the same cap and temperature"
        );
        for banned in [
            "compose_config(",
            "OpenAiCompatProvider::new(",
            "single_attempt(",
            "single_attempt_with_base_url(",
        ] {
            assert!(
                !wiring.contains(banned),
                "{relative}'s `{opens}` names `{banned}` — a coach turn's transport \
                 and config are chosen once, in `adapters::llm::coach_transport`, \
                 not re-derived at each surface (#164, review R6)"
            );
        }
    }
}
