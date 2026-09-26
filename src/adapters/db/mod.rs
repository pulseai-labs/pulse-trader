//! The `SQLite` persistence tier (VS-1.1.4 work-1.01) — the `Db` pool wrapper +
//! the embedded migration set.
//!
//! `sqlx` lives ONLY in this module tree (the domain stays I/O-free). This item
//! ships the pool + connect-options PRAGMAs + the embedded `0001_init` migration;
//! the repo CRUD (1.03, with `query!` macros + the committed `.sqlx` cache), the
//! domain types/port (1.02), the backup-before-migrate wrapper (1.04), and the
//! CLI (1.05) compose against it next.
//!
//! The constructor pair mirrors `CandleStore`'s `with_base_dir`/
//! `with_default_base_dir` injectable-base discipline: production resolves the
//! real platform path; tests inject a `tempfile` path so the suite never touches
//! the real `pulse.db`.

// r3.s3.w5: `pub(crate)` so the client-core's connection file resolves the
// SAME platform data dir (`default_data_dir`) the database uses — one
// location, never a second invented one.
pub(crate) mod paths;
pub mod strategy_repo;
// VS-1.2.4 work-4.04: the SQLite `BacktestRunRepository` adapter (FR-6 / FR-7).
// `query!` macros for `backtest_run`/`trade` are confined here (the `.sqlx` cache
// is keyed to this file). Append-only beside `strategy_repo` (keep-both at merge).
pub mod backtest_run_repo;
// VS-1.3.1 work-1.02: the SQLite `LlmCallRepository` adapter (FR-24, README C6).
// `query!` macros for `llm_call` are confined here (the `.sqlx` cache is keyed to
// this file). Append-only beside `backtest_run_repo` (trivial keep-both with 1.03's
// `strategy_repo`-adjacent additions at R2 integration).
pub mod llm_call_repo;
// r1.s2.w2 (ADR-0021): the SQLite `CoachingRepository` adapter. `query!` macros for
// `coaching_sessions`/`coaching_proposals` are confined here (the `.sqlx` cache is
// keyed to this file). Append-only beside `llm_call_repo`.
pub mod coaching_repo;

// VS-1.1.4 work-1.05 (§4a-3): the CLI dispatch resolves the default `pulse.db`
// path BEFORE calling `open_migrated` (migrate-then-open), so it must reach
// `default_db_path()`. It lives `pub(crate)` inside the private `mod paths`, so
// `src/cli/` cannot see it without this one-line additive re-export. This is the
// documented exception to spec §9's "no new re-exports" rule — a minimal,
// intentional 1.01-surface touch. `pub(crate)` keeps it crate-internal (no leak).
pub(crate) use paths::default_db_path;
// r1.s1.w2: the app-data DIRECTORY half of the same helper. `adapters::secrets`
// resolves the credential `.env` through it, so the key sits beside `pulse.db`
// rather than in a second invented location (spec step 2 / the binding constraint).
pub(crate) use paths::default_data_dir;

pub use strategy_repo::SqliteStrategyRepo;
// VS-1.2.4 work-4.04: the run-repo adapter type. REQUIRED under `deny(warnings)` —
// a `pub` type unused outside its module is a `dead_code` BUILD error (the
// `db/mod.rs` re-export is necessary but not sufficient; lib.rs mirrors it).
// Append-only (keep-both with strategy_repo's re-export at merge).
pub use backtest_run_repo::SqliteBacktestRunRepo;
// VS-1.3.1 work-1.02: the SQLite `LlmCallRepository` adapter type. REQUIRED under
// `deny(warnings)` — a `pub` type unused outside its module is a `dead_code` BUILD
// error (the `db/mod.rs` re-export is necessary but not sufficient; lib.rs mirrors
// it). Append-only (trivial keep-both with 1.03's re-exports at merge).
pub use llm_call_repo::SqliteLlmCallRepo;
// r1.s2.w2: the SQLite `CoachingRepository` adapter type. REQUIRED under
// `deny(warnings)` — a `pub` type unused outside its module is a `dead_code` BUILD
// error (this re-export is necessary but not sufficient; lib.rs mirrors it).
pub use coaching_repo::SqliteCoachingRepo;

// r1.s4.w1 (#132): the SQLite `CoachTurnSource` adapter — the repository-owned
// coach-turn projection. It adds NO `query!` macro of its own (and therefore no
// `.sqlx` entry): it composes the run and strategy repositories' existing
// fail-closed reads, keyed by the one `run_id` a caller supplies.
pub mod coach_turn_source;
pub use coach_turn_source::SqliteCoachTurnSource;

// r1.s4.w4 (ADR-0010 / ADR-0021 as amended): the SQLite `CoachAcceptanceRepository`
// adapter. One accept is one transaction — child version, run, trades and the
// proposal's links commit together or not at all — and the child/run identity is
// MINTED inside it from the injected id/clock sources, with provenance derived from
// the claimed session row.
pub mod coach_acceptance_repo;
pub use coach_acceptance_repo::SqliteCoachAcceptanceRepo;

// r2.s3.w3 (ADR-0025): the `WalkForwardRunRepository` impl on
// `SqliteBacktestRunRepo` — the `walk_forward_run`/`walk_forward_fold` surface.
// The fold runs themselves are ordinary `backtest_run` rows written by
// `backtest_run_repo`'s `insert_run_row`/`insert_trade_rows`, so `query!` stays
// confined to this module tree (the `.sqlx` cache covers this file too).
pub mod walk_forward_run_repo;

// r3.s3.w1 (D5/D8, ADR-0026): the client-token store — `client_token` rows and
// the append-only `token_audit` ledger. `query!` macros for these two tables are
// confined here (the `.sqlx` cache is keyed to this file).
pub mod client_token_repo;
// `TokenStoreError` is deliberately NOT re-exported: nothing outside this
// module tree names it (callers map it into their own error vocabulary).
pub use client_token_repo::{ClientToken, SqliteClientTokenRepo};

// r3.s3.w4 (D7/D12, ADR-0026): the small copy/verify helper set the data-ops
// verbs compose — the read-only source open, the VACUUM INTO copy, count/hash/
// id reads and the referenced-snapshot projection. Raw `query`/`query_scalar`
// only: NO `query!` macros, so the committed `.sqlx` cache is untouched.
// (`lib.rs` re-exports the items from `adapters::db::ops::` directly, which is
// what keeps them alive under deny(warnings)/dead_code.)
pub mod ops;

// VS-1.1.4 work-1.04: the backup-before-migrate protocol. Re-export EVERY public
// item — under `#![deny(warnings)]` a `pub` item unused outside its module is a
// `dead_code` BUILD ERROR, not a warning (VS-1.1.2 harvested gotcha). All three
// fns + the outcome enum are surfaced (lib.rs mirrors these). Append-only across
// the parallel R2 items (trivial keep-both with 1.03's `pub mod strategy_repo;`).
pub mod migrate;
pub use migrate::{MigrationOutcome, open_migrated, run_migrations_with_backup, undo_to};
// r3.s3 (issue #258): the data-ops opener for a temporary copy — the same
// migrate-then-open protocol in rollback-journal mode, so the file the import
// is about to rename into place can never have a `-wal`/`-shm` beside it.
pub(crate) use migrate::open_migrated_copy;

use std::path::Path;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

use crate::domain::DataError;

/// The number of seconds a contending writer waits for the WAL write lock before
/// failing with `SQLITE_BUSY` (gate-7 C5). Even in a single process the app's
/// async tasks can contend, so the busy-timeout is set on every pooled connection.
const BUSY_TIMEOUT_SECS: u64 = 5;

/// The embedded migration set (FR-4 / NFR-12). `sqlx::migrate!` reads the
/// crate-root `migrations/` directory **at compile time**, so build / test / demo
/// need neither a live DB nor `sqlx-cli` — the migrations travel in the binary.
/// 1.04 drives `MIGRATOR.run(pool)` / `MIGRATOR.undo(pool, target)` in-process;
/// re-exported from `lib.rs` so it (and the integration boundary) can reach it.
/// r1.s4.w4: `sqlx::migrate!` records a dependency on the migration files it saw
/// when it last expanded, so ADDING a file (here, `0009_external_agent_window_claim`)
/// does not by itself invalidate the cached expansion. Editing this file is what
/// forces the re-expansion, which is why a new migration always comes with a
/// touch here.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// A thin newtype over a `sqlx::SqlitePool` for the `PulseTrader` `SQLite` tier.
///
/// Every pooled connection inherits WAL + `foreign_keys = ON` + a 5s busy-timeout
/// from the connect options (the shared-contract PRAGMA mandate + gate-7 C5). The
/// constructor pair is the test seam: `with_path` injects an explicit (tempfile)
/// path; `open_default` resolves the platform `pulse.db`.
#[derive(Debug, Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    /// Open (creating if absent) a pool at an explicit path — the test seam.
    ///
    /// The production posture (ADR-0019): WAL, `foreign_keys = ON` and a 5s
    /// busy-timeout, so every pooled connection inherits them. Delegates to
    /// [`Self::with_path_journal`].
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] if the pool cannot be opened (the flattened
    /// `sqlx::Error` message).
    pub async fn with_path(path: &Path) -> Result<Self, DataError> {
        Self::with_path_journal(path, SqliteJournalMode::Wal).await
    }

    /// [`Self::with_path`] with an explicit journal mode.
    ///
    /// WAL is the database's production posture and is persisted IN the database
    /// file, so this is the seam that decides it: the import's temporary copy
    /// asks for [`SqliteJournalMode::Delete`] instead, because the install
    /// renames the database file and nothing else, and in WAL mode the rows a
    /// commit left in the `-wal` cannot travel with it (issue #258). The
    /// installed target is opened again through [`Self::with_path`], which is
    /// what puts it back in WAL on first open (ADR-0019).
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] if the pool cannot be opened (the flattened
    /// `sqlx::Error` message).
    pub async fn with_path_journal(
        path: &Path,
        journal_mode: SqliteJournalMode,
    ) -> Result<Self, DataError> {
        // r1.s1.w2 / issue #42: create the parent directory BEFORE building the
        // connect options. sqlx's `create_if_missing` creates the database FILE but
        // not the directory holding it, so on a machine that has never run `pulse`
        // this call fails with SQLITE_CANTOPEN. Every existing test injects an
        // already-present tempdir through `--db`, which is exactly why the only
        // zero-config path was never exercised — and a Finder-launched app has no
        // `--db` flag to inject one.
        //
        // It lives here rather than in `open_default` so every caller of the seam
        // benefits, and it is a no-op when the directory already exists.
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                DataError::Io(format!(
                    "could not create the database directory {}: {e}",
                    parent.display()
                ))
            })?;
        }

        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(journal_mode)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(BUSY_TIMEOUT_SECS));
        let pool = SqlitePoolOptions::new()
            .connect_with(opts)
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;
        Ok(Self { pool })
    }

    /// Open the pool at the platform-default `pulse.db` path
    /// (`~/Library/Application Support/PulseTrader/pulse.db` on macOS).
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Io`] if no platform data directory is resolvable, or
    /// [`DataError::Db`] if the pool cannot be opened.
    pub async fn open_default() -> Result<Self, DataError> {
        let path = paths::default_db_path()?;
        Self::with_path(&path).await
    }

    /// Borrow the underlying pool (so 1.03's repo can run queries against it).
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Db, MIGRATOR, paths};
    use crate::domain::DataError;
    use tempfile::TempDir;

    /// A `Db` opened at a tempfile path, with the `0001_init` migration applied.
    /// Returns the `TempDir` guard so the scratch DB outlives the test body.
    async fn migrated_db() -> (Db, TempDir) {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("pulse.db");
        let db = Db::with_path(&path)
            .await
            .expect("open db at tempfile path");
        MIGRATOR
            .run(db.pool())
            .await
            .expect("run 0001_init migration");
        (db, tmp)
    }

    /// Insert one `strategy` + one `strategy_version` fixture row (raw
    /// `sqlx::query`, no `query!` macro — this item ships no `.sqlx` cache).
    async fn seed_one_version(db: &Db) {
        sqlx::query("INSERT INTO strategy (id, name, created_at) VALUES (?1, ?2, ?3)")
            .bind("strat-1")
            .bind("Test Strategy")
            .bind("2026-06-14T00:00:00Z")
            .execute(db.pool())
            .await
            .expect("insert strategy row");

        sqlx::query(
            "INSERT INTO strategy_version \
             (id, strategy_id, dsl_schema_version, dsl, dsl_original, version_hash, created_by, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind("ver-1")
        .bind("strat-1")
        .bind("1.0.0")
        .bind("{}")
        .bind("{}")
        .bind("deadbeef")
        .bind("Human")
        .bind("2026-06-14T00:00:00Z")
        .execute(db.pool())
        .await
        .expect("insert strategy_version row");
    }

    #[tokio::test]
    async fn default_db_path_ends_in_pulse_db() {
        // The default resolver names the single-file DB; tests still inject a
        // tempfile path, never this real location.
        let path = paths::default_db_path().expect("resolve default db path");
        assert!(
            path.to_string_lossy().ends_with("pulse.db"),
            "default db path must end in pulse.db, got {}",
            path.display()
        );
        assert!(
            path.to_string_lossy()
                .to_ascii_lowercase()
                .contains("pulsetrader"),
            "default db path must be namespaced under pulsetrader (case-insensitive), got {}",
            path.display()
        );
    }

    #[tokio::test]
    async fn with_path_roundtrips_the_pool_constructor() {
        // The explicit-path constructor opens a usable pool at a tempfile path.
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("pulse.db");
        let db = Db::with_path(&path).await.expect("open db");
        // A trivial query proves the pool is live.
        let one: i64 = sqlx::query_scalar("SELECT 1")
            .fetch_one(db.pool())
            .await
            .expect("SELECT 1 against the pool");
        assert_eq!(one, 1);
    }

    #[tokio::test]
    async fn db_applies_migrations_and_creates_schema() {
        // AC-6: the embedded 0001_init migration applies in-process (no sqlx-cli,
        // no live DB) and creates the full schema: both tables, both idx_sv_*
        // indexes, and both immutability triggers.
        let (db, _tmp) = migrated_db().await;

        let names: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master \
             WHERE type IN ('table', 'index', 'trigger') ORDER BY name",
        )
        .fetch_all(db.pool())
        .await
        .expect("read sqlite_master");

        for expected in [
            "strategy",
            "strategy_version",
            "idx_sv_strategy_id",
            "idx_sv_parent",
            "strategy_version_no_update",
            "strategy_version_no_delete",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "0001_init must create `{expected}`; sqlite_master has {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn strategy_version_update_is_rejected_by_trigger() {
        // AC-7 (FR-4): a raw UPDATE on a strategy_version row is aborted by the
        // BEFORE UPDATE trigger; the RAISE(ABORT, ...) surfaces as DataError::Db
        // whose message contains "strategy_version is immutable".
        let (db, _tmp) = migrated_db().await;
        seed_one_version(&db).await;

        let err: DataError = sqlx::query("UPDATE strategy_version SET dsl = ?1 WHERE id = ?2")
            .bind("{\"mutated\":true}")
            .bind("ver-1")
            .execute(db.pool())
            .await
            .map_err(|e| DataError::Db(e.to_string()))
            .expect_err("UPDATE on an immutable strategy_version must fail");

        match err {
            DataError::Db(msg) => assert!(
                msg.contains("strategy_version is immutable"),
                "trigger ABORT message must surface; got: {msg}"
            ),
            other => panic!("expected DataError::Db, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn strategy_version_delete_is_rejected_by_trigger() {
        // AC-8 (FR-4): SQLite needs a SEPARATE BEFORE DELETE trigger — this pins
        // that BOTH are wired, not just the UPDATE one.
        let (db, _tmp) = migrated_db().await;
        seed_one_version(&db).await;

        let err: DataError = sqlx::query("DELETE FROM strategy_version WHERE id = ?1")
            .bind("ver-1")
            .execute(db.pool())
            .await
            .map_err(|e| DataError::Db(e.to_string()))
            .expect_err("DELETE on an immutable strategy_version must fail");

        match err {
            DataError::Db(msg) => assert!(
                msg.contains("strategy_version is immutable"),
                "trigger ABORT message must surface; got: {msg}"
            ),
            other => panic!("expected DataError::Db, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pragmas_wal_and_foreign_keys_are_enabled() {
        // AC-9 (gate-7 C5): the connect-options PRAGMAs apply to pooled
        // connections — journal_mode=wal, foreign_keys=1, busy_timeout=5000ms.
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("pulse.db");
        let db = Db::with_path(&path).await.expect("open db");

        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(db.pool())
            .await
            .expect("read PRAGMA journal_mode");
        assert_eq!(
            journal_mode.to_ascii_lowercase(),
            "wal",
            "WAL must be persisted on first connect"
        );

        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(db.pool())
            .await
            .expect("read PRAGMA foreign_keys");
        assert_eq!(foreign_keys, 1, "foreign_keys must be ON");

        let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(db.pool())
            .await
            .expect("read PRAGMA busy_timeout");
        assert_eq!(
            busy_timeout, 5000,
            "busy_timeout must be 5000ms (gate-7 C5)"
        );
    }
}
