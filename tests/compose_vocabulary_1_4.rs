//! Offline end-to-end coverage for the schema-1.4.0 candle-pattern builder
//! vocabulary (r5.s1.w3): the composer prompt's worked "4h support rejection"
//! example, composed through the REAL composer loop, the REAL builder tools and
//! a REAL temp `SQLite` repo, driven by the scripted fake provider (no network,
//! no live LLM; the `compose_vocabulary_1_3.rs` pattern).
//!
//! It proves two things the item promises:
//!
//! 1. **The journey composes, never substitutes.** The scripted builder calls
//!    (`create_strategy` → `add_entry_signal` → two `add_filter`s →
//!    `set_exit_rules` → `set_risk_params` → `finalize_strategy`) produce
//!    EXACTLY the checked-in `examples/strategies/support-rejection-4h.json`
//!    strategy — the lagged support touch (`low <= lag(support, 1) * (1 + k)`),
//!    the close back above the same lagged level, and the named `Hammer`
//!    pattern reading `> 0` — compared as VALUES, never as text.
//! 2. **`finalize` succeeds** and the run's last event is the composer's own
//!    `Finalized` marker.
//!
//! Offline (in-process `MIGRATOR` + committed `.sqlx/`), `TempDir`-isolated.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use pulse::{
    ArithOp, CandlePatternName, Comparator, ComposeCliOutcome, ComposeWiring, ComposerEvent,
    Condition, Db, ExitRule, FakeClock, IndicatorSpec, LlmBackend, LlmConfig, LlmError,
    LlmProvider, LlmResponse, MIGRATOR, Message, Migrator, ModelPrice, PriceField, PriceTable,
    Redactor, RiskParams, SchemaVersion, Series, SqliteLlmCallRepo, SqliteStrategyRepo,
    SweepableValue, TokenUsage, ToolCall, ToolDefinition, ValueSource, render, run_compose_with,
};
use rust_decimal::Decimal;
use serde_json::json;
use tempfile::TempDir;

/// A stand-in composer system prompt (the fake provider ignores it; the
/// composer only needs a non-empty framing string).
const TEST_PROMPT: &str = "You are PulseTrader's strategy composer. Build the \
    strategy only by calling builder tools; never emit raw DSL JSON.";

/// The checked-in example the journey's values must reproduce.
fn example_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/strategies/support-rejection-4h.json")
}

/// A fresh, untripped cancellation latch — the CLI surface has no cancellation
/// channel, so these e2es exercise the uncancelled path.
fn never_cancelled() -> AtomicBool {
    AtomicBool::new(false)
}

/// A scripted [`LlmProvider`] double: returns queued responses turn-by-turn and
/// records the tool definitions the composer advertised on the first call.
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

/// A scripted single-tool-call turn (no prose).
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
/// tools and a REAL temp `SQLite` repo.
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
        // provenance label (same convention as compose_vocabulary_1_1/1_2/1_3).
        key_source: None,
        config: config(),
    };

    let outcome = run_compose_with(
        wiring,
        &strategy_repo,
        "Enter long when a 4h candle touches the support zone and closes back \
         above it, and that same 4h candle is a bullish hammer",
        &mut |_event| {},
        &never_cancelled(),
    )
    .await
    .expect("the scripted tool sequence composes and persists a version");

    (outcome, advertised)
}

/// The 17 schema-1.4.0 pattern names the public composer default must name
/// (`composer.md`), in catalog order.
const PATTERN_VOCABULARY: [&str; 17] = [
    "Doji",
    "DragonflyDoji",
    "GravestoneDoji",
    "Hammer",
    "InvertedHammer",
    "HangingMan",
    "ShootingStar",
    "Marubozu",
    "SpinningTop",
    "Engulfing",
    "Harami",
    "Piercing",
    "DarkCloudCover",
    "MorningStar",
    "EveningStar",
    "ThreeWhiteSoldiers",
    "ThreeBlackCrows",
];

/// The public composer default, compiled into this suite so the check cannot
/// drift from the shipped prompt.
static COMPOSER_PROMPT: &str = include_str!("../src/agent/prompts/composer.md");

/// The example's support operand, as the worked example writes it: the level
/// the previous H4 bar saw.
fn support_operand() -> serde_json::Value {
    json!({
        "source": "indicator",
        "indicator": "support",
        "left": 1,
        "right": 1,
        "tolerance_pct": "0.05",
        "min_touches": 2,
        "lookback": 50,
        "timeframe": "h4"
    })
}

/// The scripted journey: the prompt's worked example, one builder call per
/// step — `create_strategy` → `add_entry_signal` (the lagged support touch) →
/// `add_filter` (the close back above the level) → `add_filter` (the named
/// `Hammer` pattern) → `set_exit_rules` → `set_risk_params` →
/// `finalize_strategy`.
fn support_rejection_journey_script() -> Vec<LlmResponse> {
    vec![
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "Support Rejection 4H", "direction": "long" }),
        ),
        tool_turn(
            "c2",
            "add_entry_signal",
            json!({
                "left": { "source": "price", "price_field": "low", "timeframe": "h4" },
                "op": "lte",
                "right": {
                    "source": "arith",
                    "op": "mul",
                    "lhs": { "source": "lag", "bars": 1, "of": support_operand() },
                    "rhs": { "source": "constant", "value": "1.005" }
                }
            }),
        ),
        tool_turn(
            "c3",
            "add_filter",
            json!({
                "left": { "source": "price", "price_field": "close", "timeframe": "h4" },
                "op": "gt",
                "right": { "source": "lag", "bars": 1, "of": support_operand() }
            }),
        ),
        tool_turn(
            "c4",
            "add_filter",
            json!({
                "left": {
                    "source": "indicator",
                    "indicator": "candle_pattern",
                    "pattern": "Hammer",
                    "timeframe": "h4"
                },
                "op": "gt",
                "right": { "source": "constant", "value": "0" }
            }),
        ),
        tool_turn(
            "c5",
            "set_exit_rules",
            json!({ "stop_loss_pct": "0.015", "take_profit_r": "1" }),
        ),
        tool_turn(
            "c6",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ),
        tool_turn("c7", "finalize_strategy", json!({})),
    ]
}

/// The example's support level, as the DSL carries it.
fn support_spec() -> IndicatorSpec {
    IndicatorSpec::Support {
        left: SweepableValue::Fixed(1),
        right: SweepableValue::Fixed(1),
        tolerance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
        min_touches: SweepableValue::Fixed(2),
        lookback: SweepableValue::Fixed(50),
    }
}

/// The lagged level both the entry and filter 0 read.
fn lagged_support() -> ValueSource {
    ValueSource::Lag {
        value: Box::new(ValueSource::Indicator {
            series: Series::Htf,
            spec: support_spec(),
        }),
        bars: 1,
    }
}

/// (i) The journey composes the worked example: every construct lands as
/// composed (the lagged support touch, the close back above it, the named
/// `Hammer` pattern), `finalize` succeeds, and the stored document is
/// **exactly** the checked-in example's document — compared as VALUES.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn support_rejection_journey_composes_the_worked_example_with_no_substitution() {
    let (outcome, _advertised) = run_scripted(support_rejection_journey_script()).await;
    let version = &outcome.version;

    // The entry: h4:low <= lag(h4:support{…}, 1) * 1.005 — the level the
    // PREVIOUS H4 bar saw, never the same bar's (the support-touch trap).
    assert_eq!(
        version.dsl.entry,
        Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Htf,
                field: PriceField::Low,
            },
            op: Comparator::Lte,
            rhs: ValueSource::Arith {
                op: ArithOp::Mul,
                lhs: Box::new(lagged_support()),
                rhs: Box::new(ValueSource::Constant {
                    value: Decimal::new(1005, 3),
                }),
            },
        },
        "the entry is the lagged support touch with the 0.5% band"
    );

    // Filter 0: h4:close > lag(h4:support{…}, 1) — the close back above the
    // level the previous H4 bar saw.
    assert_eq!(
        version.dsl.filters[0],
        Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Htf,
                field: PriceField::Close,
            },
            op: Comparator::Gt,
            rhs: lagged_support(),
        },
        "filter 0 is the close back above the lagged level"
    );

    // Filter 1: h4:hammer > 0 — the named candle pattern fired bullishly.
    assert_eq!(
        version.dsl.filters[1],
        Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Htf,
                spec: IndicatorSpec::CandlePattern {
                    pattern: CandlePatternName::Hammer,
                },
            },
            op: Comparator::Gt,
            rhs: ValueSource::Constant {
                value: Decimal::ZERO,
            },
        },
        "filter 1 is the named Hammer pattern reading `> 0`"
    );

    // Exits and risk as the worked example writes them.
    assert_eq!(
        version.dsl.exits,
        vec![
            ExitRule::StopLoss {
                distance_pct: SweepableValue::Fixed(Decimal::new(15, 3)),
            },
            ExitRule::TakeProfit {
                target_r: SweepableValue::Fixed(Decimal::new(1, 0)),
            },
        ],
        "the exit pair is the 1.5% stop and its 1R take-profit"
    );
    assert_eq!(
        version.dsl.risk,
        RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
            max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
        },
    );

    // The composed document is EXACTLY the checked-in example — same values,
    // not a lookalike.
    let example_json =
        std::fs::read_to_string(example_path()).expect("read the checked-in example json");
    let example = Migrator::v1()
        .load(&example_json)
        .expect("the example loads");
    assert_eq!(
        version.dsl, example.dsl,
        "the journey composes the checked-in example exactly"
    );
    assert_eq!(version.dsl.schema_version, SchemaVersion::CURRENT);
    assert_eq!(version.dsl_schema_version.to_string(), "1.4.0");

    // The rendered lines speak the trap's form and the pattern in trader words.
    let rendered = render::strategy(&version.dsl);
    assert_eq!(
        rendered.entry,
        "h4:low <= (lag(h4:support(pivots 1/1, 2+ touches within 5%, 50 bars), 1) * 1.005)",
        "the entry renders as the previous H4 bar's level, never the same bar's"
    );
    assert_eq!(
        rendered.filters[1], "h4:hammer > 0",
        "the pattern filter renders as the named pattern"
    );

    // `finalize` succeeded: the run's last event is the composer's own marker.
    assert!(matches!(
        outcome.events.last(),
        Some(ComposerEvent::Finalized { .. })
    ));
}

/// (ii) The public composer default names the `"candle_pattern"` token and all
/// 17 pattern values, and states the reading rule and the support-touch form an
/// agent needs to write the worked example.
#[test]
fn composer_prompt_names_the_candle_pattern_token_and_all_seventeen() {
    assert!(
        COMPOSER_PROMPT.contains("\"candle_pattern\""),
        "composer.md must name the candle_pattern token in double quotes"
    );
    for name in PATTERN_VOCABULARY {
        assert!(
            COMPOSER_PROMPT.contains(&format!("\"{name}\"")),
            "composer.md must name the {name} pattern in double quotes"
        );
    }
    // The reading rule the prompt teaches: any bullish form, or the full one.
    assert!(
        COMPOSER_PROMPT.contains("Gt 0") && COMPOSER_PROMPT.contains("Eq 100"),
        "composer.md must state the `Gt 0` / `Eq 100` reading rule"
    );
    // The trap's form, named in the worked example.
    assert!(
        COMPOSER_PROMPT.contains("support-touch form") && COMPOSER_PROMPT.contains("lag("),
        "composer.md must state the lagged support-touch form"
    );
}

/// (iii) The builder tools advertise the new operand: `candle_pattern` in the
/// indicator enum, and `pattern` as a JSON-schema enum of the 17 names — so the
/// model can write the operand without guessing a spelling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advertised_tool_definitions_carry_the_candle_pattern_operand() {
    let (outcome, advertised) = run_scripted(support_rejection_journey_script()).await;
    assert!(matches!(
        outcome.events.last(),
        Some(ComposerEvent::Finalized { .. })
    ));
    let tools = advertised
        .lock()
        .expect("advertised lock")
        .clone()
        .expect("the composer advertised tools on the first turn");

    for signal_tool in ["add_entry_signal", "add_filter"] {
        let definition = tools
            .iter()
            .find(|d| d.name == signal_tool)
            .unwrap_or_else(|| panic!("{signal_tool} is advertised"));
        for side in ["left", "right"] {
            let operand = &definition.parameters["properties"][side];
            let indicators = operand["properties"]["indicator"]["enum"]
                .as_array()
                .expect("the indicator enum is an array");
            assert!(
                indicators.iter().any(|v| v == "candle_pattern"),
                "{signal_tool}.{side} advertises candle_pattern in the indicator enum"
            );
            let patterns = operand["properties"]["pattern"]["enum"]
                .as_array()
                .expect("the pattern enum is an array");
            assert_eq!(
                patterns.len(),
                17,
                "{signal_tool}.{side} advertises all 17 pattern names"
            );
            for name in PATTERN_VOCABULARY {
                assert!(
                    patterns.iter().any(|v| v == name),
                    "{signal_tool}.{side} advertises {name} in the pattern enum"
                );
            }
        }
    }
}
