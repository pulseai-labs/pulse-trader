//! The `SQLite` adapter implementing the [`CoachingRepository`] port (r1.s2.w2,
//! ADR-0021 / audit C3).
//!
//! This is the ONLY place `query!` macros for the `coaching_sessions` /
//! `coaching_proposals` tables live (`sqlx` is confined to `adapters::db`, mirror
//! `llm_call_repo.rs:1-9`); the committed `.sqlx/` offline cache is keyed to the
//! macros in this file (regenerate with `just prepare` under sqlx-cli `=0.8.6` —
//! #41: a floating install can drift out of sync with the pinned `sqlx` crate and
//! desync the cache).
//!
//! **A turn is one transaction.** A proposal turn writes two rows — the session and
//! its proposal — and a half-written turn would be exactly the silence the
//! capability sentence forbids, so `save_session` commits both or neither.
//!
//! **Typed projection, never a blob.** The session's identity, its run/version
//! references and its outcome are explicit columns; only the two payloads that are
//! themselves typed domain values (the `Mutation` and the `CoachFailure`) are
//! stored as serde JSON, so the coach's proposal and its failure reason come back
//! as the types they went in as rather than as prose.
//!
//! **`created_at` from the injected `Clock`** (mirror `SqliteLlmCallRepo`): the
//! stored timestamp is the adapter's, serialized RFC3339-millis, deterministic
//! under a `FakeClock`.
//!
//! **`schema_version` read-reject (#68, mirror `LLM_CALL_SCHEMA_VERSION`).** An
//! unknown stored tag is a fail-closed [`DataError::Db`], never a silent partial.
//!
//! **No `validated` column, and none is read or written here** (audit C4): a
//! mutation's applicability is re-established by
//! [`apply`](crate::domain::apply) at use time.
//!
//! NO `#[derive(Debug)]` on the repo struct: the `C: Clock` carries no `Debug`
//! bound (mirror `SqliteLlmCallRepo`).

use chrono::{DateTime, SecondsFormat};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::adapters::clock::SystemClock;
use crate::domain::backtest::BacktestRunId;
use crate::domain::strategy::VersionId;
use crate::domain::{
    Clock, CoachAcceptFailure, CoachFailure, CoachSessionClaim, CoachSessionClaimResult,
    CoachingRepository, CoachingSession, CoachingSessionId, DataError, Disposition, Hypothesis,
    InitialCoachOutcome, LlmCallId, Mutation, Proposal, SessionOutcome,
};

/// The row-schema tag `save_session` writes into every
/// `coaching_sessions.schema_version` and that every read ASSERTS (mirror
/// `LLM_CALL_SCHEMA_VERSION`, #68). v1 reads only v1 and rejects the rest with a
/// real [`DataError::Db`].
const COACHING_SCHEMA_VERSION: i64 = 1;

/// The `outcome` column's three values, matching migration `0008`'s `CHECK`.
/// `pending` is the pre-call claim `0005` could not express (r1.s4.w4).
const OUTCOME_PENDING: &str = "pending";
const OUTCOME_PROPOSED: &str = "proposed";
const OUTCOME_FAILED: &str = "failed";

/// The `SQLite` [`CoachingRepository`](crate::domain::port::CoachingRepository)
/// adapter over `pulse.db`.
///
/// Constructed from a [`SqlitePool`] (cloned from `Db::pool()`). Carries an
/// injected [`Clock`] (the `created_at` source).
pub struct SqliteCoachingRepo<C: Clock> {
    pool: SqlitePool,
    clock: C,
}

impl SqliteCoachingRepo<SystemClock> {
    /// The production constructor: the wall-clock [`SystemClock`].
    #[must_use]
    pub fn new(pool: SqlitePool) -> SqliteCoachingRepo<SystemClock> {
        SqliteCoachingRepo {
            pool,
            clock: SystemClock,
        }
    }
}

impl<C: Clock> SqliteCoachingRepo<C> {
    /// The test/injection seam: supply a [`Clock`] so `created_at` is
    /// deterministic (mirror `SqliteLlmCallRepo::with_deps`).
    #[must_use]
    pub fn with_deps(pool: SqlitePool, clock: C) -> SqliteCoachingRepo<C> {
        SqliteCoachingRepo { pool, clock }
    }

    /// The current `created_at`, sourced from the injected [`Clock`], serialized
    /// as an RFC3339 millisecond UTC string for the `TEXT` column.
    fn now_rfc3339(&self) -> Result<String, DataError> {
        let now_ms = self.clock.now_ms();
        let dt = DateTime::from_timestamp_millis(now_ms).ok_or_else(|| {
            DataError::Db(format!("clock.now_ms() {now_ms} is out of DateTime range"))
        })?;
        Ok(dt.to_rfc3339_opts(SecondsFormat::Millis, true))
    }
}

/// The `failure_kind` column value for a failure — the `snake_case` tag migration
/// `0005`'s `CHECK` enumerates. Written explicitly rather than derived from serde
/// so the column's vocabulary and the schema's `CHECK` cannot drift apart silently:
/// adding a `CoachFailure` variant fails to compile here until both are updated.
fn failure_kind(failure: &CoachFailure) -> &'static str {
    match failure {
        CoachFailure::ZeroCalls => "zero_calls",
        CoachFailure::SeveralCalls { .. } => "several_calls",
        CoachFailure::MalformedArguments { .. } => "malformed_arguments",
        CoachFailure::InapplicableMutation { .. } => "inapplicable_mutation",
        CoachFailure::ProviderTimeout { .. } => "provider_timeout",
        CoachFailure::ContextOverflow { .. } => "context_overflow",
        CoachFailure::TransportFailure { .. } => "transport_failure",
        CoachFailure::InapplicableAdvice { .. } => "inapplicable_advice",
        CoachFailure::MissingBacktestInputs { .. } => "missing_backtest_inputs",
        CoachFailure::Interrupted { .. } => "interrupted",
    }
}

/// The `disposition` column value — the tag migration `0005`'s `CHECK` enumerates.
fn disposition_tag(disposition: &Disposition) -> &'static str {
    match disposition {
        Disposition::Proposed => "proposed",
        Disposition::Accepted { .. } => "accepted",
        Disposition::Rejected => "rejected",
        Disposition::Modified => "modified",
    }
}

/// Refuse an INITIAL turn that is not initial (r1.s4.w4).
///
/// Both write paths that create a turn — `save_session` and `finish_session` —
/// record the FIRST state of a proposal. A caller handing either one an
/// already-modified, accepted or rejected proposal, or one carrying a recorded
/// accept failure, is asking the store to skip the disposition rail entirely: the
/// row would land settled without ever passing the transition matrix that makes
/// settling honest. Refused here rather than half-written; `record_disposition` and
/// `CoachAcceptanceRepository` are where a proposal moves.
fn check_initial_proposal(session_id: &str, proposal: &Proposal) -> Result<(), DataError> {
    if proposal.disposition != Disposition::Proposed {
        return Err(DataError::Db(format!(
            "coaching session `{session_id}`: an initial turn records a `proposed` proposal, \
             not a `{}` one",
            proposal.disposition.kind()
        )));
    }
    if proposal.accept_failure.is_some() {
        return Err(DataError::Db(format!(
            "coaching session `{session_id}`: an initial turn carries no accept failure — \
             nothing has tried to accept it yet"
        )));
    }
    Ok(())
}

/// Insert the proposal row for a turn, on the caller's transaction.
///
/// ONE mapping, shared by `save_session` and `finish_session`, so the two paths
/// that create a turn cannot drift into writing different rows for the same
/// proposal.
async fn insert_proposal(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: &str,
    proposal: &Proposal,
) -> Result<(), DataError> {
    let proposal_id = Uuid::new_v4().to_string();
    let mutation =
        serde_json::to_string(&proposal.mutation).map_err(|e| DataError::Db(e.to_string()))?;
    let hypothesis = proposal.hypothesis.as_str().to_owned();
    let disposition = disposition_tag(&proposal.disposition);
    let child_version_id = proposal
        .disposition
        .child_version_id()
        .map(|v| v.as_str().to_owned());
    let accepted_run_id = proposal
        .disposition
        .accepted_run_id()
        .map(|r| r.as_str().to_owned());

    sqlx::query!(
        "INSERT INTO coaching_proposals \
         (id, session_id, mutation, hypothesis, disposition, child_version_id, \
          accepted_run_id, accept_failure_stage, accept_failure_detail) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL)",
        proposal_id,
        session_id,
        mutation,
        hypothesis,
        disposition,
        child_version_id,
        accepted_run_id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| DataError::Db(e.to_string()))?;
    Ok(())
}

/// Parse a JSON `TEXT` column into a deserializable value, fail-closed on a
/// malformed payload (mirror `llm_call_repo::parse_json`).
fn parse_json<T: serde::de::DeserializeOwned>(column: &str, s: &str) -> Result<T, DataError> {
    serde_json::from_str(s)
        .map_err(|e| DataError::Db(format!("malformed JSON in `{column}` = `{s}`: {e}")))
}

/// Rebuild a [`Disposition`] from its stored tag + the nullable child/run pair.
///
/// Fail-closed: `0008`'s `CHECK`s already guarantee `accepted` iff BOTH a child
/// version and its run, so a row that violates either half is corrupt and must not
/// read back as a plausible state. The run half is r1.s4.w4's — an accepted
/// proposal whose child was never re-backtested is precisely the shape release
/// exit criterion 4 forbids, and reading it as an ordinary accept would hide it.
fn parse_disposition(
    tag: &str,
    child_version_id: Option<String>,
    accepted_run_id: Option<String>,
) -> Result<Disposition, DataError> {
    match (tag, child_version_id, accepted_run_id) {
        ("proposed", None, None) => Ok(Disposition::Proposed),
        ("rejected", None, None) => Ok(Disposition::Rejected),
        ("modified", None, None) => Ok(Disposition::Modified),
        ("accepted", Some(child), Some(run)) => Ok(Disposition::Accepted {
            child_version_id: VersionId::new(child),
            accepted_run_id: BacktestRunId::new(run),
        }),
        ("accepted", child, run) => Err(DataError::Db(format!(
            "coaching_proposals: an accepted proposal must name both its child version \
             and its run (child {child:?}, run {run:?})"
        ))),
        (tag @ ("proposed" | "rejected" | "modified"), child, run) => Err(DataError::Db(format!(
            "coaching_proposals: disposition `{tag}` carries child_version_id {child:?} / \
             accepted_run_id {run:?}"
        ))),
        (other, _, _) => Err(DataError::Db(format!(
            "coaching_proposals: unknown disposition `{other}`"
        ))),
    }
}

/// Rebuild the latest [`CoachAcceptFailure`] from its stored stage + JSON detail.
///
/// Fail-closed on the same reasoning as `failure_kind`/`failure_detail`: the two
/// columns are written from ONE value, and the QUERYABLE one is the stage. A stage
/// scan for `backtest` that returns a row whose detail says `compile` is worse than
/// an error — it is a wrong answer to the question "where do accepts fail?".
fn parse_accept_failure(
    stage: Option<String>,
    detail: Option<String>,
) -> Result<Option<CoachAcceptFailure>, DataError> {
    match (stage, detail) {
        (None, None) => Ok(None),
        (Some(stage), Some(detail)) => {
            let failure: CoachAcceptFailure =
                parse_json("coaching_proposals.accept_failure_detail", &detail)?;
            if failure.stage.tag() != stage {
                return Err(DataError::Db(format!(
                    "coaching_proposals: accept_failure_stage `{stage}` disagrees with the \
                     recorded accept_failure_detail (`{}`)",
                    failure.stage.tag()
                )));
            }
            Ok(Some(failure))
        }
        (stage, detail) => Err(DataError::Db(format!(
            "coaching_proposals: half a recorded accept failure (stage {stage:?}, \
             detail {detail:?})"
        ))),
    }
}

impl<C: Clock + Send + Sync> CoachingRepository for SqliteCoachingRepo<C> {
    async fn claim_session(
        &self,
        claim: CoachSessionClaim,
    ) -> Result<CoachSessionClaimResult, DataError> {
        let id = claim.session_id.as_str().to_owned();
        let run = claim.backtest_run_id.as_str().to_owned();
        let version = claim.strategy_version_id.as_str().to_owned();
        let fingerprint = claim.request_fingerprint.as_str().to_owned();
        let schema_version = COACHING_SCHEMA_VERSION;

        // `created_at` comes from the CLAIM, not from this adapter's clock — the
        // one coaching timestamp that does. A claim's time is the time the turn
        // began, which the caller established when it built the request the
        // fingerprint covers; re-stamping it here would date the audit row to the
        // moment of a retry rather than the moment of the ask.
        //
        // A TEXT column is not a timestamp, so this is validated on the way in for
        // the same reason `get_session` validates it on the way out (PR #128,
        // finding H3): a row that reads back malformed forever is a worse outcome
        // than a write refused now.
        DateTime::parse_from_rfc3339(&claim.created_at).map_err(|e| {
            DataError::Db(format!(
                "coach session claim `{id}`: malformed created_at `{}`: {e}",
                claim.created_at
            ))
        })?;
        let created_at = claim.created_at.clone();

        // ONE statement, and it is a WRITE (the PR #128 finding-H2 ordering): an
        // `INSERT .. ON CONFLICT DO NOTHING` takes the lock immediately instead of
        // upgrading a read snapshot, so two processes racing the same claim get a
        // clean winner rather than `SQLITE_BUSY_SNAPSHOT`.
        let inserted = match sqlx::query!(
            "INSERT INTO coaching_sessions \
             (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
              failure_kind, failure_detail, schema_version, request_fingerprint) \
             VALUES (?1, ?2, ?3, ?4, NULL, 'pending', NULL, NULL, ?5, ?6) \
             ON CONFLICT(id) DO NOTHING",
            id,
            run,
            version,
            created_at,
            schema_version,
            fingerprint,
        )
        .execute(&self.pool)
        .await
        {
            Ok(result) => result.rows_affected(),
            Err(e) => {
                if !e
                    .as_database_error()
                    .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
                {
                    return Err(DataError::Db(e.to_string()));
                }
                match self.pending_claim_holder(&id, &run).await? {
                    Some(session) => {
                        return Ok(CoachSessionClaimResult::ExistingPending(session));
                    }
                    None => 0,
                }
            }
        };

        if inserted == 1 {
            return Ok(CoachSessionClaimResult::Claimed);
        }

        // The id is taken. Whether that is an idempotent hit or a COLLISION is
        // decided by the three identity columns, never by the id alone: reusing a
        // session id for a different run, version or request is a different turn
        // wearing the same key, and answering it with someone else's result would
        // be the worst possible kind of success.
        let existing = sqlx::query!(
            r#"SELECT
                 backtest_run_id     AS "backtest_run_id!: String",
                 strategy_version_id AS "strategy_version_id!: String",
                 request_fingerprint AS "request_fingerprint?: String"
               FROM coaching_sessions WHERE id = ?1"#,
            id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?
        .ok_or_else(|| {
            DataError::Db(format!(
                "coach session claim `{id}`: the row conflicted and then vanished"
            ))
        })?;

        for (label, want, got) in [
            ("backtest run", &run, &existing.backtest_run_id),
            ("strategy version", &version, &existing.strategy_version_id),
        ] {
            if want != got {
                return Err(DataError::Db(format!(
                    "coach session claim `{id}`: the id is already held by a turn on a \
                     different {label} (`{got}`, not `{want}`)"
                )));
            }
        }
        if existing.request_fingerprint.as_deref() != Some(fingerprint.as_str()) {
            return Err(DataError::Db(format!(
                "coach session claim `{id}`: the id is already held by a turn with a \
                 different request fingerprint"
            )));
        }

        let session = self.get_session(&claim.session_id).await?.ok_or_else(|| {
            DataError::Db(format!(
                "coach session claim `{id}`: the row conflicted and then vanished"
            ))
        })?;
        if matches!(session.outcome, SessionOutcome::Pending) {
            // Still open. The repository CANNOT see whether the process that
            // claimed it is alive, so it returns the row unchanged and refuses to
            // guess; `w1`'s single-flight owner is what decides.
            Ok(CoachSessionClaimResult::ExistingPending(session))
        } else {
            Ok(CoachSessionClaimResult::Existing(session))
        }
    }

    async fn finish_session(
        &self,
        session_id: &CoachingSessionId,
        outcome: InitialCoachOutcome,
    ) -> Result<CoachingSession, DataError> {
        let id = session_id.as_str().to_owned();
        let llm_call_id = outcome.llm_call_id.as_ref().map(|c| c.as_str().to_owned());

        let (tag, failure_kind_col, failure_detail) = match &outcome.outcome {
            SessionOutcome::Pending => {
                return Err(DataError::Db(format!(
                    "coaching session `{id}`: `pending` is a claim, not a settlement"
                )));
            }
            SessionOutcome::Proposed { proposal } => {
                check_initial_proposal(&id, proposal)?;
                (OUTCOME_PROPOSED, None, None)
            }
            SessionOutcome::Failed { failure } => {
                let detail =
                    serde_json::to_string(failure).map_err(|e| DataError::Db(e.to_string()))?;
                (OUTCOME_FAILED, Some(failure_kind(failure)), Some(detail))
            }
        };

        // Settling and attaching the proposal are ONE transaction, for the same
        // reason `save_session` is: a session that says `proposed` with no proposal
        // row is the half-written turn the never-silence guarantee exists to
        // prevent. The write leads (finding H2) and is CONDITIONAL on the row still
        // being pending, so a second settlement moves nothing rather than
        // overwriting the first.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        let affected = sqlx::query!(
            "UPDATE coaching_sessions \
             SET outcome = ?1, failure_kind = ?2, failure_detail = ?3, llm_call_id = ?4 \
             WHERE id = ?5 AND outcome = 'pending'",
            tag,
            failure_kind_col,
            failure_detail,
            llm_call_id,
            id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?
        .rows_affected();

        if affected != 1 {
            return Err(DataError::Db(format!(
                "coaching session `{id}`: no pending claim to settle (absent, or already \
                 settled — a turn settles exactly once)"
            )));
        }

        if let SessionOutcome::Proposed { proposal } = &outcome.outcome {
            insert_proposal(&mut tx, &id, proposal).await?;
        }

        tx.commit()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        self.get_session(session_id).await?.ok_or_else(|| {
            DataError::Db(format!(
                "coaching session `{id}`: settled and then vanished on read-back"
            ))
        })
    }

    async fn save_session(
        &self,
        session: &CoachingSession,
    ) -> Result<CoachingSessionId, DataError> {
        let id = session.id.as_str().to_owned();
        let backtest_run_id = session.backtest_run_id.as_str().to_owned();
        let strategy_version_id = session.strategy_version_id.as_str().to_owned();
        // `created_at` from the injected Clock, NOT the in-memory value.
        let created_at = self.now_rfc3339()?;
        let llm_call_id = session.llm_call_id.as_ref().map(|c| c.as_str().to_owned());
        let schema_version = COACHING_SCHEMA_VERSION;

        let (outcome, failure_kind_col, failure_detail) = match &session.outcome {
            // A claim is made by `claim_session`, which is the only path that
            // writes a request fingerprint — and `0008` requires one on every
            // pending row. Letting the initial-write path insert a `pending` row
            // would create a claim nothing can ever match.
            SessionOutcome::Pending => {
                return Err(DataError::Db(format!(
                    "coaching session `{id}`: a claim is written by claim_session, not by \
                     save_session"
                )));
            }
            SessionOutcome::Proposed { proposal } => {
                check_initial_proposal(&id, proposal)?;
                (OUTCOME_PROPOSED, None, None)
            }
            SessionOutcome::Failed { failure } => {
                let detail =
                    serde_json::to_string(failure).map_err(|e| DataError::Db(e.to_string()))?;
                (OUTCOME_FAILED, Some(failure_kind(failure)), Some(detail))
            }
        };

        // One turn, one transaction: a session without its proposal would be the
        // half-written record the never-silence guarantee exists to prevent.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        // `request_fingerprint` is written NULL on purpose (r1.s4.w4). This is the
        // Round-1 initial-write path: it has no request digest to record, and
        // `0008` allows a terminal row without one precisely so this path and the
        // copied `0005` rows stay legal. `w1` retires the production bypass; the
        // claim path is the one that keys a turn.
        sqlx::query!(
            "INSERT INTO coaching_sessions \
             (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
              failure_kind, failure_detail, schema_version, request_fingerprint) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
            id,
            backtest_run_id,
            strategy_version_id,
            created_at,
            llm_call_id,
            outcome,
            failure_kind_col,
            failure_detail,
            schema_version,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        if let SessionOutcome::Proposed { proposal } = &session.outcome {
            insert_proposal(&mut tx, &id, proposal).await?;
        }

        tx.commit()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        Ok(CoachingSessionId::new(id))
    }

    async fn get_session(
        &self,
        id: &CoachingSessionId,
    ) -> Result<Option<CoachingSession>, DataError> {
        self.fetch_session(id).await
    }

    async fn list_sessions_for_run(
        &self,
        run_id: &BacktestRunId,
    ) -> Result<Vec<CoachingSession>, DataError> {
        let run = run_id.as_str();
        // Ids first, then the full read per id: `get_session` is the one place the
        // fail-closed decoding lives, and a list that decoded rows a second way is
        // how the two drift apart. The index `idx_coaching_sessions_run` serves the
        // ordering; a run's turn count is small by construction (one per coach ask).
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM coaching_sessions WHERE backtest_run_id = ?1 \
             ORDER BY created_at, id",
        )
        .bind(run)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        let mut sessions = Vec::with_capacity(ids.len());
        for id in ids {
            let session_id = CoachingSessionId::new(id);
            // Fail-closed: a row listed and then unreadable is a corrupt record, not
            // a gap to skip (the `get_run` posture, not the catalog one).
            let session = self.get_session(&session_id).await?.ok_or_else(|| {
                DataError::Db(format!(
                    "coaching_sessions `{}` vanished between listing and read",
                    session_id.as_str()
                ))
            })?;
            sessions.push(session);
        }
        Ok(sessions)
    }

    async fn record_disposition(
        &self,
        id: &CoachingSessionId,
        disposition: &Disposition,
    ) -> Result<(), DataError> {
        let id_str = id.as_str();

        // The three targets this operation cannot honestly write.
        //
        // `Proposed`: nothing returns to the initial state — the same rule
        // `Proposal::transition` enforces, which the store must not be able to
        // contradict.
        //
        // `Modified`: a modify is an EDIT. `r1.s4`'s rail replaces the proposal's
        // stored `mutation` with the trader's version, and this operation writes
        // the disposition columns only — so recording `modified` here would move
        // the state while leaving the ORIGINAL mutation in the row: a proposal that
        // says it was edited and carries the un-edited value, with no way to tell
        // from the row which it is. Refused rather than half-written;
        // `record_modification` writes both in one statement.
        //
        // `Accepted` (r1.s4.w2, `pulseai-labs/pulse-trader#149`): accepted lineage
        // has exactly ONE writer, `CoachAcceptanceRepository::commit_acceptance`,
        // which mints the child version and its run inside the transaction that
        // settles the proposal. This operation could only be handed ids something
        // else minted, and a second writer of the same fact is how the version tree
        // acquires an edge nobody can reproduce. So the one target left is
        // `Rejected`.
        match disposition {
            Disposition::Proposed => {
                return Err(DataError::Db(format!(
                    "coaching session `{id_str}`: nothing returns to `proposed`"
                )));
            }
            Disposition::Modified => {
                return Err(DataError::Db(format!(
                    "coaching session `{id_str}`: a `modified` disposition must be written \
                     together with the edited mutation, not by itself"
                )));
            }
            Disposition::Accepted { .. } => {
                return Err(DataError::Db(format!(
                    "coaching session `{id_str}`: an accepted disposition is written only by \
                     `commit_acceptance`, which mints the child version and its run in the \
                     same transaction; this operation cannot be a second writer of accepted \
                     lineage"
                )));
            }
            Disposition::Rejected => {}
        }

        let tag = disposition_tag(disposition);
        // `Rejected` is now the only reachable target, so both links are NULL — but
        // they stay derived from the value rather than hard-coded, so a future
        // target that DOES name one cannot silently write NULL.
        let child_version_id = disposition
            .child_version_id()
            .map(|v| v.as_str().to_owned());
        let accepted_run_id = disposition.accepted_run_id().map(|r| r.as_str().to_owned());

        // One transaction: the conditional write and the read that interprets a
        // no-op must see the same row, or a concurrent accept turns "already
        // settled the same way" into "settled differently" between the two.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        // The WRITE comes first, and the provenance proof follows it inside the same
        // transaction (PR #128, finding H2). Reading first made this a DEFERRED
        // transaction that then had to upgrade to a write, and in WAL an upgrade
        // whose snapshot another commit has moved past fails immediately with
        // `SQLITE_BUSY_SNAPSHOT` — `busy_timeout` does not cover snapshot conflicts,
        // so two clients replaying one idempotent accept could collide. Taking the
        // write lock with the first statement removes the upgrade; the proof still
        // runs before any commit, so an invalid child rolls back unwritten.

        // CONDITIONAL on the current state, not a blind UPDATE. `Proposed` and
        // `Modified` are the two states a proposal may leave; `Accepted` and
        // `Rejected` are terminal (`Proposal::transition`). Without the predicate
        // this statement will happily re-point an accepted proposal at a second
        // child version — the exact thing the session-id idempotency key exists to
        // prevent — and report success.
        let affected = sqlx::query!(
            "UPDATE coaching_proposals \
             SET disposition = ?1, child_version_id = ?2, accepted_run_id = ?3, \
                 accept_failure_stage = NULL, accept_failure_detail = NULL \
             WHERE session_id = ?4 AND disposition IN ('proposed', 'modified')",
            tag,
            child_version_id,
            accepted_run_id,
            id_str,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?
        .rows_affected();

        if affected == 1 {
            // r1.s4.w2: the in-transaction child-provenance proof that used to run
            // here went with the accepted arm. `commit_acceptance` DERIVES the
            // parent and the strategy from the claimed session row rather than
            // accepting them from a caller, so the lineage this checked is no
            // longer constructible through any caller-supplied id; `0008`'s
            // `coaching_proposals_accept_lineage_*` triggers remain the SQL-layer
            // proof for the one writer that is left.
            tx.commit()
                .await
                .map_err(|e| DataError::Db(e.to_string()))?;
            return Ok(());
        }

        // Nothing moved. Either there is no proposal to disposition, or it is
        // already settled — and only one of those is benign.
        let existing = sqlx::query!(
            r#"SELECT
                 disposition      AS "disposition!: String",
                 child_version_id AS "child_version_id?: String",
                 accepted_run_id  AS "accepted_run_id?: String"
               FROM coaching_proposals WHERE session_id = ?1"#,
            id_str,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        let Some(row) = existing else {
            return Err(DataError::Db(format!(
                "no proposal to disposition for coaching session `{id_str}` \
                 (absent session, or a turn that failed)"
            )));
        };
        let current =
            parse_disposition(&row.disposition, row.child_version_id, row.accepted_run_id)?;

        // IDEMPOTENT, and only for the identical write. Replaying an accept must be
        // a no-op — that is what makes the session id an accept idempotency key —
        // but an accept naming a DIFFERENT child version is a second child for one
        // proposal, which is what the key exists to refuse. `Disposition`'s
        // equality compares the payload, so the two cases separate themselves.
        if &current == disposition {
            tx.commit()
                .await
                .map_err(|e| DataError::Db(e.to_string()))?;
            return Ok(());
        }

        Err(DataError::Db(format!(
            "coaching session `{id_str}`: the proposal is `{}` and may not be recorded as \
             `{}`",
            current.kind(),
            disposition.kind()
        )))
    }

    async fn record_modification(
        &self,
        id: &CoachingSessionId,
        mutation: &Mutation,
    ) -> Result<Proposal, DataError> {
        let id_str = id.as_str();
        let mutation_json =
            serde_json::to_string(mutation).map_err(|e| DataError::Db(e.to_string()))?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        // ONE statement writes the edited mutation AND the state, so a row can never
        // say "edited" while carrying the un-edited value. It is the transaction's
        // first statement — a WRITE, taking the lock outright rather than upgrading a
        // read snapshot (the PR #128 finding-H2 ordering) — and it is CONDITIONAL on
        // the proposal still being open, so a settled proposal cannot be re-edited.
        //
        // The stale accept failure goes with it: a failure recorded against the
        // PREVIOUS mutation says nothing about this one, and leaving it would show
        // the trader a reason that belongs to a value the row no longer holds.
        let affected = sqlx::query!(
            "UPDATE coaching_proposals \
             SET mutation = ?1, disposition = 'modified', \
                 accept_failure_stage = NULL, accept_failure_detail = NULL \
             WHERE session_id = ?2 AND disposition IN ('proposed', 'modified')",
            mutation_json,
            id_str,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?
        .rows_affected();

        if affected != 1 {
            return Err(DataError::Db(format!(
                "coaching session `{id_str}`: no open proposal to modify (absent session, a \
                 turn that failed, or an already-settled proposal)"
            )));
        }

        // Read back on the SAME transaction, so what is returned is what committed.
        let proposal = fetch_proposal_tx(&mut tx, id_str).await?.ok_or_else(|| {
            DataError::Db(format!(
                "coaching session `{id_str}`: the proposal vanished between write and read"
            ))
        })?;
        tx.commit()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;
        Ok(proposal)
    }
}

impl<C: Clock> SqliteCoachingRepo<C> {
    /// The session row for `id`, decoded fail-closed — [`CoachingRepository::get_session`]'s
    /// body, extracted so `claim_session`'s run-level-conflict path can read a session
    /// WITHOUT going through the trait (r2.s1.w1 / #158).
    async fn fetch_session(
        &self,
        id: &CoachingSessionId,
    ) -> Result<Option<CoachingSession>, DataError> {
        let id_str = id.as_str();
        let row = sqlx::query!(
            r#"SELECT
                 id                  AS "id!: String",
                 backtest_run_id     AS "backtest_run_id!: String",
                 strategy_version_id AS "strategy_version_id!: String",
                 created_at          AS "created_at!: String",
                 llm_call_id         AS "llm_call_id?: String",
                 outcome             AS "outcome!: String",
                 failure_kind        AS "failure_kind?: String",
                 failure_detail      AS "failure_detail?: String",
                 schema_version      AS "schema_version!: i64"
               FROM coaching_sessions WHERE id = ?1"#,
            id_str,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        let Some(r) = row else { return Ok(None) };

        // The stored schema_version is load-bearing — reject an unsupported tag
        // hard (fail-closed, mirror `get_call`).
        if r.schema_version != COACHING_SCHEMA_VERSION {
            return Err(DataError::Db(format!(
                "unsupported coaching_sessions schema version {}",
                r.schema_version
            )));
        }

        // `created_at` is a TEXT column, so nothing but this stops a row written
        // around the adapter from reading back as a timestamp it is not (PR #128,
        // finding H3; `get_call` already takes this posture). It is a CHECK, not a
        // normalisation: the stored text is returned unchanged, because rewriting an
        // audit value on read would make the row disagree with itself.
        DateTime::parse_from_rfc3339(&r.created_at).map_err(|e| {
            DataError::Db(format!(
                "coaching_sessions `{}`: malformed created_at `{}`: {e}",
                r.id, r.created_at
            ))
        })?;

        let outcome = match r.outcome.as_str() {
            // A claim, still open. `0008`'s CHECKs already forbid a pending row
            // from naming a ledger call or a failure; the proposal is the one
            // contradiction they cannot state (it lives in the other table), so the
            // read fails closed on it rather than returning a claim that quietly
            // owns an outcome.
            OUTCOME_PENDING => {
                if self.fetch_proposal(&r.id).await?.is_some() {
                    return Err(DataError::Db(format!(
                        "coaching_sessions `{}` is still pending and already carries a proposal",
                        r.id
                    )));
                }
                SessionOutcome::Pending
            }
            OUTCOME_PROPOSED => {
                let proposal = self.fetch_proposal(&r.id).await?.ok_or_else(|| {
                    DataError::Db(format!(
                        "coaching_sessions `{}` records a proposal but has no proposal row",
                        r.id
                    ))
                })?;
                SessionOutcome::Proposed { proposal }
            }
            OUTCOME_FAILED => {
                let detail = r.failure_detail.ok_or_else(|| {
                    DataError::Db(format!(
                        "coaching_sessions `{}` records a failure with no failure_detail",
                        r.id
                    ))
                })?;
                let stored_kind = r.failure_kind.ok_or_else(|| {
                    DataError::Db(format!(
                        "coaching_sessions `{}` records a failure with no failure_kind",
                        r.id
                    ))
                })?;
                let failure: CoachFailure =
                    parse_json("coaching_sessions.failure_detail", &detail)?;
                // `failure_kind` and `failure_detail` are written from the SAME
                // value, so a row where they disagree was written around this
                // adapter — and it is the QUERYABLE column that disagrees. A
                // `failure_kind` index scan for `provider_timeout` that returns a
                // row whose detail is a `zero_calls` is worse than an error: it is
                // a wrong answer to an audit question. Fail closed, the posture the
                // rest of this file already takes.
                let decoded_kind = failure_kind(&failure);
                if stored_kind != decoded_kind {
                    return Err(DataError::Db(format!(
                        "coaching_sessions `{}`: failure_kind `{stored_kind}` disagrees with \
                         the recorded failure_detail (`{decoded_kind}`)",
                        r.id
                    )));
                }
                SessionOutcome::Failed { failure }
            }
            other => {
                return Err(DataError::Db(format!(
                    "coaching_sessions: unknown outcome `{other}`"
                )));
            }
        };

        Ok(Some(CoachingSession {
            id: CoachingSessionId::new(r.id),
            backtest_run_id: BacktestRunId::new(r.backtest_run_id),
            strategy_version_id: VersionId::new(r.strategy_version_id),
            created_at: r.created_at,
            llm_call_id: r.llm_call_id.map(LlmCallId::new),
            outcome,
        }))
    }

    /// The session holding `run`'s pending claim after the insert was refused by
    /// `coaching_sessions_one_pending_per_run`, or `None` when OUR id already has
    /// a row — in which case the identity path applies its usual rules.
    ///
    /// r2.s1.w1 / #158: the only uniqueness constraint an `ON CONFLICT(id) DO
    /// NOTHING` insert can still hit is 0009's partial index — a `PRIMARY KEY`
    /// conflict is suppressed by the arbiter. Whichever index SQLite checked
    /// first decides the shape of the answer; a DIFFERENT pending row is the
    /// run-level claim, reported verbatim — the same answer the process-local
    /// registry gives.
    async fn pending_claim_holder(
        &self,
        id: &str,
        run: &str,
    ) -> Result<Option<CoachingSession>, DataError> {
        let ours = sqlx::query_scalar!(
            r#"SELECT id AS "id!: String" FROM coaching_sessions WHERE id = ?1"#,
            id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;
        if ours.is_some() {
            return Ok(None);
        }
        let pending = sqlx::query_scalar!(
            r#"SELECT id AS "id!: String" FROM coaching_sessions
               WHERE backtest_run_id = ?1 AND outcome = 'pending'"#,
            run,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;
        match pending {
            Some(pending_id) => Ok(Some(
                self.fetch_session(&CoachingSessionId::new(&pending_id))
                    .await?
                    .ok_or_else(|| {
                        DataError::Db(format!(
                            "coach session claim `{id}`: the pending claim that \
                             blocked the insert vanished before it could be read"
                        ))
                    })?,
            )),
            None => Err(DataError::Db(format!(
                "coach session claim `{id}`: the pending claim that blocked the \
                 insert settled before it could be read; retry the claim"
            ))),
        }
    }

    /// The proposal row for a session, decoded into the domain type.
    async fn fetch_proposal(&self, session_id: &str) -> Result<Option<Proposal>, DataError> {
        let row = sqlx::query!(
            r#"SELECT
                 mutation              AS "mutation!: String",
                 hypothesis            AS "hypothesis!: String",
                 disposition           AS "disposition!: String",
                 child_version_id      AS "child_version_id?: String",
                 accepted_run_id       AS "accepted_run_id?: String",
                 accept_failure_stage  AS "accept_failure_stage?: String",
                 accept_failure_detail AS "accept_failure_detail?: String"
               FROM coaching_proposals WHERE session_id = ?1"#,
            session_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        let Some(r) = row else { return Ok(None) };
        decode_proposal(ProposalColumns {
            mutation: &r.mutation,
            hypothesis: r.hypothesis,
            disposition: &r.disposition,
            child_version_id: r.child_version_id,
            accepted_run_id: r.accepted_run_id,
            accept_failure_stage: r.accept_failure_stage,
            accept_failure_detail: r.accept_failure_detail,
        })
        .map(Some)
    }
}

/// The `coaching_proposals` columns [`decode_proposal`] needs, as read.
///
/// A named tuple rather than seven positional arguments: three of them are
/// `Option<String>` and two more are `&str`, which is precisely the shape where a
/// silent argument swap reads back a plausible wrong proposal.
pub(crate) struct ProposalColumns<'a> {
    /// The serde-JSON `Mutation`.
    pub mutation: &'a str,
    /// The stated hypothesis.
    pub hypothesis: String,
    /// The disposition tag.
    pub disposition: &'a str,
    /// The accepted child version, if any.
    pub child_version_id: Option<String>,
    /// The accepted run, if any.
    pub accepted_run_id: Option<String>,
    /// The latest accept failure's stage, if any.
    pub accept_failure_stage: Option<String>,
    /// The latest accept failure's serde-JSON detail, if any.
    pub accept_failure_detail: Option<String>,
}

/// Decode one proposal row into the domain type, fail-closed.
///
/// ONE decode, shared by the pool read and the transaction read (r1.s4.w4): a
/// second decoding of the same columns is how two readers come to disagree about
/// what a row means, and this one is the fail-closed half of every guarantee `0008`
/// states in SQL.
///
/// # Errors
///
/// Returns [`DataError::Db`] on a malformed mutation payload, a blank hypothesis,
/// an unknown or half-populated disposition, or a stage/detail disagreement.
pub(crate) fn decode_proposal(row: ProposalColumns<'_>) -> Result<Proposal, DataError> {
    let mutation: Mutation = parse_json("coaching_proposals.mutation", row.mutation)?;
    // A stored hypothesis that is blank is a corrupt row, not an empty one: the
    // domain type refuses it and so does the `0005`/`0008` CHECK.
    let hypothesis = Hypothesis::new(row.hypothesis)
        .map_err(|e| DataError::Db(format!("coaching_proposals.hypothesis: {e}")))?;
    let disposition =
        parse_disposition(row.disposition, row.child_version_id, row.accepted_run_id)?;
    let accept_failure = parse_accept_failure(row.accept_failure_stage, row.accept_failure_detail)?;

    Ok(Proposal {
        mutation,
        hypothesis,
        disposition,
        accept_failure,
    })
}

/// The proposal row for a session, read on the CALLER'S transaction.
///
/// The accept path needs the proposal it is about to settle to be read inside the
/// transaction that settles it — a read on the pool would be a different snapshot,
/// which is exactly how "already accepted" and "open" swap places under a retry.
///
/// # Errors
///
/// Returns [`DataError::Db`] on a store failure or a corrupt row (see
/// [`decode_proposal`]).
pub(crate) async fn fetch_proposal_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: &str,
) -> Result<Option<Proposal>, DataError> {
    let row = sqlx::query!(
        r#"SELECT
             mutation              AS "mutation!: String",
             hypothesis            AS "hypothesis!: String",
             disposition           AS "disposition!: String",
             child_version_id      AS "child_version_id?: String",
             accepted_run_id       AS "accepted_run_id?: String",
             accept_failure_stage  AS "accept_failure_stage?: String",
             accept_failure_detail AS "accept_failure_detail?: String"
           FROM coaching_proposals WHERE session_id = ?1"#,
        session_id,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| DataError::Db(e.to_string()))?;

    let Some(r) = row else { return Ok(None) };
    decode_proposal(ProposalColumns {
        mutation: &r.mutation,
        hypothesis: r.hypothesis,
        disposition: &r.disposition,
        child_version_id: r.child_version_id,
        accepted_run_id: r.accepted_run_id,
        accept_failure_stage: r.accept_failure_stage,
        accept_failure_detail: r.accept_failure_detail,
    })
    .map(Some)
}
