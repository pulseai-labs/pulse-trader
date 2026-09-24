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

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use clap::Args;

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
    /// Back up the non-empty target first, then replace it.
    pub replace: bool,
    /// Set the source db a-w after a successful copy (import only).
    pub chmod_source: bool,
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

/// How the steps ended: a counted summary, a hard failure, or mismatches.
enum StepFailure {
    /// An I/O or database failure (not a verification mismatch).
    Fatal(anyhow::Error),
    /// The verification found mismatches; the target must stay untouched.
    Mismatches {
        mismatches: Vec<Mismatch>,
        added: Added,
    },
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

    // ---- Step 1: refuse a non-empty target; back it up first on --replace.
    if let Some((strategies, versions, runs)) = ops::target_row_counts(job.db_target)
        .await
        .map_err(|e| anyhow!("{e}"))?
    {
        if !job.replace {
            anyhow::bail!(
                "{label}: target {} is non-empty (strategies {strategies}, versions {versions}, \
                 runs {runs}); without --replace a non-empty target is refused",
                job.db_target.display()
            );
        }
        let out_dir = default_backup_out_dir()?;
        let backup_path = super::backup::backup_database(job.db_target, &out_dir).await?;
        println!(
            "{label}: previous target backed up to {}",
            backup_path.display()
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
    let steps = run_steps(&job, &opened).await;
    drop(opened); // close the pool before any file surgery on either path

    match steps {
        Err(StepFailure::Fatal(e)) => {
            remove_tmp_db(&tmp_db);
            Err(e)
        }
        Err(StepFailure::Mismatches { mismatches, added }) => {
            remove_tmp_db(&tmp_db);
            remove_added_snapshots(&added);
            print_refusal(label, &mismatches);
            anyhow::bail!(
                "{label}: refused — the temporary database and every snapshot this run added \
                 were deleted and the target is exactly as it was"
            );
        }
        Ok(summary) => {
            // ---- Step 7: atomic install. The target's stale -wal/-shm are
            // removed first so an old sidecar can never bleed into the
            // renamed file (the migration-protocol restore precedent).
            install_tmp_db(&tmp_db, job.db_target)?;
            if job.chmod_source {
                set_read_only(job.from_db)?;
                println!("{label}: set read-only (a-w): {}", job.from_db.display());
            }
            print_summary(label, &job, &summary);
            Ok(())
        }
    }
}

/// Steps 4–5 on the migrated copy: the snapshot copy and the five checks.
async fn run_steps(job: &VerifiedCopy<'_>, copied: &Db) -> Result<Summary, StepFailure> {
    let copy_pool = copied.pool();
    let mut mismatches: Vec<Mismatch> = Vec::new();

    // ---- Step 4: copy every source snapshot; an existing same-named file
    // must be byte-identical (snapshots are immutable and content-addressed).
    let target_store = CandleStore::with_base_dir(job.data_target.to_path_buf());
    let (source_snapshots, scan_issues) = scan_snapshots(job.from_data_dir);
    mismatches.extend(scan_issues);
    let added = copy_snapshots(&target_store, &source_snapshots, &mut mismatches)
        .map_err(StepFailure::Fatal)?;

    // ---- Step 5: verify everything, and do not stop at the first failure
    // within a check.
    let source = ops::open_read_only(job.from_db)
        .await
        .map_err(|e| StepFailure::Fatal(anyhow!("{e}")))?;
    let tables = step_table_counts(&source, copy_pool, &mut mismatches)
        .await
        .map_err(StepFailure::Fatal)?;
    step_stored_hashes(&source, copy_pool, &mut mismatches)
        .await
        .map_err(StepFailure::Fatal)?;
    let (versions_verified, runs_verified) = step_repository_reads(copy_pool, &mut mismatches)
        .await
        .map_err(StepFailure::Fatal)?;
    let snapshots_verified = step_snapshot_reads(&target_store, &source_snapshots, &mut mismatches);
    step_referenced_snapshots(&target_store, copy_pool, &mut mismatches)
        .await
        .map_err(StepFailure::Fatal)?;
    source.close().await;

    if !mismatches.is_empty() {
        return Err(StepFailure::Mismatches { mismatches, added });
    }
    let mut table_counts = Vec::with_capacity(tables.len());
    for table in &tables {
        let count = ops::table_count(copy_pool, table)
            .await
            .map_err(|e| StepFailure::Fatal(anyhow!("{e}")))?;
        table_counts.push((table.clone(), count));
    }
    Ok(Summary {
        table_counts,
        versions_verified,
        runs_verified,
        snapshots_verified,
    })
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
pub(crate) fn scan_snapshots(data_dir: &Path) -> (Vec<SourceSnapshot>, Vec<Mismatch>) {
    let mut out = Vec::new();
    let mut issues = Vec::new();
    let candles = data_dir.join("candles");
    let Ok(pair_entries) = fs::read_dir(&candles) else {
        issues.push(Mismatch::new(
            "snapshot",
            candles.display().to_string(),
            "layout",
            "the source data dir holds no candles/ directory",
        ));
        return (out, issues);
    };
    for pair_entry in pair_entries.flatten() {
        if !pair_entry.path().is_dir() {
            continue;
        }
        let pair_name = pair_entry.file_name().to_string_lossy().to_string();
        let Ok(pair) = Pair::parse(&pair_name) else {
            issues.push(Mismatch::new(
                "snapshot",
                pair_name,
                "layout",
                "not a valid pair directory",
            ));
            continue;
        };
        let Ok(tf_entries) = fs::read_dir(pair_entry.path()) else {
            issues.push(Mismatch::new(
                "snapshot",
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
                    "snapshot",
                    tf_name,
                    "layout",
                    "not a known timeframe directory (15m / 4h)",
                ));
                continue;
            };
            let Ok(files) = fs::read_dir(tf_entry.path()) else {
                issues.push(Mismatch::new(
                    "snapshot",
                    tf_name,
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

/// The timeframe directory name (`15m` / `4h`) a snapshot lives under.
fn timeframe_from_interval(name: &str) -> Option<Timeframe> {
    match name {
        "15m" => Some(Timeframe::M15),
        "4h" => Some(Timeframe::H4),
        _ => None,
    }
}

/// Copy every source snapshot into the target store; an existing same-named
/// file must be byte-identical, or the mismatch refuses the import. Returns
/// the files this run added (for the failure cleanup).
fn copy_snapshots(
    target_store: &CandleStore,
    source_snapshots: &[SourceSnapshot],
    mismatches: &mut Vec<Mismatch>,
) -> Result<Added, anyhow::Error> {
    let mut added: Added = Vec::new();
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
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| anyhow!("create snapshot directory {}: {e}", parent.display()))?;
        }
        fs::copy(&snap.path, &dest).map_err(|e| {
            anyhow!(
                "copy snapshot {} -> {}: {e}",
                snap.path.display(),
                dest.display()
            )
        })?;
        added.push(dest);
    }
    Ok(added)
}

/// Byte comparison of two files (a read failure counts as "not equal").
fn bytes_equal(a: &Path, b: &Path) -> bool {
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
