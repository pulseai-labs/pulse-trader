//! AC-3 — no secret in any log (r3.s4.w4, the Network listener and client auth
//! gate's no-secret control).
//!
//! An override reason is user text: it lives in the session row (and rides the
//! API response), and it must never reach a server log line. Tokens must never
//! appear anywhere. Each request keeps exactly one request-log line, naming
//! its label.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::Duration;

use pulse::{CreatedBy, Db, NewVersion, SqliteStrategyRepo, StrategyRepository, VersionId};
use serde_json::{Value, json};
use support::paper::PaperHost;
use support::server::{ServerOptions, TestServer, spawn_server};

/// The unique marker planted as the override reason — distinctive enough that
/// an eight-scenario scan cannot miss it.
const MARKER: &str = "MARKER-OVERRIDE-REASON-7f3a91c2d4e5";

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("build the test client")
}

fn auth(token: &str) -> String {
    format!("Bearer {token}")
}

async fn paper_server() -> TestServer {
    spawn_server(ServerOptions {
        paper: Some(Box::new(PaperHost::spawn)),
        ..Default::default()
    })
    .await
}

async fn uncertified_version(db: &Db, name: &str) -> VersionId {
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategies
        .create_strategy(name, None, &[])
        .await
        .expect("create the strategy");
    strategies
        .create_version(NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(&pulse::fixture_strategy_dsl()).expect("serialize DSL"),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create the version")
        .id
}

async fn post(ts: &TestServer, path: &str, body: Value) -> reqwest::Response {
    client()
        .post(format!("{}{path}", ts.base))
        .header("Authorization", auth(&ts.app_token))
        .json(&body)
        .send()
        .await
        .expect("the request answers")
}

/// The override reason reaches the response and the row, and neither it nor a
/// token reaches any log line; every request logs exactly one line naming its
/// label.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_override_reason_and_tokens_never_reach_a_log_line() {
    let ts = paper_server().await;
    let marker_version = uncertified_version(&ts.db, "secrets-marker").await;
    let other_version = uncertified_version(&ts.db, "secrets-other").await;

    // 1. Promote with the marker as the override reason.
    let response = post(
        &ts,
        "/api/v1/paper/promote",
        json!({
            "version_id": marker_version.as_str(),
            "override": {
                "reason": MARKER,
                "pair": "BTCUSDT",
                "primary_timeframe": "15m",
                "htf_timeframe": "4h",
            },
        }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let created: Value = response.json().await.expect("a json summary");
    assert_eq!(
        created["graduation"]["reason"],
        json!(MARKER),
        "the API response carries the reason: {created:?}"
    );
    let marker_id = created["id"].as_str().expect("id").to_owned();
    let stored: String =
        sqlx::query_scalar("SELECT override_reason FROM paper_session WHERE id = ?1")
            .bind(&marker_id)
            .fetch_one(ts.db.pool())
            .await
            .expect("read the row");
    assert_eq!(stored, MARKER, "the row carries the reason");

    // 2. Stop that session.
    let response = post(
        &ts,
        &format!("/api/v1/paper/sessions/{marker_id}/stop"),
        json!({}),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // 3. Promote another session and stop-all.
    let response = post(
        &ts,
        "/api/v1/paper/promote",
        json!({
            "version_id": other_version.as_str(),
            "override": {
                "reason": "second session",
                "pair": "BTCUSDT",
                "primary_timeframe": "15m",
            },
        }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let response = post(&ts, "/api/v1/paper/stop-all", json!({})).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // 4. The log sink: no marker, no token, ever.
    let lines = ts.log.lines();
    assert!(!lines.is_empty(), "the sink captured the request lines");
    for line in &lines {
        assert!(
            !line.contains(MARKER),
            "the override reason reached a log line: {line:?}"
        );
        assert!(
            !line.contains(&ts.app_token),
            "the app token reached a log line: {line:?}"
        );
        assert!(
            !line.contains(&ts.agent_token),
            "the agent token reached a log line: {line:?}"
        );
    }

    // 5. Exactly one request-log line per request, naming its label (two
    //    promotes happened, then one stop and one stop-all). The request log
    //    runs every path through the structural redactor, so the stop line's
    //    session id is scrubbed — the assertion matches the path's shape, not
    //    the raw id.
    for (method, path, expected) in [
        ("POST", "/api/v1/paper/promote".to_owned(), 2_usize),
        (
            "POST",
            "/api/v1/paper/sessions/«REDACTED»/stop".to_owned(),
            1,
        ),
        ("POST", "/api/v1/paper/stop-all".to_owned(), 1),
    ] {
        let prefix = format!("pulse serve: {method} {path} ");
        let matching: Vec<&String> = lines
            .iter()
            .filter(|line| line.starts_with(&prefix))
            .collect();
        assert_eq!(
            matching.len(),
            expected,
            "one log line per {method} {path} request: {matching:?} (all lines: {lines:?})"
        );
        for line in matching {
            assert!(
                line.contains("w2-app"),
                "the line names the caller's label: {line:?}"
            );
        }
    }
}
