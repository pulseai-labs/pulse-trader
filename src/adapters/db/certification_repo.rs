//! The `SQLite` adapter for the `certification` table (r4.s1.w5, G7/C1/C5).
//!
//! This is the ONLY place `query!` macros for this table live (`sqlx` is
//! confined to `adapters::db`, mirror `certification_freeze_repo.rs`); the
//! committed `.sqlx/` offline cache is keyed to the macros here (refresh with
//! `just prepare` under sqlx-cli `=0.8.6`).
//!
//! **One write path, one law.** `insert` is the only writer: it mints the id,
//! takes `created_at` from the injected [`Clock`], derives
//! `hypothesis_index = MAX(hypothesis_index) + 1` **inside one
//! `BEGIN IMMEDIATE` transaction**, reads the freeze's own `h` and closure state in that same
//! transaction and writes the row only when the derived index is within the
//! budget — so a concurrent second call waits for the write lock, sees its
//! predecessor's row and **refuses by name**
//! ([`DataError::HypothesisBudgetSpent`]) rather than minting `H + 1`. The
//! backstops a raw INSERT runs into are `0020`'s unique index (a duplicate
//! position) and its budget trigger (an index above `h`); they are not the
//! mechanism. The `certified` cell is derived from the draft's halves; the
//! schema's CHECK refuses a raw INSERT that disagrees.
//!
//! **Records are create + read only.** `0020`'s `BEFORE UPDATE` / `BEFORE
//! DELETE` triggers refuse an edit and a delete alike; this adapter offers no
//! method that would attempt one.
//!
//! **The two data-version columns are JSON**, the `0006` `backtest_run.inputs`
//! precedent: the nested `{primary, htf?, d1?}` selection shape round-trips
//! through serde with the timeframes inside it, so "per timeframe" is recorded
//! rather than implied by a column name.
//!
//! NO `#[derive(Debug)]` on the repo struct: the `C: Clock` carries no `Debug`
//! bound (mirror `SqliteClientTokenRepo`).

use chrono::{DateTime, SecondsFormat};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::adapters::clock::SystemClock;
use crate::adapters::db::backtest_run_repo::parse_decimal;
use crate::domain::backtest::WalkForwardRunId;
use crate::domain::certification::{CertificationDraft, CertificationInputs, CertificationRecord};
use crate::domain::strategy::VersionId;
use crate::domain::{Clock, DataError, Pair};

/// The `SQLite` store for `certification` rows.
///
/// Constructed from a [`SqlitePool`] (cloned from `Db::pool()`), with an
/// injected [`Clock`] for the `created_at` it writes.
pub struct SqliteCertificationRepo<C: Clock> {
    pool: SqlitePool,
    clock: C,
}

impl SqliteCertificationRepo<SystemClock> {
    /// The production constructor: the wall-clock [`SystemClock`].
    #[must_use]
    pub fn new(pool: SqlitePool) -> SqliteCertificationRepo<SystemClock> {
        SqliteCertificationRepo {
            pool,
            clock: SystemClock,
        }
    }
}

impl<C: Clock> SqliteCertificationRepo<C> {
    /// The test/injection seam: supply a [`Clock`] so `created_at` is
    /// deterministic (mirror `SqliteCertificationFreezeRepo::with_deps`).
    #[must_use]
    pub fn with_deps(pool: SqlitePool, clock: C) -> SqliteCertificationRepo<C> {
        SqliteCertificationRepo { pool, clock }
    }

    /// The current timestamp, sourced from the injected [`Clock`], as an
    /// RFC3339 millisecond UTC string for the `created_at` column.
    fn now_rfc3339(&self) -> Result<String, DataError> {
        let now_ms = self.clock.now_ms();
        let dt = DateTime::from_timestamp_millis(now_ms).ok_or_else(|| {
            DataError::Db(format!("clock.now_ms() {now_ms} is out of DateTime range"))
        })?;
        Ok(dt.to_rfc3339_opts(SecondsFormat::Millis, true))
    }

    /// The read behind every public method: one row by id, decoded.
    async fn fetch<'e, E>(
        &self,
        executor: E,
        id: &str,
    ) -> Result<Option<CertificationRecord>, DataError>
    where
        E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
    {
        let row = sqlx::query!(
            r#"SELECT
                 id                         AS "id!: String",
                 version_id                 AS "version_id!: String",
                 freeze_id                  AS "freeze_id!: String",
                 hypothesis_index           AS "hypothesis_index!: i64",
                 rule                       AS "rule!: String",
                 pair                       AS "pair!: String",
                 search_walk_forward_run_id AS "search_walk_forward_run_id!: String",
                 search_pass                AS "search_pass!: i64",
                 holdout_start_ms           AS "holdout_start_ms!: i64",
                 holdout_end_ms             AS "holdout_end_ms!: i64",
                 holdout_n                  AS "holdout_n!: i64",
                 holdout_mean_r             AS "holdout_mean_r!: String",
                 holdout_z                  AS "holdout_z!: f64",
                 holdout_lower_bound        AS "holdout_lower_bound!: f64",
                 holdout_passes             AS "holdout_passes!: i64",
                 certified                  AS "certified!: i64",
                 search_inputs              AS "search_inputs!: String",
                 holdout_inputs             AS "holdout_inputs!: String",
                 engine_fingerprint         AS "engine_fingerprint!: String",
                 created_at                 AS "created_at!: String",
                 called_by                  AS "called_by!: String"
               FROM certification WHERE id = ?1"#,
            id,
        )
        .fetch_optional(executor)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        match row {
            None => Ok(None),
            Some(r) => Ok(Some(certification_record(CertificationRow {
                id: r.id,
                version_id: r.version_id,
                freeze_id: r.freeze_id,
                hypothesis_index: r.hypothesis_index,
                rule: r.rule,
                pair: r.pair,
                search_walk_forward_run_id: r.search_walk_forward_run_id,
                search_pass: r.search_pass,
                holdout_start_ms: r.holdout_start_ms,
                holdout_end_ms: r.holdout_end_ms,
                holdout_n: r.holdout_n,
                holdout_mean_r: r.holdout_mean_r,
                holdout_z: r.holdout_z,
                holdout_lower_bound: r.holdout_lower_bound,
                holdout_passes: r.holdout_passes,
                certified: r.certified,
                search_inputs: r.search_inputs,
                holdout_inputs: r.holdout_inputs,
                engine_fingerprint: r.engine_fingerprint,
                created_at: r.created_at,
                called_by: r.called_by,
            })?)),
        }
    }
}

impl<C: Clock + Send + Sync> crate::domain::CertificationRepository for SqliteCertificationRepo<C> {
    async fn list_for_version(
        &self,
        version_id: &VersionId,
    ) -> Result<Vec<CertificationRecord>, DataError> {
        let version_id_str = version_id.as_str();
        let rows = sqlx::query!(
            r#"SELECT
                 certification.id           AS "id!: String",
                 version_id                 AS "version_id!: String",
                 freeze_id                  AS "freeze_id!: String",
                 hypothesis_index           AS "hypothesis_index!: i64",
                 rule                       AS "rule!: String",
                 pair                       AS "pair!: String",
                 search_walk_forward_run_id AS "search_walk_forward_run_id!: String",
                 search_pass                AS "search_pass!: i64",
                 certification.holdout_start_ms AS "holdout_start_ms!: i64",
                 holdout_end_ms             AS "holdout_end_ms!: i64",
                 holdout_n                  AS "holdout_n!: i64",
                 holdout_mean_r             AS "holdout_mean_r!: String",
                 holdout_z                  AS "holdout_z!: f64",
                 holdout_lower_bound        AS "holdout_lower_bound!: f64",
                 holdout_passes             AS "holdout_passes!: i64",
                 certified                  AS "certified!: i64",
                 search_inputs              AS "search_inputs!: String",
                 holdout_inputs             AS "holdout_inputs!: String",
                 engine_fingerprint         AS "engine_fingerprint!: String",
                 created_at                 AS "created_at!: String",
                 called_by                  AS "called_by!: String"
               FROM certification
               JOIN certification_freeze ON certification_freeze.id = certification.freeze_id
               WHERE version_id = ?1
               ORDER BY certification_freeze.opened_at_ms DESC, hypothesis_index DESC"#,
            version_id_str,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        rows.into_iter()
            .map(|r| {
                certification_record(CertificationRow {
                    id: r.id,
                    version_id: r.version_id,
                    freeze_id: r.freeze_id,
                    hypothesis_index: r.hypothesis_index,
                    rule: r.rule,
                    pair: r.pair,
                    search_walk_forward_run_id: r.search_walk_forward_run_id,
                    search_pass: r.search_pass,
                    holdout_start_ms: r.holdout_start_ms,
                    holdout_end_ms: r.holdout_end_ms,
                    holdout_n: r.holdout_n,
                    holdout_mean_r: r.holdout_mean_r,
                    holdout_z: r.holdout_z,
                    holdout_lower_bound: r.holdout_lower_bound,
                    holdout_passes: r.holdout_passes,
                    certified: r.certified,
                    search_inputs: r.search_inputs,
                    holdout_inputs: r.holdout_inputs,
                    engine_fingerprint: r.engine_fingerprint,
                    created_at: r.created_at,
                    called_by: r.called_by,
                })
            })
            .collect()
    }

    async fn latest_certified(
        &self,
        version_id: &VersionId,
    ) -> Result<Option<CertificationRecord>, DataError> {
        let version_id_str = version_id.as_str();
        let row = sqlx::query!(
            r#"SELECT
                 certification.id           AS "id!: String",
                 version_id                 AS "version_id!: String",
                 freeze_id                  AS "freeze_id!: String",
                 hypothesis_index           AS "hypothesis_index!: i64",
                 rule                       AS "rule!: String",
                 pair                       AS "pair!: String",
                 search_walk_forward_run_id AS "search_walk_forward_run_id!: String",
                 search_pass                AS "search_pass!: i64",
                 certification.holdout_start_ms AS "holdout_start_ms!: i64",
                 holdout_end_ms             AS "holdout_end_ms!: i64",
                 holdout_n                  AS "holdout_n!: i64",
                 holdout_mean_r             AS "holdout_mean_r!: String",
                 holdout_z                  AS "holdout_z!: f64",
                 holdout_lower_bound        AS "holdout_lower_bound!: f64",
                 holdout_passes             AS "holdout_passes!: i64",
                 certified                  AS "certified!: i64",
                 search_inputs              AS "search_inputs!: String",
                 holdout_inputs             AS "holdout_inputs!: String",
                 engine_fingerprint         AS "engine_fingerprint!: String",
                 created_at                 AS "created_at!: String",
                 called_by                  AS "called_by!: String"
               FROM certification
               JOIN certification_freeze ON certification_freeze.id = certification.freeze_id
               WHERE version_id = ?1 AND certified = 1
               ORDER BY certification_freeze.opened_at_ms DESC, hypothesis_index DESC LIMIT 1"#,
            version_id_str,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        match row {
            None => Ok(None),
            Some(r) => Ok(Some(certification_record(CertificationRow {
                id: r.id,
                version_id: r.version_id,
                freeze_id: r.freeze_id,
                hypothesis_index: r.hypothesis_index,
                rule: r.rule,
                pair: r.pair,
                search_walk_forward_run_id: r.search_walk_forward_run_id,
                search_pass: r.search_pass,
                holdout_start_ms: r.holdout_start_ms,
                holdout_end_ms: r.holdout_end_ms,
                holdout_n: r.holdout_n,
                holdout_mean_r: r.holdout_mean_r,
                holdout_z: r.holdout_z,
                holdout_lower_bound: r.holdout_lower_bound,
                holdout_passes: r.holdout_passes,
                certified: r.certified,
                search_inputs: r.search_inputs,
                holdout_inputs: r.holdout_inputs,
                engine_fingerprint: r.engine_fingerprint,
                created_at: r.created_at,
                called_by: r.called_by,
            })?)),
        }
    }

    async fn count_for_freeze(&self, freeze_id: &str) -> Result<u32, DataError> {
        let count = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM certification WHERE freeze_id = ?1"#,
            freeze_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;
        u32::try_from(count)
            .map_err(|e| DataError::Db(format!("certification count {count} exceeds u32: {e}")))
    }

    async fn insert(&self, draft: &CertificationDraft) -> Result<CertificationRecord, DataError> {
        let id = Uuid::new_v4().to_string();
        let created_at = self.now_rfc3339()?;
        let search_inputs = encode_inputs("search_inputs", &draft.search_inputs)?;
        let holdout_inputs = encode_inputs("holdout_inputs", &draft.holdout_inputs)?;
        let search_pass = i64::from(draft.search_pass);
        let holdout_passes = i64::from(draft.holdout_passes);
        let certified = i64::from(draft.certified());
        // Bound into locals so the `query!` bind list reads as one shape (the
        // `client_token_repo` precedent) and no field is moved out of `draft`.
        let version_id_str = draft.version_id.as_str().to_owned();
        let freeze_id = draft.freeze_id.clone();
        let rule = draft.rule.clone();
        let pair_symbol = draft.pair.as_str().to_owned();
        let search_run_id = draft.search_walk_forward_run_id.as_str().to_owned();
        let holdout_n = i64::try_from(draft.holdout_n)
            .map_err(|e| DataError::Db(format!("holdout_n {}: {e}", draft.holdout_n)))?;
        let holdout_mean_r = draft.holdout_mean_r.to_string();
        let engine_fingerprint = draft.engine_fingerprint.clone();
        let called_by = draft.called_by.clone();

        // `BEGIN IMMEDIATE`, not a deferred `begin()`: the first statement inside
        // is the index-deriving READ, and in WAL two connections can take read
        // snapshots before either writes — the loser then cannot upgrade its
        // stale snapshot and fails with `SQLITE_BUSY_SNAPSHOT`, which
        // `busy_timeout` does NOT retry (it covers a held lock, not a moved
        // snapshot), losing an otherwise valid hypothesis to a scheduling
        // accident. Taking the write lock up front makes a concurrent call WAIT
        // for the lock, which the timeout does cover — and then read the index
        // its predecessor actually wrote. Same rule `save_walk_forward_run`
        // states.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        let next = sqlx::query_scalar!(
            r#"SELECT COALESCE(MAX(hypothesis_index), 0) + 1 AS "next!: i64"
               FROM certification WHERE freeze_id = ?1"#,
            freeze_id,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        // The budget law, decided here INSIDE the transaction (close R1): the
        // freeze's own closure state and `h`, read from the row the record's key points
        // at — never from a caller-supplied value — refuses the write before it
        // mints an index past the budget. A missing freeze row (None) falls
        // through to the INSERT's foreign key, which raises exactly as it did
        // before this check existed.
        let freeze = sqlx::query!(
            r#"SELECT h AS "h!: i64", closed_at_ms FROM certification_freeze WHERE id = ?1"#,
            freeze_id,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;
        if let Some(freeze) = freeze {
            if freeze.closed_at_ms.is_some() {
                return Err(DataError::CertificationFreezeClosed);
            }
            let h = u8::try_from(freeze.h)
                .map_err(|e| DataError::Db(format!("certification_freeze.h {}: {e}", freeze.h)))?;
            if next > i64::from(h) {
                return Err(DataError::HypothesisBudgetSpent { h });
            }
        }

        let hypothesis_index = u32::try_from(next).map_err(|e| {
            DataError::Db(format!(
                "certification index {next} under freeze `{freeze_id}` exceeds u32: {e}"
            ))
        })?;

        let hypothesis_index_i64 = i64::from(hypothesis_index);
        sqlx::query!(
            "INSERT INTO certification \
             (id, version_id, freeze_id, hypothesis_index, rule, pair, \
              search_walk_forward_run_id, search_pass, holdout_start_ms, holdout_end_ms, \
              holdout_n, holdout_mean_r, holdout_z, holdout_lower_bound, holdout_passes, \
              certified, search_inputs, holdout_inputs, engine_fingerprint, created_at, called_by) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                     ?17, ?18, ?19, ?20, ?21)",
            id,
            version_id_str,
            freeze_id,
            hypothesis_index_i64,
            rule,
            pair_symbol,
            search_run_id,
            search_pass,
            draft.holdout_start_ms,
            draft.holdout_end_ms,
            holdout_n,
            holdout_mean_r,
            draft.holdout_z,
            draft.holdout_lower_bound,
            holdout_passes,
            certified,
            search_inputs,
            holdout_inputs,
            engine_fingerprint,
            created_at,
            called_by,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| DataError::Db(format!("insert certification: {e}")))?;

        let record = self
            .fetch(&mut *tx, &id)
            .await?
            .ok_or_else(|| DataError::Db("certification row vanished on read-back".to_owned()))?;
        tx.commit()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;
        Ok(record)
    }
}

/// One `query!` row → the domain record: the two integer booleans, the `u32`
/// index, the `usize` trade count, the Decimal-as-TEXT mean, and the two JSON
/// data-version columns. Every narrowing is checked — a row outside the
/// schema's ranges is corruption and refuses here.
fn certification_record(row: CertificationRow) -> Result<CertificationRecord, DataError> {
    Ok(CertificationRecord {
        id: row.id,
        version_id: VersionId::new(row.version_id),
        freeze_id: row.freeze_id,
        hypothesis_index: u32::try_from(row.hypothesis_index).map_err(|e| {
            DataError::Db(format!(
                "certification.hypothesis_index {}: {e}",
                row.hypothesis_index
            ))
        })?,
        rule: row.rule,
        pair: Pair::new(row.pair),
        search_walk_forward_run_id: WalkForwardRunId::new(row.search_walk_forward_run_id),
        search_pass: row.search_pass != 0,
        holdout_start_ms: row.holdout_start_ms,
        holdout_end_ms: row.holdout_end_ms,
        holdout_n: usize::try_from(row.holdout_n).map_err(|e| {
            DataError::Db(format!("certification.holdout_n {}: {e}", row.holdout_n))
        })?,
        holdout_mean_r: parse_decimal("certification.holdout_mean_r", &row.holdout_mean_r)?,
        holdout_z: row.holdout_z,
        holdout_lower_bound: row.holdout_lower_bound,
        holdout_passes: row.holdout_passes != 0,
        certified: row.certified != 0,
        search_inputs: decode_inputs("search_inputs", &row.search_inputs)?,
        holdout_inputs: decode_inputs("holdout_inputs", &row.holdout_inputs)?,
        engine_fingerprint: row.engine_fingerprint,
        created_at: row.created_at,
        called_by: row.called_by,
    })
}

/// The raw `certification` row shape the read path decodes.
struct CertificationRow {
    id: String,
    version_id: String,
    freeze_id: String,
    hypothesis_index: i64,
    rule: String,
    pair: String,
    search_walk_forward_run_id: String,
    search_pass: i64,
    holdout_start_ms: i64,
    holdout_end_ms: i64,
    holdout_n: i64,
    holdout_mean_r: String,
    holdout_z: f64,
    holdout_lower_bound: f64,
    holdout_passes: i64,
    certified: i64,
    search_inputs: String,
    holdout_inputs: String,
    engine_fingerprint: String,
    created_at: String,
    called_by: String,
}

/// Encode one side's selections for its JSON column (the `0006` precedent).
fn encode_inputs(column: &str, inputs: &CertificationInputs) -> Result<String, DataError> {
    serde_json::to_string(inputs)
        .map_err(|e| DataError::Db(format!("encode certification.{column}: {e}")))
}

/// Decode one side's selections, fail-closed: a column that no longer parses is
/// corruption, never a partially-recorded provenance.
fn decode_inputs(column: &str, raw: &str) -> Result<CertificationInputs, DataError> {
    serde_json::from_str(raw)
        .map_err(|e| DataError::Db(format!("malformed certification.{column} `{raw}`: {e}")))
}
