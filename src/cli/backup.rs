//! `pulse backup` / `pulse restore` — D12's nightly online backup and the
//! verified restore (r3.s3.w4, ADR-0026).
//!
//! `pulse backup` takes an online, consistent copy of the live database
//! (SQLite's `VACUUM INTO` from a read-only open — readers never block a
//! writer in WAL) into `<out-dir>/pulse-<UTC yyyymmddThhmmssZ>.db`, copies
//! every snapshot not already present into `<out-dir>/candles/` (one shared
//! store; snapshots are immutable), writes that backup's OWN pointer manifest
//! beside it (`pulse-<stamp>.db.heads.json`: the shared store's `HEAD` files are
//! one set every backup overwrites, so a restore takes the pointers of the
//! backup it was handed), prunes the oldest `pulse-*.db` beyond `--keep` along
//! with their manifests (never touching `candles/`), and prints one summary line
//! with the path, size and counts. Any error exits non-zero and deletes the
//! partial file.
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
    HeadChange, HeadSource, SourceHead, SourceSnapshot, VerifiedCopy, bytes_equal,
    copy_snapshot_into, default_backup_out_dir, manifest_path, named_by_pointer,
    refuse_orphaned_quarantines, resolve_target_data_dir, resolve_target_db, restore_heads,
    run_verified_copy, scan_heads, scan_snapshots, stranded_pointers, write_head_manifest,
};
use super::publish;
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
    /// How many `pulse-*.db` files to keep (the oldest are pruned, each with
    /// its own `.heads.json` pointer manifest; `candles/` is one shared store
    /// and is never pruned).
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
    // ---- Step 0: refuse a target whose committed rows are sitting in a
    // quarantined `-wal` (fix round 5, P5). A backup READS the database, so an
    // interrupted install's quarantine would make this run stamp a `pulse-<stamp>.db`
    // as good while it is missing exactly those rows — the same fail-closed refusal
    // the import makes, through the same helper, before anything is read.
    refuse_orphaned_quarantines(db_path)?;
    let out_store = CandleStore::with_base_dir(out_dir.to_path_buf());

    // ---- Step 1: freeze the DATABASE first, and derive the snapshot set from
    // THAT file (`publish_source_store` below). A scan taken before the copy
    // would miss a snapshot published — and referenced by a run committed — in
    // between: the backup would report success while the database it publishes
    // names a file the copy set omitted, and `pulse restore` would refuse that
    // backup later. The copy stays a `.partial` file for now: it takes its
    // `pulse-<stamp>.db` name only once everything it needs is published.
    let staged = stage_database(db_path, out_dir).await?;
    let mut added: Vec<PathBuf> = Vec::new();

    // ---- Step 2: the snapshots it references, the shared store's pointers, and
    // the backup's OWN pointer manifest — all in place BEFORE the database
    // becomes visible, so a `pulse-<stamp>.db` never appears without the
    // manifest a restore reads (round 6, Fix A).
    let mut head_changes: Vec<HeadChange> = Vec::new();
    let published =
        publish_source_store(&out_store, data_dir, &staged, &mut added, &mut head_changes).await;
    let outcome = match published {
        Ok(outcome) => outcome,
        Err(error) => {
            // Fix round 4, Q4: the manifest arm follows M1's rule too, through the
            // SAME unwind as the database arm below.
            let (restored, rollback) = unwind_backup(&out_store, &staged, &added, &head_changes);
            return Err(fold_rollback(error, Some(&restored), &rollback));
        }
    };

    // ---- Step 3: only now the database itself.
    if let Err(error) = publish_staged_database(&staged) {
        // The whole backup unwinds, in the order that keeps the store consistent
        // (fix round 3, M1): the shared store's HEAD pointers go back FIRST, so no
        // crash or error between the two steps can leave a pointer naming a
        // snapshot that is gone — and a pointer that could not be restored keeps
        // the snapshots it still names, because deleting them would leave the
        // store in exactly that state. Only then do this run's files go, each
        // removal made durable (M2).
        let (restored, rollback) = unwind_backup(&out_store, &staged, &added, &head_changes);
        return Err(fold_rollback(error, Some(&restored), &rollback));
    }
    Ok(outcome)
}

/// Publish the source store into `<out-dir>/candles/` (one shared store): every
/// snapshot the FROZEN COPY references, the `HEAD` pointers that name them (the
/// same rule the import's copy applies, so a restored backup lands a store whose
/// current pointer is the one it was taken with), and the backup's OWN pointer
/// manifest beside its staged database (round 6, Fix A).
///
/// The order is the contract:
///
/// 1. the references come from the frozen copy, never from a pre-scan;
/// 2. every source snapshot must verify (the store's own integrity read) — a
///    backup must not report success over a store the restore would refuse;
/// 3. every reference must resolve to a source snapshot, or the backup FAILS:
///    publishing a database whose snapshot is missing would hand the restore a
///    set it can only refuse;
/// 4. only then are the files copied, only then are the pointers written, and
///    only then is the manifest published — which the CALLER publishes the
///    database after, so the database is the last thing to become visible.
///
/// A failure is unwound by the caller ([`discard`]): the staged database copy,
/// the manifest if it landed, every snapshot this call added and every pointer
/// it moved are deleted here or there, so no half backup survives — and each
/// copy publishes through `copy_snapshot_into`, so a copy that fails part-way
/// leaves no truncated file under a snapshot's name.
async fn publish_source_store(
    out_store: &CandleStore,
    data_dir: &Path,
    staged: &StagedBackup,
    added: &mut Vec<PathBuf>,
    changes: &mut Vec<HeadChange>,
) -> anyhow::Result<BackupOutcome> {
    let source_store = CandleStore::with_base_dir(data_dir.to_path_buf());

    let (copied, total) =
        publish_store_files(out_store, &source_store, data_dir, staged, added, changes).await?;
    // The shared store's pointers this run moved are in the caller's `changes`
    // (fix rounds 2 N1 and 4 Q4): a caller whose publish fails owns the unwind.
    Ok(BackupOutcome {
        path: staged.final_path.clone(),
        snapshots_copied: copied,
        snapshots_total: total,
    })
}

/// The publishing body (see [`publish_source_store`] for the order it keeps),
/// returning how many snapshots were added and how many the source holds.
async fn publish_store_files(
    out_store: &CandleStore,
    source_store: &CandleStore,
    data_dir: &Path,
    staged: &StagedBackup,
    added: &mut Vec<PathBuf>,
    changes: &mut Vec<HeadChange>,
) -> anyhow::Result<(usize, usize)> {
    // 1. What the FROZEN COPY needs: the snapshots its runs name.
    let references = frozen_references(&staged.partial).await?;
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
    // 7. And the backup's OWN manifest beside the database it is being published
    // with — the shared store's pointers are for a store read directly, while a
    // RESTORE takes the pointers of the backup it was handed (round 6, Fix A).
    // The pointers this run moves are collected into the CALLER's vec: when the
    // manifest publish below fails, the unwind belongs to the caller (fix round
    // 4, Q4) and it needs them — it holds the staged backup and the added
    // snapshots, and M1's order (restore first, then delete keeping what the
    // unrestored pointers name) applies to this arm exactly as to the database's.
    match publish_heads(out_store, &heads) {
        Ok(moved) => changes.extend(moved),
        Err((error, moved)) => {
            changes.extend(moved);
            return Err(error);
        }
    }
    write_head_manifest(
        &staged.manifest_path(),
        &heads,
        staged.created_root.as_deref(),
    )?;
    Ok((copied, snapshots.len()))
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
        // Recorded BEFORE the copy (fix round 1, F8): the copy renames the file
        // into place and then syncs its directory, so a failure AFTER the rename
        // still leaves a snapshot behind — one this backup's rollback must
        // remove. The name is known free (the check above), so recording it up
        // front cannot delete anything this run did not write.
        added.push(dest.clone());
        copy_snapshot_into(&snap.path, &dest)?;
        published += 1;
    }
    Ok(published)
}

/// The whole unwind of a failed backup (fix round 3 M1/M2, fix round 4 Q4): the
/// shared store's pointers go back FIRST, a pointer that could not be restored
/// keeps the snapshots it still names, and every removal is fsynced.
///
/// Returns the restore outcome and the rollback outcome, so the caller reports
/// each failure it has to.
fn unwind_backup(
    out_store: &CandleStore,
    staged: &StagedBackup,
    added: &[PathBuf],
    head_changes: &[HeadChange],
) -> (anyhow::Result<()>, anyhow::Result<()>) {
    let restored = restore_heads(out_store, head_changes);
    let stranded = stranded_pointers(&restored);
    let rollback = discard_keeping(staged, added, &|path| {
        named_by_pointer(out_store, &stranded, path)
    });
    (restored, rollback)
}

/// The error a failed backup reports: the original failure, plus whatever the
/// rollback could not do (fix round 3, M1/M2).
fn fold_rollback(
    error: anyhow::Error,
    restored: Option<&anyhow::Result<()>>,
    rollback: &anyhow::Result<()>,
) -> anyhow::Error {
    let mut error = error;
    if let Some(Err(restore_error)) = restored {
        error = error.context(format!("[backup unwind] {restore_error}"));
    }
    if let Err(rollback_error) = rollback {
        error = error.context(format!("[backup unwind] {rollback_error}"));
    }
    error
}

/// The failure path's one move, with NO `keep`: no half backup survives — not the
/// staged database copy, not the manifest published beside it, and not a snapshot
/// this run added. (The production unwind always passes a keep predicate through
/// [`unwind_backup`]; this is the shape the rollback tests exercise on its own.)
///
/// The published name goes too (fix round 1, F2): a failure AFTER
/// [`publish_staged_database`]'s rename leaves the database under
/// `pulse-<stamp>.db`, and a database left without its manifest is one
/// `pulse restore` can only refuse and `--keep` counts as a backup. Removing it
/// is safe on the earlier failure paths — the name does not exist yet, and the
/// removal is a no-op.
#[cfg(test)]
fn discard_all(staged: &StagedBackup, added: &[PathBuf]) -> anyhow::Result<()> {
    discard_keeping(staged, added, &|_| false)
}

/// Unlink one entry of a rollback, recording it for the durability sync below.
///
/// A file that is already gone is fine; any other failure is COLLECTED (fix round
/// 4, Q2) — every path is attempted, and the caller reports the ones that could
/// not be removed instead of discarding the error. Shared with the import's undo
/// (fix round 5, P1), so both cleanups collect failures the same way.
pub(crate) fn unlink(path: &Path, removed: &mut Vec<PathBuf>, unremoved: &mut Vec<String>) {
    #[cfg(test)]
    let injected = publish::probe::take_injected_remove_failure(path);
    #[cfg(not(test))]
    let injected = false;
    let outcome = if injected {
        Err(std::io::Error::other("injected failure (cfg(test) seam)"))
    } else {
        fs::remove_file(path)
    };
    match outcome {
        Ok(()) => removed.push(path.to_path_buf()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => unremoved.push(format!("{}: {error}", path.display())),
    }
}

/// [`discard`] with `keep` holding back the snapshots a pointer that could not be
/// restored still names (fix round 3, M1), and every removal made durable by
/// fsyncing the directory that held the entry (fix round 3, M2 — a rollback a
/// power loss can undo is not a rollback).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] naming the entries that could not be removed and
/// the removals whose directory fsync failed. A file that was already absent is
/// fine: there is nothing to remove and nothing to sync.
fn discard_keeping(
    staged: &StagedBackup,
    added: &[PathBuf],
    keep: &dyn Fn(&Path) -> bool,
) -> anyhow::Result<()> {
    let mut removed: Vec<PathBuf> = Vec::new();
    let mut unremoved: Vec<String> = Vec::new();
    for path in [&staged.partial, &staged.final_path, &staged.manifest_path()] {
        unlink(path, &mut removed, &mut unremoved);
    }
    for path in added {
        if keep(path) {
            continue;
        }
        unlink(path, &mut removed, &mut unremoved);
    }
    finish_removals(&removed, &unremoved)
}

/// What a removal reported by [`finish_removals`] IS, for the one ordered log the
/// tests read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Removal {
    /// A file this run ADDED — a snapshot, a staged or published artifact.
    Artifact,
    /// The `HEAD` pointer this run CREATED, put back by removing it (fix round 6,
    /// C3). Removing it IS its restoration, and `restore_heads` records that as
    /// `HeadRestored`, so nothing is recorded here: one log, in which M1's "the
    /// pointers go back BEFORE the first deletion" stays readable.
    Pointer,
}

/// Make every removal durable and report the cleanup as a whole: the ONE report
/// shape the backup's rollback and the import's undo share (fix round 4, Q2; fix
/// round 5, P1), so neither can claim a cleanup that did not happen.
///
/// Every removal is followed by an fsync of the directory that held the entry —
/// a rollback a power loss can undo is not a rollback (fix round 3, M2).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] naming the entries that could not be removed and
/// the removals whose directory fsync failed. An entry that was already absent is
/// fine: there is nothing to remove and nothing to sync.
pub(crate) fn finish_removals(removed: &[PathBuf], unremoved: &[String]) -> anyhow::Result<()> {
    finish_removals_as(Removal::Artifact, removed, unremoved)
}

/// [`finish_removals`] for the `HEAD` pointer a run created (fix round 6, C3).
///
/// # Errors
///
/// As [`finish_removals`].
pub(crate) fn finish_head_removal(removed: &[PathBuf], unremoved: &[String]) -> anyhow::Result<()> {
    finish_removals_as(Removal::Pointer, removed, unremoved)
}

/// The one body of both: collect what could not be removed or made durable, and
/// record the removal the way the ordered log wants it.
fn finish_removals_as(
    what: Removal,
    removed: &[PathBuf],
    unremoved: &[String],
) -> anyhow::Result<()> {
    let mut unsynced: Vec<String> = Vec::new();
    for path in removed {
        #[cfg(test)]
        match what {
            Removal::Artifact => {
                publish::probe::record_rollback(publish::probe::RollbackStep::Unlinked {
                    path: path.clone(),
                });
            }
            Removal::Pointer => {}
        }
        #[cfg(not(test))]
        let _ = what;
        if let Err(error) = publish::sync_published(path, None) {
            unsynced.push(format!("fsync {}: {error}", path.display()));
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if !unremoved.is_empty() {
        parts.push(format!(
            "these entries could not be removed: {}",
            unremoved.join("; ")
        ));
    }
    if !unsynced.is_empty() {
        parts.push(format!(
            "these removals could not be made durable: {} (they may come back after a power loss)",
            unsynced.join("; ")
        ));
    }
    if parts.is_empty() {
        return Ok(());
    }
    Err(anyhow!("the rollback is incomplete — {}", parts.join("; ")))
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
) -> Result<Vec<HeadChange>, (anyhow::Error, Vec<HeadChange>)> {
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
    Ok(changes)
}

/// A database copy that is written but NOT yet published: `partial` holds a
/// complete copy, while `final_path` is the `pulse-<stamp>.db` name it takes
/// once the snapshots it references, the shared store's pointers and its own
/// pointer manifest are all in place (round 6, Fix A).
struct StagedBackup {
    /// `<out-dir>/pulse-<stamp>.db.partial` — a complete copy, not visible
    /// under the backup's own name yet.
    partial: PathBuf,
    /// `<out-dir>/pulse-<stamp>.db` — the name it publishes under.
    final_path: PathBuf,
    /// The deepest ancestor of the out-dir that existed BEFORE this backup
    /// created it (`.` for a relative path whose first component was absent, and
    /// `None` when nothing had to be created): every level below it is an entry
    /// this run made, so each publish into the out-dir must sync the levels it
    /// created up to and including this one (fix round 1, F6).
    created_root: Option<PathBuf>,
}

impl StagedBackup {
    /// This backup's OWN pointer manifest, beside the database it belongs to.
    fn manifest_path(&self) -> PathBuf {
        manifest_path(&self.final_path)
    }
}

/// The database-only copy primitive, STAGED: an online, consistent copy of
/// `db_path` into the `.partial` file this backup publishes under, deleted on
/// any error. Callers that need the FULL backup — the one restore can consume —
/// use [`backup_target`], which adds the snapshot store, the shared pointers and
/// the backup's own manifest BEFORE publishing this copy under its own name.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] when the source cannot be read or the copy
/// cannot be written.
async fn stage_database(db_path: &Path, out_dir: &Path) -> anyhow::Result<StagedBackup> {
    // The deepest ancestor that exists BEFORE the out-dir is created: every level
    // below it is an entry this backup makes, and each publish into it has to
    // sync its parent (fix round 1, F6).
    let created_root = publish::existing_ancestor(out_dir);
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
    // `VACUUM INTO` does not guarantee its output is on disk, and the publish
    // renames this file: the bytes must land before the name that promises them
    // (fix round 1, F5).
    if let Err(error) = publish::sync_file(&partial, &final_path) {
        // The staged copy's bytes are not confirmed on disk, and this error
        // returns before a `StagedBackup` exists — so `discard` cannot remove it
        // and this is the only place that can (fix round 2, N3).
        let _ = fs::remove_file(&partial);
        return Err(anyhow!(
            "flush the staged backup copy {}: {error}",
            partial.display()
        ));
    }
    Ok(StagedBackup {
        partial,
        final_path,
        created_root,
    })
}

/// Publish the staged copy under its `pulse-<stamp>.db` name — the LAST step of
/// a backup, run only once the snapshots it references, the shared store's
/// pointers and its own manifest are all in place — and make the rename durable
/// by fsyncing the out-dir it landed in (issue #259). The snapshots that live
/// under `candles/<PAIR>/<TF>/` get the syncs of their own levels in
/// [`copy_snapshot_into`], which is what makes the nested entries durable; the
/// out-dir sync here makes the database's own name durable, and it is the same
/// discipline `CandleStore::publish_atomically` applies.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] when the rename fails, or when the out-dir
/// cannot be fsynced — a backup may not be reported successful over a rename
/// that is not durable.
fn publish_staged_database(staged: &StagedBackup) -> anyhow::Result<()> {
    fs::rename(&staged.partial, &staged.final_path)
        .map_err(|e| anyhow!("publish {}: {e}", staged.final_path.display()))?;
    publish::sync_published(&staged.final_path, staged.created_root.as_deref())
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
/// returning how many remain. Each pruned backup's OWN pointer manifest goes
/// with its database — a manifest whose database is gone names pointers for
/// nothing, and leaving it behind would accumulate orphans in the out-dir. The
/// stamp names sort chronologically; a same-second `-N` collision keeps its
/// arbitrary but stable order, and every older second still sorts first.
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
        let manifest = manifest_path(victim);
        if manifest.exists() {
            fs::remove_file(&manifest).map_err(|e| anyhow!("prune {}: {e}", manifest.display()))?;
        }
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
        // The chosen backup's OWN manifest (round 6, Fix A): the shared store's
        // pointer directory holds one set that every backup overwrites.
        head_source: HeadSource::BackupManifest,
        replace: args.replace,
        chmod_source: false,
    })
    .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{StagedBackup, discard_all, publish_staged_database};
    use crate::cli::import::SourceHead;
    use crate::cli::publish;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A migrated source database, the committed fixture store as its data dir, a
    /// fresh out-dir with its store, and the first HEAD pointer the fixture names.
    async fn backup_fixture() -> (
        PathBuf,
        PathBuf,
        PathBuf,
        crate::adapters::store::CandleStore,
        SourceHead,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("source").join("pulse.db");
        fs::create_dir_all(db_path.parent().expect("the source's directory"))
            .expect("create the source's directory");
        let db = crate::adapters::db::open_migrated(&db_path)
            .await
            .expect("migrate the source");
        db.pool().close().await;
        let data_dir = dir.path().join("source-data");
        copy_tree(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/btcusdt-1m-store"),
            &data_dir,
        );
        let out_dir = dir.path().join("out");
        fs::create_dir_all(&out_dir).expect("the out-dir");
        let out_store = crate::adapters::store::CandleStore::with_base_dir(out_dir.clone());
        let (heads, _) = crate::cli::import::scan_heads(&data_dir);
        let head = heads
            .into_iter()
            .next()
            .expect("the fixture store has HEAD pointers");
        // The tempdir is dropped with the returned paths, so leak it deliberately:
        // each test owns its tree for the length of the test.
        std::mem::forget(dir);
        (db_path, data_dir, out_dir, out_store, head)
    }

    /// A recursive copy (the fixture store is a directory tree).
    fn copy_tree(from: &Path, to: &Path) {
        fs::create_dir_all(to).expect("create the destination directory");
        for entry in fs::read_dir(from).expect("read the source directory") {
            let entry = entry.expect("directory entry");
            let target = to.join(entry.file_name());
            if entry.file_type().expect("file type").is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).expect("copy the file");
            }
        }
    }

    /// A staged backup rooted in `dir` (the fields a test may not care about are
    /// the empty defaults).
    fn staged(dir: &std::path::Path, created_root: Option<PathBuf>) -> StagedBackup {
        StagedBackup {
            partial: dir.join("pulse-20260101T000000Z.db.partial"),
            final_path: dir.join("pulse-20260101T000000Z.db"),
            created_root,
        }
    }

    /// Issue #259: the database is the LAST thing a backup makes visible, and
    /// the rename that publishes it is followed by an fsync of the out-dir it
    /// landed in — a backup may not be reported successful over a rename that is
    /// not durable.
    #[test]
    fn the_published_backup_database_syncs_the_out_dir_after_the_rename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged = staged(dir.path(), None);
        fs::write(&staged.partial, b"a complete copy").expect("stage the copy");
        let _ = publish::probe::take();

        publish_staged_database(&staged).expect("publish");

        let syncs = publish::probe::take();
        assert_eq!(syncs.len(), 1, "one publish, one directory sync: {syncs:?}");
        assert_eq!(syncs[0].path, dir.path(), "the out-dir it landed in");
        assert!(
            syncs[0].destination_present,
            "the sync happens AFTER the rename, never before it: {syncs:?}"
        );
        assert!(
            !staged.partial.exists(),
            "the staged name is gone: {}",
            staged.partial.display()
        );
        assert_eq!(
            fs::read(&staged.final_path).expect("read the published backup"),
            b"a complete copy",
            "and the backup holds the copy's bytes"
        );
    }

    /// Fix round 4, Q2: a rollback that cannot REMOVE a file must say so — every
    /// path is attempted, and the failures are named (the round-3 code discarded
    /// the error whenever the unlink failed).
    #[test]
    fn a_rollback_that_cannot_remove_a_file_names_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged = staged(dir.path(), None);
        fs::write(&staged.partial, b"staged").expect("stage");
        fs::write(&staged.final_path, b"published").expect("publish");
        fs::write(staged.manifest_path(), b"{}").expect("manifest");
        // The published database cannot be unlinked.
        publish::probe::fail_next_remove_of(&staged.final_path);

        let error = discard_all(&staged, &[]).expect_err("the failed unlink is reported");

        let message = error.to_string();
        assert!(
            message.contains("could not be removed"),
            "the error says an entry could not be removed: {message}"
        );
        assert!(
            message.contains("the rollback is incomplete"),
            "and frames the whole rollback: {message}"
        );
        assert!(
            message.contains(&staged.final_path.display().to_string()),
            "and names it: {message}"
        );
        assert!(
            staged.final_path.exists(),
            "the file is still there, as reported"
        );
        assert!(
            !staged.partial.exists(),
            "and every OTHER path was still attempted"
        );
        assert!(!staged.manifest_path().exists(), "including the manifest");
    }

    /// Fix round 4, Q4: the manifest-publish failure arm follows M1's rule too —
    /// the pointers go back first, and a pointer that could not be restored keeps
    /// the snapshots it still names. Exercised through the injected manifest
    /// failure PLUS a pointer-restore failure.
    #[tokio::test]
    async fn a_failed_manifest_publish_unwinds_like_the_database_one() {
        let (db_path, data_dir, out_dir, out_store, head) = backup_fixture().await;
        let prior = crate::domain::DataVersion::parse("0000000000000001").expect("a version tag");
        out_store
            .write_head(&head.pair, head.timeframe, &prior)
            .expect("seed the store's prior pointer");
        let (source_snapshots, _) = crate::cli::import::scan_snapshots(&data_dir);
        // The HEAD pointer cannot go back...
        publish::probe::fail_next_head_write_to(&out_store.head_path(&head.pair, head.timeframe));
        // ...and the manifest publish (extension `json`) fails: the FIRST publish
        // step, so the database publish never runs.
        publish::probe::fail_next_publish_with_extension("json");

        let Err(error) = super::backup_target(&db_path, &data_dir, &out_dir).await else {
            panic!("the injected manifest failure must end the backup");
        };

        let message = error.to_string();
        assert!(
            message.contains("could not be restored for"),
            "the unwind restored (and reported) the pointers first: {message}"
        );
        assert!(
            message.contains(head.pair.as_str()),
            "and names the pointer that is not back: {message}"
        );
        let kept_dir = out_store
            .head_path(&head.pair, head.timeframe)
            .parent()
            .expect("the timeframe directory")
            .to_path_buf();
        let kept: Vec<String> = fs::read_dir(&kept_dir)
            .expect("read the timeframe directory")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".parquet"))
            .collect();
        assert!(
            !kept.is_empty(),
            "the snapshot the unrestored pointer names is KEPT: {kept:?}"
        );
        for snapshot in &source_snapshots {
            if snapshot.pair == head.pair && snapshot.timeframe == head.timeframe {
                continue;
            }
            let dest =
                out_store.snapshot_path(&snapshot.pair, snapshot.timeframe, &snapshot.version);
            assert!(
                !dest.exists(),
                "the snapshot no unrestored pointer names is removed: {}",
                dest.display()
            );
        }
        // And the database itself was never published (the manifest step failed).
        let published: Vec<String> = fs::read_dir(&out_dir)
            .expect("read the out-dir")
            .flatten()
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "db")
            })
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            published.is_empty(),
            "no database was published: {published:?}"
        );
    }

    /// Fix round 3, M1: on a failed final publish the shared store's pointers go
    /// back BEFORE anything is deleted — a crash or error between the two steps
    /// must never leave a HEAD pointer naming a snapshot that is gone.
    #[tokio::test]
    async fn a_failed_final_publish_restores_the_pointers_before_it_deletes_anything() {
        let (db_path, data_dir, out_dir, out_store, head) = backup_fixture().await;
        out_store
            .write_head(
                &head.pair,
                head.timeframe,
                &crate::domain::DataVersion::parse("0000000000000001").expect("a version tag"),
            )
            .expect("seed the store's prior pointer");
        publish::probe::fail_next_publish_with_extension("db");
        let _ = publish::probe::take_rollback();

        let Err(_) = super::backup_target(&db_path, &data_dir, &out_dir).await else {
            panic!("the injected publish failure must end the backup");
        };

        let steps = publish::probe::take_rollback();
        let last_restore = steps
            .iter()
            .rposition(|step| matches!(step, publish::probe::RollbackStep::HeadRestored { .. }))
            .unwrap_or_else(|| panic!("a pointer was restored: {steps:?}"));
        let first_unlink = steps
            .iter()
            .position(|step| matches!(step, publish::probe::RollbackStep::Unlinked { .. }))
            .unwrap_or_else(|| panic!("this run's files were removed: {steps:?}"));
        assert!(
            last_restore < first_unlink,
            "every pointer is restored BEFORE the first deletion: {steps:?}"
        );
    }

    /// Fix round 3, M1: a pointer that could NOT be restored keeps the snapshots
    /// it still names — deleting them would leave the store advertising a
    /// snapshot that is gone — and the error names the pointer.
    #[tokio::test]
    async fn a_pointer_that_cannot_be_restored_keeps_the_snapshot_it_names() {
        let (db_path, data_dir, out_dir, out_store, head) = backup_fixture().await;
        let prior = crate::domain::DataVersion::parse("0000000000000001").expect("a version tag");
        out_store
            .write_head(&head.pair, head.timeframe, &prior)
            .expect("seed the store's prior pointer");
        let (source_snapshots, _) = crate::cli::import::scan_snapshots(&data_dir);
        let (source_heads, _) = crate::cli::import::scan_heads(&data_dir);
        publish::probe::fail_next_publish_with_extension("db");
        // The restore of THIS pointer is injected to fail.
        publish::probe::fail_next_head_write_to(&out_store.head_path(&head.pair, head.timeframe));

        let Err(error) = super::backup_target(&db_path, &data_dir, &out_dir).await else {
            panic!("the injected publish failure must end the backup");
        };

        let message = error.to_string();
        assert!(
            message.contains("could not be restored for"),
            "the error says the pointers could not be restored: {message}"
        );
        assert!(
            message.contains(head.pair.as_str()),
            "and names the pointer that is not back: {message}"
        );
        // The named pair's snapshot stays; the other pair's goes.
        let kept_dir = out_store
            .head_path(&head.pair, head.timeframe)
            .parent()
            .expect("the timeframe directory")
            .to_path_buf();
        let kept: Vec<String> = fs::read_dir(&kept_dir)
            .expect("read the timeframe directory")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".parquet"))
            .collect();
        assert!(
            !kept.is_empty(),
            "the snapshot the unrestored pointer names is kept: {kept:?}"
        );
        for snapshot in &source_snapshots {
            if snapshot.pair == head.pair
                && source_heads.iter().any(|other| {
                    other.pair == snapshot.pair && other.timeframe == snapshot.timeframe
                })
                && snapshot.timeframe == head.timeframe
            {
                continue;
            }
            let dest =
                out_store.snapshot_path(&snapshot.pair, snapshot.timeframe, &snapshot.version);
            assert!(
                !dest.exists(),
                "the snapshot no unrestored pointer names is removed: {}",
                dest.display()
            );
        }
    }

    /// Fix round 5, P5: a backup READS the database, so a target whose committed
    /// rows sit in a quarantined `-wal` must be refused BEFORE anything is read —
    /// otherwise this run stamps a `pulse-<stamp>.db` as good while it is missing
    /// exactly those rows. The same refusal the import makes, through the same
    /// helper, and nothing is written to the out-dir.
    #[tokio::test]
    async fn a_backup_refuses_a_target_with_an_orphaned_quarantine() {
        let (db_path, data_dir, out_dir, _out_store, _head) = backup_fixture().await;
        let orphan = db_path
            .parent()
            .expect("the source's directory")
            .join(".pulse.db-wal.quarantine-999999");
        fs::write(&orphan, b"committed rows that are not in the database file")
            .expect("the quarantine");

        let Err(error) = super::backup_target(&db_path, &data_dir, &out_dir).await else {
            panic!("an orphaned quarantine must refuse the backup");
        };

        assert!(
            error
                .downcast_ref::<crate::cli::import::OrphanedQuarantines>()
                .is_some(),
            "the refusal is the typed one the import uses: {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains(&orphan.display().to_string()),
            "and it names the quarantine: {message}"
        );
        assert!(
            message.contains(
                &crate::cli::import::sidecar_path(&db_path, "-wal")
                    .display()
                    .to_string()
            ),
            "and the name those rows have to go back to: {message}"
        );
        let published: Vec<String> = fs::read_dir(&out_dir)
            .expect("read the out-dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            published.is_empty(),
            "nothing was published, not even the shared candles store: {published:?}"
        );
    }

    /// Fix round 3, M2: the rollback's removals are made durable — each directory
    /// that held a removed entry is fsynced after the unlink, so a power loss
    /// cannot resurrect a half-rolled-back backup.
    #[test]
    fn the_rollbacks_removals_are_synced_after_they_are_unlinked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged = staged(dir.path(), None);
        fs::write(&staged.partial, b"staged").expect("stage");
        fs::write(&staged.final_path, b"published").expect("publish");
        fs::write(staged.manifest_path(), b"{}").expect("manifest");
        let added = dir
            .path()
            .join("candles")
            .join("BTCUSDT")
            .join("15m")
            .join("dv.parquet");
        fs::create_dir_all(added.parent().expect("the snapshot's directory"))
            .expect("create the snapshot's directory");
        fs::write(&added, b"a copied snapshot").expect("write the copied snapshot");
        let _ = publish::probe::take();

        discard_all(&staged, std::slice::from_ref(&added))
            .expect("the rollback's removals are durable");

        let syncs = publish::probe::take();
        for path in [
            staged.partial.clone(),
            staged.final_path.clone(),
            staged.manifest_path(),
            added.clone(),
        ] {
            assert!(!path.exists(), "{} is removed", path.display());
            assert!(
                syncs.iter().any(|event| {
                    event.kind == publish::probe::SyncKind::Dir
                        && event.destination == path
                        && !event.destination_present
                }),
                "the removal of {} is made durable (a directory sync AFTER the unlink): {syncs:?}",
                path.display()
            );
        }
    }

    /// Fix round 2, N3: when the staged copy's own flush fails, the partial is
    /// removed — the error returns before a `StagedBackup` exists, so `discard`
    /// cannot do it and nothing else would.
    #[tokio::test]
    async fn a_failed_staged_flush_removes_the_partial() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("source").join("pulse.db");
        fs::create_dir_all(db_path.parent().expect("the source's directory"))
            .expect("create the source's directory");
        let db = crate::adapters::db::open_migrated(&db_path)
            .await
            .expect("migrate the source");
        db.pool().close().await;
        let out_dir = dir.path().join("out");
        publish::probe::fail_next_file_sync();

        // (`StagedBackup` deliberately carries no `Debug`, so the failure is
        // destructured rather than `expect_err`ed.)
        let Err(error) = super::stage_database(&db_path, &out_dir).await else {
            panic!("the injected file sync failure must end staging");
        };

        assert!(
            error.to_string().contains("flush the staged backup copy"),
            "the failure names the flush: {error}"
        );
        let left: Vec<String> = fs::read_dir(&out_dir)
            .expect("read the out-dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            left.is_empty(),
            "no staged copy survives the failed flush: {left:?}"
        );
    }

    /// Fix round 2, N1: when the LAST publish fails, the backup unwinds
    /// completely — F2's removals AND the shared store's HEAD pointers this run
    /// moved. They were published before the database, so without this the store
    /// keeps advertising snapshots (and versions) the failed backup never
    /// published.
    #[tokio::test]
    async fn a_failed_final_publish_also_restores_the_shared_store_pointers() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The source: a migrated database and the committed fixture store.
        let db_path = dir.path().join("source").join("pulse.db");
        fs::create_dir_all(db_path.parent().expect("the source's directory"))
            .expect("create the source's directory");
        let db = crate::adapters::db::open_migrated(&db_path)
            .await
            .expect("migrate the source");
        db.pool().close().await;
        let data_dir = dir.path().join("source-data");
        copy_tree(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/btcusdt-1m-store"),
            &data_dir,
        );

        // The out store already points at ANOTHER version for one of the pairs:
        // that is the value the failed backup has to put back.
        let out_dir = dir.path().join("out");
        fs::create_dir_all(&out_dir).expect("the out-dir");
        let out_store = crate::adapters::store::CandleStore::with_base_dir(out_dir.clone());
        let (source_heads, _) = crate::cli::import::scan_heads(&data_dir);
        assert!(
            !source_heads.is_empty(),
            "the fixture store has HEAD pointers"
        );
        let head = &source_heads[0];
        let prior = crate::domain::DataVersion::parse("0000000000000001").expect("a version tag");
        out_store
            .write_head(&head.pair, head.timeframe, &prior)
            .expect("seed the store's prior pointer");

        // The database publish (extension `db`) fails after its rename; the
        // manifest publish beside it (extension `json`) succeeds.
        publish::probe::fail_next_publish_with_extension("db");
        let Err(_) = super::backup_target(&db_path, &data_dir, &out_dir).await else {
            panic!("the injected publish failure must end the backup");
        };

        assert_eq!(
            out_store
                .read_head(&head.pair, head.timeframe)
                .expect("read the restored pointer"),
            Some(prior),
            "the shared store's HEAD pointer is back to the value this backup found"
        );
        // And F2's removals still hold: no published database, no manifest, no
        // snapshot this run added.
        let left: Vec<String> = fs::read_dir(&out_dir)
            .expect("read the out-dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with("pulse-"))
            .collect();
        assert!(
            left.is_empty(),
            "nothing of the failed backup is left: {left:?}"
        );
    }

    /// Fix round 1, F6: a FIRST backup into an out-dir that does not exist yet
    /// creates it — and every level above it — so each publish syncs the levels
    /// this run created in THEIR parents, up to and including the first ancestor
    /// that already existed. Without that, the whole tree a nightly backup makes
    /// is one power loss away from not existing.
    #[tokio::test]
    async fn a_first_backup_into_an_absent_out_dir_syncs_every_level_it_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("source").join("pulse.db");
        fs::create_dir_all(db_path.parent().expect("the source's directory"))
            .expect("create the source's directory");
        let db = crate::adapters::db::open_migrated(&db_path)
            .await
            .expect("migrate the source");
        db.pool().close().await;
        let data_dir = dir.path().join("source-data");
        // TWO levels that do not exist yet; the tempdir is the first ancestor
        // that does.
        let out_dir = dir.path().join("backups").join("nightly");
        let _ = publish::probe::take();

        super::backup_target(&db_path, &data_dir, &out_dir)
            .await
            .expect("the first backup succeeds");

        let synced: Vec<PathBuf> = publish::probe::take()
            .into_iter()
            .filter(|event| event.kind == publish::probe::SyncKind::Dir)
            .map(|event| event.path)
            .collect();
        assert!(
            synced.contains(&out_dir),
            "the out-dir this run created is synced: {synced:?}"
        );
        assert!(
            synced.contains(&dir.path().join("backups")),
            "and so is the level that holds it: {synced:?}"
        );
        assert!(
            synced.contains(&dir.path().to_path_buf()),
            "and the first ancestor that already existed: {synced:?}"
        );
        let backups: Vec<PathBuf> = fs::read_dir(&out_dir)
            .expect("read the out-dir")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "db"))
            .collect();
        assert_eq!(backups.len(), 1, "the backup is published: {backups:?}");
    }

    /// Fix round 1, F2: a failure AFTER the publish rename is a FULL rollback. The
    /// database is already under its final name by then, and leaving it there —
    /// without the manifest published beside it — gives `pulse restore` an
    /// artifact it can only refuse and `--keep` a backup it counts.
    #[test]
    fn a_publish_that_fails_after_its_rename_is_discarded_completely() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged = staged(dir.path(), None);
        fs::write(&staged.partial, b"a complete copy").expect("stage the copy");
        let manifest = staged.manifest_path();
        fs::write(&manifest, b"{}").expect("publish a manifest");
        let added = dir
            .path()
            .join("candles")
            .join("BTCUSDT")
            .join("15m")
            .join("dv.parquet");
        fs::create_dir_all(added.parent().expect("the snapshot's directory"))
            .expect("create the snapshot's directory");
        fs::write(&added, b"a copied snapshot").expect("write the copied snapshot");
        // The publish's directory sync fails: the rename has already landed.
        publish::probe::fail_next_sync_of(dir.path());

        publish_staged_database(&staged)
            .expect_err("the injected directory sync fails after the rename");
        assert!(
            staged.final_path.exists(),
            "the rename landed, so the database is under its final name"
        );

        discard_all(&staged, std::slice::from_ref(&added)).expect("the rollback runs");

        assert!(
            !staged.final_path.exists(),
            "the published database is removed too, not left manifest-less"
        );
        assert!(!manifest.exists(), "the manifest goes with it");
        assert!(!added.exists(), "and the snapshot this run added");
        assert!(
            !staged.partial.exists(),
            "and the staged name never survives"
        );
    }

    /// Fix round 1, F8 (backup side): a snapshot whose rename landed is recorded
    /// in `added` even when the directory sync after it fails, so this backup's
    /// rollback removes it.
    #[test]
    fn a_snapshot_whose_directory_sync_fails_is_recorded_for_the_rollback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source_data = dir.path().join("mac-data");
        let leaf = source_data.join("candles").join("BTCUSDT").join("15m");
        fs::create_dir_all(&leaf).expect("the source store");
        fs::write(leaf.join("b51388284a3a4371.parquet"), b"snapshot bytes")
            .expect("write the source snapshot");
        let (snapshots, issues) = crate::cli::import::scan_snapshots(&source_data);
        assert!(issues.is_empty(), "the source store is well-formed");
        assert_eq!(snapshots.len(), 1, "one source snapshot");

        let out_dir = dir.path().join("out");
        let out_store = crate::adapters::store::CandleStore::with_base_dir(out_dir.clone());
        let dest = out_store.snapshot_path(
            &snapshots[0].pair,
            snapshots[0].timeframe,
            &snapshots[0].version,
        );
        publish::probe::fail_next_sync_of(dest.parent().expect("the leaf's directory"));
        let mut added: Vec<PathBuf> = Vec::new();

        super::copy_wanted(&out_store, &snapshots, &[0], &mut added)
            .expect_err("the injected directory sync fails after the rename");

        assert!(
            dest.exists(),
            "the rename landed before the sync failed: {}",
            dest.display()
        );
        assert_eq!(
            added,
            vec![dest.clone()],
            "and the copy is recorded so the rollback can remove it"
        );

        let staged = staged(&out_dir, None);
        discard_all(&staged, &added).expect("the rollback runs");
        assert!(
            !dest.exists(),
            "the rollback removed the published snapshot"
        );
    }
}
