//! Offline end-to-end coverage for the 1.2.0-era builder vocabulary expansion
//! (r3.s2.w5, Q5 option 1): arithmetic (`arith`) and lag (`lag`) operands, the
//! rolling extremes (`highest` / `lowest`), the daily series (`d1`), and the
//! slope conditions (`rising` / `falling`) — composed through the REAL
//! composer and the REAL builder tools over a REAL `tempfile` `SQLite` repo,
//! driven by the scripted fake provider (no network, no live LLM,
//! MASTER-SPEC §9.4).
//!
//! Proves a described d47-style target ("breakout above the 20-bar high,
//! volatility-normalised, H4 trend slope, daily confirmation") is COMPOSED —
//! never silently substituted — and that the DSL's own expression rules
//! (nesting depth, rising/falling over a lag) refuse at `finalize` with errors
//! located at the operand's path while the tool layer adds no second copy of
//! them. Also pins the composer's assistant-text stream (Q5 option 1):
//! provider prose crosses the scrub-then-bound seam BEFORE it becomes a
//! stored/streamed event, on the CLI outcome AND on the desktop bus.
//! Offline (in-process `MIGRATOR` + committed `.sqlx/`), `TempDir`-isolated.
// The journey tests are deliberately LONG one-observable functions: scripted
// turns, structural document asserts, render asserts and the migrator
// round-trip for ONE run, so a failure names the whole journey. Splitting
// them would scatter the single story the AC asks to see.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use pulse::{
    ASSISTANT_TEXT_MAX_BYTES, ArithOp, BusError, BusEvent, BusEventPayload, Comparator,
    ComposeCliOutcome, ComposeDeps, ComposeWiring, ComposerEvent, Condition, CredentialSource, Db,
    EventSink, ExitRule, FakeClock, IndicatorSpec, LlmBackend, LlmConfig, LlmError, LlmProvider,
    LlmResponse, MIGRATOR, MacdOutput, Message, Migrator, ModelPrice, PriceField, PriceTable,
    Redactor, RunId, SchemaVersion, Series, SqliteLlmCallRepo, SqliteStrategyRepo, SweepableValue,
    TokenUsage, ToolCall, ToolDefinition, ValueSource, compose_strategy_core, render,
    run_compose_with,
};
use rust_decimal::Decimal;
use serde_json::json;
use tempfile::TempDir;

/// A secret the redactor KNOWS about (`from_config`) but whose shape is not a
/// key shape — so surviving in an emitted text would prove the exact-match tag
/// was NOT applied, and its placeholder appearing proves it was.
const TAGGED_SECRET: &str = "pulse-test-canary-secret-42";

/// The redaction placeholder the scrub-then-bound seam substitutes for a
/// tagged secret (`domain/redaction.rs`'s `REDACTED` — `pub(crate)`, so the
/// literal is pinned here rather than imported).
const REDACTED: &str = "«REDACTED»";

/// The seam's truncation marker (`DETAIL_TRUNCATED`, same visibility story).
const TRUNCATED: &str = "[truncated]";

/// A fresh, untripped cancellation latch — the CLI surface has no cancellation
/// channel, so these e2es exercise the uncancelled path.
fn never_cancelled() -> AtomicBool {
    AtomicBool::new(false)
}

/// A stand-in composer system prompt (the fake provider ignores it; the
/// composer only needs a non-empty framing string).
const TEST_PROMPT: &str = "You are PulseTrader's strategy composer. Build the \
    strategy only by calling builder tools; never emit raw DSL JSON.";

/// A scripted [`LlmProvider`] double: returns queued responses turn-by-turn and
/// records the tool definitions the composer advertised on the first call into
/// a shared slot (`tests/compose_vocabulary_1_1.rs`'s pattern — the provider is
/// the ONLY faked layer, and what it is actually SENT is the observable
/// surface, so the advertisement test asserts on the advertised list itself).
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

/// A scripted PROSE-ONLY turn — the shape Q5 option 1 streams as an
/// `AssistantText` event.
fn text_turn(content: &str) -> LlmResponse {
    LlmResponse {
        content: Some(content.to_owned()),
        tool_calls: Vec::new(),
        usage: usage(),
    }
}

/// A scripted turn that talks AND calls one tool — the prose must stream as an
/// `AssistantText` BEFORE the turn's tool events.
fn text_tool_turn(
    id: &str,
    content: &str,
    name: &str,
    arguments: serde_json::Value,
) -> LlmResponse {
    LlmResponse {
        content: Some(content.to_owned()),
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
/// tools and a REAL temp `SQLite` repo; returns the outcome plus the slot
/// holding the tool definitions the composer advertised on the first provider
/// turn (`tests/compose_vocabulary_1_1.rs`'s `run_scripted`, with the redactor
/// parameterized — the assistant-text tests need one that KNOWS the canary).
async fn run_scripted(
    script: Vec<LlmResponse>,
    redactor: Redactor,
) -> (ComposeCliOutcome, Arc<Mutex<Option<Vec<ToolDefinition>>>>) {
    let (_tmp, db) = migrated_db().await;
    let clock = FakeClock::at(1_700_000_000_000);
    let llm_repo = SqliteLlmCallRepo::with_deps(db.pool().clone(), clock);
    let strategy_repo = SqliteStrategyRepo::new(db.pool().clone());
    let advertised = Arc::new(Mutex::new(None));

    let wiring = ComposeWiring {
        provider: FakeComposerProvider::new(script, Arc::clone(&advertised)),
        llm_repo,
        redactor,
        prices: test_prices(),
        clock,
        prompt: TEST_PROMPT.to_owned(),
        // No resolved credential behind a fake provider — `None` is the honest
        // provenance label (same convention as compose_vocabulary_1_1.rs).
        key_source: None,
        config: config(),
    };

    let outcome = run_compose_with(
        wiring,
        &strategy_repo,
        "Breakout above the 20-bar high, volatility-normalised, with the H4 trend \
         rising and the daily trend confirmed",
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

/// The assistant-text events the composer streamed, in order.
fn assistant_texts(outcome: &ComposeCliOutcome) -> Vec<&str> {
    outcome
        .events
        .iter()
        .filter_map(|event| match event {
            ComposerEvent::AssistantText { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The d47 journey (spec case (i)): 20-bar-high breakout entry, an
/// `atr(14)/close` arithmetic volatility gate, an `h4:ema(200)` RISING slope
/// filter, a `d1:close > d1:ema(50)` daily confirmation, the ATR(14)×2 stop
/// with a 2R take-profit, default risk — every construct COMPOSED, none
/// substituted.
fn d47_journey_script() -> Vec<LlmResponse> {
    vec![
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "D47 Breakout", "direction": "long" }),
        ),
        tool_turn(
            "c2",
            "add_entry_signal",
            json!({
                "left": { "source": "price", "price_field": "close" },
                "op": "gt",
                "right": { "source": "indicator", "indicator": "highest", "period": 20, "price_field": "high" }
            }),
        ),
        tool_turn(
            "c3",
            "add_filter",
            json!({
                "left": {
                    "source": "arith", "op": "div",
                    "lhs": { "source": "indicator", "indicator": "atr", "period": 14 },
                    "rhs": { "source": "price", "price_field": "close" }
                },
                "op": "lt",
                "right": { "source": "constant", "value": "0.02" }
            }),
        ),
        tool_turn(
            "c4",
            "add_filter",
            json!({
                "left": { "source": "indicator", "indicator": "ema", "period": 200, "timeframe": "h4" },
                "op": "rising",
                "bars": 1
            }),
        ),
        tool_turn(
            "c5",
            "add_filter",
            json!({
                "left": { "source": "price", "price_field": "close", "timeframe": "d1" },
                "op": "gt",
                "right": { "source": "indicator", "indicator": "ema", "period": 50, "timeframe": "d1" }
            }),
        ),
        tool_turn(
            "c6",
            "set_exit_rules",
            json!({ "atr_stop_period": 14, "atr_stop_multiple": "2", "take_profit_r": "2" }),
        ),
        tool_turn(
            "c7",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ),
        tool_turn("c8", "finalize_strategy", json!({})),
    ]
}

/// (i) The d47 journey composes — the stored document carries EXACTLY the
/// requested constructs (compare the stored structure, not text), with no
/// `StopLoss` and no substitution, and it renders to the expected text through
/// `render::strategy`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d47_journey_composes_arithmetic_slopes_and_d1_with_no_substitution() {
    let (outcome, _advertised) = run_scripted(d47_journey_script(), Redactor::default()).await;
    let version = &outcome.version;

    // The entry: close > highest(high, 20 prior) — a Primary-series rolling
    // extreme, never a substituted plain high comparison.
    let Condition::Compare {
        lhs: entry_lhs,
        op: entry_op,
        rhs: entry_rhs,
    } = &version.dsl.entry
    else {
        panic!(
            "the entry is a Compare condition, got {:?}",
            version.dsl.entry
        );
    };
    assert_eq!(*entry_op, Comparator::Gt);
    assert_eq!(
        entry_lhs,
        &ValueSource::Price {
            series: Series::Primary,
            field: PriceField::Close,
        },
        "the entry's left operand is the primary close"
    );
    assert_eq!(
        entry_rhs,
        &ValueSource::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Highest {
                period: SweepableValue::Fixed(20),
                source: PriceField::High,
            },
        },
        "the entry's right operand is highest(high, 20 prior)"
    );

    // Filter 1: (atr(14) / close) < 0.02 — an Arith VALUE SOURCE inside a
    // plain comparison, not a sweep slot and not a substituted fixed threshold.
    let Condition::Compare {
        lhs: gate_lhs,
        op: gate_op,
        rhs: gate_rhs,
    } = &version.dsl.filters[0]
    else {
        panic!("filter 1 is a Compare, got {:?}", version.dsl.filters[0]);
    };
    assert_eq!(*gate_op, Comparator::Lt);
    assert_eq!(
        gate_lhs,
        &ValueSource::Arith {
            op: ArithOp::Div,
            lhs: Box::new(ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Atr {
                    period: SweepableValue::Fixed(14),
                },
            }),
            rhs: Box::new(ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            }),
        },
        "filter 1's left operand is the atr(14)/close arithmetic expression"
    );
    assert_eq!(
        gate_rhs,
        &ValueSource::Constant {
            value: "0.02".parse::<Decimal>().expect("0.02 parses")
        },
        "filter 1's right operand is the constant 0.02"
    );

    // Filter 2: h4:ema(200) rising (1 bar) — the slope condition on the HTF
    // series, bars as requested.
    assert_eq!(
        &version.dsl.filters[1],
        &Condition::Rising {
            value: Box::new(ValueSource::Indicator {
                series: Series::Htf,
                spec: IndicatorSpec::Ema {
                    period: SweepableValue::Fixed(200),
                },
            }),
            bars: 1,
        },
        "filter 2 is h4:ema(200) rising (1 bar)"
    );

    // Filter 3: d1:close > d1:ema(50) — BOTH operands carry the daily series.
    let Condition::Compare {
        lhs: d1_lhs,
        op: d1_op,
        rhs: d1_rhs,
    } = &version.dsl.filters[2]
    else {
        panic!("filter 3 is a Compare, got {:?}", version.dsl.filters[2]);
    };
    assert_eq!(*d1_op, Comparator::Gt);
    assert_eq!(
        d1_lhs,
        &ValueSource::Price {
            series: Series::D1,
            field: PriceField::Close,
        },
        "filter 3's left operand is the D1 close"
    );
    assert_eq!(
        d1_rhs,
        &ValueSource::Indicator {
            series: Series::D1,
            spec: IndicatorSpec::Ema {
                period: SweepableValue::Fixed(50),
            },
        },
        "filter 3's right operand is the D1 EMA(50)"
    );

    // Exits: the ATR stop composed beside the 2R take-profit; NO percent
    // StopLoss was substituted (the r2.s2.w3 no-substitution rule, extended to
    // this journey).
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
        "the 2R take-profit composes beside the ATR stop"
    );

    // The rendered TEXT matches what the trader described (render::strategy,
    // the same call the Library's summary wires build from).
    let rendered = render::strategy(&version.dsl);
    assert_eq!(rendered.direction, "long");
    assert_eq!(rendered.entry, "close > highest(high, 20 prior)");
    assert_eq!(
        rendered.filters,
        vec![
            "(atr(14) / close) < 0.02".to_owned(),
            "h4:ema(200) rising (1 bar)".to_owned(),
            "d1:close > d1:ema(50)".to_owned(),
        ],
        "the three filters render exactly as described"
    );
    assert!(
        rendered.exits.contains(&"atr(14)\u{d7}2".to_owned()),
        "the ATR stop renders as atr(14)×2: {:?}",
        rendered.exits
    );
    assert!(
        rendered.exits.contains(&"take profit 2R".to_owned()),
        "the take-profit renders as take profit 2R: {:?}",
        rendered.exits
    );

    // Schema pinning + the verbatim source round-trips through the production
    // migrator into the same typed document (already-current, no migration).
    assert_eq!(
        version.dsl_schema_version.to_string(),
        "1.2.0",
        "the persisted version is stamped schema 1.2.0"
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

/// (ii) The remaining new kinds compose: a MACD signal-output cross as the
/// entry, `lowest`, a `lag(close, 5)` filter, and a `falling` slope with
/// `bars: 3` — while the malformed slope shapes (`rising` WITH a `right`, a
/// comparison WITHOUT a `right`, `bars` on a non-slope op) are refused with
/// correctable `FieldError`s at the operand's path, and the run still
/// finalizes after the corrected call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn macd_output_lowest_lag_falling_compose_and_malformed_slope_shapes_are_refused() {
    let script = vec![
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "MACD Signal Flip", "direction": "long" }),
        ),
        tool_turn(
            "c2",
            "add_filter",
            json!({
                "left": { "source": "indicator", "indicator": "macd", "fast": 12, "slow": 26, "signal": 9, "output": "signal" },
                "op": "crosses_below",
                "right": { "source": "constant", "value": "0" }
            }),
        ),
        tool_turn(
            "c3",
            "add_filter",
            json!({
                "left": { "source": "price", "price_field": "close" },
                "op": "gt",
                "right": { "source": "indicator", "indicator": "lowest", "period": 10, "price_field": "low" }
            }),
        ),
        tool_turn(
            "c4",
            "add_filter",
            json!({
                "left": { "source": "lag", "of": { "source": "price", "price_field": "close" }, "bars": 5 },
                "op": "lt",
                "right": { "source": "price", "price_field": "close" }
            }),
        ),
        tool_turn(
            "c5",
            "add_filter",
            json!({
                "left": { "source": "indicator", "indicator": "rsi", "period": 14 },
                "op": "falling",
                "bars": 3
            }),
        ),
        // The three malformed shapes — each refused at its operand's path:
        tool_turn(
            "c6",
            "add_entry_signal",
            json!({
                "left": { "source": "indicator", "indicator": "rsi", "period": 14 },
                "op": "rising",
                "bars": 2,
                "right": { "source": "constant", "value": "30" }
            }),
        ),
        tool_turn(
            "c7",
            "add_entry_signal",
            json!({
                "left": { "source": "indicator", "indicator": "rsi", "period": 14 },
                "op": "gt"
            }),
        ),
        tool_turn(
            "c8",
            "add_filter",
            json!({
                "left": { "source": "price", "price_field": "close" },
                "op": "gt",
                "right": { "source": "constant", "value": "1" },
                "bars": 2
            }),
        ),
        // The correction: a plain `rising` (no right, bars defaulting to 1).
        tool_turn(
            "c9",
            "add_entry_signal",
            json!({
                "left": { "source": "indicator", "indicator": "rsi", "period": 14 },
                "op": "rising"
            }),
        ),
        tool_turn(
            "c10",
            "set_exit_rules",
            json!({ "stop_loss_pct": "0.01", "take_profit_r": "1.5" }),
        ),
        tool_turn(
            "c11",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ),
        tool_turn("c12", "finalize_strategy", json!({})),
    ];
    let (outcome, _advertised) = run_scripted(script, Redactor::default()).await;
    let version = &outcome.version;

    // Filter 0: macd(12,26,9).signal crosses below 0 — the output selector
    // landed on the stored spec. (A filter, not the entry: the entry slot is
    // singular and the script's correction below replaces it.)
    let Condition::CrossesBelow {
        lhs: macd_lhs,
        rhs: macd_rhs,
    } = &version.dsl.filters[0]
    else {
        panic!("filter 0 crosses below, got {:?}", version.dsl.filters[0]);
    };
    assert_eq!(
        macd_lhs,
        &ValueSource::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Macd {
                fast: SweepableValue::Fixed(12),
                slow: SweepableValue::Fixed(26),
                signal: SweepableValue::Fixed(9),
                output: MacdOutput::Signal,
            },
        },
        "the entry's left operand is the MACD SIGNAL output"
    );
    assert_eq!(
        macd_rhs,
        &ValueSource::Constant {
            value: Decimal::from(0)
        },
        "the entry crosses below the constant 0"
    );

    // Filter 1: close > lowest(low, 10 prior).
    let Condition::Compare {
        rhs: lowest_rhs, ..
    } = &version.dsl.filters[1]
    else {
        panic!("filter 1 is a Compare, got {:?}", version.dsl.filters[1]);
    };
    assert_eq!(
        lowest_rhs,
        &ValueSource::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Lowest {
                period: SweepableValue::Fixed(10),
                source: PriceField::Low,
            },
        },
        "filter 1's right operand is lowest(low, 10 prior)"
    );

    // Filter 2: lag(close, 5) < close — the lag composed as a VALUE SOURCE
    // (legal in a plain comparison; only rising/falling OVER a lag is
    // `InvalidExpression`).
    let Condition::Compare {
        lhs: lag_lhs,
        rhs: lag_rhs,
        ..
    } = &version.dsl.filters[2]
    else {
        panic!("filter 2 is a Compare, got {:?}", version.dsl.filters[2]);
    };
    assert_eq!(
        lag_lhs,
        &ValueSource::Lag {
            value: Box::new(ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            }),
            bars: 5,
        },
        "filter 2's left operand is lag(close, 5)"
    );
    assert_eq!(
        lag_rhs,
        &ValueSource::Price {
            series: Series::Primary,
            field: PriceField::Close,
        },
        "filter 2's right operand is the current close"
    );

    // Filter 3: rsi(14) falling (3 bars).
    assert_eq!(
        &version.dsl.filters[3],
        &Condition::Falling {
            value: Box::new(ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Rsi {
                    period: SweepableValue::Fixed(14),
                },
            }),
            bars: 3,
        },
        "filter 3 is rsi(14) falling (3 bars)"
    );

    // The corrected entry REPLACED the malformed attempts: a plain rising with
    // bars defaulting to 1.
    assert_eq!(
        &version.dsl.entry,
        &Condition::Rising {
            value: Box::new(ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Rsi {
                    period: SweepableValue::Fixed(14),
                },
            }),
            bars: 1,
        },
        "the final entry is rsi(14) rising (1 bar) — bars defaulted, no right"
    );

    // Each malformed shape was refused AT ITS PATH, correctably.
    let entry_refusals: Vec<&str> = tool_outcomes(&outcome, "add_entry_signal")
        .into_iter()
        .filter(|outcome| outcome.contains("\"path\""))
        .collect();
    assert!(
        entry_refusals
            .iter()
            .any(|outcome| outcome.contains("\"path\":\"right\"")
                && outcome.contains("rising")
                && outcome.contains("falling")),
        "a rising WITH a right operand is refused at `right`: {entry_refusals:?}"
    );
    assert!(
        entry_refusals
            .iter()
            .any(|outcome| outcome.contains("\"path\":\"right\"") && !outcome.contains("rising")),
        "a comparison WITHOUT a right operand is refused at `right`: {entry_refusals:?}"
    );
    let filter_refusals: Vec<&str> = tool_outcomes(&outcome, "add_filter")
        .into_iter()
        .filter(|outcome| outcome.contains("\"path\""))
        .collect();
    assert!(
        filter_refusals
            .iter()
            .any(|outcome| outcome.contains("\"path\":\"bars\"")),
        "`bars` on a non-slope op is refused at `bars`, never silently dropped: {filter_refusals:?}"
    );

    // The rendered text matches the described shapes.
    let rendered = render::strategy(&version.dsl);
    assert_eq!(rendered.entry, "rsi(14) rising (1 bar)");
    assert_eq!(
        rendered.filters,
        vec![
            "macd(12,26,9).signal crosses below 0".to_owned(),
            "close > lowest(low, 10 prior)".to_owned(),
            "lag(close, 5) < close".to_owned(),
            "rsi(14) falling (3 bars)".to_owned(),
        ],
        "the new kinds render exactly as the conventions write them"
    );

    assert!(matches!(
        outcome.events.last(),
        Some(ComposerEvent::Finalized { .. })
    ));
}

/// (iii) The DSL's OWN expression rules refuse at `finalize`, with the error at
/// the operand's path — the tool layer adds no second copy of them: a five-deep
/// `arith` chain is ACCEPTED by `add_entry_signal` and refused by
/// `finalize_strategy` with `FieldRange`; a `rising` over a `lag` is refused
/// with `InvalidExpression`. Both refusals correctable in-run (the entry slot
/// replaces), the run finalizes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dsl_only_expression_rules_are_refused_at_finalize_at_the_operand_path() {
    let script = vec![
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "Depth Probe", "direction": "long" }),
        ),
        tool_turn(
            "c1b",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ),
        tool_turn(
            "c1c",
            "set_exit_rules",
            json!({ "stop_loss_pct": "0.01", "take_profit_r": "1.5" }),
        ),
        // Offense 1: arith nested FIVE deep — the tool layer accepts it (it
        // maps shapes, it does not re-implement the DSL's depth rule).
        tool_turn(
            "c2",
            "add_entry_signal",
            json!({
                "left": {
                    "source": "arith", "op": "div",
                    "lhs": {
                        "source": "arith", "op": "div",
                        "lhs": {
                            "source": "arith", "op": "div",
                            "lhs": {
                                "source": "arith", "op": "div",
                                "lhs": {
                                    "source": "arith", "op": "div",
                                    "lhs": { "source": "price", "price_field": "close" },
                                    "rhs": { "source": "constant", "value": "2" }
                                },
                                "rhs": { "source": "constant", "value": "2" }
                            },
                            "rhs": { "source": "constant", "value": "2" }
                        },
                        "rhs": { "source": "constant", "value": "2" }
                    },
                    "rhs": { "source": "constant", "value": "2" }
                },
                "op": "lt",
                "right": { "source": "constant", "value": "0.02" }
            }),
        ),
        // Finalize #1: the DSL's depth rule refuses, pathed at the entry's
        // nested operand.
        tool_turn("c3", "finalize_strategy", json!({})),
        // Offense 2 (REPLACES the entry — the slot is singular): a rising
        // slope OVER a lag, the compiled form of a lag-of-a-lag.
        tool_turn(
            "c4",
            "add_entry_signal",
            json!({
                "left": {
                    "source": "lag",
                    "of": { "source": "price", "price_field": "close" },
                    "bars": 2
                },
                "op": "rising"
            }),
        ),
        // Finalize #2: the DSL's lag-over-lag rule refuses as InvalidExpression.
        tool_turn("c5", "finalize_strategy", json!({})),
        // The correction: a plain entry, and the run finalizes.
        tool_turn(
            "c6",
            "add_entry_signal",
            json!({
                "left": { "source": "price", "price_field": "close" },
                "op": "gt",
                "right": { "source": "indicator", "indicator": "highest", "period": 20, "price_field": "high" }
            }),
        ),
        tool_turn("c7", "finalize_strategy", json!({})),
    ];
    let (outcome, _advertised) = run_scripted(script, Redactor::default()).await;

    // Finalize #1 and #2 refused with the DSL's own codes, pathed at the
    // operand (a SUCCESSFUL finalize emits a `Finalized` event, not a
    // `ToolCallResult`, so the refusal count here is exactly two).
    let finalize_outcomes = tool_outcomes(&outcome, "finalize_strategy");
    assert_eq!(
        finalize_outcomes.len(),
        2,
        "finalize refused twice: two refusals, then the success"
    );
    assert!(
        finalize_outcomes[0].contains("FieldRange") && finalize_outcomes[0].contains(".arith"),
        "the five-deep arith is refused with FieldRange at the operand's path: {}",
        finalize_outcomes[0]
    );
    assert!(
        finalize_outcomes[1].contains("InvalidExpression"),
        "rising over a lag is refused with InvalidExpression: {}",
        finalize_outcomes[1]
    );

    // The corrected run finalized.
    assert!(matches!(
        outcome.events.last(),
        Some(ComposerEvent::Finalized { .. })
    ));
    assert_eq!(
        tool_outcomes(&outcome, "add_entry_signal").len(),
        3,
        "all three entry calls were ACCEPTED by the tool layer — the refusals are the DSL's, not duplicates of them"
    );
}

/// (iv) The composer's assistant text crosses the scrub-then-bound seam BEFORE
/// it becomes a streamed/stored event: one `AssistantText` per prose-bearing
/// provider turn, ordered BEFORE that turn's tool events; the tagged canary
/// secret is scrubbed out; a 10 000-byte prose turn is bounded to
/// `ASSISTANT_TEXT_MAX_BYTES` with the seam's truncation marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assistant_text_streams_scrubbed_and_bounded_before_each_turns_tool_events() {
    let redactor = Redactor::from_config(vec![TAGGED_SECRET.to_owned()]);
    let huge = format!("volatility context: {} end", "x".repeat(10_000));
    let script = vec![
        text_turn(&format!(
            "Building the breakout now; your canary {TAGGED_SECRET} stays private."
        )),
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "D47 Breakout", "direction": "long" }),
        ),
        tool_turn(
            "c2",
            "add_entry_signal",
            json!({
                "left": { "source": "price", "price_field": "close" },
                "op": "gt",
                "right": { "source": "indicator", "indicator": "highest", "period": 20, "price_field": "high" }
            }),
        ),
        text_tool_turn(
            "c3",
            &huge,
            "set_exit_rules",
            json!({ "atr_stop_period": 14, "atr_stop_multiple": "2" }),
        ),
        tool_turn(
            "c4",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ),
        tool_turn("c5", "finalize_strategy", json!({})),
    ];
    let (outcome, _advertised) = run_scripted(script, redactor).await;

    // Stream order: the first turn's prose is event 0, before any tool event;
    // the fourth turn's prose lands BEFORE the exit-rule turn's own events.
    let texts = assistant_texts(&outcome);
    assert_eq!(
        texts.len(),
        2,
        "one AssistantText per prose-bearing turn: {texts:?}"
    );
    assert_eq!(
        outcome.events.first(),
        Some(&ComposerEvent::AssistantText {
            text: texts[0].to_owned(),
        }),
        "the stream OPENS with the first turn's prose"
    );
    assert!(
        matches!(&outcome.events[1], ComposerEvent::ToolCallStarted { name, .. } if name == "create_strategy"),
        "the prose precedes its turn's tool events: {:?}",
        outcome.events[1]
    );

    // Scrubbed: the tagged secret is the placeholder, never the raw value.
    assert!(
        !texts[0].contains(TAGGED_SECRET),
        "the tagged secret must not survive into the emitted text: {:?}",
        texts[0]
    );
    assert!(
        texts[0].contains(REDACTED),
        "the secret's placeholder marks the scrub: {texts:?}"
    );
    assert!(
        texts[0].starts_with("Building the breakout"),
        "the prose itself survives the scrub: {texts:?}"
    );

    // Bounded: the 10 000-byte prose turn comes back within the byte ceiling,
    // ending in the seam's marker.
    assert!(huge.len() > ASSISTANT_TEXT_MAX_BYTES);
    assert!(
        texts[1].len() <= ASSISTANT_TEXT_MAX_BYTES,
        "the emitted text is bounded to ASSISTANT_TEXT_MAX_BYTES ({}): {} bytes",
        ASSISTANT_TEXT_MAX_BYTES,
        texts[1].len()
    );
    assert!(
        texts[1].ends_with(TRUNCATED),
        "a cut text ends in the seam's truncation marker: …{}",
        &texts[1][texts[1].len() - 24..]
    );

    // No prose turn — the tool-only turns around them emitted none, and the
    // run still finalized.
    assert!(matches!(
        outcome.events.last(),
        Some(ComposerEvent::Finalized { .. })
    ));
}

/// The desktop bus carries the same frames: `compose_strategy_core` with an
/// in-memory sink sees `assistantText` in stream order between `started` and
/// the turn's tool events, `finished` last.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assistant_text_reaches_the_bus_through_the_desktop_core() {
    struct Collector {
        events: Mutex<Vec<BusEvent>>,
    }
    impl EventSink for Collector {
        fn send_event(&self, event: BusEvent) -> Result<(), BusError> {
            self.events.lock().expect("events lock").push(event);
            Ok(())
        }
    }

    let (_tmp, db) = migrated_db().await;
    let clock = FakeClock::at(1_700_000_000_000);
    let advertised = Arc::new(Mutex::new(None));
    let deps = ComposeDeps {
        wiring: ComposeWiring {
            provider: FakeComposerProvider::new(
                vec![
                    text_turn("Composing the breakout — narrating each step."),
                    tool_turn(
                        "c1",
                        "create_strategy",
                        json!({ "name": "Bus Breakout", "direction": "long" }),
                    ),
                    tool_turn(
                        "c1b",
                        "set_risk_params",
                        json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
                    ),
                    tool_turn(
                        "c1c",
                        "set_exit_rules",
                        json!({ "stop_loss_pct": "0.01", "take_profit_r": "1.5" }),
                    ),
                    tool_turn(
                        "c1d",
                        "add_entry_signal",
                        json!({
                            "left": { "source": "price", "price_field": "close" },
                            "op": "gt",
                            "right": { "source": "indicator", "indicator": "highest", "period": 20, "price_field": "high" }
                        }),
                    ),
                    tool_turn("c2", "finalize_strategy", json!({})),
                ],
                Arc::clone(&advertised),
            ),
            llm_repo: SqliteLlmCallRepo::with_deps(db.pool().clone(), clock),
            redactor: Redactor::default(),
            prices: test_prices(),
            clock,
            prompt: TEST_PROMPT.to_owned(),
            key_source: Some(CredentialSource::ConfigDir),
            config: config(),
        },
        strategy_repo: SqliteStrategyRepo::new(db.pool().clone()),
    };

    let sink = Collector {
        events: Mutex::new(Vec::new()),
    };
    let run_id = RunId::new();
    let outcome = compose_strategy_core(&run_id, deps, "breakout, narrated", &sink, {
        Arc::new(AtomicBool::new(false))
    })
    .await
    .expect("the scripted sequence streams and finalizes");
    assert!(!outcome.cancelled);

    let events = sink.events.lock().expect("events lock").clone();
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|event| match &event.payload {
            BusEventPayload::Started => Some("started"),
            BusEventPayload::AssistantText { .. } => Some("assistantText"),
            BusEventPayload::ToolCallStarted { .. } => Some("toolCallStarted"),
            BusEventPayload::ToolCallResult { .. } => Some("toolCallResult"),
            BusEventPayload::Finished { .. } => Some("finished"),
            // Compose streams never emit Progress — excluded rather than named.
            BusEventPayload::Progress { .. } => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "started",
            "assistantText",
            "toolCallStarted",
            "toolCallResult",
            "toolCallStarted",
            "toolCallResult",
            "toolCallStarted",
            "toolCallResult",
            "toolCallStarted",
            "toolCallResult",
            "toolCallStarted",
            "finished",
        ],
        "the bus frames arrive in stream order, prose before its turn's tools"
    );
    let BusEventPayload::AssistantText { text } = &events[1].payload else {
        panic!("event 1 is the assistant text, got {:?}", events[1].payload);
    };
    assert_eq!(text, "Composing the breakout — narrating each step.");
}

/// The `$ref` scan over one advertised parameters tree: no `$ref` key may
/// appear at ANY depth — the schemas are unrolled inline (r3.s2.w5).
fn assert_no_refs(value: &serde_json::Value, where_: &str) {
    match value {
        serde_json::Value::Object(map) => {
            assert!(
                !map.contains_key("$ref"),
                "{where_} carries a $ref — the schemas are unrolled inline"
            );
            for (key, child) in map {
                assert_no_refs(child, &format!("{where_}.{key}"));
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                assert_no_refs(child, &format!("{where_}[{index}]"));
            }
        }
        _ => {}
    }
}

/// (v) The ADVERTISED tool definitions carry the expanded vocabulary: six
/// tools exactly; `arith` and `lag` in the operand `source` enum; `highest` /
/// `lowest` in the indicator enum; the `d1` timeframe token; the slope ops and
/// `bars` in the signal schema with `right` no longer required — and NO `$ref`
/// anywhere: the operand schema is unrolled two levels, the last level's
/// nested operand a bare `{"type": "object"}` placeholder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advertised_tool_definitions_carry_the_expanded_vocabulary_without_refs() {
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
    let (_outcome, advertised) = run_scripted(script, Redactor::default()).await;
    let tools = advertised
        .lock()
        .expect("advertised lock")
        .clone()
        .expect("the composer advertised tools on the first turn");

    // Six tools, exactly — no seventh tool was added for the new kinds.
    let mut names: Vec<&str> = tools.iter().map(|d| d.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "add_entry_signal",
            "add_filter",
            "create_strategy",
            "finalize_strategy",
            "set_exit_rules",
            "set_risk_params",
        ],
        "the vocabulary grows INSIDE the six tools"
    );

    let def = |name: &str| -> &ToolDefinition {
        tools
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("{name} is advertised"))
    };

    for signal_tool in ["add_entry_signal", "add_filter"] {
        for side in ["left", "right"] {
            let operand = &def(signal_tool).parameters["properties"][side];
            let props = &operand["properties"];
            assert_eq!(
                props["source"]["enum"],
                json!(["indicator", "price", "constant", "arith", "lag"]),
                "{signal_tool}.{side} advertises the full source enum"
            );
            let indicators = props["indicator"]["enum"]
                .as_array()
                .expect("indicator enum is an array");
            for kind in ["rsi", "ema", "adx", "macd", "atr", "highest", "lowest"] {
                assert!(
                    indicators.iter().any(|v| v == kind),
                    "{signal_tool}.{side} advertises {kind} in the indicator enum"
                );
            }
            assert_eq!(
                props["timeframe"]["enum"],
                json!(["primary", "h4", "d1"]),
                "{signal_tool}.{side} advertises timeframe primary|h4|d1"
            );
            assert_eq!(
                props["op"]["enum"],
                json!(["add", "sub", "mul", "div"]),
                "{signal_tool}.{side} advertises the arith op enum"
            );
            assert!(
                props.get("bars").is_some(),
                "{signal_tool}.{side} advertises bars (the lag window)"
            );
            // Two-level unroll: the OUTER level's lhs is the INNER (detailed)
            // level; the LAST level's nested operand is a bare object — no
            // third level of detail, no `$ref`.
            let outer_lhs = &props["lhs"];
            assert!(
                outer_lhs.get("properties").is_some()
                    && outer_lhs["properties"]["source"]["enum"]
                        .as_array()
                        .expect("inner level carries the source enum")
                        .len()
                        == 5,
                "{signal_tool}.{side}'s nested operand is the detailed inner level"
            );
            let nested = &outer_lhs["properties"]["lhs"];
            assert_eq!(
                nested,
                &json!({ "type": "object", "description": "an operand (same shape)" }),
                "{signal_tool}.{side}'s last-level nested operand is the bare placeholder"
            );
            assert!(
                nested.get("properties").is_none(),
                "{signal_tool}.{side}'s nested operand unrolls no further"
            );
        }

        // The signal schema: slope ops + bars, right no longer required.
        let signal = &def(signal_tool).parameters;
        let ops = signal["properties"]["op"]["enum"]
            .as_array()
            .expect("op enum is an array");
        for op in [
            "gt",
            "gte",
            "lt",
            "lte",
            "eq",
            "crosses_above",
            "crosses_below",
            "rising",
            "falling",
        ] {
            assert!(
                ops.iter().any(|v| v == op),
                "{signal_tool} advertises {op} in the op enum"
            );
        }
        assert!(
            signal["properties"].get("bars").is_some(),
            "{signal_tool} advertises bars (the slope window)"
        );
        let required = signal["required"].as_array().expect("required is a list");
        assert!(
            !required.iter().any(|v| v == "right"),
            "{signal_tool} no longer requires `right` (rising/falling omit it)"
        );
        assert!(
            required.iter().any(|v| v == "left") && required.iter().any(|v| v == "op"),
            "{signal_tool} still requires `left` and `op`: {required:?}"
        );
    }

    // NO `$ref` anywhere in the advertised surface.
    for tool in &tools {
        assert_no_refs(&tool.parameters, &tool.name);
    }
}
