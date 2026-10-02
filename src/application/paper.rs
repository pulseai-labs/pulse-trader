//! The promotion use case (r3.s4.w2, E2): walk-forward verdict in, paper
//! session row out.
//!
//! [`promote`] resolves every input the pure gate needs — the version, its
//! latest walk-forward run, each fold run's recorded inputs, this build's
//! fingerprint — calls [`decide_promotion`], derives the `fixture` flag from
//! the `fixture_snapshot` rows (a store fact the pure gate must not read),
//! and inserts the row through the [`PaperSessionRepository`] port. The
//! override's `at` instant is stamped here, from the injected clock, before
//! the gate runs.
//!
//! There is no route and no CLI verb: w4 wires the API surface. The gate's
//! typed refusals travel out as [`PaperPromotionError::Refused`].

use crate::domain::backtest::BacktestInputs;
use crate::domain::paper::gate::{
    PromotionDraft, PromotionOverride, PromotionRefused, decide_promotion,
};
use crate::domain::paper::session::{Graduation, NonEmptyLabel, NonEmptyReason, PaperSession};
use crate::domain::strategy::VersionId;
use crate::domain::{
    BacktestRunRepository, Clock, DataError, EngineFingerprint, Pair, PaperSessionRepository,
    StrategyRepository, Timeframe, WalkForwardRunRepository,
};

/// Why a promotion could not complete. The gate's refusals are typed through;
/// store failures and an unknown version are distinct arms.
#[derive(Debug, Clone, PartialEq)]
pub enum PaperPromotionError {
    /// The gate refused (uncertified, foreign engine, unreadable inputs).
    Refused(PromotionRefused),
    /// No such strategy version.
    UnknownVersion(VersionId),
    /// A store read or write failed.
    Data(DataError),
}

impl core::fmt::Display for PaperPromotionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Refused(refused) => write!(f, "{refused}"),
            Self::UnknownVersion(id) => write!(f, "no such strategy version: {}", id.as_str()),
            Self::Data(e) => write!(f, "paper promotion store failure: {e}"),
        }
    }
}

impl std::error::Error for PaperPromotionError {}

/// The override request an API caller supplies (spec §4.2): the non-empty
/// reason plus the session shape. `at` is stamped from the injected clock.
#[derive(Debug, Clone, PartialEq)]
pub struct OverrideRequest {
    /// Why the human overrode the gate.
    pub reason: NonEmptyReason,
    /// The pair the session trades.
    pub pair: Pair,
    /// The session's primary timeframe.
    pub primary_timeframe: Timeframe,
    /// The session's higher timeframe, when named.
    pub htf_timeframe: Option<Timeframe>,
    /// Whether the session consumes the fixed daily series.
    pub uses_d1: bool,
}

/// Promote a strategy version to a live paper session.
///
/// - **Certified path** — the version's latest walk-forward run passed: the
///   session's pair/timeframes come from the fold inputs (A10), its
///   certified data versions are the distinct `(timeframe, data_version)`
///   pairs in fold order, and `fixture` is true only when every one of them
///   has a `fixture_snapshot` row (A12).
/// - **Override path** — no passing run: promotes only with an
///   [`OverrideRequest`], whose `at` is the injected clock's instant.
///
/// # Errors
///
/// [`PaperPromotionError::Refused`] for the gate's typed refusals,
/// [`PaperPromotionError::UnknownVersion`] for an absent version, and
/// [`PaperPromotionError::Data`] for store failures.
#[allow(clippy::too_many_arguments)] // the five injected ports + the request trio — every argument is a named seam, the engine.rs precedent
pub async fn promote<S, W, R, P, C>(
    strategies: &S,
    walk_forwards: &W,
    runs: &R,
    paper: &P,
    clock: &C,
    version_id: &VersionId,
    promotion_override: Option<&OverrideRequest>,
    promoted_by: NonEmptyLabel,
) -> Result<PaperSession, PaperPromotionError>
where
    S: StrategyRepository,
    W: WalkForwardRunRepository,
    R: BacktestRunRepository,
    P: PaperSessionRepository,
    C: Clock,
{
    let version = strategies
        .get_version(version_id)
        .await
        .map_err(PaperPromotionError::Data)?
        .ok_or_else(|| PaperPromotionError::UnknownVersion(version_id.clone()))?;

    let certifying_run = match &version.latest_walk_forward_run_id {
        Some(run_id) => walk_forwards
            .get_walk_forward_run(run_id)
            .await
            .map_err(PaperPromotionError::Data)?,
        None => None,
    };

    // The fold runs' recorded inputs, in fold order — the certification's
    // data provenance. A fold run that cannot be read, or one whose `inputs`
    // are absent, is the gate's `CertificationUnreadable`, not a silent skip.
    let mut fold_inputs_owned: Vec<Option<BacktestInputs>> = Vec::new();
    if let Some(run) = &certifying_run {
        for fold in &run.folds {
            let persisted = runs
                .get_run(&fold.backtest_run_id)
                .await
                .map_err(PaperPromotionError::Data)?;
            fold_inputs_owned.push(persisted.and_then(|run| run.inputs));
        }
    }
    let fold_inputs: Vec<Option<&BacktestInputs>> = fold_inputs_owned
        .iter()
        .map(|input| input.as_ref())
        .collect();

    let override_input = match promotion_override {
        Some(request) => Some(PromotionOverride {
            reason: request.reason.clone(),
            at: clock_text(clock).map_err(PaperPromotionError::Data)?,
            pair: request.pair.clone(),
            primary_timeframe: request.primary_timeframe,
            htf_timeframe: request.htf_timeframe,
            uses_d1: request.uses_d1,
        }),
        None => None,
    };

    let draft: PromotionDraft = decide_promotion(
        &version,
        certifying_run.as_ref(),
        &fold_inputs,
        &EngineFingerprint::current(),
        override_input,
        promoted_by,
    )
    .map_err(PaperPromotionError::Refused)?;

    // The fixture flag is a store fact: every certified data version has a
    // `fixture_snapshot` row (A12). An override names no versions and is
    // never fixture-certified.
    let fixture = match &draft.graduation {
        Graduation::Certified { data_versions, .. } => paper
            .all_versions_are_fixtures(data_versions)
            .await
            .map_err(PaperPromotionError::Data)?,
        Graduation::Override { .. } => false,
    };

    paper
        .insert_session(&crate::domain::paper::session::PaperSessionDraft {
            strategy_version_id: version.id.clone(),
            pair: draft.pair.clone(),
            primary_timeframe: draft.primary_timeframe,
            htf_timeframe: draft.htf_timeframe,
            uses_d1: draft.uses_d1,
            starting_equity: draft.starting_equity,
            taker_fee_bps: draft.taker_fee_bps,
            slippage_bps: draft.slippage_bps,
            engine_fingerprint: draft.engine_fingerprint.clone(),
            graduation: draft.graduation.clone(),
            fixture,
            min_trades: draft.min_trades,
            promoted_by: draft.promoted_by.clone(),
        })
        .await
        .map_err(PaperPromotionError::Data)
}

/// The injected clock's instant, as RFC3339 UTC text.
fn clock_text<C: Clock>(clock: &C) -> Result<String, DataError> {
    let now_ms = clock.now_ms();
    let dt = chrono::DateTime::from_timestamp_millis(now_ms).ok_or_else(|| {
        DataError::Db(format!("clock.now_ms() {now_ms} is out of DateTime range"))
    })?;
    Ok(dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}
