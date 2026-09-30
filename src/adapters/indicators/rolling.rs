//! Rolling-extremes adapters — `Highest`/`Lowest` over the **N closed bars
//! before the current bar**, excluding it (schema 1.2.0, r3.s2 — Q1, the
//! Donchian prior-N convention).
//!
//! **Warmup convention (load-bearing for the r3.s2 cross-validation).** The
//! window holds the last N values from PREVIOUS calls; `next` returns the
//! window's max (or min) BEFORE pushing the current candle's value. So the
//! first `Some` lands on candle index N — the (N+1)-th candle — which is Q1's
//! N+1 warm-up, and `is_ready()` is true exactly when the NEXT call will
//! return `Some`.
//!
//! **No f64 anywhere.** A windowed price is an exact price: the window keeps
//! [`Decimal`]s straight off the candles (no `convert` seam, no rounding —
//! the value is one of the inputs, not a derived mean). Ties need no special
//! handling for a max or min.

use crate::domain::{Candle, Indicator, PriceField};
use rust_decimal::Decimal;
use std::collections::VecDeque;

/// Which extreme a [`RollingExtremes`] slot computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Extreme {
    /// The window maximum (`Highest`).
    Max,
    /// The window minimum (`Lowest`).
    Min,
}

/// Rolling highest/lowest of one price field over the prior N closed bars —
/// the shared engine behind the [`IndicatorSpec::Highest`] and
/// [`IndicatorSpec::Lowest`] domain variants.
///
/// Constructed from a concrete `u32` and a [`PriceField`] (the
/// `Fixed`-extraction factory that resolves `SweepableValue` periods is the
/// engine's concern). Panic-free.
pub struct RollingExtremes {
    /// The window: the last N `source` values from PREVIOUS calls, oldest
    /// first.
    window: VecDeque<Decimal>,
    /// Which price field the window aggregates.
    source: PriceField,
    /// Which extreme to compute.
    extreme: Extreme,
    /// The window size N.
    period: u32,
    /// Candles fed so far (warmup gate, mirrors the other adapters' `seen`).
    seen: u32,
}

impl RollingExtremes {
    /// Build a rolling extreme over `period` candles of `source`. `period`
    /// must be ≥ 1; returns `None` on 0 (a degenerate window).
    ///
    /// The window grows LAZILY — one value per candle fed, never `period`
    /// values up front. `pulse serve` accepts raw DSL over MCP from any
    /// authenticated client, so an absurd `period` must not reserve memory for
    /// a window that will never fill: it simply never warms (r3.s2 round-2
    /// fix, D2 — an eager `with_capacity(period)` aborted the process on
    /// allocation failure).
    #[must_use]
    pub fn new(period: u32, source: PriceField, highest: bool) -> Option<Self> {
        if period == 0 {
            return None;
        }
        Some(Self {
            window: VecDeque::new(),
            source,
            extreme: if highest { Extreme::Max } else { Extreme::Min },
            period,
            seen: 0,
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

    fn fold(&self, window: &VecDeque<Decimal>) -> Decimal {
        let mut iter = window.iter().copied();
        let first = iter.next().unwrap_or_default();
        iter.fold(first, |acc, v| match self.extreme {
            Extreme::Max => acc.max(v),
            Extreme::Min => acc.min(v),
        })
    }
}

impl Indicator for RollingExtremes {
    fn next(&mut self, candle: &Candle) -> Option<Decimal> {
        // Read BEFORE advancing any state: the returned value is the prior
        // window's extreme — the current candle contributes nothing to its
        // own value (Q1: excluding the current bar).
        let result = if self.window.len() < self.period as usize {
            None
        } else {
            Some(self.fold(&self.window))
        };

        // Then push the current value and keep the window at N values.
        self.seen = self.seen.saturating_add(1);
        self.window.push_back(self.value_of(candle));
        while self.window.len() > self.period as usize {
            self.window.pop_front();
        }

        result
    }

    fn is_ready(&self) -> bool {
        // Ready iff the NEXT call will return Some: the window already holds
        // N values (it fills after the N-th candle; the (N+1)-th call emits).
        self.window.len() >= self.period as usize
    }
}

// `Highest(period, source)` and `Lowest(period, source)` — the engine's
// factory arm constructs the slot with the `highest` flag selecting the
// extreme (schema 1.2.0, r3.s2 — Q1).

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::RollingExtremes;
    use crate::domain::{Candle, Indicator, PriceField};
    use rust_decimal::Decimal;

    fn candle(idx: i64, high: i64, low: i64, close: i64) -> Candle {
        Candle {
            open_time: idx * 60_000,
            close_time: idx * 60_000 + 59_999,
            open: dec(close),
            high: dec(high),
            low: dec(low),
            close: dec(close),
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    fn dec(v: i64) -> Decimal {
        Decimal::from(v)
    }

    const HIGHS: [i64; 6] = [10, 12, 11, 15, 9, 13];
    const LOWS: [i64; 6] = [8, 9, 7, 11, 5, 9];

    #[test]
    fn highest_emits_first_at_index_n_and_excludes_the_current_bar() {
        let mut highest = RollingExtremes::new(3, PriceField::High, true).expect("period >= 1");
        for (idx, high) in HIGHS.iter().enumerate() {
            let idx64 = i64::try_from(idx).expect("small index");
            let out = highest.next(&candle(idx64, *high, *high - 1, *high - 1));
            if idx < 3 {
                assert_eq!(out, None, "bar {idx}: warm-up → None");
            } else {
                let expected = HIGHS[idx - 3..idx].iter().max().expect("non-empty");
                assert_eq!(out, Some(dec(*expected)), "bar {idx}: prior-3 max");
            }
        }
    }

    /// r3.s2 round-2 fix (D2): a period far larger than any series must not
    /// pre-allocate. `pulse serve` accepts raw DSL over MCP from any
    /// authenticated client, so `highest(high, 4294967295)` used to reserve
    /// tens of gigabytes and abort the always-on process on allocation
    /// failure. The window now grows with the candles fed, so an absurd N
    /// simply never warms and the adapter stays silent.
    #[test]
    fn u32_max_period_does_not_abort_and_never_warms() {
        let mut extreme =
            RollingExtremes::new(u32::MAX, PriceField::High, true).expect("period >= 1");
        assert!(!extreme.is_ready(), "an absurd period is never ready");
        for idx in 0_i64..8 {
            assert_eq!(
                extreme.next(&candle(idx, 10 + idx, 9, 10)),
                None,
                "bar {idx}: a window that can never fill has no value"
            );
        }
        assert!(!extreme.is_ready(), "still never ready after stepping");
        // The regression itself: the reservation follows the candles seen, not
        // the period (a `with_capacity(u32::MAX)` here aborts the test process).
        assert!(
            extreme.window.capacity() < 1024,
            "the window must not pre-allocate for the period, capacity={}",
            extreme.window.capacity()
        );
    }

    #[test]
    fn lowest_mirrors() {
        let mut lowest = RollingExtremes::new(3, PriceField::Low, false).expect("period >= 1");
        for (idx, low) in LOWS.iter().enumerate() {
            let idx64 = i64::try_from(idx).expect("small index");
            let out = lowest.next(&candle(idx64, *low + 1, *low, *low + 1));
            if idx < 3 {
                assert_eq!(out, None, "bar {idx}: warm-up → None");
            } else {
                let expected = LOWS[idx - 3..idx].iter().min().expect("non-empty");
                assert_eq!(out, Some(dec(*expected)), "bar {idx}: prior-3 min");
            }
        }
    }

    #[test]
    fn is_ready_is_true_when_the_next_call_emits() {
        let mut highest = RollingExtremes::new(3, PriceField::High, true).expect("period >= 1");
        for (idx, high) in HIGHS.iter().take(3).enumerate() {
            assert!(!highest.is_ready(), "bar {idx}: not yet ready");
            let idx64 = i64::try_from(idx).expect("small index");
            highest.next(&candle(idx64, *high, *high - 1, *high - 1));
        }
        assert!(
            highest.is_ready(),
            "the window holds 3 values → the next call emits"
        );
        let out = highest.next(&candle(3, HIGHS[3], HIGHS[3] - 1, HIGHS[3] - 1));
        assert!(out.is_some(), "the first emission lands on candle index N");
    }

    #[test]
    fn reads_the_requested_source_field() {
        // Volume as the source: the window aggregates volumes.
        let mut first = candle(0, 10, 9, 9);
        first.volume = dec(5);
        let mut second = candle(1, 10, 9, 9);
        second.volume = dec(7);
        let mut third = candle(2, 10, 9, 9);
        third.volume = dec(6);

        let mut highest = RollingExtremes::new(2, PriceField::Volume, true).expect("period >= 1");
        assert_eq!(highest.next(&first), None);
        assert_eq!(highest.next(&second), None);
        // The window holds the first two volumes [5, 7]; their max is 7.
        assert_eq!(highest.next(&third), Some(dec(7)), "prior-2 volume max");
    }

    #[test]
    fn rejects_zero_period() {
        assert!(RollingExtremes::new(0, PriceField::High, true).is_none());
        assert!(RollingExtremes::new(0, PriceField::Low, false).is_none());
        assert!(RollingExtremes::new(1, PriceField::High, true).is_some());
    }

    #[test]
    fn deterministic_across_repeated_runs() {
        let run = || -> Vec<Option<Decimal>> {
            let mut highest = RollingExtremes::new(3, PriceField::High, true).expect("period >= 1");
            HIGHS
                .iter()
                .enumerate()
                .map(|(idx, high)| {
                    let idx = i64::try_from(idx).expect("small index");
                    highest.next(&candle(idx, *high, *high - 1, *high - 1))
                })
                .collect()
        };
        assert_eq!(run(), run(), "repeated runs identical (NFR-2)");
    }
}
