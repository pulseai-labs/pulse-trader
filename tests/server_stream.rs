//! r3.s3.w2 AC-1 — the server-owned operation surface over SSE
//! (`tests/server_stream.rs`, ledger line `d33`).
//!
//! Spins the REAL router in-process (`tests/support/server.rs`) and drives the
//! five `ops/` operations over HTTP, covering the required groups:
//! (i) a demo stream's ordered bus events and its single `result`;
//! (ii) `Last-Event-ID` resume with no dups and no gaps;
//! (iii) a dropped SSE connection does NOT cancel the operation;
//! (iv) a late attach receives the whole buffered stream then the terminal;
//! (v) injectable retention: progress expiry, then full expiry (`op_unknown`);
//! (vi) `compose-cancel` lands as the core's cancelled OUTCOME and the latch
//!      map empties;
//! (vii) single-flight: a concurrent duplicate is refused with the typed busy
//!      error as its terminal error event (recorded reading);
//! (viii) the scripted provider's fake credential value never reaches a frame,
//!      a response body, or the request log.
//!
//! Offline: no network, no Keychain, no credential — the compose arm runs the
//! REAL `compose_strategy_core` over the spec's narrowest test seam (the
//! compose-runner factory), exactly as `tests/tauri_compose.rs` fakes only the
//! provider.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use pulse::{
    BusError, ClientError, ComposeDeps, ComposeResult, ComposeRunCtx, ComposeRunner, ComposeWiring,
    CredentialSource, Db, FakeClock, LlmBackend, LlmConfig, LlmError, LlmProvider, LlmResponse,
    Message, ModelPrice, OperationKey, PriceTable, Redactor, ServerClient, SqliteLlmCallRepo,
    SqliteStrategyRepo, TokenUsage, ToolCall, ToolDefinition, VersionId, compose_strategy_core,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use support::server::{ServerOptions, TestServer, fast_sweep, seed_version, spawn_server};

mod support;

/// An API-key-shaped literal the redactor must scrub — NOT a real key. Deliberately
/// distinctive so an eight-scenario scan cannot miss it.
const FAKE_KEY: &str = "sk-STREAM1234abcd5678efgh9012ijkl3456";

/// A stand-in composer system prompt (the fake provider ignores it).
const TEST_PROMPT: &str = "You are PulseTrader's strategy composer. Build the \
    strategy only by calling builder tools; never emit raw DSL JSON.";

// ---------------------------------------------------------------------------
// SSE plumbing
// ---------------------------------------------------------------------------

/// One parsed server-sent event.
#[derive(Debug, Clone)]
struct Frame {
    id: u32,
    event: String,
    data: Value,
}

fn parse_frames(bytes: &[u8]) -> Vec<Frame> {
    let text = std::str::from_utf8(bytes).expect("sse body is utf8");
    let mut frames = Vec::new();
    for block in text.split("\n\n") {
        if block.trim().is_empty() {
            continue;
        }
        let mut id = None;
        let mut event = None;
        let mut data = String::new();
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("id: ") {
                id = Some(v.trim().parse::<u32>().expect("numeric sse id"));
            } else if let Some(v) = line.strip_prefix("event: ") {
                event = Some(v.trim().to_owned());
            } else if let Some(v) = line.strip_prefix("data: ") {
                data.push_str(v);
            }
        }
        frames.push(Frame {
            id: id.unwrap_or_else(|| panic!("every frame carries an id: {block:?}")),
            event: event.unwrap_or_else(|| panic!("every frame carries an event name: {block:?}")),
            data: serde_json::from_str(data.trim())
                .unwrap_or_else(|e| panic!("frame data parses as JSON ({e}): {data:?}")),
        });
    }
    frames
}

/// Read an SSE response to the END and parse every frame.
async fn read_all_frames(mut resp: reqwest::Response) -> Vec<Frame> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.expect("sse chunk") {
        buf.extend_from_slice(&chunk);
    }
    parse_frames(&buf)
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

fn auth(token: &str) -> String {
    format!("Bearer {token}")
}

async fn post_json(ts: &TestServer, path: &str, token: &str, body: Value) -> reqwest::Response {
    client()
        .post(format!("{}{path}", ts.base))
        .header("Authorization", auth(token))
        .json(&body)
        .send()
        .await
        .expect("post succeeds")
}

async fn get_text(ts: &TestServer, path: &str, token: &str) -> reqwest::Response {
    client()
        .get(format!("{}{path}", ts.base))
        .header("Authorization", auth(token))
        .send()
        .await
        .expect("get succeeds")
}

/// `POST /ops/...` -> (status, parsed body).
async fn post_op(ts: &TestServer, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
    let resp = post_json(ts, path, &ts.app_token, body).await;
    let status = resp.status();
    (status, resp.json().await.expect("json body"))
}

/// Poll `GET /ops/{id}` until the operation leaves `running` (bounded).
async fn await_terminal(ts: &TestServer, op_id: &str) -> Value {
    for _ in 0..480 {
        let resp = get_text(ts, &format!("/api/v1/ops/{op_id}"), &ts.app_token).await;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        let body: Value = resp.json().await.expect("status json");
        if body["state"] != json!("running") {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("operation {op_id} never left `running`");
}

// ---------------------------------------------------------------------------
// The compose test seam: a slow scripted provider behind the approved
// compose-runner factory (the spec's "narrowest test seam")
// ---------------------------------------------------------------------------

/// A scripted [`LlmProvider`] that sleeps before every turn, so a cancel can
/// reliably land mid-run (`tests/tauri_compose.rs`'s fake + a delay).
struct SlowScriptedProvider {
    scripts: Mutex<VecDeque<LlmResponse>>,
    delay: Duration,
}

impl LlmProvider for SlowScriptedProvider {
    fn chat(
        &self,
        _messages: Vec<Message>,
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
    ) -> impl Future<Output = Result<LlmResponse, LlmError>> {
        let next = self.scripts.lock().expect("scripts lock").pop_front();
        std::thread::sleep(self.delay);
        std::future::ready(Ok(next.unwrap_or_else(|| LlmResponse {
            content: Some("(script exhausted)".to_owned()),
            tool_calls: Vec::new(),
            usage: usage(),
        })))
    }
}

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 120,
        output_tokens: 48,
    }
}

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

/// Enough of `tests/compose_cli.rs`'s happy-path script that turn one is a
/// VALID builder turn; the cancel fires during turn one's sleep, and the
/// guard refuses turn two.
fn two_valid_turns() -> Vec<LlmResponse> {
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
    ]
}

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

fn config() -> LlmConfig {
    LlmConfig {
        backend: LlmBackend::Ollama,
        model: "gpt-oss:120b".to_owned(),
        temperature: 0.2,
        max_tokens: 1024,
        reasoning_effort: None,
    }
}

/// The compose-runner factory override: the real `compose_strategy_core` over
/// the scripted provider, wired through the context the server hands the
/// spawned task (same pool — `db` is the server's own handle).
fn scripted_compose_runner(script: Vec<LlmResponse>, delay: Duration, db: Db) -> ComposeRunner {
    std::sync::Arc::new(move |ctx: ComposeRunCtx| {
        let script = script.clone();
        let db = db.clone();
        let fut: std::pin::Pin<Box<dyn Future<Output = Result<ComposeResult, BusError>> + Send>> =
            Box::pin(async move {
                let clock = FakeClock::at(1_700_000_000_000);
                let deps = ComposeDeps {
                    wiring: ComposeWiring {
                        provider: SlowScriptedProvider {
                            scripts: Mutex::new(script.into()),
                            delay,
                        },
                        llm_repo: SqliteLlmCallRepo::with_deps(db.pool().clone(), clock),
                        redactor: Redactor::from_config(vec![FAKE_KEY.to_owned()]),
                        prices: test_prices(),
                        clock,
                        prompt: TEST_PROMPT.to_owned(),
                        key_source: Some(CredentialSource::ConfigDir),
                        config: config(),
                    },
                    strategy_repo: SqliteStrategyRepo::new(db.pool().clone()),
                };
                compose_strategy_core(
                    &ctx.run_id,
                    deps,
                    &ctx.nl_target,
                    ctx.sink.as_ref(),
                    ctx.cancelled,
                )
                .await
            });
        fut
    })
}

// ---------------------------------------------------------------------------
// (i) the demo stream: ordered bus events, one result, op_id == runId
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn demo_stream_delivers_ordered_bus_events_then_one_result() {
    let ts = spawn_server(ServerOptions::default()).await;

    let (status, body) = post_op(&ts, "/api/v1/ops/start-demo-stream", json!({ "steps": 5 })).await;
    assert_eq!(
        status,
        reqwest::StatusCode::ACCEPTED,
        "202 before any work: {body:?}"
    );
    let op_id = body["op_id"].as_str().expect("op_id string").to_owned();

    // Attach and read to the end.
    let resp = get_text(&ts, &format!("/api/v1/ops/{op_id}/events"), &ts.app_token).await;
    assert_eq!(
        resp.headers()["content-type"],
        "text/event-stream",
        "the operation stream is SSE"
    );
    let frames = read_all_frames(resp).await;

    let names: Vec<&str> = frames.iter().map(|f| f.event.as_str()).collect();
    assert_eq!(
        names,
        vec!["bus", "bus", "bus", "bus", "bus", "result"],
        "seq 0..4 arrive as bus events in order, then exactly one result: {frames:?}"
    );
    for (i, frame) in frames.iter().enumerate() {
        let expected = u32::try_from(i).expect("index fits u32");
        assert_eq!(frame.id, expected, "frame ids are the seqs 0..=5, in order");
    }
    for frame in frames.iter().take(5) {
        assert_eq!(
            frame.data["runId"],
            json!(op_id),
            "the op_id equals every event's run_id"
        );
        assert_eq!(
            frame.data["seq"],
            json!(frame.id),
            "the wire seq matches the SSE id"
        );
    }
    // The terminal: the command's success DTO, exactly one, at seq 5.
    assert_eq!(frames[5].data["runId"], json!(op_id));
    assert_eq!(
        frames[5].data["emitted"],
        json!(5),
        "the stream emitted 5 events"
    );
    assert_eq!(frames[5].data["cancelled"], json!(false));
}

// ---------------------------------------------------------------------------
// (ii) Last-Event-ID resume: exactly the unseen suffix
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_event_id_resume_replays_exactly_the_unseen_suffix() {
    let ts = spawn_server(ServerOptions::default()).await;
    let (_status, body) =
        post_op(&ts, "/api/v1/ops/start-demo-stream", json!({ "steps": 5 })).await;
    let op_id = body["op_id"].as_str().expect("op_id").to_owned();

    // Read two events, then drop the connection mid-stream.
    let resp = get_text(&ts, &format!("/api/v1/ops/{op_id}/events"), &ts.app_token).await;
    let mut buf: Vec<u8> = Vec::new();
    let mut resp = resp;
    loop {
        let chunk = resp.chunk().await.expect("chunk").expect("stream open");
        buf.extend_from_slice(&chunk);
        if parse_frames(&buf).len() >= 2 {
            break;
        }
    }
    drop(resp);
    let seen = parse_frames(&buf);
    assert_eq!(
        seen.iter().map(|f| f.id).collect::<Vec<_>>(),
        vec![0, 1],
        "the first attach saw the stream head"
    );

    // Reconnect with Last-Event-ID: 1 — the suffix, no dups, no gaps.
    let resumed = client()
        .get(format!("{}/api/v1/ops/{op_id}/events", ts.base))
        .header("Authorization", auth(&ts.app_token))
        .header("Last-Event-ID", "1")
        .send()
        .await
        .expect("reconnect");
    let frames = read_all_frames(resumed).await;
    assert_eq!(
        frames
            .iter()
            .map(|f| (f.id, f.event.as_str()))
            .collect::<Vec<_>>(),
        vec![(2, "bus"), (3, "bus"), (4, "bus"), (5, "result")],
        "only events with seq > 1 replay, then the terminal, no dups and no gaps: {frames:?}"
    );
}

// ---------------------------------------------------------------------------
// (iii) dropping the SSE connection does NOT cancel the operation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_sse_connection_does_not_cancel_a_backtest_operation() {
    let ts = spawn_server(ServerOptions::default()).await;
    let seed_state = ts.desktop().await;
    let version_id = seed_version(&seed_state).await;

    let (status, body) = post_op(
        &ts,
        "/api/v1/ops/run-backtest-version",
        json!({ "versionId": version_id.as_str() }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "{body:?}");
    let op_id = body["op_id"].as_str().expect("op_id").to_owned();

    // Open the stream and drop it immediately, never reconnecting.
    let resp = get_text(&ts, &format!("/api/v1/ops/{op_id}/events"), &ts.app_token).await;
    drop(resp);

    // The operation still runs to its terminal, and its result is persisted.
    let status_body = await_terminal(&ts, &op_id).await;
    assert_eq!(status_body["state"], json!("done"), "{status_body:?}");
    let run_id = status_body["result"]["runId"]
        .as_str()
        .expect("the terminal result carries the persisted run id")
        .to_owned();

    // The run `get-backtest-run` over HTTP can answer for afterwards.
    let resp = post_json(
        &ts,
        "/api/v1/get-backtest-run",
        &ts.app_token,
        json!({ "runId": run_id }),
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "the run persisted");
    let fetched: Value = resp.json().await.expect("run dto");
    assert_eq!(fetched["runId"], json!(run_id), "the same run comes back");
    assert!(
        fetched["tradeCount"].as_u64().expect("trade count") > 0,
        "the fixture run produced trades: {fetched:?}"
    );
}

// ---------------------------------------------------------------------------
// (iv) late attach: the whole buffer, then the terminal, then close
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_attach_receives_the_whole_buffer_then_the_terminal() {
    let ts = spawn_server(ServerOptions::default()).await;
    let (_status, body) =
        post_op(&ts, "/api/v1/ops/start-demo-stream", json!({ "steps": 3 })).await;
    let op_id = body["op_id"].as_str().expect("op_id").to_owned();
    let done = await_terminal(&ts, &op_id).await;
    assert_eq!(done["state"], json!("done"), "{done:?}");

    // A client that connects only AFTER the terminal: no Last-Event-ID, so it
    // receives the whole buffered stream and the terminal, then the stream ends.
    let frames =
        read_all_frames(get_text(&ts, &format!("/api/v1/ops/{op_id}/events"), &ts.app_token).await)
            .await;
    let ids: Vec<u32> = frames.iter().map(|f| f.id).collect();
    assert_eq!(
        ids,
        vec![0, 1, 2, 3],
        "the whole buffer, no dups: {frames:?}"
    );
    assert_eq!(frames.last().expect("terminal").event, "result");
}

// ---------------------------------------------------------------------------
// (v) injectable retention: progress expiry, then full expiry
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_progress_events_leave_only_the_terminal_then_the_record_vanishes() {
    let ts = spawn_server(ServerOptions {
        sweep: Some(fast_sweep(25, 120, 500)),
        compose_runner: None,
    })
    .await;
    let (_status, body) =
        post_op(&ts, "/api/v1/ops/start-demo-stream", json!({ "steps": 2 })).await;
    let op_id = body["op_id"].as_str().expect("op_id").to_owned();
    let done = await_terminal(&ts, &op_id).await;
    assert_eq!(done["state"], json!("done"), "{done:?}");

    // Still inside the progress window: the late attach sees everything.
    let early =
        read_all_frames(get_text(&ts, &format!("/api/v1/ops/{op_id}/events"), &ts.app_token).await)
            .await;
    assert_eq!(
        early.iter().map(|f| f.id).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "inside the window the whole buffer replays"
    );

    // Past the progress window (120ms): only the terminal remains.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let late =
        read_all_frames(get_text(&ts, &format!("/api/v1/ops/{op_id}/events"), &ts.app_token).await)
            .await;
    assert_eq!(
        late.iter()
            .map(|f| (f.id, f.event.as_str()))
            .collect::<Vec<_>>(),
        vec![(2, "result")],
        "after the progress window only the terminal replays: {late:?}"
    );

    // Past the terminal window (500ms): the record itself is gone.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let resp = get_text(&ts, &format!("/api/v1/ops/{op_id}"), &ts.app_token).await;
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "expired -> 404"
    );
    let body: Value = resp.json().await.expect("error body");
    assert_eq!(body["code"], json!("op_unknown"), "{body:?}");
    assert!(
        body["message"].as_str().expect("message").contains(&op_id),
        "the message names the asked-for id: {body:?}"
    );
}

// ---------------------------------------------------------------------------
// (vi) compose-cancel is the core's cancelled OUTCOME, and the latch empties
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compose_cancel_lands_as_the_cancelled_outcome_and_the_latch_empties() {
    let ts = spawn_server(ServerOptions {
        sweep: None,
        compose_runner: Some(Box::new(|db: &Db| {
            scripted_compose_runner(two_valid_turns(), Duration::from_millis(300), db.clone())
        })),
    })
    .await;

    let (status, body) = post_op(
        &ts,
        "/api/v1/ops/compose-strategy",
        json!({ "nl_target": "RSI(14) oversold long with an EMA(200) trend filter" }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "{body:?}");
    let op_id = body["op_id"].as_str().expect("op_id").to_owned();

    // Cancel while turn one is still sleeping. The route answers true because a
    // run is in flight under that id.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let cancel_resp = post_json(
        &ts,
        "/api/v1/compose-cancel",
        &ts.app_token,
        json!({ "runId": op_id }),
    )
    .await;
    assert_eq!(cancel_resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        cancel_resp.json::<Value>().await.expect("bool"),
        json!(true),
        "a live compose run was cancelled"
    );

    // The terminal is the core's cancelled outcome — a SUCCESS DTO, not an error.
    let frames =
        read_all_frames(get_text(&ts, &format!("/api/v1/ops/{op_id}/events"), &ts.app_token).await)
            .await;
    let terminal = frames.last().expect("terminal frame");
    assert_eq!(
        terminal.event, "result",
        "cancellation is a normal return: {frames:?}"
    );
    assert_eq!(terminal.data["cancelled"], json!(true));
    assert!(
        terminal.data["strategy"].is_null(),
        "a cancelled run persists nothing: {:?}",
        terminal.data
    );
    for frame in &frames {
        assert_eq!(
            frame.data["runId"],
            json!(op_id),
            "compose events carry the run id"
        );
    }

    // The latch map is empty afterwards: the same id answers "nothing in flight".
    let again = post_json(
        &ts,
        "/api/v1/compose-cancel",
        &ts.app_token,
        json!({ "runId": op_id }),
    )
    .await;
    assert_eq!(
        again.json::<Value>().await.expect("bool"),
        json!(false),
        "finish_compose_run deregistered the run on every exit path"
    );
}

// ---------------------------------------------------------------------------
// (vii) single flight: a concurrent duplicate is refused with the typed error
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_concurrent_duplicate_backtest_is_refused_with_the_typed_busy_error() {
    let ts = spawn_server(ServerOptions::default()).await;
    let seed_state = ts.desktop().await;
    let version_id = seed_version(&seed_state).await;

    // The FIRST operation in flight, held open for the whole scenario: this is
    // the SERVER'S OWN latch (the state the op cores take), held exactly the
    // way a genuinely in-flight first POST holds it — which makes the race
    // deterministic instead of timing-dependent.
    let guard = ts
        .state
        .desktop()
        .begin_operation(OperationKey::Backtest(VersionId::new(
            version_id.as_str().to_owned(),
        )))
        .expect("the latch is free at scenario start");

    let (status, body) = post_op(
        &ts,
        "/api/v1/ops/run-backtest-version",
        json!({ "versionId": version_id.as_str() }),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::ACCEPTED,
        "the 202 precedes all work: {body:?}"
    );
    let op_id = body["op_id"].as_str().expect("op_id").to_owned();

    // The refusal arrives as the operation's terminal error event (the recorded
    // reading), with the existing typed busy code.
    let done = await_terminal(&ts, &op_id).await;
    assert_eq!(done["state"], json!("failed"), "{done:?}");
    assert_eq!(done["error"]["code"], json!("busy"), "{done:?}");

    // Over the stream: `started` at 0, then the single `error` terminal at 1.
    let frames =
        read_all_frames(get_text(&ts, &format!("/api/v1/ops/{op_id}/events"), &ts.app_token).await)
            .await;
    assert_eq!(
        frames
            .iter()
            .map(|f| (f.id, f.event.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "started"), (1, "error")],
        "one started frame, one error terminal: {frames:?}"
    );
    assert_eq!(frames[1].data["code"], json!("busy"));

    // With the latch released, the same request succeeds — the refusal was the
    // typed busy error, not a broken route.
    drop(guard);
    let (status2, body2) = post_op(
        &ts,
        "/api/v1/ops/run-backtest-version",
        json!({ "versionId": version_id.as_str() }),
    )
    .await;
    assert_eq!(status2, reqwest::StatusCode::ACCEPTED, "{body2:?}");
    let done2 = await_terminal(&ts, body2["op_id"].as_str().expect("op_id")).await;
    assert_eq!(done2["state"], json!("done"), "{done2:?}");

    // And two genuinely concurrent POSTs: any refusal between them is the typed
    // busy error (never anything else), and at least one run completes.
    let (a, b) = tokio::join!(
        post_op(
            &ts,
            "/api/v1/ops/run-backtest-version",
            json!({ "versionId": version_id.as_str() })
        ),
        post_op(
            &ts,
            "/api/v1/ops/run-backtest-version",
            json!({ "versionId": version_id.as_str() })
        ),
    );
    assert_eq!(a.0, reqwest::StatusCode::ACCEPTED);
    assert_eq!(b.0, reqwest::StatusCode::ACCEPTED);
    let (done_a, done_b) = tokio::join!(
        await_terminal(&ts, a.1["op_id"].as_str().expect("op_id a")),
        await_terminal(&ts, b.1["op_id"].as_str().expect("op_id b")),
    );
    for (label, terminal) in [("a", &done_a), ("b", &done_b)] {
        if terminal["state"] == json!("failed") {
            assert_eq!(
                terminal["error"]["code"],
                json!("busy"),
                "the only acceptable concurrent refusal is the typed busy error ({label}): {terminal:?}"
            );
        }
    }
    assert!(
        done_a["state"] == json!("done") || done_b["state"] == json!("done"),
        "at least one of the pair completes: {done_a:?} / {done_b:?}"
    );
}

// ---------------------------------------------------------------------------
// (viii) the scripted credential value reaches no frame, body, or log line
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fake_credential_value_never_reaches_a_frame_a_body_or_the_log() {
    let ts = spawn_server(ServerOptions {
        sweep: None,
        compose_runner: Some(Box::new(|db: &Db| {
            scripted_compose_runner(two_valid_turns(), Duration::from_millis(60), db.clone())
        })),
    })
    .await;

    // Run a compose to its cancelled terminal (the cancel also exercises the
    // plain-route bodies), capturing every response body on the way.
    let (status, body) = post_op(
        &ts,
        "/api/v1/ops/compose-strategy",
        json!({ "nl_target": "any target; the scripted provider never sees a key" }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "{body:?}");
    let op_id = body["op_id"].as_str().expect("op_id").to_owned();
    let cancel = post_json(
        &ts,
        "/api/v1/compose-cancel",
        &ts.app_token,
        json!({ "runId": op_id }),
    )
    .await;
    let cancel_body: Value = cancel.json().await.expect("cancel body");
    let status_body = await_terminal(&ts, &op_id).await;
    let frames =
        read_all_frames(get_text(&ts, &format!("/api/v1/ops/{op_id}/events"), &ts.app_token).await)
            .await;

    let mut surfaces: Vec<String> = vec![
        json!(body).to_string(),
        json!(cancel_body).to_string(),
        json!(status_body).to_string(),
    ];
    for frame in &frames {
        surfaces.push(frame.data.to_string());
    }
    surfaces.extend(ts.log.lines());
    for surface in &surfaces {
        assert!(
            !surface.contains(FAKE_KEY),
            "the credential value leaked into a surface: {surface}"
        );
    }
    // The redactor really had the value under its control: the run happened.
    assert_eq!(status_body["state"], json!("done"), "{status_body:?}");
    assert_eq!(
        status_body["result"]["cancelled"],
        json!(true),
        "the scripted run cancelled as staged"
    );
}

// ---------------------------------------------------------------------------
// AC-3 — the version-skew refusal (r3.s3.w5)
// ---------------------------------------------------------------------------

/// A raw TCP stub answering the handshake with the NEXT API generation and a
/// body that is NOT a handshake body — so "the client refused on the version
/// header" and "the client never parsed the body" are observable separately:
/// a client that parsed first would answer `Unreachable { unreadable body }`,
/// not `Skew`.
#[tokio::test]
async fn a_version_skewed_handshake_is_refused_before_any_body_is_parsed() {
    use tokio::io::AsyncWriteExt as _;

    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
    let addr = probe.local_addr().expect("probe addr");
    drop(probe);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind stub");
    let base = format!("http://{}", listener.local_addr().expect("stub local"));
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept the handshake");
        // Drain the request head (and any body the client sent).
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            use tokio::io::AsyncReadExt as _;
            stream
                .read_exact(&mut byte)
                .await
                .expect("read request byte");
            buf.push(byte[0]);
            let text = String::from_utf8_lossy(&buf);
            if text.contains("\r\n\r\n") {
                break;
            }
        }
        // The sentinel body: deliberately not a handshake DTO. A client that
        // deserialized before checking the version would answer with an
        // "unreadable body" refusal and this test would catch the ordering.
        let body = "{\"sentinel\": \"if you can read this, the body was parsed\"}";
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     x-pulse-api-version: 2\r\nconnection: close\r\n\
                     content-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .expect("answer the skewed handshake");
    });

    let Err(error) = ServerClient::connect(&base, "any-token").await else {
        panic!("a version-2 server must be refused");
    };
    handle.await.expect("stub completes");

    match error {
        ClientError::Skew { server_api_version } => {
            assert_eq!(
                server_api_version, 2,
                "the skew names the generation the server announced"
            );
        }
        other => panic!(
            "a skewed handshake must refuse with Skew, got {other:?} — the version \
             header must be checked before any body byte is parsed"
        ),
    }
}
