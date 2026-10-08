//! r4.s1.w5 — AC-2: the `certify_version` MCP tool.
//!
//! The agent's whole view of a certification. It drives the real tool over a
//! `pulse mcp` session against the shared synthetic world (`support::
//! certification`), and asserts the two halves of grill Q5:
//!
//! - the **answer** carries the certification's id, its pass/fail, the
//!   search-span verdict, whether the holdout passed, and the hypotheses used
//!   and left — and NOT ONE holdout number (asserted by walking the whole
//!   payload for forbidden keys: `n`, mean, `z`, bound, end, trades);
//! - the **call** is attributable: the record the tool writes names the
//!   session's own label (never a value the tool's arguments could supply), and
//!   a refused token writes no record at all.
//!
//! The three typed refusals (no open freeze, a pre-freeze lineage root, a spent
//! budget) are asserted by name, and the 13th call is refused like the step's
//! own suite refuses it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::Path;

use pulse::{CertificationRepository, FakeClock, OpenFreezeRequest, SqliteCertificationFreezeRepo};
use serde_json::{Value, json};
use support::certification::{
    FREEZE_OPENED_MS, H, certifications, seed_search_run, seeded_draft, walk_forward_run_count,
    world, world_with_clocks,
};
use support::mcp::{call, call_err, spawn_client};
use support::server::{ServerOptions, spawn_server};

/// The keys an `agent` token must never see on any certification answer
/// (grill Q5: no holdout n, mean, bound, z, end or trade list).
const HOLDOUT_KEYS: [&str; 7] = [
    "holdout_n",
    "holdout_mean_r",
    "holdout_z",
    "holdout_lower_bound",
    "holdout_start",
    "holdout_end",
    "trades",
];

/// Every key in `value`, anywhere in the tree, that names a holdout number.
fn holdout_keys(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                if HOLDOUT_KEYS.contains(&key.as_str()) {
                    found.push(key.clone());
                }
                holdout_keys(nested, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                holdout_keys(item, found);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// The initialize frame the HTTP refusal probe sends.
fn initialize_frame() -> String {
    json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "pulse-mcp-certify-test", "version": "0.0.0" }
        }
    })
    .to_string()
}

/// POST one frame to the server's `/mcp` with `token`, answering the status.
async fn post_mcp(base: &str, token: &str) -> reqwest::StatusCode {
    reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .bearer_auth(token)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(initialize_frame())
        .send()
        .await
        .expect("POST /mcp")
        .status()
}

/// Revoke a token through the CLI (`pulse token revoke`) — the same verb an
/// operator runs.
fn revoke_token(label: &str, db_path: &Path) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args(["token", "revoke", "--label", label, "--db"])
        .arg(db_path)
        .output()
        .expect("spawn pulse token revoke");
    assert!(
        out.status.success(),
        "revoke must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The happy path: a planted-edge version certifies, and the answer carries the
/// six facts of grill Q5 — no holdout number anywhere in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certify_version_answers_without_any_holdout_number() {
    let world = world().await;
    let client = spawn_client(&world.db_path, &world.data_dir()).await;
    let result = call(
        &client,
        "certify_version",
        json!({ "version_id": world.version.as_str() }),
    )
    .await;

    assert_eq!(result["certified"], true, "the planted edge certifies");
    assert_eq!(result["holdout_passed"], true);
    assert_eq!(result["hypotheses_used"], 1, "one call is one hypothesis");
    assert_eq!(result["hypotheses_left"], u32::from(H) - 1);
    assert!(
        result["certification_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "the answer names the record it wrote: {result}"
    );
    assert_eq!(result["search"]["pass"], true, "the search-span verdict");
    assert!(
        result["search"]["folds_holding"].is_number()
            && result["search"]["folds_required"].is_number()
            && result["search"]["pooled_lower_bound"].is_number(),
        "the search-span tallies and pooled bound are the agent's own run feedback: {result}"
    );

    // Exactly the six keys (four under `search`), and not one holdout number —
    // asserted by walking the WHOLE payload, so a future nested addition cannot
    // smuggle one in.
    assert_eq!(
        result.as_object().unwrap().len(),
        6,
        "the answer is Q5's four facts plus the id and the budget pair: {result}"
    );
    assert_eq!(result["search"].as_object().unwrap().len(), 4);
    let mut offenders = Vec::new();
    holdout_keys(&result, &mut offenders);
    assert!(
        offenders.is_empty(),
        "the agent answer carries holdout numbers {offenders:?}: {result}"
    );

    // And the record exists, counts, and is the one the answer named.
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        1
    );
    let stored = certifications(&world)
        .list_for_version(&world.version)
        .await
        .unwrap();
    assert_eq!(stored[0].id, result["certification_id"].as_str().unwrap());
}

/// With no open freeze the tool refuses by name, naming the missing freeze, and
/// writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certify_version_refuses_without_an_open_freeze() {
    let world = world().await;
    // Close the open freeze: F1's operator verb, and the state a campaign that
    // ended cannot certify in.
    SqliteCertificationFreezeRepo::with_deps(
        world.db.pool().clone(),
        FakeClock::at(FREEZE_OPENED_MS + 86_400_000),
    )
    .close()
    .await
    .expect("the freeze closes");

    let client = spawn_client(&world.db_path, &world.data_dir()).await;
    let error = call_err(
        &client,
        "certify_version",
        json!({ "version_id": world.version.as_str() }),
    )
    .await;
    assert_eq!(
        error["field"], "freeze",
        "the refusal is field-pathed: {error}"
    );
    let message = error["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("no certification freeze is open"),
        "the refusal names the reason: {message}"
    );
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        0,
        "a refusal writes nothing"
    );
}

/// The 13th call under H = 12 is refused by name, and it writes nothing and
/// runs nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certify_version_refuses_the_thirteenth_attempt() {
    let world = world().await;
    let run_id = seed_search_run(&world).await;
    for _ in 1..=u32::from(H) {
        certifications(&world)
            .insert(&seeded_draft(&world, &run_id))
            .await
            .expect("the seeded hypothesis persists");
    }
    let runs_before = walk_forward_run_count(&world).await;

    let client = spawn_client(&world.db_path, &world.data_dir()).await;
    let error = call_err(
        &client,
        "certify_version",
        json!({ "version_id": world.version.as_str() }),
    )
    .await;
    assert_eq!(
        error["field"], "hypotheses",
        "the refusal is field-pathed: {error}"
    );
    let message = error["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("hypothesis budget is spent") && message.contains("12"),
        "the refusal names the spent budget and H: {message}"
    );
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        u32::from(H),
        "the refused call wrote no record"
    );
    assert_eq!(
        walk_forward_run_count(&world).await,
        runs_before,
        "the refused call ran nothing"
    );
}

/// A version whose lineage root predates the freeze is refused by name, naming
/// the root — nothing written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certify_version_refuses_a_pre_freeze_lineage_root() {
    let world = world_with_clocks(
        pulse::fixture_strategy_dsl(),
        FREEZE_OPENED_MS - 86_400_000,
        FREEZE_OPENED_MS,
    )
    .await;
    let client = spawn_client(&world.db_path, &world.data_dir()).await;
    let error = call_err(
        &client,
        "certify_version",
        json!({ "version_id": world.version.as_str() }),
    )
    .await;
    assert_eq!(
        error["field"], "version_id",
        "the refusal is field-pathed: {error}"
    );
    let message = error["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(world.version.as_str())
            && message.contains("lineage root")
            && message.contains("before the freeze opened"),
        "the refusal names the root and the freeze: {message}"
    );
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        0,
        "a refusal writes nothing"
    );
}

/// The record the tool writes names the SESSION's label — the authenticated
/// client's, never a value the tool's arguments could supply (grill Q5: the
/// call is attributable).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certify_version_records_the_calling_label() {
    let world = world().await;
    let client = spawn_client(&world.db_path, &world.data_dir()).await;
    let _ = call(
        &client,
        "certify_version",
        json!({ "version_id": world.version.as_str() }),
    )
    .await;

    let stored = certifications(&world)
        .list_for_version(&world.version)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].called_by, "test-agent",
        "the record carries the session's own (lowercased) label — the harness's \
         `--agent-name Test-Agent`, which no argument of this call supplied"
    );
}

/// A revoked or unknown token never reaches the tool: the call is refused by
/// the auth layer (401), it lands a `refused` row in the token audit, and it
/// writes no certification record at all — the state a successful call would
/// have changed is unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certify_version_with_a_refused_token_writes_no_record() {
    let server = spawn_server(ServerOptions::default()).await;
    // A version and an open freeze, so a call that got through WOULD have been
    // able to write a record: the assertion below is not vacuous.
    let state = server.desktop().await;
    let _version = support::server::seed_version(&state).await;
    SqliteCertificationFreezeRepo::with_deps(
        server.db.pool().clone(),
        FakeClock::at(FREEZE_OPENED_MS),
    )
    .open(&OpenFreezeRequest {
        holdout_start_ms: 1_750_000_000_000,
        h: H,
        alpha: "0.05".to_owned(),
        holdout_test: "one-sided lower confidence bound at 1 - 0.05/H (C1)".to_owned(),
    })
    .await
    .expect("the server's freeze opens");

    let issued = support::server::issue_token("w5-revoked", "agent", &server.db_path);
    revoke_token("w5-revoked", &server.db_path);

    for token in [issued.as_str(), "pt_this-token-was-never-issued"] {
        assert_eq!(
            post_mcp(&server.base, token).await,
            reqwest::StatusCode::UNAUTHORIZED,
            "a refused token never reaches the tool"
        );
    }

    let refusals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM token_audit WHERE event = 'refused' AND reason IN ('revoked', 'unknown')",
    )
    .fetch_one(server.db.pool())
    .await
    .unwrap();
    assert_eq!(refusals, 2, "each refused call lands one audit row");

    let records: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM certification")
        .fetch_one(server.db.pool())
        .await
        .unwrap();
    assert_eq!(records, 0, "a refused call writes no certification record");
}
