//! The `SQLite` adapter for the `example_mark` and `example_mark_withdrawal`
//! tables (r5.s3.w1, PDR-0037, ADR-0031).
//!
//! This is the ONLY place `query!` macros for these two tables live (`sqlx` is
//! confined to `adapters::db`, mirror `certification_repo.rs`); the committed
//! `.sqlx/` offline cache is keyed to the macros here (refresh with
//! `just prepare` under sqlx-cli `=0.8.6`).
//!
//! **One write path, and it is not here.** `insert` is the only method that
//! writes a mark, and the ONE caller SPINE G1 asks for is
//! `application::example_marks::write_mark`, whose first check is the holdout
//! fence. This adapter deliberately holds no fence of its own: a second copy of
//! the rule would be a second answer, and the two would drift on the first
//! tie-break anyone touched.
//!
//! **Records are create + read only.** `0021`'s `BEFORE UPDATE` / `BEFORE DELETE`
//! triggers refuse an edit and a delete on both tables; this adapter offers no
//! method that would attempt one. A withdraw APPENDS to
//! `example_mark_withdrawal`, whose `mark_id` PRIMARY KEY makes a second withdraw
//! unrepresentable rather than merely refused — the schema's own one-way door,
//! beside which the use case's named `already_withdrawn` is the readable half.
//!
//! **`created_at_ms` and `withdrawn_at_ms` come from the injected `Clock`**, so a
//! `FakeClock` makes a mark deterministic in tests (mirror
//! `SqliteCertificationFreezeRepo`).
//!
//! NO `#[derive(Debug)]` on the repo struct: the `C: Clock` carries no `Debug`
//! bound (mirror `SqliteClientTokenRepo`).

use sqlx::SqlitePool;
use uuid::Uuid;

use crate::adapters::clock::SystemClock;
use crate::domain::{
    Clock, DataError, ExampleMark, ExampleMarkDraft, MarkFilter, MarkNote, MarkSetup, MarkSide,
    MarkWriter, MarkWriterKind, Pair, StoredMark, Timeframe,
};

/// The `SQLite` store for example-trade marks.
///
/// Constructed from a [`SqlitePool`] (cloned from `Db::pool()`), with an injected
/// [`Clock`] for the timestamps it writes.
pub struct SqliteExampleMarkRepo<C: Clock> {
    pool: SqlitePool,
    clock: C,
}

impl SqliteExampleMarkRepo<SystemClock> {
    /// The production constructor: the wall-clock [`SystemClock`].
    #[must_use]
    pub fn new(pool: SqlitePool) -> SqliteExampleMarkRepo<SystemClock> {
        SqliteExampleMarkRepo {
            pool,
            clock: SystemClock,
        }
    }
}

impl<C: Clock> SqliteExampleMarkRepo<C> {
    /// The test/injection seam: supply a [`Clock`] so `created_at_ms` /
    /// `withdrawn_at_ms` are deterministic (mirror `SqliteCertificationFreezeRepo`).
    #[must_use]
    pub fn with_deps(pool: SqlitePool, clock: C) -> SqliteExampleMarkRepo<C> {
        SqliteExampleMarkRepo { pool, clock }
    }
}

/// One `example_mark` row, exactly as `SQLite` hands it back.
struct MarkRow {
    id: String,
    pair: String,
    timeframe: String,
    bar_open_ms: i64,
    side: String,
    setup: String,
    note: Option<String>,
    writer_kind: String,
    writer_name: String,
    data_version: String,
    created_at_ms: i64,
}

/// The `Binance` interval string back to its [`Timeframe`] — the inverse of
/// `Timeframe::binance_interval`, fail-closed on anything the column cannot hold
/// (the `0021` CHECK keeps the column to the three known spellings; a value that
/// is not one of them is a corrupt row and reads as one).
fn parse_timeframe(raw: &str) -> Result<Timeframe, DataError> {
    match raw {
        "15m" => Ok(Timeframe::M15),
        "4h" => Ok(Timeframe::H4),
        "1d" => Ok(Timeframe::D1),
        other => Err(DataError::Db(format!(
            "example_mark.timeframe holds an unknown interval `{other}`"
        ))),
    }
}

/// A stored row as the domain value, validating every enum the way the write
/// path did. A row that no longer parses is an ERROR, never a default: a mark
/// whose side nobody can name is not a mark this store may hand to a reader.
fn mark_from_row(row: MarkRow) -> Result<ExampleMark, DataError> {
    Ok(ExampleMark {
        id: row.id,
        pair: Pair::parse(row.pair)
            .map_err(|e| DataError::Db(format!("example_mark.pair: {e}")))?,
        timeframe: parse_timeframe(&row.timeframe)?,
        bar_open_ms: row.bar_open_ms,
        side: MarkSide::parse(&row.side)
            .map_err(|e| DataError::Db(format!("example_mark.side: {e}")))?,
        setup: MarkSetup::parse(&row.setup)
            .map_err(|e| DataError::Db(format!("example_mark.setup: {e}")))?,
        note: match row.note {
            Some(note) => MarkNote::parse(&note)
                .map_err(|e| DataError::Db(format!("example_mark.note: {e}")))?,
            None => None,
        },
        writer: MarkWriter {
            kind: MarkWriterKind::parse(&row.writer_kind)
                .map_err(|e| DataError::Db(format!("example_mark.writer_kind: {e}")))?,
            name: row.writer_name,
        },
        data_version: row.data_version,
        created_at_ms: row.created_at_ms,
    })
}

impl<C: Clock + Send + Sync> crate::domain::ExampleMarkRepository for SqliteExampleMarkRepo<C> {
    async fn insert(&self, draft: &ExampleMarkDraft) -> Result<ExampleMark, DataError> {
        let id = Uuid::new_v4().to_string();
        let created_at_ms = self.clock.now_ms();
        // The bound values are named HERE, not inline: sqlx's macro borrows every
        // argument for the life of the statement it builds, and an inline
        // `draft.pair.as_str()` is a temporary borrow that dies before the
        // statement does.
        let pair = draft.pair.as_str();
        let timeframe = draft.timeframe.binance_interval();
        let side = draft.side.as_str();
        let setup = draft.setup.as_str();
        let note = draft.note.as_ref().map(MarkNote::as_str);
        let writer_kind = draft.writer.kind.as_str();
        let writer_name = draft.writer.name.as_str();
        let data_version = draft.data_version.as_str();
        sqlx::query!(
            "INSERT INTO example_mark \
             (id, pair, timeframe, bar_open_ms, side, setup, note, writer_kind, writer_name, \
              data_version, created_at_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            id,
            pair,
            timeframe,
            draft.bar_open_ms,
            side,
            setup,
            note,
            writer_kind,
            writer_name,
            data_version,
            created_at_ms,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("insert example_mark: {e}")))?;

        Ok(ExampleMark {
            id,
            pair: draft.pair.clone(),
            timeframe: draft.timeframe,
            bar_open_ms: draft.bar_open_ms,
            side: draft.side,
            setup: draft.setup.clone(),
            note: draft.note.clone(),
            writer: draft.writer.clone(),
            data_version: draft.data_version.clone(),
            created_at_ms,
        })
    }

    async fn get(&self, mark_id: &str) -> Result<Option<StoredMark>, DataError> {
        let row = sqlx::query_as!(
            MarkRow,
            "SELECT m.id, m.pair, m.timeframe, m.bar_open_ms, m.side, m.setup, m.note, \
                    m.writer_kind, m.writer_name, m.data_version, m.created_at_ms \
             FROM example_mark m WHERE m.id = ?1",
            mark_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("read example_mark {mark_id}: {e}")))?;

        let Some(row) = row else {
            return Ok(None);
        };
        let withdrawn_at_ms: Option<i64> = sqlx::query_scalar!(
            "SELECT withdrawn_at_ms FROM example_mark_withdrawal WHERE mark_id = ?1",
            mark_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("read example_mark_withdrawal {mark_id}: {e}")))?;

        Ok(Some(StoredMark {
            mark: mark_from_row(row)?,
            withdrawn_at_ms,
        }))
    }

    async fn list(&self, filter: &MarkFilter) -> Result<Vec<ExampleMark>, DataError> {
        // Each member narrows only when it is present, and the join drops a
        // withdrawn mark by construction: the LEFT JOIN's NULL test IS the
        // "not withdrawn" predicate, so a mark taken back is not a mark the
        // reader still sees.
        let pair = filter.pair.as_ref().map(Pair::as_str);
        let timeframe = filter.timeframe.map(Timeframe::binance_interval);
        let setup = filter.setup.as_deref();
        let rows = sqlx::query_as!(
            MarkRow,
            "SELECT m.id, m.pair, m.timeframe, m.bar_open_ms, m.side, m.setup, m.note, \
                    m.writer_kind, m.writer_name, m.data_version, m.created_at_ms \
             FROM example_mark m \
             LEFT JOIN example_mark_withdrawal w ON w.mark_id = m.id \
             WHERE (?1 IS NULL OR m.pair = ?1) \
               AND (?2 IS NULL OR m.timeframe = ?2) \
               AND (?3 IS NULL OR m.setup = ?3) \
               AND w.mark_id IS NULL \
             ORDER BY m.bar_open_ms ASC, m.id ASC",
            pair,
            timeframe,
            setup,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("list example_mark: {e}")))?;

        rows.into_iter().map(mark_from_row).collect()
    }

    async fn withdraw(&self, mark_id: &str, actor: &MarkWriter) -> Result<(), DataError> {
        let withdrawn_at_ms = self.clock.now_ms();
        // Named for the same reason `insert`'s are: an inline borrow is a
        // temporary sqlx's statement outlives.
        let withdrawn_by_kind = actor.kind.as_str();
        let withdrawn_by_name = actor.name.as_str();
        sqlx::query!(
            "INSERT INTO example_mark_withdrawal \
             (mark_id, withdrawn_at_ms, withdrawn_by_kind, withdrawn_by_name) \
             VALUES (?, ?, ?, ?)",
            mark_id,
            withdrawn_at_ms,
            withdrawn_by_kind,
            withdrawn_by_name,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("insert example_mark_withdrawal {mark_id}: {e}")))?;
        Ok(())
    }
}
