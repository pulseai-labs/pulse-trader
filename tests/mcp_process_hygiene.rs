//! AC-3 (r2.s1.w2): process hygiene of `pulse mcp` as a black box.
//!
//! Raw pipes drive a hand-rolled JSON-RPC session — `initialize`,
//! `notifications/initialized`, `tools/list`, one `export_candles` call — and
//! every byte the child writes to stdout must be a newline-delimited JSON-RPC
//! message. Closing stdin ends the session and the process must exit 0.
//!
//! The third test is the AC-10 amendment: an invalid `--agent-name` refuses to
//! serve with a non-zero exit AND the reason on stderr (a bare exit-code check
//! cannot tell our validator apart from clap rejecting an unknown subcommand —
//! the stderr text can).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

/// The committed candle fixture (`BTCUSDT` 15m + 4h snapshots) — the export
/// call needs a real snapshot behind `--data-dir`.
const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

fn manifest(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// Recursively copy a directory tree (the fixture → tempdir).
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// Every byte on stdout between two newlines is one JSON-RPC message.
fn assert_line_is_jsonrpc(line: &str, expected_id: i64) {
    let msg: serde_json::Value = serde_json::from_str(line)
        .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line:?}"));
    assert_eq!(
        msg["jsonrpc"], "2.0",
        "stdout line is not a JSON-RPC envelope: {line:?}"
    );
    assert_eq!(
        msg["id"], expected_id,
        "response id matches the request: {line:?}"
    );
    assert!(
        msg.get("result").is_some() || msg.get("error").is_some(),
        "a response carries result or error: {line:?}"
    );
}

/// `pulse mcp --db <db> --data-dir <store>` with all three streams piped.
fn spawn_mcp(db: &Path, store: &Path) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_pulse"))
        .arg("mcp")
        .arg("--db")
        .arg(db)
        .arg("--data-dir")
        .arg(store)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pulse mcp")
}

#[test]
fn stdout_carries_only_jsonrpc_frames_and_stdin_close_exits_cleanly() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("pulse.db");
    let store = tmp.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store);

    let mut child = spawn_mcp(&db, &store);
    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let mut lines = BufReader::new(stdout).lines();

    let send = |stdin: &mut std::process::ChildStdin, msg: &str| {
        stdin.write_all(msg.as_bytes()).expect("write request");
        stdin.write_all(b"\n").expect("write newline");
        stdin.flush().expect("flush request");
    };

    // 1. initialize — the handshake response is stdout's first frame.
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"hygiene-test","version":"0.0.0"}}}"#,
    );
    let line = lines
        .next()
        .expect("initialize response line")
        .expect("line reads");
    assert_line_is_jsonrpc(&line, 1);
    let init: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(
        init["result"]["serverInfo"]["name"].as_str().is_some(),
        "initialize result carries serverInfo: {line:?}"
    );

    // 2. notifications/initialized — a notification: NO response line.
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
    );

    // 3. tools/list — exactly one response frame.
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    );
    let line = lines
        .next()
        .expect("tools/list response line")
        .expect("line reads");
    assert_line_is_jsonrpc(&line, 2);
    let tools: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        tools["result"]["tools"].as_array().map(Vec::len),
        Some(11),
        "the eleven declared tools are listed (seven read + w3's two write + w5's two walk-forward)"
    );

    // 4. One export — a real store read whose response must still be a single
    //    JSON-RPC frame (no progress chatter, no human-readable text).
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"export_candles","arguments":{"pair":"BTCUSDT","timeframe":"15m"}}}"#,
    );
    let line = lines
        .next()
        .expect("export_candles response line")
        .expect("line reads");
    assert_line_is_jsonrpc(&line, 3);
    let export: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(
        export["result"]["structuredContent"]["path"]
            .as_str()
            .is_some_and(|p| p.starts_with('/')),
        "the export reports an absolute path: {line:?}"
    );

    // 5. stdin close ends the session; the process exits 0 with nothing left
    //    on stdout.
    drop(stdin);
    let status = wait_with_timeout(&mut child, Duration::from_secs(30));
    assert!(
        status.success(),
        "pulse mcp exits 0 on stdin close, got {status}"
    );
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    assert!(
        rest.is_empty(),
        "no stray stdout frames after the responses: {rest:?}"
    );
}

/// `wait()` with a ceiling — a wedged server must fail the test, not hang it.
fn wait_with_timeout(
    child: &mut std::process::Child,
    budget: Duration,
) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "pulse mcp did not exit within {budget:?} of stdin close"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// AC-5 (r2.s1.w3): the identity path validates through the w1 `AgentName`
/// newtype, which counts CHARACTERS — `name.len()` on the old path counted
/// bytes, so a 33-`é` name (33 chars, 66 bytes) was refused on LENGTH. Through
/// `AgentName::parse` the same name is refused on the CHARACTER SET — the
/// stderr reason is what distinguishes the two.
#[test]
fn agent_identity_counts_characters_not_bytes() {
    let name = "é".repeat(33); // 33 chars, 66 bytes — over the byte limit, inside the char limit
    let output = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "mcp",
            "--agent-name",
            &name,
            "--db",
            "/nonexistent/never.db",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("spawn pulse mcp with a multibyte agent name");
    assert!(
        !output.status.success(),
        "an out-of-charset name still refuses to serve (status {})",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ASCII"),
        "the refusal names the character set, proving the count was in chars: {stderr:?}"
    );
    assert!(
        !stderr.contains("1-64 characters"),
        "a byte-counted validator would refuse on length instead: {stderr:?}"
    );
}

#[test]
fn agent_identity_too_long_name_exits_nonzero() {
    // 65 ASCII chars: inside the byte count either way — the char-counted
    // validator must still refuse, on length.
    let name = "x".repeat(65);
    let output = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "mcp",
            "--agent-name",
            &name,
            "--db",
            "/nonexistent/never.db",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("spawn pulse mcp with an over-long agent name");
    assert!(
        !output.status.success(),
        "an over-64-char name refuses to serve (status {})",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("1-64"),
        "the refusal names the length rule: {stderr:?}"
    );
}

#[test]
fn invalid_agent_name_exits_nonzero_with_reason_on_stderr() {
    // AC-10 + the stderr amendment: the exit code alone is vacuous (clap would
    // also exit non-zero on an unknown subcommand) — the stderr text is what
    // proves OUR validator ran before serving.
    let output = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "mcp",
            "--agent-name",
            "Bad Name!",
            "--db",
            "/nonexistent/never.db",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("spawn pulse mcp with bad agent name");
    assert!(
        !output.status.success(),
        "invalid --agent-name refuses to serve (status {})",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("agent-name") || stderr.contains("agent name"),
        "stderr names the validation reason, got: {stderr:?}"
    );
    assert!(
        !stderr.trim().is_empty(),
        "the reason is a non-empty stderr line"
    );
    assert!(
        output.stdout.is_empty(),
        "a rejected startup writes nothing to stdout"
    );
}
