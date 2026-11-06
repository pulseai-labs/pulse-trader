//! `WaveTrend` adapter (schema 1.5.0, r6.s1.w2) — WT1, the oscillator's first
//! line, over hlc3.
//!
//! ```text
//! ap  = (high + low + close) / 3
//! esa = EMA(ap, channel)
//! d   = EMA(|ap − esa|, channel)
//! ci  = (ap − esa) / (0.015 · d)
//! wt1 = EMA(ci, average)
//! ```
//!
//! Each EMA is ta-rs's `ExponentialMovingAverage` (`α = 2 / (n + 1)`, seeded
//! with its first input), the same recursion the [`Ema`](super::ema::Ema)
//! adapter wraps. The three EMAs are chained, and each starts only once the
//! one before it is warm: `d` is fed from the bar `esa` is warm, `wt1` from the
//! bar `d` is warm. So the first value lands on candle index
//! `(channel − 1) + (channel − 1) + (average − 1) = 2·channel + average − 3`.
//!
//! **No value when `d = 0`.** If `d` is exactly zero, `ci` is undefined. The
//! bar returns `None` and does NOT feed `wt1` (`esa` and `d` have advanced;
//! `wt1` has not), so one flat stretch cannot desync the recursion. `d` is
//! zero only while every bar since `esa` warmed has equalled `esa`
//! (for example a constant series, or `channel = 1`, where `esa = ap`).
//!
//! **`is_ready()`** follows the port contract: `true` iff the NEXT
//! [`Indicator::next`] call returns `Some`. The one exception is a bar whose
//! `d` is exactly zero: readiness cannot know the next bar's `d`, so it assumes
//! `d ≠ 0`.
//!
//! **Floats stay behind the seam.** The hlc3 is formed in `Decimal` and crosses
//! `convert::decimal_to_f64` once; the output crosses
//! `convert::f64_to_decimal_rounded` once (8 places, half-even). A bar whose
//! hlc3 cannot be converted returns `None` before any state advances, and a
//! non-finite `ci` returns `None` before `wt1` is fed.

use crate::adapters::indicators::convert::{decimal_to_f64, f64_to_decimal_rounded};
use crate::domain::{Candle, Indicator};
use rust_decimal::Decimal;
use ta::Next;
use ta::indicators::ExponentialMovingAverage;

/// The constant in the `ci` denominator: `ci = (ap − esa) / (0.015 · d)`.
const CI_SCALE: f64 = 0.015;

/// `WaveTrend`'s WT1 over hlc3, wrapping three ta-rs EMAs behind the
/// [`Indicator`] port.
pub struct WaveTrend {
    /// `EMA(ap, channel)`.
    esa: ExponentialMovingAverage,
    /// `EMA(|ap − esa|, channel)`.
    d: ExponentialMovingAverage,
    /// `EMA(ci, average)`.
    wt1: ExponentialMovingAverage,
    channel: u32,
    average: u32,
    /// Values fed to `esa`.
    seen_esa: u32,
    /// Values fed to `d`.
    seen_d: u32,
    /// Values fed to `wt1`.
    seen_wt1: u32,
}

impl WaveTrend {
    /// Build WT1 with the given channel length (`n1`) and average length
    /// (`n2`). Returns `None` when either is 0.
    #[must_use]
    pub fn new(channel: u32, average: u32) -> Option<Self> {
        // ta-rs rejects a zero period; its error maps to `None` (panic-free).
        let esa = ExponentialMovingAverage::new(channel as usize).ok()?;
        let d = ExponentialMovingAverage::new(channel as usize).ok()?;
        let wt1 = ExponentialMovingAverage::new(average as usize).ok()?;
        Some(Self {
            esa,
            d,
            wt1,
            channel,
            average,
            seen_esa: 0,
            seen_d: 0,
            seen_wt1: 0,
        })
    }
}

/// `(high + low + close) / 3` in `Decimal`, as an `f64`. `None` on a decimal
/// overflow or a value `f64` cannot hold.
fn hlc3(candle: &Candle) -> Option<f64> {
    let sum = candle
        .high
        .checked_add(candle.low)?
        .checked_add(candle.close)?;
    decimal_to_f64(sum.checked_div(Decimal::from(3))?)
}

impl Indicator for WaveTrend {
    fn next(&mut self, candle: &Candle) -> Option<Decimal> {
        // Convert BEFORE any state advances: a non-representable price must
        // not desync the three EMAs and their counters (the EMA adapter's rule).
        let ap = hlc3(candle)?;

        let esa = self.esa.next(ap);
        self.seen_esa = self.seen_esa.saturating_add(1);
        if self.seen_esa < self.channel {
            return None;
        }

        let d = self.d.next((ap - esa).abs());
        self.seen_d = self.seen_d.saturating_add(1);
        if self.seen_d < self.channel {
            return None;
        }

        // `ci` has no value when `d` is exactly zero; `wt1` is not fed.
        if d == 0.0 {
            return None;
        }
        let ci = (ap - esa) / (CI_SCALE * d);
        if !ci.is_finite() {
            return None;
        }

        let wt1 = self.wt1.next(ci);
        self.seen_wt1 = self.seen_wt1.saturating_add(1);
        if self.seen_wt1 < self.average {
            return None;
        }
        f64_to_decimal_rounded(wt1)
    }

    fn is_ready(&self) -> bool {
        // The next call must warm `d` and bring `wt1` to `average` values
        // (assuming that bar's `d` is not exactly zero).
        self.seen_d.saturating_add(1) >= self.channel
            && self.seen_wt1.saturating_add(1) >= self.average
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::WaveTrend;
    use crate::domain::{Candle, Indicator};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    /// A candle whose hlc3 is exactly `ap`: high `ap + 1`, low `ap − 1`,
    /// close `ap` (the mean of the three is `ap`).
    fn candle_hlc3(ap: i64) -> Candle {
        let ap = Decimal::from(ap);
        Candle {
            open_time: 0,
            close_time: 0,
            open: ap,
            high: ap + Decimal::ONE,
            low: ap - Decimal::ONE,
            close: ap,
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    /// hlc3 values of the hand-built series (channel 3, average 4).
    const RISING: [i64; 10] = [10, 12, 11, 15, 14, 18, 17, 16, 19, 21];

    /// Six flat bars, then a ragged climb: `d` is exactly 0 on the flat bars.
    const FLAT_THEN_MOVING: [i64; 12] = [10, 10, 10, 10, 10, 10, 13, 12, 15, 14, 17, 16];

    fn run(channel: u32, average: u32, series: &[i64]) -> Vec<Option<Decimal>> {
        let mut wt = WaveTrend::new(channel, average).expect("lengths >= 1");
        series.iter().map(|ap| wt.next(&candle_hlc3(*ap))).collect()
    }

    #[test]
    fn wavetrend_warm_up_first_value_lands_on_index_two_channel_plus_average_minus_three() {
        // channel 3, average 4: 2·3 + 4 − 3 = 7.
        let got = run(3, 4, &RISING);
        for (index, value) in got.iter().enumerate() {
            assert_eq!(
                value.is_some(),
                index >= 7,
                "index {index}: the first value lands on index 7, got {value:?}"
            );
        }
        // channel 2, average 1: 2·2 + 1 − 3 = 2.
        let got = run(2, 1, &RISING);
        assert_eq!(got.iter().position(Option::is_some), Some(2));
        // channel 4, average 3: 2·4 + 3 − 3 = 8.
        let got = run(4, 3, &RISING);
        assert_eq!(got.iter().position(Option::is_some), Some(8));
    }

    #[test]
    fn wavetrend_is_ready_is_true_exactly_when_the_next_call_emits() {
        let mut wt = WaveTrend::new(3, 4).expect("lengths >= 1");
        for (index, ap) in RISING.iter().enumerate() {
            assert_eq!(
                wt.is_ready(),
                index >= 7,
                "index {index}: is_ready is true iff this next() call emits"
            );
            let value = wt.next(&candle_hlc3(*ap));
            assert_eq!(value.is_some(), index >= 7, "index {index}");
        }
        assert!(wt.is_ready(), "stays ready once warm");
    }

    #[test]
    fn wavetrend_hand_computed_series_matches_the_formula() {
        // Computed by an independent script from the spec's formula (ap = hlc3,
        // esa = EMA(ap, 3), d = EMA(|ap − esa|, 3), ci = (ap − esa) / (0.015·d),
        // wt1 = EMA(ci, 4)); α = 0.5 for n = 3 and 0.4 for n = 4.
        let got = run(3, 4, &RISING);
        let expected: Vec<Option<Decimal>> = vec![
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(dec("25.41176471")),
            Some(dec("52.17013575")),
            Some(dec("64.71204313")),
        ];
        assert_eq!(got, expected);
    }

    #[test]
    fn wavetrend_reads_hlc3_not_the_close() {
        // The same closes with a higher high on every other bar give a
        // different WT1 (a constant shift of every bar would not: WT1 is
        // shift-invariant).
        let mut narrow = WaveTrend::new(3, 4).expect("lengths >= 1");
        let mut skewed = WaveTrend::new(3, 4).expect("lengths >= 1");
        let (mut a, mut b) = (None, None);
        for (index, ap) in RISING.into_iter().enumerate() {
            a = narrow.next(&candle_hlc3(ap));
            let mut c = candle_hlc3(ap);
            if index % 2 == 1 {
                c.high += Decimal::from(5);
            }
            b = skewed.next(&c);
        }
        assert!(a.is_some() && b.is_some());
        assert_ne!(a, b, "the high feeds the value");
    }

    #[test]
    fn wavetrend_flat_candles_have_no_value_and_do_not_desync_later_values() {
        // Bars 0–5 are flat: esa = 10 and d = 0 exactly, so ci has no value
        // and wt1 is not fed. Bars 6–9 feed wt1 four times, so the first
        // value lands on index 9, not index 7.
        let got = run(3, 4, &FLAT_THEN_MOVING);
        let expected: Vec<Option<Decimal>> = vec![
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(dec("70.19180036")),
            Some(dec("79.8039691")),
            Some(dec("59.54904813")),
        ];
        assert_eq!(got, expected);
    }

    #[test]
    fn wavetrend_all_flat_series_never_has_a_value() {
        let got = run(3, 4, &[7; 30]);
        assert!(got.iter().all(Option::is_none), "d is 0 on every bar");
    }

    #[test]
    fn wavetrend_channel_one_never_has_a_value() {
        // EMA(1) is the input itself, so |ap − esa| = 0 and d = 0 on every bar.
        let got = run(1, 1, &RISING);
        assert!(got.iter().all(Option::is_none));
    }

    #[test]
    fn wavetrend_zero_lengths_are_refused() {
        assert!(WaveTrend::new(0, 12).is_none());
        assert!(WaveTrend::new(9, 0).is_none());
        assert!(WaveTrend::new(0, 0).is_none());
        assert!(WaveTrend::new(9, 12).is_some());
    }

    #[test]
    fn wavetrend_is_deterministic_across_repeated_runs() {
        let closes: Vec<i64> = (0..120).map(|i| 100 + (i * 7 % 13) - (i % 5)).collect();
        let first = run(9, 12, &closes);
        for _ in 0..5 {
            assert_eq!(run(9, 12, &closes), first, "repeated runs are identical");
        }
        assert!(first.iter().any(Option::is_some), "the series warms up");
    }
}
