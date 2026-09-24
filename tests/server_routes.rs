//! r3.s3.w2 AC-2 — the plain REST command routes (`tests/server_routes.rs`).
//!
//! Spins the real router (`tests/support/server.rs`) and drives EVERY plain
//! REST route once with an app token over the BTCUSDT fixture, asserting for
//! each that the response body deserializes to the DTO the Tauri command
//! returns — and for `library-overview` and `get-backtest-run`, that it EQUALS
//! the core's own result on the same state (JSON equality). The remaining
//! required checks: `bus-selftest-failure` is a 422 carrying the real
//! self-test `BusError`; an `agent` token is a 403 `scope_refused` on a plain
//! route AND on an `ops/` route; `credential-status` answers one of the five
//! wire values and never a key.
//!
//! Seeding uses the real cores over the same db the server opened: the
//! coach-decide happy path records a proposed session through
//! `coach_turn_core` with a scripted provider (`tests/tauri_coach.rs`'s
//! pattern), the compare path runs a parent and an agent child version
//! (`tests/tauri_backtest.rs`'s pattern), and the walk-forward path runs a
//! real 2-fold battery. Offline throughout — no credential exists in this
//! process, which is exactly what `credential-status` is asked to report.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Mutex;

use pulse::{
    AgentHypothesis, AgentName, BacktestRunRequest, CoachTurnDeps, CoachTurnRequestDto,
    CredentialSource, LlmBackend, LlmConfig, LlmError, LlmProvider, LlmResponse, Message,
    ModelPrice, NewAgentSubmission, NewVersion, PriceTable, Redactor, StrategyRepository,
    TokenUsage, ToolCall, ToolDefinition, coach_turn_core, compare_child_run_core,
    get_backtest_run_core, library_overview_core, run_backtest_version_core,
    run_walk_forward_version_core,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use support::server::{ServerOptions, TestServer, seed_version, spawn_server};

mod support;

// ---------------------------------------------------------------------------
// HTTP plumbing
// ---------------------------------------------------------------------------

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

fn auth(token: &str) -> String {
    format!("Bearer {token}")
}

async fn get_json(ts: &TestServer, path: &str, token: &str) -> (reqwest::StatusCode, Value) {
    let resp = client()
        .get(format!("{}{path}", ts.base))
        .header("Authorization", auth(token))
        .send()
        .await
        .expect("get succeeds");
    let status = resp.status();
    (status, resp.json().await.expect("json body"))
}

async fn post_json(
    ts: &TestServer,
    path: &str,
    token: &str,
    body: Value,
) -> (reqwest::StatusCode, Value) {
    let resp = client()
        .post(format!("{}{path}", ts.base))
        .header("Authorization", auth(token))
        .json(&body)
        .send()
        .await
        .expect("post succeeds");
    let status = resp.status();
    (status, resp.json().await.expect("json body"))
}

// ---------------------------------------------------------------------------
// The scripted provider the coach-decide seeding needs
// ---------------------------------------------------------------------------

/// An API-key-shaped literal for the coach wiring's redactor — NOT a real key.
const FAKE_KEY: &str = "sk-ROUTES1234abcd5678efgh9012ijkl3456";

/// A stand-in coach system prompt (the fake provider ignores it).
const TEST_PROMPT: &str = "You are PulseTrader's coach.";

/// A scripted single-tool-call turn — one `propose_mutation` at the golden
/// strategy's RSI-period locator, so the seeded session lands in the
/// `proposed` state a decision can answer (`tests/tauri_coach.rs`'s
/// `propose_call`, same path, same typed value).
fn propose_call() -> LlmResponse {
    LlmResponse {
        content: None,
        tool_calls: vec![ToolCall {
            id: "call-1".to_owned(),
            name: "propose_mutation".to_owned(),
            arguments: json!({
                "path": "entry.lhs.indicator.rsi.period",
                "new_value": { "type": "Period", "value": 21 },
                "hypothesis": "a slower RSI trades less often",
            }),
        }],
        usage: TokenUsage {
            input_tokens: 120,
            output_tokens: 48,
        },
    }
}

struct ScriptedProvider {
    scripts: Mutex<VecDeque<LlmResponse>>,
}

impl LlmProvider for ScriptedProvider {
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
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
            },
        })))
    }
}

fn coach_deps(script: Vec<LlmResponse>) -> CoachTurnDeps<ScriptedProvider> {
    CoachTurnDeps {
        provider: ScriptedProvider {
            scripts: Mutex::new(script.into()),
        },
        prices: PriceTable::from_config(
            "CNY",
            HashMap::from([(
                "glm-5.3-flash".to_owned(),
                ModelPrice {
                    input_per_mtok: Decimal::from(1),
                    output_per_mtok: Decimal::from(2),
                },
            )]),
        ),
        redactor: Redactor::from_config(vec![FAKE_KEY.to_owned()]),
        key_source: Some(CredentialSource::Env),
        config: LlmConfig {
            backend: LlmBackend::Ollama,
            model: "glm-5.3-flash".to_owned(),
            temperature: 0.0,
            max_tokens: 2_048,
            reasoning_effort: None,
        },
        prompt: TEST_PROMPT.to_owned(),
        prompt_version: Some("test-1".to_owned()),
        turn_timeout: None,
        max_dsl_bytes: None,
    }
}

// ---------------------------------------------------------------------------
// Seeding
// ---------------------------------------------------------------------------

/// One version + its REAL parent run over the fixture, through the server's
/// own desktop state. Returns (`version_id`, `run_id`).
async fn seed_version_with_run(ts: &TestServer) -> (String, String) {
    let version_id = seed_version(&ts.desktop().await).await;
    let run = run_backtest_version_core(
        ts.state.desktop(),
        BacktestRunRequest {
            version_id: version_id.as_str().to_owned(),
        },
    )
    .await
    .expect("the fixture backtest runs");
    (version_id.as_str().to_owned(), run.run_id)
}

/// The compare seeding (`tests/tauri_backtest.rs`'s agent-child pattern): a
/// parent version with a run, plus an AGENT child version with its own run.
/// Returns (`parent_version_id`, `child_version_id`, `child_run_id`).
async fn seed_parent_and_child(ts: &TestServer) -> (String, String, String) {
    let state = ts.desktop().await;
    let repo = state.strategy_repo();
    let strat = repo
        .create_strategy("w2 compare demo", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create strategy");
    let dsl = std::fs::read_to_string(support::server::manifest(support::server::GOLDEN_STRATEGY))
        .expect("read golden strategy");
    let parent = repo
        .create_version(NewVersion {
            strategy_id: strat.id.clone(),
            parent_version_id: None,
            dsl_json: dsl.clone(),
            created_by: pulse::CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create parent version");
    let (child, _submission) = repo
        .create_agent_version(
            NewVersion {
                strategy_id: strat.id,
                parent_version_id: Some(parent.id.clone()),
                dsl_json: dsl,
                created_by: pulse::CreatedBy::ExternalAgent,
                creating_llm_call_ids: vec![],
            },
            NewAgentSubmission {
                agent_name: AgentName::parse("claude-code").expect("valid agent name"),
                hypothesis: AgentHypothesis::parse("a structural variant")
                    .expect("valid hypothesis"),
            },
        )
        .await
        .expect("create agent child");

    // The parent's run FIRST, so the child's comparison has a parent side, then
    // the child's own run through the real core.
    let _parent_run = run_backtest_version_core(
        ts.state.desktop(),
        BacktestRunRequest {
            version_id: parent.id.as_str().to_owned(),
        },
    )
    .await
    .expect("parent run");
    let child_run = run_backtest_version_core(
        ts.state.desktop(),
        BacktestRunRequest {
            version_id: child.id.as_str().to_owned(),
        },
    )
    .await
    .expect("child run");
    (
        parent.id.as_str().to_owned(),
        child.id.as_str().to_owned(),
        child_run.run_id,
    )
}

/// A coach session with a recorded proposal, through the REAL `coach_turn_core`
/// with the scripted provider. Returns the session id.
async fn seed_proposed_session(ts: &TestServer, run_id: &str) -> String {
    let outcome = coach_turn_core(
        ts.state.desktop(),
        coach_deps(vec![propose_call()]),
        CoachTurnRequestDto {
            session_id: "sess-w2".to_owned(),
            run_id: run_id.to_owned(),
        },
    )
    .await
    .expect("the scripted turn records a proposed session");
    assert_eq!(outcome.outcome, "proposed", "{outcome:?}");
    outcome.session_id
}

// ---------------------------------------------------------------------------
// AC-2: every plain route once, each body = the command's DTO
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_plain_route_answers_its_tauri_dto_with_an_app_token() {
    let ts = spawn_server(ServerOptions::default()).await;
    let desktop = ts.state.desktop().clone();
    let (version_id, run_id) = seed_version_with_run(&ts).await;
    let (_parent_v, _child_v, child_run) = seed_parent_and_child(&ts).await;
    let wf = run_walk_forward_version_core(
        &desktop,
        pulse::WalkForwardRunRequest {
            version_id: version_id.clone(),
            from: None,
            to: None,
            k: Some(2),
        },
    )
    .await
    .expect("a 2-fold walk-forward over the fixture");
    let session_id = seed_proposed_session(&ts, &run_id).await;

    // 1. shell-info — a ShellInfo DTO (appVersion/engineFingerprint/
    //    targetTriple/strategyCount — the handshake's binaryVersion is a
    //    different body's field).
    let (status, body) = get_json(&ts, "/api/v1/shell-info", &ts.app_token).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");
    assert!(body["appVersion"].is_string(), "{body:?}");
    assert!(body["engineFingerprint"].is_string(), "{body:?}");
    assert!(body["targetTriple"].is_string(), "{body:?}");
    assert!(body["strategyCount"].is_u64(), "{body:?}");

    // 2. credential-status — exactly one of the five wire values, never a key.
    let (status, body) = get_json(&ts, "/api/v1/credential-status", &ts.app_token).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");
    let wire = body.as_str().expect("credential status is a bare string");
    assert!(
        matches!(
            wire,
            "env" | "config-dir" | "cwd-dotenv" | "app-data-dir" | "none"
        ),
        "one of the five wire values, got {wire}"
    );

    // 3. library-overview — deserializes AND equals the core's own result.
    let (status, body) = get_json(&ts, "/api/v1/library-overview", &ts.app_token).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");
    let core = library_overview_core(&desktop)
        .await
        .expect("the core answers on the same state");
    assert_eq!(
        body,
        serde_json::to_value(&core).expect("core dto json"),
        "the route equals the core's own result on the same state"
    );

    // 4. compose-cancel — the bool, false for an id nothing is streaming under.
    let (status, body) = post_json(
        &ts,
        "/api/v1/compose-cancel",
        &ts.app_token,
        json!({ "runId": pulse::RunId::new().as_str() }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");
    assert_eq!(body, json!(false), "no live run under a fresh id");

    // 5. coach-decide — a CoachDecisionDto: {accepted, session} with the
    //    session's proposal disposition flipped to the decision.
    let (status, body) = post_json(
        &ts,
        "/api/v1/coach-decide",
        &ts.app_token,
        json!({ "sessionId": session_id, "action": { "kind": "reject" } }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");
    assert_eq!(body["session"]["sessionId"], json!(session_id), "{body:?}");
    assert_eq!(
        body["session"]["proposal"]["disposition"],
        json!("rejected"),
        "a reject decision is recorded: {body:?}"
    );
    assert!(
        body["accepted"].is_null(),
        "a reject accepts nothing: {body:?}"
    );

    // 6. compare-child-run — a CompareChildRunDto for the agent child's run.
    let (status, body) = post_json(
        &ts,
        "/api/v1/compare-child-run",
        &ts.app_token,
        json!({ "childRunId": child_run }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");
    let core = compare_child_run_core(
        &desktop,
        pulse::CompareChildRunRequest {
            child_run_id: child_run.clone(),
        },
    )
    .await
    .expect("the core compares on the same state");
    assert_eq!(
        body,
        serde_json::to_value(&core).expect("compare dto json"),
        "the compare route equals the core's own result on the same state"
    );
    // 7. get-walk-forward-run — the persisted WalkForwardRunDto comes back.
    let (status, body) = post_json(
        &ts,
        "/api/v1/get-walk-forward-run",
        &ts.app_token,
        json!({ "walkForwardRunId": wf.walk_forward_run_id }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");
    assert!(body["walkForwardRunId"].is_string(), "{body:?}");
    assert!(
        body["folds"].as_array().expect("folds").len() == 2,
        "{body:?}"
    );

    // 8. get-backtest-run — deserializes AND equals the core's own result.
    let (status, body) = post_json(
        &ts,
        "/api/v1/get-backtest-run",
        &ts.app_token,
        json!({ "runId": run_id }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");
    let core = get_backtest_run_core(
        &desktop,
        pulse::GetBacktestRunRequest {
            run_id: run_id.clone(),
        },
    )
    .await
    .expect("the core reads the run on the same state");
    assert_eq!(
        body,
        serde_json::to_value(&core).expect("run dto json"),
        "the read route equals the core's own result on the same state"
    );

    // 9. bus-selftest-failure — ALWAYS a 422 whose body is the real self-test
    //    BusError (DataError mapped through the real From impl, code `data`).
    let (status, body) = post_json(
        &ts,
        "/api/v1/bus-selftest-failure",
        &ts.app_token,
        json!({}),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        "{body:?}"
    );
    assert_eq!(body["code"], json!("data"), "{body:?}");
    assert!(
        body["message"]
            .as_str()
            .expect("message")
            .contains("self-test"),
        "the real self-test message comes through: {body:?}"
    );
}

// ---------------------------------------------------------------------------
// Scope isolation over the command surface
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_token_is_a_403_on_a_plain_route_and_an_ops_route() {
    let ts = spawn_server(ServerOptions::default()).await;

    // A plain route.
    let (status, body) = get_json(&ts, "/api/v1/shell-info", &ts.agent_token).await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body:?}");
    assert_eq!(body["code"], json!("scope_refused"), "{body:?}");

    // An ops/ route — the spawns are app-scoped like everything else.
    let (status, body) = post_json(
        &ts,
        "/api/v1/ops/start-demo-stream",
        &ts.agent_token,
        json!({ "steps": 3 }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body:?}");
    assert_eq!(body["code"], json!("scope_refused"), "{body:?}");
}

// ---------------------------------------------------------------------------
// Malformed bodies are 422 validation, never 500
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_request_body_is_a_422_validation_bus_error() {
    let ts = spawn_server(ServerOptions::default()).await;
    let resp = client()
        .post(format!("{}/api/v1/get-backtest-run", ts.base))
        .header("Authorization", auth(&ts.app_token))
        .header("Content-Type", "application/json")
        .body("{not json")
        .send()
        .await
        .expect("post succeeds");
    assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.expect("bus error json");
    assert_eq!(body["code"], json!("validation"), "{body:?}");
}
