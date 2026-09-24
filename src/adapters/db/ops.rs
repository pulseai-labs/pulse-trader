//! r3.s3.w4 (D7/D12, ADR-0026): the small copy/verify helper set the data-ops
//! verbs (`pulse import` / `pulse backup` / `pulse restore`) compose, beside
//! the repositories.
//!
//! The read-only opener, the `VACUUM INTO` consistent-copy primitive, table
//! enumeration + counts, the two stored-hash row reads, run/version id lists,
//! the referenced-snapshot projection and the target emptiness probe. Raw
//! `sqlx::query`/`query_scalar` ONLY — no `query!` macros — so the committed
//! `.sqlx` offline cache is untouched (sqlx-cli is not a pre-flight tool).
//!
//! The sources these helpers read are never written: [`open_read_only`]
//! refuses to create files and [`vacuum_into_copy`] writes only its new
//! target file.

use std::path::Path;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqlitePoolOptions;

use crate::domain::DataError;

/// The busy-timeout every pooled connection inherits (gate-7 C5, mirroring
/// [`Db`](super::Db)'s contract).
const BUSY_TIMEOUT_SECS: u64 = 5;

/// Open an existing SQLite file READ-ONLY (the import/backup source open).
///
/// `create_if_missing` is deliberately NOT set: a missing source is a named
/// error, not a silently-created empty database. The source is never written.
///
/// # Errors
///
/// Returns [`DataError::Db`] when the pool cannot be opened (missing file,
/// corrupt header, unreadable directory).
pub async fn open_read_only(path: &Path) -> Result<SqlitePool, DataError> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .busy_timeout(Duration::from_secs(BUSY_TIMEOUT_SECS));
    SqlitePoolOptions::new()
        .connect_with(opts)
        .await
        .map_err(|e| DataError::Db(format!("open {} read-only: {e}", path.display())))
}

/// A consistent, WAL-safe copy of `source` into `target` via `VACUUM INTO`
/// (the migration protocol's backup primitive — self-contained here so `ops`
/// stays beside the repositories it serves). The source is only read; the
/// target file must not already exist.
///
/// # Errors
///
/// Returns [`DataError::Db`] when the statement fails (existing target,
/// unreadable source, unwritable destination directory).
pub async fn vacuum_into_copy(source: &SqlitePool, target: &Path) -> Result<(), DataError> {
    // `VACUUM INTO` takes a string literal, not a bound parameter; the path is
    // a process-local, caller-chosen name (not user input). Escape single
    // quotes defensively so a quote in a temp dir can't break the statement.
    let target_str = target.to_string_lossy().replace('\'', "''");
    sqlx::query(&format!("VACUUM INTO '{target_str}'"))
        .execute(source)
        .await
        .map_err(|e| DataError::Db(format!("VACUUM INTO {}: {e}", target.display())))?;
    Ok(())
}

/// Every table the database holds — the import's per-table count check walks
/// THIS list, never a hand-picked one. `sqlite_*` internals are excluded;
/// `_sqlx_migrations` stays in (the count check skips it by name, because a
/// copy migrated forward necessarily holds more migration rows).
///
/// # Errors
///
/// Returns [`DataError::Db`] when the catalog read fails.
pub async fn table_names(pool: &SqlitePool) -> Result<Vec<String>, DataError> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite\\_%' \
         ESCAPE '\\' ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| DataError::Db(format!("read sqlite_master tables: {e}")))?;
    Ok(rows)
}

/// The row count of one table (its name comes from [`table_names`] of the
/// same file, and is still quote-escaped before splicing).
///
/// # Errors
///
/// Returns [`DataError::Db`] when the count fails.
pub async fn table_count(pool: &SqlitePool, table: &str) -> Result<i64, DataError> {
    let escaped = table.replace('"', "\"\"");
    let sql = format!("SELECT COUNT(*) FROM \"{escaped}\"");
    sqlx::query_scalar(&sql)
        .fetch_one(pool)
        .await
        .map_err(|e| DataError::Db(format!("count {table}: {e}")))
}

/// Every `strategy_version.version_hash`, keyed by id (the stored-hash check).
///
/// # Errors
///
/// Returns [`DataError::Db`] when the read fails.
pub async fn stored_version_hashes(pool: &SqlitePool) -> Result<Vec<(String, String)>, DataError> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, version_hash FROM strategy_version ORDER BY id")
            .fetch_all(pool)
            .await
            .map_err(|e| DataError::Db(format!("read stored version hashes: {e}")))?;
    Ok(rows)
}

/// Every `backtest_run.result_content_hash`, keyed by id (the stored-hash
/// check).
///
/// # Errors
///
/// Returns [`DataError::Db`] when the read fails.
pub async fn stored_run_hashes(pool: &SqlitePool) -> Result<Vec<(String, String)>, DataError> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, result_content_hash FROM backtest_run ORDER BY id")
            .fetch_all(pool)
            .await
            .map_err(|e| DataError::Db(format!("read stored run hashes: {e}")))?;
    Ok(rows)
}

/// Every `strategy_version` id (the repository-read check iterates these).
///
/// # Errors
///
/// Returns [`DataError::Db`] when the read fails.
pub async fn all_version_ids(pool: &SqlitePool) -> Result<Vec<String>, DataError> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT id FROM strategy_version ORDER BY id")
        .fetch_all(pool)
        .await
        .map_err(|e| DataError::Db(format!("read version ids: {e}")))?;
    Ok(rows)
}

/// Every `backtest_run` id (the repository-read check iterates these).
///
/// # Errors
///
/// Returns [`DataError::Db`] when the read fails.
pub async fn all_run_ids(pool: &SqlitePool) -> Result<Vec<String>, DataError> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT id FROM backtest_run ORDER BY id")
        .fetch_all(pool)
        .await
        .map_err(|e| DataError::Db(format!("read run ids: {e}")))?;
    Ok(rows)
}

/// One `(pair, timeframe, data_version)` snapshot reference a run carries —
/// the D7 check (e) walks these and demands a verified snapshot in the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRef {
    /// The run's canonical pair string.
    pub pair: String,
    /// The referenced snapshot's timeframe interval (`15m` / `4h`).
    pub timeframe: String,
    /// The referenced snapshot's content-hash identity.
    pub data_version: String,
}

/// The distinct snapshot references across every run: the primary snapshot of
/// each run, plus its HTF snapshot where present. Pre-`0006` rows (no stored
/// provenance) carry no reference and are skipped.
///
/// # Errors
///
/// Returns [`DataError::Db`] when the read fails.
pub async fn referenced_snapshots(pool: &SqlitePool) -> Result<Vec<SnapshotRef>, DataError> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT DISTINCT pair, primary_timeframe, primary_data_version FROM backtest_run \
         WHERE pair IS NOT NULL AND primary_timeframe IS NOT NULL \
           AND primary_data_version IS NOT NULL \
         UNION \
         SELECT DISTINCT pair, htf_timeframe, htf_data_version FROM backtest_run \
         WHERE pair IS NOT NULL AND htf_timeframe IS NOT NULL \
           AND htf_data_version IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| DataError::Db(format!("read referenced snapshots: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|(pair, timeframe, data_version)| SnapshotRef {
            pair,
            timeframe,
            data_version,
        })
        .collect())
}

/// The `(strategies, versions, runs)` triple of an existing target — or `None`
/// when the file does not exist, or exists and holds none of the three (the
/// "empty target" an import may proceed against; D7).
///
/// # Errors
///
/// Returns [`DataError::Db`] when the file exists but its counts cannot be
/// read (a corrupt or foreign file is a named error, never a silent empty).
pub async fn target_row_counts(path: &Path) -> Result<Option<(i64, i64, i64)>, DataError> {
    if !path.exists() {
        return Ok(None);
    }
    let pool = open_read_only(path).await?;
    let counts: Result<(i64, i64, i64), sqlx::Error> = async {
        let strategies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy")
            .fetch_one(&pool)
            .await?;
        let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
            .fetch_one(&pool)
            .await?;
        let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backtest_run")
            .fetch_one(&pool)
            .await?;
        Ok((strategies, versions, runs))
    }
    .await;
    pool.close().await;
    match counts {
        Ok((0, 0, 0)) => Ok(None),
        Ok(counts) => Ok(Some(counts)),
        Err(e) => Err(DataError::Db(format!(
            "read target counts from {}: {e}",
            path.display()
        ))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{open_read_only, table_count, table_names, target_row_counts, vacuum_into_copy};
    use crate::adapters::db::open_migrated;
    use std::path::Path;
    use tempfile::TempDir;

    #[tokio::test]
    async fn read_only_open_refuses_writes_and_missing_files() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("pulse.db");
        let db = open_migrated(&db_path).await.unwrap();
        drop(db);

        let ro = open_read_only(&db_path).await.unwrap();
        let write =
            sqlx::query("INSERT INTO strategy (id, name, created_at) VALUES ('x', 'x', 'x')")
                .execute(&ro)
                .await;
        assert!(
            write.is_err(),
            "a read-only open must refuse writes (the source is never written)"
        );
        ro.close().await;

        let missing = open_read_only(&tmp.path().join("nope.db")).await;
        assert!(
            missing.is_err(),
            "a missing source must be a named error, not a created file"
        );
        assert!(!tmp.path().join("nope.db").exists(), "no file was created");
    }

    #[tokio::test]
    async fn vacuum_copy_matches_source_table_counts() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("pulse.db");
        let copy_path = tmp.path().join("copy.db");
        let db = open_migrated(&db_path).await.unwrap();
        sqlx::query("INSERT INTO strategy (id, name, created_at) VALUES ('s1', 'S', 'x')")
            .execute(db.pool())
            .await
            .unwrap();
        vacuum_into_copy(db.pool(), &copy_path).await.unwrap();
        drop(db);

        let source = open_read_only(&db_path).await.unwrap();
        let copy = open_read_only(&copy_path).await.unwrap();
        for table in table_names(&source).await.unwrap() {
            let a = table_count(&source, &table).await.unwrap();
            let b = table_count(&copy, &table).await.unwrap();
            assert_eq!(a, b, "table {table}: the copy matches the source");
        }
    }

    #[tokio::test]
    async fn target_row_counts_distinguishes_absent_empty_and_seeded() {
        let tmp = TempDir::new().unwrap();
        let absent = target_row_counts(&tmp.path().join("nope.db"))
            .await
            .unwrap();
        assert_eq!(absent, None, "a missing target is the empty case");

        let empty_path = tmp.path().join("empty.db");
        let db = open_migrated(&empty_path).await.unwrap();
        drop(db);
        let empty = target_row_counts(&empty_path).await.unwrap();
        assert_eq!(
            empty, None,
            "a migrated but unseeded target is the empty case"
        );

        let seeded_path = tmp.path().join("seeded.db");
        let db = open_migrated(&seeded_path).await.unwrap();
        sqlx::query("INSERT INTO strategy (id, name, created_at) VALUES ('s1', 'S', 'x')")
            .execute(db.pool())
            .await
            .unwrap();
        drop(db);
        let seeded = target_row_counts(Path::new(&seeded_path)).await.unwrap();
        assert_eq!(seeded, Some((1, 0, 0)), "a seeded target is non-empty");
    }
}
