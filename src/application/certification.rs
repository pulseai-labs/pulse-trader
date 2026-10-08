//! The certification step (r4.s1.w5; spec A2/A3, grill Q2/Q4/Q5).
//!
//! One call = one hypothesis. In order, refusing typed and by name and
//! **writing nothing** on a refusal (C4, Q2):
//!
//! 1. no open freeze → [`CertifyRefusal::NoOpenFreeze`];
//! 2. the version's **lineage root** created before the freeze opened →
//!    [`CertifyRefusal::PreFreezeLineage`];
//! 3. the freeze's H certifications already recorded →
//!    [`CertifyRefusal::HypothesisBudgetSpent`];
//! 4. resolve the pair and timeframes as the run tools do (an optional `pair`
//!    override, else inherited — the same [`resolve_default_request`] seam
//!    MCP's `run_backtest`/`run_walk_forward` use);
//! 5. walk forward under **`wf-v2`** on the search span, the freeze passed so
//!    the window is the guard's clamped default `[first warm bar,
//!    holdout_start)`. That walk-forward persists as an ordinary run;
//! 6. run **one** backtest over `[holdout_start, the snapshot's end)`. This is
//!    the holdout guard's only run-time exemption (grill Q4): the load below
//!    names its window explicitly and never consults a freeze, so no guard
//!    decision is skipped — the window IS the decision. The run is **never
//!    persisted**: no run read may ever surface holdout trades to an agent, so
//!    its numbers live only in the certification record;
//! 7. `holdout_test(holdout realized_r, freeze.h)` and ONE immutable record
//!    with the next `hypothesis_index`. A failed or uncertified attempt still
//!    counts and is still recorded.
//!
//! An error before step 7 (a missing snapshot, a gapped series, an engine
//! error) writes nothing and counts nothing — every [`CertifyError`] arm says
//! so.
//!
//! The step composes the existing use cases rather than re-implementing them:
//! [`run_walk_forward`] for the search span and the shared
//! [`prepare_over_loaded_series`] for the holdout, so a certification evaluates
//! exactly what the campaign's other tools would over the same window.
//!
//! **The freeze arrives as a value.** The edge (the MCP tool) reads
//! `open_freeze()` once and hands it in, mirroring w4's guard wiring — the step
//! decides refusal 1 against the record the caller read, never a second read of
//! its own.

use rust_decimal::Decimal;

use crate::adapters::backtest::BacktestConfig;
use crate::application::backtest::{
    BacktestAppError, BacktestRequest, PreSaveStage, SnapshotPins, load_series,
    prepare_over_loaded_series, resolve_default_request,
};
use crate::application::walk_forward::{WalkForwardAppError, WalkForwardRequest, run_walk_forward};
use crate::domain::backtest::{BacktestInputs, RunVerdict, VerdictRule, WalkForwardRun};
use crate::domain::certification::{
    CertificationDraft, CertificationInputs, CertificationRecord, CertifyRefusal,
};
use crate::domain::dsl::CompiledStrategy;
use crate::domain::strategy::{StrategyVersion, VersionId};
use crate::domain::{
    BacktestRunRepository, CandleSeriesRepository, CandleWindow, CertificationRepository,
    DataError, ExchangeAdapter, FreezeRecord, Pair, SnapshotSelection, StrategyRepository,
    Timeframe, ValidatedDsl, WalkForwardRunRepository, compile, holdout_test, validate,
};

/// The longest `parent_version_id` chain this walk will follow before calling
/// the lineage broken. The chain is `created_at`-bounded in practice (a handful
/// of hops); the bound exists so a cycle forged at the database cannot hang the
/// step, and it is far above any real lineage.
const LINEAGE_MAX_HOPS: usize = 10_000;

/// The rule every certification's search span runs under (spec A2: "walk
/// forward under wf-v2"). Recorded verbatim on the record, so the row says
/// which rule judged it rather than assuming today's.
const CERTIFY_RULE: VerdictRule = VerdictRule::WfV2;

/// The certification step's errors: a typed refusal, a missing version, or a
/// failure from one of the use cases it composes. Every arm means **nothing was
/// written and no hypothesis was spent** — the record write is the step's last
/// action.
///
/// It lives here, in the application ring (close R2): the two composed arms
/// carry [`WalkForwardAppError`] and [`BacktestAppError`], so a domain home
/// would make a pure domain type compile only together with the application
/// ring. The domain keeps the refusal vocabulary, [`CertifyRefusal`].
#[derive(Debug, thiserror::Error)]
pub enum CertifyError {
    /// One of the step's typed refusals (C4, Q2).
    #[error(transparent)]
    Refused(#[from] CertifyRefusal),
    /// No such version.
    #[error("no such strategy version `{}`", .0.as_str())]
    VersionNotFound(VersionId),
    /// A repository read or the record write failed.
    #[error(transparent)]
    Store(#[from] DataError),
    /// The search-span walk-forward failed (a missing snapshot, a gapped
    /// series, an engine error): nothing was written and nothing counts.
    #[error("the search-span walk-forward failed before any record was written: {0}")]
    WalkForward(#[from] WalkForwardAppError),
    /// The holdout backtest failed (a missing snapshot, a gapped series, an
    /// engine error): nothing was written and nothing counts.
    #[error("the holdout backtest failed before any record was written: {0}")]
    Backtest(#[from] BacktestAppError),
    /// A defect in this layer (a lineage cycle, a failed task join).
    #[error("internal: {0}")]
    Internal(String),
}

/// What the certification step is asked for: one persisted version, an
/// optional **already-validated** pair override, and the calling label.
///
/// The pair arrives as a [`Pair`], not a raw string: the MCP boundary validates
/// it against the run's own exchange adapter before the step (the `pair_override`
/// seam `run_backtest`/`run_walk_forward` use), so a malformed or unpinned
/// symbol is a `pair` field error there and never reaches — or burns — a
/// hypothesis.
///
/// `called_by` is the **calling token's label** from the authenticated request
/// context (grill Q5: the risk gate's audit trail). It is never taken from tool
/// arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertifyRequest {
    /// The immutable strategy version to certify.
    pub version_id: VersionId,
    /// The validated pair override; `None` inherits the version's lineage pair.
    pub pair: Option<Pair>,
    /// The calling client's label.
    pub called_by: String,
}

/// What one certification call answers: the record it wrote, the search span's
/// verdict (grill Q5 lets the agent see it), and the freeze's budget position
/// **after** it (`hypotheses_used` counts this call).
#[derive(Debug, Clone, PartialEq)]
pub struct CertifyOutcome {
    /// The immutable record this call wrote.
    pub record: CertificationRecord,
    /// The search-span `wf-v2` verdict — the folds that held, the folds
    /// required, and the pooled bound the record's `search_pass` summarises.
    pub search_verdict: RunVerdict,
    /// Hypotheses recorded under the freeze, this call included.
    pub hypotheses_used: u32,
    /// Hypotheses the freeze's budget leaves.
    pub hypotheses_left: u32,
}

/// Run one certification hypothesis and write its record.
///
/// # Errors
///
/// [`CertifyError::Refused`] for the three named refusals (nothing written,
/// nothing counted), [`CertifyError::VersionNotFound`] for an absent version,
/// and the composed use cases' errors — see [`CertifyError`].
pub async fn certify_version<S, C, E, R, A>(
    strategies: &S,
    candles: &C,
    exchange: &E,
    runs: &R,
    certifications: &A,
    freeze: Option<&FreezeRecord>,
    request: &CertifyRequest,
) -> Result<CertifyOutcome, CertifyError>
where
    S: StrategyRepository,
    C: CandleSeriesRepository + Clone + Send + 'static,
    E: ExchangeAdapter + Clone + Send + 'static,
    R: BacktestRunRepository + WalkForwardRunRepository,
    A: CertificationRepository,
{
    // 1. An open freeze, or nothing at all (C4): the holdout and the budget
    //    both come from it.
    let Some(freeze) = freeze else {
        return Err(CertifyRefusal::NoOpenFreeze.into());
    };

    // 2. The version and its lineage root. Both reads happen before the budget
    //    check because the root's creation instant is what C4 refuses on.
    let version = strategies
        .get_version(&request.version_id)
        .await
        .map_err(CertifyError::Store)?
        .ok_or_else(|| CertifyError::VersionNotFound(request.version_id.clone()))?;
    let root = lineage_root(strategies, &version).await?;
    let root_created_at_ms = root.created_at.timestamp_millis();
    if root_created_at_ms < freeze.opened_at_ms {
        return Err(CertifyRefusal::PreFreezeLineage {
            version_id: version.id.clone(),
            root_version_id: root.id.clone(),
            root_created_at_ms,
            freeze_opened_at_ms: freeze.opened_at_ms,
        }
        .into());
    }

    // 3. The budget: H hypotheses per freeze, counted from the records
    //    themselves (Q2). This is the cheap refusal — the write transaction
    //    re-derives the index and reads the freeze's own H under the write
    //    lock, and THAT is the enforcement (close R1): two calls that overlap
    //    here at `H - 1` cannot both write.
    let used_before = certifications
        .count_for_freeze(&freeze.id)
        .await
        .map_err(CertifyError::Store)?;
    if used_before >= u32::from(freeze.h) {
        return Err(CertifyRefusal::HypothesisBudgetSpent { h: freeze.h }.into());
    }

    // 4. The pair and timeframes, resolved exactly as the run tools resolve
    //    them: the version's parent's latest run, then its own, then the app
    //    defaults (BTCUSDT, M15 + H4, HEAD). The override then replaces the pair
    //    and clears the inherited pins — the mirror of the MCP tools'
    //    `apply_pair_override`, which lives in the MCP ring and cannot be
    //    imported here (the ring boundary).
    let mut resolved = resolve_default_request(strategies, runs, &request.version_id, None)
        .await
        .map_err(CertifyError::Backtest)?;
    apply_pair_override(
        &mut resolved.pair,
        &mut resolved.snapshots,
        request.pair.as_ref(),
    );

    // 5. The search span, under wf-v2, with the freeze passed so the guard
    //    clamps a defaulted `to` to the holdout start. Persisted as an ordinary
    //    walk-forward run — the record's `search_walk_forward_run_id`.
    let search = run_walk_forward(
        strategies,
        candles,
        exchange,
        runs,
        &WalkForwardRequest {
            version_id: request.version_id.clone(),
            pair: resolved.pair.clone(),
            primary_timeframe: resolved.primary_timeframe,
            htf_timeframe: resolved.htf_timeframe,
            config: resolved.config,
            snapshots: resolved.snapshots.clone(),
            // The guards own both bounds: `from` defaults to the first fully-warm
            // bar, `to` to the snapshot's end — clamped to the holdout start
            // while this freeze is open, which is what makes the search span the
            // search span instead of a caller-supplied window.
            from_ms: None,
            to_ms: None,
            k: None,
            rule: Some(CERTIFY_RULE),
        },
        Some(freeze.holdout()),
    )
    .await
    .map_err(CertifyError::WalkForward)?;

    // The search run's provenance: every fold consumed the same pins, so the
    // first fold's recorded inputs ARE the search side's data versions. A fold
    // run that cannot be read (or records no inputs) is a failure, never a
    // silently blank provenance.
    let search_inputs = search_inputs(runs, &search.run).await?;

    // 6. The ONE holdout backtest, unpersisted, outside the guard (Q4).
    let validated = validate(&version.dsl).map_err(BacktestAppError::DslInvalid)?;
    let compiled = compile(&validated)
        .map_err(|e| CertifyError::Backtest(BacktestAppError::CompileFailed(e.to_string())))?;
    let holdout_run = run_holdout(
        candles.clone(),
        exchange.clone(),
        validated,
        compiled,
        resolved.pair.clone(),
        resolved.primary_timeframe,
        resolved.htf_timeframe,
        resolved.config,
        search_inputs.clone(),
        freeze.holdout_start_ms,
        freeze.h,
    )
    .await?;

    // 7. The C1 test at the freeze's H, then the record — last, so every
    //    failure above left the budget untouched.
    let draft = certification_draft(
        &version,
        freeze,
        &resolved,
        &search.run,
        search_inputs,
        holdout_run,
        &request.called_by,
    );
    let record = match certifications.insert(&draft).await {
        Ok(record) => record,
        // The write transaction's own refusal (close R1): a call that took the
        // budget's last index between the pre-check above and this write. The
        // loser of that overlap wrote nothing, exactly like the loser of the
        // pre-check.
        Err(DataError::HypothesisBudgetSpent { h }) => {
            return Err(CertifyRefusal::HypothesisBudgetSpent { h }.into());
        }
        Err(e) => return Err(CertifyError::Store(e)),
    };

    // The record's index IS the budget count (the store minted it under the
    // write lock), so a concurrent call beside this one cannot leave the
    // answer stale. `used_before` above is the pre-check's read only.
    let hypotheses_used = record.hypothesis_index;
    Ok(CertifyOutcome {
        hypotheses_left: record.hypotheses_left(freeze.h),
        hypotheses_used,
        search_verdict: search.run.verdict.clone(),
        record,
    })
}

/// The search side's recorded data versions (spec A1): the FIRST fold's
/// `inputs`. Every fold consumed the same pins by construction, so the first
/// fold's row IS the search span's provenance. A run with no folds, or a fold
/// run that cannot be read or records no inputs, is a failure — never a
/// silently blank provenance.
async fn search_inputs<R>(
    runs: &R,
    search: &WalkForwardRun,
) -> Result<CertificationInputs, CertifyError>
where
    R: BacktestRunRepository,
{
    let Some(fold) = search.folds.first() else {
        return Err(CertifyError::Internal(
            "the search walk-forward persisted no folds".to_owned(),
        ));
    };
    let persisted = runs
        .get_run(&fold.backtest_run_id)
        .await
        .map_err(CertifyError::Store)?;
    let inputs = persisted.and_then(|run| run.inputs).ok_or_else(|| {
        CertifyError::Internal(format!(
            "the search run's fold `{}` records no inputs",
            fold.backtest_run_id.as_str()
        ))
    })?;
    Ok(certification_inputs(&inputs))
}

/// The record one hypothesis writes (spec A1): the search span's run and
/// verdict, the ONE holdout backtest's window and numbers, the data versions
/// each side evaluated, and the caller. `certified` is derived by the store
/// (and held by the schema).
fn certification_draft(
    version: &StrategyVersion,
    freeze: &FreezeRecord,
    resolved: &BacktestRequest,
    search: &WalkForwardRun,
    search_inputs: CertificationInputs,
    holdout: HoldoutNumbers,
    called_by: &str,
) -> CertificationDraft {
    CertificationDraft {
        version_id: version.id.clone(),
        freeze_id: freeze.id.clone(),
        rule: CERTIFY_RULE.name().to_owned(),
        pair: resolved.pair.clone(),
        search_walk_forward_run_id: search.id.clone(),
        search_pass: search.verdict.pass,
        holdout_start_ms: freeze.holdout_start_ms,
        holdout_end_ms: holdout.end_ms,
        holdout_n: holdout.realized_rs_len,
        holdout_mean_r: holdout.mean_r,
        holdout_z: holdout.verdict.z,
        holdout_lower_bound: holdout.verdict.lower_bound,
        holdout_passes: holdout.verdict.passes,
        search_inputs,
        holdout_inputs: holdout.inputs,
        engine_fingerprint: search.engine_fingerprint.clone(),
        called_by: called_by.to_owned(),
    }
}

/// The version's lineage root: walk `parent_version_id` to the end of the
/// chain. `LINEAGE_MAX_HOPS` bounds the walk so a forged cycle cannot hang the
/// step; a missing parent is a broken lineage, reported rather than treated as
/// a root (the root's instant is what C4 refuses on, and guessing it would
/// decide the refusal on invented evidence).
async fn lineage_root<S>(
    strategies: &S,
    version: &StrategyVersion,
) -> Result<StrategyVersion, CertifyError>
where
    S: StrategyRepository,
{
    let mut current = version.clone();
    let mut hops = 0_usize;
    while let Some(parent_id) = current.parent_version_id.clone() {
        hops += 1;
        if hops > LINEAGE_MAX_HOPS {
            return Err(CertifyError::Internal(format!(
                "lineage of `{}` exceeds {LINEAGE_MAX_HOPS} hops — `parent_version_id` is cyclic",
                version.id.as_str()
            )));
        }
        current = strategies
            .get_version(&parent_id)
            .await
            .map_err(CertifyError::Store)?
            .ok_or_else(|| {
                CertifyError::Internal(format!(
                    "version `{}` names missing parent `{}`",
                    current.id.as_str(),
                    parent_id.as_str()
                ))
            })?;
    }
    Ok(current)
}

/// Apply a validated `pair` override to a resolved request — the mirror of the
/// MCP tools' `apply_pair_override` (r4.s1.w2). A **differing** pair clears the
/// inherited snapshot pins: the pins name the other pair's exact
/// `data_version`s and the new pair must resolve its own `HEAD` snapshots. An
/// identical pair keeps them.
fn apply_pair_override(
    pair: &mut Pair,
    snapshots: &mut Option<SnapshotPins>,
    override_pair: Option<&Pair>,
) {
    if let Some(override_pair) = override_pair
        && override_pair != pair
    {
        *pair = override_pair.clone();
        *snapshots = None;
    }
}

/// One side's recorded `(timeframe, data_version)` selections, from the inputs
/// a run actually consumed (never from the request's pins, which may be `None`
/// when the run resolved `HEAD`).
fn certification_inputs(inputs: &BacktestInputs) -> CertificationInputs {
    let selection = |s: &SnapshotSelection| SnapshotSelection {
        timeframe: s.timeframe,
        data_version: s.data_version.clone(),
    };
    CertificationInputs {
        primary: selection(&inputs.primary),
        htf: inputs.htf.as_ref().map(selection),
        d1: inputs.d1.as_ref().map(selection),
    }
}

/// What the holdout backtest produced: the C1 verdict over its realized-R
/// series, the numbers the record carries, and the provenance it loaded.
struct HoldoutNumbers {
    verdict: crate::domain::backtest::HoldoutVerdict,
    realized_rs_len: usize,
    mean_r: Decimal,
    inputs: CertificationInputs,
    end_ms: i64,
}

/// Run the ONE holdout backtest over `[holdout_start, the snapshot's end)`.
///
/// Q4's exemption, stated where it is made: this function passes `holdout:
/// None` *by construction* — it loads the series with the search run's own
/// pins, cuts the window explicitly, and calls the shared prepare path
/// directly. No guard runs because no guard applies: the certification step is
/// the one caller allowed to evaluate the holdout, and this is that call.
///
/// The load and the engine run happen on a blocking thread, exactly as the
/// standalone backtest path does (filesystem I/O + `Parquet` decode + the CPU
/// engine).
#[allow(clippy::too_many_arguments)]
async fn run_holdout<C, E>(
    candles: C,
    exchange: E,
    validated: ValidatedDsl,
    compiled: CompiledStrategy,
    pair: Pair,
    primary_timeframe: Timeframe,
    htf_timeframe: Option<Timeframe>,
    config: BacktestConfig,
    search_inputs: CertificationInputs,
    holdout_start_ms: i64,
    h: u8,
) -> Result<HoldoutNumbers, CertifyError>
where
    C: CandleSeriesRepository + Send + 'static,
    E: ExchangeAdapter + Send + 'static,
{
    // The pins ARE the search side's recorded versions, so both halves of one
    // hypothesis read the same immutable snapshots even if `HEAD` moved between
    // them.
    let pins = SnapshotPins {
        primary: search_inputs.primary.data_version.clone(),
        htf: search_inputs.htf.as_ref().map(|s| s.data_version.clone()),
        d1: search_inputs.d1.as_ref().map(|s| s.data_version.clone()),
    };
    tokio::task::spawn_blocking(move || -> Result<HoldoutNumbers, CertifyError> {
        let mut primary = load_series(
            &candles,
            &pair,
            primary_timeframe,
            Some(&pins.primary),
            PreSaveStage::PrimarySnapshot,
        )
        .map_err(CertifyError::Backtest)?;
        let mut htf = match htf_timeframe {
            Some(tf) => Some(
                load_series(
                    &candles,
                    &pair,
                    tf,
                    pins.htf.as_ref(),
                    PreSaveStage::HtfSnapshot,
                )
                .map_err(CertifyError::Backtest)?,
            ),
            None => None,
        };
        let mut d1 = if compiled.needs_d1() {
            Some(
                load_series(
                    &candles,
                    &pair,
                    Timeframe::D1,
                    pins.d1.as_ref(),
                    PreSaveStage::D1Snapshot,
                )
                .map_err(CertifyError::Backtest)?,
            )
        } else {
            None
        };

        // The snapshot's real end: the primary series' last candle's close —
        // the same "snapshot end" convention the walk-forward's span resolution
        // uses, so the holdout stops exactly where the search span stops.
        let end_ms = primary
            .candles
            .last()
            .map_or(holdout_start_ms, |candle| candle.close_time);
        if end_ms <= holdout_start_ms {
            return Err(CertifyError::Backtest(BacktestAppError::WindowEmpty {
                pair: pair.clone(),
                timeframe: primary_timeframe,
                from_ms: holdout_start_ms,
                to_ms: end_ms,
            }));
        }
        let window = CandleWindow {
            from_ms: holdout_start_ms,
            to_ms: end_ms,
        };
        let prepared = prepare_over_loaded_series(
            &validated,
            &exchange,
            &pair,
            primary_timeframe,
            &config,
            Some(window),
            &mut primary,
            &mut htf,
            &mut d1,
        )
        .map_err(CertifyError::Backtest)?;

        // The C1 test sums the SAME realized-R series the rule's fold verdicts
        // do (`FoldVerdict::from_rs`), and splits the family-wise alpha across
        // the FREEZE's budget H — the budget the campaign actually runs under,
        // read from the open record rather than assumed (Q2/ADR-0028).
        let realized_rs: Vec<Decimal> = prepared
            .result
            .trades
            .iter()
            .map(|trade| trade.realized_r)
            .collect();
        let verdict = holdout_test(&realized_rs, h);
        Ok(HoldoutNumbers {
            realized_rs_len: realized_rs.len(),
            mean_r: verdict.mean_r,
            inputs: certification_inputs(&prepared.inputs),
            end_ms,
            verdict,
        })
    })
    .await
    .map_err(|e| CertifyError::Internal(format!("the holdout backtest thread failed: {e}")))?
}
