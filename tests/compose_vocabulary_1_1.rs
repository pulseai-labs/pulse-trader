//! Offline end-to-end coverage for the schema-1.1.0 builder vocabulary
//! (r2.s2.w3, #160's HTF/ATR half + #163): the `timeframe` operand token, `atr`
//! as an indicator, and the either/or ATR stop on `set_exit_rules` — composed
//! through the REAL composer + REAL builder tools over a REAL `tempfile`
//! `SQLite` repo, driven by the scripted fake provider (no network, no live
//! LLM, MASTER-SPEC §9.4).
//!
//! Proves a described "H4 EMA(200) filter, 2xATR(14) stop" is COMPOSED — never
//! silently substituted with a primary-series EMA and a percent stop — and that
//! the malformed stop/timeframe shapes are refused with localized, correctable
//! `FieldError`s. Offline (in-process `MIGRATOR` + committed `.sqlx/`),
//! `TempDir`-isolated.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};

use pulse::{
    ComposeCliOutcome, ComposeWiring, ComposerEvent, Condition, Db, ExitRule, FakeClock,
    IndicatorSpec, LlmBackend, LlmConfig, LlmError, LlmProvider, LlmResponse, MIGRATOR, Message,
    Migrator, ModelPrice, PriceField, PriceTable, Redactor, SchemaVersion, Series,
    SqliteLlmCallRepo, SqliteStrategyRepo, SweepableValue, TokenUsage, ToolCall, ToolDefinition,
    ValueSource, run_compose_with,
};
use rust_decimal::Decimal;
use serde_json::json;
use tempfile::TempDir;

/// A latch nobody trips — the CLI surface has no cancellation channel, so these
/// e2es exercise the uncancelled path.
fn never_cancelled() -> std::sync::atomic::AtomicBool {
    std::sync::atomic::AtomicBool::new(false)
}

/// A stand-in composer system prompt (the fake provider ignores it; the composer
/// only needs a non-empty framing string).
const TEST_PROMPT: &str = "You are PulseTrader's strategy composer. Build the \
    strategy only by calling builder tools; never emit raw DSL JSON.";

/// A scripted [`LlmProvider`] double: returns queued responses turn-by-turn and
/// records the tool definitions it was advertised on the first call into a
/// shared slot (the builder definitions are `pub(crate)` — what the composer
/// actually SENDS the model is the observable surface, so (e) asserts on the
/// advertised list itself). No network, no keychain — the provider is the ONLY
/// faked layer.
struct FakeComposerProvider {
    scripts: Mutex<VecDeque<LlmResponse>>,
    advertised: Arc<Mutex<Option<Vec<ToolDefinition>>>>,
}

impl FakeComposerProvider {
    fn new(
        responses: Vec<LlmResponse>,
        advertised: Arc<Mutex<Option<Vec<ToolDefinition>>>>,
    ) -> Self {
        Self {
            scripts: Mutex::new(responses.into()),
            advertised,
        }
    }
}

impl LlmProvider for FakeComposerProvider {
    fn chat(
        &self,
        _messages: Vec<Message>,
        tools: &[ToolDefinition],
        _config: &LlmConfig,
    ) -> impl Future<Output = Result<LlmResponse, LlmError>> {
        {
            let mut advertised = self.advertised.lock().expect("advertised lock");
            if advertised.is_none() {
                *advertised = Some(tools.to_vec());
            }
        }
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

/// A TEST price table keyed on `gpt-oss:120b` (the [`config`] model) so the
/// decorator prices the model + writes a non-zero cost. TEST values, not
/// production moat data.
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

/// Run the scripted sequence through the REAL composer loop, the REAL builder
/// tools and a REAL temp `SQLite` repo; returns the outcome plus the slot
/// holding the tool definitions the composer advertised on the first provider
/// turn.
async fn run_scripted(
    script: Vec<LlmResponse>,
) -> (ComposeCliOutcome, Arc<Mutex<Option<Vec<ToolDefinition>>>>) {
    let (_tmp, db) = migrated_db().await;
    let clock = FakeClock::at(1_700_000_000_000);
    let llm_repo = SqliteLlmCallRepo::with_deps(db.pool().clone(), clock);
    let strategy_repo = SqliteStrategyRepo::new(db.pool().clone());
    let advertised = Arc::new(Mutex::new(None));

    let wiring = ComposeWiring {
        provider: FakeComposerProvider::new(script, Arc::clone(&advertised)),
        llm_repo,
        redactor: Redactor::default(),
        prices: test_prices(),
        clock,
        prompt: TEST_PROMPT.to_owned(),
        // No resolved credential behind a fake provider — `None` is the honest
        // provenance label (same convention as compose_cli.rs).
        key_source: None,
        config: config(),
    };

    let outcome = run_compose_with(
        wiring,
        &strategy_repo,
        "M15 RSI oversold, H4 EMA(200) trend filter, 2xATR(14) stop",
        &mut |_event| {},
        &never_cancelled(),
    )
    .await
    .expect("the scripted tool sequence composes and persists a version");

    (outcome, advertised)
}

/// The `ToolCallResult` outcome strings the composer streamed for `tool_name`
/// (an `Err` outcome is the serialized correctable `FieldError`s).
fn tool_outcomes<'a>(outcome: &'a ComposeCliOutcome, tool_name: &str) -> Vec<&'a str> {
    outcome
        .events
        .iter()
        .filter_map(|event| match event {
            ComposerEvent::ToolCallResult { name, outcome } if name == tool_name => {
                Some(outcome.as_str())
            }
            _ => None,
        })
        .collect()
}

/// The journey's setup (spec case (a)): create → RSI(14)<30 entry → H4
/// close>H4 EMA(200) filter → [2xATR(14) stop, 2R TP] exits → 1%/3x risk →
/// finalize.
fn htf_atr_journey_script() -> Vec<LlmResponse> {
    vec![
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "H4 Trend RSI", "direction": "long" }),
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
                "left": { "source": "price", "price_field": "close", "timeframe": "h4" },
                "op": "gt",
                "right": { "source": "indicator", "indicator": "ema", "period": 200, "timeframe": "h4" }
            }),
        ),
        tool_turn(
            "c4",
            "set_exit_rules",
            json!({ "atr_stop_period": 14, "atr_stop_multiple": "2", "take_profit_r": "2" }),
        ),
        tool_turn(
            "c5",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ),
        tool_turn("c6", "finalize_strategy", json!({})),
    ]
}

/// (a) The journey's setup composed through the tools: `timeframe: "h4"` on both
/// filter operands lands `Series::Htf` on the persisted DSL, and the ATR pair on
/// `set_exit_rules` lands `ExitRule::AtrStop { 14, 2 }` — no substitution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn htf_filter_and_atr_stop_compose_with_no_substitution() {
    let (outcome, _advertised) = run_scripted(htf_atr_journey_script()).await;
    let version = &outcome.version;

    // The filter's BOTH operands carry the higher-timeframe series — the
    // described H4 filter is composed, never substituted with primary series.
    let filter = version
        .dsl
        .filters
        .first()
        .expect("the filter was composed");
    let Condition::Compare { lhs, rhs, .. } = filter else {
        panic!("the filter is a Compare condition, got {filter:?}");
    };
    assert_eq!(
        lhs,
        &ValueSource::Price {
            series: Series::Htf,
            field: PriceField::Close,
        },
        "the left operand is the H4 close, not a primary-series close"
    );
    assert_eq!(
        rhs,
        &ValueSource::Indicator {
            series: Series::Htf,
            spec: IndicatorSpec::Ema {
                period: SweepableValue::Fixed(200),
            },
        },
        "the right operand is the H4 EMA(200), not a primary-series EMA"
    );

    // The ATR pair composed an AtrStop; no StopLoss was silently substituted.
    assert!(
        version.dsl.exits.iter().any(|rule| matches!(
            rule,
            ExitRule::AtrStop {
                period: SweepableValue::Fixed(14),
                multiple,
            } if *multiple == SweepableValue::Fixed(Decimal::from(2))
        )),
        "exits must carry AtrStop {{ period: 14, multiple: 2 }}: {:?}",
        version.dsl.exits
    );
    assert!(
        !version
            .dsl
            .exits
            .iter()
            .any(|rule| matches!(rule, ExitRule::StopLoss { .. })),
        "no percent StopLoss may be substituted for the ATR stop: {:?}",
        version.dsl.exits
    );
    assert!(
        version
            .dsl
            .exits
            .iter()
            .any(|rule| matches!(rule, ExitRule::TakeProfit { .. })),
        "the 2R take-profit still composes beside the ATR stop"
    );

    // Schema pinning + the verbatim source round-trips through the production
    // migrator into the same typed document (already-current, no migration).
    assert_eq!(
        version.dsl_schema_version.to_string(),
        "1.1.0",
        "the persisted version is stamped schema 1.1.0"
    );
    assert_eq!(version.dsl.schema_version, SchemaVersion::CURRENT);
    let loaded = Migrator::v1()
        .load(&version.dsl_original)
        .expect("dsl_original loads through Migrator::v1");
    assert_eq!(loaded.dsl, version.dsl);
    assert_eq!(loaded.dsl_original, version.dsl_original);

    assert!(matches!(
        outcome.events.last(),
        Some(ComposerEvent::Finalized { .. })
    ));
}

/// (b) `set_exit_rules` enforces exactly ONE stop family: both families
/// together, one ATR half alone, and neither all refuse with a `FieldError`
/// pathed at `stop_loss_pct` whose message names both choices — and the run
/// still finalizes after the corrected call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_exit_rules_refuses_ambiguous_stop_families() {
    let script = vec![
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "Stop Family Refusals", "direction": "long" }),
        ),
        // Both families at once → refuse.
        tool_turn(
            "c2",
            "set_exit_rules",
            json!({ "stop_loss_pct": "0.05", "atr_stop_period": 14, "atr_stop_multiple": "2" }),
        ),
        // One ATR half without the other → refuse.
        tool_turn("c3", "set_exit_rules", json!({ "atr_stop_period": 14 })),
        // Neither family → refuse.
        tool_turn("c4", "set_exit_rules", json!({ "take_profit_r": "2" })),
        // Corrected: the ATR pair alone.
        tool_turn(
            "c5",
            "set_exit_rules",
            json!({ "atr_stop_period": 14, "atr_stop_multiple": "2", "take_profit_r": "2" }),
        ),
        tool_turn(
            "c6",
            "add_entry_signal",
            json!({
                "left": { "source": "indicator", "indicator": "rsi", "period": 14 },
                "op": "lt",
                "right": { "source": "constant", "value": "30" }
            }),
        ),
        tool_turn(
            "c7",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ),
        tool_turn("c8", "finalize_strategy", json!({})),
    ];
    let (outcome, _advertised) = run_scripted(script).await;

    let refusals = tool_outcomes(&outcome, "set_exit_rules");
    assert_eq!(
        refusals.len(),
        4,
        "three refusals + one accept were streamed: {refusals:?}"
    );
    for refusal in &refusals[..3] {
        assert!(
            refusal.contains("stop_loss_pct"),
            "the refusal is pathed at stop_loss_pct: {refusal}"
        );
        assert!(
            refusal.contains("atr_stop_period") && refusal.contains("atr_stop_multiple"),
            "the refusal names both choices: {refusal}"
        );
    }
    assert!(
        refusals[3].contains("exit rule"),
        "the corrected call succeeded: {}",
        refusals[3]
    );
    assert!(matches!(
        outcome.events.last(),
        Some(ComposerEvent::Finalized { .. })
    ));
}

/// (c) An operand `timeframe` token other than `primary`/`h4` is refused with a
/// `FieldError` pathed at `{left|right}.timeframe` — the run then composes with
/// the corrected operand.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_timeframe_token_is_a_localized_field_error() {
    let script = vec![
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "Bad Timeframe", "direction": "long" }),
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
        // The malformed call: `timeframe: "1h"` on the RIGHT operand.
        tool_turn(
            "c3",
            "add_filter",
            json!({
                "left": { "source": "price", "price_field": "close" },
                "op": "gt",
                "right": { "source": "indicator", "indicator": "ema", "period": 200, "timeframe": "1h" }
            }),
        ),
        // Corrected: `h4`.
        tool_turn(
            "c4",
            "add_filter",
            json!({
                "left": { "source": "price", "price_field": "close", "timeframe": "h4" },
                "op": "gt",
                "right": { "source": "indicator", "indicator": "ema", "period": 200, "timeframe": "h4" }
            }),
        ),
        tool_turn(
            "c5",
            "set_exit_rules",
            json!({ "stop_loss_pct": "0.015", "take_profit_r": "2" }),
        ),
        tool_turn(
            "c6",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ),
        tool_turn("c7", "finalize_strategy", json!({})),
    ];
    let (outcome, _advertised) = run_scripted(script).await;

    let filter_results = tool_outcomes(&outcome, "add_filter");
    assert_eq!(filter_results.len(), 2, "one refusal + one accept");
    assert!(
        filter_results[0].contains("right.timeframe"),
        "the refusal is pathed at right.timeframe: {}",
        filter_results[0]
    );
    assert!(
        filter_results[0].contains("1h"),
        "the refusal names the rejected token: {}",
        filter_results[0]
    );
    assert!(matches!(
        outcome.events.last(),
        Some(ComposerEvent::Finalized { .. })
    ));
}

/// (d) `indicator: "atr"` on an entry operand composes `IndicatorSpec::Atr`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atr_indicator_operand_composes_indicator_spec_atr() {
    let script = vec![
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "ATR Volatility Entry", "direction": "long" }),
        ),
        tool_turn(
            "c2",
            "add_entry_signal",
            json!({
                "left": { "source": "indicator", "indicator": "atr", "period": 14 },
                "op": "gt",
                "right": { "source": "constant", "value": "20" }
            }),
        ),
        tool_turn(
            "c3",
            "set_exit_rules",
            json!({ "stop_loss_pct": "0.015", "take_profit_r": "2" }),
        ),
        tool_turn(
            "c4",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ),
        tool_turn("c5", "finalize_strategy", json!({})),
    ];
    let (outcome, _advertised) = run_scripted(script).await;

    let Condition::Compare { lhs, .. } = &outcome.version.dsl.entry else {
        panic!("the entry is a Compare condition");
    };
    assert_eq!(
        lhs,
        &ValueSource::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Atr {
                period: SweepableValue::Fixed(14),
            },
        },
        "the atr operand composes IndicatorSpec::Atr on the primary series"
    );
    assert!(matches!(
        outcome.events.last(),
        Some(ComposerEvent::Finalized { .. })
    ));
}

/// (e) The tool definitions the composer actually advertises carry the 1.1.0
/// vocabulary: `timeframe` on every operand, `atr` in the indicator enum, the
/// ATR pair on `set_exit_rules`, and `stop_loss_pct` no longer required.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advertised_tool_definitions_carry_the_1_1_0_vocabulary() {
    let script = vec![
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "Schema Check", "direction": "long" }),
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
            "set_exit_rules",
            json!({ "stop_loss_pct": "0.015", "take_profit_r": "2" }),
        ),
        tool_turn(
            "c4",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ),
        tool_turn("c5", "finalize_strategy", json!({})),
    ];
    let (_outcome, advertised) = run_scripted(script).await;
    let tools = advertised
        .lock()
        .expect("advertised lock")
        .clone()
        .expect("the composer advertised tools on the first turn");

    let def = |name: &str| -> &ToolDefinition {
        tools
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("{name} is advertised"))
    };
    for signal_tool in ["add_entry_signal", "add_filter"] {
        for side in ["left", "right"] {
            let operand_props = &def(signal_tool).parameters["properties"][side]["properties"];
            assert_eq!(
                operand_props["timeframe"]["enum"],
                json!(["primary", "h4"]),
                "{signal_tool}.{side} advertises timeframe primary|h4"
            );
            let indicators = operand_props["indicator"]["enum"]
                .as_array()
                .expect("indicator enum is an array");
            assert!(
                indicators.iter().any(|v| v == "atr"),
                "{signal_tool}.{side} advertises atr in the indicator enum"
            );
        }
    }

    let exits = &def("set_exit_rules").parameters;
    assert!(
        exits["properties"].get("atr_stop_period").is_some(),
        "set_exit_rules advertises atr_stop_period"
    );
    assert!(
        exits["properties"].get("atr_stop_multiple").is_some(),
        "set_exit_rules advertises atr_stop_multiple"
    );
    let required = exits["required"].as_array();
    assert!(
        required.is_none_or(|req| !req.iter().any(|v| v == "stop_loss_pct")),
        "stop_loss_pct is no longer a required property: {}",
        exits["required"]
    );
}
