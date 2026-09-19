//! The coach DECISION use case (r1.s4.w2, ADR-0010 / ADR-0019 / ADR-0021) — one
//! session id and one action in, one durable outcome out.
//!
//! `coach.rs` records what a TURN produced. This module records what the TRADER
//! did with it: modify the proposed mutation, reject it, or accept it — and on
//! accept, re-run the backtest on the parent run's EXACT persisted inputs and
//! commit the child version, its run, its trades and the proposal's links in W4's
//! one transaction.
//!
//! **Identifiers in, not fragments.** The request carries a session id and an
//! action. The module loads the session, its current proposal and the coached
//! version itself, so a caller cannot pair a proposal with a DSL it was not made
//! against, cannot substitute another run's inputs, and cannot supply the child's
//! provenance — [`PreparedCoachAcceptance`](crate::domain::PreparedCoachAcceptance)
//! has nowhere to put it. That is `#132`'s false-audit-row argument applied to the
//! decision half.
//!
//! **The accept path never fetches candles.** Snapshots come from
//! [`CandleSeriesRepository`] by the data versions the parent run's persisted
//! inputs name. There is no `HEAD` lookup and no default reconstruction of the
//! request: re-running the child on DIFFERENT data would produce a before/after
//! comparison that looks valid and is not.
//!
//! **Nothing expensive happens inside a write transaction.** Re-applying the
//! mutation, loading snapshots and running the engine all complete before
//! `commit_acceptance` is called, and the snapshot load + engine run go off the
//! async runtime through `spawn_blocking` exactly as `run_version_backtest` does —
//! they are the same filesystem I/O plus Parquet decode plus CPU engine.
//!
//! **A failed accept is a RECORD, not an error.** Each of the six stages the accept
//! can stop at is written to the proposal as a typed
//! [`CoachAcceptFailure`](crate::domain::CoachAcceptFailure), the proposal stays
//! open, and the outcome is [`CoachDecisionOutcome::AcceptFailed`]. The typed
//! [`CoachDecisionError`] is reserved for the cases where there is nothing to
//! record against: no such session, no proposal, or an action the proposal's state
//! cannot take.
//!
//! **A read-back failure AFTER a committed accept is not an accept failure.** The
//! child and the run exist; the accept succeeded. The outcome carries both ids,
//! `after: None` and the read-back error — the `r1.s3` saved-but-unreadable
//! precedent, and the reason `AcceptFailureStage` has no `read_back` variant.

use rust_decimal::Decimal;

use crate::application::backtest::{PrepareError, ReadBackFailure, prepare_backtest};
use crate::domain::backtest::SummaryStats;
use crate::domain::strategy::VersionId;
use crate::domain::{
    AcceptFailureStage, BacktestRunId, BacktestRunRepository, CandleSeries, CandleSeriesRepository,
    CoachAcceptFailure, CoachAcceptanceRepository, CoachingRepository, CoachingSession,
    CoachingSessionId, DataError, Disposition, DispositionKind, EngineFingerprint, ExchangeAdapter,
    Mutation, MutationError, PersistedRun, PreparedBacktest, PreparedCoachAcceptance, Proposal,
    SessionOutcome, StrategyRepository, SymbolFilters, ValidatedDsl, apply,
};

// ---------------------------------------------------------------------------
// Request and outcome
// ---------------------------------------------------------------------------

/// What the trader did with the coach's proposal.
///
/// Three variants and no fourth: the decision surface is exactly the one ADR-0021
/// names, and "do nothing" is the absence of a call rather than an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoachAction {
    /// Replace the proposal's mutation with the trader's own edited `SetParam`.
    Modify(Mutation),
    /// Record the terminal rejection. No child, no run.
    Reject,
    /// Re-apply the current mutation, re-backtest it on the parent run's exact
    /// persisted inputs, and commit the child.
    Accept,
}

impl CoachAction {
    /// The action's tag, for an error message that has no payload to print.
    #[must_use]
    pub fn kind(&self) -> CoachActionKind {
        match self {
            Self::Modify(_) => CoachActionKind::Modify,
            Self::Reject => CoachActionKind::Reject,
            Self::Accept => CoachActionKind::Accept,
        }
    }
}

/// A [`CoachAction`] without its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoachActionKind {
    /// [`CoachAction::Modify`].
    Modify,
    /// [`CoachAction::Reject`].
    Reject,
    /// [`CoachAction::Accept`].
    Accept,
}

impl std::fmt::Display for CoachActionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Modify => "modified",
            Self::Reject => "rejected",
            Self::Accept => "accepted",
        })
    }
}

/// One decision: which session, and what the trader did.
///
/// **Identifiers and the action only.** No run, no version, no trade set, no
/// candidate DSL — every one of those is loaded from the session, which is what
/// makes them consistent by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoachDecisionRequest {
    /// The coaching session whose proposal is being decided. Also the accept
    /// idempotency key.
    pub session_id: CoachingSessionId,
    /// What the trader did.
    pub action: CoachAction,
}

/// What one committed accept produced, as the rail needs to show it.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedCoachResult {
    /// The child version the accept minted.
    pub child_version_id: VersionId,
    /// The re-backtest run OF that child.
    pub accepted_run_id: BacktestRunId,
    /// The PARENT run's persisted summary — the "before" half of the comparison,
    /// read from the stored row rather than recomputed.
    pub before: SummaryStats,
    /// The CHILD run's persisted summary. `None` only for a saved-but-unreadable
    /// child run, which is a read failure and not an accept failure.
    pub after: Option<SummaryStats>,
    /// Whether the post-commit read back succeeded. `Err` with `after: None` is the
    /// `r1.s3` saved-but-unreadable shape: the accept SUCCEEDED and both ids are
    /// real, but this process could not re-read the row it just wrote.
    pub read_back: Result<(), ReadBackFailure>,
}

/// The durable result of one decision.
///
/// The `Accepted` variant is much larger than the others because [`SummaryStats`]
/// is a wide value type and it carries two of them. Boxing it would even the
/// variants out and change the interface this item's spec pins; the enum is
/// returned once per decision, never held in a collection, so the size difference
/// costs one move of a few hundred bytes at the call boundary. Stated rather than
/// silenced blindly.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum CoachDecisionOutcome {
    /// The trader's mutation was re-validated and stored; the proposal is still
    /// open and may be modified again or accepted.
    Modified(Proposal),
    /// The proposal is terminally rejected. No child, no run.
    Rejected(Proposal),
    /// The accept committed.
    Accepted(AcceptedCoachResult),
    /// The accept stopped at a named stage, the failure is RECORDED on the
    /// proposal, and the proposal is still actionable.
    AcceptFailed(Proposal),
}

/// Everything the decision can refuse with — the cases where there is nothing to
/// record a failure against.
///
/// Deliberately narrow. A failed ACCEPT is not here: it is a recorded
/// [`CoachAcceptFailure`] and a [`CoachDecisionOutcome::AcceptFailed`], because the
/// proposal exists and the trader has to be told which stage stopped. What is here
/// is the set of requests that never had a proposal to act on in the first place.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CoachDecisionError {
    /// No such coaching session.
    #[error("no such coaching session `{}`", .0.as_str())]
    SessionNotFound(CoachingSessionId),

    /// The session exists but produced no proposal — it is still a `pending` claim,
    /// or the turn recorded a typed failure.
    #[error("coaching session `{}` produced no proposal to decide on", .0.as_str())]
    NoProposal(CoachingSessionId),

    /// The proposal's current state cannot take this action — accepting a rejected
    /// proposal, rejecting an accepted one, modifying a settled one.
    #[error("a `{current}` proposal cannot be {action}")]
    NotActionable {
        /// Where the proposal stands.
        current: DispositionKind,
        /// What was asked of it.
        action: CoachActionKind,
    },

    /// The version the session coached no longer exists, so there is no DSL to
    /// apply the mutation to.
    #[error("the coached strategy version `{}` no longer exists", .0.as_str())]
    ParentVersionMissing(VersionId),

    /// The trader's own mutation does not apply to the coached version's DSL. A
    /// MODIFY-only error: nothing is written, and the proposal keeps the mutation
    /// it had.
    #[error("the modified mutation does not apply: {source}")]
    InapplicableMutation {
        /// Why it does not apply, carried verbatim.
        #[source]
        source: MutationError,
    },

    /// The accept failed AND recording that failure failed too.
    ///
    /// Both halves are carried because each answers a different question and the
    /// second one erases the first if it is allowed to. The store error says why
    /// nothing could be written down; the stage and message say what actually went
    /// wrong with the accept — which is the half the trader needs and the half a
    /// bare `Data(..)` would drop on the floor. Nothing is on record after this, so
    /// the message is the only account of it that exists.
    #[error(
        "the accept failed at `{stage}` ({detail}), and recording that failure failed too: {source}"
    )]
    FailureUnrecordable {
        /// Where the accept stopped.
        stage: AcceptFailureStage,
        /// What went wrong there, verbatim.
        detail: String,
        /// Why it could not be written down.
        #[source]
        source: DataError,
    },

    /// The store failed.
    #[error("{0}")]
    Data(#[from] DataError),
}

// ---------------------------------------------------------------------------
// The entry point
// ---------------------------------------------------------------------------

/// Decide one coach proposal: modify, reject or accept it.
///
/// # Errors
///
/// Returns a [`CoachDecisionError`] when the session is absent, produced no
/// proposal, or is in a state the action cannot be taken from, when the coached
/// version has vanished, when a modify's mutation does not apply, or when the store
/// fails. A failed ACCEPT is not an error — see
/// [`CoachDecisionOutcome::AcceptFailed`].
pub async fn run_coach_decision<S, C, E, R, A, Q>(
    strategies: &S,
    candles: &C,
    exchange: &E,
    runs: &R,
    acceptance: &A,
    sessions: &Q,
    request: CoachDecisionRequest,
) -> Result<CoachDecisionOutcome, CoachDecisionError>
where
    S: StrategyRepository,
    C: CandleSeriesRepository + Clone + Send + 'static,
    E: ExchangeAdapter + Clone + Send + 'static,
    R: BacktestRunRepository,
    A: CoachAcceptanceRepository,
    Q: CoachingRepository,
{
    let CoachDecisionRequest { session_id, action } = request;

    // The session and its current proposal are LOADED, never supplied.
    let session = sessions
        .get_session(&session_id)
        .await?
        .ok_or_else(|| CoachDecisionError::SessionNotFound(session_id.clone()))?;
    let proposal = match &session.outcome {
        SessionOutcome::Proposed { proposal } => proposal.clone(),
        SessionOutcome::Pending | SessionOutcome::Failed { .. } => {
            return Err(CoachDecisionError::NoProposal(session_id));
        }
    };

    match action {
        CoachAction::Modify(mutation) => {
            modify(strategies, sessions, &session, &proposal, mutation).await
        }
        CoachAction::Reject => reject(sessions, &session_id, &proposal).await,
        CoachAction::Accept => {
            accept(
                strategies, candles, exchange, runs, acceptance, &session, &proposal,
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// Modify
// ---------------------------------------------------------------------------

/// Re-validate the trader's own `SetParam` against the COACHED version's DSL and
/// store it.
///
/// The parent DSL comes from the session's `strategy_version_id`, never from a
/// caller-supplied document: a modify that re-validated against some other DSL
/// would certify a mutation the accept then re-applies to a different one.
async fn modify<S, Q>(
    strategies: &S,
    sessions: &Q,
    session: &CoachingSession,
    proposal: &Proposal,
    mutation: Mutation,
) -> Result<CoachDecisionOutcome, CoachDecisionError>
where
    S: StrategyRepository,
    Q: CoachingRepository,
{
    guard_actionable(proposal, CoachActionKind::Modify)?;
    let parent = parent_dsl(strategies, session).await?;

    // Validity is USE-TIME (audit C4): `apply` is the check, and nothing about it is
    // stored. A mutation that does not apply writes NOTHING — the proposal keeps the
    // mutation it had, which is still the one an accept would re-apply.
    apply(&parent.dsl, &mutation)
        .map_err(|source| CoachDecisionError::InapplicableMutation { source })?;

    let stored = sessions.record_modification(&session.id, &mutation).await?;
    Ok(CoachDecisionOutcome::Modified(stored))
}

// ---------------------------------------------------------------------------
// Reject
// ---------------------------------------------------------------------------

/// Record the terminal rejection. No child, no run.
///
/// Rejecting an ALREADY rejected proposal is the idempotent no-op the `0008`
/// transition trigger permits, so it short-circuits without a write. Rejecting an
/// accepted proposal is refused by the domain transition and surfaced as
/// [`CoachDecisionError::NotActionable`].
async fn reject<Q>(
    sessions: &Q,
    session_id: &CoachingSessionId,
    proposal: &Proposal,
) -> Result<CoachDecisionOutcome, CoachDecisionError>
where
    Q: CoachingRepository,
{
    match &proposal.disposition {
        Disposition::Rejected => return Ok(CoachDecisionOutcome::Rejected(proposal.clone())),
        Disposition::Accepted { .. } => {
            return Err(CoachDecisionError::NotActionable {
                current: DispositionKind::Accepted,
                action: CoachActionKind::Reject,
            });
        }
        Disposition::Proposed | Disposition::Modified => {}
    }

    sessions
        .record_disposition(session_id, &Disposition::Rejected)
        .await?;
    let settled = reread_proposal(sessions, session_id).await?;
    Ok(CoachDecisionOutcome::Rejected(settled))
}

// ---------------------------------------------------------------------------
// Accept
// ---------------------------------------------------------------------------

/// The seven-step accept (spec §Accept), each step's failure recorded as its stage.
async fn accept<S, C, E, R, A>(
    strategies: &S,
    candles: &C,
    exchange: &E,
    runs: &R,
    acceptance: &A,
    session: &CoachingSession,
    proposal: &Proposal,
) -> Result<CoachDecisionOutcome, CoachDecisionError>
where
    S: StrategyRepository,
    C: CandleSeriesRepository + Clone + Send + 'static,
    E: ExchangeAdapter + Clone + Send + 'static,
    R: BacktestRunRepository,
    A: CoachAcceptanceRepository,
{
    // 1. IDEMPOTENCY FIRST. The session id is the accept idempotency key, so a
    //    client that lost the response retries and gets the same two ids back —
    //    without applying, computing or writing anything.
    if let Disposition::Accepted {
        child_version_id,
        accepted_run_id,
    } = &proposal.disposition
    {
        return replay_accepted(runs, session, child_version_id, accepted_run_id).await;
    }
    if proposal.disposition == Disposition::Rejected {
        return Err(CoachDecisionError::NotActionable {
            current: DispositionKind::Rejected,
            action: CoachActionKind::Accept,
        });
    }

    // 2. APPLY — the CURRENT mutation: the original proposal's, or the latest one a
    //    modify stored in its place.
    let parent = parent_dsl(strategies, session).await?;
    let mutation_path = match &proposal.mutation {
        Mutation::SetParam { path, .. } => path.clone(),
    };
    let candidate = match apply(&parent.dsl, &proposal.mutation) {
        Ok(candidate) => candidate,
        Err(error) => {
            return record_failure(
                acceptance,
                &session.id,
                AcceptFailureStage::Apply,
                error.to_string(),
                Some(mutation_path),
            )
            .await;
        }
    };

    // 3. LOAD INPUTS — the parent run's own persisted provenance. A pre-`0006` run
    //    (`inputs: None`) and a read error are the same recorded stage: in both
    //    cases there is no exact data set to re-run the child on, and reconstructing
    //    a default request would produce a comparison that looks valid and is not.
    let (parent_run, inputs) = match load_parent_inputs(runs, session).await {
        Ok(loaded) => loaded,
        Err(failure) => return record_staged(acceptance, &session.id, failure).await,
    };

    // 4-5. LOAD SNAPSHOTS, COMPILE AND COMPUTE — off the async runtime, exactly as
    //      `run_version_backtest` does it: the same Parquet decode and the same
    //      synchronous engine, and holding a Tokio worker for either stalls every
    //      other command on the bus.
    let prepared = match prepare_offthread(
        candles.clone(),
        exchange.clone(),
        candidate.validated().clone(),
        inputs,
        parent_run.starting_equity,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(failure) => return record_staged(acceptance, &session.id, failure).await,
    };

    // 5b. THE ENGINE MUST BE THE ONE THAT PRODUCED THE PARENT — see
    //     `engine_divergence`. Checked before the commit, so a refusal leaves no
    //     child behind to mislead anyone.
    if let Some(message) = engine_divergence(&parent_run, &prepared) {
        return record_failure(
            acceptance,
            &session.id,
            AcceptFailureStage::Backtest,
            message,
            Some("engine fingerprint".to_owned()),
        )
        .await;
    }

    // 6. COMMIT — W4's one transaction. The adapter mints the child id, the run id
    //    and `created_at`, and DERIVES the strategy, the parent and the creating
    //    call from the claimed session row. `expected_mutation` is the optimistic
    //    lock: everything above ran outside the transaction, so the adapter refuses
    //    unless the proposal still carries the mutation this child came from.
    let committed = match acceptance
        .commit_acceptance(PreparedCoachAcceptance {
            session_id: session.id.clone(),
            expected_mutation: proposal.mutation.clone(),
            child_dsl: candidate.dsl().clone(),
            prepared_run: prepared,
        })
        .await
    {
        Ok(outcome) => outcome,
        Err(e) => {
            // A NEW transaction, on a rolled-back write: no child exists.
            return record_failure(
                acceptance,
                &session.id,
                AcceptFailureStage::Persist,
                e.to_string(),
                None,
            )
            .await;
        }
    };

    // 7. READ BACK. Past this line the accept has SUCCEEDED — the child, its run and
    //    the proposal's links are committed — so nothing below may be recorded as an
    //    accept failure, and `0008` would refuse it anyway.
    let (after, read_back) = child_summary(runs, &committed.accepted_run_id).await;
    Ok(CoachDecisionOutcome::Accepted(AcceptedCoachResult {
        child_version_id: committed.child_version_id,
        accepted_run_id: committed.accepted_run_id,
        before: parent_run.summary,
        after,
        read_back,
    }))
}

// ---------------------------------------------------------------------------
// Shared steps
// ---------------------------------------------------------------------------

/// Step 1: the ALREADY-accepted answer — both stored ids and the two persisted run
/// summaries, with nothing applied, computed or written.
async fn replay_accepted<R>(
    runs: &R,
    session: &CoachingSession,
    child_version_id: &VersionId,
    accepted_run_id: &BacktestRunId,
) -> Result<CoachDecisionOutcome, CoachDecisionError>
where
    R: BacktestRunRepository,
{
    let before = parent_summary(runs, session).await?;
    let (after, read_back) = child_summary(runs, accepted_run_id).await;
    Ok(CoachDecisionOutcome::Accepted(AcceptedCoachResult {
        child_version_id: child_version_id.clone(),
        accepted_run_id: accepted_run_id.clone(),
        before,
        after,
        read_back,
    }))
}

/// Step 3: the parent run and the provenance it recorded.
///
/// A read error, a vanished run and a pre-`0006` row are one recorded stage, because
/// they are one fact: there is no exact data set to re-run the child on. The
/// alternative — reconstructing a default request — is what would produce a
/// before/after comparison that looks valid and is not.
async fn load_parent_inputs<R>(
    runs: &R,
    session: &CoachingSession,
) -> Result<(PersistedRun, crate::domain::BacktestInputs), StagedFailure>
where
    R: BacktestRunRepository,
{
    let run_id = session.backtest_run_id.as_str().to_owned();
    let staged = |message: String| StagedFailure {
        stage: AcceptFailureStage::LoadInputs,
        message,
        subject: Some(run_id.clone()),
    };

    let run = runs
        .get_run(&session.backtest_run_id)
        .await
        .map_err(|e| staged(e.to_string()))?
        .ok_or_else(|| staged(format!("the coached run `{run_id}` no longer exists")))?;
    let inputs = run.inputs.clone().ok_or_else(|| {
        staged(format!(
            "the coached run `{run_id}` predates migration 0006 and records no input \
             provenance, so its child cannot be re-backtested on the same data"
        ))
    })?;
    Ok((run, inputs))
}

/// A stage-tagged reason one accept stopped before the commit.
struct StagedFailure {
    stage: AcceptFailureStage,
    message: String,
    subject: Option<String>,
}

/// Steps 4-5 off the async runtime: load the snapshots the inputs NAME, resolve the
/// symbol filters, then compile and compute through the shared prepare step.
///
/// The closure owns its clones; nothing is borrowed across the await.
async fn prepare_offthread<C, E>(
    candles: C,
    exchange: E,
    validated: ValidatedDsl,
    inputs: crate::domain::BacktestInputs,
    starting_equity: Decimal,
) -> Result<PreparedBacktest, StagedFailure>
where
    C: CandleSeriesRepository + Send + 'static,
    E: ExchangeAdapter + Send + 'static,
{
    let joined = tokio::task::spawn_blocking(move || -> Result<PreparedBacktest, StagedFailure> {
        let primary = load_named_snapshot(
            &candles,
            &inputs.pair,
            inputs.primary.timeframe,
            &inputs.primary.data_version,
        )?;
        let htf = match inputs.htf.as_ref() {
            Some(selection) => Some(load_named_snapshot(
                &candles,
                &inputs.pair,
                selection.timeframe,
                &selection.data_version,
            )?),
            None => None,
        };
        // Symbol filters are pinned exchange METADATA, not price data — resolving
        // them is not "fetching candles from an exchange", which the accept path
        // never does.
        let filters: SymbolFilters =
            exchange
                .symbol_filters(&inputs.pair)
                .map_err(|e| StagedFailure {
                    stage: AcceptFailureStage::Backtest,
                    message: format!("the symbol filters the engine needs are unavailable: {e}"),
                    subject: Some(inputs.pair.as_str().to_owned()),
                })?;

        prepare_backtest(
            &validated,
            inputs,
            &primary,
            htf.as_ref(),
            &filters,
            starting_equity,
        )
        .map_err(|e| match e {
            PrepareError::Compile(reason) => StagedFailure {
                stage: AcceptFailureStage::Compile,
                message: reason,
                subject: None,
            },
            PrepareError::Engine(source) => StagedFailure {
                stage: AcceptFailureStage::Backtest,
                message: source.to_string(),
                subject: None,
            },
        })
    })
    .await;

    joined.unwrap_or_else(|join_err| {
        Err(StagedFailure {
            stage: AcceptFailureStage::Backtest,
            message: format!("the re-backtest worker thread failed: {join_err}"),
            subject: None,
        })
    })
}

/// The reason this child cannot be compared to this parent, when there is one.
///
/// The whole point of replaying the parent's exact inputs is that the only
/// difference between the two summaries is the mutation. A child computed by a
/// DIFFERENT engine build breaks that: the rail still shows the two side by side as
/// before/after, and a delta the engine caused is read as the coach's doing.
///
/// The standalone path compares the same two fingerprints (`backtest.rs`, FR-7)
/// because a run is only comparable to another run from the same engine. The
/// difference here is what a mismatch costs: there the comparison is a note beside
/// the result, so it warns; here the comparison IS the product, so it refuses.
fn engine_divergence(parent_run: &PersistedRun, prepared: &PreparedBacktest) -> Option<String> {
    let parent_fp = EngineFingerprint::from_stored(parent_run.engine_fingerprint.clone());
    prepared
        .result
        .engine_fingerprint
        .compare(&parent_fp)
        .map(|divergence| {
            format!(
                "the parent run was produced by a different engine build, so a before/after \
                 comparison would attribute the engine's difference to the coach's change: \
                 {divergence}. Re-run the parent version on this build, then accept."
            )
        })
}

/// One snapshot BY THE IDENTITY THE INPUTS NAME — never `HEAD`, which may have
/// moved since the parent run.
fn load_named_snapshot<C>(
    candles: &C,
    pair: &crate::domain::Pair,
    timeframe: crate::domain::Timeframe,
    version: &crate::domain::DataVersion,
) -> Result<CandleSeries, StagedFailure>
where
    C: CandleSeriesRepository,
{
    let staged = |message: String| StagedFailure {
        stage: AcceptFailureStage::LoadSnapshots,
        message,
        subject: Some(version.as_str().to_owned()),
    };
    let series = candles
        .load_version(pair, timeframe, version)
        .map_err(|e| {
            staged(format!(
                "the {} {} snapshot `{}` the coached run used could not be loaded: {e}",
                pair,
                timeframe.binance_interval(),
                version
            ))
        })?
        .series;
    if series.candles.is_empty() {
        return Err(staged(format!(
            "the {} {} snapshot `{}` the coached run used is empty",
            pair,
            timeframe.binance_interval(),
            version
        )));
    }
    // The SAME refusal the standalone path applies before it runs the engine
    // (`backtest.rs::load_snapshot`): structural corruption and spacing gaps are both
    // refusals, because the engine and the indicator stream assume a contiguous series
    // and neither detects nor fills a hole. Accepting a gapped snapshot here would
    // persist a child whose summary is skewed by the hole and then show it beside its
    // parent as the mutation's effect — silently, which is precisely what the
    // standalone path refuses to do quietly. An accept REPLAYS the parent's inputs, so
    // it owes the parent's guards.
    let gaps = series.validate().map_err(|e| {
        staged(format!(
            "the {} {} snapshot `{}` the coached run used is structurally unsound: {e}",
            pair,
            timeframe.binance_interval(),
            version
        ))
    })?;
    if let Some(first) = gaps.first() {
        return Err(staged(format!(
            "the {} {} snapshot `{}` the coached run used has a gap: a candle was expected at \
             {} and the next one found is at {}",
            pair,
            timeframe.binance_interval(),
            version,
            first.expected,
            first.found
        )));
    }
    Ok(series)
}

/// Record a [`StagedFailure`] as it stands — the stage, message and subject the
/// failing step already chose, unaltered.
///
/// The staged steps all fail the same way, so they all record the same way; spelling
/// the three fields out at each call site is how one of them ends up dropping the
/// subject or relabelling the stage.
async fn record_staged<A>(
    acceptance: &A,
    session_id: &CoachingSessionId,
    failure: StagedFailure,
) -> Result<CoachDecisionOutcome, CoachDecisionError>
where
    A: CoachAcceptanceRepository,
{
    record_failure(
        acceptance,
        session_id,
        failure.stage,
        failure.message,
        failure.subject,
    )
    .await
}

/// Record a typed accept failure and return the still-open proposal.
///
/// The proposal stays `proposed`/`modified` and no child or run is stored, so the
/// trader can fix what went wrong and try again.
async fn record_failure<A>(
    acceptance: &A,
    session_id: &CoachingSessionId,
    stage: AcceptFailureStage,
    message: String,
    subject: Option<String>,
) -> Result<CoachDecisionOutcome, CoachDecisionError>
where
    A: CoachAcceptanceRepository,
{
    // A DOUBLE FAULT carries both halves. `?` here would return only the store's
    // error, and the accept's own stage and message — the half that says what
    // actually went wrong, and the only account of it that will exist, since
    // nothing was written — would be gone.
    let failure = CoachAcceptFailure {
        stage,
        message,
        subject,
    };
    match acceptance
        .record_accept_failure(session_id, failure.clone())
        .await
    {
        Ok(proposal) => Ok(CoachDecisionOutcome::AcceptFailed(proposal)),
        Err(source) => Err(CoachDecisionError::FailureUnrecordable {
            stage: failure.stage,
            detail: failure.message,
            source,
        }),
    }
}

/// The coached version's DSL — the ONE document a mutation is applied to.
async fn parent_dsl<S>(
    strategies: &S,
    session: &CoachingSession,
) -> Result<crate::domain::strategy::StrategyVersion, CoachDecisionError>
where
    S: StrategyRepository,
{
    strategies
        .get_version(&session.strategy_version_id)
        .await?
        .ok_or_else(|| {
            CoachDecisionError::ParentVersionMissing(session.strategy_version_id.clone())
        })
}

/// The parent run's persisted summary — the "before" half.
async fn parent_summary<R>(
    runs: &R,
    session: &CoachingSession,
) -> Result<SummaryStats, CoachDecisionError>
where
    R: BacktestRunRepository,
{
    let run: PersistedRun = runs
        .get_run(&session.backtest_run_id)
        .await?
        .ok_or_else(|| {
            CoachDecisionError::Data(DataError::Db(format!(
                "the coached run `{}` no longer exists",
                session.backtest_run_id.as_str()
            )))
        })?;
    Ok(run.summary)
}

/// The child run's persisted summary, and whether it could be read at all.
///
/// Never an error: by the time this is called the accept has committed, and a
/// failure here is the saved-but-unreadable shape rather than a failed accept.
async fn child_summary<R>(
    runs: &R,
    run_id: &BacktestRunId,
) -> (Option<SummaryStats>, Result<(), ReadBackFailure>)
where
    R: BacktestRunRepository,
{
    match runs.get_run(run_id).await {
        Ok(Some(run)) => (Some(run.summary), Ok(())),
        Ok(None) => (None, Err(ReadBackFailure::Missing)),
        Err(e) => (None, Err(ReadBackFailure::Data(e))),
    }
}

/// The proposal as it now stands, read back through the session.
async fn reread_proposal<Q>(
    sessions: &Q,
    session_id: &CoachingSessionId,
) -> Result<Proposal, CoachDecisionError>
where
    Q: CoachingRepository,
{
    let session = sessions
        .get_session(session_id)
        .await?
        .ok_or_else(|| CoachDecisionError::SessionNotFound(session_id.clone()))?;
    match session.outcome {
        SessionOutcome::Proposed { proposal } => Ok(proposal),
        SessionOutcome::Pending | SessionOutcome::Failed { .. } => {
            Err(CoachDecisionError::NoProposal(session_id.clone()))
        }
    }
}

/// Refuse an action a settled proposal cannot take, BEFORE any write.
fn guard_actionable(
    proposal: &Proposal,
    action: CoachActionKind,
) -> Result<(), CoachDecisionError> {
    match proposal.disposition {
        Disposition::Proposed | Disposition::Modified => Ok(()),
        Disposition::Accepted { .. } | Disposition::Rejected => {
            Err(CoachDecisionError::NotActionable {
                current: proposal.disposition.kind(),
                action,
            })
        }
    }
}
