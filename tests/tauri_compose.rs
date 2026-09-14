//! AC-1 (r1.s1.w4) — the desktop compose stream's offline end-to-end, ledger `d3`.
//!
//! Drives [`compose_strategy_core`] — the transport-free core behind the
//! `compose_strategy` bus command — with a **fake** provider (no network, no
//! Keychain, MASTER-SPEC §9.4) over the REAL composer + REAL builder tools + a
//! `tempfile` migrated `pulse.db`, mirroring `tests/compose_cli.rs`'s
//! established pattern. What this file adds over the CLI e2e is the BUS half:
//!
//! - the composer's tool-call steps arrive as **structured `BusEvent`s in
//!   order** — `Started` first, `ToolCallStarted`/`ToolCallResult` per step,
//!   `Finished` (carrying the finalize summary) last — one by one, which is
//!   `d1`'s observable and `d3`'s first claim;
//! - the finalized version persists **attributable** (`created_by =
//!   ComposerLlm`, non-empty `creating_llm_call_ids`) — `d3`'s second claim;
//! - the risk gate's three controls land at the IPC seam: every persisted
//!   `LlmCall` row carries the credential-source **label** (audit trail), is
//!   redacted at rest (no-secret-in-log), and the key itself never crosses the
//!   core's arguments, return value, or any event (least privilege — the
//!   `FakeComposerProvider` wiring carries only the `key_source` LABEL);
//! - a sink that dies mid-run is **cancellation, not an error** (bus contract
//!   clause 2): the core stops the compose via the provider guard and returns
//!   `cancelled: true`, never a `BusError`.
//!
//! The LIVE arm (real credential, real transport) is `d1`'s human walk, not
//! here. Offline (in-process `MIGRATOR` + committed `.sqlx/`), `TempDir`-isolated.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pulse::{
    BusError, BusEvent, BusEventPayload, ComposeDeps, ComposeWiring, CreatedBy, CredentialSource,
    Db, EventSink, FakeClock, LlmBackend, LlmCallId, LlmCallRepository, LlmConfig, LlmProvider,
    LlmResponse, MIGRATOR, Message, ModelPrice, PriceTable, Redactor, RunId, SqliteLlmCallRepo,
    SqliteStrategyRepo, StrategyRepository, SystemClock, TokenUsage, ToolCall, ToolDefinition,
    VersionId, compose_strategy_core,
};
use rust_decimal::Decimal;
use serde_json::json;
use tempfile::TempDir;

/// An API-key-shaped literal the redactor must strip from every persisted
/// `LlmCall` prompt. NOT a real key.
const FAKE_KEY: &str = "sk-COMPOSE1234abcd5678efgh9012ijkl3456";

/// A fresh, untripped cancellation latch — what the `compose_strategy` command
/// receives from `DesktopState::register_compose_run` for a run nobody cancelled.
fn new_latch() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// A stand-in composer system prompt (the fake provider ignores it).
const TEST_PROMPT: &str = "You are PulseTrader's strategy composer. Build the \
    strategy only by calling builder tools; never emit raw DSL JSON.";

/// A scripted [`LlmProvider`] double: returns queued responses turn-by-turn
/// (`tests/compose_cli.rs`'s pattern — the provider is the ONLY faked layer).
struct FakeComposerProvider {
    scripts: Mutex<VecDeque<LlmResponse>>,
}

impl FakeComposerProvider {
    fn new(responses: Vec<LlmResponse>) -> Self {
        Self {
            scripts: Mutex::new(responses.into()),
        }
    }
}

impl LlmProvider for FakeComposerProvider {
    fn chat(
        &self,
        _messages: Vec<Message>,
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
    ) -> impl Future<Output = Result<LlmResponse, pulse::LlmError>> {
        let next = self.scripts.lock().expect("scripts lock").pop_front();
        std::future::ready(Ok(next.unwrap_or_else(|| LlmResponse {
            content: Some("(script exhausted)".to_owned()),
            tool_calls: Vec::new(),
            usage: usage(),
        })))
    }
}

/// A known per-turn token usage (so each persisted `LlmCall` cost is non-zero).
fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 120,
        output_tokens: 48,
    }
}

/// A scripted single-tool-call turn.
fn tool_turn(id: &str, name: &str, arguments: serde_json::Value) -> LlmResponse {
    LlmResponse {
        content: None,
        tool_calls: vec![ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments,
        }],
        usage: usage(),
    }
}

/// A TEST price table keyed on the demo model so the decorator prices the run.
fn test_prices() -> PriceTable {
    let mut models = HashMap::new();
    models.insert(
        "gpt-oss:120b".to_owned(),
        ModelPrice {
            input_per_mtok: Decimal::from(2),
            output_per_mtok: Decimal::from(8),
        },
    );
    PriceTable::from_config("USD", models)
}

/// The per-request chat config (Ollama backend, the priced demo model).
fn config() -> LlmConfig {
    LlmConfig {
        backend: LlmBackend::Ollama,
        model: "gpt-oss:120b".to_owned(),
        temperature: 0.2,
        max_tokens: 1024,
        reasoning_effort: None,
    }
}

/// A fresh `TempDir` + a migrated `pulse.db` [`Db`] over it (offline, in-process
/// `MIGRATOR`; the `TempDir` guard keeps the scratch db alive for the test body).
async fn migrated_db() -> (TempDir, Db) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    (tmp, db)
}

/// The demo happy-path script: create → RSI(14)<30 entry → Close>EMA(200)
/// filter → [5% stop, 2R TP] exits → [1%, 3x] risk → finalize (built only via
/// tools) — `tests/compose_cli.rs`'s script, so the two e2es compose the same
/// strategy through two different surfaces.
fn happy_path_script() -> Vec<LlmResponse> {
    vec![
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "RSI Oversold", "direction": "long" }),
        ),
        tool_turn(
            "c2",
            "add_entry_signal",
            json!({
                "left": { "source": "indicator", "indicator": "rsi", "period": 14 },
                "op": "lt",
                "right": { "source": "constant", "value": "30" }
            }),
        ),
        tool_turn(
            "c3",
            "add_filter",
            json!({
                "left": { "source": "price", "price_field": "close" },
                "op": "gt",
                "right": { "source": "indicator", "indicator": "ema", "period": 200 }
            }),
        ),
        tool_turn(
            "c4",
            "set_exit_rules",
            json!({ "stop_loss_pct": "0.05", "take_profit_r": "2" }),
        ),
        tool_turn(
            "c5",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ),
        tool_turn("c6", "finalize_strategy", json!({})),
    ]
}

/// A collecting [`EventSink`] — the in-file double `demo_stream_core`'s own
/// tests use (`src/tauri/commands.rs`), standing in for the webview's channel.
/// `Mutex` (not `RefCell`) so the core's event closure stays `Send` — the same
/// property the real `Channel` gives the command wrapper.
struct Collector {
    events: Mutex<Vec<BusEvent>>,
}

impl EventSink for Collector {
    fn send_event(&self, event: BusEvent) -> Result<(), BusError> {
        self.events.lock().expect("events lock").push(event);
        Ok(())
    }
}

/// A sink that accepts the first `ok_for` sends and refuses everything after —
/// the shape of a screen that unmounted while its run was still streaming.
struct FlakySink {
    ok_for: usize,
    sent: AtomicUsize,
}

impl EventSink for FlakySink {
    fn send_event(&self, _event: BusEvent) -> Result<(), BusError> {
        let n = self.sent.fetch_add(1, Ordering::SeqCst);
        if n < self.ok_for {
            Ok(())
        } else {
            Err(BusError::internal("channel closed".to_owned()))
        }
    }
}

/// A provider that always fails with a typed [`pulse::LlmError`] — the transport
/// half of the compose failure taxonomy.
struct FailingProvider;

impl LlmProvider for FailingProvider {
    fn chat(
        &self,
        _messages: Vec<Message>,
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
    ) -> impl Future<Output = Result<LlmResponse, pulse::LlmError>> {
        std::future::ready(Err(pulse::LlmError::Provider(
            "upstream returned 503".to_owned(),
        )))
    }
}

/// A provider that trips the run's latch as it answers turn `trip_on_turn` —
/// standing in for `compose_cancel` arriving while that response is in flight.
struct LatchTrippingProvider {
    inner: FakeComposerProvider,
    latch: Arc<AtomicBool>,
    trip_on_turn: usize,
    turns: AtomicUsize,
}

impl LlmProvider for LatchTrippingProvider {
    fn chat(
        &self,
        messages: Vec<Message>,
        tools: &[ToolDefinition],
        config: &LlmConfig,
    ) -> impl Future<Output = Result<LlmResponse, pulse::LlmError>> {
        let n = self.turns.fetch_add(1, Ordering::SeqCst) + 1;
        if n >= self.trip_on_turn {
            self.latch.store(true, Ordering::SeqCst);
        }
        self.inner.chat(messages, tools, config)
    }
}

/// A **healthy** sink that trips the run's cancellation latch after `trip_after`
/// events — standing in for the `compose_cancel` command arriving mid-run.
///
/// Every send succeeds, which is the point: this is the case a failed send can
/// never catch, because a JavaScript `Channel`'s callback outlives the screen
/// that made it.
struct TrippingSink {
    latch: Arc<AtomicBool>,
    trip_after: usize,
    sent: AtomicUsize,
    events: Mutex<Vec<BusEvent>>,
}

impl EventSink for TrippingSink {
    fn send_event(&self, event: BusEvent) -> Result<(), BusError> {
        let n = self.sent.fetch_add(1, Ordering::SeqCst);
        self.events.lock().expect("events lock").push(event);
        if n + 1 >= self.trip_after {
            self.latch.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// A sink that refuses every send — a screen that unmounted before the run
/// opened its stream.
struct DeadSink;

impl EventSink for DeadSink {
    fn send_event(&self, _event: BusEvent) -> Result<(), BusError> {
        Err(BusError::internal("channel closed".to_owned()))
    }
}

/// Assert every persisted `LlmCall` row this run wrote is safe at rest AND
/// carries the audit label: redacted (NFR-6 / no-secret-in-log), attributed to
/// the composer, and stamped with the credential-source LABEL that answered
/// (r1.s1.w2's audit-trail control — the risk gate's IPC half lands here).
async fn assert_ledger_redacted_labelled_and_attributed(
    ledger: &SqliteLlmCallRepo<FakeClock>,
    ids: &[String],
) {
    for id in ids {
        let call = ledger
            .get_call(&LlmCallId::new(id.clone()))
            .await
            .expect("get_call")
            .unwrap_or_else(|| panic!("ledger row {id} is present"));
        let wire = serde_json::to_string(&call.prompt_messages).expect("serialize prompt");
        assert!(
            !wire.contains(FAKE_KEY),
            "persisted LlmCall {id} leaks the secret: {wire}"
        );
        assert!(
            wire.contains("REDACTED"),
            "persisted LlmCall {id} prompt not redacted: {wire}"
        );
        assert_eq!(
            call.created_by,
            CreatedBy::ComposerLlm,
            "LlmCall {id} must be attributed to the composer"
        );
        assert_eq!(
            call.key_source,
            Some(CredentialSource::ConfigDir),
            "LlmCall {id} must carry the credential-source label (audit trail)"
        );
    }
}

/// Assert the streamed run's SHAPE: `Started` first, `Finished` (carrying the
/// finalize summary) last, structured step events between — seq monotonic from
/// 0, the run id on every event, no secret riding any payload, and the
/// composer's own emission contract mirrored on the channel (builder turns as
/// started/result pairs; a SUCCESSFUL finalize turn streams its
/// `ToolCallStarted` then goes straight to `Finalized`).
fn assert_stream_shape(events: &[BusEvent], run_id: &RunId) {
    assert!(matches!(events[0].payload, BusEventPayload::Started));
    let last = events.last().expect("the stream has a last event");
    let finalize_message = match &last.payload {
        BusEventPayload::Finished { message } => message.clone(),
        other => panic!("the last event must be Finished, got {other:?}"),
    };
    assert!(
        !finalize_message.is_empty(),
        "Finished must carry the composer's finalize summary"
    );
    for (i, event) in events.iter().enumerate() {
        assert_eq!(
            event.seq,
            u32::try_from(i).expect("index fits in u32"),
            "seq must be monotonic from 0"
        );
        assert_eq!(event.run_id, *run_id, "every event carries its run id");
    }

    // The five builder turns stream (ToolCallStarted, ToolCallResult) pairs;
    // the finalize turn's ToolCallStarted is the 11th step, closed by the
    // Finished bookend rather than a result.
    let steps = &events[1..events.len() - 1];
    assert_eq!(
        steps.len(),
        11,
        "5 builder turns x 2 events + finalize's ToolCallStarted"
    );
    for pair in steps[..10].chunks(2) {
        assert!(
            matches!(&pair[0].payload, BusEventPayload::ToolCallStarted { .. }),
            "a builder step opens with ToolCallStarted"
        );
        assert!(
            matches!(&pair[1].payload, BusEventPayload::ToolCallResult { .. }),
            "a builder step closes with ToolCallResult"
        );
    }
    assert!(
        matches!(
            &steps[10].payload,
            BusEventPayload::ToolCallStarted { name, .. } if name == "finalize_strategy"
        ),
        "the finalize turn's step opens the way every step does"
    );
    // The structured payloads carry the composer's own fields: the tool name and
    // its arguments preview, then the outcome.
    assert!(steps.iter().any(|e| matches!(
        &e.payload,
        BusEventPayload::ToolCallStarted { name, arguments_preview }
            if name == "add_filter" && arguments_preview.contains("ema")
    )));
    // No secret ever rode an event (least privilege: the key crosses no boundary).
    for event in events {
        let wire = serde_json::to_string(&event.payload).expect("serialize payload");
        assert!(
            !wire.contains(FAKE_KEY),
            "an event leaked the secret: {wire}"
        );
    }
}

/// The wiring bundle every test below builds: fake provider over the real
/// composer + real builder tools + a migrated temp db, with the audit LABEL set
/// (a label, never a value — the key itself never exists in this file's wiring).
fn fake_deps(
    db: &Db,
    script: Vec<LlmResponse>,
) -> ComposeDeps<
    FakeComposerProvider,
    SqliteLlmCallRepo<FakeClock>,
    SqliteStrategyRepo<SystemClock>,
    FakeClock,
> {
    let clock = FakeClock::at(1_700_000_000_000);
    let llm_repo = SqliteLlmCallRepo::with_deps(db.pool().clone(), clock);
    ComposeDeps {
        wiring: ComposeWiring {
            provider: FakeComposerProvider::new(script),
            llm_repo,
            redactor: Redactor::from_config(vec![FAKE_KEY.to_owned()]),
            prices: test_prices(),
            clock,
            prompt: TEST_PROMPT.to_owned(),
            key_source: Some(CredentialSource::ConfigDir),
            config: config(),
        },
        strategy_repo: SqliteStrategyRepo::new(db.pool().clone()),
    }
}

/// The same wiring over an arbitrary provider — the failure-taxonomy tests need
/// a provider `fake_deps`'s concrete return type cannot express.
fn deps_with_provider<P: LlmProvider>(
    db: &Db,
    provider: P,
) -> ComposeDeps<P, SqliteLlmCallRepo<FakeClock>, SqliteStrategyRepo<SystemClock>, FakeClock> {
    let clock = FakeClock::at(1_700_000_000_000);
    ComposeDeps {
        wiring: ComposeWiring {
            provider,
            llm_repo: SqliteLlmCallRepo::with_deps(db.pool().clone(), clock),
            redactor: Redactor::from_config(vec![FAKE_KEY.to_owned()]),
            prices: test_prices(),
            clock,
            prompt: TEST_PROMPT.to_owned(),
            key_source: Some(CredentialSource::ConfigDir),
            config: config(),
        },
        strategy_repo: SqliteStrategyRepo::new(db.pool().clone()),
    }
}

/// The bus's typed error contract: a transport failure reaches the Designer as
/// `BusErrorCode::Llm`, not as the catch-all `Internal`.
///
/// This is a REGRESSION test with a specific history. `compose_failure` recovers
/// the family by downcasting the anyhow chain, but `run_compose_with` used to
/// build its errors with `anyhow!("compose run failed: {e}")` — a formatted
/// string is a NEW error with no source, so the typed cause was erased and every
/// LLM, composer and persistence failure alike surfaced as `Internal`. The
/// classifier existed and could never match. `.context(...)` keeps the chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transport_failure_keeps_its_llm_error_family_across_the_bus() {
    let (_tmp, db) = migrated_db().await;
    let deps = deps_with_provider(&db, FailingProvider);

    let sink = Collector {
        events: Mutex::new(Vec::new()),
    };
    let error = compose_strategy_core(
        &RunId::new(),
        deps,
        "RSI oversold on BTC",
        &sink,
        new_latch(),
    )
    .await
    .expect_err("a provider that always fails is a genuine failure, not cancellation");

    assert_eq!(
        error.code,
        pulse::BusErrorCode::Llm,
        "a transport failure must keep its family across the bus, not collapse to Internal"
    );
    // The whole chain reaches the user, not just the outermost context: an
    // anyhow error Displays only its top layer, so a `BusError` built from
    // `to_string()` alone would say "compose run failed" and nothing else.
    assert!(
        error.message.contains("compose run failed"),
        "the context layer names the stage: {}",
        error.message
    );
    assert!(
        error.message.contains("503"),
        "the underlying cause must survive for the Designer to render: {}",
        error.message
    );
}

/// AC-1's main claim (`d3`): a compose run driven through the desktop core
/// streams its tool-call steps as structured events **in order** and persists an
/// attributable `StrategyVersion` — with the risk gate's controls holding at
/// the ledger.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compose_streams_steps_and_persists_an_attributable_version() {
    let (_tmp, db) = migrated_db().await;
    let deps = fake_deps(&db, happy_path_script());

    // The NL target smuggles an API-key-shaped secret — it must NOT reach any
    // persisted LlmCall row (NFR-6), and it must not leak into any event.
    let nl_target = format!("RSI oversold bounce on BTC; my api key {FAKE_KEY} do not leak it");

    let sink = Collector {
        events: Mutex::new(Vec::new()),
    };
    let run_id = RunId::new();
    let outcome = compose_strategy_core(&run_id, deps, &nl_target, &sink, new_latch())
        .await
        .expect("the scripted tool sequence streams and persists a strategy version");

    // (a) the run completed, uncancelled, and reports what actually crossed.
    assert!(!outcome.cancelled);
    // Clone out of the mutex: the assertions below hold `await`s (fresh repo
    // reads), and a `MutexGuard` must not ride across one.
    let events = sink.events.lock().expect("events lock").clone();
    assert_eq!(
        outcome.emitted as usize,
        events.len(),
        "emitted must count exactly the events that crossed the sink"
    );
    assert_stream_shape(&events, &run_id);

    // (c) `d3`'s persistence claim, read back through a FRESH repo: the version
    //     is attributable — created_by = ComposerLlm, non-empty provenance ids.
    let summary = outcome
        .strategy
        .as_ref()
        .expect("a finalized run carries its strategy summary");
    assert_eq!(summary.strategy_name, "RSI Oversold");
    assert_eq!(summary.created_by, "composer_llm");
    assert_eq!(summary.llm_call_count, 6, "one LlmCall per model turn");

    let reader = SqliteStrategyRepo::new(db.pool().clone());
    let version = reader
        .get_version(&VersionId::new(summary.version_id.clone()))
        .await
        .expect("get_version")
        .expect("the summary names a version that persisted");
    assert_eq!(version.created_by, CreatedBy::ComposerLlm);
    assert!(
        !version.creating_llm_call_ids.is_empty(),
        "the version's provenance ids must be non-empty (attributable)"
    );
    assert_eq!(version.dsl.name, "RSI Oversold");
    assert_eq!(version.dsl.filters.len(), 1, "the trend filter was built");

    // The compact DSL summary renders what the version carries: the entry line,
    // the one filter, exits and risk — nothing invented.
    assert_eq!(summary.dsl.direction, "long");
    assert!(
        summary.dsl.entry.contains("rsi") && summary.dsl.entry.contains("30"),
        "the entry line renders the DSL's own fields: {}",
        summary.dsl.entry
    );
    assert_eq!(summary.dsl.filters.len(), 1);
    assert!(summary.dsl.filters[0].contains("ema"));
    assert_eq!(summary.dsl.exits.len(), 2);
    assert_eq!(summary.dsl.risk.len(), 2);

    // (d) the risk gate's ledger controls, through the version's OWN provenance
    //     ids: redacted at rest, composer-attributed, credential-source labelled.
    let ledger = SqliteLlmCallRepo::with_deps(db.pool().clone(), FakeClock::at(0));
    assert_ledger_redacted_labelled_and_attributed(&ledger, &version.creating_llm_call_ids).await;
}

/// Bus contract clause 2 + spec step 4: a sink that dies MID-run is
/// cancellation. The provider guard trips on the failed send, the composer ends
/// with a provider error, and the core maps that to `cancelled: true` — never a
/// `BusError` — with nothing persisted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sink_that_dies_mid_run_cancels_the_compose_rather_than_erroring() {
    let (_tmp, db) = migrated_db().await;
    let deps = fake_deps(&db, happy_path_script());

    let sink = FlakySink {
        ok_for: 3,
        sent: AtomicUsize::new(0),
    };
    let outcome = compose_strategy_core(
        &RunId::new(),
        deps,
        "RSI oversold on BTC",
        &sink,
        new_latch(),
    )
    .await
    .expect("a dead channel is cancellation, not a bus error");

    assert!(
        outcome.cancelled,
        "the failed send must read as cancellation"
    );
    assert_eq!(outcome.emitted, 3, "the run stops at the first failed send");
    assert!(
        outcome.strategy.is_none(),
        "a cancelled run carries no finalize summary"
    );

    // And nothing half-persisted: the compose never reached its persist step.
    let strategies = SqliteStrategyRepo::new(db.pool().clone())
        .list_strategies(true)
        .await
        .expect("list_strategies");
    assert!(
        strategies.is_empty(),
        "a cancelled run must not persist a strategy"
    );
}

/// A cancel that lands DURING the final model turn still stops before persisting.
///
/// The provider guard checks the latch BEFORE each turn, so a cancel arriving while
/// the last turn is already in flight misses it: the response comes back `Ok`, the
/// composer finalizes, and without a second check the two repository writes would
/// persist a strategy the user cancelled seconds earlier. `run_compose_with` checks
/// the latch between the composer returning and the first write.
///
/// The sink here stays HEALTHY and the latch is tripped by the LAST scripted
/// provider turn — the closest a test can get to `compose_cancel` arriving while
/// the final response is in flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_during_the_final_turn_stops_before_persisting() {
    let (_tmp, db) = migrated_db().await;
    let latch = new_latch();
    let deps = deps_with_provider(
        &db,
        LatchTrippingProvider {
            inner: FakeComposerProvider::new(happy_path_script()),
            latch: Arc::clone(&latch),
            trip_on_turn: 6,
            turns: AtomicUsize::new(0),
        },
    );

    let sink = Collector {
        events: Mutex::new(Vec::new()),
    };
    let outcome = compose_strategy_core(&RunId::new(), deps, "RSI oversold on BTC", &sink, latch)
        .await
        .expect("a cancel is cancellation, not a bus error");

    assert!(
        outcome.cancelled,
        "the tripped latch must read as cancellation"
    );
    assert!(outcome.strategy.is_none());

    let strategies = SqliteStrategyRepo::new(db.pool().clone())
        .list_strategies(true)
        .await
        .expect("list_strategies");
    assert!(
        strategies.is_empty(),
        "a run cancelled during its final turn must not persist a strategy"
    );
}

/// The same clause at the stream's first beat: a sink already dead cancels
/// before the composer is ever invoked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_already_dead_sink_cancels_before_the_run_opens() {
    let (_tmp, db) = migrated_db().await;
    let deps = fake_deps(&db, happy_path_script());

    let outcome = compose_strategy_core(
        &RunId::new(),
        deps,
        "RSI oversold on BTC",
        &DeadSink,
        new_latch(),
    )
    .await
    .expect("a dead channel is cancellation, not a bus error");

    assert!(outcome.cancelled);
    assert_eq!(outcome.emitted, 0, "nothing crossed");
    assert!(outcome.strategy.is_none());
}

/// The OTHER way a run is cancelled: the `compose_cancel` command trips the
/// run's latch while the sink is perfectly healthy.
///
/// This is the path the Designer's unmount cleanup takes. It exists because a
/// dead sink is not detectable from the frontend: a JavaScript `Channel`'s
/// callback stays registered with Tauri for the life of the webview, so
/// navigating away left every send SUCCEEDING and the run streamed on, burning
/// billable LLM calls and persisting a strategy nobody was waiting for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_externally_tripped_latch_cancels_a_run_whose_sink_is_alive() {
    let (_tmp, db) = migrated_db().await;
    let deps = fake_deps(&db, happy_path_script());

    // Tripped BEFORE the run opens — the shape a cancel that lands during the
    // command's credential/config load takes.
    let latch = new_latch();
    latch.store(true, Ordering::SeqCst);

    let sink = Collector {
        events: Mutex::new(Vec::new()),
    };
    let outcome = compose_strategy_core(&RunId::new(), deps, "RSI oversold on BTC", &sink, latch)
        .await
        .expect("an external cancel is cancellation, not a bus error");

    assert!(outcome.cancelled);
    assert_eq!(
        outcome.emitted, 0,
        "a run cancelled before it opens emits nothing — not even Started"
    );
    assert!(outcome.strategy.is_none());
    assert!(
        sink.events.lock().expect("events lock").is_empty(),
        "the sink is alive, so an empty event list proves the run never ran"
    );

    let strategies = SqliteStrategyRepo::new(db.pool().clone())
        .list_strategies(true)
        .await
        .expect("list_strategies");
    assert!(
        strategies.is_empty(),
        "a cancelled run must not persist a strategy"
    );
}

/// The same latch, tripped MID-run: the provider guard refuses at the next
/// model turn, so cancellation costs at most one further LLM call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_latch_tripped_mid_run_stops_the_compose_at_the_next_model_turn() {
    let (_tmp, db) = migrated_db().await;
    let deps = fake_deps(&db, happy_path_script());

    // A sink that trips the latch once a few events have crossed — standing in
    // for the `compose_cancel` command arriving while the run streams.
    let latch = new_latch();
    let sink = TrippingSink {
        latch: Arc::clone(&latch),
        trip_after: 3,
        sent: AtomicUsize::new(0),
        events: Mutex::new(Vec::new()),
    };

    let outcome = compose_strategy_core(&RunId::new(), deps, "RSI oversold on BTC", &sink, latch)
        .await
        .expect("an external cancel is cancellation, not a bus error");

    assert!(
        outcome.cancelled,
        "the tripped latch must read as cancellation"
    );
    assert!(
        outcome.strategy.is_none(),
        "a cancelled run carries no finalize summary"
    );

    let strategies = SqliteStrategyRepo::new(db.pool().clone())
        .list_strategies(true)
        .await
        .expect("list_strategies");
    assert!(
        strategies.is_empty(),
        "a run cancelled before it finalized must not persist a strategy"
    );
}
