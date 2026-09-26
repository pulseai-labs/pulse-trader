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
    copy_snapshot_into, default_backup_out_dir, manifest_path, resolve_target_data_dir,
    resolve_target_db, restore_heads, run_verified_copy, scan_heads, scan_snapshots,
    write_head_manifest,
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
    let outcome = match publish_source_store(&out_store, data_dir, &staged, &mut added).await {
        Ok(outcome) => outcome,
        Err(error) => {
            discard(&staged, &added);
            return Err(error);
        }
    };

    // ---- Step 3: only now the database itself.
    if let Err(error) = publish_staged_database(&staged) {
        discard(&staged, &added);
        return Err(error);
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
) -> anyhow::Result<BackupOutcome> {
    let source_store = CandleStore::with_base_dir(data_dir.to_path_buf());

    let (copied, total) =
        publish_store_files(out_store, &source_store, data_dir, staged, added).await?;
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
    let (changes, published) = match publish_heads(out_store, &heads) {
        Ok(changes) => (
            changes,
            write_head_manifest(&staged.manifest_path(), &heads),
        ),
        Err((error, changes)) => (changes, Err(error)),
    };
    match published {
        Ok(()) => Ok((copied, snapshots.len())),
        Err(error) => {
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

/// The failure path's one move: no half backup survives — not the staged
/// database copy, not the manifest published beside it, and not a snapshot this
/// run added.
fn discard(staged: &StagedBackup, added: &[PathBuf]) {
    let _ = fs::remove_file(&staged.partial);
    let _ = fs::remove_file(staged.manifest_path());
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
    Ok(StagedBackup {
        partial,
        final_path,
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
    publish::sync_published(&staged.final_path, None)
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
    use super::{StagedBackup, publish_staged_database};
    use crate::cli::publish;
    use std::fs;

    /// Issue #259: the database is the LAST thing a backup makes visible, and
    /// the rename that publishes it is followed by an fsync of the out-dir it
    /// landed in — a backup may not be reported successful over a rename that is
    /// not durable (a power loss could otherwise keep the newest snapshot's
    /// entries while losing the database that names them, or the reverse).
    #[test]
    fn the_published_backup_database_syncs_the_out_dir_after_the_rename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged = StagedBackup {
            partial: dir.path().join("pulse-20260101T000000Z.db.partial"),
            final_path: dir.path().join("pulse-20260101T000000Z.db"),
        };
        fs::write(&staged.partial, b"a complete copy").expect("stage the copy");
        let _ = publish::probe::take();

        publish_staged_database(&staged).expect("publish");

        let syncs = publish::probe::take();
        assert_eq!(syncs.len(), 1, "one publish, one directory sync: {syncs:?}");
        assert_eq!(syncs[0].dir, dir.path(), "the out-dir it landed in");
        assert!(
            syncs[0].published_present,
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
}
