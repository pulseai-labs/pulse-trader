//! AC-1 — the app token lives in the token store; `server-connection.toml` keeps the URL only.
//!
//! The w2 ledger line (`cargo nextest run --test client_token_store`). One claim
//! per case, all driven from OUTSIDE the crate through the same seams the app
//! uses: [`ClientState`] with an injected [`pulse::ClientWiring`] (the
//! in-memory token store + a capturing audit sink), and a per-test
//! `$PULSE_CONFIG_DIR` for the connection files.
//!
//! Hermetic by construction: the production Keychain adapter is never
//! constructed here, so no real Keychain is touched on any platform — and on
//! Linux the keyring mock is not reached either, because the fake replaces the
//! production store wholesale.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pulse::{
    ClientState, ClientWiring, ConnectOutcome, ConnectionFile, InMemoryTokenStore,
    ServerStatusState, TokenAudit, load_app, load_mcp, server_address, store_mcp,
};
use support::server::{ServerOptions, spawn_server};

/// Serializes the tests that set or read `$PULSE_CONFIG_DIR` — the
/// `tests/client_proxy.rs` precedent: `src/client/connection.rs` reads the
/// variable at CALL time and process-wide, so two tests interleaving their
/// windows would read each other's directory.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The audit sink every case captures: the risk gate's "log line captured by
/// the test".
#[derive(Default)]
struct RecordingAudit {
    lines: Mutex<Vec<String>>,
}

impl RecordingAudit {
    fn lines(&self) -> Vec<String> {
        self.lines.lock().expect("the audit capture lock").clone()
    }
}

impl TokenAudit for RecordingAudit {
    fn line(&self, line: &str) {
        self.lines
            .lock()
            .expect("the audit capture lock")
            .push(line.to_owned());
    }
}

/// One hermetic world: its own `$PULSE_CONFIG_DIR`, its own in-memory store,
/// its own audit capture, and the [`ClientWiring`] over them.
struct World {
    dir: tempfile::TempDir,
    store: Arc<InMemoryTokenStore>,
    audit: Arc<RecordingAudit>,
    wiring: ClientWiring,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("config tempdir");
        // SAFETY: serialized by ENV_LOCK — every test in this file that reads
        // `$PULSE_CONFIG_DIR` holds the same lock, so no other thread can be
        // inside an env window while this one is.
        unsafe {
            std::env::set_var("PULSE_CONFIG_DIR", dir.path());
        }
        let store = Arc::new(InMemoryTokenStore::default());
        let audit = Arc::new(RecordingAudit::default());
        Self {
            dir,
            store: Arc::clone(&store),
            audit: Arc::clone(&audit),
            wiring: ClientWiring { store, audit },
        }
    }

    fn app_file(&self) -> PathBuf {
        self.dir.path().join("server-connection.toml")
    }

    fn mcp_file(&self) -> PathBuf {
        self.dir.path().join("mcp-connection.toml")
    }

    /// Hand-write the app file at 0600 (the legacy `{url, token}` shape, or a
    /// URL-only one) — the vetted read demands the owner-only mode.
    fn write_app_file(&self, body: &str) {
        write_0600(&self.app_file(), body);
    }

    /// A wiring with a store of the caller's choosing and this world's audit
    /// capture — the migration's failure branches each drive their own store.
    fn wiring_with(&self, store: Arc<InMemoryTokenStore>) -> ClientWiring {
        ClientWiring {
            store,
            audit: self.audit.clone(),
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // SAFETY: still inside the ENV_LOCK window (the guard outlives this value).
        unsafe {
            std::env::remove_var("PULSE_CONFIG_DIR");
        }
    }
}

/// Write `body` at mode 0600, the mode the connection-file vetting demands.
fn write_0600(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::write(path, body).expect("write the file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("pin 0600");
}

/// The file's mode, without the type bits.
fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::metadata(path)
        .expect("stat the file")
        .permissions()
        .mode()
        & 0o777
}

/// A good connect stores the token in the STORE and leaves the app file
/// holding the URL only — and never touches the relay's own file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_stores_the_token_and_leaves_the_url_only_file() {
    let _guard = ENV_LOCK.lock().await;
    let world = World::new();
    let server = spawn_server(ServerOptions::default()).await;
    let state = ClientState::with_wiring(world.wiring.clone());

    let outcome = state.connect(&server.base, &server.app_token).await;
    assert!(
        matches!(outcome, ConnectOutcome::Connected { .. }),
        "the connect succeeds: {outcome:?}"
    );

    assert_eq!(
        world.store.token().as_deref(),
        Some(server.app_token.as_str()),
        "the app token is in the token store"
    );
    let url = server.base.trim_end_matches('/');
    let text = std::fs::read_to_string(world.app_file()).expect("the app file exists");
    assert!(
        text.contains(url),
        "the app file still names the server: {text}"
    );
    assert!(
        !text.contains("token"),
        "no token is ever written into the app file: {text}"
    );
    assert_eq!(
        mode_of(&world.app_file()),
        0o600,
        "the app file stays owner-only"
    );
    let parsed = load_app().expect("the app file reads").expect("it exists");
    assert_eq!(parsed.url, url);
    assert!(
        parsed.token.is_none(),
        "the app file parses with no token: {parsed:?}"
    );
    assert!(
        !world.mcp_file().exists(),
        "connect must not write the relay's file"
    );
    let lines = world.audit.lines();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("PulseTrader/server_app_token:")),
        "the store write logs one line naming the item: {lines:?}"
    );
    assert!(
        lines.iter().all(|line| !line.contains(&server.app_token)),
        "no audit line carries the value: {lines:?}"
    );
}

/// A relaunch with the token in the store and a URL-only file comes up connected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relaunch_loads_the_token_from_the_store_and_comes_up_connected() {
    let _guard = ENV_LOCK.lock().await;
    let world = World::new();
    let server = spawn_server(ServerOptions::default()).await;

    // What a relaunch finds: the URL-only file, and the token in the store.
    let url = server.base.trim_end_matches('/').to_owned();
    world.write_app_file(&format!("url = \"{url}\"\n"));
    let wiring = ClientWiring {
        store: Arc::new(InMemoryTokenStore::with_token(&server.app_token)),
        audit: world.audit.clone(),
    };

    let reloaded = ClientState::loaded_with_wiring(wiring);
    assert_eq!(
        reloaded.status().await.state,
        ServerStatusState::Up,
        "a relaunch reconnects from the store, not from the file"
    );
}

/// The relay's own file is untouched by this item: it still writes and reads
/// `{ url, token }` (G4 — #384 tracks moving the agent token later).
#[test]
fn the_mcp_file_still_reads_and_writes_url_and_token() {
    let _guard = ENV_LOCK.blocking_lock();
    let world = World::new();

    store_mcp(&ConnectionFile {
        url: "http://100.64.0.1:7433".to_owned(),
        token: "pt_mcp_w2_test_token".to_owned(),
    })
    .expect("store the relay's connection");

    let text = std::fs::read_to_string(world.mcp_file()).expect("the mcp file exists");
    assert!(
        text.contains("pt_mcp_w2_test_token") && text.contains("url"),
        "the relay's file keeps the token and the URL: {text}"
    );
    assert_eq!(mode_of(&world.mcp_file()), 0o600, "still owner-only");
    let loaded = load_mcp().expect("the file reads").expect("it exists");
    assert_eq!(loaded.url, "http://100.64.0.1:7433");
    assert_eq!(loaded.token, "pt_mcp_w2_test_token");
}

// ---------------------------------------------------------------------------
// The one-time verified migration (G3): write, read back, compare, rewrite —
// and on ANY failure the file is left exactly as it was and the app starts
// refused with the named reason, never falling back to the file token.
// ---------------------------------------------------------------------------

/// A fixed, non-secret stand-in for an older file's token.
const FILE_TOKEN: &str = "pt_w2_legacy_file_token";

/// The URL the failure-branch tests write into the file; no server is needed
/// because a refused state never handshakes.
const FILE_URL: &str = "http://100.64.0.1:7433";

/// The legacy `{ url, token }` body at 0600.
fn legacy_body(url: &str, token: &str) -> String {
    format!("url = \"{url}\"\ntoken = \"{token}\"\n")
}

/// The migration's happy path: the file token moves into the store, the file is
/// rewritten URL-only, and the same token still connects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_migration_moves_a_file_token_into_the_store_and_rewrites_the_file() {
    let _guard = ENV_LOCK.lock().await;
    let world = World::new();
    let server = spawn_server(ServerOptions::default()).await;
    let url = server.base.trim_end_matches('/').to_owned();
    world.write_app_file(&legacy_body(&url, &server.app_token));

    let state = ClientState::loaded_with_wiring(world.wiring.clone());
    assert_eq!(
        state.status().await.state,
        ServerStatusState::Up,
        "the migrated token still connects"
    );
    assert_eq!(
        world.store.token().as_deref(),
        Some(server.app_token.as_str()),
        "the store now holds the file's token"
    );
    let text = std::fs::read_to_string(world.app_file()).expect("the file exists");
    assert!(
        text.contains(&url) && !text.contains("token"),
        "the file was rewritten as URL-only: {text}"
    );
    let parsed = load_app().expect("the file reads").expect("it exists");
    assert!(parsed.token.is_none());
    let lines = world.audit.lines();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("PulseTrader/server_app_token:")),
        "the migration logs one line naming the item: {lines:?}"
    );
    assert!(
        lines.iter().all(|line| !line.contains(&server.app_token)),
        "no audit line carries the value: {lines:?}"
    );
}

/// A failed WRITE: the file is untouched, the state refused with the named
/// reason, nothing stored, and no client was built from the file token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_migration_leaves_the_file_untouched_when_the_write_fails() {
    let _guard = ENV_LOCK.lock().await;
    let world = World::new();
    world.write_app_file(&legacy_body(FILE_URL, FILE_TOKEN));
    let before = std::fs::read(world.app_file()).expect("read the file");
    let store = Arc::new(InMemoryTokenStore::default());
    store.fail_writes("the store refused the write");

    let state = ClientState::loaded_with_wiring(world.wiring_with(Arc::clone(&store)));
    let status = state.status().await;
    assert_eq!(status.state, ServerStatusState::Refused);
    let reason = status.reason.expect("a refusal carries its reason");
    assert!(
        reason.contains("write failed") && reason.contains("server_app_token"),
        "the named reason names the failing step and the item: {reason}"
    );
    assert!(!reason.contains(FILE_TOKEN), "never the value: {reason}");
    assert!(store.token().is_none(), "the failed write stored nothing");
    assert_eq!(
        std::fs::read(world.app_file()).expect("the file still reads"),
        before,
        "the file is byte-for-byte unchanged"
    );
    assert!(
        state
            .get::<serde_json::Value>("/api/v1/shell-info")
            .await
            .is_err(),
        "no client was built from the file token"
    );
    let lines = world.audit.lines();
    assert!(
        lines.iter().any(|line| line.contains("write failed")
            && line.starts_with("PulseTrader/server_app_token:")),
        "the outcome is logged against the item: {lines:?}"
    );
    assert!(lines.iter().all(|line| !line.contains(FILE_TOKEN)));
}

/// A failed READ-BACK: the write landed, the check did not — the file is still
/// untouched and the state refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_migration_leaves_the_file_untouched_when_the_read_back_fails() {
    let _guard = ENV_LOCK.lock().await;
    let world = World::new();
    world.write_app_file(&legacy_body(FILE_URL, FILE_TOKEN));
    let before = std::fs::read(world.app_file()).expect("read the file");
    let store = Arc::new(InMemoryTokenStore::default());
    store.fail_reads("the store refused the read");

    let state = ClientState::loaded_with_wiring(world.wiring_with(Arc::clone(&store)));
    let status = state.status().await;
    assert_eq!(status.state, ServerStatusState::Refused);
    let reason = status.reason.expect("a refusal carries its reason");
    assert!(
        reason.contains("read-back failed") && reason.contains("server_app_token"),
        "the named reason names the failing step and the item: {reason}"
    );
    assert!(!reason.contains(FILE_TOKEN), "never the value: {reason}");
    assert_eq!(
        std::fs::read(world.app_file()).expect("the file still reads"),
        before,
        "the file is byte-for-byte unchanged"
    );
    assert!(
        state
            .get::<serde_json::Value>("/api/v1/shell-info")
            .await
            .is_err(),
        "no client was built from the file token"
    );
}

/// A MISMATCHED read-back: the store answers a different value than the file
/// held — treated exactly like a failed verification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_migration_leaves_the_file_untouched_on_a_mismatched_read_back() {
    let _guard = ENV_LOCK.lock().await;
    let world = World::new();
    world.write_app_file(&legacy_body(FILE_URL, FILE_TOKEN));
    let before = std::fs::read(world.app_file()).expect("read the file");
    let store = Arc::new(InMemoryTokenStore::default());
    store.answer_reads_with("pt_w2_some_other_stored_value");

    let state = ClientState::loaded_with_wiring(world.wiring_with(Arc::clone(&store)));
    let status = state.status().await;
    assert_eq!(status.state, ServerStatusState::Refused);
    let reason = status.reason.expect("a refusal carries its reason");
    assert!(
        reason.contains("did not match") && reason.contains("server_app_token"),
        "the named reason names the mismatch and the item: {reason}"
    );
    assert!(!reason.contains(FILE_TOKEN), "never the value: {reason}");
    assert!(
        !reason.contains("pt_w2_some_other_stored_value"),
        "nor the value the store answered: {reason}"
    );
    assert_eq!(
        std::fs::read(world.app_file()).expect("the file still reads"),
        before,
        "the file is byte-for-byte unchanged"
    );
}

/// A failed REWRITE: the store now holds the token, the file still holds it too
/// — untouched, refused, and the next launch retries the same sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_migration_leaves_the_file_untouched_when_the_rewrite_fails() {
    let _guard = ENV_LOCK.lock().await;
    let world = World::new();
    world.write_app_file(&legacy_body(FILE_URL, FILE_TOKEN));
    let before = std::fs::read(world.app_file()).expect("read the file");
    // A symlink at the temporary path makes the rewrite refuse by name (the
    // shared store's own guard), so the failure is deterministic.
    let tmp = world.app_file().with_extension("toml.tmp");
    std::os::unix::fs::symlink(world.dir.path().join("elsewhere"), &tmp).expect("plant the link");
    let store = Arc::new(InMemoryTokenStore::default());

    let state = ClientState::loaded_with_wiring(world.wiring_with(Arc::clone(&store)));
    let status = state.status().await;
    assert_eq!(status.state, ServerStatusState::Refused);
    let reason = status.reason.expect("a refusal carries its reason");
    assert!(
        reason.contains("rewriting the connection file") && reason.contains("server_app_token"),
        "the named reason names the failing step and the item: {reason}"
    );
    assert!(!reason.contains(FILE_TOKEN), "never the value: {reason}");
    assert_eq!(
        std::fs::read(world.app_file()).expect("the file still reads"),
        before,
        "the file is byte-for-byte unchanged"
    );
    assert_eq!(
        store.token().as_deref(),
        Some(FILE_TOKEN),
        "the store holds the verified token; the next launch retries the rewrite"
    );
    let lines = world.audit.lines();
    assert!(
        lines.iter().all(|line| !line.contains(FILE_TOKEN)),
        "{lines:?}"
    );
}

/// An ABSENT store item under a URL-only file is `not_connected` — the trader
/// connects again, not a refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absent_store_item_with_a_url_only_file_comes_up_not_connected() {
    let _guard = ENV_LOCK.lock().await;
    let world = World::new();
    world.write_app_file(&format!("url = \"{FILE_URL}\"\n"));

    let state = ClientState::loaded_with_wiring(world.wiring.clone());
    let status = state.status().await;
    assert_eq!(status.state, ServerStatusState::NotConnected);
    assert!(status.reason.is_none(), "absence carries no reason");
}

/// A store ERROR under a URL-only file is `refused` with the named reason — the
/// two load outcomes are distinct.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_error_on_load_is_refused_with_its_named_reason() {
    let _guard = ENV_LOCK.lock().await;
    let world = World::new();
    world.write_app_file(&format!("url = \"{FILE_URL}\"\n"));
    let store = Arc::new(InMemoryTokenStore::default());
    store.fail_reads("the store is locked");

    let state = ClientState::loaded_with_wiring(world.wiring_with(Arc::clone(&store)));
    let status = state.status().await;
    assert_eq!(status.state, ServerStatusState::Refused);
    let reason = status.reason.expect("a refusal carries its reason");
    assert!(
        reason.contains("server_app_token") && reason.contains("the store is locked"),
        "the named reason names the item and the failure: {reason}"
    );
}

/// Disconnect removes the store item AND the file; a missing item or file is
/// already the goal (G9).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_removes_the_store_item_and_the_file() {
    let _guard = ENV_LOCK.lock().await;
    let world = World::new();
    let server = spawn_server(ServerOptions::default()).await;
    let state = ClientState::with_wiring(world.wiring.clone());
    let outcome = state.connect(&server.base, &server.app_token).await;
    assert!(
        matches!(outcome, ConnectOutcome::Connected { .. }),
        "{outcome:?}"
    );
    assert!(world.store.token().is_some() && world.app_file().exists());

    state.disconnect().expect("disconnect answers");
    assert!(world.store.token().is_none(), "the item is gone");
    assert!(!world.app_file().exists(), "the file is gone");
    assert_eq!(state.status().await.state, ServerStatusState::NotConnected);
    let lines = world.audit.lines();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("PulseTrader/server_app_token:")
                && line.contains("removed")),
        "the delete logs one line naming the item and its outcome: {lines:?}"
    );

    // Idempotent: a second disconnect has nothing to remove and still answers.
    state.disconnect().expect("a second disconnect answers");
}

/// The redaction rule, over every path a value could escape: no error string,
/// `ConnectOutcome`, `ServerStatus`, captured audit line or `server_address`
/// result ever carries the fixed token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_failure_path_leaks_the_token_into_a_reason_outcome_status_or_log_line() {
    const REDACTION_TOKEN: &str = "pt_w2_redaction_canary_token";

    let _guard = ENV_LOCK.lock().await;
    let world = World::new();
    let server = spawn_server(ServerOptions::default()).await;
    let url = server.base.trim_end_matches('/').to_owned();

    // 1. A refused connect: the outcome and the status carry the reason, never
    //    the value that was offered.
    let state = ClientState::with_wiring(world.wiring.clone());
    let outcome = state.connect(&server.base, REDACTION_TOKEN).await;
    assert!(
        matches!(outcome, ConnectOutcome::TokenRefused { .. }),
        "an unknown token is refused: {outcome:?}"
    );
    assert!(!format!("{outcome:?}").contains(REDACTION_TOKEN));
    let status = state.status().await;
    assert_eq!(status.state, ServerStatusState::Refused);
    assert!(!format!("{status:?}").contains(REDACTION_TOKEN));
    assert!(
        !serde_json::to_string(&status)
            .expect("serialise the status")
            .contains(REDACTION_TOKEN)
    );
    assert!(
        !status
            .reason
            .clone()
            .unwrap_or_default()
            .contains(REDACTION_TOKEN)
    );

    // 2. A failed migration with the value in the file: the named reason and
    //    the audit lines name the item and the step, never the value.
    world.write_app_file(&legacy_body(&url, REDACTION_TOKEN));
    let failing = Arc::new(InMemoryTokenStore::default());
    failing.fail_writes("the store refused the write");
    let failed = ClientState::loaded_with_wiring(world.wiring_with(Arc::clone(&failing)));
    let refused = failed.status().await;
    assert_eq!(refused.state, ServerStatusState::Refused);
    assert!(
        !refused
            .reason
            .clone()
            .unwrap_or_default()
            .contains(REDACTION_TOKEN)
    );

    // 3. A successful migration of the same value: the store holds it, the file
    //    is rewritten, and no captured line carries it.
    let stored = Arc::new(InMemoryTokenStore::default());
    let migrated = ClientState::loaded_with_wiring(world.wiring_with(Arc::clone(&stored)));
    assert_eq!(stored.token().as_deref(), Some(REDACTION_TOKEN));
    let _ = migrated.status().await; // may be refused (the token is not the server's)

    // 4. `server_address` answers the URL and nothing else.
    let address = server_address().await.expect("the read answers");
    assert_eq!(address.as_deref(), Some(url.as_str()));
    assert!(!format!("{address:?}").contains(REDACTION_TOKEN));

    // 5. Disconnect's delete line, same rule.
    let state = ClientState::with_wiring(world.wiring_with(Arc::clone(&stored)));
    state.disconnect().expect("disconnect answers");

    let lines = world.audit.lines();
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("PulseTrader/server_app_token:")),
        "every audit line names the item: {lines:?}"
    );
    assert!(
        lines.iter().all(|line| !line.contains(REDACTION_TOKEN)),
        "no audit line carries the value: {lines:?}"
    );
}

// ---------------------------------------------------------------------------
// #234 (the Rust half): a stale status poll must not overwrite a newer link
// ---------------------------------------------------------------------------

/// The handshake body the stub answers with (the `client_proxy.rs` stub shape,
/// plus the app scope the client requires).
const STUB_HANDSHAKE_BODY: &str = "{\"api_version\":1,\"binary_version\":\"0.0.1\",\
     \"engine_fingerprint\":\"sha256:w2-stub\",\"target_triple\":\"x86_64-unknown-linux-gnu\",\
     \"scope\":\"app\"}";

/// Read one request off a stub connection (small GETs arrive in one read).
async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt as _;

    let mut buffer = vec![0_u8; 4096];
    let read = stream.read(&mut buffer).await.expect("read the request");
    String::from_utf8_lossy(&buffer[..read]).into_owned()
}

/// Answer one request with `status` and a JSON body.
async fn answer(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    use tokio::io::AsyncWriteExt as _;

    stream
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                 x-pulse-api-version: 1\r\nconnection: close\r\n\
                 content-length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .expect("answer the request");
}

/// A stub that answers the FIRST handshake 200 (so a connect installs that
/// link), then HOLDS the next handshake until the test releases it, and then
/// answers 401 — the stale-poll interleaving, made deterministic.
async fn spawn_holdable_stub() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the stub");
    let base = format!("http://{}", listener.local_addr().expect("stub addr"));
    let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept the connect");
        let request = read_request(&mut stream).await;
        assert!(
            request.contains("GET /api/v1/handshake"),
            "the stub answers the handshake: {request}"
        );
        answer(&mut stream, "200 OK", STUB_HANDSHAKE_BODY).await;

        let (mut stream, _) = listener.accept().await.expect("accept the poll");
        let request = read_request(&mut stream).await;
        assert!(
            request.contains("GET /api/v1/handshake"),
            "the poll is a handshake too: {request}"
        );
        let _ = arrived_tx.send(());
        let _ = release_rx.await;
        answer(&mut stream, "401 Unauthorized", "{}").await;
    });
    (base, arrived_rx, release_tx)
}

/// A connect that completes while an older poll is in flight keeps the NEW link
/// when the old poll returns refused (#234, the Rust half).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connect_that_completes_during_an_older_poll_keeps_the_new_link() {
    let _guard = ENV_LOCK.lock().await;
    let world = World::new();
    let (stub_base, arrived, release) = spawn_holdable_stub().await;
    let server = spawn_server(ServerOptions::default()).await;

    let state = Arc::new(ClientState::with_wiring(world.wiring.clone()));
    let outcome = state.connect(&stub_base, "pt_w2_stub_token").await;
    assert!(
        matches!(outcome, ConnectOutcome::Connected { .. }),
        "the stub's link installs: {outcome:?}"
    );

    // The poll starts against THAT link, and its handshake is held open.
    let polled = {
        let state = Arc::clone(&state);
        tokio::spawn(async move { state.status().await })
    };
    arrived
        .await
        .expect("the stub received the poll's handshake");

    // While it is in flight, a NEW connection completes and installs.
    let outcome = state.connect(&server.base, &server.app_token).await;
    assert!(
        matches!(outcome, ConnectOutcome::Connected { .. }),
        "the new connection installs: {outcome:?}"
    );

    // The old poll comes back refused — it must not replace the new link.
    release.send(()).expect("release the held handshake");
    let stale_result = polled.await.expect("the poll completes");
    assert_eq!(
        stale_result.state,
        ServerStatusState::Up,
        "the stale poll reports the CURRENT link, not its own refusal"
    );
    let status = state.status().await;
    assert_eq!(
        status.state,
        ServerStatusState::Up,
        "the new link survives the older poll's refusal"
    );
    assert!(
        status.binary_version.is_some(),
        "and it is a live handshake against the new server: {status:?}"
    );
}

/// A file-write failure AFTER the store write keeps the stored token, records
/// `Failed` — and the next launch never comes up `Up` on a URL/token pair
/// nothing verified.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_write_failure_after_the_store_write_never_comes_up_on_the_next_launch() {
    let _guard = ENV_LOCK.lock().await;
    let world = World::new();
    let first = spawn_server(ServerOptions::default()).await;
    let second = spawn_server(ServerOptions::default()).await;
    let state = ClientState::with_wiring(world.wiring.clone());

    // A first, complete connection: one URL in the file, one token in the store.
    let outcome = state.connect(&first.base, &first.app_token).await;
    assert!(
        matches!(outcome, ConnectOutcome::Connected { .. }),
        "{outcome:?}"
    );
    assert!(world.app_file().exists());

    // The next connect's FILE write fails (a symlink at the shared temp path);
    // its store write has already landed.
    let tmp = world.app_file().with_extension("toml.tmp");
    std::os::unix::fs::symlink(world.dir.path().join("elsewhere"), &tmp).expect("plant the link");
    let outcome = state.connect(&second.base, &second.app_token).await;
    let reason = match outcome {
        ConnectOutcome::Unreachable { reason } => reason,
        other => panic!("a failed save is an unreachable outcome: {other:?}"),
    };
    assert!(
        reason.contains("saving the connection failed"),
        "the named reason reports the save failure: {reason}"
    );
    assert!(
        !reason.contains(&second.app_token) && !reason.contains(&first.app_token),
        "never a value: {reason}"
    );
    assert_eq!(
        state.status().await.state,
        ServerStatusState::Refused,
        "the failed save is the recorded state"
    );
    assert_eq!(
        world.store.token().as_deref(),
        Some(second.app_token.as_str()),
        "the stored token is kept, not rolled back"
    );

    // The half-saved pair must never be read back as a connection: the file that
    // would have lent the new token the OLD URL is gone, so the next launch
    // starts `not_connected`.
    assert!(
        !world.app_file().exists(),
        "the stale file was removed, never left to pair with the new token"
    );
    let reloaded = ClientState::loaded_with_wiring(world.wiring.clone());
    assert_eq!(
        reloaded.status().await.state,
        ServerStatusState::NotConnected,
        "a pair nothing verified never comes up connected"
    );
}
