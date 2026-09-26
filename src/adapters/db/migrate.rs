//! The backup-before-migrate protocol (VS-1.1.4 work-1.04) — FR-4 / NFR-12.
//!
//! [`run_migrations_with_backup`] brings a `pulse.db` schema forward *safely*:
//! detect-behind → consistent-snapshot backup (`VACUUM INTO`) → run the embedded
//! [`MIGRATOR`](super::MIGRATOR) → verify → **restore + refuse to start** on any
//! failure. [`open_migrated`] is the single startup entry point (migrate-then-open)
//! 1.05's CLI wires; [`undo_to`] is the in-process down path the up/down round
//! test exercises.
//!
//! `VACUUM INTO` is `SQLite`'s first-class consistent-backup primitive — WAL-safe by
//! construction (no manual `wal_checkpoint`, no `-wal`/`-shm` sidecar copy, no torn
//! snapshot), the right tool because the DB is the system-of-record for real-money
//! trades. The restore path mirrors `store/mod.rs`'s atomic temp→rename + fsync
//! discipline (those helpers are private to a different module tree — mirrored, not
//! imported — per the audit-C5 re-derive convention).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use std::time::Duration;

use chrono::Utc;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{Connection, SqliteConnection, SqlitePool};

use super::{Db, MIGRATOR};
use crate::domain::DataError;

/// The outcome of a [`run_migrations_with_backup`] call (migration-protocol
/// vocabulary, not a domain type). A small owned result so callers/tests can
/// assert the from/to versions and the backup path without re-querying
/// `_sqlx_migrations`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// Every embedded migration is already applied; no backup taken, none run.
    AlreadyCurrent {
        /// The highest applied version (== the embedded max).
        version: i64,
    },
    /// Migrated `from` → `to`; the pre-migration db was backed up at `backup`.
    ///
    /// `from` and `to` are MAXIMA, and with reserved numbers landing out of order
    /// a run can legitimately report `from == to`: filling a reserved gap (adding
    /// `0005` to a db that already holds `0007`) applies a migration without moving
    /// the maximum. The `backup` is the reliable signal that a migration ran.
    Migrated {
        /// The highest applied version before the migration ran.
        from: i64,
        /// The highest applied version after the migration ran (the embedded max).
        to: i64,
        /// The retained pre-migration backup file (NFR-12).
        backup: PathBuf,
    },
}

/// Bring `db_path`'s schema forward safely: back up before migrating, verify
/// after, restore + refuse to start on ANY failure.
///
/// Detect-behind first (no backup is paid for when already current); on a real
/// migration the pre-migration db is snapshotted to
/// `pulse.db.bak-<from_version>-<timestamp>` (NFR-12) via `VACUUM INTO` before the
/// embedded [`MIGRATOR`](super::MIGRATOR) runs. A migrate error or post-verify
/// mismatch restores the original db from the backup and returns an error — the
/// caller MUST treat that as fatal (REFUSE TO START, MASTER-SPEC §7.4).
///
/// # Cross-process serialization (r2.s1)
/// `pulse mcp` (r2.s1.w2) is a second long-lived process that reaches this
/// protocol — it can start against a behind-schema db *while the desktop is
/// starting against the same one*. The whole body below therefore runs under a
/// `flock(2)` on `pulse.db.migrate.lock` (co-located with the db, so it is on
/// the same filesystem): the second migrator parks until the first releases,
/// then re-derives `applied` and reports `AlreadyCurrent`. Without it, two
/// concurrent migrators collide on the `VACUUM INTO` name probe, and a losing
/// migrator's restore-on-failure renames the pre-migration backup over a db the
/// winner already migrated — a torn schema under live pools. Unix-only; on
/// `not(unix)` [`acquire_migration_lock`] is a no-op (matching
/// `mcp/export.rs`'s `set_owner_only` posture — desktop parity is a separate
/// work item).
///
/// # Errors
/// Returns [`DataError::Migration`] if the backup, migrate, or post-verify step
/// fails (the original db is restored from the backup first; the backup is
/// retained for forensics), OR if the db is **ahead** of the embedded set — it has
/// applied a migration this binary does not ship, at ANY version, so a db newer than
/// the binary must NOT be migrated *or* opened; refuse to start (MASTER-SPEC §7.4),
/// OR if an applied migration's stored checksum no longer matches this binary's
/// embedded file (the db holds a different version of a migration it has "applied").
/// Returns [`DataError::Db`] if the pool cannot be opened.
pub async fn run_migrations_with_backup(db_path: &Path) -> Result<MigrationOutcome, DataError> {
    run_migrations_with_backup_using(db_path, &MIGRATOR).await
}

/// The protocol body, parameterised over the migration source so tests can inject
/// a deliberately-broken runtime [`Migrator`] (the forced-failure test) without
/// poisoning the committed embedded set. Production calls it with `&MIGRATOR`.
async fn run_migrations_with_backup_using(
    db_path: &Path,
    migrator: &Migrator,
) -> Result<MigrationOutcome, DataError> {
    run_migrations_with_backup_using_journal(db_path, migrator, SqliteJournalMode::Wal).await
}

/// [`run_migrations_with_backup_using`] with an explicit journal mode for the
/// pool it opens on `db_path`.
///
/// Issue #258: the import's temporary copy is opened through
/// [`open_migrated_copy`], which asks for rollback-journal mode — the file is
/// about to be RENAMED into place, and a `-wal` beside it cannot travel with the
/// rename.
///
/// The pool is CLOSED — never merely dropped — before this returns, on every
/// path. Dropping a pool is not a close: sqlx returns a dropped connection to
/// the pool from a SPAWNED task, and the connection's `SQLite` handle is released
/// by that connection's own worker thread, so a dropped pool can keep the
/// database file open after its owner has moved on. `SQLite` checkpoints and
/// runs its final checkpoint and normally removes `-wal`/`-shm` when the LAST
/// connection closes (it also checkpoints on its own at the WAL threshold), so
/// which close removes the sidecars — and whether it has happened yet — is
/// exactly the timing a caller that renames the file cannot tolerate (#258).
///
/// Closing is not a total guarantee either — a connection still in flight when
/// the close starts is closed by its own spawned return — so the import's copy
/// does not rely on close timing at all: it is opened with no WAL to strand
/// ([`open_migrated_copy`]), and the install refuses rather than rename while
/// any sidecar is still beside it.
async fn run_migrations_with_backup_using_journal(
    db_path: &Path,
    migrator: &Migrator,
    journal_mode: SqliteJournalMode,
) -> Result<MigrationOutcome, DataError> {
    // Serialize the whole protocol — detect → backup → migrate → verify →
    // restore-on-failure — across processes. The second migrator parks here
    // until the first drops its lock, then derives `applied` from the
    // FINISHED schema (a loser's mid-protocol read could otherwise observe a
    // restored file it must not trust).
    let _migration_lock = acquire_migration_lock(db_path).await?;

    let db = Db::with_path_journal(db_path, journal_mode).await?;
    let pool = db.pool();

    let outcome = migration_protocol(db_path, migrator, pool).await;
    // CLOSE, don't merely drop (#258) — see this function's doc.
    pool.close().await;
    outcome
}

/// The protocol body, on a pool the caller owns (and closes on every path).
async fn migration_protocol(
    db_path: &Path,
    migrator: &Migrator,
    pool: &SqlitePool,
) -> Result<MigrationOutcome, DataError> {
    // SETS, not maxima. This project allocates migration numbers at release planning
    // and ships them out of order — `r1.s1` shipped `0007` while `0005`/`0006` stayed
    // reserved for `r1.s2`/`r1.s3` — so "applied" and "current" are not the same
    // question. An installation holding `0001-0004` + `0007` has an applied max of 7,
    // and so does the binary that later adds `0005`: a max comparison reports
    // `AlreadyCurrent`, returns without ever invoking sqlx, and leaves `0005`
    // unapplied while startup reports success. Silent schema divergence on a real
    // installation. The set difference is what makes the reserved-number scheme safe.
    let applied = applied_versions(pool).await?;
    let embedded = embedded_versions(migrator);
    let applied_max = applied.iter().copied().max().unwrap_or(0);
    let embedded_max = embedded.iter().copied().max().unwrap_or(0);

    // AHEAD-state refusal (#38): the db has successfully applied a migration this
    // binary does not ship. A db newer than the binary must NOT be migrated (there is
    // no down path for migrations we don't ship) NOR opened — refuse to start
    // (MASTER-SPEC §7.4). This is a REAL `Err` (#65), never a `debug_assert!`: the
    // determinism gate + CI run `--release`, where a `debug_assert!` is compiled out
    // exactly when this guard matters. It fires BEFORE the behind-branch so no backup
    // is taken and `Migrated{from,to}` can never be reported inverted.
    //
    // Set-based for the same reason as above, and strictly stronger than the max
    // comparison it replaces: a db carrying a version this binary lacks is refused
    // even when that version sorts BELOW the embedded max.
    let ahead: Vec<i64> = applied.difference(&embedded).copied().collect();
    if !ahead.is_empty() {
        let names = ahead
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(DataError::Migration(format!(
            "db has applied migration(s) [{names}] that this binary does not ship \
             (embedded max {embedded_max}): refusing to migrate or open a db newer \
             than the binary"
        )));
    }

    // CONTENT, not just coverage. `sqlx` stores each migration's checksum and
    // refuses to run when an already-applied file has changed underneath it — but
    // only when the migrator RUNS. The early return below skips the migrator
    // entirely, so a db whose applied `0005` predates an in-place edit to
    // `0005_coaching.up.sql` would report `AlreadyCurrent` and keep the OLD schema
    // (the six-kind failure_kind CHECK, say) while the binary believes the new one
    // is live. Validating here makes the mismatch loud on BOTH branches, and covers
    // `r1.s3`'s reserved `0006` the same way.
    validate_applied_checksums(pool, migrator, &applied).await?;

    let missing: Vec<i64> = embedded.difference(&applied).copied().collect();
    if missing.is_empty() {
        return Ok(MigrationOutcome::AlreadyCurrent {
            version: applied_max,
        });
    }

    // Behind: snapshot the live db (consistent, WAL-safe) BEFORE migrating, then
    // make the snapshot crash-durable (fsync the backup file + its parent dir)
    // BEFORE the migrate runs — so a crash mid-migrate cannot lose the only recovery
    // copy (#38 durability).
    let backup = backup_path(db_path, applied_max);
    vacuum_into(pool, &backup).await?;
    fsync_file(&backup)?;
    if let Some(parent) = backup.parent() {
        fsync_dir(parent)?;
    }

    // Migrate, then verify; on ANY failure restore from the backup and refuse.
    match migrate_and_verify(pool, migrator, &embedded).await {
        Ok(()) => Ok(MigrationOutcome::Migrated {
            from: applied_max,
            to: embedded_max,
            backup,
        }),
        Err(e) => {
            // CLOSE the pool before restoring — not merely drop it — so no open
            // handle holds the file the restore's rename is about to replace
            // (issue #258: a dropped pool's connection is released later, by a
            // worker thread nothing joins).
            pool.close().await;
            restore_from_backup(db_path, &backup)?;
            Err(e)
        }
    }
}

/// Run the backup-before-migrate protocol on `db_path`, THEN open the working pool.
///
/// The single startup entry point (migrate-then-open) — keeps 1.01's
/// [`Db::with_path`](super::Db::with_path)/`open_default` pure pool-openers (no
/// migration side effect) while giving the CLI/app ONE call satisfying
/// MASTER-SPEC §7.4's "on startup migrate the schema else refuse to start".
///
/// # Errors
/// [`DataError::Migration`] if the migration step fails (the db is already
/// restored — the caller MUST NOT start); [`DataError::Db`] if the pool cannot be
/// opened after a successful migrate.
pub async fn open_migrated(db_path: &Path) -> Result<Db, DataError> {
    run_migrations_with_backup(db_path).await?;
    Db::with_path(db_path).await
}

/// [`open_migrated`] on a **copy that is about to be renamed into place** (the
/// import's temporary database, issue #258).
///
/// Same protocol, same verification, one difference that matters: the file is
/// opened in [`SqliteJournalMode::Delete`], never WAL. The install renames the
/// database file and nothing else, so a `-wal`/`-shm` beside it would be left
/// behind under a name the rename does not carry — with every row still
/// committed in it — which is exactly the silent data loss 0857a92 meant to
/// close. In rollback-journal mode there is no WAL to strand, and that is the
/// only guarantee available: `SQLite` checkpoints on its own at the WAL
/// threshold and a final checkpoint normally removes `-wal`/`-shm` when the LAST
/// connection closes, so whether a WAL-mode copy's sidecars are gone by the time
/// of a rename depends on when that close lands — and sqlx closes connections
/// from spawned tasks and worker threads that nothing here awaits.
///
/// The mode is READ BACK, not assumed: sqlx applies `PRAGMA journal_mode` when
/// it connects and ignores the statement's result, so a database that stayed in
/// WAL (another connection holding the file, a locked file) would otherwise
/// pass unnoticed and hand the install exactly the sidecars this prevents.
///
/// # Errors
///
/// As [`open_migrated`], plus [`DataError::Db`] when the copy is not in
/// rollback-journal mode after opening.
pub(crate) async fn open_migrated_copy(db_path: &Path) -> Result<Db, DataError> {
    let journal_mode = SqliteJournalMode::Delete;
    run_migrations_with_backup_using_journal(db_path, &MIGRATOR, journal_mode).await?;
    let db = Db::with_path_journal(db_path, journal_mode).await?;
    match verified_journal_mode(db.pool(), journal_mode).await {
        Ok(()) => Ok(db),
        Err(error) => {
            // Never hand a copy in the wrong mode to a rename.
            db.pool().close().await;
            Err(error)
        }
    }
}

/// Put `db_path` back in WAL and PROVE it, for a database that was just
/// published (ADR-0019; issue #259 review, F4).
///
/// `journal_mode` is persisted IN the database file, and the import installs a
/// copy that ran in rollback-journal mode (#258) — so without this the file a
/// `pulse import` leaves behind is in DELETE mode until some later process
/// happens to open it. The production posture is restored here, read back, and
/// closed before the command reports success, exactly as
/// [`open_migrated_copy`] proves the copy's mode on the way in.
///
/// # Errors
///
/// [`DataError::Db`] when the database cannot be opened, the mode cannot be
/// read, or it reports anything but WAL.
pub(crate) async fn put_in_wal(db_path: &Path) -> Result<(), DataError> {
    // ONE connection, never a pool (review correction on F4): a pool's `close`
    // can return with a connection still in flight, and the sidecars that close
    // lands late are exactly #258's failure mode. `Connection::close` shuts this
    // connection's worker thread down SYNCHRONOUSLY — the worker drops its
    // `SQLite` handle (which checkpoints and unlinks the `-wal`/`-shm` this
    // switch created) before it acknowledges — so when this returns, the file is
    // released and the directory holds the database alone.
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(super::BUSY_TIMEOUT_SECS));
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .map_err(|e| DataError::Db(format!("open {} in WAL: {e}", db_path.display())))?;
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&mut connection)
        .await
        .map_err(|e| DataError::Db(format!("read PRAGMA journal_mode: {e}")))?;
    let outcome = require_journal_mode(&mode, SqliteJournalMode::Wal);
    connection
        .close()
        .await
        .map_err(|e| DataError::Db(format!("close {}: {e}", db_path.display())))?;
    outcome
}

/// Read `PRAGMA journal_mode` back and refuse anything but `expected`.
///
/// # Errors
///
/// Returns [`DataError::Db`] when the pragma cannot be read, or when the
/// database reports a different mode — the copy would be renamed with sidecars
/// it cannot carry (issue #258).
async fn verified_journal_mode(
    pool: &SqlitePool,
    expected: SqliteJournalMode,
) -> Result<(), DataError> {
    let actual: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(pool)
        .await
        .map_err(|e| DataError::Db(format!("read PRAGMA journal_mode: {e}")))?;
    require_journal_mode(&actual, expected).map_err(|error| {
        DataError::Db(format!(
            "{error}: it is opened as a copy that is renamed into place, and a sidecar it cannot \
             carry would strand committed rows outside the installed database"
        ))
    })
}

/// Refuse a database that does not report `expected` as its journal mode.
///
/// sqlx applies `PRAGMA journal_mode` at connect and ignores the statement's
/// result, so a mode is a property to READ BACK, not to assume — the seam both
/// the copy's opener ([`open_migrated_copy`]) and the install's WAL switch
/// ([`put_in_wal`]) go through.
///
/// # Errors
///
/// Returns [`DataError::Db`] naming both modes.
fn require_journal_mode(actual: &str, expected: SqliteJournalMode) -> Result<(), DataError> {
    let expected = journal_mode_name(expected);
    if actual.eq_ignore_ascii_case(expected) {
        return Ok(());
    }
    Err(DataError::Db(format!(
        "the database reports journal_mode `{actual}`, not `{expected}`"
    )))
}

/// The `PRAGMA journal_mode` spelling of a mode, for a read-back comparison
/// (sqlx's own `as_str` is crate-private).
fn journal_mode_name(mode: SqliteJournalMode) -> &'static str {
    match mode {
        SqliteJournalMode::Delete => "delete",
        SqliteJournalMode::Truncate => "truncate",
        SqliteJournalMode::Persist => "persist",
        SqliteJournalMode::Memory => "memory",
        SqliteJournalMode::Wal => "wal",
        SqliteJournalMode::Off => "off",
    }
}

/// Revert the schema down to `target_version` (in-process, no CLI).
///
/// A thin wrapper over [`Migrator::undo`] — sqlx reverts every applied
/// down-migration with `version > target_version` via the matching `*.down.sql`.
/// No backup is taken: the backup discipline is a forward-migration property
/// (down is an explicit operator/test action and the caller already holds the
/// pool). Used by the up/down round test (run → undo → re-run).
///
/// # Errors
/// Returns [`DataError::Migration`] if the revert fails.
pub async fn undo_to(pool: &SqlitePool, target_version: i64) -> Result<(), DataError> {
    MIGRATOR
        .undo(pool, target_version)
        .await
        .map_err(|e| DataError::Migration(format!("undo to version {target_version} failed: {e}")))
}

/// Run the migrator, then re-derive the applied version SET and assert it now
/// covers every embedded version (defense-in-depth, mirroring `store/mod.rs`'s
/// audit-C5 re-derive-and-reject guard). Any error here drives the caller's
/// restore path.
///
/// Set-based rather than max-based for the reason the caller's gate is: with
/// reserved migration numbers landing out of order, a run that filled a gap
/// leaves the max unchanged, so a max comparison would report success whether the
/// gap was filled or silently skipped. The property that matters is coverage.
async fn migrate_and_verify(
    pool: &SqlitePool,
    migrator: &Migrator,
    embedded: &BTreeSet<i64>,
) -> Result<(), DataError> {
    migrator
        .run(pool)
        .await
        .map_err(|e| DataError::Migration(format!("migration run failed: {e}")))?;

    let post = applied_versions(pool).await?;
    let still_missing: Vec<i64> = embedded.difference(&post).copied().collect();
    if !still_missing.is_empty() {
        let names = still_missing
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(DataError::Migration(format!(
            "post-migration verify mismatch: migration(s) [{names}] are embedded but \
             still not applied"
        )));
    }
    Ok(())
}

/// The max **successfully-applied** migration version, or `0` when none has ever
/// committed. TEST-ONLY: the protocol itself compares version SETS (see
/// [`applied_versions`]), because with reserved numbers landing out of order a max
/// cannot distinguish "current" from "missing a lower version". The tests still
/// assert on the max, which is the readable thing to assert once coverage is
/// established.
#[cfg(test)]
async fn applied_max_version(pool: &SqlitePool) -> Result<i64, DataError> {
    Ok(applied_versions(pool)
        .await?
        .iter()
        .copied()
        .max()
        .unwrap_or(0))
}

/// Every successfully-applied version, as a set.
///
/// The MAX alone is not enough to decide whether a database is current, because
/// this project ALLOCATES MIGRATION NUMBERS AT RELEASE PLANNING and ships them out
/// of order: `r1.s1` shipped `0007` while `0005` and `0006` stayed reserved for
/// `r1.s2` and `r1.s3`. When those land, an installation that already applied
/// `0001-0004` + `0007` has `applied_max == embedded_max == 7` while genuinely
/// missing two migrations. A max comparison calls that current and skips the
/// migrator entirely; the set difference sees the gap.
///
/// **Committed-state filter (#38 / audit C6).** sqlx records a row in
/// `_sqlx_migrations` for *every* attempt, with a `success` column that is `0`
/// until the migration commits. This function MUST filter on `success = TRUE` so
/// the value reflects the **applied schema**, not arbitrary migration history: a
/// failed or partially-applied future-version row (`success = 0`) must NOT count,
/// otherwise a single botched future-migration attempt would spuriously trip the
/// ahead-state refusal and brick the binary. The ahead-state guard in
/// [`run_migrations_with_backup_using`] depends on this filter being in place.
///
/// Raw `sqlx::query_scalar` (no `query!` macro — this item ships no `.sqlx` cache).
async fn applied_versions(pool: &SqlitePool) -> Result<BTreeSet<i64>, DataError> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| DataError::Db(e.to_string()))?;
    if exists == 0 {
        return Ok(BTreeSet::new());
    }
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success = TRUE")
            .fetch_all(pool)
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;
    Ok(versions.into_iter().collect())
}

/// Assert every APPLIED migration still matches the embedded file's checksum.
///
/// `sqlx` performs this check inside [`Migrator::run`]; this is the same property
/// asserted where the protocol can act on it — before the `AlreadyCurrent` early
/// return, which never reaches `run`. A db carrying stale content for a version
/// this binary ships is not "current": it is a silent schema divergence, the exact
/// failure the set-based coverage check was written to prevent, one level down.
///
/// Versions applied but NOT embedded are skipped here — the ahead-state guard has
/// already refused those, and re-reporting them as checksum mismatches would bury
/// the clearer message.
///
/// # Errors
/// [`DataError::Migration`] listing every version whose stored checksum differs
/// from the embedded file's; [`DataError::Db`] if the table cannot be read.
async fn validate_applied_checksums(
    pool: &SqlitePool,
    migrator: &Migrator,
    applied: &BTreeSet<i64>,
) -> Result<(), DataError> {
    if applied.is_empty() {
        return Ok(());
    }

    let stored: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version, checksum FROM _sqlx_migrations WHERE success = TRUE ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| DataError::Db(e.to_string()))?;

    let mut mismatched = Vec::new();
    for (version, checksum) in stored {
        let Some(embedded) = migrator
            .iter()
            .find(|m| m.migration_type.is_up_migration() && m.version == version)
        else {
            continue;
        };
        if embedded.checksum.as_ref() != checksum.as_slice() {
            mismatched.push(version);
        }
    }

    if !mismatched.is_empty() {
        let names = mismatched
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(DataError::Migration(format!(
            "db has applied migration(s) [{names}] whose content no longer matches this \
             binary's embedded file: the schema on disk is not the schema this binary \
             expects; refusing to open"
        )));
    }
    Ok(())
}

/// Every version among the migrator's *up* migrations (the `iter()` yields both
/// up and down entries per reversible step; only up entries define the target).
fn embedded_versions(migrator: &Migrator) -> BTreeSet<i64> {
    migrator
        .iter()
        .filter(|m| m.migration_type.is_up_migration())
        .map(|m| m.version)
        .collect()
}

/// The max version among the migrator's *up* migrations. TEST-ONLY, for the same
/// reason as [`applied_max_version`].
#[cfg(test)]
fn embedded_max_version(migrator: &Migrator) -> i64 {
    embedded_versions(migrator)
        .iter()
        .copied()
        .max()
        .unwrap_or(0)
}

/// `pulse.db.migrate.lock` co-located beside `db_path` — the `flock` target for
/// [`acquire_migration_lock`]. The lock file is never written to or deleted;
/// its inode is the rendezvous point for every process that migrates this db.
fn migration_lock_path(db_path: &Path) -> Result<PathBuf, DataError> {
    let file_name = db_path
        .file_name()
        .ok_or_else(|| {
            DataError::Migration(format!("db path has no file name: {}", db_path.display()))
        })?
        .to_string_lossy();
    let dir = db_path.parent().ok_or_else(|| {
        DataError::Migration(format!("db path has no parent: {}", db_path.display()))
    })?;
    Ok(dir.join(format!("{file_name}.migrate.lock")))
}

/// Take an exclusive `flock(2)` on the migration lock file and return the open
/// [`std::fs::File`] that holds it — drop releases the lock (flock is per
/// open-file-description, so closing the fd frees a parked migrator even on a
/// panicking path).
///
/// `flock` blocks until the holder releases, so the wait runs on the blocking
/// pool rather than parking a runtime worker. The lock is deliberately NOT
/// released between backup and migrate: the restore-on-failure path is part of
/// the critical section, since it is the step that renames over a live db.
#[cfg(unix)]
async fn acquire_migration_lock(db_path: &Path) -> Result<std::fs::File, DataError> {
    use std::os::unix::io::AsRawFd;

    let lock_path = migration_lock_path(db_path)?;
    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| DataError::Migration(format!("open {}: {e}", lock_path.display())))?;
        // SAFETY: `file` is a live open fd for the duration of the call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(DataError::Migration(format!(
                "flock {}: {}",
                lock_path.display(),
                std::io::Error::last_os_error()
            )));
        }
        Ok(file)
    })
    .await
    .map_err(|e| DataError::Migration(format!("migration lock task failed: {e}")))?
}

/// No-op on platforms without `flock` (matching `mcp/export.rs`'s
/// `set_owner_only` posture). Returns a file that holds no lock so the call
/// site is identical — the guard's only job is to stay alive to end of scope.
#[cfg(not(unix))]
async fn acquire_migration_lock(db_path: &Path) -> Result<std::fs::File, DataError> {
    let lock_path = migration_lock_path(db_path)?;
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| DataError::Migration(format!("open {}: {e}", lock_path.display())))
}

/// `pulse.db.bak-<from_version>-<timestamp>` co-located beside `db_path` (NFR-12).
/// Same directory ⇒ the restore-rename is atomic on the same filesystem. The
/// timestamp is a filesystem-safe UTC stamp (`%Y%m%dT%H%M%SZ`, no colons).
///
/// `VACUUM INTO` requires the target NOT already exist. Second-resolution stamps
/// can collide when two migrations of the same db run inside one second (the
/// up/down round: run → undo → re-run). The common case keeps the exact
/// `pulse.db.bak-<from>-<stamp>` name; a `-N` suffix is appended ONLY on a probed
/// collision, so the name stays unique without losing the documented convention.
fn backup_path(db_path: &Path, from_version: i64) -> PathBuf {
    let dir = db_path.parent();
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let base = format!("pulse.db.bak-{from_version}-{stamp}");
    let join = |name: &str| match dir {
        Some(d) => d.join(name),
        None => PathBuf::from(name),
    };

    let primary = join(&base);
    if !primary.exists() {
        return primary;
    }
    // Disambiguate a within-second collision; bounded probe keeps it total.
    for n in 1..u32::MAX {
        let candidate = join(&format!("{base}-{n}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    primary
}

/// Consistent-snapshot backup via `VACUUM INTO` (WAL-safe by construction). Writes
/// a single transactionally-consistent copy of the live db regardless of WAL
/// state — no manual checkpoint, no sidecar handling, no torn-snapshot race. The
/// snapshot is *logically* identical to the source (defragmented, not byte-equal).
async fn vacuum_into(pool: &SqlitePool, backup: &Path) -> Result<(), DataError> {
    // `VACUUM INTO` takes a string literal, not a bound parameter; the path is a
    // process-local, timestamp-derived name (not user input). Escape single quotes
    // defensively so a quote in the temp dir can't break the statement.
    let target = backup.to_string_lossy().replace('\'', "''");
    let stmt = format!("VACUUM INTO '{target}'");
    sqlx::query(&stmt)
        .execute(pool)
        .await
        .map_err(|e| DataError::Migration(format!("backup VACUUM INTO failed: {e}")))?;
    Ok(())
}

/// Restore `db_path` from `backup` (atomic rename on the same dir, mirroring
/// `store/mod.rs`'s temp→rename discipline) and delete stale `-wal`/`-shm`
/// sidecars beside `db_path`.
///
/// The backup is *copied* to a hidden temp then renamed over `db_path` so the
/// backup file itself is retained (forensics + NFR-12). A failed migrate may have
/// left WAL frames that would otherwise re-apply over the restored file; the
/// `VACUUM INTO` snapshot already has everything committed into the main file, so
/// the sidecars must go.
fn restore_from_backup(db_path: &Path, backup: &Path) -> Result<(), DataError> {
    let dir = db_path.parent().ok_or_else(|| {
        DataError::Migration(format!("db path has no parent: {}", db_path.display()))
    })?;

    let tmp = hidden_temp_path(db_path)?;
    std::fs::copy(backup, &tmp).map_err(|e| {
        DataError::Migration(format!(
            "restore: copy {} -> {} failed: {e}",
            backup.display(),
            tmp.display()
        ))
    })?;
    std::fs::rename(&tmp, db_path).map_err(|e| {
        DataError::Migration(format!(
            "restore: rename {} -> {} failed: {e}",
            tmp.display(),
            db_path.display()
        ))
    })?;

    // Drop stale WAL/SHM sidecars so they cannot re-apply over the restored file.
    for ext in ["-wal", "-shm"] {
        let sidecar = sidecar_path(db_path, ext);
        if sidecar.exists() {
            std::fs::remove_file(&sidecar).map_err(|e| {
                DataError::Migration(format!(
                    "restore: remove sidecar {} failed: {e}",
                    sidecar.display()
                ))
            })?;
        }
    }

    // fsync the directory so the rename + sidecar removals are durable.
    fsync_dir(dir)?;
    Ok(())
}

/// fsync a directory so a just-completed `rename` into it is durable (mirrors
/// `store/mod.rs::fsync_dir` — that fn is private to a different module).
fn fsync_dir(dir: &Path) -> Result<(), DataError> {
    let file = std::fs::File::open(dir).map_err(|e| {
        DataError::Migration(format!("restore: open dir {} failed: {e}", dir.display()))
    })?;
    file.sync_all().map_err(|e| {
        DataError::Migration(format!("restore: fsync dir {} failed: {e}", dir.display()))
    })
}

/// fsync a just-written file so its bytes are durable on disk before the migrate
/// runs (#38 backup durability — pairs with [`fsync_dir`] on the parent so both the
/// file contents AND the directory entry survive a crash).
fn fsync_file(path: &Path) -> Result<(), DataError> {
    let file = std::fs::File::open(path).map_err(|e| {
        DataError::Migration(format!("backup: open {} failed: {e}", path.display()))
    })?;
    file.sync_all().map_err(|e| {
        DataError::Migration(format!("backup: fsync file {} failed: {e}", path.display()))
    })
}

/// A hidden temp path co-located with `db_path` (same dir ⇒ rename is atomic on
/// the same filesystem; mirrors `store/mod.rs::temp_path`).
fn hidden_temp_path(db_path: &Path) -> Result<PathBuf, DataError> {
    let file_name = db_path
        .file_name()
        .ok_or_else(|| {
            DataError::Migration(format!("db path has no file name: {}", db_path.display()))
        })?
        .to_string_lossy();
    let dir = db_path.parent().ok_or_else(|| {
        DataError::Migration(format!("db path has no parent: {}", db_path.display()))
    })?;
    Ok(dir.join(format!(".{file_name}.restore.tmp")))
}

/// The `-wal` / `-shm` sidecar path beside a `SQLite` db file (the suffix is
/// appended to the full file name, e.g. `pulse.db-wal`).
fn sidecar_path(db_path: &Path, ext: &str) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(ext);
    PathBuf::from(name)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        MigrationOutcome, applied_max_version, embedded_max_version, migration_lock_path,
        run_migrations_with_backup, run_migrations_with_backup_using, undo_to,
    };
    use crate::adapters::db::{Db, MIGRATOR};
    use crate::domain::DataError;
    use sqlx::migrate::Migrator;
    use std::path::Path;
    use tempfile::TempDir;

    /// Count `pulse.db.bak-*` files in `dir`.
    fn count_backups(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .expect("read temp dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("pulse.db.bak-"))
            .count()
    }

    /// The single `pulse.db.bak-*` file in `dir` (panics if not exactly one).
    fn the_backup(dir: &Path) -> std::path::PathBuf {
        let mut found: Vec<_> = std::fs::read_dir(dir)
            .expect("read temp dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("pulse.db.bak-"))
            })
            .collect();
        assert_eq!(found.len(), 1, "expected exactly one backup, got {found:?}");
        found.pop().unwrap()
    }

    /// `_sqlx_migrations` max version (0 if the table is absent/empty).
    async fn applied_max(db: &Db) -> i64 {
        let exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        if exists == 0 {
            return 0;
        }
        sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
            .fetch_one(db.pool())
            .await
            .unwrap()
    }

    /// Whether `idx_strategy_name` is present in `sqlite_master`.
    async fn index_present(db: &Db) -> bool {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_strategy_name'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        n == 1
    }

    /// Bring a fresh temp db up to version 1 only (apply 0001, then undo to 1 so
    /// only 0001 remains applied) and return the guard + db path.
    async fn db_at_0001() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("pulse.db");
        let db = Db::with_path(&path).await.expect("open db");
        // Run the full embedded set then revert down to 1 — leaves exactly 0001.
        MIGRATOR.run(db.pool()).await.expect("run embedded");
        undo_to(db.pool(), 1).await.expect("undo to 1");
        assert_eq!(applied_max(&db).await, 1, "fixture must sit at version 1");
        assert!(!index_present(&db).await, "0002 index must be gone at v1");
        (tmp, path)
    }

    #[tokio::test]
    async fn migration_backup_created_when_behind() {
        // AC-11 / NFR-12: a db at 0001 is behind 0002 → backup fires, schema advances.
        let (tmp, path) = db_at_0001().await;

        let outcome = run_migrations_with_backup(&path)
            .await
            .expect("migrate from behind");

        match outcome {
            MigrationOutcome::Migrated { from, to, backup } => {
                assert_eq!(from, 1, "from must be the pre-migration version");
                assert_eq!(to, 16, "to must be the embedded max");
                assert!(
                    backup.exists(),
                    "backup file must exist: {}",
                    backup.display()
                );
                let name = backup.file_name().unwrap().to_string_lossy().into_owned();
                assert!(
                    name.starts_with("pulse.db.bak-1-"),
                    "backup name must be pulse.db.bak-1-<ts>, got {name}"
                );
            }
            other @ MigrationOutcome::AlreadyCurrent { .. } => {
                panic!("expected Migrated, got {other:?}")
            }
        }

        let db = Db::with_path(&path).await.expect("reopen db");
        assert_eq!(applied_max(&db).await, 16, "schema must now be at 0016");
        assert!(
            index_present(&db).await,
            "idx_strategy_name must exist after migrate"
        );
        assert_eq!(count_backups(tmp.path()), 1, "exactly one backup retained");
    }

    #[tokio::test]
    async fn migration_already_current_takes_no_backup() {
        // AC-12: a db already at the embedded max → AlreadyCurrent, NO backup.
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("pulse.db");
        let db = Db::with_path(&path).await.expect("open db");
        MIGRATOR
            .run(db.pool())
            .await
            .expect("bring up to embedded max");
        drop(db);

        let outcome = run_migrations_with_backup(&path)
            .await
            .expect("already-current must succeed");

        match outcome {
            MigrationOutcome::AlreadyCurrent { version } => {
                assert_eq!(version, 16, "version must be the embedded max");
            }
            other @ MigrationOutcome::Migrated { .. } => {
                panic!("expected AlreadyCurrent, got {other:?}")
            }
        }
        assert_eq!(
            count_backups(tmp.path()),
            0,
            "no backup when already current"
        );

        let db = Db::with_path(&path).await.expect("reopen");
        assert!(
            index_present(&db).await,
            "schema unchanged (index still present)"
        );
    }

    #[tokio::test]
    async fn a_stale_applied_migration_is_refused_rather_than_reported_current() {
        // PR #128 finding 3. Coverage says "every embedded version is applied";
        // CONTENT says "and it is the version this binary ships". Only the second
        // catches a db that applied `0005` before an in-place edit to it — the
        // reserved-number scheme's other half, and the case the AlreadyCurrent early
        // return skips entirely because it never reaches sqlx's own check.
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("pulse.db");
        let db = Db::with_path(&path).await.expect("open db");
        MIGRATOR
            .run(db.pool())
            .await
            .expect("bring up to embedded max");

        // The db now holds `0005` as it is TODAY. Rewrite its stored checksum to
        // stand for a db that applied yesterday's `0005`.
        sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00' WHERE version = 5")
            .execute(db.pool())
            .await
            .expect("stale the applied checksum");
        drop(db);

        let outcome = run_migrations_with_backup(&path).await;

        match outcome {
            Err(DataError::Migration(message)) => {
                assert!(
                    message.contains('5'),
                    "the refusal must name the diverged migration: {message}"
                );
            }
            other => panic!(
                "a db holding stale content for an applied migration must be refused, got {other:?}"
            ),
        }
        assert_eq!(
            count_backups(tmp.path()),
            0,
            "the refusal happens before any backup is paid for"
        );
    }

    #[tokio::test]
    async fn migration_forced_failure_restores_and_refuses_to_start() {
        // AC-13 (FR-4): a deliberately-broken migration source (test-scoped, NOT in
        // the committed migrations/ dir) → restore-on-failure + REFUSE TO START.
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("pulse.db");

        // Seed the db at 0001 (a valid first step) so there is a known-good state
        // to restore to and the broken second step makes the source "behind".
        {
            let db = Db::with_path(&path).await.expect("open db");
            MIGRATOR.run(db.pool()).await.expect("run embedded");
            undo_to(db.pool(), 1).await.expect("undo to 1");
        }

        // Build a broken runtime migrator: a temp migrations dir with an already-
        // applied 0001 + a syntactically broken 0002.
        //
        // 0001's CONTENT is copied from the committed file rather than stubbed:
        // the protocol validates the checksum of every APPLIED version against the
        // migrator it was handed, so a stub 0001 would be refused as a stale
        // migration before the broken 0002 ever ran — and this test is about the
        // restore path, not about that guard.
        let mig_dir = TempDir::new().expect("migrations tempdir");
        let real_0001 = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/0001_init.up.sql");
        std::fs::copy(&real_0001, mig_dir.path().join("0001_init.up.sql"))
            .expect("copy the committed 0001 so its checksum matches the applied row");
        std::fs::write(
            mig_dir.path().join("0002_broken.up.sql"),
            "THIS IS NOT VALID SQL ;;;",
        )
        .unwrap();
        let broken: Migrator = Migrator::new(mig_dir.path())
            .await
            .expect("build runtime migrator");

        let err = run_migrations_with_backup_using(&path, &broken)
            .await
            .expect_err("a broken migration must fail the protocol");
        assert!(
            matches!(err, DataError::Migration(_)),
            "forced failure must surface DataError::Migration, got {err:?}"
        );

        // The backup is retained for forensics (NFR-12).
        assert_eq!(
            count_backups(tmp.path()),
            1,
            "backup must be retained after restore"
        );
        let bak = the_backup(tmp.path());

        // Restore happened: db_path's bytes equal the pre-migration backup (the
        // restore is a byte copy of the backup over db_path). NOTE: this is the
        // backup snapshot's bytes, NOT the live pre-migration WAL-mode file — the
        // `VACUUM INTO` snapshot is logically identical but byte-defragmented
        // (spec §3), so the contract is "restored == backup", which the restore
        // file-copy makes byte-exact.
        let restored_bytes = std::fs::read(&path).expect("read restored db bytes");
        let backup_bytes = std::fs::read(&bak).expect("read backup db bytes");
        assert_eq!(
            restored_bytes, backup_bytes,
            "db must be restored byte-for-byte from the pre-migration backup"
        );
        // The retained backup is itself a logically-valid db at version 1.
        let bak_db = Db::with_path(&bak).await.expect("open backup as db");
        assert_eq!(
            applied_max(&bak_db).await,
            1,
            "backup snapshot is the v1 db"
        );
    }

    /// G4 / T24: `pulse mcp` is a second long-lived process that can reach the
    /// protocol while the desktop is mid-migrate on the same db. The flock on
    /// `pulse.db.migrate.lock` must park the loser until the winner's whole
    /// critical section — backup + migrate + any restore — is done.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_migrator_parks_until_the_lock_holder_releases() {
        use std::os::unix::io::AsRawFd;

        let (_tmp, path) = db_at_0001().await;

        // Stand in for the concurrently-starting process: flock conflicts across
        // open-file-descriptions, so holding LOCK_EX here parks a second
        // migrator exactly as an out-of-process holder would.
        let holder = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(migration_lock_path(&path).unwrap())
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX) },
            0,
            "the test must hold the migration lock"
        );

        let migrator_path = path.clone();
        let mut migrating =
            tokio::spawn(async move { run_migrations_with_backup(&migrator_path).await });

        // While the lock is held the loser cannot even reach detect-behind:
        // the acquire sits before the first pool open.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(500), &mut migrating)
                .await
                .is_err(),
            "a concurrent migrator must park on the lock, not race the protocol"
        );

        // Releasing frees the parked migrator, which then does the full job.
        drop(holder);
        let outcome = migrating
            .await
            .expect("migrator task panicked")
            .expect("the parked migrator must complete once the lock frees");
        assert!(
            matches!(outcome, MigrationOutcome::Migrated { from: 1, .. }),
            "the released migrator runs the real protocol, got {outcome:?}"
        );
        let db = Db::with_path(&path).await.expect("reopen");
        assert_eq!(
            applied_max(&db).await,
            16,
            "schema reached the embedded max"
        );
    }

    #[tokio::test]
    async fn migration_up_down_round_run_undo_rerun() {
        // run → undo_to(1) → re-run. Index gone then back; max 16→1→16.
        // The embedded max is 16, not 5: `0005`/`0006` were reserved for `r1.s2`
        // and `r1.s3`, allocated at release planning so parallel spines cannot
        // collide on a migration number. sqlx applies versions in numeric order
        // and does not require them to be contiguous.
        let (_tmp, path) = db_at_0001().await;

        // Up: 1 → 16 (the embedded max, now that r3.s3.w1's 0016 ships).
        run_migrations_with_backup(&path).await.expect("up to 0016");
        let db = Db::with_path(&path).await.expect("reopen after up");
        assert_eq!(applied_max(&db).await, 16, "after run, max == 16");
        assert!(index_present(&db).await, "after run, index present");

        // Down: 16 → 1.
        undo_to(db.pool(), 1).await.expect("undo to 1");
        assert_eq!(applied_max(&db).await, 1, "after undo, max == 1");
        assert!(!index_present(&db).await, "after undo, index gone");
        drop(db);

        // Re-run: 1 → 16.
        run_migrations_with_backup(&path)
            .await
            .expect("re-run to 0016");
        let db = Db::with_path(&path).await.expect("reopen after re-run");
        assert_eq!(applied_max(&db).await, 16, "after re-run, max == 16");
        assert!(index_present(&db).await, "after re-run, index back");
    }

    /// Insert a synthetic `_sqlx_migrations` row at `version` with the given
    /// `success` flag (all NOT NULL columns supplied — `description`, `checksum`
    /// BLOB, `execution_time`). Used to fabricate an ahead-of-embedded / failed
    /// future-version state without shipping a real future migration.
    async fn seed_migration_row(db: &Db, version: i64, success: bool) {
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
             (version, description, installed_on, success, checksum, execution_time) \
             VALUES (?1, ?2, CURRENT_TIMESTAMP, ?3, ?4, 0)",
        )
        .bind(version)
        .bind(format!("synthetic future migration {version}"))
        .bind(success)
        .bind(vec![0_u8; 32])
        .execute(db.pool())
        .await
        .expect("seed _sqlx_migrations row");
    }

    #[tokio::test]
    async fn migration_refuses_ahead_of_embedded() {
        // AC-12 / #38 / #65: a db whose SUCCESSFULLY-applied schema is ahead of the
        // binary's embedded max must be REFUSED with a real DataError::Migration Err
        // (refuse to start, MASTER-SPEC §7.4) — and NO backup is taken (the guard
        // fires before the behind-branch).
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("pulse.db");
        let db = Db::with_path(&path).await.expect("open db");
        MIGRATOR
            .run(db.pool())
            .await
            .expect("bring up to embedded max");

        let embedded = embedded_max_version(&MIGRATOR);
        // Seed a COMMITTED (success = TRUE) future-version row → applied_max > embedded.
        seed_migration_row(&db, embedded + 1, true).await;
        assert_eq!(
            applied_max_version(db.pool()).await.unwrap(),
            embedded + 1,
            "a committed future row advances applied_max above embedded"
        );
        drop(db);

        let err = run_migrations_with_backup(&path)
            .await
            .expect_err("an ahead-of-embedded db must be refused");
        assert!(
            matches!(err, DataError::Migration(_)),
            "ahead-state refusal must surface DataError::Migration, got {err:?}"
        );
        assert_eq!(
            count_backups(tmp.path()),
            0,
            "no backup is taken on the ahead-state refusal (it precedes the behind-branch)"
        );
    }

    #[tokio::test]
    async fn applied_max_ignores_failed_migration_rows() {
        // AC-15 / audit C6: a FAILED/partial future-version row (success = 0) must
        // NOT count toward applied_max — otherwise one botched future-migration
        // attempt would spuriously trip the ahead-state refusal and brick the binary.
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("pulse.db");
        let db = Db::with_path(&path).await.expect("open db");
        MIGRATOR
            .run(db.pool())
            .await
            .expect("bring up to embedded max");

        let embedded = embedded_max_version(&MIGRATOR);
        // Seed a FAILED (success = 0) future-version row.
        seed_migration_row(&db, embedded + 5, false).await;

        // applied_max_version filters on success = TRUE, so the failed row is ignored.
        assert_eq!(
            applied_max_version(db.pool()).await.unwrap(),
            embedded,
            "a failed (success = 0) future row must NOT raise applied_max"
        );
        drop(db);

        // And because applied_max == embedded, the protocol proceeds normally
        // (AlreadyCurrent) rather than spuriously refusing to start.
        let outcome = run_migrations_with_backup(&path)
            .await
            .expect("a failed future row must not brick the migrator");
        assert!(
            matches!(outcome, MigrationOutcome::AlreadyCurrent { version } if version == embedded),
            "expected AlreadyCurrent at the embedded max, got {outcome:?}"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod reserved_number_tests {
    use super::{MigrationOutcome, applied_max_version, run_migrations_with_backup_using};
    use crate::adapters::db::Db;
    use sqlx::migrate::Migrator;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    /// Copy the shipped `migrations/` set into `dir`.
    fn copy_shipped_set(dir: &Path) {
        let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        for entry in std::fs::read_dir(&shipped).unwrap() {
            let path = entry.unwrap().path();
            std::fs::copy(&path, dir.join(path.file_name().unwrap())).unwrap();
        }
    }

    /// Move the real `<prefix>` migration pair OUT of `dir` (into a sibling holding
    /// pen), leaving the set as it stood before that number shipped. Returns the pen
    /// for [`restore`].
    ///
    /// **Withhold-and-restore, never a synthetic probe.** Writing a stand-in
    /// migration at a chosen number works only while that number is unclaimed; every
    /// spine that ships one moves the collision to the next free number. The real
    /// pair cannot collide with anything, and it exercises the shipped migration
    /// rather than a fake of it. Generalised over the prefix at r1.s3.w2 so `0005`
    /// and `0006` share one mechanism.
    fn withhold(dir: &Path, prefix: &str) -> PathBuf {
        let pen = dir.with_extension(format!("withheld-{}", prefix.trim_end_matches('_')));
        std::fs::create_dir_all(&pen).unwrap();
        let mut moved = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name.starts_with(prefix) {
                std::fs::rename(&path, pen.join(&name)).unwrap();
                moved += 1;
            }
        }
        assert_eq!(
            moved, 2,
            "expected the real {prefix} up+down pair to withhold, moved {moved}"
        );
        pen
    }

    /// Put the withheld files back — the binary that ships the reserved migration
    /// opening the same database.
    fn restore(dir: &Path, pen: &Path) {
        for entry in std::fs::read_dir(pen).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            std::fs::rename(&path, dir.join(&name)).unwrap();
        }
    }

    async fn table_present(db: &Db, name: &str) -> bool {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1")
                .bind(name)
                .fetch_one(db.pool())
                .await
                .unwrap();
        n == 1
    }

    /// **The reserved-number scheme actually works** — a later, LOWER-numbered
    /// migration is applied through the production startup path after `0007` is
    /// already in the database.
    ///
    /// Two PR review findings meet here (#115). The first claimed sqlx itself would
    /// refuse: that adding the reserved `0005`/`0006` after `0007` is applied makes
    /// sqlx report a missing-version error rather than execute them. That is false —
    /// `sqlx-core-0.8.6`'s `Migrator::run_to` applies any version absent from
    /// `_sqlx_migrations` regardless of how it sorts against the current maximum,
    /// and its only ordering check fires in the opposite direction.
    ///
    /// The second finding was RIGHT, and is why this test lives here rather than in
    /// `tests/migration_roundtrip.rs` driving `Migrator::run` directly: THIS wrapper
    /// gated on `applied_max == embedded_max` and returned `AlreadyCurrent` without
    /// invoking sqlx at all. Filling a reserved gap does not move the maximum, so
    /// `0005` would have been silently skipped while startup reported success —
    /// schema divergence on a real installation, reached by a different route than
    /// the one first claimed. The gate compares version SETS now.
    ///
    /// **r1.s2.w2:** this used to write a SYNTHETIC `0005_reserved_spine_r1s2`
    /// probe into the copied set. `r1.s2` has since shipped the real
    /// `0005_coaching`, so a synthetic 0005 would collide with it — and picking
    /// another free low number would only move the collision to whichever spine
    /// ships next. The test now withholds and then restores the REAL `0005`
    /// instead, which cannot collide with anything and exercises the shipped
    /// migration rather than a stand-in.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_later_lower_numbered_migration_applies_through_the_startup_path() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("migrations");
        std::fs::create_dir_all(&dir).unwrap();
        copy_shipped_set(&dir);

        // The "older binary": the shipped set as it stood while `0005` was still a
        // reserved gap and `0007` had already shipped. Withhold the real 0005 —
        // and, since r1.s4.w4 the real 0008, since r2.s1.w1 the real 0009 and
        // 0010, since r2.s2.w2 the real 0011, since r2.s3.w2 the real 0012,
        // since r2.s3.w3 the real 0013, since r2.s3.w4 the real 0014, and
        // since r3.s3.w1 the real 0016.
        //
        // `0008` REBUILDS the two tables `0005` creates, so a set holding `0008`
        // without `0005` is not an older binary, it is an impossible one ("no such
        // table: coaching_proposals"). `0009` indexes `coaching_sessions`,
        // `0010` alters `backtest_run`, `0011` alters `trade`, and `0012` alters
        // `backtest_run` for the same reason, `0013` builds on
        // `backtest_run` plus `strategy_version`, `0014` builds on
        // `walk_forward_run` plus `coaching_proposals`, and `0016` is simply
        // newer. Withholding all nine is
        // also what keeps this test testing what it says: the property under
        // test is that filling a reserved gap runs a migration WITHOUT moving
        // the maximum, and letting `0008`–`0014` or `0016` ride
        // along in the same run would move it past 7 and make the
        // `from: 7, to: 7` assertion below meaningless.
        let withheld_0016 = withhold(&dir, "0016_");
        let withheld_0014 = withhold(&dir, "0014_");
        let withheld_0013 = withhold(&dir, "0013_");
        let withheld_0012 = withhold(&dir, "0012_");
        let withheld_0011 = withhold(&dir, "0011_");
        let withheld_0010 = withhold(&dir, "0010_");
        let withheld_0009 = withhold(&dir, "0009_");
        let withheld_0008 = withhold(&dir, "0008_");
        let withheld = withhold(&dir, "0005_");

        let db_path = tmp.path().join("pulse.db");
        let older = Migrator::new(dir.as_path()).await.unwrap();

        let first = run_migrations_with_backup_using(&db_path, &older)
            .await
            .expect("the older set applies");
        assert!(matches!(first, MigrationOutcome::Migrated { .. }));
        {
            let db = Db::with_path(&db_path).await.unwrap();
            assert_eq!(applied_max_version(db.pool()).await.unwrap(), 7);
        }

        // An unchanged set is still a genuine no-op — the short-circuit must survive.
        let again = run_migrations_with_backup_using(&db_path, &older)
            .await
            .expect("re-running an unchanged set is current");
        assert!(
            matches!(again, MigrationOutcome::AlreadyCurrent { version: 7 }),
            "an unchanged set must short-circuit: {again:?}"
        );

        // r1.s2 lands its reserved 0005 — BELOW the database's current maximum, so
        // the max is unchanged and a max-based gate would call this current.
        restore(&dir, &withheld);
        let gapped = Migrator::new(dir.as_path()).await.unwrap();

        let filled = run_migrations_with_backup_using(&db_path, &gapped)
            .await
            .expect("a lower-numbered migration arriving later must NOT be skipped");
        assert!(
            matches!(filled, MigrationOutcome::Migrated { from: 7, to: 7, .. }),
            "filling a reserved gap runs a migration without moving the max: {filled:?}"
        );

        let db = Db::with_path(&db_path).await.unwrap();
        assert!(
            table_present(&db, "coaching_sessions").await,
            "0005 was applied out of numeric order, as the reserved-number scheme needs"
        );
        assert_eq!(
            applied_max_version(db.pool()).await.unwrap(),
            7,
            "0005 is recorded at its own version, not appended after 0007"
        );

        // Put 0008-0014 and 0016 back so the scratch directory is the shipped
        // set again.
        restore(&dir, &withheld_0008);
        restore(&dir, &withheld_0009);
        restore(&dir, &withheld_0010);
        restore(&dir, &withheld_0011);
        restore(&dir, &withheld_0012);
        restore(&dir, &withheld_0013);
        restore(&dir, &withheld_0014);
        restore(&dir, &withheld_0016);
    }

    /// A database carrying a migration this binary does not ship is refused as
    /// ahead — even when that version sorts BELOW the embedded maximum.
    ///
    /// The refusal used to be `applied_max > embedded_max`, which a db holding an
    /// unknown LOW version walks straight past. There is no down path for a
    /// migration we do not ship, so opening that db is not an option.
    ///
    /// **r1.s3.w2:** this used to WRITE a synthetic `0006_from_a_newer_binary`
    /// probe and delete it again. `r1.s3` has since shipped the real
    /// `0006_backtest_inputs`, so a synthetic 0006 would be a SECOND version-6
    /// migration in the same directory — `Migrator::new` refuses a duplicate
    /// version, and the two `remove_file` calls would have removed the probe while
    /// leaving the real pair behind, so the "older binary" would not have been older
    /// at all. Picking another free number only moves the collision to whichever
    /// spine ships next. It now withholds and restores the REAL `0006`, the same way
    /// the test above does for `0005`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_db_holding_an_unknown_low_version_is_refused_as_ahead() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("migrations");
        std::fs::create_dir_all(&dir).unwrap();
        copy_shipped_set(&dir);

        // The NEWER binary ships everything, real 0006 included, and applies it.
        let db_path = tmp.path().join("pulse.db");
        let newer = Migrator::new(dir.as_path()).await.unwrap();
        run_migrations_with_backup_using(&db_path, &newer)
            .await
            .expect("the newer binary's set applies");
        {
            let db = Db::with_path(&db_path).await.unwrap();
            assert!(
                super::applied_versions(db.pool())
                    .await
                    .unwrap()
                    .contains(&6),
                "the newer binary applied 0006 — the version the older one below lacks"
            );
        }

        // Now the OLDER binary — the same set with the real 0006 withheld — opens
        // the same db. 0006 sorts BELOW the embedded max, so a max comparison
        // would walk straight past it.
        let withheld = withhold(&dir, "0006_");
        let older = Migrator::new(dir.as_path()).await.unwrap();

        let err = run_migrations_with_backup_using(&db_path, &older)
            .await
            .expect_err("a db holding a migration this binary lacks must refuse to start");
        let message = err.to_string();
        assert!(
            message.contains('6'),
            "the refusal must name the offending version: {message}"
        );

        restore(&dir, &withheld);
    }
}

// ---------------------------------------------------------------------------
// issue #258 — the copy the install renames, and the sidecars it can strand
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod install_copy_tests {
    use super::{open_migrated, open_migrated_copy};
    use crate::adapters::db::Db;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    /// Every `-wal`/`-shm`/`-journal`/`.partial` sidecar beside a database —
    /// the files a rename of the database alone cannot carry. `migrate.lock` is
    /// deliberately NOT one of them: the protocol creates and keeps it by
    /// design (it holds no committed rows).
    fn sidecars(db: &Path) -> Vec<PathBuf> {
        ["-wal", "-shm", "-journal", ".partial"]
            .iter()
            .map(|suffix| PathBuf::from(format!("{}{suffix}", db.display())))
            .filter(|sidecar| sidecar.exists())
            .collect()
    }

    /// `PRAGMA journal_mode` as this connection sees it.
    async fn journal_mode(db: &Db) -> String {
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(db.pool())
            .await
            .unwrap();
        mode.to_ascii_lowercase()
    }

    /// WAL is PERSISTED IN the database file, not just in the connection: header
    /// bytes 18/19 are the file format's write/read versions (2 = WAL, 1 =
    /// legacy rollback journal). Reading them is the file-level check that
    /// [`journal_mode`] alone cannot make.
    fn file_declares_wal(db: &Path) -> bool {
        let header = std::fs::read(db).unwrap();
        header.get(18) == Some(&2) && header.get(19) == Some(&2)
    }

    /// Issue #258 (AC-1): the copy the install renames into place runs in
    /// ROLLBACK-JOURNAL mode, so no `-wal`/`-shm` can exist beside it at any
    /// moment — the structural half of the fix. The mode is read back, not
    /// assumed: sqlx applies `PRAGMA journal_mode` and ignores its result.
    #[tokio::test]
    async fn a_copy_opened_for_the_install_runs_in_rollback_journal_mode() {
        let tmp = TempDir::new().unwrap();
        let copy_path = tmp.path().join(".pulse.db.import-tmp-1.db");

        let copy = open_migrated_copy(&copy_path).await.unwrap();
        assert_eq!(
            journal_mode(&copy).await,
            "delete",
            "the copy the install renames must not be in WAL"
        );
        copy.pool().close().await;

        assert!(
            !file_declares_wal(&copy_path),
            "and the file itself does not declare WAL: a rename carries the file only"
        );
        assert!(
            sidecars(&copy_path).is_empty(),
            "so nothing is left beside it to strand: {:?}",
            sidecars(&copy_path)
        );
    }

    /// ADR-0019's WAL posture is a property OF THE FILE the install publishes:
    /// the copy arrives in rollback-journal mode (#258), so the app's NEXT normal
    /// open ([`open_migrated`], which opens through `Db::with_path`) must put the
    /// database back in WAL — otherwise a production database would run in
    /// DELETE mode after every import or restore.
    #[tokio::test]
    async fn an_installed_copy_returns_to_wal_on_the_next_normal_open() {
        let tmp = TempDir::new().unwrap();
        let copy_path = tmp.path().join(".pulse.db.import-tmp-1.db");
        let target = tmp.path().join("pulse.db");

        let copy = open_migrated_copy(&copy_path).await.unwrap();
        copy.pool().close().await;
        // The install: the copy takes the target's name (what `install_tmp_db`
        // does with the file).
        std::fs::rename(&copy_path, &target).unwrap();
        assert!(!file_declares_wal(&target), "installed in rollback mode");

        let installed = open_migrated(&target).await.unwrap();
        assert_eq!(
            journal_mode(&installed).await,
            "wal",
            "the first normal open puts the installed database back in WAL"
        );
        installed.pool().close().await;

        // And it stays that way across restarts: a fresh open reads the same
        // mode back, and the file itself now declares WAL.
        let reopened = open_migrated(&target).await.unwrap();
        assert_eq!(
            journal_mode(&reopened).await,
            "wal",
            "WAL survives a restart"
        );
        reopened.pool().close().await;
        assert!(
            file_declares_wal(&target),
            "the installed database's own header says WAL"
        );
    }
}
