//! AC-1 — the thin-client proxy surface (`tests/client_proxy.rs`).
//!
//! The Mac app's command bus is a PROXY now: every `#[tauri::command]` speaks
//! HTTP to the always-on server through [`pulse::ServerClient`], driven by the
//! connection [`pulse::ClientState`]. This suite pins that proxy over the REAL
//! in-process server (`support::server`) — the same harness
//! `tests/server_stream.rs` and `tests/server_routes.rs` use — plus a raw TCP
//! stub where the suite must control the WIRE itself (a dropped SSE connection,
//! a non-202 POST).
//!
//! Covered groups (spec AC-1 (i)–(viii)):
//!
//! (i)–(iii)  the three GET reads (shell-info, credential-status,
//!            library-overview) proxy to the same values the cores produce;
//! (iv)       `bus-selftest-failure` proxies to the typed `BusError`;
//! (v)        `compose-cancel` proxies its plain POST;
//! (vi)       `compare-child-run` proxies read AND typed refusal;
//! (vii)      an operation stream forwards its events into a REAL
//!            `tauri::ipc::Channel` and returns the terminal result;
//! (viii)     a stream that outlives one SSE connection resumes with
//!            `Last-Event-ID` — no duplicates, no gaps — and refusal statuses
//!            (422 body, 404 `op_unknown`) map to the typed error.
//!
//! plus the connection lifecycle the three new commands own: connect
//! (handshake pinned to [`pulse::API_VERSION`]), `ServerStatus` transitions
//! (Disconnected → Refused → Connected → Disconnected), and dead-socket
//! refusal. The version-skew case lives in `tests/server_stream.rs` (AC-3),
//! where the spec put it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pulse::{
    BusErrorCode, ClientState, CompareChildRunRequest, ConnectOutcome, CredentialStatus,
    LibraryOverview, RunOpOutcome, ServerClient, ServerStatusState, ShellInfo, StreamOutcome,
    compare_child_run_core, compose_strategy_body, library_overview_core, llm_credential_status,
    shell_info_core,
};

// ---------------------------------------------------------------------------
// Dispatch-2 correction 1 — the compose proxy forwards the route's LITERAL key
// ---------------------------------------------------------------------------

/// F1: each route's shape is forwarded exactly as w2 landed it. The route
/// (`ops.rs::op_compose_strategy`) parses `nl_target`; a `nlTarget` body 422s
/// with `compose-strategy requires {"nl_target": "..."}` — which is what the
/// Designer's every compose did before this fix.
#[test]
fn the_compose_proxy_posts_the_routes_literal_body_key() {
    let body = compose_strategy_body("a strategy that buys the dip");
    assert_eq!(
        body["nl_target"],
        json!("a strategy that buys the dip"),
        "the route's literal key is forwarded unchanged"
    );
    assert!(
        body.get("nlTarget").is_none(),
        "the camelCase key is the wire bug: the route refuses it with 422"
    );
}
use serde_json::{Value, json};
use support::server::{ServerOptions, TestServer, spawn_server};
use tauri::ipc::{Channel, InvokeResponseBody};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A real `tauri::ipc::Channel<BusEvent>` plus the decoded events it received
/// (`tests/tauri_bus_contract.rs`'s recorder — the proxy must feed the SAME
/// channel type the webview would hold).
fn recording_channel() -> (Channel<pulse::BusEvent>, Arc<Mutex<Vec<Value>>>) {
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

/// A client already past the handshake, asserting the pinned version on the way.
async fn connected(server: &TestServer) -> ServerClient {
    let (client, outcome) = ServerClient::connect(&server.base, &server.app_token)
        .await
        .expect("handshake against the in-process server");
    match outcome {
        ConnectOutcome::Connected {
            binary_version,
            engine_fingerprint,
        } => {
            assert!(
                !binary_version.is_empty() && !engine_fingerprint.is_empty(),
                "the handshake names the server's identity"
            );
        }
        other => panic!("the in-process server must connect cleanly, got {other:?}"),
    }
    client
}

/// One JSON value comparison, so DTO `PartialEq` bounds never gate a test.
fn same_wire<T: serde::Serialize>(a: &T, b: &T) -> bool {
    serde_json::to_value(a).unwrap() == serde_json::to_value(b).unwrap()
}

// ---------------------------------------------------------------------------
// (i)–(iii) — the three GET reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shell_info_via_the_proxy_matches_the_core() {
    let server = spawn_server(ServerOptions::default()).await;
    let client = connected(&server).await;

    let proxied: ShellInfo = client
        .get_json("/api/v1/shell-info")
        .await
        .expect("proxy the shell-info read");
    let desktop = server.desktop().await;
    let core = shell_info_core(&desktop).await.expect("core read");

    assert!(
        same_wire(&proxied, &core),
        "the proxy must deliver exactly what the core produces: {proxied:?} vs {core:?}"
    );
}

#[tokio::test]
async fn credential_status_via_the_proxy_matches_the_direct_read() {
    let server = spawn_server(ServerOptions::default()).await;
    let client = connected(&server).await;

    let proxied: CredentialStatus = client
        .get_json("/api/v1/credential-status")
        .await
        .expect("proxy the credential-status read");
    // The server runs IN this process, so its credential read sees the same
    // environment the direct call does — the comparison is exact, not-shaped.
    let direct = llm_credential_status();

    assert!(
        same_wire(&proxied, &direct),
        "the proxy must deliver the credential banner's own read"
    );
}

#[tokio::test]
async fn library_overview_via_the_proxy_matches_the_core() {
    let server = spawn_server(ServerOptions::default()).await;
    let desktop = server.desktop().await;
    support::server::seed_version(&desktop).await;
    let client = connected(&server).await;

    let proxied: LibraryOverview = client
        .get_json("/api/v1/library-overview")
        .await
        .expect("proxy the library read");
    let core = library_overview_core(&desktop).await.expect("core read");

    assert!(
        same_wire(&proxied, &core),
        "the proxy must deliver the seeded overview the core produces"
    );
}

// ---------------------------------------------------------------------------
// (iv) — the deliberate-failure command proxies to the typed error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bus_selftest_failure_proxies_to_the_typed_bus_error() {
    let server = spawn_server(ServerOptions::default()).await;
    let client = connected(&server).await;

    let err = client
        .post_json::<Value, _>("/api/v1/bus-selftest-failure", &json!({}))
        .await
        .expect_err("the deliberate failure must come back as an error, not a parse accident");

    assert_eq!(
        err.code,
        BusErrorCode::Data,
        "the self-test maps DataError::Parse; the proxy must preserve the mapped family"
    );
    assert!(
        err.message.contains("deliberate bus self-test failure"),
        "the typed message must survive the round trip, got: {}",
        err.message
    );
}

// ---------------------------------------------------------------------------
// (v) — compose-cancel's plain POST
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compose_cancel_proxies_its_plain_post() {
    let server = spawn_server(ServerOptions::default()).await;
    let client = connected(&server).await;

    // An unknown run id is an ordinary `false`, not an error (the run may have
    // finished between the screen deciding to cancel and this arriving).
    let was_in_flight: bool = client
        .post_json("/api/v1/compose-cancel", &json!({ "runId": "no-such-run" }))
        .await
        .expect("proxy the compose-cancel POST");
    assert!(
        !was_in_flight,
        "cancelling an unknown run must report not-in-flight"
    );
}

// ---------------------------------------------------------------------------
// (vi) — compare-child-run proxies reads AND typed refusals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compare_child_run_proxies_the_typed_refusal() {
    let server = spawn_server(ServerOptions::default()).await;
    let client = connected(&server).await;
    let request = CompareChildRunRequest {
        child_run_id: "no-such-run".to_owned(),
    };

    let err = client
        .post_json::<Value, _>("/api/v1/compare-child-run", &request)
        .await
        .expect_err("an unknown run must refuse");

    assert_eq!(err.code, BusErrorCode::NotFound);
    assert!(
        err.message.contains("no-such-run"),
        "the refusal must name the run it was asked about, got: {}",
        err.message
    );

    // The core refuses with the SAME family on the same state — the proxy adds
    // transport, not semantics.
    let desktop = server.desktop().await;
    let core_err = compare_child_run_core(&desktop, request)
        .await
        .expect_err("the core refuses identically");
    assert_eq!(
        err.code, core_err.code,
        "proxy and core must refuse with the same mapped family"
    );
}

// ---------------------------------------------------------------------------
// (vii) — an operation stream into a REAL channel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_operation_stream_forwards_events_into_a_real_channel_and_returns_the_terminal() {
    let server = spawn_server(ServerOptions::default()).await;
    let client = connected(&server).await;
    let (channel, received) = recording_channel();

    let outcome: RunOpOutcome<StreamOutcome> = client
        .run_op("ops/start-demo-stream", &json!({ "steps": 4 }), &channel)
        .await
        .expect("run the demo op through the proxy");

    let value = outcome.value;
    assert!(!value.cancelled, "an undisturbed run is not cancelled");
    assert!(!outcome.cancelled);
    assert_eq!(
        outcome.emitted, value.emitted,
        "the proxy's send count and the terminal result's must agree"
    );

    let events = received.lock().unwrap().clone();
    assert!(
        !events.is_empty(),
        "a stream that emits nothing proves nothing"
    );
    assert_eq!(
        u32::try_from(events.len()).expect("event count fits u32"),
        outcome.emitted,
        "every forwarded event must have reached the channel"
    );
    for (i, event) in events.iter().enumerate() {
        assert_eq!(
            event["runId"],
            json!(value.run_id.as_str()),
            "event {i} belongs to the op's run id"
        );
        assert_eq!(
            event["seq"],
            json!(i as u64),
            "sequence numbers must arrive monotonic from 0 with no gaps"
        );
    }
}

// ---------------------------------------------------------------------------
// (viii) — resume across a dropped SSE connection, over the raw wire
// ---------------------------------------------------------------------------

/// Minimal HTTP plumbing for the wire stubs below: read one request
/// (head + Content-Length body) off a fresh TCP stream.
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

/// The demo-op wire stub lives in [`run_stub_drop_mid_stream`] below.

#[tokio::test]
async fn the_client_resumes_after_a_dropped_stream_without_dups_or_gaps() {
    // The stub binds its own ephemeral port and hands the client the real one.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub");
    let base = format!("http://{}", listener.local_addr().expect("stub local"));
    let handle = tokio::spawn(async move {
        run_stub_drop_mid_stream(listener).await;
    });

    let (channel, received) = recording_channel();
    let client = ServerClient::new(&base, "any-token");
    let outcome: RunOpOutcome<StreamOutcome> = client
        .run_op("ops/start-demo-stream", &json!({ "steps": 3 }), &channel)
        .await
        .expect("the proxy must resume past the dropped connection");

    handle.await.expect("stub completes");

    let value = outcome.value;
    assert_eq!(
        value.emitted, 3,
        "the terminal result must be the server's own outcome"
    );
    assert!(!value.cancelled);
    assert!(!outcome.cancelled);
    assert_eq!(outcome.emitted, 3, "exactly three sends, once each");

    let events = received.lock().unwrap().clone();
    let seqs: Vec<u64> = events
        .iter()
        .map(|e| e["seq"].as_u64().expect("numeric seq"))
        .collect();
    assert_eq!(
        seqs,
        vec![0, 1, 2],
        "the resumed stream must be gap-free and duplicate-free, got {seqs:?}"
    );
}

/// The stub body, factored so the test above owns only its assertions.
async fn run_stub_drop_mid_stream(listener: tokio::net::TcpListener) {
    use tokio::io::AsyncWriteExt as _;

    let run_id = "op-resume-1";
    let frame = |seq: u32, message: &str| {
        format!(
            "id: {seq}\nevent: bus\ndata: {}\n\n",
            json!({
                "runId": run_id,
                "seq": seq,
                "payload": { "kind": "progress", "message": message }
            })
        )
    };
    let terminal = format!(
        "event: result\ndata: {}\n\n",
        json!({ "runId": run_id, "emitted": 3, "cancelled": false })
    );

    // Connection 1 — the POST (answered with `connection: close` so the
    // client's next request opens the fresh socket the stub controls).
    let (mut stream, _) = listener.accept().await.expect("accept POST");
    let (request_line, _) = read_request(&mut stream).await;
    assert!(
        request_line.contains("/api/v1/ops/start-demo-stream"),
        "the proxy must POST the op route, got: {request_line}"
    );
    let body = json!({ "op_id": run_id }).to_string();
    stream
        .write_all(
            format!(
                "HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\n\
                 connection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .as_bytes(),
        )
        .await
        .expect("answer the POST");

    // Connection 2 — the first events GET: two frames, then the socket dies.
    let (mut stream, _) = listener.accept().await.expect("accept first GET");
    let (request_line, _) = read_request(&mut stream).await;
    assert!(
        request_line.contains(format!("/api/v1/ops/{run_id}/events").as_str()),
        "the proxy must GET the events stream, got: {request_line}"
    );
    let body = format!("{}{}", frame(0, "one"), frame(1, "two"));
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
    drop(stream); // the premature close — no terminal frame

    // Connection 3 — the reconnect; it MUST carry Last-Event-ID: 1.
    let (mut stream, _) = listener.accept().await.expect("accept reconnect");
    let (request_line, headers) = read_request(&mut stream).await;
    assert!(
        request_line.contains(format!("/api/v1/ops/{run_id}/events").as_str()),
        "the reconnect must GET the same events route, got: {request_line}"
    );
    let resume = headers
        .iter()
        .find_map(|h| {
            let (name, value) = h.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("last-event-id")
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default();
    assert_eq!(
        resume, "1",
        "the reconnect must resume from the last seen seq"
    );
    let body = format!("{}{}", frame(2, "three"), terminal);
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
        .expect("stream the rest");
}

// ---------------------------------------------------------------------------
// Refusal mapping over the raw wire
// ---------------------------------------------------------------------------

/// A POST that answers 422 with a `BusError` body must map to the typed error.
#[tokio::test]
async fn a_422_post_maps_to_the_typed_bus_error() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
    let addr = probe.local_addr().expect("probe addr");
    drop(probe);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind stub");
    let base = format!("http://{}", listener.local_addr().expect("stub local"));
    let handle = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt as _;
        let (mut stream, _) = listener.accept().await.expect("accept POST");
        let _ = read_request(&mut stream).await;
        let body = json!({
            "code": "validation",
            "message": "window must be within the pinned snapshot",
            "run_id": null,
            "session_id": null,
            "child_run_id": null
        })
        .to_string();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 422 Unprocessable Entity\r\ncontent-type: application/json\r\n\
                     connection: close\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .expect("answer 422");
    });

    let client = ServerClient::new(&base, "any-token");
    let err = client
        .run_op::<StreamOutcome, _>(
            "ops/start-demo-stream",
            &json!({ "steps": 1 }),
            &recording_channel().0,
        )
        .await
        .expect_err("a 422 must surface as the typed error");
    handle.await.expect("stub completes");

    assert_eq!(err.code, BusErrorCode::Validation);
    assert!(
        err.message
            .contains("window must be within the pinned snapshot"),
        "the typed message must survive: {}",
        err.message
    );
}

/// A 202 whose events GET answers 404 `op_unknown` must map to the typed error
/// naming the op id.
#[tokio::test]
async fn an_op_unknown_events_get_maps_to_the_typed_bus_error() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
    let addr = probe.local_addr().expect("probe addr");
    drop(probe);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind stub");
    let base = format!("http://{}", listener.local_addr().expect("stub local"));
    let handle = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt as _;
        // POST → 202.
        let (mut stream, _) = listener.accept().await.expect("accept POST");
        let _ = read_request(&mut stream).await;
        let body = json!({ "op_id": "gone-op" }).to_string();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\n\
                     connection: close\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .expect("answer 202");
        // GET events → 404 op_unknown.
        let (mut stream, _) = listener.accept().await.expect("accept GET");
        let _ = read_request(&mut stream).await;
        let body = json!({
            "code": "not_found",
            "message": "op_unknown: gone-op was never issued here or has expired",
            "run_id": null,
            "session_id": null,
            "child_run_id": null
        })
        .to_string();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\n\
                     connection: close\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .expect("answer 404");
    });

    let client = ServerClient::new(&base, "any-token");
    let err = client
        .run_op::<StreamOutcome, _>(
            "ops/start-demo-stream",
            &json!({ "steps": 1 }),
            &recording_channel().0,
        )
        .await
        .expect_err("an op the server never issued must refuse");
    handle.await.expect("stub completes");

    assert_eq!(err.code, BusErrorCode::NotFound);
    assert!(
        err.message.contains("gone-op"),
        "the refusal must name the op id: {}",
        err.message
    );
    // Spec ~123: the message tells the operator where the run may still be.
    assert!(
        err.message.contains("may still be in the library"),
        "an operation-expired refusal must say the run may still be in the library: {}",
        err.message
    );
}

/// The 404 the ops route ACTUALLY sends: `{"code":"op_unknown","message":…}`
/// (`src/server/ops.rs::op_unknown`) — a route token that is not a bus family
/// and two fields, so it cannot deserialize as a `BusError` at all. The typed
/// expiry must still be reached (reading the JSON `code` field), or the app
/// would report a generic internal error for a server-forgotten operation.
#[tokio::test]
async fn the_servers_own_op_unknown_body_maps_to_the_typed_expiry() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub");
    let base = format!("http://{}", listener.local_addr().expect("stub local"));
    let handle = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt as _;
        // POST → 202.
        let (mut stream, _) = listener.accept().await.expect("accept POST");
        let _ = read_request(&mut stream).await;
        let body = json!({ "op_id": "gone-op" }).to_string();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\n\
                     connection: close\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .expect("answer 202");
        // GET events → 404, the route's own shape (two fields, route token).
        let (mut stream, _) = listener.accept().await.expect("accept GET");
        let _ = read_request(&mut stream).await;
        let body = json!({
            "code": "op_unknown",
            "message": "no operation with id gone-op was issued by this server, \
                        or its record has expired"
        })
        .to_string();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\n\
                     connection: close\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .expect("answer 404");
    });

    let client = ServerClient::new(&base, "any-token");
    let err = client
        .run_op::<StreamOutcome, _>(
            "ops/start-demo-stream",
            &json!({ "steps": 1 }),
            &recording_channel().0,
        )
        .await
        .expect_err("an op the server never issued must refuse");
    handle.await.expect("stub completes");

    assert_eq!(
        err.code,
        BusErrorCode::NotFound,
        "the route's token maps to the typed expiry, not an internal error: {}",
        err.message
    );
    assert!(
        err.message.contains("gone-op"),
        "the typed expiry names the op id: {}",
        err.message
    );
    assert!(
        err.message.contains("may still be in the library"),
        "and says where the run may still be: {}",
        err.message
    );
}

// ---------------------------------------------------------------------------
// The connection lifecycle — the three new commands' state machine
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_state_tracks_connect_refusal_disconnect_and_dead_sockets() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = ClientState::new();

    assert_eq!(
        state.status().await.state,
        ServerStatusState::NotConnected,
        "a fresh app state is disconnected"
    );

    // A wrong token is REFUSED — the outcome names it, and the status carries
    // the refusal (with its reason) so the UI returns to Connect.
    let outcome = state
        .connect(&server.base, "pulse-definitely-not-a-token")
        .await;
    match outcome {
        ConnectOutcome::TokenRefused { reason } => {
            assert!(
                !reason.is_empty(),
                "the refusal must carry a display reason"
            );
        }
        other => panic!("a bad token must name token_refused, got {other:?}"),
    }
    let status = state.status().await;
    assert_eq!(status.state, ServerStatusState::Refused);
    assert!(
        status.reason.as_deref().is_some_and(|r| !r.is_empty()),
        "the refused status carries the reason the Connect screen shows"
    );

    // The real token connects; the outcome carries the handshake identity.
    let outcome = state.connect(&server.base, &server.app_token).await;
    match outcome {
        ConnectOutcome::Connected {
            binary_version,
            engine_fingerprint,
        } => {
            assert!(
                !binary_version.is_empty() && !engine_fingerprint.is_empty(),
                "the connect outcome carries the server's identity"
            );
        }
        other => panic!("the app token must connect, got {other:?}"),
    }
    let status = state.status().await;
    assert_eq!(status.state, ServerStatusState::Up);
    assert!(
        status.binary_version.is_some() && status.engine_fingerprint.is_some(),
        "an up status names the server's version and fingerprint"
    );

    // Disconnect returns to not-connected.
    state.disconnect().expect("disconnect");
    assert_eq!(state.status().await.state, ServerStatusState::NotConnected);

    // A dead socket answers Unreachable — and leaves the state not-connected
    // (nothing is wrong with the saved connection; there isn't one).
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
    let dead = format!("http://{}", probe.local_addr().expect("probe addr"));
    drop(probe); // nothing listens there now
    let outcome = state.connect(&dead, "any-token").await;
    match outcome {
        ConnectOutcome::Unreachable { reason } => {
            assert!(!reason.is_empty());
        }
        other => panic!("a dead socket must name unreachable, got {other:?}"),
    }
    assert_eq!(
        state.status().await.state,
        ServerStatusState::NotConnected,
        "an unreachable connect must not poison the connection state"
    );
}

// ---------------------------------------------------------------------------
// Dispatch-2 correction 4 — capped, injectable backoff (spec ~121)
// ---------------------------------------------------------------------------

/// Retry timing is injectable ("tests use milliseconds"): a millisecond policy
/// must make the drop→resume cycle fast — with the default 1s→30s policy this
/// very scenario would take at least a second on the first re-attach alone.
#[tokio::test]
async fn a_millisecond_backoff_policy_makes_the_resume_fast() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub");
    let base = format!("http://{}", listener.local_addr().expect("stub local"));
    let handle = tokio::spawn(async move {
        run_stub_drop_mid_stream(listener).await;
    });

    let (channel, received) = recording_channel();
    let client = ServerClient::with_backoff(
        &base,
        "any-token",
        pulse::RetryBackoff::new(Duration::from_millis(5), Duration::from_millis(5)),
    );
    let started = std::time::Instant::now();
    let outcome: RunOpOutcome<StreamOutcome> = client
        .run_op("ops/start-demo-stream", &json!({ "steps": 3 }), &channel)
        .await
        .expect("the proxy resumes past the dropped connection");
    let elapsed = started.elapsed();

    handle.await.expect("stub completes");
    assert_eq!(outcome.value.emitted, 3);
    assert_eq!(outcome.emitted, 3);
    assert!(
        elapsed < Duration::from_secs(2),
        "an injected millisecond policy must keep the resume fast, took {elapsed:?}"
    );

    let seqs: Vec<u64> = received
        .lock()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().expect("numeric seq"))
        .collect();
    assert_eq!(seqs, vec![0, 1, 2], "no dups, no gaps");
}

// ---------------------------------------------------------------------------
// Dispatch-2 correction 3(b) — TWO files: the app's server-connection.toml,
// with mcp-connection.toml BESIDE it (login's output, read by the relay)
// ---------------------------------------------------------------------------

/// A good connect persists THE APP'S OWN file — `server-connection.toml`,
/// mode 0600, holding `{ url, token }` — and never touches
/// `mcp-connection.toml` (that one is `pulse mcp login`'s artifact).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_persists_the_apps_server_connection_file_beside_mcp_connection() {
    use std::os::unix::fs::PermissionsExt as _;

    let server = spawn_server(ServerOptions::default()).await;
    let config_dir = tempfile::TempDir::new().expect("tempdir");
    // SAFETY: nextest runs every test in its own process — no other thread
    // reads the environment while this runs.
    unsafe {
        std::env::set_var("PULSE_CONFIG_DIR", config_dir.path());
    }

    let state = ClientState::new();
    let outcome = state.connect(&server.base, &server.app_token).await;
    assert!(
        matches!(outcome, ConnectOutcome::Connected { .. }),
        "the connect must succeed: {outcome:?}"
    );

    let app_file = config_dir.path().join("server-connection.toml");
    assert!(
        app_file.exists(),
        "the app's own connection file must exist at {}",
        app_file.display()
    );
    assert!(
        !config_dir.path().join("mcp-connection.toml").exists(),
        "connect must not write login's file"
    );
    let text = std::fs::read_to_string(&app_file).expect("read the file");
    assert!(
        text.contains(server.base.trim_end_matches('/')) && text.contains("token"),
        "the file names the server and the token: {text}"
    );
    let mode = std::fs::metadata(&app_file)
        .expect("stat the file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "the app file is owner-only");

    // And a fresh app state loads FROM that file: connected on start-up.
    let reloaded = ClientState::loaded();
    assert_eq!(
        reloaded.status().await.state,
        ServerStatusState::Up,
        "the connection file reconnects a relaunched app"
    );

    // SAFETY: see above — single-test process isolation.
    unsafe {
        std::env::remove_var("PULSE_CONFIG_DIR");
    }
}

/// AC-1 (vii): a loose (`0644`) connection file is refused on load with a
/// named reason — the group/world-bit check `secrets.rs` applies, by name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_loose_connection_file_is_refused_on_load_with_a_named_reason() {
    use std::os::unix::fs::PermissionsExt as _;

    let server = spawn_server(ServerOptions::default()).await;
    let config_dir = tempfile::TempDir::new().expect("tempdir");
    // SAFETY: nextest runs every test in its own process — no other thread
    // reads the environment while this runs.
    unsafe {
        std::env::set_var("PULSE_CONFIG_DIR", config_dir.path());
    }

    let app_file = config_dir.path().join("server-connection.toml");
    std::fs::write(
        &app_file,
        format!(
            "url = \"{}\"\ntoken = \"{}\"\n",
            server.base.trim_end_matches('/'),
            server.app_token
        ),
    )
    .expect("write the loose file");
    std::fs::set_permissions(&app_file, std::fs::Permissions::from_mode(0o644))
        .expect("make it loose");

    let state = ClientState::loaded();
    let status = state.status().await;
    assert_eq!(status.state, ServerStatusState::Refused);
    let reason = status.reason.expect("a refusal carries its reason");
    assert!(
        reason.contains("group/world-readable") || reason.contains("0644"),
        "the refusal must name the loose mode, got: {reason}"
    );

    // The refused state never half-connects: commands refuse too.
    let err = state
        .get::<serde_json::Value>("/api/v1/shell-info")
        .await
        .expect_err("a refused state refuses commands");
    assert!(
        err.message.contains("not connected"),
        "the command refusal names the state: {err}"
    );

    // SAFETY: see above — single-test process isolation.
    unsafe {
        std::env::remove_var("PULSE_CONFIG_DIR");
    }
}
