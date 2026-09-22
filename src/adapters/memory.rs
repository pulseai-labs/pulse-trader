//! In-memory adapters at product-owned seams (r1.s4.w4).
//!
//! [`InMemoryCoachAcceptanceRepo`] is a **test adapter, not a product shell**: the
//! seam it implements ([`CoachAcceptanceRepository`]) is owned by the product and
//! has a real `SQLite` implementation shipping alongside it. It exists so `w2`/`w3`
//! can drive the accept rail's decision logic without a database, and so a test can
//! assert on the identity the accept MINTS — which is only meaningful if the
//! in-memory adapter mints the same way the real one does.
//!
//! "The same way" is load-bearing and is what this file is careful about:
//!
//!   * ids come from the injected [`IdSource`] and `created_at` from the injected
//!     [`Clock`], in the same order (child id, then run id);
//!   * provenance is DERIVED from the registered turn — strategy, parent version,
//!     [`CreatedBy::CoachLlm`] and the creating call id — never taken from the
//!     caller, because [`PreparedCoachAcceptance`] has nowhere to put it;
//!   * the same refusals fire: a turn that is not `proposed`, a turn with no
//!     attributable call, a settled proposal, an accept failure on a settled
//!     proposal;
//!   * replaying an accept returns the existing exact pair and stores nothing new.
//!
//! What it deliberately does NOT do is enforce `0008`'s constraints a second time.
//! A double of the schema is a second schema, and the moment it disagrees with the
//! real one it starts certifying writes the database would refuse. The SQLite
//! adapter is the one that proves those rules hold; this one proves the decision
//! logic that sits on top of them.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Mutex;

use crate::domain::backtest::{BacktestRunId, WalkForwardRunId};
use crate::domain::strategy::{CreatedBy, StrategyId, VersionId};
use crate::domain::{
    AcceptedCoachOutcome, Clock, CoachAcceptFailure, CoachAcceptanceRepository, CoachingSessionId,
    DataError, Disposition, IdSource, LlmCallId, PreparedBacktest, PreparedCoachAcceptance,
    Proposal, SessionOutcome, StrategyDsl,
};

/// One recorded coach turn, as the in-memory adapter needs to see it.
///
/// Registered by the test rather than claimed through a repository: this adapter
/// stands in for the accept half of the seam only, so the turn that produced the
/// proposal is a precondition it is given, not one it creates.
#[derive(Debug, Clone)]
pub struct MemoryCoachTurn {
    /// The session id — also the accept idempotency key.
    pub session_id: CoachingSessionId,
    /// The strategy the coached version belongs to.
    pub strategy_id: StrategyId,
    /// The version the coach read; the child's parent.
    pub parent_version_id: VersionId,
    /// The one attributable provider call, when the turn correlated one.
    pub llm_call_id: Option<LlmCallId>,
    /// The coached parent's `latest_walk_forward_run_id` as the version store
    /// holds it. Registration seeds the certification map with it; a later
    /// [`InMemoryCoachAcceptanceRepo::record_certification`] is how a
    /// memory-side walk-forward save moves it.
    pub parent_certification: Option<WalkForwardRunId>,
    /// What the turn produced.
    pub outcome: SessionOutcome,
}

/// A child version an accept minted, with everything the real adapter would have
/// written into `strategy_version` and `backtest_run`.
#[derive(Debug, Clone)]
pub struct MemoryAcceptedChild {
    /// The minted child version id.
    pub child_version_id: VersionId,
    /// The minted run id.
    pub accepted_run_id: BacktestRunId,
    /// Derived from the turn, never from the caller.
    pub strategy_id: StrategyId,
    /// Derived from the turn, never from the caller.
    pub parent_version_id: VersionId,
    /// Always [`CreatedBy::CoachLlm`] — an accepted child is coach-made.
    pub created_by: CreatedBy,
    /// The creating call, derived from the turn.
    pub creating_llm_call_ids: Vec<String>,
    /// Minted from the injected clock.
    pub created_at: String,
    /// The validated child candidate, as `apply()` produced it.
    pub child_dsl: StrategyDsl,
    /// The deterministic re-backtest that was committed with it.
    pub prepared_run: PreparedBacktest,
    /// The certification run id minted for a gate-passing accept (r2.s3.w4) —
    /// what the accept WROTE as the child's first pointer. The replay does not
    /// read this field: the child's CURRENT pointer is the certification map's
    /// entry (a later save moves it), so this records the accept's own act only.
    pub walk_forward_run_id: Option<WalkForwardRunId>,
}

/// The mutable half, behind one lock.
#[derive(Debug, Default)]
struct MemoryState {
    turns: BTreeMap<String, MemoryCoachTurn>,
    children: Vec<MemoryAcceptedChild>,
    /// The `strategy_version.latest_walk_forward_run_id` column, modelled per
    /// version id: the certification pointer a walk-forward save moves and the
    /// accept commit's optimistic lock re-reads — the same cell the SQLite
    /// adapter guards. An accept that commits a gate-passing draft records the
    /// CHILD's initial pointer here too, so the entry is the child's column and
    /// the replay reads the current value from it.
    certification: BTreeMap<String, WalkForwardRunId>,
}

/// A deterministic in-memory [`CoachAcceptanceRepository`].
pub struct InMemoryCoachAcceptanceRepo<C: Clock, I: IdSource> {
    clock: C,
    ids: I,
    state: Mutex<MemoryState>,
}

impl<C: Clock, I: IdSource> InMemoryCoachAcceptanceRepo<C, I> {
    /// A repository with no turns registered.
    #[must_use]
    pub fn new(clock: C, ids: I) -> Self {
        Self {
            clock,
            ids,
            state: Mutex::new(MemoryState::default()),
        }
    }

    /// Register the coach turn an accept will be made against.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] when the lock is poisoned.
    pub fn register_turn(&self, turn: MemoryCoachTurn) -> Result<(), DataError> {
        let mut state = self.lock()?;
        if let Some(pointer) = &turn.parent_certification {
            state
                .certification
                .insert(turn.parent_version_id.as_str().to_owned(), pointer.clone());
        }
        state
            .turns
            .insert(turn.session_id.as_str().to_owned(), turn);
        Ok(())
    }

    /// Move one version's certification pointer — the memory-side stand-in for
    /// `save_walk_forward_run`'s `latest_walk_forward_run_id` write, so a test
    /// can advance a pointer between an accept's computation and its commit.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] when the lock is poisoned.
    pub fn record_certification(
        &self,
        version_id: &VersionId,
        run_id: WalkForwardRunId,
    ) -> Result<(), DataError> {
        self.lock()?
            .certification
            .insert(version_id.as_str().to_owned(), run_id);
        Ok(())
    }

    /// The proposal currently stored for `session_id`, if the turn produced one.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] when the lock is poisoned.
    pub fn proposal(&self, session_id: &CoachingSessionId) -> Result<Option<Proposal>, DataError> {
        let state = self.lock()?;
        Ok(state
            .turns
            .get(session_id.as_str())
            .and_then(|turn| match &turn.outcome {
                SessionOutcome::Proposed { proposal } => Some(proposal.clone()),
                SessionOutcome::Pending | SessionOutcome::Failed { .. } => None,
            }))
    }

    /// Every child an accept has minted here, in mint order.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] when the lock is poisoned.
    pub fn accepted_children(&self) -> Result<Vec<MemoryAcceptedChild>, DataError> {
        Ok(self.lock()?.children.clone())
    }

    /// The state lock, as a `DataError` rather than a panic — the crate denies
    /// `unwrap`/`expect` on library paths.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>, DataError> {
        self.state
            .lock()
            .map_err(|_| DataError::Db("in-memory acceptance repo: poisoned lock".to_owned()))
    }

    /// The minted RFC3339-millis timestamp, from the injected clock.
    fn now_rfc3339(&self) -> Result<String, DataError> {
        let now_ms = self.clock.now_ms();
        let dt = chrono::DateTime::from_timestamp_millis(now_ms).ok_or_else(|| {
            DataError::Db(format!("clock.now_ms() {now_ms} is out of DateTime range"))
        })?;
        Ok(dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    }
}

/// The open proposal for a turn, or the reason there is none.
fn open_proposal<'a>(
    session_id: &str,
    turn: Option<&'a mut MemoryCoachTurn>,
) -> Result<&'a mut Proposal, DataError> {
    let turn = turn.ok_or_else(|| {
        DataError::Db(format!("coaching session `{session_id}`: no such session"))
    })?;
    match &mut turn.outcome {
        SessionOutcome::Proposed { proposal } => Ok(proposal),
        SessionOutcome::Pending | SessionOutcome::Failed { .. } => Err(DataError::Db(format!(
            "coaching session `{session_id}`: the turn produced no proposal to accept"
        ))),
    }
}

/// The idempotent-retry read of an already-accepted pair — the certification id
/// rides along from the child's CURRENT pointer, matching the SQLite replay's
/// `SELECT latest_walk_forward_run_id FROM strategy_version WHERE id = child`.
///
/// That pointer is read from the certification map — the column this model
/// carries — and NOT from the stored [`MemoryAcceptedChild`], which records only
/// what the accept itself wrote. The distinction is the whole point of the
/// replay: a later `record_certification(child, run)` (the memory-side
/// walk-forward save) must be visible here, exactly as the SQLite read sees the
/// moved column. A child the accept committed with no gate-passing draft has no
/// map entry, which is this model's NULL.
fn replayed_outcome(
    state: &MemoryState,
    child_version_id: &VersionId,
    accepted_run_id: &BacktestRunId,
) -> AcceptedCoachOutcome {
    let walk_forward_run_id = state.certification.get(child_version_id.as_str()).cloned();
    AcceptedCoachOutcome {
        child_version_id: child_version_id.clone(),
        accepted_run_id: accepted_run_id.clone(),
        walk_forward_run_id,
    }
}

impl<C: Clock + Send + Sync, I: IdSource + Send + Sync> CoachAcceptanceRepository
    for InMemoryCoachAcceptanceRepo<C, I>
{
    // The two trait methods are `fn ... -> impl Future`, not `async fn`. There is no
    // `.await` anywhere in this adapter and there never will be — doing no I/O is
    // precisely what it is for — so an `async fn` here would be a future that is
    // always ready pretending to be one that suspends. `std::future::ready` says the
    // true thing (and is what `tests/coach_failures.rs`'s fake repositories already
    // do). The work lives in the inherent sync methods below.
    fn record_accept_failure(
        &self,
        session_id: &CoachingSessionId,
        failure: CoachAcceptFailure,
    ) -> impl Future<Output = Result<Proposal, DataError>> + Send {
        std::future::ready(self.record_accept_failure_now(session_id, failure))
    }

    fn commit_acceptance(
        &self,
        acceptance: PreparedCoachAcceptance,
    ) -> impl Future<Output = Result<AcceptedCoachOutcome, DataError>> + Send {
        std::future::ready(self.commit_acceptance_now(acceptance))
    }
}

impl<C: Clock, I: IdSource> InMemoryCoachAcceptanceRepo<C, I> {
    /// [`CoachAcceptanceRepository::record_accept_failure`], synchronously.
    fn record_accept_failure_now(
        &self,
        session_id: &CoachingSessionId,
        failure: CoachAcceptFailure,
    ) -> Result<Proposal, DataError> {
        let id = session_id.as_str();
        let mut state = self.lock()?;
        let proposal = open_proposal(id, state.turns.get_mut(id))?;

        match proposal.disposition {
            Disposition::Proposed | Disposition::Modified => {}
            Disposition::Accepted { .. } | Disposition::Rejected => {
                return Err(DataError::Db(format!(
                    "coaching session `{id}`: the proposal is `{}` and is not an attempt that \
                     can still fail",
                    proposal.disposition.kind()
                )));
            }
        }

        // The LATEST outcome, so a second failed attempt replaces the first.
        proposal.accept_failure = Some(failure);
        Ok(proposal.clone())
    }

    /// [`CoachAcceptanceRepository::commit_acceptance`], synchronously.
    fn commit_acceptance_now(
        &self,
        acceptance: PreparedCoachAcceptance,
    ) -> Result<AcceptedCoachOutcome, DataError> {
        let id = acceptance.session_id.as_str().to_owned();

        // ONE lock for the whole operation — read, guard and mutate — because the
        // SQLite adapter does the same work in ONE transaction and a test adapter
        // that admits an interleaving the real one refuses is a test adapter that
        // certifies the wrong thing. Taking it twice let two concurrent accepts both
        // observe an open proposal and each mint a child.
        let mut state = self.lock()?;

        // The provenance the accept DERIVES from, read before anything is mutated so
        // a refused accept leaves the turn exactly as it was.
        let (strategy_id, parent_version_id, llm_call_id, disposition) = {
            let turn = state.turns.get(&id).cloned().ok_or_else(|| {
                DataError::Db(format!("coaching session `{id}`: no such session"))
            })?;
            // Read BEFORE `open_proposal` takes the turns borrow: the
            // certification map is the `latest_walk_forward_run_id` column this
            // model carries, and the guard below compares it inside the same
            // lock — the same optimistic-lock re-read the SQLite adapter makes
            // inside its transaction.
            let current_pointer = state
                .certification
                .get(turn.parent_version_id.as_str())
                .cloned();
            let proposal = open_proposal(&id, state.turns.get_mut(&id))?;

            // The proposal must still say what this child was built from — the same
            // optimistic-lock check the SQLite adapter makes inside its transaction.
            // Applying and re-running happen outside any lock, so a modify recorded
            // meanwhile leaves the proposal open with a DIFFERENT mutation, and
            // committing then records a child its stored mutation did not produce.
            if proposal.mutation != acceptance.expected_mutation {
                return Err(DataError::Db(format!(
                    "coaching session `{id}`: the proposal changed while this accept was being \
                     computed, so the child would not match the mutation now on record; \
                     re-run the accept against the current proposal"
                )));
            }

            // The parent's certification pointer must still be the one the gate
            // read — the second optimistic lock: a walk-forward landing between
            // the gate's computation and this commit certifies the child from
            // fold parameters that are no longer the parent's current ones.
            if current_pointer != acceptance.expected_certification_pointer {
                return Err(DataError::Db(format!(
                    "coaching session `{id}`: the parent's certification pointer moved while \
                     this accept was being computed, so the child would be certified from \
                     stale fold parameters; re-run the accept against the current state"
                )));
            }

            (
                turn.strategy_id,
                turn.parent_version_id,
                turn.llm_call_id,
                proposal.disposition.clone(),
            )
        };

        match &disposition {
            // The session id IS the accept idempotency key: a client that lost the
            // response retries, and the retry returns the existing exact pair.
            Disposition::Accepted {
                child_version_id,
                accepted_run_id,
            } => {
                return Ok(replayed_outcome(&state, child_version_id, accepted_run_id));
            }
            Disposition::Rejected => {
                return Err(DataError::Db(format!(
                    "coaching session `{id}`: a rejected proposal cannot be accepted"
                )));
            }
            Disposition::Proposed | Disposition::Modified => {}
        }

        let llm_call_id = llm_call_id.ok_or_else(|| {
            DataError::Db(format!(
                "coaching session `{id}`: no attributable llm_call, so an accepted child could \
                 not name the coach call that produced it (ADR-0010)"
            ))
        })?;

        // Mint in the SAME order the SQLite adapter does — child, run, then the
        // certification id when a gate-passing draft came through — so a test
        // written against one reads the same ids from the other.
        let child_version_id = VersionId::new(self.ids.next_id());
        let accepted_run_id = BacktestRunId::new(self.ids.next_id());
        let walk_forward_run_id = acceptance
            .walk_forward
            .as_ref()
            .map(|_| WalkForwardRunId::new(self.ids.next_id()));
        let created_at = self.now_rfc3339()?;

        state.children.push(MemoryAcceptedChild {
            child_version_id: child_version_id.clone(),
            accepted_run_id: accepted_run_id.clone(),
            strategy_id,
            parent_version_id,
            created_by: CreatedBy::CoachLlm,
            creating_llm_call_ids: vec![llm_call_id.as_str().to_owned()],
            created_at,
            child_dsl: acceptance.child_dsl,
            prepared_run: acceptance.prepared_run,
            walk_forward_run_id: walk_forward_run_id.clone(),
        });

        // The child's `latest_walk_forward_run_id` cell, written in the same act
        // as its row — the SQLite `insert_child_version` carries it on the INSERT.
        // A gate-passing draft's run id is the child's FIRST pointer; a child
        // accepted without one simply has no entry, which is this model's NULL.
        // The replay reads THIS, not the row above, so a later
        // `record_certification(child, run)` is visible to it.
        if let Some(run_id) = &walk_forward_run_id {
            state
                .certification
                .insert(child_version_id.as_str().to_owned(), run_id.clone());
        }

        let proposal = open_proposal(&id, state.turns.get_mut(&id))?;
        proposal.disposition = Disposition::Accepted {
            child_version_id: child_version_id.clone(),
            accepted_run_id: accepted_run_id.clone(),
        };
        // A successful accept clears the stale failure, in the same act.
        proposal.accept_failure = None;

        Ok(AcceptedCoachOutcome {
            child_version_id,
            accepted_run_id,
            walk_forward_run_id,
        })
    }
}
