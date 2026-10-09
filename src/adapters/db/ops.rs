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

use sha2::{Digest, Sha256};
use sqlx::Row;
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
    /// The referenced snapshot's timeframe interval (`15m` / `4h` / `1d`).
    pub timeframe: String,
    /// The referenced snapshot's content-hash identity.
    pub data_version: String,
}

/// The distinct snapshot references across every run: the primary snapshot of
/// each run, plus its HTF snapshot where present, plus its daily-series snapshot
/// where present (r3.s2.w4 — a run's `d1` operands read real candles, so D7
/// check (e) must demand that snapshot in the target too). Pre-`0006` rows (no
/// stored provenance) carry no reference and are skipped.
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
           AND htf_data_version IS NOT NULL \
         UNION \
         SELECT DISTINCT pair, '1d', d1_data_version FROM backtest_run \
         WHERE pair IS NOT NULL AND d1_data_version IS NOT NULL",
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

/// What an existing target holds (the D7 emptiness probe).
///
/// The three tables the refusal message has always named, plus every OTHER
/// application table that holds rows — `client_token`, `token_audit`,
/// `llm_call`, the coaching tables. The install replaces the whole FILE, so a
/// row anywhere is data an import must not silently destroy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetContents {
    /// `strategy` rows.
    pub strategies: i64,
    /// `strategy_version` rows.
    pub versions: i64,
    /// `backtest_run` rows.
    pub runs: i64,
    /// `(table, rows)` for every other application table holding rows, in
    /// `table_names` order.
    pub other: Vec<(String, i64)>,
}

impl TargetContents {
    /// Nothing to lose: the ONLY state an import may replace without
    /// `--replace` and without a backup. EVERY application table must be empty
    /// — [`table_names`] already excludes the `sqlite_%` internals, and
    /// `_sqlx_migrations` is excluded here, because a migrated-but-unseeded
    /// target is the D7 fresh-install case and its migration rows say nothing
    /// about the operator's data.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.strategies == 0 && self.versions == 0 && self.runs == 0 && self.other.is_empty()
    }

    /// The refusal message's parenthetical: the named triple, then every other
    /// non-empty table, so the operator sees what is actually there.
    #[must_use]
    pub fn named_counts(&self) -> String {
        let mut parts = vec![
            format!("strategies {}", self.strategies),
            format!("versions {}", self.versions),
            format!("runs {}", self.runs),
        ];
        for (table, rows) in &self.other {
            parts.push(format!("{table} {rows}"));
        }
        parts.join(", ")
    }
}

/// What an existing target holds — or `None` when the file does not exist, or
/// exists with EVERY application table empty (the "empty target" an import may
/// proceed against; D7).
///
/// The probe walks every application table ([`table_names`] minus
/// `_sqlx_migrations`), not only the three the message names: `install_tmp_db`
/// replaces the whole file, so a target holding nothing but `client_token` /
/// `token_audit` / `llm_call` / coaching rows is NOT empty, and replacing it
/// without `--replace` and without a backup would take the operator's tokens
/// and audit trail with it.
///
/// # Errors
///
/// Returns [`DataError::Db`] when the file exists but its counts cannot be
/// read (a corrupt or foreign file is a named error, never a silent empty).
pub async fn target_row_counts(path: &Path) -> Result<Option<TargetContents>, DataError> {
    if !path.exists() {
        return Ok(None);
    }
    let pool = open_read_only(path).await?;
    let scanned = read_target_contents(&pool).await;
    pool.close().await;
    match scanned {
        Ok(contents) if contents.is_empty() => Ok(None),
        Ok(contents) => Ok(Some(contents)),
        Err(e) => Err(DataError::Db(format!(
            "read target counts from {}: {e}",
            path.display()
        ))),
    }
}

/// The per-table walk behind [`target_row_counts`]: every application table,
/// counted by name.
async fn read_target_contents(pool: &SqlitePool) -> Result<TargetContents, DataError> {
    let mut contents = TargetContents {
        strategies: 0,
        versions: 0,
        runs: 0,
        other: Vec::new(),
    };
    for table in table_names(pool).await? {
        if table == "_sqlx_migrations" {
            continue; // the migration bookkeeping, never the operator's data
        }
        let rows = table_count(pool, &table).await?;
        match table.as_str() {
            "strategy" => contents.strategies = rows,
            "strategy_version" => contents.versions = rows,
            "backtest_run" => contents.runs = rows,
            _ if rows > 0 => contents.other.push((table, rows)),
            _ => {}
        }
    }
    Ok(contents)
}

// ---------------------------------------------------------------------------
// r4.s2.w2: the paper tables' content digests (the move made safe)
// ---------------------------------------------------------------------------

/// The three paper tables the import verifies by content digest, each with the
/// key its rows are read in: `paper_session`'s primary key, and the UNIQUE keys
/// `paper_event` (`session_id`, `seq`) and `paper_bar` (`session_id`,
/// `timeframe`, `open_time`) carry in its place (migration `0018`). This order
/// is the order the summary prints and the refusal walks.
pub const PAPER_TABLES: [(&str, &[&str]); 3] = [
    ("paper_session", &["id"]),
    ("paper_event", &["session_id", "seq"]),
    ("paper_bar", &["session_id", "timeframe", "open_time"]),
];

/// The key columns of one paper table, or `None` for any other name.
#[must_use]
pub fn paper_table_keys(table: &str) -> Option<&'static [&'static str]> {
    PAPER_TABLES
        .iter()
        .find(|(name, _)| *name == table)
        .map(|(_, keys)| *keys)
}

/// One paper table's digest comparison between a source and its copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaperTableDigest {
    /// The table.
    pub table: String,
    /// The source's row count — and, for a table the source does not have at
    /// all (PR-354 fix C5), the COPY's, so the refusal names the rows it found.
    pub rows: i64,
    /// SHA-256 (hex) over every source row's columns, in key order.
    pub source_digest: String,
    /// SHA-256 (hex) over every copy row's columns, in key order.
    pub target_digest: String,
    /// The first key whose row differs, when one does — the refusal's `id`.
    pub first_difference: Option<String>,
}

/// Compare one paper table between `source` and `target` by content digest:
/// SHA-256 over every row's columns in key order, each value length-prefixed
/// (`u64` little-endian length, then the UTF-8 bytes; a NULL is the `u64::MAX`
/// length marker, a length no value can have). The columns come from the
/// SOURCE's schema (`PRAGMA table_info`, in declared order) and the column
/// NAMES are folded in too, so a source whose rows moved exactly agrees with
/// its copy — while a changed column value still changes the digest, a
/// difference the refusal names on the first key like any other.
///
/// The SOURCE decides the shape (PR-354 fix C5):
///
/// - a paper table the source does NOT have (a database older than migration
///   `0018`) counts as EMPTY, and the migrated copy must then hold zero rows in
///   it — any row is a real difference, named by its first key;
/// - where the table IS there, the copy is read with the SOURCE's column list,
///   so a column this build's migrations ADD to the copy cannot change the
///   digest (the digest compares the same columns on both sides).
///
/// This is the check behind "the paper state moved exactly" (r4.s2.w2, spec
/// §Approach 3): row counts alone cannot see a changed column, and this can.
///
/// # Errors
///
/// Returns [`DataError::Db`] when either side's schema or rows cannot be read,
/// when the copy lacks a column the source has, or when `table` is not one of
/// [`PAPER_TABLES`].
pub async fn paper_table_digest(
    source: &SqlitePool,
    target: &SqlitePool,
    table: &str,
) -> Result<PaperTableDigest, DataError> {
    let keys = paper_table_keys(table)
        .ok_or_else(|| DataError::Db(format!("{table} is not a paper table")))?;
    let Some(source_columns) = paper_columns(source, table).await? else {
        return absent_from_source(target, table, keys).await;
    };
    let source_rows = paper_rows(source, table, &source_columns, keys).await?;
    let target_rows = paper_rows(target, table, &source_columns, keys).await?;
    let first_difference = first_difference(&source_columns, keys, &source_rows, &target_rows);
    Ok(PaperTableDigest {
        table: table.to_owned(),
        rows: i64::try_from(source_rows.len()).unwrap_or(i64::MAX),
        source_digest: hex::encode(sha256_of(&source_columns, &source_rows)),
        target_digest: hex::encode(sha256_of(&source_columns, &target_rows)),
        first_difference,
    })
}

/// The digest of a paper table the SOURCE does not have (PR-354 fix C5): a
/// database older than migration `0018` carries no paper table at all, so its
/// side is EMPTY — and the migrated copy, which always has the table, must
/// hold ZERO rows in it. A row is a real content difference and is named by its
/// first key, like any other; an empty copy compares equal (both digests are
/// the empty input's).
async fn absent_from_source(
    target: &SqlitePool,
    table: &str,
    keys: &[&str],
) -> Result<PaperTableDigest, DataError> {
    let empty = hex::encode(sha256_of(&[], &[]));
    let (rows, target_digest, first_difference) = match paper_columns(target, table).await? {
        None => (0, empty.clone(), None),
        Some(target_columns) => {
            let target_rows = paper_rows(target, table, &target_columns, keys).await?;
            if target_rows.is_empty() {
                (0, empty.clone(), None)
            } else {
                let first = target_rows
                    .first()
                    .map(|row| key_of(&target_columns, keys, row));
                (
                    target_rows.len(),
                    hex::encode(sha256_of(&target_columns, &target_rows)),
                    first,
                )
            }
        }
    };
    Ok(PaperTableDigest {
        table: table.to_owned(),
        rows: i64::try_from(rows).unwrap_or(i64::MAX),
        source_digest: empty,
        target_digest,
        first_difference,
    })
}

/// One table's column names, in schema order — `None` when the table is not
/// there at all (the absent-from-source case PR-354 fix C5 covers).
async fn paper_columns(pool: &SqlitePool, table: &str) -> Result<Option<Vec<String>>, DataError> {
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
            .bind(table)
            .fetch_all(pool)
            .await
            .map_err(|e| DataError::Db(format!("read {table}'s columns: {e}")))?;
    Ok((!columns.is_empty()).then_some(columns))
}

/// Every row of one table, in key order, each column read as its text
/// rendering (`NULL` stays `None`) — the digest's input.
///
/// Every column is `CAST(… AS TEXT)` in the query: SQLite's own rendering is
/// the canonical form (an INTEGER `seq` reads as its decimal digits), and it is
/// what lets a typed column be read as one `Option<String>` whatever its
/// declared type — the alternative (one decode per declared type) would encode
/// the same values differently per column type for no gain.
async fn paper_rows(
    pool: &SqlitePool,
    table: &str,
    columns: &[String],
    keys: &[&str],
) -> Result<Vec<Vec<Option<String>>>, DataError> {
    let column_list = columns
        .iter()
        .map(|column| format!("CAST({} AS TEXT)", quote_ident(column)))
        .collect::<Vec<_>>()
        .join(", ");
    let key_list = keys
        .iter()
        .map(|key| quote_ident(key))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {column_list} FROM {} ORDER BY {key_list}",
        quote_ident(table)
    );
    let rows = sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .map_err(|e| DataError::Db(format!("read {table} rows for the digest: {e}")))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut values = Vec::with_capacity(columns.len());
        for (index, column) in columns.iter().enumerate() {
            let value: Option<String> = row
                .try_get(index)
                .map_err(|e| DataError::Db(format!("read {table}.{column} for the digest: {e}")))?;
            values.push(value);
        }
        out.push(values);
    }
    Ok(out)
}

/// A SQLite identifier quoted for a statement this module builds. Every name
/// comes from the schema or the fixed [`PAPER_TABLES`] table, never from input.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// One table's digest: the column names, then every row in key order, each row
/// encoded as below.
fn sha256_of(columns: &[String], rows: &[Vec<Option<String>>]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(
        u64::try_from(columns.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    for column in columns {
        hasher.update(
            u64::try_from(column.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        hasher.update(column.as_bytes());
    }
    for row in rows {
        hasher.update(encode_row(row));
    }
    hasher.finalize().to_vec()
}

/// One row's canonical encoding: every column length-prefixed, in order. A
/// NULL is `u64::MAX` — a length no value can have, so a NULL can never be
/// confused with an empty string.
fn encode_row(values: &[Option<String>]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in values {
        match value {
            Some(text) => {
                let bytes = text.as_bytes();
                out.extend_from_slice(
                    &u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes(),
                );
                out.extend_from_slice(bytes);
            }
            None => out.extend_from_slice(&u64::MAX.to_le_bytes()),
        }
    }
    out
}

/// The first key whose row differs. Both sides are read in the same key order
/// AND with the same column list — the source's (PR-354 fix C5) — so the first
/// index whose encodings differ, or where only one side has a row, names it.
fn first_difference(
    columns: &[String],
    keys: &[&str],
    source_rows: &[Vec<Option<String>>],
    target_rows: &[Vec<Option<String>>],
) -> Option<String> {
    for index in 0..source_rows.len().max(target_rows.len()) {
        let left = source_rows.get(index);
        let right = target_rows.get(index);
        let equal = matches!(
            (left, right),
            (Some(left), Some(right)) if encode_row(left) == encode_row(right)
        );
        if !equal {
            return left.or(right).map(|row| key_of(columns, keys, row));
        }
    }
    None
}

/// One row's key, rendered as `column=value` pairs in the table's key order —
/// the refusal's `id`.
fn key_of(columns: &[String], keys: &[&str], row: &[Option<String>]) -> String {
    let mut parts = Vec::with_capacity(keys.len());
    for key in keys {
        let value = columns
            .iter()
            .position(|column| column.as_str() == *key)
            .and_then(|index| row.get(index).cloned().flatten())
            .unwrap_or_else(|| "(absent)".to_owned());
        parts.push(format!("{key}={value}"));
    }
    parts.join(" ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        PAPER_TABLES, open_read_only, paper_table_digest, paper_table_keys, read_target_contents,
        table_count, table_names, target_row_counts, vacuum_into_copy,
    };
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
        let seeded = target_row_counts(Path::new(&seeded_path))
            .await
            .unwrap()
            .expect("a seeded target is non-empty");
        assert_eq!(
            (seeded.strategies, seeded.versions, seeded.runs),
            (1, 0, 0),
            "the named triple is reported: {seeded:?}"
        );
        assert!(seeded.other.is_empty(), "the other tables are empty");
        assert_eq!(seeded.named_counts(), "strategies 1, versions 0, runs 0");
    }

    /// Emptiness counts EVERY application table: a target holding nothing but
    /// token/audit rows is not empty, because the install replaces the whole
    /// file (and the migration bookkeeping alone never makes one non-empty).
    #[tokio::test]
    async fn target_row_counts_counts_every_application_table() {
        let tmp = TempDir::new().unwrap();

        // A migrated target: only `_sqlx_migrations` holds rows — empty.
        let fresh_path = tmp.path().join("fresh.db");
        let db = open_migrated(&fresh_path).await.unwrap();
        let migrations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert!(migrations > 0, "a migrated database holds migration rows");
        assert_eq!(
            target_row_counts(&fresh_path).await.unwrap(),
            None,
            "migration bookkeeping is not the operator's data — a fresh \
             migrated target is still the empty case"
        );
        drop(db);

        // Every OTHER application table counts on its own: one row is enough.
        let token_insert = format!(
            "INSERT INTO client_token \
               (id, label, scope, token_sha256, created_at, created_by, schema_version) \
             VALUES ('t1', 'laptop', 'agent', '{}', '2026-01-01T00:00:00Z', \
                     'cli:token-issue', '1')",
            "0".repeat(64)
        );
        for (table, insert) in [
            ("client_token", token_insert.as_str()),
            (
                "token_audit",
                "INSERT INTO token_audit \
                   (id, at, event, token_id, label, reason, route, peer, schema_version) \
                 VALUES ('a1', '2026-01-01T00:00:00Z', 'refused', NULL, NULL, 'unknown', \
                         'GET /api/v1/handshake', '100.64.0.1', '1')",
            ),
            (
                "llm_call",
                "INSERT INTO llm_call \
                   (id, backend, model, prompt_messages, completion, input_tokens, \
                    output_tokens, cost, cost_currency, created_at, created_by, schema_version) \
                 VALUES ('c1', 'glm', 'glm-5.1', '[]', NULL, 1, 1, '0', 'CNY', \
                         '2026-01-01T00:00:00Z', 'test', '1')",
            ),
        ] {
            let path = tmp.path().join(format!("{table}.db"));
            let db = open_migrated(&path).await.unwrap();
            sqlx::query(insert)
                .execute(db.pool())
                .await
                .unwrap_or_else(|error| panic!("seed one {table} row: {error}"));
            drop(db);
            let contents = target_row_counts(&path)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("a target holding {table} rows is NOT empty"));
            assert!(
                contents
                    .other
                    .iter()
                    .any(|(name, rows)| name == table && *rows == 1),
                "the table is named with its count: {contents:?}"
            );
            assert!(
                contents.named_counts().contains(table),
                "the refusal message names it: {}",
                contents.named_counts()
            );
        }
    }

    /// The walk itself: the named triple plus the other tables, and the
    /// `_sqlx_migrations` exclusion.
    #[tokio::test]
    async fn read_target_contents_reads_every_table_by_name() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mixed.db");
        let db = open_migrated(&path).await.unwrap();
        sqlx::query("INSERT INTO strategy (id, name, created_at) VALUES ('s1', 'S', 'x')")
            .execute(db.pool())
            .await
            .unwrap();
        let contents = read_target_contents(db.pool()).await.unwrap();
        assert_eq!(
            (contents.strategies, contents.versions, contents.runs),
            (1, 0, 0)
        );
        assert!(
            contents
                .other
                .iter()
                .all(|(table, _)| table != "_sqlx_migrations"),
            "the migration bookkeeping is excluded: {contents:?}"
        );
        assert!(!contents.is_empty());
    }

    /// r4.s2.w2: the paper digest compares every column — equal tables agree,
    /// one changed column names the table and the first differing key, and a
    /// row only one side has is named too.
    #[tokio::test]
    async fn paper_table_digest_names_the_first_differing_key() {
        let tmp = TempDir::new().unwrap();
        let source = open_migrated(&tmp.path().join("source.db")).await.unwrap();
        let target = open_migrated(&tmp.path().join("target.db")).await.unwrap();
        for pool in [source.pool(), target.pool()] {
            sqlx::query(
                "INSERT INTO strategy (id, name, created_at) \
                 VALUES ('st-1', 'digest', '2026-01-01T00:00:00.000Z')",
            )
            .execute(pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO strategy_version \
                 (id, strategy_id, dsl_schema_version, dsl, dsl_original, version_hash, \
                  created_by, created_at) \
                 VALUES ('ver-1', 'st-1', '1.0.0', '{}', '{}', 'h', 'human', \
                         '2026-01-01T00:00:00.000Z')",
            )
            .execute(pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO paper_session \
                 (id, seq, strategy_version_id, created_at, pair, primary_timeframe, \
                  htf_timeframe, uses_d1, starting_equity, taker_fee_bps, slippage_bps, \
                  engine_fingerprint, graduation, walk_forward_run_id, override_reason, \
                  override_at, certified_data_versions, fixture, min_trades, promoted_by) \
                 VALUES ('sess-1', 1, 'ver-1', '2026-01-01T00:00:00.000Z', 'BTCUSDT', '15m', NULL, \
                         0, '10000', '4', '1', 'fp', 'override', NULL, 'digest', \
                         '2026-01-01T00:00:00.000Z', '[]', 0, 1, 'operator-token')",
            )
            .execute(pool)
            .await
            .unwrap();
            for seq in [1_i64, 2] {
                sqlx::query(
                    "INSERT INTO paper_event (session_id, seq, at, kind, payload) \
                     VALUES ('sess-1', ?1, '2026-01-01T00:00:00.000Z', 'bar_processed', ?2)",
                )
                .bind(seq)
                .bind(format!("payload-{seq}"))
                .execute(pool)
                .await
                .unwrap();
            }
        }

        let digest = paper_table_digest(source.pool(), target.pool(), "paper_event")
            .await
            .unwrap();
        assert_eq!(digest.rows, 2);
        assert_eq!(digest.first_difference, None, "equal tables agree");
        assert_eq!(digest.source_digest, digest.target_digest);
        assert_eq!(digest.source_digest.len(), 64, "SHA-256, hex");

        // One changed column: the table and the exact key are named.
        sqlx::query("DROP TRIGGER paper_event_no_update")
            .execute(target.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE paper_event SET payload = 'changed' WHERE seq = 2")
            .execute(target.pool())
            .await
            .unwrap();
        let digest = paper_table_digest(source.pool(), target.pool(), "paper_event")
            .await
            .unwrap();
        assert_eq!(
            digest.first_difference.as_deref(),
            Some("session_id=sess-1 seq=2"),
            "the changed row's key is named"
        );
        assert_ne!(digest.source_digest, digest.target_digest);

        // A row only one side has: the first unmatched key is named.
        sqlx::query("DROP TRIGGER paper_event_no_delete")
            .execute(target.pool())
            .await
            .unwrap();
        sqlx::query("DELETE FROM paper_event WHERE seq = 1")
            .execute(target.pool())
            .await
            .unwrap();
        let digest = paper_table_digest(source.pool(), target.pool(), "paper_event")
            .await
            .unwrap();
        assert_eq!(
            digest.first_difference.as_deref(),
            Some("session_id=sess-1 seq=1"),
            "the missing row's key is named"
        );

        // Every paper table's keys are declared, and nothing else is a paper
        // table.
        for (table, keys) in PAPER_TABLES {
            assert_eq!(paper_table_keys(table), Some(keys), "{table}");
        }
        assert_eq!(paper_table_keys("backtest_run"), None);

        source.pool().close().await;
        target.pool().close().await;
    }

    /// The smallest paper fixture the digest reads: one strategy, one version,
    /// one session, and `events` `paper_event` rows.
    async fn seed_paper(pool: &sqlx::SqlitePool, events: i64) {
        sqlx::query(
            "INSERT INTO strategy (id, name, created_at) \
             VALUES ('st-1', 'digest', '2026-01-01T00:00:00.000Z')",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO strategy_version \
             (id, strategy_id, dsl_schema_version, dsl, dsl_original, version_hash, \
              created_by, created_at) \
             VALUES ('ver-1', 'st-1', '1.0.0', '{}', '{}', 'h', 'human', \
                     '2026-01-01T00:00:00.000Z')",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO paper_session \
             (id, seq, strategy_version_id, created_at, pair, primary_timeframe, \
              htf_timeframe, uses_d1, starting_equity, taker_fee_bps, slippage_bps, \
              engine_fingerprint, graduation, walk_forward_run_id, override_reason, \
              override_at, certified_data_versions, fixture, min_trades, promoted_by) \
             VALUES ('sess-1', 1, 'ver-1', '2026-01-01T00:00:00.000Z', 'BTCUSDT', '15m', NULL, \
                     0, '10000', '4', '1', 'fp', 'override', NULL, 'digest', \
                     '2026-01-01T00:00:00.000Z', '[]', 0, 1, 'operator-token')",
        )
        .execute(pool)
        .await
        .unwrap();
        for seq in 1..=events {
            sqlx::query(
                "INSERT INTO paper_event (session_id, seq, at, kind, payload) \
                 VALUES ('sess-1', ?1, '2026-01-01T00:00:00.000Z', 'bar_processed', ?2)",
            )
            .bind(seq)
            .bind(format!("payload-{seq}"))
            .execute(pool)
            .await
            .unwrap();
        }
    }

    /// PR-354 fix C5: the digest follows the SOURCE's shape. A copy whose schema
    /// drifted — a column this build's migrations ADD, or a REORDERED column
    /// list — must still agree on content, while a real content difference is
    /// still refused.
    #[tokio::test]
    async fn the_paper_digest_tolerates_a_drifted_copy_schema() {
        let tmp = TempDir::new().unwrap();
        let source = open_migrated(&tmp.path().join("source.db")).await.unwrap();
        let target = open_migrated(&tmp.path().join("target.db")).await.unwrap();
        seed_paper(source.pool(), 1).await;
        seed_paper(target.pool(), 1).await;

        // An ADDED column on the copy (a migration this build's target carries):
        // the digest reads the source's columns on both sides, so it agrees.
        sqlx::query("ALTER TABLE paper_event ADD COLUMN extra TEXT DEFAULT 'x'")
            .execute(target.pool())
            .await
            .unwrap();
        let digest = paper_table_digest(source.pool(), target.pool(), "paper_event")
            .await
            .unwrap();
        assert_eq!(
            digest.first_difference, None,
            "an added column does not change the digest"
        );
        assert_eq!(digest.source_digest, digest.target_digest);

        // A real content difference under that drifted schema is still refused.
        sqlx::query("DROP TRIGGER paper_event_no_update")
            .execute(target.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE paper_event SET payload = 'changed' WHERE seq = 1")
            .execute(target.pool())
            .await
            .unwrap();
        let digest = paper_table_digest(source.pool(), target.pool(), "paper_event")
            .await
            .unwrap();
        assert_eq!(
            digest.first_difference.as_deref(),
            Some("session_id=sess-1 seq=1"),
            "the changed row is still named"
        );

        // A REORDERED column list on the copy: rebuild the table with the same
        // columns in another order and the same rows. The old table is renamed
        // ASIDE (not dropped — its triggers are part of the schema), and the two
        // triggers whose BODIES name paper_event go before the replacement takes
        // the name: SQLite re-parses the schema on that rename and refuses while
        // a body still names a table that is not there. The digest reads a
        // table, not triggers.
        let reordered = open_migrated(&tmp.path().join("reordered.db"))
            .await
            .unwrap();
        seed_paper(reordered.pool(), 1).await;
        sqlx::query(
            "CREATE TABLE reordered_event ( \
               payload TEXT, kind TEXT, at TEXT, seq INTEGER, session_id TEXT)",
        )
        .execute(reordered.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO reordered_event (payload, kind, at, seq, session_id) \
             SELECT payload, kind, at, seq, session_id FROM paper_event",
        )
        .execute(reordered.pool())
        .await
        .unwrap();
        sqlx::query("ALTER TABLE paper_event RENAME TO original_event")
            .execute(reordered.pool())
            .await
            .unwrap();
        for trigger in [
            "paper_bar_no_insert_after_stop",
            "paper_event_no_insert_after_stop",
        ] {
            sqlx::query(&format!("DROP TRIGGER {trigger}"))
                .execute(reordered.pool())
                .await
                .unwrap();
        }
        sqlx::query("ALTER TABLE reordered_event RENAME TO paper_event")
            .execute(reordered.pool())
            .await
            .unwrap();
        let digest = paper_table_digest(source.pool(), reordered.pool(), "paper_event")
            .await
            .unwrap();
        assert_eq!(
            digest.first_difference, None,
            "a reordered column list does not change the digest"
        );
        assert_eq!(digest.source_digest, digest.target_digest);
        assert_eq!(digest.rows, 1);

        source.pool().close().await;
        target.pool().close().await;
        reordered.pool().close().await;
    }

    /// PR-354 fix C5: a source that predates the paper tables entirely counts as
    /// EMPTY, and the migrated copy must then hold zero rows — any row is a real
    /// difference, named by its first key.
    #[tokio::test]
    async fn a_paper_table_the_source_lacks_must_be_empty_in_the_copy() {
        let tmp = TempDir::new().unwrap();
        let source = open_migrated(&tmp.path().join("source.db")).await.unwrap();
        let copy = open_migrated(&tmp.path().join("copy.db")).await.unwrap();
        seed_paper(copy.pool(), 1).await;
        // The source predating migration 0018: no paper tables at all.
        // paper_bar first: its trigger's body names paper_event.
        for table in ["paper_bar", "paper_event", "paper_session"] {
            sqlx::query(&format!("DROP TABLE {table}"))
                .execute(source.pool())
                .await
                .unwrap();
        }

        let digest = paper_table_digest(source.pool(), copy.pool(), "paper_event")
            .await
            .unwrap();
        assert_eq!(
            digest.first_difference.as_deref(),
            Some("session_id=sess-1 seq=1"),
            "a row in a table the source does not have is refused by its key"
        );
        assert_ne!(digest.source_digest, digest.target_digest);

        // The same copy with the table emptied agrees: absent from the source
        // means "zero rows in the migrated copy". (The append-only trigger goes
        // first — this test is about the digest, not the trigger.)
        sqlx::query("DROP TRIGGER paper_event_no_delete")
            .execute(copy.pool())
            .await
            .unwrap();
        sqlx::query("DELETE FROM paper_event")
            .execute(copy.pool())
            .await
            .unwrap();
        let digest = paper_table_digest(source.pool(), copy.pool(), "paper_event")
            .await
            .unwrap();
        assert_eq!(digest.first_difference, None, "an empty copy agrees");
        assert_eq!(digest.rows, 0);
        assert_eq!(digest.source_digest, digest.target_digest);

        source.pool().close().await;
        copy.pool().close().await;
    }
}
