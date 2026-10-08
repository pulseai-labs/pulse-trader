//! r4.s2.w2 AC-1 (ledger d69) — the move made safe (`tests/import_move_safety.rs`).
//!
//! Four behaviours, driven through the spawned `pulse` binary (the
//! `import_restore.rs` / `server_auth.rs` patterns), plus the serve-side half of
//! the same lock:
//!
//! (i) a held instance lock on the target refuses import AND restore, and the
//!     same import succeeds once the holder exits; a second `pulse serve` on a
//!     held database refuses too (#250);
//! (ii) a source that resolves to the target is refused — the path itself, a
//!     symlink to it, and equal data dirs (#264) — with the source untouched;
//! (iii) a source `paper_event` column changed AFTER the copy is refused by the
//!     paper digest, naming the table and the first differing key;
//! (iv) a clean import prints the three paper digests and moves the rows.
//!
//! Every temporary directory is `tempfile`'s (TMPDIR-rooted, #284: never `/tmp`),
//! and every spawned binary gets `HOME` inside the test's own temp dir so no
//! default path (`~/pulse-backups` included) escapes it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::sleep;
use std::time::{Duration, Instant};

use pulse::{
    BarRef, CreatedBy, Db, NewVersion, PaperEvent, SqliteStrategyRepo, StrategyId,
    StrategyRepository, Timeframe, open_migrated,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// The RFC3339 instant every seeded row carries.
const AT: &str = "2026-01-01T00:00:00.000Z";

/// The same minimal, valid, compilable DSL the provenance/import suites seed
/// with — it produces a REAL repository-written `version_hash`, which the
/// import's repository-read check re-derives.
const MINIMAL_DSL: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold (move safety)",
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

/// The first seeded 15m bar's open time (2025-01-01T00:00:00Z, 8-hour aligned).
const BAR_0_MS: i64 = 1_735_689_600_000;
const M15_MS: i64 = 900_000;

/// How many source snapshots the digest-refusal case seeds: the copy step is
/// the window the tamper lands inside, so it must be wide (thousands of tiny
/// files ≈ seconds) against a sub-millisecond UPDATE.
const SNAPSHOTS: usize = 3_000;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Fold any write-ahead log into the database file before a byte comparison.
///
/// SQLite's checkpoint-on-close is asynchronous relative to the pool's
/// `close()` — it can land after the first read of the file, changing the main
/// file's bytes (same size, different content) and deleting the WAL. The settle
/// is therefore FORCED here rather than raced: after a `TRUNCATE` checkpoint the
/// main file holds everything and there is nothing left for a later close to
/// write.
async fn settle_wal(db_path: &Path) {
    let db = Db::with_path(db_path)
        .await
        .expect("open the source to settle its WAL");
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(db.pool())
        .await
        .expect("checkpoint the source's WAL");
    db.pool().close().await;
}

/// SHA-256 of a file, hex-encoded (the source-untouched checks).
fn sha256_file(path: &Path) -> String {
    hex::encode(Sha256::digest(fs::read(path).unwrap()))
}

fn combined(out: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Spawn the real `pulse` binary with `HOME` inside the test's temp dir (the
/// XDG overrides stripped) so every default path resolves hermetically.
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

/// The `pulse-*.db` backup files in an out-dir (never `candles/`, never a
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

/// The seeded source "Mac" host: a migrated db with one strategy/version and
/// one paper session (with `events` event rows and `bars` bar rows), plus a
/// data dir holding `snapshots` (deliberately opaque) `.parquet` files.
struct Source {
    db: PathBuf,
    data: PathBuf,
}

async fn seed_source(dir: &Path, events: i64, bars: i64, snapshots: usize) -> Source {
    fs::create_dir_all(dir).unwrap();
    let db_path = dir.join("pulse.db");
    let db = open_migrated(&db_path).await.expect("migrate the source");
    let pool = db.pool().clone();

    // The strategy and version are written through the REAL repository: the
    // import's repository-read check re-derives `version_hash`, so a hand-made
    // one would be refused for a reason this suite is not about.
    let repo = SqliteStrategyRepo::new(pool.clone());
    let strategy = repo
        .create_strategy("move-safety", None, &[])
        .await
        .expect("seed the strategy");
    let version = repo
        .create_version(NewVersion {
            strategy_id: StrategyId::new(strategy.id.as_str().to_owned()),
            parent_version_id: None,
            dsl_json: MINIMAL_DSL.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("seed the version");

    sqlx::query(
        "INSERT INTO paper_session \
         (id, seq, strategy_version_id, created_at, pair, primary_timeframe, htf_timeframe, \
          uses_d1, starting_equity, taker_fee_bps, slippage_bps, engine_fingerprint, graduation, \
          walk_forward_run_id, override_reason, override_at, certified_data_versions, fixture, \
          min_trades, promoted_by) \
         VALUES ('sess-1', 1, ?2, ?1, 'BTCUSDT', '15m', NULL, 0, '10000', '4', '1', \
                 '1111111111111111111111111111111111111111111111111111111111111111', 'override', \
                 NULL, 'move-safety seed', ?1, '[]', 0, 1, 'operator-token')",
    )
    .bind(AT)
    .bind(version.id.as_str())
    .execute(&pool)
    .await
    .unwrap();

    for seq in 1..=events {
        let payload = serde_json::to_string(&PaperEvent::BarProcessed {
            seq,
            at: AT.to_owned(),
            bars: vec![BarRef {
                timeframe: Timeframe::M15,
                open_time: BAR_0_MS + (seq - 1) * M15_MS,
            }],
        })
        .unwrap();
        sqlx::query(
            "INSERT INTO paper_event (session_id, seq, at, kind, payload) \
             VALUES ('sess-1', ?1, ?2, 'bar_processed', ?3)",
        )
        .bind(seq)
        .bind(AT)
        .bind(&payload)
        .execute(&pool)
        .await
        .unwrap();
    }
    for seq in 1..=bars {
        let open_time = BAR_0_MS + (seq - 1) * M15_MS;
        sqlx::query(
            "INSERT INTO paper_bar \
             (session_id, timeframe, seq, open_time, close_time, open, high, low, close, volume, \
              funding_rate, lead_in) \
             VALUES ('sess-1', '15m', ?1, ?2, ?3, '60000', '60100', '59900', '60050', '100', \
                     NULL, 0)",
        )
        .bind(seq)
        .bind(open_time)
        .bind(open_time + M15_MS - 1)
        .execute(&pool)
        .await
        .unwrap();
    }
    db.pool().close().await;
    settle_wal(&db_path).await;

    let store = dir.join("candles").join("BTCUSDT").join("15m");
    fs::create_dir_all(&store).unwrap();
    for i in 0..snapshots {
        fs::write(
            store.join(format!("snap-{i:05}.parquet")),
            b"opaque snapshot bytes (the digest case's copy window)",
        )
        .unwrap();
    }
    Source {
        db: db_path,
        data: dir.to_path_buf(),
    }
}

/// Drop a table's append-only trigger (the tamper these tests inject is the only
/// thing the trigger would refuse).
async fn drop_trigger(db_path: &Path, trigger: &str) {
    let holder = Db::with_path(db_path).await.unwrap();
    sqlx::query(&format!("DROP TRIGGER {trigger}"))
        .execute(holder.pool())
        .await
        .unwrap();
    holder.pool().close().await;
}

/// A spawned `pulse serve`, with its stderr drained into `lines`.
struct Serve {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Serve {
    /// Spawn `pulse serve` on `db` (the `server_auth.rs` harness shape: loopback
    /// ephemeral bind, HOME/config redirected into the test's temp dir).
    fn spawn(home: &Path, config_dir: &Path, db: &Path, data_dir: &Path) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pulse"));
        cmd.args(["serve", "--bind", "127.0.0.1:0", "--dev-loopback", "--db"])
            .arg(db)
            .arg("--data-dir")
            .arg(data_dir);
        cmd.env_remove("OLLAMA_API_KEY");
        cmd.env("PULSE_CONFIG_DIR", config_dir);
        cmd.env("HOME", home);
        cmd.env_remove("XDG_DATA_HOME");
        cmd.env_remove("XDG_CONFIG_HOME");
        cmd.env_remove("XDG_CACHE_HOME");
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn pulse serve");
        // Drain stdout so a chatty child can never block on a full pipe.
        let stdout = child.stdout.take().expect("take stdout");
        std::thread::spawn(move || {
            let mut sink = String::new();
            let _ = BufReader::new(stdout).read_to_string(&mut sink);
        });
        let stderr = child.stderr.take().expect("take stderr");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&lines);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) => collected
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(line),
                    Err(_) => break,
                }
            }
        });
        Self { child, lines }
    }

    fn output(&self) -> String {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .join("\n")
    }

    /// Poll the collected stderr lines for one containing `needle`, up to `secs`.
    fn wait_for_line(&mut self, needle: &str, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if self.output().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            match self.child.try_wait().expect("try_wait the serve child") {
                Some(_) => {
                    sleep(Duration::from_millis(200));
                    return self.output().contains(needle);
                }
                None => sleep(Duration::from_millis(50)),
            }
        }
    }

    /// Wait for the child to exit on its own (the refusal path), up to `secs`.
    fn wait_for_exit(&mut self, secs: u64) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            sleep(Duration::from_millis(50));
        }
        None
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The import's temporary database files BESIDE the target, if any: the copy
/// itself and its `-wal`/`-shm`/`-journal` sidecars. A `<tmp>.migrate.lock` is
/// NOT one — that lock file is created beside any database the migration
/// protocol opens and is deliberately never removed (it is inert, and removing
/// it would race a holder).
fn import_temporaries(target: &Path) -> Vec<PathBuf> {
    let Some(dir) = target.parent() else {
        return Vec::new();
    };
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.contains("import-tmp") && !n.ends_with(".migrate.lock"))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `paper_session` row count of a database file.
async fn paper_session_count(path: &Path) -> i64 {
    let db = Db::with_path(path)
        .await
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM paper_session")
        .fetch_one(db.pool())
        .await
        .unwrap();
    db.pool().close().await;
    count
}

// ---------------------------------------------------------------------------
// (i) the instance lock: import and restore refuse while a server holds it
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_lock_refuses_import_and_restore_and_lifts_when_the_server_exits() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let config = dir.path().join("config");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&config).unwrap();
    let src = seed_source(&dir.path().join("mac"), 2, 2, 0).await;
    let target_db = dir.path().join("server").join("pulse.db");
    let target_data = dir.path().join("server-data");
    // `pulse serve` opens the db's migration lock beside it, so the database's
    // directory must exist first (a real deployment's does).
    fs::create_dir_all(target_db.parent().unwrap()).unwrap();

    // A real backup (of the SOURCE — unrelated to the target) so the restore arm
    // is refused on a genuine restore invocation.
    let out_dir = dir.path().join("backups");
    let backup = run_pulse(
        &home,
        &[
            "backup",
            "--db",
            src.db.to_str().unwrap(),
            "--data-dir",
            src.data.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    assert!(
        backup.status.success(),
        "seed backup: {}",
        combined(&backup)
    );
    let backup_db = backup_db_files(&out_dir)
        .into_iter()
        .next()
        .expect("the backup db exists");

    // The live server holds the target's instance lock before it binds.
    let mut serve = Serve::spawn(&home, &config, &target_db, &target_data);
    assert!(
        serve.wait_for_line("listening on", 60),
        "pulse serve starts and holds the lock; lines:\n{}",
        serve.output()
    );

    // A second server on the SAME database refuses by name.
    let mut second = Serve::spawn(&home, &config, &target_db, &target_data);
    let status = second
        .wait_for_exit(60)
        .expect("the second serve exits on its own");
    let second_lines = second.output();
    assert!(
        !status.success(),
        "a second serve on a held database must refuse: {second_lines}"
    );
    assert!(
        second_lines.contains("refusing to start") && second_lines.contains("serve.lock"),
        "the refusal names the lock: {second_lines}"
    );

    // The import is refused, naming the target and the reason.
    let import = run_pulse(
        &home,
        &[
            "import",
            "--from-db",
            src.db.to_str().unwrap(),
            "--from-data-dir",
            src.data.to_str().unwrap(),
            "--db",
            target_db.to_str().unwrap(),
            "--data-dir",
            target_data.to_str().unwrap(),
        ],
    );
    let text = combined(&import);
    assert!(!import.status.success(), "import refused: {text}");
    assert!(
        text.contains(target_db.to_str().unwrap()),
        "the refusal names the target: {text}"
    );
    assert!(
        text.contains("a running pulse serve holds it"),
        "the refusal names the holder: {text}"
    );
    assert!(
        import_temporaries(&target_db).is_empty(),
        "no temporary was left beside the target"
    );
    assert_eq!(
        paper_session_count(&target_db).await,
        0,
        "the refused import installed nothing"
    );

    // The restore is refused the same way.
    let restore = run_pulse(
        &home,
        &[
            "restore",
            backup_db.to_str().unwrap(),
            "--backup-dir",
            out_dir.to_str().unwrap(),
            "--db",
            target_db.to_str().unwrap(),
            "--data-dir",
            target_data.to_str().unwrap(),
        ],
    );
    let text = combined(&restore);
    assert!(!restore.status.success(), "restore refused: {text}");
    assert!(
        text.contains("a running pulse serve holds it"),
        "the restore refusal names the holder: {text}"
    );

    // The holder exits: the SAME import now succeeds — the lock was the reason.
    serve.kill();
    let import = run_pulse(
        &home,
        &[
            "import",
            "--from-db",
            src.db.to_str().unwrap(),
            "--from-data-dir",
            src.data.to_str().unwrap(),
            "--db",
            target_db.to_str().unwrap(),
            "--data-dir",
            target_data.to_str().unwrap(),
        ],
    );
    assert!(
        import.status.success(),
        "the import succeeds once the lock is free: {}",
        combined(&import)
    );
    assert_eq!(
        paper_session_count(&target_db).await,
        1,
        "the session moved"
    );
}

// ---------------------------------------------------------------------------
// (ii) source == target (#264)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_that_is_the_target_is_refused_including_through_a_symlink() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let src = seed_source(&dir.path().join("mac"), 2, 2, 0).await;
    let before = sha256_file(&src.db);

    // The path itself.
    let out = run_pulse(
        &home,
        &[
            "import",
            "--from-db",
            src.db.to_str().unwrap(),
            "--from-data-dir",
            src.data.to_str().unwrap(),
            "--db",
            src.db.to_str().unwrap(),
            "--data-dir",
            src.data.to_str().unwrap(),
        ],
    );
    let text = combined(&out);
    assert!(!out.status.success(), "same-path import refused: {text}");
    assert!(
        text.contains("IS the target"),
        "the refusal says so: {text}"
    );
    assert_eq!(sha256_file(&src.db), before, "the source is never touched");

    // Through a symlink to the source database.
    let link = dir.path().join("link.db");
    std::os::unix::fs::symlink(&src.db, &link).unwrap();
    let out = run_pulse(
        &home,
        &[
            "import",
            "--from-db",
            src.db.to_str().unwrap(),
            "--from-data-dir",
            src.data.to_str().unwrap(),
            "--db",
            link.to_str().unwrap(),
            "--data-dir",
            dir.path().join("other-data").to_str().unwrap(),
        ],
    );
    let text = combined(&out);
    assert!(!out.status.success(), "symlink import refused: {text}");
    assert!(
        text.contains("IS the target"),
        "the refusal says so: {text}"
    );
    assert_eq!(
        sha256_file(&src.db),
        before,
        "the source is never touched through a symlink either"
    );

    // Equal data dirs, a distinct database.
    let target_db = dir.path().join("fresh").join("pulse.db");
    let out = run_pulse(
        &home,
        &[
            "import",
            "--from-db",
            src.db.to_str().unwrap(),
            "--from-data-dir",
            src.data.to_str().unwrap(),
            "--db",
            target_db.to_str().unwrap(),
            "--data-dir",
            src.data.to_str().unwrap(),
        ],
    );
    let text = combined(&out);
    assert!(!out.status.success(), "equal data dirs refused: {text}");
    assert!(
        text.contains("IS the target data dir"),
        "the refusal says so: {text}"
    );
    assert!(
        !target_db.exists(),
        "refused before anything was written: {}",
        target_db.display()
    );
}

// ---------------------------------------------------------------------------
// (iii) a source changed after the copy: the paper digest refuses
// ---------------------------------------------------------------------------

/// The temporary database the import writes beside its target, for THIS child.
fn import_tmp_for(target: &Path, pid: u32) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("pulse.db");
    target
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!(".{name}.import-tmp-{pid}.db"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_paper_event_row_after_the_copy_is_refused_by_digest() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    // `SNAPSHOTS` tiny files make the copy step (the window the tamper lands
    // inside) run for seconds against a sub-millisecond UPDATE.
    let src = seed_source(&dir.path().join("mac"), 3, 1, SNAPSHOTS).await;
    // The append-only trigger would refuse the injected change; the source is
    // this test's own file.
    drop_trigger(&src.db, "paper_event_no_update").await;
    // A warm write connection, so the tamper itself is one statement.
    let writer = Db::with_path(&src.db).await.unwrap();

    let target_db = dir.path().join("server").join("pulse.db");
    let target_data = dir.path().join("server-data");
    fs::create_dir_all(target_db.parent().unwrap()).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args(["import"])
        .args(["--from-db"])
        .arg(&src.db)
        .args(["--from-data-dir"])
        .arg(&src.data)
        .args(["--db"])
        .arg(&target_db)
        .args(["--data-dir"])
        .arg(&target_data)
        .env("HOME", &home)
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_CACHE_HOME")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the import");

    // Wait for the engine's copy to exist (its read snapshot on the source is
    // taken before this file appears), then change one source column "after the
    // copy" — the concurrent writer the digest exists to catch. The verification
    // reads happen after the snapshot copy, seconds away.
    let tmp_db = import_tmp_for(&target_db, child.id());
    let deadline = Instant::now() + Duration::from_secs(60);
    while !tmp_db.exists() && Instant::now() < deadline {
        sleep(Duration::from_millis(2));
    }
    if !tmp_db.exists() {
        let out = child.wait_with_output().expect("wait for the import");
        panic!(
            "the import never made its temporary database {}; output:\n{}",
            tmp_db.display(),
            combined(&out)
        );
    }
    // A beat to be certain the copy's own read has finished (a tiny db) — the
    // copy of the 3,000 snapshots is still running.
    sleep(Duration::from_millis(300));
    sqlx::query("UPDATE paper_event SET payload = ?1 WHERE session_id = 'sess-1' AND seq = 2")
        .bind("changed after the copy")
        .execute(writer.pool())
        .await
        .unwrap();
    writer.pool().close().await;

    let out = child.wait_with_output().expect("wait for the import");
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "the digest refuses the changed source: {text}"
    );
    assert!(
        text.contains("paper_event"),
        "the refusal names the table: {text}"
    );
    assert!(
        text.contains("session_id=sess-1") && text.contains("seq=2"),
        "the refusal names the first differing key: {text}"
    );
    assert!(
        !target_db.exists(),
        "nothing was installed: {}",
        target_db.display()
    );
    assert!(
        import_temporaries(&target_db).is_empty(),
        "and the temporary was removed"
    );
}

// ---------------------------------------------------------------------------
// (iv) a clean import prints the three paper digests
// ---------------------------------------------------------------------------

/// Whether `out` carries a 64-hex digest on the same line as `table`.
fn has_digest_for(out: &str, table: &str) -> bool {
    out.lines().any(|line| {
        line.contains(table)
            && line
                .split(|c: char| !c.is_ascii_hexdigit())
                .any(|run| run.len() == 64 && run.chars().all(|c| c.is_ascii_hexdigit()))
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clean_import_prints_the_three_paper_digests() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let src = seed_source(&dir.path().join("mac"), 3, 2, 0).await;
    let target_db = dir.path().join("server").join("pulse.db");
    let target_data = dir.path().join("server-data");

    let out = run_pulse(
        &home,
        &[
            "import",
            "--from-db",
            src.db.to_str().unwrap(),
            "--from-data-dir",
            src.data.to_str().unwrap(),
            "--db",
            target_db.to_str().unwrap(),
            "--data-dir",
            target_data.to_str().unwrap(),
        ],
    );
    let text = combined(&out);
    assert!(out.status.success(), "the clean import succeeds: {text}");
    for table in ["paper_session", "paper_event", "paper_bar"] {
        assert!(
            has_digest_for(&text, table),
            "the summary prints {table}'s digest: {text}"
        );
    }

    // The paper rows really moved, byte for byte on the hashed columns.
    let target = Db::with_path(&target_db).await.unwrap();
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT payload, at FROM paper_event ORDER BY seq")
            .fetch_all(target.pool())
            .await
            .unwrap();
    let source = Db::with_path(&src.db).await.unwrap();
    let source_rows: Vec<(String, String)> =
        sqlx::query_as("SELECT payload, at FROM paper_event ORDER BY seq")
            .fetch_all(source.pool())
            .await
            .unwrap();
    assert_eq!(rows, source_rows, "the events moved unchanged");
    target.pool().close().await;
    source.pool().close().await;
}
