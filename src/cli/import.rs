//! `pulse import` — D7's verified one-way move of a Mac `pulse.db` + its
//! candle snapshots onto this host (r3.s3.w4, ADR-0026).
//!
//! The order (spec §Interface contract): refuse a source that resolves to the
//! target (#264, symlinks resolved) and take the target's instance lock (#250)
//! BEFORE anything else; refuse a non-empty target (with `--replace`, back it
//! up first through the same backup `pulse backup` makes); copy the source
//! consistently with `VACUUM INTO` from a read-only open into a temporary file
//! BESIDE the target (never `/tmp`); migrate the copy forward
//! (`open_migrated`); copy the snapshots (an existing same-named file must be
//! byte-identical); verify everything — row counts, stored hashes, the three
//! paper digests, repository reads, snapshot reads, referenced snapshots —
//! without stopping at the first failure within a check; then atomically rename
//! the temporary database into place, `chmod a-w` the source path, and print the
//! summary. On any failure every temporary is deleted, every snapshot this
//! run added is removed, the target is untouched, and every mismatch is named
//! (up to 20, then "…and N more").
//!
//! `pulse restore` (`cli/backup.rs`) reuses [`run_verified_copy`] with the
//! backup as the source: same verification, same atomic install, no chmod.
//!
//! **Its precondition: the server must be stopped — checked, not just
//! documented (#250).** The install deletes the target's `-wal`/`-shm` and
//! renames over `pulse.db`, so a running `pulse serve` would keep serving the
//! unlinked old inode, the deleted WAL content is gone, and every write it
//! commits after the swap is silently lost. Import therefore takes the target's
//! instance lock ([`crate::adapters::db::instance_lock`]) before any write — a
//! running server holds it, so the run refuses by name — and holds it until the
//! install completes. D7's cutover order is unchanged: quit the old Mac app,
//! stop the unit, then import.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use clap::Args;
use serde::{Deserialize, Serialize};

use super::backup;
use super::publish;
// The interrupted-install refusal is SHARED with the production open (fix round 6,
// D1), the backup (P5) and the import (Q1): one scan, one message, three callers.
#[cfg(test)]
use super::publish::probe;
use crate::adapters::db::default_db_path;
use crate::adapters::db::instance_lock::{InstanceLock, canonical_identity};
use crate::adapters::db::ops;
pub(crate) use crate::adapters::db::{
    DB_STATE_SUFFIXES, TargetIdentity, inode_of, quarantine_name, refuse_orphaned_quarantines,
    target_dir,
};
use crate::adapters::db::{
    Db, SqliteBacktestRunRepo, SqliteStrategyRepo, open_migrated_copy, put_in_wal,
};
#[cfg(test)]
pub(crate) use crate::adapters::db::{InterruptedInstall, QuarantineOwner, orphaned_quarantines};
use crate::adapters::store::CandleStore;
use crate::adapters::store::default_base_dir;
use crate::domain::strategy::VersionId;
use crate::domain::{
    BacktestRunId, BacktestRunRepository, DataVersion, Pair, StrategyRepository, Timeframe,
};

/// How many mismatches the refusal names before it summarizes the rest.
const MAX_NAMED_MISMATCHES: usize = 20;

/// `pulse import --from-db <mac.db> --from-data-dir <dir holding its
/// candles/> [--db <target>] [--data-dir <target>] [--replace]`.
///
/// **Precondition: the server must be stopped — and this command enforces it
/// (#250).** The target's instance lock is taken before any write and held
/// until the install completes, so a running `pulse serve` refuses this run by
/// name instead of losing every write it commits after the swap. A source that
/// resolves to the target is refused too (#264), before anything is touched.
/// D7's cutover order stands: quit the old Mac app, stop the unit, then import.
#[derive(Debug, Args)]
pub struct ImportArgs {
    /// The Mac `pulse.db` copy to import. Read-only throughout; set a-w on
    /// success.
    #[arg(long)]
    pub from_db: PathBuf,
    /// The directory holding the Mac candle store (its `candles/` subtree).
    #[arg(long)]
    pub from_data_dir: PathBuf,
    /// The target database. Defaults to the server's platform default.
    #[arg(long)]
    pub db: Option<PathBuf>,
    /// The target data dir. Defaults to the server's platform default.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Back up the non-empty target first (the same backup `pulse backup`
    /// makes, into `~/pulse-backups`), then replace it. Without this, a
    /// non-empty target is refused.
    #[arg(long, default_value_t = false)]
    pub replace: bool,
}

/// Run the import (the composition root's thin wrapper over the engine).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on any refusal or failure; the target is left
/// exactly as it was.
pub(crate) async fn run_import(args: &ImportArgs) -> anyhow::Result<()> {
    let db_target = resolve_target_db(args.db.as_ref())?;
    let data_target = resolve_target_data_dir(args.data_dir.as_ref())?;
    run_verified_copy(VerifiedCopy {
        source_label: "import",
        from_db: &args.from_db,
        from_data_dir: &args.from_data_dir,
        db_target: &db_target,
        data_target: &data_target,
        head_source: HeadSource::SourceStore,
        replace: args.replace,
        chmod_source: true,
    })
    .await
}

/// One parameterised verified copy: `pulse import` over the Mac source, or
/// `pulse restore` over a backup — same steps, same verification, same atomic
/// install; only the printed label and the chmod (an import's Mac file is set
/// a-w; a backup file is left alone) differ.
pub(crate) struct VerifiedCopy<'a> {
    /// The printed verb: `import` or `restore`.
    pub source_label: &'static str,
    /// The source database (read-only throughout).
    pub from_db: &'a Path,
    /// The directory holding the source's candle store.
    pub from_data_dir: &'a Path,
    /// The target database.
    pub db_target: &'a Path,
    /// The target data dir.
    pub data_target: &'a Path,
    /// Where this copy's HEAD pointers come from (round 6, Fix A).
    pub head_source: HeadSource,
    /// Back up the non-empty target first, then replace it.
    pub replace: bool,
    /// Set the source db a-w after a successful copy (import only).
    pub chmod_source: bool,
}

/// Where a verified copy takes its HEAD pointers from.
///
/// An IMPORT reads the source store's own pointer files: the data dir is the
/// operator's store and its pointers are part of what travels.
///
/// A RESTORE must not: the backup store is SHARED by every backup in the
/// out-dir and each one overwrites the same pointer files, so reading them back
/// would publish the NEWEST pointers over an OLDER database — an older run's
/// frozen snapshot set silently replaced by newer current candles. A restore
/// therefore takes the pointers from the chosen backup's own manifest, and a
/// backup with no manifest is refused by name rather than guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadSource {
    /// The source store's own `HEAD` files (an import).
    SourceStore,
    /// The chosen backup's manifest, beside its database (a restore).
    BackupManifest,
}

/// One named verification failure: the table, the id, the field.
pub(crate) struct Mismatch {
    /// The table (or `snapshot`) the mismatch names.
    pub table: String,
    /// The row id (or snapshot path / `data_version`) it names.
    pub id: String,
    /// The field (or `bytes` / `layout` / `row_count`) it names.
    pub field: String,
    /// The concrete difference.
    pub detail: String,
}

impl Mismatch {
    pub(crate) fn new(
        table: &str,
        id: impl Into<String>,
        field: &str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            table: table.to_owned(),
            id: id.into(),
            field: field.to_owned(),
            detail: detail.into(),
        }
    }
}

/// What a successful verification counted (the summary's substance).
struct Summary {
    table_counts: Vec<(String, i64)>,
    versions_verified: usize,
    runs_verified: usize,
    snapshots_verified: usize,
    /// The three paper tables' content digests (r4.s2.w2), in
    /// [`ops::PAPER_TABLES`] order — the summary prints them.
    paper_digests: Vec<ops::PaperTableDigest>,
}

/// The files this run added to the target data dir — removed on any failure
/// so the target keeps exactly its previous contents.
type Added = Vec<PathBuf>;

/// What the steps changed in the target store — everything the failure cleanup
/// has to undo, so the target is left exactly as it was.
#[derive(Default)]
struct StoreWrites {
    /// The snapshot files this run added.
    added: Added,
    /// The `HEAD` pointers this run changed.
    heads: Vec<HeadChange>,
}

/// How the steps ended: a counted summary, a hard failure, or mismatches.
enum StepFailure {
    /// An I/O or database failure (not a verification mismatch). It carries
    /// what this run had ALREADY written to the target — a step can fail after
    /// `copy_snapshots` put files in place or `publish_heads` moved a pointer,
    /// and the cleanup must undo both, or the target is left dirty against the
    /// contract.
    Fatal {
        error: anyhow::Error,
        writes: StoreWrites,
    },
    /// The verification found mismatches; the target must stay untouched.
    Mismatches {
        mismatches: Vec<Mismatch>,
        writes: StoreWrites,
    },
}

/// A fatal step failure that carries the target writes to the cleanup — the
/// shape EVERY fatal path in [`run_steps`] uses.
fn fatal(error: anyhow::Error, writes: &StoreWrites) -> StepFailure {
    StepFailure::Fatal {
        error,
        writes: StoreWrites {
            added: writes.added.clone(),
            heads: writes.heads.clone(),
        },
    }
}

/// Resolve the `--db` override or the server's default.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] when the platform default cannot be resolved.
pub(crate) fn resolve_target_db(over: Option<&PathBuf>) -> anyhow::Result<PathBuf> {
    over.cloned().map_or_else(
        || default_db_path().map_err(|e| anyhow!("resolve default db path: {e}")),
        Ok,
    )
}

/// Resolve the `--data-dir` override or the server's default.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] when the platform default cannot be resolved.
pub(crate) fn resolve_target_data_dir(over: Option<&PathBuf>) -> anyhow::Result<PathBuf> {
    over.cloned().map_or_else(
        || default_base_dir().map_err(|e| anyhow!("resolve default data dir: {e}")),
        Ok,
    )
}

/// The default backup out-dir: `~/pulse-backups` (D12).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] when no home directory is resolvable.
pub(crate) fn default_backup_out_dir() -> anyhow::Result<PathBuf> {
    // A test that reaches a safety backup must never write the operator's real
    // `~/pulse-backups` (fix round 5, P3): it points this at its own tempdir, the
    // same way `publish::probe` injects sync failures.
    #[cfg(test)]
    if let Some(dir) = BACKUP_OUT_DIR_OVERRIDE.with(|cell| cell.borrow().clone()) {
        return Ok(dir);
    }
    let home = directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .ok_or_else(|| anyhow!("cannot resolve the home directory for the default backup dir"))?;
    Ok(home.join("pulse-backups"))
}

// Where a test points [`default_backup_out_dir`] (fix round 5, P3), so no test
// can read or write the operator's real backup directory. `cfg(test)`-only, in
// the same spirit as `publish::probe`.
#[cfg(test)]
thread_local! {
    static BACKUP_OUT_DIR_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Point [`default_backup_out_dir`] at `dir` on this thread (fix round 5, P3).
#[cfg(test)]
fn override_backup_out_dir(dir: &Path) {
    BACKUP_OUT_DIR_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(dir.to_path_buf()));
}

/// A source mutation the engine awaits right after its copy step: the
/// concurrent writer the paper digest check exists to catch.
#[cfg(test)]
pub(crate) type SourceMutation =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send>;

// Where a test hands the next verified copy the source mutation to await
// (r4.s2.w2), in the same spirit as `publish::probe`: a writer that lands
// between the copy's read snapshot and the verification reads cannot be
// scheduled from outside the engine, and the digest refusal is worth proving
// deterministically.
#[cfg(test)]
thread_local! {
    static SOURCE_MUTATION: std::cell::RefCell<Option<SourceMutation>> =
        const { std::cell::RefCell::new(None) };
}

/// Arrange the mutation the next verified copy on this thread awaits after its
/// copy step.
#[cfg(test)]
fn set_source_mutation(mutation: SourceMutation) {
    SOURCE_MUTATION.with(|cell| *cell.borrow_mut() = Some(mutation));
}

/// Take the pending mutation, if one is arranged.
#[cfg(test)]
fn take_source_mutation() -> Option<SourceMutation> {
    SOURCE_MUTATION.with(|cell| cell.borrow_mut().take())
}

// ---------------------------------------------------------------------------
// #264: a source that resolves to the target
// ---------------------------------------------------------------------------

/// Refuse (#264, r4.s2.w2) a source database — or data dir — that resolves to
/// the target: the import would write over the very thing it is moving.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] naming the source, the target and the mistake.
fn refuse_source_is_target(label: &str, job: &VerifiedCopy<'_>) -> anyhow::Result<()> {
    if resolves_to_same(job.from_db, job.db_target) {
        anyhow::bail!(
            "{label}: refusing: the source database {} IS the target database {} — nothing was \
             touched (#264)",
            job.from_db.display(),
            job.db_target.display()
        );
    }
    if resolves_to_same(job.from_data_dir, job.data_target) {
        anyhow::bail!(
            "{label}: refusing: the source data dir {} IS the target data dir {} — nothing was \
             touched (#264)",
            job.from_data_dir.display(),
            job.data_target.display()
        );
    }
    Ok(())
}

/// Whether `other` names the same filesystem object as `existing`.
///
/// A SYMLINK resolves to its target: when both paths exist their inodes are
/// compared — which catches a symlink, a hard link, and a differently-cased
/// spelling on a case-insensitive filesystem — and the CANONICAL paths are
/// compared otherwise (the target may not exist yet, so its deepest existing
/// ancestor is canonicalized and the component below it appended).
#[must_use]
fn resolves_to_same(existing: &Path, other: &Path) -> bool {
    if let (TargetIdentity::Inode(left), TargetIdentity::Inode(right)) =
        (inode_of(existing), inode_of(other))
    {
        return left == right;
    }
    canonical_identity(existing) == canonical_identity(other)
}

/// The shared verified-copy engine (import over the Mac source; restore over
/// a backup — the same verification, the same atomic install).
///
/// It refuses a source that resolves to the target (#264) and a target whose
/// instance lock is held (#250) before any write, holds that lock until the
/// install completes, and compares the three paper tables by content digest as
/// part of the verification (r4.s2.w2).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on any refusal, mismatch or failure; the
/// target is left exactly as it was and every temporary is deleted.
#[allow(clippy::too_many_lines)] // one linear sequence: the refusals, the copy, the steps, the install
pub(crate) async fn run_verified_copy(job: VerifiedCopy<'_>) -> anyhow::Result<()> {
    let label = job.source_label;

    // ---- #264 (r4.s2.w2): a source that resolves to the target is the
    // mistake that writes over the very thing being moved. Refused before
    // ANYTHING else — before the lock file, before any read of the target — so
    // the source is never touched, not even by this run's first byte.
    refuse_source_is_target(label, &job)?;

    // ---- The deepest ancestor that exists BEFORE the target's directory is
    // created — computed HERE, before the instance lock below, whose
    // `create_dir_all` makes the target's parent. The lock used to run first,
    // so on a first import into a new directory the created root WAS the
    // directory the lock had just made, and the parent that received its entry
    // was never synced (PR-354 fix C3c; a regression from main). Every level
    // below it is an entry this run makes, so the install's directory sync has
    // to cover them (fix round 1, F6). A bare relative target resolves to the
    // current directory, which already exists (fix round 5, P4).
    let created_root = publish::existing_ancestor(&target_dir(job.db_target));

    // ---- #250 (r4.s2.w2): the target's instance lock, held until the install
    // completes. A running `pulse serve` holds it, so this run refuses by name
    // instead of writing under a live server — and a concurrent data op cannot
    // interleave with this one.
    let _instance_lock = InstanceLock::acquire(job.db_target)
        .map_err(|error| anyhow!("{label}: refusing: {error}"))?;

    // ---- Round 6, Fix A: a RESTORE takes its pointers from the chosen
    // backup's own manifest, never from the shared store's pointer directory
    // (every backup overwrites that directory, so an older backup read from it
    // would publish the newest pointers over its own database). Reading it here
    // means a backup without one is refused before anything is touched.
    let manifest_heads = match job.head_source {
        HeadSource::SourceStore => None,
        HeadSource::BackupManifest => Some(
            read_head_manifest(&manifest_path(job.from_db))
                .map_err(|error| anyhow!("{label}: {error}"))?,
        ),
    };

    refuse_orphaned_quarantines(job.db_target)?;

    // ---- Step 1: refuse a non-empty target; back it up first on --replace.
    if let Some(contents) = ops::target_row_counts(job.db_target)
        .await
        .map_err(|e| anyhow!("{e}"))?
    {
        if !job.replace {
            anyhow::bail!(
                "{label}: target {} is non-empty ({}); without --replace a non-empty target \
                 is refused",
                job.db_target.display(),
                contents.named_counts()
            );
        }
        let out_dir = default_backup_out_dir()?;
        // The SAME backup `pulse backup` makes of this target — its database
        // AND the candle snapshots of its data dir, into the out-dir's one
        // shared `candles/` store — so the backup named here can be restored
        // with `pulse restore --backup-dir <out-dir>`. A database-only copy
        // could not be.
        let backup = super::backup::backup_target(job.db_target, job.data_target, &out_dir).await?;
        println!(
            "{label}: previous target backed up to {} (database + {} snapshot(s) in {}/candles)",
            backup.path.display(),
            backup.snapshots_total,
            out_dir.display()
        );
    }

    // ---- Step 2: a consistent copy of the source, beside the target (the
    // same filesystem, under the target's directory, never /tmp). The created
    // root this run's syncs must cover was computed before the lock.
    if let Some(parent) = job.db_target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create target directory {}: {e}", parent.display()))?;
    }
    let tmp_db = temp_db_path(job.db_target, label);
    let source = ops::open_read_only(job.from_db)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let copied = ops::vacuum_into_copy(&source, &tmp_db).await;
    source.close().await;
    if let Err(error) = copied {
        return Err(copy_failure(error, &tmp_db));
    }
    // `VACUUM INTO` does not guarantee its output is on disk, and the install
    // renames this file: the bytes must land before the name that promises them
    // (fix round 1, F5).
    if let Err(error) = publish::sync_file(&tmp_db, job.db_target) {
        // The temporary's bytes are not confirmed on disk, so it must not survive
        // this run (fix round 2, N4): nothing has been installed yet.
        let cleanup = remove_tmp_db(&tmp_db);
        return Err(fold_cleanup(
            anyhow!("flush the copied database: {error}"),
            cleanup,
        ));
    }

    // The `cfg(test)` seam this module's own tests drive (the `publish::probe`
    // spirit): a writer that lands AFTER the copy — whose read snapshot is
    // closed here — and before the verification reads. That writer is the paper
    // digest check's real trigger, and no black-box test can schedule it, so
    // the test hands the engine the mutation to await.
    #[cfg(test)]
    if let Some(mutate) = take_source_mutation()
        && let Err(error) = mutate().await
    {
        let cleanup = remove_tmp_db(&tmp_db);
        return Err(fold_cleanup(
            anyhow!("the injected source mutation failed: {error}"),
            cleanup,
        ));
    }

    // ---- Steps 3–5: migrate the copy forward, copy the snapshots, verify.
    // The copy is opened in ROLLBACK-JOURNAL mode (`open_migrated_copy`), never
    // WAL: the install renames this file and only this file, so a `-wal`/`-shm`
    // beside it would be left behind under a name the rename does not carry —
    // taking every row still committed in it (#258). In rollback-journal mode
    // there is no WAL to strand, and the mode is READ BACK when the copy is
    // opened (sqlx applies `PRAGMA journal_mode` without checking its result),
    // so it is a checked property rather than a request.
    let opened = match open_migrated_copy(&tmp_db).await {
        Ok(db) => db,
        Err(e) => {
            let cleanup = remove_tmp_db(&tmp_db);
            return Err(fold_cleanup(
                anyhow!("migrate the copy forward: {e}"),
                cleanup,
            ));
        }
    };
    let steps = run_steps(&job, &opened, manifest_heads.as_deref()).await;
    // CLOSE the copy's pool — do not merely drop the handle — before any file
    // surgery on either path. A dropped handle releases the pool without closing
    // its connections, and sqlx returns even a closed pool's in-flight
    // connections from spawned tasks, so no close here can be relied on to have
    // released the file by the time the rename runs. Closing is what the pool
    // owes the file; NOT being in WAL (`open_migrated_copy`) is what makes the
    // install safe when the close lands late, and `install_tmp_db` additionally
    // REFUSES to rename while any sidecar is still beside the copy.
    opened.pool().close().await;

    // Undoing a step's writes is always the same two moves: the files it added
    // and the store pointers it changed (the target must be left exactly as it
    // was, whichever path failed).
    let target_store = CandleStore::with_base_dir(job.data_target.to_path_buf());
    let undo = |writes: &StoreWrites| unwind_store_writes(&target_store, writes);
    match steps {
        Err(StepFailure::Fatal { error, writes }) => {
            // Every fatal path cleans up BOTH: the temporary database and
            // everything this run wrote to the target store before it failed.
            let cleanup = remove_tmp_db(&tmp_db);
            let undone = undo(&writes);
            Err(fold_cleanup(fold_undo(error, &undone), cleanup))
        }
        Err(StepFailure::Mismatches { mismatches, writes }) => {
            let cleanup = remove_tmp_db(&tmp_db);
            let undone = undo(&writes);
            print_refusal(label, &mismatches);
            Err(fold_cleanup(refusal_error(label, &undone), cleanup))
        }
        Ok((summary, writes)) => {
            // ---- Step 7: the install. Once its rename has landed the database
            // IS in place, and a failure after that point is NOT the pre-rename
            // failure the cleanup below handles (fix round 1, F1): deleting the
            // snapshots this run copied or rewinding a HEAD pointer would leave
            // the installed database pointing at rows that are gone. Only
            // `Untouched` — nothing renamed — may run the undo.
            match install_tmp_db(&tmp_db, job.db_target, created_root.as_deref()).await {
                Ok(()) => {}
                Err(InstallFailure::Untouched(error)) => {
                    let cleanup = remove_tmp_db(&tmp_db);
                    let undone = undo(&writes);
                    return Err(fold_cleanup(fold_undo(error, &undone), cleanup));
                }
                Err(failure) => return Err(failure.into_error()),
            }
            if job.chmod_source {
                set_read_only(job.from_db)?;
                println!("{label}: set read-only (a-w): {}", job.from_db.display());
            }
            print_summary(label, &job, &summary);
            Ok(())
        }
    }
}

/// Steps 4–5 on the migrated copy: the snapshot and pointer copy, and the five
/// checks.
///
/// Returns the summary AND what this run wrote to the target store (snapshots
/// added, pointers moved), so the caller's success path can undo it if the
/// install itself fails. Every fatal path carries those writes out with the
/// error (see [`fatal`]).
async fn run_steps(
    job: &VerifiedCopy<'_>,
    copied: &Db,
    manifest_heads: Option<&[SourceHead]>,
) -> Result<(Summary, StoreWrites), StepFailure> {
    let copy_pool = copied.pool();
    let mut mismatches: Vec<Mismatch> = Vec::new();

    // ---- Step 4: copy every source snapshot; an existing same-named file
    // must be byte-identical (snapshots are immutable and content-addressed).
    let target_store = CandleStore::with_base_dir(job.data_target.to_path_buf());
    let (source_snapshots, scan_issues) = scan_snapshots(job.from_data_dir);
    mismatches.extend(scan_issues);
    // The store's HEAD pointers are NOT snapshots (the scan steps over
    // everything that is not a `.parquet` file) and they are read here so the
    // last step can publish them with the snapshots they name. An IMPORT reads
    // them from the source store; a RESTORE uses the chosen backup's manifest
    // (read by the caller), never the shared store's pointer directory.
    let (source_heads, head_issues) = match manifest_heads {
        Some(heads) => (heads.to_vec(), Vec::new()),
        None => scan_heads(job.from_data_dir),
    };
    mismatches.extend(head_issues);
    let mut writes = StoreWrites::default();
    // `copy_snapshots` adds files as it goes and can fail part-way: the error
    // carries whatever it had added by then.
    writes.added = match copy_snapshots(&target_store, &source_snapshots, &mut mismatches) {
        Ok(added) => added,
        Err((error, added)) => {
            let writes = StoreWrites {
                added,
                heads: Vec::new(),
            };
            return Err(fatal(error, &writes));
        }
    };

    // ---- Step 5: verify everything, and do not stop at the first failure
    // within a check. Every step below can fail AFTER the target was written
    // to, so each fatal path hands the writes to the cleanup.
    let source = ops::open_read_only(job.from_db)
        .await
        .map_err(|e| fatal(anyhow!("{e}"), &writes))?;
    let tables = step_table_counts(&source, copy_pool, &mut mismatches)
        .await
        .map_err(|e| fatal(e, &writes))?;
    step_stored_hashes(&source, copy_pool, &mut mismatches)
        .await
        .map_err(|e| fatal(e, &writes))?;
    let paper_digests = step_paper_digests(&source, copy_pool, &mut mismatches)
        .await
        .map_err(|e| fatal(e, &writes))?;
    let (versions_verified, runs_verified) = step_repository_reads(copy_pool, &mut mismatches)
        .await
        .map_err(|e| fatal(e, &writes))?;
    let snapshots_verified = step_snapshot_reads(&target_store, &source_snapshots, &mut mismatches);
    step_referenced_snapshots(&target_store, copy_pool, &mut mismatches)
        .await
        .map_err(|e| fatal(e, &writes))?;
    source.close().await;

    // ---- The pointers LAST, after every read-back above: a pointer is only
    // published once the snapshot it names is verified to be there.
    writes.heads = publish_heads(
        &target_store,
        job.data_target,
        &source_heads,
        &mut mismatches,
    );

    if !mismatches.is_empty() {
        return Err(StepFailure::Mismatches { mismatches, writes });
    }
    let mut table_counts = Vec::with_capacity(tables.len());
    for table in &tables {
        let count = ops::table_count(copy_pool, table)
            .await
            .map_err(|e| fatal(anyhow!("{e}"), &writes))?;
        table_counts.push((table.clone(), count));
    }
    Ok((
        Summary {
            table_counts,
            versions_verified,
            runs_verified,
            snapshots_verified,
            paper_digests,
        },
        writes,
    ))
}

/// Step 5a: per-table row counts, copy versus source. The list is every table
/// the SOURCE has (never a hand-picked one); `_sqlx_migrations` is the one
/// exclusion, recorded in the plan gate: a copy migrated forward necessarily
/// holds more migration rows than its behind-source.
async fn step_table_counts(
    source: &sqlx::SqlitePool,
    copy_pool: &sqlx::SqlitePool,
    mismatches: &mut Vec<Mismatch>,
) -> Result<Vec<String>, anyhow::Error> {
    let tables = ops::table_names(source).await.map_err(|e| anyhow!("{e}"))?;
    for table in &tables {
        if table == "_sqlx_migrations" {
            continue;
        }
        let source_count = ops::table_count(source, table)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let copy_count = ops::table_count(copy_pool, table)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        if source_count != copy_count {
            mismatches.push(Mismatch::new(
                table,
                "(table)",
                "row_count",
                format!("source {source_count} rows, copy {copy_count} rows"),
            ));
        }
    }
    Ok(tables)
}

/// Step 5b: every stored hash equals the source row's, keyed by id.
async fn step_stored_hashes(
    source: &sqlx::SqlitePool,
    copy_pool: &sqlx::SqlitePool,
    mismatches: &mut Vec<Mismatch>,
) -> Result<(), anyhow::Error> {
    let source_versions = ops::stored_version_hashes(source)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let copy_versions = ops::stored_version_hashes(copy_pool)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    push_hash_mismatches(
        "strategy_version",
        "version_hash",
        source_versions,
        copy_versions,
        mismatches,
    );
    let source_runs = ops::stored_run_hashes(source)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let copy_runs = ops::stored_run_hashes(copy_pool)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    push_hash_mismatches(
        "backtest_run",
        "result_content_hash",
        source_runs,
        copy_runs,
        mismatches,
    );
    Ok(())
}

/// One direction of the stored-hash comparison, keyed by id (a row missing
/// from either side is a mismatch too).
fn push_hash_mismatches(
    table: &str,
    field: &str,
    source_rows: Vec<(String, String)>,
    copy_rows: Vec<(String, String)>,
    out: &mut Vec<Mismatch>,
) {
    let source_map: HashMap<String, String> = source_rows.into_iter().collect();
    let copy_map: HashMap<String, String> = copy_rows.into_iter().collect();
    for (id, source_hash) in &source_map {
        match copy_map.get(id) {
            Some(copy_hash) if copy_hash == source_hash => {}
            Some(copy_hash) => out.push(Mismatch::new(
                table,
                id.clone(),
                field,
                format!("stored {source_hash} in the source, {copy_hash} in the copy"),
            )),
            None => out.push(Mismatch::new(
                table,
                id.clone(),
                field,
                "row missing from the copy",
            )),
        }
    }
    for id in copy_map.keys() {
        if !source_map.contains_key(id) {
            out.push(Mismatch::new(
                table,
                id.clone(),
                field,
                "row the source does not have",
            ));
        }
    }
}

/// Step 5b′ (r4.s2.w2): the three paper tables compared by CONTENT DIGEST, not
/// only by row count — every row's columns, in primary-key order, hashed on
/// both sides. A difference refuses the import, naming the table and the first
/// differing key; on success the digests ride the summary and are printed.
///
/// The check is the "the paper state moved exactly" half of the move made safe
/// (spec §Approach 3): a changed column, a missing row or a reordered schema is
/// invisible to a count and visible here.
async fn step_paper_digests(
    source: &sqlx::SqlitePool,
    copy_pool: &sqlx::SqlitePool,
    mismatches: &mut Vec<Mismatch>,
) -> Result<Vec<ops::PaperTableDigest>, anyhow::Error> {
    let mut digests = Vec::with_capacity(ops::PAPER_TABLES.len());
    for (table, _) in ops::PAPER_TABLES {
        let digest = ops::paper_table_digest(source, copy_pool, table)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        if let Some(key) = &digest.first_difference {
            mismatches.push(Mismatch::new(
                table,
                key.clone(),
                "digest",
                format!(
                    "the paper digest differs from the source ({} row(s); source {} vs copy {})",
                    digest.rows, digest.source_digest, digest.target_digest
                ),
            ));
        }
        digests.push(digest);
    }
    Ok(digests)
}

/// Step 5c: every version and every run reads back through its repository on
/// the COPY — the tamper defenses (`version_hash` re-derived on read; #39's
/// `result_content_hash` re-derived from the trades).
pub(crate) async fn step_repository_reads(
    copy_pool: &sqlx::SqlitePool,
    mismatches: &mut Vec<Mismatch>,
) -> Result<(usize, usize), anyhow::Error> {
    let version_ids = ops::all_version_ids(copy_pool)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let strat_repo = SqliteStrategyRepo::new(copy_pool.clone());
    let mut versions_verified = 0usize;
    for id in &version_ids {
        match strat_repo.get_version(&VersionId::new(id.clone())).await {
            Ok(Some(_)) => versions_verified += 1,
            Ok(None) => mismatches.push(Mismatch::new(
                "strategy_version",
                id.clone(),
                "version_hash",
                "the repository read returned no row",
            )),
            Err(e) => mismatches.push(Mismatch::new(
                "strategy_version",
                id.clone(),
                "version_hash",
                format!("the repository read rejected it: {e}"),
            )),
        }
    }
    let run_ids = ops::all_run_ids(copy_pool)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let run_repo = SqliteBacktestRunRepo::new(copy_pool.clone());
    let mut runs_verified = 0usize;
    for id in &run_ids {
        match run_repo.get_run(&BacktestRunId::new(id.clone())).await {
            Ok(Some(_)) => runs_verified += 1,
            Ok(None) => mismatches.push(Mismatch::new(
                "backtest_run",
                id.clone(),
                "result_content_hash",
                "the repository read returned no row",
            )),
            Err(e) => mismatches.push(Mismatch::new(
                "backtest_run",
                id.clone(),
                "result_content_hash",
                format!("the repository read rejected it: {e}"),
            )),
        }
    }
    Ok((versions_verified, runs_verified))
}

/// Step 5d: every copied snapshot reads back through `CandleStore` in the
/// TARGET (embedded provenance plus the re-derived `data_version`).
pub(crate) fn step_snapshot_reads(
    target_store: &CandleStore,
    source_snapshots: &[SourceSnapshot],
    mismatches: &mut Vec<Mismatch>,
) -> usize {
    let mut snapshots_verified = 0usize;
    for snap in source_snapshots {
        let path = target_store.snapshot_path(&snap.pair, snap.timeframe, &snap.version);
        match target_store.read_snapshot(&snap.pair, snap.timeframe, &snap.version) {
            Ok(_) => snapshots_verified += 1,
            Err(e) => mismatches.push(Mismatch::new(
                "snapshot",
                path.display().to_string(),
                "data_version",
                format!("the read-back rejected it: {e}"),
            )),
        }
    }
    snapshots_verified
}

/// Step 5e: every `data_version` a run references (primary and HTF) exists as
/// a verified snapshot in the target.
pub(crate) async fn step_referenced_snapshots(
    target_store: &CandleStore,
    copy_pool: &sqlx::SqlitePool,
    mismatches: &mut Vec<Mismatch>,
) -> Result<(), anyhow::Error> {
    for reference in ops::referenced_snapshots(copy_pool)
        .await
        .map_err(|e| anyhow!("{e}"))?
    {
        let pair = Pair::parse(&reference.pair);
        let timeframe = timeframe_from_interval(&reference.timeframe);
        let version = DataVersion::parse(&reference.data_version);
        match (pair, timeframe, version) {
            (Ok(pair), Some(timeframe), Ok(version)) => {
                if let Err(e) = target_store.read_snapshot(&pair, timeframe, &version) {
                    mismatches.push(Mismatch::new(
                        "backtest_run",
                        reference.data_version.clone(),
                        "data_version",
                        format!(
                            "the referenced snapshot is missing or unverified in the target: {e}"
                        ),
                    ));
                }
            }
            _ => mismatches.push(Mismatch::new(
                "backtest_run",
                reference.data_version.clone(),
                "data_version",
                format!(
                    "the reference does not parse: pair {:?}, timeframe {:?}",
                    reference.pair, reference.timeframe
                ),
            )),
        }
    }
    Ok(())
}

/// One snapshot under the source's candle store.
pub(crate) struct SourceSnapshot {
    /// The pair directory it lives under.
    pub pair: Pair,
    /// The timeframe directory it lives under.
    pub timeframe: Timeframe,
    /// The content-hash identity from the file stem.
    pub version: DataVersion,
    /// The absolute source path.
    pub path: PathBuf,
}

/// Walk `<data>/candles/<PAIR>/<TF>/*.parquet` — every snapshot the source
/// holds, plus the layout problems as mismatches (the caller decides whether
/// those refuse an import or fail a backup).
/// Every `(pair, timeframe)` directory a store holds, with the layout
/// mismatches a broken one produces.
///
/// `kind` labels the mismatches (`snapshot` for the snapshot scan, `head` for
/// the pointer scan) — the two scans walk the SAME directories on purpose, so
/// they cannot disagree about what the store holds.
fn store_dirs(
    data_dir: &Path,
    kind: &'static str,
) -> (Vec<(Pair, Timeframe, PathBuf)>, Vec<Mismatch>) {
    let mut out = Vec::new();
    let mut issues = Vec::new();
    let candles = data_dir.join("candles");
    let pair_entries = match fs::read_dir(&candles) {
        Ok(entries) => entries,
        // A MISSING candles/ root is an EMPTY store, not a layout problem:
        // every fresh install (a migrated database with no candles fetched yet)
        // has one, and its nightly backup must still write the database.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (out, issues),
        // Any other read error is a refusal: the store is there and cannot be
        // read, which no backup may paper over.
        Err(_) => {
            issues.push(Mismatch::new(
                kind,
                candles.display().to_string(),
                "layout",
                "the candle store directory is unreadable",
            ));
            return (out, issues);
        }
    };
    for pair_entry in pair_entries.flatten() {
        if !pair_entry.path().is_dir() {
            continue;
        }
        let pair_name = pair_entry.file_name().to_string_lossy().to_string();
        let Ok(pair) = Pair::parse(&pair_name) else {
            issues.push(Mismatch::new(
                kind,
                pair_name,
                "layout",
                "not a valid pair directory",
            ));
            continue;
        };
        let Ok(tf_entries) = fs::read_dir(pair_entry.path()) else {
            issues.push(Mismatch::new(
                kind,
                pair_name,
                "layout",
                "unreadable timeframe directory",
            ));
            continue;
        };
        for tf_entry in tf_entries.flatten() {
            if !tf_entry.path().is_dir() {
                continue;
            }
            let tf_name = tf_entry.file_name().to_string_lossy().to_string();
            let Some(timeframe) = timeframe_from_interval(&tf_name) else {
                issues.push(Mismatch::new(
                    kind,
                    tf_name,
                    "layout",
                    "not a known timeframe directory (15m / 4h)",
                ));
                continue;
            };
            out.push((pair.clone(), timeframe, tf_entry.path()));
        }
    }
    (out, issues)
}

pub(crate) fn scan_snapshots(data_dir: &Path) -> (Vec<SourceSnapshot>, Vec<Mismatch>) {
    let (dirs, mut issues) = store_dirs(data_dir, "snapshot");
    let mut out = Vec::new();
    for (pair, timeframe, dir) in dirs {
        {
            let Ok(files) = fs::read_dir(&dir) else {
                issues.push(Mismatch::new(
                    "snapshot",
                    dir.display().to_string(),
                    "layout",
                    "unreadable snapshot directory",
                ));
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                // HEAD pointers and strays are not snapshots: the store is
                // `<version>.parquet` files only.
                if !path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("parquet"))
                {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    issues.push(Mismatch::new(
                        "snapshot",
                        path.display().to_string(),
                        "layout",
                        "unreadable snapshot file name",
                    ));
                    continue;
                };
                match DataVersion::parse(stem) {
                    Ok(version) => out.push(SourceSnapshot {
                        pair: pair.clone(),
                        timeframe,
                        version,
                        path,
                    }),
                    Err(e) => issues.push(Mismatch::new(
                        "snapshot",
                        path.display().to_string(),
                        "layout",
                        format!("bad snapshot identity: {e}"),
                    )),
                }
            }
        }
    }
    (out, issues)
}

/// One `HEAD` pointer in a store: the `(pair, timeframe)` it belongs to, the
/// `data_version` it names, and the file itself.
#[derive(Debug, Clone)]
pub(crate) struct SourceHead {
    /// The pair directory it lives under.
    pub pair: Pair,
    /// The timeframe directory it lives under.
    pub timeframe: Timeframe,
    /// The data version it names.
    pub version: DataVersion,
    /// The `HEAD` file.
    pub path: PathBuf,
}

/// One backup's pointer manifest: the `HEAD` pointers its database was frozen
/// with, written beside the backup database (round 6, Fix A).
///
/// It exists because the out-dir's store holds ONE shared pointer set that every
/// backup overwrites: with two retained backups restoring the OLDER database
/// would otherwise find the shared directory's newest pointers, validate them
/// (their snapshots are all still present) and publish them — silently
/// combining an older database with newer current-candle pointers.
#[derive(Debug, Serialize, Deserialize)]
struct HeadManifest {
    /// The manifest's own shape, so a future change can refuse an old file
    /// loudly instead of misreading it.
    version: u32,
    /// The pointers, in the order the source store held them.
    heads: Vec<ManifestHead>,
}

/// One pointer inside a [`HeadManifest`], as the store spells it on the wire.
#[derive(Debug, Serialize, Deserialize)]
struct ManifestHead {
    /// The pair directory.
    pair: String,
    /// The timeframe directory (`15m` / `4h`).
    timeframe: String,
    /// The `data_version` the pointer names.
    data_version: String,
}

/// The manifest's shape this build writes and understands.
const MANIFEST_VERSION: u32 = 1;

/// Where one backup's manifest lives: beside its database, named after it.
pub(crate) fn manifest_path(backup_db: &Path) -> PathBuf {
    let name = backup_db
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("pulse-backup.db");
    backup_db.with_file_name(format!("{name}.heads.json"))
}

/// Publish `bytes` at `path` ATOMICALLY — the same temp→fsync→rename discipline
/// the snapshot copies use, so a reader never sees a half-written file and a
/// crash never leaves a partial one under the final name — and fsync the
/// directory it landed in, so the published name itself is durable (issue #259:
/// a backup's database must not outlive the manifest and snapshots it
/// references).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] when the temporary cannot be written or
/// flushed, when the rename fails (the temporary is removed), or when the
/// directory cannot be fsynced.
fn write_file_atomic(path: &Path, bytes: &[u8], created: Option<&Path>) -> anyhow::Result<()> {
    let temporary = temporary_sibling(path)?;
    let _ = fs::remove_file(&temporary);
    fs::write(&temporary, bytes).map_err(|e| anyhow!("write {}: {e}", temporary.display()))?;
    let flushed = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temporary)
        .and_then(|handle| handle.sync_all());
    if let Err(error) = flushed {
        let _ = fs::remove_file(&temporary);
        return Err(anyhow!("flush {}: {error}", temporary.display()));
    }
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        anyhow!(
            "install {} -> {}: {error}",
            temporary.display(),
            path.display()
        )
    })?;
    publish::sync_published(path, created)
}

/// Write one backup's pointer manifest beside it (atomically: the same
/// discipline the rest of the surface uses).
pub(crate) fn write_head_manifest(
    path: &Path,
    heads: &[SourceHead],
    created: Option<&Path>,
) -> anyhow::Result<()> {
    let manifest = HeadManifest {
        version: MANIFEST_VERSION,
        heads: heads
            .iter()
            .map(|head| ManifestHead {
                pair: head.pair.as_str().to_owned(),
                timeframe: head.timeframe.binance_interval().to_owned(),
                data_version: head.version.to_string(),
            })
            .collect(),
    };
    let bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| anyhow!("serialize the HEAD manifest: {e}"))?;
    write_file_atomic(path, &bytes, created)
}

/// Read one backup's pointer manifest — the ONLY pointer source a restore uses.
///
/// # Errors
///
/// A named refusal for a manifest that is absent, unreadable, corrupt, written
/// by a shape this build does not know, or carrying an entry that does not parse
/// as a `(pair, timeframe, data_version)`: the shared store's pointers are never
/// guessed in its place.
pub(crate) fn read_head_manifest(path: &Path) -> anyhow::Result<Vec<SourceHead>> {
    let bytes = fs::read(path).map_err(|error| {
        anyhow!(
            "the backup has no readable HEAD-pointer manifest at {} ({error}); refusing rather \
             than guessing the shared store's pointers",
            path.display()
        )
    })?;
    let manifest: HeadManifest = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow!(
            "the HEAD-pointer manifest {} is unreadable ({error}); refusing rather than guessing",
            path.display()
        )
    })?;
    if manifest.version != MANIFEST_VERSION {
        anyhow::bail!(
            "the HEAD-pointer manifest {} is shape {} and this build writes {MANIFEST_VERSION}; \
             refusing rather than guessing",
            path.display(),
            manifest.version
        );
    }
    let mut heads = Vec::with_capacity(manifest.heads.len());
    for entry in &manifest.heads {
        let pair = Pair::parse(&entry.pair).map_err(|e| {
            anyhow!(
                "the manifest {} names an unreadable pair: {e}",
                path.display()
            )
        })?;
        let timeframe = timeframe_from_interval(&entry.timeframe).ok_or_else(|| {
            anyhow!(
                "the manifest {} names an unknown timeframe {:?}",
                path.display(),
                entry.timeframe
            )
        })?;
        let version = DataVersion::parse(&entry.data_version).map_err(|e| {
            anyhow!(
                "the manifest {} names an unreadable data_version: {e}",
                path.display()
            )
        })?;
        heads.push(SourceHead {
            pair,
            timeframe,
            version,
            path: path.to_path_buf(),
        });
    }
    Ok(heads)
}

/// Every `HEAD` pointer in `data_dir`, with the mismatches a broken one makes.
///
/// [`scan_snapshots`] walks the store's `.parquet` files and steps over
/// everything else — HEAD pointers included, which is exactly how a copy left
/// them behind. The pointer is the store's authoritative "current snapshot"
/// (audit C6): a target without it resolves no current data at all, and a target
/// with a STALE one silently selects the pre-import dataset.
pub(crate) fn scan_heads(data_dir: &Path) -> (Vec<SourceHead>, Vec<Mismatch>) {
    let (dirs, mut issues) = store_dirs(data_dir, "head");
    let store = CandleStore::with_base_dir(data_dir.to_path_buf());
    let mut out = Vec::new();
    for (pair, timeframe, _dir) in dirs {
        let path = store.head_path(&pair, timeframe);
        if !path.exists() {
            continue; // no pointer yet: a first run, not a fault
        }
        match store.read_head(&pair, timeframe) {
            Ok(Some(version)) => out.push(SourceHead {
                pair,
                timeframe,
                version,
                path,
            }),
            // Vanished between the check and the read: treated as absent.
            Ok(None) => {}
            Err(error) => issues.push(Mismatch::new(
                "head",
                path.display().to_string(),
                "read",
                format!("the HEAD pointer does not read: {error}"),
            )),
        }
    }
    (out, issues)
}

/// The timeframe directory name (`15m` / `4h` / `1d`) a snapshot lives under.
///
/// r3.s2.w4: a backup taken from a store the daily series landed in carries
/// `candles/<PAIR>/1d/`, and `0017`'s runs reference those snapshots — so the
/// name must resolve here or a restore would report the daily snapshot as an
/// unknown-timeframe issue it must skip.
fn timeframe_from_interval(name: &str) -> Option<Timeframe> {
    match name {
        "15m" => Some(Timeframe::M15),
        "4h" => Some(Timeframe::H4),
        "1d" => Some(Timeframe::D1),
        _ => None,
    }
}

/// What one published `HEAD` pointer replaced, so a later failure can put the
/// store back exactly as it was.
///
/// Shared with `cli/backup.rs`: its store publishing writes the same pointers,
/// and its failure path undoes them the same way.
#[derive(Clone)]
pub(crate) struct HeadChange {
    /// The pair whose pointer changed.
    pair: Pair,
    /// The timeframe whose pointer changed.
    timeframe: Timeframe,
    /// The pointer's previous target, or `None` when there was none.
    previous: Option<DataVersion>,
}

impl HeadChange {
    /// Record one pointer's previous target.
    pub(crate) fn new(pair: Pair, timeframe: Timeframe, previous: Option<DataVersion>) -> Self {
        Self {
            pair,
            timeframe,
            previous,
        }
    }
}

/// Publish the source store's `HEAD` pointers into the target, atomically, and
/// drop the target pointers the source does not have.
///
/// Ordering IS the contract: each pointer goes through the store's own
/// temp→fsync→rename ([`CandleStore::write_head`]) and only AFTER the snapshot
/// it names is present AND verifies in the target — so there is never a window
/// in which a target pointer names a snapshot that is not there. A target
/// pointer the source does not have names the pre-import dataset (a `--replace`
/// replaces the database while the store's snapshots survive), so it is removed
/// rather than left to select it.
///
/// Returns the changes, for the cleanup a later failure runs.
fn publish_heads(
    target_store: &CandleStore,
    target_dir: &Path,
    source_heads: &[SourceHead],
    mismatches: &mut Vec<Mismatch>,
) -> Vec<HeadChange> {
    let mut changes: Vec<HeadChange> = Vec::new();
    for head in source_heads {
        // The snapshot must be there and verify BEFORE its pointer is published.
        if let Err(error) = target_store.read_snapshot(&head.pair, head.timeframe, &head.version) {
            mismatches.push(Mismatch::new(
                "head",
                head.path.display().to_string(),
                "snapshot",
                format!(
                    "the HEAD pointer names {} and the target does not verify it: {error}",
                    head.version
                ),
            ));
            continue;
        }
        let previous = target_store
            .read_head(&head.pair, head.timeframe)
            .unwrap_or(None);
        changes.push(HeadChange {
            pair: head.pair.clone(),
            timeframe: head.timeframe,
            previous,
        });
        if let Err(error) = target_store.write_head(&head.pair, head.timeframe, &head.version) {
            mismatches.push(Mismatch::new(
                "head",
                head.path.display().to_string(),
                "write",
                format!("the HEAD pointer could not be published: {error}"),
            ));
        }
    }

    // Pointers the source does not have. The target's own layout problems are
    // not the import's business here (its store is whatever it is), so only the
    // directories come from the walk.
    let (target_dirs, _target_issues) = store_dirs(target_dir, "head");
    for (pair, timeframe, _dir) in target_dirs {
        if source_heads
            .iter()
            .any(|head| head.pair == pair && head.timeframe == timeframe)
        {
            continue;
        }
        let path = target_store.head_path(&pair, timeframe);
        if !path.exists() {
            continue;
        }
        let previous = target_store.read_head(&pair, timeframe).unwrap_or(None);
        changes.push(HeadChange {
            pair: pair.clone(),
            timeframe,
            previous,
        });
        if let Err(error) = fs::remove_file(&path) {
            mismatches.push(Mismatch::new(
                "head",
                path.display().to_string(),
                "remove",
                format!("a stale HEAD pointer could not be removed: {error}"),
            ));
        }
    }
    changes
}

/// The error a refused run reports: what happened to the target, plus whatever
/// the undo could not put back ([`fold_undo`]).
///
/// Fix round 5, P1: the "every snapshot this run added were deleted … the target
/// is exactly as it was" claim is made ONLY when the undo actually finished —
/// otherwise the message says the target was NOT put back, and the undo below it
/// names what is still in place.
fn refusal_error(label: &str, undone: &anyhow::Result<()>) -> anyhow::Error {
    fold_undo(
        if undone.is_ok() {
            anyhow!(
                "{label}: refused — the temporary database and every snapshot this run added were \
                 deleted, every pointer it moved was put back, and the target is exactly as it was"
            )
        } else {
            anyhow!(
                "{label}: refused — the target was NOT put back exactly as it was; see the undo \
                 below for what is still in place"
            )
        },
        undone,
    )
}

/// The error an undone failure reports: the original failure, plus whatever the
/// store could not put back (fix round 3, M1 — the undo no longer swallows it) —
/// a pointer that stayed rewound, or (fix round 5, P1) a snapshot that could not
/// be deleted or whose removal could not be made durable.
fn fold_undo(error: anyhow::Error, undone: &anyhow::Result<()>) -> anyhow::Error {
    match undone {
        Ok(()) => error,
        Err(undo_error) => error.context(format!("[undo] {undo_error}")),
    }
}

/// The pointers a failed [`restore_heads`] left unrestored (fix round 3, M1 —
/// shared by the backup's unwind and, since fix round 4 Q5, the import's undo).
pub(crate) fn stranded_pointers(restored: &anyhow::Result<()>) -> Vec<(Pair, Timeframe)> {
    restored
        .as_ref()
        .err()
        .and_then(|error| error.downcast_ref::<HeadRestoreFailure>())
        .map(|failure| failure.stranded.clone())
        .unwrap_or_default()
}

/// Does a pointer that could NOT be restored still name `path`? The snapshots an
/// unrestored pointer names must stay: deleting them would leave the store
/// advertising a snapshot that is gone.
pub(crate) fn named_by_pointer(
    store: &CandleStore,
    stranded: &[(Pair, Timeframe)],
    path: &Path,
) -> bool {
    stranded.iter().any(|(pair, timeframe)| {
        store
            .head_path(pair, *timeframe)
            .parent()
            .is_some_and(|dir| path.starts_with(dir))
    })
}

/// Put back every pointer a publish step changed — the failure path: the store
/// must be left exactly as it was.
pub(crate) fn restore_heads(
    target_store: &CandleStore,
    changes: &[HeadChange],
) -> anyhow::Result<()> {
    let mut stranded: Vec<(Pair, Timeframe)> = Vec::new();
    for change in changes {
        if restore_one_head(target_store, change).is_err() {
            stranded.push((change.pair.clone(), change.timeframe));
        } else {
            #[cfg(test)]
            probe::record_rollback(probe::RollbackStep::HeadRestored {
                pair: change.pair.as_str().to_owned(),
                timeframe: change.timeframe.binance_interval().to_owned(),
            });
        }
    }
    if stranded.is_empty() {
        return Ok(());
    }
    Err(HeadRestoreFailure { stranded }.into())
}

/// The `HEAD` pointers [`restore_heads`] could not put back (fix round 3, M1).
///
/// A pointer left unrestored still names the snapshots this run published, so the
/// caller must NOT delete those — the store would be left naming a snapshot that
/// is gone.
#[derive(Debug)]
pub(crate) struct HeadRestoreFailure {
    /// The `(pair, timeframe)` of every pointer that is NOT back where it was.
    pub(crate) stranded: Vec<(Pair, Timeframe)>,
}

impl std::fmt::Display for HeadRestoreFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let named: Vec<String> = self
            .stranded
            .iter()
            .map(|(pair, timeframe)| format!("{} {}", pair.as_str(), timeframe.binance_interval()))
            .collect();
        write!(
            f,
            "the shared store's HEAD pointers could not be restored for {} — those pointers still \
             name the snapshots this run published",
            named.join(", ")
        )
    }
}

impl std::error::Error for HeadRestoreFailure {}

/// Put ONE pointer back to what it was.
fn restore_one_head(target_store: &CandleStore, change: &HeadChange) -> anyhow::Result<()> {
    let head = target_store.head_path(&change.pair, change.timeframe);
    let Some(version) = &change.previous else {
        // This run CREATED the pointer (the target had none), so putting the store
        // back means removing it — and that removal has to be as durable and as
        // reported as every other rollback removal (fix round 6, C3): a power loss
        // cannot resurrect a pointer to a snapshot this run deleted, and a failure
        // is named instead of dropped. The shared helpers do both.
        let mut removed: Vec<PathBuf> = Vec::new();
        let mut unremoved: Vec<String> = Vec::new();
        backup::unlink(&head, &mut removed, &mut unremoved);
        return backup::finish_head_removal(&removed, &unremoved);
    };
    #[cfg(test)]
    if probe::take_injected_head_write_failure(&head) {
        anyhow::bail!("injected HEAD write failure (cfg(test) seam)");
    }
    target_store
        .write_head(&change.pair, change.timeframe, version)
        .map_err(|error| anyhow!("write {}: {error}", head.display()))
}

/// Copy every source snapshot into the target store; an existing same-named
/// file must be byte-identical, or the mismatch refuses the import. Returns
/// the files this run added (for the failure cleanup).
///
/// # Errors
///
/// A copy failure returns the files added SO FAR beside the error: a fatal
/// failure part-way through must still leave the target exactly as it was.
fn copy_snapshots(
    target_store: &CandleStore,
    source_snapshots: &[SourceSnapshot],
    mismatches: &mut Vec<Mismatch>,
) -> Result<Added, (anyhow::Error, Added)> {
    let mut added: Added = Vec::new();
    let copied = (|| -> anyhow::Result<()> {
        for snap in source_snapshots {
            let dest = target_store.snapshot_path(&snap.pair, snap.timeframe, &snap.version);
            // Fail closed (fix round 11, R2): `exists()` would read a stat failure
            // as "absent", and this run would then RECORD the destination — which
            // the rollback deletes. A snapshot this run did not write must never be
            // in `added`, so an unreadable destination refuses the import instead.
            if path_present(&dest)? {
                if !bytes_equal(&snap.path, &dest) {
                    mismatches.push(Mismatch::new(
                        "snapshot",
                        dest.display().to_string(),
                        "bytes",
                        format!(
                            "an existing target snapshot differs from the source snapshot {} \
                             (equal names must mean equal bytes)",
                            snap.path.display()
                        ),
                    ));
                }
                continue;
            }
            // Recorded BEFORE the copy (fix round 1, F8): `copy_snapshot_into`
            // renames the file into place and then syncs its directory, so a
            // failure AFTER the rename still leaves a snapshot behind — one the
            // rollback must remove. The name is known free (the probe above), so
            // recording it up front cannot delete anything this run did not write.
            added.push(dest.clone());
            copy_snapshot_into(&snap.path, &dest)?;
        }
        Ok(())
    })();
    match copied {
        Ok(()) => Ok(added),
        Err(error) => Err((error, added)),
    }
}

/// Copy one file to its final destination — never writing a partial file UNDER
/// that name.
///
/// The destination name IS the snapshot's identity (immutable, content-
/// addressed), so a truncated file left there by a failure part-way through a
/// copy — `ENOSPC` is the likely case — is a snapshot no cleanup knows about: it
/// stays behind under a valid name, every later run refuses it as a byte
/// mismatch, and nothing can tell it apart from a deliberately replaced file.
/// The bytes therefore go to a temporary SIBLING, are flushed to the filesystem,
/// and only then is the temporary RENAMED onto the destination: the rename is
/// atomic within one directory, so the destination only ever holds a complete
/// file and a failure removes the temporary, leaving the destination exactly as
/// it was (absent, or whatever was there).
///
/// The rename is then made durable by fsyncing the directory it landed in — and
/// every level the copy's `create_dir_all` created below the deepest ancestor
/// that already existed, because the nested `<PAIR>/<TF>/` directories are
/// entries in THEIR parents too (issue #259: a backup must not keep a database
/// while losing a snapshot it references).
///
/// Shared with `cli/backup.rs`, so the backup store's copy is the same copy the
/// import makes.
///
/// # Errors
///
/// Any I/O failure — creating the destination's directory, copying, flushing,
/// renaming or fsyncing the directory it landed in — with the temporary removed.
/// A temporary left by an earlier crashed run is deleted first: its bytes are
/// not this run's. A failure AFTER the rename (the directory fsync) leaves the
/// published file in place and reports the error: the bytes are complete and
/// content-addressed, so the next run adopts them byte-for-byte, but this run
/// must not be reported as durable.
pub(crate) fn copy_snapshot_into(source: &Path, dest: &Path) -> anyhow::Result<()> {
    // The deepest ancestor that exists BEFORE the copy creates anything: the
    // levels below it are this copy's own directory entries (issue #259).
    let created = dest.parent().and_then(publish::existing_ancestor);
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create snapshot directory {}: {e}", parent.display()))?;
    }
    let temporary = temporary_sibling(dest)?;
    // Never reuse a temporary a crashed run left: those bytes are not ours.
    let _ = fs::remove_file(&temporary);
    fs::copy(source, &temporary).map_err(|e| {
        anyhow!(
            "copy snapshot {} -> {}: {e}",
            source.display(),
            temporary.display()
        )
    })?;
    // The bytes must be ON DISK before the name that promises them exists: the
    // rename publishes them, and a crash must not leave a durable name over
    // bytes that never landed. (Opened read+write so the flush is legal on every
    // platform, not just where `fsync` accepts a read-only handle.)
    if let Err(error) = publish::sync_file(&temporary, dest) {
        let _ = fs::remove_file(&temporary);
        return Err(anyhow!("flush {}: {error}", temporary.display()));
    }
    fs::rename(&temporary, dest).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        anyhow!(
            "install snapshot {} -> {}: {error}",
            temporary.display(),
            dest.display()
        )
    })?;
    publish::sync_published(dest, created.as_deref())
}

/// The temporary sibling of one content-addressed destination: its own name
/// plus `.partial` — a name no snapshot can hold (the store holds
/// `<version>.parquet` files, and every store walk skips anything else).
fn temporary_sibling(dest: &Path) -> anyhow::Result<PathBuf> {
    let name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("unusable snapshot path: {}", dest.display()))?;
    Ok(dest.with_file_name(format!("{name}.partial")))
}

/// Byte comparison of two files (a read failure counts as "not equal").
/// Shared with `cli/backup.rs`, so the backup store's existing-snapshot check
/// is the same comparison the import's copy makes.
pub(crate) fn bytes_equal(a: &Path, b: &Path) -> bool {
    match (fs::read(a), fs::read(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The hidden temporary database path BESIDE the target (same directory ⇒ the
/// install rename is atomic on the same filesystem; never `/tmp`). A bare
/// relative target keeps its temporary in the current directory (fix round 5, P4).
fn temp_db_path(target: &Path, label: &str) -> PathBuf {
    let dir = target_dir(target);
    let stem = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("pulse");
    dir.join(format!(".{stem}.{label}-tmp-{}.db", std::process::id()))
}

/// Delete the temporary database and EVERY sidecar a copy can have (failure
/// path) — the same suffix set the refusal checks, from the one constant, so a
/// sidecar can never be refused and then left behind (fix round 1, F7).
///
/// # Errors
///
/// Fix round 9, K4: every path is attempted and a removal that failed for any
/// reason but "already gone" is REPORTED — a temporary left under the copy's name
/// is what makes a same-pid retry hit an existing file.
fn remove_tmp_db(tmp_db: &Path) -> anyhow::Result<()> {
    let mut failures: Vec<String> = Vec::new();
    for path in std::iter::once(tmp_db.to_path_buf())
        .chain(COPY_SIDECAR_SUFFIXES.map(|suffix| sidecar_path(tmp_db, suffix)))
    {
        #[cfg(test)]
        let injected = probe::take_injected_remove_failure(&path);
        #[cfg(not(test))]
        let injected = false;
        let outcome = if injected {
            Err(std::io::Error::other("injected failure (cfg(test) seam)"))
        } else {
            fs::remove_file(&path)
        };
        match outcome {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => failures.push(format!("{}: {error}", path.display())),
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    Err(anyhow!(
        "the temporary database could not be fully removed — {}",
        failures.join("; ")
    ))
}

/// The failure of the copy step (fix round 9, K4): whatever `VACUUM INTO` wrote
/// before it failed is removed — under the temporary's own name, so a same-pid
/// retry would otherwise meet an existing file — and a cleanup that fails is
/// reported with it, never discarded.
fn copy_failure(error: impl std::fmt::Display, tmp_db: &Path) -> anyhow::Error {
    fold_cleanup(
        anyhow!("copy the source database: {error}"),
        remove_tmp_db(tmp_db),
    )
}

/// Fold a failed cleanup into the failure that caused it (fix round 9, K4), the
/// way [`fold_undo`] folds a failed undo: the report carries both, so "the
/// temporary is gone" is never claimed over a removal that failed.
pub(crate) fn fold_cleanup(error: anyhow::Error, cleanup: anyhow::Result<()>) -> anyhow::Error {
    match cleanup {
        Ok(()) => error,
        Err(cleanup_error) => error.context(format!("[cleanup] {cleanup_error}")),
    }
}

/// The import's unwind of everything a run wrote to the target store.
///
/// Fix round 4, Q5: the SAME order the backup's unwind uses (M1) — the pointers
/// go back FIRST, so no crash or error between the two steps can leave a pointer
/// naming a snapshot that is gone — and a pointer that could not be restored KEEPS
/// the snapshots it still names, because deleting them would leave the store in
/// exactly that state.
///
/// # Errors
///
/// Returns what [`restore_heads`] could not put back, and — since fix round 5, P1
/// — what [`remove_added_snapshots_keeping`] could not remove or make durable.
/// Both halves are reported: an undo that could not delete a snapshot must never
/// read as if every snapshot were gone.
fn unwind_store_writes(target_store: &CandleStore, writes: &StoreWrites) -> anyhow::Result<()> {
    let restored = restore_heads(target_store, &writes.heads);
    let stranded = stranded_pointers(&restored);
    let removed = remove_added_snapshots_keeping(&writes.added, &|path| {
        named_by_pointer(target_store, &stranded, path)
    });
    match (restored, removed) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(restore_error), Ok(())) => Err(restore_error),
        (Ok(()), Err(remove_error)) => Err(remove_error),
        // Both halves failed: the restore failure is the typed one the caller may
        // downcast, so it stays the outer error and the removal failure rides with it.
        (Err(restore_error), Err(remove_error)) => {
            Err(restore_error.context(format!("[undo] {remove_error}")))
        }
    }
}

/// Delete every snapshot this run added (failure path — the target keeps
/// exactly its previous contents), holding back the snapshots an unrestored
/// `HEAD` pointer still names (fix round 4, Q5).
///
/// The rollback ALWAYS runs with a keep predicate now: `remove_added_snapshots_keeping(added, &|_|
/// false)` is the no-keep form the tests use.
///
/// # Errors
///
/// Fix round 5, P1: a snapshot that could not be removed, and a removal whose
/// directory fsync failed, are both reported (through [`backup::finish_removals`],
/// the same report the backup's rollback uses) instead of being dropped by
/// `let _ =`. An already-absent snapshot is fine.
fn remove_added_snapshots_keeping(
    added: &[PathBuf],
    keep: &dyn Fn(&Path) -> bool,
) -> anyhow::Result<()> {
    let mut removed: Vec<PathBuf> = Vec::new();
    let mut unremoved: Vec<String> = Vec::new();
    for path in added {
        if keep(path) {
            continue;
        }
        backup::unlink(path, &mut removed, &mut unremoved);
    }
    backup::finish_removals(&removed, &unremoved)
}

/// [`DB_STATE_SUFFIXES`] plus this crate's own copy temporary (`.partial`):
/// every suffix a file beside the temporary COPY can hold state in.
///
/// ONE source of truth (fix round 1, F7): the refusal checks these suffixes and
/// the cleanup removes these suffixes, so a suffix can never be checked but left
/// behind.
const COPY_SIDECAR_SUFFIXES: [&str; 4] = ["-wal", "-shm", "-journal", ".partial"];

/// Append a suffix to a path's FINAL component, through the raw `OsStr` — never
/// `display()`, which is lossy for a name that is not valid UTF-8 (the migration
/// protocol's own `sidecar_path` does the same; fix round 1, F7). Shared with the
/// backup's tests, which assert the name the refusal names (fix round 5, P5).
pub(crate) fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Why an install refused a temporary database (#258).
///
/// A database's committed rows can live OUTSIDE its file: SQLite keeps them in
/// a `-wal` (write-ahead log) or, mid-transaction, in a `-journal`, and the
/// install renames the database file and nothing else. A sidecar left beside
/// the copy therefore has no way to travel with it — the rename strands the
/// rows under a name no database owns any more — so the install refuses rather
/// than publishing a database missing its most recent commits.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InstallRefusal {
    /// A sidecar that holds (or may hold) commits the renamed file would not
    /// carry.
    #[error(
        "refusing to install {db}: the temporary database still has its {suffix} sidecar \
         {sidecar} beside it, and a rename carries the database file only — committed rows \
         would be stranded outside the installed database",
        db = .db.display(),
        sidecar = .sidecar.display()
    )]
    Sidecar {
        /// The temporary database that was not installed.
        db: PathBuf,
        /// The sidecar's suffix (`-wal`, `-shm`, `-journal`, `.partial`).
        suffix: &'static str,
        /// The sidecar itself.
        sidecar: PathBuf,
    },
    /// A sidecar's presence could not be ESTABLISHED (fix round 9, K2). The stat
    /// failed for a reason other than "not there", so whether the copy carries a
    /// sidecar is unknown — and an unreadable sidecar is not an absent one.
    #[error(
        "refusing to install {db}: cannot tell whether its {suffix} sidecar {sidecar} is there ({error}) — a sidecar that exists would be stranded by the rename",
        db = .db.display(),
        sidecar = .sidecar.display()
    )]
    Unreadable {
        /// The temporary database that was not installed.
        db: PathBuf,
        /// The sidecar's suffix (`-wal`, `-shm`, `-journal`, `.partial`).
        suffix: &'static str,
        /// The sidecar whose presence could not be checked.
        sidecar: PathBuf,
        /// Why the probe failed.
        error: String,
    },
}

/// Is `path` there — with a stat FAILURE distinguished from "not there" (fix
/// round 9, K2)?
///
/// `Path::exists()` answers `false` for every stat error, so a probe that decides
/// whether bytes would be stranded — or whether this run may RECORD a path as its
/// own to delete later — must not use it: an unreadable file is not an absent one.
/// Only a definite "not there" is `Ok(false)`. `pub(crate)` because both sides of
/// the store need it: the import's sidecar probes and snapshot dedup (K2, R2), and
/// the backup's `copy_wanted` (fix round 12, S1 — the same dedup against the
/// SHARED store, where a wrongly recorded path costs a previous backup's snapshot).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] naming the path when its metadata cannot be read.
pub(crate) fn path_present(path: &Path) -> anyhow::Result<bool> {
    #[cfg(test)]
    if probe::take_injected_stat_failure(path) {
        return Err(anyhow!(
            "cannot stat {}: injected failure (cfg(test) seam)",
            path.display()
        ));
    }
    path.try_exists()
        .map_err(|error| anyhow!("cannot stat {}: {error}", path.display()))
}

/// Refuse the install when any sidecar still sits beside the copy (#258).
///
/// Fail closed: the check deletes nothing and renames nothing, so the target is
/// left exactly as it was and the caller's failure path removes the temporary
/// (with its sidecars) for a clean retry. A sidecar whose presence cannot be
/// established refuses too (fix round 9, K2): an unreadable one is not an absent
/// one, and the rename would strand it.
///
/// # Errors
///
/// Returns [`InstallRefusal::Sidecar`] for the first sidecar found, and
/// [`InstallRefusal::Unreadable`] when one cannot be stat'ed.
fn refuse_stranded_sidecar(tmp_db: &Path) -> anyhow::Result<()> {
    for suffix in COPY_SIDECAR_SUFFIXES {
        let sidecar = sidecar_path(tmp_db, suffix);
        match path_present(&sidecar) {
            Ok(false) => {}
            Ok(true) => {
                return Err(InstallRefusal::Sidecar {
                    db: tmp_db.to_path_buf(),
                    suffix,
                    sidecar,
                }
                .into());
            }
            Err(error) => {
                return Err(InstallRefusal::Unreadable {
                    db: tmp_db.to_path_buf(),
                    suffix,
                    sidecar,
                    error: format!("{error:#}"),
                }
                .into());
            }
        }
    }
    Ok(())
}

/// The quarantine name of one of the target's sidecars: a hidden sibling that no
/// database name matches, unique to this process, and therefore inert even if a
/// crash leaves one behind.
///
/// A bare relative target (`--db pulse.db`) quarantines into the current
/// directory, which is where its sidecars are (fix round 5, P4). `inode` is the
/// identity the name records so a later run can tell whose bytes these are
/// (fix round 7, E3).
fn quarantine_path(
    target: &Path,
    suffix: &str,
    identity: TargetIdentity,
) -> anyhow::Result<PathBuf> {
    let name = target
        .file_name()
        .ok_or_else(|| anyhow!("unusable target path: {}", target.display()))?;
    Ok(target_dir(target).join(quarantine_name(name, suffix, identity.inode())))
}

/// The target's own sidecars, MOVED ASIDE for the duration of the install
/// (fix round 1, F3: findings L3 and L10).
///
/// The install replaces the whole database file, so the old target's `-wal`,
/// `-shm` and `-journal` must never be replayed into the new one — and a rename
/// that FAILS must not have stripped the old target of the commits they hold.
/// Renaming them aside answers both: the copy is renamed into a directory where
/// no sidecar of the old name can be replayed onto it, and a failed install puts
/// them straight back.
struct QuarantinedSidecars {
    /// `(quarantine, original)` for each sidecar that was moved.
    moved: Vec<(PathBuf, PathBuf)>,
}

impl QuarantinedSidecars {
    /// Move the target's existing sidecars aside (paths built from the raw
    /// `OsStr`, never through `display()`).
    ///
    /// # Errors
    ///
    /// Returns an [`anyhow::Error`] when a sidecar cannot be moved — the install
    /// has not renamed anything yet, so the caller reports it as untouched.
    fn take(target: &Path) -> anyhow::Result<Self> {
        let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
        // The identity a later run needs to tell whether these bytes belong to the
        // database that ends up at the target path (fix round 7, E3): the rename
        // preserves the inode, so a quarantine taken now matches only the database
        // that is there NOW.
        let identity = inode_of(target);
        for suffix in DB_STATE_SUFFIXES {
            // (the test seam records what was moved, below)
            let original = sidecar_path(target, suffix);
            match path_present(&original) {
                Ok(true) => {}
                Ok(false) => continue,
                // Fail closed (fix round 9, K2): a sidecar that cannot be stat'ed
                // might be holding rows, and this install would strip the target of
                // it — refuse instead of guessing that it is not there. And put
                // back what an EARLIER iteration already moved (fix round 10, L1):
                // this arm used to return here and leave those under their
                // quarantine names, which is the state N2 exists to prevent.
                Err(error) => {
                    return Err(Self { moved }.fail_after_move(
                        target,
                        anyhow!(
                            "cannot tell whether the target's {suffix} sidecar {} is there: \
                             {error:#}",
                            original.display()
                        ),
                    ));
                }
            }
            let quarantine = match quarantine_path(target, suffix, identity) {
                Ok(quarantine) => quarantine,
                // The one other error between the move and the push, folded through
                // the same restore (fix round 10, L1): "any error after a move" is
                // the invariant, not "the two arms we happened to think of".
                Err(error) => return Err(Self { moved }.fail_after_move(target, error)),
            };
            // Never reuse a quarantine a crashed run left: those bytes are not
            // this run's.
            let _ = fs::remove_file(&quarantine);
            if let Err(error) = move_aside(&original, &quarantine) {
                // Put back what THIS attempt already moved before failing (fix
                // round 2, N2): the install has not renamed the database, so the
                // target must be left exactly as it was — and whatever cannot be
                // put back is named for the operator (N5).
                let failure = anyhow!(
                    "quarantine the target's {suffix} sidecar {} -> {}: {error}",
                    original.display(),
                    quarantine.display()
                );
                return Err(Self { moved }.fail_after_move(target, failure));
            }
            moved.push((quarantine, original));
        }
        #[cfg(test)]
        probe::record_quarantine(
            &moved
                .iter()
                .map(|(_, original)| original.clone())
                .collect::<Vec<PathBuf>>(),
        );
        Ok(Self { moved })
    }

    /// Fail a `take` that may already have moved sidecars: put back what it moved
    /// — through [`Self::restore`], so the move-backs are fsynced (G1) — and fold a
    /// restore failure into `failure`, naming what could not be put back (N5).
    ///
    /// Every error between the first move and the last `moved.push` goes through
    /// here (fix round 10, L1). The invariant is the point: the caller reports an
    /// [`InstallFailure::Untouched`] — "the target is exactly as it was" — so a
    /// sidecar left under its quarantine name would make that report a lie, and
    /// the operator would have no reason to look for it.
    ///
    /// An empty `moved` needs no restore: [`Self::restore`] returns `Ok` without
    /// touching the filesystem or the directory sync, so a failure before the
    /// first move is reported exactly as it is.
    fn fail_after_move(self, target: &Path, failure: anyhow::Error) -> anyhow::Error {
        match self.restore(target) {
            Ok(()) => failure,
            Err(restore_error) => anyhow!(
                "{failure}; and sidecars this attempt had already moved could not be put back \
                 either: {restore_error}"
            ),
        }
    }

    /// Put every quarantined sidecar back: a failed install leaves the old
    /// target exactly as it was, un-checkpointed commits included.
    /// # Errors
    ///
    /// Returns an [`anyhow::Error`] naming EVERY quarantine that could not be
    /// moved back (fix round 2, N5) — a sidecar left under its quarantine name is
    /// bytes the target no longer has, and the operator has to move them back by
    /// hand.
    fn restore(&self, target: &Path) -> anyhow::Result<()> {
        let mut stranded: Vec<String> = Vec::new();
        for (quarantine, original) in &self.moved {
            if let Err(error) = move_aside(quarantine, original) {
                stranded.push(format!(
                    "{} (back to {}): {error}",
                    quarantine.display(),
                    original.display()
                ));
            }
        }
        let mut parts: Vec<String> = Vec::new();
        if !stranded.is_empty() {
            parts.push(format!(
                "the replaced database's sidecars could not be put back — {} — those files still \
                 hold the bytes; move them back by hand",
                stranded.join("; ")
            ));
        }
        // Fix round 7, G1: a move-back that a power loss can undo is not a move-back.
        // The caller reports "the target is exactly as it was" on Ok, so the renames
        // are fsynced first — and a failure is reported, never swallowed.
        if !self.moved.is_empty()
            && let Err(error) = sync_quarantine_dir(target)
        {
            parts.push(format!(
                "the sidecars were moved back but their directory could not be fsynced, so the \
                 move-backs are not confirmed durable: {error}"
            ));
        }
        if parts.is_empty() {
            return Ok(());
        }
        Err(anyhow!("{}", parts.join("; ")))
    }

    /// Remove the quarantined sidecars — the install landed, so their bytes
    /// belong to the database that was replaced — and fsync each directory so
    /// the removals are durable.
    ///
    /// # Errors
    ///
    /// Returns an [`anyhow::Error`] when a quarantined file cannot be removed or
    /// its directory cannot be synced. The install has already landed; the
    /// caller reports this as an installed-but-unconfirmed state, and the
    /// leftovers are inert (no database name matches them).
    fn discard(&self) -> anyhow::Result<()> {
        let mut failures: Vec<String> = Vec::new();
        for (quarantine, _) in &self.moved {
            if let Err(error) = remove_quarantine(quarantine) {
                failures.push(error);
            }
        }
        for dir in self.directories() {
            if let Err(error) = publish::sync_dir(&dir) {
                failures.push(format!("fsync {}: {error}", dir.display()));
            }
        }
        if failures.is_empty() {
            return Ok(());
        }
        Err(anyhow!(
            "the replaced database's quarantined sidecars could not all be cleaned up — {} — the \
             files are inert (no database name matches them), but they are still on disk",
            failures.join("; ")
        ))
    }

    /// Name every quarantine this take is still holding, with the name each one's
    /// bytes belong back at — for the one error that has to say what is still on
    /// disk (fix round 11, R1: the quarantines kept because the install's rename
    /// is not confirmed durable).
    fn left_in_place(&self) -> String {
        self.moved
            .iter()
            .map(|(quarantine, original)| {
                format!(
                    "{} (its bytes belong back at {})",
                    quarantine.display(),
                    original.display()
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// The distinct directories the quarantined files live in.
    fn directories(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        for (quarantine, _) in &self.moved {
            if let Some(dir) = quarantine.parent()
                && !dirs.iter().any(|seen| seen == dir)
            {
                dirs.push(dir.to_path_buf());
            }
        }
        dirs
    }
}

/// Unlink a quarantined sidecar, with the test seam at the raw `remove_file`.
fn remove_quarantine(path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if probe::take_injected_remove_failure(path) {
        return Err(format!(
            "remove {}: injected failure (cfg(test) seam)",
            path.display()
        ));
    }
    match fs::remove_file(path) {
        Ok(()) => {
            // Recorded (fix round 11, R1) so a test can assert WHERE this unlink
            // sits relative to the syncs: the install's rename has to be durable
            // before the old sidecars go.
            #[cfg(test)]
            probe::record(publish::probe::SyncKind::Remove, path, path);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove {}: {error}", path.display())),
    }
}

/// Rename one of the target's sidecars aside — or back.
///
/// Fix round 2, N5's test seam sits at the raw `rename`, so a restore that fails
/// is reproducible.
fn move_aside(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if probe::take_injected_rename_failure(to) {
        return Err(std::io::Error::other(
            "injected rename failure (cfg(test) seam)",
        ));
    }
    fs::rename(from, to)?;
    // Every rename this crate publishes is recorded (fix round 6, C1), so a test
    // can assert which syncs ran BEFORE one — the quarantine moves, the install's
    // rename and the sidecar restores share the one stream.
    #[cfg(test)]
    probe::record_rename(from, to);
    Ok(())
}

/// fsync the directory that holds the quarantined sidecars, and record it (fix
/// round 6, C1).
///
/// [`QuarantinedSidecars::take`] renames the target's `-wal`/`-shm`/`-journal`
/// aside; those renames have to be durable BEFORE the install's own rename lands,
/// or a power loss can publish the NEW database while the OLD sidecars are still
/// under their old names — the new file beside a stale hot journal is exactly the
/// replay the quarantine exists to prevent. [`publish::sync_dir`] is the shared
/// helper the other publish sites use for the same job.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] naming the directory when it cannot be opened or
/// synced; the caller then puts the sidecars back and leaves the target alone.
fn sync_quarantine_dir(target: &Path) -> anyhow::Result<()> {
    let dir = target_dir(target);
    publish::sync_dir(&dir)?;
    #[cfg(test)]
    probe::record(publish::probe::SyncKind::Dir, &dir, target);
    Ok(())
}

/// The install's outcome when it fails: WHAT the target is left as, so the
/// caller knows whether its pre-rename cleanup still applies (fix round 1, F1).
#[derive(Debug)]
pub(crate) enum InstallFailure {
    /// Nothing was renamed: the target is exactly as it was (its own sidecars
    /// put back), so the caller's pre-rename cleanup — delete the temporary,
    /// undo the store writes — still applies.
    Untouched(anyhow::Error),
    /// The rename LANDED: the target IS this run's database. A step after the
    /// rename failed, so the pre-rename cleanup must NOT run — it would delete
    /// the snapshots this run copied and rewind HEAD pointers the installed
    /// database now depends on.
    Installed {
        /// The database that IS in place.
        target: PathBuf,
        /// The failure, naming the installed state and what is unconfirmed.
        error: anyhow::Error,
    },
}

impl InstallFailure {
    /// The error the caller reports. An [`Self::Installed`] failure names the
    /// database that IS in place first, so the operator reads the state before
    /// the detail and never has to guess whether the target was replaced.
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Untouched(error) => error,
            Self::Installed { target, error } => anyhow!(
                "the database IS installed at {} but its publish is not confirmed: {error}",
                target.display()
            ),
        }
    }
}

/// Install the temporary database: refuse a copy with a sidecar left to strand,
/// move the target's own sidecars aside, rename the copy into place, make the
/// rename durable, drop what was moved aside, and put the installed file back in
/// WAL.
///
/// The durability step precedes the drop (fix round 11, R1): the old sidecars may
/// only be unlinked once the rename that replaced their database is on disk.
///
/// # Errors
///
/// [`InstallFailure::Untouched`] when nothing was renamed (the refusal, the
/// quarantine, or the rename itself failed — the target's sidecars are put
/// back), and [`InstallFailure::Installed`] when the rename landed and a later
/// step failed, so the caller must not run its pre-rename cleanup. A rename whose
/// durability could not be confirmed reports `Installed` and leaves the
/// quarantined sidecars in place, named in the error.
async fn install_tmp_db(
    tmp_db: &Path,
    target: &Path,
    created: Option<&Path>,
) -> Result<(), InstallFailure> {
    // 1. The copy must have nothing left beside it that the rename cannot carry.
    if let Err(error) = refuse_stranded_sidecar(tmp_db) {
        return Err(InstallFailure::Untouched(error));
    }
    // 2. The old target's sidecars move aside: a stale hot `-journal`/`-wal` must
    //    never be replayed into the freshly installed file (L3), and a rename
    //    that fails must not have stripped the old target of the commits they
    //    hold (L10).
    let quarantined = match QuarantinedSidecars::take(target) {
        Ok(quarantined) => quarantined,
        Err(error) => return Err(InstallFailure::Untouched(error)),
    };
    // 3. Those renames must be DURABLE before the next one lands (fix round 6, C1):
    //    otherwise a power loss can keep the install's rename and lose the
    //    quarantine's, exposing the freshly installed database to the old target's
    //    hot `-journal`/`-wal` — the replay the quarantine exists to prevent. Fail
    //    closed: put the sidecars back and leave the target exactly as it was.
    if let Err(error) = sync_quarantine_dir(target) {
        let failure = anyhow!(
            "the replaced database's sidecars were moved aside but their directory could not be \
             fsynced, so those renames are not durable: {error}"
        );
        return Err(InstallFailure::Untouched(
            match quarantined.restore(target) {
                Ok(()) => failure,
                Err(restore_error) => anyhow!(
                    "{failure}; and putting the replaced database's sidecars back failed too: \
                 {restore_error}"
                ),
            },
        ));
    }
    // 4. The rename itself. Past this point the database IS installed.
    if let Err(error) = move_aside(tmp_db, target) {
        // The old target keeps its database, and its sidecars go back — with
        // whatever cannot be put back named in the error (fix round 2, N5).
        let failure = anyhow!(
            "install {} -> {}: {error}",
            tmp_db.display(),
            target.display()
        );
        return Err(InstallFailure::Untouched(
            match quarantined.restore(target) {
                Ok(()) => failure,
                Err(restore_error) => anyhow!(
                    "{failure}; and putting the replaced database's sidecars back failed too: \
                 {restore_error}"
                ),
            },
        ));
    }
    let installed = |error: anyhow::Error| InstallFailure::Installed {
        target: target.to_path_buf(),
        error,
    };
    // 5. The rename is durable only once the directory that received it is
    //    fsynced — including every level this run created (issue #259, F6).
    //
    //    This runs BEFORE the quarantine is discarded (fix round 11, R1). Round
    //    3's M3 had the discard first — "the database those sidecars belonged to
    //    is gone, so they go" — which is right about OWNERSHIP and wrong about
    //    ORDER: a crash can persist the unlinks without the rename, and the
    //    target would then still be the OLD database with its committed `-wal`
    //    rows already unlinked. The rename's own durability lands first, so the
    //    unlinks can only ever follow a rename that is already on disk.
    let mut failures: Vec<String> = Vec::new();
    match publish::sync_published(target, created) {
        Ok(()) => {
            // 6. That durability is in place, so the old sidecars go — with their
            //    own directory fsync as before. A cleanup that FAILS must not skip
            //    the WAL switch (fix round 5, P2): a leftover quarantine is a mess,
            //    a database that is neither durable nor in WAL is a data-loss
            //    risk. Every step runs, and the error names all of it.
            if let Err(error) = quarantined.discard() {
                failures.push(format!(
                    "the replaced database's quarantined sidecars could not all be cleaned up: \
                     {error}"
                ));
            }
        }
        // The rename is NOT confirmed durable, so those quarantined files are the
        // only copy of the old database's un-checkpointed rows. Discarding them
        // here is exactly the data loss R1 is about: they stay, and the error
        // names them so the operator can put them back if the rename turns out to
        // have been lost.
        Err(error) => failures.push(format!(
            "its directory could not be fsynced, so the new name is not durable: {error}; the \
             replaced database's sidecars were NOT discarded — {} — because their removal could \
             be persisted without the rename",
            quarantined.left_in_place()
        )),
    }
    // 7. ADR-0019, deterministically (F4): the installed file is in WAL, read
    //    back, before this command reports success — never left to the next
    //    process to fix with a fire-and-forget pragma.
    if let Err(error) = put_in_wal(target).await {
        failures.push(format!(
            "it could not be put back in WAL, so the production journal mode is not in place: {error}"
        ));
    }
    if !failures.is_empty() {
        return Err(installed(anyhow!("{}", failures.join("; and "))));
    }
    Ok(())
}

/// `chmod a-w` — clear every write bit on the source path the import read.
fn set_read_only(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path).map_err(|e| anyhow!("stat {}: {e}", path.display()))?;
    let mut perms = metadata.permissions();
    perms.set_mode(perms.mode() & !0o222);
    fs::set_permissions(path, perms).map_err(|e| anyhow!("chmod a-w {}: {e}", path.display()))
}

/// Print every mismatch, up to [`MAX_NAMED_MISMATCHES`], then "…and N more".
pub(crate) fn print_refusal(label: &str, mismatches: &[Mismatch]) {
    eprintln!(
        "{label}: REFUSED — {} verification mismatch(es):",
        mismatches.len()
    );
    for mismatch in mismatches.iter().take(MAX_NAMED_MISMATCHES) {
        eprintln!(
            "  mismatch: {} id={} field={}: {}",
            mismatch.table, mismatch.id, mismatch.field, mismatch.detail
        );
    }
    if mismatches.len() > MAX_NAMED_MISMATCHES {
        eprintln!("  …and {} more", mismatches.len() - MAX_NAMED_MISMATCHES);
    }
}

/// Print the success summary: counts per table, versions verified, runs
/// verified, snapshots verified, and the target paths. The last line names
/// the Mac-original runbook step (D7).
fn print_summary(label: &str, job: &VerifiedCopy<'_>, summary: &Summary) {
    let tables = summary
        .table_counts
        .iter()
        .map(|(table, count)| format!("{table}={count}"))
        .collect::<Vec<_>>()
        .join(", ");
    println!("{label}: verification summary");
    println!("  tables: {tables}");
    println!("  versions verified: {}", summary.versions_verified);
    println!("  runs verified: {}", summary.runs_verified);
    let paper = summary
        .paper_digests
        .iter()
        .map(|digest| {
            format!(
                "{} rows={} sha256={}",
                digest.table, digest.rows, digest.source_digest
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    println!("  paper digests: {paper}");
    println!("  snapshots verified: {}", summary.snapshots_verified);
    println!("  target db: {}", job.db_target.display());
    println!("  target data dir: {}", job.data_target.display());
    println!(
        "note: making the original pulse.db on the Mac read-only is a cutover-runbook step \
         at the release walk (D7), not this command's job."
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        COPY_SIDECAR_SUFFIXES, HeadSource, InstallFailure, InstallRefusal, StepFailure,
        VerifiedCopy, copy_snapshot_into, copy_snapshots, install_tmp_db, publish,
        remove_added_snapshots_keeping, remove_tmp_db, run_steps, run_verified_copy, scan_heads,
        scan_snapshots, set_source_mutation, sidecar_path, write_head_manifest,
    };
    use crate::adapters::db::instance_lock::InstanceLock;
    use crate::adapters::db::{Db, open_migrated, open_migrated_copy};
    use crate::adapters::store::CandleStore;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    /// The same minimal, valid, compilable DSL the import/restore integration
    /// suite seeds with — a REAL repository-written `version_hash` comes out of
    /// it, which the import's repository-read check re-derives.
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

    /// Every `*.partial` file beside one destination.
    fn partial_files(dest: &Path) -> Vec<PathBuf> {
        let Some(parent) = dest.parent() else {
            return Vec::new();
        };
        fs::read_dir(parent)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.to_string_lossy().ends_with(".partial"))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A cwd guard for the bare-relative-target tests (fix round 5, P4): the test
    /// moves the process cwd into a tempdir and puts it back on drop. Safe here for
    /// the reason `tests/secrets_profile.rs` documents — nextest runs each test in
    /// its own process — and it is what makes a bare `--db pulse.db` resolvable at
    /// all, since such a target is by definition relative to the cwd.
    struct CwdGuard {
        original: PathBuf,
    }

    impl CwdGuard {
        fn enter(dir: &Path) -> Self {
            let original = std::env::current_dir().expect("record the cwd");
            std::env::set_current_dir(dir).expect("move the process cwd");
            Self { original }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
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

    /// Everything an in-process import needs: a migrated source database and the
    /// committed fixture store as the source data dir, plus fresh target paths.
    struct Flow {
        dir: TempDir,
        source_db: PathBuf,
        source_data: PathBuf,
        target_db: PathBuf,
        target_data: PathBuf,
    }

    async fn flow() -> Flow {
        let dir = TempDir::new().expect("tempdir");
        let source_data = dir.path().join("mac-data");
        copy_tree(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store"),
            &source_data,
        );
        let source_db = dir.path().join("mac").join("pulse.db");
        fs::create_dir_all(source_db.parent().expect("the source's directory"))
            .expect("create the source directory");
        let db = open_migrated(&source_db).await.expect("migrate the source");
        db.pool().close().await;
        settle_wal(&source_db).await;
        let target_db = dir.path().join("server").join("pulse.db");
        let target_data = dir.path().join("server-data");
        Flow {
            dir,
            source_db,
            source_data,
            target_db,
            target_data,
        }
    }

    /// The job an import hands [`run_verified_copy`].
    fn import_job(flow: &Flow) -> VerifiedCopy<'_> {
        VerifiedCopy {
            source_label: "import",
            from_db: &flow.source_db,
            from_data_dir: &flow.source_data,
            db_target: &flow.target_db,
            data_target: &flow.target_data,
            head_source: HeadSource::SourceStore,
            replace: false,
            chmod_source: false,
        }
    }

    /// The store's `HEAD` pointer for a `(pair, timeframe)`.
    fn head_of(
        flow: &Flow,
        pair: &crate::domain::Pair,
        timeframe: crate::domain::Timeframe,
    ) -> PathBuf {
        CandleStore::with_base_dir(flow.target_data.clone()).head_path(pair, timeframe)
    }

    /// Fix 2 (round 4): a copy publishes through a temporary sibling, so a failure
    /// can never leave a truncated file under a final content-addressed name — and
    /// a successful one leaves no temporary behind.
    #[test]
    fn a_failed_copy_leaves_nothing_under_the_final_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("BTCUSDT").join("15m").join("dv1.parquet");

        // A source that cannot be copied: a DIRECTORY. The real case this guards
        // is a copy failing part-way (ENOSPC); a directory fails the same way
        // without needing a filled disk.
        let source_dir = dir.path().join("source-dir");
        fs::create_dir_all(&source_dir).expect("source dir");
        let error =
            copy_snapshot_into(&source_dir, &dest).expect_err("a directory cannot be copied");
        assert!(
            error.to_string().contains("copy snapshot"),
            "the failure names the copy: {error}"
        );
        assert!(
            !dest.exists(),
            "no file under the final name: {}",
            dest.display()
        );
        assert!(
            partial_files(&dest).is_empty(),
            "and no temporary is left: {:?}",
            partial_files(&dest)
        );

        // A good copy lands byte-identical, with no temporary left behind.
        let source = dir.path().join("source.parquet");
        fs::write(&source, b"snapshot bytes").expect("write the source");
        copy_snapshot_into(&source, &dest).expect("copy");
        assert_eq!(
            fs::read(&dest).expect("read the destination"),
            b"snapshot bytes"
        );
        assert!(
            partial_files(&dest).is_empty(),
            "the temporary is renamed away, not kept: {:?}",
            partial_files(&dest)
        );

        // A temporary a crashed run left is never reused or published: the
        // destination holds THIS run's bytes and the stray is gone.
        let temporary = dir
            .path()
            .join("BTCUSDT")
            .join("15m")
            .join("dv1.parquet.partial");
        fs::write(&temporary, b"stale partial bytes").expect("plant the stray temporary");
        let second = dir.path().join("second.parquet");
        fs::write(&second, b"second snapshot").expect("write the second source");
        copy_snapshot_into(&second, &dest).expect("copy over");
        assert_eq!(
            fs::read(&dest).expect("read the destination"),
            b"second snapshot",
            "the published bytes are this run's"
        );
        assert!(
            !temporary.exists(),
            "and the stray temporary is gone, not left beside it"
        );
    }

    /// Issue #258 (fail closed): a sidecar left beside the copy is REFUSED with
    /// a named, typed reason — never renamed over — and a target that was
    /// already there is left exactly as it was. A rename carries the database
    /// file and nothing else, so renaming over this would publish a database
    /// whose committed rows were left behind under a name no database owns.
    #[tokio::test]
    async fn the_install_refuses_a_leftover_sidecar_and_leaves_the_target_untouched() {
        for suffix in COPY_SIDECAR_SUFFIXES {
            let dir = tempfile::tempdir().expect("tempdir");
            let target = dir.path().join("pulse.db");
            let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
            fs::write(&target, b"the previous target").expect("write the target");
            fs::write(&tmp_db, b"the copy").expect("write the copy");
            let sidecar = sidecar_path(&tmp_db, suffix);
            fs::write(&sidecar, b"rows that only live here").expect("write the sidecar");

            let failure = install_tmp_db(&tmp_db, &target, None)
                .await
                .expect_err("an install with a leftover sidecar must be refused");

            let InstallFailure::Untouched(error) = failure else {
                panic!("nothing was renamed, so the target is untouched: {failure:?}");
            };
            let refusal = error
                .downcast_ref::<InstallRefusal>()
                .unwrap_or_else(|| panic!("the refusal must be typed, got: {error:?}"));
            match refusal {
                InstallRefusal::Sidecar {
                    db,
                    suffix: named_suffix,
                    sidecar: named_sidecar,
                } => {
                    assert_eq!(db, &tmp_db, "the refusal names the temporary database");
                    assert_eq!(*named_suffix, suffix);
                    assert_eq!(named_sidecar, &sidecar, "and the sidecar it found");
                }
                InstallRefusal::Unreadable { .. } => {
                    panic!("a sidecar that IS there is the `Sidecar` refusal: {refusal:?}");
                }
            }
            assert!(
                error.to_string().contains("refusing to install"),
                "the reason is named for the operator ({suffix}): {error}"
            );
            assert_eq!(
                fs::read(&target).expect("read the target"),
                b"the previous target",
                "the target that was already there is untouched ({suffix})"
            );
            assert!(tmp_db.exists(), "the copy was not renamed away ({suffix})");
            assert_eq!(
                fs::read(&sidecar).expect("read the sidecar"),
                b"rows that only live here",
                "the sidecar is neither renamed nor deleted ({suffix})"
            );
        }
    }

    /// Fix round 1, F7: the cleanup and the refusal are ONE list. Every suffix
    /// the refusal rejects is a suffix `remove_tmp_db` removes — so a refused
    /// install's failure path cannot leave a sidecar behind (the round-1 code
    /// checked four suffixes and removed two).
    #[test]
    fn the_temporarys_cleanup_removes_every_sidecar_the_refusal_checks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        fs::write(&tmp_db, b"the copy").expect("write the copy");
        for suffix in COPY_SIDECAR_SUFFIXES {
            fs::write(sidecar_path(&tmp_db, suffix), b"state").expect("write the sidecar");
        }
        assert!(
            super::refuse_stranded_sidecar(&tmp_db).is_err(),
            "the refusal rejects the copy while a sidecar is beside it"
        );

        remove_tmp_db(&tmp_db).expect("the temporary is removable");

        assert!(!tmp_db.exists(), "the copy is gone");
        let left: Vec<PathBuf> = COPY_SIDECAR_SUFFIXES
            .iter()
            .map(|suffix| sidecar_path(&tmp_db, suffix))
            .filter(|sidecar| sidecar.exists())
            .collect();
        assert!(
            left.is_empty(),
            "and so is every sidecar the refusal checks: {left:?}"
        );
    }

    /// Fix round 1, F7 (L5/L9): sidecar paths are built from the raw `OsStr`, so
    /// a directory name that is not valid UTF-8 still matches — a `display()`
    /// round-trip rewrites those bytes and would look for a file that is not
    /// there.
    //
    // Linux only: APFS refuses a name that is not valid UTF-8 outright
    // (`EINVAL`, os error 92), so this test cannot even build its fixture on
    // macOS (fix round 2, N7 — what CI caught).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_non_utf8_directory_still_detects_a_planted_sidecar() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let raw = dir.path().join(OsStr::from_bytes(b"host-\xff\xfe"));
        fs::create_dir_all(&raw).expect("create the non-UTF-8 directory");
        let target = raw.join("pulse.db");
        let tmp_db = raw.join(".pulse.db.import-tmp-1.db");
        fs::write(&tmp_db, b"the copy").expect("write the copy");
        let sidecar = sidecar_path(&tmp_db, "-wal");
        fs::write(&sidecar, b"rows that only live here").expect("write the sidecar");

        let failure = install_tmp_db(&tmp_db, &target, None)
            .await
            .expect_err("the planted -wal must be detected");
        let InstallFailure::Untouched(error) = failure else {
            panic!("nothing was renamed: {failure:?}");
        };
        assert!(
            error.downcast_ref::<InstallRefusal>().is_some(),
            "the refusal is typed even for a non-UTF-8 path: {error:?}"
        );
        assert!(
            sidecar.exists() && !target.exists(),
            "nothing was renamed and the sidecar is still there"
        );
    }

    /// Issue #259: the install's rename is followed by an fsync of the directory
    /// that received the database (the store's own
    /// temp→fsync→rename→fsync-dir discipline), so the target's new name is
    /// durable before the import reports success.
    #[tokio::test]
    async fn the_install_syncs_the_target_directory_after_the_rename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        let copy = open_migrated_copy(&tmp_db).await.expect("the copy");
        copy.pool().close().await;
        let _ = publish::probe::take();

        install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect("install");

        let syncs = publish::probe::take();
        let install_rename = syncs
            .iter()
            .position(|step| {
                step.kind == publish::probe::SyncKind::Rename && step.destination == target
            })
            .unwrap_or_else(|| panic!("the install's rename is recorded: {syncs:?}"));
        assert!(
            syncs
                .iter()
                .enumerate()
                .any(|(index, sync)| index > install_rename
                    && sync.kind == publish::probe::SyncKind::Dir
                    && sync.path == *dir.path()
                    && sync.destination == target
                    && sync.destination_present),
            "the target's own directory is synced AFTER the rename: {syncs:?}"
        );
        assert!(target.exists(), "and the database is there");
    }

    /// Fix round 11, R1: the fsync that makes the install's rename durable runs
    /// BEFORE the old sidecars are unlinked. Round 3's M3 had it the other way
    /// round — the discard first — so a crash could persist the unlinks without
    /// the rename, leaving the OLD database at the target with the `-wal` holding
    /// its committed rows already gone.
    #[tokio::test]
    async fn the_install_syncs_the_rename_before_it_discards_the_quarantine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        let copy = open_migrated_copy(&tmp_db).await.expect("the copy");
        copy.pool().close().await;
        fs::write(&target, b"the old target").expect("write the old target");
        for suffix in super::DB_STATE_SUFFIXES {
            fs::write(sidecar_path(&target, suffix), b"the old target's state")
                .expect("write the old target's sidecar");
        }
        let _ = publish::probe::take();

        install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect("install");

        let events = publish::probe::take();
        let rename = events
            .iter()
            .position(|step| {
                step.kind == publish::probe::SyncKind::Rename && step.destination == target
            })
            .unwrap_or_else(|| panic!("the install's rename is recorded: {events:?}"));
        let durable = events
            .iter()
            .enumerate()
            .position(|(index, step)| {
                index > rename
                    && step.kind == publish::probe::SyncKind::Dir
                    && step.path == *dir.path()
                    && step.destination == target
            })
            .unwrap_or_else(|| panic!("the rename's directory sync is recorded: {events:?}"));
        let unlink = events
            .iter()
            .position(|step| step.kind == publish::probe::SyncKind::Remove)
            .unwrap_or_else(|| panic!("the quarantine unlinks are recorded: {events:?}"));
        assert!(
            durable < unlink,
            "the rename is durable BEFORE the old sidecars go: {events:?}"
        );
        for suffix in super::DB_STATE_SUFFIXES {
            assert!(
                !sidecar_path(&target, suffix).exists(),
                "and the quarantine WAS discarded once that was true ({suffix})"
            );
        }
    }

    /// Fix round 11, R1: when that sync fails, the quarantined sidecars are NOT
    /// discarded — while the rename is unconfirmed they are the only copy of the
    /// old database's un-checkpointed rows — and the error names every one of
    /// them, with the name its bytes belong back at.
    #[tokio::test]
    async fn a_failed_sync_keeps_the_quarantine_and_names_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        let copy = open_migrated_copy(&tmp_db).await.expect("the copy");
        copy.pool().close().await;
        fs::write(&target, b"the old target").expect("write the old target");
        // The quarantine names are keyed on the OLD target's identity, so they are
        // computed while it is still the file at that path.
        let quarantines: Vec<PathBuf> = super::DB_STATE_SUFFIXES
            .iter()
            .map(|suffix| {
                let sidecar = sidecar_path(&target, suffix);
                fs::write(&sidecar, b"the old target's state").expect("write the sidecar");
                super::quarantine_path(&target, suffix, super::inode_of(&target))
                    .expect("the quarantine name is built from the target's own name")
            })
            .collect();
        let _ = publish::probe::take();
        // Keyed on the published file, not the directory: the C1 quarantine sync
        // runs in the same directory and would eat a directory-keyed injection.
        publish::probe::fail_next_publish_with_extension("db");

        let failure = install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect_err("the injected directory sync fails after the rename");
        let InstallFailure::Installed {
            target: installed,
            error,
        } = failure
        else {
            panic!("the rename landed, so the database IS installed: {failure:?}");
        };
        assert_eq!(installed, target, "and it names the installed database");

        let message = error.to_string();
        assert!(
            message.contains("not durable"),
            "the failure names the unconfirmed rename: {message}"
        );
        assert!(
            message.contains("were NOT discarded"),
            "and says the quarantine was kept: {message}"
        );
        for quarantine in &quarantines {
            assert!(
                quarantine.exists(),
                "the quarantine is still on disk: {}",
                quarantine.display()
            );
            assert!(
                message.contains(&quarantine.display().to_string()),
                "and the error names it: {message}"
            );
        }
        let events = publish::probe::take();
        assert!(
            !events
                .iter()
                .any(|step| step.kind == publish::probe::SyncKind::Remove),
            "nothing was unlinked: {events:?}"
        );
    }

    /// Fix round 1, F4 (ADR-0019): the file the import leaves behind is in WAL
    /// before the command reports success. The install's copy runs in
    /// rollback-journal mode (#258), and `journal_mode` is persisted IN the file,
    /// so the install puts it back itself — read back, not left to whatever opens
    /// the database next.
    #[tokio::test]
    async fn the_install_puts_the_target_back_in_wal_before_it_returns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        let copy = open_migrated_copy(&tmp_db).await.expect("the copy");
        copy.pool().close().await;

        install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect("install");

        // The file itself declares WAL (header bytes 18/19 are the format
        // versions: 2 = WAL) the moment the install returns.
        let header = fs::read(&target).expect("read the installed database");
        assert_eq!(
            (header.get(18), header.get(19)),
            (Some(&2), Some(&2)),
            "the installed database's own header says WAL when the install returns"
        );

        // The single connection the switch used closed synchronously, so the
        // directory holds the database alone — asserted BEFORE anything else
        // opens it (a pool's close could leave a connection in flight, and the
        // strays it strands are #258's failure mode, not this test's subject).
        let strays: Vec<String> = fs::read_dir(dir.path())
            .expect("read the target's directory")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with("-wal") || name.ends_with("-shm"))
            .collect();
        assert!(
            strays.is_empty(),
            "no sidecar survives the install: {strays:?}"
        );

        // And the normal opener still reads WAL back out of it.
        let installed = crate::adapters::db::Db::with_path(&target)
            .await
            .expect("open the installed database");
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(installed.pool())
            .await
            .expect("read journal_mode");
        installed.pool().close().await;
        assert_eq!(mode.to_ascii_lowercase(), "wal", "and it stays WAL");
    }

    /// Fix round 1, F3 (L3): the OLD target's sidecars never survive the install.
    /// A stale hot `-journal`/`-wal` beside the target belongs to the database
    /// being replaced, and replaying it into the freshly installed file would be
    /// exactly the corruption this PR is about — so the install quarantines them
    /// and drops them once the new file is in place.
    #[tokio::test]
    async fn an_install_does_not_leave_the_old_targets_sidecars_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        let copy = open_migrated_copy(&tmp_db).await.expect("the copy");
        copy.pool().close().await;
        fs::write(&target, b"the old target").expect("write the old target");
        for suffix in ["-wal", "-shm", "-journal"] {
            fs::write(sidecar_path(&target, suffix), b"the old target's state")
                .expect("write the old target's sidecar");
        }

        let _ = publish::probe::take_quarantine();
        install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect("install");

        // The MECHANISM, not just the end state: the old sidecars were taken out
        // of the way before the copy was renamed in. (The WAL switch that follows
        // reopens the database and cleans a stale sidecar up on its own, so the
        // end state alone cannot tell the quarantine from leaving them there.)
        let quarantined = publish::probe::take_quarantine();
        for suffix in ["-wal", "-shm", "-journal"] {
            assert!(
                quarantined.contains(&sidecar_path(&target, suffix)),
                "the old target's {suffix} was moved aside before the rename: {quarantined:?}"
            );
        }
        let names: Vec<String> = fs::read_dir(dir.path())
            .expect("read the directory")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        for suffix in ["-wal", "-shm", "-journal"] {
            assert!(
                !sidecar_path(&target, suffix).exists(),
                "the old target's {suffix} is gone after a successful install: {names:?}"
            );
        }
        assert!(
            names.iter().all(|name| !name.contains(".quarantine-")),
            "and no quarantine is left behind: {names:?}"
        );
        assert!(target.exists(), "the new database is in place");
    }

    /// Fix round 1, F3 (L10): a FAILED install must not strip the old target of
    /// its un-checkpointed commits. The sidecars it moved aside go straight back.
    #[tokio::test]
    async fn a_failed_install_puts_the_old_targets_sidecars_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        // The copy is ABSENT, so the rename itself fails — the one step between
        // the quarantine and the target becoming the new database.
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        fs::write(&target, b"the old target").expect("write the old target");
        let sidecar = sidecar_path(&target, "-wal");
        fs::write(&sidecar, b"the old target's un-checkpointed rows")
            .expect("write the old target's -wal");

        let failure = install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect_err("the rename fails: the copy does not exist");
        let InstallFailure::Untouched(error) = failure else {
            panic!("nothing was renamed: {failure:?}");
        };
        assert!(
            error.to_string().contains("install"),
            "the failure names the rename: {error}"
        );
        assert_eq!(
            fs::read(&sidecar).expect("read the restored sidecar"),
            b"the old target's un-checkpointed rows",
            "the old target's -wal is still beside it, with its bytes"
        );
        assert_eq!(
            fs::read(&target).expect("read the old target"),
            b"the old target",
            "and the old database is untouched"
        );
    }

    /// Issue #259: a snapshot's rename is followed by an fsync of the directory
    /// it landed in AND of every level the copy created below the deepest
    /// ancestor that already existed — the nested `candles/<PAIR>/<TF>/` entries
    /// are what a backup would otherwise lose while keeping the database that
    /// references them.
    #[test]
    fn a_snapshot_copy_syncs_every_directory_it_created_after_the_rename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.parquet");
        fs::write(&source, b"snapshot bytes").expect("write the source");

        // The store root exists; the pair and timeframe directories do not, so
        // the copy creates two levels and each is an entry in its own parent.
        let candles = dir.path().join("candles");
        fs::create_dir_all(&candles).expect("the store root");
        let pair = candles.join("BTCUSDT");
        let timeframe = pair.join("15m");
        let dest = timeframe.join("dv1.parquet");
        let _ = publish::probe::take();

        copy_snapshot_into(&source, &dest).expect("copy");

        let events = publish::probe::take();
        let synced: Vec<PathBuf> = events
            .iter()
            .filter(|event| event.kind == publish::probe::SyncKind::Dir)
            .map(|event| event.path.clone())
            .collect();
        assert_eq!(
            synced,
            vec![timeframe.clone(), pair.clone(), candles.clone()],
            "the leaf's directory, the level it was created in, and the store root"
        );
        assert!(
            events
                .iter()
                .any(|event| event.kind == publish::probe::SyncKind::File
                    && event.path == dest.with_extension("parquet.partial")
                    && !event.destination_present),
            "and the temporary's bytes were fsynced BEFORE the rename: {events:?}"
        );

        // A copy into a directory that ALREADY exists creates nothing, so only
        // the leaf's own directory is synced — no walk up the store.
        let second = timeframe.join("dv2.parquet");
        let _ = publish::probe::take();
        copy_snapshot_into(&source, &second).expect("copy into an existing directory");
        let second_syncs: Vec<PathBuf> = publish::probe::take()
            .into_iter()
            .filter(|event| event.kind == publish::probe::SyncKind::Dir)
            .map(|event| event.path)
            .collect();
        assert_eq!(
            second_syncs,
            vec![timeframe.clone()],
            "nothing was created, so nothing above the leaf is synced"
        );
    }

    /// Fix round 2, N8: the suffix sets are PINNED, as literals.
    ///
    /// F7 made the refusal and the cleanup share one list; this is what makes a
    /// member disappearing from it a test failure instead of a silent narrowing —
    /// the round-1 mutant "drop `-journal` from the shared list" survived every
    /// other test in the suite.
    #[test]
    fn the_sidecar_suffix_sets_are_pinned() {
        // Compared as SLICES: a member disappearing from a const array would
        // otherwise fail to compile rather than fail this test (the mutant has to
        // be runnable to be caught).
        assert_eq!(
            COPY_SIDECAR_SUFFIXES.as_slice(),
            ["-wal", "-shm", "-journal", ".partial"].as_slice(),
            "the copy's sidecars: what the refusal checks and the cleanup removes"
        );
        assert_eq!(
            super::DB_STATE_SUFFIXES.as_slice(),
            ["-wal", "-shm", "-journal"].as_slice(),
            "the target's own sidecars: what the install quarantines"
        );
    }

    /// Fix round 2, N2: when the quarantine cannot move a sidecar, every sidecar
    /// it has ALREADY moved goes back before the error returns — the install has
    /// not renamed the database, so the target has to be exactly as it was.
    #[tokio::test]
    async fn a_quarantine_that_fails_part_way_puts_the_moved_sidecars_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        let copy = open_migrated_copy(&tmp_db).await.expect("the copy");
        copy.pool().close().await;
        fs::write(&target, b"the old target").expect("write the old target");
        let first = sidecar_path(&target, "-wal");
        let second = sidecar_path(&target, "-shm");
        fs::write(&first, b"the old target's rows").expect("write the -wal");
        fs::write(&second, b"the old target's index").expect("write the -shm");
        // The SECOND move fails: a directory already occupies its quarantine
        // name (a rename onto a directory cannot succeed, and `take`'s own
        // "never reuse a stale quarantine" removal cannot delete a directory).
        let blocked = super::quarantine_path(&target, "-shm", super::inode_of(&target))
            .expect("the quarantine name");
        fs::create_dir_all(&blocked).expect("block the second quarantine name");

        let failure = install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect_err("the quarantine cannot complete");
        let InstallFailure::Untouched(error) = failure else {
            panic!("nothing was renamed: {failure:?}");
        };

        assert!(
            error.to_string().contains("quarantine"),
            "the failure names the quarantine: {error}"
        );
        assert_eq!(
            fs::read(&first).expect("read the sidecar that was moved first"),
            b"the old target's rows",
            "the sidecar moved before the failure is BACK under its own name"
        );
        assert_eq!(
            fs::read(&second).expect("read the sidecar whose move failed"),
            b"the old target's index",
            "and the one that could not be moved never left"
        );
    }

    /// Fix round 2, N5: when the install's rename fails AND a sidecar cannot be
    /// put back, the error names the stranded quarantine — those bytes are still
    /// on disk, and only the operator can move them back.
    #[tokio::test]
    async fn a_sidecar_that_cannot_be_put_back_is_named_in_the_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        // The copy is absent, so the install's rename fails.
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        fs::write(&target, b"the old target").expect("write the old target");
        let sidecar = sidecar_path(&target, "-wal");
        fs::write(&sidecar, b"the old target's un-checkpointed rows").expect("write the -wal");
        // ...and putting it back is injected to fail, which is the state the
        // error has to describe.
        publish::probe::fail_next_rename_to(&sidecar);

        let failure = install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect_err("the rename fails, and the sidecar cannot go back");
        let InstallFailure::Untouched(error) = failure else {
            panic!("nothing was renamed: {failure:?}");
        };

        let quarantine = super::quarantine_path(&target, "-wal", super::inode_of(&target))
            .expect("the quarantine name");
        let message = error.to_string();
        assert!(
            message.contains("put back"),
            "the error says the sidecars could not be put back: {message}"
        );
        assert!(
            message.contains(&quarantine.display().to_string()),
            "and names the stranded quarantine {}: {message}",
            quarantine.display()
        );
        assert_eq!(
            fs::read(&quarantine).expect("read the stranded quarantine"),
            b"the old target's un-checkpointed rows",
            "the bytes are still there, under the name the error gives"
        );
    }

    /// Fix round 2, N6: the quarantine works on a target whose FILE NAME is not
    /// valid UTF-8 — the round-1 `quarantine_path` built its name through
    /// `to_str()` and refused such a target outright.
    ///
    /// The quarantine is exercised directly rather than through a whole install:
    /// sqlx refuses a non-UTF-8 filename when it OPENS a database ("filename
    /// passed to `SQLite` must be valid UTF-8"), so the install's WAL switch (F4)
    /// cannot run on such a target at all — a limitation of the driver, not of
    /// this crate's own file surgery, and not something this PR can lift.
    //
    // Linux only: APFS refuses a name that is not valid UTF-8 (`EINVAL`), so the
    // fixture cannot be built on macOS (fix round 2, N7).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_utf8_target_name_still_gets_a_quarantine() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join(OsStr::from_bytes(b"pulse-\xff.db"));
        fs::write(&target, b"the old target").expect("write the old target");
        let sidecar = sidecar_path(&target, "-wal");
        fs::write(&sidecar, b"the old target's rows").expect("write the -wal");

        let quarantined = super::QuarantinedSidecars::take(&target)
            .expect("a non-UTF-8 target name must not refuse the quarantine");

        assert!(
            !sidecar.exists(),
            "the old target's sidecar was moved aside: {}",
            sidecar.display()
        );
        let quarantine = super::quarantine_path(&target, "-wal", super::inode_of(&target))
            .expect("the quarantine name is built from the raw OsStr, never through display()");
        assert!(
            quarantine.exists(),
            "and it is where the quarantine says: {}",
            quarantine.display()
        );
        quarantined
            .restore(&target)
            .expect("the quarantined sidecar goes back");
        assert_eq!(
            fs::read(&sidecar).expect("read the restored sidecar"),
            b"the old target's rows",
            "with its bytes"
        );
    }

    /// Fix round 4, Q1: quarantine files an interrupted install left behind make
    /// the next run REFUSE — before anything reads (or backs up, or replaces) the
    /// target — because those bytes are the target's own un-checkpointed rows.
    #[tokio::test]
    async fn an_orphaned_quarantine_refuses_the_run_before_the_target_is_touched() {
        let flow = flow().await;
        // An earlier install's quarantine: any pid, the target's -wal name.
        let target_dir = flow.target_db.parent().expect("the target's directory");
        fs::create_dir_all(target_dir).expect("the target's directory");
        fs::write(&flow.target_db, b"the old target, still holding rows").expect("the old target");
        let orphan = target_dir.join(".pulse.db-wal.quarantine-999999");
        fs::write(&orphan, b"the old target's un-checkpointed rows").expect("the quarantine");
        // `--replace` would back the non-empty target up first: the refusal must
        // come before that, so no safety backup may be written. The out-dir is a
        // tempdir (fix round 5, P3): no test reads or writes the operator's real
        // `~/pulse-backups`.
        let out_dir = flow.dir.path().join("pulse-backups");
        super::override_backup_out_dir(&out_dir);
        // The line that keeps this test off the operator's machine (fix round 5,
        // P3): the out-dir a safety backup would use IS this tempdir — never the
        // real `~/pulse-backups` the seam replaced.
        assert_eq!(
            super::default_backup_out_dir().expect("the out-dir resolves"),
            out_dir,
            "the test's safety-backup out-dir must be its own tempdir"
        );
        let mut job = import_job(&flow);
        job.replace = true;

        let error = run_verified_copy(job)
            .await
            .expect_err("an orphaned quarantine refuses the run");

        let refusal = error
            .downcast_ref::<super::InterruptedInstall>()
            .unwrap_or_else(|| panic!("the refusal must be typed, got: {error:?}"));
        let message = refusal.to_string();
        assert!(
            message.contains(&orphan.display().to_string()),
            "the refusal names the quarantine file: {message}"
        );
        assert!(
            message.contains(&sidecar_path(&flow.target_db, "-wal").display().to_string()),
            "and the name it has to go back to: {message}"
        );
        assert_eq!(
            fs::read(&flow.target_db).expect("read the target"),
            b"the old target, still holding rows",
            "the target was not touched"
        );
        assert!(
            orphan.exists(),
            "and the quarantine is still there to be moved back"
        );
        let backups: Vec<String> = fs::read_dir(&out_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| {
                        entry
                            .path()
                            .extension()
                            .is_some_and(|extension| extension == "db")
                            && entry.file_name().to_string_lossy().starts_with("pulse-")
                    })
                    .map(|entry| entry.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            backups.is_empty(),
            "no safety backup was written before the refusal: {backups:?}"
        );
    }

    /// Fix round 4, Q5: the import's undo follows M1's order too — the pointers
    /// go back BEFORE the added snapshots are removed, and a pointer that could
    /// not be restored keeps the snapshots it still names.
    #[tokio::test]
    async fn the_imports_undo_restores_the_pointers_before_it_removes_snapshots() {
        let flow = flow().await;
        let target_store = CandleStore::with_base_dir(flow.target_data.clone());
        let (source_snapshots, issues) = scan_snapshots(&flow.source_data);
        assert!(issues.is_empty(), "the fixture store is well-formed");
        let snapshot = &source_snapshots[0];
        // A snapshot this run added, under the pointer this run moved.
        let dest =
            target_store.snapshot_path(&snapshot.pair, snapshot.timeframe, &snapshot.version);
        fs::create_dir_all(dest.parent().expect("the leaf's directory"))
            .expect("create the leaf's directory");
        fs::write(&dest, b"a copied snapshot").expect("write the copied snapshot");
        target_store
            .write_head(&snapshot.pair, snapshot.timeframe, &snapshot.version)
            .expect("the pointer this run published");
        let prior = crate::domain::DataVersion::parse("0000000000000001").expect("a version tag");
        let writes = super::StoreWrites {
            added: vec![dest.clone()],
            heads: vec![super::HeadChange::new(
                snapshot.pair.clone(),
                snapshot.timeframe,
                Some(prior.clone()),
            )],
        };
        let _ = publish::probe::take_rollback();

        super::unwind_store_writes(&target_store, &writes).expect("the undo runs");

        let steps = publish::probe::take_rollback();
        let last_restore = steps
            .iter()
            .rposition(|step| matches!(step, publish::probe::RollbackStep::HeadRestored { .. }))
            .unwrap_or_else(|| panic!("a pointer was restored: {steps:?}"));
        let first_unlink = steps
            .iter()
            .position(|step| matches!(step, publish::probe::RollbackStep::Unlinked { .. }))
            .unwrap_or_else(|| panic!("the added snapshots were removed: {steps:?}"));
        assert!(
            last_restore < first_unlink,
            "the pointers go back BEFORE the first snapshot is deleted: {steps:?}"
        );
        assert_eq!(
            target_store
                .read_head(&snapshot.pair, snapshot.timeframe)
                .expect("read the pointer"),
            Some(prior),
            "the pointer is back to what it was"
        );
        assert!(
            !dest.exists(),
            "and the snapshot this run added is gone: {}",
            dest.display()
        );
    }

    /// Fix round 5, P1: the undo must not DROP a snapshot-removal failure — an
    /// undo that could not delete a snapshot cannot read as if every snapshot
    /// were gone, so the failure is collected, named, and carried by the
    /// caller's report (`fold_undo`).
    #[tokio::test]
    async fn the_imports_undo_reports_a_snapshot_it_cannot_remove() {
        let flow = flow().await;
        let target_store = CandleStore::with_base_dir(flow.target_data.clone());
        let (source_snapshots, issues) = scan_snapshots(&flow.source_data);
        assert!(issues.is_empty(), "the fixture store is well-formed");
        let snapshot = &source_snapshots[0];
        let dest =
            target_store.snapshot_path(&snapshot.pair, snapshot.timeframe, &snapshot.version);
        fs::create_dir_all(dest.parent().expect("the leaf's directory"))
            .expect("create the leaf's directory");
        fs::write(&dest, b"a copied snapshot").expect("write the copied snapshot");
        let writes = super::StoreWrites {
            added: vec![dest.clone()],
            heads: Vec::new(),
        };
        publish::probe::fail_next_remove_of(&dest);

        let undone = super::unwind_store_writes(&target_store, &writes)
            .expect_err("the undo cannot remove the snapshot");
        let message = format!("{undone:#}");
        assert!(
            message.contains(&dest.display().to_string()),
            "the undo names the snapshot it could not remove: {message}"
        );
        assert!(
            message.contains("could not be removed"),
            "and says what could not be done with it: {message}"
        );
        assert!(
            dest.exists(),
            "the bytes are still there — the undo must not report them gone: {}",
            dest.display()
        );

        // The run's own report carries the incomplete undo: it can no longer say
        // that every snapshot this run added was deleted.
        let folded = super::fold_undo(anyhow::anyhow!("the step failed"), &Err(undone));
        let folded_message = format!("{folded:#}");
        assert!(
            folded_message.contains("the step failed") && folded_message.contains("[undo]"),
            "the failure keeps the original error and the undo's: {folded_message}"
        );
        assert!(
            folded_message.contains(&dest.display().to_string()),
            "and the snapshot that is still there is named: {folded_message}"
        );
    }

    /// Fix round 5, P1: the undo's removals are made durable too — every directory
    /// that held a removed snapshot is fsynced after the unlink (the same rule the
    /// backup's rollback follows, M2) — and a removal whose fsync fails is reported
    /// rather than dropped.
    #[tokio::test]
    async fn the_imports_undo_makes_its_removals_durable() {
        let flow = flow().await;
        let target_store = CandleStore::with_base_dir(flow.target_data.clone());
        let (source_snapshots, issues) = scan_snapshots(&flow.source_data);
        assert!(issues.is_empty(), "the fixture store is well-formed");
        let snapshot = &source_snapshots[0];
        let dest =
            target_store.snapshot_path(&snapshot.pair, snapshot.timeframe, &snapshot.version);
        fs::create_dir_all(dest.parent().expect("the leaf's directory"))
            .expect("create the leaf's directory");
        fs::write(&dest, b"a copied snapshot").expect("write the copied snapshot");
        let writes = super::StoreWrites {
            added: vec![dest.clone()],
            heads: Vec::new(),
        };
        let _ = publish::probe::take();

        super::unwind_store_writes(&target_store, &writes).expect("the undo runs");

        assert!(
            !dest.exists(),
            "the snapshot is removed: {}",
            dest.display()
        );
        let syncs = publish::probe::take();
        assert!(
            syncs.iter().any(|event| {
                event.kind == publish::probe::SyncKind::Dir
                    && event.destination == dest
                    && !event.destination_present
            }),
            "the removal of {} is made durable (a directory sync AFTER the unlink): {syncs:?}",
            dest.display()
        );

        // And when that fsync fails, the undo says so instead of claiming a
        // cleanup that a power loss can undo.
        fs::create_dir_all(dest.parent().expect("the leaf's directory"))
            .expect("re-create the leaf's directory");
        fs::write(&dest, b"a copied snapshot").expect("write it again");
        let _ = publish::probe::take();
        publish::probe::fail_next_sync_of(dest.parent().expect("the leaf's directory"));

        let undone = super::unwind_store_writes(&target_store, &writes)
            .expect_err("the injected directory sync failure is reported");
        let message = format!("{undone:#}");
        assert!(
            message.contains("could not be made durable"),
            "the undo says the removal is not durable: {message}"
        );
        assert!(
            message.contains(&dest.display().to_string()),
            "and names it: {message}"
        );
    }

    /// Fix round 5, P2: a quarantine cleanup that fails must NOT skip the two
    /// steps that make the install durable. The leftover quarantine is named AND
    /// the installed file is still fsynced and put back in WAL — asserted from the
    /// file's own header, so this is the state on disk, not a code path.
    #[tokio::test]
    async fn a_failed_quarantine_cleanup_still_syncs_and_switches_to_wal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        let copy = open_migrated_copy(&tmp_db).await.expect("the copy");
        copy.pool().close().await;
        fs::write(&target, b"the old target").expect("write the old target");
        let sidecar = sidecar_path(&target, "-wal");
        fs::write(&sidecar, b"the old target's rows").expect("write the -wal");
        let quarantine = super::quarantine_path(&target, "-wal", super::inode_of(&target))
            .expect("the quarantine name");
        publish::probe::fail_next_remove_of(&quarantine);
        let _ = publish::probe::take();

        let failure = install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect_err("the quarantine cannot be cleaned up");

        // The cleanup failure is reported — and the install is NOT left in the
        // half-done state the early return used to leave it in.
        let InstallFailure::Installed { error, .. } = failure else {
            panic!("the rename landed, so the database IS installed: {failure:?}");
        };
        assert!(
            error.to_string().contains("could not all be cleaned up"),
            "the error reports the cleanup failure: {error}"
        );
        let syncs = publish::probe::take();
        assert!(
            syncs.iter().any(|event| {
                event.kind == publish::probe::SyncKind::Dir
                    && event.destination == target
                    && event.destination_present
            }),
            "the directory sync still ran after the failed cleanup: {syncs:?}"
        );
        let header = fs::read(&target).expect("read the installed database");
        assert_eq!(
            (header.get(18), header.get(19)),
            (Some(&2), Some(&2)),
            "and the installed file's own header says WAL, cleanup failure or not"
        );
    }

    /// Fix round 5, P4: the quarantine a bare relative target's sidecars need
    /// resolves to the current directory — and the scan for an interrupted
    /// install's leftovers looks there too. `parent()` on `pulse.db` is `Some("")`,
    /// not `None`, so this is the exact case that used to answer "has no parent
    /// directory" after the whole copy had run.
    #[test]
    fn a_bare_relative_targets_quarantine_lands_in_the_current_directory() {
        let pid = std::process::id();
        assert_eq!(
            super::quarantine_path(
                Path::new("pulse.db"),
                "-wal",
                super::inode_of(Path::new("pulse.db"))
            )
            .expect("a bare relative target has a usable directory"),
            PathBuf::from(".").join(format!(".pulse.db-wal.quarantine-{pid}")),
            "a sidecar's quarantine is a hidden sibling in the current directory"
        );
        assert_eq!(
            super::orphaned_quarantines(&PathBuf::from("pulse.db"))
                .expect("a readable directory")
                .len(),
            0,
            "the same directory is where the orphan scan looks (nothing is stranded here)"
        );
    }

    /// Fix round 5, P4: `--db pulse.db` is a relative target whose parent is the
    /// EMPTY path (not `None`). It must install into the current directory — the
    /// same #258 contract, no temporary and no sidecar left beside it — instead of
    /// failing with "has no parent directory" after the whole copy ran.
    #[tokio::test]
    async fn a_bare_relative_target_installs_into_the_current_directory() {
        let flow = flow().await;
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let _guard = CwdGuard::enter(cwd.path());
        let target = Path::new("pulse.db");
        let data_target = Path::new("server-data");
        let _ = publish::probe::take();

        run_verified_copy(VerifiedCopy {
            source_label: "import",
            from_db: &flow.source_db,
            from_data_dir: &flow.source_data,
            db_target: target,
            data_target,
            head_source: HeadSource::SourceStore,
            replace: false,
            chmod_source: false,
        })
        .await
        .expect("a bare relative target installs");

        let installed = cwd.path().join("pulse.db");
        assert!(
            installed.is_file(),
            "the database landed in the current directory: {}",
            installed.display()
        );
        // The file's own header proves the post-rename steps ran — the WAL switch
        // is the last step, and it is reached only when the directory sync ran.
        let header = fs::read(&installed).expect("read the installed database");
        assert_eq!(
            (header.get(18), header.get(19)),
            (Some(&2), Some(&2)),
            "and it is in WAL when the run returns"
        );
        let syncs = publish::probe::take();
        assert!(
            syncs.iter().any(|event| {
                event.kind == publish::probe::SyncKind::Dir
                    && event.destination == target
                    && event.destination_present
            }),
            "the install's directory sync covered the current directory: {syncs:?}"
        );
        // The #258 contract, on a path that never had a directory to work in: no
        // sidecar that could hold state, no quarantine, and no temporary database.
        // The migration protocol's own `.migrate.lock` marker is excluded on
        // purpose: it is an empty lock, carries no rows, and is a separate
        // pre-existing leftover (reported to the ledger, not widened into this
        // round).
        let left: Vec<String> = fs::read_dir(cwd.path())
            .expect("read the current directory")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| {
                let temporary = name.contains("import-tmp") && !name.ends_with(".migrate.lock");
                temporary
                    || name.contains(".quarantine-")
                    || name.ends_with("-wal")
                    || name.ends_with("-shm")
                    || name.ends_with("-journal")
            })
            .collect();
        assert!(left.is_empty(), "nothing is stranded beside it: {left:?}");
    }

    /// Fix round 5, P4: the orphan scan of a bare relative target looks in the
    /// CURRENT directory — where its quarantine files are — so an interrupted
    /// install's leftovers are refused rather than read past.
    #[tokio::test]
    async fn a_bare_relative_targets_orphaned_quarantine_refuses_the_run() {
        let flow = flow().await;
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let _guard = CwdGuard::enter(cwd.path());
        let orphan = cwd.path().join(".pulse.db-wal.quarantine-999999");
        fs::write(&orphan, b"the target's un-checkpointed rows").expect("the quarantine");

        let error = run_verified_copy(VerifiedCopy {
            source_label: "import",
            from_db: &flow.source_db,
            from_data_dir: &flow.source_data,
            db_target: Path::new("pulse.db"),
            data_target: Path::new("server-data"),
            head_source: HeadSource::SourceStore,
            replace: false,
            chmod_source: false,
        })
        .await
        .expect_err("an orphaned quarantine refuses the run");

        assert!(
            error.downcast_ref::<super::InterruptedInstall>().is_some(),
            "the refusal is typed: {error:?}"
        );
        // The scan resolved the bare target's EMPTY parent to the current
        // directory: the quarantine it names is the one planted here (its path is
        // relative — `./.pulse.db-wal.quarantine-…` — because the target is).
        let message = error.to_string();
        assert!(
            message.contains(".pulse.db-wal.quarantine-999999"),
            "and names the quarantine in the current directory: {message}"
        );
        assert!(
            message.contains("pulse.db-wal"),
            "and the name those rows have to go back to: {message}"
        );
        assert!(
            orphan.exists() && !cwd.path().join("pulse.db").exists(),
            "nothing was installed and the quarantine is still there to be moved back"
        );
    }

    /// Fix round 6, C1: the renames that move the target's sidecars aside must be
    /// DURABLE before the install's own rename lands. Otherwise a power loss can
    /// keep the install's rename and lose theirs, and the freshly installed
    /// database sits beside the old target's `-wal`/`-journal` — the replay the
    /// quarantine exists to prevent.
    #[tokio::test]
    async fn the_quarantine_renames_are_durable_before_the_install_rename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        let copy = open_migrated_copy(&tmp_db).await.expect("the copy");
        copy.pool().close().await;
        fs::write(&target, b"the old target").expect("write the old target");
        for suffix in super::DB_STATE_SUFFIXES {
            fs::write(sidecar_path(&target, suffix), b"the old target's state")
                .expect("write the old target's sidecar");
        }
        let _ = publish::probe::take();

        install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect("install");

        let events = publish::probe::take();
        let last_quarantine_rename = events
            .iter()
            .rposition(|step| {
                step.kind == publish::probe::SyncKind::Rename && step.destination != target
            })
            .unwrap_or_else(|| panic!("the sidecars were moved aside: {events:?}"));
        let quarantine_sync = events
            .iter()
            .position(|step| {
                step.kind == publish::probe::SyncKind::Dir && step.destination == target
            })
            .unwrap_or_else(|| panic!("their directory is fsynced: {events:?}"));
        let install_rename = events
            .iter()
            .position(|step| {
                step.kind == publish::probe::SyncKind::Rename && step.destination == target
            })
            .unwrap_or_else(|| panic!("the install's rename is recorded: {events:?}"));
        assert!(
            last_quarantine_rename < quarantine_sync,
            "the sidecar moves are durable only after they happened: {events:?}"
        );
        assert!(
            quarantine_sync < install_rename,
            "the directory sync after the quarantine happens BEFORE the install rename: {events:?}"
        );
        assert!(target.exists(), "and the install landed");
    }

    /// Fix round 6, C1: when that sync fails the install does NOT proceed. The
    /// sidecars go back and the target keeps its database — never "the new database
    /// is in place while its sidecars sit somewhere else".
    #[tokio::test]
    async fn a_failed_quarantine_sync_leaves_the_target_and_its_sidecars_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        let copy = open_migrated_copy(&tmp_db).await.expect("the copy");
        copy.pool().close().await;
        fs::write(&target, b"the old target").expect("write the old target");
        for suffix in super::DB_STATE_SUFFIXES {
            fs::write(sidecar_path(&target, suffix), b"the old target's state")
                .expect("write the old target's sidecar");
        }
        publish::probe::fail_next_sync_of(dir.path());

        let failure = install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect_err("the injected directory sync failure stops the install");

        let InstallFailure::Untouched(error) = failure else {
            panic!("nothing was renamed, so the target is untouched: {failure:?}");
        };
        assert!(
            error.to_string().contains("not durable"),
            "the error says the sidecar renames are not durable: {error}"
        );
        assert_eq!(
            fs::read(&target).expect("read the target"),
            b"the old target",
            "the target still holds the OLD database"
        );
        for suffix in super::DB_STATE_SUFFIXES {
            assert!(
                sidecar_path(&target, suffix).exists(),
                "its {suffix} went back"
            );
        }
        let quarantines: Vec<String> = fs::read_dir(dir.path())
            .expect("read the directory")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".quarantine-"))
            .collect();
        assert!(
            quarantines.is_empty(),
            "and no quarantine is left behind: {quarantines:?}"
        );
        assert!(
            tmp_db.exists(),
            "the copy was not renamed over anything: {}",
            tmp_db.display()
        );
    }

    /// Fix round 6, C3: putting back a pointer this run CREATED (the target had
    /// none) means removing it — and that removal is made durable and reported like
    /// every other rollback removal, so a power loss cannot resurrect a pointer to a
    /// snapshot the same rollback deleted.
    #[tokio::test]
    async fn the_imports_undo_removes_a_created_pointer_durably() {
        let flow = flow().await;
        let target_store = CandleStore::with_base_dir(flow.target_data.clone());
        let (source_heads, _) = scan_heads(&flow.source_data);
        let head = &source_heads[0];
        let path = target_store.head_path(&head.pair, head.timeframe);
        fs::create_dir_all(path.parent().expect("the pointer's directory"))
            .expect("create the pointer's directory");
        target_store
            .write_head(&head.pair, head.timeframe, &head.version)
            .expect("the pointer this run created");
        let writes = super::StoreWrites {
            added: Vec::new(),
            heads: vec![super::HeadChange::new(
                head.pair.clone(),
                head.timeframe,
                None,
            )],
        };
        let _ = publish::probe::take();

        super::unwind_store_writes(&target_store, &writes).expect("the undo runs");

        assert!(
            !path.exists(),
            "the pointer this run created is removed: {}",
            path.display()
        );
        let syncs = publish::probe::take();
        assert!(
            syncs.iter().any(|event| {
                event.kind == publish::probe::SyncKind::Dir
                    && event.destination == path
                    && !event.destination_present
            }),
            "and its removal is made durable (a directory sync AFTER the unlink): {syncs:?}"
        );

        // A removal that cannot be made durable is REPORTED, never swallowed: the
        // pointer counts as not restored (fix round 3, M1's typed failure), which is
        // what keeps the snapshot it names.
        target_store
            .write_head(&head.pair, head.timeframe, &head.version)
            .expect("re-create the pointer");
        publish::probe::fail_next_sync_of(path.parent().expect("the pointer's directory"));
        let undone = super::unwind_store_writes(&target_store, &writes)
            .expect_err("the failed fsync is reported");

        let failure = undone
            .downcast_ref::<super::HeadRestoreFailure>()
            .unwrap_or_else(|| panic!("the failure is the typed one: {undone:?}"));
        let named = format!("{failure}");
        assert!(
            named.contains(head.pair.as_str()),
            "and it names the pointer that is not back: {named}"
        );
    }

    /// Fix round 7, E3: the quarantine name records the identity of the database
    /// those bytes belong to — the inode, which a rename preserves — so a later
    /// run can tell whether moving them back is safe.
    #[tokio::test]
    async fn the_quarantine_records_the_identity_of_the_database_it_belongs_to() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        fs::write(&target, b"the old target").expect("write the old target");
        fs::write(sidecar_path(&target, "-wal"), b"its rows").expect("write the -wal");
        let super::TargetIdentity::Inode(inode) = super::inode_of(&target) else {
            panic!("the target has an inode");
        };

        let quarantined =
            super::QuarantinedSidecars::take(&target).expect("the sidecar moves aside");

        let names: Vec<String> = fs::read_dir(dir.path())
            .expect("read the directory")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".quarantine-"))
            .collect();
        assert_eq!(names.len(), 1, "one quarantine: {names:?}");
        assert!(
            names[0].ends_with(&format!("-{inode}")),
            "and its name ends in the target's inode ({inode}): {names:?}"
        );
        // The recorded identity is what the scan reads back: the database at the
        // target path still IS that file, so the bytes are its own.
        let stranded = super::orphaned_quarantines(&target).expect("the scan reads the directory");
        assert_eq!(
            stranded.first().map(|item| item.owner),
            Some(super::QuarantineOwner::Current),
            "the scan reads the name back as this database's own quarantine: {stranded:?}"
        );
        drop(quarantined);
    }

    /// Fix round 7, G1: the restore's move-backs are durable BEFORE the caller
    /// reports the target untouched — the directory sync follows the renames, so a
    /// power loss cannot keep the target as it was while losing the sidecars it
    /// was supposed to have back.
    #[tokio::test]
    async fn the_restores_move_backs_are_durable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        fs::write(&target, b"the old target").expect("write the old target");
        for suffix in super::DB_STATE_SUFFIXES {
            fs::write(sidecar_path(&target, suffix), b"the old target's state")
                .expect("write the old target's sidecar");
        }
        let quarantined =
            super::QuarantinedSidecars::take(&target).expect("the sidecars move aside");
        let _ = publish::probe::take();

        quarantined.restore(&target).expect("the sidecars go back");

        let events = publish::probe::take();
        let last_move_back = events
            .iter()
            .rposition(|step| {
                step.kind == publish::probe::SyncKind::Rename && step.destination != target
            })
            .unwrap_or_else(|| panic!("the move-backs are recorded: {events:?}"));
        let sync = events
            .iter()
            .position(|step| {
                step.kind == publish::probe::SyncKind::Dir && step.destination == target
            })
            .unwrap_or_else(|| panic!("their directory is fsynced: {events:?}"));
        assert!(
            last_move_back < sync,
            "the move-backs are durable only after they happened: {events:?}"
        );
        for suffix in super::DB_STATE_SUFFIXES {
            assert!(
                sidecar_path(&target, suffix).exists(),
                "and the {suffix} is back under its own name"
            );
        }
    }

    /// Fix round 7, G1: a restore that cannot be made durable is REPORTED — the
    /// install's `Untouched` error says the target is not confirmed as it was.
    #[tokio::test]
    async fn a_restore_that_cannot_be_made_durable_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        let copy = open_migrated_copy(&tmp_db).await.expect("the copy");
        copy.pool().close().await;
        fs::write(&target, b"the old target").expect("write the old target");
        for suffix in super::DB_STATE_SUFFIXES {
            fs::write(sidecar_path(&target, suffix), b"the old target's state")
                .expect("write the old target's sidecar");
        }
        // The SECOND sidecar's move fails, so `take` puts the first one back — and
        // that restore is the one whose directory sync is injected to fail.
        let second = super::quarantine_path(&target, "-shm", super::inode_of(&target))
            .expect("the quarantine name");
        publish::probe::fail_next_rename_to(&second);
        publish::probe::fail_next_sync_of(dir.path());

        let failure = install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect_err("the failed quarantine ends the install");
        let InstallFailure::Untouched(error) = failure else {
            panic!("nothing was renamed, so the target is untouched: {failure:?}");
        };

        let message = error.to_string();
        assert!(
            message.contains("could not be put back"),
            "the error names what the restore could not put back: {message}"
        );
        assert!(
            message.contains("not confirmed durable"),
            "and that the move-backs are not confirmed durable: {message}"
        );
        assert_eq!(
            fs::read(&target).expect("read the target"),
            b"the old target",
            "the target still holds the OLD database"
        );
    }

    /// Fix round 9, K2: a sidecar whose presence cannot be ESTABLISHED refuses the
    /// install. `Path::exists()` would call that "not there" and let the rename
    /// strand whatever the sidecar holds.
    #[tokio::test]
    async fn a_sidecar_whose_presence_cannot_be_checked_refuses_the_install() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        let copy = open_migrated_copy(&tmp_db).await.expect("the copy");
        copy.pool().close().await;
        let sidecar = sidecar_path(&tmp_db, "-wal");
        publish::probe::fail_next_stat_of(&sidecar);

        let failure = install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect_err("an unreadable sidecar refuses the install");
        let InstallFailure::Untouched(error) = failure else {
            panic!("nothing was renamed, so the target is untouched: {failure:?}");
        };

        let refusal = error
            .downcast_ref::<super::InstallRefusal>()
            .unwrap_or_else(|| panic!("the refusal is typed: {error:?}"));
        let super::InstallRefusal::Unreadable { sidecar: named, .. } = refusal else {
            panic!("an unreadable sidecar is its own refusal: {refusal:?}");
        };
        assert_eq!(
            named, &sidecar,
            "and it names the sidecar it could not check"
        );
        assert!(!target.exists(), "the target was not touched");
        assert!(tmp_db.exists(), "and the copy is still there");
    }

    /// Fix round 9, K2: the same for the TARGET's own sidecars — a sidecar that
    /// cannot be stat'ed might be holding rows, so the install must not strip the
    /// target of it by guessing that it is not there.
    #[tokio::test]
    async fn a_target_sidecar_whose_presence_cannot_be_checked_refuses_the_take() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        fs::write(&target, b"the old target").expect("write the old target");
        let sidecar = sidecar_path(&target, "-shm");
        publish::probe::fail_next_stat_of(&sidecar);

        let Err(error) = super::QuarantinedSidecars::take(&target) else {
            panic!("an unreadable sidecar refuses the take");
        };

        let message = error.to_string();
        assert!(
            message.contains(&sidecar.display().to_string()),
            "the refusal names the sidecar: {message}"
        );
        assert!(
            message.contains("cannot tell whether"),
            "and says why: {message}"
        );
        assert!(
            target.exists() && !sidecar_path(&target, "-wal").exists(),
            "and nothing was moved aside"
        );
    }

    /// Fix round 10, L1: that same probe returned on its error WITHOUT putting back
    /// the sidecars `take` had already moved — the target was left stripped of its
    /// `-wal`, sitting under a quarantine name, while the caller reported the
    /// install as untouched. ANY error after a move now runs N2's restore first
    /// (fsynced per G1), so `Untouched` is reported only when the target really is
    /// as it was.
    #[tokio::test]
    async fn a_take_that_fails_after_a_move_puts_what_it_moved_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        fs::write(&target, b"the old target").expect("write the old target");
        fs::write(&tmp_db, b"the copy").expect("write the copy");
        // The FIRST suffix is moved aside; the SECOND's probe is made to fail.
        let moved_suffix = super::DB_STATE_SUFFIXES[0];
        let moved = sidecar_path(&target, moved_suffix);
        fs::write(&moved, b"the old target's un-checkpointed rows").expect("write the -wal");
        let unreadable = sidecar_path(&target, super::DB_STATE_SUFFIXES[1]);
        publish::probe::fail_next_stat_of(&unreadable);
        let _ = publish::probe::take();

        let failure = install_tmp_db(&tmp_db, &target, None)
            .await
            .expect_err("an unreadable sidecar refuses the install");
        let InstallFailure::Untouched(error) = failure else {
            panic!("nothing was renamed, so the target is untouched: {failure:?}");
        };

        let message = error.to_string();
        assert!(
            message.contains(&unreadable.display().to_string()),
            "the failure names the sidecar it could not check: {message}"
        );
        assert!(
            !message.contains("could not be put back"),
            "and the sidecar it HAD moved went back cleanly, so there is no restore failure to \
             report: {message}"
        );
        assert_eq!(
            fs::read(&moved).expect("the moved sidecar is back under its own name"),
            b"the old target's un-checkpointed rows",
            "with its own bytes"
        );
        let quarantine = super::quarantine_path(&target, moved_suffix, super::inode_of(&target))
            .expect("the quarantine name is built from the target's own name");
        assert!(
            !quarantine.exists(),
            "and nothing is left under the quarantine name {}",
            quarantine.display()
        );
        // The move-back is durable before the failure is reported (G1).
        let events = publish::probe::take();
        let last_move_back = events
            .iter()
            .rposition(|step| {
                step.kind == publish::probe::SyncKind::Rename && step.destination != target
            })
            .unwrap_or_else(|| panic!("the move-backs are recorded: {events:?}"));
        let sync = events
            .iter()
            .position(|step| {
                step.kind == publish::probe::SyncKind::Dir && step.destination == target
            })
            .unwrap_or_else(|| panic!("their directory is fsynced: {events:?}"));
        assert!(
            last_move_back < sync,
            "the move-backs are durable only after they happened: {events:?}"
        );
    }

    /// Fix round 9, K4: a failed `VACUUM INTO` copy removes the partial temporary
    /// (and every sidecar a copy can have) before returning, so a same-pid retry
    /// does not meet an existing file — the same cleanup N4 gave the flush failure.
    #[tokio::test]
    async fn a_failed_copy_removes_the_partial_temporary() {
        let flow = flow().await;
        let tmp_db = super::temp_db_path(&flow.target_db, "import");
        fs::create_dir_all(tmp_db.parent().expect("the temporary's directory"))
            .expect("create the target's directory");
        // `VACUUM INTO` refuses a target that already exists — exactly the leftover
        // this cleanup is about.
        fs::write(&tmp_db, b"a leftover from a failed run").expect("plant the leftover");
        for suffix in super::COPY_SIDECAR_SUFFIXES {
            fs::write(sidecar_path(&tmp_db, suffix), b"its state").expect("plant the sidecar");
        }

        let error = run_verified_copy(import_job(&flow))
            .await
            .expect_err("the copy cannot overwrite an existing file");

        assert!(
            error.to_string().contains("copy the source database"),
            "the failure is the copy: {error}"
        );
        assert!(
            !tmp_db.exists(),
            "and the leftover is removed: {}",
            tmp_db.display()
        );
        for suffix in super::COPY_SIDECAR_SUFFIXES {
            assert!(
                !sidecar_path(&tmp_db, suffix).exists(),
                "including its {suffix}"
            );
        }
    }

    /// Fix round 9, K4: when that cleanup cannot remove the temporary, the failure
    /// is REPORTED — the file is still there, and the operator has to know.
    #[tokio::test]
    async fn a_cleanup_that_cannot_remove_the_temporary_is_reported() {
        let flow = flow().await;
        let tmp_db = super::temp_db_path(&flow.target_db, "import");
        fs::create_dir_all(tmp_db.parent().expect("the temporary's directory"))
            .expect("create the target's directory");
        fs::write(&tmp_db, b"a leftover from a failed run").expect("plant the leftover");
        publish::probe::fail_next_remove_of(&tmp_db);

        let error = run_verified_copy(import_job(&flow))
            .await
            .expect_err("the copy cannot overwrite an existing file");

        let message = format!("{error:#}");
        assert!(
            message.contains("copy the source database"),
            "the copy failure is there: {message}"
        );
        assert!(
            message.contains("[cleanup]"),
            "and the cleanup failure rides with it: {message}"
        );
        assert!(
            message.contains(&tmp_db.display().to_string()),
            "naming the file it could not remove: {message}"
        );
        assert!(tmp_db.exists(), "which is still there");
    }

    /// Fix round 3, M3: once the install's rename has landed, NO quarantine file
    /// outlives the install on any path. The replaced database's sidecars are
    /// dropped immediately after the rename — before the directory sync and the
    /// WAL switch can fail — so a later failure cannot leave them behind.
    #[tokio::test]
    async fn a_failed_wal_conversion_leaves_no_quarantine_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        // A copy that is not a database at all: the rename lands and the WAL
        // switch (the step after the quarantine cleanup) fails.
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        fs::write(&tmp_db, b"not a database").expect("write the copy");
        fs::write(&target, b"the old target").expect("write the old target");
        for suffix in super::DB_STATE_SUFFIXES {
            fs::write(sidecar_path(&target, suffix), b"the old target's state")
                .expect("write the old target's sidecar");
        }

        let failure = install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect_err("a copy that is not a database cannot be put back in WAL");
        let InstallFailure::Installed { error, .. } = failure else {
            panic!("the rename landed, so the database IS installed: {failure:?}");
        };
        assert!(
            error.to_string().contains("put back in WAL"),
            "the failure is the WAL switch: {error}"
        );

        let names: Vec<String> = fs::read_dir(dir.path())
            .expect("read the directory")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().all(|name| !name.contains(".quarantine-")),
            "no quarantine outlives a landed install: {names:?}"
        );
        for suffix in super::DB_STATE_SUFFIXES {
            assert!(
                !sidecar_path(&target, suffix).exists(),
                "and the old target's {suffix} is gone: {names:?}"
            );
        }
    }

    /// Fix round 3, M3: if the quarantine cleanup itself fails, the installed
    /// error names the files still on disk — nothing of the replaced database may
    /// be left behind silently.
    #[tokio::test]
    async fn a_quarantine_that_cannot_be_cleaned_up_is_named_in_the_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("pulse.db");
        let tmp_db = dir.path().join(".pulse.db.import-tmp-1.db");
        let copy = open_migrated_copy(&tmp_db).await.expect("the copy");
        copy.pool().close().await;
        fs::write(&target, b"the old target").expect("write the old target");
        let sidecar = sidecar_path(&target, "-wal");
        fs::write(&sidecar, b"the old target's rows").expect("write the -wal");
        let quarantine = super::quarantine_path(&target, "-wal", super::inode_of(&target))
            .expect("the quarantine name");
        publish::probe::fail_next_remove_of(&quarantine);

        let failure = install_tmp_db(&tmp_db, &target, Some(dir.path()))
            .await
            .expect_err("the quarantine cannot be cleaned up");
        let InstallFailure::Installed { error, .. } = failure else {
            panic!("the rename landed, so the database IS installed: {failure:?}");
        };

        let message = error.to_string();
        assert!(
            message.contains("could not all be cleaned up"),
            "the error says the cleanup failed: {message}"
        );
        assert!(
            message.contains(&quarantine.display().to_string()),
            "and names the quarantine still on disk ({}): {message}",
            quarantine.display()
        );
        assert!(
            quarantine.exists(),
            "the bytes are still there under the name the error gives"
        );
    }

    /// Fix round 2, N4: when the copied database's own flush fails, the temporary
    /// (and every sidecar it could have) is removed before the error returns —
    /// nothing has been installed, and a half-flushed copy must not be left for
    /// the next run to trip over.
    #[tokio::test]
    async fn a_failed_copy_flush_removes_the_temporary() {
        let flow = flow().await;
        let tmp_db = super::temp_db_path(&flow.target_db, "import");
        publish::probe::fail_next_file_sync();

        let error = run_verified_copy(import_job(&flow))
            .await
            .expect_err("the injected file sync failure ends the run");

        assert!(
            error.to_string().contains("flush the copied database"),
            "the failure names the flush: {error}"
        );
        assert!(
            !tmp_db.exists(),
            "the temporary database is removed: {}",
            tmp_db.display()
        );
        let left: Vec<String> = fs::read_dir(flow.target_db.parent().expect("the target's dir"))
            .expect("read the target's directory")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains("import-tmp"))
            .collect();
        assert!(left.is_empty(), "and nothing of the copy is left: {left:?}");
        assert!(
            !flow.target_db.exists(),
            "nothing was installed either: {}",
            flow.target_db.display()
        );
    }

    /// Fix round 1, F5: `VACUUM INTO` does not guarantee its output is on disk,
    /// and the install renames that file — so the copy's bytes are fsynced before
    /// the name that promises them exists. The seam records both, which is what
    /// proves the ORDER: the file sync sees no destination, the directory sync
    /// sees it.
    #[tokio::test]
    async fn the_copied_database_is_flushed_before_its_rename() {
        let flow = flow().await;
        let _ = publish::probe::take();

        run_verified_copy(import_job(&flow))
            .await
            .expect("the import succeeds");

        let events = publish::probe::take();
        let database_file = events
            .iter()
            .find(|event| {
                event.kind == publish::probe::SyncKind::File && event.destination == flow.target_db
            })
            .unwrap_or_else(|| panic!("the copied database is fsynced: {events:?}"));
        assert!(
            !database_file.destination_present,
            "the file sync runs BEFORE the rename: {database_file:?}"
        );
        assert!(
            events.iter().any(|event| {
                event.kind == publish::probe::SyncKind::Dir
                    && event.destination == flow.target_db
                    && event.destination_present
            }),
            "and the directory sync runs AFTER it: {events:?}"
        );
    }

    /// Fix round 1, F1: once the install's rename has landed the database IS in
    /// place, so a failure AFTER it must not run the pre-rename undo — deleting
    /// the snapshots this run copied or rewinding a HEAD pointer would leave the
    /// installed database pointing at rows that are gone.
    #[tokio::test]
    async fn a_post_install_failure_leaves_the_snapshots_and_heads_it_wrote_in_place() {
        let flow = flow().await;
        let (source_snapshots, _) = scan_snapshots(&flow.source_data);
        let (source_heads, _) = scan_heads(&flow.source_data);
        assert!(
            !source_snapshots.is_empty(),
            "the fixture store has snapshots"
        );
        assert!(!source_heads.is_empty(), "and HEAD pointers");
        // The install's own directory sync — the one AFTER the rename — is injected
        // to fail. It is keyed on the published extension rather than on the
        // directory because the quarantine's sync runs in that same directory
        // BEFORE the rename (fix round 6, C1).
        publish::probe::fail_next_publish_with_extension("db");

        let error = run_verified_copy(import_job(&flow))
            .await
            .expect_err("the injected post-rename failure ends the run");

        assert!(
            error.to_string().contains("IS installed"),
            "the error names the installed state: {error}"
        );
        assert!(flow.target_db.exists(), "the database IS installed");
        let target_store = CandleStore::with_base_dir(flow.target_data.clone());
        for snapshot in &source_snapshots {
            let dest =
                target_store.snapshot_path(&snapshot.pair, snapshot.timeframe, &snapshot.version);
            assert!(
                dest.exists(),
                "the snapshot this run copied was NOT undone: {}",
                dest.display()
            );
        }
        for head in &source_heads {
            assert_eq!(
                target_store
                    .read_head(&head.pair, head.timeframe)
                    .expect("read the target's HEAD pointer"),
                Some(head.version.clone()),
                "the HEAD pointer this run published was NOT rewound ({})",
                head_of(&flow, &head.pair, head.timeframe).display()
            );
        }
    }

    /// Fix round 1, F8: a snapshot whose RENAME landed is recorded as added even
    /// when the directory sync after it fails, so the rollback removes it — a
    /// snapshot left behind under a valid name is one every later run refuses as
    /// a byte mismatch.
    #[test]
    fn a_snapshot_whose_directory_sync_fails_is_still_rolled_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source_data = dir.path().join("mac-data");
        let leaf = source_data.join("candles").join("BTCUSDT").join("15m");
        fs::create_dir_all(&leaf).expect("the source store");
        fs::write(leaf.join("b51388284a3a4371.parquet"), b"snapshot bytes")
            .expect("write the source snapshot");
        let (snapshots, issues) = scan_snapshots(&source_data);
        assert!(issues.is_empty(), "the source store is well-formed");
        assert_eq!(snapshots.len(), 1, "one source snapshot");

        let target_store = CandleStore::with_base_dir(dir.path().join("server-data"));
        let dest = target_store.snapshot_path(
            &snapshots[0].pair,
            snapshots[0].timeframe,
            &snapshots[0].version,
        );
        publish::probe::fail_next_sync_of(dest.parent().expect("the leaf's directory"));
        let mut mismatches = Vec::new();

        let (error, added) = copy_snapshots(&target_store, &snapshots, &mut mismatches)
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
        assert!(
            error.to_string().contains("fsync directory"),
            "the failure is the injected sync: {error}"
        );

        remove_added_snapshots_keeping(&added, &|_| false).expect("the snapshot is removable");
        assert!(
            !dest.exists(),
            "the rollback removed the snapshot the failed copy published"
        );
    }

    /// Fix round 11, R2: the dedup that decides whether the destination is this
    /// run's to record — and therefore to DELETE on a rollback — must not read a
    /// stat failure as "absent". An unreadable destination refuses the copy, and
    /// records nothing.
    #[test]
    fn a_snapshot_whose_presence_cannot_be_checked_refuses_the_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source_data = dir.path().join("mac-data");
        let leaf = source_data.join("candles").join("BTCUSDT").join("15m");
        fs::create_dir_all(&leaf).expect("the source store");
        fs::write(leaf.join("b51388284a3a4371.parquet"), b"snapshot bytes")
            .expect("write the source snapshot");
        let (snapshots, issues) = scan_snapshots(&source_data);
        assert!(issues.is_empty(), "the source store is well-formed");

        let target_store = CandleStore::with_base_dir(dir.path().join("server-data"));
        let dest = target_store.snapshot_path(
            &snapshots[0].pair,
            snapshots[0].timeframe,
            &snapshots[0].version,
        );
        publish::probe::fail_next_stat_of(&dest);
        let mut mismatches = Vec::new();

        let (error, added) = copy_snapshots(&target_store, &snapshots, &mut mismatches)
            .expect_err("an unreadable destination refuses the copy");

        assert!(
            error.to_string().contains(&dest.display().to_string()),
            "the refusal names the path it could not stat: {error}"
        );
        assert!(
            added.is_empty(),
            "and nothing is recorded for the rollback to delete: {added:?}"
        );
        assert!(
            !dest.exists(),
            "no snapshot was written either: {}",
            dest.display()
        );
        assert!(
            mismatches.is_empty(),
            "an unreadable destination is a refusal, not a byte mismatch ({} recorded)",
            mismatches.len()
        );
    }

    /// Fix round 1, F6: a relative path whose first component does not exist
    /// resolves its ancestor to the CURRENT DIRECTORY (never `None`) — the entry
    /// that component creates lands there, so that is what has to be synced.
    #[test]
    fn a_relative_path_with_no_existing_component_syncs_the_current_directory() {
        let relative = Path::new("no-such-directory-3f7a/pulse.db");
        assert_eq!(
            super::publish::existing_ancestor(relative.parent().expect("a parent")),
            Some(PathBuf::from(".")),
            "the walk ends at the current directory, never at nothing"
        );
        assert_eq!(
            super::publish::levels_to_sync(relative, Some(Path::new("."))),
            vec![PathBuf::from("no-such-directory-3f7a"), PathBuf::from(".")],
            "the created level, then the directory that holds it"
        );
    }

    /// Issue #259: a backup's OWN `HEAD` manifest is published before the
    /// database that references it, so the directory the manifest landed in is
    /// fsynced too — a database must not outlive its manifest.
    #[test]
    fn a_published_manifest_syncs_its_directory_after_the_rename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = dir.path().join("pulse-20260101T000000Z.db.heads.json");
        let _ = publish::probe::take();

        write_head_manifest(&manifest, &[], Some(dir.path())).expect("write the manifest");

        let syncs = publish::probe::take();
        assert_eq!(
            syncs
                .iter()
                .filter(|event| event.kind == publish::probe::SyncKind::Dir)
                .count(),
            1,
            "one publish, one directory sync: {syncs:?}"
        );
        assert_eq!(syncs[0].path, dir.path(), "the out-dir it landed in");
        assert!(
            syncs[0].destination_present,
            "the sync happens AFTER the rename, never before it: {syncs:?}"
        );
        assert!(manifest.exists(), "the manifest is published");
    }

    // -----------------------------------------------------------------------
    // r4.s2.w2 — the move made safe (#250, #264, the paper digests)
    // -----------------------------------------------------------------------

    /// Fold any write-ahead log into the file before a byte comparison:
    /// SQLite's checkpoint-on-close is asynchronous relative to the pool's
    /// `close()`, and the "source untouched" checks must not race it.
    async fn settle_wal(db_path: &Path) {
        let db = Db::with_path(db_path)
            .await
            .expect("open to settle the WAL");
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(db.pool())
            .await
            .expect("checkpoint the WAL");
        db.pool().close().await;
    }

    /// Seed the flow's source with one strategy + version (through the REAL
    /// repository — the import's repository-read check re-derives the version
    /// hash), one paper session, `events` event rows and `bars` bar rows.
    async fn seed_paper_rows(flow: &Flow, events: i64, bars: i64) {
        use crate::adapters::db::SqliteStrategyRepo;
        use crate::domain::StrategyRepository as _;
        use crate::domain::strategy::{CreatedBy, NewVersion, StrategyId};

        let db = Db::with_path(&flow.source_db)
            .await
            .expect("open the source");
        let pool = db.pool().clone();
        let repo = SqliteStrategyRepo::new(pool.clone());
        let strategy = repo
            .create_strategy("move safety", None, &[])
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
              uses_d1, starting_equity, taker_fee_bps, slippage_bps, engine_fingerprint, \
              graduation, walk_forward_run_id, override_reason, override_at, \
              certified_data_versions, fixture, min_trades, promoted_by) \
             VALUES ('sess-1', 1, ?1, '2026-01-01T00:00:00.000Z', 'BTCUSDT', '15m', NULL, 0, \
                     '10000', '4', '1', 'fp', 'override', NULL, 'move safety', \
                     '2026-01-01T00:00:00.000Z', '[]', 0, 1, 'operator-token')",
        )
        .bind(version.id.as_str())
        .execute(&pool)
        .await
        .expect("seed the paper session");
        for seq in 1..=events {
            sqlx::query(
                "INSERT INTO paper_event (session_id, seq, at, kind, payload) \
                 VALUES ('sess-1', ?1, '2026-01-01T00:00:00.000Z', 'bar_processed', ?2)",
            )
            .bind(seq)
            .bind(format!("payload-{seq}"))
            .execute(&pool)
            .await
            .expect("seed a paper event");
        }
        for seq in 1..=bars {
            sqlx::query(
                "INSERT INTO paper_bar \
                 (session_id, timeframe, seq, open_time, close_time, open, high, low, close, \
                  volume, funding_rate, lead_in) \
                 VALUES ('sess-1', '15m', ?1, ?2, ?3, '60000', '60100', '59900', '60050', '100', \
                         NULL, 0)",
            )
            .bind(seq)
            .bind(1_735_689_600_000_i64 + seq * 900_000)
            .bind(1_735_689_600_000_i64 + seq * 900_000 + 899_999)
            .execute(&pool)
            .await
            .expect("seed a paper bar");
        }
        db.pool().close().await;
        settle_wal(&flow.source_db).await;
    }

    /// The append-only triggers the tamper needs out of the way (the source is
    /// this test's own file).
    async fn drop_paper_event_triggers(db_path: &Path) {
        let holder = Db::with_path(db_path).await.expect("open the writer");
        for trigger in ["paper_event_no_update", "paper_event_no_delete"] {
            sqlx::query(&format!("DROP TRIGGER {trigger}"))
                .execute(holder.pool())
                .await
                .expect("drop the append-only trigger");
        }
        holder.pool().close().await;
    }

    /// Change exactly one column of one source `paper_event` row.
    async fn change_one_paper_event(db_path: &Path) {
        let holder = Db::with_path(db_path).await.expect("open the writer");
        sqlx::query(
            "UPDATE paper_event SET payload = 'changed after the copy' \
             WHERE session_id = 'sess-1' AND seq = 2",
        )
        .execute(holder.pool())
        .await
        .expect("change one column");
        holder.pool().close().await;
    }

    /// #264: a source that IS the target is refused before the run touches
    /// anything — the same path, a symlink to it, and equal data dirs.
    #[tokio::test]
    async fn a_source_that_is_the_target_is_refused_before_anything_is_written() {
        let flow = flow().await;
        let before = fs::read(&flow.source_db).expect("read the source");

        let mut job = import_job(&flow);
        job.db_target = &flow.source_db;
        let error = run_verified_copy(job)
            .await
            .expect_err("a source that IS the target is refused");
        assert!(error.to_string().contains("IS the target"), "{error}");

        let link = flow.dir.path().join("link.db");
        std::os::unix::fs::symlink(&flow.source_db, &link).expect("symlink the source");
        let mut job = import_job(&flow);
        job.db_target = &link;
        let error = run_verified_copy(job)
            .await
            .expect_err("a symlink to the source is refused too");
        assert!(error.to_string().contains("IS the target"), "{error}");

        let mut job = import_job(&flow);
        job.data_target = &flow.source_data;
        let error = run_verified_copy(job)
            .await
            .expect_err("equal data dirs are refused");
        assert!(
            error.to_string().contains("IS the target data dir"),
            "{error}"
        );

        assert_eq!(
            fs::read(&flow.source_db).expect("re-read the source"),
            before,
            "the source is never touched"
        );
        assert!(!flow.target_db.exists(), "and nothing was installed");
    }

    /// #250: a held instance lock on the target refuses the run by name, and
    /// dropping the holder lets the same run through.
    #[tokio::test]
    async fn a_held_target_lock_refuses_the_run() {
        let flow = flow().await;
        let holder = InstanceLock::acquire(&flow.target_db).expect("hold the target's lock");
        let error = run_verified_copy(import_job(&flow))
            .await
            .expect_err("a held lock refuses the run");
        assert!(
            error.to_string().contains("a running pulse serve holds it"),
            "{error}"
        );

        drop(holder);
        run_verified_copy(import_job(&flow))
            .await
            .expect("the lock is free and the import runs");
        assert!(flow.target_db.exists(), "the import installed the target");
    }

    /// The digest comparison driven exactly as the engine drives it: copy the
    /// source, change one column AFTER the copy, verify — the mismatch names the
    /// table and the first differing key.
    #[tokio::test]
    async fn a_source_changed_after_the_copy_names_the_table_and_key() {
        let flow = flow().await;
        seed_paper_rows(&flow, 2, 0).await;
        drop_paper_event_triggers(&flow.source_db).await;

        let tmp_db = super::temp_db_path(&flow.target_db, "import");
        fs::create_dir_all(tmp_db.parent().expect("the target's directory"))
            .expect("create the target's directory");
        let source = crate::adapters::db::ops::open_read_only(&flow.source_db)
            .await
            .expect("open the source");
        crate::adapters::db::ops::vacuum_into_copy(&source, &tmp_db)
            .await
            .expect("copy the source");
        source.close().await;

        // "injected after the copy": one changed column in one source row.
        change_one_paper_event(&flow.source_db).await;

        let opened = open_migrated_copy(&tmp_db).await.expect("the copy opens");
        let failure = run_steps(&import_job(&flow), &opened, None).await;
        opened.pool().close().await;

        let Err(StepFailure::Mismatches { mismatches, .. }) = failure else {
            panic!("the changed source is a verification mismatch");
        };
        let named = mismatches
            .iter()
            .find(|mismatch| mismatch.table == "paper_event")
            .expect("the paper_event digest is named");
        assert_eq!(
            named.id, "session_id=sess-1 seq=2",
            "the first differing key"
        );
        assert_eq!(named.field, "digest");
        let _ = fs::remove_file(&tmp_db);
    }

    /// The whole run refuses when the source changes between its copy and the
    /// verification reads — the deterministic arm of the digest refusal (the
    /// integration suite's timed arm only corroborates the same refusal).
    #[tokio::test]
    async fn a_source_changed_after_the_copy_refuses_the_run() {
        let flow = flow().await;
        seed_paper_rows(&flow, 2, 0).await;
        drop_paper_event_triggers(&flow.source_db).await;
        let source_db = flow.source_db.clone();
        set_source_mutation(Box::new(move || {
            Box::pin(async move {
                let writer = Db::with_path(&source_db).await.map_err(|e| e.to_string())?;
                sqlx::query(
                    "UPDATE paper_event SET payload = 'changed after the copy' \
                     WHERE session_id = 'sess-1' AND seq = 2",
                )
                .execute(writer.pool())
                .await
                .map_err(|e| e.to_string())?;
                writer.pool().close().await;
                Ok(())
            })
        }));

        let error = run_verified_copy(import_job(&flow))
            .await
            .expect_err("the digest refuses the changed source");
        assert!(error.to_string().contains("refused"), "{error}");
        assert!(
            !flow.target_db.exists(),
            "nothing was installed: {}",
            flow.target_db.display()
        );
        let leftovers: Vec<PathBuf> =
            fs::read_dir(flow.target_db.parent().expect("the target's directory"))
                .expect("read the target's directory")
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            // The migration lock beside the copy is inert and is
                            // never removed by design; the COPY itself must be gone.
                            name.contains("import-tmp") && !name.ends_with(".migrate.lock")
                        })
                })
                .collect();
        assert!(leftovers.is_empty(), "the temporary is gone: {leftovers:?}");
    }

    /// PR-354 fix C3c: `created_root` is computed BEFORE the instance lock's
    /// `create_dir_all`. The fixture's target directory (`<temp>/server`) does
    /// not exist yet, so the pre-existing ancestor is the tempdir itself: the
    /// install's directory syncs must reach it (its entry is the one that
    /// received the new directory). With the lock first, the created root WAS
    /// `server/` and the tempdir's sync never ran — the entry was left
    /// unflushed, a regression from main.
    #[tokio::test]
    async fn a_first_import_syncs_the_parent_that_received_the_new_directory() {
        let flow = flow().await;
        assert!(
            !flow.target_db.parent().expect("a parent").exists(),
            "the target's directory is this run's to create"
        );
        let _ = publish::probe::take();
        // Keyed on the directory the walk must REACH: the injection fires only
        // if `levels_to_sync` climbed past the target's own directory.
        publish::probe::fail_next_sync_of(flow.dir.path());

        let error = run_verified_copy(import_job(&flow))
            .await
            .expect_err("the first import must sync the pre-existing ancestor");
        let message = error.to_string();
        assert!(
            message.contains("injected failure")
                && message.contains(&flow.dir.path().display().to_string()),
            "the failure is the pre-existing ancestor's own sync: {message}"
        );
    }

}
