//! AC-1 (d60) — the paper routes' scope wall and the control handle
//! (r3.s4.w4, the Network listener and client auth gate's least-privilege and
//! kill-switch controls).
//!
//! - (i) an `agent` token is 403 `scope_refused` on promote, stop, stop-all
//!   and shadow-check, and on every GET paper route, and writes no row and no
//!   event;
//! - (ii) stop-all by an `app` token stops every running session, each after
//!   a final `ShadowChecked` and recording the sweep's issuer, and later wakes
//!   poll none of them;
//! - (iii) stop records the caller's label; a second stop is 409;
//! - (iv) a session the runtime never attached is still stopped, without a
//!   shadow check;
//! - (v) with no runtime the control routes answer 503 and never hang.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::time::Duration;

use pulse::{
    CreatedBy, NewVersion, NonEmptyLabel, NonEmptyReason, OverrideRequest, PaperEvent,
    PaperSession, PaperSessionRepository, StopActor, StrategyRepository, Timeframe,
    fixture_h4_candles, fixture_m15_candles, fixture_strategy_dsl, promote,
};
use reqwest::Method;
use serde_json::{Value, json};
use support::paper::PaperHost;
use support::server::{ServerOptions, TestServer, spawn_server};

/// A client with a deadline, so a hanging control route fails the test instead
/// of wedging the suite (the "never hang" half of AC-1 (v)).
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("build the test client")
}

fn auth(token: &str) -> String {
    format!("Bearer {token}")
}

/// A server with the paper runtime seam installed.
async fn paper_server() -> TestServer {
    spawn_server(ServerOptions {
        paper: Some(Box::new(PaperHost::spawn)),
        ..Default::default()
    })
    .await
}

async fn row_count(ts: &TestServer, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(ts.db.pool())
        .await
        .expect("count rows")
}

/// Insert one running session through the promotion use case — the same path
/// the route rides, without the route (AC-1 drives the control surface, not
/// promotion).
async fn seed_session(host: &PaperHost, name: &str) -> PaperSession {
    let strategies = host.strategies();
    let strategy = strategies
        .create_strategy(name, None, &[])
        .await
        .expect("create the strategy");
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(&fixture_strategy_dsl()).expect("serialize the DSL"),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create the version");
    promote(
        &host.strategies(),
        &host.runs(),
        &host.runs(),
        &host.paper(),
        &host.clock,
        &version.id,
        Some(&OverrideRequest {
            reason: NonEmptyReason::try_new("scope-suite seed").expect("non-empty reason"),
            pair: pulse::Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: Some(Timeframe::H4),
            uses_d1: false,
        }),
        NonEmptyLabel::try_new("seed-token").expect("non-empty label"),
    )
    .await
    .expect("the override promotion succeeds")
}

/// `count` sessions, scripted bars, and one wake so the runtime attaches them
/// all (the running state AC-1 (ii)/(iii) drive).
async fn running_sessions(host: &PaperHost, count: usize) -> Vec<PaperSession> {
    host.source.script(Timeframe::M15, fixture_m15_candles());
    host.source.script(Timeframe::H4, fixture_h4_candles());
    let mut sessions = Vec::new();
    for index in 0..count {
        sessions.push(seed_session(host, &format!("scope-{index}")).await);
    }
    host.tick().await;
    sessions
}

fn stops(events: &[PaperEvent]) -> Vec<StopActor> {
    events
        .iter()
        .filter_map(|event| match event {
            PaperEvent::Stop { actor, .. } => Some(actor.clone()),
            _ => None,
        })
        .collect()
}

fn kinds(events: &[PaperEvent]) -> Vec<&'static str> {
    events.iter().map(PaperEvent::kind).collect()
}

/// (i) The scope wall: an agent token is refused on every paper route, and the
/// refusals write no row and no event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i_an_agent_token_is_refused_on_every_paper_route_and_writes_nothing() {
    let ts = paper_server().await;
    let host = ts.paper.as_ref().expect("the paper seam is installed");
    let session = seed_session(host, "scope-agent").await;
    let sessions_before = row_count(&ts, "paper_session").await;
    let events_before = row_count(&ts, "paper_event").await;

    let id = session.id.as_str().to_owned();
    let version_id = session.strategy_version_id.as_str().to_owned();
    let cases: Vec<(&str, Method, String, Option<Value>)> = vec![
        (
            "promote",
            Method::POST,
            "/api/v1/paper/promote".to_owned(),
            Some(json!({ "version_id": version_id })),
        ),
        (
            "stop",
            Method::POST,
            format!("/api/v1/paper/sessions/{id}/stop"),
            Some(json!({})),
        ),
        (
            "stop-all",
            Method::POST,
            "/api/v1/paper/stop-all".to_owned(),
            Some(json!({})),
        ),
        (
            "shadow-check",
            Method::POST,
            format!("/api/v1/paper/sessions/{id}/shadow-check"),
            Some(json!({})),
        ),
        (
            "list",
            Method::GET,
            "/api/v1/paper/sessions".to_owned(),
            None,
        ),
        (
            "get",
            Method::GET,
            format!("/api/v1/paper/sessions/{id}"),
            None,
        ),
        (
            "trades",
            Method::GET,
            format!("/api/v1/paper/sessions/{id}/trades"),
            None,
        ),
        (
            "events",
            Method::GET,
            format!("/api/v1/paper/sessions/{id}/events"),
            None,
        ),
    ];

    for (name, method, path, body) in cases {
        let mut request = client()
            .request(method, format!("{}{path}", ts.base))
            .header("Authorization", auth(&ts.agent_token));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("the refusal answers");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::FORBIDDEN,
            "{name} must refuse an agent token"
        );
        let body: Value = response.json().await.expect("a json refusal body");
        assert_eq!(body["code"], json!("scope_refused"), "{name}: {body:?}");
    }

    assert_eq!(
        row_count(&ts, "paper_session").await,
        sessions_before,
        "a refused request writes no session row"
    );
    assert_eq!(
        row_count(&ts, "paper_event").await,
        events_before,
        "a refused request writes no event"
    );
}

/// (ii) Stop-all halts every running session, each with its final shadow check
/// and the sweep's issuer, and later wakes poll none of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ii_stop_all_halts_every_running_session_with_its_issuer() {
    let ts = paper_server().await;
    let host = ts.paper.as_ref().expect("the paper seam is installed");
    let sessions = running_sessions(host, 3).await;

    let response = client()
        .post(format!("{}/api/v1/paper/stop-all", ts.base))
        .header("Authorization", auth(&ts.app_token))
        .json(&json!({}))
        .send()
        .await
        .expect("stop-all answers");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("a json body");
    let stopped: Vec<&str> = body["stopped"]
        .as_array()
        .expect("a stopped list")
        .iter()
        .map(|id| id.as_str().expect("string ids"))
        .collect();
    assert_eq!(stopped.len(), 3, "every running session stops: {body:?}");
    assert_eq!(
        body["failures"].as_array().map(Vec::len),
        Some(0),
        "no failure on a healthy sweep: {body:?}"
    );

    let issuer = NonEmptyLabel::try_new("w2-app").expect("the app token's label");
    for session in &sessions {
        assert!(
            stopped.contains(&session.id.as_str()),
            "{} is named in stopped: {body:?}",
            session.id
        );
        let events = host.paper().events(&session.id).await.expect("the log");
        let all_kinds = kinds(&events);
        assert_eq!(
            &all_kinds[all_kinds.len() - 2..],
            ["shadow_checked", "stop"],
            "each stop is preceded by its own final check: {all_kinds:?}"
        );
        assert_eq!(
            stops(&events),
            vec![StopActor::StopAll {
                issuer: issuer.clone()
            }],
            "the sweep's issuer is recorded"
        );
    }

    // Later wakes poll none of them: no fetch is made and no row is written.
    let calls_before = host.source.calls().len();
    let rows_before: Vec<usize> = {
        let mut rows = Vec::new();
        for session in &sessions {
            rows.push(
                host.paper()
                    .bars(&session.id, Timeframe::M15)
                    .await
                    .expect("bars")
                    .len(),
            );
        }
        rows
    };
    host.tick().await;
    host.tick().await;
    assert_eq!(
        host.source.calls().len(),
        calls_before,
        "a stopped session is never polled again"
    );
    for (session, rows) in sessions.iter().zip(rows_before) {
        assert_eq!(
            host.paper()
                .bars(&session.id, Timeframe::M15)
                .await
                .expect("bars")
                .len(),
            rows
        );
    }
}

/// (iii) Stop records the caller's token label; a second stop is 409.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iii_stop_records_the_callers_label_and_a_second_stop_is_409() {
    let ts = paper_server().await;
    let host = ts.paper.as_ref().expect("the paper seam is installed");
    let sessions = running_sessions(host, 2).await;
    let id = sessions[0].id.as_str().to_owned();

    let response = client()
        .post(format!("{}/api/v1/paper/sessions/{id}/stop", ts.base))
        .header("Authorization", auth(&ts.app_token))
        .json(&json!({}))
        .send()
        .await
        .expect("stop answers");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("a json body");
    assert_eq!(body["session_id"], json!(id), "{body:?}");
    assert_eq!(
        body["stopped_without_shadow"],
        json!(false),
        "an attached session stops through its final shadow check: {body:?}"
    );

    let events = host.paper().events(&sessions[0].id).await.expect("the log");
    assert_eq!(
        stops(&events),
        vec![StopActor::Token {
            label: NonEmptyLabel::try_new("w2-app").expect("the app token's label")
        }],
        "the stopping token's label is recorded"
    );
    let all_kinds = kinds(&events);
    assert_eq!(
        &all_kinds[all_kinds.len() - 2..],
        ["shadow_checked", "stop"],
        "{all_kinds:?}"
    );

    // The second stop is refused: the session is stopped.
    let response = client()
        .post(format!("{}/api/v1/paper/sessions/{id}/stop", ts.base))
        .header("Authorization", auth(&ts.app_token))
        .json(&json!({}))
        .send()
        .await
        .expect("the second stop answers");
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    let body: Value = response.json().await.expect("a json body");
    assert_eq!(body["code"], json!("session_stopped"), "{body:?}");
}

/// (iv) A session the runtime never attached is still stoppable — a final
/// `Stop` append with no shadow check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iv_an_unattached_session_is_still_stopped_without_a_shadow_check() {
    let ts = paper_server().await;
    let host = ts.paper.as_ref().expect("the paper seam is installed");
    // No tick after the insert: the runtime has not seen this session.
    let session = seed_session(host, "scope-unattached").await;

    let response = client()
        .post(format!(
            "{}/api/v1/paper/sessions/{}/stop",
            ts.base,
            session.id.as_str()
        ))
        .header("Authorization", auth(&ts.app_token))
        .json(&json!({}))
        .send()
        .await
        .expect("stop answers");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("a json body");
    assert_eq!(
        body["stopped_without_shadow"],
        json!(true),
        "the reply names the unattached stop: {body:?}"
    );

    let events = host.paper().events(&session.id).await.expect("the log");
    assert_eq!(
        kinds(&events),
        ["stop"],
        "the direct stop appends exactly the Stop event"
    );
    assert_eq!(
        stops(&events),
        vec![StopActor::Token {
            label: NonEmptyLabel::try_new("w2-app").expect("the app token's label")
        }],
        "the caller's label is recorded even without a shadow check"
    );
}

/// (v) With no runtime, the control routes answer 503 `runtime_unavailable`,
/// never hang, and promote writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v_with_no_runtime_the_control_routes_answer_503_and_never_hang() {
    // No paper seam: the server carries no control handle.
    let ts = spawn_server(ServerOptions::default()).await;
    let before = row_count(&ts, "paper_session").await;
    let id = "00000000-0000-0000-0000-000000000000";
    let cases: Vec<(&str, Method, String, Value)> = vec![
        (
            "promote",
            Method::POST,
            "/api/v1/paper/promote".to_owned(),
            json!({ "version_id": id }),
        ),
        (
            "stop",
            Method::POST,
            format!("/api/v1/paper/sessions/{id}/stop"),
            json!({}),
        ),
        (
            "stop-all",
            Method::POST,
            "/api/v1/paper/stop-all".to_owned(),
            json!({}),
        ),
        (
            "shadow-check",
            Method::POST,
            format!("/api/v1/paper/sessions/{id}/shadow-check"),
            json!({}),
        ),
    ];
    for (name, method, path, body) in cases {
        let response = client()
            .request(method, format!("{}{path}", ts.base))
            .header("Authorization", auth(&ts.app_token))
            .json(&body)
            .send()
            .await
            .expect("the 503 answers inside the client deadline");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "{name} must answer 503 without a runtime"
        );
        let body: Value = response.json().await.expect("a json body");
        assert_eq!(
            body["code"],
            json!("runtime_unavailable"),
            "{name}: {body:?}"
        );
    }
    assert_eq!(
        row_count(&ts, "paper_session").await,
        before,
        "a refused promote writes nothing"
    );
}
