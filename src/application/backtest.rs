//! The version-id backtest use case (r1.s3.w3) — one flow, two adapters.
//!
//! The debug CLI's `--version` path and the desktop `run_backtest_version` command
//! run *this* sequence, not two copies of it:
//!
//! 1. load the immutable strategy version;
//! 2. validate its stored DSL;
//! 3. **off the async runtime** (`spawn_blocking`): load the primary and optional
//!    HTF `HEAD` snapshots, reject gapped series, resolve symbol filters, build
//!    [`BacktestInputs`] from the series about to be consumed, then compile and run
//!    the synchronous deterministic engine through [`prepare_backtest`];
//! 4. back on the runtime: read the prior run and compare fingerprints **before**
//!    saving — after the insert the fresh row would be its own prior and the
//!    warning could never fire;
//! 5. save through [`persist_backtest`], receiving a fresh [`BacktestRunId`];
//! 6. reload that run, its trades, and the primary/HTF snapshots **named by the
//!    persisted inputs** — never `HEAD`, which may have moved — with the snapshot
//!    loads off the async runtime (`spawn_blocking`), exactly like step 3: they
//!    are the same filesystem I/O + Parquet decode;
//! 7. answer from those reloaded values alone.
//!
//! **The prepare/persist split (r1.s4.w2).** Steps 3 and 5-7 are named functions —
//! [`prepare_backtest`] (compile + compute, deterministic, no I/O and no identity)
//! and [`persist_backtest`] (save + read back) — which `run_version_backtest` still
//! composes in the same order. The coach accept path re-runs the SAME
//! `prepare_backtest` on the parent run's persisted inputs and hands the result to
//! `commit_acceptance` instead, so the child's numbers come from one computation
//! rather than a second copy of it. Nothing under `domain::backtest` or
//! `adapters::backtest` moved.
//!
//! **Step 6 is the point of the standalone flow.** A response assembled from the
//! in-memory result would render identically today and would be a claim about
//! memory rather than about what is stored. Reading it back proves the row is
//! complete, decodable, and still resolves its snapshots — which is what makes the
//! number on the screen re-derivable tomorrow.
//!
//! **Failures before and after the save are different facts.** [`Persist`] means no
//! row exists. [`SavedButReadBackFailed`] means one does, and carries its id and the
//! stage that failed, so a caller can say "saved, but could not be read back"
//! instead of "the run failed" — which would be a lie that costs the user a run.
//!
//! [`Persist`]: BacktestAppError::Persist
//! [`SavedButReadBackFailed`]: BacktestAppError::SavedButReadBackFailed

use rust_decimal::Decimal;

use crate::domain::backtest::EquityCurve;
use crate::domain::strategy::{StrategyVersion, VersionId};
use crate::domain::{
    BacktestError, BacktestInputs, BacktestRunId, BacktestRunRepository, CandleSeries,
    CandleSeriesRepository, CandleWindow, DataError, DataVersion, EngineFingerprint,
    ExchangeAdapter, ExchangeError, FundingConfig, Pair, PersistedRun, PreparedBacktest, SeriesEnd,
    SnapshotSelection, StrategyRepository, SymbolFilters, Timeframe, Trade, ValidatedDsl,
    ValidationErrors, compile, validate,
};

use crate::adapters::backtest::{BacktestConfig, run_backtest};

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// The exact snapshots a run loads instead of `HEAD` (r2.s1.w3, a14): the
/// `data_version`s an earlier persisted run recorded, so a follow-up run is
/// comparable with the run it iterates on rather than whatever `HEAD` happens
/// to point at now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPins {
    /// The primary snapshot's `data_version`.
    pub primary: DataVersion,
    /// The HTF snapshot's `data_version`, when the source run used one.
    pub htf: Option<DataVersion>,
}

/// What a caller asks for: one persisted version, one pair, one primary timeframe,
/// an optional higher timeframe, and the exact cost configuration.
///
/// The desktop adapter builds the fixed r1 request (BTCUSDT, M15 + H4, default
/// costs); the CLI builds one from its flags. Neither can express a strategy the
/// database does not hold — the flow is version-id-only by construction, which is
/// what keeps every run attributable to an immutable `StrategyVersion` (ADR-0010).
///
/// r2.s1.w3 adds two optional refinements: `snapshots` pins the exact
/// `data_version`s to load instead of `HEAD` (the resolver's output), and
/// `window` slices both series to `[from_ms, to_ms)` on `open_time` at load
/// time — indicators warm up inside the window and the window is recorded on
/// the run's `inputs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacktestRequest {
    /// The immutable strategy version to run.
    pub version_id: VersionId,
    /// The pair to load candles for.
    pub pair: Pair,
    /// The primary timeframe the engine steps over.
    pub primary_timeframe: Timeframe,
    /// An optional higher timeframe for MTF alignment.
    pub htf_timeframe: Option<Timeframe>,
    /// Starting equity and the cost model, exactly as the engine will receive it.
    pub config: BacktestConfig,
    /// Exact `data_version`s to load instead of `HEAD`; `None` loads `HEAD`.
    pub snapshots: Option<SnapshotPins>,
    /// The half-open candle window `[from_ms, to_ms)` to slice both series to;
    /// `None` runs the whole snapshot.
    pub window: Option<CandleWindow>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Which read failed **before** anything was saved.
///
/// Every one of these happens with no row in the database, so none of them can name
/// a run. They are separated from [`BacktestAppError::Persist`] because that variant
/// says "persist backtest run", and saying it for a strategy-version read or a
/// snapshot load names an operation that never ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreSaveStage {
    /// Loading the immutable strategy version.
    StrategyVersion,
    /// Reading the prior run for the FR-7 fingerprint comparison.
    PriorRun,
    /// Loading or validating the primary `HEAD` snapshot.
    PrimarySnapshot,
    /// Loading or validating the HTF `HEAD` snapshot.
    HtfSnapshot,
}

impl PreSaveStage {
    /// A stable label naming the operation that actually failed.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PreSaveStage::StrategyVersion => "the strategy version",
            PreSaveStage::PriorRun => "the prior run",
            PreSaveStage::PrimarySnapshot => "the primary candle snapshot",
            PreSaveStage::HtfSnapshot => "the HTF candle snapshot",
        }
    }
}

/// Which read failed after the run was already saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadBackStage {
    /// Reading the saved run header.
    Run,
    /// Reading the saved trade log.
    Trades,
    /// Reloading the primary snapshot named by the persisted inputs.
    PrimarySnapshot,
    /// Reloading the HTF snapshot named by the persisted inputs.
    HtfSnapshot,
    /// Projecting the saved values onto the wire shape.
    Projection,
}

impl ReadBackStage {
    /// A stable lower-case label for messages and logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ReadBackStage::Run => "run",
            ReadBackStage::Trades => "trades",
            ReadBackStage::PrimarySnapshot => "primary snapshot",
            ReadBackStage::HtfSnapshot => "htf snapshot",
            ReadBackStage::Projection => "wire projection",
        }
    }
}

/// How a post-save read failed.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadBackFailure {
    /// The store reported an error.
    Data(DataError),
    /// The store reported success but the row/snapshot was absent.
    Missing,
    /// The saved run read back with `inputs: None`. Only a pre-migration-`0006` row
    /// may do that, and this run was written seconds ago — so the row is not the
    /// legacy shape it claims to be, and nothing downstream may trust it.
    FreshInputsMissing,
    /// A saved value could not be represented on the wire — a count or schema tag
    /// that does not fit the narrower wire type.
    ///
    /// The alternative was clamping, and clamping is how a corrupt-but-hash-consistent
    /// row renders a plausible false number instead of refusing. A run whose stored
    /// trade count does not fit is not a run this binary may report on.
    Projection(String),
}

impl std::fmt::Display for ReadBackFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadBackFailure::Data(source) => write!(f, "{source}"),
            ReadBackFailure::Missing => f.write_str("no such row"),
            ReadBackFailure::FreshInputsMissing => {
                f.write_str("a freshly saved run read back with no input provenance")
            }
            ReadBackFailure::Projection(reason) => write!(f, "{reason}"),
        }
    }
}

/// Everything the use case can refuse with.
///
/// The split that matters is [`Persist`](Self::Persist) versus
/// [`SavedButReadBackFailed`](Self::SavedButReadBackFailed): the first means no row
/// exists, the second means one does. Collapsing them into a generic failure is what
/// loses a user their run.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BacktestAppError {
    /// No such strategy version.
    #[error("no such strategy version `{}`", .0.as_str())]
    VersionNotFound(VersionId),

    /// The stored DSL failed semantic validation.
    #[error("stored strategy failed validation: {0}")]
    DslInvalid(#[from] ValidationErrors),

    /// The stored DSL validated but would not compile.
    #[error("compile stored strategy: {0}")]
    CompileFailed(String),

    /// No `HEAD` snapshot exists for a requested `(pair, timeframe)`.
    #[error("no HEAD snapshot for {pair} {} in the candle store", timeframe.binance_interval())]
    SnapshotMissing {
        /// The pair with no snapshot.
        pair: Pair,
        /// The timeframe with no snapshot.
        timeframe: Timeframe,
    },

    /// A loaded series has a spacing gap; the engine assumes contiguity.
    #[error(
        "candle series for {pair} {} has a spacing gap (expected open_time {expected}, found {found}); \
         the backtester requires a gap-free series — re-fetch the snapshot",
        timeframe.binance_interval()
    )]
    SeriesGapped {
        /// The gapped pair.
        pair: Pair,
        /// The gapped timeframe.
        timeframe: Timeframe,
        /// Where the next candle was expected.
        expected: i64,
        /// What was found instead.
        found: i64,
    },

    /// A windowed run's PRIMARY slice came up empty: no candle's `open_time`
    /// fell inside `[from_ms, to_ms)` (r2.s1.w3). Refused before the engine
    /// runs — an empty window is a bad argument, not a zero-trade run, and no
    /// row is written.
    #[error(
        "windowed backtest on {pair} {} has no candles in [{from_ms}, {to_ms}) — the window is empty",
        timeframe.binance_interval()
    )]
    WindowEmpty {
        /// The pair whose sliced primary series was empty.
        pair: Pair,
        /// The primary timeframe that was sliced.
        timeframe: Timeframe,
        /// The inclusive lower bound (epoch ms).
        from_ms: i64,
        /// The exclusive upper bound (epoch ms).
        to_ms: i64,
    },

    /// Symbol filters could not be resolved.
    #[error("resolve exchange filters: {0}")]
    ExchangeFilters(#[from] ExchangeError),

    /// The engine refused the run.
    #[error("backtest failed: {0}")]
    Engine(#[from] BacktestError),

    /// The strategy needs a higher-timeframe series the request did not supply
    /// (schema 1.1.0, r2.s2.w2). `field` names the missing input —
    /// `"inputs.htf"` — so MCP/Tauri callers can point at the exact request
    /// member instead of parsing the message.
    #[error("strategy requires a higher-timeframe series but {field} was not supplied")]
    HtfRequired {
        /// The request/input field that was missing — always `"inputs.htf"`.
        field: &'static str,
    },

    /// The request's `inputs.htf` selection is not strictly higher than the
    /// primary timeframe — compared by [`Timeframe::duration_ms`], so the rule
    /// holds for any future timeframe pair rather than hardcoding M15→H4.
    /// An equal or lower interval would advance `Series::Htf` operands on the
    /// wrong cadence while the DSL renders them as the HTF — silently wrong
    /// signals — so the request is refused before any candle I/O (r2.s2
    /// round-1 fix F1).
    #[error(
        "{field} must be a strictly higher timeframe than the primary {} — got {}",
        primary.binance_interval(),
        htf.binance_interval()
    )]
    HtfNotHigher {
        /// The request/input field at fault — always `"inputs.htf"`.
        field: &'static str,
        /// The request's primary timeframe.
        primary: Timeframe,
        /// The request's higher-timeframe selection.
        htf: Timeframe,
    },

    /// A READ failed before anything was saved.
    ///
    /// Distinct from [`Persist`](Self::Persist) because that one says "persist
    /// backtest run", and a strategy-version read or a snapshot load is not that
    /// operation. Neither carries a run id: no row exists in either case.
    #[error("read {} before the run was saved: {source}", stage.as_str())]
    PreSaveRead {
        /// Which read failed.
        stage: PreSaveStage,
        /// Why it failed.
        source: DataError,
    },

    /// The **save itself** failed, so no row was committed. There is no run id
    /// because there is no run.
    #[error("persist backtest run: {0}")]
    Persist(DataError),

    /// The run **was saved** and then could not be read back.
    #[error(
        "backtest run `{}` was saved, but reading back its {} failed: {failure}",
        run_id.as_str(),
        stage.as_str()
    )]
    SavedButReadBackFailed {
        /// The id of the row that exists.
        run_id: BacktestRunId,
        /// Which read failed.
        stage: ReadBackStage,
        /// How it failed.
        failure: ReadBackFailure,
    },

    /// A defect in this layer.
    #[error("internal: {0}")]
    Internal(String),
}

impl BacktestAppError {
    /// The id of a run that **is** persisted, when one is.
    ///
    /// `Some` only for [`SavedButReadBackFailed`](Self::SavedButReadBackFailed).
    /// Every other variant describes a state in which no row was committed, and
    /// reporting an id there would be worse than reporting none.
    #[must_use]
    pub fn persisted_run_id(&self) -> Option<&BacktestRunId> {
        match self {
            BacktestAppError::SavedButReadBackFailed { run_id, .. } => Some(run_id),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The pinned MFE/MAE histogram projection
// ---------------------------------------------------------------------------

/// The exact bin width, in R-multiples.
pub const HISTOGRAM_BIN_WIDTH_STR: &str = "0.25";

/// How many finite bins each histogram carries. 12 × `0.25R` covers `[0, 3)`.
pub const HISTOGRAM_BIN_COUNT: usize = 12;

/// The bin width as a `Decimal` (`0.25`). A function rather than a `const` because
/// `Decimal` has no const constructor for a scaled value.
#[must_use]
pub fn histogram_bin_width() -> Decimal {
    Decimal::new(25, 2)
}

/// One finite bin: `[lower, upper)` and how many normalized values fell in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistogramBin {
    /// Inclusive lower bound, in R.
    pub lower: Decimal,
    /// Exclusive upper bound, in R.
    pub upper: Decimal,
    /// How many values landed here.
    pub count: u32,
}

/// A deterministic excursion histogram.
///
/// **Pinned, not derived per run.** Bounds computed from each run's own data would
/// make two runs' charts incomparable, which is most of what a reader wants them
/// for. The domain comes from the strategy geometry the fixture was measured
/// against — a 1R stop and a 2R target put MAE near `-1R` and MFE near `+2R` — with
/// `[0, 3)` leaving a full R of headroom before anything overflows.
///
/// **`Decimal` throughout, and nothing is dropped.** Every value lands in exactly
/// one of the 12 bins, `underflow` or `overflow`, so the counts always sum to the
/// number of trades.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Histogram {
    /// The shared bin width (`0.25`).
    pub bin_width: Decimal,
    /// The 12 finite bins, ascending.
    pub bins: Vec<HistogramBin>,
    /// Normalized values below `0` — impossible for a well-formed run, and counted
    /// rather than hidden so an engine regression shows up as a number instead of a
    /// silently missing trade.
    pub underflow: u32,
    /// Normalized values at or above `3R`.
    pub overflow: u32,
}

/// Bin already-normalized R-multiples into the pinned histogram.
///
/// Callers normalize first: MFE passes `mfe_r` as-is, MAE passes `-mae_r`. That is
/// deliberately not `abs()` — a sign-violating value (a positive MAE, a negative
/// MFE) must stay negative so it lands in `underflow` and is visible, rather than
/// being folded into a plausible-looking bin.
#[must_use]
pub fn project_histogram(values: impl Iterator<Item = Decimal>) -> Histogram {
    let width = histogram_bin_width();
    let upper_bound = width * Decimal::from(u32::try_from(HISTOGRAM_BIN_COUNT).unwrap_or(u32::MAX));
    let mut bins: Vec<HistogramBin> = (0..HISTOGRAM_BIN_COUNT)
        .map(|i| {
            let index = Decimal::from(u32::try_from(i).unwrap_or(u32::MAX));
            HistogramBin {
                lower: width * index,
                upper: width * (index + Decimal::ONE),
                count: 0,
            }
        })
        .collect();
    let mut underflow = 0_u32;
    let mut overflow = 0_u32;

    for value in values {
        if value < Decimal::ZERO {
            underflow = underflow.saturating_add(1);
        } else if value >= upper_bound {
            overflow = overflow.saturating_add(1);
        } else {
            // Linear scan over 12 bins: the `[lo, hi)` rule is read straight off the
            // bounds, so an off-by-one in index arithmetic cannot silently reclassify
            // a boundary value.
            for bin in &mut bins {
                if value >= bin.lower && value < bin.upper {
                    bin.count = bin.count.saturating_add(1);
                    break;
                }
            }
        }
    }

    Histogram {
        bin_width: width,
        bins,
        underflow,
        overflow,
    }
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// The use case's answer — built from persisted values only, plus the one piece of
/// control metadata that has no persisted form.
#[derive(Debug, Clone, PartialEq)]
pub struct BacktestOutcome {
    /// The saved run, read back.
    pub run: PersistedRun,
    /// The saved run's input provenance.
    ///
    /// Non-optional here even though [`PersistedRun::inputs`] is an `Option`: that
    /// `Option` exists for pre-migration-`0006` rows, and this run was written
    /// seconds ago. The use case refuses the read-back when a fresh row comes back
    /// without inputs, so by the time an outcome exists the value is proven — and
    /// carrying it proven means no consumer needs an `unwrap` to reach it.
    pub inputs: BacktestInputs,
    /// The saved trades, read back in `seq` order.
    pub trades: Vec<Trade>,
    /// The primary snapshot, reloaded by the identity the run records.
    pub primary: CandleSeries,
    /// The HTF snapshot, reloaded the same way, when the run used one.
    pub htf: Option<CandleSeries>,
    /// The FR-7 fingerprint warning, if the prior run was built by another engine.
    ///
    /// The only field not read back from storage: the comparison must happen before
    /// the insert (afterwards the fresh row is its own prior), and it has no column.
    pub fingerprint_warning: Option<String>,
    /// The pinned MFE histogram over the persisted trades.
    pub mfe: Histogram,
    /// The pinned MAE histogram over the persisted trades.
    pub mae: Histogram,
}

impl BacktestOutcome {
    /// The equity curve, rebuilt from the reloaded snapshot's first candle, the
    /// persisted starting equity and the persisted trades — never a stored curve
    /// (there is no such table) and never the pre-save in-memory one.
    #[must_use]
    pub fn equity_curve(&self) -> EquityCurve {
        let start = self
            .primary
            .candles
            .first()
            .map_or(0, |candle| candle.open_time);
        EquityCurve::from_trades(start, self.run.starting_equity, &self.trades)
    }
}

// ---------------------------------------------------------------------------
// The use case
// ---------------------------------------------------------------------------

/// Why the deterministic prepare step declined (r1.s4.w2).
///
/// Two cases, because the accept path records them as two different
/// [`AcceptFailureStage`](crate::domain::AcceptFailureStage)s and a caller that
/// could not tell them apart would have to guess which one to store.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PrepareError {
    /// The validated document did not compile.
    Compile(String),
    /// The compiled strategy carries an `Htf` operand but no higher-timeframe
    /// series was supplied (schema 1.1.0, r2.s2.w2) — refused before any candle
    /// work rather than silently evaluating the operand against primary data.
    /// Surfaces as [`BacktestAppError::HtfRequired`] on the standalone path.
    HtfRequired,
    /// The engine refused the run.
    Engine(BacktestError),
}

/// **Step A of the split (r1.s4.w2): compile and compute, deterministically.**
///
/// No I/O, no identity, no timestamps — everything this returns is a pure function
/// of the arguments, which is what lets the standalone backtest path and the coach
/// accept path share ONE computation rather than keeping two copies of it in step.
///
/// The engine config is rebuilt from `inputs` (the fee/slippage the run is declared
/// to have used) plus `starting_equity`, so the numbers cannot silently be computed
/// against a different cost model than the one the persisted provenance names.
///
/// # Errors
///
/// Returns [`PrepareError::Compile`] when the validated document will not compile
/// and [`PrepareError::Engine`] when the engine refuses the run.
pub(crate) fn prepare_backtest(
    validated: &ValidatedDsl,
    inputs: BacktestInputs,
    primary: &CandleSeries,
    htf: Option<&CandleSeries>,
    filters: &SymbolFilters,
    starting_equity: Decimal,
    series_end: SeriesEnd,
) -> Result<PreparedBacktest, PrepareError> {
    let compiled = compile(validated).map_err(|e| PrepareError::Compile(e.to_string()))?;
    // The typed application-ring guard (r2.s2.w2 / ADR-0015): a strategy with an
    // `Htf` operand and no HTF series refuses here — after compile, before any
    // candle work — rather than reaching the engine, which would raise its own
    // `BacktestError::HtfRequired` as the last line of defense.
    if compiled.needs_htf() && htf.is_none() {
        return Err(PrepareError::HtfRequired);
    }
    let config = BacktestConfig {
        starting_equity,
        taker_fee_bps: inputs.taker_fee_bps,
        slippage_bps: inputs.slippage_bps,
    };
    let result = run_backtest(&compiled, primary, htf, &config, filters, series_end)
        .map_err(PrepareError::Engine)?;
    let summary = result.summary.clone();
    Ok(PreparedBacktest {
        inputs,
        result,
        summary,
        starting_equity,
    })
}

/// **Step B of the split (r1.s4.w2): persist, then read back.**
///
/// The standalone path's half. The accept path deliberately does NOT call it: its
/// write is `commit_acceptance`, which puts the child version, the run, the trades
/// and the proposal's links in one transaction — a guarantee this function cannot
/// make, because a run saved here has no child to belong to.
///
/// # Errors
///
/// Returns [`BacktestAppError::Persist`] when the save itself fails (no row
/// exists) and [`BacktestAppError::SavedButReadBackFailed`] for anything after it
/// (a row does).
pub(crate) async fn persist_backtest<C, R>(
    candles: C,
    runs: &R,
    version_id: &VersionId,
    prepared: &PreparedBacktest,
    fingerprint_warning: Option<String>,
) -> Result<BacktestOutcome, BacktestAppError>
where
    C: CandleSeriesRepository + Clone + Send + 'static,
    R: BacktestRunRepository,
{
    let run_id = runs
        .save_run(
            version_id,
            &prepared.inputs,
            &prepared.result,
            &prepared.summary,
            prepared.starting_equity,
        )
        .await
        .map_err(BacktestAppError::Persist)?;

    read_back(candles, runs, run_id, fingerprint_warning).await
}

/// What the blocking section produces.
struct EngineOutput {
    prepared: PreparedBacktest,
}

/// Run one persisted strategy version and answer from the saved row.
///
/// # Errors
///
/// Returns a [`BacktestAppError`]. Anything after `save_run` returns is
/// [`SavedButReadBackFailed`](BacktestAppError::SavedButReadBackFailed) and carries
/// the persisted run id.
pub async fn run_version_backtest<S, C, E, R>(
    strategies: &S,
    candles: &C,
    exchange: &E,
    runs: &R,
    request: &BacktestRequest,
) -> Result<BacktestOutcome, BacktestAppError>
where
    S: StrategyRepository,
    C: CandleSeriesRepository + Clone + Send + 'static,
    E: ExchangeAdapter + Clone + Send + 'static,
    R: BacktestRunRepository,
{
    // 1-2. The immutable version, validated and compiled through the existing path.
    let version = strategies
        .get_version(&request.version_id)
        .await
        .map_err(|source| BacktestAppError::PreSaveRead {
            stage: PreSaveStage::StrategyVersion,
            source,
        })?
        .ok_or_else(|| BacktestAppError::VersionNotFound(request.version_id.clone()))?;
    let validated = validate(&version.dsl)?;

    // r2.s2 round-1 fixes F1/F6: the request-level guards run BEFORE any
    // candle I/O. `compile` is pure, so an `htf`-operand strategy missing
    // `inputs.htf` — or an `inputs.htf` selection that is not strictly higher
    // than the primary timeframe — is refused here rather than surfacing as a
    // `PreSaveRead`/`SnapshotMissing` after loading (and possibly failing on)
    // candle data the run can never use. `prepare_backtest` keeps the same
    // `needs_htf` guard and `run_backtest` the same pair check as the last
    // line of defence for callers that skip this ring.
    let compiled =
        compile(&validated).map_err(|e| BacktestAppError::CompileFailed(e.to_string()))?;
    if compiled.needs_htf() && request.htf_timeframe.is_none() {
        return Err(BacktestAppError::HtfRequired {
            field: "inputs.htf",
        });
    }
    if let Some(htf_tf) = request.htf_timeframe
        && htf_tf.duration_ms() <= request.primary_timeframe.duration_ms()
    {
        return Err(BacktestAppError::HtfNotHigher {
            field: "inputs.htf",
            primary: request.primary_timeframe,
            htf: htf_tf,
        });
    }

    // 3. Everything synchronous — Parquet decode and the CPU engine — happens on a
    //    blocking thread. Both are hundreds of milliseconds on the real fixture, and
    //    holding a Tokio worker for that stalls every other command on the bus. The
    //    closure owns clones; nothing is borrowed across the await.
    //
    //    r1.s4.w2: the compile + compute half of that closure is now
    //    `prepare_backtest`, the SHARED deterministic step the coach accept path
    //    also calls. Only its address moved; the sequence inside is unchanged.
    let engine =
        run_engine_offthread(candles.clone(), exchange.clone(), validated, request).await?;

    // 4. FR-7 compare BEFORE the insert (D3): afterwards the fresh row is its own
    //    prior and the warning can never fire.
    let prior = runs
        .latest_run_for_version(&request.version_id)
        .await
        .map_err(|source| BacktestAppError::PreSaveRead {
            stage: PreSaveStage::PriorRun,
            source,
        })?;
    let fingerprint_warning = prior.and_then(|prior| {
        let prior_fp = EngineFingerprint::from_stored(prior.engine_fingerprint);
        engine.prepared.result.engine_fingerprint.compare(&prior_fp)
    });

    // 5-8. Save the prepared run and answer from the saved row. From inside
    //      `persist_backtest`, every failure after `save_run` returns names the row
    //      that exists.
    persist_backtest(
        candles.clone(),
        runs,
        &request.version_id,
        &engine.prepared,
        fingerprint_warning,
    )
    .await
}

/// Steps 7-8: reload the saved run, its trades and its exact snapshots.
async fn read_back<C, R>(
    candles: C,
    runs: &R,
    run_id: BacktestRunId,
    fingerprint_warning: Option<String>,
) -> Result<BacktestOutcome, BacktestAppError>
where
    C: CandleSeriesRepository + Clone + Send + 'static,
    R: BacktestRunRepository,
{
    let saved =
        |stage: ReadBackStage, failure: ReadBackFailure| BacktestAppError::SavedButReadBackFailed {
            run_id: run_id.clone(),
            stage,
            failure,
        };

    let run = runs
        .get_run(&run_id)
        .await
        .map_err(|e| saved(ReadBackStage::Run, ReadBackFailure::Data(e)))?
        .ok_or_else(|| saved(ReadBackStage::Run, ReadBackFailure::Missing))?;
    let inputs = run
        .inputs
        .clone()
        .ok_or_else(|| saved(ReadBackStage::Run, ReadBackFailure::FreshInputsMissing))?;

    let trades = runs
        .get_trades(&run_id)
        .await
        .map_err(|e| saved(ReadBackStage::Trades, ReadBackFailure::Data(e)))?;

    // The identities come from the SAVED row, never from HEAD — HEAD may already
    // point somewhere else, which is the whole reason #110 exists. Both loads go
    // through the blocking pool (see `load_version_offthread`): same filesystem
    // I/O + Parquet decode as step 3, so the same off-runtime rule.
    let mut primary = load_version_offthread(
        candles.clone(),
        inputs.pair.clone(),
        inputs.primary.timeframe,
        inputs.primary.data_version.clone(),
    )
    .await
    .map_err(|e| saved(ReadBackStage::PrimarySnapshot, ReadBackFailure::Data(e)))?;
    // ruling 1 binds the read-back too: the engine consumed the WINDOWED slice
    // of the snapshot the persisted inputs name, so the outcome answers from
    // that same slice — `equity_curve()` opens at the window's first candle
    // and no consumer sees candles the run did not. Slicing BEFORE the empty
    // check keeps the refusal meaningful for a windowed run: a snapshot that
    // no longer covers the window the run recorded is missing the run's data
    // the same way an empty one is.
    if let Some(w) = &inputs.window {
        primary = primary.windowed(w);
    }
    if primary.candles.is_empty() {
        // An empty reload cannot produce a truthful date range, and fabricating one
        // is exactly what the provenance header exists to prevent.
        return Err(saved(
            ReadBackStage::PrimarySnapshot,
            ReadBackFailure::Missing,
        ));
    }

    let mut htf = match inputs.htf.as_ref() {
        Some(selection) => Some(
            load_version_offthread(
                candles,
                inputs.pair.clone(),
                selection.timeframe,
                selection.data_version.clone(),
            )
            .await
            .map_err(|e| saved(ReadBackStage::HtfSnapshot, ReadBackFailure::Data(e)))?,
        ),
        None => None,
    };
    if let Some(w) = &inputs.window {
        htf = htf.map(|series| series.windowed(w));
    }

    let mfe = project_histogram(trades.iter().map(|t| t.mfe_r));
    // MAE is negated, not `abs()`d: a positive MAE would be a sign violation and
    // must surface in `underflow` rather than be folded into a plausible bin.
    let mae = project_histogram(trades.iter().map(|t| -t.mae_r));

    Ok(BacktestOutcome {
        run,
        inputs,
        trades,
        primary,
        htf,
        fingerprint_warning,
        mfe,
        mae,
    })
}

/// One read-back `load_version`, off the async runtime. Step 3's rule applies to
/// step 7 unchanged: the load is filesystem I/O plus Parquet decode — hundreds of
/// milliseconds on a real multi-year snapshot — and running it on a Tokio worker
/// stalls every other command on the bus. The closure owns the repo clone and the
/// identity; nothing is borrowed across the await.
async fn load_version_offthread<C>(
    candles: C,
    pair: Pair,
    timeframe: Timeframe,
    version: DataVersion,
) -> Result<CandleSeries, DataError>
where
    C: CandleSeriesRepository + Send + 'static,
{
    // Rendered before the move so the JoinError formatter can name the load.
    let label = format!("{pair}/{}/{version}", timeframe.binance_interval());
    tokio::task::spawn_blocking(move || {
        candles
            .load_version(&pair, timeframe, &version)
            .map(|stored| stored.series)
    })
    .await
    .unwrap_or_else(|join_err| {
        Err(DataError::Io(format!(
            "blocking snapshot load for {label} panicked: {join_err}"
        )))
    })
}

/// Step 3, off the async runtime.
async fn run_engine_offthread<C, E>(
    candles: C,
    exchange: E,
    validated: ValidatedDsl,
    request: &BacktestRequest,
) -> Result<EngineOutput, BacktestAppError>
where
    C: CandleSeriesRepository + Send + 'static,
    E: ExchangeAdapter + Send + 'static,
{
    let pair = request.pair.clone();
    let primary_tf = request.primary_timeframe;
    let htf_tf = request.htf_timeframe;
    let config = request.config;
    let pins = request.snapshots.clone();
    let window = request.window.clone();

    let joined = tokio::task::spawn_blocking(move || -> Result<EngineOutput, BacktestAppError> {
        let mut primary = load_series(
            &candles,
            &pair,
            primary_tf,
            pins.as_ref().map(|p| &p.primary),
            PreSaveStage::PrimarySnapshot,
        )?;
        let mut htf = match htf_tf {
            Some(tf) => Some(load_series(
                &candles,
                &pair,
                tf,
                pins.as_ref().and_then(|p| p.htf.as_ref()),
                PreSaveStage::HtfSnapshot,
            )?),
            None => None,
        };
        // ruling 1 (r2.s1.w3): slice BOTH series to `[from_ms, to_ms)` AFTER the
        // whole-snapshot gap check. The engine only ever sees the window's
        // candles, so indicators warm up inside the window and nothing forces a
        // flat at `to`. An empty PRIMARY slice is a refusal — an empty HTF
        // slice is legal (every aligned HTF bar is simply `None`).
        let mut series_end = SeriesEnd::SnapshotEnd;
        if let Some(w) = &window {
            // The snapshot's real last candle, remembered BEFORE the slice: a
            // window whose `to` still covers it ends at genuine end-of-data, so
            // the engine's force-close stays correct (and an unwindowed run over
            // the same extent agrees). A `to` cutting earlier makes the last bar
            // a window edge — no flat is forced there (r2.s1 G1).
            let snapshot_last_open = primary.candles.last().map(|c| c.open_time);
            primary = primary.windowed(w);
            if primary.candles.is_empty() {
                return Err(BacktestAppError::WindowEmpty {
                    pair: pair.clone(),
                    timeframe: primary_tf,
                    from_ms: w.from_ms,
                    to_ms: w.to_ms,
                });
            }
            if snapshot_last_open
                .is_some_and(|last| primary.candles.last().is_some_and(|c| c.open_time < last))
            {
                series_end = SeriesEnd::WindowEdge;
            }
            htf = htf.map(|series| series.windowed(w));
        }
        let filters: SymbolFilters = exchange.symbol_filters(&pair)?;
        // Provenance from the series the engine is ABOUT to consume, so the
        // prepared run and the row it becomes name the same snapshots.
        let inputs = inputs_from_run(&primary, htf.as_ref(), &config, window);
        let prepared = prepare_backtest(
            &validated,
            inputs,
            &primary,
            htf.as_ref(),
            &filters,
            config.starting_equity,
            series_end,
        )
        .map_err(|e| match e {
            PrepareError::Compile(reason) => BacktestAppError::CompileFailed(reason),
            PrepareError::HtfRequired => BacktestAppError::HtfRequired {
                field: "inputs.htf",
            },
            PrepareError::Engine(source) => BacktestAppError::Engine(source),
        })?;
        Ok(EngineOutput { prepared })
    })
    .await;

    match joined {
        Ok(inner) => inner,
        Err(e) => Err(BacktestAppError::Internal(format!(
            "the backtest worker thread failed: {e}"
        ))),
    }
}

/// Load one snapshot — the pinned `data_version` when `pin` names one, else
/// `HEAD` — and refuse it if the engine cannot interpret it.
///
/// The pinned branch is the [`coach_decision`](crate::application::coach_decision)
/// `load_named_snapshot` shape: a run that iterates on a prior run replays the
/// exact snapshots that run recorded, never whatever `HEAD` points at now. Gap
/// validation stays on the WHOLE snapshot either way (unchanged; `r2.s3` owns
/// fold-aware handling) — the window slice happens after this function returns.
fn load_series<C>(
    candles: &C,
    pair: &Pair,
    timeframe: Timeframe,
    pin: Option<&DataVersion>,
    stage: PreSaveStage,
) -> Result<CandleSeries, BacktestAppError>
where
    C: CandleSeriesRepository,
{
    let series = match pin {
        Some(version) => {
            candles
                .load_version(pair, timeframe, version)
                .map_err(|source| BacktestAppError::PreSaveRead { stage, source })?
                .series
        }
        None => {
            candles
                .load_head(pair, timeframe)
                .map_err(|source| BacktestAppError::PreSaveRead { stage, source })?
                .ok_or_else(|| BacktestAppError::SnapshotMissing {
                    pair: pair.clone(),
                    timeframe,
                })?
                .series
        }
    };
    // Structural corruption and spacing gaps are both refusals: the engine and the
    // indicator stream assume a contiguous series and neither detects nor fills a
    // hole, so a gapped snapshot would skew signals, holding periods and funding
    // silently rather than loudly.
    let gaps = series
        .validate()
        .map_err(|source| BacktestAppError::PreSaveRead { stage, source })?;
    if let Some(first) = gaps.first() {
        return Err(BacktestAppError::SeriesGapped {
            pair: pair.clone(),
            timeframe,
            expected: first.expected,
            found: first.found,
        });
    }
    Ok(series)
}

/// Provenance from the series the engine consumed and the config it ran with — no
/// second `HEAD` read, which would record what is current rather than what ran.
/// The `window` is the caller's request, recorded verbatim: the sliced series
/// still names the whole snapshot's `data_version`, so the window columns are
/// what distinguish this run's coverage from an unwindowed one.
fn inputs_from_run(
    primary: &CandleSeries,
    htf: Option<&CandleSeries>,
    config: &BacktestConfig,
    window: Option<CandleWindow>,
) -> BacktestInputs {
    BacktestInputs {
        pair: primary.pair.clone(),
        primary: SnapshotSelection {
            timeframe: primary.timeframe,
            data_version: primary.version.clone(),
        },
        htf: htf.map(|series| SnapshotSelection {
            timeframe: series.timeframe,
            data_version: series.version.clone(),
        }),
        taker_fee_bps: config.taker_fee_bps,
        slippage_bps: config.slippage_bps,
        funding: FundingConfig::SnapshotRates,
        window,
    }
}

// ---------------------------------------------------------------------------
// The a14 default-input resolver (r2.s1.w3)
// ---------------------------------------------------------------------------

/// One resolver for "what should this version run with?" — the MCP
/// `run_backtest` tool and the desktop `run_backtest_version` command call the
/// same function, so a defaulted run resolves identically on both surfaces.
///
/// Precedence: the version's **parent's** latest persisted run → the version's
/// own latest persisted run → the application defaults (`BTCUSDT`, `M15`, `H4`,
/// `BacktestConfig::default()`, `HEAD`). A run whose `inputs` is `None` (a
/// pre-0006 row) is skipped, never an error. From a run the resolver takes the
/// pair, the timeframes, the taker/slippage bps and [`SnapshotPins`] naming the
/// run's exact `data_version`s — an agent run is then comparable with the run
/// it iterates on. `window` is NEVER inherited: it is always the caller's.
///
/// One correction sits on top of the inherit (r2.s2 round-3 fix): when the prior
/// run recorded **no** HTF selection and the version's compiled strategy needs
/// one (`needs_htf()`), `htf_timeframe` falls back to the application default
/// `H4` at `HEAD` — identical to the no-run path, pin included. Neither
/// surface DTO can name `inputs.htf`, so an unconditional `None` inherit would
/// mint a request `run_version_backtest` refuses as `HtfRequired` forever.
/// The fallback is gated on `needs_htf()` so a non-HTF version never loads an
/// H4 snapshot it cannot use.
///
/// # Errors
///
/// [`BacktestAppError::VersionNotFound`] when the version does not exist, and
/// [`BacktestAppError::PreSaveRead`] when a repository read fails.
pub async fn resolve_default_request<S, R>(
    strategies: &S,
    runs: &R,
    version_id: &VersionId,
    window: Option<CandleWindow>,
) -> Result<BacktestRequest, BacktestAppError>
where
    S: StrategyRepository,
    R: BacktestRunRepository,
{
    let version = strategies
        .get_version(version_id)
        .await
        .map_err(|source| BacktestAppError::PreSaveRead {
            stage: PreSaveStage::StrategyVersion,
            source,
        })?
        .ok_or_else(|| BacktestAppError::VersionNotFound(version_id.clone()))?;

    let inherited = match version.parent_version_id.as_ref() {
        Some(parent_id) => match latest_run_inputs(runs, parent_id).await? {
            found @ Some(_) => found,
            None => latest_run_inputs(runs, version_id).await?,
        },
        None => latest_run_inputs(runs, version_id).await?,
    };

    Ok(match inherited {
        Some(inputs) => {
            let mut request = request_from_inputs(version_id, inputs, window);
            // r2.s2 round-3 fix: an htf-needing version inheriting a run that
            // recorded NO HTF selection must not mint `htf_timeframe: None` —
            // `run_version_backtest` would refuse it as `HtfRequired`, a field
            // no surface can set, dead-ending the iterate path. Fall back to
            // the app default `H4` at HEAD (the pin stays `None`), the same
            // shape the no-run arm below produces. Gated on `needs_htf()`: a
            // non-HTF version must not load an H4 snapshot it does not need.
            if request.htf_timeframe.is_none() && version_needs_htf(&version) {
                request.htf_timeframe = Some(Timeframe::H4);
            }
            request
        }
        None => BacktestRequest {
            version_id: version_id.clone(),
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: Some(Timeframe::H4),
            config: BacktestConfig::default(),
            snapshots: None,
            window,
        },
    })
}

/// Whether the version's stored DSL compiles to a strategy that consumes an
/// HTF series. Best-effort for the resolver's fallback only: a document that
/// fails `validate`/`compile` answers `false`, so the resolver never mints a
/// new error and `run_version_backtest` reports the real compile failure
/// exactly as it does on the fresh-default path.
fn version_needs_htf(version: &StrategyVersion) -> bool {
    validate(&version.dsl)
        .ok()
        .and_then(|validated| compile(&validated).ok())
        .is_some_and(|compiled| compiled.needs_htf())
}

/// The version's latest persisted run's `inputs`, when it has a usable row.
/// `inputs: None` (a pre-0006 row) reads the same as no run at all — skipped,
/// never an error.
async fn latest_run_inputs<R>(
    runs: &R,
    version_id: &VersionId,
) -> Result<Option<BacktestInputs>, BacktestAppError>
where
    R: BacktestRunRepository,
{
    let run = runs
        .latest_run_for_version(version_id)
        .await
        .map_err(|source| BacktestAppError::PreSaveRead {
            stage: PreSaveStage::PriorRun,
            source,
        })?;
    Ok(run.and_then(|run| run.inputs))
}

/// Build the request off a prior run's recorded inputs: pair, timeframes,
/// taker/slippage bps, and `SnapshotPins` naming the exact `data_version`s the
/// prior run consumed. `starting_equity` stays the app default — comparability
/// lives in the snapshots and the cost model, and the spec enumerates exactly
/// these fields. The window is the caller's, never the prior run's.
fn request_from_inputs(
    version_id: &VersionId,
    inputs: BacktestInputs,
    window: Option<CandleWindow>,
) -> BacktestRequest {
    let BacktestInputs {
        pair,
        primary,
        htf,
        taker_fee_bps,
        slippage_bps,
        ..
    } = inputs;
    BacktestRequest {
        version_id: version_id.clone(),
        pair,
        primary_timeframe: primary.timeframe,
        htf_timeframe: htf.as_ref().map(|selection| selection.timeframe),
        config: BacktestConfig {
            taker_fee_bps,
            slippage_bps,
            ..BacktestConfig::default()
        },
        snapshots: Some(SnapshotPins {
            primary: primary.data_version,
            htf: htf.map(|selection| selection.data_version),
        }),
        window,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        BacktestAppError, HISTOGRAM_BIN_COUNT, HISTOGRAM_BIN_WIDTH_STR, ReadBackFailure,
        ReadBackStage, SnapshotPins, histogram_bin_width, project_histogram,
        resolve_default_request,
    };
    use crate::adapters::backtest::BacktestConfig;
    use crate::domain::strategy::{
        AgentSubmission, CreatedBy, NewAgentSubmission, NewVersion, Strategy, StrategyId,
        StrategyVersion, VersionId,
    };
    use crate::domain::{
        BacktestInputs, BacktestResult, BacktestRunId, BacktestRunRepository, CandleWindow,
        Comparator, Condition, DataError, DataVersion, Direction, ExitRule, FundingConfig,
        IndicatorSpec, Pair, PersistedRun, PriceField, RegimeBreakdown, RiskParams, RunSummary,
        SchemaVersion, Series, SkippedEntryCounts, SnapshotSelection, StrategyDsl,
        StrategyRepository, SummaryStats, SweepableValue, Timeframe, Trade, ValueSource,
    };
    use chrono::{TimeZone, Utc};
    use rust_decimal::Decimal;
    use std::collections::HashMap;
    use std::future::Future;
    use std::sync::Mutex;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    /// The wire-pinned string and the arithmetic width are two spellings of one
    /// fact; this pin is the only thing that fails when someone widens the domain
    /// (bin count × width) without updating the string the DTO contract re-exports.
    #[test]
    fn the_wire_bin_width_string_equals_the_arithmetic_width() {
        assert_eq!(
            HISTOGRAM_BIN_WIDTH_STR,
            histogram_bin_width().to_string(),
            "the DTO string and histogram_bin_width() drifted apart"
        );
    }

    #[test]
    fn the_pinned_domain_is_zero_to_three_in_twelve_quarter_bins() {
        let h = project_histogram(std::iter::empty());
        assert_eq!(h.bins.len(), HISTOGRAM_BIN_COUNT);
        assert_eq!(h.bin_width, histogram_bin_width());
        assert_eq!(h.bins[0].lower, d("0"));
        assert_eq!(h.bins.last().unwrap().upper, d("3"));
        for pair in h.bins.windows(2) {
            assert_eq!(pair[0].upper, pair[1].lower, "bins tile without a seam");
        }
    }

    #[test]
    fn a_sign_violation_underflows_rather_than_being_folded_by_abs() {
        // A positive MAE normalizes to a NEGATIVE value. `abs()` would put it in a
        // plausible-looking bin; the contract keeps it visible.
        let h = project_histogram([-d("0.5")].into_iter());
        assert_eq!(h.underflow, 1);
        assert!(h.bins.iter().all(|b| b.count == 0));
    }

    #[test]
    fn only_the_saved_variant_reports_a_run_id() {
        let saved = BacktestAppError::SavedButReadBackFailed {
            run_id: BacktestRunId::new("run-1"),
            stage: ReadBackStage::Trades,
            failure: ReadBackFailure::Missing,
        };
        assert_eq!(
            saved.persisted_run_id().map(BacktestRunId::as_str),
            Some("run-1")
        );
        let message = saved.to_string();
        assert!(
            message.contains("run-1") && message.contains("saved"),
            "{message}"
        );

        let pre_save = BacktestAppError::Persist(DataError::Db("nope".to_owned()));
        assert!(
            pre_save.persisted_run_id().is_none(),
            "a pre-save failure has no row to name"
        );
    }

    // -----------------------------------------------------------------------
    // resolve_default_request — local fakes + precedence tests
    // -----------------------------------------------------------------------
    //
    // In-memory `StrategyRepository`/`BacktestRunRepository` stubs in the
    // `port.rs` test-mod style: only the reads the resolver performs are real;
    // every other method is an `unimplemented!` stub.

    /// Minimal lookup over `get_version` — the only `StrategyRepository` read
    /// `resolve_default_request` performs.
    #[derive(Default)]
    struct FakeStrategies {
        versions: Mutex<HashMap<String, StrategyVersion>>,
    }

    impl FakeStrategies {
        fn insert(&self, version: StrategyVersion) {
            self.versions
                .lock()
                .unwrap()
                .insert(version.id.as_str().to_owned(), version);
        }
    }

    impl StrategyRepository for FakeStrategies {
        async fn create_strategy(
            &self,
            _name: &str,
            _owner: Option<&str>,
            _tags: &[String],
        ) -> Result<Strategy, DataError> {
            unimplemented!("FakeStrategies only implements get_version")
        }

        async fn get_strategy(&self, _id: &StrategyId) -> Result<Option<Strategy>, DataError> {
            unimplemented!("FakeStrategies only implements get_version")
        }

        async fn list_strategies(
            &self,
            _include_archived: bool,
        ) -> Result<Vec<Strategy>, DataError> {
            unimplemented!("FakeStrategies only implements get_version")
        }

        async fn rename_strategy(
            &self,
            _id: &StrategyId,
            _new_name: &str,
        ) -> Result<Strategy, DataError> {
            unimplemented!("FakeStrategies only implements get_version")
        }

        async fn set_tags(
            &self,
            _id: &StrategyId,
            _tags: &[String],
        ) -> Result<Strategy, DataError> {
            unimplemented!("FakeStrategies only implements get_version")
        }

        async fn set_pinned_version(
            &self,
            _id: &StrategyId,
            _version_id: Option<&VersionId>,
        ) -> Result<Strategy, DataError> {
            unimplemented!("FakeStrategies only implements get_version")
        }

        async fn archive_strategy(
            &self,
            _id: &StrategyId,
            _archived: bool,
        ) -> Result<Strategy, DataError> {
            unimplemented!("FakeStrategies only implements get_version")
        }

        async fn create_version(&self, _request: NewVersion) -> Result<StrategyVersion, DataError> {
            unimplemented!("FakeStrategies only implements get_version")
        }

        fn get_version(
            &self,
            id: &VersionId,
        ) -> impl Future<Output = Result<Option<StrategyVersion>, DataError>> + Send {
            std::future::ready(Ok(self.versions.lock().unwrap().get(id.as_str()).cloned()))
        }

        async fn list_versions(
            &self,
            _strategy_id: &StrategyId,
        ) -> Result<Vec<StrategyVersion>, DataError> {
            unimplemented!("FakeStrategies only implements get_version")
        }

        async fn version_tree(
            &self,
            _strategy_id: &StrategyId,
        ) -> Result<Vec<StrategyVersion>, DataError> {
            unimplemented!("FakeStrategies only implements get_version")
        }

        async fn create_agent_version(
            &self,
            _request: NewVersion,
            _submission: NewAgentSubmission,
        ) -> Result<(StrategyVersion, AgentSubmission), DataError> {
            unimplemented!("FakeStrategies only implements get_version")
        }

        async fn create_agent_strategy_version(
            &self,
            _strategy_name: &str,
            _dsl_json: String,
            _submission: NewAgentSubmission,
        ) -> Result<(Strategy, StrategyVersion, AgentSubmission), DataError> {
            unimplemented!("FakeStrategies only implements get_version")
        }

        async fn get_agent_submission(
            &self,
            _version_id: &VersionId,
        ) -> Result<Option<AgentSubmission>, DataError> {
            unimplemented!("FakeStrategies only implements get_version")
        }
    }

    /// Flat run store; `latest_run_for_version` returns the most recently
    /// pushed row for the version, matching the adapter's `created_at DESC`
    /// read without depending on timestamp strings.
    #[derive(Default)]
    struct FakeRuns {
        runs: Mutex<Vec<PersistedRun>>,
    }

    impl FakeRuns {
        fn push(&self, run: PersistedRun) {
            self.runs.lock().unwrap().push(run);
        }
    }

    impl BacktestRunRepository for FakeRuns {
        async fn save_run(
            &self,
            _strategy_version_id: &VersionId,
            _inputs: &BacktestInputs,
            _result: &BacktestResult,
            _summary: &SummaryStats,
            _starting_equity: Decimal,
        ) -> Result<BacktestRunId, DataError> {
            unimplemented!("FakeRuns only implements latest_run_for_version")
        }

        async fn get_run(&self, _id: &BacktestRunId) -> Result<Option<PersistedRun>, DataError> {
            unimplemented!("FakeRuns only implements latest_run_for_version")
        }

        fn latest_run_for_version(
            &self,
            strategy_version_id: &VersionId,
        ) -> impl Future<Output = Result<Option<PersistedRun>, DataError>> + Send {
            std::future::ready(Ok(self
                .runs
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|run| run.strategy_version_id == *strategy_version_id)
                .cloned()))
        }

        async fn list_runs_for_version(
            &self,
            _strategy_version_id: &VersionId,
        ) -> Result<Vec<RunSummary>, DataError> {
            unimplemented!("FakeRuns only implements latest_run_for_version")
        }

        async fn get_trades(&self, _id: &BacktestRunId) -> Result<Vec<Trade>, DataError> {
            unimplemented!("FakeRuns only implements latest_run_for_version")
        }
    }

    /// The smallest DSL `StrategyVersion` can carry — `port.rs`'s
    /// `canonical_dsl` shape.
    fn canonical_dsl() -> StrategyDsl {
        StrategyDsl {
            schema_version: SchemaVersion::CURRENT,
            name: "RSI Oversold".to_owned(),
            direction: Direction::Long,
            entry: Condition::Compare {
                lhs: ValueSource::Indicator {
                    series: Series::Primary,
                    spec: IndicatorSpec::Rsi {
                        period: SweepableValue::Fixed(14),
                    },
                },
                op: Comparator::Lt,
                rhs: ValueSource::Constant {
                    value: Decimal::new(30, 0),
                },
            },
            filters: vec![],
            exits: vec![ExitRule::TakeProfit {
                target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
            }],
            risk: RiskParams {
                risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
                max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
            },
        }
    }

    fn version(id: &str, parent: Option<&str>) -> StrategyVersion {
        StrategyVersion {
            id: VersionId::new(id),
            strategy_id: StrategyId::new("strat-1"),
            parent_version_id: parent.map(VersionId::new),
            dsl_schema_version: SchemaVersion::CURRENT,
            dsl: canonical_dsl(),
            dsl_original: "{}".to_owned(),
            version_hash: "deadbeef".to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        }
    }

    /// `canonical_dsl` re-exited so `validate` passes (rule 3 refuses a
    /// `TakeProfit` with no stop in the same strategy). The resolver's
    /// `needs_htf` gate only sees a DSL that validates AND compiles, so a
    /// fixture for it must carry a stop.
    fn compilable_dsl(entry: Condition) -> StrategyDsl {
        StrategyDsl {
            entry,
            exits: vec![ExitRule::StopLoss {
                distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
            }],
            ..canonical_dsl()
        }
    }

    /// An entry reading the H4 close — the operand that makes the compiled
    /// strategy report `needs_htf()`.
    fn htf_entry() -> Condition {
        Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Htf,
                field: PriceField::Close,
            },
            op: Comparator::Gt,
            rhs: ValueSource::Constant {
                value: Decimal::ZERO,
            },
        }
    }

    fn version_with_dsl(id: &str, parent: Option<&str>, dsl: StrategyDsl) -> StrategyVersion {
        StrategyVersion {
            dsl,
            ..version(id, parent)
        }
    }

    fn persisted_run(version_id: &VersionId, inputs: Option<BacktestInputs>) -> PersistedRun {
        PersistedRun {
            id: BacktestRunId::new(format!("run-{}", version_id.as_str())),
            strategy_version_id: version_id.clone(),
            inputs,
            schema_version: 1,
            created_at: "2026-06-30T00:00:00.000Z".to_owned(),
            engine_fingerprint: "fp".to_owned(),
            engine_target: "target".to_owned(),
            result_content_hash: "hash".to_owned(),
            starting_equity: Decimal::new(10_000, 0),
            net_pnl: Decimal::ZERO,
            fees_total: Decimal::ZERO,
            funding_total: Decimal::ZERO,
            slippage_total: Decimal::ZERO,
            summary: SummaryStats::default(),
            regime_breakdown: RegimeBreakdown::new(),
            skipped_entries: SkippedEntryCounts::new(),
            open_position: None,
        }
    }

    /// Recorded inputs distinguishable from the app defaults (non-default bps,
    /// non-`HEAD` snapshot names) so a pin in the resolved request is proof of
    /// which tier supplied it.
    fn recorded_inputs(
        primary_v: &str,
        htf_v: Option<&str>,
        taker: i64,
        slippage: i64,
    ) -> BacktestInputs {
        BacktestInputs {
            pair: Pair::new("BTCUSDT"),
            primary: SnapshotSelection {
                timeframe: Timeframe::M15,
                data_version: DataVersion::new(primary_v),
            },
            htf: htf_v.map(|v| SnapshotSelection {
                timeframe: Timeframe::H4,
                data_version: DataVersion::new(v),
            }),
            taker_fee_bps: Decimal::new(taker, 0),
            slippage_bps: Decimal::new(slippage, 0),
            funding: FundingConfig::SnapshotRates,
            window: None,
        }
    }

    #[tokio::test]
    async fn resolve_default_request_parent_run_wins_over_the_versions_own() {
        let strategies = FakeStrategies::default();
        strategies.insert(version("child-1", Some("parent-1")));
        let runs = FakeRuns::default();
        runs.push(persisted_run(
            &VersionId::new("parent-1"),
            Some(recorded_inputs("v-parent", Some("v-parent-htf"), 7, 2)),
        ));
        runs.push(persisted_run(
            &VersionId::new("child-1"),
            Some(recorded_inputs("v-own", None, 4, 1)),
        ));

        let request = resolve_default_request(&strategies, &runs, &VersionId::new("child-1"), None)
            .await
            .expect("resolve");

        assert_eq!(request.version_id.as_str(), "child-1");
        assert_eq!(request.primary_timeframe, Timeframe::M15);
        assert_eq!(request.htf_timeframe, Some(Timeframe::H4));
        assert_eq!(request.config.taker_fee_bps, Decimal::new(7, 0));
        assert_eq!(request.config.slippage_bps, Decimal::new(2, 0));
        assert_eq!(
            request.snapshots,
            Some(SnapshotPins {
                primary: DataVersion::new("v-parent"),
                htf: Some(DataVersion::new("v-parent-htf")),
            }),
            "the parent's recorded data_versions pin the load, not the child's"
        );
        assert_eq!(request.window, None);
    }

    #[tokio::test]
    async fn resolve_default_request_uses_the_versions_own_run_when_parent_has_none() {
        let strategies = FakeStrategies::default();
        strategies.insert(version("child-1", Some("parent-1")));
        let runs = FakeRuns::default();
        runs.push(persisted_run(
            &VersionId::new("child-1"),
            Some(recorded_inputs("v-own", Some("v-own-htf"), 5, 3)),
        ));

        let request = resolve_default_request(&strategies, &runs, &VersionId::new("child-1"), None)
            .await
            .expect("resolve");

        assert_eq!(
            request.snapshots,
            Some(SnapshotPins {
                primary: DataVersion::new("v-own"),
                htf: Some(DataVersion::new("v-own-htf")),
            }),
            "the parent has no run, so the version's own latest run supplies the request"
        );
        assert_eq!(request.config.taker_fee_bps, Decimal::new(5, 0));
    }

    #[tokio::test]
    async fn resolve_default_request_skips_runs_whose_inputs_are_none() {
        // A pre-0006 row (`inputs: None`) reads the same as no run at all: the
        // parent's unusable latest is skipped and the version's own run wins.
        let strategies = FakeStrategies::default();
        strategies.insert(version("child-1", Some("parent-1")));
        let runs = FakeRuns::default();
        runs.push(persisted_run(&VersionId::new("parent-1"), None));
        runs.push(persisted_run(
            &VersionId::new("child-1"),
            Some(recorded_inputs("v-own", None, 4, 1)),
        ));

        let request = resolve_default_request(&strategies, &runs, &VersionId::new("child-1"), None)
            .await
            .expect("resolve");

        assert_eq!(
            request.snapshots,
            Some(SnapshotPins {
                primary: DataVersion::new("v-own"),
                htf: None,
            }),
            "the parent's pre-0006 row is skipped, not an error"
        );
    }

    #[tokio::test]
    async fn resolve_default_request_falls_back_to_app_defaults() {
        // No parent, no runs at all → the app defaults, HEAD (no pins), and
        // whatever window the caller passed.
        let strategies = FakeStrategies::default();
        strategies.insert(version("orphan-1", None));
        let runs = FakeRuns::default();
        let window = CandleWindow::new(1_000, 2_000).unwrap();

        let request = resolve_default_request(
            &strategies,
            &runs,
            &VersionId::new("orphan-1"),
            Some(window.clone()),
        )
        .await
        .expect("resolve");

        assert_eq!(request.pair, Pair::new("BTCUSDT"));
        assert_eq!(request.primary_timeframe, Timeframe::M15);
        assert_eq!(request.htf_timeframe, Some(Timeframe::H4));
        assert_eq!(request.config, BacktestConfig::default());
        assert_eq!(request.snapshots, None, "no prior run → HEAD, never a pin");
        assert_eq!(request.window, Some(window));
    }

    #[tokio::test]
    async fn resolve_default_request_never_inherits_the_prior_runs_window() {
        // The prior run recorded its own window; the resolver must still put
        // the CALLER's window (here `None`) in the request.
        let strategies = FakeStrategies::default();
        strategies.insert(version("child-1", Some("parent-1")));
        let runs = FakeRuns::default();
        let mut inherited = recorded_inputs("v-parent", None, 4, 1);
        inherited.window = Some(CandleWindow::new(100, 200).unwrap());
        runs.push(persisted_run(&VersionId::new("parent-1"), Some(inherited)));

        let request = resolve_default_request(&strategies, &runs, &VersionId::new("child-1"), None)
            .await
            .expect("resolve");

        assert_eq!(
            request.window, None,
            "a prior run's window is never inherited — it is always the caller's"
        );
    }

    #[tokio::test]
    async fn resolve_default_request_defaults_htf_when_an_htf_version_inherits_none() {
        // r2.s2 round-3 fix: the parent's run is M15-only (`inputs.htf` None)
        // and the child's compiled strategy needs HTF. An unconditional `None`
        // inherit would mint a request `run_version_backtest` refuses as
        // `HtfRequired` — a field neither surface can set — so the resolver
        // falls back to the app default `H4` at HEAD.
        let strategies = FakeStrategies::default();
        strategies.insert(version_with_dsl(
            "child-1",
            Some("parent-1"),
            compilable_dsl(htf_entry()),
        ));
        let runs = FakeRuns::default();
        runs.push(persisted_run(
            &VersionId::new("parent-1"),
            Some(recorded_inputs("v-parent", None, 7, 2)),
        ));

        let request = resolve_default_request(&strategies, &runs, &VersionId::new("child-1"), None)
            .await
            .expect("resolve");

        assert_eq!(
            request.htf_timeframe,
            Some(Timeframe::H4),
            "an htf-needing version inheriting no HTF selection resolves the app default H4"
        );
        assert_eq!(
            request.snapshots,
            Some(SnapshotPins {
                primary: DataVersion::new("v-parent"),
                htf: None,
            }),
            "the primary pin is inherited; the HTF pin stays HEAD, the fresh-default shape"
        );
    }

    #[tokio::test]
    async fn resolve_default_request_keeps_htf_none_for_a_non_htf_version() {
        // The same inherited M15-only run for a version whose strategy does
        // NOT need HTF: `htf_timeframe` stays `None`. The fallback is gated on
        // `needs_htf()`, so no run is made to load an H4 snapshot it cannot
        // use — and a machine without an H4 snapshot for the pair never fails.
        let strategies = FakeStrategies::default();
        strategies.insert(version_with_dsl(
            "child-1",
            Some("parent-1"),
            compilable_dsl(canonical_dsl().entry),
        ));
        let runs = FakeRuns::default();
        runs.push(persisted_run(
            &VersionId::new("parent-1"),
            Some(recorded_inputs("v-parent", None, 7, 2)),
        ));

        let request = resolve_default_request(&strategies, &runs, &VersionId::new("child-1"), None)
            .await
            .expect("resolve");

        assert_eq!(request.htf_timeframe, None);
        assert_eq!(
            request.snapshots,
            Some(SnapshotPins {
                primary: DataVersion::new("v-parent"),
                htf: None,
            }),
            "a non-HTF version inherits the run's inputs untouched"
        );
    }

    #[tokio::test]
    async fn resolve_default_request_unknown_version_is_version_not_found() {
        let err = resolve_default_request(
            &FakeStrategies::default(),
            &FakeRuns::default(),
            &VersionId::new("missing"),
            None,
        )
        .await
        .expect_err("an unknown version cannot resolve");

        assert!(matches!(err, BacktestAppError::VersionNotFound(_)));
    }
}
