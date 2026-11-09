//! r7.s1.w2 (PDR-0051 decision 3, FR-11 archive) — the archive routes and the
//! archived-aware Library read.
//!
//! - (i) an app token archives and unarchives a strategy, idempotently, and an
//!   unknown id is a 404 `not_found`;
//! - (ii) an agent token is a 403 `scope_refused` on all three routes (least
//!   privilege, ADR-0026);
//! - (iii) archiving deletes nothing;
//! - (iv) the default Library read hides an archived strategy, the
//!   `with-archived` read lists it, and `pulse mcp`'s `list_strategies` lists
//!   the same strategies as the app (PDR-0051 decision 4);
//! - (v) the request log names the archiving token's label, never the token;
//! - (vi) promoting a version of an archived strategy is a 409 `archived`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::collections::BTreeSet;
use std::time::Duration;

use pulse::StrategyRepository;
use serde_json::{Value, json};
use support::mcp::{call, spawn_client};
use support::paper::PaperHost;
use support::server::{ServerOptions, TestServer, seed_version, spawn_server};

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("build the test client")
}

fn auth(token: &str) -> String {
    format!("Bearer {token}")
}

async fn post_as(ts: &TestServer, token: &str, path: &str, body: &Value) -> reqwest::Response {
    client()
        .post(format!("{}{path}", ts.base))
        .header("Authorization", auth(token))
        .json(body)
        .send()
        .await
        .expect("the route answers")
}

async fn get_as(ts: &TestServer, token: &str, path: &str) -> reqwest::Response {
    client()
        .get(format!("{}{path}", ts.base))
        .header("Authorization", auth(token))
        .send()
        .await
        .expect("the read answers")
}

/// Seed one strategy with one version; return `(strategy id, version id)`.
async fn seed_strategy(ts: &TestServer) -> (String, String) {
    let desktop = ts.desktop().await;
    let version_id = seed_version(&desktop).await;
    let version = desktop
        .strategy_repo()
        .get_version(&version_id)
        .await
        .expect("read the version")
        .expect("the seeded version exists");
    (
        version.strategy_id.as_str().to_owned(),
        version_id.as_str().to_owned(),
    )
}

async fn archive(ts: &TestServer, strategy_id: &str) -> reqwest::Response {
    post_as(
        ts,
        &ts.app_token,
        &format!("/api/v1/strategies/{strategy_id}/archive"),
        &json!({}),
    )
    .await
}

async fn unarchive(ts: &TestServer, strategy_id: &str) -> reqwest::Response {
    post_as(
        ts,
        &ts.app_token,
        &format!("/api/v1/strategies/{strategy_id}/unarchive"),
        &json!({}),
    )
    .await
}

/// Whether the store says this strategy is archived.
async fn stored_archived(ts: &TestServer, strategy_id: &str) -> bool {
    ts.desktop()
        .await
        .strategy_repo()
        .get_strategy(&pulse::StrategyId::new(strategy_id.to_owned()))
        .await
        .expect("read the strategy")
        .expect("the strategy exists")
        .archived
}

/// The ids a Library read lists.
async fn library_ids(ts: &TestServer, path: &str) -> BTreeSet<String> {
    let response = get_as(ts, &ts.app_token, path).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK, "{path}");
    let body: Value = response.json().await.expect("a library body");
    body["strategies"]
        .as_array()
        .expect("a strategies array")
        .iter()
        .map(|s| s["id"].as_str().expect("a strategy id").to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_app_token_archives_and_unarchives_a_strategy() {
    let ts = spawn_server(ServerOptions::default()).await;
    let (sid, _vid) = seed_strategy(&ts).await;

    let response = archive(&ts, &sid).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("an archive body");
    assert_eq!(body, json!({"id": sid, "archived": true}));
    assert!(stored_archived(&ts, &sid).await, "the store reads archived");

    let response = unarchive(&ts, &sid).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("an unarchive body");
    assert_eq!(body, json!({"id": sid, "archived": false}));
    assert!(
        !stored_archived(&ts, &sid).await,
        "the store reads unarchived"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_and_unarchive_are_idempotent() {
    let ts = spawn_server(ServerOptions::default()).await;
    let (sid, _vid) = seed_strategy(&ts).await;

    for _ in 0..2 {
        let response = archive(&ts, &sid).await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.expect("an archive body");
        assert_eq!(body, json!({"id": sid, "archived": true}));
    }
    for _ in 0..2 {
        let response = unarchive(&ts, &sid).await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.expect("an unarchive body");
        assert_eq!(body, json!({"id": sid, "archived": false}));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_strategy_is_a_404_not_found() {
    let ts = spawn_server(ServerOptions::default()).await;

    for verb in ["archive", "unarchive"] {
        let response = post_as(
            &ts,
            &ts.app_token,
            &format!("/api/v1/strategies/no-such-strategy/{verb}"),
            &json!({}),
        )
        .await;
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND, "{verb}");
        let body: Value = response.json().await.expect("a json error body");
        assert_eq!(body["code"], json!("not_found"), "{verb}: {body:?}");
        assert_eq!(
            body["message"],
            json!("no strategy with id no-such-strategy"),
            "{verb}: {body:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_token_is_refused_on_both_archive_routes_and_the_with_archived_read() {
    let ts = spawn_server(ServerOptions::default()).await;
    let (sid, _vid) = seed_strategy(&ts).await;

    let archive_path = format!("/api/v1/strategies/{sid}/archive");
    let unarchive_path = format!("/api/v1/strategies/{sid}/unarchive");
    for path in [&archive_path, &unarchive_path] {
        let response = post_as(&ts, &ts.agent_token, path, &json!({})).await;
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN, "{path}");
        let body: Value = response.json().await.expect("a json refusal body");
        assert_eq!(body["code"], json!("scope_refused"), "{path}: {body:?}");
    }
    let response = get_as(
        &ts,
        &ts.agent_token,
        "/api/v1/library-overview/with-archived",
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    let body: Value = response.json().await.expect("a json refusal body");
    assert_eq!(body["code"], json!("scope_refused"), "{body:?}");

    assert!(
        !stored_archived(&ts, &sid).await,
        "a refused call changes nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archiving_deletes_nothing() {
    let ts = spawn_server(ServerOptions::default()).await;
    let (sid, vid) = seed_strategy(&ts).await;

    assert_eq!(archive(&ts, &sid).await.status(), reqwest::StatusCode::OK);

    let desktop = ts.desktop().await;
    let repo = desktop.strategy_repo();
    let strategy_id = pulse::StrategyId::new(sid.clone());
    assert!(
        repo.get_strategy(&strategy_id)
            .await
            .expect("read the strategy")
            .is_some(),
        "the strategy row stays"
    );
    let tree = repo.version_tree(&strategy_id).await.expect("the tree");
    assert!(
        tree.iter().any(|v| v.id.as_str() == vid),
        "the version stays in the tree"
    );
    assert!(
        repo.get_version(&pulse::VersionId::new(vid))
            .await
            .expect("read the version")
            .is_some(),
        "the version still reads by id"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_library_overview_hides_an_archived_strategy_unless_with_archived() {
    let ts = spawn_server(ServerOptions::default()).await;
    let (kept, _) = seed_strategy(&ts).await;
    let (shelved, _) = seed_strategy(&ts).await;
    assert_eq!(
        archive(&ts, &shelved).await.status(),
        reqwest::StatusCode::OK
    );

    let response = get_as(&ts, &ts.app_token, "/api/v1/library-overview").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("a library body");
    let listed = body["strategies"].as_array().expect("strategies");
    assert_eq!(listed.len(), 1, "only the unarchived strategy: {body:?}");
    assert_eq!(listed[0]["id"], json!(kept));
    assert_eq!(listed[0]["archived"], json!(false));

    let response = get_as(&ts, &ts.app_token, "/api/v1/library-overview/with-archived").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("a library body");
    let listed = body["strategies"].as_array().expect("strategies");
    assert_eq!(listed.len(), 2, "both strategies: {body:?}");
    let archived_row = listed
        .iter()
        .find(|s| s["id"] == json!(shelved))
        .expect("the archived strategy is listed");
    assert_eq!(archived_row["archived"], json!(true));
    let kept_row = listed
        .iter()
        .find(|s| s["id"] == json!(kept))
        .expect("the kept strategy is listed");
    assert_eq!(kept_row["archived"], json!(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_app_library_and_mcp_list_the_same_strategies_after_an_archive() {
    let ts = spawn_server(ServerOptions::default()).await;
    let (_a, _) = seed_strategy(&ts).await;
    let (b, _) = seed_strategy(&ts).await;
    let (_c, _) = seed_strategy(&ts).await;
    assert_eq!(archive(&ts, &b).await.status(), reqwest::StatusCode::OK);

    let client = spawn_client(&ts.db_path, &ts.data_dir).await;
    let mcp_ids = |value: &Value| -> BTreeSet<String> {
        value["strategies"]
            .as_array()
            .expect("a strategies array")
            .iter()
            .map(|s| s["id"].as_str().expect("a strategy id").to_owned())
            .collect()
    };

    let default_mcp = call(&client, "list_strategies", json!({})).await;
    let default_app = library_ids(&ts, "/api/v1/library-overview").await;
    assert_eq!(mcp_ids(&default_mcp), default_app);
    assert_eq!(default_app.len(), 2);
    assert!(!default_app.contains(&b));

    let all_mcp = call(
        &client,
        "list_strategies",
        json!({"include_archived": true}),
    )
    .await;
    let all_app = library_ids(&ts, "/api/v1/library-overview/with-archived").await;
    assert_eq!(mcp_ids(&all_mcp), all_app);
    assert_eq!(all_app.len(), 3);
    assert!(all_app.contains(&b));

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_request_log_names_the_archiving_token_and_never_the_token() {
    let ts = spawn_server(ServerOptions::default()).await;
    let (sid, _vid) = seed_strategy(&ts).await;
    assert_eq!(archive(&ts, &sid).await.status(), reqwest::StatusCode::OK);

    let lines = ts.log.lines();
    assert!(
        lines.iter().any(|line| {
            line.starts_with("pulse serve: POST /api/v1/strategies/")
                && line.contains("/archive 200 w2-app ")
        }),
        "the archive call is logged with the token's label: {lines:?}"
    );
    for line in &lines {
        assert!(
            !line.contains(&ts.app_token),
            "the token string leaked into the log: {line}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promoting_a_version_of_an_archived_strategy_is_refused() {
    let ts = spawn_server(ServerOptions {
        paper: Some(Box::new(PaperHost::spawn)),
        ..Default::default()
    })
    .await;
    let (sid, vid) = seed_strategy(&ts).await;
    assert_eq!(archive(&ts, &sid).await.status(), reqwest::StatusCode::OK);

    let sessions_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM paper_session")
        .fetch_one(ts.db.pool())
        .await
        .expect("count sessions");

    let response = post_as(
        &ts,
        &ts.app_token,
        "/api/v1/paper/promote",
        &json!({"version_id": vid}),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    let body: Value = response.json().await.expect("a json refusal body");
    assert_eq!(body["code"], json!("archived"), "{body:?}");
    assert_eq!(
        body["message"],
        json!("strategy w2 demo is archived; unarchive it to promote"),
        "{body:?}"
    );

    let sessions_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM paper_session")
        .fetch_one(ts.db.pool())
        .await
        .expect("count sessions");
    assert_eq!(
        sessions_after, sessions_before,
        "no session row was written"
    );
}
