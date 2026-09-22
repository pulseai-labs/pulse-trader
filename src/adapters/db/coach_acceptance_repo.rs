//! The `SQLite` adapter implementing the [`CoachAcceptanceRepository`] port
//! (r1.s4.w4, ADR-0010 / ADR-0019 / ADR-0021 as amended).
//!
//! **One accept is one transaction.** `commit_acceptance` writes the child
//! `strategy_version`, its `backtest_run`, every `trade`, and the proposal's
//! `disposition` + `child_version_id` + `accepted_run_id` — then commits once. Any
//! failure rolls the whole thing back, so the two shapes release exit criterion 4
//! forbids are unreachable by construction: an accepted proposal with no child, and
//! a child with neither a run nor a recorded failure.
//!
//! **No transaction spans network or CPU work.** Everything expensive — re-applying
//! the mutation, loading snapshots, running the deterministic backtest — has
//! already happened by the time a [`PreparedCoachAcceptance`] exists. This file
//! does column mapping and nothing else.
//!
//! **Identity is minted here, provenance is derived here.**
//! [`PreparedCoachAcceptance`] deliberately carries no `VersionId`, no
//! `BacktestRunId` and no timestamp; the child id, the run id and `created_at` come
//! from the injected [`IdSource`] / [`Clock`], and the strategy id, the parent
//! version, [`CreatedBy::CoachLlm`] and the creating call id are read out of the
//! CLAIMED SESSION ROW. A caller cannot supply provenance that disagrees with the
//! session because it has nowhere to put it.
//!
//! **The mappings are reused, not copied.** The child version row goes through
//! `strategy_repo::insert_version_row` and the run/trades through
//! `backtest_run_repo::{insert_run_row, insert_trade_rows}` — the same code the
//! ordinary create paths use. A second mapping for these rows would be a second
//! place for money columns and `version_hash` to drift.
//!
//! NO `#[derive(Debug)]` on the repo struct: neither `C: Clock` nor `I: IdSource`
//! carries a `Debug` bound (mirror `SqliteCoachingRepo`).

use chrono::{DateTime, SecondsFormat};
use sqlx::SqlitePool;

use crate::adapters::clock::SystemClock;
use crate::adapters::db::backtest_run_repo::{
    check_inputs_path_safe, insert_run_row, insert_trade_rows,
};
use crate::adapters::db::coaching_repo::fetch_proposal_tx;
use crate::adapters::db::strategy_repo::{VersionInsert, insert_version_row, version_hash};
use crate::adapters::db::walk_forward_run_repo::insert_walk_forward_in_tx;
use crate::adapters::ids::UuidIdSource;
use crate::domain::backtest::{BacktestRunId, WalkForwardRunId};
use crate::domain::strategy::{CreatedBy, VersionId};
use crate::domain::{
    AcceptedCoachOutcome, Clock, CoachAcceptFailure, CoachAcceptanceRepository, CoachingSessionId,
    DataError, Disposition, IdSource, PreparedCoachAcceptance, Proposal, SchemaVersion,
};

/// The `SQLite` [`CoachAcceptanceRepository`](crate::domain::port::CoachAcceptanceRepository)
/// adapter over `pulse.db`.
pub struct SqliteCoachAcceptanceRepo<C: Clock, I: IdSource> {
    pool: SqlitePool,
    clock: C,
    ids: I,
}

impl SqliteCoachAcceptanceRepo<SystemClock, UuidIdSource> {
    /// The production constructor: wall-clock time and v4 UUID ids.
    #[must_use]
    pub fn new(pool: SqlitePool) -> SqliteCoachAcceptanceRepo<SystemClock, UuidIdSource> {
        SqliteCoachAcceptanceRepo {
            pool,
            clock: SystemClock,
            ids: UuidIdSource,
        }
    }
}

impl<C: Clock, I: IdSource> SqliteCoachAcceptanceRepo<C, I> {
    /// The test/injection seam: supply a [`Clock`] and an [`IdSource`] so the ids
    /// and the timestamp this adapter MINTS are deterministic (mirror
    /// `SqliteCoachingRepo::with_deps`).
    #[must_use]
    pub fn with_deps(pool: SqlitePool, clock: C, ids: I) -> SqliteCoachAcceptanceRepo<C, I> {
        SqliteCoachAcceptanceRepo { pool, clock, ids }
    }

    /// The current `created_at`, RFC3339-millis UTC, from the injected [`Clock`].
    fn now_rfc3339(&self) -> Result<String, DataError> {
        let now_ms = self.clock.now_ms();
        let dt = DateTime::from_timestamp_millis(now_ms).ok_or_else(|| {
            DataError::Db(format!("clock.now_ms() {now_ms} is out of DateTime range"))
        })?;
        Ok(dt.to_rfc3339_opts(SecondsFormat::Millis, true))
    }
}

/// The session facts an accept derives its provenance from.
///
/// Every field is an id, which is what `struct_field_names` objects to — and here
/// the postfix is carrying weight rather than repeating the struct's name. Dropping
/// it would leave `strategy: String` and `llm_call: String`, which read as the
/// entities rather than as references to them, in a file whose entire hazard is
/// confusing one id for another.
#[allow(clippy::struct_field_names)]
struct CoachedSession {
    /// The version the coach read — the child's parent.
    parent_version_id: String,
    /// That version's owning strategy — the child's strategy.
    strategy_id: String,
    /// The one attributable provider call.
    llm_call_id: String,
}

impl<C: Clock + Send + Sync, I: IdSource + Send + Sync> CoachAcceptanceRepository
    for SqliteCoachAcceptanceRepo<C, I>
{
    async fn record_accept_failure(
        &self,
        session_id: &CoachingSessionId,
        failure: CoachAcceptFailure,
    ) -> Result<Proposal, DataError> {
        let id = session_id.as_str();
        let stage = failure.stage.tag();
        let detail = serde_json::to_string(&failure).map_err(|e| DataError::Db(e.to_string()))?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        // The WRITE leads (the PR #128 finding-H2 ordering), and it is CONDITIONAL
        // on the proposal still being open. A blind UPDATE would happily stamp a
        // failure onto a settled proposal — `0008` would refuse the accepted case,
        // but the rejected case would land, recording that someone tried to accept
        // a proposal the trader had already thrown away.
        let affected = sqlx::query!(
            "UPDATE coaching_proposals \
             SET accept_failure_stage = ?1, accept_failure_detail = ?2 \
             WHERE session_id = ?3 AND disposition IN ('proposed', 'modified')",
            stage,
            detail,
            id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?
        .rows_affected();

        if affected != 1 {
            return Err(DataError::Db(format!(
                "coaching session `{id}`: no open proposal to record an accept failure \
                 against (absent session, a turn that failed, or an already-settled \
                 proposal)"
            )));
        }

        // Read it back on the SAME transaction, so what is returned is what
        // committed rather than whatever a second snapshot happens to hold.
        let proposal = fetch_proposal_tx(&mut tx, id).await?.ok_or_else(|| {
            DataError::Db(format!(
                "coaching session `{id}`: the proposal vanished between write and read"
            ))
        })?;
        tx.commit()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;
        Ok(proposal)
    }

    async fn commit_acceptance(
        &self,
        acceptance: PreparedCoachAcceptance,
    ) -> Result<AcceptedCoachOutcome, DataError> {
        let id = acceptance.session_id.as_str().to_owned();
        let prepared = &acceptance.prepared_run;
        let serialized = preflight(&acceptance)?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        // The transaction's FIRST statement is a WRITE (finding H2): it takes the
        // lock outright instead of upgrading a read snapshot, which in WAL fails
        // immediately with `SQLITE_BUSY_SNAPSHOT` that `busy_timeout` does not
        // cover. It is not a throwaway lock-grab either — clearing any stale accept
        // failure is a step this accept owes anyway, and doing it here means a
        // successful accept clears it inside the same transaction that writes the
        // child, never as a second act that could fail on its own.
        let opened = sqlx::query!(
            "UPDATE coaching_proposals \
             SET accept_failure_stage = NULL, accept_failure_detail = NULL \
             WHERE session_id = ?1 AND disposition IN ('proposed', 'modified')",
            id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?
        .rows_affected();

        let proposal = fetch_proposal_tx(&mut tx, &id).await?.ok_or_else(|| {
            DataError::Db(format!(
                "coaching session `{id}`: no proposal to accept (absent session, or a turn \
                 that failed)"
            ))
        })?;

        // THE PROPOSAL MUST STILL SAY WHAT THE CHILD WAS BUILT FROM, and this is
        // checked BEFORE the already-accepted branch below, not after it.
        //
        // A modify recorded while this accept was applying, loading and re-running —
        // all of it necessarily outside this transaction — leaves the proposal
        // carrying a DIFFERENT mutation. Two shapes follow from that, and the guard
        // has to cover both:
        //
        //   - the proposal is still open: writing now would record a child its own
        //     stored mutation did not produce, with every constraint passing;
        //   - the proposal was already ACCEPTED by the process that modified it:
        //     returning its child and run ids as an idempotent replay would tell
        //     THIS caller that the mutation their trader reviewed was accepted,
        //     when the stored child came from someone else's.
        //
        // Checking after the replay branch caught only the first. It also put this
        // adapter out of step with the in-memory one, which checks first — and two
        // adapters disagreeing about an invariant is the defect this guard exists
        // to prevent, one layer up.
        if proposal.mutation != acceptance.expected_mutation {
            return Err(DataError::Db(format!(
                "coaching session `{id}`: the proposal changed while this accept was being \
                 computed, so the child would not match the mutation now on record; \
                 re-run the accept against the current proposal"
            )));
        }

        // THE PARENT'S CERTIFICATION POINTER MUST STILL BE THE ONE THE GATE READ
        // — the second optimistic lock, beside the mutation guard and checked
        // BEFORE the already-accepted branch for the same reason.
        //
        // The gate walked the candidate forward on the parameters of the
        // walk-forward run this pointer named when the parent was loaded — all
        // of it outside this transaction. A `save_walk_forward_run` landing in
        // that window moves `latest_walk_forward_run_id`, and committing then
        // certifies the child from fold parameters that are no longer the
        // parent's current ones, with every constraint still passing.
        //
        // Firing before the replay branch costs a concurrent-accept caller one
        // re-run that lands on the honest replay; letting a stale pointer slip
        // through instead commits a child certified against a run the parent no
        // longer names.
        let current_pointer = sqlx::query!(
            r#"SELECT v.latest_walk_forward_run_id AS "pointer?: String"
               FROM coaching_sessions s
               JOIN strategy_version v ON v.id = s.strategy_version_id
               WHERE s.id = ?1"#,
            id,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?
        .and_then(|row| row.pointer);
        if current_pointer.as_deref()
            != acceptance
                .expected_certification_pointer
                .as_ref()
                .map(|p| p.as_str())
        {
            return Err(DataError::Db(format!(
                "coaching session `{id}`: the parent's certification pointer moved while this \
                 accept was being computed, so the child would be certified from stale fold \
                 parameters; re-run the accept against the current state"
            )));
        }

        if opened != 1 {
            // Nothing was open. Either this is the retry of an accept that already
            // landed — the session id IS the accept idempotency key, so that must
            // succeed and insert nothing — or the proposal is settled some other
            // way, which is not something an accept may undo.
            return replay_acceptance(&mut tx, &id, &proposal).await;
        }

        let session = self.coached_session(&mut tx, &id).await?;

        // Mint identity INSIDE the transaction, from the injected sources. The
        // walk-forward id is minted only when a certification draft came through
        // the gate (D2): no draft, no id, no pointer.
        let minted = MintedAcceptance {
            child_id: self.ids.next_id(),
            run_id: self.ids.next_id(),
            walk_forward_run_id: acceptance.walk_forward.as_ref().map(|_| self.ids.next_id()),
            created_at: self.now_rfc3339()?,
        };

        insert_child_version(&mut tx, &session, &minted, &serialized).await?;

        // The run is written against the MINTED child, and the trades against the
        // MINTED run — the two links `0008`'s lineage trigger then re-checks from
        // the other side.
        insert_run_row(
            &mut tx,
            &minted.run_id,
            &minted.child_id,
            &minted.created_at,
            &prepared.inputs,
            &prepared.result,
            &prepared.summary,
            prepared.starting_equity,
            None,
        )
        .await?;
        insert_trade_rows(&mut tx, &minted.run_id, &prepared.result.trades).await?;

        // The certification run persists in the SAME transaction and moves the
        // child's `latest_walk_forward_run_id` pointer — the child is born
        // certified exactly when its gate-passing draft lands.
        if let (Some(draft), Some(wf_id)) = (
            &acceptance.walk_forward,
            minted.walk_forward_run_id.as_deref(),
        ) {
            insert_walk_forward_in_tx(&mut tx, &minted.child_id, draft, wf_id, &minted.created_at)
                .await?;
        }

        settle_proposal_tx(&mut tx, &id, &minted.child_id, &minted.run_id).await?;

        tx.commit()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        Ok(AcceptedCoachOutcome {
            child_version_id: VersionId::new(minted.child_id),
            accepted_run_id: BacktestRunId::new(minted.run_id),
            walk_forward_run_id: minted.walk_forward_run_id.map(WalkForwardRunId::new),
        })
    }
}

/// Everything `commit_acceptance` settles BEFORE the transaction opens:
/// path-safety on every persisted data-version tag (the prepared run's and —
/// exactly as `save_walk_forward_run` does it — every certification fold's)
/// plus the child's serialized columns. None of it can fail for a reason a
/// write lock should be held for.
fn preflight(acceptance: &PreparedCoachAcceptance) -> Result<SerializedChild, DataError> {
    check_inputs_path_safe(&acceptance.prepared_run.inputs)?;
    if let Some(draft) = &acceptance.walk_forward {
        for fold in &draft.folds {
            check_inputs_path_safe(&fold.inputs)?;
        }
    }
    Ok(SerializedChild {
        child_dsl_json: serde_json::to_string(&acceptance.child_dsl)
            .map_err(|e| DataError::Db(e.to_string()))?,
        schema_version: SchemaVersion::CURRENT.to_string(),
        created_by: serde_json::to_string(&CreatedBy::CoachLlm)
            .map_err(|e| DataError::Db(e.to_string()))?,
    })
}

/// The child row's serialized columns — computed before the transaction opens
/// because none of them can fail for a reason a write lock should be held for.
struct SerializedChild {
    /// The candidate DSL as JSON (`dsl` and `dsl_original` are the same bytes —
    /// a coach-minted child has no authoring history to claim).
    child_dsl_json: String,
    /// `SchemaVersion::CURRENT` rendered for the `dsl_schema_version` column.
    schema_version: String,
    /// `CreatedBy::CoachLlm` serialized for the `created_by` column.
    created_by: String,
}

/// Everything `commit_acceptance` mints inside the transaction, from the
/// injected [`IdSource`]/[`Clock`].
struct MintedAcceptance {
    /// The child `strategy_version` id.
    child_id: String,
    /// The accepted re-backtest run id.
    run_id: String,
    /// The certification run's id — `Some` only when the accept carries a
    /// gate-passing draft to persist.
    walk_forward_run_id: Option<String>,
    /// The one timestamp every row shares.
    created_at: String,
}

/// The already-committed accept's answer, read on the same transaction.
///
/// The outcome names the child's CURRENT certification pointer (D5), not the
/// run the original accept persisted: a later walk-forward may have advanced
/// `latest_walk_forward_run_id`, and a replay must answer what is true now.
async fn replay_acceptance(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: &str,
    proposal: &Proposal,
) -> Result<AcceptedCoachOutcome, DataError> {
    match &proposal.disposition {
        Disposition::Accepted {
            child_version_id,
            accepted_run_id,
        } => {
            let cid = child_version_id.as_str();
            let row = sqlx::query!(
                r#"SELECT latest_walk_forward_run_id AS "p?: String"
                   FROM strategy_version WHERE id = ?1"#,
                cid,
            )
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| DataError::Db(e.to_string()))?
            .ok_or_else(|| {
                DataError::Db(format!(
                    "the accepted child version `{cid}` no longer exists"
                ))
            })?;
            Ok(AcceptedCoachOutcome {
                child_version_id: child_version_id.clone(),
                accepted_run_id: accepted_run_id.clone(),
                walk_forward_run_id: row.p.map(WalkForwardRunId::new),
            })
        }
        other => Err(DataError::Db(format!(
            "coaching session `{session_id}`: the proposal is `{}` and cannot be accepted",
            other.kind()
        ))),
    }
}

/// The child `strategy_version` row: hash + creating-call provenance derived
/// from the session row, then the shared `insert_version_row` mapping.
async fn insert_child_version(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session: &CoachedSession,
    minted: &MintedAcceptance,
    serialized: &SerializedChild,
) -> Result<(), DataError> {
    let llm_ids_json = serde_json::to_string(&vec![session.llm_call_id.clone()])
        .map_err(|e| DataError::Db(e.to_string()))?;
    let hash = version_hash(
        &session.strategy_id,
        Some(session.parent_version_id.as_str()),
        &serialized.schema_version,
        &serialized.child_dsl_json,
    );

    // The child is the `apply()` output, so `dsl` and `dsl_original` are the
    // same bytes: it was authored at the current schema and has never been
    // through a migration. Storing a different `dsl_original` would claim an
    // authoring history the child does not have.
    insert_version_row(
        tx,
        &VersionInsert {
            id: &minted.child_id,
            strategy_id: &session.strategy_id,
            parent_version_id: Some(&session.parent_version_id),
            dsl_schema_version: &serialized.schema_version,
            dsl: &serialized.child_dsl_json,
            dsl_original: &serialized.child_dsl_json,
            version_hash: &hash,
            created_by: &serialized.created_by,
            creating_llm_call_ids: &llm_ids_json,
            created_at: &minted.created_at,
        },
    )
    .await
}

/// Settle the proposal last: the links it names now exist, so `0008`'s lineage
/// trigger has something true to find.
async fn settle_proposal_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: &str,
    child_id: &str,
    run_id: &str,
) -> Result<(), DataError> {
    let settled = sqlx::query!(
        "UPDATE coaching_proposals \
         SET disposition = 'accepted', child_version_id = ?1, accepted_run_id = ?2 \
         WHERE session_id = ?3 AND disposition IN ('proposed', 'modified')",
        child_id,
        run_id,
        session_id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| DataError::Db(e.to_string()))?
    .rows_affected();
    if settled != 1 {
        return Err(DataError::Db(format!(
            "coaching session `{session_id}`: the proposal stopped being open mid-accept"
        )));
    }
    Ok(())
}

impl<C: Clock, I: IdSource> SqliteCoachAcceptanceRepo<C, I> {
    /// The session's provenance facts, read on the caller's transaction.
    ///
    /// Three refusals live here, and each is a false record it prevents rather than
    /// a defensive check: a session that is not `proposed` has no proposal an accept
    /// may settle; a session with no correlated `llm_call_id` cannot attribute the
    /// child to a coach call, and ADR-0010 says a coach-made version names the call
    /// that made it; and a session whose coached version has vanished cannot say
    /// what the child descends from.
    async fn coached_session(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        session_id: &str,
    ) -> Result<CoachedSession, DataError> {
        let row = sqlx::query!(
            r#"SELECT
                 s.outcome             AS "outcome!: String",
                 s.strategy_version_id AS "strategy_version_id!: String",
                 s.llm_call_id         AS "llm_call_id?: String",
                 v.strategy_id         AS "strategy_id!: String"
               FROM coaching_sessions s
               JOIN strategy_version v ON v.id = s.strategy_version_id
               WHERE s.id = ?1"#,
            session_id,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?
        .ok_or_else(|| {
            DataError::Db(format!(
                "coaching session `{session_id}`: no such session, or its coached version no \
                 longer exists"
            ))
        })?;

        if row.outcome != "proposed" {
            return Err(DataError::Db(format!(
                "coaching session `{session_id}`: a `{}` turn has no proposal to accept",
                row.outcome
            )));
        }
        let llm_call_id = row.llm_call_id.ok_or_else(|| {
            DataError::Db(format!(
                "coaching session `{session_id}`: no attributable llm_call, so an accepted \
                 child could not name the coach call that produced it (ADR-0010)"
            ))
        })?;

        Ok(CoachedSession {
            parent_version_id: row.strategy_version_id,
            strategy_id: row.strategy_id,
            llm_call_id,
        })
    }
}
