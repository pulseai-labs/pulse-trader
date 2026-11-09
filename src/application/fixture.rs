//! The certify-fixture application ring (r3.s4.w2, ADR-0027).
//!
//! `domain::paper::fixture` generates the synthetic BTCUSDT M15 + H4 series;
//! this module is the ring the rest of the crate and the integration tests
//! see: pair/candle accessors and the pinned fixture strategy document (a plain
//! long dip-buy whose thresholds are minted from the generator's own
//! constants so the two cannot drift apart). The `seed` use case that stamped
//! these series into a database was retired in r7.s1.w1 (PDR-0051 decision 2);
//! the records it made stay readable as history (ADR-0027's amendment).
//!
//! The generator is deterministic (a pinned `SplitMix64` seed), so a given
//! build always produces the same series, the same content-derived
//! `data_version`s, and the same walk-forward folds — the property
//! `tests/fixture_history.rs` pins as constants.

use rust_decimal::Decimal;

use crate::domain::paper::fixture as synth;
use crate::domain::{
    Candle, Comparator, Condition, Direction, ExitRule, Pair, PriceField, RiskParams,
    SchemaVersion, Series, StrategyDsl, SweepableValue, ValueSource,
};

/// The fixture pair, straight from the generator (`"BTCUSDT"`).
pub const FIXTURE_PAIR: &str = synth::PAIR;

/// The fixture generator's seed, pinned alongside the data versions it
/// determines.
pub const FIXTURE_SEED: u64 = synth::SEED;

/// The FIXTURE strategy's name (tagged [`FIXTURE_STRATEGY_TAG`]), as the
/// retired seeder minted it (E3): it existed to keep the fixture's own
/// walk-forward certified, not to trade anything real.
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
        setup: None,
    }
}
