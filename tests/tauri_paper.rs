//! AC-1 — the paper command surface (r3.s4.w5, spec §1).
//!
//! The eight commands drive the REAL in-process server (`support::server`'s
//! `spawn_server` with the `PaperHost` seam) through the `ClientState` the
//! command wrappers use — the `tests/client_proxy.rs` precedent, one layer
//! deeper:
//!
//! - (i) each of the eight commands calls its route and decodes the response
//!   into its DTO, field for field (the DTO's own serialization is compared
//!   against the route's raw JSON, with the one wire shape the DTO carries as
//!   exact text normalized);
//! - (ii) every paper error code decodes to its `BusErrorCode`,
//!   `unknown_version`/`unknown_session` map to `not_found`, and E2's message
//!   keeps both fingerprints;
//! - (iii) the session stream: backlog then live frames in seq order; a
//!   dropped connection resumes with `Last-Event-ID`, no duplicate and no gap
//!   (driven over a raw TCP stub, the `client_proxy.rs` wire-stub precedent);
//!   revocation emits a terminal `TokenRefused` and stops; a dropped channel
//!   stops the reader;
//! - (iv) `paper_stop_all` returns every failure, never a bare success.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pulse::{
    BusError, BusErrorCode, CandleStore, ClientState, ConnectOutcome, CreatedBy, Db, NewVersion,
    NonEmptyLabel, PaperEvent, PaperEventFrame, PaperJsonText, PaperSessionId,
    PaperSessionRepository, PaperStreamEvent, PaperStreamSink, PromoteOverride, PromoteRequest,
    RetryBackoff, ServerClient, SqliteBacktestRunRepo, SqliteClientTokenRepo,
    SqlitePaperSessionRepo, SqliteStrategyRepo, StopActor, StrategyRepository, Timeframe,
    VersionId, WalkForwardRunDraft, WalkForwardRunRepository, promote,
};
use serde::Serialize;
use serde_json::{Value, json};
use support::mcp::seeded_walk_forward_draft;
use support::paper::PaperHost;
use support::server::{ServerOptions, TestServer, spawn_server};
use tauri::ipc::{Channel, InvokeResponseBody};

/// The foreign fingerprint the E2 refusal names (`paper_api_routes.rs`'s
/// constant, shared here so both suites refuse the same shape).
const FOREIGN: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("build the test client")
}

fn auth(token: &str) -> String {
    format!("Bearer {token}")
}

/// The server with the paper-runtime seam installed.
async fn paper_server() -> TestServer {
    spawn_server(ServerOptions {
        paper: Some(Box::new(PaperHost::spawn)),
        ..Default::default()
    })
    .await
}

/// The scripted fixture bars every promoting case runs over.
fn script_bars(ts: &TestServer) {
    let host = ts.paper.as_ref().expect("the paper seam is installed");
    host.source
        .script(Timeframe::M15, pulse::fixture_m15_candles());
    host.source
        .script(Timeframe::H4, pulse::fixture_h4_candles());
}

/// One process-lifetime config dir so `ClientState::connect` never writes the
/// real user data dir. `cargo test` runs these tests in threads of ONE process,
/// so the env var is set exactly once (`LazyLock`) and never mutated again —
/// the `client_proxy.rs` isolation, made concurrency-safe.
fn isolate_config_dir() -> &'static std::path::Path {
    static DIR: std::sync::LazyLock<std::path::PathBuf> = std::sync::LazyLock::new(|| {
        let dir = tempfile::TempDir::new().expect("config tempdir");
        let path = dir.keep();
        // SAFETY: the lazy initializer runs on one thread while every other
        // caller blocks on it, and nothing writes the variable afterwards.
        unsafe {
            std::env::set_var("PULSE_CONFIG_DIR", &path);
        }
        path
    });
    &DIR
}

/// A connected app state over the in-process server — what the eight command
/// wrappers hold as `tauri::State<'_, ClientState>`.
///
/// The connect writes the shared connection file, whose temp name is fixed, so
/// the connects are serialized: two tests racing on the same file is a flake,
/// not a finding.
async fn paper_state(ts: &TestServer) -> ClientState {
    static CONNECT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = CONNECT_LOCK.lock().await;
    let _ = isolate_config_dir();
    let state = ClientState::new();
    let outcome = state.connect(&ts.base, &ts.app_token).await;
    assert!(
        matches!(outcome, ConnectOutcome::Connected { .. }),
        "the in-process server must connect cleanly: {outcome:?}"
    );
    state
}

/// A real `tauri::ipc::Channel<PaperStreamEvent>` plus the decoded events it
/// received (the `client_proxy.rs` recorder, for the paper channel).
fn recording_sink() -> (Channel<PaperStreamEvent>, Arc<Mutex<Vec<Value>>>) {
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&received);
    let channel = Channel::new(move |body: InvokeResponseBody| {
        let json = match body {
            InvokeResponseBody::Json(s) => s,
            InvokeResponseBody::Raw(bytes) => String::from_utf8(bytes).unwrap(),
        };
        let value: Value = serde_json::from_str(&json).unwrap();
        sink.lock().unwrap().push(value);
        Ok(())
    });
    (channel, received)
}

/// A sink that refuses every send — the shape of a screen whose channel died.
struct DeadSink;

impl PaperStreamSink for DeadSink {
    fn send_paper_event(&self, _event: PaperStreamEvent) -> Result<(), BusError> {
        Err(BusError::new(
            BusErrorCode::Internal,
            "channel closed".to_owned(),
        ))
    }
}

/// A sink that records the FIRST frame and then refuses — the bounded form of
/// a screen that unmounts after one frame. It lets a case assert the decoded
/// frame and still let the reader return.
#[derive(Default)]
struct OneFrameSink {
    frames: Mutex<Vec<Value>>,
}

impl PaperStreamSink for OneFrameSink {
    fn send_paper_event(&self, event: PaperStreamEvent) -> Result<(), BusError> {
        let mut frames = self.frames.lock().unwrap();
        if !frames.is_empty() {
            return Err(BusError::new(
                BusErrorCode::Internal,
                "channel closed".to_owned(),
            ));
        }
        frames.push(serde_json::to_value(&event).unwrap());
        Ok(())
    }
}

/// Wait until `check` holds, or fail naming `what`.
async fn wait_for(mut check: impl FnMut() -> bool, within: Duration, what: &str) {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

// ---------------------------------------------------------------------------
// Fixture versions
// ---------------------------------------------------------------------------

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

async fn certified_version(db: &Db, name: &str) -> VersionId {
    let version_id = uncertified_version(db, name).await;
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    runs.save_walk_forward_run(&version_id, &seeded_walk_forward_draft(true))
        .await
        .expect("save the passing certification");
    version_id
}

async fn foreign_certified_version(db: &Db, name: &str) -> VersionId {
    let version_id = uncertified_version(db, name).await;
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let mut draft: WalkForwardRunDraft = seeded_walk_forward_draft(true);
    FOREIGN.clone_into(&mut draft.engine_fingerprint);
    for fold in &mut draft.folds {
        fold.result.engine_fingerprint = pulse::EngineFingerprint::from_stored(FOREIGN.to_owned());
    }
    runs.save_walk_forward_run(&version_id, &draft)
        .await
        .expect("save the foreign certification");
    version_id
}

fn promote_request(version_id: &VersionId) -> PromoteRequest {
    PromoteRequest {
        version_id: version_id.as_str().to_owned(),
        r#override: None,
    }
}

fn override_request(version_id: &VersionId, reason: &str) -> PromoteRequest {
    PromoteRequest {
        version_id: version_id.as_str().to_owned(),
        r#override: Some(PromoteOverride {
            reason: reason.to_owned(),
            pair: "BTCUSDT".to_owned(),
            primary_timeframe: "15m".to_owned(),
            htf_timeframe: None,
            uses_d1: false,
        }),
    }
}

/// Promote a session directly against the server's database, bypassing the
/// route — the runtime never sees it (no tick), so it is running but
/// unattached. The `session_not_attached` arm's only honest source.
async fn promote_out_of_band(ts: &TestServer, version_id: &VersionId) -> PaperSessionId {
    let strategies = SqliteStrategyRepo::new(ts.db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(ts.db.pool().clone());
    let paper = SqlitePaperSessionRepo::new(
        ts.db.pool().clone(),
        CandleStore::with_base_dir(ts.data_dir.clone()),
    );
    let certifications = pulse::SqliteCertificationRepo::new(ts.db.pool().clone());
    let session = promote(
        &strategies,
        &runs,
        &runs,
        &paper,
        &pulse::SystemClock,
        &certifications,
        version_id,
        None,
        NonEmptyLabel::try_new("w5-out-of-band").unwrap(),
    )
    .await
    .expect("the out-of-band promotion succeeds");
    session.id
}

/// Stop a session directly in the log, out of band — the runtime still holds
/// it, so its own stop of it must fail on the read-only wall (A4).
async fn stop_out_of_band(ts: &TestServer, id: &str) {
    let paper = SqlitePaperSessionRepo::new(
        ts.db.pool().clone(),
        CandleStore::with_base_dir(ts.data_dir.clone()),
    );
    paper
        .append_bar(
            &PaperSessionId::new(id.to_owned()),
            &[],
            &[PaperEvent::Stop {
                seq: 0,
                at: "2025-02-01T01:00:00.000Z".to_owned(),
                actor: StopActor::Token {
                    label: NonEmptyLabel::try_new("w5-out-of-band").unwrap(),
                },
            }],
        )
        .await
        .expect("the out-of-band stop lands");
}

/// The version's persisted certifying run (the fold ids the fault injection
/// needs).
async fn latest_walk_forward(ts: &TestServer, version_id: &VersionId) -> pulse::WalkForwardRun {
    let version = SqliteStrategyRepo::new(ts.db.pool().clone())
        .get_version(version_id)
        .await
        .expect("read the version")
        .expect("the version exists");
    SqliteBacktestRunRepo::new(ts.db.pool().clone())
        .get_walk_forward_run(
            version
                .latest_walk_forward_run_id
                .as_ref()
                .expect("a certifying run exists"),
        )
        .await
        .expect("read the certifying run")
        .expect("the certifying run exists")
}

// ---------------------------------------------------------------------------
// Route plumbing for the raw comparisons
// ---------------------------------------------------------------------------

async fn get_json(ts: &TestServer, path: &str) -> Value {
    let response = client()
        .get(format!("{}{path}", ts.base))
        .header("Authorization", auth(&ts.app_token))
        .send()
        .await
        .expect("the read answers");
    assert!(
        response.status().is_success(),
        "GET {path} must succeed: {}",
        response.status()
    );
    response.json().await.expect("a json body")
}

async fn post_json(ts: &TestServer, path: &str) -> Value {
    let response = client()
        .post(format!("{}{path}", ts.base))
        .header("Authorization", auth(&ts.app_token))
        .json(&json!({}))
        .send()
        .await
        .expect("the post answers");
    assert!(
        response.status().is_success(),
        "POST {path} must succeed: {}",
        response.status()
    );
    response.json().await.expect("a json body")
}

/// The DTO's own serialization must equal the route's raw JSON, field for
/// field. The one normalization: `last_bar_open_time` is epoch milliseconds on
/// the wire and exact integer text in the DTO (specta refuses `i64`), so the
/// raw number is rendered as its text before the comparison.
fn assert_mirrors<T: Serialize>(dto: &T, raw: &Value) {
    let mut expected = raw.clone();
    if let Some(ms) = expected
        .get("last_bar_open_time")
        .and_then(Value::as_i64)
        .map(|ms| ms.to_string())
    {
        expected["last_bar_open_time"] = json!(ms);
    }
    assert_eq!(
        serde_json::to_value(dto).expect("the DTO serializes"),
        expected,
        "the DTO must mirror the route's JSON exactly"
    );
}

/// The DTO's key set must equal the route's raw JSON's key set — the
/// field-for-field half of the mirror for a MUTATION's answer, which a second
/// call cannot repeat (a stop is not idempotent). The values that vary between
/// two calls (the session id) are asserted separately.
fn assert_same_keys<T: Serialize>(dto: &T, raw: &Value) {
    let dto_value = serde_json::to_value(dto).expect("the DTO serializes");
    let mut dto_keys: Vec<&str> = dto_value
        .as_object()
        .expect("a DTO object")
        .keys()
        .map(String::as_str)
        .collect();
    let mut raw_keys: Vec<&str> = raw
        .as_object()
        .expect("a raw object")
        .keys()
        .map(String::as_str)
        .collect();
    dto_keys.sort_unstable();
    raw_keys.sort_unstable();
    assert_eq!(
        dto_keys, raw_keys,
        "the DTO must decode every field the route sent, and invent none"
    );
}

// ---------------------------------------------------------------------------
// (i) the eight commands
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i_the_eight_commands_call_their_routes_and_decode_their_dtos() {
    let ts = paper_server().await;
    script_bars(&ts);
    let state = paper_state(&ts).await;
    let first = certified_version(&ts.db, "w5-commands").await;
    let second = certified_version(&ts.db, "w5-commands-2").await;

    // 1. `paper_promote` — POST /api/v1/paper/promote, decoded field for field.
    let promoted = state
        .paper_promote(&promote_request(&first))
        .await
        .expect("promote decodes");
    assert_eq!(
        promoted.promoted_by, "w2-app",
        "the token's own label rides the summary (never the body)"
    );
    let raw = get_json(&ts, &format!("/api/v1/paper/sessions/{}", promoted.id)).await;
    assert_mirrors(&promoted, &raw);
    assert_eq!(
        raw["graduation"]["graduation"],
        json!("certified"),
        "the fixture certification is the certified arm: {raw}"
    );

    // 2. `paper_sessions` — GET /api/v1/paper/sessions.
    let sessions = state.paper_sessions().await.expect("list decodes");
    let raw_list: Vec<Value> = get_json(&ts, "/api/v1/paper/sessions")
        .await
        .as_array()
        .expect("a list")
        .clone();
    assert_eq!(sessions.len(), raw_list.len());
    assert_eq!(sessions.len(), 1, "one session so far");
    assert_mirrors(&sessions[0], &raw_list[0]);

    // 3. `paper_session` — GET /api/v1/paper/sessions/{id}.
    let one = state
        .paper_session(&promoted.id)
        .await
        .expect("get decodes");
    assert_mirrors(&one, &raw);

    // 4. `paper_session_trades` — GET .../trades.
    let trades = state
        .paper_session_trades(&promoted.id)
        .await
        .expect("trades decode");
    let raw_trades = get_json(
        &ts,
        &format!("/api/v1/paper/sessions/{}/trades", promoted.id),
    )
    .await;
    assert_mirrors(&trades, &raw_trades);

    // 5. `paper_shadow_check` — POST .../shadow-check, the bare verdict.
    let check = state
        .paper_shadow_check(&promoted.id)
        .await
        .expect("the shadow check decodes");
    let raw_check = post_json(
        &ts,
        &format!("/api/v1/paper/sessions/{}/shadow-check", promoted.id),
    )
    .await;
    assert_mirrors(&check, &raw_check);
    assert_eq!(
        raw_check["verdict"],
        json!("identical"),
        "a fresh session's on-demand check is identical: {raw_check}"
    );

    // 6. `paper_stop` — POST .../stop, with the recorded actor. A stop is a
    // mutation, so the route's own answer is compared over a SECOND session:
    // the two sessions decode to the same shape (both attached, so both stop
    // with a final shadow check).
    let other = state
        .paper_promote(&promote_request(&second))
        .await
        .expect("the second promote decodes");
    let stopped = state.paper_stop(&promoted.id).await.expect("stop decodes");
    assert_eq!(stopped.session_id, promoted.id);
    let raw_stop = post_json(&ts, &format!("/api/v1/paper/sessions/{}/stop", other.id)).await;
    assert_eq!(raw_stop["session_id"], json!(other.id));
    assert_eq!(
        raw_stop["stopped_without_shadow"], stopped.stopped_without_shadow,
        "the DTO carries the route's own flag"
    );
    assert_same_keys(&stopped, &raw_stop);

    // 7. `paper_stop_all` — POST /api/v1/paper/stop-all. Same rule: a sweep is
    // a mutation, so the route's answer is compared over a fresh pair.
    for tag in ["w5-commands-3", "w5-commands-4"] {
        state
            .paper_promote(&promote_request(&certified_version(&ts.db, tag).await))
            .await
            .expect("the pair's promotes decode");
    }
    let raw_all = post_json(&ts, "/api/v1/paper/stop-all").await;
    assert_eq!(
        raw_all["stopped"].as_array().map(Vec::len),
        Some(2),
        "{raw_all}"
    );
    assert_eq!(raw_all["failures"], json!([]), "{raw_all}");
    let (fifth, sixth) = (
        state
            .paper_promote(&promote_request(
                &certified_version(&ts.db, "w5-commands-5").await,
            ))
            .await
            .expect("the fifth promote decodes"),
        state
            .paper_promote(&promote_request(
                &certified_version(&ts.db, "w5-commands-6").await,
            ))
            .await
            .expect("the sixth promote decodes"),
    );
    let all = state.paper_stop_all().await.expect("stop-all decodes");
    assert_eq!(
        {
            let mut ids = all.stopped.clone();
            ids.sort_unstable();
            ids
        },
        {
            let mut ids = vec![fifth.id.clone(), sixth.id.clone()];
            ids.sort_unstable();
            ids
        },
        "the command's sweep names every session the route's sweep would"
    );
    assert!(all.failures.is_empty(), "{all:?}");
    assert_same_keys(&all, &raw_all);

    // 8. `paper_session_events` — GET .../events, decoded into frames. The
    // stream is long-lived, so this case bounds it with a sink that records
    // the first frame and refuses the rest (the unmount shape); the full
    // backlog/live/resume behaviour is case (iii).
    let log = ts
        .paper
        .as_ref()
        .expect("the paper seam")
        .paper()
        .events(&PaperSessionId::new(promoted.id.clone()))
        .await
        .expect("the log reads back");
    assert!(!log.is_empty(), "the attach wrote a backlog");
    let sink = OneFrameSink::default();
    state
        .paper_session_events(&promoted.id, None, &sink)
        .await
        .expect("the reader stops on the dead channel");
    let frames = sink.frames.lock().unwrap().clone();
    assert_eq!(
        frames.len(),
        1,
        "one frame before the channel died: {frames:?}"
    );
    let frame = &frames[0];
    assert_eq!(frame["kind"], json!("frame"));
    assert_eq!(frame["seq"], json!(log[0].seq()));
    assert_eq!(frame["type"], json!(log[0].kind()));
    let payload: Value = serde_json::from_str(frame["payload"].as_str().expect("payload text"))
        .expect("the payload is JSON text");
    assert_eq!(payload["type"], json!(log[0].kind()));
    assert_eq!(payload["seq"], json!(log[0].seq()));
}

// ---------------------------------------------------------------------------
// (ii) the paper error codes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ii_every_paper_error_code_decodes_to_its_bus_error_code() {
    let ts = paper_server().await;
    script_bars(&ts);
    let state = paper_state(&ts).await;

    // `uncertified` — no walk-forward run at all.
    let uncertified = uncertified_version(&ts.db, "w5-uncertified").await;
    let error = state
        .paper_promote(&promote_request(&uncertified))
        .await
        .expect_err("an uncertified version refuses");
    assert_eq!(error.code, BusErrorCode::Uncertified);

    // `empty_reason` — a whitespace-only reason is not a reason.
    let error = state
        .paper_promote(&override_request(&uncertified, "   "))
        .await
        .expect_err("an empty reason refuses");
    assert_eq!(error.code, BusErrorCode::EmptyReason);

    // `certified_under_other_engine` — E2, and the override cannot bypass it.
    let foreign = foreign_certified_version(&ts.db, "w5-foreign").await;
    let error = state
        .paper_promote(&promote_request(&foreign))
        .await
        .expect_err("a foreign certification refuses");
    assert_eq!(error.code, BusErrorCode::CertifiedUnderOtherEngine);
    assert!(
        error.message.contains(FOREIGN),
        "the message names the certifying engine: {}",
        error.message
    );
    assert!(
        error
            .message
            .contains(pulse::EngineFingerprint::current().as_str()),
        "the message names this build: {}",
        error.message
    );
    let error = state
        .paper_promote(&override_request(&foreign, "override attempt"))
        .await
        .expect_err("the override does not bypass E2");
    assert_eq!(error.code, BusErrorCode::CertifiedUnderOtherEngine);

    // `certification_unreadable` — a fold run whose recorded inputs cannot be
    // read. The inputs are immutable once written (0003's trigger), so the
    // fault injection drops that trigger and NULLs the provenance — including
    // the window bounds, whose presence without provenance the read
    // fail-closes on — leaving exactly the shape an older binary's row reads
    // back as: a run with no input provenance at all.
    let unreadable = certified_version(&ts.db, "w5-unreadable").await;
    let run = latest_walk_forward(&ts, &unreadable).await;
    sqlx::query("DROP TRIGGER backtest_run_no_update")
        .execute(ts.db.pool())
        .await
        .expect("drop the immutability trigger");
    sqlx::query(
        "UPDATE backtest_run SET pair = NULL, primary_timeframe = NULL, \
         primary_data_version = NULL, htf_timeframe = NULL, htf_data_version = NULL, \
         taker_fee_bps = NULL, slippage_bps = NULL, funding_config = NULL, \
         window_from_ms = NULL, window_to_ms = NULL, window_lead_in_from_ms = NULL \
         WHERE id = ?1",
    )
    .bind(run.folds[0].backtest_run_id.as_str())
    .execute(ts.db.pool())
    .await
    .expect("NULL the fold's provenance");
    let error = state
        .paper_promote(&promote_request(&unreadable))
        .await
        .expect_err("an unreadable certification refuses");
    assert_eq!(
        error.code,
        BusErrorCode::CertificationUnreadable,
        "the refusal's own message: {}",
        error.message
    );

    // `unknown_version` maps onto the existing `not_found`.
    let error = state
        .paper_promote(&promote_request(&VersionId::new("no-such-version")))
        .await
        .expect_err("an unknown version refuses");
    assert_eq!(error.code, BusErrorCode::NotFound);

    // `session_stopped` — stopping a session whose log already ends in `stop`.
    let stopped = state
        .paper_promote(&promote_request(
            &certified_version(&ts.db, "w5-stopped").await,
        ))
        .await
        .expect("promote decodes");
    state
        .paper_stop(&stopped.id)
        .await
        .expect("the first stop lands");
    let error = state
        .paper_stop(&stopped.id)
        .await
        .expect_err("a second stop refuses");
    assert_eq!(error.code, BusErrorCode::SessionStopped);

    // `unknown_session` maps onto the existing `not_found`.
    let error = state
        .paper_session("00000000-0000-0000-0000-000000000000")
        .await
        .expect_err("an unknown session refuses");
    assert_eq!(error.code, BusErrorCode::NotFound);

    // `session_not_attached` — a running session this runtime never attached.
    let out_of_band =
        promote_out_of_band(&ts, &certified_version(&ts.db, "w5-not-attached").await).await;
    let error = state
        .paper_shadow_check(out_of_band.as_str())
        .await
        .expect_err("an unattached session refuses the check");
    assert_eq!(error.code, BusErrorCode::SessionNotAttached);

    // `runtime_unavailable` — a server with no runtime at all.
    let bare = spawn_server(ServerOptions::default()).await;
    let bare_state = paper_state(&bare).await;
    let error = bare_state
        .paper_stop_all()
        .await
        .expect_err("no runtime refuses the kill switch");
    assert_eq!(error.code, BusErrorCode::RuntimeUnavailable);
}

// ---------------------------------------------------------------------------
// (iii) the session stream
// ---------------------------------------------------------------------------

/// The backlog arrives in seq order, then the live frame the runtime appends
/// after a wake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iii_a_the_stream_delivers_backlog_then_live_in_seq_order() {
    let ts = paper_server().await;
    script_bars(&ts);
    let host = ts.paper.as_ref().expect("the paper seam").clone();
    let state = Arc::new(paper_state(&ts).await);
    let version_id = certified_version(&ts.db, "w5-stream-live").await;
    let promoted = state
        .paper_promote(&promote_request(&version_id))
        .await
        .expect("promote decodes");
    let id = promoted.id.clone();
    let session_id = PaperSessionId::new(id.clone());
    let backlog = host.paper().events(&session_id).await.expect("the log");

    let (channel, received) = recording_sink();
    let reader_state = Arc::clone(&state);
    let reader_id = id.clone();
    let reader = tokio::spawn(async move {
        reader_state
            .paper_session_events(&reader_id, None, &channel)
            .await
    });

    // The backlog, in seq order.
    wait_for(
        || received.lock().unwrap().len() >= backlog.len(),
        Duration::from_secs(15),
        "the backlog frames",
    )
    .await;

    // Then one live frame: a closed bar the runtime consumes on a wake.
    host.clock.advance(900_000);
    host.tick().await;
    wait_for(
        || received.lock().unwrap().len() > backlog.len(),
        Duration::from_secs(15),
        "a live frame",
    )
    .await;

    let frames = received.lock().unwrap().clone();
    let seqs: Vec<i64> = frames
        .iter()
        .map(|frame| frame["seq"].as_i64().expect("a numeric seq"))
        .collect();
    let first = backlog[0].seq();
    assert_eq!(
        seqs,
        (first..first + i64::try_from(frames.len()).unwrap()).collect::<Vec<_>>(),
        "frames arrive in seq order with no gap: {seqs:?}"
    );
    assert!(
        frames.iter().all(|frame| frame["kind"] == json!("frame")),
        "every frame is a frame: {frames:?}"
    );
    reader.abort();
}

/// A dropped connection resumes with `Last-Event-ID` and delivers no duplicate
/// and no gap — driven over a raw TCP stub, where the suite controls the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iii_b_a_dropped_connection_resumes_with_no_duplicate_and_no_gap() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the stub");
    let base = format!("http://{}", listener.local_addr().expect("stub local"));
    let stub = tokio::spawn(async move {
        run_drop_mid_stream_stub(listener).await;
    });

    let (channel, received) = recording_sink();
    let client = ServerClient::with_backoff(
        &base,
        "any-token",
        RetryBackoff::new(Duration::from_millis(10), Duration::from_millis(50)),
    );
    client
        .stream_paper_session("s-resume", None, &channel)
        .await
        .expect("the reader resumes past the drop and ends on the refusal");
    stub.await.expect("the stub completes");

    let events = received.lock().unwrap().clone();
    let seqs: Vec<i64> = events
        .iter()
        .filter(|event| event["kind"] == json!("frame"))
        .map(|event| event["seq"].as_i64().expect("a numeric seq"))
        .collect();
    assert_eq!(
        seqs,
        vec![1, 2, 3],
        "the resumed stream is gap-free and duplicate-free, got {seqs:?}"
    );
    let last = events.last().expect("a terminal event");
    assert_eq!(
        last["kind"],
        json!("tokenRefused"),
        "the refusal is the terminal event: {events:?}"
    );
}

/// The stub body: two frames then a premature close; the reconnect must carry
/// `Last-Event-ID: 2`, and the replay it answers with deliberately repeats seq
/// 2 (the reader must drop it) before seq 3 and a `token_refused` terminal.
async fn run_drop_mid_stream_stub(listener: tokio::net::TcpListener) {
    use tokio::io::AsyncWriteExt as _;

    let frame = |seq: i64| {
        format!(
            "id: {seq}\nevent: paper\ndata: {}\n\n",
            json!({
                "type": "data_event",
                "seq": seq,
                "at": "2025-02-01T00:00:00.000Z",
                "summary": format!("frame {seq}")
            })
        )
    };

    // Connection 1 — two frames, then the socket dies with no terminal.
    let (mut stream, _) = listener.accept().await.expect("accept the first GET");
    let (request_line, _) = read_request(&mut stream).await;
    assert!(
        request_line.contains("/api/v1/paper/sessions/s-resume/events"),
        "the reader must GET the session's events route, got: {request_line}"
    );
    let body = format!("{}{}", frame(1), frame(2));
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                 connection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .as_bytes(),
        )
        .await
        .expect("stream the first two frames");
    drop(stream);

    // Connection 2 — the reconnect; it MUST carry Last-Event-ID: 2.
    let (mut stream, _) = listener.accept().await.expect("accept the reconnect");
    let (request_line, headers) = read_request(&mut stream).await;
    assert!(
        request_line.contains("/api/v1/paper/sessions/s-resume/events"),
        "the reconnect must GET the same route, got: {request_line}"
    );
    let resume = headers
        .iter()
        .find_map(|header| {
            let (name, value) = header.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("last-event-id")
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default();
    assert_eq!(resume, "2", "the reconnect resumes from the last seen seq");
    let terminal = format!(
        "event: error\ndata: {}\n\n",
        json!({ "code": "token_refused", "message": "the presented token has been revoked" })
    );
    let body = format!("{}{}{}", frame(2), frame(3), terminal);
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                 connection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .as_bytes(),
        )
        .await
        .expect("stream the tail");
}

/// One request (head + Content-Length body) off a fresh TCP stream — the
/// `client_proxy.rs` plumbing, copied so this suite owns its own stub.
async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, Vec<String>) {
    use tokio::io::AsyncReadExt as _;

    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream
            .read_exact(&mut byte)
            .await
            .expect("read request byte");
        buf.push(byte[0]);
        let text = String::from_utf8_lossy(&buf);
        if let Some(pos) = text.find("\r\n\r\n") {
            let head = text[..pos].to_owned();
            let mut lines = head.lines();
            let request_line = lines.next().unwrap_or_default().to_owned();
            let mut headers: Vec<String> = lines.map(str::to_owned).collect();
            let length = headers
                .iter()
                .find_map(|h| {
                    let (name, value) = h.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if length > 0 {
                let mut body = vec![0u8; length];
                stream
                    .read_exact(&mut body)
                    .await
                    .expect("read request body");
                headers.push(format!("__body:{}", String::from_utf8_lossy(&body)));
            }
            return (request_line, headers);
        }
    }
}

/// Revocation ends the stream with a terminal `TokenRefused` and the reader
/// returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iii_c_revocation_emits_a_terminal_token_refused_and_stops() {
    let ts = paper_server().await;
    script_bars(&ts);
    let host = ts.paper.as_ref().expect("the paper seam").clone();
    let state = Arc::new(paper_state(&ts).await);
    let version_id = certified_version(&ts.db, "w5-stream-revoke").await;
    let promoted = state
        .paper_promote(&promote_request(&version_id))
        .await
        .expect("promote decodes");
    // A tail so the stream has something to deliver before the revocation.
    host.paper()
        .append_bar(
            &PaperSessionId::new(promoted.id.clone()),
            &[],
            &[PaperEvent::DataEvent {
                seq: 0,
                at: "2025-02-01T01:00:00.000Z".to_owned(),
                summary: "revocation probe".to_owned(),
            }],
        )
        .await
        .expect("append the probe event");

    let (channel, received) = recording_sink();
    let reader_state = Arc::clone(&state);
    let reader_id = promoted.id.clone();
    let reader = tokio::spawn(async move {
        reader_state
            .paper_session_events(&reader_id, None, &channel)
            .await
    });

    wait_for(
        || !received.lock().unwrap().is_empty(),
        Duration::from_secs(15),
        "the backlog before the revocation",
    )
    .await;
    SqliteClientTokenRepo::new(ts.db.pool().clone())
        .revoke("w2-app")
        .await
        .expect("revoke the token");

    let outcome = tokio::time::timeout(Duration::from_secs(15), reader)
        .await
        .expect("the revocation ends the stream within one poll")
        .expect("the reader task joins");
    assert!(
        outcome.is_ok(),
        "the refusal is terminal, not an error: {outcome:?}"
    );

    let events = received.lock().unwrap().clone();
    let last = events.last().expect("a terminal event");
    assert_eq!(
        last["kind"],
        json!("tokenRefused"),
        "the refusal is the last event: {events:?}"
    );
    assert!(
        last["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "the refusal carries the server's reason: {last:?}"
    );
}

/// A dropped channel stops the reader — the unmount shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iii_d_a_dropped_channel_stops_the_reader() {
    let ts = paper_server().await;
    script_bars(&ts);
    let host = ts.paper.as_ref().expect("the paper seam").clone();
    let state = paper_state(&ts).await;
    let version_id = certified_version(&ts.db, "w5-stream-drop").await;
    let promoted = state
        .paper_promote(&promote_request(&version_id))
        .await
        .expect("promote decodes");
    host.paper()
        .append_bar(
            &PaperSessionId::new(promoted.id.clone()),
            &[],
            &[PaperEvent::DataEvent {
                seq: 0,
                at: "2025-02-01T01:00:00.000Z".to_owned(),
                summary: "drop probe".to_owned(),
            }],
        )
        .await
        .expect("append the probe event");

    let outcome = tokio::time::timeout(
        Duration::from_secs(15),
        state.paper_session_events(&promoted.id, None, &DeadSink),
    )
    .await
    .expect("a dead channel ends the reader, not a hang");
    assert!(
        outcome.is_ok(),
        "a dead channel is cancellation, not an error: {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// (iv) stop_all failures
// ---------------------------------------------------------------------------

/// Promote one healthy session plus one the runtime will fail to stop (its log
/// is stopped out of band, so the runtime's own stop hits the read-only wall).
async fn broken_pair(ts: &TestServer, state: &ClientState, tag: &str) -> (String, String) {
    let healthy = state
        .paper_promote(&promote_request(
            &certified_version(&ts.db, &format!("{tag}-ok")).await,
        ))
        .await
        .expect("the healthy promote decodes");
    let broken = state
        .paper_promote(&promote_request(
            &certified_version(&ts.db, &format!("{tag}-bad")).await,
        ))
        .await
        .expect("the broken promote decodes");
    stop_out_of_band(ts, &broken.id).await;
    (healthy.id, broken.id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iv_stop_all_returns_every_failure_never_a_bare_success() {
    // The route's own answer, on its own server: one session stopped, one
    // failure named.
    let raw_ts = paper_server().await;
    script_bars(&raw_ts);
    let raw_state = paper_state(&raw_ts).await;
    let (healthy, broken) = broken_pair(&raw_ts, &raw_state, "w5-sweep-raw").await;
    let raw = post_json(&raw_ts, "/api/v1/paper/stop-all").await;
    assert_eq!(raw["stopped"], json!([healthy]));
    assert_eq!(raw["failures"].as_array().map(Vec::len), Some(1), "{raw}");
    assert_eq!(raw["failures"][0]["id"], json!(broken));
    assert_eq!(raw["failures"][0]["code"], json!("data"));

    // The command decodes that same shape — never a bare success. A separate
    // server: the failed session stays attached (its stop failed), so a second
    // sweep on the same runtime would legitimately fail it again.
    let ts = paper_server().await;
    script_bars(&ts);
    let state = paper_state(&ts).await;
    let (healthy, broken) = broken_pair(&ts, &state, "w5-sweep-dto").await;
    let all = state.paper_stop_all().await.expect("stop-all decodes");
    assert_eq!(
        all.stopped,
        vec![healthy.clone()],
        "the healthy session stops"
    );
    assert_eq!(
        all.failures
            .iter()
            .filter(|failure| failure.id == broken)
            .count(),
        1,
        "the broken one is a failure, not a silent skip: {all:?}"
    );
    assert_eq!(
        all.failures
            .iter()
            .find(|failure| failure.id == broken)
            .map(|failure| failure.code.as_str()),
        Some("data")
    );
    assert_same_keys(&all, &raw);
}

// ---------------------------------------------------------------------------
// The frame DTO, directly
// ---------------------------------------------------------------------------

/// The stream event's frame mirrors the wire event: the seq and type the
/// reader decoded, and the payload as its JSON text.
#[test]
fn the_frame_payload_is_the_events_json_text() {
    let frame = PaperEventFrame {
        seq: 7,
        r#type: "fill".to_owned(),
        payload: PaperJsonText::new(json!({ "type": "fill", "seq": 7 }).to_string()),
    };
    let value = serde_json::to_value(PaperStreamEvent::Frame(frame)).expect("serialize");
    assert_eq!(value["kind"], json!("frame"));
    assert_eq!(value["seq"], json!(7));
    assert_eq!(value["type"], json!("fill"));
    let payload: Value =
        serde_json::from_str(value["payload"].as_str().expect("text")).expect("json");
    assert_eq!(payload["seq"], json!(7));
}
