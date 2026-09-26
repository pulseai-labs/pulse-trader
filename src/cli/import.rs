//! `pulse import` — D7's verified one-way move of a Mac `pulse.db` + its
//! candle snapshots onto this host (r3.s3.w4, ADR-0026).
//!
//! The order (spec §Interface contract): refuse a non-empty target (with
//! `--replace`, back it up first through the same backup `pulse backup`
//! makes); copy the source consistently with `VACUUM INTO` from a read-only
//! open into a temporary file BESIDE the target (never `/tmp`); migrate the
//! copy forward (`open_migrated`); copy the snapshots (an existing same-named
//! file must be byte-identical); verify everything — row counts, stored
//! hashes, repository reads, snapshot reads, referenced snapshots — without
//! stopping at the first failure within a check; then atomically rename the
//! temporary database into place, `chmod a-w` the source path, and print the
//! summary. On any failure every temporary is deleted, every snapshot this
//! run added is removed, the target is untouched, and every mismatch is named
//! (up to 20, then "…and N more").
//!
//! `pulse restore` (`cli/backup.rs`) reuses [`run_verified_copy`] with the
//! backup as the source: same verification, same atomic install, no chmod.
//!
//! **Its precondition: the server must be stopped.** The install deletes the
//! target's `-wal`/`-shm` and renames over `pulse.db`, so a running `pulse
//! serve` would keep serving the unlinked old inode, the deleted WAL content is
//! gone, and every write it commits after the swap is silently lost. Import
//! does not check the process — the same posture restore documents: the CLI's
//! `--help` states the precondition, and the operator's cutover order is D7's —
//! quit the old Mac app, then import.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use clap::Args;
use serde::{Deserialize, Serialize};

use super::publish;
#[cfg(test)]
use super::publish::probe;
use crate::adapters::db::default_db_path;
use crate::adapters::db::ops;
use crate::adapters::db::{
    Db, SqliteBacktestRunRepo, SqliteStrategyRepo, open_migrated_copy, put_in_wal,
};
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
/// **Precondition: the server must be stopped.** Import does not check the
/// process; the install renames over `pulse.db` and deletes its stale
/// `-wal`/`-shm`, so a running `pulse serve` would keep serving the unlinked
/// old file and lose every write it commits afterwards. D7's cutover order:
/// quit the old Mac app, then import on the server host with the unit stopped.
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
    fn new(table: &str, id: impl Into<String>, field: &str, detail: impl Into<String>) -> Self {
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
    let home = directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .ok_or_else(|| anyhow!("cannot resolve the home directory for the default backup dir"))?;
    Ok(home.join("pulse-backups"))
}

/// The shared verified-copy engine (import over the Mac source; restore over
/// a backup — the same verification, the same atomic install).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on any refusal, mismatch or failure; the
/// target is left exactly as it was and every temporary is deleted.
pub(crate) async fn run_verified_copy(job: VerifiedCopy<'_>) -> anyhow::Result<()> {
    let label = job.source_label;

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
    // same filesystem, under the target's directory, never /tmp).
    //
    // The deepest ancestor that exists BEFORE the target's directory is created:
    // every level below it is an entry this run makes, so the install's directory
    // sync has to cover them (fix round 1, F6).
    let created_root = job.db_target.parent().and_then(publish::existing_ancestor);
    if let Some(parent) = job.db_target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create target directory {}: {e}", parent.display()))?;
    }
    let tmp_db = temp_db_path(job.db_target, label)?;
    let source = ops::open_read_only(job.from_db)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let copied = ops::vacuum_into_copy(&source, &tmp_db).await;
    source.close().await;
    copied.map_err(|e| anyhow!("copy the source database: {e}"))?;
    // `VACUUM INTO` does not guarantee its output is on disk, and the install
    // renames this file: the bytes must land before the name that promises them
    // (fix round 1, F5).
    publish::sync_file(&tmp_db, job.db_target)
        .map_err(|error| anyhow!("flush the copied database: {error}"))?;

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
            remove_tmp_db(&tmp_db);
            return Err(anyhow!("migrate the copy forward: {e}"));
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
    let undo = |writes: &StoreWrites| {
        remove_added_snapshots(&writes.added);
        restore_heads(&target_store, &writes.heads);
    };
    match steps {
        Err(StepFailure::Fatal { error, writes }) => {
            // Every fatal path cleans up BOTH: the temporary database and
            // everything this run wrote to the target store before it failed.
            remove_tmp_db(&tmp_db);
            undo(&writes);
            Err(error)
        }
        Err(StepFailure::Mismatches { mismatches, writes }) => {
            remove_tmp_db(&tmp_db);
            undo(&writes);
            print_refusal(label, &mismatches);
            anyhow::bail!(
                "{label}: refused — the temporary database and every snapshot this run added \
                 were deleted, every pointer it moved was put back, and the target is exactly \
                 as it was"
            );
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
                    remove_tmp_db(&tmp_db);
                    undo(&writes);
                    return Err(error);
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

/// Step 5c: every version and every run reads back through its repository on
/// the COPY — the tamper defenses (`version_hash` re-derived on read; #39's
/// `result_content_hash` re-derived from the trades).
async fn step_repository_reads(
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
fn step_snapshot_reads(
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
async fn step_referenced_snapshots(
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

/// The timeframe directory name (`15m` / `4h`) a snapshot lives under.
fn timeframe_from_interval(name: &str) -> Option<Timeframe> {
    match name {
        "15m" => Some(Timeframe::M15),
        "4h" => Some(Timeframe::H4),
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

/// Put back every pointer a publish step changed — the failure path: the store
/// must be left exactly as it was.
pub(crate) fn restore_heads(target_store: &CandleStore, changes: &[HeadChange]) {
    for change in changes {
        match &change.previous {
            Some(version) => {
                let _ = target_store.write_head(&change.pair, change.timeframe, version);
            }
            None => {
                let _ = fs::remove_file(target_store.head_path(&change.pair, change.timeframe));
            }
        }
    }
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
            if dest.exists() {
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
            // rollback must remove. The name is known free (the `exists` check
            // above), so recording it up front cannot delete anything this run
            // did not write.
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
/// install rename is atomic on the same filesystem; never `/tmp`).
fn temp_db_path(target: &Path, label: &str) -> anyhow::Result<PathBuf> {
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("target {} has no parent directory", target.display()))?;
    let stem = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("pulse");
    Ok(dir.join(format!(".{stem}.{label}-tmp-{}.db", std::process::id())))
}

/// Delete the temporary database and EVERY sidecar a copy can have (failure
/// path) — the same suffix set the refusal checks, from the one constant, so a
/// sidecar can never be refused and then left behind (fix round 1, F7).
fn remove_tmp_db(tmp_db: &Path) {
    let _ = fs::remove_file(tmp_db);
    for suffix in COPY_SIDECAR_SUFFIXES {
        let _ = fs::remove_file(sidecar_path(tmp_db, suffix));
    }
}

/// Delete every snapshot this run added (failure path — the target keeps
/// exactly its previous contents).
fn remove_added_snapshots(added: &[PathBuf]) {
    for path in added {
        let _ = fs::remove_file(path);
    }
}

/// The suffixes `SQLite` puts a database's own un-checkpointed state in: the
/// files the install moves aside beside the TARGET, so an old hot journal cannot
/// be replayed into the freshly installed file (fix round 1, F3).
const DB_STATE_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];

/// [`DB_STATE_SUFFIXES`] plus this crate's own copy temporary (`.partial`):
/// every suffix a file beside the temporary COPY can hold state in.
///
/// ONE source of truth (fix round 1, F7): the refusal checks these suffixes and
/// the cleanup removes these suffixes, so a suffix can never be checked but left
/// behind.
const COPY_SIDECAR_SUFFIXES: [&str; 4] = ["-wal", "-shm", "-journal", ".partial"];

/// Append a suffix to a path's FINAL component, through the raw `OsStr` — never
/// `display()`, which is lossy for a name that is not valid UTF-8 (the migration
/// protocol's own `sidecar_path` does the same; fix round 1, F7).
fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
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
}

/// Refuse the install when any sidecar still sits beside the copy (#258).
///
/// Fail closed: the check deletes nothing and renames nothing, so the target is
/// left exactly as it was and the caller's failure path removes the temporary
/// (with its sidecars) for a clean retry.
///
/// # Errors
///
/// Returns [`InstallRefusal::Sidecar`] for the first sidecar found.
fn refuse_stranded_sidecar(tmp_db: &Path) -> anyhow::Result<()> {
    for suffix in COPY_SIDECAR_SUFFIXES {
        let sidecar = sidecar_path(tmp_db, suffix);
        if sidecar.exists() {
            return Err(InstallRefusal::Sidecar {
                db: tmp_db.to_path_buf(),
                suffix,
                sidecar,
            }
            .into());
        }
    }
    Ok(())
}

/// The quarantine name of one of the target's sidecars: a hidden sibling that no
/// database name matches, unique to this process, and therefore inert even if a
/// crash leaves one behind.
fn quarantine_path(target: &Path, suffix: &str) -> anyhow::Result<PathBuf> {
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("unusable target path: {}", target.display()))?;
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("target {} has no parent directory", target.display()))?;
    Ok(dir.join(format!(".{name}{suffix}.quarantine-{}", std::process::id())))
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
        for suffix in DB_STATE_SUFFIXES {
            // (the test seam records what was moved, below)
            let original = sidecar_path(target, suffix);
            if !original.exists() {
                continue;
            }
            let quarantine = quarantine_path(target, suffix)?;
            // Never reuse a quarantine a crashed run left: those bytes are not
            // this run's.
            let _ = fs::remove_file(&quarantine);
            fs::rename(&original, &quarantine).map_err(|error| {
                anyhow!(
                    "quarantine the target's {suffix} sidecar {} -> {}: {error}",
                    original.display(),
                    quarantine.display()
                )
            })?;
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

    /// Put every quarantined sidecar back: a failed install leaves the old
    /// target exactly as it was, un-checkpointed commits included.
    fn restore(&self) {
        for (quarantine, original) in &self.moved {
            let _ = fs::rename(quarantine, original);
        }
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
        for (quarantine, _) in &self.moved {
            match fs::remove_file(quarantine) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(anyhow!("remove {}: {error}", quarantine.display()));
                }
            }
        }
        for dir in self.directories() {
            publish::sync_dir(&dir)?;
        }
        Ok(())
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
/// rename durable, put the installed file back in WAL, and drop what was moved
/// aside.
///
/// # Errors
///
/// [`InstallFailure::Untouched`] when nothing was renamed (the refusal, the
/// quarantine, or the rename itself failed — the target's sidecars are put
/// back), and [`InstallFailure::Installed`] when the rename landed and a later
/// step failed, so the caller must not run its pre-rename cleanup.
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
    // 3. The rename itself. Past this point the database IS installed.
    if let Err(error) = fs::rename(tmp_db, target) {
        quarantined.restore();
        return Err(InstallFailure::Untouched(anyhow!(
            "install {} -> {}: {error}",
            tmp_db.display(),
            target.display()
        )));
    }
    let installed = |unconfirmed: &str, error: anyhow::Error| InstallFailure::Installed {
        target: target.to_path_buf(),
        error: anyhow!("{unconfirmed}: {error}"),
    };
    // 4. The rename is durable only once the directory that received it is
    //    fsynced — including every level this run created (issue #259, F6).
    if let Err(error) = publish::sync_published(target, created) {
        return Err(installed(
            "its directory could not be fsynced, so the new name is not durable",
            error,
        ));
    }
    // 5. ADR-0019, deterministically (F4): the installed file is in WAL, read
    //    back, before this command reports success — never left to the next
    //    process to fix with a fire-and-forget pragma.
    if let Err(error) = put_in_wal(target).await {
        return Err(installed(
            "it could not be put back in WAL, so the production journal mode is not in place",
            anyhow::Error::new(error),
        ));
    }
    // 6. What was moved aside belonged to the database that was just replaced:
    //    drop it (the leftovers are inert — no database name matches them).
    if let Err(error) = quarantined.discard() {
        return Err(installed(
            "the replaced database's quarantined sidecars could not be removed (they are inert: \
             no database name matches them)",
            error,
        ));
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
fn print_refusal(label: &str, mismatches: &[Mismatch]) {
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
        COPY_SIDECAR_SUFFIXES, HeadSource, InstallFailure, InstallRefusal, VerifiedCopy,
        copy_snapshot_into, copy_snapshots, install_tmp_db, publish, remove_added_snapshots,
        remove_tmp_db, run_verified_copy, scan_heads, scan_snapshots, sidecar_path,
        write_head_manifest,
    };
    use crate::adapters::db::{open_migrated, open_migrated_copy};
    use crate::adapters::store::CandleStore;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

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
        _dir: TempDir,
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
        let target_db = dir.path().join("server").join("pulse.db");
        let target_data = dir.path().join("server-data");
        Flow {
            _dir: dir,
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

        remove_tmp_db(&tmp_db);

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
    #[cfg(unix)]
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
        assert!(
            syncs.iter().any(|sync| sync.path == *dir.path()
                && sync.destination == target
                && sync.destination_present),
            "the target's own directory is synced AFTER the rename: {syncs:?}"
        );
        assert!(target.exists(), "and the database is there");
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
        // The install's own directory sync is injected to fail: the rename has
        // already landed when the error comes back.
        publish::probe::fail_next_sync_of(flow.target_db.parent().expect("the target's directory"));

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

        remove_added_snapshots(&added);
        assert!(
            !dest.exists(),
            "the rollback removed the snapshot the failed copy published"
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
}
