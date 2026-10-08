//! AC-5 — the four read-only paper tools (r3.s4.w4, A3 least privilege).
//!
//! - the four tools return the same data as the routes (both project through
//!   `paper_read`);
//! - the tool list has no tool whose name or description promotes a session,
//!   stops one, sweep-stops them or runs a shadow check;
//! - an `agent` token reaches them over `/mcp`;
//! - the exact-list and count assertions live in `mcp_stdio.rs` /
//!   `mcp_process_hygiene.rs`, which this AC's command runs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::path::Path;

use pulse::{
    CandleStore, CreatedBy, Db, EngineFingerprint, Graduation, NewVersion, NonEmptyLabel,
    PaperEvent, PaperSessionDraft, PaperSessionId, PaperSessionRepository, PaperSide, ShadowResult,
    SqliteBacktestRunRepo, SqlitePaperSessionRepo, SqliteStrategyRepo, StrategyRepository,
    Timeframe, WalkForwardRunRepository, session_summary, session_trades,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use support::mcp::{
    call, copy_tree, manifest, migrated_db, seeded_walk_forward_draft, spawn_client,
};
use support::server::{ServerOptions, spawn_server};
use tempfile::TempDir;

const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

/// The tools this item adds, and the words their names/descriptions must never
/// carry.
const PAPER_TOOLS: [&str; 4] = [
    "list_paper_sessions",
    "get_paper_session",
    "get_paper_trades",
    "get_paper_comparison",
];
const BANNED_WORDS: [&str; 3] = ["promote", "stop", "shadow"];

/// Seed one certified paper session with a scripted log: two closed trades
/// with R, an open position, a shadow verdict — enough for every read tool to
/// answer something non-trivial.
async fn seed_session(db: &Db, store_dir: &Path) -> PaperSessionId {
    seed_session_with_trade_count(db, store_dir, 2).await
}

async fn seed_session_with_trade_count(db: &Db, store_dir: &Path, count: usize) -> PaperSessionId {
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let paper = SqlitePaperSessionRepo::new(
        db.pool().clone(),
        CandleStore::with_base_dir(store_dir.to_path_buf()),
    );
    let strategy = strategies
        .create_strategy("mcp-paper", None, &[])
        .await
        .expect("create the strategy");
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(&pulse::fixture_strategy_dsl()).expect("serialize DSL"),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create the version");
    runs.save_walk_forward_run(&version.id, &seeded_walk_forward_draft(true))
        .await
        .expect("save the passing certification");
    let certified = strategies
        .get_version(&version.id)
        .await
        .expect("read the version")
        .expect("the version exists")
        .latest_walk_forward_run_id
        .expect("the saved run is the latest");
    let session = paper
        .insert_session(&PaperSessionDraft {
            strategy_version_id: version.id.clone(),
            pair: pulse::Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: Some(Timeframe::H4),
            uses_d1: false,
            starting_equity: Decimal::from(10_000),
            taker_fee_bps: Decimal::from(4),
            slippage_bps: Decimal::from(1),
            engine_fingerprint: EngineFingerprint::current(),
            graduation: Graduation::Certified {
                walk_forward_run_id: certified,
                // The schema requires a certified promotion to name at least
                // one certified data version.
                data_versions: vec![pulse::CertifiedDataVersion {
                    timeframe: Timeframe::M15,
                    data_version: pulse::DataVersion::new("mcp-seed-data-version"),
                }],
            },
            fixture: false,
            min_trades: 20,
            promoted_by: NonEmptyLabel::try_new("seed-token").expect("non-empty"),
        })
        .await
        .expect("insert the session");
    let mut events = vec![PaperEvent::BarProcessed {
        seq: 0,
        at: "2025-02-01T00:15:00.000Z".to_owned(),
        bars: vec![pulse::BarRef {
            timeframe: Timeframe::M15,
            open_time: 1_000,
        }],
    }];
    let mut seq = 1_i64;
    for r in (0..count).map(|i| {
        if i % 2 == 0 {
            Decimal::ONE
        } else {
            Decimal::new(5, 1)
        }
    }) {
        seq += 1;
        events.push(PaperEvent::Fill {
            seq,
            at: "2025-02-01T00:20:00.000Z".to_owned(),
            side: PaperSide::Long,
            qty: Decimal::ONE,
            price: Decimal::from(60_000),
            exit_reason: None,
            realized_r: None,
            fill_time_ms: None,
        });
        seq += 1;
        events.push(PaperEvent::Fill {
            seq,
            at: "2025-02-01T00:40:00.000Z".to_owned(),
            side: PaperSide::Long,
            qty: Decimal::ONE,
            price: Decimal::from(60_100),
            exit_reason: Some(pulse::ExitReason::TakeProfit),
            realized_r: Some(r),
            fill_time_ms: None,
        });
    }
    seq += 1;
    events.push(PaperEvent::ShadowChecked {
        seq,
        at: "2025-02-01T01:00:00.000Z".to_owned(),
        data_versions: Vec::new(),
        bar_count: 1,
        result: serde_json::to_value(ShadowResult::Identical {
            closed_trades: 2,
            open_position: false,
        })
        .expect("serialize the verdict"),
    });
    paper
        .append_bar(&session.id, &[], &events)
        .await
        .expect("append the scripted log");
    session.id
}

async fn expected_summary(db: &Db, store_dir: &Path, id: &PaperSessionId) -> Value {
    let paper = SqlitePaperSessionRepo::new(
        db.pool().clone(),
        CandleStore::with_base_dir(store_dir.to_path_buf()),
    );
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    serde_json::to_value(
        session_summary(&paper, &runs, id)
            .await
            .expect("the read model")
            .expect("the session exists"),
    )
    .expect("json")
}

/// The four tools answer exactly what `paper_read` builds, over stdio.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_four_tools_match_the_read_model_over_stdio() {
    let tmp_db = TempDir::new().unwrap();
    let (db_path, db) = migrated_db(&tmp_db).await;
    let tmp_store = TempDir::new().unwrap();
    let store_dir = tmp_store.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);
    let id = seed_session(&db, &store_dir).await;
    let expected = expected_summary(&db, &store_dir, &id).await;

    let client = spawn_client(&db_path, &store_dir).await;

    // The list carries the summary.
    let listed = call(&client, "list_paper_sessions", json!({})).await;
    let sessions = listed["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1, "{listed}");
    assert_eq!(sessions[0], expected, "the list equals the read model");

    // Get one.
    let one = call(
        &client,
        "get_paper_session",
        json!({ "session_id": id.as_str() }),
    )
    .await;
    assert_eq!(one, expected, "get equals the read model");

    // Trades.
    let trades = call(
        &client,
        "get_paper_trades",
        json!({ "session_id": id.as_str() }),
    )
    .await;
    let paper = SqlitePaperSessionRepo::new(
        db.pool().clone(),
        CandleStore::with_base_dir(store_dir.clone()),
    );
    let expected_trades = serde_json::to_value(
        session_trades(&paper, &id)
            .await
            .expect("the read model")
            .expect("the session exists"),
    )
    .expect("json");
    assert_eq!(trades, expected_trades, "trades equal the read model");
    assert_eq!(
        trades["closed_trades"]
            .as_array()
            .expect("closed trades")
            .len(),
        2,
        "{trades}"
    );

    // Comparison.
    let comparison = call(
        &client,
        "get_paper_comparison",
        json!({ "session_id": id.as_str() }),
    )
    .await;
    assert_eq!(
        comparison, expected["comparison"],
        "the comparison equals the summary's own"
    );
    assert_eq!(comparison["status"], json!("pending"), "{comparison}");
    assert_eq!(comparison["n"], json!(2), "{comparison}");
    assert_eq!(comparison["of"], json!(20), "{comparison}");
}

/// No advertised tool's name or description promotes, stops or shadow-checks;
/// an unknown session id is a field error, not a silent empty result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_tool_promotes_stops_or_shadow_checks() {
    let tmp_db = TempDir::new().unwrap();
    let (db_path, db) = migrated_db(&tmp_db).await;
    let tmp_store = TempDir::new().unwrap();
    let store_dir = tmp_store.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);
    seed_session(&db, &store_dir).await;
    let client = spawn_client(&db_path, &store_dir).await;

    let tools = client.list_tools(None).await.expect("tools/list").tools;
    for tool in &tools {
        let haystack = format!(
            "{} {}",
            tool.name,
            tool.description.as_deref().unwrap_or("")
        )
        .to_lowercase();
        for banned in BANNED_WORDS {
            assert!(
                !haystack.contains(banned),
                "tool {} names `{banned}`: {haystack}",
                tool.name
            );
        }
    }
    for name in PAPER_TOOLS {
        assert!(
            tools.iter().any(|tool| tool.name == name),
            "the four paper reads are advertised: {name} missing"
        );
    }

    // An unknown id refuses with the field shape.
    let error = support::mcp::call_err(
        &client,
        "get_paper_session",
        json!({ "session_id": "00000000-0000-0000-0000-000000000000" }),
    )
    .await;
    assert_eq!(error["field"], json!("session_id"), "{error}");
}

// ---------------------------------------------------------------------------
// The /mcp leg (an agent token reaches the four tools over HTTP)
// ---------------------------------------------------------------------------

fn initialize_frame() -> String {
    json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "paper-mcp-read-test", "version": "0.0.0" }
        }
    })
    .to_string()
}

async fn post_mcp(
    base: &str,
    token: &str,
    session: Option<&str>,
    protocol_version: Option<&str>,
    frame: &str,
) -> (reqwest::StatusCode, Option<String>, Vec<Value>) {
    let http = reqwest::Client::new();
    let mut request = http
        .post(format!("{base}/mcp"))
        .bearer_auth(token)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(id) = session {
        request = request.header("mcp-session-id", id);
    }
    if let Some(version) = protocol_version {
        request = request.header("mcp-protocol-version", version);
    }
    let response = request
        .body(frame.to_owned())
        .send()
        .await
        .expect("POST /mcp");
    let status = response.status();
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let body = response.text().await.expect("read the response body");
    let payloads: Vec<Value> = if content_type.contains("text/event-stream") {
        body.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str(data).ok())
            .collect()
    } else if body.trim().is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(&body)
            .map(|value| vec![value])
            .unwrap_or_default()
    };
    (status, session_id, payloads)
}

/// An `agent` token reaches the four paper tools over `/mcp`, and they answer
/// the same data the read model holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_token_reaches_the_four_tools_over_mcp() {
    let ts = spawn_server(ServerOptions::default()).await;
    let id = seed_session(&ts.db, &ts.data_dir).await;
    let expected = expected_summary(&ts.db, &ts.data_dir, &id).await;
    let paper = SqlitePaperSessionRepo::new(
        ts.db.pool().clone(),
        CandleStore::with_base_dir(ts.data_dir.clone()),
    );
    let expected_trades = serde_json::to_value(
        session_trades(&paper, &id)
            .await
            .expect("the read model")
            .expect("the session exists"),
    )
    .expect("json");

    let (status, session, payloads) =
        post_mcp(&ts.base, &ts.agent_token, None, None, &initialize_frame()).await;
    assert!(status.is_success(), "initialize answers: {status}");
    let session = session.expect("the server issues an Mcp-Session-Id");
    let init = payloads
        .iter()
        .find(|payload| payload.get("id") == Some(&json!(0)))
        .expect("an initialize result frame");
    let protocol_version = init["result"]["protocolVersion"]
        .as_str()
        .expect("a protocol version")
        .to_owned();

    let (status, _, _) = post_mcp(
        &ts.base,
        &ts.agent_token,
        Some(&session),
        Some(&protocol_version),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    assert!(status.is_success(), "initialized accepted: {status}");

    let calls: [(&str, Value, Value); 4] = [
        (
            "list_paper_sessions",
            json!({}),
            json!({ "sessions": [expected.clone()] }),
        ),
        (
            "get_paper_session",
            json!({ "session_id": id.as_str() }),
            expected.clone(),
        ),
        (
            "get_paper_trades",
            json!({ "session_id": id.as_str() }),
            expected_trades.clone(),
        ),
        (
            "get_paper_comparison",
            json!({ "session_id": id.as_str() }),
            expected["comparison"].clone(),
        ),
    ];
    for (index, (name, arguments, want)) in calls.into_iter().enumerate() {
        let id_frame = index + 1;
        let (status, _, payloads) = post_mcp(
            &ts.base,
            &ts.agent_token,
            Some(&session),
            Some(&protocol_version),
            &json!({
                "jsonrpc": "2.0",
                "id": id_frame,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            })
            .to_string(),
        )
        .await;
        assert!(status.is_success(), "{name} answers: {status}");
        let call = payloads
            .iter()
            .find(|payload| payload.get("id") == Some(&json!(id_frame)))
            .expect("a tools/call result frame");
        assert!(
            call.get("error").is_none(),
            "{name} answers a result, not a protocol error: {call}"
        );
        let structured = &call["result"]["structuredContent"];
        assert_eq!(structured, &want, "{name}: {structured}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paper_mcp_withholds_certifying_fold_results_during_freeze() {
    let tmp = TempDir::new().unwrap();
    let (db_path, db) = migrated_db(&tmp).await;
    let store_dir = tmp.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);
    let id = seed_session_with_trade_count(&db, &store_dir, 20).await;
    let client = spawn_client(&db_path, &store_dir).await;
    let before = call(
        &client,
        "get_paper_comparison",
        json!({"session_id": id.as_str()}),
    )
    .await;
    assert!(
        before.get("fold_min").is_some(),
        "the comparison contains backtest fold statistics"
    );
    pulse::SqliteCertificationFreezeRepo::with_deps(
        db.pool().clone(),
        pulse::FakeClock::at(1_760_000_000_000),
    )
    .open(&pulse::OpenFreezeRequest {
        holdout_start_ms: 1_736_985_600_000,
        h: 12,
        alpha: "0.05".to_owned(),
        holdout_test: "C1".to_owned(),
    })
    .await
    .unwrap();
    for tool in ["get_paper_comparison", "get_paper_session"] {
        let error = support::mcp::call_err(&client, tool, json!({"session_id": id.as_str()})).await;
        assert_eq!(error["field"], "run_id");
        assert!(error["message"].as_str().unwrap().contains("BTCUSDT"));
        assert!(error["message"].as_str().unwrap().contains("2025-01-16"));
    }
    let list = call(&client, "list_paper_sessions", json!({})).await;
    assert_eq!(list["withheld_for_holdout"], 1);
    assert!(list["sessions"].as_array().unwrap().is_empty());
    // The operator read is unchanged, and own-engine paper trades are exempt.
    let operator = expected_summary(&db, &store_dir, &id).await;
    assert_eq!(operator["comparison"], before);
    let trades = call(
        &client,
        "get_paper_trades",
        json!({"session_id": id.as_str()}),
    )
    .await;
    assert_eq!(trades["closed_trades"].as_array().unwrap().len(), 20);
    pulse::SqliteCertificationFreezeRepo::with_deps(
        db.pool().clone(),
        pulse::FakeClock::at(1_760_000_000_001),
    )
    .close()
    .await
    .unwrap();
    assert_eq!(
        call(
            &client,
            "get_paper_comparison",
            json!({"session_id": id.as_str()})
        )
        .await,
        before
    );
    client.cancel().await.unwrap();
}
