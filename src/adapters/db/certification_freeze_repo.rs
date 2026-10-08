//! The `SQLite` adapter for the `certification_freeze` table (r4.s1.w4, F1/C4).
//!
//! This is the ONLY place `query!` macros for this table live (`sqlx` is
//! confined to `adapters::db`, mirror `client_token_repo.rs`); the committed
//! `.sqlx/` offline cache is keyed to the macros here (refresh with
//! `just prepare` under sqlx-cli `=0.8.6`).
//!
//! **Two laws, named here AND held by the schema.** `open` refuses when a
//! freeze is already open, and when the requested holdout start is not strictly
//! later than every earlier close (F1: a spent holdout is never reused). The
//! `0019` partial unique index and the `certification_freeze_start_after_last_close`
//! trigger refuse the same two states at the database, so a raw INSERT cannot
//! reach them either — the store's checks exist so the operator reads a named
//! reason instead of an opaque constraint error.
//!
//! **The record is immutable except one close.** `close` writes `closed_at_ms`
//! once, on the open row, inside a `BEGIN IMMEDIATE` transaction; the `0019`
//! update trigger refuses a second close, an edit, and a DELETE regardless.
//!
//! **`opened_at_ms` / `closed_at_ms` come from the injected `Clock`**, so a
//! `FakeClock` makes the record deterministic in tests (mirror `client_token_repo`).

use sqlx::SqlitePool;
use uuid::Uuid;

use crate::adapters::clock::SystemClock;
use crate::domain::{Clock, DataError, FreezeRecord, FreezeStoreError, OpenFreezeRequest};

/// The `SQLite` store for `certification_freeze` rows.
pub struct SqliteCertificationFreezeRepo<C: Clock> {
    pool: SqlitePool,
    clock: C,
}

impl SqliteCertificationFreezeRepo<SystemClock> {
    /// The production constructor: the wall-clock [`SystemClock`].
    #[must_use]
    pub fn new(pool: SqlitePool) -> SqliteCertificationFreezeRepo<SystemClock> {
        SqliteCertificationFreezeRepo {
            pool,
            clock: SystemClock,
        }
    }
}

impl<C: Clock> SqliteCertificationFreezeRepo<C> {
    /// The test/injection seam: supply a [`Clock`] so `opened_at_ms` /
    /// `closed_at_ms` are deterministic (mirror `SqliteClientTokenRepo::with_deps`).
    #[must_use]
    pub fn with_deps(pool: SqlitePool, clock: C) -> SqliteCertificationFreezeRepo<C> {
        SqliteCertificationFreezeRepo { pool, clock }
    }

    /// The OPEN freeze, or `Ok(None)` when none is open — the guard's read.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] when the read fails.
    pub async fn open_freeze(&self) -> Result<Option<FreezeRecord>, DataError> {
        let row = sqlx::query_as!(
            FreezeRow,
            "SELECT id, holdout_start_ms, h, alpha, holdout_test, opened_at_ms, closed_at_ms \
             FROM certification_freeze WHERE closed_at_ms IS NULL",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("read open certification_freeze: {e}")))?;
        row.map(freeze_record).transpose()
    }

    /// Open a freeze: validate the budget and F1's two laws, then write the one
    /// row and return it.
    ///
    /// # Errors
    ///
    /// [`FreezeStoreError::HOutOfRange`] when `h` is outside `1..=12`;
    /// [`FreezeStoreError::FreezeOpen`] when a freeze is already open;
    /// [`FreezeStoreError::HoldoutStartNotAfterLastClose`] when the holdout
    /// would start at or before every earlier close; [`FreezeStoreError::Db`]
    /// on a database failure.
    pub async fn open(
        &self,
        request: &OpenFreezeRequest,
    ) -> Result<FreezeRecord, FreezeStoreError> {
        if !(1..=12).contains(&request.h) {
            return Err(FreezeStoreError::HOutOfRange { h: request.h });
        }
        let opened_at_ms = self.clock.now_ms();
        let id = Uuid::new_v4().to_string();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| DataError::Db(format!("begin freeze tx: {e}")))?;

        // Law 1, named: at most one open freeze (the partial unique index backs
        // it at the schema).
        let open = sqlx::query!(
            "SELECT opened_at_ms FROM certification_freeze WHERE closed_at_ms IS NULL LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| DataError::Db(format!("freeze open pre-check: {e}")))?;
        if let Some(open) = open {
            return Err(FreezeStoreError::FreezeOpen {
                opened_at_ms: open.opened_at_ms,
            });
        }

        // Law 2, named: a new holdout starts strictly after every earlier close
        // (F1). MAX ignores the NULLs of a never-closed history.
        let last_close: Option<i64> =
            sqlx::query_scalar!("SELECT MAX(closed_at_ms) FROM certification_freeze")
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| DataError::Db(format!("freeze last-close pre-check: {e}")))?;
        if let Some(last_closed_at_ms) = last_close
            && request.holdout_start_ms <= last_closed_at_ms
        {
            return Err(FreezeStoreError::HoldoutStartNotAfterLastClose {
                holdout_start_ms: request.holdout_start_ms,
                last_closed_at_ms,
            });
        }

        let h = i64::from(request.h);
        sqlx::query!(
            "INSERT INTO certification_freeze \
             (id, holdout_start_ms, h, alpha, holdout_test, opened_at_ms, closed_at_ms) \
             VALUES (?, ?, ?, ?, ?, ?, NULL)",
            id,
            request.holdout_start_ms,
            h,
            request.alpha,
            request.holdout_test,
            opened_at_ms,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| DataError::Db(format!("insert certification_freeze: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| DataError::Db(format!("commit freeze open: {e}")))?;
        Ok(FreezeRecord {
            id,
            holdout_start_ms: request.holdout_start_ms,
            h: request.h,
            alpha: request.alpha.clone(),
            holdout_test: request.holdout_test.clone(),
            opened_at_ms,
            closed_at_ms: None,
        })
    }

    /// Close the open freeze: write `closed_at_ms` once and return the closed
    /// record.
    ///
    /// # Errors
    ///
    /// [`FreezeStoreError::NoOpenFreeze`] when no freeze is open;
    /// [`FreezeStoreError::Db`] on a database failure.
    pub async fn close(&self) -> Result<FreezeRecord, FreezeStoreError> {
        let closed_at_ms = self.clock.now_ms();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| DataError::Db(format!("begin close-freeze tx: {e}")))?;

        let open = sqlx::query_as!(
            FreezeRow,
            "SELECT id, holdout_start_ms, h, alpha, holdout_test, opened_at_ms, closed_at_ms \
             FROM certification_freeze WHERE closed_at_ms IS NULL",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| DataError::Db(format!("close-freeze lookup: {e}")))?;
        let Some(open) = open else {
            return Err(FreezeStoreError::NoOpenFreeze);
        };

        let applied = sqlx::query!(
            "UPDATE certification_freeze SET closed_at_ms = ? \
             WHERE id = ? AND closed_at_ms IS NULL",
            closed_at_ms,
            open.id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| DataError::Db(format!("close certification_freeze: {e}")))?;
        if applied.rows_affected() != 1 {
            // The update trigger refused or another writer closed it first —
            // the named refusal is the same either way.
            return Err(FreezeStoreError::NoOpenFreeze);
        }

        tx.commit()
            .await
            .map_err(|e| DataError::Db(format!("commit close-freeze: {e}")))?;
        let mut record = freeze_record(open)?;
        record.closed_at_ms = Some(closed_at_ms);
        Ok(record)
    }
}

/// One `query!` row → the domain record (`h` narrows back to `u8`; the schema
/// CHECK is the range, so a row outside it is corruption and refuses here).
fn freeze_record(row: FreezeRow) -> Result<FreezeRecord, DataError> {
    let h = u8::try_from(row.h)
        .map_err(|_| DataError::Db(format!("certification_freeze h {} is not a u8", row.h)))?;
    Ok(FreezeRecord {
        id: row.id,
        holdout_start_ms: row.holdout_start_ms,
        h,
        alpha: row.alpha,
        holdout_test: row.holdout_test,
        opened_at_ms: row.opened_at_ms,
        closed_at_ms: row.closed_at_ms,
    })
}

/// The columns every `certification_freeze` read selects — the row shape behind
/// [`freeze_record`]. A plain struct so the mapping stays one place.
struct FreezeRow {
    id: String,
    holdout_start_ms: i64,
    h: i64,
    alpha: String,
    holdout_test: String,
    opened_at_ms: i64,
    closed_at_ms: Option<i64>,
}
