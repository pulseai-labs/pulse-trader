//! The `SQLite` adapter implementing the [`StrategyRepository`] port (VS-1.1.4
//! work-1.03, FR-4 / FR-11).
//!
//! This is the ONLY place `query!`/`query_as!` macros for the `strategy` /
//! `strategy_version` tables live (`sqlx` is confined to `adapters::db`); the
//! committed `.sqlx/` offline cache is keyed to the macros in this file.
//!
//! Two injected seams keep the load-path and the timestamp source testable
//! *now*, before a second real schema version or a wall-clock dependency exists:
//!
//! - **The DSL [`Migrator`] is a struct field** (gate-2 Q1). Every version
//!   write/read routes its `dsl_json` / `dsl_original` through
//!   `self.migrator.load(..)` — NEVER a raw `serde_json::from_str::<StrategyDsl>`
//!   (which is migration-UNAWARE per `migrate.rs`). `Migrator::v1()` has an empty
//!   registry, so a current-version document passes through untouched; the
//!   migrate-on-read half is exercised via an injected synthetic migration
//!   (AC-19).
//! - **The [`Clock`] port is a generic parameter** (`<C: Clock>`, gate-7 C1).
//!   Every `created_at` comes from `DateTime::from_timestamp_millis(clock.now_ms())`,
//!   never a bare `Utc::now()`, so provenance timestamps are reproducible (NFR-2).
//!
//! **Immutable versions (FR-4):** create + read only — there is no
//! `update_version` / `delete_version`. Immutability is structural in the port
//! (1.02) and enforced by the DB `BEFORE UPDATE` / `BEFORE DELETE` triggers
//! (1.01). Reads re-derive `.dsl` from the verbatim `dsl_original` (#19) and
//! reject a stored `version_hash` mismatch (audit-C5 re-derive defense).

use std::fmt::Write as _;

use chrono::{DateTime, SecondsFormat, Utc};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::adapters::clock::SystemClock;
use crate::domain::strategy::{
    AgentName, AgentSubmission, AgentSubmissionId, CreatedBy, Hypothesis, NewAgentSubmission,
    NewVersion, Strategy, StrategyId, StrategyVersion, VersionId,
};
use crate::domain::{Clock, DataError, Migrator, SchemaVersion, StrategyRepository, validate};

/// The `SQLite` [`StrategyRepository`](crate::domain::port::StrategyRepository)
/// adapter over `pulse.db`.
///
/// Constructed from a [`SqlitePool`] (cloned from `Db::pool()`). Carries the
/// injected DSL [`Migrator`] (load-path) and a [`Clock`] (timestamp source).
///
/// No `#[derive(Debug)]`: the domain [`Migrator`] is not `Debug` (its `apply`
/// fn-pointers carry no `Debug`), and `C: Clock` carries no `Debug` bound.
pub struct SqliteStrategyRepo<C: Clock> {
    pool: SqlitePool,
    migrator: Migrator,
    clock: C,
}

impl SqliteStrategyRepo<SystemClock> {
    /// The production constructor: the wall-clock [`SystemClock`] + the empty
    /// production [`Migrator::v1`] registry.
    #[must_use]
    pub fn new(pool: SqlitePool) -> SqliteStrategyRepo<SystemClock> {
        SqliteStrategyRepo {
            pool,
            migrator: Migrator::v1(),
            clock: SystemClock,
        }
    }
}

impl<C: Clock> SqliteStrategyRepo<C> {
    /// The test/injection seam: supply BOTH a [`Migrator`] (so the migrate path
    /// can carry a synthetic migration, AC-19) AND a [`Clock`] (so `created_at`
    /// is deterministic, AC-22).
    #[must_use]
    pub fn with_deps(pool: SqlitePool, migrator: Migrator, clock: C) -> SqliteStrategyRepo<C> {
        SqliteStrategyRepo {
            pool,
            migrator,
            clock,
        }
    }

    /// The current `created_at`, sourced from the injected [`Clock`] (gate-7 C1),
    /// serialized as an RFC3339 millisecond string for the `TEXT` column.
    fn now_rfc3339(&self) -> Result<(DateTime<Utc>, String), DataError> {
        let now_ms = self.clock.now_ms();
        let dt = DateTime::from_timestamp_millis(now_ms).ok_or_else(|| {
            DataError::Db(format!("clock.now_ms() {now_ms} is out of DateTime range"))
        })?;
        Ok((dt, dt.to_rfc3339_opts(SecondsFormat::Millis, true)))
    }
}

/// Parse an RFC3339 `created_at` `TEXT` column back into a `DateTime<Utc>`.
fn parse_created_at(s: &str) -> Result<DateTime<Utc>, DataError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| DataError::Db(format!("malformed created_at `{s}`: {e}")))
}

/// Deserialize a JSON-array `TEXT` column into a `Vec<String>`.
fn parse_json_str_array(s: &str) -> Result<Vec<String>, DataError> {
    serde_json::from_str(s).map_err(|e| DataError::Db(format!("malformed JSON array `{s}`: {e}")))
}

/// Parse the `created_by` `TEXT` column back into a [`CreatedBy`]. The column
/// stores the quoted JSON token (`"human"`), symmetric with the write
/// (`serde_json::to_string(&CreatedBy)`); see §4a-6.
fn parse_created_by(s: &str) -> Result<CreatedBy, DataError> {
    serde_json::from_str(s).map_err(|e| DataError::Db(format!("malformed created_by `{s}`: {e}")))
}

/// Parse the `dsl_schema_version` `TEXT` column into a [`SchemaVersion`].
fn parse_schema_version(s: &str) -> Result<SchemaVersion, DataError> {
    s.parse::<SchemaVersion>()
        .map_err(|e| DataError::Db(format!("malformed dsl_schema_version `{s}`: {e}")))
}

/// Length-prefixed string feed mirroring `store/version.rs::feed_str`: an 8-byte
/// big-endian length, then the UTF-8 bytes, so concatenation is unambiguous.
fn feed_str(hasher: &mut Sha256, s: &str) {
    hasher.update((s.len() as u64).to_be_bytes());
    hasher.update(s.as_bytes());
}

/// The per-version content-identity hash (ADR-0009, mirrors
/// `store/version.rs`'s length-prefixed SHA-256 discipline).
///
/// Feeds, in a FIXED order: `strategy_id`, then `parent` (a 1-byte present/absent
/// tag — `1` for `Some`, `0` for `None` — then the value, the empty string when
/// `None`), then `schema_version`, then `dsl_original`. Emits the FULL 64-char
/// lowercase hex digest (unlike version.rs's 16-char `data_version` truncation,
/// this is an integrity field). Scope is position-scoped (strategy + parent), not
/// pure-content (gate-7 C6).
///
/// `pub(crate)` since r1.s4.w4: the coach's accept transaction mints a child
/// version and must key it with the SAME hash this file computes, or the two
/// creation paths would disagree about a version's content identity.
pub(crate) fn version_hash(
    strategy_id: &str,
    parent: Option<&str>,
    schema_version: &str,
    dsl_original: &str,
) -> String {
    let mut hasher = Sha256::new();
    feed_str(&mut hasher, strategy_id);
    if let Some(p) = parent {
        hasher.update([1u8]);
        feed_str(&mut hasher, p);
    } else {
        hasher.update([0u8]);
        feed_str(&mut hasher, "");
    }
    feed_str(&mut hasher, schema_version);
    feed_str(&mut hasher, dsl_original);
    let digest = hasher.finalize();

    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in &digest {
        // `write!` to a String is infallible; the result is discarded.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

impl<C: Clock + Send + Sync> StrategyRepository for SqliteStrategyRepo<C> {
    async fn create_strategy(
        &self,
        name: &str,
        owner: Option<&str>,
        tags: &[String],
    ) -> Result<Strategy, DataError> {
        let id = Uuid::new_v4().to_string();
        let (_, created_at) = self.now_rfc3339()?;
        let tags_json = serde_json::to_string(tags).map_err(|e| DataError::Db(e.to_string()))?;

        sqlx::query!(
            "INSERT INTO strategy (id, name, tags, owner, pinned_version_id, archived, created_at) \
             VALUES (?1, ?2, ?3, ?4, NULL, 0, ?5)",
            id,
            name,
            tags_json,
            owner,
            created_at,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        self.get_strategy(&StrategyId::new(id))
            .await?
            .ok_or_else(|| DataError::Db("created strategy vanished on read-back".to_owned()))
    }

    async fn get_strategy(&self, id: &StrategyId) -> Result<Option<Strategy>, DataError> {
        let id_str = id.as_str();
        let row = sqlx::query!(
            r#"SELECT
                 id                AS "id!: String",
                 name              AS "name!: String",
                 tags              AS "tags!: String",
                 owner             AS "owner?: String",
                 pinned_version_id AS "pinned_version_id?: String",
                 archived          AS "archived!: i64",
                 created_at        AS "created_at!: String"
               FROM strategy WHERE id = ?1"#,
            id_str,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        match row {
            None => Ok(None),
            Some(r) => Ok(Some(Strategy {
                id: StrategyId::new(r.id),
                name: r.name,
                tags: parse_json_str_array(&r.tags)?,
                owner: r.owner,
                pinned_version_id: r.pinned_version_id.map(VersionId::new),
                archived: r.archived != 0,
                created_at: parse_created_at(&r.created_at)?,
            })),
        }
    }

    async fn list_strategies(&self, include_archived: bool) -> Result<Vec<Strategy>, DataError> {
        let archived_filter = i64::from(include_archived);
        let rows = sqlx::query!(
            r#"SELECT
                 id                AS "id!: String",
                 name              AS "name!: String",
                 tags              AS "tags!: String",
                 owner             AS "owner?: String",
                 pinned_version_id AS "pinned_version_id?: String",
                 archived          AS "archived!: i64",
                 created_at        AS "created_at!: String"
               FROM strategy
               WHERE (?1 = 1 OR archived = 0)
               ORDER BY created_at, id"#,
            archived_filter,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(Strategy {
                id: StrategyId::new(r.id),
                name: r.name,
                tags: parse_json_str_array(&r.tags)?,
                owner: r.owner,
                pinned_version_id: r.pinned_version_id.map(VersionId::new),
                archived: r.archived != 0,
                created_at: parse_created_at(&r.created_at)?,
            });
        }
        Ok(out)
    }

    async fn rename_strategy(
        &self,
        id: &StrategyId,
        new_name: &str,
    ) -> Result<Strategy, DataError> {
        let id_str = id.as_str();
        let result = sqlx::query!(
            "UPDATE strategy SET name = ?1 WHERE id = ?2",
            new_name,
            id_str,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(DataError::Db(format!("no such strategy `{id_str}`")));
        }
        self.get_strategy(id)
            .await?
            .ok_or_else(|| DataError::Db(format!("renamed strategy `{id_str}` vanished")))
    }

    async fn set_tags(&self, id: &StrategyId, tags: &[String]) -> Result<Strategy, DataError> {
        let id_str = id.as_str();
        let tags_json = serde_json::to_string(tags).map_err(|e| DataError::Db(e.to_string()))?;
        let result = sqlx::query!(
            "UPDATE strategy SET tags = ?1 WHERE id = ?2",
            tags_json,
            id_str,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(DataError::Db(format!("no such strategy `{id_str}`")));
        }
        self.get_strategy(id)
            .await?
            .ok_or_else(|| DataError::Db(format!("retagged strategy `{id_str}` vanished")))
    }

    async fn set_pinned_version(
        &self,
        id: &StrategyId,
        version_id: Option<&VersionId>,
    ) -> Result<Strategy, DataError> {
        let id_str = id.as_str();
        // The ownership check and the write run in one transaction (gate-7 C5):
        // the version-∈-strategy SELECT and the UPDATE are atomic.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        let result = if let Some(v) = version_id {
            let v_str = v.as_str();
            let owns = sqlx::query!(
                r#"SELECT 1 AS "one!: i64" FROM strategy_version
                   WHERE id = ?1 AND strategy_id = ?2"#,
                v_str,
                id_str,
            )
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;
            if owns.is_none() {
                return Err(DataError::Db(format!(
                    "pinned version `{v_str}` does not belong to strategy `{id_str}`"
                )));
            }
            sqlx::query!(
                "UPDATE strategy SET pinned_version_id = ?1 WHERE id = ?2",
                v_str,
                id_str,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| DataError::Db(e.to_string()))?
        } else {
            sqlx::query!(
                "UPDATE strategy SET pinned_version_id = NULL WHERE id = ?1",
                id_str,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| DataError::Db(e.to_string()))?
        };
        if result.rows_affected() == 0 {
            return Err(DataError::Db(format!("no such strategy `{id_str}`")));
        }

        tx.commit()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;
        self.get_strategy(id)
            .await?
            .ok_or_else(|| DataError::Db(format!("pinned strategy `{id_str}` vanished")))
    }

    async fn archive_strategy(
        &self,
        id: &StrategyId,
        archived: bool,
    ) -> Result<Strategy, DataError> {
        let id_str = id.as_str();
        let archived_int = i64::from(archived);
        let result = sqlx::query!(
            "UPDATE strategy SET archived = ?1 WHERE id = ?2",
            archived_int,
            id_str,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(DataError::Db(format!("no such strategy `{id_str}`")));
        }
        self.get_strategy(id)
            .await?
            .ok_or_else(|| DataError::Db(format!("archived strategy `{id_str}` vanished")))
    }

    async fn create_version(&self, request: NewVersion) -> Result<StrategyVersion, DataError> {
        // 1. Route the raw dsl_json through the injected migrator (AC-7/AC-13).
        //    NEVER serde_json::from_str::<StrategyDsl> directly (migration-unaware).
        let loaded = self
            .migrator
            .load(&request.dsl_json)
            .map_err(|e| DataError::Db(format!("dsl load failed: {e}")))?;

        // 1b. Validate the migrated DSL before persisting (gate-7 C2). The DB is
        //     the system-of-record; reject invalid-but-loadable DSLs here.
        validate(&loaded.dsl).map_err(|e| DataError::Db(format!("dsl validation failed: {e}")))?;

        // 2. Build the column values.
        let id = Uuid::new_v4().to_string();
        let schema_version_str = SchemaVersion::CURRENT.to_string();
        let strategy_id_str = request.strategy_id.as_str().to_owned();
        let parent_str = request
            .parent_version_id
            .as_ref()
            .map(|p| p.as_str().to_owned());
        let dsl_current =
            serde_json::to_string(&loaded.dsl).map_err(|e| DataError::Db(e.to_string()))?;
        let hash = version_hash(
            &strategy_id_str,
            parent_str.as_deref(),
            &schema_version_str,
            &loaded.dsl_original,
        );
        let created_by_text =
            serde_json::to_string(&request.created_by).map_err(|e| DataError::Db(e.to_string()))?;
        let llm_ids_json = serde_json::to_string(&request.creating_llm_call_ids)
            .map_err(|e| DataError::Db(e.to_string()))?;
        let (_, created_at) = self.now_rfc3339()?;

        // 3. INSERT + read-back in one transaction (gate-7 C5).
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;
        insert_version_row(
            &mut tx,
            &VersionInsert {
                id: &id,
                strategy_id: &strategy_id_str,
                parent_version_id: parent_str.as_deref(),
                dsl_schema_version: &schema_version_str,
                dsl: &dsl_current,
                dsl_original: &loaded.dsl_original,
                version_hash: &hash,
                created_by: &created_by_text,
                creating_llm_call_ids: &llm_ids_json,
                created_at: &created_at,
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        self.get_version(&VersionId::new(id))
            .await?
            .ok_or_else(|| DataError::Db("created version vanished on read-back".to_owned()))
    }

    async fn get_version(&self, id: &VersionId) -> Result<Option<StrategyVersion>, DataError> {
        let id_str = id.as_str();
        let row = sqlx::query!(
            r#"SELECT
                 id                    AS "id!: String",
                 strategy_id           AS "strategy_id!: String",
                 parent_version_id     AS "parent_version_id?: String",
                 dsl_schema_version    AS "dsl_schema_version!: String",
                 dsl_original          AS "dsl_original!: String",
                 version_hash          AS "version_hash!: String",
                 created_by            AS "created_by!: String",
                 creating_llm_call_ids AS "creating_llm_call_ids!: String",
                 created_at            AS "created_at!: String"
               FROM strategy_version WHERE id = ?1"#,
            id_str,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        match row {
            None => Ok(None),
            Some(r) => Ok(Some(self.row_to_version(VersionRow {
                id: r.id,
                strategy_id: r.strategy_id,
                parent_version_id: r.parent_version_id,
                dsl_schema_version: r.dsl_schema_version,
                dsl_original: r.dsl_original,
                version_hash: r.version_hash,
                created_by: r.created_by,
                creating_llm_call_ids: r.creating_llm_call_ids,
                created_at: r.created_at,
            })?)),
        }
    }

    async fn list_versions(
        &self,
        strategy_id: &StrategyId,
    ) -> Result<Vec<StrategyVersion>, DataError> {
        let sid = strategy_id.as_str();
        let rows = sqlx::query!(
            r#"SELECT
                 id                    AS "id!: String",
                 strategy_id           AS "strategy_id!: String",
                 parent_version_id     AS "parent_version_id?: String",
                 dsl_schema_version    AS "dsl_schema_version!: String",
                 dsl_original          AS "dsl_original!: String",
                 version_hash          AS "version_hash!: String",
                 created_by            AS "created_by!: String",
                 creating_llm_call_ids AS "creating_llm_call_ids!: String",
                 created_at            AS "created_at!: String"
               FROM strategy_version WHERE strategy_id = ?1 ORDER BY created_at, id"#,
            sid,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(self.row_to_version(VersionRow {
                id: r.id,
                strategy_id: r.strategy_id,
                parent_version_id: r.parent_version_id,
                dsl_schema_version: r.dsl_schema_version,
                dsl_original: r.dsl_original,
                version_hash: r.version_hash,
                created_by: r.created_by,
                creating_llm_call_ids: r.creating_llm_call_ids,
                created_at: r.created_at,
            })?);
        }
        Ok(out)
    }

    async fn version_tree(
        &self,
        strategy_id: &StrategyId,
    ) -> Result<Vec<StrategyVersion>, DataError> {
        let versions = self.list_versions(strategy_id).await?;
        Ok(parent_order(versions))
    }

    async fn create_agent_version(
        &self,
        request: NewVersion,
        submission: NewAgentSubmission,
    ) -> Result<(StrategyVersion, AgentSubmission), DataError> {
        // Refusals BEFORE touching the database (r2.s1.w1): the submission is
        // the version's whole audit trail — it names no LlmCall rows, and a
        // non-external-agent request is not this port's to write.
        if request.created_by != CreatedBy::ExternalAgent {
            return Err(DataError::Db(
                "create_agent_version requires created_by = external_agent".to_owned(),
            ));
        }
        if !request.creating_llm_call_ids.is_empty() {
            return Err(DataError::Db(
                "an external-agent version names no LlmCall rows".to_owned(),
            ));
        }

        // The same load → validate → canonicalize pipeline `create_version`
        // runs — the migrator and validator are provenance-blind.
        let loaded = self
            .migrator
            .load(&request.dsl_json)
            .map_err(|e| DataError::Db(format!("dsl load failed: {e}")))?;
        validate(&loaded.dsl).map_err(|e| DataError::Db(format!("dsl validation failed: {e}")))?;

        let id = Uuid::new_v4().to_string();
        let submission_id = Uuid::new_v4().to_string();
        let schema_version_str = SchemaVersion::CURRENT.to_string();
        let strategy_id_str = request.strategy_id.as_str().to_owned();
        let parent_str = request
            .parent_version_id
            .as_ref()
            .map(|p| p.as_str().to_owned());
        let dsl_current =
            serde_json::to_string(&loaded.dsl).map_err(|e| DataError::Db(e.to_string()))?;
        let hash = version_hash(
            &strategy_id_str,
            parent_str.as_deref(),
            &schema_version_str,
            &loaded.dsl_original,
        );
        let created_by_text =
            serde_json::to_string(&request.created_by).map_err(|e| DataError::Db(e.to_string()))?;
        let llm_ids_json = serde_json::to_string(&request.creating_llm_call_ids)
            .map_err(|e| DataError::Db(e.to_string()))?;
        // One timestamp for the committed pair — they are minted atomically.
        let (_, created_at) = self.now_rfc3339()?;

        // ONE transaction: the version row and its submission row, or neither.
        // `BEGIN IMMEDIATE` takes the write lock up front rather than upgrading
        // a deferred transaction on first write — the two writes are committed
        // as one unit and a concurrent writer fails fast instead of racing the
        // upgrade.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;
        insert_version_row(
            &mut tx,
            &VersionInsert {
                id: &id,
                strategy_id: &strategy_id_str,
                parent_version_id: parent_str.as_deref(),
                dsl_schema_version: &schema_version_str,
                dsl: &dsl_current,
                dsl_original: &loaded.dsl_original,
                version_hash: &hash,
                created_by: &created_by_text,
                creating_llm_call_ids: &llm_ids_json,
                created_at: &created_at,
            },
        )
        .await?;
        let agent_name = submission.agent_name.as_str();
        let hypothesis = submission.hypothesis.as_str();
        sqlx::query!(
            "INSERT INTO agent_submission (id, version_id, agent_name, hypothesis, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            submission_id,
            id,
            agent_name,
            hypothesis,
            created_at,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        // Read both halves back through the defended paths — `get_version`
        // re-derives the hash; `get_agent_submission` re-validates the newtypes.
        let version = self
            .get_version(&VersionId::new(id))
            .await?
            .ok_or_else(|| {
                DataError::Db("created agent version vanished on read-back".to_owned())
            })?;
        let submission = self
            .get_agent_submission(&version.id)
            .await?
            .ok_or_else(|| DataError::Db("created submission vanished on read-back".to_owned()))?;
        Ok((version, submission))
    }

    async fn get_agent_submission(
        &self,
        version_id: &VersionId,
    ) -> Result<Option<AgentSubmission>, DataError> {
        let vid = version_id.as_str();
        let row = sqlx::query!(
            r#"SELECT
                 id         AS "id!: String",
                 version_id AS "version_id!: String",
                 agent_name AS "agent_name!: String",
                 hypothesis AS "hypothesis!: String",
                 created_at AS "created_at!: String"
               FROM agent_submission WHERE version_id = ?1"#,
            vid,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        match row {
            None => Ok(None),
            Some(r) => Ok(Some(AgentSubmission {
                id: AgentSubmissionId::new(r.id),
                version_id: VersionId::new(r.version_id),
                // Defended read: a stored row that fails domain validation is
                // a corrupt row (a write that bypassed the typed port), not a
                // `None`.
                agent_name: AgentName::parse(&r.agent_name).map_err(|e| {
                    DataError::Db(format!("stored agent_name failed domain validation: {e}"))
                })?,
                hypothesis: Hypothesis::parse(&r.hypothesis).map_err(|e| {
                    DataError::Db(format!("stored hypothesis failed domain validation: {e}"))
                })?,
                created_at: parse_created_at(&r.created_at)?,
            })),
        }
    }
}

/// The raw `strategy_version` column values as read from the DB — the input to
/// [`SqliteStrategyRepo::row_to_version`]. A struct (not a long arg list) so the
/// re-derive helper consumes one owned value and avoids a wide signature.
struct VersionRow {
    id: String,
    strategy_id: String,
    parent_version_id: Option<String>,
    dsl_schema_version: String,
    dsl_original: String,
    version_hash: String,
    created_by: String,
    creating_llm_call_ids: String,
    created_at: String,
}

impl<C: Clock> SqliteStrategyRepo<C> {
    /// Build a [`StrategyVersion`] from its raw column values, re-deriving `.dsl`
    /// from the verbatim `dsl_original` (#19) and rejecting a `version_hash`
    /// mismatch (audit-C5). All `get_version`/`list_versions`/`version_tree`
    /// reads funnel through here so every read is defended.
    fn row_to_version(&self, row: VersionRow) -> Result<StrategyVersion, DataError> {
        // Re-derive `.dsl` by re-routing dsl_original through the migrator (#19);
        // a stored row that cannot be reconstructed into a valid current DSL is
        // rejected (`LoadError::Deserialize` → `DataError::Db`) rather than
        // silently passing the read defense.
        let loaded = self
            .migrator
            .load(&row.dsl_original)
            .map_err(|e| DataError::Db(format!("stored dsl_original failed to load: {e}")))?;

        // Re-derive the hash and reject a mismatch (audit-C5 tamper defense).
        let derived = version_hash(
            &row.strategy_id,
            row.parent_version_id.as_deref(),
            &row.dsl_schema_version,
            &row.dsl_original,
        );
        if derived != row.version_hash {
            return Err(DataError::Db(format!(
                "version_hash mismatch for `{}`: stored {}, derived {derived}",
                row.id, row.version_hash
            )));
        }

        Ok(StrategyVersion {
            id: VersionId::new(row.id),
            strategy_id: StrategyId::new(row.strategy_id),
            parent_version_id: row.parent_version_id.map(VersionId::new),
            dsl_schema_version: parse_schema_version(&row.dsl_schema_version)?,
            dsl: loaded.dsl,
            dsl_original: row.dsl_original,
            version_hash: row.version_hash,
            created_by: parse_created_by(&row.created_by)?,
            creating_llm_call_ids: parse_json_str_array(&row.creating_llm_call_ids)?,
            created_at: parse_created_at(&row.created_at)?,
        })
    }
}

/// Order a version set parent-before-child: roots first, then each child after
/// its parent, ties broken by `created_at`. A stable topological order over the
/// `parent_version_id` self-ref tree, computed in Rust (the per-strategy version
/// count is small).
fn parent_order(mut versions: Vec<StrategyVersion>) -> Vec<StrategyVersion> {
    // Stable input order: created_at, then id (deterministic tie-break even when
    // two versions share a created_at millisecond under a coarse clock).
    versions.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });

    // The full id set, computed once: a parent is "in this set" iff it appears
    // here (so a node whose parent is later in `remaining` is NOT mistaken for an
    // orphan — the per-pass bug fix).
    let all_ids: std::collections::HashSet<String> =
        versions.iter().map(|v| v.id.as_str().to_owned()).collect();

    let mut emitted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<StrategyVersion> = Vec::with_capacity(versions.len());
    let mut remaining: Vec<StrategyVersion> = versions;

    // Repeatedly emit every node whose parent is already emitted (root, or a
    // parent pointing outside this set — an orphan — counts as ready). Bounded by
    // the node count.
    while !remaining.is_empty() {
        let mut progressed = false;
        let mut still: Vec<StrategyVersion> = Vec::with_capacity(remaining.len());
        for v in remaining {
            let parent_ready = match &v.parent_version_id {
                None => true,
                Some(p) => emitted.contains(p.as_str()) || !all_ids.contains(p.as_str()),
            };
            if parent_ready {
                emitted.insert(v.id.as_str().to_owned());
                out.push(v);
                progressed = true;
            } else {
                still.push(v);
            }
        }
        remaining = still;
        if !progressed {
            // A cycle (shouldn't happen — versions are immutable) — emit the rest
            // in their stable order rather than loop forever.
            out.append(&mut remaining);
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// r1.s4.w4 — the crate-private version insert mapping, extracted so the coach's
// accept transaction can REUSE it.
//
// `commit_acceptance` mints the child `strategy_version` inside the same
// transaction that writes its run. Copying this mapping there would have created a
// second place that knows how a version becomes columns, and `strategy_version`
// carries `version_hash` — an integrity field whose two writers drifting apart is
// not a cosmetic problem.
// ---------------------------------------------------------------------------

/// The `strategy_version` column tuple, already canonicalized by the caller.
///
/// Borrowed rather than owned: every field is a `TEXT` column and both call sites
/// already hold the rendered strings, so this is a parameter list with names
/// instead of ten positional `&str`s (`clippy::too_many_arguments`, and the
/// positional form is exactly how a `dsl`/`dsl_original` swap gets written).
pub(crate) struct VersionInsert<'a> {
    /// The minted row id.
    pub id: &'a str,
    /// The owning strategy.
    pub strategy_id: &'a str,
    /// The parent version, when this is not a root.
    pub parent_version_id: Option<&'a str>,
    /// The DSL schema version tag.
    pub dsl_schema_version: &'a str,
    /// The migrated, current-schema DSL as JSON.
    pub dsl: &'a str,
    /// The DSL exactly as authored, byte-for-byte.
    pub dsl_original: &'a str,
    /// The content-identity hash (see [`version_hash`]).
    pub version_hash: &'a str,
    /// The serialized `CreatedBy`.
    pub created_by: &'a str,
    /// The serialized creating-call id array.
    pub creating_llm_call_ids: &'a str,
    /// RFC3339 UTC creation timestamp.
    pub created_at: &'a str,
}

/// Insert one `strategy_version` row on the caller's transaction.
///
/// # Errors
///
/// Returns [`DataError::Db`] when the store rejects the write — including the
/// `0001` immutability triggers and the FK on `parent_version_id`.
pub(crate) async fn insert_version_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    row: &VersionInsert<'_>,
) -> Result<(), DataError> {
    sqlx::query!(
        "INSERT INTO strategy_version \
         (id, strategy_id, parent_version_id, dsl_schema_version, dsl, dsl_original, \
          version_hash, created_by, creating_llm_call_ids, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        row.id,
        row.strategy_id,
        row.parent_version_id,
        row.dsl_schema_version,
        row.dsl,
        row.dsl_original,
        row.version_hash,
        row.created_by,
        row.creating_llm_call_ids,
        row.created_at,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| DataError::Db(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{SqliteStrategyRepo, version_hash};
    use crate::adapters::clock::FakeClock;
    use crate::adapters::db::{Db, MIGRATOR};
    use crate::domain::strategy::{CreatedBy, NewVersion, StrategyId, VersionId};
    use crate::domain::{
        Comparator, Condition, Direction, ExitRule, IndicatorSpec, Migration, MigrationError,
        MigrationKind, Migrator, RiskParams, SchemaVersion, StrategyDsl, StrategyRepository,
        SweepableValue, ValueSource,
    };
    use rust_decimal::Decimal;
    use serde_json::{Value, json};
    use sqlx::SqlitePool;
    use tempfile::TempDir;

    /// A `(repo, pool, tempdir)` triple over a fresh migrated tempfile DB, with
    /// the production deps (`SystemClock` + `Migrator::v1()`). The `TempDir`
    /// guard keeps the scratch DB alive for the test body.
    async fn repo() -> (
        SqliteStrategyRepo<crate::adapters::clock::SystemClock>,
        SqlitePool,
        TempDir,
    ) {
        let tmp = TempDir::new().expect("tempdir");
        let db = Db::with_path(&tmp.path().join("pulse.db"))
            .await
            .expect("open db");
        MIGRATOR.run(db.pool()).await.expect("run 0001_init");
        let pool = db.pool().clone();
        (SqliteStrategyRepo::new(pool.clone()), pool, tmp)
    }

    /// The canonical `1.0.0` RSI-oversold strategy (valid: has a `StopLoss`).
    fn canonical_dsl() -> StrategyDsl {
        StrategyDsl {
            schema_version: SchemaVersion::CURRENT,
            name: "RSI Oversold".to_owned(),
            direction: Direction::Long,
            entry: Condition::Compare {
                lhs: ValueSource::Indicator {
                    spec: IndicatorSpec::Rsi {
                        period: SweepableValue::Fixed(14),
                    },
                },
                op: Comparator::Lt,
                rhs: ValueSource::Constant {
                    value: Decimal::new(30, 0),
                },
            },
            filters: vec![],
            exits: vec![
                ExitRule::StopLoss {
                    distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
                },
                ExitRule::TakeProfit {
                    target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
                },
            ],
            risk: RiskParams {
                risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
                max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
            },
        }
    }

    fn canonical_json() -> String {
        serde_json::to_string(&canonical_dsl()).expect("serialize canonical dsl")
    }

    /// Build a `NewVersion` for `strategy_id` from the canonical DSL.
    fn new_version(strategy_id: &StrategyId, parent: Option<&VersionId>) -> NewVersion {
        NewVersion {
            strategy_id: strategy_id.clone(),
            parent_version_id: parent.cloned(),
            dsl_json: canonical_json(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        }
    }

    // ---- AC-10 (FR-4 / NFR-2): byte-identical dsl_original round-trip --------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn version_reloads_byte_identical_dsl_original() {
        let (repo, _pool, _tmp) = repo().await;
        let s = repo.create_strategy("Demo", None, &[]).await.unwrap();
        let dsl_json = canonical_json();
        let created = repo
            .create_version(NewVersion {
                strategy_id: s.id.clone(),
                parent_version_id: None,
                dsl_json: dsl_json.clone(),
                created_by: CreatedBy::Human,
                creating_llm_call_ids: vec![],
            })
            .await
            .unwrap();

        let fetched = repo.get_version(&created.id).await.unwrap().unwrap();
        // The verbatim source bytes survive the create→read round-trip (==).
        assert_eq!(fetched.dsl_original, dsl_json);
    }

    // ---- AC-11 / AC-12 (FR-4): immutability triggers abort raw UPDATE/DELETE -

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn raw_update_on_strategy_version_is_aborted_by_trigger() {
        let (repo, pool, _tmp) = repo().await;
        let s = repo.create_strategy("Demo", None, &[]).await.unwrap();
        let v = repo.create_version(new_version(&s.id, None)).await.unwrap();

        let id = v.id.as_str().to_owned();
        let err = sqlx::query("UPDATE strategy_version SET dsl = ?1 WHERE id = ?2")
            .bind("{}")
            .bind(&id)
            .execute(&pool)
            .await
            .expect_err("raw UPDATE on an immutable row must fail");
        assert!(
            err.to_string().contains("strategy_version is immutable"),
            "trigger ABORT message must surface, got: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn raw_delete_on_strategy_version_is_aborted_by_trigger() {
        let (repo, pool, _tmp) = repo().await;
        let s = repo.create_strategy("Demo", None, &[]).await.unwrap();
        let v = repo.create_version(new_version(&s.id, None)).await.unwrap();

        let id = v.id.as_str().to_owned();
        let err = sqlx::query("DELETE FROM strategy_version WHERE id = ?1")
            .bind(&id)
            .execute(&pool)
            .await
            .expect_err("raw DELETE on an immutable row must fail");
        assert!(
            err.to_string().contains("strategy_version is immutable"),
            "trigger ABORT message must surface, got: {err}"
        );
    }

    // ---- AC-13 (FR-4): write routes dsl_json through the migrator ------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_version_routes_dsl_through_migrator() {
        let (repo, pool, _tmp) = repo().await;
        let s = repo.create_strategy("Demo", None, &[]).await.unwrap();
        let dsl_json = canonical_json();
        let v = repo
            .create_version(NewVersion {
                strategy_id: s.id.clone(),
                parent_version_id: None,
                dsl_json: dsl_json.clone(),
                created_by: CreatedBy::Human,
                creating_llm_call_ids: vec![],
            })
            .await
            .unwrap();

        // Read the stored `dsl` column directly (get_version re-derives, so we
        // bypass it here to inspect the stored migrator output).
        let id = v.id.as_str().to_owned();
        let stored_dsl: String =
            sqlx::query_scalar("SELECT dsl FROM strategy_version WHERE id = ?1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let expected_dsl =
            serde_json::to_string(&Migrator::v1().load(&dsl_json).unwrap().dsl).unwrap();
        assert_eq!(
            stored_dsl, expected_dsl,
            "stored dsl is the migrator output"
        );

        // And dsl_original is the raw input verbatim.
        let stored_original: String =
            sqlx::query_scalar("SELECT dsl_original FROM strategy_version WHERE id = ?1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored_original, dsl_json);

        // A malformed dsl_json returns Err, not a persisted row.
        let bad = repo
            .create_version(NewVersion {
                strategy_id: s.id.clone(),
                parent_version_id: None,
                dsl_json: "not json".to_owned(),
                created_by: CreatedBy::Human,
                creating_llm_call_ids: vec![],
            })
            .await;
        assert!(bad.is_err(), "malformed dsl_json must reject");
    }

    // ---- AC-14 (#17): get_version rejects an unknown field in stored dsl -----

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_version_rejects_unknown_fields_in_stored_dsl() {
        let (repo, pool, _tmp) = repo().await;
        let s = repo.create_strategy("Demo", None, &[]).await.unwrap();

        // Seed a row by hand whose stored `dsl_original` carries an UNKNOWN
        // shape the current grammar cannot reconstruct — here an unknown
        // `indicator` variant tag (`"Frobnicate"`). The read re-routes
        // `dsl_original` through the migrator, whose typed deserialize rejects it
        // (`LoadError::Deserialize` → `DataError::Db`), so a corrupt stored row
        // does NOT silently pass the re-derive-on-read defense (#19).
        //
        // NOTE (spec deviation, see report §5): the spec named this case "an
        // extra unknown key" citing `deny_unknown_fields` (#17). The merged DSL
        // types do NOT carry `deny_unknown_fields`, so an *extra* top-level/inner
        // key is silently ignored by serde (verified). An unknown *variant tag*
        // is the genuine corruption the read defense rejects — the AC's intent
        // (a malformed stored DSL must reject on read, not silently pass) holds.
        let mut value: Value = serde_json::from_str(&canonical_json()).unwrap();
        value["entry"]["lhs"]["spec"]["indicator"] = json!("Frobnicate");
        let tampered = value.to_string();
        let schema = SchemaVersion::CURRENT.to_string();
        let hash = version_hash(s.id.as_str(), None, &schema, &tampered);

        sqlx::query(
            "INSERT INTO strategy_version \
             (id, strategy_id, dsl_schema_version, dsl, dsl_original, version_hash, created_by, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind("ver-unknown")
        .bind(s.id.as_str())
        .bind(&schema)
        .bind("{}")
        .bind(&tampered)
        .bind(&hash)
        .bind("\"human\"")
        .bind("2026-06-14T00:00:00.000Z")
        .execute(&pool)
        .await
        .unwrap();

        let err = repo.get_version(&VersionId::new("ver-unknown")).await;
        assert!(
            err.is_err(),
            "stored dsl_original with an unknown DSL shape must reject on read (#19)"
        );
    }

    // ---- AC-15 (audit-C5 / NFR-2): version_hash mismatch rejected -----------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_version_rejects_version_hash_mismatch() {
        let (repo, pool, _tmp) = repo().await;
        let s = repo.create_strategy("Demo", None, &[]).await.unwrap();

        // Happy path: a repo-written version re-derives to an equal hash → Ok.
        let v = repo.create_version(new_version(&s.id, None)).await.unwrap();
        assert!(repo.get_version(&v.id).await.unwrap().is_some());

        // Seed a row whose stored version_hash does NOT match its content.
        let schema = SchemaVersion::CURRENT.to_string();
        sqlx::query(
            "INSERT INTO strategy_version \
             (id, strategy_id, dsl_schema_version, dsl, dsl_original, version_hash, created_by, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind("ver-tampered")
        .bind(s.id.as_str())
        .bind(&schema)
        .bind(canonical_json())
        .bind(canonical_json())
        .bind("deadbeefdeadbeef") // wrong hash
        .bind("\"human\"")
        .bind("2026-06-14T00:00:00.000Z")
        .execute(&pool)
        .await
        .unwrap();

        let err = repo.get_version(&VersionId::new("ver-tampered")).await;
        assert!(
            err.is_err(),
            "a version_hash mismatch must reject (audit-C5)"
        );
    }

    // ---- AC-16 (FR-11): set_pinned_version rejects a foreign version --------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_pinned_version_rejects_foreign_version() {
        let (repo, _pool, _tmp) = repo().await;
        let a = repo.create_strategy("A", None, &[]).await.unwrap();
        let b = repo.create_strategy("B", None, &[]).await.unwrap();
        let va = repo.create_version(new_version(&a.id, None)).await.unwrap();
        let vb = repo.create_version(new_version(&b.id, None)).await.unwrap();

        // A foreign version (B's) is rejected.
        let err = repo.set_pinned_version(&a.id, Some(&vb.id)).await;
        assert!(err.is_err(), "pinning a foreign version must reject");

        // A's own version pins, and reloads equal.
        let pinned = repo.set_pinned_version(&a.id, Some(&va.id)).await.unwrap();
        assert_eq!(pinned.pinned_version_id.as_ref(), Some(&va.id));

        // None clears it back.
        let cleared = repo.set_pinned_version(&a.id, None).await.unwrap();
        assert_eq!(cleared.pinned_version_id, None);
    }

    // ---- AC-17 (FR-11): version_tree is parent-ordered ----------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn version_tree_is_parent_ordered() {
        let (repo, _pool, _tmp) = repo().await;
        let s = repo.create_strategy("Tree", None, &[]).await.unwrap();
        let root = repo.create_version(new_version(&s.id, None)).await.unwrap();
        let child = repo
            .create_version(new_version(&s.id, Some(&root.id)))
            .await
            .unwrap();
        let grandchild = repo
            .create_version(new_version(&s.id, Some(&child.id)))
            .await
            .unwrap();

        let tree = repo.version_tree(&s.id).await.unwrap();
        let pos = |id: &VersionId| tree.iter().position(|v| &v.id == id).unwrap();
        // Every non-root's parent appears earlier.
        assert!(pos(&root.id) < pos(&child.id), "root before child");
        assert!(
            pos(&child.id) < pos(&grandchild.id),
            "child before grandchild"
        );
        assert_eq!(tree.len(), 3);
    }

    // ---- AC-18 (FR-11): the full lifecycle matrix ---------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_fr11_strategy_lifecycle() {
        use crate::domain::strategy::diff_versions;

        let (repo, _pool, _tmp) = repo().await;
        let s = repo
            .create_strategy("Lifecycle", Some("alice"), &["btc".to_owned()])
            .await
            .unwrap();

        let root = repo.create_version(new_version(&s.id, None)).await.unwrap();
        let clone = repo
            .create_version(new_version(&s.id, Some(&root.id)))
            .await
            .unwrap();

        let renamed = repo.rename_strategy(&s.id, "Renamed").await.unwrap();
        assert_eq!(renamed.name, "Renamed");

        let retagged = repo
            .set_tags(&s.id, &["scalp".to_owned(), "eth".to_owned()])
            .await
            .unwrap();
        assert_eq!(retagged.tags, vec!["scalp".to_owned(), "eth".to_owned()]);

        let pinned = repo
            .set_pinned_version(&s.id, Some(&root.id))
            .await
            .unwrap();
        assert_eq!(pinned.pinned_version_id.as_ref(), Some(&root.id));

        let archived = repo.archive_strategy(&s.id, true).await.unwrap();
        assert!(archived.archived);

        // list_strategies(false) excludes the archived one; (true) includes it.
        let visible = repo.list_strategies(false).await.unwrap();
        assert!(!visible.iter().any(|x| x.id == s.id), "archived excluded");
        let all = repo.list_strategies(true).await.unwrap();
        assert!(all.iter().any(|x| x.id == s.id), "archived included");

        // list_versions returns both versions.
        let versions = repo.list_versions(&s.id).await.unwrap();
        assert_eq!(versions.len(), 2);

        // Fetch two versions and diff them (the FR-11 compare flow).
        let a = repo.get_version(&root.id).await.unwrap().unwrap();
        let b = repo.get_version(&clone.id).await.unwrap().unwrap();
        let diff = diff_versions(&a, &b);
        // The clone differs in parent (root vs root's parent=None) + version_hash.
        assert!(diff.parent_changed, "clone has a different parent");
        assert!(diff.version_hash_changed, "different version identity");
    }

    // ---- AC-19 (gate-2 Q1): migrate-on-write via an injected migrator -------

    /// A synthetic minor migration `0.9.0 → 1.0.0` renaming `strat_name → name`
    /// and stamping the current `schema_version`. A NON-CAPTURING named `fn`
    /// (Migration.apply is a bare fn pointer, §4a-5) — mirrors migrate.rs's own
    /// `synthetic_minor_0_9_to_1_0`.
    fn synthetic_minor_0_9_to_1_0() -> Migration {
        fn apply(mut value: Value) -> Result<Value, MigrationError> {
            let obj = value
                .as_object_mut()
                .ok_or_else(|| MigrationError("document is not a JSON object".to_owned()))?;
            let name = obj
                .remove("strat_name")
                .ok_or_else(|| MigrationError("old doc missing `strat_name`".to_owned()))?;
            obj.insert("name".to_owned(), name);
            obj.insert("schema_version".to_owned(), json!("1.0.0"));
            Ok(value)
        }
        Migration {
            from: SchemaVersion {
                major: 0,
                minor: 9,
                patch: 0,
            },
            to: SchemaVersion::CURRENT,
            kind: MigrationKind::Minor,
            apply,
        }
    }

    /// The canonical strategy in its old `0.9.0` shape (`strat_name` instead of
    /// `name`, downgraded version) — derived from the current JSON.
    fn old_0_9_json() -> String {
        let mut value: Value = serde_json::from_str(&canonical_json()).unwrap();
        let obj = value.as_object_mut().unwrap();
        let name = obj.remove("name").unwrap();
        obj.insert("strat_name".to_owned(), name);
        obj.insert("schema_version".to_owned(), json!("0.9.0"));
        value.to_string()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_version_migrates_old_dsl_via_injected_migrator() {
        let tmp = TempDir::new().unwrap();
        let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
        MIGRATOR.run(db.pool()).await.unwrap();
        let migrator = Migrator::with_migrations(vec![synthetic_minor_0_9_to_1_0()]);
        let repo = SqliteStrategyRepo::with_deps(
            db.pool().clone(),
            migrator,
            FakeClock::at(1_700_000_000_000),
        );

        let s = repo.create_strategy("Migrated", None, &[]).await.unwrap();
        let old = old_0_9_json();
        let v = repo
            .create_version(NewVersion {
                strategy_id: s.id.clone(),
                parent_version_id: None,
                dsl_json: old.clone(),
                created_by: CreatedBy::Human,
                creating_llm_call_ids: vec![],
            })
            .await
            .unwrap();

        let fetched = repo.get_version(&v.id).await.unwrap().unwrap();
        // (a) dsl_original is the verbatim OLD bytes.
        assert_eq!(
            fetched.dsl_original, old,
            "dsl_original is the old input verbatim"
        );
        // (b) the loaded `.dsl` is the migrated current form → differs from old.
        let current_serialized = serde_json::to_string(&fetched.dsl).unwrap();
        assert_ne!(
            current_serialized, old,
            "migrated dsl differs from old original"
        );
        assert_eq!(fetched.dsl.name, "RSI Oversold");
        assert_eq!(fetched.dsl.schema_version, SchemaVersion::CURRENT);
        // (c) the load reported migrated == true (proved by stored dsl != original).
    }

    // ---- AC-21 (gate-7 C2): create_version rejects an invalid DSL -----------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_version_rejects_invalid_dsl() {
        let (repo, _pool, _tmp) = repo().await;
        let s = repo.create_strategy("Invalid", None, &[]).await.unwrap();

        // Loads/migrates clean but fails validate(): a TakeProfit with no
        // StopLoss → rule 3 TakeProfitWithoutStop (§4a-7).
        let mut dsl = canonical_dsl();
        dsl.exits = vec![ExitRule::TakeProfit {
            target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
        }];
        let bad_json = serde_json::to_string(&dsl).unwrap();

        let err = repo
            .create_version(NewVersion {
                strategy_id: s.id.clone(),
                parent_version_id: None,
                dsl_json: bad_json,
                created_by: CreatedBy::Human,
                creating_llm_call_ids: vec![],
            })
            .await;
        assert!(err.is_err(), "an invalid-but-loadable DSL must reject");

        // No row was inserted.
        let versions = repo.list_versions(&s.id).await.unwrap();
        assert_eq!(versions.len(), 0, "rejected version must not persist");
    }

    // ---- AC-22 (gate-7 C1): created_at uses the injected clock --------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn created_at_uses_injected_clock() {
        let t: i64 = 1_700_000_900_000;
        let tmp = TempDir::new().unwrap();
        let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
        MIGRATOR.run(db.pool()).await.unwrap();
        let repo =
            SqliteStrategyRepo::with_deps(db.pool().clone(), Migrator::v1(), FakeClock::at(t));

        let s = repo.create_strategy("Clocked", None, &[]).await.unwrap();
        let v = repo.create_version(new_version(&s.id, None)).await.unwrap();

        let expected = DateTime::from_timestamp_millis(t).unwrap();
        assert_eq!(
            s.created_at, expected,
            "strategy created_at is the injected clock"
        );
        assert_eq!(
            v.created_at, expected,
            "version created_at is the injected clock"
        );
    }

    use chrono::DateTime;
}
