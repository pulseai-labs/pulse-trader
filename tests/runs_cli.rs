//! End-to-end integration tests for VS-1.2.4 work-4.05 — the `--version` persist +
//! FR-7 compare path + the `pulse runs list/show` read verb (FR-6 / FR-7).
//!
//! Drives the **binary** (`CARGO_BIN_EXE_pulse`) over a `TempDir` `pulse.db` (the
//! `--db` override — never the real Application Support dir) + the committed
//! `tests/fixtures/btcusdt-1m-store/` candle fixture, mirroring
//! `tests/backtest_cli.rs`'s invocation seam. Where a *prior* persisted run with a
//! DIFFERENT `engine_fingerprint` is needed (the FR-7 mismatch case), it seeds it
//! through the **library** (`pulse::{Db, SqliteBacktestRunRepo, …}`) over the SAME
//! tempfile db — the content hash excludes the fingerprint (D4), so a hand-seeded
//! bogus-fingerprint prior run round-trips cleanly through `get_run`/
//! `latest_run_for_version`.
//!
//! Offline (`SQLX_OFFLINE=true` + committed `.sqlx/` + in-process `MIGRATOR`),
//! `TempDir`-isolated.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use pulse::{
    BacktestInputs, BacktestResult, BacktestRunRepository, CreatedBy, DataVersion, Db,
    EngineFingerprint, EquityCurve, FundingConfig, MIGRATOR, NewVersion, Pair, RegimeBreakdown,
    SkippedEntryCounts, SnapshotSelection, SqliteBacktestRunRepo, SqliteStrategyRepo, StrategyId,
    StrategyRepository, SummaryStats, Timeframe, VersionId,
};
use rust_decimal::Decimal;
use tempfile::TempDir;

/// The fixture candle store the backtest runs over (same as `backtest_cli.rs`).
const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

/// A minimal, valid DSL document (long RSI-oversold; 5% stop = 1R; 2R take-profit;
/// 1% risk) — the same shape `backtest_cli.rs` uses, which produces real trades
/// over the fixture store. Decimals serialize as strings (`rust_decimal`).
const MINIMAL_DSL: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold (runs_cli)",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Indicator", "spec": { "indicator": "Rsi", "period": 14 } },
    "op": "Lt",
    "rhs": { "type": "Constant", "value": "30" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": {
    "risk_per_trade_pct": "0.01",
    "max_leverage": "3"
  }
}"#;

/// A fresh `TempDir` + its `pulse.db` path string.
fn temp_db() -> (TempDir, String) {
    let tmp = TempDir::new().expect("tempdir");
    let path = tmp.path().join("pulse.db");
    let path_str = path.to_str().expect("db path utf8").to_owned();
    (tmp, path_str)
}

/// Seed a strategy + one VERSION (with a real, compilable DSL) through the library
/// repo over the tempfile db, returning the created version id. The db is migrated
/// in-process; the binary then re-opens the SAME file via `open_migrated`.
async fn seed_version(db_path: &str) -> VersionId {
    let db = Db::with_path(Path::new(db_path)).await.expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    let repo = SqliteStrategyRepo::new(db.pool().clone());
    let strat = repo
        .create_strategy("Demo", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create strategy");
    let created = repo
        .create_version(NewVersion {
            strategy_id: StrategyId::new(strat.id.as_str().to_owned()),
            parent_version_id: None,
            dsl_json: MINIMAL_DSL.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");
    created.id
}

/// Seed a PRIOR persisted run carrying a BOGUS (clearly-non-current) fingerprint
/// against `version_id`, through the library `save_run` over the tempfile db. The
/// content hash excludes the fingerprint (D4), so this round-trips cleanly and the
/// subsequent live `--version` run compares its CURRENT fingerprint against this
/// bogus prior → an FR-7 mismatch warning.
async fn seed_prior_run_with_bogus_fingerprint(db_path: &str, version_id: &VersionId) {
    let db = Db::with_path(Path::new(db_path)).await.expect("open db");
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let starting_equity = Decimal::new(10_000, 0);
    // A trade-free prior run: net_pnl 0, all totals 0, default summary/curve. The
    // bogus fingerprint is all-`f` hex (64 chars) — guaranteed != current().
    let result = BacktestResult {
        trades: vec![],
        net_pnl: Decimal::ZERO,
        fees_total: Decimal::ZERO,
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        regime_breakdown: RegimeBreakdown::new(),
        skipped_entries: SkippedEntryCounts::new(),
        open_position: None,
        engine_fingerprint: EngineFingerprint::from_stored("f".repeat(64)),
        summary: SummaryStats::default(),
        equity_curve: EquityCurve::default(),
    };
    // r1.s3.w2 (#110): a fresh save now carries its input provenance. This seed is
    // about the FR-7 fingerprint mismatch, so the tuple is a plain complete one.
    let inputs = BacktestInputs {
        pair: Pair::new("BTCUSDT"),
        primary: SnapshotSelection {
            timeframe: Timeframe::M15,
            data_version: DataVersion::new("v-primary"),
        },
        htf: None,
        taker_fee_bps: Decimal::new(4, 0),
        slippage_bps: Decimal::new(1, 0),
        funding: FundingConfig::SnapshotRates,
        window: None,
        lead_in_from_ms: None,
    };
    repo.save_run(
        version_id,
        &inputs,
        &result,
        &result.summary,
        starting_equity,
    )
    .await
    .expect("seed prior run");
}

/// Run `pulse backtest --version <id> --pair BTCUSDT --tf M15 --store <fixture>
/// --db <tempdb>` and return the captured output.
fn run_version_backtest(db_path: &str, version_id: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "backtest",
            "--version",
            version_id,
            "--pair",
            "BTCUSDT",
            "--tf",
            "M15",
            "--store",
            FIXTURE_STORE,
            "--db",
            db_path,
        ])
        .output()
        .expect("run pulse backtest --version")
}

/// `pulse runs list --version <id> --db <tempdb>`.
fn run_runs_list(db_path: &str, version_id: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args(["runs", "list", "--version", version_id, "--db", db_path])
        .output()
        .expect("run pulse runs list")
}

/// `pulse runs show <run-id> --db <tempdb>`.
fn run_runs_show(db_path: &str, run_id: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args(["runs", "show", run_id, "--db", db_path])
        .output()
        .expect("run pulse runs show")
}

/// Extract the first persisted run id from `runs list` stdout (the second
/// tab-cell of the first `run\t<id>\t…` line).
fn first_run_id(list_stdout: &str) -> String {
    let line = list_stdout
        .lines()
        .find(|l| l.starts_with("run\t"))
        .expect("a run row in `runs list` output");
    line.split('\t').nth(1).expect("run id cell").to_owned()
}

/// Assert a process exited 0, surfacing stderr/stdout on failure.
fn assert_success(out: &std::process::Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} status={:?}\nstderr={}\nstdout={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
}

/// The FR-7 warning substring `compare()` emits (build-env mismatch copy).
const FR7_WARNING: &str = "engine fingerprint mismatch";

// ---- AC-2: persist on a --version run round-trips through the read verb -------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_on_version_roundtrip() {
    let (_tmp, db_path) = temp_db();
    let vid = seed_version(&db_path).await;

    // A --version backtest runs + ALWAYS persists (no --save flag).
    let run = run_version_backtest(&db_path, vid.as_str());
    assert_success(&run, "backtest --version");

    // The run is now in the catalog, and `runs show` renders it end-to-end.
    let list = run_runs_list(&db_path, vid.as_str());
    assert_success(&list, "runs list");
    let list_stdout = String::from_utf8(list.stdout).expect("list stdout utf8");
    assert!(
        list_stdout.contains("run\t"),
        "the persisted run must appear in `runs list`; stdout was:\n{list_stdout}"
    );
    let run_id = first_run_id(&list_stdout);

    let show = run_runs_show(&db_path, &run_id);
    assert_success(&show, "runs show");
    let show_stdout = String::from_utf8(show.stdout).expect("show stdout utf8");
    assert!(
        show_stdout.contains("expectancy="),
        "`runs show` must render the expectancy stat; stdout was:\n{show_stdout}"
    );
}

// ---- AC-3: FR-7 warning fires on a fingerprint mismatch (to STDERR) -----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fr7_warning_emitted_on_fingerprint_mismatch() {
    let (_tmp, db_path) = temp_db();
    let vid = seed_version(&db_path).await;
    // A prior run recorded under a DIFFERENT (bogus) engine fingerprint.
    seed_prior_run_with_bogus_fingerprint(&db_path, &vid).await;

    let run = run_version_backtest(&db_path, vid.as_str());
    assert_success(&run, "backtest --version (mismatch)");

    let stderr = String::from_utf8(run.stderr).expect("stderr utf8");
    let stdout = String::from_utf8(run.stdout).expect("stdout utf8");

    // The FR-7 warning fires on STDERR …
    assert!(
        stderr.contains(FR7_WARNING),
        "the FR-7 mismatch warning must fire on stderr; stderr was:\n{stderr}"
    );
    // … and NEVER leaks to stdout (stdout's footer/JSON byte string is pinned, D4).
    assert!(
        !stdout.contains(FR7_WARNING),
        "the FR-7 warning must NOT leak to stdout; stdout was:\n{stdout}"
    );
}

// ---- AC-4: NO warning when the prior run's fingerprint matches ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_warning_on_fingerprint_match() {
    let (_tmp, db_path) = temp_db();
    let vid = seed_version(&db_path).await;

    // First run: creates a prior carrying the CURRENT engine fingerprint.
    let first = run_version_backtest(&db_path, vid.as_str());
    assert_success(&first, "backtest --version (first)");

    // Second run: its current fingerprint EQUALS the prior's ⇒ compare() is None
    // ⇒ no warning. (The first run has no prior, so it is also warning-free.)
    let second = run_version_backtest(&db_path, vid.as_str());
    assert_success(&second, "backtest --version (second)");
    let stderr = String::from_utf8(second.stderr).expect("stderr utf8");
    assert!(
        !stderr.contains(FR7_WARNING),
        "a matching fingerprint must emit NO FR-7 warning; stderr was:\n{stderr}"
    );
}

// ---- AC-5: silent (no warning) when there is no prior run ---------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silent_when_no_prior_run() {
    let (_tmp, db_path) = temp_db();
    let vid = seed_version(&db_path).await;

    // The very first --version run has no prior to compare against ⇒ no warning.
    let run = run_version_backtest(&db_path, vid.as_str());
    assert_success(&run, "backtest --version (no prior)");
    let stderr = String::from_utf8(run.stderr).expect("stderr utf8");
    assert!(
        !stderr.contains(FR7_WARNING),
        "the first run (no prior) must be FR-7-silent; stderr was:\n{stderr}"
    );
}

// ---- AC-6: runs list shows the persisted run ----------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_list_shows_persisted_run() {
    let (_tmp, db_path) = temp_db();
    let vid = seed_version(&db_path).await;
    let run = run_version_backtest(&db_path, vid.as_str());
    assert_success(&run, "backtest --version");

    let list = run_runs_list(&db_path, vid.as_str());
    assert_success(&list, "runs list");
    let stdout = String::from_utf8(list.stdout).expect("list stdout utf8");
    assert!(
        stdout.lines().filter(|l| l.starts_with("run\t")).count() == 1,
        "exactly one run must be listed for the version; stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("version={}", vid.as_str())),
        "the listed run must name its version; stdout was:\n{stdout}"
    );
}

// ---- AC-7: runs show renders expectancy + the persisted trade log -------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_show_renders_expectancy_and_trade_log() {
    let (_tmp, db_path) = temp_db();
    let vid = seed_version(&db_path).await;
    let run = run_version_backtest(&db_path, vid.as_str());
    assert_success(&run, "backtest --version");
    let list_stdout =
        String::from_utf8(run_runs_list(&db_path, vid.as_str()).stdout).expect("list utf8");
    let run_id = first_run_id(&list_stdout);

    let show = run_runs_show(&db_path, &run_id);
    assert_success(&show, "runs show");
    let stdout = String::from_utf8(show.stdout).expect("show stdout utf8");

    // The headline stat the user reads.
    assert!(
        stdout.contains("expectancy="),
        "`runs show` must render the expectancy; stdout was:\n{stdout}"
    );
    // The persisted trade log, via the SAME trade-row header the live backtest uses.
    assert!(
        stdout.contains("entry_time") && stdout.contains("exit_reason"),
        "`runs show` must render the persisted trade log header; stdout was:\n{stdout}"
    );
    // The headline stats block carries the win-rate / profit-factor / Sharpe cells.
    assert!(
        stdout.contains("win_rate=")
            && stdout.contains("profit_factor=")
            && stdout.contains("sharpe="),
        "`runs show` must render the headline stats; stdout was:\n{stdout}"
    );
}

// ---- AC-8: the --dsl path stays persistence-free + silent ---------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dsl_path_stays_persistence_free_and_silent() {
    let (_tmp, db_path) = temp_db();
    // Seed a version so we have a version id to query `runs list` against — but the
    // --dsl path must persist NOTHING under it.
    let vid = seed_version(&db_path).await;

    // Write the DSL file and run the --dsl path WITH a --db override present (it
    // must be ignored — the --dsl path never persists, README C7 / D1).
    let dsl_dir = TempDir::new().expect("dsl tempdir");
    let dsl_path = dsl_dir.path().join("strategy.json");
    std::fs::write(&dsl_path, MINIMAL_DSL).expect("write dsl");

    let out = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "backtest",
            "--dsl",
            dsl_path.to_str().expect("dsl path utf8"),
            "--pair",
            "BTCUSDT",
            "--tf",
            "M15",
            "--store",
            FIXTURE_STORE,
            "--db",
            &db_path,
        ])
        .output()
        .expect("run pulse backtest --dsl");
    assert_success(&out, "backtest --dsl");

    // (a) NO FR-7 warning (the --dsl path is comparison-free).
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(
        !stderr.contains(FR7_WARNING),
        "the --dsl path must be FR-7-silent; stderr was:\n{stderr}"
    );

    // (b) NO run was persisted — `runs list` for the version is empty.
    let list = run_runs_list(&db_path, vid.as_str());
    assert_success(&list, "runs list (after --dsl)");
    let list_stdout = String::from_utf8(list.stdout).expect("list utf8");
    assert!(
        !list_stdout.contains("run\t"),
        "the --dsl path must persist NO run; `runs list` was:\n{list_stdout:?}"
    );
}

// ---- AC-18: runs show reconstructs the equity curve (value equality) ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_show_reconstructs_equity_curve() {
    let (_tmp, db_path) = temp_db();
    let vid = seed_version(&db_path).await;
    let run = run_version_backtest(&db_path, vid.as_str());
    assert_success(&run, "backtest --version");
    let list_stdout =
        String::from_utf8(run_runs_list(&db_path, vid.as_str()).stdout).expect("list utf8");
    let run_id = first_run_id(&list_stdout);

    let show = run_runs_show(&db_path, &run_id);
    assert_success(&show, "runs show");
    let stdout = String::from_utf8(show.stdout).expect("show stdout utf8");

    // Parse the reconstructed equity_curve summary line + the totals line.
    let curve_line = stdout
        .lines()
        .find(|l| l.starts_with("equity_curve\t"))
        .expect("an equity_curve summary line in `runs show`");
    let totals_line = stdout
        .lines()
        .find(|l| l.starts_with("totals\t"))
        .expect("a totals line in `runs show`");

    let last_equity = cell_value(curve_line, "last_equity=");
    let starting_equity = cell_value(totals_line, "starting_equity=");
    let net_pnl = cell_value(totals_line, "net_pnl=");

    // VALUE equality (audit C4 / README C2): the DB-reconstructed curve's final
    // equity == starting_equity + net_pnl (time-independent of the leading point).
    let expected = parse_dec(&starting_equity) + parse_dec(&net_pnl);
    assert_eq!(
        parse_dec(&last_equity),
        expected,
        "reconstructed equity-curve final equity must equal starting_equity + net_pnl \
         (last={last_equity}, starting={starting_equity}, net_pnl={net_pnl})"
    );
}

/// Extract a `key=value` tab cell's VALUE from a tab-separated line.
fn cell_value(line: &str, key: &str) -> String {
    line.split('\t')
        .find_map(|c| c.strip_prefix(key))
        .unwrap_or_else(|| panic!("cell `{key}` not found in line: {line}"))
        .to_owned()
}

/// Parse a normalized-Decimal cell value back to a `Decimal` for value comparison.
fn parse_dec(s: &str) -> Decimal {
    s.parse::<Decimal>()
        .unwrap_or_else(|e| panic!("parse decimal {s:?}: {e}"))
}
