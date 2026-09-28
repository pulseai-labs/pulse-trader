//! r3.s1.w2 — AC-1: conditions mean what they say (spine demo line d43).
//!
//! Seven cases over the impossibility rule (G2), the #36 warm fix and the #16
//! decision (spine ruling a1):
//!
//!   i.  An impossible entry is refused at submit over the MCP application
//!       function with one field-pathed `ImpossibleCondition` error and nothing
//!       persisted.
//!   ii. Refused in compose — the builder's correctable error, same path.
//!   iii. The impossibility table, both ways, with And/Or propagation.
//!   iv. #16: a `Not(Compare)` entry cannot fire before the indicator warms.
//!   v.  #36: a `Not(CrossesAbove)` entry cannot fire on the first warm bar.
//!   vi. Determinism: two cold runs of a (iv) version persist identical results.
//!   vii. A persisted impossible version still lists but is refused at run.
//!
//! Offline throughout: the committed 1-month BTCUSDT fixture store, a scripted
//! fake compose provider, tempfile SQLite with the in-process `MIGRATOR`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};

use pulse::{
    BacktestAppError, BacktestConfig, BacktestRequest, BinanceAdapter, CandleSeries, CandleStore,
    Comparator, CompiledValue, ComposeCliOutcome, ComposeWiring, ComposerEvent, Condition,
    CreatedBy, Db, Direction, EvalContext, ExchangeAdapter, ExitRule, FakeClock, IndicatorEngine,
    IndicatorSpec, LlmBackend, LlmConfig, LlmError, LlmProvider, LlmResponse, Message, ModelPrice,
    NewVersion, Pair, PriceField, PriceTable, Redactor, RiskParams, SchemaVersion, Series,
    SeriesEnd, SqliteBacktestRunRepo, SqliteLlmCallRepo, SqliteStrategyRepo, StrategyDsl,
    StrategyId, StrategyRepository, SubmitError, SubmitRequest, SubmitTarget, SweepableValue,
    SymbolFilters, Timeframe, TokenUsage, ToolCall, ToolDefinition, Trade, ValidationCode,
    ValueSource, VersionId, compile, run_backtest, run_compose_with, run_version_backtest,
    submit_agent_version, validate, version_hash,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use support::mcp::{FIXTURE_STORE, copy_tree, manifest, migrated_db};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Shared DSL builders (Rust-side documents for the table and the engine cases)
// ---------------------------------------------------------------------------

fn dec(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

fn fixed(mantissa: i64, scale: u32) -> SweepableValue<Decimal> {
    SweepableValue::Fixed(dec(mantissa, scale))
}

fn price(series: Series, field: PriceField) -> ValueSource {
    ValueSource::Price { series, field }
}

fn ind_rsi(period: u32) -> ValueSource {
    ValueSource::Indicator {
        series: Series::Primary,
        spec: ind_rsi_spec(period),
    }
}

fn ind_rsi_spec(period: u32) -> IndicatorSpec {
    IndicatorSpec::Rsi {
        period: SweepableValue::Fixed(period),
    }
}

fn ind_ema_spec(period: u32) -> IndicatorSpec {
    IndicatorSpec::Ema {
        period: SweepableValue::Fixed(period),
    }
}

fn ind_ema(period: u32) -> ValueSource {
    ValueSource::Indicator {
        series: Series::Primary,
        spec: ind_ema_spec(period),
    }
}

fn constant(value: i64) -> ValueSource {
    ValueSource::Constant {
        value: dec(value, 0),
    }
}

fn cmp(lhs: ValueSource, op: Comparator, rhs: ValueSource) -> Condition {
    Condition::Compare { lhs, op, rhs }
}

fn cross_above(lhs: ValueSource, rhs: ValueSource) -> Condition {
    Condition::CrossesAbove { lhs, rhs }
}

fn cross_below(lhs: ValueSource, rhs: ValueSource) -> Condition {
    Condition::CrossesBelow { lhs, rhs }
}

fn and(conditions: Vec<Condition>) -> Condition {
    Condition::And { conditions }
}

fn or(conditions: Vec<Condition>) -> Condition {
    Condition::Or { conditions }
}

fn not(condition: Condition) -> Condition {
    Condition::Not {
        condition: Box::new(condition),
    }
}

fn close() -> ValueSource {
    price(Series::Primary, PriceField::Close)
}

fn high() -> ValueSource {
    price(Series::Primary, PriceField::High)
}

fn low() -> ValueSource {
    price(Series::Primary, PriceField::Low)
}

/// A valid document differing only in the entry — the shape every refusal
/// case rides on (stop + take-profit exits, 1% risk, 3x leverage).
fn dsl_with_entry(entry: Condition) -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "Impossible Entry".to_owned(),
        direction: Direction::Long,
        entry,
        filters: vec![],
        exits: vec![
            ExitRule::StopLoss {
                distance_pct: fixed(5, 2),
            },
            ExitRule::TakeProfit {
                target_r: fixed(2, 0),
            },
        ],
        risk: RiskParams {
            risk_per_trade_pct: fixed(1, 2),
            max_leverage: fixed(3, 0),
        },
    }
}

/// The same document with filters set.
fn dsl_with_filters(entry: Condition, filters: Vec<Condition>) -> StrategyDsl {
    let mut dsl = dsl_with_entry(entry);
    dsl.filters = filters;
    dsl
}

/// The same document with exits set.
fn dsl_with_exits(entry: Condition, exits: Vec<ExitRule>) -> StrategyDsl {
    let mut dsl = dsl_with_entry(entry);
    dsl.exits = exits;
    dsl
}

/// Pull the `ImpossibleCondition` errors out of a validation failure.
fn impossible_errors(dsl: &StrategyDsl) -> Vec<pulse::FieldError> {
    validate(dsl)
        .expect_err("the document is refused")
        .into_errors()
        .into_iter()
        .filter(|e| e.code == ValidationCode::ImpossibleCondition)
        .collect()
}

// ---------------------------------------------------------------------------
// (iii) the impossibility table, both ways, with propagation
// ---------------------------------------------------------------------------

#[test]
fn impossible_rows_are_refused_with_one_error_at_the_leaf_path() {
    let cases: Vec<(Condition, &str)> = vec![
        // Same-series OHLC-invariant refusals, Gt direction: each pair below is
        // provably ordered (low ≤ open ≤ high, low ≤ close ≤ high), so the
        // strict `>` can never hold.
        (cmp(close(), Comparator::Gt, high()), "entry"),
        (
            cmp(
                price(Series::Primary, PriceField::Open),
                Comparator::Gt,
                high(),
            ),
            "entry",
        ),
        (
            cmp(
                price(Series::Primary, PriceField::Low),
                Comparator::Gt,
                high(),
            ),
            "entry",
        ),
        (
            cmp(
                price(Series::Primary, PriceField::Low),
                Comparator::Gt,
                price(Series::Primary, PriceField::Open),
            ),
            "entry",
        ),
        (
            cmp(
                price(Series::Primary, PriceField::Low),
                Comparator::Gt,
                close(),
            ),
            "entry",
        ),
        // Same-series OHLC-invariant refusals, Lt direction: `a < b` can never
        // hold when the invariant pins a ≥ b on every bar.
        (cmp(close(), Comparator::Lt, low()), "entry"),
        (
            cmp(
                price(Series::Primary, PriceField::Open),
                Comparator::Lt,
                low(),
            ),
            "entry",
        ),
        (
            cmp(
                price(Series::Primary, PriceField::High),
                Comparator::Lt,
                low(),
            ),
            "entry",
        ),
        (
            cmp(
                price(Series::Primary, PriceField::High),
                Comparator::Lt,
                price(Series::Primary, PriceField::Open),
            ),
            "entry",
        ),
        (
            cmp(
                price(Series::Primary, PriceField::High),
                Comparator::Lt,
                close(),
            ),
            "entry",
        ),
        // Same-operand strict comparisons — Price and Indicator alike.
        (cmp(close(), Comparator::Gt, close()), "entry"),
        (cmp(ind_rsi(14), Comparator::Lt, ind_rsi(14)), "entry"),
        // Both-Constant comparisons that are false.
        (cmp(constant(1), Comparator::Gt, constant(2)), "entry"),
        (cmp(constant(2), Comparator::Lt, constant(1)), "entry"),
        (cmp(constant(1), Comparator::Gte, constant(2)), "entry"),
        (cmp(constant(2), Comparator::Lte, constant(1)), "entry"),
        (cmp(constant(1), Comparator::Eq, constant(2)), "entry"),
        // Crosses the invariant rules out, plus same-operand crosses.
        (cross_above(close(), high()), "entry.crosses_above"),
        (
            cross_above(price(Series::Primary, PriceField::Low), high()),
            "entry.crosses_above",
        ),
        (cross_below(close(), low()), "entry.crosses_below"),
        (cross_above(close(), close()), "entry.crosses_above"),
        (cross_below(low(), low()), "entry.crosses_below"),
    ];

    for (entry, expected_path) in cases {
        let errors = impossible_errors(&dsl_with_entry(entry));
        assert_eq!(
            errors.len(),
            1,
            "exactly one ImpossibleCondition error for the {expected_path} case, got {errors:?}"
        );
        assert_eq!(
            errors[0].path, expected_path,
            "the error is pathed at the leaf"
        );
    }
}

#[test]
fn the_close_above_high_message_speaks_trader_terms() {
    let errors = impossible_errors(&dsl_with_entry(cmp(close(), Comparator::Gt, high())));
    assert_eq!(
        errors[0].message,
        "close can never be above high on the same bar"
    );
}

#[test]
fn the_accept_list_stays_accepted() {
    let htf_high = price(Series::Htf, PriceField::High);
    let accepted: Vec<Condition> = vec![
        // The spec's accept list.
        cmp(
            close(),
            Comparator::Gt,
            price(Series::Primary, PriceField::Open),
        ),
        cmp(high(), Comparator::Gte, low()),
        cmp(high(), Comparator::Eq, low()),
        cmp(close(), Comparator::Gt, htf_high),
        not(cmp(close(), Comparator::Gt, high())),
        or(vec![
            cmp(close(), Comparator::Gt, high()),
            cmp(ind_rsi(14), Comparator::Lt, constant(30)),
        ]),
        cmp(
            price(Series::Primary, PriceField::Volume),
            Comparator::Gt,
            high(),
        ),
        // Not-judged corners: true constant comparisons; non-strict price pairs
        // a flat bar makes possible; a Not branch inside an Or; price pairs no
        // invariant orders (open == low and close == low on a flat bar).
        cmp(constant(2), Comparator::Gt, constant(1)),
        cmp(constant(2), Comparator::Gte, constant(1)),
        cmp(constant(1), Comparator::Lte, constant(2)),
        cmp(constant(1), Comparator::Eq, constant(1)),
        cmp(
            price(Series::Primary, PriceField::Open),
            Comparator::Gt,
            low(),
        ),
        cmp(close(), Comparator::Gt, low()),
        or(vec![
            cmp(close(), Comparator::Gt, high()),
            not(cmp(close(), Comparator::Gt, high())),
        ]),
        not(cross_above(close(), high())),
        cross_above(ind_rsi(5), ind_rsi(20)),
    ];

    for entry in accepted {
        let validated = validate(&dsl_with_entry(entry)).expect("the document is accepted");
        assert_eq!(validated.dsl().name, "Impossible Entry");
    }
}

#[test]
fn and_propagation_reports_at_the_impossible_child() {
    // And(rsi < 30, close > high) — refused at the And's second child.
    let entry = and(vec![
        cmp(ind_rsi(14), Comparator::Lt, constant(30)),
        cmp(close(), Comparator::Gt, high()),
    ]);
    let errors = impossible_errors(&dsl_with_entry(entry));
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].path, "entry.and[1]");

    // A nested And surfaces the deepest impossible child's path.
    let nested = and(vec![
        cmp(
            price(Series::Primary, PriceField::Volume),
            Comparator::Gt,
            constant(0),
        ),
        and(vec![
            cmp(close(), Comparator::Gt, high()),
            cmp(ind_rsi(14), Comparator::Lt, constant(30)),
        ]),
    ]);
    let errors = impossible_errors(&dsl_with_entry(nested));
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].path, "entry.and[1].and[0]");
}

#[test]
fn or_refuses_only_when_every_branch_is_impossible_and_reports_once_at_the_or() {
    // Or(close > high, low > high) — both branches impossible: refused at the Or.
    let entry = or(vec![
        cmp(close(), Comparator::Gt, high()),
        cmp(low(), Comparator::Gt, high()),
    ]);
    let errors = impossible_errors(&dsl_with_entry(entry));
    assert_eq!(errors.len(), 1, "one error at the Or, never one per branch");
    assert_eq!(errors[0].path, "entry.or");
    assert!(
        errors[0].message.contains("every branch"),
        "the message names that every branch is impossible: {}",
        errors[0].message
    );

    // An Or nested in an And reports at the Or's own node.
    let nested = and(vec![
        cmp(ind_rsi(14), Comparator::Lt, constant(30)),
        or(vec![
            cmp(close(), Comparator::Gt, high()),
            cmp(low(), Comparator::Gt, high()),
        ]),
    ]);
    let errors = impossible_errors(&dsl_with_entry(nested));
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].path, "entry.and[1].or");

    // An Or nested in an Or: the inner Or's error is suppressed; the outer
    // Or reports once.
    let outer = or(vec![
        cmp(close(), Comparator::Gt, high()),
        or(vec![
            cmp(low(), Comparator::Gt, high()),
            cmp(
                price(Series::Primary, PriceField::Open),
                Comparator::Gt,
                high(),
            ),
        ]),
    ]);
    let errors = impossible_errors(&dsl_with_entry(outer));
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].path, "entry.or");
}

#[test]
fn filters_and_signal_exits_are_judged() {
    let filter_errors = impossible_errors(&dsl_with_filters(
        cmp(ind_rsi(14), Comparator::Lt, constant(30)),
        vec![cmp(close(), Comparator::Gt, high())],
    ));
    assert_eq!(filter_errors.len(), 1);
    assert_eq!(filter_errors[0].path, "filters[0]");

    let exit_errors = impossible_errors(&dsl_with_exits(
        cmp(ind_rsi(14), Comparator::Lt, constant(30)),
        vec![ExitRule::SignalExit {
            condition: cmp(close(), Comparator::Gt, high()),
        }],
    ));
    assert_eq!(exit_errors.len(), 1);
    assert_eq!(exit_errors[0].path, "exits[0].condition");
}

#[test]
fn degenerate_cross_is_not_double_reported() {
    for (entry, expected_path) in [
        (cross_above(constant(1), constant(2)), "entry.crosses_above"),
        (cross_below(constant(2), constant(1)), "entry.crosses_below"),
    ] {
        let errors = validate(&dsl_with_entry(entry))
            .expect_err("the both-Constant cross is refused")
            .into_errors();
        assert_eq!(errors.len(), 1, "exactly one error: {errors:?}");
        assert_eq!(errors[0].code, ValidationCode::DegenerateCross);
        assert_eq!(errors[0].path, expected_path);
    }
}

// ---------------------------------------------------------------------------
// JSON documents (the submit case) and row counts
// ---------------------------------------------------------------------------

fn price_json(field: &str) -> Value {
    json!({ "type": "Price", "series": "primary", "field": field })
}

fn dsl_json_with_entry(entry: &Value) -> Value {
    json!({
        "schema_version": "1.0.0",
        "name": "Impossible Entry",
        "direction": "long",
        "entry": entry,
        "filters": [],
        "exits": [
            { "type": "StopLoss", "distance_pct": "0.05" },
            { "type": "TakeProfit", "target_r": "2.0" }
        ],
        "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
    })
}

fn crosses_above_close_high_json() -> Value {
    json!({ "type": "CrossesAbove", "lhs": price_json("Close"), "rhs": price_json("High") })
}

fn compare_close_gt_high_json() -> Value {
    json!({ "type": "Compare", "lhs": price_json("Close"), "op": "Gt", "rhs": price_json("High") })
}

async fn row_count(pool: &sqlx::SqlitePool, table: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .expect("row count query")
}

// ---------------------------------------------------------------------------
// (i) refused at submit over the MCP application function, nothing persisted
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_refuses_impossible_entries_and_persists_nothing() {
    let tmp = TempDir::new().expect("tempdir");
    let (_db_path, db) = migrated_db(&tmp).await;
    let strategies = SqliteStrategyRepo::new(db.pool().clone());

    let cases = [
        (crosses_above_close_high_json(), "entry.crosses_above"),
        (compare_close_gt_high_json(), "entry"),
    ];

    for (entry, expected_path) in cases {
        let request = SubmitRequest {
            target: SubmitTarget::Root {
                strategy_name: "Impossible Entry".to_owned(),
            },
            dsl: dsl_json_with_entry(&entry),
            hypothesis: "an entry that fires on every close-above-high bar".to_owned(),
            agent_name: "Test-Agent".to_owned(),
        };
        let error = submit_agent_version(&strategies, request)
            .await
            .expect_err("the impossible entry is refused at submit");

        match error {
            SubmitError::Validation(errors) => {
                let errors = errors.into_errors();
                assert_eq!(
                    errors.len(),
                    1,
                    "exactly one FieldError for the {expected_path} case, got {errors:?}"
                );
                assert_eq!(errors[0].code, ValidationCode::ImpossibleCondition);
                assert_eq!(errors[0].path, expected_path);
            }
            other => panic!("expected SubmitError::Validation, got {other:?}"),
        }
    }

    // Nothing persisted: no strategy, no version, no submission.
    for table in ["strategy", "strategy_version", "agent_submission"] {
        let rows = row_count(db.pool(), table).await;
        assert_eq!(rows, 0, "{table} must stay empty after refused submits");
    }
}

// ---------------------------------------------------------------------------
// (ii) refused in compose — the builder's correctable error
// ---------------------------------------------------------------------------

/// A stand-in composer system prompt (the fake provider ignores it).
const TEST_PROMPT: &str = "compose the described strategy with the builder tools";

/// A known per-turn token usage (so each persisted `LlmCall` cost is non-zero).
fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 120,
        output_tokens: 48,
    }
}

/// A scripted single-tool-call turn.
fn tool_turn(id: &str, name: &str, arguments: Value) -> LlmResponse {
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

/// A scripted [`LlmProvider`] double: returns queued responses turn-by-turn and
/// records the tool definitions it was advertised on the first call. No
/// network, no keychain — the provider is the ONLY faked layer.
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

/// A TEST price table keyed on `gpt-oss:120b` so the decorator prices the
/// model. TEST values, not production moat data.
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

/// A latch nobody trips — this suite exercises the uncancelled path.
fn never_cancelled() -> std::sync::atomic::AtomicBool {
    std::sync::atomic::AtomicBool::new(false)
}

/// Run the scripted sequence through the REAL composer loop, the REAL builder
/// tools and a REAL temp SQLite repo; returns the loop's result plus every
/// streamed `ComposerEvent` (an Err ending in `NotFinalized` still streamed
/// each correctable tool result).
async fn run_scripted(
    script: Vec<LlmResponse>,
) -> (Result<ComposeCliOutcome, anyhow::Error>, Vec<ComposerEvent>) {
    let tmp = TempDir::new().expect("tempdir");
    let (_db_path, db) = migrated_db(&tmp).await;
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

    let mut streamed: Vec<ComposerEvent> = Vec::new();
    let result = run_compose_with(
        wiring,
        &strategy_repo,
        "enter when close crosses above high",
        &mut |event| {
            streamed.push(event);
        },
        &never_cancelled(),
    )
    .await;

    (result, streamed)
}

/// The `ToolCallResult` outcome strings the composer streamed for `tool_name`
/// (an `Err` outcome is the serialized correctable `FieldError`s).
fn tool_outcomes<'a>(events: &'a [ComposerEvent], tool_name: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter_map(|event| match event {
            ComposerEvent::ToolCallResult { name, outcome } if name == tool_name => {
                Some(outcome.as_str())
            }
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compose_finalize_refuses_the_impossible_entry_with_the_correctable_error() {
    let script = vec![
        tool_turn(
            "c1",
            "create_strategy",
            json!({ "name": "Impossible Cross", "direction": "long" }),
        ),
        tool_turn(
            "c2",
            "add_entry_signal",
            json!({
                "left": { "source": "price", "price_field": "close" },
                "op": "crosses_above",
                "right": { "source": "price", "price_field": "high" }
            }),
        ),
        tool_turn("c3", "set_exit_rules", json!({ "stop_loss_pct": "0.05" })),
        tool_turn(
            "c4",
            "set_risk_params",
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ),
        // The impossible entry reaches finalize; the builder refuses with the
        // correctable error at the leaf's path, and the compose ends without a
        // finalized strategy (nothing impossible is ever persisted).
        tool_turn("c5", "finalize_strategy", json!({})),
    ];
    let (result, events) = run_scripted(script).await;

    let Err(error) = result else {
        panic!("the compose must not finalize an impossible strategy")
    };
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("did not finalize"),
        "the compose ends without finalizing: {rendered}"
    );
    let finals = tool_outcomes(&events, "finalize_strategy");
    assert_eq!(finals.len(), 1, "finalize ran exactly once: {finals:?}");
    let refusal = finals[0];
    assert!(
        refusal.contains("ImpossibleCondition"),
        "the refusal names the code: {refusal}"
    );
    assert!(
        refusal.contains("entry.crosses_above"),
        "the refusal is pathed at the leaf: {refusal}"
    );
}

// ---------------------------------------------------------------------------
// (vii) a persisted impossible version lists, but is refused at run
// ---------------------------------------------------------------------------

/// The fixture world: migrated temp DB + a copied 1-month BTCUSDT M15 store.
struct World {
    db: Db,
    strategies: SqliteStrategyRepo<pulse::SystemClock>,
    runs: SqliteBacktestRunRepo<pulse::SystemClock>,
    store: CandleStore,
}

async fn world() -> (TempDir, World) {
    let tmp = TempDir::new().expect("tempdir");
    let (_db_path, db) = migrated_db(&tmp).await;
    let store_dir = tmp.path().join("candles");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    (
        tmp,
        World {
            db,
            strategies,
            runs,
            store: CandleStore::with_base_dir(store_dir),
        },
    )
}

/// Insert an OUT-OF-BAND version row carrying the impossible entry — the raw
/// write path, bypassing every application-layer gate (the repo's
/// `create_version` validates; a pre-r3 row predates this item's rule). The
/// row carries the CORRECT `version_hash` so the read path accepts it: the
/// point of the case is the run-time refusal, not a corrupt row.
async fn persisted_impossible_version(world: &World) -> (StrategyId, VersionId) {
    let strategy = world
        .strategies
        .create_strategy("Legacy Impossible", None, &[])
        .await
        .expect("create strategy");
    let mut doc = dsl_json_with_entry(&crosses_above_close_high_json());
    doc["schema_version"] = json!(SchemaVersion::CURRENT.to_string());
    let doc_str = serde_json::to_string(&doc).expect("serialize the document");
    let schema = SchemaVersion::CURRENT.to_string();
    let hash = version_hash(strategy.id.as_str(), None, &schema, &doc_str);
    let created_by = serde_json::to_string(&CreatedBy::Human).expect("serialize created_by");
    let version_id = VersionId::new("raw-impossible-0001");
    sqlx::query(
        "INSERT INTO strategy_version \
         (id, strategy_id, parent_version_id, dsl_schema_version, dsl, dsl_original, \
          version_hash, created_by, creating_llm_call_ids, created_at) \
         VALUES (?1, ?2, NULL, ?3, ?4, ?4, ?5, ?6, '[]', ?7)",
    )
    .bind(version_id.as_str())
    .bind(strategy.id.as_str())
    .bind(&schema)
    .bind(&doc_str)
    .bind(&hash)
    .bind(&created_by)
    .bind("2026-01-01T00:00:00+00:00")
    .execute(world.db.pool())
    .await
    .expect("the pre-r3 row inserts out of band");
    (strategy.id, version_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_impossible_version_lists_but_is_refused_at_run() {
    let (_tmp, world) = world().await;
    let (strategy_id, version_id) = persisted_impossible_version(&world).await;

    // It lists: reading a stored version does not re-validate.
    let listed = world
        .strategies
        .list_versions(&strategy_id)
        .await
        .expect("list versions");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, version_id);

    // Running it is refused by the existing re-validation with the typed code.
    let runs_before = row_count(world.db.pool(), "backtest_run").await;
    let error = run_version_backtest(
        &world.strategies,
        &world.store,
        &BinanceAdapter::new(),
        &SqliteBacktestRunRepo::new(world.db.pool().clone()),
        &BacktestRequest {
            version_id: version_id.clone(),
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: None,
            config: BacktestConfig::default(),
            snapshots: None,
            window: None,
        },
    )
    .await
    .expect_err("the persisted impossible version is refused at run");

    match error {
        BacktestAppError::DslInvalid(errors) => {
            let errors = errors.into_errors();
            assert!(
                errors.iter().any(|e| {
                    e.code == ValidationCode::ImpossibleCondition && e.path == "entry.crosses_above"
                }),
                "the typed refusal names ImpossibleCondition at the leaf: {errors:?}"
            );
        }
        other => panic!("expected BacktestAppError::DslInvalid, got {other:?}"),
    }

    // No run row was written.
    let runs_after = row_count(world.db.pool(), "backtest_run").await;
    assert_eq!(runs_after, runs_before, "no run row for a refused run");
}

// ---------------------------------------------------------------------------
// Engine cases: the committed 1-month BTCUSDT M15 fixture, run in-process
// ---------------------------------------------------------------------------

/// Load the primary M15 candle series from the committed offline fixture store.
fn load_primary() -> CandleSeries {
    let store = CandleStore::with_base_dir(manifest(FIXTURE_STORE));
    let pair = Pair::new("BTCUSDT");
    let head = store
        .read_head(&pair, Timeframe::M15)
        .expect("read M15 HEAD")
        .expect("M15 HEAD present in the fixture store");
    store
        .read_snapshot(&pair, Timeframe::M15, &head)
        .expect("read M15 snapshot")
}

/// The BTCUSDT filters through the exchange-metadata port, as the engine
/// consumes them.
fn btc_filters() -> SymbolFilters {
    BinanceAdapter::new()
        .symbol_filters(&Pair::new("BTCUSDT"))
        .expect("BTCUSDT filters resolve through the port")
}

/// The first bar index where EVERY spec reports a CURRENT value — the old
/// `is_warm` definition (current + readiness, no previous). Case (v) measures
/// the entry behaviour against exactly this bar.
fn first_bar_with_current_values(series: &CandleSeries, specs: &[IndicatorSpec]) -> usize {
    let mut engine = IndicatorEngine::from_specs(specs).expect("engine builds");
    let values: Vec<CompiledValue> = specs
        .iter()
        .map(|spec| CompiledValue::Indicator {
            series: Series::Primary,
            spec: spec.clone(),
            lag: 0,
        })
        .collect();
    for (idx, candle) in series.candles.iter().enumerate() {
        engine.step(candle);
        if idx > 0 && values.iter().all(|value| engine.current(value).is_some()) {
            return idx;
        }
    }
    panic!("the indicators never report a current value over the fixture");
}

/// The first bar index where `is_warm()` holds under the CURRENT engine —
/// post-#36 that includes a previous value for every slot.
fn first_warm_bar(series: &CandleSeries, specs: &[IndicatorSpec]) -> usize {
    let mut engine = IndicatorEngine::from_specs(specs).expect("engine builds");
    for (idx, candle) in series.candles.iter().enumerate() {
        engine.step(candle);
        if idx > 0 && engine.is_warm() {
            return idx;
        }
    }
    panic!("the indicators never warm over the fixture");
}

/// The candle index of a trade's entry-signal bar.
fn entry_signal_index(series: &CandleSeries, trade: &Trade) -> usize {
    series
        .candles
        .iter()
        .position(|c| c.close_time == trade.entry_signal_time)
        .unwrap_or_else(|| {
            panic!(
                "entry signal {} resolves to a fixture bar",
                trade.entry_signal_time
            )
        })
}

/// (iv) #16 — a `Not(rsi(14) > 70)` entry produces no entry before RSI(14) is
/// warm: the first entry's signal bar is at or after the first warm bar (the
/// entry gate the engine already holds — pinned by spine ruling a1).
#[test]
fn not_compare_entry_cannot_fire_before_rsi_is_warm() {
    let primary = load_primary();
    let dsl = dsl_with_entry(not(cmp(ind_rsi(14), Comparator::Gt, constant(70))));
    let compiled = compile(&validate(&dsl).expect("the document validates")).expect("compiles");
    let result = run_backtest(
        &compiled,
        &primary,
        None,
        &BacktestConfig::default(),
        &btc_filters(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("the backtest runs over the fixture");

    let warm = first_warm_bar(&primary, &[ind_rsi_spec(14)]);
    assert!(
        !result.trades.is_empty(),
        "non-vacuous: the Not entry fires after warmup"
    );
    for trade in &result.trades {
        assert!(
            trade.entry_signal_time >= primary.candles[warm].close_time,
            "no entry signal before RSI(14) is warm (warm bar {warm})"
        );
    }
    let first = entry_signal_index(&primary, &result.trades[0]);
    assert!(
        first <= warm + 100,
        "the first entry fires near the warm boundary (signal idx {first}, warm {warm})"
    );
}

/// (v) #36 — a `Not(CrossesAbove(ema(5), ema(20)))` entry does not fire on the
/// first bar where both EMAs have a current value; the earliest possible entry
/// signal is the next bar. On the pre-fix `is_warm` the gate admitted bar k
/// (previous still `None`, the cross leaf read `false`, the `Not` read `true`)
/// and the entry fired there — this test failed on the pre-fix engine.
#[test]
fn not_cross_entry_skips_the_first_bar_with_current_values() {
    let primary = load_primary();
    let dsl = dsl_with_entry(not(cross_above(ind_ema(5), ind_ema(20))));
    let compiled = compile(&validate(&dsl).expect("the document validates")).expect("compiles");
    let result = run_backtest(
        &compiled,
        &primary,
        None,
        &BacktestConfig::default(),
        &btc_filters(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("the backtest runs over the fixture");

    let k = first_bar_with_current_values(&primary, &[ind_ema_spec(5), ind_ema_spec(20)]);
    assert!(
        !result.trades.is_empty(),
        "non-vacuous: the Not cross entry fires after warmup"
    );
    for trade in &result.trades {
        assert_ne!(
            trade.entry_signal_time, primary.candles[k].close_time,
            "no entry on the first bar where both EMAs have a current value (bar {k})"
        );
        assert!(
            trade.entry_signal_time >= primary.candles[k + 1].close_time,
            "the earliest possible entry signal is the next bar after k ({k})"
        );
    }
    let first = entry_signal_index(&primary, &result.trades[0]);
    assert!(
        first <= k + 50,
        "the first entry fires near the warm boundary (signal idx {first}, k {k})"
    );
}

/// (vi) Determinism — two cold runs of a version exercising (iv) persist
/// identical `result_content_hash` and trades.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn determinism_two_cold_runs_of_a_not_entry_are_identical() {
    let (_tmp, world) = world().await;
    let dsl = dsl_with_entry(not(cmp(ind_rsi(14), Comparator::Gt, constant(70))));
    let strategy = world
        .strategies
        .create_strategy("Warm Determinism", None, &[])
        .await
        .expect("create strategy");
    let version = world
        .strategies
        .create_version(NewVersion {
            strategy_id: StrategyId::new(strategy.id.as_str().to_owned()),
            parent_version_id: None,
            dsl_json: serde_json::to_string(&dsl).expect("serialize the document"),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("the version persists (the Not entry is not judged)");

    let request = BacktestRequest {
        version_id: version.id.clone(),
        pair: Pair::new("BTCUSDT"),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: None,
        config: BacktestConfig::default(),
        snapshots: None,
        window: None,
    };
    let first = run_version_backtest(
        &world.strategies,
        &world.store,
        &BinanceAdapter::new(),
        &world.runs,
        &request,
    )
    .await
    .expect("the first cold run");
    let second = run_version_backtest(
        &world.strategies,
        &world.store,
        &BinanceAdapter::new(),
        &world.runs,
        &request,
    )
    .await
    .expect("the second cold run");

    assert!(
        !first.trades.is_empty(),
        "non-vacuous: the determinism pair runs real trades"
    );
    assert_eq!(
        first.run.result_content_hash, second.run.result_content_hash,
        "two cold runs persist the same result hash"
    );
    assert_eq!(first.trades.len(), second.trades.len());
    for (a, b) in first.trades.iter().zip(&second.trades) {
        assert_eq!(a.entry_signal_time, b.entry_signal_time);
        assert_eq!(a.entry_fill_time, b.entry_fill_time);
        assert_eq!(a.realized_pnl, b.realized_pnl);
    }
}
