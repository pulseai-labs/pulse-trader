//! ATR adapter (Wilder) — Average True Range behind the domain [`Indicator`]
//! port over `candle.high`/`low`/`close`.
//!
//! **Smoothing convention (load-bearing for the r2.s2 cross-validation).**
//! True range is Wilder's three-term max — `max(high − low, |high − prev_close|,
//! |low − prev_close|)` — smoothed with the shared [`WilderRma`] core
//! (`α = 1/period`, SMA-seeded), the same recursion ADX uses. This is
//! deliberately NOT ta-rs `AverageTrueRange`, which smooths with an
//! `ExponentialMovingAverage` (`α = 2/(period+1)`). pandas-ta-classic's
//! `atr(mamode="rma")` SMA-seeds identically, so the cross-validation measured
//! **zero** settling bars — agreement to ~1e-12 straight off the warmup edge.
//!
//! **Warmup convention.** The first candle has no predecessor close, so it
//! produces no TR at all. ATR(p) needs `p` real TRs to seed the RMA, so the
//! first `Some` lands on candle index `period` (the `period + 1`-th candle),
//! and the generator blanks the pandas-ta reference's rows before it — the
//! alignment the cross-validation asserts.
//!
//! `f64` is confined to this adapter; inputs cross the `convert` seam from
//! `Decimal` and the smoothed ATR is rounded back to scale-8 (half-even) for
//! the port.

use crate::adapters::indicators::convert::{decimal_to_f64, f64_to_decimal_rounded};
use crate::adapters::indicators::wilder::{WilderRma, true_range};
use crate::domain::{Candle, Indicator};
use rust_decimal::Decimal;

/// ATR(period) over candles, computed via the shared Wilder RMA behind the
/// [`Indicator`] port. Emits `None` through candle index `period − 1`, then
/// `Some(atr)` rounded to scale-8.
pub struct Atr {
    /// Previous candle's high/low/close (the one-bar state true range needs).
    /// `None` before the first candle — the first bar contributes no TR.
    prev_close: Option<f64>,
    /// Wilder RMA of the true range → ATR (the emitted value).
    rma: WilderRma,
    /// Candles fed so far (warmup gate, mirrors the other adapters' `seen`).
    seen: u32,
    /// The candle index on which the first `Some` is emitted (`period`).
    warmup: u32,
}

impl Atr {
    /// Build an ATR over `period` candles. `period` must be ≥ 1.
    ///
    /// Returns `None` if `period` is 0 (a degenerate ATR). Constructed from a
    /// concrete `u32` (the `Fixed`-extraction factory that resolves
    /// `SweepableValue` periods is the engine's concern). Panic-free.
    #[must_use]
    pub fn new(period: u32) -> Option<Self> {
        if period == 0 {
            return None;
        }
        Some(Self {
            prev_close: None,
            rma: WilderRma::new(period),
            seen: 0,
            warmup: period,
        })
    }
}

impl Indicator for Atr {
    fn next(&mut self, candle: &Candle) -> Option<Decimal> {
        // Convert across the seam BEFORE advancing the warmup counter: a
        // non-representable price (`None`) must not desync `seen`/readiness
        // from the smoothing state (the `convert`/`rsi` precedent — a phase
        // shift in the determinism layer).
        let high = decimal_to_f64(candle.high)?;
        let low = decimal_to_f64(candle.low)?;
        let close = decimal_to_f64(candle.close)?;

        self.seen = self.seen.saturating_add(1);

        // The first candle has no predecessor close → no TR; record state and
        // stay silent. From the second bar on, TR is the Wilder three-term max.
        let Some(prev_close) = self.prev_close else {
            self.prev_close = Some(close);
            return None;
        };
        let tr = true_range(high, low, prev_close);
        self.prev_close = Some(close);

        let atr = self.rma.next(tr)?;
        // Defensive: the RMA emits its seed on the `period`-th real TR, which
        // lands exactly on candle index `period` — the gate is belt-and-braces
        // against a future state reorder.
        if self.seen <= self.warmup {
            return None;
        }
        f64_to_decimal_rounded(atr)
    }

    fn is_ready(&self) -> bool {
        // ready iff the NEXT call (the `seen + 1`-th candle) reaches candle
        // index `period` — the bar that emits the first seeded ATR.
        self.seen.saturating_add(1) > self.warmup
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::Atr;
    use crate::domain::{Candle, Indicator};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    /// Build an OHLC candle (volume/open/funding irrelevant to ATR).
    fn ohlc(high: f64, low: f64, close: f64) -> Candle {
        let to_d = |x: f64| Decimal::from_str(&format!("{x}")).unwrap();
        Candle {
            open_time: 0,
            close_time: 0,
            open: to_d(close),
            high: to_d(high),
            low: to_d(low),
            close: to_d(close),
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    /// A small fixed OHLC series exercised by the value + determinism tests.
    fn fixture() -> Vec<(f64, f64, f64)> {
        vec![
            (10.0, 9.0, 9.5),
            (10.5, 9.2, 10.2),
            (11.0, 9.8, 10.7),
            (10.8, 9.5, 9.7),
            (11.5, 10.0, 11.2),
            (12.0, 10.5, 11.0),
            (11.2, 9.9, 10.1),
            (12.5, 10.8, 12.2),
        ]
    }

    /// Independent Wilder ATR reference over the OHLC series: TR absent on bar
    /// 0, SMA-seeded RMA (`α = 1/period`) over the TRs, first emission at index
    /// `period` — computed entirely in-test, the adapter's own oracle.
    fn wilder_reference_atr(series: &[(f64, f64, f64)], period: u32) -> Vec<Option<f64>> {
        let n = series.len();
        let period_f = f64::from(period);
        let mut tr = vec![None; n];
        for t in 1..n {
            let (high, low, _) = series[t];
            let (_, _, prev_close) = series[t - 1];
            tr[t] = Some(
                (high - low)
                    .max((high - prev_close).abs())
                    .max((low - prev_close).abs()),
            );
        }
        let mut out = vec![None; n];
        let mut smoothed: Option<f64> = None;
        let mut seed_sum = 0.0;
        let mut count = 0u32;
        for (i, v) in tr.iter().enumerate() {
            let Some(x) = v else { continue };
            count += 1;
            if let Some(prev) = smoothed {
                let updated = prev + (x - prev) / period_f;
                smoothed = Some(updated);
                out[i] = Some(updated);
            } else {
                seed_sum += x;
                if count == period {
                    let seed_val = seed_sum / period_f;
                    smoothed = Some(seed_val);
                    out[i] = Some(seed_val);
                }
            }
        }
        out
    }

    #[test]
    fn atr_returns_none_during_warmup_then_some() {
        // ATR(period) is `None` through candle index `period − 1` and `Some` on
        // index `period` — the `period`-th real TR seeds the RMA.
        let period = 3u32;
        let mut atr = Atr::new(period).expect("period >= 1");
        let mut h = 10.0_f64;
        for candle_idx in 0..period {
            assert!(!atr.is_ready(), "not ready before candle {candle_idx}");
            let out = atr.next(&ohlc(h + 1.0, h - 1.0, h));
            assert_eq!(out, None, "candle {candle_idx} is warmup → None");
            h += 1.0;
        }
        assert!(atr.is_ready(), "ready right before candle index `period`");
        let out = atr.next(&ohlc(h + 1.0, h - 1.0, h));
        assert!(out.is_some(), "candle index `period` → Some");
        assert!(atr.is_ready(), "stays ready once warm");
    }

    #[test]
    fn atr_matches_reference_within_epsilon() {
        let period = 3u32;
        let series = fixture();
        let reference = wilder_reference_atr(&series, period);

        let mut atr = Atr::new(period).expect("period >= 1");
        for (i, &(h, l, c)) in series.iter().enumerate() {
            let out = atr.next(&ohlc(h, l, c));
            match reference[i] {
                None => assert_eq!(out, None, "candle {i} reference is None"),
                Some(expected) => {
                    let got: f64 = out
                        .expect("warm candle → Some")
                        .to_string()
                        .parse()
                        .unwrap();
                    assert!(
                        (got - expected).abs() < 1e-6,
                        "candle {i}: got {got}, expected {expected}"
                    );
                }
            }
        }
    }

    #[test]
    fn atr_deterministic_across_repeated_runs() {
        let period = 3u32;
        let series = fixture();
        let run = || -> Vec<Option<Decimal>> {
            let mut atr = Atr::new(period).expect("period >= 1");
            series
                .iter()
                .map(|&(h, l, c)| atr.next(&ohlc(h, l, c)))
                .collect()
        };
        assert_eq!(run(), run(), "repeated runs identical (NFR-2)");
    }

    #[test]
    fn atr_new_rejects_zero_period() {
        assert!(Atr::new(0).is_none(), "period 0 is degenerate → None");
        assert!(Atr::new(1).is_some(), "period 1 is constructible");
    }
}
