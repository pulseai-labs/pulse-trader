//! Offline end-to-end test for VS-1.3.2 work-2.05 — the `pulse compose`
//! composition root (demo criterion 1, FR-3 / FR-4 / NFR-6).
//!
//! Drives the injectable core [`run_compose_with`] with a **fake** provider (no
//! network, MASTER-SPEC §9.4) over the REAL composer + REAL builder tools + a REAL
//! `tempfile` `SQLite` repo, and asserts a finalized, schema-valid `StrategyVersion` is
//! persisted with `created_by = ComposerLlm`, non-empty `creating_llm_call_ids`, and
//! REDACTED `LlmCall` rows (NFR-6). A second case scripts an invalid tool arg → a
//! correctable `FieldError` fed back → the run still finalizes. NO live LLM.
//!
//! Offline (in-process `MIGRATOR` + committed `.sqlx/`), `TempDir`-isolated.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Mutex;

use pulse::{
    ComposeCliOutcome, ComposeWiring, ComposerEvent, CreatedBy, Db, FakeClock, LlmBackend,
    LlmCallId, LlmCallRepository, LlmConfig, LlmError, LlmProvider, LlmResponse, MIGRATOR, Message,
    ModelPrice, PriceTable, Redactor, SqliteLlmCallRepo, SqliteStrategyRepo, StrategyRepository,
    TokenUsage, ToolCall, ToolDefinition, run_compose_with,
};
use rust_decimal::Decimal;
use serde_json::json;
use tempfile::TempDir;

/// A latch nobody trips — the CLI surface has no cancellation channel, so these
/// e2es exercise the uncancelled path.
fn never_cancelled() -> std::sync::atomic::AtomicBool {
    std::sync::atomic::AtomicBool::new(false)
}

/// An API-key-shaped literal the composer (compose-time) + the decorator (at rest)
/// must both strip from every persisted `LlmCall` prompt. NOT a real key.
const FAKE_KEY: &str = "sk-COMPOSE1234abcd5678efgh9012ijkl3456";

/// A stand-in composer system prompt (the fake provider ignores it; the composer
/// only needs a non-empty framing string).
const TEST_PROMPT: &str = "You are PulseTrader's strategy composer. Build the \
    strategy only by calling builder tools; never emit raw DSL JSON.";

/// A scripted [`LlmProvider`] double: returns queued responses turn-by-turn, each
/// carrying a known [`TokenUsage`] so the REAL decorator computes a non-zero cost +
/// writes a redacted `LlmCall`. No network, no keychain — the provider is the ONLY
/// faked layer (the composer, builder tools, and `SQLite` repo are all real).
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
    ) -> impl Future<Output = Result<LlmResponse, LlmError>> {
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

/// A TEST price table keyed on `gpt-oss:120b` (the [`config`] model) so the decorator
/// prices the model + writes a non-zero cost. TEST values, not production moat data.
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

/// The demo happy-path script: create → RSI(14)<30 entry → Close>EMA(200) filter →
/// [5% stop, 2R TP] exits → [1%, 3x] risk → finalize (built only via tools).
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

/// AC-3 (demo criterion 1): the fake-provider e2e drives the REAL composer + builder
/// tools + REAL temp `SQLite`; asserts a persisted schema-valid `StrategyVersion`,
/// `created_by = ComposerLlm`, non-empty `creating_llm_call_ids`, and REDACTED
/// `LlmCall` prompts (NFR-6 / BACKLOG-7). NO live LLM.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn composes_and_persists_strategy_version_over_fake_provider() {
    let (_tmp, db) = migrated_db().await;

    // The SINGLE SHARED CLOCK (#82): ONE FakeClock into BOTH the ledger repo AND (via
    // the core) the redacting decorator, so the persisted LlmCall.created_at is
    // single-sourced. FakeClock is Copy, so the same instant reaches both seams.
    let clock = FakeClock::at(1_700_000_000_000);
    let llm_repo = SqliteLlmCallRepo::with_deps(db.pool().clone(), clock);
    let strategy_repo = SqliteStrategyRepo::new(db.pool().clone());

    let wiring = ComposeWiring {
        provider: FakeComposerProvider::new(happy_path_script()),
        llm_repo,
        // Tag the fake key so the decorator ALSO strips it at rest (defense in depth).
        redactor: Redactor::from_config(vec![FAKE_KEY.to_owned()]),
        prices: test_prices(),
        clock,
        prompt: TEST_PROMPT.to_owned(),
        // r1.s1.w2: the e2e's fake provider has no resolved credential behind it,
        // so it records no provenance. `None` is the honest label, and it keeps
        // this fixture asserting composition rather than credential resolution.
        key_source: None,
        config: config(),
    };

    // The NL target smuggles an API-key-shaped secret — it must NOT reach any
    // persisted LlmCall row (NFR-6).
    let nl_target = format!("RSI oversold bounce on BTC; my api key {FAKE_KEY} do not leak it");

    let mut streamed: Vec<ComposerEvent> = Vec::new();
    let outcome: ComposeCliOutcome = run_compose_with(
        wiring,
        &strategy_repo,
        &nl_target,
        &mut |event| {
            streamed.push(event);
        },
        &never_cancelled(),
    )
    .await
    .expect("the scripted tool sequence composes + persists a strategy version");

    // (a) a finalized, schema-valid StrategyVersion is persisted (repo-minted ids).
    let version = &outcome.version;
    assert!(
        !version.id.as_str().is_empty(),
        "the repo minted a version id"
    );
    assert!(
        !version.strategy_id.as_str().is_empty(),
        "the repo minted a strategy id"
    );
    assert!(
        !version.version_hash.is_empty(),
        "the repo minted a version hash"
    );
    assert_eq!(
        version.parent_version_id, None,
        "a new strategy's initial version has no parent"
    );
    assert_eq!(version.dsl.name, "RSI Oversold");
    assert_eq!(
        version.dsl.filters.len(),
        1,
        "the trend filter was built via tools"
    );
    assert_eq!(outcome.strategy.name, "RSI Oversold");

    // (b) composer provenance: created_by = ComposerLlm + non-empty call ids.
    assert_eq!(version.created_by, CreatedBy::ComposerLlm);
    assert!(
        !version.creating_llm_call_ids.is_empty(),
        "creating_llm_call_ids must be non-empty"
    );
    assert!(
        !outcome.llm_call_ids.is_empty(),
        "the run minted LlmCall ids"
    );

    // TRUE persistence: read the version back through a fresh repo over the pool; the
    // read-path re-derives .dsl from dsl_original + checks the version_hash.
    let reader = SqliteStrategyRepo::new(db.pool().clone());
    let fetched = reader
        .get_version(&version.id)
        .await
        .expect("get_version")
        .expect("the persisted version is fetchable");
    assert_eq!(fetched.created_by, CreatedBy::ComposerLlm);
    assert_eq!(
        fetched.dsl, version.dsl,
        "the persisted DSL re-derives equal"
    );

    // (c) NFR-6 + provenance: every persisted LlmCall row is redacted at rest AND
    // attributed to the composer.
    let ledger = SqliteLlmCallRepo::with_deps(db.pool().clone(), clock);
    assert_eq!(
        outcome.llm_call_ids.len(),
        6,
        "one LlmCall persisted per model turn"
    );
    assert_ledger_rows_redacted_and_composer_attributed(&ledger, &outcome.llm_call_ids).await;

    // Streaming: the callback saw the recorded events, ending in Finalized.
    assert_eq!(
        streamed, outcome.events,
        "the streamed callback matches the recorded event copy"
    );
    assert!(matches!(
        outcome.events.last(),
        Some(ComposerEvent::Finalized { .. })
    ));
}

/// Assert every persisted `LlmCall` row is safe at rest AND correctly attributed:
///
/// - NFR-6: the prompt carries no secret and does carry a redaction marker.
/// - PR #93 review (Codex): `created_by` must be `ComposerLlm`, agreeing with the
///   `StrategyVersion` these rows are provenance-linked from. `RedactingLoggingProvider`
///   defaults to `CreatedBy::Human`, so the compose composition root MUST override it —
///   and `llm_call` is UPDATE/DELETE-trigger-immutable, so a row written under the wrong
///   actor could never be corrected in place.
async fn assert_ledger_rows_redacted_and_composer_attributed(
    ledger: &SqliteLlmCallRepo<FakeClock>,
    ids: &[LlmCallId],
) {
    for id in ids {
        let call = ledger
            .get_call(id)
            .await
            .expect("get_call")
            .expect("the ledger row is present");
        let wire = serde_json::to_string(&call.prompt_messages).expect("serialize prompt");
        assert!(
            !wire.contains(FAKE_KEY),
            "persisted LlmCall {} leaks the secret: {wire}",
            id.as_str()
        );
        assert!(
            wire.contains("REDACTED"),
            "persisted LlmCall {} prompt not redacted: {wire}",
            id.as_str()
        );
        assert_eq!(
            call.created_by,
            CreatedBy::ComposerLlm,
            "LlmCall {} must be attributed to the composer, not a human",
            id.as_str()
        );
    }
}

/// The correctable-recovery script: create → an out-of-range risk (> 1) rejected as
/// a correctable `FieldError` → corrected → entry → filter → exits → finalize.
fn correctable_then_finalize_script() -> Vec<LlmResponse> {
    vec![
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "RSI Oversold", "direction": "long" }),
        ),
        // Turn 2: out-of-range risk (> 1) → correctable FieldError, fed back.
        tool_turn(
            "c2",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "2.0", "max_leverage": "3" }),
        ),
        // Turn 3: corrected.
        tool_turn(
            "c3",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ),
        tool_turn(
            "c4",
            "add_entry_signal",
            json!({
                "left": { "source": "indicator", "indicator": "rsi", "period": 14 },
                "op": "lt",
                "right": { "source": "constant", "value": "30" }
            }),
        ),
        tool_turn(
            "c5",
            "add_filter",
            json!({
                "left": { "source": "price", "price_field": "close" },
                "op": "gt",
                "right": { "source": "indicator", "indicator": "ema", "period": 200 }
            }),
        ),
        tool_turn(
            "c6",
            "set_exit_rules",
            json!({ "stop_loss_pct": "0.05", "take_profit_r": "2" }),
        ),
        tool_turn("c7", "finalize_strategy", json!({})),
    ]
}

/// AC-4: a scripted invalid tool arg surfaces a correctable `FieldError` (streamed +
/// fed back), and the run STILL finalizes into a persisted `ComposerLlm` version.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_tool_input_surfaces_correctable_error_then_finalizes() {
    let (_tmp, db) = migrated_db().await;
    let clock = FakeClock::at(1_700_000_500_000);
    let llm_repo = SqliteLlmCallRepo::with_deps(db.pool().clone(), clock);
    let strategy_repo = SqliteStrategyRepo::new(db.pool().clone());

    let wiring = ComposeWiring {
        provider: FakeComposerProvider::new(correctable_then_finalize_script()),
        llm_repo,
        redactor: Redactor::default(),
        prices: test_prices(),
        clock,
        prompt: TEST_PROMPT.to_owned(),
        // r1.s1.w2: the e2e's fake provider has no resolved credential behind it,
        // so it records no provenance. `None` is the honest label, and it keeps
        // this fixture asserting composition rather than credential resolution.
        key_source: None,
        config: config(),
    };

    let mut streamed: Vec<ComposerEvent> = Vec::new();
    let outcome = run_compose_with(
        wiring,
        &strategy_repo,
        "RSI oversold on BTC",
        &mut |event| {
            streamed.push(event);
        },
        &never_cancelled(),
    )
    .await
    .expect("recovers from the correctable error and still finalizes");

    // The correctable FieldError was surfaced as a streamed tool-result event ...
    assert!(
        outcome.events.iter().any(|event| matches!(
            event,
            ComposerEvent::ToolCallResult { outcome, .. } if outcome.contains("risk_per_trade_pct")
        )),
        "a correctable risk error must be streamed as a tool-result event"
    );
    // ... and the run STILL finalized into a persisted ComposerLlm version.
    assert!(matches!(
        outcome.events.last(),
        Some(ComposerEvent::Finalized { .. })
    ));
    assert_eq!(outcome.version.created_by, CreatedBy::ComposerLlm);
    assert!(
        !outcome.version.id.as_str().is_empty(),
        "the finalized version persisted with a repo-minted id"
    );

    // The persisted version is fetchable (true persistence through the recovery).
    let fetched = strategy_repo
        .get_version(&outcome.version.id)
        .await
        .expect("get_version")
        .expect("the finalized version is fetchable");
    assert_eq!(fetched.dsl.name, "RSI Oversold");
}
