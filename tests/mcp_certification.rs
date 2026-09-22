//! r2.s3.w4 — AC-4: the MCP read surfaces carry certification.
//!
//! `list_strategies`'s version rows and `get_version`'s detail both expose
//! `certified` and `latest_walk_forward_run_id` (a12) — projected from the
//! joined run, never invented: a version with no pointer reports
//! `latest_walk_forward_run_id: null` (the key is PRESENT, not omitted) and
//! `certified: false`.
//!
//! The wire is read back over a real `pulse mcp` stdio session — the same
//! harness `mcp_stdio.rs` uses — with a synthetic passing walk-forward run
//! persisted on the child before the server spawns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{FakeClock, SqliteBacktestRunRepo, WalkForwardRunRepository};
use serde_json::{Value, json};
use support::mcp::{
    FIXTURE_STORE, call, copy_tree, manifest, migrated_db, seed_versions,
    seeded_walk_forward_draft, spawn_client,
};
use tempfile::TempDir;

/// Seed parent→child, certify the CHILD with one synthetic passing
/// walk-forward, and spawn the server over that db + a copied fixture store.
#[allow(clippy::type_complexity)]
async fn certified_fixture() -> (
    TempDir,
    TempDir,
    pulse::VersionId,
    pulse::VersionId,
    String,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
) {
    let tmp_db = TempDir::new().unwrap();
    let (db_path, db) = migrated_db(&tmp_db).await;
    let (parent, child) = seed_versions(&db).await;

    let run_id =
        SqliteBacktestRunRepo::with_deps(db.pool().clone(), FakeClock::at(1_756_512_000_000))
            .save_walk_forward_run(&child, &seeded_walk_forward_draft(true))
            .await
            .expect("the certifying walk-forward run persists");

    let tmp_store = TempDir::new().unwrap();
    let store_dir = tmp_store.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);
    let client = spawn_client(&db_path, &store_dir).await;
    (
        tmp_db,
        tmp_store,
        parent,
        child,
        run_id.as_str().to_owned(),
        client,
    )
}

/// `list_strategies`: every version row carries both keys — `null`/`false`
/// for the uncertified parent, the run id/`true` for the certified child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_strategies_carries_certification_on_every_version_row() {
    let (_db, _store, parent, child, run_id, client) = certified_fixture().await;

    let strategies = call(&client, "list_strategies", json!({})).await;
    let versions = strategies["strategies"][0]["versions"]
        .as_array()
        .expect("versions array");
    assert_eq!(versions.len(), 2, "parent + child");

    let parent_row = &versions[0];
    assert_eq!(parent_row["id"], parent.as_str());
    assert!(
        parent_row.get("latest_walk_forward_run_id").is_some(),
        "the pointer key is PRESENT, not omitted: {parent_row}"
    );
    assert_eq!(
        parent_row["latest_walk_forward_run_id"],
        Value::Null,
        "an absent pointer serializes as null"
    );
    assert_eq!(parent_row["certified"], false, "the parent is uncertified");

    let child_row = &versions[1];
    assert_eq!(child_row["id"], child.as_str());
    assert_eq!(
        child_row["latest_walk_forward_run_id"], run_id,
        "the child's pointer names its certifying run"
    );
    assert_eq!(child_row["certified"], true, "the child is certified");

    client.cancel().await.expect("cancel session");
}

/// `get_version`: the same two fields on the detail shape — certified child,
/// uncertified parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_version_carries_certification_on_the_detail_shape() {
    let (_db, _store, parent, child, run_id, client) = certified_fixture().await;

    let detail = call(
        &client,
        "get_version",
        json!({"version_id": child.as_str()}),
    )
    .await;
    assert_eq!(detail["certified"], true);
    assert_eq!(detail["latest_walk_forward_run_id"], run_id);

    let detail = call(
        &client,
        "get_version",
        json!({"version_id": parent.as_str()}),
    )
    .await;
    assert!(
        detail.get("latest_walk_forward_run_id").is_some(),
        "the pointer key is PRESENT, not omitted: {detail}"
    );
    assert_eq!(detail["latest_walk_forward_run_id"], Value::Null);
    assert_eq!(detail["certified"], false);

    client.cancel().await.expect("cancel session");
}
