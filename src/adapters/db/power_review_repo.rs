//! The `SQLite` adapter for the `power_review` table (r5.s5.w1, PDR-0040/ADR-0032).
//!
//! This is the ONLY place `query!` macros for this table live (`sqlx` is confined to `adapters::db`, mirror
//! `certification_repo.rs`); the committed `.sqlx/` offline cache is keyed to the macros here (refresh with
//! `just prepare` under sqlx-cli `=0.8.6`).
//!
//! **One write path, no law to keep.** `insert` is the only writer: it mints the id, takes `created_at` from
//! the injected [`Clock`] and writes one row. Unlike the certification's budget, a review spends nothing and
//! changes no counter, so there is no transaction and no write-time check — the row is a record of a read.
//! **A review is created once and read only**: `0022`'s `BEFORE UPDATE` / `BEFORE DELETE` triggers refuse an
//! edit and a delete alike, and this adapter offers no method that would attempt one.
//!
//! **The two read shapes share one decode.** `fetch` names every column once; `latest_for` and
//! `list_for_version` select the ids they want and go through the same `fetch`, so the row → record mapping
//! exists once. Both lists are a handful of rows (one per review), so the id-then-fetch shape costs nothing
//! measurable and cannot drift.
//!
//! **Insertion order decides, never `created_at`.** `latest_for` and `list_for_version` order by
//! `rowid DESC`: the row id is a random v4 UUID and `created_at` comes from a clock that can tie at one
//! millisecond or step backwards, so neither can say which review is newest. `rowid` is the table's
//! insertion order, which is what "the newest review" means here — the row written last.
//!
//! NO `#[derive(Debug)]` on the repo struct: the `C: Clock` carries no `Debug` bound (mirror
//! `SqliteCertificationRepo`).

use chrono::{DateTime, SecondsFormat};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::adapters::clock::SystemClock;
use crate::adapters::db::certification_repo::{decode_inputs, encode_inputs};
use crate::domain::backtest::WalkForwardRunId;
use crate::domain::power_review::{PowerReviewDraft, PowerReviewRecord};
use crate::domain::strategy::VersionId;
use crate::domain::{Clock, DataError, Pair};

/// The `SQLite` store for `power_review` rows.
///
/// Constructed from a [`SqlitePool`] (cloned from `Db::pool()`), with an injected [`Clock`] for the
/// `created_at` it writes.
pub struct SqlitePowerReviewRepo<C: Clock> {
    pool: SqlitePool,
    clock: C,
}

impl SqlitePowerReviewRepo<SystemClock> {
    /// The production constructor: the wall-clock [`SystemClock`].
    #[must_use]
    pub fn new(pool: SqlitePool) -> SqlitePowerReviewRepo<SystemClock> {
        SqlitePowerReviewRepo {
            pool,
            clock: SystemClock,
        }
    }
}

impl<C: Clock> SqlitePowerReviewRepo<C> {
    /// The test/injection seam: supply a [`Clock`] so `created_at` is deterministic.
    #[must_use]
    pub fn with_deps(pool: SqlitePool, clock: C) -> SqlitePowerReviewRepo<C> {
        SqlitePowerReviewRepo { pool, clock }
    }

    /// The current timestamp, sourced from the injected [`Clock`], as an RFC3339 millisecond UTC string for
    /// the `created_at` column.
    fn now_rfc3339(&self) -> Result<String, DataError> {
        let now_ms = self.clock.now_ms();
        let dt = DateTime::from_timestamp_millis(now_ms).ok_or_else(|| {
            DataError::Db(format!("clock.now_ms() {now_ms} is out of DateTime range"))
        })?;
        Ok(dt.to_rfc3339_opts(SecondsFormat::Millis, true))
    }

    /// The one decode path: one row by id, mapped to the domain record.
    async fn fetch<'e, E>(
        &self,
        executor: E,
        id: &str,
    ) -> Result<Option<PowerReviewRecord>, DataError>
    where
        E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
    {
        let row = sqlx::query!(
            r#"SELECT
                 id                         AS "id!: String",
                 version_id                 AS "version_id!: String",
                 freeze_id                  AS "freeze_id!: String",
                 pair                       AS "pair!: String",
                 h                          AS "h!: i64",
                 z                          AS "z!: f64",
                 holdout_start_ms           AS "holdout_start_ms!: i64",
                 holdout_end_ms             AS "holdout_end_ms!: i64",
                 holdout_n                  AS "holdout_n!: i64",
                 holdout_inputs             AS "holdout_inputs!: String",
                 search_walk_forward_run_id AS "search_walk_forward_run_id!: String",
                 search_pass                AS "search_pass!: i64",
                 search_n                   AS "search_n!: i64",
                 sigma_r                    AS "sigma_r!: String",
                 assumed_edge_r             AS "assumed_edge_r!: String",
                 bar                        AS "bar!: String",
                 min_holdout_trades         AS "min_holdout_trades!: i64",
                 pass_chance                AS "pass_chance!: f64",
                 allowed                    AS "allowed!: i64",
                 reason                     AS "reason?: String",
                 engine_fingerprint         AS "engine_fingerprint!: String",
                 called_by                  AS "called_by!: String",
                 created_at                 AS "created_at!: String"
               FROM power_review WHERE id = ?1"#,
            id,
        )
        .fetch_optional(executor)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        match row {
            None => Ok(None),
            Some(r) => Ok(Some(power_review_record(PowerReviewRow {
                id: r.id,
                version_id: r.version_id,
                freeze_id: r.freeze_id,
                pair: r.pair,
                h: r.h,
                z: r.z,
                holdout_start_ms: r.holdout_start_ms,
                holdout_end_ms: r.holdout_end_ms,
                holdout_n: r.holdout_n,
                holdout_inputs: r.holdout_inputs,
                search_walk_forward_run_id: r.search_walk_forward_run_id,
                search_pass: r.search_pass,
                search_n: r.search_n,
                sigma_r: r.sigma_r,
                assumed_edge_r: r.assumed_edge_r,
                bar: r.bar,
                min_holdout_trades: r.min_holdout_trades,
                pass_chance: r.pass_chance,
                allowed: r.allowed,
                reason: r.reason,
                engine_fingerprint: r.engine_fingerprint,
                called_by: r.called_by,
                created_at: r.created_at,
            })?)),
        }
    }
}

impl<C: Clock + Send + Sync> crate::domain::PowerReviewRepository for SqlitePowerReviewRepo<C> {
    async fn insert(&self, draft: &PowerReviewDraft) -> Result<PowerReviewRecord, DataError> {
        let id = Uuid::new_v4().to_string();
        let created_at = self.now_rfc3339()?;
        let holdout_inputs = encode_inputs("power_review.holdout_inputs", &draft.holdout_inputs)?;
        // Bound into locals so the `query!` bind list reads as one shape and no field is moved out of `draft`.
        let version_id_str = draft.version_id.as_str().to_owned();
        let freeze_id = draft.freeze_id.clone();
        let pair = draft.pair.as_str().to_owned();
        let search_run_id = draft.search_walk_forward_run_id.as_str().to_owned();
        let h = i64::from(draft.h);
        let holdout_n = i64::try_from(draft.holdout_n).map_err(|e| {
            DataError::Db(format!("power_review.holdout_n {}: {e}", draft.holdout_n))
        })?;
        let search_n = i64::try_from(draft.search_n)
            .map_err(|e| DataError::Db(format!("power_review.search_n {}: {e}", draft.search_n)))?;
        let min_holdout_trades = i64::try_from(draft.min_holdout_trades).map_err(|e| {
            DataError::Db(format!(
                "power_review.min_holdout_trades {}: {e}",
                draft.min_holdout_trades
            ))
        })?;
        let search_pass = i64::from(draft.search_pass);
        let allowed = i64::from(draft.allowed);
        let sigma_r = draft.sigma_r.clone();
        let assumed_edge_r = draft.assumed_edge_r.clone();
        let bar = draft.bar.clone();
        let reason = draft.reason.clone();
        let engine_fingerprint = draft.engine_fingerprint.clone();
        let called_by = draft.called_by.clone();

        // A plain INSERT: a review spends no hypothesis and keeps no counter, so there is nothing to do
        // under a write lock.
        sqlx::query!(
            "INSERT INTO power_review \
             (id, version_id, freeze_id, pair, h, z, holdout_start_ms, holdout_end_ms, holdout_n, \
              holdout_inputs, search_walk_forward_run_id, search_pass, search_n, sigma_r, assumed_edge_r, \
              bar, min_holdout_trades, pass_chance, allowed, reason, engine_fingerprint, called_by, \
              created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, \
                     ?19, ?20, ?21, ?22, ?23)",
            id,
            version_id_str,
            freeze_id,
            pair,
            h,
            draft.z,
            draft.holdout_start_ms,
            draft.holdout_end_ms,
            holdout_n,
            holdout_inputs,
            search_run_id,
            search_pass,
            search_n,
            sigma_r,
            assumed_edge_r,
            bar,
            min_holdout_trades,
            draft.pass_chance,
            allowed,
            reason,
            engine_fingerprint,
            called_by,
            created_at,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("insert power_review: {e}")))?;

        self.fetch(&self.pool, &id)
            .await?
            .ok_or_else(|| DataError::Db("power_review row vanished on read-back".to_owned()))
    }

    async fn latest_for(
        &self,
        version_id: &VersionId,
        pair: &Pair,
        freeze_id: &str,
    ) -> Result<Option<PowerReviewRecord>, DataError> {
        let version_id_str = version_id.as_str();
        let pair_str = pair.as_str();
        let id = sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM power_review
               WHERE version_id = ?1 AND pair = ?2 AND freeze_id = ?3
               ORDER BY rowid DESC LIMIT 1"#,
            version_id_str,
            pair_str,
            freeze_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;
        match id {
            None => Ok(None),
            Some(id) => self.fetch(&self.pool, &id).await,
        }
    }

    async fn list_for_version(
        &self,
        version_id: &VersionId,
    ) -> Result<Vec<PowerReviewRecord>, DataError> {
        let version_id_str = version_id.as_str();
        let ids = sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM power_review WHERE version_id = ?1
               ORDER BY rowid DESC"#,
            version_id_str,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;
        let mut records = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(record) = self.fetch(&self.pool, &id).await? {
                records.push(record);
            }
        }
        Ok(records)
    }
}

/// One `query!` row → the domain record: the two integer booleans, the `u8` budget, the `usize` counts, and
/// the JSON inputs. Every narrowing is checked — a row outside the schema's ranges is corruption and refuses
/// here.
fn power_review_record(row: PowerReviewRow) -> Result<PowerReviewRecord, DataError> {
    Ok(PowerReviewRecord {
        id: row.id,
        version_id: VersionId::new(row.version_id),
        freeze_id: row.freeze_id,
        pair: Pair::new(row.pair),
        h: u8::try_from(row.h)
            .map_err(|e| DataError::Db(format!("power_review.h {}: {e}", row.h)))?,
        z: row.z,
        holdout_start_ms: row.holdout_start_ms,
        holdout_end_ms: row.holdout_end_ms,
        holdout_n: usize::try_from(row.holdout_n)
            .map_err(|e| DataError::Db(format!("power_review.holdout_n {}: {e}", row.holdout_n)))?,
        holdout_inputs: decode_inputs("power_review.holdout_inputs", &row.holdout_inputs)?,
        search_walk_forward_run_id: WalkForwardRunId::new(row.search_walk_forward_run_id),
        search_pass: row.search_pass != 0,
        search_n: usize::try_from(row.search_n)
            .map_err(|e| DataError::Db(format!("power_review.search_n {}: {e}", row.search_n)))?,
        sigma_r: row.sigma_r,
        assumed_edge_r: row.assumed_edge_r,
        bar: row.bar,
        min_holdout_trades: usize::try_from(row.min_holdout_trades).map_err(|e| {
            DataError::Db(format!(
                "power_review.min_holdout_trades {}: {e}",
                row.min_holdout_trades
            ))
        })?,
        pass_chance: row.pass_chance,
        allowed: row.allowed != 0,
        reason: row.reason,
        engine_fingerprint: row.engine_fingerprint,
        called_by: row.called_by,
        created_at: row.created_at,
    })
}

/// The raw `power_review` row shape the read path decodes.
struct PowerReviewRow {
    id: String,
    version_id: String,
    freeze_id: String,
    pair: String,
    h: i64,
    z: f64,
    holdout_start_ms: i64,
    holdout_end_ms: i64,
    holdout_n: i64,
    holdout_inputs: String,
    search_walk_forward_run_id: String,
    search_pass: i64,
    search_n: i64,
    sigma_r: String,
    assumed_edge_r: String,
    bar: String,
    min_holdout_trades: i64,
    pass_chance: f64,
    allowed: i64,
    reason: Option<String>,
    engine_fingerprint: String,
    called_by: String,
    created_at: String,
}
