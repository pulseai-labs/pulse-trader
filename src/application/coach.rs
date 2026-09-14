//! The SEALED coach turn (r1.s4.w1, `pulseai-labs/pulse-trader#131` + `#132`,
//! ADR-0015 / ADR-0021 as amended by r1.s4.w4).
//!
//! One crate-private entry point — [`run_coach_turn`] — runs a coach turn end to
//! end over `w4`'s claim/finish contract. A caller supplies **two identifiers** and
//! gets back exactly one durable [`CoachingSession`]: proposed with one validated
//! `SetParam`, or failed with one typed reason.
//!
//! **Why identifiers, and why sealed.** The surface this replaces
//! (`Coach::new` + `Coach::run_turn`) took the FRAGMENTS of a turn — a provider, a
//! capture handle, a run, a trade vector, a version — and every one of them was a
//! way to write an audit row that is individually valid and collectively false
//! (`#132`): a session relating a run to a version it never touched, a session
//! naming another turn's ledger row, a session written only AFTER the call so a
//! crash left no row at all, a turn coached on a truncated trade set. None of those
//! is expressible here, because there is nowhere to put them:
//!
//! - the run, its trades and its owning version arrive from [`CoachTurnSource`],
//!   keyed by `run_id` alone;
//! - the response and the ledger row it minted arrive TOGETHER from
//!   [`AttributedCoachProvider`], so a turn either names its own row or refuses;
//! - the session id is CLAIMED before any network I/O, and settled exactly once.
//!
//! **No write transaction is held across the provider call.** The claim commits and
//! returns; that ordering is the item's central invariant and
//! `tests/coach_turn_boundary.rs` asserts it by having the provider read the row
//! while it is being called.
//!
//! **The registry is process-local and clears on every exit path.** A repository can
//! see that a claim is unfinished; it cannot see whether the process that made it is
//! alive. [`CoachTurnRegistry`] is what can, and its guard is released on return,
//! on error, on panic-unwind and on the future being dropped — which is what lets
//! the NEXT turn tell a live claim from a stale one.
//!
//! **NO adapter import** (r1.s4.w2, `pulseai-labs/pulse-trader#150`). This module
//! used to reach `crate::adapters::llm::redacting_logging::Redactor`, which made
//! ADR-0015's ONE deliberate adapters exception into two. The scrubber's pure text
//! logic now lives in the domain ring as [`crate::domain::Redactor`], so the turn
//! still holds the SAME scrubber the ledger decorator uses — passing a second one
//! would be how the two roads drift apart — and reaches nothing outward to get it.
//! `tests/tauri_backtest.rs` scans every file in this ring and keeps the exception
//! count at one.

use std::collections::HashSet;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat};
use sha2::{Digest, Sha256};

use crate::agent::{
    DEFAULT_MAX_TURN_BYTES, TurnAnswer, check_turn_budget, classify, coach_tool_definitions,
};
use crate::domain::Redactor;
// The two coach-turn ports live in the DOMAIN ring beside every other port
// (ADR-0015, one home for ports); this module is one of their consumers, not their
// owner.
use crate::domain::{
    AttributedCallError, AttributedCoachProvider, BacktestRunId, Clock, CoachContext, CoachFailure,
    CoachRequestFingerprint, CoachSessionClaim, CoachSessionClaimResult, CoachTurnProjection,
    CoachTurnSource, CoachingError, CoachingRepository, CoachingSession, CoachingSessionId,
    DataError, InitialCoachOutcome, LlmCallId, LlmConfig, LlmError, Message, ProjectedRun,
    SessionOutcome, ToolDefinition,
};

// ---------------------------------------------------------------------------
// Request and settings
// ---------------------------------------------------------------------------

/// What a caller asks for: one session id to record under, one persisted run to
/// coach on. **Identifiers only.**
///
/// There is deliberately no free-text coaching intent (out of scope for this item),
/// no version, no trade set and no provider: the coaching goal is whatever the
/// resolved prompt already reads from the persisted context, and everything else is
/// loaded from the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoachTurnRequest {
    /// The session id this turn claims and settles. Also `r1.s4`'s accept
    /// idempotency key.
    pub(crate) session_id: CoachingSessionId,
    /// The persisted backtest run the coach reads (and never recomputes).
    pub(crate) run_id: BacktestRunId,
}

/// The turn's deterministic POLICY inputs — everything the composition root decided
/// before the turn began, and nothing a turn could be lied to with.
///
/// This is not a loophole in the identifiers-only rule above: none of these is a
/// run, a version, a trade set, a provider or a capture handle, so none of them can
/// produce a false audit row. They are here rather than inside the module because
/// the composition root owns them — the resolved prompt and the `prompt_version`
/// stamped on the ledger row must be the same bytes (audit C2), and the request
/// fingerprint has to cover exactly the settings the call will use.
pub(crate) struct CoachTurnSettings {
    /// The resolved system prompt (after `$PULSE_PROMPT_DIR` overlay resolution).
    pub(crate) prompt: String,
    /// The `prompt_version` this turn's ledger row will carry — the SHA-256 of the
    /// resolved prompt bytes. `None` only where no version is stamped.
    pub(crate) prompt_version: Option<String>,
    /// The per-request chat config.
    pub(crate) config: LlmConfig,
    /// The NFR-6 scrubber for TOOL ARGUMENTS, which become stored domain values.
    pub(crate) redactor: Redactor,
    /// The per-turn wall-clock guard (audit C5).
    pub(crate) turn_timeout: Duration,
    /// The pre-call DSL size budget (grill L4).
    pub(crate) max_dsl_bytes: usize,
}

// ---------------------------------------------------------------------------
// The process-local single-flight registry
// ---------------------------------------------------------------------------

/// Who, in THIS process, is currently running a turn for a given session id.
///
/// The repository deliberately refuses to judge whether a `pending` claim is live —
/// it cannot see the process that made it. This is the thing that can, for the only
/// process whose liveness is knowable: our own. A session id registered here is a
/// call in flight now; a `pending` row NOT registered here was left by an earlier
/// process lifetime and is finalized as [`CoachFailure::Interrupted`].
///
/// Held by the composition root for as long as turns may overlap. A fresh registry
/// per turn is not wrong, it is simply blind: it can never say "in flight".
pub struct CoachTurnRegistry {
    in_flight: Mutex<HashSet<CoachingSessionId>>,
}

impl Default for CoachTurnRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CoachTurnRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Is a turn for `session_id` running in this process right now?
    #[must_use]
    pub fn in_flight(&self, session_id: &CoachingSessionId) -> bool {
        self.lock().contains(session_id)
    }

    /// Take single-flight ownership of `session_id`, or `None` when another turn in
    /// this process already holds it.
    ///
    /// The returned guard releases the entry on EVERY exit path — return, `?`,
    /// panic-unwind, and the future being dropped by a cancelled `timeout` — because
    /// it releases in `Drop` rather than at a call site someone can forget.
    fn claim(&self, session_id: &CoachingSessionId) -> Option<InFlightGuard<'_>> {
        if self.lock().insert(session_id.clone()) {
            Some(InFlightGuard {
                registry: self,
                session_id: session_id.clone(),
            })
        } else {
            None
        }
    }

    /// The guard set, with a poisoned lock recovered rather than propagated: a
    /// panicking turn must not make every LATER turn unrunnable.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<CoachingSessionId>> {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The single-flight entry, released on drop.
struct InFlightGuard<'a> {
    registry: &'a CoachTurnRegistry,
    session_id: CoachingSessionId,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.registry.lock().remove(&self.session_id);
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A turn that produced no RECORDED outcome — the faults that are not coaching
/// outcomes.
///
/// Deliberately not [`CoachFailure`]s: those are the ways a *coach turn* can
/// deviate, and every one of them is written to the audit trail. What is left here
/// is what a session row would have to lie about — a run that does not exist, a
/// session id that already belongs to a different request, a fault this process
/// raised on the call path, and a failure to write the row at all. Each surfaces at
/// the CLI edge, preserved (ADR-0017).
#[derive(Debug, thiserror::Error)]
pub enum CoachTurnError {
    /// No such persisted backtest run.
    #[error("no persisted backtest run `{}`", .0.as_str())]
    RunNotFound(BacktestRunId),

    /// The projection could not be read.
    #[error("loading the coach turn projection for run `{}`: {source}", .run.as_str())]
    Projection {
        /// The run whose projection failed to load.
        run: BacktestRunId,
        /// Why it failed.
        #[source]
        source: DataError,
    },

    /// A turn for this session id is already in flight IN THIS PROCESS.
    ///
    /// Refused rather than reattached or re-asked: the first call owns the turn, and
    /// a second provider call under one session id would spend money to produce a
    /// second answer only one of which could ever be recorded.
    #[error("a coach turn for session `{}` is already in flight", .session.as_str())]
    TurnInFlight {
        /// The contested session id.
        session: CoachingSessionId,
    },

    /// The session id could not be claimed — it is held by a turn on a different
    /// run, version or request, or the store refused the write.
    #[error("claiming coach session `{}`: {source}", .session.as_str())]
    SessionConflict {
        /// The session id that could not be claimed.
        session: CoachingSessionId,
        /// The repository's reason.
        #[source]
        source: DataError,
    },

    /// The turn never happened because THIS process faulted on the provider call
    /// path — the decorator's ledger insert failed, the configured model has no
    /// price-table entry, the clock is out of range.
    ///
    /// A true *transport* fault is NOT here: it is recorded as
    /// [`CoachFailure::TransportFailure`] and the CLI still exits non-zero for it.
    #[error("the coach turn could not run: {0}")]
    LocalFault(#[from] LlmError),

    /// A response came back and no ledger row appeared for this turn.
    #[error(
        "the coach turn reached the provider but captured no ledger row: the provider or the \
         capture handle is not the one the ledger decorator writes through"
    )]
    LedgerRowMissing,

    /// Several ledger rows appeared for one turn.
    #[error("the coach turn captured {seen} ledger rows; one turn is one call is one row")]
    LedgerRowsAmbiguous {
        /// How many rows the one call correlated.
        seen: usize,
    },

    /// The request fingerprint could not be built — structurally unreachable (a
    /// SHA-256 hex digest is never empty), typed rather than unwrapped because a
    /// claim keyed on nothing is a row no later call could ever match.
    #[error("the coach turn's request fingerprint is not usable: {0}")]
    Fingerprint(#[from] CoachingError),

    /// The injected clock produced a timestamp outside the representable range, so
    /// the claim would carry a `created_at` no reader could parse.
    #[error("the coach turn could not be timestamped: {detail}")]
    Clock {
        /// What the clock reported.
        detail: String,
    },

    /// The turn's outcome could not be recorded. Fatal by design: an unrecordable
    /// turn is the silence this spine exists to prevent.
    #[error("the coach turn could not be recorded: {source}")]
    Record {
        /// Why the write failed.
        #[source]
        source: DataError,
    },

    /// The turn deviated AND the deviation could not be recorded — the double fault.
    ///
    /// Both halves travel together on purpose (PR #128, finding 6). Reporting only
    /// the write error leaves the operator with "the session could not be written"
    /// and no way to learn what the turn actually did — which, on the paths that
    /// reach here after a timeout or a transport fault, is the entire content of the
    /// incident.
    #[error("the coach turn failed ({failure}) and the failure could not be recorded: {source}")]
    RecordFailed {
        /// What the turn actually did — the reason that never reached the row.
        ///
        /// Boxed to keep this error small: a `CoachFailure` carries a `Mutation` and
        /// a `MutationError`, and inlining it makes every `Ok` pay for the rarest
        /// failure.
        failure: Box<CoachFailure>,
        /// Why it could not be written.
        #[source]
        source: DataError,
    },
}

// ---------------------------------------------------------------------------
// The request fingerprint
// ---------------------------------------------------------------------------

/// The single-flight key for one coach turn: lowercase SHA-256 over an explicit
/// ORDERED, LENGTH-PREFIXED feed (ADR-0010's style).
///
/// The feed, in this exact order — each element as its u64 big-endian byte length
/// followed by its bytes, so no concatenation of two elements can ever collide with
/// a different split of the same bytes:
///
/// 1. the resolved coach prompt text (after `$PULSE_PROMPT_DIR` overlay resolution);
/// 2. the rendered context, `CoachContext::render()` — empty when the context was
///    refused pre-call, which is a turn with no context to send;
/// 3. every advertised tool, in ADVERTISEMENT ORDER, as three elements: its name,
///    its description, then its parameter schema as canonical JSON (sorted keys, no
///    insignificant whitespace);
/// 4. the prompt version the ledger row will carry, or the empty string when none;
/// 5. the behaviour-affecting `LlmConfig` fields, in this fixed order: the model,
///    then the sampling control (`temperature`, in its round-trip decimal form),
///    then the length control (`max_tokens`), then the reasoning control
///    (`reasoning_effort`, in its `{:?}` form: `None`, or e.g. `Some(Low)`).
///
/// **What is deliberately NOT in it.** Credentials, base URLs, API keys and price
/// data — none is a property of the REQUEST, and a fingerprint that changed when a
/// key rotated would stop two identical asks from recognizing each other. Nor is a
/// serialized `LlmConfig` struct or any map-shaped blob acceptable in place of the
/// explicit fields: a map's iteration order is not a contract, and a struct gains
/// fields (a base URL, a key source) that would silently enter the digest.
/// `LlmBackend` is excluded for the same "explicit fields only" reason and because
/// it is single-valued today; adding it is a deliberate act that changes the pinned
/// digest in `tests/coach_turn_boundary.rs`, which is exactly the review this
/// deserves.
#[must_use]
pub fn coach_request_fingerprint(
    prompt: &str,
    rendered_context: &str,
    tools: &[ToolDefinition],
    prompt_version: Option<&str>,
    config: &LlmConfig,
) -> String {
    let mut hasher = Sha256::new();
    let mut feed = |bytes: &[u8]| {
        // Length-prefixed, so "ab" + "c" and "a" + "bc" are different feeds.
        hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(bytes);
    };

    feed(prompt.as_bytes());
    feed(rendered_context.as_bytes());
    for tool in tools {
        feed(tool.name.as_bytes());
        feed(tool.description.as_bytes());
        feed(canonical_json(&tool.parameters).as_bytes());
    }
    feed(prompt_version.unwrap_or_default().as_bytes());
    feed(config.model.as_bytes());
    // `{:?}` is the shortest round-trip decimal form (`0.0`, `0.7`), so two configs
    // hash the same iff the same f32 would be sent. `{}` would print `0` for `0.0`
    // and lose the distinction between an integral and a fractional setting.
    feed(format!("{:?}", config.temperature).as_bytes());
    feed(config.max_tokens.to_string().as_bytes());
    // `{:?}` spells an unset effort `None` and a set one `Some(Low)`: always one
    // element, so the feed's shape does not depend on whether an effort was chosen
    // (#164).
    feed(format!("{:?}", config.reasoning_effort).as_bytes());

    hex::encode(hasher.finalize())
}

/// Canonical JSON: object keys sorted, no insignificant whitespace.
///
/// Written out rather than delegated to `serde_json::to_string`, which sorts keys
/// only while the crate's `preserve_order` feature is off — a transitive dependency
/// enabling it would silently change every fingerprint in the database.
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .into_iter()
                .map(|key| {
                    let rendered = map
                        .get(key)
                        .map_or_else(|| "null".to_owned(), canonical_json);
                    format!("{}:{rendered}", serde_json::Value::String(key.clone()))
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        serde_json::Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", body.join(","))
        }
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The turn
// ---------------------------------------------------------------------------

/// Run one coach turn for `request` and return the session it settled.
///
/// The sequence, and the reason for each step's position:
///
/// 1. **Project.** Load the run, its trades and the version IT names, by `run_id`.
/// 2. **Own the id.** Take the process-local single-flight entry BEFORE the durable
///    claim, so a duplicate in this process cannot slip between the claim's commit
///    and the registration and mistake a live turn for a stale one.
/// 3. **Claim.** Commit the `pending` row keyed by the request fingerprint — before
///    any provider call, holding no write transaction across it.
/// 4. **Legacy run.** A pre-`0006` run settles as `MissingBacktestInputs`, no call.
/// 5. **Budgets.** The context sub-budget and the whole-turn ceiling both refuse
///    pre-call, recorded with `llm_call_id = NULL` (audit C3), free.
/// 6. **One call**, under the wall-clock guard, through the attributed provider.
/// 7. **Route** the one response: one validated proposal, one recorded
///    inapplicability, or one typed failure — and settle the claim exactly once.
///
/// # Errors
///
/// Returns [`CoachTurnError`] for the faults that are not coaching outcomes: an
/// absent run, an unreadable projection, a session id already in flight or already
/// held by a different request, a local fault on the call path, a turn that cannot
/// name exactly the ledger row it produced, and a failure to record.
pub(crate) async fn run_coach_turn<S, P, R, C>(
    source: &S,
    provider: &P,
    sessions: &R,
    registry: &CoachTurnRegistry,
    clock: &C,
    settings: &CoachTurnSettings,
    request: CoachTurnRequest,
) -> Result<CoachingSession, CoachTurnError>
where
    S: CoachTurnSource + Sync,
    P: AttributedCoachProvider + Sync,
    R: CoachingRepository + Sync,
    C: Clock + Sync,
{
    let CoachTurnRequest { session_id, run_id } = request;

    // 1. The repository-owned projection: run + trades + THE VERSION THE RUN NAMES.
    let projection = source
        .load_coach_turn(&run_id)
        .await
        .map_err(|source| CoachTurnError::Projection {
            run: run_id.clone(),
            source,
        })?
        .ok_or_else(|| CoachTurnError::RunNotFound(run_id.clone()))?;
    let (projected, legacy) = match projection {
        CoachTurnProjection::Coachable(projected) => (projected, false),
        CoachTurnProjection::Legacy(projected) => (projected, true),
    };

    // 2. The deterministic request. The context is built BEFORE the claim because
    //    the fingerprint covers the exact bytes the call would send; a refusal is
    //    recorded below, after the claim, so even a pre-call refusal leaves a row.
    let tools = coach_tool_definitions();
    let context = CoachContext::build(
        &projected.run,
        &projected.trades,
        &projected.version.dsl,
        settings.max_dsl_bytes,
    );
    let rendered = match &context {
        Ok(context) => context.render(),
        // A refused context has no rendered form: there is nothing to send, and the
        // empty element says exactly that. The claim's identity is still the run,
        // the version and this feed together, so two refusals of DIFFERENT runs are
        // still different claims.
        Err(_) => String::new(),
    };
    let fingerprint = CoachRequestFingerprint::new(coach_request_fingerprint(
        &settings.prompt,
        &rendered,
        &tools,
        settings.prompt_version.as_deref(),
        &settings.config,
    ))?;

    // 3. Single-flight ownership, then the durable claim. In this order on purpose
    //    (see the doc above): registering after the claim would leave a window in
    //    which a duplicate sees a pending row that nobody has registered yet and
    //    finalizes a LIVE turn as `Interrupted`.
    let Some(_in_flight) = registry.claim(&session_id) else {
        return Err(CoachTurnError::TurnInFlight {
            session: session_id,
        });
    };

    let claim = CoachSessionClaim {
        session_id: session_id.clone(),
        backtest_run_id: projected.run.id.clone(),
        // From the PROJECTION, never from the caller: this is the pair `#132` said
        // could be individually valid and jointly false.
        strategy_version_id: projected.version.id.clone(),
        request_fingerprint: fingerprint,
        created_at: now_rfc3339(clock)?,
    };
    match sessions
        .claim_session(claim)
        .await
        .map_err(|source| CoachTurnError::SessionConflict {
            session: session_id.clone(),
            source,
        })? {
        CoachSessionClaimResult::Claimed => {}
        // The same request already settled: this is the idempotent answer, and
        // re-asking would bill a second call to overwrite a record that cannot be
        // overwritten anyway.
        CoachSessionClaimResult::Existing(session) => return Ok(session),
        CoachSessionClaimResult::ExistingPending(session) => {
            // We hold the process-local entry, so nobody in THIS process is running
            // it: the claim was left by an earlier lifetime. Finalize it as the
            // typed interruption — without a second provider call, because the turn
            // really did end without an answer and re-asking on the claimant's
            // behalf spends money on a turn nobody is waiting for.
            let detail = format!(
                "the claim on run `{}` (version `{}`) made at {} was left unfinished by an \
                 earlier process lifetime",
                session.backtest_run_id.as_str(),
                session.strategy_version_id.as_str(),
                session.created_at
            );
            return settle_failure(
                sessions,
                &session_id,
                None,
                CoachFailure::Interrupted { detail },
            )
            .await;
        }
    }

    // 4-7. The claim is ours: run the turn under it. Split out so the CLAIMING half
    //      above and the SPENDING half below are each readable in one screen — and
    //      so the guard's lifetime visibly spans both.
    settle_claimed_turn(
        provider,
        sessions,
        &session_id,
        &projected,
        legacy,
        context,
        rendered,
        &tools,
        settings,
    )
    .await
}

/// Steps 4-7 of the turn, with the claim already committed and the process-local
/// entry already held: the legacy refusal, the two pre-call budget refusals, the one
/// call, and the routing of its response into the single settlement.
///
/// Every path here settles the claim exactly once or returns a
/// [`CoachTurnError`] that leaves it `pending` for a later turn to finalize — there
/// is no path that silently abandons it.
#[allow(clippy::too_many_arguments)]
async fn settle_claimed_turn<P, R>(
    provider: &P,
    sessions: &R,
    session_id: &CoachingSessionId,
    projected: &ProjectedRun,
    legacy: bool,
    context: Result<CoachContext, CoachFailure>,
    rendered: String,
    tools: &[ToolDefinition],
    settings: &CoachTurnSettings,
) -> Result<CoachingSession, CoachTurnError>
where
    P: AttributedCoachProvider + Sync,
    R: CoachingRepository + Sync,
{
    // 4. A pre-0006 run: recorded, never approximated. Re-running a child on
    //    DIFFERENT data would produce a comparison that looks valid and is not.
    if legacy {
        let detail = format!(
            "backtest run `{}` was written before migration 0006 and records no input \
             provenance, so a child of it cannot be re-backtested on the same data",
            projected.run.id.as_str()
        );
        return settle_failure(
            sessions,
            session_id,
            None,
            CoachFailure::MissingBacktestInputs { detail },
        )
        .await;
    }

    // 5a. The context sub-budget (grill L4), refused pre-call and free.
    let context = match context {
        Ok(context) => context,
        Err(failure) => return settle_failure(sessions, session_id, None, failure).await,
    };
    debug_assert_eq!(
        rendered,
        context.render(),
        "the fed context is the sent one"
    );

    // Exactly two messages: the resolved system prompt and the projection. The user
    // message is the SAME rendered bytes the fingerprint covered.
    let messages = vec![
        Message::system(settings.prompt.clone()),
        Message::user(rendered),
    ];

    // 5b. The whole-turn ceiling, which the sub-budget cannot see (PR #128, C1).
    if let Err(failure) = check_turn_budget(&messages, tools, DEFAULT_MAX_TURN_BYTES) {
        return settle_failure(sessions, session_id, None, failure).await;
    }

    // 6. ONE call, under the wall-clock guard. No retries, no nudges (grill L3).
    let started = Instant::now();
    let attributed = match tokio::time::timeout(
        settings.turn_timeout,
        provider.attributed_chat(messages, tools, &settings.config),
    )
    .await
    {
        Err(_elapsed) => {
            // The MEASURED wait, not the configured budget (PR #128, finding 10):
            // the recorded number is read as evidence of what happened.
            let failure = CoachFailure::ProviderTimeout {
                elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            };
            let call_id = provider.attempted_call_id().map_err(attribution_error)?;
            return settle_failure(sessions, session_id, call_id, failure).await;
        }
        Ok(Err(AttributedCallError::Provider {
            error: LlmError::Provider(detail),
            llm_call_id,
        })) => {
            // r1.s2.w4: a TRANSPORT fault is a RECORDED outcome. The error text is
            // scrubbed on the way in — an error body can echo the request that
            // produced it.
            let failure = CoachFailure::TransportFailure {
                detail: settings.redactor.redact(&detail),
            };
            return settle_failure(sessions, session_id, llm_call_id, failure).await;
        }
        // NOT a coaching outcome (PR #128, finding 5): an unpriced model or a failed
        // ledger insert is this process faulting, and recording it as
        // `TransportFailure` would write "the coach's provider call failed" into the
        // audit trail for something the provider never did.
        Ok(Err(AttributedCallError::Provider { error: local, .. })) => {
            return Err(CoachTurnError::LocalFault(local));
        }
        Ok(Err(other)) => return Err(attribution_error(other)),
        Ok(Ok(attributed)) => attributed,
    };

    // 7. Route the one response and settle the claim exactly once.
    let llm_call_id = Some(attributed.llm_call_id);
    match classify(
        &settings.redactor,
        &projected.version.dsl,
        attributed.response.tool_calls,
    ) {
        Ok(TurnAnswer::Proposal(proposal)) => settle(
            sessions,
            session_id,
            llm_call_id,
            SessionOutcome::Proposed { proposal },
        )
        .await
        .map_err(|source| CoachTurnError::Record { source }),
        // #131: structural advice, recorded honestly. No proposal row, no child.
        Ok(TurnAnswer::Inapplicable { intent, evidence }) => {
            settle_failure(
                sessions,
                session_id,
                llm_call_id,
                CoachFailure::InapplicableAdvice { intent, evidence },
            )
            .await
        }
        Err(failure) => settle_failure(sessions, session_id, llm_call_id, failure).await,
    }
}

/// Map an attribution fault onto the turn's error taxonomy. `Provider` is handled
/// at the call site (it is the only variant that can still be a recorded outcome).
fn attribution_error(error: AttributedCallError) -> CoachTurnError {
    match error {
        AttributedCallError::Provider { error, .. } => CoachTurnError::LocalFault(error),
        // Both land on `LedgerRowMissing`, and the shared arm is the honest shape:
        // the turn reached the provider and cannot say which ledger row is its own.
        // A shrunk buffer gets there by LOSING the ids this call minted rather than
        // by never capturing one, but the consequence — and what a caller may do
        // about it — is identical, and the distinction survives in the source error.
        AttributedCallError::LedgerRowMissing | AttributedCallError::CaptureBufferShrank { .. } => {
            CoachTurnError::LedgerRowMissing
        }
        AttributedCallError::LedgerRowsAmbiguous { seen } => {
            CoachTurnError::LedgerRowsAmbiguous { seen }
        }
    }
}

/// Settle the claim with a FAILED outcome — the never-silence path.
///
/// On the double fault the returned error carries BOTH the write error and the
/// reason that never reached the row: that reason exists only in this frame.
async fn settle_failure<R: CoachingRepository>(
    sessions: &R,
    session_id: &CoachingSessionId,
    llm_call_id: Option<LlmCallId>,
    failure: CoachFailure,
) -> Result<CoachingSession, CoachTurnError> {
    let reason = failure.clone();
    settle(
        sessions,
        session_id,
        llm_call_id,
        SessionOutcome::Failed { failure },
    )
    .await
    .map_err(|source| CoachTurnError::RecordFailed {
        failure: Box::new(reason),
        source,
    })
}

/// The ONE settle path in production (`w4`'s contract): `finish_session` moves the
/// claimed `pending` row to its single initial outcome and returns **what was
/// recorded**, not what was submitted.
///
/// `save_session` keeps its initial-only contract for the repository's own tests and
/// gains no production caller here — that bypass is what `scripts/check-coach-boundary.sh`
/// keeps retired.
async fn settle<R: CoachingRepository>(
    sessions: &R,
    session_id: &CoachingSessionId,
    llm_call_id: Option<LlmCallId>,
    outcome: SessionOutcome,
) -> Result<CoachingSession, DataError> {
    sessions
        .finish_session(
            session_id,
            InitialCoachOutcome {
                llm_call_id,
                outcome,
            },
        )
        .await
}

/// The claim's `created_at`, from the injected clock — the one coaching timestamp
/// the caller supplies, because a claim's time is the time the TURN began.
///
/// Validated here rather than at the repository, for the reason `w4` recorded (PR
/// #128, finding H3): a row that reads back malformed forever is worse than a write
/// refused now.
fn now_rfc3339<C: Clock>(clock: &C) -> Result<String, CoachTurnError> {
    let now_ms = clock.now_ms();
    DateTime::from_timestamp_millis(now_ms)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Millis, true))
        .ok_or_else(|| CoachTurnError::Clock {
            detail: format!("clock.now_ms() {now_ms} is out of DateTime range"),
        })
}
