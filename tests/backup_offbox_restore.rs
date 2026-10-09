//! r4.s2.w5 AC-1 (demo line d72) — the off-box backup proof: every night the
//! Mini takes a `pulse backup`; draco-desk pulls it with a key that can only
//! read the backup directory; and the pulled copy restores into a fresh
//! directory and verifies hash for hash.
//!
//! The cases, in the spec's order:
//!
//! 1. **#240.** `pulse backup --keep 0` is refused by name before anything is
//!    created — it used to prune every backup (the new one included) and then
//!    fail.
//! 2. **The round trip.** Back up a seeded database into A; run
//!    `deploy/pulse-backup-pull.sh` with A as a LOCAL source into B (the tests
//!    never contact the Mini); `pulse backup-verify B/pulse-<newest>.db`;
//!    restore B's newest into a fresh C and assert every stored version and run
//!    hash matches the source and every paper table matches by digest (w2's
//!    helpers).
//! 3. **No mirrored deletion.** Delete A's newest backup (database + manifest),
//!    pull again, and B still holds it.
//! 4. **A tampered pulled backup fails the pull's verify** — and the pull never
//!    overwrites an artifact that is already off-box (that is why the tamper is
//!    still visible).
//! 5. **Retention.** The pull keeps the newest `PULSE_OFFBOX_KEEP` databases,
//!    each with its manifest, and never prunes `candles/`.
//! 6. **The forced command, composed.** A fake `ssh` on PATH (the transport the
//!    pull builds) hands a REAL rsync sender invocation to
//!    `deploy/pulse-backup-serve.sh`, which must serve it — proving the two
//!    scripts and the whitelist compose, locally, with no Mini.
//! 7. **The manifest refusal.** `backup-verify` on a backup whose
//!    `.heads.json` is gone is refused by name.
//!
//! Every spawned run gets `HOME` inside the test's temp dir, so nothing in the
//! real home is read or written, and every command's scratch stays under the
//! item's `TMPDIR` (never `/tmp`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use pulse::{
    BacktestInputs, BacktestResult, BacktestRunRepository, CreatedBy, DataVersion,
    EngineFingerprint, EquityCurve, FundingConfig, NewVersion, PAPER_TABLES, Pair, RegimeBreakdown,
    SkippedEntryCounts, SnapshotSelection, SqliteBacktestRunRepo, SqliteStrategyRepo, StrategyId,
    StrategyRepository, SummaryStats, Timeframe, VersionId, open_migrated, open_read_only,
    paper_table_digest, stored_run_hashes, stored_version_hashes,
};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// The committed candle fixture the seeded runs point at (BTCUSDT 15m + 4h,
/// one snapshot each, HEADs included).
const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

/// The same minimal, valid, compilable DSL the import suite seeds with — it
/// produces real repository-written `version_hash`es.
const MINIMAL_DSL: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold (offbox)",
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

/// The repo's deploy script, run with `bash` (the unit does the same through
/// its shebang).
fn deploy_script(name: &str) -> PathBuf {
    manifest("deploy").join(name)
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// The one `parquet` snapshot stem of a fixture timeframe.
fn snapshot_stem(data_dir: &Path, tf_dir: &str) -> String {
    let dir = data_dir.join("candles").join("BTCUSDT").join(tf_dir);
    let mut stems: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "parquet"))
        .filter_map(|path| path.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
        .collect();
    stems.sort();
    assert_eq!(stems.len(), 1, "the fixture holds one {tf_dir} snapshot");
    stems.remove(0)
}

fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path).unwrap();
    hex::encode(Sha256::digest(&bytes))
}

/// The `pulse-*.db` files in a backup out-dir (never `candles/`, never a
/// `.partial`), sorted by name — the name order is what the retention sort and
/// "the newest" mean.
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
                                    .is_some_and(|ext| ext.eq_ignore_ascii_case("db"))
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

/// Run the pull script exactly as the unit does: `bash deploy/pulse-backup-pull.sh
/// <source> <dest>`, `HOME` inside the temp dir, plus whatever environment the
/// case needs (`PULSE_BIN`, `PULSE_OFFBOX_KEEP`, `PULSE_BACKUP_ROOT`, `PATH`).
fn run_pull(home: &Path, envs: &[(&str, &str)], source: &str, dest: &Path) -> Output {
    let mut command = Command::new("bash");
    command
        .arg(deploy_script("pulse-backup-pull.sh"))
        .arg(source)
        .arg(dest)
        .env("HOME", home)
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_CACHE_HOME");
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("spawn the pull script")
}

/// A trade-free result whose totals are all zero — `get_run`'s re-derive guard
/// reconstructs the same hash input from the stored row (the import suite's
/// shape).
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

/// The seeded "Mini" host: a migrated `pulse.db` written through the real
/// repositories, the committed fixture store as its data dir (HEADs included),
/// and a fake `HOME` for every spawned process.
struct Seeded {
    dir: TempDir,
    home: PathBuf,
    db: PathBuf,
    data: PathBuf,
}

async fn seed_source() -> Seeded {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let data = dir.path().join("mini-data");
    // The fixture root IS a data dir (it holds `candles/`), so it copies
    // straight in.
    copy_tree(&manifest(FIXTURE_STORE), &data);
    let m15 = snapshot_stem(&data, "15m");
    let h4 = snapshot_stem(&data, "4h");

    let db_path = dir.path().join("mini").join("pulse.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = open_migrated(&db_path).await.expect("migrate the source");
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let alpha = strategies
        .create_strategy("Alpha", None, &[])
        .await
        .expect("seed strategy alpha");
    let parent = strategies
        .create_version(NewVersion {
            strategy_id: StrategyId::new(alpha.id.as_str().to_owned()),
            parent_version_id: None,
            dsl_json: MINIMAL_DSL.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("seed the parent version");
    let child = strategies
        .create_version(NewVersion {
            strategy_id: StrategyId::new(alpha.id.as_str().to_owned()),
            parent_version_id: Some(VersionId::new(parent.id.as_str().to_owned())),
            dsl_json: MINIMAL_DSL.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("seed the child version");

    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let inputs = |primary: SnapshotSelection, htf: Option<SnapshotSelection>| BacktestInputs {
        pair: Pair::new("BTCUSDT"),
        primary,
        htf,
        d1: None,
        taker_fee_bps: Decimal::new(4, 0),
        slippage_bps: Decimal::new(1, 0),
        funding: FundingConfig::SnapshotRates,
        symbol_filters: None,
        window: None,
        lead_in_from_ms: None,
    };
    let primary = || SnapshotSelection {
        timeframe: Timeframe::M15,
        data_version: DataVersion::new(m15.clone()),
    };
    let htf = || SnapshotSelection {
        timeframe: Timeframe::H4,
        data_version: DataVersion::new(h4.clone()),
    };
    runs.save_run(
        &parent.id,
        &inputs(primary(), None),
        &empty_result(),
        &SummaryStats::default(),
        Decimal::new(10_000, 0),
    )
    .await
    .expect("seed run 1");
    runs.save_run(
        &child.id,
        &inputs(primary(), Some(htf())),
        &empty_result(),
        &SummaryStats::default(),
        Decimal::new(10_000, 0),
    )
    .await
    .expect("seed run 2");
    db.pool().close().await;

    Seeded {
        dir,
        home,
        db: db_path,
        data,
    }
}

fn backup(seeded: &Seeded, out_dir: &Path) -> Output {
    run_pulse(
        &seeded.home,
        &[
            "backup",
            "--db",
            seeded.db.to_str().unwrap(),
            "--data-dir",
            seeded.data.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    )
}

fn manifest_of(backup_db: &Path) -> PathBuf {
    PathBuf::from(format!("{}.heads.json", backup_db.display()))
}

// ---------------------------------------------------------------------------
// 1. #240 — `--keep 0` is refused by name, before anything is created
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_zero_is_refused_by_name_before_anything_is_created() {
    let seeded = seed_source().await;
    let out = seeded.dir.path().join("out");

    let result = run_pulse(
        &seeded.home,
        &[
            "backup",
            "--db",
            seeded.db.to_str().unwrap(),
            "--data-dir",
            seeded.data.to_str().unwrap(),
            "--out-dir",
            out.to_str().unwrap(),
            "--keep",
            "0",
        ],
    );
    let text = combined(&result);
    assert!(
        !result.status.success(),
        "#240: --keep 0 must be refused: {text}"
    );
    assert!(
        text.contains("--keep"),
        "the refusal names the flag: {text}"
    );
    assert!(
        !out.exists() || fs::read_dir(&out).unwrap().next().is_none(),
        "the refusal comes before anything is created: {text}"
    );
}

// ---------------------------------------------------------------------------
// 2. The round trip: backup, pull, verify, restore — hash for hash
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_pull_verify_and_restore_round_trip_every_hash() {
    let seeded = seed_source().await;
    let root = seeded.dir.path();
    let a = root.join("A");
    let b = root.join("B");

    // The Mini's nightly backup.
    let backed_up = backup(&seeded, &a);
    assert!(backed_up.status.success(), "{}", combined(&backed_up));
    let source_backups = backup_db_files(&a);
    assert_eq!(source_backups.len(), 1, "one backup, one database");

    // draco-desk pulls it — a LOCAL source here; the tests never contact the
    // Mini.
    let pulled = run_pull(
        &seeded.home,
        &[("PULSE_BIN", env!("CARGO_BIN_EXE_pulse"))],
        a.to_str().unwrap(),
        &b,
    );
    assert!(pulled.status.success(), "{}", combined(&pulled));
    let pulled_backups = backup_db_files(&b);
    assert_eq!(pulled_backups.len(), 1);
    assert_eq!(
        pulled_backups[0].file_name(),
        source_backups[0].file_name(),
        "the same artifact lands off-box"
    );
    assert!(
        manifest_of(&pulled_backups[0]).is_file(),
        "a backup's own manifest travels with it"
    );
    assert!(
        b.join("candles").join("BTCUSDT").join("15m").is_dir(),
        "the snapshot store travels too"
    );

    // The pull verifies; the verify is also runnable on its own.
    let verify = run_pulse(
        &seeded.home,
        &["backup-verify", pulled_backups[0].to_str().unwrap()],
    );
    assert!(verify.status.success(), "{}", combined(&verify));
    let verify_text = combined(&verify);
    assert!(
        verify_text.contains("verified"),
        "the verify reports what it checked: {verify_text}"
    );

    // The restore drill's core: a fresh directory, never a live one.
    let c_db = root.join("C").join("pulse.db");
    let c_data = root.join("C").join("data");
    let restored = run_pulse(
        &seeded.home,
        &[
            "restore",
            pulled_backups[0].to_str().unwrap(),
            "--backup-dir",
            b.to_str().unwrap(),
            "--db",
            c_db.to_str().unwrap(),
            "--data-dir",
            c_data.to_str().unwrap(),
        ],
    );
    assert!(restored.status.success(), "{}", combined(&restored));

    // Hash for hash: every version and run, and the paper tables by digest.
    let source_pool = open_read_only(&seeded.db).await.unwrap();
    let restored_pool = open_read_only(&c_db).await.unwrap();
    let source_versions = stored_version_hashes(&source_pool).await.unwrap();
    let source_runs = stored_run_hashes(&source_pool).await.unwrap();
    assert!(
        source_versions.len() >= 2 && source_runs.len() >= 2,
        "the seed is real: {} version(s), {} run(s)",
        source_versions.len(),
        source_runs.len()
    );
    assert_eq!(
        source_versions,
        stored_version_hashes(&restored_pool).await.unwrap(),
        "every stored version hash matches"
    );
    assert_eq!(
        source_runs,
        stored_run_hashes(&restored_pool).await.unwrap(),
        "every stored run hash matches"
    );
    for (table, _keys) in PAPER_TABLES {
        let digest = paper_table_digest(&source_pool, &restored_pool, table)
            .await
            .unwrap();
        assert!(
            digest.first_difference.is_none(),
            "paper table {table} differs: {digest:?}"
        );
        assert_eq!(digest.source_digest, digest.target_digest, "{table}");
    }
    source_pool.close().await;
    restored_pool.close().await;
}

// ---------------------------------------------------------------------------
// 3. A deletion on the Mini must not erase the off-box copy
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_deletion_does_not_erase_the_off_box_copy() {
    let seeded = seed_source().await;
    let root = seeded.dir.path();
    let a = root.join("A");
    let b = root.join("B");

    let backed_up = backup(&seeded, &a);
    assert!(backed_up.status.success(), "{}", combined(&backed_up));
    let first_pull = run_pull(
        &seeded.home,
        &[("PULSE_BIN", env!("CARGO_BIN_EXE_pulse"))],
        a.to_str().unwrap(),
        &b,
    );
    assert!(first_pull.status.success(), "{}", combined(&first_pull));
    let pulled = backup_db_files(&b);
    assert_eq!(pulled.len(), 1);
    let before = sha256_file(&pulled[0]);

    // The Mini loses the newest backup — database and manifest.
    let source_backups = backup_db_files(&a);
    fs::remove_file(&source_backups[0]).unwrap();
    fs::remove_file(manifest_of(&source_backups[0])).unwrap();

    let second_pull = run_pull(
        &seeded.home,
        &[("PULSE_BIN", env!("CARGO_BIN_EXE_pulse"))],
        a.to_str().unwrap(),
        &b,
    );
    assert!(
        second_pull.status.success(),
        "a pull with nothing new is still a successful pull: {}",
        combined(&second_pull)
    );
    let after = backup_db_files(&b);
    assert_eq!(after.len(), 1, "no mirrored deletion: {after:?}");
    assert_eq!(after[0], pulled[0], "the off-box copy is still there");
    assert_eq!(
        sha256_file(&after[0]),
        before,
        "byte for byte, the copy is untouched"
    );
}

// ---------------------------------------------------------------------------
// 4. A tampered pulled backup fails the pull's verify — and is never papered
//    over by the next pull
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tampered_pulled_backup_fails_the_pulls_verify() {
    let seeded = seed_source().await;
    let root = seeded.dir.path();
    let a = root.join("A");
    let b = root.join("B");

    let backed_up = backup(&seeded, &a);
    assert!(backed_up.status.success(), "{}", combined(&backed_up));
    let first_pull = run_pull(
        &seeded.home,
        &[("PULSE_BIN", env!("CARGO_BIN_EXE_pulse"))],
        a.to_str().unwrap(),
        &b,
    );
    assert!(first_pull.status.success(), "{}", combined(&first_pull));
    let pulled = backup_db_files(&b);
    assert_eq!(pulled.len(), 1);

    // Tamper the pulled copy's SQLite header: the file is no longer a
    // database, and the verify must refuse it by name.
    let mut bytes = fs::read(&pulled[0]).unwrap();
    for byte in bytes.iter_mut().take(16) {
        *byte = 0;
    }
    fs::write(&pulled[0], &bytes).unwrap();
    let tampered = sha256_file(&pulled[0]);

    let second_pull = run_pull(
        &seeded.home,
        &[("PULSE_BIN", env!("CARGO_BIN_EXE_pulse"))],
        a.to_str().unwrap(),
        &b,
    );
    let text = combined(&second_pull);
    assert!(
        !second_pull.status.success(),
        "the tampered newest fails the pull's verify: {text}"
    );
    assert!(
        text.contains("failed verification"),
        "the failure is named: {text}"
    );
    assert_eq!(
        sha256_file(&pulled[0]),
        tampered,
        "the pull never overwrites an artifact that is already off-box"
    );

    // The Mini's own copy is still good — the corruption is the off-box copy's
    // alone, and the source of truth still verifies.
    let source_backups = backup_db_files(&a);
    let source_verify = run_pulse(
        &seeded.home,
        &["backup-verify", source_backups[0].to_str().unwrap()],
    );
    assert!(
        source_verify.status.success(),
        "{}",
        combined(&source_verify)
    );
}

// ---------------------------------------------------------------------------
// 5. Retention: the newest N stay, `candles/` is never pruned
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_keeps_the_newest_and_never_prunes_candles() {
    let seeded = seed_source().await;
    let root = seeded.dir.path();
    let a = root.join("A");
    let b = root.join("B");

    // The backups in CREATION order — three runs inside one second make
    // `pulse-<stamp>.db`, `-1`, `-2`, and a name sort does not equal that order
    // (PR-354 fix D4). Each run's new file is the one that was not there before.
    let mut created: Vec<String> = Vec::new();
    for run in 0..3 {
        let before: Vec<String> = backup_db_files(&a)
            .iter()
            .filter_map(|path| path.file_name().and_then(|n| n.to_str()).map(str::to_owned))
            .collect();
        let output = backup(&seeded, &a);
        assert!(
            output.status.success(),
            "backup {run}: {}",
            combined(&output)
        );
        let after: Vec<String> = backup_db_files(&a)
            .iter()
            .filter_map(|path| path.file_name().and_then(|n| n.to_str()).map(str::to_owned))
            .collect();
        let made = after
            .into_iter()
            .find(|name| !before.contains(name))
            .unwrap_or_else(|| panic!("backup {run} made one new file: {before:?}"));
        created.push(made);
    }
    let source_backups = backup_db_files(&a);
    assert_eq!(source_backups.len(), 3, "three nightly backups");

    let pulled = run_pull(
        &seeded.home,
        &[
            ("PULSE_BIN", env!("CARGO_BIN_EXE_pulse")),
            ("PULSE_OFFBOX_KEEP", "2"),
        ],
        a.to_str().unwrap(),
        &b,
    );
    assert!(pulled.status.success(), "{}", combined(&pulled));

    let kept = backup_db_files(&b);
    assert_eq!(kept.len(), 2, "the newest two are kept: {kept:?}");
    let mut kept_names: Vec<String> = kept
        .iter()
        .filter_map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .collect();
    kept_names.sort();
    let mut newest: Vec<String> = created[created.len() - 2..].to_vec();
    newest.sort();
    assert_eq!(
        kept_names, newest,
        "and they are the newest two by CREATION order"
    );
    let pruned = created[0].clone();
    assert!(!b.join(&pruned).exists(), "the oldest is pruned: {pruned}");
    assert!(
        !manifest_of(&b.join(&pruned)).exists(),
        "a pruned backup takes its manifest with it"
    );
    assert!(
        b.join("candles").join("BTCUSDT").join("15m").is_dir(),
        "candles/ is never pruned"
    );
    assert!(
        fs::read_dir(b.join("candles").join("BTCUSDT").join("15m"))
            .unwrap()
            .next()
            .is_some(),
        "and it still holds its snapshots"
    );
}

// ---------------------------------------------------------------------------
// 6. The forced command, composed: the pull's ssh branch through the serve
//    script, over a real rsync protocol
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pull_serves_through_the_forced_command_transport() {
    let seeded = seed_source().await;
    let root = seeded.dir.path();
    let a = root.join("A");
    let b = root.join("B");

    let backed_up = backup(&seeded, &a);
    assert!(backed_up.status.success(), "{}", combined(&backed_up));

    // The Mini's side: the backup directory the forced command serves.
    let mini_root = root.join("mini-root");
    copy_tree(&a, &mini_root);

    // The fake `ssh` the pull's `-e` resolves: it takes the host and forwards
    // everything after it as SSH_ORIGINAL_COMMAND to the forced command, the
    // way sshd would — options first, exactly as the pull builds them.
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let ssh_log = root.join("ssh-args.log");
    let fake_ssh = bin.join("ssh");
    fs::write(
        &fake_ssh,
        format!(
            "#!/usr/bin/env bash\n\
             set -euo pipefail\n\
             printf '%s\\n' \"$@\" >> \"{log}\"\n\
             while [ \"$#\" -gt 0 ]; do\n\
               case \"$1\" in\n\
                 -i|-o) shift 2 ;;\n\
                 -*) shift ;;\n\
                 *) break ;;\n\
               esac\n\
             done\n\
             host=\"$1\"\n\
             shift\n\
             SSH_ORIGINAL_COMMAND=\"$*\" exec bash \"{serve}\"\n",
            log = ssh_log.display(),
            serve = deploy_script("pulse-backup-serve.sh").display(),
        ),
    )
    .unwrap();
    // Executable, or rsync's PATH search skips it and reaches the REAL ssh —
    // which would contact the Mini. This test never leaves the host.
    let mut permissions = fs::metadata(&fake_ssh).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_ssh, permissions).unwrap();

    let path = format!(
        "{}:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        bin.display()
    );
    let remote = format!("macmini:{}", mini_root.display());
    let pulled = run_pull(
        &seeded.home,
        &[
            ("PULSE_BIN", env!("CARGO_BIN_EXE_pulse")),
            ("PULSE_BACKUP_ROOT", mini_root.to_str().unwrap()),
            ("PATH", &path),
        ],
        &remote,
        &b,
    );
    assert!(pulled.status.success(), "{}", combined(&pulled));
    assert!(
        ssh_log.is_file(),
        "the fake ssh was not used — the pull must never reach a real host"
    );

    // The transport carried the sender invocation rooted at the backup dir,
    // with the dedicated key — what the Mini's authorized_keys will force.
    let logged = fs::read_to_string(&ssh_log).unwrap();
    for needle in [
        "macmini",
        "rsync",
        "--server",
        "--sender",
        "pulse_backup_ed25519",
    ] {
        assert!(
            logged.contains(needle),
            "ssh args missing {needle}: {logged}"
        );
    }

    // And what landed is the same artifact, verified by the same command.
    let pulled_backups = backup_db_files(&b);
    assert_eq!(pulled_backups.len(), 1);
    let verify = run_pulse(
        &seeded.home,
        &["backup-verify", pulled_backups[0].to_str().unwrap()],
    );
    assert!(verify.status.success(), "{}", combined(&verify));
}

// ---------------------------------------------------------------------------
// 7. A backup without its manifest is refused by name
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_verify_refuses_a_backup_without_its_manifest() {
    let seeded = seed_source().await;
    let a = seeded.dir.path().join("A");

    let backed_up = backup(&seeded, &a);
    assert!(backed_up.status.success(), "{}", combined(&backed_up));
    let backups = backup_db_files(&a);
    assert_eq!(backups.len(), 1);
    fs::remove_file(manifest_of(&backups[0])).unwrap();

    let verify = run_pulse(
        &seeded.home,
        &["backup-verify", backups[0].to_str().unwrap()],
    );
    let text = combined(&verify);
    assert!(
        !verify.status.success(),
        "a manifest-less backup is refused: {text}"
    );
    assert!(text.contains("manifest"), "by name: {text}");
}

// ---------------------------------------------------------------------------
// 8. Same-second backups sort by their numeric suffix (PR-354 fix D4)
// ---------------------------------------------------------------------------

/// PR-354 fix D4: same-second backups are `pulse-<stamp>.db`,
/// `pulse-<stamp>-1.db`, `pulse-<stamp>-2.db`, … — the order the writer creates
/// them in. A plain name sort put every suffixed file before the unsuffixed
/// first and `-10` before `-2`, so NEWEST (the file the pull verifies) and the
/// prune picked the wrong files. The sort is now (stamp, numeric suffix, no
/// suffix = 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_second_backups_are_ordered_by_their_numeric_suffix() {
    let seeded = seed_source().await;
    let root = seeded.dir.path();
    let a = root.join("A");
    let b = root.join("B");

    // One real backup (a valid database plus its manifest), then the family the
    // writer makes inside one second: unsuffixed, then -1, -2, -10.
    let made = backup(&seeded, &a);
    assert!(made.status.success(), "{}", combined(&made));
    let template = backup_db_files(&a).into_iter().next().expect("one backup");
    let bytes = fs::read(&template).unwrap();
    let manifest = fs::read(manifest_of(&template)).unwrap();
    fs::remove_file(&template).unwrap();
    fs::remove_file(manifest_of(&template)).unwrap();

    let stamp = "20261009T133000Z";
    let mut names = vec![format!("pulse-{stamp}.db")];
    for suffix in [1, 2, 10] {
        names.push(format!("pulse-{stamp}-{suffix}.db"));
    }
    // An older stamp, so the stamp key is exercised too.
    names.push("pulse-20250101T000000Z.db".to_owned());
    for name in &names {
        fs::write(a.join(name), &bytes).unwrap();
        fs::write(manifest_of(&a.join(name)), &manifest).unwrap();
    }

    let pulled = run_pull(
        &seeded.home,
        &[
            ("PULSE_BIN", env!("CARGO_BIN_EXE_pulse")),
            ("PULSE_OFFBOX_KEEP", "2"),
        ],
        a.to_str().unwrap(),
        &b,
    );
    assert!(pulled.status.success(), "{}", combined(&pulled));

    let mut kept: Vec<String> = backup_db_files(&b)
        .iter()
        .filter_map(|path| path.file_name().and_then(|n| n.to_str()).map(str::to_owned))
        .collect();
    kept.sort();
    let mut expected = vec![
        format!("pulse-{stamp}-2.db"),
        format!("pulse-{stamp}-10.db"),
    ];
    expected.sort();
    assert_eq!(
        kept, expected,
        "the two newest are -2 and -10, not the unsuffixed and -1"
    );
    assert!(
        !b.join(format!("pulse-{stamp}-1.db")).exists(),
        "the older same-second backups were pruned"
    );
    assert!(
        !b.join(format!("pulse-{stamp}.db")).exists(),
        "and the unsuffixed one (suffix 0) is the oldest of its second"
    );
    assert!(
        !b.join("pulse-20250101T000000Z.db").exists(),
        "and the older stamp is gone"
    );
}
