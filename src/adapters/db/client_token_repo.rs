//! The `SQLite` adapter for the `client_token` + `token_audit` tables
//! (r3.s3.w1, D5/D8, ADR-0026).
//!
//! This is the ONLY place `query!` macros for these two tables live (`sqlx` is
//! confined to `adapters::db`, mirror `llm_call_repo.rs`); the committed
//! `.sqlx/` offline cache is keyed to the macros here (refresh with
//! `just prepare` under sqlx-cli `=0.8.6`).
//!
//! **The token itself never crosses this adapter.** Callers hand in the SHA-256
//! hex of the full token string; every read-back (`ClientToken`) carries the
//! label, scope and timestamps but NOT the hash — there is no surface by which
//! a stored hash could reach a log or a CLI line.
//!
//! **Named refusals (D5).** A duplicate label is `LabelExists` even when the
//! existing token is revoked (labels are never reused); revoking an unknown
//! label is `LabelUnknown`; revoking an already-revoked label is
//! `AlreadyRevoked`. The migration's triggers back every one of these at the
//! database: label is UNIQUE, rows are never deleted, and the only legal
//! mutation is a FIRST `revoked_at` transition.
//!
//! **Writes are write-first atomic.** `issue` and `revoke` each take one
//! `BEGIN IMMEDIATE` transaction that covers the row write AND its `issued` /
//! `revoked` audit row (the `Pool::begin_with` idiom, mirror `strategy_repo`).
//!
//! **`created_at` / `at` from the injected `Clock` (D7, mirror `llm_call_repo`).**
//! RFC3339 millisecond UTC; deterministic under a `FakeClock`.
//!
//! NO `#[derive(Debug)]` on the repo struct: the `C: Clock` carries no `Debug`
//! bound (mirror `SqliteLlmCallRepo`).

use chrono::{DateTime, SecondsFormat};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::adapters::clock::SystemClock;
use crate::domain::{Clock, DataError};

/// The row-schema tag `issue` writes into every `client_token.schema_version`
/// and that every read asserts — a fail-closed read control, not a ceremonial
/// column (mirror `LLM_CALL_SCHEMA_VERSION`, #68).
const CLIENT_TOKEN_SCHEMA_VERSION: &str = "1";

/// A stored client token — everything EXCEPT the token and its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientToken {
    /// The row id (a UUID minted by this adapter).
    pub id: String,
    /// The unique, lowercased label.
    pub label: String,
    /// The scope: `app` or `agent`.
    pub scope: String,
    /// When the token was issued (RFC3339 UTC).
    pub created_at: String,
    /// When the token was revoked, if it was (RFC3339 UTC).
    pub revoked_at: Option<String>,
}

/// The `find_by_hash` projection: the client-token columns PLUS the row-schema
/// tag the read asserts on (the tag never leaves this module).
struct FoundTokenRow {
    id: String,
    label: String,
    scope: String,
    created_at: String,
    revoked_at: Option<String>,
    schema_version: String,
}

/// The named refusals of the token store (D5), plus the wrapped `DataError`.
#[derive(Debug, thiserror::Error)]
pub enum TokenStoreError {
    /// A token with this label already exists — even a revoked one; labels are
    /// never reused.
    #[error("a token named {0:?} already exists; labels are never reused")]
    LabelExists(String),
    /// No token carries this label.
    #[error("no token named {0:?}")]
    LabelUnknown(String),
    /// The token exists but its `revoked_at` is already set.
    #[error("the token named {0:?} is already revoked")]
    AlreadyRevoked(String),
    /// A database error underneath the refusal.
    #[error(transparent)]
    Db(#[from] DataError),
}

/// The `SQLite` store for `client_token` rows and the `token_audit` ledger.
///
/// Constructed from a [`SqlitePool`] (cloned from `Db::pool()`), with an
/// injected [`Clock`] for every timestamp it writes (D7).
pub struct SqliteClientTokenRepo<C: Clock> {
    pool: SqlitePool,
    clock: C,
}

impl SqliteClientTokenRepo<SystemClock> {
    /// The production constructor: the wall-clock [`SystemClock`].
    #[must_use]
    pub fn new(pool: SqlitePool) -> SqliteClientTokenRepo<SystemClock> {
        SqliteClientTokenRepo {
            pool,
            clock: SystemClock,
        }
    }
}

impl<C: Clock> SqliteClientTokenRepo<C> {
    /// The test/injection seam: supply a [`Clock`] so every written timestamp is
    /// deterministic (mirror `SqliteLlmCallRepo::with_deps`). Deliberately
    /// unused in w1 — w2/w5's deterministic repo tests drive it; the D7
    /// injected-clock capability exists from the start so the seam does not
    /// change shape later.
    #[allow(dead_code)]
    #[must_use]
    pub fn with_deps(pool: SqlitePool, clock: C) -> SqliteClientTokenRepo<C> {
        SqliteClientTokenRepo { pool, clock }
    }

    /// The current timestamp, sourced from the injected [`Clock`] (D7), as an
    /// RFC3339 millisecond UTC string for the `TEXT` columns.
    fn now_rfc3339(&self) -> Result<String, DataError> {
        let now_ms = self.clock.now_ms();
        let dt = DateTime::from_timestamp_millis(now_ms).ok_or_else(|| {
            DataError::Db(format!("clock.now_ms() {now_ms} is out of DateTime range"))
        })?;
        Ok(dt.to_rfc3339_opts(SecondsFormat::Millis, true))
    }

    /// Issue a token: insert the `client_token` row and its `issued` audit row
    /// in one write-first transaction.
    ///
    /// # Errors
    ///
    /// [`TokenStoreError::LabelExists`] when the label is taken (even by a
    /// revoked token); [`TokenStoreError::Db`] on a database failure.
    pub async fn issue(
        &self,
        label: &str,
        scope: &str,
        token_sha256: &str,
        created_by: &str,
    ) -> Result<ClientToken, TokenStoreError> {
        let now = self.now_rfc3339()?;
        let id = Uuid::new_v4().to_string();
        let audit_id = Uuid::new_v4().to_string();
        let schema_version = CLIENT_TOKEN_SCHEMA_VERSION.to_owned();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| DataError::Db(format!("begin issue tx: {e}")))?;

        // Named duplicate refusal BEFORE the insert: a taken label is refused
        // even when the holder is revoked (revoked labels are never reused).
        let taken = sqlx::query_scalar!("SELECT id FROM client_token WHERE label = ?", label)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| DataError::Db(format!("label pre-check: {e}")))?;
        if taken.is_some() {
            return Err(TokenStoreError::LabelExists(label.to_owned()));
        }

        sqlx::query!(
            "INSERT INTO client_token \
             (id, label, scope, token_sha256, created_at, revoked_at, created_by, schema_version) \
             VALUES (?, ?, ?, ?, ?, NULL, ?, ?)",
            id,
            label,
            scope,
            token_sha256,
            now,
            created_by,
            schema_version,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| DataError::Db(format!("insert client_token: {e}")))?;

        sqlx::query!(
            "INSERT INTO token_audit \
             (id, at, event, token_id, label, reason, route, peer, schema_version) \
             VALUES (?, ?, 'issued', ?, ?, NULL, NULL, NULL, ?)",
            audit_id,
            now,
            id,
            label,
            schema_version,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| DataError::Db(format!("insert issued audit: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| DataError::Db(format!("commit issue: {e}")))?;
        Ok(ClientToken {
            id,
            label: label.to_owned(),
            scope: scope.to_owned(),
            created_at: now,
            revoked_at: None,
        })
    }

    /// Revoke a token: set `revoked_at` once and append the `revoked` audit row
    /// in one write-first transaction.
    ///
    /// # Errors
    ///
    /// [`TokenStoreError::LabelUnknown`] when no token carries the label;
    /// [`TokenStoreError::AlreadyRevoked`] when its `revoked_at` is already set
    /// (the migration trigger refuses the second write regardless).
    pub async fn revoke(&self, label: &str) -> Result<ClientToken, TokenStoreError> {
        let now = self.now_rfc3339()?;
        let audit_id = Uuid::new_v4().to_string();
        let schema_version = CLIENT_TOKEN_SCHEMA_VERSION.to_owned();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| DataError::Db(format!("begin revoke tx: {e}")))?;

        let row = sqlx::query_as!(
            FoundTokenRow,
            "SELECT id, label, scope, created_at, revoked_at, schema_version \
             FROM client_token WHERE label = ?",
            label
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| DataError::Db(format!("revoke lookup: {e}")))?;
        let Some(found) = row else {
            return Err(TokenStoreError::LabelUnknown(label.to_owned()));
        };
        if found.revoked_at.is_some() {
            return Err(TokenStoreError::AlreadyRevoked(label.to_owned()));
        }

        let applied = sqlx::query!(
            "UPDATE client_token SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL",
            now,
            found.id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| DataError::Db(format!("revoke update: {e}")))?;
        if applied.rows_affected() != 1 {
            // The trigger refused or another writer got there first — the named
            // error is the same either way.
            return Err(TokenStoreError::AlreadyRevoked(label.to_owned()));
        }

        sqlx::query!(
            "INSERT INTO token_audit \
             (id, at, event, token_id, label, reason, route, peer, schema_version) \
             VALUES (?, ?, 'revoked', ?, ?, NULL, NULL, NULL, ?)",
            audit_id,
            now,
            found.id,
            label,
            schema_version,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| DataError::Db(format!("insert revoked audit: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| DataError::Db(format!("commit revoke: {e}")))?;
        Ok(ClientToken {
            id: found.id,
            label: found.label,
            scope: found.scope,
            created_at: found.created_at,
            revoked_at: Some(now),
        })
    }

    /// Resolve a presented token by its SHA-256 hex. Returns the row regardless
    /// of `revoked_at` so the auth layer can name the reason (unknown vs
    /// revoked); the auth layer is what refuses.
    ///
    /// # Errors
    ///
    /// [`DataError::Db`] on a database failure or an unknown row
    /// `schema_version`.
    pub async fn find_by_hash(&self, token_sha256: &str) -> Result<Option<ClientToken>, DataError> {
        let row = sqlx::query_as!(
            FoundTokenRow,
            "SELECT id, label, scope, created_at, revoked_at, schema_version \
             FROM client_token WHERE token_sha256 = ?",
            token_sha256
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("find by hash: {e}")))?;
        let Some(found) = row else {
            return Ok(None);
        };
        // The read-reject control (mirror #68): an unknown row schema fails
        // closed instead of authenticating against a foreign shape.
        if found.schema_version != CLIENT_TOKEN_SCHEMA_VERSION {
            return Err(DataError::Db(format!(
                "client_token row {} carries unknown schema_version {:?}",
                found.id, found.schema_version
            )));
        }
        Ok(Some(ClientToken {
            id: found.id,
            label: found.label,
            scope: found.scope,
            created_at: found.created_at,
            revoked_at: found.revoked_at,
        }))
    }

    /// List every token, oldest first. The result carries no token and no hash.
    ///
    /// # Errors
    ///
    /// [`DataError::Db`] on a database failure.
    pub async fn list(&self) -> Result<Vec<ClientToken>, DataError> {
        let rows = sqlx::query_as!(
            ClientToken,
            "SELECT id, label, scope, created_at, revoked_at \
             FROM client_token ORDER BY created_at, label",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("list tokens: {e}")))?;
        Ok(rows)
    }

    /// Append one audit row. Best-effort by design: the auth layer logs
    /// refusals even when this fails, and never lets an audit failure turn
    /// into a panic.
    ///
    /// # Errors
    ///
    /// [`DataError::Db`] on a database failure (including the append-only
    /// triggers, which cannot fire here — this is a pure INSERT).
    pub async fn audit_append(
        &self,
        event: &str,
        token_id: Option<&str>,
        label: Option<&str>,
        reason: Option<&str>,
        route: Option<&str>,
        peer: Option<&str>,
    ) -> Result<(), DataError> {
        let now = self.now_rfc3339()?;
        let id = Uuid::new_v4().to_string();
        sqlx::query!(
            "INSERT INTO token_audit \
             (id, at, event, token_id, label, reason, route, peer, schema_version) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            id,
            now,
            event,
            token_id,
            label,
            reason,
            route,
            peer,
            CLIENT_TOKEN_SCHEMA_VERSION,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("insert audit: {e}")))?;
        Ok(())
    }
}
