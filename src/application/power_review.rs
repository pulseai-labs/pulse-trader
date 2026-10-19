//! r5.s5.w1 (PDR-0040, ADR-0032) — the power review use case.
//!
//! One review answers, for one version and one pair under the open freeze: how many trades the candidate
//! makes in the freeze's holdout window — the count and nothing else about those trades (G1) — and the
//! chance ADR-0028's C1 test would pass a true `+0.25R` edge at that count, against the bar fixed at plan
//! time. It **spends no hypothesis** and writes one immutable `power_review` row; a refusal writes nothing.
//!
//! **The count comes from the same code certify runs (G2).** [`prepare_attempt`] and [`holdout_trades`] are
//! the certification step's own steps, extracted so both callers reach the holdout window through one path:
//! the freeze checks, the pair/timeframe resolution, the persisted `wf-v2` search walk-forward, the recorded
//! inputs, the compiled strategy and the holdout backtest are byte-for-byte the certify call's.
//!
//! **`holdout_test` is never called here.** The review reads `trades.len()`, the window's end and the pinned
//! inputs; the trades' R-multiples are never touched, never summarized and never stored.
//!
//! **Operator only (G6).** The CLI arm (`pulse certify power`) is this item's surface; w3 adds the app-scope
//! route. No MCP tool, description or bus command carries a power read or a holdout count.
//!
//! [`prepare_attempt`]: crate::application::certification::prepare_attempt
//! [`holdout_trades`]: crate::application::certification::holdout_trades

use rust_decimal::Decimal;

use crate::application::backtest::BacktestAppError;
use crate::application::certification::{
    CertifyError, PreparedAttempt, holdout_trades, prepare_attempt,
};
use crate::application::walk_forward::WalkForwardAppError;
use crate::domain::power_review::{
    ASSUMED_EDGE_R, MIN_HOLDOUT_TRADES, POWER_BAR, PowerReviewDraft, PowerReviewRecord,
    PowerVerdict, c1_z, pass_chance, sample_sigma, verdict,
};
use crate::domain::strategy::VersionId;
use crate::domain::{
    BacktestRunRepository, CandleSeriesRepository, CertificationRepository, DataError,
    ExchangeAdapter, FreezeRecord, Pair, PowerReviewRepository, StrategyRepository,
    WalkForwardRunRepository,
};

/// What a power review is asked for: one persisted version, an optional **already-validated** pair override,
/// and the calling label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerReviewRequest {
    /// The immutable strategy version to review.
    pub version_id: VersionId,
    /// The validated pair override; `None` inherits the version's lineage pair.
    pub pair: Option<Pair>,
    /// The calling client's label (`"cli"` from the CLI arm), the audit trail's name.
    pub called_by: String,
}

/// What one review answers: the immutable record it wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct PowerReviewOutcome {
    /// The record this read wrote — the only thing a review reports.
    pub record: PowerReviewRecord,
}

/// Everything a power review can refuse with — the same variants as [`CertifyError`], one to one, plus the
/// store failure of the record itself. Every arm means **no review row was written**; a searched span's
/// persisted walk-forward can remain, exactly as it would after a certification error.
#[derive(Debug, thiserror::Error)]
pub enum PowerReviewError {
    /// One of the certification step's typed refusals (no freeze, pre-freeze lineage, budget spent).
    #[error(transparent)]
    Refused(#[from] crate::domain::CertifyRefusal),
    /// No such version.
    #[error("no such strategy version `{}`", .0.as_str())]
    VersionNotFound(VersionId),
    /// A repository read or the record write failed.
    #[error(transparent)]
    Store(#[from] DataError),
    /// The search-span walk-forward failed.
    #[error("the search-span walk-forward failed before any record was written: {0}")]
    WalkForward(#[from] WalkForwardAppError),
    /// The holdout backtest failed (a missing snapshot, a gapped series, an empty window, an engine error).
    #[error("the holdout backtest failed before any record was written: {0}")]
    Backtest(#[from] BacktestAppError),
    /// A defect in this layer (a failed task join).
    #[error("internal: {0}")]
    Internal(String),
}

impl From<CertifyError> for PowerReviewError {
    fn from(error: CertifyError) -> Self {
        match error {
            CertifyError::Refused(refusal) => Self::Refused(refusal),
            CertifyError::VersionNotFound(id) => Self::VersionNotFound(id),
            CertifyError::Store(e) => Self::Store(e),
            CertifyError::WalkForward(e) => Self::WalkForward(e),
            CertifyError::Backtest(e) => Self::Backtest(e),
            CertifyError::Internal(message) => Self::Internal(message),
        }
    }
}

/// Run one power review and write its record.
///
/// # Errors
///
/// [`PowerReviewError::Refused`] for the three named refusals (nothing written), and the composed steps'
/// errors — see [`PowerReviewError`].
#[allow(clippy::too_many_arguments)]
pub async fn review_power<S, C, E, R, A, P>(
    strategies: &S,
    candles: &C,
    exchange: &E,
    runs: &R,
    certifications: &A,
    reviews: &P,
    freeze: Option<&FreezeRecord>,
    request: &PowerReviewRequest,
) -> Result<PowerReviewOutcome, PowerReviewError>
where
    S: StrategyRepository,
    C: CandleSeriesRepository + Clone + Send + 'static,
    E: ExchangeAdapter + Clone + Send + 'static,
    R: BacktestRunRepository + WalkForwardRunRepository,
    A: CertificationRepository,
    P: PowerReviewRepository,
{
    // Steps 1–6, the certification step's own (G2): a refusal here is that step's typed refusal, and nothing
    // is written.
    let prepared = prepare_attempt(
        strategies,
        candles,
        exchange,
        runs,
        certifications,
        freeze,
        &request.version_id,
        request.pair.as_ref(),
    )
    .await?;
    let PreparedAttempt {
        freeze,
        resolved,
        search,
        search_inputs,
        validated,
        compiled,
        ..
    } = prepared;

    // The holdout window through the SAME load the certify call runs. Only `trades.len()`, `end_ms` and
    // `inputs` are read below; the trades themselves are never inspected and never leave this function.
    let holdout = holdout_trades(
        candles.clone(),
        exchange.clone(),
        validated,
        compiled,
        resolved.pair.clone(),
        resolved.primary_timeframe,
        resolved.htf_timeframe,
        resolved.config,
        search_inputs,
        freeze.holdout_start_ms,
    )
    .await?;

    // σ and the search-span count: every out-of-sample trade of the search walk-forward's folds, pooled
    // (ADR-0032 — the candidate's own dispersion, not a campaign constant).
    let mut realized_rs: Vec<Decimal> = Vec::new();
    for fold in &search.run.folds {
        let trades = runs
            .get_trades(&fold.backtest_run_id)
            .await
            .map_err(PowerReviewError::Store)?;
        realized_rs.extend(trades.iter().map(|trade| trade.realized_r));
    }
    let search_n = realized_rs.len();
    let sigma = sample_sigma(&realized_rs);
    let sigma_r = sigma.map_or_else(|| "0.000000".to_owned(), |s| format!("{s:.6}"));

    let z = c1_z(freeze.h);
    let holdout_n = holdout.trades.len();
    let chance = sigma.map_or(0.0, |s| pass_chance(holdout_n, z, ASSUMED_EDGE_R, s));
    let PowerVerdict { allowed, reason } = verdict(search.run.verdict.pass, holdout_n, chance);

    let draft = PowerReviewDraft {
        version_id: request.version_id.clone(),
        freeze_id: freeze.id.clone(),
        pair: resolved.pair,
        h: freeze.h,
        z,
        holdout_start_ms: freeze.holdout_start_ms,
        holdout_end_ms: holdout.end_ms,
        holdout_n,
        holdout_inputs: holdout.inputs,
        search_walk_forward_run_id: search.run.id.clone(),
        search_pass: search.run.verdict.pass,
        search_n,
        sigma_r,
        assumed_edge_r: format!("{ASSUMED_EDGE_R:.2}"),
        bar: format!("{POWER_BAR:.2}"),
        min_holdout_trades: MIN_HOLDOUT_TRADES,
        pass_chance: chance,
        allowed,
        reason: reason.map(|block| block.as_str().to_owned()),
        // The same source `certification_draft` reads (A3): the search run's fingerprint.
        engine_fingerprint: search.run.engine_fingerprint.clone(),
        called_by: request.called_by.clone(),
    };
    let record = reviews
        .insert(&draft)
        .await
        .map_err(PowerReviewError::Store)?;
    Ok(PowerReviewOutcome { record })
}
