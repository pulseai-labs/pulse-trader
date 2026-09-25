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
//!
//! [`backup_target`] is the one full-backup body — database + the source data
//! dir's snapshots — that `pulse backup` and BOTH `--replace` safety backups
//! (import's and restore's) run, so a backup named by any of them restores with
//! `pulse restore --backup-dir <out-dir>`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use chrono::Utc;
use clap::Args;

use super::import::{
    HeadChange, SourceHead, SourceSnapshot, VerifiedCopy, bytes_equal, copy_snapshot_into,
    default_backup_out_dir, resolve_target_data_dir, resolve_target_db, restore_heads,
    run_verified_copy, scan_heads, scan_snapshots,
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

    let outcome = backup_target(&db_path, &data_dir, &out_dir).await?;
    let kept = prune_backups(&out_dir, args.keep)?;
    let size = fs::metadata(&outcome.path)
        .map(|m| m.len())
        .map_err(|e| anyhow!("stat {}: {e}", outcome.path.display()))?;
    println!(
        "pulse backup: {} ({} bytes); snapshots copied {} (store {}); backups kept {kept}",
        outcome.path.display(),
        size,
        outcome.snapshots_copied,
        outcome.snapshots_total
    );
    Ok(())
}

/// What a full backup wrote.
pub(crate) struct BackupOutcome {
    /// The published `pulse-<stamp>.db`.
    pub path: PathBuf,
    /// How many snapshots this run ADDED to the out-dir's shared store.
    pub snapshots_copied: usize,
    /// How many snapshots the source store holds in total.
    pub snapshots_total: usize,
}

/// The one full-backup body: the database copy plus every snapshot of
/// `data_dir` into `out_dir`'s shared `candles/` store — the artifact set
/// `pulse backup` makes, without its prune and its summary line.
///
/// Shared by `pulse backup` itself and the `--replace` safety backup of import
/// and restore, so a backup named by any of them can be restored with
/// `pulse restore --backup-dir <out-dir>`.
///
/// # Errors
///
/// A snapshot-layout problem in the source store (nothing is written at all),
/// a snapshot the store already holds with different bytes (refused, naming
/// it), or a failed database/snapshot copy — the partial database file and the
/// snapshots this call added are deleted.
pub(crate) async fn backup_target(
    db_path: &Path,
    data_dir: &Path,
    out_dir: &Path,
) -> anyhow::Result<BackupOutcome> {
    let out_store = CandleStore::with_base_dir(out_dir.to_path_buf());

    // ---- Step 1: freeze the DATABASE first, and derive the snapshot set from
    // THAT file (`publish_source_store` below). A scan taken before the copy
    // would miss a snapshot published — and referenced by a run committed — in
    // between: the backup would report success while the database it publishes
    // names a file the copy set omitted, and `pulse restore` would refuse that
    // backup later.
    let backup_path = backup_database(db_path, out_dir).await?;
    publish_source_store(&out_store, data_dir, &backup_path).await
}

/// Publish the source store into `<out-dir>/candles/` (one shared store):
/// every snapshot the FROZEN COPY references, plus the `HEAD` pointers that
/// name them (the same rule the import's copy applies, so a restored backup
/// lands a store whose current pointer is the one it was taken with).
///
/// The order is the contract:
///
/// 1. the references come from the frozen copy, never from a pre-scan;
/// 2. every source snapshot must verify (the store's own integrity read) — a
///    backup must not report success over a store the restore would refuse;
/// 3. every reference must resolve to a source snapshot, or the backup FAILS:
///    publishing a database whose snapshot is missing would hand the restore a
///    set it can only refuse;
/// 4. only then are the files copied, and only then are the pointers written.
///
/// A failure deletes the partial database file, every snapshot this call added
/// and every pointer it moved, so no half backup survives — and each copy
/// publishes through `copy_snapshot_into`, so a copy that fails part-way leaves
/// no truncated file under a snapshot's name.
async fn publish_source_store(
    out_store: &CandleStore,
    data_dir: &Path,
    backup_path: &Path,
) -> anyhow::Result<BackupOutcome> {
    let source_store = CandleStore::with_base_dir(data_dir.to_path_buf());
    let mut added: Vec<PathBuf> = Vec::new();

    let published =
        publish_store_files(out_store, &source_store, data_dir, backup_path, &mut added).await;
    match published {
        Ok((copied, total)) => Ok(BackupOutcome {
            path: backup_path.to_path_buf(),
            snapshots_copied: copied,
            snapshots_total: total,
        }),
        Err(error) => {
            discard(backup_path, &added);
            Err(error)
        }
    }
}

/// The publishing body (see [`publish_source_store`] for the order it keeps),
/// returning how many snapshots were added and how many the source holds.
async fn publish_store_files(
    out_store: &CandleStore,
    source_store: &CandleStore,
    data_dir: &Path,
    backup_path: &Path,
    added: &mut Vec<PathBuf>,
) -> anyhow::Result<(usize, usize)> {
    // 1. What the FROZEN COPY needs: the snapshots its runs name.
    let references = frozen_references(backup_path).await?;
    // 2. What the source store holds (layout problems refuse: nothing written).
    let (snapshots, heads) = source_store_contents(data_dir)?;
    // 3. Every source snapshot must VERIFY before this backup reports success.
    verify_source_snapshots(source_store, &snapshots)?;
    // 4. Every reference must resolve to a source snapshot.
    let wanted = resolve_copy_set(&snapshots, &heads, &references)?;
    // 5. What the store already holds is verified, never skipped.
    verify_store_bytes(out_store, &snapshots, &wanted)?;
    // 6. Copy, then publish the pointers that name what was copied.
    let copied = copy_wanted(out_store, &snapshots, &wanted, added)?;
    match publish_heads(out_store, &heads) {
        Ok(()) => Ok((copied, snapshots.len())),
        Err((error, changes)) => {
            restore_heads(out_store, &changes);
            Err(error)
        }
    }
}

/// The snapshots the FROZEN COPY references — read from the copy itself, never
/// from a scan taken before it: a snapshot published and referenced in between
/// would otherwise be missing from the copy set while the database the backup
/// publishes names it, and `pulse restore` would refuse that backup later.
async fn frozen_references(backup_path: &Path) -> anyhow::Result<Vec<ops::SnapshotRef>> {
    let copy_pool = ops::open_read_only(backup_path)
        .await
        .map_err(|e| anyhow!("read the frozen copy {}: {e}", backup_path.display()))?;
    let referenced = ops::referenced_snapshots(&copy_pool).await;
    copy_pool.close().await;
    referenced.map_err(|e| anyhow!("{e}"))
}

/// What the source store holds: its snapshots and its `HEAD` pointers. Layout
/// problems in either refuse the backup before anything is written.
fn source_store_contents(
    data_dir: &Path,
) -> anyhow::Result<(Vec<SourceSnapshot>, Vec<SourceHead>)> {
    let (snapshots, scan_issues) = scan_snapshots(data_dir);
    let (heads, head_issues) = scan_heads(data_dir);
    let issues: Vec<&super::import::Mismatch> =
        scan_issues.iter().chain(head_issues.iter()).collect();
    if !issues.is_empty() {
        for issue in &issues {
            eprintln!(
                "  mismatch: {} id={} field={}: {}",
                issue.table, issue.id, issue.field, issue.detail
            );
        }
        anyhow::bail!(
            "backup: the data dir has snapshot-layout or HEAD-pointer problems; nothing was \
             backed up"
        );
    }
    Ok((snapshots, heads))
}

/// Every source snapshot must VERIFY before this backup reports success: bit
/// rot, or a snapshot swapped in under an old name, would otherwise be
/// published under a success line while `pulse restore` runs the same integrity
/// step later and refuses the very artifact the backup named.
fn verify_source_snapshots(
    source_store: &CandleStore,
    snapshots: &[SourceSnapshot],
) -> anyhow::Result<()> {
    for snap in snapshots {
        if let Err(error) = source_store.read_snapshot(&snap.pair, snap.timeframe, &snap.version) {
            anyhow::bail!(
                "backup: the source snapshot {} does not verify ({error}); nothing was backed up",
                snap.path.display()
            );
        }
    }
    Ok(())
}

/// The snapshots the backup must publish, as indices into `snapshots`: every
/// one the frozen copy references, plus every one a `HEAD` pointer names (a
/// pointer travels with its snapshot, and a pointer whose snapshot is missing
/// fails the backup rather than publishing a set only the restore can refuse).
fn resolve_copy_set(
    snapshots: &[SourceSnapshot],
    heads: &[SourceHead],
    references: &[ops::SnapshotRef],
) -> anyhow::Result<Vec<usize>> {
    let find = |pair: &str, timeframe: &str, version: &str| -> Option<usize> {
        snapshots.iter().position(|snap| {
            snap.pair.as_str() == pair
                && snap.timeframe.binance_interval() == timeframe
                && snap.version.to_string() == version
        })
    };
    let mut wanted: Vec<usize> = Vec::new();
    for reference in references {
        let Some(index) = find(
            &reference.pair,
            &reference.timeframe,
            &reference.data_version,
        ) else {
            anyhow::bail!(
                "backup: the frozen copy references {} {} {} and the data dir does not hold it; \
                 nothing was backed up",
                reference.pair,
                reference.timeframe,
                reference.data_version
            );
        };
        if !wanted.contains(&index) {
            wanted.push(index);
        }
    }
    for head in heads {
        let Some(index) = snapshots.iter().position(|snap| {
            snap.pair == head.pair
                && snap.timeframe == head.timeframe
                && snap.version == head.version
        }) else {
            anyhow::bail!(
                "backup: the HEAD pointer {} names {} and the data dir does not hold it; \
                 nothing was backed up",
                head.path.display(),
                head.version
            );
        };
        if !wanted.contains(&index) {
            wanted.push(index);
        }
    }
    Ok(wanted)
}

/// A snapshot the store ALREADY holds is verified, never skipped: an existing
/// same-named file must be byte-identical, or the database this backup writes
/// would reference bytes the restore must refuse.
fn verify_store_bytes(
    out_store: &CandleStore,
    snapshots: &[SourceSnapshot],
    wanted: &[usize],
) -> anyhow::Result<()> {
    for index in wanted {
        let snap = &snapshots[*index];
        let dest = out_store.snapshot_path(&snap.pair, snap.timeframe, &snap.version);
        if dest.exists() && !bytes_equal(&snap.path, &dest) {
            anyhow::bail!(
                "backup: the store already holds {} with different bytes than the source \
                 snapshot {} — refusing to write a database that would reference it",
                dest.display(),
                snap.path.display()
            );
        }
    }
    Ok(())
}

/// Copy the wanted snapshots the store is missing, returning how many were
/// added (recorded in `added` for the caller's cleanup).
fn copy_wanted(
    out_store: &CandleStore,
    snapshots: &[SourceSnapshot],
    wanted: &[usize],
    added: &mut Vec<PathBuf>,
) -> anyhow::Result<usize> {
    let mut published = 0usize;
    for index in wanted {
        let snap = &snapshots[*index];
        let dest = out_store.snapshot_path(&snap.pair, snap.timeframe, &snap.version);
        if dest.exists() {
            continue;
        }
        copy_snapshot_into(&snap.path, &dest)?;
        added.push(dest);
        published += 1;
    }
    Ok(published)
}

/// The failure path's one move: no half backup survives — not the database file
/// and not a snapshot this run added.
fn discard(backup_path: &Path, added: &[PathBuf]) {
    let _ = fs::remove_file(backup_path);
    for path in added {
        let _ = fs::remove_file(path);
    }
}

/// Write the source's `HEAD` pointers into the out-dir's store, each one only
/// AFTER the snapshot it names is there and verifies, and each through the
/// store's own temp→fsync→rename ([`CandleStore::write_head`]).
///
/// The out store is SHARED between backups, so a pointer the source does not
/// have is left alone here (another backup's store may be using it) — the
/// import is where stale pointers are dropped, because it replaces a database's
/// whole store view.
///
/// # Errors
///
/// The error carries the pointers written before the failure, so the caller can
/// put the store back exactly as it was.
fn publish_heads(
    out_store: &CandleStore,
    heads: &[SourceHead],
) -> Result<(), (anyhow::Error, Vec<HeadChange>)> {
    let mut changes: Vec<HeadChange> = Vec::new();
    for head in heads {
        if let Err(error) = out_store.read_snapshot(&head.pair, head.timeframe, &head.version) {
            return Err((
                anyhow!(
                    "backup: the HEAD pointer {} names {} and the backup store does not verify \
                     it: {error}",
                    head.path.display(),
                    head.version
                ),
                changes,
            ));
        }
        let previous = out_store
            .read_head(&head.pair, head.timeframe)
            .unwrap_or(None);
        changes.push(HeadChange::new(head.pair.clone(), head.timeframe, previous));
        if let Err(error) = out_store.write_head(&head.pair, head.timeframe, &head.version) {
            return Err((
                anyhow!(
                    "backup: the HEAD pointer for {} {} could not be written: {error}",
                    head.pair.as_str(),
                    head.timeframe.binance_interval()
                ),
                changes,
            ));
        }
    }
    Ok(())
}

/// The database-only online-copy primitive. Writes a `.partial` file, then
/// renames it to its `pulse-<stamp>.db` name; on any error the partial file is
/// deleted. Callers that need the FULL backup — the one restore can consume —
/// use [`backup_target`], which adds the snapshot store to this copy.
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
