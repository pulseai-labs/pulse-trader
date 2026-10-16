//! r5.s3.w1 (PDR-0037, d106) — the example-mark store contract.
//!
//! A trader marks an example trade in the app and an agent marks one through
//! `pulse mcp`; each mark names one candle, a side, a setup label, an optional
//! note and its writer. Every write goes through ONE application use case whose
//! first check is the holdout fence (G1, G2), so a mark whose candle reaches an
//! open freeze's holdout start is refused for BOTH writers — before any candle
//! lookup. Reads hide marks a later freeze covers and say how many (Q4).
//!
//! **Both edges are driven as the product drives them.** The trader writes over
//! `POST /api/v1/marks` with a real app token, so the stored writer is the
//! token's LABEL; the agent writes over the real `pulse mcp` stdio transport, so
//! the stored writer is the self-reported agent name. Neither leg reaches into
//! the application ring behind the other's back, and each edge reads the open
//! freeze inside the call it serves.
//!
//! **The candles are the fixture's own.** Every bar open time below is read out
//! of the committed `btcusdt-1m-store` at run time, never written down here: the
//! fixture's 15m and 4h snapshots are the HEAD the store serves, and a hard-coded
//! time would silently stop testing anything the day the fixture moved.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::path::{Path, PathBuf};

use chrono::SecondsFormat;
use pulse::{
    CandleSeries, CandleSeriesRepository, CandleStore, Db, FakeClock, MarkWriter, MarkWriterKind,
    OpenFreezeRequest, Pair, SqliteCertificationFreezeRepo, Timeframe, WithdrawMarkRequest,
    WriteMarkRequest, withdraw_mark_core, write_mark_core,
};
use serde_json::{Value, json};
use support::server::{ServerOptions, TestServer, spawn_server};
use tempfile::TempDir;

/// The holdout start every freeze case uses — inside the fixture's January 2025
/// range, a UTC midnight and therefore a 15m and a 4h boundary at once
/// (`tests/chart_read.rs`'s own constant).
const HOLDOUT_MS: i64 = 1_736_985_600_000;

/// The instant the test freeze opens at (deterministic, injected).
const OPENED_MS: i64 = 1_760_000_000_000;

/// The label `tests/support/server.rs` issues the app token under — the trader
/// writer's name, asserted by value.
const APP_LABEL: &str = "w2-app";

/// The fixture's pair.
fn pair() -> Pair {
    Pair::new("BTCUSDT")
}

/// A store over the server's own (copied) fixture data dir.
fn store(dir: &Path) -> CandleStore {
    CandleStore::with_base_dir(dir.to_path_buf())
}

/// The HEAD series for one timeframe, read straight from the fixture — the
/// bars the tests mark are chosen from it, never written down.
fn series(dir: &Path, timeframe: Timeframe) -> CandleSeries {
    store(dir)
        .load_head(&pair(), timeframe)
        .expect("the fixture HEAD reads")
        .expect("the fixture has a HEAD")
        .series
}

/// The fixture's own pair of ADJACENT 15m bars whose shared instant is at or
/// before `at_ms`. The first bar CLOSES exactly on that instant — legal to mark
/// under a freeze opened there — and the second OPENS exactly on it, which the
/// same freeze refuses. Both bars are read out of the snapshot, so the test
/// neither assumes the fixture is aligned to a UTC midnight nor invents a time
/// the store has no candle for.
///
/// Returns `(the bar that closes on it, the bar that opens on it)`.
fn adjacent_bars_at_or_before(m15: &CandleSeries, at_ms: i64) -> (i64, i64) {
    let step = Timeframe::M15.duration_ms();
    m15.candles
        .windows(2)
        .find(|pair| {
            let (first, second) = (&pair[0], &pair[1]);
            second.open_time == first.open_time + step && first.open_time + step <= at_ms
        })
        .map_or_else(
            || {
                let first = m15.candles.first().map(|c| (c.open_time, c.close_time));
                let last = m15.candles.last().map(|c| (c.open_time, c.close_time));
                panic!(
                    "the fixture holds no two adjacent 15m bars at or before {at_ms}: {} bars, \
                     first {first:?}, last {last:?}, step {step}",
                    m15.candles.len()
                )
            },
            |pair| (pair[0].open_time, pair[1].open_time),
        )
}

/// Open one freeze through the store, with the injected clock.
async fn open_freeze(server: &TestServer, holdout_start_ms: i64) {
    SqliteCertificationFreezeRepo::with_deps(server.db.pool().clone(), FakeClock::at(OPENED_MS))
        .open(&OpenFreezeRequest {
            holdout_start_ms,
            h: 12,
            alpha: "0.0463".to_owned(),
            holdout_test: "C1: z = z(1 - 0.05/H)".to_owned(),
        })
        .await
        .expect("open the freeze");
}

/// An epoch-ms instant as the RFC 3339 UTC string the MCP tools take.
fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .expect("the instant is in range")
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// An RFC 3339 string's instant, epoch ms — what a time comparison must be on,
/// since seconds and millisecond precision are both legal RFC 3339.
fn instant(value: &Value) -> i64 {
    chrono::DateTime::parse_from_rfc3339(value.as_str().expect("an RFC 3339 string"))
        .expect("parse the instant")
        .timestamp_millis()
}

/// How many `example_mark` rows the store holds.
async fn mark_rows(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM example_mark")
        .fetch_one(db.pool())
        .await
        .expect("count the marks")
}

/// How many rows one table holds — the "a write touches nothing else" probe.
async fn rows_in(db: &Db, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(db.pool())
        .await
        .unwrap_or_else(|e| panic!("count {table}: {e}"))
}

/// One app-scope call: the status and the parsed body.
async fn post(server: &TestServer, token: &str, path: &str, body: Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{}{path}", server.base))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("POST the app route");
    let status = response.status().as_u16();
    let body: Value = response.json().await.expect("the route's JSON");
    (status, body)
}

/// Write one mark through the app route and hand back its id, failing loudly on
/// anything but a 201.
async fn app_write(server: &TestServer, body: Value) -> String {
    let (status, body) = post(server, &server.app_token, "/api/v1/marks", body).await;
    assert_eq!(status, 201, "the app write must be created: {body}");
    body["markId"]
        .as_str()
        .expect("the created mark's id")
        .to_owned()
}

/// The body of one app write, with the candle's open time filled in.
fn write_body(timeframe: &str, bar_open_ms: i64, setup: &str) -> Value {
    json!({
        "pair": "BTCUSDT",
        "timeframe": timeframe,
        "barTime": bar_open_ms.to_string(),
        "side": "long",
        "setup": setup,
        "note": "the fourth touch of the same shelf",
    })
}

/// The agent the stdio world serves under — lowercased by `AgentName::parse`.
const AGENT_NAME: &str = "test-agent";

/// The tables a mark write must leave exactly as it found them: every paper,
/// order and credential store the risk gate's least-privilege control names.
const WATCHED: [&str; 8] = [
    "paper_session",
    "paper_event",
    "trade",
    "backtest_run",
    "client_token",
    "strategy",
    "certification_freeze",
    "llm_call",
];

/// The MCP world: a migrated db, a copied fixture store, and one live stdio
/// client (`pulse mcp --local --agent-name Test-Agent`). The freeze, when a case
/// wants one, is opened BEFORE the client spawns and read by the tool itself.
struct Mcp {
    _tmp_db: TempDir,
    _tmp_store: TempDir,
    client: rmcp::service::RunningService<rmcp::RoleClient, ()>,
    store_dir: PathBuf,
}

/// Build the stdio world, optionally with an open freeze as its first act.
async fn mcp_world(holdout_start_ms: Option<i64>) -> Mcp {
    let tmp_db = TempDir::new().expect("tempdir");
    let (db_path, db) = support::mcp::migrated_db(&tmp_db).await;
    if let Some(holdout) = holdout_start_ms {
        SqliteCertificationFreezeRepo::with_deps(db.pool().clone(), FakeClock::at(OPENED_MS))
            .open(&OpenFreezeRequest {
                holdout_start_ms: holdout,
                h: 12,
                alpha: "0.0463".to_owned(),
                holdout_test: "C1: z = z(1 - 0.05/H)".to_owned(),
            })
            .await
            .expect("open the freeze");
    }
    let tmp_store = TempDir::new().expect("tempdir");
    let store_dir = tmp_store.path().join("store");
    support::mcp::copy_tree(
        &support::mcp::manifest(support::mcp::FIXTURE_STORE),
        &store_dir,
    );
    let client = support::mcp::spawn_client(&db_path, &store_dir).await;
    Mcp {
        _tmp_db: tmp_db,
        _tmp_store: tmp_store,
        client,
        store_dir,
    }
}

/// The `write_mark` arguments one MCP call carries.
fn mcp_write_args(timeframe: &str, bar_open_ms: i64, setup: &str) -> Value {
    json!({
        "pair": "BTCUSDT",
        "timeframe": timeframe,
        "bar_time": rfc3339(bar_open_ms),
        "side": "long",
        "setup": setup,
        "note": "the fourth touch of the same shelf",
    })
}

// ---------------------------------------------------------------------------
// Both writers: what lands, and with whose name on it
// ---------------------------------------------------------------------------

/// d106: a trader's app write stores `trader` with the token's LABEL, and every
/// field lists back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_app_write_stores_the_token_label_as_the_trader_writer_and_lists_back() {
    let server = spawn_server(ServerOptions::default()).await;
    let bar = series(&server.data_dir, Timeframe::M15).candles[0].open_time;

    let id = app_write(&server, write_body("15m", bar, "support-rejection longs")).await;

    let (status, body) = post(
        &server,
        &server.app_token,
        "/api/v1/marks/list",
        json!({ "pair": "BTCUSDT" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let marks = body["marks"].as_array().expect("a marks array");
    assert_eq!(marks.len(), 1, "the write lists back: {body}");
    let mark = &marks[0];
    assert_eq!(mark["markId"], id);
    assert_eq!(mark["pair"], "BTCUSDT");
    assert_eq!(mark["timeframe"], "15m");
    assert_eq!(mark["barTime"], bar.to_string());
    assert_eq!(mark["side"], "long");
    assert_eq!(mark["setup"], "support-rejection longs");
    assert_eq!(mark["note"], "the fourth touch of the same shelf");
    assert_eq!(
        mark["writer"]["kind"], "trader",
        "the app writes as the trader: {mark}"
    );
    assert_eq!(
        mark["writer"]["name"], APP_LABEL,
        "the writer's name is the token's label, never the token: {mark}"
    );
    assert_eq!(
        mark["writer"]["selfReported"], false,
        "a trader mark is not self-reported (PDR-0015): {mark}"
    );
    assert!(
        mark["dataVersion"].as_str().is_some_and(|v| !v.is_empty()),
        "the row names the HEAD snapshot the candle was found in: {mark}"
    );
    assert!(
        mark["createdAt"]
            .as_str()
            .is_some_and(|v| v.parse::<i64>().is_ok()),
        "createdAt crosses as a millisecond string: {mark}"
    );
    assert!(
        body["hiddenByHoldout"].is_null(),
        "no freeze is open, so nothing is hidden: {body}"
    );
}

/// d106: an agent's write over `pulse mcp` stores `agent` with the agent's own
/// self-reported name, and lists back the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_write_over_mcp_stores_the_agent_name_as_its_writer() {
    let mcp = mcp_world(None).await;
    let bar = series(&mcp.store_dir, Timeframe::M15).candles[0].open_time;

    let written = support::mcp::call(
        &mcp.client,
        "write_mark",
        mcp_write_args("15m", bar, "support-rejection longs"),
    )
    .await;
    assert_eq!(written["pair"], "BTCUSDT", "{written}");
    // The instant, not the spelling: both `2025-01-01T00:00:00Z` and its
    // millisecond-precision form are RFC 3339, and the tools' own renderer picks
    // the second.
    assert_eq!(
        instant(&written["bar_time"]),
        bar,
        "the mark names the candle that was asked for: {written}"
    );
    assert_eq!(written["writer"]["kind"], "agent", "{written}");
    assert_eq!(
        written["writer"]["name"], AGENT_NAME,
        "the writer's name is the resolved agent identity: {written}"
    );
    assert_eq!(
        written["writer"]["self_reported"], true,
        "an agent mark says it is self-reported (PDR-0015): {written}"
    );

    let listed = support::mcp::call(&mcp.client, "list_marks", json!({ "pair": "BTCUSDT" })).await;
    let marks = listed["marks"].as_array().expect("a marks array");
    assert_eq!(marks.len(), 1, "the write lists back: {listed}");
    assert_eq!(marks[0]["mark_id"], written["mark_id"]);
    assert_eq!(marks[0]["setup"], "support-rejection longs");
    assert_eq!(marks[0]["note"], "the fourth touch of the same shelf");
    assert_eq!(marks[0]["side"], "long");
    assert_eq!(marks[0]["writer"]["kind"], "agent");
    assert!(
        marks[0]["created_at"]
            .as_str()
            .is_some_and(|v| chrono::DateTime::parse_from_rfc3339(v).is_ok()),
        "MCP times cross as RFC 3339: {listed}"
    );
}

// ---------------------------------------------------------------------------
// The fence: G2, for both writers
// ---------------------------------------------------------------------------

/// Q4/G2: a candle that ENDS exactly at the holdout start is legal — the
/// holdout is open at that instant, so the bar never reaches into it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_candle_ending_exactly_at_the_holdout_start_is_allowed() {
    let server = spawn_server(ServerOptions::default()).await;
    let step = Timeframe::M15.duration_ms();
    let (legal, opens_on_the_fence) =
        adjacent_bars_at_or_before(&series(&server.data_dir, Timeframe::M15), HOLDOUT_MS);
    let boundary = legal + step;
    assert_eq!(opens_on_the_fence, boundary);
    open_freeze(&server, boundary).await;

    let (status, body) = post(
        &server,
        &server.app_token,
        "/api/v1/marks",
        write_body("15m", legal, "the last legal bar"),
    )
    .await;
    assert_eq!(
        status, 201,
        "a bar closing exactly at the holdout start is legal: {body}"
    );
}

/// d106/G2: a candle STRADDLING the holdout start is refused for both writers,
/// and the refusal names the holdout, not a missing candle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_candle_straddling_the_holdout_start_is_refused_for_both_writers() {
    // The 4h bar that OPENS before 01:00Z on the fixture's own 16 January and
    // CLOSES after it: a real candle of the fixture whose body spans the fence.
    let probe = spawn_server(ServerOptions::default()).await;
    let straddler = series(&probe.data_dir, Timeframe::H4)
        .candles
        .iter()
        .find(|candle| {
            let inside = candle.open_time + Timeframe::H4.duration_ms() / 4;
            candle.open_time < inside && inside < candle.close_time
        })
        .expect("the fixture holds the 4h bar that straddles 01:00Z")
        .clone();
    let holdout_start = straddler.open_time + Timeframe::H4.duration_ms() / 4;
    assert!(
        straddler.close_time > holdout_start,
        "the chosen bar closes after the freeze opens"
    );

    // The trader, over the app route.
    let server = spawn_server(ServerOptions::default()).await;
    open_freeze(&server, holdout_start).await;
    let (status, body) = post(
        &server,
        &server.app_token,
        "/api/v1/marks",
        write_body("4h", straddler.open_time, "straddles the fence"),
    )
    .await;
    assert_eq!(status, 422, "the straddling bar is refused: {body}");
    assert_eq!(body["code"], "holdout", "{body}");
    assert_eq!(
        mark_rows(&server.db).await,
        0,
        "a refused write writes nothing"
    );

    // The agent, over the MCP transport.
    let mcp = mcp_world(Some(holdout_start)).await;
    let refused = support::mcp::call_err(
        &mcp.client,
        "write_mark",
        mcp_write_args("4h", straddler.open_time, "straddles the fence"),
    )
    .await;
    assert_eq!(
        refused["code"], "holdout",
        "the same fence refuses the agent: {refused}"
    );
}

/// d106/G2: a candle OPENING at or after the holdout start is refused for both
/// writers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_candle_after_the_holdout_start_is_refused_for_both_writers() {
    let probe = spawn_server(ServerOptions::default()).await;
    let bar = series(&probe.data_dir, Timeframe::M15)
        .candles
        .iter()
        .find(|candle| candle.open_time == HOLDOUT_MS)
        .expect("the fixture holds a bar opening at the holdout start")
        .open_time;
    assert!(bar >= HOLDOUT_MS);

    let server = spawn_server(ServerOptions::default()).await;
    open_freeze(&server, HOLDOUT_MS).await;
    let (status, body) = post(
        &server,
        &server.app_token,
        "/api/v1/marks",
        write_body("15m", bar, "inside the holdout"),
    )
    .await;
    assert_eq!(status, 422, "a bar at the holdout start is refused: {body}");
    assert_eq!(body["code"], "holdout", "{body}");
    assert_eq!(
        mark_rows(&server.db).await,
        0,
        "a refused write writes nothing"
    );

    let mcp = mcp_world(Some(HOLDOUT_MS)).await;
    let refused = support::mcp::call_err(
        &mcp.client,
        "write_mark",
        mcp_write_args("15m", bar, "inside the holdout"),
    )
    .await;
    assert_eq!(
        refused["code"], "holdout",
        "the same fence refuses the agent: {refused}"
    );
}

/// d106/G2, the ordering law: a time AFTER the holdout start where NO candle
/// exists is refused for the HOLDOUT reason, never for a missing candle — the
/// fence runs before any lookup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_time_after_the_start_with_no_candle_is_refused_for_the_holdout_not_for_a_missing_candle()
{
    let probe = spawn_server(ServerOptions::default()).await;
    let series_m15 = series(&probe.data_dir, Timeframe::M15);
    let last = series_m15.candles.last().expect("the fixture is not empty");
    let step = Timeframe::M15.duration_ms();
    // Ten bars past the snapshot's own end: no candle is there, and the instant
    // is unambiguously after the holdout start.
    let missing = last.close_time + 10 * step;
    assert!(missing > HOLDOUT_MS);
    assert!(
        !series_m15
            .candles
            .iter()
            .any(|candle| candle.open_time == missing),
        "the probe bar must not exist"
    );

    let server = spawn_server(ServerOptions::default()).await;
    open_freeze(&server, HOLDOUT_MS).await;
    let (status, body) = post(
        &server,
        &server.app_token,
        "/api/v1/marks",
        write_body("15m", missing, "no candle here"),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(
        body["code"], "holdout",
        "the fence answers BEFORE the lookup: {body}"
    );
    assert_eq!(
        mark_rows(&server.db).await,
        0,
        "a refused write writes nothing"
    );

    let mcp = mcp_world(Some(HOLDOUT_MS)).await;
    let refused = support::mcp::call_err(
        &mcp.client,
        "write_mark",
        mcp_write_args("15m", missing, "no candle here"),
    )
    .await;
    assert_eq!(
        refused["code"], "holdout",
        "the fence answers BEFORE the lookup for the agent too: {refused}"
    );
}

/// G2: a refusal leaves the table exactly as it was, even when the store already
/// holds another writer's marks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_write_leaves_the_table_unchanged() {
    let server = spawn_server(ServerOptions::default()).await;
    let step = Timeframe::M15.duration_ms();
    let (legal, opens_on_the_fence) =
        adjacent_bars_at_or_before(&series(&server.data_dir, Timeframe::M15), HOLDOUT_MS);
    app_write(&server, write_body("15m", legal, "before the fence")).await;
    let before = mark_rows(&server.db).await;
    assert_eq!(before, 1);

    open_freeze(&server, legal + step).await;
    let (status, _) = post(
        &server,
        &server.app_token,
        "/api/v1/marks",
        write_body("15m", opens_on_the_fence, "after the fence"),
    )
    .await;
    assert_eq!(status, 422);
    assert_eq!(
        mark_rows(&server.db).await,
        before,
        "the refused write added nothing"
    );
}

// ---------------------------------------------------------------------------
// The candle-existence rule, once the fence is clear
// ---------------------------------------------------------------------------

/// Q2: with no freeze open, a time the HEAD snapshot holds no candle for is a
/// `candle_not_found` refusal naming the pair, the timeframe and the time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_freeze_a_missing_candle_is_a_candle_not_found_refusal() {
    let server = spawn_server(ServerOptions::default()).await;
    let m15 = series(&server.data_dir, Timeframe::M15);
    let step = Timeframe::M15.duration_ms();
    let missing = m15.candles[0].open_time - 10 * step;
    assert!(
        !m15.candles.iter().any(|c| c.open_time == missing),
        "the probe bar must not exist"
    );

    let (status, body) = post(
        &server,
        &server.app_token,
        "/api/v1/marks",
        write_body("15m", missing, "a bar that is not there"),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "candle_not_found", "{body}");
    let message = body["message"].as_str().expect("a message");
    assert!(message.contains("BTCUSDT"), "names the pair: {message}");
    assert!(message.contains("15m"), "names the timeframe: {message}");
    assert_eq!(mark_rows(&server.db).await, 0, "nothing was written");

    // The fixture holds no daily snapshot at all, so a 1d mark names a candle
    // the store cannot hold — still the candle's fault, not the fence's.
    let (status, body) = post(
        &server,
        &server.app_token,
        "/api/v1/marks",
        write_body("1d", missing, "a daily bar that is not there"),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "candle_not_found", "{body}");
}

/// Q1: an invalid side, timeframe, setup or note is refused naming the field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_side_timeframe_setup_or_note_names_the_field() {
    let server = spawn_server(ServerOptions::default()).await;
    let bar = series(&server.data_dir, Timeframe::M15).candles[0].open_time;
    let long = "x".repeat(281);

    for (field, body) in [
        (
            "side",
            json!({ "pair": "BTCUSDT", "timeframe": "15m", "barTime": bar.to_string(),
                    "side": "sideways", "setup": "a setup" }),
        ),
        (
            "timeframe",
            json!({ "pair": "BTCUSDT", "timeframe": "1h", "barTime": bar.to_string(),
                    "side": "long", "setup": "a setup" }),
        ),
        (
            "setup",
            json!({ "pair": "BTCUSDT", "timeframe": "15m", "barTime": bar.to_string(),
                    "side": "long", "setup": "   " }),
        ),
        (
            "note",
            json!({ "pair": "BTCUSDT", "timeframe": "15m", "barTime": bar.to_string(),
                    "side": "long", "setup": "a setup", "note": long }),
        ),
        (
            "pair",
            json!({ "pair": "not a pair", "timeframe": "15m", "barTime": bar.to_string(),
                    "side": "long", "setup": "a setup" }),
        ),
    ] {
        let (status, response) = post(&server, &server.app_token, "/api/v1/marks", body).await;
        assert_eq!(status, 422, "{field}: {response}");
        let message = response["message"].as_str().expect("a message");
        assert!(
            message.contains(field),
            "the refusal names the field `{field}`: {message}"
        );
    }
    assert_eq!(mark_rows(&server.db).await, 0, "no invalid write landed");
}

// ---------------------------------------------------------------------------
// The read-side hide (Q4)
// ---------------------------------------------------------------------------

/// Q4: a freeze opened AFTER a write covers that mark, so the list hides it and
/// counts it — the row itself stays stored and immutable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_freeze_opened_after_a_write_hides_that_mark_and_counts_it() {
    let server = spawn_server(ServerOptions::default()).await;
    let m15 = series(&server.data_dir, Timeframe::M15);
    let (before, after) = adjacent_bars_at_or_before(&m15, HOLDOUT_MS);
    let boundary = before + Timeframe::M15.duration_ms();

    // Both marks are written while NO freeze is open, so both are legal when they
    // land — the freeze that follows is what covers one of them.
    app_write(
        &server,
        write_body("15m", before, "written before the freeze"),
    )
    .await;
    app_write(
        &server,
        write_body("15m", after, "written before the freeze"),
    )
    .await;
    assert_eq!(mark_rows(&server.db).await, 2);

    open_freeze(&server, boundary).await;

    let (status, body) = post(&server, &server.app_token, "/api/v1/marks/list", json!({})).await;
    assert_eq!(status, 200, "{body}");
    let marks = body["marks"].as_array().expect("a marks array");
    assert_eq!(
        marks.len(),
        1,
        "only the bar that clears the fence stays listed: {body}"
    );
    assert_eq!(marks[0]["barTime"], before.to_string());
    assert_eq!(
        body["hiddenByHoldout"]["holdoutStartMs"],
        boundary.to_string(),
        "{body}"
    );
    assert_eq!(
        body["hiddenByHoldout"]["count"], 1,
        "the hidden mark is counted: {body}"
    );
    assert_eq!(
        mark_rows(&server.db).await,
        2,
        "hiding is a read-side act — both rows stay stored"
    );
}

// ---------------------------------------------------------------------------
// Withdraw (Q3)
// ---------------------------------------------------------------------------

/// Q3: the trader withdraws any mark, an agent only its own, once. A withdrawn
/// mark leaves the list while its row stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_withdraw_matrix() {
    let server = spawn_server(ServerOptions::default()).await;
    let m15 = series(&server.data_dir, Timeframe::M15);
    let bar = m15.candles[0].open_time;

    // One mark the trader wrote, one an agent wrote.
    let trader_mark = app_write(&server, write_body("15m", bar, "the trader's")).await;
    let agent_mark = {
        let core = write_mark_core(
            &server.desktop().await,
            MarkWriter {
                kind: MarkWriterKind::Agent,
                name: AGENT_NAME.to_owned(),
            },
            WriteMarkRequest {
                pair: "BTCUSDT".to_owned(),
                timeframe: "15m".to_owned(),
                bar_time: (bar + Timeframe::M15.duration_ms()).to_string(),
                side: "long".to_owned(),
                setup: "the agent's".to_owned(),
                note: None,
            },
        )
        .await
        .expect("the agent's mark lands");
        core.mark_id
    };

    // An agent may not withdraw another writer's mark.
    let (status, body) = {
        let state = server.desktop().await;
        match withdraw_mark_core(
            &state,
            MarkWriter {
                kind: MarkWriterKind::Agent,
                name: "someone-else".to_owned(),
            },
            WithdrawMarkRequest {
                mark_id: trader_mark.clone(),
            },
        )
        .await
        {
            Ok(_) => panic!("an agent must not withdraw the trader's mark"),
            Err(error) => (422_u16, json!({ "code": error.code() })),
        }
    };
    assert_eq!(status, 422);
    assert_eq!(body["code"], "not_writer", "{body}");

    // An agent withdraws its own, over the route the app exposes... and the
    // trader withdraws the agent's.
    let (status, body) = post(
        &server,
        &server.app_token,
        "/api/v1/marks/withdraw",
        json!({ "markId": agent_mark }),
    )
    .await;
    assert_eq!(status, 200, "the trader withdraws any mark: {body}");
    assert_eq!(body["markId"], agent_mark);

    // A second withdraw is refused by name.
    let (status, body) = post(
        &server,
        &server.app_token,
        "/api/v1/marks/withdraw",
        json!({ "markId": agent_mark }),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "already_withdrawn", "{body}");

    // An unknown id is `not_found`.
    let (status, body) = post(
        &server,
        &server.app_token,
        "/api/v1/marks/withdraw",
        json!({ "markId": "00000000-0000-0000-0000-000000000000" }),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "not_found", "{body}");

    // The withdrawn mark left the list; its row did not.
    let (_, body) = post(&server, &server.app_token, "/api/v1/marks/list", json!({})).await;
    let marks = body["marks"].as_array().expect("a marks array");
    assert_eq!(marks.len(), 1, "only the un-withdrawn mark lists: {body}");
    assert_eq!(marks[0]["markId"], trader_mark);
    assert_eq!(mark_rows(&server.db).await, 2, "both rows stay stored");
    let withdrawals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM example_mark_withdrawal")
        .fetch_one(server.db.pool())
        .await
        .expect("count the withdrawals");
    assert_eq!(withdrawals, 1);
}

/// Q3: an agent withdraws its own mark, and only its own, over the MCP surface.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_withdraws_its_own_mark_over_mcp_and_only_its_own() {
    let mcp = mcp_world(None).await;
    let bar = series(&mcp.store_dir, Timeframe::M15).candles[0].open_time;
    let own = support::mcp::call(
        &mcp.client,
        "write_mark",
        mcp_write_args("15m", bar, "the agent's own"),
    )
    .await;
    let id = own["mark_id"].as_str().expect("an id").to_owned();

    let withdrawn =
        support::mcp::call(&mcp.client, "withdraw_mark", json!({ "mark_id": id })).await;
    assert_eq!(
        withdrawn["mark_id"], id,
        "an agent withdraws the mark it wrote: {withdrawn}"
    );

    let again = support::mcp::call_err(
        &mcp.client,
        "withdraw_mark",
        json!({ "mark_id": own["mark_id"] }),
    )
    .await;
    assert_eq!(again["code"], "already_withdrawn", "{again}");
}

// ---------------------------------------------------------------------------
// The schema's own laws, and the blast radius
// ---------------------------------------------------------------------------

/// Q3/ADR-0018: the tables hold their own immutability against a raw statement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_triggers_refuse_an_update_and_a_delete_on_both_tables() {
    let server = spawn_server(ServerOptions::default()).await;
    let bar = series(&server.data_dir, Timeframe::M15).candles[0].open_time;
    let id = app_write(&server, write_body("15m", bar, "immutable")).await;
    let (_, _) = post(
        &server,
        &server.app_token,
        "/api/v1/marks/withdraw",
        json!({ "markId": id }),
    )
    .await;

    let edit = sqlx::query("UPDATE example_mark SET setup = 'edited' WHERE id = ?1")
        .bind(&id)
        .execute(server.db.pool())
        .await
        .expect_err("an edit is refused");
    assert!(edit.to_string().contains("immutable"), "{edit}");

    let delete = sqlx::query("DELETE FROM example_mark WHERE id = ?1")
        .bind(&id)
        .execute(server.db.pool())
        .await
        .expect_err("a delete is refused");
    assert!(delete.to_string().contains("never deleted"), "{delete}");

    let unwithdraw = sqlx::query("DELETE FROM example_mark_withdrawal WHERE mark_id = ?1")
        .bind(&id)
        .execute(server.db.pool())
        .await
        .expect_err("un-withdrawing is refused");
    assert!(
        unwithdraw.to_string().contains("never deleted"),
        "{unwithdraw}"
    );

    let rewrite = sqlx::query(
        "UPDATE example_mark_withdrawal SET withdrawn_by_name = 'x' WHERE mark_id = ?1",
    )
    .bind(&id)
    .execute(server.db.pool())
    .await
    .expect_err("editing a withdrawal is refused");
    assert!(rewrite.to_string().contains("never edited"), "{rewrite}");
}

/// The risk gate's least-privilege control: a mark write touches no paper, order
/// or credential table.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_touches_no_paper_order_or_credential_table() {
    let server = spawn_server(ServerOptions::default()).await;
    let bar = series(&server.data_dir, Timeframe::M15).candles[0].open_time;

    let mut before = Vec::new();
    for table in WATCHED {
        before.push(rows_in(&server.db, table).await);
    }

    app_write(&server, write_body("15m", bar, "no blast radius")).await;

    for (index, table) in WATCHED.iter().enumerate() {
        assert_eq!(
            rows_in(&server.db, table).await,
            before[index],
            "the mark write must not touch {table}"
        );
    }
    assert_eq!(mark_rows(&server.db).await, 1);
}

// ---------------------------------------------------------------------------
// The transports
// ---------------------------------------------------------------------------

/// D5/ADR-0026: each `/api/v1/marks*` route is app-scope — a 2xx for an app token
/// and a 403 `scope_refused` for an agent token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_marks_route_is_app_scope() {
    let server = spawn_server(ServerOptions::default()).await;
    let bar = series(&server.data_dir, Timeframe::M15).candles[0].open_time;
    let id = app_write(&server, write_body("15m", bar, "scoped")).await;

    let calls: [(&str, Value); 3] = [
        ("/api/v1/marks", write_body("15m", bar, "scoped")),
        ("/api/v1/marks/list", json!({})),
        ("/api/v1/marks/withdraw", json!({ "markId": id })),
    ];

    for (path, body) in calls {
        let (status, response) = post(&server, &server.app_token, path, body.clone()).await;
        assert!(status < 300, "the app token reaches {path}: {response}");

        let (status, response) = post(&server, &server.agent_token, path, body).await;
        assert_eq!(
            status, 403,
            "an agent token cannot reach {path}: {response}"
        );
        assert_eq!(response["code"], "scope_refused", "{path}: {response}");
    }
}

/// G5/PDR-0015: the writer's name is a LABEL, never the token, and no token
/// string reaches a mark row or the log sink.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_token_string_lands_in_any_mark_row_or_the_log_sink() {
    let server = spawn_server(ServerOptions::default()).await;
    let bar = series(&server.data_dir, Timeframe::M15).candles[0].open_time;
    app_write(&server, write_body("15m", bar, "a labelled writer")).await;

    let token = server.app_token.clone();
    assert!(token.starts_with("pt_"), "the harness issues a real token");

    let rows: Vec<(String, String, String, String)> =
        sqlx::query_as("SELECT id, pair, setup, COALESCE(note, '') FROM example_mark")
            .fetch_all(server.db.pool())
            .await
            .expect("read the mark rows");
    assert_eq!(rows.len(), 1);
    for row in &rows {
        let haystack = format!("{}{}{}{}", row.0, row.1, row.2, row.3);
        assert!(
            !haystack.contains(&token),
            "the token leaked into a mark row: {haystack}"
        );
    }

    let writers: Vec<String> =
        sqlx::query_scalar("SELECT writer_name FROM example_mark WHERE writer_kind = 'trader'")
            .fetch_all(server.db.pool())
            .await
            .expect("read the writers");
    assert_eq!(writers, vec![APP_LABEL.to_owned()]);

    for line in server.log.lines() {
        assert!(
            !line.contains(&token),
            "the token leaked into the log sink: {line}"
        );
    }
}

/// Print probe for the item report's ONE `list_marks` response with a
/// hidden-by-holdout count (spec Approach item 11). Run:
///
/// ```sh
/// cargo test --test example_marks -- --ignored --nocapture print_list_marks_response
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "print probe for the item report's list_marks response"]
async fn print_list_marks_response() {
    let server = spawn_server(ServerOptions::default()).await;
    let m15 = series(&server.data_dir, Timeframe::M15);
    let (before, after) = adjacent_bars_at_or_before(&m15, HOLDOUT_MS);
    app_write(
        &server,
        write_body("15m", before, "support-rejection longs"),
    )
    .await;
    app_write(&server, write_body("15m", after, "support-rejection longs")).await;
    open_freeze(&server, before + Timeframe::M15.duration_ms()).await;

    let (_, body) = post(&server, &server.app_token, "/api/v1/marks/list", json!({})).await;
    println!(
        "{}",
        serde_json::to_string_pretty(&body).expect("the list renders")
    );
}
