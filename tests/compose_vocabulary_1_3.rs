//! Offline end-to-end coverage for the schema-1.3.0 structure/pattern builder
//! vocabulary (r4.s6.w4): the composer prompt's worked "4h resistance breakout
//! with a strong body and a trendline break" example, composed through the REAL
//! composer loop, the REAL builder tools and a REAL temp `SQLite` repo, driven
//! by the scripted fake provider (no network, no live LLM; the
//! `compose_vocabulary_1_2.rs` pattern).
//!
//! It proves three things the item promises:
//!
//! 1. **The journey composes, never substitutes.** The scripted builder calls
//!    (`create_strategy` → `add_entry_signal` → two `add_filter`s →
//!    `set_exit_rules` → `set_risk_params` → `finalize_strategy`) produce
//!    EXACTLY the checked-in `examples/strategies/sr-breakout-4h.json` strategy
//!    — the zone-break entry (`crosses_above` of the H4 close over the previous
//!    H4 bar's `resistance`, via a `lag` operand), the H4 anatomy gate
//!    (`body > 0.5 × range`), and the H4 trendline-highs break — compared as
//!    VALUES, never as text.
//! 2. **`finalize` succeeds** and the run's last event is the composer's own
//!    `Finalized` marker.
//! 3. **The prompt names the vocabulary.** `composer.md` (the public default the
//!    shipped agent reads) names every one of the twelve schema-1.3.0 tokens in
//!    double quotes, including the `resistance`/`support` zone-break form the
//!    never-true same-bar comparison would violate.
//!
//! Offline (in-process `MIGRATOR` + committed `.sqlx/`), `TempDir`-isolated.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use pulse::{
    ArithOp, Comparator, ComposeCliOutcome, ComposeWiring, ComposerEvent, Condition, Db, ExitRule,
    FakeClock, IndicatorSpec, LlmBackend, LlmConfig, LlmError, LlmProvider, LlmResponse, MIGRATOR,
    Message, Migrator, ModelPrice, PriceField, PriceTable, Redactor, RiskParams, SchemaVersion,
    Series, SqliteLlmCallRepo, SqliteStrategyRepo, SweepableValue, TokenUsage, ToolCall,
    ToolDefinition, ValueSource, render, run_compose_with,
};
use rust_decimal::Decimal;
use serde_json::json;
use tempfile::TempDir;

/// A stand-in composer system prompt (the fake provider ignores it; the
/// composer only needs a non-empty framing string).
const TEST_PROMPT: &str = "You are PulseTrader's strategy composer. Build the \
    strategy only by calling builder tools; never emit raw DSL JSON.";

/// The twelve schema-1.3.0 structure/pattern tokens the public composer default
/// must name (`composer.md`).
const VOCABULARY: [&str; 12] = [
    "pivot_high",
    "pivot_low",
    "resistance",
    "support",
    "double_top_neckline",
    "double_bottom_neckline",
    "trendline_lows",
    "trendline_highs",
    "body",
    "upper_wick",
    "lower_wick",
    "range",
];

/// The public composer default, compiled into this suite so the check cannot
/// drift from the shipped prompt.
static COMPOSER_PROMPT: &str = include_str!("../src/agent/prompts/composer.md");

/// The checked-in example the journey's values must reproduce.
fn example_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/strategies/sr-breakout-4h.json")
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
        // provenance label (same convention as compose_vocabulary_1_1/1_2).
        key_source: None,
        config: config(),
    };

    let outcome = run_compose_with(
        wiring,
        &strategy_repo,
        "Break of the 4h resistance built from swing highs, only when the H4 \
         candle that breaks it has a strong body and closes above the falling \
         trendline through the last swing highs",
        &mut |_event| {},
        &never_cancelled(),
    )
    .await
    .expect("the scripted tool sequence composes and persists a version");

    (outcome, advertised)
}

/// The scripted journey: the prompt's worked example, one builder call per
/// step — `create_strategy` → `add_entry_signal` (the zone-break cross) →
/// `add_filter` (the anatomy gate) → `add_filter` (the trendline break) →
/// `set_exit_rules` → `set_risk_params` → `finalize_strategy`.
fn sr_breakout_journey_script() -> Vec<LlmResponse> {
    vec![
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "S/R Breakout 4H", "direction": "long" }),
        ),
        tool_turn(
            "c2",
            "add_entry_signal",
            json!({
                "left": { "source": "price", "price_field": "close", "timeframe": "h4" },
                "op": "crosses_above",
                "right": {
                    "source": "lag",
                    "bars": 1,
                    "of": {
                        "source": "indicator",
                        "indicator": "resistance",
                        "left": 2,
                        "right": 1,
                        "tolerance_pct": "0.01",
                        "min_touches": 2,
                        "lookback": 150,
                        "timeframe": "h4"
                    }
                }
            }),
        ),
        tool_turn(
            "c3",
            "add_filter",
            json!({
                "left": { "source": "indicator", "indicator": "body", "timeframe": "h4" },
                "op": "gt",
                "right": {
                    "source": "arith",
                    "op": "mul",
                    "lhs": { "source": "indicator", "indicator": "range", "timeframe": "h4" },
                    "rhs": { "source": "constant", "value": "0.5" }
                }
            }),
        ),
        tool_turn(
            "c4",
            "add_filter",
            json!({
                "left": { "source": "price", "price_field": "close", "timeframe": "h4" },
                "op": "gt",
                "right": { "source": "indicator", "indicator": "trendline_highs", "left": 2, "right": 1, "timeframe": "h4" }
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

/// (i) The journey composes the worked example: every construct lands as
/// composed (a `Resistance` behind a `Lag` operand, the H4 anatomy comparison,
/// the H4 trendline break), `finalize` succeeds, and the stored document is
/// **exactly** the checked-in example's document — compared as VALUES.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sr_breakout_journey_composes_the_worked_example_with_no_substitution() {
    let (outcome, _advertised) = run_scripted(sr_breakout_journey_script()).await;
    let version = &outcome.version;

    // The entry: h4 close crosses above lag(h4 resistance{2/1, 1%, 2 touches, 150 bars}, 1).
    let Condition::CrossesAbove { lhs, rhs } = &version.dsl.entry else {
        panic!("the entry is a CrossesAbove, got {:?}", version.dsl.entry);
    };
    assert_eq!(
        lhs,
        &ValueSource::Price {
            series: Series::Htf,
            field: PriceField::Close,
        },
        "the entry's left operand is the H4 close"
    );
    let ValueSource::Lag { value, bars } = rhs else {
        panic!("the entry's right operand is a Lag, got {rhs:?}");
    };
    assert_eq!(*bars, 1, "the lag reads the previous H4 bar");
    assert_eq!(
        value.as_ref(),
        &ValueSource::Indicator {
            series: Series::Htf,
            spec: IndicatorSpec::Resistance {
                left: SweepableValue::Fixed(2),
                right: SweepableValue::Fixed(1),
                tolerance_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
                min_touches: SweepableValue::Fixed(2),
                lookback: SweepableValue::Fixed(150),
            },
        },
        "the lagged value is the H4 resistance built from swing highs"
    );

    // Filter 1: h4:body > (h4:range × 0.5).
    assert_eq!(
        &version.dsl.filters[0],
        &Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Htf,
                spec: IndicatorSpec::Body,
            },
            op: Comparator::Gt,
            rhs: ValueSource::Arith {
                op: ArithOp::Mul,
                lhs: Box::new(ValueSource::Indicator {
                    series: Series::Htf,
                    spec: IndicatorSpec::Range,
                }),
                rhs: Box::new(ValueSource::Constant {
                    value: Decimal::new(5, 1),
                }),
            },
        },
        "filter 0 is the H4 anatomy gate"
    );

    // Filter 2: h4:close > h4:trendline_highs(2/1).
    assert_eq!(
        &version.dsl.filters[1],
        &Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Htf,
                field: PriceField::Close,
            },
            op: Comparator::Gt,
            rhs: ValueSource::Indicator {
                series: Series::Htf,
                spec: IndicatorSpec::TrendlineHighs {
                    left: SweepableValue::Fixed(2),
                    right: SweepableValue::Fixed(1),
                },
            },
        },
        "filter 1 is the H4 trendline-highs break"
    );

    // Exits: the percent stop with the 1R take-profit; risk at 1% / 3×.
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
        "the exit pair is the stop and its 1R take-profit"
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
    assert_eq!(version.dsl_schema_version.to_string(), "1.3.0");

    // The rendered entry line shows the zone-break form in trader language.
    let rendered = render::strategy(&version.dsl);
    assert_eq!(
        rendered.entry,
        "h4:close crosses above lag(h4:resistance(pivots 2/1, 2+ touches within 1%, 150 bars), 1)",
        "the entry renders as the previous H4 bar's level, never the same bar's"
    );

    // `finalize` succeeded: the run's last event is the composer's own marker.
    assert!(matches!(
        outcome.events.last(),
        Some(ComposerEvent::Finalized { .. })
    ));
}

/// The public composer default names every schema-1.3.0 structure/pattern token
/// in double quotes, so an agent reading it can write the variants.
#[test]
fn composer_prompt_names_every_structure_and_pattern_token() {
    for token in VOCABULARY {
        assert!(
            COMPOSER_PROMPT.contains(&format!("\"{token}\"")),
            "composer.md must name the {token} token in double quotes"
        );
    }
    // The zone-break rule the trap demands: the prompt states the lag form and
    // says the same-bar comparison can never hold.
    assert!(
        COMPOSER_PROMPT.contains("lag") && COMPOSER_PROMPT.contains("can never hold"),
        "composer.md must state the zone-break lag form"
    );
}
