//! AC-3 — the coach's two attached risk-gate controls, asserted (r1.s2.w3,
//! operator ruling 2026-08-24).
//!
//! **no-secret-in-log.** A canary credential is tagged into the redactor and then
//! echoed back by the model — in its free text AND inside its tool arguments,
//! which is the path an ordinary redactor misses because those arguments become a
//! *stored domain value* (the hypothesis), not just a logged string. No persisted
//! artifact of the turn may contain it: not the `LlmCall` prompt or completion
//! columns, not the coaching session row, not the proposal row.
//!
//! **least privilege.** The prompt carries the bounded `CoachContext` and nothing
//! else. The fixture's trades and equity curve are built with distinctive marker
//! values that appear NOWHERE in an aggregate, so if a trade-log entry or an
//! equity point ever leaks into the rendered prompt, the marker shows up and this
//! test fails.
//!
//! No live LLM call happens here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use pulse::{
    BacktestInputs, BacktestResult, BacktestRunId, BacktestRunRepository, CoachCliOutcome,
    CoachWiring, CreatedBy, DataVersion, Db, Direction, EngineFingerprint, ExitReason, FakeClock,
    Fill, FundingConfig, LlmCallRepository, LlmConfig, LlmError, LlmProvider, LlmResponse,
    MIGRATOR, Message, NewVersion, Pair, Redactor, Regime, RegimeBreakdown, SessionOutcome,
    SkippedEntryCounts, SnapshotSelection, SqliteBacktestRunRepo, SqliteCoachTurnSource,
    SqliteCoachingRepo, SqliteLlmCallRepo, SqliteStrategyRepo, StrategyRepository, SummaryStats,
    Timeframe, TokenUsage, ToolCall, ToolDefinition, Trade, TradeSource, run_coach_with,
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
use coach_support::{CapturingLlmRepo, canonical_dsl_json, config, test_prices};

/// The canary. API-key-shaped AND tagged, so both the structural and the
/// tagged-value halves of the redactor are in play.
const CANARY: &str = "sk-canary-DO-NOT-LEAK-9f8e7d6c5b4a3210";

/// Marker values that exist ONLY on the trade log and the equity curve. None is an
/// aggregate, so none has any legitimate route into the rendered context.
const TRADE_ENTRY_PRICE: &str = "31337.4242";
const TRADE_EXIT_PRICE: &str = "31338.5151";
const EQUITY_MARKER: &str = "77771.2323";

struct ScriptedProvider {
    scripts: Mutex<VecDeque<LlmResponse>>,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<LlmResponse>) -> (Self, Arc<Mutex<Vec<Vec<Message>>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                scripts: Mutex::new(responses.into()),
                seen: Arc::clone(&seen),
            },
            seen,
        )
    }
}

impl LlmProvider for ScriptedProvider {
    fn chat(
        &self,
        messages: Vec<Message>,
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
    ) -> impl std::future::Future<Output = Result<LlmResponse, LlmError>> {
        self.seen.lock().expect("seen lock").push(messages);
        std::future::ready(Ok(self
            .scripts
            .lock()
            .expect("scripts lock")
            .pop_front()
            .unwrap_or_else(|| LlmResponse {
                content: None,
                tool_calls: Vec::new(),
                usage: usage(),
            })))
    }
}

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 800,
        output_tokens: 120,
    }
}

/// A trade whose prices are markers — nothing aggregates them, so they must never
/// appear in the rendered context.
fn marker_trade() -> Trade {
    let entry: Decimal = TRADE_ENTRY_PRICE.parse().expect("marker parses");
    let exit: Decimal = TRADE_EXIT_PRICE.parse().expect("marker parses");
    Trade {
        direction: Direction::Long,
        qty: Decimal::new(1, 0),
        entry_price: entry,
        exit_price: exit,
        entry_signal_time: 1_699_999_100_000,
        entry_fill_time: 1_700_000_000_000,
        exit_signal_time: 1_700_000_000_000,
        exit_fill_time: 1_700_000_900_000,
        fills: vec![
            Fill {
                price: entry,
                qty: Decimal::new(1, 0),
                time_ms: 1_700_000_000_000,
                fee: Decimal::new(1, 2),
            },
            Fill {
                price: exit,
                qty: Decimal::new(1, 0),
                time_ms: 1_700_000_900_000,
                fee: Decimal::new(1, 2),
            },
        ],
        fees_total: Decimal::new(2, 2),
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        realized_pnl: Decimal::new(11, 1),
        realized_r: Decimal::new(2, 0),
        // These two ARE aggregated, so they are ordinary values, not markers.
        mfe_r: Decimal::new(25, 1),
        mae_r: Decimal::new(-5, 1),
        exit_reason: ExitReason::TakeProfit,
        source: TradeSource::Backtest,
        regime: Regime::TrendingUp,
        stop_price: None,
    }
}

/// A migrated DB with a version and a run whose trade log + equity curve carry the
/// markers.
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
    let trades = vec![marker_trade()];
    let equity_marker: Decimal = EQUITY_MARKER.parse().expect("marker parses");
    let result = BacktestResult {
        trades: trades.clone(),
        net_pnl: Decimal::new(11, 1),
        fees_total: Decimal::new(2, 2),
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        regime_breakdown: RegimeBreakdown::default(),
        skipped_entries: SkippedEntryCounts::default(),
        open_position: None,
        engine_fingerprint: EngineFingerprint::current(),
        summary: SummaryStats::default(),
        equity_curve: pulse::EquityCurve::from_trades(1_699_999_000_000, equity_marker, &trades),
    };
    let run_id = run_repo
        .save_run(
            &version.id,
            &seed_inputs(),
            &result,
            &SummaryStats::default(),
            equity_marker,
        )
        .await
        .expect("save run");
    (tmp, db, run_id)
}

/// Run one turn with the canary tagged into the redactor.
async fn turn_with(db: &Db, run_id: &BacktestRunId, provider: ScriptedProvider) -> CoachCliOutcome {
    let clock = FakeClock::at(1_700_000_000_000);
    let ids = Arc::new(Mutex::new(Vec::new()));
    let wiring = CoachWiring {
        provider,
        llm_repo: CapturingLlmRepo::new(
            SqliteLlmCallRepo::with_deps(db.pool().clone(), clock),
            Arc::clone(&ids),
        ),
        // The live composition root tags the resolved key exactly like this.
        redactor: Redactor::from_config(vec![CANARY.to_owned()]),
        prices: test_prices(),
        clock,
        key_source: None,
        config: config(),
        prompt_dir: None,
        turn_timeout: None,
        max_dsl_bytes: None,
        captured: ids,
        // One independent turn per call: a fresh claimed id, a registry that outlives
        // nothing (r1.s4.w1).
        session_id: None,
        registry: None,
    };
    let source = SqliteCoachTurnSource::new(db.pool().clone());
    let coaching_repo = SqliteCoachingRepo::with_deps(db.pool().clone(), clock);
    run_coach_with(wiring, &source, &coaching_repo, run_id)
        .await
        .expect("the turn completes")
}

/// Every text column of every coaching + ledger row, concatenated — the whole
/// persisted footprint of a turn, as a haystack.
async fn persisted_text(db: &Db) -> String {
    let mut out = String::new();
    for sql in [
        "SELECT COALESCE(group_concat(id || outcome || COALESCE(failure_kind,'') || \
         COALESCE(failure_detail,'')), '') FROM coaching_sessions",
        "SELECT COALESCE(group_concat(mutation || hypothesis || disposition), '') \
         FROM coaching_proposals",
        "SELECT COALESCE(group_concat(prompt_messages || COALESCE(completion,'') || model), '') \
         FROM llm_call",
    ] {
        let chunk: String = sqlx::query_scalar(sql)
            .fetch_one(db.pool())
            .await
            .expect("read persisted text");
        out.push_str(&chunk);
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// no-secret-in-log
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_persisted_artifact_of_a_turn_contains_the_canary() {
    let (_tmp, db, run_id) = seeded().await;

    // The model echoes the canary back BOTH in free text and inside its tool
    // arguments — the hypothesis becomes a stored domain value, so the tool-argument
    // path is the one that matters most here.
    let (provider, _seen) = ScriptedProvider::new(vec![LlmResponse {
        content: Some(format!("using your key {CANARY} I suggest a slower RSI")),
        tool_calls: vec![ToolCall {
            id: "c1".to_owned(),
            name: "propose_mutation".to_owned(),
            arguments: json!({
                "path": "entry.lhs.indicator.rsi.period",
                "new_value": { "type": "Period", "value": 21 },
                "hypothesis": format!("your key {CANARY} shows the entries fire too early"),
            }),
        }],
        usage: usage(),
    }]);

    let outcome = turn_with(&db, &run_id, provider).await;

    // The turn still succeeded — redaction is not refusal.
    assert!(
        matches!(outcome.session.outcome, SessionOutcome::Proposed { .. }),
        "a canary in the arguments must be scrubbed, not turned into a failure"
    );

    let haystack = persisted_text(&db).await;
    assert!(
        !haystack.contains(CANARY),
        "the canary reached a persisted artifact of the turn"
    );

    // And specifically the two places a reviewer would check by hand.
    match &outcome.session.outcome {
        SessionOutcome::Proposed { proposal } => assert!(
            !proposal.hypothesis.as_str().contains(CANARY),
            "the stored hypothesis carries the canary: {}",
            proposal.hypothesis.as_str()
        ),
        SessionOutcome::Failed { failure } => panic!("expected a proposal, got {failure:?}"),
        SessionOutcome::Pending => panic!("expected a settled turn, got an open claim"),
    }

    let call_id = outcome
        .session
        .llm_call_id
        .as_ref()
        .expect("the turn reached the provider");
    let llm_repo = SqliteLlmCallRepo::new(db.pool().clone());
    let call = llm_repo
        .get_call(call_id)
        .await
        .expect("get_call")
        .expect("row present");
    assert!(
        !call.completion.unwrap_or_default().contains(CANARY),
        "the ledger's completion column carries the canary"
    );
    let prompt_text = serde_json::to_string(&call.prompt_messages).expect("serialize prompt");
    assert!(
        !prompt_text.contains(CANARY),
        "the ledger's prompt column carries the canary"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_canary_in_a_failed_turns_reason_is_scrubbed_too() {
    let (_tmp, db, run_id) = seeded().await;

    // Malformed arguments carrying the canary: the recorded REASON quotes the
    // arguments, which is exactly how a secret sneaks into an audit trail.
    let (provider, _seen) = ScriptedProvider::new(vec![LlmResponse {
        content: None,
        tool_calls: vec![ToolCall {
            id: "c1".to_owned(),
            name: "propose_mutation".to_owned(),
            arguments: json!({
                "path": "entry.lhs.indicator.rsi.period",
                "new_value": format!("not-the-tagged-shape-{CANARY}"),
                "hypothesis": "faster",
            }),
        }],
        usage: usage(),
    }]);

    let outcome = turn_with(&db, &run_id, provider).await;

    assert!(
        matches!(outcome.session.outcome, SessionOutcome::Failed { .. }),
        "a malformed argument shape is a recorded failure"
    );
    let haystack = persisted_text(&db).await;
    assert!(
        !haystack.contains(CANARY),
        "the canary reached the recorded failure reason"
    );
}

// ---------------------------------------------------------------------------
// least privilege
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_prompt_carries_no_trade_log_and_no_equity_curve() {
    let (_tmp, db, run_id) = seeded().await;
    let (provider, seen) = ScriptedProvider::new(vec![LlmResponse {
        content: None,
        tool_calls: vec![ToolCall {
            id: "c1".to_owned(),
            name: "propose_mutation".to_owned(),
            arguments: json!({
                "path": "entry.lhs.indicator.rsi.period",
                "new_value": { "type": "Period", "value": 21 },
                "hypothesis": "a slower RSI should cut whipsaw entries",
            }),
        }],
        usage: usage(),
    }]);

    let _outcome = turn_with(&db, &run_id, provider).await;

    let seen = seen.lock().expect("seen lock");
    assert_eq!(seen.len(), 1, "exactly one call");
    let rendered = serde_json::to_string(&seen[0]).expect("serialize the sent messages");

    // Least privilege: the markers live only on the trade log / equity curve.
    for marker in [TRADE_ENTRY_PRICE, TRADE_EXIT_PRICE, EQUITY_MARKER] {
        assert!(
            !rendered.contains(marker),
            "a trade-log / equity-curve value ({marker}) reached the prompt"
        );
    }

    // ...while the projection the coach IS meant to see did arrive.
    for expected in [
        "Backtest result",
        "MFE / MAE",
        "Skipped entries",
        "Strategy DSL",
        "Regime breakdown",
    ] {
        assert!(
            rendered.contains(expected),
            "the bounded projection is missing its `{expected}` section"
        );
    }

    // Exactly two messages: the system prompt and the projection. Nothing else is
    // ever injected.
    assert_eq!(
        seen[0].len(),
        2,
        "system prompt + bounded context, and nothing else"
    );
}
