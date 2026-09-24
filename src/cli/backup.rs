//! `pulse backup` / `pulse restore` — D12's nightly online backup and the
//! verified restore (r3.s3.w4, ADR-0026).
//!
//! `pulse backup` takes an online, consistent copy of the live database
//! (SQLite's `VACUUM INTO` from a read-only open — readers never block a
//! writer in WAL) into `<out-dir>/pulse-<UTC yyyymmddThhmmssZ>.db`, copies
//! every snapshot not already present into `<out-dir>/candles/` (one shared
//! store; snapshots are immutable), prunes the oldest `pulse-*.db` beyond
//! `--keep` (never touching `candles/`), and prints one summary line with the
//! path, size and counts. Any error exits non-zero and deletes the partial
//! file.
//!
//! `pulse restore` runs the SAME verification as import (steps 3–5 of the
//! import contract) on the backup into a temporary file beside the target,
//! refuses a non-empty target without `--replace` (backing the target up
//! first when it replaces), then performs the same atomic rename and prints a
//! summary. Its precondition — the server must be stopped — is stated in
//! `--help`; the `just restore` recipe stops the unit, restores, then starts
//! it again.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use chrono::Utc;
use clap::Args;

use super::import::{
    VerifiedCopy, default_backup_out_dir, resolve_target_data_dir, resolve_target_db,
    run_verified_copy, scan_snapshots,
};
use crate::adapters::db::ops;
use crate::adapters::store::CandleStore;

/// `pulse backup [--db <db>] [--data-dir <dir>] [--out-dir <dir>]
/// [--keep <n>]`.
#[derive(Debug, Args)]
pub struct BackupArgs {
    /// The database to back up. Defaults to the server's platform default.
    #[arg(long)]
    pub db: Option<PathBuf>,
    /// The data dir whose candle snapshots back up too. Defaults to the
    /// server's platform default.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// The backup output directory. Defaults to `~/pulse-backups`.
    #[arg(long)]
    pub out_dir: Option<PathBuf>,
    /// How many `pulse-*.db` files to keep (the oldest are pruned; `candles/`
    /// is one shared store and is never pruned).
    #[arg(long, default_value_t = 14)]
    pub keep: u32,
}

/// `pulse restore <backup.db> --backup-dir <dir holding its candles/>
/// [--db <target>] [--data-dir <target>] --replace`.
///
/// **Precondition: the server must be stopped.** Restore does not check the
/// process; the `just restore` recipe stops `pulse-serve`, restores, then
/// starts it again.
#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// The backup database file (a `pulse-<stamp>.db` from an out-dir).
    pub backup_db: PathBuf,
    /// The directory holding the backup's candle store (its `candles/`).
    #[arg(long)]
    pub backup_dir: PathBuf,
    /// The target database. Defaults to the server's platform default.
    #[arg(long)]
    pub db: Option<PathBuf>,
    /// The target data dir. Defaults to the server's platform default.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Required to overwrite a non-empty target; the target is backed up
    /// first.
    #[arg(long, default_value_t = false)]
    pub replace: bool,
}

/// Run the backup (the composition root's thin wrapper).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on any failure; the partial file is deleted.
pub(crate) async fn run_backup(args: &BackupArgs) -> anyhow::Result<()> {
    let db_path = resolve_target_db(args.db.as_ref())?;
    let data_dir = resolve_target_data_dir(args.data_dir.as_ref())?;
    let out_dir = match &args.out_dir {
        Some(dir) => dir.clone(),
        None => default_backup_out_dir()?,
    };

    // The snapshot scan comes FIRST: a broken store must leave no backup
    // files behind at all.
    let (snapshots, scan_issues) = scan_snapshots(&data_dir);
    if !scan_issues.is_empty() {
        for issue in &scan_issues {
            eprintln!(
                "  mismatch: {} id={} field={}: {}",
                issue.table, issue.id, issue.field, issue.detail
            );
        }
        anyhow::bail!("backup: the data dir has snapshot-layout problems; nothing was backed up");
    }

    let backup_path = backup_database(&db_path, &out_dir).await?;

    // The snapshots: copy every snapshot not already present into
    // `<out-dir>/candles/` (one shared store; snapshots are immutable).
    let out_store = CandleStore::with_base_dir(out_dir.clone());
    let mut copied = 0usize;
    let mut added: Vec<PathBuf> = Vec::new();
    for snap in &snapshots {
        let dest = out_store.snapshot_path(&snap.pair, snap.timeframe, &snap.version);
        if dest.exists() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| anyhow!("create snapshot directory {}: {e}", parent.display()))?;
        }
        if let Err(e) = fs::copy(&snap.path, &dest) {
            let _ = fs::remove_file(&backup_path);
            for path in &added {
                let _ = fs::remove_file(path);
            }
            return Err(anyhow!(
                "copy snapshot {} -> {}: {e}",
                snap.path.display(),
                dest.display()
            ));
        }
        added.push(dest);
        copied += 1;
    }

    let kept = prune_backups(&out_dir, args.keep)?;
    let size = fs::metadata(&backup_path)
        .map(|m| m.len())
        .map_err(|e| anyhow!("stat {}: {e}", backup_path.display()))?;
    println!(
        "pulse backup: {} ({} bytes); snapshots copied {copied} (store {}); backups kept {kept}",
        backup_path.display(),
        size,
        snapshots.len()
    );
    Ok(())
}

/// The one online-copy primitive — shared by `pulse backup`, import's
/// `--replace` safety backup and restore's `--replace` safety backup. Writes
/// a `.partial` file, then renames it to its `pulse-<stamp>.db` name; on any
/// error the partial file is deleted.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] when the source cannot be read or the copy
/// cannot be written.
pub(crate) async fn backup_database(db_path: &Path, out_dir: &Path) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(out_dir)
        .map_err(|e| anyhow!("create backup dir {}: {e}", out_dir.display()))?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let final_path = unique_backup_path(out_dir, &stamp);
    let partial = out_dir.join(format!(
        "{}.partial",
        final_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("unnameable backup path in {}", out_dir.display()))?
    ));
    let source = ops::open_read_only(db_path)
        .await
        .map_err(|e| anyhow!("backing up {}: {e}", db_path.display()))?;
    let copied = ops::vacuum_into_copy(&source, &partial).await;
    source.close().await;
    if let Err(e) = copied {
        let _ = fs::remove_file(&partial); // a partial file is deleted
        return Err(anyhow!(
            "the backup copy of {} failed: {e}",
            db_path.display()
        ));
    }
    fs::rename(&partial, &final_path)
        .map_err(|e| anyhow!("publish {}: {e}", final_path.display()))?;
    if let Ok(handle) = fs::File::open(out_dir) {
        let _ = handle.sync_all();
    }
    Ok(final_path)
}

/// `pulse-<UTC stamp>.db`, with a `-N` suffix probed on a same-second
/// collision (the migration protocol's `backup_path` convention).
fn unique_backup_path(out_dir: &Path, stamp: &str) -> PathBuf {
    let base = out_dir.join(format!("pulse-{stamp}.db"));
    if !base.exists() {
        return base;
    }
    for n in 1u32.. {
        let candidate = out_dir.join(format!("pulse-{stamp}-{n}.db"));
        if !candidate.exists() {
            return candidate;
        }
    }
    base // unreachable in practice
}

/// Remove the oldest `pulse-*.db` beyond `keep` (never touching `candles/`),
/// returning how many remain. The stamp names sort chronologically; a
/// same-second `-N` collision keeps its arbitrary but stable order, and every
/// older second still sorts first.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] when the directory cannot be read or a prune
/// removal fails.
fn prune_backups(out_dir: &Path, keep: u32) -> anyhow::Result<usize> {
    let mut backups: Vec<PathBuf> = fs::read_dir(out_dir)
        .map_err(|e| anyhow!("read backup dir {}: {e}", out_dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with("pulse-")
                        && Path::new(n)
                            .extension()
                            .is_some_and(|e| e.eq_ignore_ascii_case("db"))
                })
        })
        .collect();
    backups.sort();
    let keep = keep as usize;
    let pruned = backups.len().saturating_sub(keep);
    for victim in &backups[..pruned] {
        fs::remove_file(victim).map_err(|e| anyhow!("prune {}: {e}", victim.display()))?;
    }
    Ok(backups.len() - pruned)
}

/// Run the restore: the same verified copy as import, with the backup as the
/// source and no chmod (a backup file is not write-protected; the Mac
/// original is an import concern).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on any refusal or failure; the target is left
/// exactly as it was.
pub(crate) async fn run_restore(args: &RestoreArgs) -> anyhow::Result<()> {
    let db_target = resolve_target_db(args.db.as_ref())?;
    let data_target = resolve_target_data_dir(args.data_dir.as_ref())?;
    run_verified_copy(VerifiedCopy {
        source_label: "restore",
        from_db: &args.backup_db,
        from_data_dir: &args.backup_dir,
        db_target: &db_target,
        data_target: &data_target,
        replace: args.replace,
        chmod_source: false,
    })
    .await
}
