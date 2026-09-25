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

use crate::adapters::db::default_db_path;
use crate::adapters::db::ops;
use crate::adapters::db::{Db, SqliteBacktestRunRepo, SqliteStrategyRepo, open_migrated};
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
    if let Some(parent) = job.db_target.parent() {
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

    // ---- Steps 3–5: migrate the copy forward, copy the snapshots, verify.
    let opened = match open_migrated(&tmp_db).await {
        Ok(db) => db,
        Err(e) => {
            remove_tmp_db(&tmp_db);
            return Err(anyhow!("migrate the copy forward: {e}"));
        }
    };
    let steps = run_steps(&job, &opened, manifest_heads.as_deref()).await;
    // CLOSE the copy's pool — do not merely drop the handle — before any file
    // surgery on either path. A dropped handle releases the pool without
    // closing its connections, so committed rows could still sit in the
    // temporary database's `-wal` when the install renames only the database
    // file: the rename would publish a database whose writes live in a WAL left
    // behind (and the target's own stale sidecars are removed on the way in), so
    // a fresh reader would see a database missing its most recent commits.
    // Closing checkpoints the WAL into the file and removes it.
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
            // ---- Step 7: atomic install. The target's stale -wal/-shm are
            // removed first so an old sidecar can never bleed into the
            // renamed file (the migration-protocol restore precedent).
            if let Err(error) = install_tmp_db(&tmp_db, job.db_target) {
                // The install is the last thing that can fail: it cleans up
                // exactly as every earlier fatal path does.
                remove_tmp_db(&tmp_db);
                undo(&writes);
                return Err(error);
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
/// crash never leaves a partial one under the final name.
fn write_file_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
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
    })
}

/// Write one backup's pointer manifest beside it (atomically: the same
/// discipline the rest of the surface uses).
pub(crate) fn write_head_manifest(path: &Path, heads: &[SourceHead]) -> anyhow::Result<()> {
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
    write_file_atomic(path, &bytes)
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
            copy_snapshot_into(&snap.path, &dest)?;
            added.push(dest);
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
/// Shared with `cli/backup.rs`, so the backup store's copy is the same copy the
/// import makes.
///
/// # Errors
///
/// Any I/O failure — creating the destination's directory, copying, flushing or
/// renaming — with the temporary removed. A temporary left by an earlier
/// crashed run is deleted first: its bytes are not this run's.
pub(crate) fn copy_snapshot_into(source: &Path, dest: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
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
    let flushed = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temporary)
        .and_then(|handle| handle.sync_all());
    if let Err(error) = flushed {
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
    })
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

/// Delete the temporary database and its sidecars (failure path).
fn remove_tmp_db(tmp_db: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", tmp_db.display()));
    }
}

/// Delete every snapshot this run added (failure path — the target keeps
/// exactly its previous contents).
fn remove_added_snapshots(added: &[PathBuf]) {
    for path in added {
        let _ = fs::remove_file(path);
    }
}

/// Remove a target's stale `-wal`/`-shm` sidecars so they can never bleed
/// into the freshly renamed file.
fn remove_stale_sidecars(db: &Path) {
    let _ = fs::remove_file(format!("{}-wal", db.display()));
    let _ = fs::remove_file(format!("{}-shm", db.display()));
}

/// Atomically rename the temporary database into place, then fsync the
/// directory so the rename is durable (the store's temp→fsync→rename
/// discipline).
fn install_tmp_db(tmp_db: &Path, target: &Path) -> anyhow::Result<()> {
    remove_stale_sidecars(target);
    fs::rename(tmp_db, target)
        .map_err(|e| anyhow!("install {} -> {}: {e}", tmp_db.display(), target.display()))?;
    if let Some(dir) = target.parent()
        && let Ok(handle) = fs::File::open(dir)
    {
        let _ = handle.sync_all();
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
    use super::copy_snapshot_into;
    use std::fs;
    use std::path::{Path, PathBuf};

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

    /// Fix 2: a copy publishes through a temporary sibling, so a failure can
    /// never leave a truncated file under a final content-addressed name — and
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
}
