//! Shared in-process server harness for the r3.s3.w2 command-surface tests
//! (`tests/server_stream.rs` and `tests/server_routes.rs`).
//!
//! Mirrors `tests/server_auth.rs`'s established harness — a real router served
//! on `127.0.0.1:0` over a migrated temp DB, tokens issued by the real
//! `pulse token issue` binary — and adds what the command surface needs:
//! the BTCUSDT fixture store copied under the server's own `data_dir` (so the
//! server's `CandleStore`, rooted at `data_dir` per the spec, sees the fixture
//! candles), the `SweepConfig` seam for millisecond-scale retention tests, and
//! the compose-runner seam for the scripted-provider tests.
//!
//! The server's `DesktopState` is built over the SAME pool the server opened
//! (the one-pool rule); a test that needs a state of its own opens a separate
//! pool sequentially, exactly like `tests/tauri_backtest.rs`'s `cold_state`.
//!
//! This module compiles into EVERY suite that declares `mod support;` —
//! including the ones that only use `support::mcp` — so each item here is
//! dead code from their perspective; the module-wide allowance is what keeps
//! `deny(warnings)` honest about that, not a blanket escape hatch.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pulse::{
    CandleStore, CaptureLog, ComposeRunner, Db, DesktopState, ServerState, StrategyId,
    StrategyRepository, SweepConfig, VersionId, open_migrated, router,
};
use tempfile::TempDir;

/// The committed candle fixture every arm runs over (`tests/tauri_backtest.rs`).
pub const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

/// The golden strategy the fixture contract was measured against.
pub const GOLDEN_STRATEGY: &str = "tests/fixtures/strategies/rsi-oversold-long.json";

/// One path inside the committed fixtures, resolved from the manifest dir.
pub fn manifest(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// Recursive copy used to give each test a WRITABLE fixture (the committed
/// store is never touched).
pub fn copy_tree(from: &Path, to: &Path) {
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

/// The compose option's factory type: built against the server's own pool
/// handle once the harness has made it (a runner cannot be built beforehand).
pub type ComposeRunnerFactory = Box<dyn Fn(&Db) -> ComposeRunner + Send + Sync>;

/// Construction-time seams: `None` keeps the production default.
#[derive(Default)]
pub struct ServerOptions {
    /// Retention override — tests use milliseconds.
    pub sweep: Option<SweepConfig>,
    /// The compose-runner seam (the spec's "narrowest test seam").
    pub compose_runner: Option<ComposeRunnerFactory>,
}

/// A live in-process server plus everything a scenario needs to drive it.
/// Some fields serve only one of the two suites (`server_stream` never reads
/// `agent_token`; `server_routes` does) — hence the struct-wide allowance.
#[allow(dead_code)]
pub struct TestServer {
    pub _tmp: TempDir,
    /// `http://127.0.0.1:<port>` — append `/api/v1/...` paths.
    pub base: String,
    pub db_path: PathBuf,
    /// A handle to the SAME pool the server opened (the one-pool rule) — the
    /// compose-runner seam builds its repos on it.
    pub db: Db,
    /// The server's own state — its latch map is THE single-flight map, which
    /// the busy-refusal test holds open the way a real in-flight op does.
    pub state: Arc<ServerState>,
    /// The server's data dir; its `candles/` subtree IS the fixture copy.
    pub data_dir: PathBuf,
    pub log: Arc<CaptureLog>,
    pub app_token: String,
    pub agent_token: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl TestServer {
    /// A test-side desktop state over the same paths (a separate, sequential
    /// pool — the `cold_state` pattern). Used for seeding and for comparing a
    /// route's body against the core's own result on the same state.
    pub async fn desktop(&self) -> DesktopState {
        DesktopState::open_with_store(
            &self.db_path,
            CandleStore::with_base_dir(self.data_dir.clone()),
        )
        .await
        .expect("open test-side desktop state")
    }
}

/// Spawn the server exactly as `pulse serve` does: one pool opened over a
/// migrated temp DB, `ServerState` over that pool + a `CandleStore` rooted at
/// `data_dir`, the full router on `127.0.0.1:0`.
pub async fn spawn_server(opts: ServerOptions) -> TestServer {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("pulse.db");
    let data_dir = tmp.path().join("data");
    // The fixture candles land INSIDE data_dir so the server's store — rooted
    // at data_dir by the spec — reads the fixture subtree.
    copy_tree(&manifest(FIXTURE_STORE), &data_dir);

    let db: Db = open_migrated(&db_path)
        .await
        .expect("migrate-then-open the temp db");
    let log = Arc::new(CaptureLog::default());
    let mut state = ServerState::with_log(db.clone(), data_dir.clone(), log.clone());
    if let Some(sweep) = opts.sweep {
        state = state.with_sweep(sweep);
    }
    if let Some(make_runner) = opts.compose_runner {
        state = state.with_compose_runner(make_runner(&db));
    }
    let state = Arc::new(state);
    let app = router(state.clone());

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind server listener");
    // `from_std` hands the fd to tokio, which requires non-blocking mode.
    listener
        .set_nonblocking(true)
        .expect("set server listener non-blocking");
    let addr: SocketAddr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        axum::serve(
            tokio::net::TcpListener::from_std(listener).expect("tokio listener"),
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("axum serve");
    });

    let app_token = issue_token("w2-app", "app", &db_path);
    let agent_token = issue_token("w2-agent", "agent", &db_path);

    TestServer {
        _tmp: tmp,
        base: format!("http://{addr}"),
        db_path,
        db,
        state,
        data_dir,
        log,
        app_token,
        agent_token,
        handle,
    }
}

/// Spawn `pulse token issue` against the server's DB (`tests/server_auth.rs`).
pub fn issue_token(label: &str, scope: &str, db_path: &Path) -> String {
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
    lines[0].to_owned()
}

/// Seed a strategy plus one real, compilable version from the golden DSL
/// (`tests/tauri_backtest.rs`'s `seed_version`) and return the version id.
pub async fn seed_version(state: &DesktopState) -> VersionId {
    let repo = state.strategy_repo();
    let strat = repo
        .create_strategy("w2 demo", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create strategy");
    let dsl = std::fs::read_to_string(manifest(GOLDEN_STRATEGY)).expect("read golden strategy");
    repo.create_version(pulse::NewVersion {
        strategy_id: StrategyId::new(strat.id.as_str().to_owned()),
        parent_version_id: None,
        dsl_json: dsl,
        created_by: pulse::CreatedBy::Human,
        creating_llm_call_ids: vec![],
    })
    .await
    .expect("create version")
    .id
}

/// Millisecond-scale sweep config for the retention tests. Only the stream
/// suite needs it, so the routes suite's dead-code gate gets an allowance.
#[allow(dead_code)]
pub fn fast_sweep(interval_ms: u64, progress_ms: u64, terminal_ms: u64) -> SweepConfig {
    SweepConfig {
        interval: Duration::from_millis(interval_ms),
        progress_window: Duration::from_millis(progress_ms),
        terminal_window: Duration::from_millis(terminal_ms),
    }
}
