//! AC-2 — the paper routes (r3.s4.w4, spec §5): promote with its typed
//! refusals, the immediate attach, the reads, the SSE stream and the
//! on-demand shadow check.
//!
//! - (i) promote: certified ⇒ 201 with the token's label (a body
//!   `promoted_by` is ignored); uncertified ⇒ 422 `uncertified`, no row; an
//!   empty reason ⇒ 422 `empty_reason`; an override ⇒ 201 badged override; a
//!   foreign-fingerprint certification ⇒ 422 `certified_under_other_engine`
//!   naming both fingerprints, override or not; an unknown version ⇒ 404;
//! - (ii) a promoted session is attached at once (its lead-in rows exist
//!   without waiting `IDLE_SCAN_MS`);
//! - (iii) list, get and trades return what `paper_read` builds; an unknown
//!   id ⇒ 404;
//! - (iv) SSE: the backlog in `seq` order with `id:` = seq; `Last-Event-ID: n`
//!   replays only `seq > n`; a new event appears live; a malformed header ⇒
//!   400; a revoked token ends the stream with `token_refused`;
//! - (v) the on-demand shadow check: `Identical` for a fresh session, 409 for
//!   a stopped one.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::time::{Duration, Instant};

use pulse::{
    BarRef, Candle, CandleStore, CreatedBy, Db, Direction, EngineFingerprint, Graduation,
    NewVersion, NonEmptyReason, Pair, PaperEvent, PaperSessionDraft, PaperSessionId,
    PaperSessionRepository, PaperSide, SqliteBacktestRunRepo, SqliteClientTokenRepo,
    SqlitePaperSessionRepo, SqliteStrategyRepo, StrategyRepository, Timeframe, VersionId,
    WalkForwardRunDraft, WalkForwardRunRepository, list_summaries, realized_pnl, session_summary,
    session_trades, taker_fee,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use support::mcp::{seeded_walk_forward_draft, seeded_walk_forward_draft_k};
use support::paper::PaperHost;
use support::server::{ServerOptions, TestServer, spawn_server};

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

async fn row_count(ts: &TestServer, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(ts.db.pool())
        .await
        .expect("count rows")
}

/// A fixture-DSL version with no walk-forward run (uncertified).
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

/// A version whose latest walk-forward run is the seeded passing draft — a
/// certification on THIS build, with known fold `mean_r` values.
async fn certified_version(db: &Db, name: &str) -> VersionId {
    let version_id = uncertified_version(db, name).await;
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    runs.save_walk_forward_run(&version_id, &seeded_walk_forward_draft(true))
        .await
        .expect("save the passing certification");
    version_id
}

/// The same, but certified under a foreign engine fingerprint (E2's refusal).
async fn foreign_certified_version(db: &Db, name: &str) -> VersionId {
    const FOREIGN: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let version_id = uncertified_version(db, name).await;
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let mut draft: WalkForwardRunDraft = seeded_walk_forward_draft_k(true, 2);
    // The save gate recomputes the run fingerprint from the folds' results, so
    // a coherent foreign certification sets BOTH.
    FOREIGN.clone_into(&mut draft.engine_fingerprint);
    for fold in &mut draft.folds {
        fold.result.engine_fingerprint = pulse::EngineFingerprint::from_stored(FOREIGN.to_owned());
    }
    runs.save_walk_forward_run(&version_id, &draft)
        .await
        .expect("save the foreign certification");
    version_id
}

async fn post_promote(ts: &TestServer, body: Value) -> reqwest::Response {
    client()
        .post(format!("{}/api/v1/paper/promote", ts.base))
        .header("Authorization", auth(&ts.app_token))
        .json(&body)
        .send()
        .await
        .expect("promote answers")
}

async fn get(ts: &TestServer, path: &str) -> reqwest::Response {
    client()
        .get(format!("{}{path}", ts.base))
        .header("Authorization", auth(&ts.app_token))
        .send()
        .await
        .expect("the read answers")
}

// ---------------------------------------------------------------------------
// The SSE frame reader (incremental: the session stream never ends)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Frame {
    id: Option<i64>,
    event: String,
    data: Value,
}

fn parse_complete_frames(bytes: &[u8]) -> Vec<Frame> {
    let text = std::str::from_utf8(bytes).expect("sse bytes are utf8");
    let mut frames = Vec::new();
    let mut rest = text;
    while let Some(index) = rest.find("\n\n") {
        let block = &rest[..index];
        rest = &rest[index + 2..];
        let mut id = None;
        let mut event = None;
        let mut data = String::new();
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("id: ") {
                id = value.trim().parse::<i64>().ok();
            } else if let Some(value) = line.strip_prefix("event: ") {
                event = Some(value.trim().to_owned());
            } else if let Some(value) = line.strip_prefix("data: ") {
                data.push_str(value);
            }
        }
        let Some(event) = event else { continue };
        frames.push(Frame {
            id,
            event,
            data: serde_json::from_str(data.trim()).unwrap_or(Value::Null),
        });
    }
    frames
}

/// Read frames until `done(frames)` holds or the deadline passes.
async fn read_frames_until(
    response: &mut reqwest::Response,
    deadline: Duration,
    done: impl Fn(&[Frame]) -> bool,
) -> Vec<Frame> {
    let start = Instant::now();
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let frames = parse_complete_frames(&buffer);
        if done(&frames) || start.elapsed() >= deadline {
            return frames;
        }
        match tokio::time::timeout(Duration::from_millis(250), response.chunk()).await {
            Ok(Ok(Some(chunk))) => buffer.extend_from_slice(&chunk),
            Ok(Ok(None)) => return parse_complete_frames(&buffer),
            Ok(Err(error)) => panic!("sse chunk failed: {error}"),
            Err(_) => {}
        }
    }
}

async fn open_events(ts: &TestServer, id: &str, last_event_id: Option<&str>) -> reqwest::Response {
    let mut request = client()
        .get(format!("{}/api/v1/paper/sessions/{id}/events", ts.base))
        .header("Authorization", auth(&ts.app_token));
    if let Some(last) = last_event_id {
        request = request.header("Last-Event-ID", last);
    }
    request.send().await.expect("the stream opens")
}

// ---------------------------------------------------------------------------
// (i) promote
// ---------------------------------------------------------------------------

/// A certified promotion answers 201 with the TOKEN's label — a `promoted_by`
/// in the body is ignored, in the response and in the row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i_certified_promote_uses_the_token_label_and_ignores_the_body() {
    let ts = paper_server().await;
    let version_id = certified_version(&ts.db, "routes-certified").await;
    let response = post_promote(
        &ts,
        json!({
            "version_id": version_id.as_str(),
            "promoted_by": "mallory",
        }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let body: Value = response.json().await.expect("a json summary");
    assert_eq!(body["promoted_by"], json!("w2-app"), "{body:?}");
    assert_eq!(
        body["graduation"]["graduation"],
        json!("certified"),
        "{body:?}"
    );
    let id = body["id"]
        .as_str()
        .expect("the summary names the id")
        .to_owned();
    let promoted_by: String =
        sqlx::query_scalar("SELECT promoted_by FROM paper_session WHERE id = ?1")
            .bind(&id)
            .fetch_one(ts.db.pool())
            .await
            .expect("read the row");
    assert_eq!(
        promoted_by, "w2-app",
        "the row records the token's label, never the body's"
    );
}

/// An uncertified version with no override is 422 `uncertified` and writes no
/// row; an empty reason is 422 `empty_reason`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i_uncertified_and_empty_reason_refuse_without_writing() {
    let ts = paper_server().await;
    let version_id = uncertified_version(&ts.db, "routes-uncertified").await;
    let before = row_count(&ts, "paper_session").await;

    let response = post_promote(&ts, json!({ "version_id": version_id.as_str() })).await;
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = response.json().await.expect("a json refusal");
    assert_eq!(body["code"], json!("uncertified"), "{body:?}");
    assert_eq!(
        row_count(&ts, "paper_session").await,
        before,
        "a refused promotion writes no row"
    );

    let response = post_promote(
        &ts,
        json!({
            "version_id": version_id.as_str(),
            "override": {
                "reason": "   ",
                "pair": "BTCUSDT",
                "primary_timeframe": "15m",
                "htf_timeframe": "4h",
            },
        }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = response.json().await.expect("a json refusal");
    assert_eq!(body["code"], json!("empty_reason"), "{body:?}");
    assert_eq!(row_count(&ts, "paper_session").await, before);
}

/// Round 3 (Codex): an override naming a pair the exchange adapter does not
/// know is 422 `validation`, and no row is written — the runtime could never
/// attach it. r4.s1.w2 pinned ETHUSDT, so the still-unsupported symbol is
/// DOGEUSDT.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i_an_override_with_an_unsupported_pair_refuses_without_writing() {
    let ts = paper_server().await;
    let version_id = uncertified_version(&ts.db, "routes-unsupported-pair").await;
    let before = row_count(&ts, "paper_session").await;
    let response = post_promote(
        &ts,
        json!({
            "version_id": version_id.as_str(),
            "override": {
                "reason": "promoting early on purpose",
                "pair": "DOGEUSDT",
                "primary_timeframe": "15m",
                "htf_timeframe": "4h",
                "uses_d1": false,
            },
        }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = response.json().await.expect("a json refusal");
    assert_eq!(body["code"], json!("validation"), "{body:?}");
    assert_eq!(row_count(&ts, "paper_session").await, before);
}

/// An override promotes with 201, badged override with its reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i_an_override_promotes_badged_with_its_reason() {
    let ts = paper_server().await;
    let version_id = uncertified_version(&ts.db, "routes-override").await;
    let response = post_promote(
        &ts,
        json!({
            "version_id": version_id.as_str(),
            "override": {
                "reason": "promoting early on purpose",
                "pair": "BTCUSDT",
                "primary_timeframe": "15m",
                "htf_timeframe": "4h",
                "uses_d1": false,
            },
        }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let body: Value = response.json().await.expect("a json summary");
    assert_eq!(
        body["graduation"]["graduation"],
        json!("override"),
        "{body:?}"
    );
    assert_eq!(
        body["graduation"]["reason"],
        json!("promoting early on purpose"),
        "{body:?}"
    );
    assert_eq!(body["promoted_by"], json!("w2-app"), "{body:?}");
}

/// A foreign-fingerprint certification refuses with both fingerprints named —
/// with and without an override (E2: the override does not apply).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i_a_foreign_certification_refuses_with_both_fingerprints() {
    const FOREIGN: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let ts = paper_server().await;
    let version_id = foreign_certified_version(&ts.db, "routes-foreign").await;
    for body in [
        json!({ "version_id": version_id.as_str() }),
        json!({
            "version_id": version_id.as_str(),
            "override": {
                "reason": "override cannot bypass the engine gate",
                "pair": "BTCUSDT",
                "primary_timeframe": "15m",
            },
        }),
    ] {
        let response = post_promote(&ts, body).await;
        assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        let body: Value = response.json().await.expect("a json refusal");
        assert_eq!(
            body["code"],
            json!("certified_under_other_engine"),
            "{body:?}"
        );
        assert_eq!(body["certified_under"], json!(FOREIGN), "{body:?}");
        assert_eq!(
            body["current"],
            json!(pulse::EngineFingerprint::current().as_str()),
            "{body:?}"
        );
    }
}

/// An unknown version is 404 `unknown_version`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i_an_unknown_version_is_404() {
    let ts = paper_server().await;
    let response = post_promote(
        &ts,
        json!({ "version_id": "00000000-0000-0000-0000-000000000000" }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("a json refusal");
    assert_eq!(body["code"], json!("unknown_version"), "{body:?}");
}

// ---------------------------------------------------------------------------
// (ii) the immediate attach
// ---------------------------------------------------------------------------

/// A promoted session is attached at once: its recorded rows exist without
/// waiting for the runtime's idle scan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ii_a_promoted_session_is_attached_at_once() {
    let ts = paper_server().await;
    let host = ts.paper.as_ref().expect("the paper seam is installed");
    host.source
        .script(Timeframe::M15, pulse::fixture_m15_candles());
    host.source
        .script(Timeframe::H4, pulse::fixture_h4_candles());
    let version_id = uncertified_version(&ts.db, "routes-attach").await;
    let response = post_promote(
        &ts,
        json!({
            "version_id": version_id.as_str(),
            "override": {
                "reason": "attach immediately",
                "pair": "BTCUSDT",
                "primary_timeframe": "15m",
                "htf_timeframe": "4h",
            },
        }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let body: Value = response.json().await.expect("a json summary");
    let id = PaperSessionId::new(body["id"].as_str().expect("id").to_owned());

    // No tick: the attach ran inside the request.
    let bars = host.paper().bars(&id, Timeframe::M15).await.expect("bars");
    assert!(
        !bars.is_empty(),
        "the attach fetched the lead-in at once (no IDLE_SCAN_MS wait)"
    );
    let summary = session_summary(
        &host.paper(),
        &SqliteBacktestRunRepo::new(ts.db.pool().clone()),
        None,
        &id,
    )
    .await
    .expect("summary")
    .expect("the session exists");
    assert_eq!(
        summary.shadow_checks.len(),
        1,
        "the attach ran the session's first shadow check: {summary:?}"
    );
}

// ---------------------------------------------------------------------------
// (iii) the reads
// ---------------------------------------------------------------------------

/// List, get and trades answer exactly what `paper_read` builds; an unknown id
/// is 404.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iii_list_get_and_trades_match_the_read_model() {
    let ts = paper_server().await;
    let host = ts.paper.as_ref().expect("the paper seam is installed");
    host.source
        .script(Timeframe::M15, pulse::fixture_m15_candles());
    host.source
        .script(Timeframe::H4, pulse::fixture_h4_candles());
    let version_id = certified_version(&ts.db, "routes-reads").await;
    let response = post_promote(&ts, json!({ "version_id": version_id.as_str() })).await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let created: Value = response.json().await.expect("a json summary");
    let id = PaperSessionId::new(created["id"].as_str().expect("id").to_owned());

    let paper = host.paper();
    let runs = SqliteBacktestRunRepo::new(ts.db.pool().clone());

    // The list carries the summary.
    let response = get(&ts, "/api/v1/paper/sessions").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let list: Vec<Value> = response.json().await.expect("a json list");
    assert_eq!(list.len(), 1, "{list:?}");
    let expected = serde_json::to_value(
        list_summaries(&paper, &runs, None)
            .await
            .expect("the read model")
            .first()
            .expect("one summary"),
    )
    .expect("json");
    assert_eq!(list[0], expected, "the list equals the read model");

    // Get one.
    let response = get(&ts, &format!("/api/v1/paper/sessions/{}", id.as_str())).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let one: Value = response.json().await.expect("a json summary");
    let expected = serde_json::to_value(
        session_summary(&paper, &runs, None, &id)
            .await
            .expect("the read model")
            .expect("the session"),
    )
    .expect("json");
    assert_eq!(one, expected, "get equals the read model");

    // Trades.
    let response = get(
        &ts,
        &format!("/api/v1/paper/sessions/{}/trades", id.as_str()),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let trades: Value = response.json().await.expect("a json trades body");
    let expected = serde_json::to_value(
        session_trades(&paper, &id)
            .await
            .expect("the read model")
            .expect("the session"),
    )
    .expect("json");
    assert_eq!(trades, expected, "trades equals the read model");

    // Unknown ids are 404 on every single-session read.
    for path in [
        "/api/v1/paper/sessions/00000000-0000-0000-0000-000000000000".to_owned(),
        "/api/v1/paper/sessions/00000000-0000-0000-0000-000000000000/trades".to_owned(),
    ] {
        let response = get(&ts, &path).await;
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND, "{path}");
        let body: Value = response.json().await.expect("a json refusal");
        assert_eq!(body["code"], json!("unknown_session"), "{body:?}");
    }
}

// ---------------------------------------------------------------------------
// (iv) SSE
// ---------------------------------------------------------------------------

/// The backlog arrives in `seq` order with `id:` = the event's seq, and
/// `Last-Event-ID: n` replays only `seq > n`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iv_sse_backlog_is_seq_ordered_and_last_event_id_replays_the_tail() {
    let ts = paper_server().await;
    let host = ts.paper.as_ref().expect("the paper seam is installed");
    host.source
        .script(Timeframe::M15, pulse::fixture_m15_candles());
    host.source
        .script(Timeframe::H4, pulse::fixture_h4_candles());
    let version_id = uncertified_version(&ts.db, "routes-sse").await;
    let response = post_promote(
        &ts,
        json!({
            "version_id": version_id.as_str(),
            "override": {
                "reason": "sse backlog",
                "pair": "BTCUSDT",
                "primary_timeframe": "15m",
                "htf_timeframe": "4h",
            },
        }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let created: Value = response.json().await.expect("a json summary");
    let id = created["id"].as_str().expect("id").to_owned();
    // Give the log a tail to replay: the attach's own event, plus two
    // repository-appended probes (as the runtime would append them).
    let session_id = PaperSessionId::new(id.clone());
    for summary in ["sse probe one", "sse probe two"] {
        host.paper()
            .append_bar(
                &session_id,
                &[],
                &[PaperEvent::DataEvent {
                    seq: 0,
                    at: "2025-02-01T01:00:00.000Z".to_owned(),
                    summary: summary.to_owned(),
                }],
            )
            .await
            .expect("append the probe event");
    }
    let log = host.paper().events(&session_id).await.expect("the log");
    assert!(log.len() >= 3, "the attach wrote events");

    let mut stream = open_events(&ts, &id, None).await;
    assert_eq!(stream.status(), reqwest::StatusCode::OK);
    let content_type = stream
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
    let frames = read_frames_until(&mut stream, Duration::from_secs(10), |frames| {
        frames.len() >= log.len()
    })
    .await;
    let ids: Vec<i64> = frames.iter().filter_map(|frame| frame.id).collect();
    let expected: Vec<i64> = log.iter().map(PaperEvent::seq).collect();
    assert_eq!(ids, expected, "the backlog is every event, in seq order");
    for (frame, event) in frames.iter().zip(&log) {
        assert_eq!(frame.event, "paper");
        assert_eq!(frame.data["type"].as_str(), Some(event.kind()), "{frame:?}");
    }
    drop(stream);

    // Last-Event-ID: n replays only seq > n.
    let n = expected[expected.len() / 2];
    let mut stream = open_events(&ts, &id, Some(&n.to_string())).await;
    assert_eq!(stream.status(), reqwest::StatusCode::OK);
    let frames = read_frames_until(&mut stream, Duration::from_secs(10), |frames| {
        !frames.is_empty()
    })
    .await;
    let ids: Vec<i64> = frames.iter().filter_map(|frame| frame.id).collect();
    assert_eq!(
        ids.first(),
        Some(&(n + 1)),
        "the replay starts after the header's seq: {ids:?}"
    );
    assert!(
        ids.iter().all(|id| *id > n),
        "nothing at or below n replays: {ids:?}"
    );
}

/// A malformed `Last-Event-ID` is 400 `validation`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iv_a_malformed_last_event_id_is_400() {
    let ts = paper_server().await;
    let response = open_events(&ts, "00000000-0000-0000-0000-000000000000", Some("nope")).await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("a json refusal");
    assert_eq!(body["code"], json!("validation"), "{body:?}");
}

/// A new event appears live on an open stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iv_a_new_event_appears_live() {
    let ts = paper_server().await;
    let host = ts.paper.as_ref().expect("the paper seam is installed");
    let version_id = uncertified_version(&ts.db, "routes-live").await;
    let response = post_promote(
        &ts,
        json!({
            "version_id": version_id.as_str(),
            "override": {
                "reason": "live events",
                "pair": "BTCUSDT",
                "primary_timeframe": "15m",
            },
        }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let created: Value = response.json().await.expect("a json summary");
    let id = PaperSessionId::new(created["id"].as_str().expect("id").to_owned());

    let mut stream = open_events(&ts, id.as_str(), None).await;
    assert_eq!(stream.status(), reqwest::StatusCode::OK);
    // Drain the current backlog first.
    let log_len = host.paper().events(&id).await.expect("log").len();
    read_frames_until(&mut stream, Duration::from_secs(10), |frames| {
        frames.len() >= log_len
    })
    .await;

    // Append one event through the repository, as the runtime would.
    let appended = host
        .paper()
        .append_bar(
            &id,
            &[],
            &[PaperEvent::DataEvent {
                seq: 0,
                at: "2025-02-01T01:00:00.000Z".to_owned(),
                summary: "live-tail probe".to_owned(),
            }],
        )
        .await
        .expect("append the probe event");
    let probe_seq = appended[0].seq();

    let frames = read_frames_until(&mut stream, Duration::from_secs(10), |frames| {
        frames.iter().any(|frame| frame.id == Some(probe_seq))
    })
    .await;
    let live = frames
        .iter()
        .find(|frame| frame.id == Some(probe_seq))
        .expect("the live event arrives");
    assert_eq!(live.event, "paper");
    assert_eq!(live.data["summary"], json!("live-tail probe"));
}

/// Revoking the token ends the stream with an `error` frame `token_refused`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iv_revoking_the_token_ends_the_stream_with_token_refused() {
    let ts = paper_server().await;
    assert!(
        ts.paper.is_some(),
        "the paper seam is installed (promote requires a runtime)"
    );
    let version_id = uncertified_version(&ts.db, "routes-revoke").await;
    let response = post_promote(
        &ts,
        json!({
            "version_id": version_id.as_str(),
            "override": {
                "reason": "revocation",
                "pair": "BTCUSDT",
                "primary_timeframe": "15m",
            },
        }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let created: Value = response.json().await.expect("a json summary");
    let id = created["id"].as_str().expect("id").to_owned();

    let mut stream = open_events(&ts, &id, None).await;
    assert_eq!(stream.status(), reqwest::StatusCode::OK);
    // Revoke the app token the stream rides.
    SqliteClientTokenRepo::new(ts.db.pool().clone())
        .revoke("w2-app")
        .await
        .expect("revoke the token");

    let frames = read_frames_until(&mut stream, Duration::from_secs(10), |frames| {
        frames.iter().any(|frame| frame.event == "error")
    })
    .await;
    let error = frames
        .iter()
        .find(|frame| frame.event == "error")
        .expect("the revocation ends the stream");
    assert_eq!(error.data["code"], json!("token_refused"), "{error:?}");
    // The stream ends after the error frame: the next read is EOF.
    let tail = read_frames_until(&mut stream, Duration::from_secs(5), |_| false).await;
    assert_eq!(
        tail.iter().filter(|frame| frame.event == "error").count(),
        0,
        "the stream closes after the error frame"
    );
}

// ---------------------------------------------------------------------------
// (v) the on-demand shadow check
// ---------------------------------------------------------------------------

/// A fresh session's check is `Identical`; a stopped session's is 409.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v_shadow_check_answers_identical_then_409_once_stopped() {
    let ts = paper_server().await;
    let host = ts.paper.as_ref().expect("the paper seam is installed");
    host.source
        .script(Timeframe::M15, pulse::fixture_m15_candles());
    host.source
        .script(Timeframe::H4, pulse::fixture_h4_candles());
    let version_id = uncertified_version(&ts.db, "routes-shadow").await;
    let response = post_promote(
        &ts,
        json!({
            "version_id": version_id.as_str(),
            "override": {
                "reason": "shadow check",
                "pair": "BTCUSDT",
                "primary_timeframe": "15m",
                "htf_timeframe": "4h",
            },
        }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let created: Value = response.json().await.expect("a json summary");
    let id = created["id"].as_str().expect("id").to_owned();

    let response = client()
        .post(format!(
            "{}/api/v1/paper/sessions/{id}/shadow-check",
            ts.base
        ))
        .header("Authorization", auth(&ts.app_token))
        .json(&json!({}))
        .send()
        .await
        .expect("the check answers");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let verdict: Value = response.json().await.expect("a json verdict");
    assert_eq!(
        verdict["verdict"],
        json!("identical"),
        "a fresh session matches its shadow: {verdict:?}"
    );

    // Stop it, then the check refuses.
    let response = client()
        .post(format!("{}/api/v1/paper/sessions/{id}/stop", ts.base))
        .header("Authorization", auth(&ts.app_token))
        .json(&json!({}))
        .send()
        .await
        .expect("the stop answers");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let response = client()
        .post(format!(
            "{}/api/v1/paper/sessions/{id}/shadow-check",
            ts.base
        ))
        .header("Authorization", auth(&ts.app_token))
        .json(&json!({}))
        .send()
        .await
        .expect("the refused check answers");
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    let body: Value = response.json().await.expect("a json refusal");
    assert_eq!(body["code"], json!("session_stopped"), "{body:?}");
}

// ---------------------------------------------------------------------------
// (vi) the money view — a seeded session, no runtime installed
// ---------------------------------------------------------------------------

/// One M15 bar in milliseconds.
const SEED_BAR_MS: i64 = 900_000;

/// The seeded log's first bar open — 8-hour aligned.
const SEED_T0: i64 = 1_735_689_600_000;

/// The default token fee the seeded session runs at (A1), bps.
const SEED_FEE_BPS: i64 = 4;

/// An instant, rendered the way the runtime stamps events.
fn millis_text(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// A decimal as the wire carries it (exact text).
fn decimal_of(value: &Value) -> Decimal {
    value
        .as_str()
        .expect("a decimal is exact text on this wire")
        .parse::<Decimal>()
        .expect("the text parses as a decimal")
}

/// One M15 bar on the fixture grid.
fn seed_bar(open_time: i64, close: i64) -> Candle {
    Candle {
        open_time,
        close_time: open_time + SEED_BAR_MS - 1,
        open: Decimal::from(close),
        high: Decimal::from(close + 10),
        low: Decimal::from(close - 10),
        close: Decimal::from(close),
        volume: Decimal::from(1),
        funding_rate: None,
    }
}

/// A `bar_processed` event for one M15 bar.
fn bar_processed(open_time: i64) -> PaperEvent {
    PaperEvent::BarProcessed {
        seq: 0,
        at: millis_text(open_time),
        bars: vec![BarRef {
            timeframe: Timeframe::M15,
            open_time,
        }],
    }
}

/// An `order` event at one grid instant.
fn order_at(at: i64, qty: Decimal) -> PaperEvent {
    PaperEvent::Order {
        seq: 0,
        at: millis_text(at),
        side: PaperSide::Long,
        qty,
    }
}

/// A `fill` event at one grid instant (long, no engine fill time).
fn fill_at(
    at: i64,
    qty: Decimal,
    price: i64,
    exit_reason: Option<pulse::ExitReason>,
    realized_r: Option<Decimal>,
) -> PaperEvent {
    PaperEvent::Fill {
        seq: 0,
        at: millis_text(at),
        side: PaperSide::Long,
        qty,
        price: Decimal::from(price),
        exit_reason,
        realized_r,
        fill_time_ms: None,
    }
}

/// A `funding` event at one grid instant.
fn funding_at(at: i64, amount: Decimal) -> PaperEvent {
    PaperEvent::Funding {
        seq: 0,
        at: millis_text(at),
        rate: Decimal::new(1, 5),
        amount,
    }
}

/// Seed a session whose log holds ONE closed trade (its funding accrued on a
/// step the position was open for) and ONE standing long — the read's money
/// view without any runtime in the process (r4.s4.w1).
async fn seed_open_session(ts: &TestServer) -> PaperSessionId {
    let strategies = SqliteStrategyRepo::new(ts.db.pool().clone());
    let strategy = strategies
        .create_strategy("w1-pnl-reads", None, &[])
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
    let paper = SqlitePaperSessionRepo::new(
        ts.db.pool().clone(),
        CandleStore::with_base_dir(ts.data_dir.clone()),
    );
    let session = paper
        .insert_session(&PaperSessionDraft {
            strategy_version_id: version.id,
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: None,
            uses_d1: false,
            starting_equity: Decimal::from(10_000),
            taker_fee_bps: Decimal::from(SEED_FEE_BPS),
            slippage_bps: Decimal::from(1),
            engine_fingerprint: EngineFingerprint::current(),
            graduation: Graduation::Override {
                reason: NonEmptyReason::try_new("w1 seeded read").expect("non-empty"),
                at: millis_text(SEED_T0),
            },
            fixture: false,
            min_trades: 20,
            promoted_by: pulse::NonEmptyLabel::try_new("seed-token").expect("non-empty"),
        })
        .await
        .expect("insert the session");

    let t0 = SEED_T0;
    let t1 = t0 + SEED_BAR_MS;
    let t2 = t0 + 2 * SEED_BAR_MS;
    let t3 = t0 + 3 * SEED_BAR_MS;
    let events = vec![
        // Step 0: the entry (qty 1 at 60000).
        bar_processed(t0),
        order_at(t0, Decimal::ONE),
        fill_at(t0, Decimal::ONE, 60_000, None, None),
        // Step 1: the funding on the open position (its funding so far).
        bar_processed(t1),
        funding_at(t1, Decimal::new(-6, 1)),
        // Step 2: the exit (qty 1 at 60100, +1R).
        bar_processed(t2),
        fill_at(
            t2,
            Decimal::ONE,
            60_100,
            Some(pulse::ExitReason::TakeProfit),
            Some(Decimal::ONE),
        ),
        // Step 3: the standing entry (qty 0.5 at 59900) — the open position.
        bar_processed(t3),
        order_at(t3, Decimal::new(5, 1)),
        fill_at(t3, Decimal::new(5, 1), 59_900, None, None),
    ];

    let bars = vec![
        (Timeframe::M15, seed_bar(t0, 60_000), false),
        (Timeframe::M15, seed_bar(t1, 60_050), false),
        (Timeframe::M15, seed_bar(t2, 60_100), false),
        (Timeframe::M15, seed_bar(t3, 60_050), false),
    ];
    paper
        .append_bar(&session.id, &bars, &events)
        .await
        .expect("append the seeded batch");
    session.id
}

/// The two reads carry the money view: the totals, the marked open position
/// (stops absent — no runtime is installed) and the entries with each trade's
/// USDT figures.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vi_the_reads_carry_the_money_view_for_a_seeded_session() {
    let ts = spawn_server(ServerOptions::default()).await;
    let id = seed_open_session(&ts).await;
    let fee_bps = Decimal::from(SEED_FEE_BPS);

    // The expected figures, from the seeded numbers and the engine's own cost
    // functions (never from the read model).
    let entry_fee = taker_fee(Decimal::ONE * Decimal::from(60_000), fee_bps);
    let exit_fee = taker_fee(Decimal::ONE * Decimal::from(60_100), fee_bps);
    let trade_usdt = realized_pnl(
        Decimal::from(60_000),
        Decimal::from(60_100),
        Decimal::ONE,
        Direction::Long,
    ) + Decimal::new(-6, 1)
        - entry_fee
        - exit_fee;
    let open_fee = taker_fee(Decimal::new(5, 1) * Decimal::from(59_900), fee_bps);
    let unrealized = realized_pnl(
        Decimal::from(59_900),
        Decimal::from(60_050),
        Decimal::new(5, 1),
        Direction::Long,
    ) + Decimal::ZERO
        - open_fee;

    let response = get(&ts, &format!("/api/v1/paper/sessions/{}", id.as_str())).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let summary: Value = response.json().await.expect("a json summary");
    assert_eq!(summary["closed_trade_count"], json!(1));
    assert_eq!(decimal_of(&summary["realized_usdt_total"]), trade_usdt);
    assert_eq!(summary["realized_usdt_count"], json!(1));
    assert_eq!(decimal_of(&summary["realized_r_total"]), Decimal::ONE);
    assert_eq!(summary["realized_r_count"], json!(1));
    assert_eq!(decimal_of(&summary["unrealized_usdt"]), unrealized);
    assert_eq!(
        decimal_of(&summary["session_pnl_usdt"]),
        trade_usdt + unrealized
    );

    let open = &summary["open_position"];
    assert_eq!(open["side"], json!("long"));
    assert_eq!(decimal_of(&open["qty"]), Decimal::new(5, 1));
    assert_eq!(decimal_of(&open["mark_price"]), Decimal::from(60_050));
    assert_eq!(
        open["mark_time"],
        json!(millis_text(SEED_T0 + 3 * SEED_BAR_MS + SEED_BAR_MS - 1))
    );
    assert_eq!(decimal_of(&open["funding_usdt"]), Decimal::ZERO);
    assert_eq!(decimal_of(&open["entry_fee_usdt"]), open_fee);
    assert_eq!(decimal_of(&open["unrealized_usdt"]), unrealized);
    // No runtime is installed: the stops are absent and no R is derivable.
    assert_eq!(open["unrealized_r"], Value::Null);
    assert_eq!(open["initial_stop"], Value::Null);
    assert_eq!(open["stop_price"], Value::Null);
    assert_eq!(open["take_profit_price"], Value::Null);

    let response = get(
        &ts,
        &format!("/api/v1/paper/sessions/{}/trades", id.as_str()),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let trades: Value = response.json().await.expect("a json trades body");
    let trade = &trades["closed_trades"][0];
    assert_eq!(decimal_of(&trade["realized_usdt"]), trade_usdt);
    assert_eq!(decimal_of(&trade["funding_usdt"]), Decimal::new(-6, 1));
    assert_eq!(decimal_of(&trade["entry_fee_usdt"]), entry_fee);
    assert_eq!(decimal_of(&trade["exit_fee_usdt"]), exit_fee);
    let entries = trades["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 5, "two orders and three fills: {entries:?}");
    assert_eq!(entries[0]["kind"], json!("order"));
    assert_eq!(entries[1]["kind"], json!("fill"));
    assert_eq!(entries[1]["exit_reason"], Value::Null);
    assert_eq!(entries[2]["exit_reason"], json!("take_profit"));
    assert_eq!(entries[4]["kind"], json!("fill"));
    assert_eq!(entries[4]["exit_reason"], Value::Null);
}

// ---------------------------------------------------------------------------
// (vii) the money view — served beside a running engine
// ---------------------------------------------------------------------------

/// Drive the host's runtime until the session holds an open position.
async fn drive_to_open(
    host: &support::paper::PaperHost,
    id: &PaperSessionId,
) -> pulse::OpenPositionStops {
    for _ in 0..6_000 {
        host.clock.advance(SEED_BAR_MS);
        host.tick().await;
        if let Ok(Some(reply)) = host.control.position(id.clone()).await {
            return reply.stops;
        }
    }
    panic!("no open position within the fixture series");
}

/// The summary's marked open position carries the running engine's live stops
/// (initial, current, take-profit), and its R is derivable because of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vii_the_summary_carries_the_live_stops_from_the_running_engine() {
    let ts = paper_server().await;
    let host = ts.paper.as_ref().expect("the paper seam is installed");
    host.source
        .script(Timeframe::M15, pulse::fixture_m15_candles());
    let version_id = uncertified_version(&ts.db, "routes-stops").await;
    let response = post_promote(
        &ts,
        json!({
            "version_id": version_id.as_str(),
            "override": {
                "reason": "live stops",
                "pair": "BTCUSDT",
                "primary_timeframe": "15m",
            },
        }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let created: Value = response.json().await.expect("a json summary");
    let id = PaperSessionId::new(created["id"].as_str().expect("id").to_owned());

    let stops = drive_to_open(host, &id).await;

    let response = get(&ts, &format!("/api/v1/paper/sessions/{}", id.as_str())).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let summary: Value = response.json().await.expect("a json summary");
    let open = &summary["open_position"];
    assert_eq!(decimal_of(&open["initial_stop"]), stops.initial_stop);
    assert_eq!(decimal_of(&open["stop_price"]), stops.stop_price);
    match stops.take_profit_price {
        Some(take_profit) => assert_eq!(decimal_of(&open["take_profit_price"]), take_profit),
        None => assert_eq!(open["take_profit_price"], Value::Null),
    }
    assert!(
        open["unrealized_r"].is_string(),
        "the live initial stop makes the R derivable: {open}"
    );
    // The mark is the newest processed bar the runtime recorded.
    let bars = host.paper().bars(&id, Timeframe::M15).await.expect("bars");
    let newest = bars.last().expect("the session processed bars");
    assert_eq!(decimal_of(&open["mark_price"]), newest.close);
    assert_eq!(open["mark_time"], json!(millis_text(newest.close_time)));
}

// ---------------------------------------------------------------------------
// (viii) the mark and the live stops describe the replayed log (r4.s4.w2)
// ---------------------------------------------------------------------------

/// The mark is the bar the replayed log last processed: a newer `paper_bar`
/// row — the runtime's next bar landing between the events read and the mark
/// read — is never the mark (it belongs to a log state the summary does not
/// describe).
#[tokio::test]
async fn the_mark_never_jumps_past_the_logs_last_processed_bar() {
    let ts = spawn_server(ServerOptions::default()).await;
    let id = seed_open_session(&ts).await;
    let paper = SqlitePaperSessionRepo::new(
        ts.db.pool().clone(),
        CandleStore::with_base_dir(ts.data_dir.clone()),
    );
    // The interleaving: the events read ends at the seeded log, while the
    // store already holds the runtime's NEXT bar row (its log event still
    // unread by that read).
    let newer = seed_bar(SEED_T0 + 4 * SEED_BAR_MS, 60_900);
    paper
        .append_bar(&id, &[(Timeframe::M15, newer, false)], &[])
        .await
        .expect("append the newer row");

    let response = get(&ts, &format!("/api/v1/paper/sessions/{}", id.as_str())).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let summary: Value = response.json().await.expect("a json summary");
    let open = &summary["open_position"];
    assert_eq!(
        decimal_of(&open["mark_price"]),
        Decimal::from(60_050),
        "the mark stays the replayed log's last processed bar"
    );
    assert_eq!(
        open["mark_time"],
        json!(millis_text(SEED_T0 + 4 * SEED_BAR_MS - 1)),
        "the mark time is that bar's close, not the newer row's"
    );
}

/// The live stops are shown only while the runtime's answer describes the same
/// log state the read replayed: a runtime that committed fewer events than the
/// read saw answers a lagging seq, and the read drops the stops — the reads
/// before and after prove the drop is the mismatch, not a missing runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_live_stops_are_dropped_when_the_runtime_describes_another_log_state() {
    let ts = paper_server().await;
    let host = ts.paper.as_ref().expect("the paper seam is installed");
    host.source
        .script(Timeframe::M15, pulse::fixture_m15_candles());
    let version_id = uncertified_version(&ts.db, "routes-stops-mismatch").await;
    let response = post_promote(
        &ts,
        json!({
            "version_id": version_id.as_str(),
            "override": {
                "reason": "stops mismatch",
                "pair": "BTCUSDT",
                "primary_timeframe": "15m",
            },
        }),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let created: Value = response.json().await.expect("a json summary");
    let id = PaperSessionId::new(created["id"].as_str().expect("id").to_owned());
    drive_to_open(host, &id).await;

    // While the read and the runtime hold the same log, the stops are shown.
    let response = get(&ts, &format!("/api/v1/paper/sessions/{}", id.as_str())).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let summary: Value = response.json().await.expect("a json summary");
    assert!(
        summary["open_position"]["initial_stop"].is_string(),
        "the same log state shows the stops: {summary}"
    );

    // A log event the runtime never committed moves the replayed log ahead of
    // the engine's state: the reply's seq no longer matches, so the stops are
    // dropped and the read still answers.
    host.paper()
        .append_bar(
            &id,
            &[],
            &[PaperEvent::DataEvent {
                seq: 0,
                at: millis_text(SEED_T0),
                summary: "a log write the runtime did not commit".to_owned(),
            }],
        )
        .await
        .expect("append the event behind the runtime");

    let response = get(&ts, &format!("/api/v1/paper/sessions/{}", id.as_str())).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let summary: Value = response.json().await.expect("a json summary");
    let open = &summary["open_position"];
    assert_eq!(open["initial_stop"], Value::Null, "{summary}");
    assert_eq!(open["stop_price"], Value::Null);
    assert_eq!(open["take_profit_price"], Value::Null);
    assert_eq!(open["unrealized_r"], Value::Null, "no live stop, no R");
}
