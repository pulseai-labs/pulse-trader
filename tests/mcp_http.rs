//! AC-2 — MCP over HTTP (`tests/mcp_http.rs`, r3.s3.w5).
//!
//! The always-on server serves the SAME `PulseMcp` the stdio transport serves,
//! mounted at `/mcp` under the Agent scope over the streamable-HTTP transport.
//! This suite proves the three behaviours the AC names:
//!
//! 1. **The mounted service answers real MCP traffic over HTTP**: initialize,
//!    the `notifications/initialized` ack, `tools/list`, and one read tool
//!    call — driven with raw HTTP exactly as the streamable-HTTP transport
//!    frames it (SSE bodies unwrapped, `Mcp-Session-Id` learned from the
//!    response header, `MCP-Protocol-Version` echoed back). These are also
//!    the wire semantics the relay speaks, so this test is the relay's
//!    protocol reference.
//! 2. **The scope boundary holds over HTTP**: an `app` token on `/mcp` is a
//!    403 `scope_refused`, the same refusal a plain route answers.
//! 3. **The relay is the stdio agent's path**: `pulse mcp login --server`
//!    verifies the token and writes the connection file, then a bare
//!    `pulse mcp` — the exact process an MCP client spawns — relays
//!    initialize, initialized, and `tools/list` through to the in-process
//!    server over newline-delimited JSON on stdout.
//!
//! Offline: no network beyond 127.0.0.1, no credential, no Keychain.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::io::Write as _;
use std::time::Duration;

use serde_json::{Value, json};
use support::server::{TestServer, spawn_server};

/// The initialize request the relay and this suite's direct client send.
fn initialize_frame() -> String {
    json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "pulse-mcp-http-test", "version": "0.0.0" }
        }
    })
    .to_string()
}

/// POST one JSON-RPC frame to the server's `/mcp` and return
/// `(status, session id header, payloads)` — the `data:`-unwrapped payloads of
/// an SSE body, or the bare JSON body as one payload.
async fn post_mcp(
    server: &TestServer,
    token: &str,
    session: Option<&str>,
    protocol_version: Option<&str>,
    frame: &str,
) -> (reqwest::StatusCode, Option<String>, Vec<Value>) {
    let http = reqwest::Client::new();
    let mut request = http
        .post(format!("{}/mcp", server.base))
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
            .map(|v| vec![v])
            .unwrap_or_default()
    };
    (status, session_id, payloads)
}

/// Drive initialize + initialized + tools/list + one read tool call straight
/// over HTTP and assert every answer is the server's own MCP voice.
#[tokio::test]
async fn mcp_over_http_serves_initialize_tools_and_a_read_call() {
    let server = spawn_server(support::server::ServerOptions::default()).await;

    // initialize → the server's identity and a session id.
    let (status, session, payloads) = post_mcp(
        &server,
        &server.agent_token,
        None,
        None,
        &initialize_frame(),
    )
    .await;
    assert!(status.is_success(), "initialize must answer: HTTP {status}");
    let session = session.expect("the server issues an Mcp-Session-Id");
    let init = payloads
        .iter()
        .find(|p| p.get("id") == Some(&json!(0)))
        .expect("an initialize result frame");
    let protocol_version = init["result"]["protocolVersion"]
        .as_str()
        .expect("the server names its protocol version")
        .to_owned();
    assert!(
        init["result"]["serverInfo"]["name"] == json!("pulse"),
        "the mounted service is PulseTrader's MCP server"
    );

    // notifications/initialized → the protocol's 202-style ack, no payload.
    let (status, _, _) = post_mcp(
        &server,
        &server.agent_token,
        Some(&session),
        Some(&protocol_version),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    assert!(
        status.is_success(),
        "the initialized notification is accepted: HTTP {status}"
    );

    // tools/list → the read+write tools the stdio transport serves.
    let (status, _, payloads) = post_mcp(
        &server,
        &server.agent_token,
        Some(&session),
        Some(&protocol_version),
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string(),
    )
    .await;
    assert!(status.is_success(), "tools/list must answer: HTTP {status}");
    let tools = payloads
        .iter()
        .find(|p| p.get("id") == Some(&json!(1)))
        .expect("a tools/list result frame")["result"]["tools"]
        .as_array()
        .expect("tools is an array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for name in ["list_strategies", "run_backtest", "submit_strategy_version"] {
        assert!(
            tools.iter().any(|n| n == name),
            "the tool set is unchanged over HTTP: missing {name} in {tools:?}"
        );
    }

    // one read tool call → a result frame, not an error.
    let (status, _, payloads) = post_mcp(
        &server,
        &server.agent_token,
        Some(&session),
        Some(&protocol_version),
        &json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/call",
            "params": { "name": "list_strategies", "arguments": {} }
        })
        .to_string(),
    )
    .await;
    assert!(status.is_success(), "tools/call must answer: HTTP {status}");
    let call = payloads
        .iter()
        .find(|p| p.get("id") == Some(&json!(2)))
        .expect("a tools/call result frame");
    assert!(
        call.get("result").is_some() && call.get("error").is_none(),
        "list_strategies answers with a result over HTTP: {call}"
    );
}

/// An `app` token on `/mcp` is a 403 `scope_refused` — the agent-scoped mount
/// refuses exactly as a plain route does.
#[tokio::test]
async fn an_app_token_on_mcp_is_refused_with_scope_refused() {
    let server = spawn_server(support::server::ServerOptions::default()).await;
    let (status, _, payloads) =
        post_mcp(&server, &server.app_token, None, None, &initialize_frame()).await;
    assert_eq!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "an app token must not reach the MCP mount"
    );
    let body = payloads
        .first()
        .cloned()
        .unwrap_or_else(|| json!({ "message": "" }));
    let rendered = serde_json::to_string(&body).unwrap_or_default();
    assert!(
        rendered.contains("scope_refused") || rendered.contains("scope"),
        "the refusal names the scope boundary: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// The relay — `pulse mcp` as the stdio agent's path
// ---------------------------------------------------------------------------

/// `pulse mcp login --server <url>` verifies the token and writes the
/// connection file; stdout stays EMPTY (the file is the artifact).
// Multi-threaded runtime ON PURPOSE: the harness server is a task on this
// test's own runtime, and the login CHILD below is waited on with a BLOCKING
// `wait_with_output` — on the default current-thread flavor that wait would
// starve the server task and deadlock the handshake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_verifies_the_token_and_writes_the_connection_file() {
    use std::os::unix::fs::PermissionsExt as _;

    let server = spawn_server(support::server::ServerOptions::default()).await;
    let config_dir = tempfile::TempDir::new().expect("tempdir");

    let mut login = std::process::Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args(["mcp", "login", "--server", &server.base])
        .env("PULSE_CONFIG_DIR", config_dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn pulse mcp login");
    login
        .stdin
        .take()
        .expect("login stdin")
        .write_all(format!("{}\n", server.agent_token).as_bytes())
        .expect("write the token");
    let out = login.wait_with_output().expect("login completes");

    assert!(
        out.status.success(),
        "login must succeed against a live server: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "login prints nothing on stdout — the file is the artifact, got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let file = config_dir.path().join("mcp-connection.toml");
    let text = std::fs::read_to_string(&file).expect("the connection file exists");
    assert!(
        text.contains(server.base.trim_end_matches('/')) && text.contains("token"),
        "the file names the server and a token, never a Keychain: {text}"
    );
    // Mode 0600: a connection file with wider permissions is a refusal waiting
    // to happen.
    let mode = std::fs::metadata(&file)
        .expect("stat the file")
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o777,
        0o600,
        "the connection file is owner-only: mode {:o}",
        mode & 0o777
    );
}

/// With no login and no `--local`, the relay refuses with the spec's literal
/// line on stderr (spec ~199) — the operator's next action, verbatim.
#[tokio::test]
async fn bare_mcp_without_login_names_the_spec_literal() {
    let config_dir = tempfile::TempDir::new().expect("tempdir");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pulse"))
        .arg("mcp")
        .env("PULSE_CONFIG_DIR", config_dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("spawn bare pulse mcp");
    assert!(
        !out.status.success(),
        "no login must exit non-zero, got {}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no server login: run 'pulse mcp login --server <url>' or pass --local"),
        "the spec's literal wording must reach stderr, got: {stderr}"
    );
}

/// Item 2's missing spec AC-2 case: a version submitted over `/mcp` with a
/// token labelled `claude-code` records `agent_name = claude-code` — the
/// authenticated label is the agent's final identity, outranking whatever the
/// client calls itself in `clientInfo`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_labelled_claude_code_records_its_agent_identity() {
    let server = spawn_server(support::server::ServerOptions::default()).await;
    let token = support::server::issue_token("claude-code", "agent", &server.db_path);

    // initialize + initialized + one submit, over the mounted /mcp.
    let (status, session, payloads) =
        post_mcp(&server, &token, None, None, &initialize_frame()).await;
    assert!(status.is_success(), "initialize must answer: HTTP {status}");
    let session = session.expect("session id");
    let protocol_version = payloads
        .iter()
        .find_map(|p| p["result"]["protocolVersion"].as_str().map(str::to_owned))
        .expect("protocol version");
    let (status, _, _) = post_mcp(
        &server,
        &token,
        Some(&session),
        Some(&protocol_version),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    assert!(status.is_success());
    let dsl: Value =
        serde_json::from_str(support::mcp::MINIMAL_DSL).expect("the minimal DSL is JSON");
    let (status, _, payloads) = post_mcp(
        &server,
        &token,
        Some(&session),
        Some(&protocol_version),
        &json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/call",
            "params": {
                "name": "submit_strategy_version",
                "arguments": {
                    "strategy_name": "Claude Coded RSI",
                    "dsl": dsl,
                    "hypothesis": "the label outlives the client's self-chosen name"
                }
            }
        })
        .to_string(),
    )
    .await;
    assert!(status.is_success(), "the submit must answer: HTTP {status}");
    let call = payloads
        .iter()
        .find(|p| p.get("id") == Some(&json!(2)))
        .expect("a tools/call result frame");
    assert!(
        call.get("error").is_none(),
        "the submit must succeed over /mcp: {call}"
    );

    // The persisted version's author label is the TOKEN's, not clientInfo's.
    let http = reqwest::Client::new();
    let overview = http
        .get(format!("{}/api/v1/library-overview", server.base))
        .bearer_auth(&server.app_token)
        .send()
        .await
        .expect("read the library");
    let body: Value = overview.json().await.expect("overview JSON");
    let rendered = serde_json::to_string(&body).expect("render");
    assert!(
        rendered.contains("claude-code"),
        "the token label must be the recorded agent identity, got: {rendered}"
    );
    assert!(
        !rendered.contains("pulse-mcp-http-test"),
        "clientInfo.name must never outrank the authenticated label"
    );
}

/// A bare `pulse mcp` relays newline-delimited JSON-RPC on stdin to the
/// server's `/mcp` and answers on stdout — initialize, the initialized
/// notification (which produces NO stdout line), and tools/list.
// Multi-threaded for the same reason as `login_verifies_...`: the relay
// child's login step blocks on `wait_with_output`, and the in-process server
// must keep polling while it does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_serves_a_stdio_client_over_the_mounted_mcp() {
    use tokio::io::AsyncWriteExt as _;

    let server = spawn_server(support::server::ServerOptions::default()).await;
    let config_dir = tempfile::TempDir::new().expect("tempdir");

    // Log in first — the same two steps the spec's operator walk names.
    let mut login = std::process::Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args(["mcp", "login", "--server", &server.base])
        .env("PULSE_CONFIG_DIR", config_dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn pulse mcp login");
    login
        .stdin
        .take()
        .expect("login stdin")
        .write_all(format!("{}\n", server.agent_token).as_bytes())
        .expect("write the token");
    let out = login.wait_with_output().expect("login completes");
    assert!(out.status.success(), "login must succeed");

    // The bare relay: exactly the process an MCP client spawns.
    let mut relay = tokio::process::Command::new(env!("CARGO_BIN_EXE_pulse"))
        .arg("mcp")
        .env("PULSE_CONFIG_DIR", config_dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn bare pulse mcp");

    let mut stdin = relay.stdin.take().expect("relay stdin");
    let mut stdout = tokio::io::BufReader::new(relay.stdout.take().expect("relay stdout"));

    // initialize → one JSON line on stdout with the server's identity.
    stdin
        .write_all(format!("{}\n", initialize_frame()).as_bytes())
        .await
        .expect("write initialize");
    let init_line = read_line_with_timeout(&mut stdout).await;
    let init: Value =
        serde_json::from_str(init_line.trim()).expect("the initialize answer is one JSON-RPC line");
    assert_eq!(init["id"], json!(0), "the answer matches the request id");
    assert!(
        init["result"]["serverInfo"]["name"] == json!("pulse"),
        "the relayed initialize carries the server's identity: {init}"
    );
    let protocol_version = init["result"]["protocolVersion"]
        .as_str()
        .expect("protocolVersion")
        .to_owned();

    // notifications/initialized → accepted, and NOT answered on stdout (a
    // stdio client would hang on a spurious line).
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .expect("write initialized");
    // Give the relay a moment; then assert nothing crossed stdout.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // tools/list → the tools the stdio transport serves, over the relay.
    stdin
        .write_all(
            format!(
                "{}\n",
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})
            )
            .as_bytes(),
        )
        .await
        .expect("write tools/list");
    let list_line = read_line_with_timeout(&mut stdout).await;
    let list: Value =
        serde_json::from_str(list_line.trim()).expect("the tools/list answer is one JSON-RPC line");
    assert_eq!(list["id"], json!(1));
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        names.contains(&"list_strategies") && names.contains(&"run_backtest"),
        "the relayed tools/list carries the unchanged tool set: {names:?}"
    );
    let _ = protocol_version; // the relay owns the version echo; the test
    // asserts the frames the CLIENT sees.

    // stdin close → the relay exits 0.
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(10), relay.wait())
        .await
        .expect("the relay exits after stdin closes")
        .expect("wait the relay");
    assert!(
        status.success(),
        "the relay exits 0 on stdin close, got {status}"
    );
}

/// Read one stdout line with a guard timeout, so a broken relay fails the
/// test instead of hanging the suite.
async fn read_line_with_timeout(
    stdout: &mut tokio::io::BufReader<tokio::process::ChildStdout>,
) -> String {
    use tokio::io::AsyncBufReadExt as _;
    tokio::time::timeout(Duration::from_secs(15), stdout.lines().next_line())
        .await
        .expect("a stdout line arrives within 15s")
        .expect("the relay wrote a line before EOF")
        .expect("the line is valid utf-8")
}
