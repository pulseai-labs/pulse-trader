//! #353 (r4.s6.w2): the `pulse://dsl/schema` wire shape Claude Code validates.
//!
//! Claude Code 2.1.294+ reaches the `pulse mcp` server on the 2026-07-28
//! protocol era and validates `resources/read` / `resources/list` results
//! against that revision's result schema, where SEP-2549 makes `ttlMs` (an
//! integer >= 0) and `cacheScope` (`public` | `private`) REQUIRED on a
//! cacheable result. rmcp 3.4.0 models both as `Option` and leaves them unset,
//! so the read failed client-side with
//! `Invalid result for resources/read: [{path: ["ttlMs"], …}, {path:
//! ["cacheScope"], …}]` (#353) while tool calls kept working — the tool result
//! schema carries no cache hints.
//!
//! Two halves, on purpose:
//!
//! 1. `the_served_resource_carries_the_cache_hints_claude_code_requires` — the
//!    `tests/mcp_stdio.rs` harness (rmcp's client over the real child process),
//!    asserting the CLIENT-visible result: `ttl_ms`, `cache_scope`, the text
//!    entry's MIME type, and the DSL schema version.
//! 2. `the_raw_read_and_list_results_carry_only_allowed_keys` — the same child
//!    over piped stdio, driven with hand-built JSON-RPC frames, so EVERY key
//!    on the wire is asserted; the typed client cannot see an unknown key, and
//!    an unexpected one is exactly what a strict client validator rejects.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::process::Stdio;
use std::time::Duration;

use pulse::DSL_SCHEMA_VERSION;
use rmcp::model::{CacheScope, ReadResourceRequestParams, ResourceContents};
use serde_json::{Map, Value, json};
use support::mcp::{FIXTURE_STORE, copy_tree, manifest, migrated_db, spawn_client};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// The keys the 2026-07-28 result schema admits on a `resources/read` result.
const READ_RESULT_KEYS: &[&str] = &["resultType", "ttlMs", "cacheScope", "contents", "_meta"];
/// The keys the 2026-07-28 result schema admits on a `resources/list` result.
const LIST_RESULT_KEYS: &[&str] = &[
    "resultType",
    "ttlMs",
    "cacheScope",
    "resources",
    "nextCursor",
    "_meta",
];
/// The keys the 2026-07-28 schema admits on one `contents[]` entry.
const CONTENT_KEYS: &[&str] = &["uri", "mimeType", "text", "blob", "_meta"];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_served_resource_carries_the_cache_hints_claude_code_requires() {
    let tmp_db = TempDir::new().unwrap();
    let (db_path, _db) = migrated_db(&tmp_db).await;
    let tmp_store = TempDir::new().unwrap();
    let store_dir = tmp_store.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);
    let client = spawn_client(&db_path, &store_dir).await;

    let read = client
        .read_resource(ReadResourceRequestParams::new("pulse://dsl/schema"))
        .await
        .expect("resources/read pulse://dsl/schema");
    assert_eq!(
        read.ttl_ms,
        Some(0),
        "#353: the read result must carry ttlMs — the 2026-07-28 schema requires it"
    );
    assert_eq!(
        read.cache_scope,
        Some(CacheScope::Private),
        "#353: the read result must carry cacheScope — the 2026-07-28 schema requires it"
    );
    assert_eq!(read.contents.len(), 1);
    let text = match &read.contents[0] {
        ResourceContents::TextResourceContents {
            text, mime_type, ..
        } => {
            assert_eq!(mime_type.as_deref(), Some("application/json"));
            text.clone()
        }
        other => panic!("dsl_schema must be text contents, got {other:?}"),
    };
    let doc: Value = serde_json::from_str(&text).expect("dsl_schema body parses as JSON");
    assert_eq!(doc["schema_version"], DSL_SCHEMA_VERSION);

    let list = client.list_resources(None).await.expect("resources/list");
    assert_eq!(
        list.ttl_ms,
        Some(0),
        "#353: the list result must carry ttlMs — the 2026-07-28 schema requires it"
    );
    assert_eq!(
        list.cache_scope,
        Some(CacheScope::Private),
        "#353: the list result must carry cacheScope — the 2026-07-28 schema requires it"
    );
    assert_eq!(list.resources.len(), 1);
    assert_eq!(list.resources[0].uri, "pulse://dsl/schema");

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_raw_read_and_list_results_carry_only_allowed_keys() {
    let tmp_db = TempDir::new().unwrap();
    let (db_path, _db) = migrated_db(&tmp_db).await;
    let tmp_store = TempDir::new().unwrap();
    let store_dir = tmp_store.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);

    let mut child = spawn_raw_child(&db_path, &store_dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));

    // Claude Code asks for the modern revision; the server's `initialize`
    // handshake answers with the newest revision it can grant. The capture
    // records what it actually agreed to.
    send_line(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1001,
            "method": "initialize",
            "params": {
                "protocolVersion": "2026-07-28",
                "capabilities": {},
                "clientInfo": {"name": "mcp-dsl-schema-resource", "version": "0.1.0"}
            }
        }),
    )
    .await;
    let init = read_response(&mut stdout, 1001).await;
    let negotiated = init["result"]["protocolVersion"]
        .as_str()
        .expect("initialize result carries protocolVersion");
    assert!(
        !negotiated.is_empty(),
        "the negotiated protocolVersion must be a non-empty string"
    );

    send_line(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;

    send_line(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1002,
            "method": "resources/read",
            "params": {"uri": "pulse://dsl/schema"}
        }),
    )
    .await;
    let read = read_response(&mut stdout, 1002).await;
    let read_result = read["result"]
        .as_object()
        .unwrap_or_else(|| panic!("resources/read must answer a result object, got {read}"));
    assert_keys_subset(read_result, READ_RESULT_KEYS, "resources/read result");
    assert_cache_hints(read_result, "resources/read result");
    let contents = read_result["contents"]
        .as_array()
        .filter(|contents| contents.len() == 1)
        .unwrap_or_else(|| panic!("resources/read must carry exactly one contents entry: {read}"));
    let entry = contents[0]
        .as_object()
        .expect("the contents entry is an object");
    assert_keys_subset(entry, CONTENT_KEYS, "resources/read contents[0]");
    assert_eq!(entry["uri"], "pulse://dsl/schema");
    assert_eq!(entry["mimeType"], "application/json");
    let text = entry["text"]
        .as_str()
        .expect("the contents entry carries the schema text");
    let doc: Value = serde_json::from_str(text).expect("dsl_schema body parses as JSON");
    assert_eq!(doc["schema_version"], DSL_SCHEMA_VERSION);

    send_line(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 1003, "method": "resources/list"}),
    )
    .await;
    let list = read_response(&mut stdout, 1003).await;
    let list_result = list["result"]
        .as_object()
        .unwrap_or_else(|| panic!("resources/list must answer a result object, got {list}"));
    assert_keys_subset(list_result, LIST_RESULT_KEYS, "resources/list result");
    assert_cache_hints(list_result, "resources/list result");
    let resources = list_result["resources"]
        .as_array()
        .filter(|resources| resources.len() == 1)
        .unwrap_or_else(|| panic!("resources/list must carry exactly one resource: {list}"));
    assert_eq!(resources[0]["uri"], "pulse://dsl/schema");

    drop(stdin);
    let _ = child.kill().await;
}

/// The child `tests/mcp_stdio.rs` spawns, with piped stdio so the raw
/// JSON-RPC frames are readable.
fn spawn_raw_child(db_path: &std::path::Path, store_dir: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_pulse"))
        .arg("mcp")
        .arg("--local")
        .arg("--db")
        .arg(db_path)
        .arg("--data-dir")
        .arg(store_dir)
        .arg("--agent-name")
        .arg("Raw-Probe")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn pulse mcp")
}

async fn send_line(stdin: &mut ChildStdin, frame: &Value) {
    let mut line = serde_json::to_string(frame).expect("frame serializes");
    line.push('\n');
    stdin.write_all(line.as_bytes()).await.expect("write frame");
    stdin.flush().await.expect("flush frame");
}

/// Read stdout lines until the JSON-RPC response with `id` arrives —
/// notifications the server sends on its own are skipped, never mistaken for
/// the answer — guarded by a timeout so a dead server fails the test instead
/// of hanging the suite.
async fn read_response(stdout: &mut BufReader<ChildStdout>, id: u64) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mut line = String::new();
            let n = stdout.read_line(&mut line).await.expect("read stdout line");
            assert_ne!(n, 0, "the child closed stdout before response {id}");
            let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if frame.get("id").and_then(Value::as_u64) == Some(id) {
                return frame;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for response {id}"))
}

fn assert_keys_subset(result: &Map<String, Value>, allowed: &[&str], what: &str) {
    for key in result.keys() {
        assert!(
            allowed.contains(&key.as_str()),
            "{what}: unexpected key {key:?} is outside the allowed set {allowed:?}"
        );
    }
}

/// The two fields #353's client error names: both present, `ttlMs` a
/// non-negative integer and `cacheScope` one of the schema's two options.
fn assert_cache_hints(result: &Map<String, Value>, what: &str) {
    let ttl = result
        .get("ttlMs")
        .unwrap_or_else(|| panic!("{what}: ttlMs missing — SEP-2549 requires it"));
    assert!(
        ttl.as_u64().is_some(),
        "{what}: ttlMs must be an integer >= 0, got {ttl}"
    );
    let scope = result
        .get("cacheScope")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{what}: cacheScope missing — SEP-2549 requires it"));
    assert!(
        matches!(scope, "public" | "private"),
        "{what}: cacheScope must be \"public\" or \"private\", got {scope:?}"
    );
}
