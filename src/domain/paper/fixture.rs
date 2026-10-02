//! The certification fixture's synthetic BTCUSDT snapshot (r3.s4.w2, spec §3).
//!
//! E4's proof is that the *graduation path* works end to end on this build, not
//! that some window of market history happened to look good: a fixture strategy
//! run over a real window would be one lucky sample presented as an edge. So the
//! snapshot is synthetic and plainly labelled — a fixed-seed in-tree PRNG
//! ([`SplitMix64`], splitmix64) drives a mean-reverting price path, and the
//! strategy is a plain dip-buy whose exits bracket the reversion.
//!
//! Determinism is the point: same seed, same bars, same content version, on
//! every machine and every run — so the certification fixture's data versions
//! (pinned in `tests/certify_fixture.rs`) are a stable contract, not a value
//! that drifts with the generator's internals.
//!
//! The module is pure: it builds [`Candle`]s and nothing else. Stamping the
//! content versions needs [`crate::adapters::store::CandleStore`], which is the
//! application ring's job (`crate::application::fixture`).

use rust_decimal::Decimal;

use crate::domain::candle::Candle;

/// The fixture's pair. `BTCUSDT` only — the fixture is not parameterised.
pub(crate) const PAIR: &str = "BTCUSDT";

/// The fixture generator's seed. Changing it changes every data version, so it
/// is pinned alongside them.
pub(crate) const SEED: u64 = 0x5EED_1CE5_F1C7_2026;

/// M15 bars in the fixture snapshot (175 days of 15-minute bars).
pub(crate) const M15_BARS: usize = 16_800;

/// The first M15 bar's open time: 2025-01-01T00:00:00Z, which is 8-hour
/// aligned, so the funding cadence starts on the series' first bar.
pub(crate) const START_MS: i64 = 1_735_689_600_000;

/// One M15 bar, in milliseconds.
pub(crate) const M15_MS: i64 = 900_000;

/// `Binance`'s funding cadence: every 8 hours.
pub(crate) const FUNDING_EVERY_MS: i64 = 28_800_000;

/// M15 bars per H4 bar — the fixture's HTF is a plain aggregation. Four HOURS
/// is 4 × 60 min = 16 M15 bars (4 M15 bars would be an H1 grid, which the
/// walk-forward's HTF cadence check rightly refuses).
pub(crate) const H4_GROUP: usize = 16;

/// The price level the synthetic path oscillates around.
/// The synthetic series' central level, shared with `application::fixture`
/// (the DSL document is minted there, so the entry/exit thresholds and this
/// base cannot drift apart).
pub(crate) const BASE_PRICE: i64 = 60_000;

/// Half-width of the per-bar PRNG step, in price units (0.5% of the base).
const NOISE_HALF_WIDTH: i64 = 300;

/// Mean-reversion strength per bar, in permille: the next bar pulls 30% of the
/// way back toward the base.
const REVERSION_PERMILLE: i64 = 300;

/// Half-width of the bar's own high/low jitter, in price units.
const WICK_HALF_WIDTH: i64 = 40;

/// The funding rate stamped at every 8-hour boundary: `0.00001` (0.001%), the
/// smallest effect the engine's funding accrual can express. Real venues charge
/// more, but the fixture's job is to prove the *stamp-and-accrue* path, not to
/// model a carry trade.
const FUNDING_RATE_SCALED: i64 = 1;
const FUNDING_RATE_SCALE: u32 = 5;

/// splitmix64 (Steele et al.): the fixed-seed PRNG the fixture's noise comes
/// from. In-tree and tiny on purpose — a fixture that pulled `rand` would make
/// the data version depend on a dependency's stream stability.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Seed the stream.
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next 64 bits of the stream.
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform draw from `0..=span` (`span` inclusive), where `span` is
    /// non-negative.
    #[allow(clippy::cast_possible_wrap)]
    fn below_inclusive(&mut self, span: i64) -> i64 {
        let width = u64::try_from(span).unwrap_or(0).saturating_add(1);
        (self.next_u64() % width) as i64
    }
}

/// The M15 mid prices the fixture's bars are built from.
///
/// A mean-reverting (AR(1)) walk around [`BASE_PRICE`]: each step pulls
/// [`REVERSION_PERMILLE`] of the displacement back toward the base and adds a
/// PRNG innovation, so dips reliably revert — which is what gives the fixture
/// strategy a real, if plainly synthetic, edge.
#[allow(clippy::cast_possible_truncation)]
fn mid_prices() -> Vec<i64> {
    let mut rng = SplitMix64::new(SEED);
    let mut displacement: i64 = 0;
    let mut out = Vec::with_capacity(M15_BARS);
    for _ in 0..M15_BARS {
        out.push(BASE_PRICE + displacement);
        let innovation = rng.below_inclusive(2 * NOISE_HALF_WIDTH) - NOISE_HALF_WIDTH;
        displacement = displacement - REVERSION_PERMILLE * displacement / 1000 + innovation;
    }
    out
}

/// The fixture's M15 candles: one OHLCV(+funding) bar per mid price.
///
/// Bars are contiguous (`open_time` steps by exactly [`M15_MS`]) and carry a
/// funding rate on every 8-hour boundary, which is what the engine's funding
/// cadence check requires of a counted span.
#[must_use]
pub(crate) fn m15_candles() -> Vec<Candle> {
    let mids = mid_prices();
    let mut rng = SplitMix64::new(SEED ^ 0x5151_5151_5151_5151);
    let funding = Decimal::new(FUNDING_RATE_SCALED, FUNDING_RATE_SCALE);
    let mut candles = Vec::with_capacity(mids.len());
    let mut prev_close = Decimal::from(mids.first().copied().unwrap_or(BASE_PRICE));
    for (i, mid) in mids.iter().copied().enumerate() {
        let open_time = START_MS + i64::try_from(i).unwrap_or(0) * M15_MS;
        let close = Decimal::from(mid);
        let wick = Decimal::from(rng.below_inclusive(WICK_HALF_WIDTH));
        let high = prev_close.max(close) + wick;
        let low = prev_close.min(close) - wick;
        let volume = Decimal::from(50 + rng.below_inclusive(150));
        candles.push(Candle {
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
    candles
}

/// Aggregate M15 candles into the fixture's H4 candles, [`H4_GROUP`] at a time.
///
/// The group is exactly `H4_GROUP` wide; a trailing partial group is dropped,
/// which is why [`M15_BARS`] is a multiple of it. Each H4 bar takes the group's
/// first open, last close, extreme high/low, summed volume, and the group's
/// funding rate if any member carried one.
#[must_use]
pub(crate) fn h4_candles(m15: &[Candle]) -> Vec<Candle> {
    let mut out = Vec::with_capacity(m15.len() / H4_GROUP);
    for group in m15.as_chunks::<H4_GROUP>().0 {
        let first = &group[0];
        let last = &group[group.len() - 1];
        let mut high = first.high;
        let mut low = first.low;
        let mut volume = Decimal::ZERO;
        let mut funding_rate = None;
        for candle in group {
            high = high.max(candle.high);
            low = low.min(candle.low);
            volume += candle.volume;
            if funding_rate.is_none() {
                funding_rate = candle.funding_rate;
            }
        }
        out.push(Candle {
            open_time: first.open_time,
            close_time: last.close_time,
            open: first.open,
            high,
            low,
            close: last.close,
            volume,
            funding_rate,
        });
    }
    out
}

/// The fixture strategy's entry threshold, relative to [`BASE_PRICE`]: buy the
/// dip below it. Shared with the DSL document in [`crate::application::fixture`]
/// so the two cannot drift apart.
pub(crate) const ENTRY_OFFSET: i64 = 180;
