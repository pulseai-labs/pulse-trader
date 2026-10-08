//! The certify-fixture application ring (r3.s4.w2, ADR-0027).
//!
//! `domain::paper::fixture` generates the synthetic BTCUSDT M15 + H4 series;
//! this module is the ring the rest of the crate and the integration tests
//! see: pair/candle accessors, the pinned fixture strategy document (a plain
//! long dip-buy whose thresholds are minted from the generator's own
//! constants so the two cannot drift apart), and (with `seed`, added by the
//! AC-4 loop) the durable stamping of those series into the candle store and
//! the `fixture_snapshot`/`strategy`/`walk_forward_run` rows.
//!
//! The generator is deterministic (a pinned `SplitMix64` seed), so a given
//! build always produces the same series, the same content-derived
//! `data_version`s, and the same walk-forward folds — the property
//! `tests/certify_fixture.rs` pins as constants.

use rust_decimal::Decimal;

use crate::domain::paper::fixture as synth;
use crate::domain::{
    Candle, Comparator, Condition, Direction, ExitRule, Pair, PriceField, RiskParams,
    SchemaVersion, Series, StrategyDsl, SweepableValue, Timeframe, ValueSource,
};

/// The fixture pair, straight from the generator (`"BTCUSDT"`).
pub const FIXTURE_PAIR: &str = synth::PAIR;

/// The fixture generator's seed, pinned alongside the data versions it
/// determines.
pub const FIXTURE_SEED: u64 = synth::SEED;

/// The strategy `seed` mints (tagged [`FIXTURE_STRATEGY_TAG`]). Named plainly
/// (E3): it exists to keep the fixture's own walk-forward certified, not to
/// trade anything real.
pub const FIXTURE_STRATEGY_NAME: &str = "FIXTURE certify path";

/// The tag marking every fixture-minted strategy row.
pub const FIXTURE_STRATEGY_TAG: &str = "fixture";

/// The signal exit's threshold, relative to [`synth::BASE_PRICE`]: the dip-buy
/// leaves once price has reverted to `BASE − EXIT_OFFSET` (30 below the
/// center), well before the walk's upper excursions.
const EXIT_OFFSET: i64 = 30;

/// The fixture pair.
#[must_use]
pub fn fixture_pair() -> Pair {
    Pair::new(synth::PAIR)
}

/// The fixture M15 series (16,800 bars, deterministic).
#[must_use]
pub fn fixture_m15_candles() -> Vec<Candle> {
    synth::m15_candles()
}

/// The fixture H4 series (aggregated from the M15 series, deterministic).
#[must_use]
pub fn fixture_h4_candles() -> Vec<Candle> {
    let m15 = synth::m15_candles();
    synth::h4_candles(&m15)
}

/// The H4 aggregation of an arbitrary M15 slice — the SAME aggregation the
/// full fixture series rides, exposed so the promotion-gate tests can certify
/// over a shifted slice (different content, different data versions, provably
/// not the fixture stamp).
#[must_use]
pub fn fixture_h4_candles_from(m15: &[Candle]) -> Vec<Candle> {
    synth::h4_candles(m15)
}

/// The fixture strategy document: a long-only dip-buy.
///
/// - **Entry** `close < BASE − ENTRY_OFFSET` — buy a dip below the
///   center; the AR(1) walk pulls 30% of the displacement back each bar, so
///   the reversion leg is the trade.
/// - **Exit** the reversion itself: a [`ExitRule::SignalExit`] at
///   `close ≥ BASE − 30`, bracketed by a [`ExitRule::StopLoss`] 0.5% below
///   entry (which defines 1R for sizing and the R ledger).
///
/// The thresholds are computed from the generator's own constants, so
/// retuning the walk retunes the strategy with it.
#[must_use]
pub fn fixture_strategy_dsl() -> StrategyDsl {
    // The bracket around the reversion (fractions of price): the stop distance
    // (0.5% ≈ 300 at the base) defines 1R — the size the sizing identity risks
    // per trade — while the signal exit captures the mean-reversion leg. Risk
    // is 1% of equity under a 3x leverage cap (spec §5).
    let stop_distance_pct = Decimal::new(5, 3);
    let risk_per_trade_pct = Decimal::new(1, 2);
    let max_leverage = Decimal::from(3);
    let base = Decimal::from(synth::BASE_PRICE);
    let entry = Condition::Compare {
        lhs: ValueSource::Price {
            series: Series::Primary,
            field: PriceField::Close,
        },
        op: Comparator::Lt,
        rhs: ValueSource::Constant {
            value: base - Decimal::from(synth::ENTRY_OFFSET),
        },
    };
    let exit_signal = Condition::Compare {
        lhs: ValueSource::Price {
            series: Series::Primary,
            field: PriceField::Close,
        },
        op: Comparator::Gte,
        rhs: ValueSource::Constant {
            value: base - Decimal::from(EXIT_OFFSET),
        },
    };
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: FIXTURE_STRATEGY_NAME.to_owned(),
        direction: Direction::Long,
        entry,
        filters: vec![],
        exits: vec![
            ExitRule::StopLoss {
                distance_pct: SweepableValue::Fixed(stop_distance_pct),
            },
            ExitRule::SignalExit {
                condition: exit_signal,
            },
        ],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(risk_per_trade_pct),
            max_leverage: SweepableValue::Fixed(max_leverage),
        },
    }
}

// ---------------------------------------------------------------------------
// The seed use case (spec §5, E4)
// ---------------------------------------------------------------------------

/// What a seed left behind: the strategy, its version, the certifying
/// walk-forward run, and whether this call changed anything.
#[derive(Debug, Clone, PartialEq)]
pub struct FixtureSeedOutcome {
    /// The fixture strategy's id.
    pub strategy_id: crate::domain::strategy::StrategyId,
    /// The fixture version's id.
    pub version_id: crate::domain::strategy::VersionId,
    /// The certifying walk-forward run's id (this build's).
    pub walk_forward_run_id: crate::domain::backtest::WalkForwardRunId,
    /// True when the strategy, version and THIS BUILD's run already existed —
    /// a second seed on one build adds no row and no file.
    pub already_seeded: bool,
    /// True when a NEW walk-forward run was added (a new build re-certifies:
    /// exactly one run per fingerprint, ever).
    pub added_run: bool,
}

/// Why a seed refused.
#[derive(Debug, Clone, PartialEq)]
pub enum FixtureSeedError {
    /// A same-named strategy exists whose versions carry a DIFFERENT document
    /// than this build's generator mints — the fixture is per-build content,
    /// and a drift is a bug to surface, not to overwrite.
    VersionMismatch,
    /// A store failure.
    Data(crate::domain::DataError),
    /// The certifying walk-forward failed (a real run — the fixture proof's
    /// halt rule is the test suite's, not the seed's).
    WalkForward(crate::application::walk_forward::WalkForwardAppError),
}

impl core::fmt::Display for FixtureSeedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::VersionMismatch => write!(
                f,
                "a strategy named {FIXTURE_STRATEGY_NAME:?} exists with a different document \
                 than this build generates; the fixture is per-build content — remove the \
                 stale strategy instead of overriding it"
            ),
            Self::Data(e) => write!(f, "fixture seed store failure: {e}"),
            Self::WalkForward(e) => write!(f, "fixture seed walk-forward failed: {e}"),
        }
    }
}

impl std::error::Error for FixtureSeedError {}

/// The certify fixture's durable stamp, idempotent per build (spec §5):
///
/// 1. both snapshots land with `write_snapshot` only (never HEAD) — skipped
///    when the content-addressed file already exists, so a second seed adds
///    no file;
/// 2. their `fixture_snapshot` rows are stamped (`INSERT OR IGNORE`);
/// 3. the strategy (named [`FIXTURE_STRATEGY_NAME`], tagged
///    [`FIXTURE_STRATEGY_TAG`]) and its version exist — a same-named strategy
///    whose versions all differ from this build's document refuses as
///    [`FixtureSeedError::VersionMismatch`];
/// 4. a real [`run_walk_forward`] runs with the snapshots pinned, ONCE per
///    engine fingerprint: a re-seed on the same build is a no-op, and a new
///    build (whose latest run carries a foreign fingerprint) adds exactly one
///    run.
///
/// # Errors
///
/// [`FixtureSeedError::VersionMismatch`], [`FixtureSeedError::Data`],
/// [`FixtureSeedError::WalkForward`].
pub async fn seed<S, R, P, C, E>(
    strategies: &S,
    snapshots: &C,
    runs: &R,
    paper: &P,
    exchange: &E,
) -> Result<FixtureSeedOutcome, FixtureSeedError>
where
    S: crate::domain::StrategyRepository,
    R: crate::domain::BacktestRunRepository + crate::domain::WalkForwardRunRepository,
    P: crate::domain::PaperSessionRepository,
    C: crate::domain::CandleSeriesRepository
        + crate::domain::FixtureSnapshotStore
        + Clone
        + Send
        + 'static,
    E: crate::domain::ExchangeAdapter + Clone + Send + 'static,
{
    use crate::domain::DataVersion;

    let pair = fixture_pair();
    let m15 = fixture_m15_candles();
    let h4 = fixture_h4_candles();
    let m15_version: DataVersion = snapshots.fixture_content_version(&pair, Timeframe::M15, &m15);
    let h4_version: DataVersion = snapshots.fixture_content_version(&pair, Timeframe::H4, &h4);

    // 1. The snapshot FILES (content-addressed; a present file is left alone).
    for (timeframe, version, candles) in [
        (Timeframe::M15, &m15_version, &m15),
        (Timeframe::H4, &h4_version, &h4),
    ] {
        if !snapshots.fixture_snapshot_exists(&pair, timeframe, version) {
            snapshots
                .fixture_write_snapshot(&crate::domain::CandleSeries {
                    pair: pair.clone(),
                    timeframe,
                    version: version.clone(),
                    candles: candles.clone(),
                })
                .map_err(FixtureSeedError::Data)?;
        }
    }

    // 2. The `fixture_snapshot` rows (idempotent stamps) — BEFORE the
    //    strategy resolution, the spec §5's written order. They stand even
    //    when step 3 refuses on document drift: the stamps name the fixture
    //    data, which exists regardless of what the drifted strategy says.
    paper
        .insert_fixture_snapshot(&pair, Timeframe::M15, &m15_version)
        .await
        .map_err(FixtureSeedError::Data)?;
    paper
        .insert_fixture_snapshot(&pair, Timeframe::H4, &h4_version)
        .await
        .map_err(FixtureSeedError::Data)?;

    // 3. The strategy + version — find-or-create, with the document drift
    //    refusal.
    let strategies_list = strategies
        .list_strategies(false)
        .await
        .map_err(FixtureSeedError::Data)?;
    let existing = strategies_list
        .iter()
        .find(|strategy| strategy.name == FIXTURE_STRATEGY_NAME)
        .cloned();
    let already_seeded = existing.is_some();
    let (strategy_id, version_id) = if let Some(strategy) = existing {
        find_fixture_version(strategies, &strategy).await?
    } else {
        create_fixture_strategy(strategies).await?
    };

    // 4. The certifying walk-forward — once per fingerprint. The pointer
    //    names the latest run: current fingerprint ⇒ done; anything else
    //    (no run, or a foreign one) ⇒ this build runs its own.
    let version = strategies
        .get_version(&version_id)
        .await
        .map_err(FixtureSeedError::Data)?
        .ok_or_else(|| {
            FixtureSeedError::Data(crate::domain::DataError::Db(
                "the fixture version vanished mid-seed".to_owned(),
            ))
        })?;
    let pointer_run = latest_pointer_run(runs, version.latest_walk_forward_run_id.as_ref()).await?;
    let current = crate::domain::EngineFingerprint::current();
    if let Some(run) = pointer_run.filter(|run| run.engine_fingerprint == current.as_str()) {
        return Ok(FixtureSeedOutcome {
            strategy_id,
            version_id,
            walk_forward_run_id: run.id,
            already_seeded: true,
            added_run: false,
        });
    }
    let walk_forward_run_id = certify_fixture(
        strategies,
        snapshots,
        runs,
        &version_id,
        &pair,
        m15_version,
        h4_version,
        exchange,
    )
    .await?;
    Ok(FixtureSeedOutcome {
        strategy_id,
        version_id,
        walk_forward_run_id,
        already_seeded,
        added_run: true,
    })
}

/// The version's latest walk-forward run, if the pointer names one — the
/// once-per-fingerprint check's input.
async fn latest_pointer_run<R>(
    runs: &R,
    latest_walk_forward_run_id: Option<&crate::domain::backtest::WalkForwardRunId>,
) -> Result<Option<crate::domain::backtest::WalkForwardRun>, FixtureSeedError>
where
    R: crate::domain::WalkForwardRunRepository,
{
    match latest_walk_forward_run_id {
        Some(run_id) => runs
            .get_walk_forward_run(run_id)
            .await
            .map_err(FixtureSeedError::Data),
        None => Ok(None),
    }
}

/// Step 4: the real pinned walk-forward — once per fingerprint.
#[allow(clippy::too_many_arguments)] // version + pair + the two pinned versions
async fn certify_fixture<S, R, C, E>(
    strategies: &S,
    snapshots: &C,
    runs: &R,
    version_id: &crate::domain::strategy::VersionId,
    pair: &Pair,
    m15_version: crate::domain::DataVersion,
    h4_version: crate::domain::DataVersion,
    exchange: &E,
) -> Result<crate::domain::backtest::WalkForwardRunId, FixtureSeedError>
where
    S: crate::domain::StrategyRepository,
    R: crate::domain::BacktestRunRepository + crate::domain::WalkForwardRunRepository,
    C: crate::domain::CandleSeriesRepository
        + crate::domain::FixtureSnapshotStore
        + Clone
        + Send
        + 'static,
    E: crate::domain::ExchangeAdapter + Clone + Send + 'static,
{
    let request = crate::application::walk_forward::WalkForwardRequest {
        version_id: version_id.clone(),
        pair: pair.clone(),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: Some(Timeframe::H4),
        config: crate::BacktestConfig::default(),
        snapshots: Some(crate::application::backtest::SnapshotPins {
            primary: m15_version,
            htf: Some(h4_version),
            d1: None,
        }),
        from_ms: None,
        to_ms: None,
        k: None,
    };
    let outcome = crate::application::walk_forward::run_walk_forward(
        strategies, snapshots, exchange, runs, &request,
        // EXEMPT (grill Q4, G8): the certify-fixture seed runs on its own
        // synthetic snapshots — never HEAD, never the campaign's data — so it
        // is exempt from the holdout guard and passes `None` deliberately. The
        // other two exemptions are the certification step (w5) and paper
        // sessions; the guard module's doc names all three.
        None,
    )
    .await
    .map_err(FixtureSeedError::WalkForward)?;
    Ok(outcome.run.id)
}

/// The fixture version's id under an EXISTING same-named strategy, refusing
/// when none of its versions carries this build's document.
async fn find_fixture_version<S>(
    strategies: &S,
    strategy: &crate::domain::strategy::Strategy,
) -> Result<
    (
        crate::domain::strategy::StrategyId,
        crate::domain::strategy::VersionId,
    ),
    FixtureSeedError,
>
where
    S: crate::domain::StrategyRepository,
{
    let versions = strategies
        .list_versions(&strategy.id)
        .await
        .map_err(FixtureSeedError::Data)?;
    let document = fixture_strategy_dsl();
    match versions.iter().find(|version| version.dsl == document) {
        Some(version) => Ok((strategy.id.clone(), version.id.clone())),
        None => Err(FixtureSeedError::VersionMismatch),
    }
}

/// Mint the fixture strategy (named [`FIXTURE_STRATEGY_NAME`], tagged
/// [`FIXTURE_STRATEGY_TAG`]) and its version.
async fn create_fixture_strategy<S>(
    strategies: &S,
) -> Result<
    (
        crate::domain::strategy::StrategyId,
        crate::domain::strategy::VersionId,
    ),
    FixtureSeedError,
>
where
    S: crate::domain::StrategyRepository,
{
    let strategy = strategies
        .create_strategy(
            FIXTURE_STRATEGY_NAME,
            None,
            &[FIXTURE_STRATEGY_TAG.to_owned()],
        )
        .await
        .map_err(FixtureSeedError::Data)?;
    let version = strategies
        .create_version(crate::domain::strategy::NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: serde_json::to_string(&fixture_strategy_dsl()).map_err(|e| {
                FixtureSeedError::Data(crate::domain::DataError::Db(format!("dsl serialize: {e}")))
            })?,
            created_by: crate::domain::strategy::CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .map_err(FixtureSeedError::Data)?;
    Ok((strategy.id, version.id))
}
