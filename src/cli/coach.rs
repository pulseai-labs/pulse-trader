//! `pulse coach <run-id>` — the coach composition root + debug verb (r1.s2.w3,
//! ADR-0021 / ADR-0017).
//!
//! **A developer/debug surface, and it claims no user journey** (A4, and the
//! operator's CLI-is-a-dev-surface ruling): the product surface for coaching is
//! `r1.s4`'s rail in the app. This verb exists so a human can drive one real turn
//! against the configured provider and read what was recorded.
//!
//! This is the ONE place the coach's concrete stack is assembled, keeping every
//! layer generic underneath — the `run_compose` precedent, with the coach's own
//! prompt and tools. The transport posture and the chat knobs are NOT chosen here:
//! they are the desktop rail's too, so they live in
//! [`adapters::llm::coach_transport`](crate::adapters::llm::coach_transport) and
//! this root builds through it (#164, PR #165 review R5):
//!
//! ```text
//! resolve_llm_api_key()  →  ApiKey (opaque; carries the CredentialSource label)
//! coach_transport::coach_provider(key, base_url)   ← one attempt, coach timeout
//!                      →  RedactingLoggingProvider::new(inner, capturing, clock, redactor, prices)
//!                         .with_created_by(CoachLlm).with_key_source(source)
//!                         .with_prompt_version(Some(sha256(resolved coach.md)))   ← audit C2
//!                      →  AttributedProvider::new(decorator, buffer)   ← response + its LlmCallId
//!                      →  run_coach_turn(source, provider, sessions, registry, …)
//!                                             →  ONE recorded CoachingSession
//! ```
//!
//! **The turn is sealed** (r1.s4.w1, `#132`). This root no longer builds a `Coach`
//! out of fragments and hands it a run, a trade vector and a version: it builds the
//! ports and passes IDENTIFIERS. It also no longer coordinates the run and strategy
//! repositories — [`SqliteCoachTurnSource`](crate::adapters::db::SqliteCoachTurnSource)
//! owns that projection now (ADR-0015: adapters and the CLI do not coordinate
//! repositories).
//!
//! **Injectable core.** [`run_coach_with`] takes the LLM-side deps bundled in a
//! [`CoachWiring`] plus the projection and the coaching repo, so the offline tests
//! (`coach_turn` = demo `d6`, `coach_failures` = demo `d7`, `coach_redaction`,
//! `coach_turn_boundary`) drive the SAME composition with a scripted provider over
//! the REAL sealed turn, REAL `apply()`, REAL repos and a `tempfile` `SQLite` —
//! never a live LLM, never the network.
//!
//! **Prompt resolution lives in the core, not the caller** (unlike
//! `ComposeWiring.prompt`): the resolved prompt and the `prompt_version` stamped on
//! the ledger row must be the same bytes, and the only way to guarantee that is to
//! resolve them together, once, here.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;

use crate::adapters::clock::SystemClock;
use crate::adapters::db::{Db, SqliteCoachTurnSource, SqliteCoachingRepo, SqliteLlmCallRepo};
use crate::adapters::llm::attributed::AttributedProvider;
use crate::adapters::llm::coach_transport::{coach_config, coach_provider};
use crate::adapters::llm::redacting_logging::RedactingLoggingProvider;
use crate::agent::config::load_coach_prompt_from;
use crate::agent::{DEFAULT_MAX_DSL_BYTES, DEFAULT_TURN_TIMEOUT, LlmCallCapture};
use crate::application::coach::{
    CoachTurnRegistry, CoachTurnRequest, CoachTurnSettings, run_coach_turn,
};
use crate::domain::Redactor;
use crate::domain::strategy::CreatedBy;
use crate::domain::{
    BacktestRunId, Clock, CoachFailure, CoachTurnSource, CoachingRepository, CoachingSession,
    CoachingSessionId, CredentialSource, LlmCallRepository, LlmConfig, LlmProvider, PriceTable,
    SessionOutcome,
};

use crate::adapters::llm::capturing::CapturingRepo;

/// `pulse coach <RUN_ID> [--db <path>]`.
#[derive(Debug, clap::Args)]
pub struct CoachArgs {
    /// The persisted backtest run to coach on.
    pub run_id: String,
    /// Override the `pulse.db` path (defaults to the app-support location).
    #[arg(long)]
    pub db: Option<PathBuf>,
}

/// The LLM-side wiring for one coach turn — everything the injectable core needs
/// that a test wants to substitute.
pub struct CoachWiring<P, R, C> {
    /// The inner provider (live `OpenAiCompatProvider`, or a test's scripted double).
    pub provider: P,
    /// The `LlmCall` ledger repo the redacting decorator writes each row through.
    pub llm_repo: R,
    /// The NFR-6 secret scrubber for the PERSISTED prompt/completion copy.
    pub redactor: Redactor,
    /// The cost table (loaded from `config/prices.toml` in the live arm).
    pub prices: PriceTable,
    /// The SINGLE shared clock injected into the decorator AND the repos (#82).
    pub clock: C,
    /// Which credential source supplied the API key — a LABEL, never the key.
    pub key_source: Option<CredentialSource>,
    /// The per-request chat config.
    pub config: LlmConfig,
    /// The prompt-override directory. The live arm passes the RESOLVED
    /// `$PULSE_PROMPT_DIR` ([`prompt_override_dir`]); a test passes an explicit
    /// directory so it never mutates process-global env. `None` means "no overlay
    /// — use the compiled-in default", which is what keeps the offline tests
    /// hermetic against a developer's exported `$PULSE_PROMPT_DIR`.
    pub prompt_dir: Option<PathBuf>,
    /// Override the per-turn wall-clock guard (audit C5). `None` = the default.
    pub turn_timeout: Option<Duration>,
    /// Override the pre-call DSL size budget. `None` = the default.
    pub max_dsl_bytes: Option<usize>,
    /// The shared buffer the capturing ledger repo pushes minted ids into, and the
    /// attributed provider reads back to name the turn's ledger row.
    ///
    /// It is handed to the ATTRIBUTED PROVIDER here, not to the turn: the pairing
    /// obligation `#132` showed a caller can get wrong is discharged in this one
    /// place, and the sealed module never sees a capture handle at all.
    pub captured: LlmCallCapture,
    /// The session id to claim and settle under. `None` mints a fresh one — the
    /// live arm's case, where every `pulse coach` invocation is a new turn.
    ///
    /// An explicit id is what makes a turn RETRYABLE: the same id with the same
    /// request is the idempotent answer, and a `pending` row under it is a claim to
    /// finalize. `r1.s4`'s rail supplies one; the debug verb does not need to.
    pub session_id: Option<CoachingSessionId>,
    /// The process-local single-flight registry. `None` mints a fresh one per
    /// invocation, which is correct for a CLI process that runs exactly one turn and
    /// deliberately blind for anything that runs several: a registry that never saw
    /// the first turn can never say "in flight" about it.
    pub registry: Option<Arc<CoachTurnRegistry>>,
}

/// The outcome of one coach turn at the CLI edge.
pub struct CoachCliOutcome {
    /// The single recorded session — a proposal or a typed failure, never neither.
    pub session: CoachingSession,
    /// The version stamped on the turn's ledger row: SHA-256 hex of the RESOLVED
    /// prompt (audit C2).
    pub prompt_version: String,
}

/// The injectable, doubleable core: resolve the prompt, assemble the attributed
/// provider, and run exactly one SEALED coach turn.
///
/// It hands the turn two identifiers and three ports. It does not load the run, the
/// trades or the version — that is the projection's job now — and it does not pair a
/// provider with a capture handle at the turn's boundary, because the attributed
/// provider assembled here has already done it.
///
/// # Errors
///
/// Returns an error when the prompt overlay exists but cannot be read, when the run
/// is absent or its projection cannot be loaded, when the session id is already in
/// flight or held by a different request, when this process faults on the provider
/// call path (an unpriced model, a failed ledger write — the turn never happened),
/// or when the session cannot be recorded. A provider TRANSPORT fault is not an
/// error here: it is a recorded `TransportFailure` session, which the live arm below
/// then exits non-zero on (recorded AND loud, ADR-0017).
///
/// The `private_bounds` allow is the sealed-trait shape, stated on purpose:
/// [`CoachTurnSource`] is `pub(crate)` because nothing outside this crate may
/// implement or name the projection port (ADR-0015), while this function is `pub`
/// because the four offline coach test binaries — separate crates — drive the
/// injectable core through it. Widening the port to `pub` to silence the lint would
/// re-open exactly the surface `#132` asked to seal; narrowing this function would
/// take the regression suite off the production path. So the bound stays private and
/// the seam stays testable: an outside caller can CALL `run_coach_with`, and still
/// cannot name a type to satisfy `S` that this crate did not give it.
#[allow(private_bounds)]
pub async fn run_coach_with<P, L, S, K, C>(
    wiring: CoachWiring<P, L, C>,
    source: &S,
    coaching_repo: &K,
    run_id: &BacktestRunId,
) -> anyhow::Result<CoachCliOutcome>
where
    P: LlmProvider + Send + Sync,
    L: LlmCallRepository + Send + Sync,
    S: CoachTurnSource + Send + Sync,
    K: CoachingRepository + Send + Sync,
    C: Clock + Copy + Send + Sync,
{
    let CoachWiring {
        provider,
        llm_repo,
        redactor,
        prices,
        clock,
        key_source,
        config,
        prompt_dir,
        turn_timeout,
        max_dsl_bytes,
        captured,
        session_id,
        registry,
    } = wiring;

    // The prompt and its version, resolved together from the same bytes (audit C2).
    let prompt =
        load_coach_prompt_from(prompt_dir.as_deref()).context("resolving the coach prompt")?;

    // The turn speaks to the PORT, behind the decorator that redacts what is
    // persisted and stamps the ledger row's cost, actor and prompt version — and
    // behind the attribution that pairs the response with the row it minted.
    let decorated =
        RedactingLoggingProvider::new(provider, llm_repo, clock, redactor.clone(), prices)
            .with_created_by(CreatedBy::CoachLlm)
            .with_key_source(key_source)
            .with_prompt_version(Some(prompt.version.clone()));
    let attributed = AttributedProvider::new(decorated, captured);

    // The SAME redactor on both roads: the decorator scrubs the ledger copy of the
    // prompt/completion, the turn scrubs the tool arguments that become stored
    // domain values (AC-3).
    let settings = CoachTurnSettings {
        prompt: prompt.text,
        prompt_version: Some(prompt.version.clone()),
        config,
        redactor,
        turn_timeout: turn_timeout.unwrap_or(DEFAULT_TURN_TIMEOUT),
        max_dsl_bytes: max_dsl_bytes.unwrap_or(DEFAULT_MAX_DSL_BYTES),
    };

    let registry = registry.unwrap_or_else(|| Arc::new(CoachTurnRegistry::new()));
    let request = CoachTurnRequest {
        session_id: session_id
            .unwrap_or_else(|| CoachingSessionId::new(uuid::Uuid::new_v4().to_string())),
        run_id: run_id.clone(),
    };

    let session = run_coach_turn(
        source,
        &attributed,
        coaching_repo,
        &registry,
        &clock,
        &settings,
        request,
    )
    .await?;

    Ok(CoachCliOutcome {
        session,
        prompt_version: prompt.version,
    })
}

/// The live arm: assemble the real stack against `db` and run one turn, printing
/// the proposal or the typed failure.
///
/// # Errors
///
/// Returns an error when the credential cannot be resolved, the run/version is
/// absent, the transport fails, or the session cannot be recorded — each preserved
/// with its context at the CLI edge (ADR-0017).
pub async fn run_coach(db: Option<&Db>, args: &CoachArgs) -> anyhow::Result<()> {
    let db = db.context("`pulse coach` needs an opened database")?;
    let key = crate::adapters::secrets::resolve_llm_api_key()
        .map_err(|e| anyhow::anyhow!("resolve LLM API key: {e}"))?;
    let key_source = Some(key.source());
    // Tag the live key so an accidental echo is scrubbed at rest too (structural
    // api-key-shaped stripping is always on) — the `run_compose` discipline.
    let redactor = Redactor::from_config(vec![key.expose().to_owned()]);

    let transport =
        crate::agent::config::load_llm_transport().context("loading the [llm] transport config")?;
    let prices = crate::agent::config::load_price_table().context("loading the price table")?;
    let provider = coach_provider(key.expose(), transport.base_url.as_deref());

    let clock = SystemClock;
    let captured: LlmCallCapture = Arc::new(Mutex::new(Vec::new()));
    let llm_repo = CapturingRepo::new(
        SqliteLlmCallRepo::with_deps(db.pool().clone(), clock),
        Arc::new(Mutex::new(None)),
        Arc::clone(&captured),
    );

    let wiring = CoachWiring {
        provider,
        llm_repo,
        redactor,
        prices,
        clock,
        key_source,
        // The coach's own knobs, shared with the desktop rail (#164): GLM spends
        // thinking tokens against the cap BEFORE the tool call, so a cap sized for
        // a one-sentence `llm-check` prompt produces a turn with no tool call —
        // which this taxonomy records as `ZeroCalls`, indistinguishable from a
        // model that genuinely declined to propose.
        config: coach_config(transport.model.as_deref()),
        // The live arm honours the operator's `$PULSE_PROMPT_DIR/coach.md` overlay
        // — the whole point of the resolved-bytes prompt version (audit C2) is that
        // an overlay edit changes what the coach says AND what the ledger records.
        prompt_dir: crate::agent::config::prompt_override_dir(),
        turn_timeout: None,
        max_dsl_bytes: None,
        captured,
        // One turn per invocation: a fresh id, and a registry that outlives nothing.
        session_id: None,
        registry: None,
    };

    let source = SqliteCoachTurnSource::new(db.pool().clone());
    let coaching_repo = SqliteCoachingRepo::with_deps(db.pool().clone(), clock);

    let outcome = run_coach_with(
        wiring,
        &source,
        &coaching_repo,
        &BacktestRunId::new(args.run_id.clone()),
    )
    .await?;

    print_outcome(&outcome);

    // r1.s2.w4: a transport fault is now RECORDED (the session row above) AND
    // LOUD. Routing it into the taxonomy must not quietly turn a provider outage
    // into a successful `pulse coach` invocation — the row is for the audit trail,
    // the non-zero exit is for the human and the shell (ADR-0017). The other six
    // failures are genuine coaching outcomes and exit 0.
    if let SessionOutcome::Failed {
        failure: CoachFailure::TransportFailure { detail },
    } = &outcome.session.outcome
    {
        anyhow::bail!(
            "the coach's provider call failed: {detail} (recorded as coaching session {})",
            outcome.session.id.as_str()
        );
    }
    Ok(())
}

/// Print one recorded turn. A failure is printed as loudly as a proposal — the
/// whole point of the taxonomy is that a failed turn is a result, not a blank.
fn print_outcome(outcome: &CoachCliOutcome) {
    println!("session:        {}", outcome.session.id.as_str());
    println!(
        "run:            {}",
        outcome.session.backtest_run_id.as_str()
    );
    println!(
        "version:        {}",
        outcome.session.strategy_version_id.as_str()
    );
    println!("prompt_version: {}", outcome.prompt_version);
    match outcome.session.llm_call_id.as_ref() {
        Some(id) => println!("llm_call:       {}", id.as_str()),
        None => println!("llm_call:       (none — {})", no_ledger_reason(outcome)),
    }
    match &outcome.session.outcome {
        SessionOutcome::Proposed { proposal } => {
            let (path, value) = match &proposal.mutation {
                crate::domain::Mutation::SetParam { path, new_value } => (path, new_value),
            };
            println!("\nPROPOSAL");
            println!("  path:       {path}");
            println!("  new_value:  {value:?}");
            println!("  hypothesis: {}", proposal.hypothesis.as_str());
        }
        SessionOutcome::Failed { failure } => {
            println!("\nRECORDED FAILURE");
            println!("  {failure}");
            // r1.s4.w1 (ADR-0017): the three failures this item can now produce are
            // printed as themselves rather than flattened into one line of prose.
            // Each says something structurally different about what to do next, and
            // an operator reading `pulse coach` is deciding exactly that.
            match failure {
                CoachFailure::InapplicableAdvice { intent, evidence } => {
                    println!("\n  STRUCTURAL ADVICE (this release cannot apply it)");
                    println!("    intent:   {intent}");
                    println!("    evidence: {evidence}");
                    println!(
                        "    recorded rather than approximated with a parameter change (#131)"
                    );
                }
                CoachFailure::MissingBacktestInputs { detail } => {
                    println!("\n  NO INPUT PROVENANCE");
                    println!("    {detail}");
                    println!("    re-run the backtest to produce a run this release can coach on");
                }
                CoachFailure::Interrupted { detail } => {
                    println!("\n  INTERRUPTED CLAIM (finalized, not re-asked)");
                    println!("    {detail}");
                    println!("    no second provider call was made on the claimant's behalf");
                }
                _ => {}
            }
        }
        // r1.s4.w4: `pulse coach` runs one turn to completion, so it never prints a
        // claim. Printing "(none)" here would be the wrong shape of honest — the
        // turn this command reports on either produced something or recorded why
        // not, and a pending row on THIS path is a wiring fault worth naming.
        SessionOutcome::Pending => {
            println!("\nSTILL PENDING");
            println!("  the turn was claimed and never settled — this is a wiring fault");
        }
    }
}

/// Why a session names no ledger row.
///
/// A missing `llm_call_id` used to print "the turn failed before any provider
/// call" unconditionally, which is FALSE for the two failures that reach the
/// provider and come back with nothing to bill — a transport fault and a timeout
/// (PR #128, finding 5). The operator reading this line is deciding whether a
/// billed call happened; the answer has to come from the recorded failure, not
/// from the NULL alone.
fn no_ledger_reason(outcome: &CoachCliOutcome) -> &'static str {
    match &outcome.session.outcome {
        SessionOutcome::Failed {
            failure: CoachFailure::TransportFailure { .. },
        } => "the call was attempted and produced no usable exchange",
        SessionOutcome::Failed {
            failure: CoachFailure::ProviderTimeout { .. },
        } => "the call was attempted and did not answer inside the turn's budget",
        // r1.s4.w1: an interrupted claim is finalized by a process that made no call
        // of its own — and cannot know whether the claimant's call happened. Saying
        // "before any provider call" here would be a claim about someone else's
        // process, which is exactly what the `Interrupted` tag exists to avoid.
        SessionOutcome::Failed {
            failure: CoachFailure::Interrupted { .. },
        } => "this process finalized an abandoned claim and made no call of its own",
        _ => "the turn failed before any provider call",
    }
}
