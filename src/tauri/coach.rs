//! The coach rail's wire contract and its two command cores (r1.s4.w3,
//! ADR-0015 / ADR-0016 / ADR-0020 / ADR-0021).
//!
//! **Ring-owned wire types, not `specta` derives on domain types** — the
//! [`BacktestRunDto`](super::backtest::BacktestRunDto) pattern. Every crossing type
//! is declared here and built by a pure projection, so the coaching domain never
//! grows a serialization concern and the rail never sees a shape that changed
//! because a domain field moved.
//!
//! **Every decimal, id and timestamp crosses as an exact string** (NFR-2). The rail
//! renders them verbatim and computes nothing: no money math, no percentages, no
//! deltas. Counts stay `u32` because they are counts.
//!
//! **`recovery` is chosen HERE, by the backend, from the typed
//! [`CoachFailure`](crate::domain::CoachFailure) variant.** A TypeScript mapping
//! from failure kind to recovery text would be a second, silently-diverging copy of
//! a decision the domain already makes — and the one that a new variant would slip
//! past. [`recovery_for`] is an exhaustive `match`, so a new failure kind is a
//! compile error here rather than an empty box on someone's screen.
//!
//! **Cost and prompt version are READ from the ledger row the session names**
//! ([`LlmCallRepository::get_call`]), never recomputed. The rail's claim is "this is
//! what the call cost and this is the prompt that produced it" — recomputing either
//! would make it a claim about what the price table says today.
//!
//! # The two cores, and where the credential lives
//!
//! [`coach_turn_core`] is transport-free and takes its provider in a
//! [`CoachTurnDeps`], exactly as [`compose_strategy_core`](super::commands::compose_strategy_core)
//! takes a [`ComposeDeps`](super::commands::ComposeDeps): the `#[tauri::command]`
//! wrapper in `commands.rs` loads the config overlays, resolves the credential,
//! builds the redactor and the provider from it, and hands the result here. The key
//! therefore never appears in an argument, a return value, an event, an error or a
//! DTO — it never leaves the wrapper — and the offline suite can still drive the
//! REAL core over a scripted provider, which is what makes `tests/tauri_coach.rs`
//! possible at all.
//!
//! [`coach_decide_core`] performs no provider call: an accept re-runs the parent's
//! exact persisted inputs through the engine, and the coach is not asked anything.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use chrono::DateTime;
use serde::{Deserialize, Serialize};

use crate::adapters::broker::BinanceAdapter;
use crate::adapters::clock::SystemClock;
use crate::adapters::db::{SqliteCoachAcceptanceRepo, SqliteCoachTurnSource, SqliteCoachingRepo};
use crate::adapters::llm::attributed::AttributedProvider;
use crate::adapters::llm::capturing::CapturingRepo;
use crate::adapters::llm::redacting_logging::RedactingLoggingProvider;
use crate::agent::{DEFAULT_MAX_DSL_BYTES, DEFAULT_TURN_TIMEOUT};
use crate::application::coach::{
    CoachTurnError, CoachTurnRequest, CoachTurnSettings, run_coach_turn,
};
use crate::application::coach_decision::{
    AcceptedCoachResult, CoachAction, CoachDecisionError, CoachDecisionOutcome,
    CoachDecisionRequest, run_coach_decision,
};
use crate::domain::backtest::SummaryStats;
use crate::domain::strategy::CreatedBy;
use crate::domain::{
    AcceptFailureStage, BacktestRunId, Clock, CoachAcceptFailure, CoachFailure, CoachingRepository,
    CoachingSession, CoachingSessionId, CredentialSource, Disposition, InitialCoachOutcome,
    LlmCallRepository, LlmConfig, LlmProvider, Mutation, ParamKind, ParamValue, PriceTable,
    Proposal, Redactor, SessionOutcome,
};

use super::commands::{DesktopState, OperationKey};
use super::error::{BusError, BusErrorCode};

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// What the rail asks for when the trader presses "Ask the coach", and again on
/// every reload.
///
/// **The DESKTOP mints the session id** (a UUID) and passes the same one back
/// forever after; the backend never mints one. That is what makes a reload
/// idempotent rather than a second billable turn: the id is the key the durable
/// session is stored under, and only the caller that owns the screen knows whether
/// this is a new ask or the same one being looked at again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CoachTurnRequestDto {
    /// The persisted backtest run to coach on.
    pub run_id: String,
    /// The session id this turn is recorded under — minted by the desktop.
    pub session_id: String,
}

/// What the trader did with the coach's proposal.
///
/// Internally tagged on `kind`, so the generated TypeScript is the discriminated
/// union the rail switches on rather than three optional fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CoachActionDto {
    /// Replace the proposal's value with the trader's own edit, and re-validate.
    ///
    /// `new_value` is the value as TEXT — the same exact-string discipline every
    /// other decimal crosses under. Which typed [`ParamValue`] it becomes is decided
    /// by the CURRENT proposal's own parameter kind, not by parsing the string and
    /// guessing: a `"21"` that is a period and a `"21"` that is a threshold are
    /// different mutations, and only the stored proposal knows which leaf this is.
    #[serde(rename_all = "camelCase")]
    Modify {
        /// The sweepable leaf being retuned.
        path: String,
        /// Its new value, as text.
        new_value: String,
    },
    /// Record the terminal rejection. No child, no run.
    Reject,
    /// Re-apply the current mutation and re-backtest it on the parent's exact
    /// persisted inputs.
    Accept,
}

/// One decision: which session, and what the trader did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CoachDecisionRequestDto {
    /// The coaching session being decided.
    pub session_id: String,
    /// What the trader did.
    pub action: CoachActionDto,
}

// ---------------------------------------------------------------------------
// Response DTOs
// ---------------------------------------------------------------------------

/// One turn's cost, in the price table's own billing currency.
///
/// A pair rather than a bare number: the ledger bills in the model's native
/// currency (CNY for GLM), and an amount with no currency beside it is the kind of
/// figure a reader silently reads as dollars.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CoachCostDto {
    /// The exact decimal cost, as a string.
    pub amount: String,
    /// Its billing currency (`"CNY"`).
    pub currency: String,
}

/// The proposed change: one path, one value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct MutationDto {
    /// The sweepable leaf's locator.
    pub path: String,
    /// The value to write there, as an exact string.
    pub new_value: String,
}

/// The latest accept attempt's typed failure, when the most recent one failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct AcceptFailureDto {
    /// Where the accept stopped (`apply` / `load_inputs` / … / `persist`).
    pub stage: String,
    /// What went wrong, stated for the trader.
    pub message: String,
    /// What the failure is about, when it is about one thing.
    pub subject: Option<String>,
}

/// The coach's single proposal, as the rail's card renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ProposalDto {
    /// The one change.
    pub mutation: MutationDto,
    /// Why the coach believes it helps.
    pub hypothesis: String,
    /// Where the proposal stands.
    pub disposition: String,
    /// The child version an accept minted.
    pub child_version_id: Option<String>,
    /// The re-backtest run of that child.
    pub accepted_run_id: Option<String>,
    /// The latest accept attempt's typed failure, if it failed.
    pub accept_failure: Option<AcceptFailureDto>,
}

/// A recorded turn failure and what the trader can do about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CoachFailureDto {
    /// The typed variant's own `snake_case` tag.
    pub kind: String,
    /// The failure's `Display` rendering — prose, already scrubbed.
    pub detail: String,
    /// The named recovery, chosen by the backend from the typed variant.
    pub recovery: String,
}

/// One recorded coach turn, as the rail shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CoachSessionDto {
    /// The session id the desktop minted.
    pub session_id: String,
    /// The persisted run this turn coached on.
    pub run_id: String,
    /// The strategy version whose DSL the proposal mutates.
    pub version_id: String,
    /// `pending` | `proposed` | `failed`.
    pub outcome: String,
    /// The one proposal, when the turn produced one.
    pub proposal: Option<ProposalDto>,
    /// The one recorded failure, when it did not.
    pub failure: Option<CoachFailureDto>,
    /// The ledger row this turn produced, if any.
    pub llm_call_id: Option<String>,
    /// That row's recorded cost.
    pub cost: Option<CoachCostDto>,
    /// That row's recorded prompt version.
    pub prompt_version: Option<String>,
    /// When the turn was claimed (RFC3339 UTC).
    pub created_at: String,
}

/// One persisted run summary, every money value an exact string.
///
/// `sharpe`/`sortino` stay nullable numbers for the reason
/// [`BacktestRunDto`](super::backtest::BacktestRunDto) keeps them so: they are
/// genuinely `f64`-derived, and `null` is already how "not enough trades to
/// compute" is spelled everywhere else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SummaryDto {
    /// Completed trades.
    pub trade_count: u32,
    /// How many won.
    pub win_count: u32,
    /// How many lost.
    pub loss_count: u32,
    /// Fraction of trades that won.
    pub win_rate: String,
    /// Sum of winning P&L.
    pub gross_profit: String,
    /// Sum of losing P&L magnitudes.
    pub gross_loss: String,
    /// Net P&L.
    pub net_pnl: String,
    /// Gross profit over gross loss; `null` when there is no loss to divide by.
    pub profit_factor: Option<String>,
    /// Mean winning trade.
    pub avg_win: String,
    /// Mean losing trade.
    pub avg_loss: String,
    /// Mean P&L per trade — the rail's headline comparison.
    pub expectancy: String,
    /// Largest peak-to-trough equity drop.
    pub max_drawdown: String,
    /// Longest winning streak.
    pub max_win_streak: u32,
    /// Longest losing streak.
    pub max_loss_streak: u32,
    /// Total taker commission.
    pub commission_total: String,
    /// Total signed funding.
    pub funding_total: String,
    /// Sharpe ratio; `null` when undefined for this run.
    pub sharpe: Option<f64>,
    /// Sortino ratio; `null` when undefined for this run.
    pub sortino: Option<f64>,
}

/// Whether the committed child run could be read back.
///
/// Untagged so the wire shape is exactly `"ok" | { failure }`: past the commit the
/// accept has SUCCEEDED, so this is not an error family — it is a note about what
/// this process could re-read, and the rail says so beside two real ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(untagged)]
pub enum ReadBackDto {
    /// The child run read back. Serializes as the bare string `"ok"`.
    Ok(ReadBackOk),
    /// It did not — the accept still committed.
    Failed {
        /// Why the read back failed.
        failure: String,
    },
}

/// The `"ok"` token of [`ReadBackDto`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum ReadBackOk {
    /// The one value.
    Ok,
}

/// What one committed accept produced, as the side-by-side panel needs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedCoachDto {
    /// The child version the accept minted.
    pub child_version_id: String,
    /// The re-backtest run OF that child.
    pub accepted_run_id: String,
    /// The PARENT run's persisted summary — the "before" half.
    pub before: SummaryDto,
    /// The CHILD run's persisted summary. Absent only for a saved-but-unreadable
    /// child run, which is a read failure and not an accept failure.
    pub after: Option<SummaryDto>,
    /// Whether that read back succeeded.
    pub read_back: ReadBackDto,
}

/// The durable result of one decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CoachDecisionDto {
    /// The session as it now stands.
    pub session: CoachSessionDto,
    /// Present only for a committed accept.
    pub accepted: Option<AcceptedCoachDto>,
}

// ---------------------------------------------------------------------------
// The named recoveries (spec §"The rail")
// ---------------------------------------------------------------------------

/// The one named recovery for each typed failure — an exhaustive `match`, so a
/// new [`CoachFailure`] variant is a compile error here rather than a blank box.
///
/// The three specific recoveries name a DIFFERENT next action each; everything
/// else is operational and its next action really is "retry", so saying anything
/// more specific would be inventing advice.
fn recovery_for(failure: &CoachFailure) -> &'static str {
    match failure {
        CoachFailure::InapplicableAdvice { .. } => {
            "the coach's advice was structural; ask again on another run or edit the strategy"
        }
        CoachFailure::MissingBacktestInputs { .. } => "run this version again, then ask the coach",
        CoachFailure::Interrupted { .. } => "start a new coaching session",
        CoachFailure::ZeroCalls
        | CoachFailure::SeveralCalls { .. }
        | CoachFailure::MalformedArguments { .. }
        | CoachFailure::InapplicableMutation { .. }
        | CoachFailure::ProviderTimeout { .. }
        | CoachFailure::ContextOverflow { .. }
        | CoachFailure::TransportFailure { .. } => "retry",
    }
}

/// The failure's own `snake_case` tag — the same token its serde representation
/// carries, produced by an exhaustive `match` rather than a serde round trip.
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

/// The stage's `snake_case` tag — the one `0008`'s `CHECK` enumerates.
fn stage_label(stage: AcceptFailureStage) -> &'static str {
    stage.tag()
}

/// The disposition's `snake_case` tag.
fn disposition_label(disposition: &Disposition) -> &'static str {
    match disposition {
        Disposition::Proposed => "proposed",
        Disposition::Modified => "modified",
        Disposition::Rejected => "rejected",
        Disposition::Accepted { .. } => "accepted",
    }
}

// ---------------------------------------------------------------------------
// Projections
// ---------------------------------------------------------------------------

/// Exact decimal text — the same `.normalize()`d form the database stores.
fn dec(value: rust_decimal::Decimal) -> String {
    value.normalize().to_string()
}

/// A stored count narrowed to the wire's `u32`, refusing rather than clamping —
/// the [`BacktestRunDto`](super::backtest::BacktestRunDto) rule.
fn count(field: &str, value: usize) -> Result<u32, BusError> {
    u32::try_from(value).map_err(|_| {
        BusError::internal(format!(
            "stored `{field}` = {value} does not fit the wire's u32"
        ))
    })
}

/// The mutation's value as the exact text the rail renders and edits.
fn mutation_dto(mutation: &Mutation) -> MutationDto {
    let Mutation::SetParam { path, new_value } = mutation;
    MutationDto {
        path: path.clone(),
        new_value: match new_value {
            ParamValue::Period { value } => value.to_string(),
            ParamValue::Threshold { value } => dec(*value),
        },
    }
}

fn proposal_dto(proposal: &Proposal) -> ProposalDto {
    ProposalDto {
        mutation: mutation_dto(&proposal.mutation),
        hypothesis: proposal.hypothesis.as_str().to_owned(),
        disposition: disposition_label(&proposal.disposition).to_owned(),
        child_version_id: proposal
            .disposition
            .child_version_id()
            .map(|id| id.as_str().to_owned()),
        accepted_run_id: proposal
            .disposition
            .accepted_run_id()
            .map(|id| id.as_str().to_owned()),
        accept_failure: proposal.accept_failure.as_ref().map(accept_failure_dto),
    }
}

fn accept_failure_dto(failure: &CoachAcceptFailure) -> AcceptFailureDto {
    AcceptFailureDto {
        stage: stage_label(failure.stage).to_owned(),
        message: failure.message.clone(),
        subject: failure.subject.clone(),
    }
}

fn failure_dto(failure: &CoachFailure) -> CoachFailureDto {
    CoachFailureDto {
        kind: failure_kind(failure).to_owned(),
        detail: failure.to_string(),
        recovery: recovery_for(failure).to_owned(),
    }
}

/// Project one persisted summary. Fallible for exactly one reason: a stored count
/// that will not fit the wire refuses rather than rendering a plausible false
/// number.
///
/// `pub(crate)` so `compare_child_run` (r2.s1.w4 C3) renders the same cell
/// shape — one summary projection, one compare table.
pub(crate) fn summary_dto(summary: &SummaryStats) -> Result<SummaryDto, BusError> {
    Ok(SummaryDto {
        trade_count: count("trade_count", summary.trade_count)?,
        win_count: count("win_count", summary.win_count)?,
        loss_count: count("loss_count", summary.loss_count)?,
        win_rate: dec(summary.win_rate),
        gross_profit: dec(summary.gross_profit),
        gross_loss: dec(summary.gross_loss),
        net_pnl: dec(summary.net_pnl),
        profit_factor: summary.profit_factor.map(dec),
        avg_win: dec(summary.avg_win),
        avg_loss: dec(summary.avg_loss),
        expectancy: dec(summary.expectancy),
        max_drawdown: dec(summary.max_drawdown),
        max_win_streak: count("max_win_streak", summary.max_win_streak)?,
        max_loss_streak: count("max_loss_streak", summary.max_loss_streak)?,
        commission_total: dec(summary.commission_total),
        funding_total: dec(summary.funding_total),
        sharpe: summary.sharpe,
        sortino: summary.sortino,
    })
}

/// Project one recorded session, reading cost and prompt version from the LEDGER
/// row it names — never recomputing either.
///
/// A ledger row the session names but that cannot be read is reported as a session
/// with no cost rather than as a failed turn: the turn happened and its outcome is
/// the thing the rail exists to show, and refusing the whole projection over a
/// missing price would hide it.
async fn session_dto<L: LlmCallRepository>(
    ledger: &L,
    session: &CoachingSession,
) -> Result<CoachSessionDto, BusError> {
    let (outcome, proposal, failure) = match &session.outcome {
        SessionOutcome::Pending => ("pending", None, None),
        SessionOutcome::Proposed { proposal } => ("proposed", Some(proposal_dto(proposal)), None),
        SessionOutcome::Failed { failure } => ("failed", None, Some(failure_dto(failure))),
    };

    // Cost and prompt version are READ from the row the session names, together,
    // so the pair the rail shows is the pair one call actually produced. A session
    // that names no row, or whose row cannot be read, shows neither rather than a
    // half-pair — a cost with no prompt beside it invites the reader to assume the
    // current one.
    // A read FAILURE degrades exactly as a missing row does. `?` here would abort the
    // whole projection over a price lookup and the rail would show nothing at all —
    // no proposal, no recorded failure — for a turn that happened and is on record.
    // The outcome is what this surface exists to show; the cost is a detail beside
    // it, and losing the detail must not lose the thing.
    let ledger_row = match session.llm_call_id.as_ref() {
        Some(call_id) => ledger.get_call(call_id).await.unwrap_or(None),
        None => None,
    };
    let (cost, prompt_version) = match ledger_row {
        Some(row) => (
            Some(CoachCostDto {
                amount: dec(row.cost),
                currency: row.cost_currency,
            }),
            row.prompt_version,
        ),
        None => (None, None),
    };

    Ok(CoachSessionDto {
        session_id: session.id.as_str().to_owned(),
        run_id: session.backtest_run_id.as_str().to_owned(),
        version_id: session.strategy_version_id.as_str().to_owned(),
        outcome: outcome.to_owned(),
        proposal,
        failure,
        llm_call_id: session
            .llm_call_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        cost,
        prompt_version,
        created_at: session.created_at.clone(),
    })
}

fn accepted_dto(accepted: &AcceptedCoachResult) -> Result<AcceptedCoachDto, BusError> {
    let after = match accepted.after.as_ref() {
        Some(summary) => Some(summary_dto(summary)?),
        None => None,
    };
    Ok(AcceptedCoachDto {
        child_version_id: accepted.child_version_id.as_str().to_owned(),
        accepted_run_id: accepted.accepted_run_id.as_str().to_owned(),
        before: summary_dto(&accepted.before)?,
        after,
        read_back: match &accepted.read_back {
            Ok(()) => ReadBackDto::Ok(ReadBackOk::Ok),
            Err(failure) => ReadBackDto::Failed {
                failure: failure.to_string(),
            },
        },
    })
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Map the sealed turn's error taxonomy onto the one bus shape.
///
/// [`CoachTurnError::TurnInFlight`] is the only [`BusErrorCode::Busy`] here: it is
/// the refusal that means "the answer is already coming", and the rail renders it
/// as running rather than as a fault.
fn turn_failure(error: &CoachTurnError) -> BusError {
    let code = match error {
        CoachTurnError::TurnInFlight { .. } => BusErrorCode::Busy,
        CoachTurnError::RunNotFound(_)
        | CoachTurnError::Projection { .. }
        | CoachTurnError::SessionConflict { .. }
        | CoachTurnError::Record { .. }
        | CoachTurnError::RecordFailed { .. } => BusErrorCode::Data,
        CoachTurnError::LocalFault(_) => BusErrorCode::Llm,
        CoachTurnError::LedgerRowMissing
        | CoachTurnError::LedgerRowsAmbiguous { .. }
        | CoachTurnError::Fingerprint(_)
        | CoachTurnError::Clock { .. } => BusErrorCode::Internal,
    };
    BusError::new(code, error.to_string())
}

/// Map the decision's refusals onto the one bus shape.
///
/// A failed ACCEPT is deliberately absent: it is a recorded outcome the rail shows
/// on the proposal card, not an error.
fn decision_failure(error: &CoachDecisionError) -> BusError {
    let code = match error {
        // `FailureUnrecordable` is a STORE failure that also carries what the accept
        // was failing at, so it codes with the store errors. Its message is the only
        // account of that reason there is — nothing reached a row — which is why it
        // reaches the rail whole rather than being flattened to "data error".
        CoachDecisionError::SessionNotFound(_)
        | CoachDecisionError::NoProposal(_)
        | CoachDecisionError::ParentVersionMissing(_)
        | CoachDecisionError::FailureUnrecordable { .. }
        | CoachDecisionError::Data(_) => BusErrorCode::Data,
        CoachDecisionError::NotActionable { .. }
        | CoachDecisionError::InapplicableMutation { .. } => BusErrorCode::Validation,
    };
    BusError::new(code, error.to_string())
}

// ---------------------------------------------------------------------------
// coach_turn
// ---------------------------------------------------------------------------

/// Everything the coach turn needs that is NOT owned by managed state — the
/// per-call composition the `coach_turn` wrapper builds from config and the
/// resolved credential.
///
/// The provider arrives already constructed, so the credential that built it stays
/// in the wrapper. The redactor is built from the same key and travels here because
/// BOTH roads need the same scrubber: the ledger decorator scrubs the persisted
/// prompt and completion, and the turn scrubs the tool arguments that become stored
/// domain values. Passing a second one is how the two roads drift apart.
pub struct CoachTurnDeps<P> {
    /// The transport. Live: `OpenAiCompatProvider`. Offline: a scripted double.
    pub provider: P,
    /// The cost table the ledger decorator prices each call against.
    pub prices: PriceTable,
    /// The NFR-6 scrubber, built from the resolved key.
    pub redactor: Redactor,
    /// Which credential source supplied that key — a LABEL, never the key.
    pub key_source: Option<CredentialSource>,
    /// The per-request chat config.
    pub config: LlmConfig,
    /// The resolved coach prompt text.
    pub prompt: String,
    /// The version stamped on this turn's ledger row — the SHA-256 of the bytes in
    /// `prompt`, resolved together with them (audit C2).
    pub prompt_version: Option<String>,
    /// Override the per-turn wall-clock guard. `None` = the default.
    pub turn_timeout: Option<Duration>,
    /// Override the pre-call DSL size budget. `None` = the default.
    pub max_dsl_bytes: Option<usize>,
}

/// How old a `pending` claim must be before a reload may adopt it.
///
/// A turn cannot outlive its own wall-clock guard, so a claim older than that guard
/// plus a margin belongs to no call that is still running anywhere. The margin
/// covers the write and the clock skew between the claim and this read, not a
/// judgement about the other process.
const ABANDONED_AFTER: Duration = Duration::from_secs(DEFAULT_TURN_TIMEOUT.as_secs() + 30);

/// What this run's unfinished claim, if it has one, means for a reload.
enum PendingClaim {
    /// Old enough that no live call can still hold it: adopt its id, so W1 settles
    /// it as `interrupted`.
    Abandoned(CoachingSessionId),
    /// Too young to be treated as abandoned. It MAY be a call in flight in another
    /// process, or it may be a claim whose owner died seconds ago — this cannot tell
    /// them apart, and says so rather than guessing. Carries the id so the refusal
    /// can name the claim it is declining to touch.
    MaybeLive(CoachingSessionId),
}

/// Classify this run's unfinished claim.
///
/// A `pending` row is a session that was claimed and never settled. The LATEST is
/// taken if a history holds more than one.
///
/// **Pending alone does not mean abandoned, and adopting on that alone steals live
/// turns.** Both single-flight registries are process-local: a second process sees
/// an adopted id as free, and `run_coach_turn` then reads `ExistingPending` as stale
/// and settles it `interrupted` — killing a call the first process has already been
/// billed for, and which will fail when it tries to settle its own row.
///
/// AGE is the evidence available without a durable lease. A turn cannot outlive
/// [`DEFAULT_TURN_TIMEOUT`], so a claim older than that plus a margin is held by
/// nothing; a younger one is left alone.
///
/// **What this does NOT provide, stated because the code reads as if it might.**
/// This is a read, not a reservation. Two processes can both complete it before
/// either calls `claim_session`, and each then claims its own session id and makes
/// its own billable call — the operation latch is keyed by session id and no
/// constraint makes pending rows unique per run. So there is single-flight WITHIN a
/// process, and adoption of rows old enough to be nobody's, and no run-level
/// exclusivity ACROSS processes. The fix is a run-level claim in migration `0009`
/// (a partial unique index on pending rows per run, with `claim_session` mapping the
/// violation to `ExistingPending`), tracked as
/// `pulseai-labs/pulse-trader#158` and out of r1.s4's scope. Nothing here should be
/// read as promising more than the paragraph above.
async fn pending_claim_for_run<R: CoachingRepository>(
    sessions: &R,
    run_id: &BacktestRunId,
    now: &impl Clock,
) -> Result<Option<PendingClaim>, BusError> {
    let recorded = sessions.list_sessions_for_run(run_id).await?;
    let Some(pending) = recorded
        .into_iter()
        .rfind(|session| matches!(session.outcome, SessionOutcome::Pending))
    else {
        return Ok(None);
    };

    let claimed_at = DateTime::parse_from_rfc3339(&pending.created_at)
        .map_err(|e| {
            BusError::new(
                BusErrorCode::Data,
                format!(
                    "coaching session `{}` has an unreadable created_at: {e}",
                    pending.id.as_str()
                ),
            )
        })?
        .timestamp_millis();
    let age_ms = now.now_ms().saturating_sub(claimed_at);

    // Compared in i64 milliseconds, the units both sides already carry — no cast
    // that could truncate the window.
    //
    // A future-dated row is never adopted: `saturating_sub` floors it at zero,
    // which reads as young, which refuses. Refusing is the safe direction — it
    // costs the trader a retry, where adopting wrongly costs someone a live turn.
    let window_ms = i64::try_from(ABANDONED_AFTER.as_millis()).unwrap_or(i64::MAX);
    Ok(Some(if age_ms >= window_ms {
        PendingClaim::Abandoned(pending.id)
    } else {
        PendingClaim::MaybeLive(pending.id)
    }))
}

/// Start or reload one coach turn for a persisted run — the drivable core behind
/// the `coach_turn` command.
///
/// The whole call is held under the `#141` single-flight latch keyed on the session
/// id, released through an RAII guard on every exit path. A second invocation while
/// the key is held is refused with [`BusErrorCode::Busy`] and reaches neither the
/// provider nor the database.
///
/// # Errors
///
/// Returns a [`BusError`]: `Busy` for a live duplicate, `data` for an absent run or
/// an unwritable record, `llm` for a local fault on the call path, `internal` for a
/// turn that cannot name exactly the ledger row it produced. A TRANSPORT fault is
/// **not** an error — it is a recorded failed session, which is the point.
pub async fn coach_turn_core<P>(
    state: &DesktopState,
    deps: CoachTurnDeps<P>,
    request: CoachTurnRequestDto,
) -> Result<CoachSessionDto, BusError>
where
    P: LlmProvider + Send + Sync,
{
    let run_id = BacktestRunId::new(request.run_id);

    // ADOPT this run's unfinished claim, if it has one.
    //
    // W1 settles a stale `pending` row as `interrupted` — but only when a turn
    // arrives under that row's OWN session id, and nothing reached it: the desktop
    // mints a fresh id per ask, and after a restart the operation store that held
    // the previous one is empty. So a claim left by a killed process stayed pending
    // for ever, and the recovery W1 wrote for it was unreachable in production.
    //
    // One run has one rail, so a pending row for this run IS this rail's unfinished
    // business. Reloading under its id is what lets W1 tell a live claim from a
    // stale one and settle the stale one honestly — but ONLY once the row is old
    // enough that no live call can still hold it. See `pending_claim_for_run`: both
    // registries are process-local, so adopting on "pending" alone steals turns
    // running in another process.
    let reload_sessions = SqliteCoachingRepo::with_deps(state.db().pool().clone(), SystemClock);
    match pending_claim_for_run(&reload_sessions, &run_id, &SystemClock).await? {
        Some(PendingClaim::Abandoned(pending)) => {
            // SETTLE IT HERE, not by re-claiming under its id.
            //
            // Going through `run_coach_turn` would recompute the request fingerprint
            // from the CURRENT prompt, model config and tool definitions, and
            // `claim_session` refuses a reused id whose fingerprint has moved — so
            // any release that changed one of those left the row pending, every
            // later ask selected the same row and failed the same way, and coaching
            // for that run was blocked for good short of restoring the old config or
            // editing the database.
            //
            // The fingerprint answers "is this the same request?", which is the
            // wrong question about a claim already proven abandoned by age. What is
            // being recorded is that a turn ended without an answer; the settlement
            // is the same whatever the request was.
            let settled = reload_sessions
                .finish_session(
                    &pending,
                    InitialCoachOutcome {
                        llm_call_id: None,
                        outcome: SessionOutcome::Failed {
                            failure: CoachFailure::Interrupted {
                                detail: format!(
                                    "the claim on run `{}` was left unfinished by an earlier \
                                     process lifetime",
                                    run_id.as_str()
                                ),
                            },
                        },
                    },
                )
                .await?;
            return session_dto(&state.llm_call_repo(), &settled).await;
        }
        Some(PendingClaim::MaybeLive(pending)) => {
            // The message says only what is known: this run has an UNSETTLED turn,
            // too recent to treat as abandoned. Whether it is still running cannot
            // be determined from here — that would need the run-level claim #158
            // tracks — and a refusal that asserted "is already running" would be
            // promising exclusivity the code does not provide.
            //
            // The contested id crosses as a FIELD, not only in the prose: the rail's
            // "Check again" has to reload THAT session, and asking under a freshly
            // minted id instead would start a second billable turn — the opposite of
            // what a button offering the other turn's result promises.
            return Err(BusError::with_session_id(
                BusErrorCode::Busy,
                format!(
                    "coach turn {} for this run has not settled yet; check again to see \
                     where it got to",
                    pending.as_str()
                ),
                pending.as_str().to_owned(),
            ));
        }
        // No unfinished claim: this run's next turn is the caller's own.
        None => {}
    }
    let session_id = CoachingSessionId::new(request.session_id);

    // Acquired after the id is settled and dropped last: every exit below — the
    // `?`s, a panic, the future being dropped by a navigation — releases it,
    // because it releases in `Drop` rather than at a call site someone can forget.
    let _operation = state.begin_operation(OperationKey::Coach(session_id.clone()))?;

    let CoachTurnDeps {
        provider,
        prices,
        redactor,
        key_source,
        config,
        prompt,
        prompt_version,
        turn_timeout,
        max_dsl_bytes,
    } = deps;

    // ONE capture buffer per turn, handed to exactly one capturing repo and one
    // attributed provider — the composition-root obligation `#132` names, kept in
    // one place.
    let captured: crate::agent::LlmCallCapture = Arc::new(StdMutex::new(Vec::new()));
    let ledger = state.llm_call_repo();
    let capturing = CapturingRepo::new(
        state.llm_call_repo(),
        Arc::new(StdMutex::new(None)),
        Arc::clone(&captured),
    );
    let decorated =
        RedactingLoggingProvider::new(provider, capturing, SystemClock, redactor.clone(), prices)
            .with_created_by(CreatedBy::CoachLlm)
            .with_key_source(key_source)
            .with_prompt_version(prompt_version.clone());
    let attributed = AttributedProvider::new(decorated, captured);

    let source = SqliteCoachTurnSource::new(state.db().pool().clone());
    let sessions = SqliteCoachingRepo::with_deps(state.db().pool().clone(), SystemClock);
    let settings = CoachTurnSettings {
        prompt,
        prompt_version,
        config,
        redactor,
        turn_timeout: turn_timeout.unwrap_or(DEFAULT_TURN_TIMEOUT),
        max_dsl_bytes: max_dsl_bytes.unwrap_or(DEFAULT_MAX_DSL_BYTES),
    };

    let session = run_coach_turn(
        &source,
        &attributed,
        &sessions,
        state.coach_registry(),
        &SystemClock,
        &settings,
        CoachTurnRequest { session_id, run_id },
    )
    .await
    .map_err(|e| turn_failure(&e))?;

    session_dto(&ledger, &session).await
}

// ---------------------------------------------------------------------------
// coach_decide
// ---------------------------------------------------------------------------

/// Apply one decision to a recorded proposal — the drivable core behind the
/// `coach_decide` command.
///
/// Performs **no provider call**: an accept re-runs the parent run's exact
/// persisted inputs through the real engine, and asks the coach nothing.
///
/// # Errors
///
/// Returns a [`BusError`]: `Busy` for a decision already in flight for this
/// session, `data` for an absent session or store failure, `validation` for an
/// action the proposal's state cannot take or a modification that does not apply.
/// A failed ACCEPT is not an error — it comes back as a recorded `acceptFailure` on
/// the proposal.
pub async fn coach_decide_core(
    state: &DesktopState,
    request: CoachDecisionRequestDto,
) -> Result<CoachDecisionDto, BusError> {
    let session_id = CoachingSessionId::new(request.session_id);
    let _operation = state.begin_operation(OperationKey::Coach(session_id.clone()))?;

    let pool = state.db().pool().clone();
    let sessions = SqliteCoachingRepo::with_deps(pool.clone(), SystemClock);
    let ledger = state.llm_call_repo();

    let action = decode_action(&sessions, &session_id, request.action).await?;

    let outcome = run_coach_decision(
        &state.strategy_repo(),
        &state.candles(),
        &BinanceAdapter::new(),
        &state.backtest_run_repo(),
        &SqliteCoachAcceptanceRepo::new(pool.clone()),
        &sessions,
        CoachDecisionRequest {
            session_id: session_id.clone(),
            action,
        },
    )
    .await
    .map_err(|e| decision_failure(&e))?;

    // The DURABLE state, re-read: what the rail renders is what the database holds,
    // not what the outcome carried in memory a moment before.
    let session = sessions.get_session(&session_id).await?.ok_or_else(|| {
        BusError::new(
            BusErrorCode::Data,
            format!(
                "the coaching session `{}` vanished between the decision and its read back",
                session_id.as_str()
            ),
        )
    })?;

    let accepted = match &outcome {
        CoachDecisionOutcome::Accepted(result) => Some(accepted_dto(result)?),
        CoachDecisionOutcome::Modified(_)
        | CoachDecisionOutcome::Rejected(_)
        | CoachDecisionOutcome::AcceptFailed(_) => None,
    };

    Ok(CoachDecisionDto {
        session: session_dto(&ledger, &session).await?,
        accepted,
    })
}

/// Turn the wire action into the typed one.
///
/// The only interesting case is `modify`: `new_value` crosses as TEXT, and which
/// [`ParamValue`] it becomes is decided by the CURRENT proposal's own parameter
/// kind. Parsing the string and guessing would make `"21"` mean different things on
/// different days; the stored proposal is the only thing that knows which leaf this
/// is. A value that will not parse as that kind is a `validation` refusal naming the
/// kind expected — the correctable-rejection family, which is what it is.
async fn decode_action<Q: CoachingRepository>(
    sessions: &Q,
    session_id: &CoachingSessionId,
    action: CoachActionDto,
) -> Result<CoachAction, BusError> {
    match action {
        CoachActionDto::Reject => Ok(CoachAction::Reject),
        CoachActionDto::Accept => Ok(CoachAction::Accept),
        CoachActionDto::Modify { path, new_value } => {
            let session = sessions.get_session(session_id).await?.ok_or_else(|| {
                BusError::new(
                    BusErrorCode::Data,
                    format!("no such coaching session `{}`", session_id.as_str()),
                )
            })?;
            let SessionOutcome::Proposed { proposal } = &session.outcome else {
                return Err(BusError::new(
                    BusErrorCode::Data,
                    format!(
                        "coaching session `{}` produced no proposal to modify",
                        session_id.as_str()
                    ),
                ));
            };
            let Mutation::SetParam {
                path: proposal_path,
                new_value: current,
            } = &proposal.mutation;

            // A modify EDITS THIS PROPOSAL'S VALUE. It does not re-target the
            // mutation at another leaf, and the two halves have to agree about
            // which leaf that is: the value below is parsed against the kind of the
            // leaf the PROPOSAL names, so accepting a caller's different path would
            // parse the new value against the wrong leaf's type and store the result
            // under a path nothing validated it for.
            if &path != proposal_path {
                return Err(BusError::new(
                    BusErrorCode::Validation,
                    format!(
                        "this proposal changes `{proposal_path}`; a modify edits that value \
                         rather than re-targeting the mutation at `{path}`"
                    ),
                ));
            }

            let parsed = match current.kind() {
                ParamKind::Period => new_value
                    .trim()
                    .parse::<u32>()
                    .map(|value| ParamValue::Period { value })
                    .map_err(|e| {
                        BusError::new(
                            BusErrorCode::Validation,
                            format!("`{new_value}` is not a whole-number period: {e}"),
                        )
                    })?,
                ParamKind::Threshold => rust_decimal::Decimal::from_str_exact(new_value.trim())
                    .map(|value| ParamValue::Threshold { value })
                    .map_err(|e| {
                        BusError::new(
                            BusErrorCode::Validation,
                            format!("`{new_value}` is not a decimal threshold: {e}"),
                        )
                    })?,
            };
            // The PROPOSAL's path, now proven equal to the caller's — the one the
            // kind above was read from, so the path and the value cannot disagree.
            Ok(CoachAction::Modify(Mutation::SetParam {
                path: proposal_path.clone(),
                new_value: parsed,
            }))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        CoachFailure, ReadBackDto, ReadBackOk, dec, failure_kind, mutation_dto, recovery_for,
    };
    use crate::domain::{Mutation, ParamValue};
    use rust_decimal::Decimal;

    /// The three named recoveries are the spec's own words, and everything else is
    /// operational. Pinned so an edit to the rail's copy is a deliberate act.
    #[test]
    fn each_typed_failure_carries_its_named_recovery() {
        assert_eq!(
            recovery_for(&CoachFailure::InapplicableAdvice {
                intent: "i".to_owned(),
                evidence: "e".to_owned(),
            }),
            "the coach's advice was structural; ask again on another run or edit the strategy"
        );
        assert_eq!(
            recovery_for(&CoachFailure::MissingBacktestInputs {
                detail: "d".to_owned(),
            }),
            "run this version again, then ask the coach"
        );
        assert_eq!(
            recovery_for(&CoachFailure::Interrupted {
                detail: "d".to_owned(),
            }),
            "start a new coaching session"
        );
        assert_eq!(recovery_for(&CoachFailure::ZeroCalls), "retry");
        assert_eq!(
            recovery_for(&CoachFailure::ProviderTimeout { elapsed_ms: 1 }),
            "retry"
        );
    }

    /// The kind token is the one the domain's serde representation uses, so the
    /// rail's `kind` is the same word the audit trail holds.
    #[test]
    fn the_failure_kind_is_the_domains_own_tag() {
        let failure = CoachFailure::Interrupted {
            detail: "d".to_owned(),
        };
        let value = serde_json::to_value(&failure).unwrap();
        assert_eq!(value["type"], serde_json::json!(failure_kind(&failure)));
    }

    /// A period renders as an integer, a threshold as its exact decimal — never a
    /// float, and never a reformatted one.
    #[test]
    fn a_mutation_value_crosses_as_its_exact_text() {
        let period = Mutation::SetParam {
            path: "p".to_owned(),
            new_value: ParamValue::Period { value: 21 },
        };
        assert_eq!(mutation_dto(&period).new_value, "21");

        let threshold = Mutation::SetParam {
            path: "p".to_owned(),
            new_value: ParamValue::Threshold {
                value: Decimal::from_str_exact("0.030").unwrap(),
            },
        };
        assert_eq!(mutation_dto(&threshold).new_value, "0.03");
    }

    /// `readBack` crosses as `"ok"` or `{ failure }` — the shape the rail branches
    /// on, asserted on the wire rather than on the Rust type.
    #[test]
    fn read_back_crosses_as_ok_or_a_failure_object() {
        assert_eq!(
            serde_json::to_value(ReadBackDto::Ok(ReadBackOk::Ok)).unwrap(),
            serde_json::json!("ok")
        );
        assert_eq!(
            serde_json::to_value(ReadBackDto::Failed {
                failure: "nope".to_owned(),
            })
            .unwrap(),
            serde_json::json!({ "failure": "nope" })
        );
    }

    #[test]
    fn decimals_normalize_to_the_stored_form() {
        assert_eq!(dec(Decimal::from_str_exact("1.2300").unwrap()), "1.23");
    }
}
