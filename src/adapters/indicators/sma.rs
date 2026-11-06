//! SMA adapter (schema 1.5.0, r6.s1.w1) — the simple moving average of one
//! price field over the last `period` bars, the current bar included.
//!
//! **Warm-up.** The first value lands on candle index `period − 1` (the
//! `period`-th candle). `is_ready()` follows the port contract: it is `true`
//! iff the NEXT [`Indicator::next`] call returns `Some`.
//!
//! **No f64 anywhere.** The window keeps [`Decimal`]s straight off the candles
//! and the running sum is exact (candle prices carry at most 8 decimal places,
//! far inside `Decimal`'s 28 digits), so the sum never drifts. Only the final
//! division rounds, to 8 decimal places half-even, the `convert` seam's scale.
//! Every step is checked: an overflow returns `None` and leaves the window and
//! the sum exactly as they were (the EMA adapter's "convert before advancing"
//! rule), so one bad bar cannot desync the indicator.

use crate::domain::{Candle, Indicator, PriceField};
use rust_decimal::{Decimal, RoundingStrategy};
use std::collections::VecDeque;

/// Decimal places of an SMA value (the `convert` seam's indicator scale).
const SMA_SCALE: u32 = 8;

/// SMA(period) over one price field.
pub struct Sma {
    period: u32,
    source: PriceField,
    /// The last `period` source values, oldest first.
    window: VecDeque<Decimal>,
    /// The exact sum of `window`.
    sum: Decimal,
}

impl Sma {
    /// Build an SMA over `period` bars of `source`. Returns `None` for
    /// `period == 0`.
    #[must_use]
    pub fn new(period: u32, source: PriceField) -> Option<Self> {
        if period == 0 {
            return None;
        }
        Some(Self {
            period,
            source,
            // Not pre-sized: a huge `period` must not allocate up front.
            window: VecDeque::new(),
            sum: Decimal::ZERO,
        })
    }

    fn value_of(&self, candle: &Candle) -> Decimal {
        match self.source {
            PriceField::Open => candle.open,
            PriceField::High => candle.high,
            PriceField::Low => candle.low,
            PriceField::Close => candle.close,
            PriceField::Volume => candle.volume,
        }
    }

    /// Bars in the window, as a `u32` (it never exceeds `period`).
    fn len(&self) -> u32 {
        u32::try_from(self.window.len()).unwrap_or(u32::MAX)
    }
}

impl Indicator for Sma {
    fn next(&mut self, candle: &Candle) -> Option<Decimal> {
        let value = self.value_of(candle);
        // Work out the new sum and the output first; touch the state only when
        // every step succeeded.
        let mut sum = self.sum.checked_add(value)?;
        let evicted = if self.len() >= self.period {
            self.window.front().copied()
        } else {
            None
        };
        if let Some(oldest) = evicted {
            sum = sum.checked_sub(oldest)?;
        }
        // After this bar the window holds `period` values iff it held
        // `period − 1` (or already `period`, when the oldest was evicted).
        let out = if self.is_ready() {
            Some(
                sum.checked_div(Decimal::from(self.period))?
                    .round_dp_with_strategy(SMA_SCALE, RoundingStrategy::MidpointNearestEven),
            )
        } else {
            None
        };
        if evicted.is_some() {
            self.window.pop_front();
        }
        self.window.push_back(value);
        self.sum = sum;
        out
    }

    fn is_ready(&self) -> bool {
        // The next call fills (or keeps full) the window.
        self.len().saturating_add(1) >= self.period
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::Sma;
    use crate::domain::{Candle, Indicator, PriceField};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn candle_close(close: &str) -> Candle {
        let c = dec(close);
        Candle {
            open_time: 0,
            close_time: 0,
            open: c,
            high: c,
            low: c,
            close: c,
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    fn candle_ohlcv(open: &str, high: &str, low: &str, close: &str, volume: &str) -> Candle {
        Candle {
            open_time: 0,
            close_time: 0,
            open: dec(open),
            high: dec(high),
            low: dec(low),
            close: dec(close),
            volume: dec(volume),
            funding_rate: None,
        }
    }

    #[test]
    fn sma_warm_up_first_value_lands_on_index_period_minus_one() {
        let mut sma = Sma::new(3, PriceField::Close).expect("period >= 1");
        assert!(!sma.is_ready(), "no bar fed: the next call is bar 1 of 3");
        assert_eq!(sma.next(&candle_close("10")), None, "index 0 is warm-up");
        assert!(!sma.is_ready(), "one bar fed: the next call is bar 2 of 3");
        assert_eq!(sma.next(&candle_close("20")), None, "index 1 is warm-up");
        assert!(sma.is_ready(), "two bars fed: the next call is the 3rd");
        assert_eq!(
            sma.next(&candle_close("30")),
            Some(dec("20")),
            "index period - 1 = 2 yields the first mean"
        );
        assert!(sma.is_ready(), "stays ready once warm");
    }

    #[test]
    fn sma_hand_computed_mean_slides_over_the_window() {
        let mut sma = Sma::new(3, PriceField::Close).expect("period >= 1");
        let got: Vec<Option<Decimal>> = ["1", "2", "3", "4", "5", "11"]
            .iter()
            .map(|c| sma.next(&candle_close(c)))
            .collect();
        assert_eq!(
            got,
            vec![
                None,
                None,
                Some(dec("2")),          // (1+2+3)/3
                Some(dec("3")),          // (2+3+4)/3
                Some(dec("4")),          // (3+4+5)/3
                Some(dec("6.66666667")), // (4+5+11)/3 = 20/3, 8 places
            ]
        );
    }

    #[test]
    fn sma_rounds_to_eight_places_half_even() {
        // (1 + 1 + 2) / 3 = 1.333…
        let mut sma = Sma::new(3, PriceField::Close).expect("period >= 1");
        sma.next(&candle_close("1"));
        sma.next(&candle_close("1"));
        assert_eq!(sma.next(&candle_close("2")), Some(dec("1.33333333")));

        // (0.00000001 + 0.00000002) / 2 = 0.000000015 → half-even → 0.00000002
        let mut sma = Sma::new(2, PriceField::Close).expect("period >= 1");
        sma.next(&candle_close("0.00000001"));
        assert_eq!(
            sma.next(&candle_close("0.00000002")),
            Some(dec("0.00000002"))
        );
        // (0.00000002 + 0.00000003) / 2 = 0.000000025 → half-even → 0.00000002
        assert_eq!(
            sma.next(&candle_close("0.00000003")),
            Some(dec("0.00000002"))
        );
    }

    #[test]
    fn sma_reads_the_requested_source_field() {
        let bars = [
            candle_ohlcv("1", "10", "0.5", "2", "100"),
            candle_ohlcv("3", "20", "1.5", "4", "300"),
        ];
        let cases = [
            (PriceField::Open, "2"),
            (PriceField::High, "15"),
            (PriceField::Low, "1"),
            (PriceField::Close, "3"),
            (PriceField::Volume, "200"),
        ];
        for (field, expected) in cases {
            let mut sma = Sma::new(2, field).expect("period >= 1");
            assert_eq!(sma.next(&bars[0]), None, "{field:?}: warm-up");
            assert_eq!(sma.next(&bars[1]), Some(dec(expected)), "{field:?}");
        }
    }

    #[test]
    fn sma_period_zero_is_refused() {
        assert!(Sma::new(0, PriceField::Close).is_none());
    }

    #[test]
    fn sma_period_one_is_the_source_itself() {
        let mut sma = Sma::new(1, PriceField::Close).expect("period >= 1");
        assert!(
            sma.is_ready(),
            "period 1: the very next call yields a value"
        );
        assert_eq!(sma.next(&candle_close("7.25")), Some(dec("7.25")));
        assert_eq!(sma.next(&candle_close("8")), Some(dec("8")));
    }

    #[test]
    fn sma_overflow_returns_none_without_advancing_the_window() {
        let mut sma = Sma::new(2, PriceField::Close).expect("period >= 1");
        let max = Decimal::MAX;
        let huge = Candle {
            close: max,
            ..candle_close("0")
        };
        assert_eq!(sma.next(&huge), None, "warm-up");
        // MAX + MAX overflows: no value, and the window must stay [MAX].
        assert_eq!(sma.next(&huge), None, "overflow gives no value");
        // Had the failed call advanced the window, this would overflow again.
        assert_eq!(
            sma.next(&candle_close("0")),
            Some(max / Decimal::TWO),
            "window is [MAX, 0] — the failed call left no trace"
        );
    }

    #[test]
    fn sma_is_deterministic_across_repeated_runs() {
        let closes = [
            "100.123456789",
            "101.5",
            "99.987654321",
            "102.25",
            "103.0",
            "101.75",
        ];
        let run = || -> Vec<Option<Decimal>> {
            let mut sma = Sma::new(4, PriceField::Close).expect("period >= 1");
            closes.iter().map(|c| sma.next(&candle_close(c))).collect()
        };
        assert_eq!(run(), run(), "repeated runs yield identical output");
    }
}
