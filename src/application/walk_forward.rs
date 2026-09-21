//! The walk-forward use case (r2.s3.w3 — `rolling-oos/v1` + `wf-v1`, ADR-0025).
//!
//! A strategy version is walked forward over K contiguous out-of-sample folds:
//!
//! 1. load the immutable strategy version, validate and compile it — the same
//!    guards [`run_version_backtest`] runs, shared verbatim;
//! 2. **one** `spawn_blocking` (a13, #201): load the pinned primary and HTF
//!    snapshots once, compute [`first_fully_warm_bar_ms`], resolve the counted
//!    span (`from` defaults to the first fully-warm bar, `to` to the last
//!    candle's `close_time`), refuse an empty fold BEFORE anything runs, then
//!    per fold slice a fresh copy of the series under full-history lead-in and
//!    run the ordinary engine — the extracted
//!    [`prepare_over_loaded_series`] does it byte-for-byte as
//!    `run_engine_offthread` does;
//! 3. back on the runtime: one transaction writes the `walk_forward_run`
//!    parent, the `walk_forward_fold` rows, and every fold's ordinary
//!    `backtest_run` + `trade` rows (a9);
//! 4. answer from the saved rows — the reloaded [`WalkForwardRun`] and the
//!    fold `RunSummary`s read back from the run log they joined (L8).
//!
//! [`run_version_backtest`]: crate::application::backtest::run_version_backtest
//! [`prepare_over_loaded_series`]: crate::application::backtest::prepare_over_loaded_series

use crate::adapters::backtest::{BacktestConfig, first_fully_warm_bar_ms};
use crate::application::backtest::{
    BacktestAppError, PreSaveStage, SnapshotPins, load_series, prepare_over_loaded_series,
};
use crate::domain::backtest::{
    CandleWindow, FoldScheme, FoldVerdict, K_DEFAULT, RunSummary, RunVerdict, VerdictRule,
    WalkForwardError, WalkForwardFoldDraft, WalkForwardRun, WalkForwardRunDraft, WalkForwardRunId,
    fold_windows,
};
use crate::domain::strategy::VersionId;
use crate::domain::{
    BacktestRunRepository, CandleSeries, CandleSeriesRepository, CompiledStrategy, DataError,
    ExchangeAdapter, Pair, PreparedBacktest, StrategyRepository, Timeframe, ValidatedDsl,
    WalkForwardRunRepository, compile, validate,
};

// ---------------------------------------------------------------------------
// Request / outcome
// ---------------------------------------------------------------------------

/// What a caller asks for: one persisted version, one pair, one primary
/// timeframe, an optional higher timeframe, the exact cost configuration, and
/// the `rolling-oos/v1` scheme parameters. `k` defaults to [`K_DEFAULT`];
/// `to_ms` defaults to the snapshot's last candle's `close_time`; `from_ms`
/// defaults to the first fully-warm bar (recorded `from_defaulted = true`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkForwardRequest {
    /// The immutable strategy version to walk forward.
    pub version_id: VersionId,
    /// The pair to load candles for.
    pub pair: Pair,
    /// The primary timeframe the engine steps over.
    pub primary_timeframe: Timeframe,
    /// An optional higher timeframe for MTF alignment.
    pub htf_timeframe: Option<Timeframe>,
    /// Starting equity and the cost model, exactly as the fold runs receive it.
    pub config: BacktestConfig,
    /// Exact `data_version`s to load instead of `HEAD`; `None` loads `HEAD`.
    pub snapshots: Option<SnapshotPins>,
    /// The counted span's lower bound; `None` defaults to the first fully-warm
    /// bar. An explicit value earlier than that refuses `FromBeforeWarm`.
    pub from_ms: Option<i64>,
    /// The counted span's exclusive upper bound; `None` defaults to the
    /// snapshot's last candle's `close_time`.
    pub to_ms: Option<i64>,
    /// The fold count; `None` defaults to [`K_DEFAULT`]. In `2..=12`.
    pub k: Option<u8>,
}

/// The use case's answer — built from the saved rows, never the in-memory
/// draft (the `run_version_backtest` read-back discipline).
#[derive(Debug, Clone, PartialEq)]
pub struct WalkForwardOutcome {
    /// The persisted walk-forward run, read back with its folds.
    pub run: WalkForwardRun,
    /// The fold runs' catalog rows, in `fold_index` order — proof the folds
    /// joined the ordinary run log (L8).
    pub fold_summaries: Vec<RunSummary>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything the walk-forward use case can refuse with.
///
/// The shared-path refusals (version read, DSL validation, snapshot load,
/// series gaps, engine errors) wrap [`BacktestAppError`] verbatim — a fold run
/// IS an ordinary windowed run, so its failure vocabulary is the same. The
/// walk-forward-specific refusals are field-pathed so MCP/Tauri callers can
/// point at the exact request member (a6).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WalkForwardAppError {
    /// A refusal on the shared backtest path (version, DSL, snapshots, engine).
    #[error("{0}")]
    Shared(#[from] BacktestAppError),

    /// A `rolling-oos/v1` argument refused (the domain taxonomy — `k` out of
    /// `2..=12` is [`WalkForwardError::KOutOfRange`]).
    #[error("{0}")]
    Domain(#[from] WalkForwardError),

    /// `from >= to` — the counted span is empty by construction.
    #[error(
        "walk-forward {field} is invalid: from_ms {from_ms} >= to_ms {to_ms} — \
         the counted span [from, to) is empty"
    )]
    InvalidRange {
        /// The request field at fault — always `"to"` (it must exceed `from`).
        field: &'static str,
        /// The resolved span lower bound.
        from_ms: i64,
        /// The resolved span upper bound.
        to_ms: i64,
    },

    /// An explicit `from_ms` earlier than the first fully-warm bar.
    #[error(
        "walk-forward {field} {from_ms} is earlier than the first fully-warm bar \
         {earliest_allowed_ms} — counting cannot start before the strategy's \
         indicators are warm"
    )]
    FromBeforeWarm {
        /// The request field at fault — always `"from"`.
        field: &'static str,
        /// The refused explicit bound.
        from_ms: i64,
        /// The earliest allowed `from_ms` (the first fully-warm bar's open).
        earliest_allowed_ms: i64,
    },

    /// The strategy's entry warm gate never holds anywhere on the snapshot —
    /// there is no bar a fold could count from.
    #[error("the strategy is never fully warm on this snapshot — no fold can count")]
    NeverWarm,

    /// A fold's counted window holds no primary candle — refused before any
    /// fold runs and before any row persists.
    #[error(
        "walk-forward fold {fold_index} has no candles in [{from_ms}, {to_ms}) — \
         the fold is empty"
    )]
    FoldEmpty {
        /// The offending fold's position in the scheme.
        fold_index: u8,
        /// The fold window's inclusive lower bound.
        from_ms: i64,
        /// The fold window's exclusive upper bound.
        to_ms: i64,
    },

    /// The fold runs disagree on the engine fingerprint — impossible for one
    /// compiled strategy over one snapshot pair unless the engine is
    /// nondeterministic, which is exactly what this assertion exists to catch.
    #[error("folds {first_index} and {other_index} produced different engine fingerprints")]
    FingerprintMismatch {
        /// The first fold's index (the reference fingerprint).
        first_index: u8,
        /// The disagreeing fold's index.
        other_index: u8,
    },

    /// The save itself failed — no row was committed (the transaction rolled
    /// back; nothing partial persists).
    #[error("persist walk-forward run: {0}")]
    Persist(DataError),

    /// The run **was saved** and then could not be read back — the `run_version_backtest`
    /// distinction: a committed row exists, so the error names it.
    #[error("walk-forward run `{}` was saved, but reading it back failed: {source}", run_id.as_str())]
    SavedButReadBackFailed {
        /// The id of the row that exists.
        run_id: WalkForwardRunId,
        /// Why the read-back failed.
        source: DataError,
    },

    /// The saved run read back `None` — a just-committed row vanished.
    #[error("walk-forward run `{}` was saved, but read back missing", .0.as_str())]
    SavedButReadBackMissing(WalkForwardRunId),

    /// A defect in this layer (a failed blocking-task join).
    #[error("internal: {0}")]
    Internal(String),
}

// ---------------------------------------------------------------------------
// The use case
// ---------------------------------------------------------------------------

/// What the blocking section produces.
struct WalkForwardBlockingOutput {
    /// The persistable parent + fold drafts.
    draft: WalkForwardRunDraft,
}

/// The counted span resolution (L3): `to` defaults to the snapshot's last
/// candle's `close_time`; `from` defaults to the first fully-warm bar — an
/// explicit `from` earlier than that refuses, field-pathed, and `from >= to`
/// refuses an empty-by-construction span. Returns the proven `from < to`
/// window plus whether `from` was defaulted.
fn resolve_counted_span(
    compiled: &CompiledStrategy,
    primary: &CandleSeries,
    htf: Option<&CandleSeries>,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
) -> Result<(CandleWindow, bool), WalkForwardAppError> {
    let first_warm =
        first_fully_warm_bar_ms(compiled, primary, htf).ok_or(WalkForwardAppError::NeverWarm)?;
    let resolved_to =
        to_ms.unwrap_or_else(|| primary.candles.last().map_or(first_warm, |c| c.close_time));
    let (resolved_from, from_defaulted) = match from_ms {
        Some(explicit) => {
            if explicit < first_warm {
                return Err(WalkForwardAppError::FromBeforeWarm {
                    field: "from",
                    from_ms: explicit,
                    earliest_allowed_ms: first_warm,
                });
            }
            (explicit, false)
        }
        None => (first_warm, true),
    };
    if resolved_from >= resolved_to {
        return Err(WalkForwardAppError::InvalidRange {
            field: "to",
            from_ms: resolved_from,
            to_ms: resolved_to,
        });
    }
    // `from < to` proven above — a struct literal, the same shape
    // `apply_lead_in_window` uses for its proven-bound window.
    Ok((
        CandleWindow {
            from_ms: resolved_from,
            to_ms: resolved_to,
        },
        from_defaulted,
    ))
}

/// The K fold executions inside the blocking task: each fold gets a FRESH copy
/// of the whole snapshot (the lead-in cut `prepare_over_loaded_series` makes is
/// destructive), runs the shared prepare path, and contributes its trades to
/// the pooled `wf-v1` bound. Asserts all folds share one engine fingerprint.
/// Returns the shared fingerprint, the fold drafts, and the assessed verdict.
#[allow(clippy::too_many_arguments)]
fn execute_folds<E>(
    validated: &ValidatedDsl,
    exchange: &E,
    pair: &Pair,
    primary_tf: Timeframe,
    config: &BacktestConfig,
    folds: &[CandleWindow],
    primary: &CandleSeries,
    htf: Option<&CandleSeries>,
) -> Result<(String, Vec<WalkForwardFoldDraft>, RunVerdict), WalkForwardAppError>
where
    E: ExchangeAdapter,
{
    let mut fold_drafts = Vec::with_capacity(folds.len());
    let mut fold_verdicts = Vec::with_capacity(folds.len());
    let mut pooled_rs = Vec::new();
    let mut fingerprint: Option<String> = None;
    for (i, w) in folds.iter().enumerate() {
        let fold_index = u8::try_from(i).unwrap_or(u8::MAX);
        let mut fold_primary = primary.clone();
        let mut fold_htf = htf.cloned();
        let prepared: PreparedBacktest = prepare_over_loaded_series(
            validated,
            exchange,
            pair,
            primary_tf,
            config,
            Some(w.clone()),
            &mut fold_primary,
            &mut fold_htf,
        )?;
        let run_fp = prepared.result.engine_fingerprint.as_str().to_owned();
        match &fingerprint {
            None => fingerprint = Some(run_fp),
            Some(first) if *first != run_fp => {
                return Err(WalkForwardAppError::FingerprintMismatch {
                    first_index: 0,
                    other_index: fold_index,
                });
            }
            _ => {}
        }
        let fold_rs: Vec<rust_decimal::Decimal> = prepared
            .result
            .trades
            .iter()
            .map(|t| t.realized_r)
            .collect();
        pooled_rs.extend_from_slice(&fold_rs);
        fold_verdicts.push(FoldVerdict::from_rs(&fold_rs));
        fold_drafts.push(WalkForwardFoldDraft {
            index: fold_index,
            window: w.clone(),
            verdict: fold_verdicts[i].clone(),
            inputs: prepared.inputs,
            result: prepared.result,
            summary: prepared.summary,
            starting_equity: prepared.starting_equity,
        });
    }
    Ok((
        fingerprint.unwrap_or_default(),
        fold_drafts,
        RunVerdict::assess(&fold_verdicts, &pooled_rs),
    ))
}

/// The one blocking task's whole body (a13, #201): load the pinned snapshots
/// once, resolve the counted span (refusing its boundary failures), cut the
/// folds and refuse an empty one BEFORE any fold runs, then run each fold
/// through the shared [`prepare_over_loaded_series`] and fold the `wf-v1`
/// verdict. No I/O beyond the snapshot loads; nothing persists here.
#[allow(clippy::too_many_arguments)]
fn run_walk_forward_blocking<C, E>(
    candles: &C,
    exchange: &E,
    pair: &Pair,
    primary_tf: Timeframe,
    htf_tf: Option<Timeframe>,
    pins: Option<&SnapshotPins>,
    config: &BacktestConfig,
    validated: &ValidatedDsl,
    compiled: &CompiledStrategy,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    scheme: FoldScheme,
) -> Result<WalkForwardBlockingOutput, WalkForwardAppError>
where
    C: CandleSeriesRepository,
    E: ExchangeAdapter,
{
    let primary = load_series(
        candles,
        pair,
        primary_tf,
        pins.map(|p| &p.primary),
        PreSaveStage::PrimarySnapshot,
    )?;
    let htf = match htf_tf {
        Some(tf) => Some(load_series(
            candles,
            pair,
            tf,
            pins.and_then(|p| p.htf.as_ref()),
            PreSaveStage::HtfSnapshot,
        )?),
        None => None,
    };

    // The counted span: `to` defaults to the snapshot's last candle's
    // close_time (L3); `from` defaults to the first fully-warm bar.
    let (span, from_defaulted) =
        resolve_counted_span(compiled, &primary, htf.as_ref(), from_ms, to_ms)?;
    let folds = fold_windows(&span, scheme.k());
    // Refuse an empty fold BEFORE any fold runs (a6): the counted slice is
    // `open_time ∈ [from, to)` on the whole primary series.
    for (i, w) in folds.iter().enumerate() {
        let counted = primary
            .candles
            .iter()
            .any(|c| c.open_time >= w.from_ms && c.open_time < w.to_ms);
        if !counted {
            return Err(WalkForwardAppError::FoldEmpty {
                fold_index: u8::try_from(i).unwrap_or(u8::MAX),
                from_ms: w.from_ms,
                to_ms: w.to_ms,
            });
        }
    }

    let (fingerprint, fold_drafts, verdict) = execute_folds(
        validated,
        exchange,
        pair,
        primary_tf,
        config,
        &folds,
        &primary,
        htf.as_ref(),
    )?;
    Ok(WalkForwardBlockingOutput {
        draft: WalkForwardRunDraft {
            scheme,
            rule: VerdictRule::WfV1,
            span,
            from_defaulted,
            engine_fingerprint: fingerprint,
            verdict,
            folds: fold_drafts,
        },
    })
}

/// Run one persisted strategy version's walk-forward and answer from the saved
/// rows.
///
/// # Errors
///
/// Returns a [`WalkForwardAppError`]. Anything after `save_walk_forward_run`
/// returns names the committed parent id; everything before it persists
/// nothing.
pub async fn run_walk_forward<S, C, E, R>(
    strategies: &S,
    candles: &C,
    exchange: &E,
    runs: &R,
    request: &WalkForwardRequest,
) -> Result<WalkForwardOutcome, WalkForwardAppError>
where
    S: StrategyRepository,
    C: CandleSeriesRepository + Clone + Send + 'static,
    E: ExchangeAdapter + Clone + Send + 'static,
    R: BacktestRunRepository + WalkForwardRunRepository,
{
    // The immutable version, validated and compiled through the existing path —
    // the same guards `run_version_backtest` runs before any candle I/O.
    let version = strategies
        .get_version(&request.version_id)
        .await
        .map_err(|source| BacktestAppError::PreSaveRead {
            stage: PreSaveStage::StrategyVersion,
            source,
        })?
        .ok_or_else(|| BacktestAppError::VersionNotFound(request.version_id.clone()))?;
    let validated = validate(&version.dsl).map_err(BacktestAppError::DslInvalid)?;
    let compiled =
        compile(&validated).map_err(|e| BacktestAppError::CompileFailed(e.to_string()))?;
    if compiled.needs_htf() && request.htf_timeframe.is_none() {
        return Err(BacktestAppError::HtfRequired {
            field: "inputs.htf",
        }
        .into());
    }
    if let Some(htf_tf) = request.htf_timeframe
        && htf_tf.duration_ms() <= request.primary_timeframe.duration_ms()
    {
        return Err(BacktestAppError::HtfNotHigher {
            field: "inputs.htf",
            primary: request.primary_timeframe,
            htf: htf_tf,
        }
        .into());
    }
    let scheme = FoldScheme::rolling_oos(request.k.unwrap_or(K_DEFAULT))?;

    // ONE blocking task for the snapshot loads, the warm-bar probe, the span
    // resolution, and all K fold runs (a13, #201).
    let pair = request.pair.clone();
    let primary_tf = request.primary_timeframe;
    let htf_tf = request.htf_timeframe;
    let config = request.config;
    let pins = request.snapshots.clone();
    let from_ms = request.from_ms;
    let to_ms = request.to_ms;
    let candles_owned = candles.clone();
    let exchange_owned = exchange.clone();
    let output = tokio::task::spawn_blocking(move || {
        run_walk_forward_blocking(
            &candles_owned,
            &exchange_owned,
            &pair,
            primary_tf,
            htf_tf,
            pins.as_ref(),
            &config,
            &validated,
            &compiled,
            from_ms,
            to_ms,
            scheme,
        )
    })
    .await
    .map_err(|e| {
        WalkForwardAppError::Internal(format!("the walk-forward worker thread failed: {e}"))
    })??;

    // One transaction: parent + folds + every fold's ordinary run rows (a9);
    // then answer from the saved rows (the `run_version_backtest` read-back
    // discipline).
    let wf_id = runs
        .save_walk_forward_run(&request.version_id, &output.draft)
        .await
        .map_err(WalkForwardAppError::Persist)?;
    read_back_outcome(runs, &request.version_id, wf_id).await
}

/// The post-save read-back: the parent reloads with its folds, and each fold's
/// `RunSummary` comes out of the ordinary run log — proving the folds are real
/// runs (L8), not a parallel shape.
async fn read_back_outcome<R>(
    runs: &R,
    version_id: &VersionId,
    wf_id: WalkForwardRunId,
) -> Result<WalkForwardOutcome, WalkForwardAppError>
where
    R: BacktestRunRepository + WalkForwardRunRepository,
{
    let run = runs
        .get_walk_forward_run(&wf_id)
        .await
        .map_err(|source| WalkForwardAppError::SavedButReadBackFailed {
            run_id: wf_id.clone(),
            source,
        })?
        .ok_or_else(|| WalkForwardAppError::SavedButReadBackMissing(wf_id.clone()))?;

    let catalog = runs
        .list_runs_for_version(version_id)
        .await
        .map_err(|source| WalkForwardAppError::SavedButReadBackFailed {
            run_id: wf_id.clone(),
            source,
        })?;
    let fold_summaries = run
        .folds
        .iter()
        .map(|fold| {
            catalog
                .iter()
                .find(|s| s.id == fold.backtest_run_id)
                .cloned()
                .ok_or_else(|| WalkForwardAppError::SavedButReadBackFailed {
                    run_id: wf_id.clone(),
                    source: DataError::Db(format!(
                        "fold {}'s backtest_run `{}` is not in the version's run log",
                        fold.index,
                        fold.backtest_run_id.as_str(),
                    )),
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(WalkForwardOutcome {
        run,
        fold_summaries,
    })
}
