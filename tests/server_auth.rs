//! r3.s3.w1 AC-1 — the server auth integration proof (`tests/server_auth.rs`).
//!
//! Spins the real router in-process on `127.0.0.1:0` over a migrated temp DB and
//! drives it with `reqwest`, covering the eight required groups:
//! (i) the refusal table; (ii) scope isolation; (iii) the `pulse token` CLI
//! contract via the spawned binary; (iv) the no-leak scan; (v) the
//! `X-Pulse-Api-Version` header on every response; (vi) the handshake body;
//! (vii) the append-only/once-only triggers; (viii) the request-log line.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::routing::get;
use pulse::{
    API_VERSION, CaptureLog, Db, EngineFingerprint, Scope, ServerState, mount_scoped,
    open_migrated, router,
};
use reqwest::StatusCode;
use sha2::{Digest, Sha256};
use sqlx::Row;
use tempfile::TempDir;

/// A GET handler the probe routes mount — the smallest authenticated 200.
async fn ok_handler() -> &'static str {
    "ok"
}

/// One in-process server: migrated temp DB, captured log sink, an `app`-scoped
/// and an `agent`-scoped probe route, and the bound local address.
struct TestServer {
    base: String,
    db_path: PathBuf,
    log: Arc<CaptureLog>,
    db: Db,
    _dir: TempDir,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Spawn the server: migrate-then-open a temp DB, build the router with probe
/// routes under both scopes, and serve it on `127.0.0.1:0` with connect info.
async fn spawn_server() -> TestServer {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("pulse.db");
    let db = open_migrated(&db_path)
        .await
        .expect("migrate-then-open the temp db");
    let log = Arc::new(CaptureLog::default());
    let data_dir = dir.path().join("data");
    let state = Arc::new(ServerState::with_log(db.clone(), data_dir, log.clone()));
    let app = mount_scoped(
        mount_scoped(
            router(state.clone()),
            &state,
            Scope::App,
            "/probe/app",
            get(ok_handler),
        ),
        &state,
        Scope::Agent,
        "/probe/agent",
        get(ok_handler),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe listener");
    // `from_std` hands the fd to tokio, which requires non-blocking mode.
    listener
        .set_nonblocking(true)
        .expect("set probe listener non-blocking");
    let addr: SocketAddr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        axum::serve(
            tokio::net::TcpListener::from_std(listener).expect("tokio listener"),
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("axum serve");
    });
    TestServer {
        base: format!("http://{addr}"),
        db_path,
        log,
        db,
        _dir: dir,
        handle,
    }
}

/// Assert the token matches `^pt_[A-Za-z0-9_-]{43}$` without a regex dep.
fn assert_token_shape(token: &str) {
    assert!(
        token.starts_with("pt_"),
        "token must carry the pt_ prefix: {token}"
    );
    let body = &token["pt_".len()..];
    assert_eq!(
        body.len(),
        43,
        "token body must be 43 base64url chars: {body}"
    );
    assert!(
        body.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "token body must be unpadded base64url: {body}"
    );
}

/// Spawn `pulse token issue` against the server's DB and return the token.
fn issue_token(label: &str, scope: &str, db_path: &Path) -> String {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args(["token", "issue", "--scope", scope, "--label", label, "--db"])
        .arg(db_path)
        .output()
        .expect("spawn pulse token issue");
    assert!(
        out.status.success(),
        "issue must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "issue prints exactly one stdout line: {stdout:?}"
    );
    assert_token_shape(lines[0]);
    lines[0].to_owned()
}

/// Spawn `pulse token revoke` against the server's DB.
fn revoke_token(label: &str, db_path: &Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args(["token", "revoke", "--label", label, "--db"])
        .arg(db_path)
        .output()
        .expect("spawn pulse token revoke")
}

/// Count audit rows for one (event, reason) pair.
async fn audit_count(db: &Db, event: &str, reason: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM token_audit WHERE event = ? AND reason = ?")
        .bind(event)
        .bind(reason)
        .fetch_one(db.pool())
        .await
        .expect("audit count query")
}

/// Every TEXT column value of both tables, for the no-leak scan.
async fn all_text_cells(db: &Db) -> Vec<String> {
    let mut cells = Vec::new();
    for table in ["client_token", "token_audit"] {
        let rows = sqlx::query(&format!("SELECT * FROM {table}"))
            .fetch_all(db.pool())
            .await
            .unwrap_or_else(|e| panic!("scan {table}: {e}"));
        for row in rows {
            for (idx, _col) in row.columns().iter().enumerate() {
                if let Ok(Some(value)) = row.try_get::<Option<String>, _>(idx) {
                    cells.push(value);
                }
            }
        }
    }
    cells
}

/// True when the string contains a run of 64 hex characters (a stored hash).
fn contains_64_hex(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut run = 0;
    for &b in bytes {
        let is_hex = b.is_ascii_hexdigit();
        run = if is_hex { run + 1 } else { 0 };
        if run >= 64 {
            return true;
        }
    }
    false
}

/// A well-formed but unknown bearer token (never issued).
fn fake_unknown_token() -> String {
    format!("pt_{}", "A".repeat(43))
}

/// Read a response body as JSON (reqwest's `json` feature is not enabled, so
/// the body crosses as text and parses through `serde_json` here).
async fn json_body(resp: reqwest::Response) -> serde_json::Value {
    let text = resp.text().await.expect("body text");
    serde_json::from_str(&text).expect("json body")
}

// ---------------------------------------------------------------------------
// (i) The refusal table.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals_match_the_table_and_audit_once_each() {
    let server = spawn_server().await;
    let client = reqwest::Client::new();

    // Missing header.
    let resp = client
        .get(format!("{}/probe/app", server.base))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body = json_body(resp).await;
    assert_eq!(body["code"], "token_missing");
    assert!(
        audit_count(&server.db, "refused", "missing").await == 1,
        "one missing row"
    );

    // Non-Bearer scheme.
    let resp = client
        .get(format!("{}/probe/app", server.base))
        .header("Authorization", "Basic dXNlcjpwYXNz")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body = json_body(resp).await;
    assert_eq!(body["code"], "token_missing");
    assert!(
        audit_count(&server.db, "refused", "missing").await == 2,
        "second missing row"
    );

    // Unknown token.
    let resp = client
        .get(format!("{}/probe/app", server.base))
        .header("Authorization", format!("Bearer {}", fake_unknown_token()))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body = json_body(resp).await;
    assert_eq!(body["code"], "token_refused");
    assert!(
        audit_count(&server.db, "refused", "unknown").await == 1,
        "one unknown row"
    );

    // Revoked token.
    let token = issue_token("revoked-me", "app", &server.db_path);
    let out = revoke_token("revoked-me", &server.db_path);
    assert!(out.status.success(), "revoke must succeed");
    let resp = client
        .get(format!("{}/probe/app", server.base))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body = json_body(resp).await;
    assert_eq!(body["code"], "token_refused");
    assert!(
        audit_count(&server.db, "refused", "revoked").await == 1,
        "one revoked row"
    );

    // The refusal audit row names the route (method + path) and the peer, and
    // never a query string or a header value.
    let (missing_route, peer_is_set): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT route, peer FROM token_audit WHERE reason = 'missing' ORDER BY at DESC LIMIT 1",
    )
    .fetch_one(server.db.pool())
    .await
    .expect("missing row read");
    assert_eq!(missing_route.as_deref(), Some("GET /probe/app"));
    assert!(peer_is_set.is_some(), "the peer ip is recorded");
}

// ---------------------------------------------------------------------------
// (ii) Scope isolation.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_mismatch_is_403_and_own_scope_and_handshake_are_200() {
    let server = spawn_server().await;
    let client = reqwest::Client::new();
    let app_token = issue_token("scope-app", "app", &server.db_path);
    let agent_token = issue_token("scope-agent", "agent", &server.db_path);

    // Cross-scope refusals, each with exactly one audit row.
    for (token, path) in [(&agent_token, "/probe/app"), (&app_token, "/probe/agent")] {
        let resp = client
            .get(format!("{}{path}", server.base))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::FORBIDDEN,
            "{path} cross-scope"
        );
        let body = json_body(resp).await;
        assert_eq!(body["code"], "scope_refused");
    }
    assert!(
        audit_count(&server.db, "refused", "scope").await == 2,
        "two scope rows"
    );

    // Own-scope probes succeed.
    for (token, path) in [(&app_token, "/probe/app"), (&agent_token, "/probe/agent")] {
        let resp = client
            .get(format!("{}{path}", server.base))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), StatusCode::OK, "{path} own scope");
    }
    // The handshake accepts either scope.
    for token in [&app_token, &agent_token] {
        let resp = client
            .get(format!("{}/api/v1/handshake", server.base))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "handshake accepts either scope"
        );
    }
}

// ---------------------------------------------------------------------------
// (iii) The token CLI contract, through the spawned binary.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_cli_issue_revoke_and_list_honor_the_contract() {
    let server = spawn_server().await;

    let token = issue_token("cli-contract", "app", &server.db_path);
    assert!(
        audit_count(&server.db, "issued", "").await == 0,
        "issued rows carry no reason"
    );
    let issued: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM token_audit WHERE event = 'issued' AND label = 'cli-contract'",
    )
    .fetch_one(server.db.pool())
    .await
    .expect("issued count");
    assert_eq!(issued, 1, "one issued audit row");

    let out = revoke_token("cli-contract", &server.db_path);
    assert!(out.status.success(), "revoke must succeed");
    let revoked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM token_audit WHERE event = 'revoked' AND label = 'cli-contract'",
    )
    .fetch_one(server.db.pool())
    .await
    .expect("revoked count");
    assert_eq!(revoked, 1, "one revoked audit row");

    // `list` names label, scope, created_at and the state, and never the token
    // or its hash.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args(["token", "list", "--db"])
        .arg(&server.db_path)
        .output()
        .expect("spawn pulse token list");
    assert!(out.status.success(), "list must succeed");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "one token, one line: {stdout:?}");
    let line = lines[0];
    assert!(line.contains("cli-contract"), "label is listed: {line}");
    assert!(line.contains("app"), "scope is listed: {line}");
    assert!(line.contains("revoked"), "state is listed: {line}");
    assert!(!line.contains(&token), "list never prints the token");
    assert!(
        !contains_64_hex(line),
        "list never prints a 64-hex hash: {line}"
    );

    // A duplicate label is refused, even though the first token is revoked.
    let dup = std::process::Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "token",
            "issue",
            "--scope",
            "app",
            "--label",
            "cli-contract",
            "--db",
        ])
        .arg(&server.db_path)
        .output()
        .expect("spawn duplicate issue");
    assert!(!dup.status.success(), "duplicate label must be refused");
    assert!(
        String::from_utf8_lossy(&dup.stdout).trim().is_empty(),
        "a refused issue prints nothing on stdout"
    );
    let issued_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM token_audit WHERE event = 'issued' AND label = 'cli-contract'",
    )
    .fetch_one(server.db.pool())
    .await
    .expect("issued count after dup");
    assert_eq!(issued_after, 1, "the duplicate wrote nothing");

    // Revoking an unknown label is refused.
    let unknown = std::process::Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args(["token", "revoke", "--label", "no-such-label", "--db"])
        .arg(&server.db_path)
        .output()
        .expect("spawn unknown revoke");
    assert!(
        !unknown.status.success(),
        "revoking an unknown label must be refused"
    );
}

// ---------------------------------------------------------------------------
// (iv) Nothing leaks.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_token_string_lands_in_any_table_or_the_log_sink() {
    let server = spawn_server().await;
    let client = reqwest::Client::new();
    let app_token = issue_token("leak-app", "app", &server.db_path);
    let agent_token = issue_token("leak-agent", "agent", &server.db_path);

    // Exercise the auth path with both, then revoke one.
    for (token, path) in [(&app_token, "/probe/app"), (&agent_token, "/probe/agent")] {
        let resp = client
            .get(format!("{}{path}", server.base))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let _ = revoke_token("leak-agent", &server.db_path);

    for token in [&app_token, &agent_token] {
        for cell in all_text_cells(&server.db).await {
            assert!(
                !cell.contains(token.as_str()),
                "the token string leaked into a table cell: {cell}"
            );
        }
        for line in server.log.lines() {
            assert!(
                !line.contains(token.as_str()),
                "the token string leaked into the log sink: {line}"
            );
        }
    }

    // The stored hash equals the SHA-256 hex of the issued token.
    let stored: String =
        sqlx::query_scalar("SELECT token_sha256 FROM client_token WHERE label = 'leak-app'")
            .fetch_one(server.db.pool())
            .await
            .expect("stored hash");
    let expected = hex::encode(Sha256::digest(app_token.as_bytes()));
    assert_eq!(
        stored, expected,
        "token_sha256 is the sha256 hex of the token"
    );
    assert!(
        stored.chars().all(|c| c.is_ascii_hexdigit()) && stored.len() == 64,
        "the stored hash is 64 hex chars"
    );
}

// ---------------------------------------------------------------------------
// (v) The API-version header on every response.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_response_carries_the_api_version_header() {
    let server = spawn_server().await;
    let client = reqwest::Client::new();
    let token = issue_token("header-app", "app", &server.db_path);

    let mut responses = Vec::new();
    // Authenticated 200.
    responses.push(
        client
            .get(format!("{}/probe/app", server.base))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("send"),
    );
    // 401 refusal.
    responses.push(
        client
            .get(format!("{}/probe/app", server.base))
            .send()
            .await
            .expect("send"),
    );
    // 404, authenticated.
    responses.push(
        client
            .get(format!("{}/no-such-route", server.base))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("send"),
    );
    // 404, unauthenticated.
    responses.push(
        client
            .get(format!("{}/no-such-route", server.base))
            .send()
            .await
            .expect("send"),
    );
    // Handshake.
    responses.push(
        client
            .get(format!("{}/api/v1/handshake", server.base))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("send"),
    );

    for resp in &responses {
        let got = resp
            .headers()
            .get("X-Pulse-Api-Version")
            .expect("api-version header");
        let expected = API_VERSION.to_string();
        assert_eq!(
            got.to_str().expect("ascii header"),
            expected.as_str(),
            "version header value"
        );
    }
    assert_eq!(
        responses[2].status(),
        StatusCode::NOT_FOUND,
        "unknown route is 404"
    );
    assert_eq!(
        responses[3].status(),
        StatusCode::NOT_FOUND,
        "unknown route is 404"
    );
}

// ---------------------------------------------------------------------------
// (vi) The handshake body.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_body_reports_the_running_versions() {
    let server = spawn_server().await;
    let client = reqwest::Client::new();
    let token = issue_token("handshake-app", "app", &server.db_path);

    let resp = client
        .get(format!("{}/api/v1/handshake", server.base))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(
        body["api_version"], API_VERSION,
        "api_version == the constant"
    );
    assert_eq!(
        body["binary_version"],
        env!("CARGO_PKG_VERSION"),
        "binary_version == crate version"
    );
    assert_eq!(
        body["engine_fingerprint"],
        EngineFingerprint::current().as_str(),
        "engine_fingerprint == the build-time fingerprint"
    );
    assert_eq!(
        body["target_triple"],
        EngineFingerprint::target(),
        "target_triple == compiled triple"
    );
}

// ---------------------------------------------------------------------------
// (vii) Append-only and once-only triggers.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tables_refuse_every_write_beyond_their_law() {
    let server = spawn_server().await;
    let db2 = open_migrated(&server.db_path).await.expect("second pool");
    let _ = issue_token("trigger-app", "app", &server.db_path);

    // token_audit is append-only.
    let result = sqlx::query("UPDATE token_audit SET reason = 'x'")
        .execute(db2.pool())
        .await;
    assert!(result.is_err(), "token_audit UPDATE must fail");
    let result = sqlx::query("DELETE FROM token_audit")
        .execute(db2.pool())
        .await;
    assert!(result.is_err(), "token_audit DELETE must fail");

    // client_token refuses every column change except a FIRST revoked_at.
    for column in [
        "label",
        "scope",
        "token_sha256",
        "created_at",
        "created_by",
        "schema_version",
    ] {
        let sql = format!("UPDATE client_token SET {column} = {column} || '-x'");
        let result = sqlx::query(&sql).execute(db2.pool()).await;
        assert!(result.is_err(), "client_token {column} UPDATE must fail");
    }
    let result = sqlx::query("DELETE FROM client_token")
        .execute(db2.pool())
        .await;
    assert!(result.is_err(), "client_token DELETE must fail");

    // The one sanctioned transition: revoked_at NULL -> value, exactly once.
    let first = sqlx::query("UPDATE client_token SET revoked_at = ? WHERE label = ?")
        .bind("2026-09-24T00:00:00Z")
        .bind("trigger-app")
        .execute(db2.pool())
        .await
        .expect("the first revoked_at write is legal");
    assert_eq!(first.rows_affected(), 1, "the first transition applies");
    let second = sqlx::query("UPDATE client_token SET revoked_at = ? WHERE label = ?")
        .bind("2026-09-25T00:00:00Z")
        .bind("trigger-app")
        .execute(db2.pool())
        .await;
    assert!(second.is_err(), "a second revoked_at write must fail");
}

// ---------------------------------------------------------------------------
// (viii) The request-log line.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_request_log_names_the_label_or_a_dash() {
    let server = spawn_server().await;
    let client = reqwest::Client::new();
    let token = issue_token("logged-app", "app", &server.db_path);

    client
        .get(format!("{}/probe/app", server.base))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("send");
    client
        .get(format!("{}/probe/app", server.base))
        .send()
        .await
        .expect("send");
    client
        .get(format!("{}/probe/app?secret=1", server.base))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("send");

    let lines = server.log.lines();
    let authenticated = lines
        .iter()
        .find(|l| l.contains("GET /probe/app") && l.contains("200") && l.contains("logged-app"))
        .expect("an authenticated line naming the label");
    assert!(
        authenticated.starts_with("pulse serve: GET /probe/app 200 logged-app ")
            && authenticated.ends_with("ms"),
        "the line format is exact: {authenticated}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("GET /probe/app") && l.contains("401") && l.contains(" - ")),
        "a refused request is logged with a dash: {lines:?}"
    );
    for line in &lines {
        assert!(
            !line.contains("secret=1"),
            "the query string is never logged: {line}"
        );
        assert!(!line.contains(&token), "no header value is logged: {line}");
    }
}
