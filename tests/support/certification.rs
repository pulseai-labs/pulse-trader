//! The certification suites' shared world (r4.s1.w5): a migrated database, a
//! synthetic candle store with `HEAD`, one strategy version and one open
//! `H = 12` freeze.
//!
//! **The synthetic series is the certify-fixture generator's own shape, at a
//! length the holdout test needs.** The fixture's walk — a mean-reverting
//! AR(1) around 60,000 driven by a fixed-seed splitmix64 stream — is what
//! gives the pinned dip-buy strategy ([`fixture_strategy_dsl`]) a real, plainly
//! synthetic edge. The fixture's own 16,800 bars are too few to give the
//! holdout the trade count the C1 test's power needs (ADR-0028 measured ≈ 49%
//! at 160 holdout trades), so the same walk is generated here over a longer
//! span and the SAME strategy document trades it: scaling the series scales
//! the sample, not the edge.
//!
//! `tests/certification_holdout.rs` (AC-1) drives the step itself;
//! `tests/mcp_certify_version.rs` (AC-2) drives the same world through a real
//! `pulse mcp` session. Both import from here so the two suites cannot drift
//! into testing different worlds.

// Named-lint allow, scoped to this module (the `support/mcp.rs` precedent):
// every suite that declares `mod support;` compiles this file, and a helper one
// suite does not call is not dead code — it is another suite's.
#![allow(dead_code)]

use crate::support::mcp::migrated_db;
use pulse::{
    Candle, CandleSeries, CandleStore, CertifyRequest, CreatedBy, Db, FakeClock, NewVersion,
    OpenFreezeRequest, Pair, SqliteBacktestRunRepo, SqliteCertificationFreezeRepo,
    SqliteCertificationRepo, SqliteStrategyRepo, StrategyRepository, Timeframe, VersionId,
    WalkForwardRunRepository, certify_version, fixture_h4_candles_from,
};
use rust_decimal::Decimal;
use tempfile::TempDir;

/// The hypothesis budget every world freezes: the campaign's H (ADR-0028).
pub const H: u8 = 12;
/// M15 bars in the synthetic series (a multiple of the H4 group, so the H4
/// aggregation drops nothing).
pub const BARS: usize = 50_000;
/// Where the holdout starts, as a bar index — 45% of the series, so the search
/// span still holds six folds of thousands of bars and the holdout holds enough
/// trades for the C1 test to have the power ADR-0028 measured.
pub const HOLDOUT_INDEX: usize = 27_500;
/// The fixture generator's price level — the pinned strategy's thresholds are
/// minted around it.
pub const BASE_PRICE: i64 = 60_000;
const NOISE_HALF_WIDTH: i64 = 300;
const REVERSION_PERMILLE: i64 = 300;
const WICK_HALF_WIDTH: i64 = 40;
/// One M15 bar, in milliseconds.
pub const M15_MS: i64 = 900_000;
const FUNDING_EVERY_MS: i64 = 28_800_000;
/// The fixture generator's seed (the same stream, a longer walk).
pub const SEED: u64 = 0x5EED_1CE5_F1C7_2026;
/// 2025-01-01T00:00:00Z — the fixture's own start instant.
const START_MS: i64 = 1_735_689_600_000;

/// When the freeze opens — before the version, so C4's lineage rule is
/// satisfied by construction: only lineages rooted AFTER the freeze may be
/// certified, and the campaign's own candidates are started after it.
pub const FREEZE_OPENED_MS: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
/// When the version's root is created — after the freeze opens (the
/// pre-freeze case builds its own world with these two reversed).
pub const VERSION_CREATED_MS: i64 = 1_767_312_000_000; // 2026-01-02T00:00:00Z

/// splitmix64 (Steele et al.) — the fixture generator's PRNG, copied so these
/// suites' series is deterministic for the same reason the fixture's is.
struct SplitMix64(u64);

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    #[allow(clippy::cast_possible_wrap)]
    fn below_inclusive(&mut self, span: i64) -> i64 {
        let width = u64::try_from(span).unwrap_or(0).saturating_add(1);
        (self.next_u64() % width) as i64
    }
}

/// The fixture walk's mid prices, one per bar: each step pulls 30% of the
/// displacement back toward the base and adds a uniform innovation.
fn mid_prices(seed: u64, bars: usize) -> Vec<i64> {
    let mut rng = SplitMix64::new(seed);
    let mut displacement: i64 = 0;
    let mut out = Vec::with_capacity(bars);
    for _ in 0..bars {
        out.push(BASE_PRICE + displacement);
        let innovation = rng.below_inclusive(2 * NOISE_HALF_WIDTH) - NOISE_HALF_WIDTH;
        displacement = displacement - REVERSION_PERMILLE * displacement / 1000 + innovation;
    }
    out
}

/// The fixture walk's M15 bars: contiguous `open_time`s, one OHLCV(+funding)
/// bar per mid price, funding stamped on every 8-hour boundary.
pub fn m15_candles(seed: u64, bars: usize) -> Vec<Candle> {
    let mids = mid_prices(seed, bars);
    let mut rng = SplitMix64::new(seed ^ 0x5151_5151_5151_5151);
    let funding = Decimal::new(1, 5);
    let mut out = Vec::with_capacity(mids.len());
    let mut prev_close = Decimal::from(mids.first().copied().unwrap_or(BASE_PRICE));
    for (i, mid) in mids.iter().copied().enumerate() {
        let open_time = START_MS + i64::try_from(i).unwrap_or(0) * M15_MS;
        let close = Decimal::from(mid);
        let wick = Decimal::from(rng.below_inclusive(WICK_HALF_WIDTH));
        let high = prev_close.max(close) + wick;
        let low = prev_close.min(close) - wick;
        let volume = Decimal::from(50 + rng.below_inclusive(150));
        out.push(Candle {
            open_time,
            close_time: open_time + M15_MS - 1,
            open: prev_close,
            high,
            low,
            close,
            volume,
            funding_rate: if open_time % FUNDING_EVERY_MS == 0 {
                Some(funding)
            } else {
                None
            },
        });
        prev_close = close;
    }
    out
}

/// Write one pair's M15 + H4 series as content-addressed snapshots AND `HEAD`
/// pointers (the resolver's no-prior-run path loads `HEAD`), answering both
/// data versions.
pub fn write_pair_series(
    store: &CandleStore,
    pair: &Pair,
    seed: u64,
    bars: usize,
) -> (pulse::DataVersion, pulse::DataVersion) {
    let m15 = m15_candles(seed, bars);
    let h4 = fixture_h4_candles_from(&m15);
    let m15_version = CandleStore::content_version(pair, Timeframe::M15, &m15);
    let h4_version = CandleStore::content_version(pair, Timeframe::H4, &h4);
    store
        .write_snapshot(&CandleSeries {
            pair: pair.clone(),
            timeframe: Timeframe::M15,
            version: m15_version.clone(),
            candles: m15,
        })
        .unwrap();
    store
        .write_snapshot(&CandleSeries {
            pair: pair.clone(),
            timeframe: Timeframe::H4,
            version: h4_version.clone(),
            candles: h4,
        })
        .unwrap();
    store
        .write_head(pair, Timeframe::M15, &m15_version)
        .unwrap();
    store.write_head(pair, Timeframe::H4, &h4_version).unwrap();
    (m15_version, h4_version)
}

/// Everything a certification suite needs: the temp DB (and its path), the
/// candle store, the version, and the open freeze.
pub struct World {
    /// The temp directory holding the database and the store.
    pub tmp: TempDir,
    /// The migrated pool.
    pub db: Db,
    /// `pulse.db`'s path — the MCP child process opens this same file.
    pub db_path: std::path::PathBuf,
    /// The candle store (its base dir is the MCP child's `--data-dir`).
    pub store: CandleStore,
    /// The pair the version's snapshots were written under.
    pub pair: Pair,
    /// The M15 data version the world wrote.
    pub m15_version: pulse::DataVersion,
    /// The H4 data version the world wrote.
    pub h4_version: pulse::DataVersion,
    /// Where the open freeze's holdout starts.
    pub holdout_start_ms: i64,
    /// The version to certify.
    pub version: VersionId,
    /// The open `H = 12` freeze.
    pub freeze: pulse::FreezeRecord,
}

impl World {
    /// The candle store's base dir — the MCP child's `--data-dir`.
    #[must_use]
    pub fn data_dir(&self) -> std::path::PathBuf {
        self.tmp.path().join("candles")
    }
}

/// The planted-edge world: the fixture's own dip-buy over the fixture's own
/// walk, at this suite's length.
pub async fn world() -> World {
    world_with(pulse::fixture_strategy_dsl()).await
}

/// Build the world over `dsl`, with the standard instants.
pub async fn world_with(dsl: pulse::StrategyDsl) -> World {
    world_with_clocks(dsl, VERSION_CREATED_MS, FREEZE_OPENED_MS).await
}

/// [`world_with`], with the version's creation instant and the freeze's opening
/// instant chosen by the caller — the pre-freeze-lineage case reverses them.
pub async fn world_with_clocks(
    dsl: pulse::StrategyDsl,
    version_created_ms: i64,
    freeze_opened_ms: i64,
) -> World {
    let tmp = TempDir::new().unwrap();
    let (db_path, db) = migrated_db(&tmp).await;
    let pair = pulse::fixture_pair();
    let store = CandleStore::with_base_dir(tmp.path().join("candles"));

    let m15 = m15_candles(SEED, BARS);
    let holdout_start_ms = m15[HOLDOUT_INDEX].open_time;
    let h4 = fixture_h4_candles_from(&m15);
    let m15_version = CandleStore::content_version(&pair, Timeframe::M15, &m15);
    let h4_version = CandleStore::content_version(&pair, Timeframe::H4, &h4);
    store
        .write_snapshot(&CandleSeries {
            pair: pair.clone(),
            timeframe: Timeframe::M15,
            version: m15_version.clone(),
            candles: m15,
        })
        .unwrap();
    store
        .write_snapshot(&CandleSeries {
            pair: pair.clone(),
            timeframe: Timeframe::H4,
            version: h4_version.clone(),
            candles: h4,
        })
        .unwrap();
    store
        .write_head(&pair, Timeframe::M15, &m15_version)
        .unwrap();
    store.write_head(&pair, Timeframe::H4, &h4_version).unwrap();

    let strategies = SqliteStrategyRepo::with_deps(
        db.pool().clone(),
        pulse::Migrator::v1(),
        FakeClock::at(version_created_ms),
    );
    let strategy = strategies
        .create_strategy("w5 certification", Some("w5"), &[])
        .await
        .unwrap();
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(&dsl).unwrap(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .unwrap()
        .id;

    let freeze = SqliteCertificationFreezeRepo::with_deps(
        db.pool().clone(),
        FakeClock::at(freeze_opened_ms),
    )
    .open(&OpenFreezeRequest {
        holdout_start_ms,
        h: H,
        alpha: "0.05".to_owned(),
        holdout_test: "one-sided lower confidence bound at 1 - 0.05/H (C1)".to_owned(),
    })
    .await
    .unwrap();

    World {
        tmp,
        db,
        db_path,
        store,
        pair,
        m15_version,
        h4_version,
        holdout_start_ms,
        version,
        freeze,
    }
}

/// One certification call, exactly as the MCP tool makes it.
pub async fn certify(
    world: &World,
    freeze: Option<&pulse::FreezeRecord>,
) -> Result<pulse::CertifyOutcome, pulse::CertifyError> {
    certify_with_pair(world, freeze, None).await
}

/// [`certify`] with an optional (already-validated) pair override.
pub async fn certify_with_pair(
    world: &World,
    freeze: Option<&pulse::FreezeRecord>,
    pair: Option<Pair>,
) -> Result<pulse::CertifyOutcome, pulse::CertifyError> {
    certify_version(
        &SqliteStrategyRepo::new(world.db.pool().clone()),
        &world.store,
        &pulse::BinanceAdapter::new(),
        &SqliteBacktestRunRepo::new(world.db.pool().clone()),
        &SqliteCertificationRepo::new(world.db.pool().clone()),
        freeze,
        &CertifyRequest {
            version_id: world.version.clone(),
            pair,
            called_by: "w5-test-agent".to_owned(),
        },
    )
    .await
}

/// One walk-forward over the world's snapshots under `rule`, with the freeze's
/// clamp — the search-span run a certification makes, and (with no record
/// behind it) the run AC-3's refusal and AC-1's flag case turn on.
pub async fn walk_forward_under(
    world: &World,
    rule: pulse::VerdictRule,
) -> pulse::WalkForwardOutcome {
    pulse::run_walk_forward(
        &SqliteStrategyRepo::new(world.db.pool().clone()),
        &world.store,
        &pulse::BinanceAdapter::new(),
        &runs(world),
        &pulse::WalkForwardRequest {
            version_id: world.version.clone(),
            pair: world.pair.clone(),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: Some(Timeframe::H4),
            config: pulse::BacktestConfig::default(),
            snapshots: None,
            from_ms: None,
            to_ms: None,
            k: None,
            rule: Some(rule),
        },
        Some(world.freeze.holdout()),
    )
    .await
    .expect("the walk-forward runs over the world's snapshots")
}

/// A version with **no edge on this series**: it buys the rip (the reversion's
/// own exit level) and leaves below its entry, so every trade that closes does
/// so at a loss. The C1 test must refuse it; the record must still exist.
pub fn inverted_dsl() -> pulse::StrategyDsl {
    use pulse::{
        Comparator, Condition, Direction, ExitRule, PriceField, RiskParams, SchemaVersion, Series,
        StrategyDsl, SweepableValue, ValueSource,
    };
    let close = || ValueSource::Price {
        series: Series::Primary,
        field: PriceField::Close,
    };
    let against = |value: i64| ValueSource::Constant {
        value: Decimal::from(value),
    };
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "w5 anti-reversion probe".to_owned(),
        direction: Direction::Long,
        entry: Condition::Compare {
            lhs: close(),
            op: Comparator::Gt,
            rhs: against(BASE_PRICE + 180),
        },
        filters: vec![],
        exits: vec![
            ExitRule::StopLoss {
                distance_pct: SweepableValue::Fixed(Decimal::new(5, 3)),
            },
            ExitRule::SignalExit {
                condition: Condition::Compare {
                    lhs: close(),
                    op: Comparator::Lte,
                    rhs: against(BASE_PRICE + 30),
                },
            },
        ],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
            max_leverage: SweepableValue::Fixed(Decimal::from(3)),
        },
    }
}

/// The certification store over the world's pool, with a fixed clock (so a
/// seeded record's `created_at` is deterministic).
#[must_use]
pub fn certifications(world: &World) -> SqliteCertificationRepo<FakeClock> {
    SqliteCertificationRepo::with_deps(world.db.pool().clone(), FakeClock::at(FREEZE_OPENED_MS))
}

/// The backtest-run / walk-forward store over the world's pool.
#[must_use]
pub fn runs(world: &World) -> SqliteBacktestRunRepo<pulse::SystemClock> {
    SqliteBacktestRunRepo::new(world.db.pool().clone())
}

/// The pieces a seeded hypothesis names — everything [`probe_draft`] needs,
/// lifted out so a second DB (the route test's server, the MCP suite's child)
/// can seed the same shape.
pub struct Probe<'a> {
    /// The version the record names.
    pub version: &'a VersionId,
    /// The pair the record names.
    pub pair: &'a Pair,
    /// The freeze the record ran under.
    pub freeze_id: &'a str,
    /// Where the record's holdout starts.
    pub holdout_start_ms: i64,
    /// The primary data version the record names.
    pub primary_version: &'a str,
    /// The HTF data version the record names.
    pub htf_version: &'a str,
}

/// One seeded hypothesis draft: a passing certification pointing at `run_id`.
#[must_use]
pub fn probe_draft(
    probe: &Probe<'_>,
    run_id: &pulse::WalkForwardRunId,
) -> pulse::CertificationDraft {
    let selection = |timeframe: Timeframe, version: &str| pulse::SnapshotSelection {
        timeframe,
        data_version: pulse::DataVersion::new(version.to_owned()),
    };
    let inputs = pulse::CertificationInputs {
        primary: selection(Timeframe::M15, probe.primary_version),
        htf: Some(selection(Timeframe::H4, probe.htf_version)),
        d1: None,
    };
    pulse::CertificationDraft {
        version_id: probe.version.clone(),
        freeze_id: probe.freeze_id.to_owned(),
        rule: "wf-v2".to_owned(),
        pair: probe.pair.clone(),
        search_walk_forward_run_id: run_id.clone(),
        search_pass: true,
        holdout_start_ms: probe.holdout_start_ms,
        holdout_end_ms: probe.holdout_start_ms + M15_MS,
        holdout_n: 200,
        holdout_mean_r: Decimal::new(4, 1),
        holdout_z: 2.64,
        holdout_lower_bound: 0.2,
        holdout_passes: true,
        search_inputs: inputs.clone(),
        holdout_inputs: inputs,
        engine_fingerprint: pulse::EngineFingerprint::current().as_str().to_owned(),
        called_by: "w5-test-agent".to_owned(),
    }
}

/// [`probe_draft`] over the world's own version and freeze.
#[must_use]
pub fn seeded_draft(world: &World, run_id: &pulse::WalkForwardRunId) -> pulse::CertificationDraft {
    probe_draft(
        &Probe {
            version: &world.version,
            pair: &world.pair,
            freeze_id: &world.freeze.id,
            holdout_start_ms: world.holdout_start_ms,
            primary_version: world.m15_version.as_str(),
            htf_version: world.h4_version.as_str(),
        },
        run_id,
    )
}

/// One persisted walk-forward run for the world's version — the FK target the
/// seeded hypotheses point at. A budget test seeds its records directly
/// (through the same store the step uses) so it does not pay engine runs; the
/// count of records is what the budget reads.
pub async fn seed_search_run(world: &World) -> pulse::WalkForwardRunId {
    SqliteBacktestRunRepo::new(world.db.pool().clone())
        .save_walk_forward_run(
            &world.version,
            &crate::support::mcp::seeded_walk_forward_draft(true),
        )
        .await
        .expect("the seeded search run persists")
}

/// How many walk-forward runs the version has — the "a refusal runs nothing"
/// probe.
pub async fn walk_forward_run_count(world: &World) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_run WHERE strategy_version_id = ?1")
        .bind(world.version.as_str())
        .fetch_one(world.db.pool())
        .await
        .unwrap()
}
