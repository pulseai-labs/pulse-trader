//! r3.s3.w4 AC-1 (ledger d32) — the verified import / backup / restore proof
//! (`tests/import_restore.rs`).
//!
//! Builds a source "Mac" database seeded through the REAL repositories (two
//! strategies, a parent + a child version, runs over the committed BTCUSDT
//! fixture snapshots, one hashed client token) pinned at the embedded schema —
//! no fabricated old-schema file — then drives the spawned `pulse` binary
//! (the `server_auth.rs` pattern) through the nine required groups:
//! (i) a clean import; (ii) a flipped snapshot byte refused with the target
//! byte-identical; (iii) a tampered stored `version_hash` refused, naming the
//! table and id; (iv) a run referencing a missing snapshot refused, naming the
//! `data_version`; (v) the non-empty-target refusal and the `--replace`
//! backup-first path; (vi) backup then restore into a fresh target; (vii)
//! `--keep` pruning; (viii) a backup taken while a write transaction is in
//! flight; (ix) no token or credential material in any output. Plus: the
//! `pulse restore --help` precondition line.
//!
//! Every spawned run gets `HOME` pointed inside the test's temp dir, so the
//! `~/pulse-backups` default (used by `--replace`'s safety backup) stays
//! hermetic and never touches the operator's real home.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread::sleep;
use std::time::Duration;

use pulse::{
    BacktestInputs, BacktestResult, BacktestRunRepository, CreatedBy, DataVersion, Db,
    EngineFingerprint, EquityCurve, FundingConfig, NewVersion, Pair, RegimeBreakdown,
    SkippedEntryCounts, SnapshotSelection, SqliteBacktestRunRepo, SqliteStrategyRepo, StrategyId,
    StrategyRepository, SummaryStats, Timeframe, VersionId, hash_token, mint_token, open_migrated,
};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// The committed candle fixture the seeded runs point at (the provenance
/// suite's anchor store: BTCUSDT 15m + 4h, one snapshot each).
const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

/// The same minimal, valid, compilable DSL the provenance suite seeds with —
/// it produces real repository-written `version_hash`es (never hand-made ones).
const MINIMAL_DSL: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold (import)",
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

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn manifest(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// Recursively copy a directory tree (the fixture store, so a test may tamper
/// with its copy without touching the committed one).
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst);
        } else {
            fs::copy(&src, &dst).unwrap();
        }
    }
}

/// The parquet file stems (one per snapshot) under `<data>/candles/<pair>/<tf>/`.
fn snapshot_stems(data_dir: &Path, tf_dir: &str) -> Vec<String> {
    let mut stems: Vec<String> =
        fs::read_dir(data_dir.join("candles").join("BTCUSDT").join(tf_dir))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("parquet"))
            .map(|e| e.path().file_stem().unwrap().to_string_lossy().to_string())
            .collect();
    stems.sort();
    stems
}

/// Sorted `relative path -> length` listing of a data dir — the "listing of
/// its data dir" the flipped-byte group compares before and after.
fn data_listing(data_dir: &Path) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    let mut stack = vec![data_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p
                    .strip_prefix(data_dir)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                out.insert(rel, entry.metadata().unwrap().len());
            }
        }
    }
    out
}

fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path).unwrap();
    hex::encode(Sha256::digest(&bytes))
}

/// Spawn the real `pulse` binary with `HOME` inside the test's temp dir (and
/// the XDG overrides stripped) so every default path resolves hermetically.
fn run_pulse(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_CACHE_HOME")
        .output()
        .expect("spawn the pulse binary")
}

/// The `pulse-*.db` files in a backup out-dir (never `candles/`, never a
/// `.partial`), sorted by name.
fn backup_db_files(out_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(out_dir)
        .map(|dir| {
            dir.flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_file()
                        && path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                            n.starts_with("pulse-")
                                && Path::new(n)
                                    .extension()
                                    .is_some_and(|e| e.eq_ignore_ascii_case("db"))
                        })
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn combined(out: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A trade-free result whose totals are all zero — `get_run`'s re-derive guard
/// reconstructs the same hash input from the stored row (the provenance
/// suite's shape).
fn empty_result() -> BacktestResult {
    BacktestResult {
        trades: vec![],
        net_pnl: Decimal::ZERO,
        fees_total: Decimal::ZERO,
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        regime_breakdown: RegimeBreakdown::new(),
        skipped_entries: SkippedEntryCounts::new(),
        open_position: None,
        engine_fingerprint: EngineFingerprint::current(),
        summary: SummaryStats::default(),
        equity_curve: EquityCurve::default(),
    }
}

/// The seeded source "Mac" host: a migrated `pulse.db` written through the
/// real repositories, a candle data dir copied from the committed fixture, a
/// fake `HOME` for the spawned binary, and the seeded identities a test needs.
struct Seeded {
    dir: TempDir,
    home: PathBuf,
    from_db: PathBuf,
    from_data: PathBuf,
    /// The two fixture snapshot stems: (15m primary, 4h htf).
    stems: (String, String),
    /// The child version id (its parent is the first version).
    child_version_id: String,
    token_plain: String,
    token_hash: String,
}

/// Build the source: two strategies, a parent + child version, two runs over
/// the committed fixture snapshots, one hashed client token.
async fn seed_mac_source(with_missing_ref_run: bool) -> Seeded {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let from_data = dir.path().join("mac-data");
    // The fixture root IS a data dir (it holds `candles/`), so it copies
    // straight into the Mac data dir.
    copy_tree(&manifest(FIXTURE_STORE), &from_data);
    let stems_15m = snapshot_stems(&from_data, "15m");
    let stems_4h = snapshot_stems(&from_data, "4h");
    assert_eq!(stems_15m.len(), 1, "fixture has exactly one 15m snapshot");
    assert_eq!(stems_4h.len(), 1, "fixture has exactly one 4h snapshot");
    let stems = (stems_15m[0].clone(), stems_4h[0].clone());

    let from_db = dir.path().join("mac").join("pulse.db");
    // The migration lock opens beside the db BEFORE any parent-dir creation,
    // so the nested `mac/` dir must exist first (a fresh target's parent is
    // the engine's job; a seed's is the test's).
    fs::create_dir_all(from_db.parent().unwrap()).unwrap();
    let db = open_migrated(&from_db)
        .await
        .expect("migrate the source db");
    let strat_repo = SqliteStrategyRepo::new(db.pool().clone());
    let alpha = strat_repo
        .create_strategy("Alpha", None, &[])
        .await
        .expect("seed strategy alpha");
    let _beta = strat_repo
        .create_strategy("Beta", None, &[])
        .await
        .expect("seed strategy beta");
    let parent = strat_repo
        .create_version(NewVersion {
            strategy_id: StrategyId::new(alpha.id.as_str().to_owned()),
            parent_version_id: None,
            dsl_json: MINIMAL_DSL.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("seed parent version");
    let child = strat_repo
        .create_version(NewVersion {
            strategy_id: StrategyId::new(alpha.id.as_str().to_owned()),
            parent_version_id: Some(VersionId::new(parent.id.as_str().to_owned())),
            dsl_json: MINIMAL_DSL.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("seed child version");

    let run_repo = SqliteBacktestRunRepo::new(db.pool().clone());
    let primary = || SnapshotSelection {
        timeframe: Timeframe::M15,
        data_version: DataVersion::new(stems.0.clone()),
    };
    let htf = || SnapshotSelection {
        timeframe: Timeframe::H4,
        data_version: DataVersion::new(stems.1.clone()),
    };
    let inputs = |primary: SnapshotSelection, htf: Option<SnapshotSelection>| BacktestInputs {
        pair: Pair::new("BTCUSDT"),
        primary,
        htf,
        taker_fee_bps: Decimal::new(4, 0),
        slippage_bps: Decimal::new(1, 0),
        funding: FundingConfig::SnapshotRates,
        window: None,
        lead_in_from_ms: None,
    };
    run_repo
        .save_run(
            &parent.id,
            &inputs(primary(), None),
            &empty_result(),
            &SummaryStats::default(),
            Decimal::new(10_000, 0),
        )
        .await
        .expect("seed run 1");
    run_repo
        .save_run(
            &child.id,
            &inputs(primary(), Some(htf())),
            &empty_result(),
            &SummaryStats::default(),
            Decimal::new(10_000, 0),
        )
        .await
        .expect("seed run 2");
    if with_missing_ref_run {
        let missing = SnapshotSelection {
            timeframe: Timeframe::M15,
            data_version: DataVersion::new("missing0000000000f"),
        };
        run_repo
            .save_run(
                &child.id,
                &inputs(missing, None),
                &empty_result(),
                &SummaryStats::default(),
                Decimal::new(10_000, 0),
            )
            .await
            .expect("seed the missing-reference run");
    }

    let token_plain = mint_token();
    let token_hash = hash_token(&token_plain);
    // Raw INSERT (the hashed-row shape AC-1's import copies like any other
    // table); the issue CLI/repo path is w1's surface, not this test's.
    sqlx::query(
        "INSERT INTO client_token \
         (id, label, scope, token_sha256, created_at, revoked_at, created_by, schema_version) \
         VALUES ('tok-1', 'mac-import-test', 'app', ?1, '2026-09-24T00:00:00Z', NULL, 'test', '1')",
    )
    .bind(&token_hash)
    .execute(db.pool())
    .await
    .expect("seed one hashed client token");

    drop(db);
    Seeded {
        dir,
        home,
        from_db,
        from_data,
        stems,
        child_version_id: child.id.as_str().to_owned(),
        token_plain,
        token_hash,
    }
}

/// The import command against explicit temp targets.
fn import_args(s: &Seeded, db: &Path, data: &Path, replace: bool) -> Vec<String> {
    let mut args = vec![
        "import".to_owned(),
        "--from-db".to_owned(),
        s.from_db.display().to_string(),
        "--from-data-dir".to_owned(),
        s.from_data.display().to_string(),
        "--db".to_owned(),
        db.display().to_string(),
        "--data-dir".to_owned(),
        data.display().to_string(),
    ];
    if replace {
        args.push("--replace".to_owned());
    }
    args
}

fn str_args(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// A successful baseline import into a fresh target.
fn import_once(s: &Seeded, db: &Path, data: &Path) -> Output {
    run_pulse(&s.home, &str_args(&import_args(s, db, data, false)))
}

/// The `(strategies, versions, runs)`-style per-table count comparison the
/// clean-import group asserts: every table the SOURCE has (minus the
/// `_sqlx_migrations` bookkeeping a forward migration necessarily grows and
/// the `sqlite_*` internals) must have an equal count in the copy.
async fn assert_table_counts_equal(source: &Path, copy: &Path) {
    let src = Db::with_path(source).await.unwrap();
    let dst = Db::with_path(copy).await.unwrap();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' AND name != '_sqlx_migrations' \
         ORDER BY name",
    )
    .fetch_all(src.pool())
    .await
    .unwrap();
    assert!(!tables.is_empty(), "the source has data tables");
    for table in tables {
        let sql = format!("SELECT COUNT(*) FROM \"{table}\"");
        let a: i64 = sqlx::query_scalar(&sql)
            .fetch_one(src.pool())
            .await
            .unwrap();
        let b: i64 = sqlx::query_scalar(&sql)
            .fetch_one(dst.pool())
            .await
            .unwrap();
        assert_eq!(a, b, "table {table}: copy count must equal the source");
    }
}

/// Every `strategy_version.version_hash` and `backtest_run.result_content_hash`
/// must be equal, keyed by id (the stored-hash group).
async fn assert_stored_hashes_equal(source: &Path, copy: &Path) {
    let src = Db::with_path(source).await.unwrap();
    let dst = Db::with_path(copy).await.unwrap();
    for (table, field) in [
        ("strategy_version", "version_hash"),
        ("backtest_run", "result_content_hash"),
    ] {
        let sql = format!("SELECT id, {field} FROM {table} ORDER BY id");
        let a: Vec<(String, String)> = sqlx::query_as(&sql).fetch_all(src.pool()).await.unwrap();
        let b: Vec<(String, String)> = sqlx::query_as(&sql).fetch_all(dst.pool()).await.unwrap();
        assert_eq!(a, b, "{table}.{field}: copy rows must equal the source's");
    }
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM strategy_version")
            .fetch_one(dst.pool())
            .await
            .unwrap()
            >= 2,
        "the seeded versions are present"
    );
}

// ---------------------------------------------------------------------------
// (i) clean import
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_import_verifies_counts_hashes_reads_and_readonly_source() {
    let s = seed_mac_source(false).await;
    let target_db = s.dir.path().join("server").join("pulse.db");
    let target_data = s.dir.path().join("server-data");

    let out = import_once(&s, &target_db, &target_data);
    let text = combined(&out);
    assert!(out.status.success(), "clean import must exit 0: {text}");
    assert!(
        text.contains("versions verified: 2"),
        "versions counted: {text}"
    );
    assert!(text.contains("runs verified: 2"), "runs counted: {text}");
    assert!(
        text.contains("snapshots verified: 2"),
        "both fixture snapshots counted: {text}"
    );
    assert!(
        text.contains("set read-only (a-w)") && text.contains(&s.from_db.display().to_string()),
        "the source chmod is printed: {text}"
    );
    assert!(
        text.contains("cutover-runbook step"),
        "the summary's last line names the Mac-original runbook step: {text}"
    );
    assert!(
        fs::metadata(&s.from_db).unwrap().permissions().readonly(),
        "--from-db must be left a-w"
    );
    assert!(
        text.contains(&target_db.display().to_string()),
        "the summary names the target db path: {text}"
    );

    assert_table_counts_equal(&s.from_db, &target_db).await;
    assert_stored_hashes_equal(&s.from_db, &target_db).await;

    // The copies read back through the real repositories on the target.
    let tgt = Db::with_path(&target_db).await.unwrap();
    let strats = SqliteStrategyRepo::new(tgt.pool().clone());
    let version = strats
        .get_version(&VersionId::new(s.child_version_id.clone()))
        .await
        .expect("the child version reads back through the strategy repository");
    assert!(version.is_some(), "the child version exists in the target");
    let run_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM backtest_run ORDER BY id")
        .fetch_all(tgt.pool())
        .await
        .unwrap();
    let runs = SqliteBacktestRunRepo::new(tgt.pool().clone());
    for id in &run_ids {
        runs.get_run(&pulse::BacktestRunId::new(id.clone()))
            .await
            .expect("each run reads back through the run repository (re-derived hash)");
    }
    drop(tgt);

    // The snapshots landed under the target data dir.
    for (tf, stem) in [("15m", &s.stems.0), ("4h", &s.stems.1)] {
        let snap = target_data
            .join("candles")
            .join("BTCUSDT")
            .join(tf)
            .join(format!("{stem}.parquet"));
        assert!(snap.is_file(), "snapshot landed: {}", snap.display());
    }
}

// ---------------------------------------------------------------------------
// (ii) flipped snapshot byte
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flipped_snapshot_byte_is_refused_and_target_stays_byte_identical() {
    let s = seed_mac_source(false).await;
    let target_db = s.dir.path().join("server").join("pulse.db");
    let target_data = s.dir.path().join("server-data");
    assert!(import_once(&s, &target_db, &target_data).status.success());

    let before_db = sha256_file(&target_db);
    let before_listing = data_listing(&target_data);

    // Flip one byte in the source's 15m snapshot.
    let victim = s
        .from_data
        .join("candles")
        .join("BTCUSDT")
        .join("15m")
        .join(format!("{}.parquet", s.stems.0));
    let mut bytes = fs::read(&victim).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xFF;
    fs::write(&victim, &bytes).unwrap();

    let out = run_pulse(
        &s.home,
        &str_args(&import_args(&s, &target_db, &target_data, true)),
    );
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "the flipped byte must be refused: {text}"
    );
    assert!(text.contains("REFUSED"), "the refusal is named: {text}");
    assert!(
        text.contains(&s.stems.0),
        "the refusal names the snapshot data_version: {text}"
    );
    assert_eq!(
        sha256_file(&target_db),
        before_db,
        "the target db must be byte-identical after the refusal"
    );
    assert_eq!(
        data_listing(&target_data),
        before_listing,
        "the target data dir must be byte-identical after the refusal"
    );
    // --replace still wrote its safety backup of the previous target first.
    let backups = fs::read_dir(s.home.join("pulse-backups"))
        .unwrap()
        .flatten()
        .count();
    assert!(
        backups >= 1,
        "the replace path backed the previous target up first"
    );
}

// ---------------------------------------------------------------------------
// (iii) tampered stored version_hash
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tampered_version_hash_in_source_is_refused_naming_table_and_id() {
    let s = seed_mac_source(false).await;

    // Tamper the throwaway source: the immutability trigger must be dropped to
    // forge a stored hash, then the raw UPDATE lands the wrong version_hash.
    let db = Db::with_path(&s.from_db).await.unwrap();
    sqlx::query("DROP TRIGGER strategy_version_no_update")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE strategy_version SET version_hash = 'tampered000000001' WHERE id = ?1")
        .bind(&s.child_version_id)
        .execute(db.pool())
        .await
        .unwrap();
    drop(db);

    let target_db = s.dir.path().join("server").join("pulse.db");
    let target_data = s.dir.path().join("server-data");
    let out = import_once(&s, &target_db, &target_data);
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "the tampered hash must be refused: {text}"
    );
    assert!(text.contains("REFUSED"), "the refusal is named: {text}");
    assert!(
        text.contains("strategy_version"),
        "the refusal names the table: {text}"
    );
    assert!(
        text.contains(&s.child_version_id),
        "the refusal names the id: {text}"
    );
    assert!(
        text.contains("version_hash"),
        "the refusal names the field: {text}"
    );
    assert!(
        !target_db.exists(),
        "a refused import leaves no target database behind"
    );
}

// ---------------------------------------------------------------------------
// (iv) run referencing a missing snapshot
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_referencing_missing_snapshot_is_refused_naming_the_data_version() {
    let s = seed_mac_source(true).await;
    let target_db = s.dir.path().join("server").join("pulse.db");
    let target_data = s.dir.path().join("server-data");
    let out = import_once(&s, &target_db, &target_data);
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "the missing reference must be refused: {text}"
    );
    assert!(text.contains("REFUSED"), "the refusal is named: {text}");
    assert!(
        text.contains("missing0000000000f"),
        "the refusal names the referenced data_version: {text}"
    );
}

// ---------------------------------------------------------------------------
// (v) non-empty target refusal + --replace backup-first
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonempty_target_is_refused_then_replace_backs_up_first_and_succeeds() {
    let s = seed_mac_source(false).await;
    let target_db = s.dir.path().join("server").join("pulse.db");
    let target_data = s.dir.path().join("server-data");
    assert!(import_once(&s, &target_db, &target_data).status.success());

    let refused = run_pulse(
        &s.home,
        &str_args(&import_args(&s, &target_db, &target_data, false)),
    );
    let text = combined(&refused);
    assert!(
        !refused.status.success(),
        "a non-empty target is refused: {text}"
    );
    assert!(
        text.contains("non-empty"),
        "the refusal names the state: {text}"
    );
    assert!(
        text.contains("--replace"),
        "the refusal names the way out: {text}"
    );

    let backups_dir = s.home.join("pulse-backups");
    let before = backup_db_files(&backups_dir);

    let replaced = run_pulse(
        &s.home,
        &str_args(&import_args(&s, &target_db, &target_data, true)),
    );
    let text = combined(&replaced);
    assert!(replaced.status.success(), "--replace must succeed: {text}");
    assert!(
        text.contains("backed up to"),
        "the replace path names the backup path: {text}"
    );
    let after = backup_db_files(&backups_dir);
    assert_eq!(
        after.len(),
        before.len() + 1,
        "exactly one backup database was written"
    );
    let new_backup = after.iter().find(|p| !before.contains(p)).unwrap();
    assert!(
        new_backup
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("pulse-")),
        "the backup follows the pulse-<stamp>.db convention: {}",
        new_backup.display()
    );
    // The safety backup is the SAME artifact `pulse backup` makes: the database
    // AND the target's candle snapshots, so it can be restored with
    // `pulse restore --backup-dir` — a database-only copy could not be.
    for (tf, stem) in [("15m", &s.stems.0), ("4h", &s.stems.1)] {
        let snap = backups_dir
            .join("candles")
            .join("BTCUSDT")
            .join(tf)
            .join(format!("{stem}.parquet"));
        assert!(
            snap.is_file(),
            "the --replace backup holds the target's snapshot: {}",
            snap.display()
        );
    }
    let restored_db = s.dir.path().join("restored-from-replace").join("pulse.db");
    let restored_data = s.dir.path().join("restored-from-replace-data");
    let restored = run_pulse(
        &s.home,
        &[
            "restore",
            new_backup.to_str().unwrap(),
            "--backup-dir",
            backups_dir.to_str().unwrap(),
            "--db",
            restored_db.to_str().unwrap(),
            "--data-dir",
            restored_data.to_str().unwrap(),
        ],
    );
    let text = combined(&restored);
    assert!(
        restored.status.success(),
        "the backup the --replace path named must restore: {text}"
    );
    assert_table_counts_equal(&s.from_db, &restored_db).await;
    assert_table_counts_equal(&s.from_db, &target_db).await;
}

// ---------------------------------------------------------------------------
// (vi) backup then restore
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_then_restore_roundtrips_counts_and_hashes() {
    let s = seed_mac_source(false).await;
    let target_db = s.dir.path().join("server").join("pulse.db");
    let target_data = s.dir.path().join("server-data");
    assert!(import_once(&s, &target_db, &target_data).status.success());

    let out_dir = s.dir.path().join("backups");
    let out = run_pulse(
        &s.home,
        &[
            "backup",
            "--db",
            target_db.to_str().unwrap(),
            "--data-dir",
            target_data.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    let text = combined(&out);
    assert!(out.status.success(), "backup must succeed: {text}");
    assert!(text.contains("pulse backup:"), "one summary line: {text}");
    let backup_files: Vec<PathBuf> = fs::read_dir(&out_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("pulse-")
                    && Path::new(n)
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("db"))
            })
        })
        .collect();
    assert_eq!(backup_files.len(), 1, "one backup db written");
    for (tf, stem) in [("15m", &s.stems.0), ("4h", &s.stems.1)] {
        let snap = out_dir
            .join("candles")
            .join("BTCUSDT")
            .join(tf)
            .join(format!("{stem}.parquet"));
        assert!(
            snap.is_file(),
            "the backup holds the snapshot: {}",
            snap.display()
        );
    }

    let target2 = s.dir.path().join("restored").join("pulse.db");
    let data2 = s.dir.path().join("restored-data");
    let out = run_pulse(
        &s.home,
        &[
            "restore",
            backup_files[0].to_str().unwrap(),
            "--backup-dir",
            out_dir.to_str().unwrap(),
            "--db",
            target2.to_str().unwrap(),
            "--data-dir",
            data2.to_str().unwrap(),
        ],
    );
    let text = combined(&out);
    assert!(
        out.status.success(),
        "restore into a fresh target must succeed: {text}"
    );
    assert!(
        text.contains("restore:"),
        "the restore prints its own summary: {text}"
    );

    assert_table_counts_equal(&target_db, &target2).await;
    assert_stored_hashes_equal(&target_db, &target2).await;
    assert_eq!(
        data_listing(&target_data),
        data_listing(&data2),
        "the restored data dir matches the original"
    );
}

// ---------------------------------------------------------------------------
// (vii) pruning
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pruning_keeps_three_newest_backups_and_unchanged_candles() {
    let s = seed_mac_source(false).await;
    let target_db = s.dir.path().join("server").join("pulse.db");
    let target_data = s.dir.path().join("server-data");
    assert!(import_once(&s, &target_db, &target_data).status.success());

    let out_dir = s.dir.path().join("backups");
    let candle_listing = || -> BTreeMap<String, u64> { data_listing(&out_dir.join("candles")) };
    let backup_names = || -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&out_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| {
                n.starts_with("pulse-")
                    && Path::new(n)
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("db"))
            })
            .collect();
        names.sort();
        names
    };

    let mut first_backup: Option<String> = None;
    for i in 0..5 {
        let out = run_pulse(
            &s.home,
            &[
                "backup",
                "--db",
                target_db.to_str().unwrap(),
                "--data-dir",
                target_data.to_str().unwrap(),
                "--out-dir",
                out_dir.to_str().unwrap(),
                "--keep",
                "3",
            ],
        );
        assert!(
            out.status.success(),
            "backup {i} must succeed: {}",
            combined(&out)
        );
        if i == 0 {
            first_backup = backup_names().first().cloned();
            // Distinct wall-clock seconds keep the stamp names chronological.
            sleep(Duration::from_millis(1100));
        }
    }
    let candles_after = candle_listing();
    let names = backup_names();
    assert_eq!(names.len(), 3, "exactly --keep backups remain: {names:?}");
    let first = first_backup.expect("the first backup was recorded");
    assert!(
        !names.contains(&first),
        "the oldest backup was pruned: {first} vs {names:?}"
    );
    assert_eq!(
        candles_after,
        candle_listing(),
        "pruning never touches candles/"
    );
    assert_eq!(
        candles_after.len(),
        2,
        "both fixture snapshots are retained"
    );
}

// ---------------------------------------------------------------------------
// (viii) backup under an in-flight write transaction
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_while_a_write_transaction_is_in_flight_is_consistent_and_restorable() {
    let s = seed_mac_source(false).await;
    let target_db = s.dir.path().join("server").join("pulse.db");
    let target_data = s.dir.path().join("server-data");
    assert!(import_once(&s, &target_db, &target_data).status.success());

    // A second connection holds the database with an UNCOMMITTED write in
    // flight while the backup runs — the copy must stay consistent (pre-txn).
    let holder = Db::with_path(&target_db).await.unwrap();
    let mut conn = holder.pool().acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO strategy (id, name, created_at) VALUES ('zzz-in-flight', 'in-flight', '2026-01-01T00:00:00Z')",
    )
    .execute(&mut *conn)
    .await
    .unwrap();

    let out_dir = s.dir.path().join("backups");
    let out = run_pulse(
        &s.home,
        &[
            "backup",
            "--db",
            target_db.to_str().unwrap(),
            "--data-dir",
            target_data.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    let text = combined(&out);
    assert!(
        out.status.success(),
        "the backup succeeds despite the in-flight write: {text}"
    );
    drop(conn);
    holder.pool().close().await;

    let backup: PathBuf = fs::read_dir(&out_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("pulse-")
                    && Path::new(n)
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("db"))
            })
        })
        .expect("the backup db exists");
    let target2 = s.dir.path().join("restored").join("pulse.db");
    let data2 = s.dir.path().join("restored-data");
    let out = run_pulse(
        &s.home,
        &[
            "restore",
            backup.to_str().unwrap(),
            "--backup-dir",
            out_dir.to_str().unwrap(),
            "--db",
            target2.to_str().unwrap(),
            "--data-dir",
            data2.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "the concurrent backup restores cleanly: {}",
        combined(&out)
    );
    let restored = Db::with_path(&target2).await.unwrap();
    let in_flight: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM strategy WHERE id = 'zzz-in-flight'")
            .fetch_one(restored.pool())
            .await
            .unwrap();
    assert_eq!(
        in_flight, 0,
        "the uncommitted write is absent from the copy"
    );
    let strategies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy")
        .fetch_one(restored.pool())
        .await
        .unwrap();
    assert_eq!(
        strategies, 2,
        "the committed state is exactly what was seeded"
    );
}

// ---------------------------------------------------------------------------
// (ix) no token or credential material in any output
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_token_or_credential_material_appears_in_any_output() {
    let s = seed_mac_source(false).await;
    let target_db = s.dir.path().join("server").join("pulse.db");
    let target_data = s.dir.path().join("server-data");

    let import = combined(&import_once(&s, &target_db, &target_data));
    let backup = combined(&run_pulse(
        &s.home,
        &[
            "backup",
            "--db",
            target_db.to_str().unwrap(),
            "--data-dir",
            target_data.to_str().unwrap(),
            "--out-dir",
            s.dir.path().join("backups").to_str().unwrap(),
        ],
    ));
    let target2 = s.dir.path().join("restored").join("pulse.db");
    let data2 = s.dir.path().join("restored-data");
    let restore = combined(&run_pulse(
        &s.home,
        &[
            "restore",
            s.dir
                .path()
                .join("backups")
                .join("pulse-latest.db")
                .to_str()
                .unwrap(),
            "--backup-dir",
            s.dir.path().join("backups").to_str().unwrap(),
            "--db",
            target2.to_str().unwrap(),
            "--data-dir",
            data2.to_str().unwrap(),
        ],
    ));

    for (label, text) in [
        ("import", &import),
        ("backup", &backup),
        ("restore", &restore),
    ] {
        assert!(
            !text.contains(&s.token_plain),
            "{label} output must not carry the token plaintext"
        );
        assert!(
            !text.contains(&s.token_hash),
            "{label} output must not carry the stored token hash"
        );
        assert!(
            !text.to_lowercase().contains("pt_"),
            "{label} output must not carry any token shape"
        );
    }
    // The import copied the hashed rows like any other table.
    assert_stored_hashes_equal(&s.from_db, &target_db).await;
    let tgt = Db::with_path(&target_db).await.unwrap();
    let tokens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_token")
        .fetch_one(tgt.pool())
        .await
        .unwrap();
    assert_eq!(tokens, 1, "the hashed client_token row was copied");
}

// ---------------------------------------------------------------------------
// the offline precondition, on import
// ---------------------------------------------------------------------------

/// D7's other half of restore's precondition: `pulse import` REPLACES the
/// database file (and deletes its stale `-wal`/`-shm`), so its `--help` and its
/// module doc state that the server must be stopped — the cutover order is
/// quit the old app, then import. The command still checks no process: the
/// spec deliberately has none.
#[test]
fn import_help_states_the_server_stopped_precondition() {
    let dir = TempDir::new().unwrap();
    let out = run_pulse(dir.path(), &["import", "--help"]);
    assert!(out.status.success(), "import --help must succeed");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("Precondition: the server must be stopped"),
        "the --help text states the server-stopped precondition: {help}"
    );
    assert!(
        help.contains("quit the old Mac app"),
        "and names D7's cutover order: {help}"
    );
}

// ---------------------------------------------------------------------------
// emptiness counts every application table
// ---------------------------------------------------------------------------

/// A target that holds NO strategy rows but DOES hold a token is not empty:
/// the install replaces the whole file, so importing over it without
/// `--replace` would destroy the operator's tokens (and their audit trail)
/// without a backup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_target_holding_only_token_rows_is_not_empty() {
    let s = seed_mac_source(false).await;
    let target_db = s.dir.path().join("server").join("pulse.db");
    let target_data = s.dir.path().join("server-data");
    // The server's own layout: the token command opens (and migrates) the
    // database in place, so its directory exists on a served host.
    fs::create_dir_all(target_db.parent().unwrap()).unwrap();

    // A real token in the target, through the real CLI (WAL permits the second
    // writer): the state a served host is in before its first import.
    let issued = run_pulse(
        &s.home,
        &[
            "token",
            "issue",
            "--scope",
            "agent",
            "--label",
            "laptop",
            "--db",
            target_db.to_str().unwrap(),
        ],
    );
    assert!(
        issued.status.success(),
        "token issue must succeed: {}",
        combined(&issued)
    );
    let tgt = Db::with_path(&target_db).await.unwrap();
    for table in ["strategy", "strategy_version", "backtest_run"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(tgt.pool())
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "{table} is empty, so only the token makes it used"
        );
    }

    let refused = run_pulse(
        &s.home,
        &str_args(&import_args(&s, &target_db, &target_data, false)),
    );
    let text = combined(&refused);
    assert!(
        !refused.status.success(),
        "a target holding token rows must be refused without --replace: {text}"
    );
    assert!(
        text.contains("non-empty"),
        "the refusal names the state: {text}"
    );
    assert!(
        text.contains("client_token 1"),
        "the refusal names the table that makes it non-empty: {text}"
    );

    // --replace still works, and backs the token row up with everything else.
    let replaced = run_pulse(
        &s.home,
        &str_args(&import_args(&s, &target_db, &target_data, true)),
    );
    let text = combined(&replaced);
    assert!(replaced.status.success(), "--replace must succeed: {text}");
    assert!(
        text.contains("backed up to"),
        "the replace path names the backup: {text}"
    );
    assert_table_counts_equal(&s.from_db, &target_db).await;
}

// ---------------------------------------------------------------------------
// the backup store's integrity
// ---------------------------------------------------------------------------

/// A missing `<data>/candles` root is an EMPTY snapshot store, not a broken
/// layout: a fresh install (a migrated database, no candles fetched yet) must
/// still back its database up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_of_a_fresh_install_without_candles_writes_the_database() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let db_path = dir.path().join("server").join("pulse.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let data_dir = dir.path().join("server-data"); // never created: no candles/
    open_migrated(&db_path).await.unwrap();
    assert!(!data_dir.join("candles").exists(), "the store is empty");

    let out_dir = dir.path().join("backups");
    let out = run_pulse(
        &home,
        &[
            "backup",
            "--db",
            db_path.to_str().unwrap(),
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    let text = combined(&out);
    assert!(
        out.status.success(),
        "a fresh install's backup must succeed: {text}"
    );
    assert!(
        text.contains("snapshots copied 0 (store 0)"),
        "the empty store is reported as empty: {text}"
    );
    assert_eq!(
        backup_db_files(&out_dir).len(),
        1,
        "the database was backed up"
    );
}

/// A snapshot the backup store ALREADY holds is verified, never skipped: a
/// truncated file would otherwise let every later backup report success while
/// its database references unusable bytes — and the restore would refuse that
/// backup, days after the fact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_corrupt_snapshot_already_in_the_backup_store_is_refused() {
    let s = seed_mac_source(false).await;
    let target_db = s.dir.path().join("server").join("pulse.db");
    let target_data = s.dir.path().join("server-data");
    assert!(import_once(&s, &target_db, &target_data).status.success());

    let out_dir = s.dir.path().join("backups");
    let backup_args = || {
        vec![
            "backup".to_owned(),
            "--db".to_owned(),
            target_db.display().to_string(),
            "--data-dir".to_owned(),
            target_data.display().to_string(),
            "--out-dir".to_owned(),
            out_dir.display().to_string(),
        ]
    };
    let first = run_pulse(&s.home, &str_args(&backup_args()));
    assert!(
        first.status.success(),
        "the first backup must succeed: {}",
        combined(&first)
    );
    let written = backup_db_files(&out_dir);
    assert_eq!(written.len(), 1, "one backup db");

    // Truncate the store's copy of the 15m snapshot.
    let victim = out_dir
        .join("candles")
        .join("BTCUSDT")
        .join("15m")
        .join(format!("{}.parquet", s.stems.0));
    let truncated = fs::read(&victim).unwrap();
    fs::write(&victim, &truncated[..truncated.len() / 2]).unwrap();

    let second = run_pulse(&s.home, &str_args(&backup_args()));
    let text = combined(&second);
    assert!(
        !second.status.success(),
        "a corrupt file already in the store must refuse the backup: {text}"
    );
    assert!(
        text.contains("already holds") && text.contains(&victim.display().to_string()),
        "the refusal names the snapshot: {text}"
    );
    assert_eq!(
        backup_db_files(&out_dir).len(),
        1,
        "the refusal wrote no new backup database"
    );
}

// ---------------------------------------------------------------------------
// the restore --help precondition
// ---------------------------------------------------------------------------

#[test]
fn restore_help_states_the_server_stopped_precondition() {
    let dir = TempDir::new().unwrap();
    let out = run_pulse(dir.path(), &["restore", "--help"]);
    assert!(out.status.success(), "restore --help must succeed");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("must be stopped"),
        "the --help text states the server-stopped precondition: {help}"
    );
}
