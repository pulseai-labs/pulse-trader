//! ADX adapter (Wilder `+DI`/`−DI`/ATR) — computed **in-adapter** behind the
//! domain [`Indicator`] port over `candle.high`/`low`/`close`.
//!
//! ADX is the one VS-1.1.3 indicator ta-rs v0.5.0 does **not** ship (it has only
//! `TrueRange` and `AverageTrueRange`), so it is built from scratch via Wilder's
//! method. The smoothing is a hand-rolled **Wilder RMA** with `α = 1/period`
//! (`S_t = S_{t−1} + (x_t − S_{t−1})/period`), seeded by the average of the first
//! `period` values. This is deliberately NOT ta-rs `AverageTrueRange`, which
//! smooths with an `ExponentialMovingAverage` (`α = 2/(period+1)`, a different
//! constant) and would diverge from a Wilder ADX by far more than epsilon.
//!
//! Warmup convention (mirrors 3.01's EMA `seen`-counter gating): ADX(p) needs
//! `2·period` candles — one `period` to seed the directional-index smoothing
//! (TR/+DM/−DM), a second `period` to seed the ADX smoothing of DX. The first
//! `2·period − 1` candles return `None`; the `2·period`-th candle is the first
//! `Some`. This count aligns with pandas-ta's first non-NaN ADX row (0-based
//! index `2·period − 1`), the alignment 3.04 cross-validation depends on.
//!
//! `f64` is confined to this adapter; inputs cross the `convert` seam from
//! `Decimal` and the final ADX is rounded back to scale-8 (half-even) for the
//! port.

use crate::adapters::indicators::convert::{decimal_to_f64, f64_to_decimal_rounded};
use crate::adapters::indicators::wilder::{WilderRma, true_range};
use crate::domain::{Candle, Indicator};
use rust_decimal::Decimal;

/// One-bar carry of the previous candle's high/low/close, in `f64`.
struct PrevBar {
    high: f64,
    low: f64,
    close: f64,
}

/// ADX(period) over candles, computed in-adapter via Wilder's method behind the
/// [`Indicator`] port. Emits `None` during warmup (`< 2·period` candles), then
/// `Some(adx)` rounded to scale-8.
pub struct Adx {
    period: u32,
    /// Previous candle's high/low/close (the one-bar state directional movement
    /// and true range need). `None` before the first candle.
    prev: Option<PrevBar>,
    /// Wilder RMA of true range → ATR.
    atr: WilderRma,
    /// Wilder RMA of `+DM` → smoothed `+DM`.
    smoothed_plus_dm: WilderRma,
    /// Wilder RMA of `−DM` → smoothed `−DM`.
    smoothed_minus_dm: WilderRma,
    /// Wilder RMA of `DX` → ADX (the emitted value).
    dx_smoother: WilderRma,
    /// Candles fed so far (warmup gate, mirrors EMA's `seen`).
    seen: u32,
}

impl Adx {
    /// Build an ADX over `period` candles. `period` must be ≥ 1.
    ///
    /// Returns `None` if `period` is 0 (a degenerate ADX). Constructed from a
    /// concrete `u32` — the `Fixed`-extraction factory that resolves
    /// `SweepableValue` periods is 3.03's concern. Panic-free (no
    /// `unwrap`/`expect`, both denied crate-wide).
    #[must_use]
    pub fn new(period: u32) -> Option<Self> {
        if period == 0 {
            return None;
        }
        Some(Self {
            period,
            prev: None,
            atr: WilderRma::new(period),
            smoothed_plus_dm: WilderRma::new(period),
            smoothed_minus_dm: WilderRma::new(period),
            dx_smoother: WilderRma::new(period),
            seen: 0,
        })
    }

    /// The candle count below which the adapter is still in warmup (`2·period`).
    /// The first `Some` is emitted on the `2·period`-th candle (0-based index
    /// `2·period − 1`), aligned to pandas-ta's first non-NaN ADX row.
    fn warmup(&self) -> u32 {
        self.period.saturating_mul(2)
    }

    /// Per-bar directional movement `(+DM, −DM)` from successive highs/lows.
    fn directional_movement(prev: &PrevBar, high: f64, low: f64) -> (f64, f64) {
        let up = high - prev.high;
        let down = prev.low - low;
        let pdm = if up > down && up > 0.0 { up } else { 0.0 };
        let ndm = if down > up && down > 0.0 { down } else { 0.0 };
        (pdm, ndm)
    }
}

impl Indicator for Adx {
    fn next(&mut self, candle: &Candle) -> Option<Decimal> {
        // Convert this bar's H/L/C across the seam BEFORE advancing the warmup
        // counter: a non-representable price (`None`) must not desync
        // `seen`/readiness from the indicator state (a phase shift in the
        // determinism layer). All three conversions must succeed before the bar
        // is counted/consumed.
        let high = decimal_to_f64(candle.high)?;
        let low = decimal_to_f64(candle.low)?;
        let close = decimal_to_f64(candle.close)?;

        self.seen = self.seen.saturating_add(1);

        // The first candle has no predecessor → no DM/TR; just record state.
        let Some(prev) = self.prev.as_ref() else {
            self.prev = Some(PrevBar { high, low, close });
            return None;
        };

        let (pdm, ndm) = Self::directional_movement(prev, high, low);
        let tr = true_range(high, low, prev.close);
        self.prev = Some(PrevBar { high, low, close });

        // Wilder-smooth TR, +DM, −DM. All three seed together (same period), so
        // they become `Some` on the same bar.
        let atr = self.atr.next(tr);
        let plus = self.smoothed_plus_dm.next(pdm);
        let minus = self.smoothed_minus_dm.next(ndm);

        let (Some(atr), Some(plus), Some(minus)) = (atr, plus, minus) else {
            return None;
        };

        // Directional indicators + DX. Guard the degenerate `ATR == 0` and
        // `+DI + −DI == 0` cases → DX = 0 (a flat market has no trend).
        let dx = if atr == 0.0 {
            0.0
        } else {
            let plus_di = 100.0 * plus / atr;
            let minus_di = 100.0 * minus / atr;
            let denom = plus_di + minus_di;
            if denom == 0.0 {
                0.0
            } else {
                100.0 * (plus_di - minus_di).abs() / denom
            }
        };

        // ADX = Wilder-smoothed DX. Suppress until the warmup count is reached
        // (defensive: warmup gate and the DX-RMA seed coincide at `2·period`).
        let adx = self.dx_smoother.next(dx)?;
        if self.seen < self.warmup() {
            return None;
        }
        f64_to_decimal_rounded(adx)
    }

    fn is_ready(&self) -> bool {
        // ready iff the NEXT call (the `seen + 1`-th candle) reaches the
        // `2·period`-th candle or beyond.
        self.seen.saturating_add(1) >= self.warmup()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::Adx;
    use crate::domain::{Candle, Indicator};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    /// Build an OHLC candle (volume/open/funding irrelevant to ADX).
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

    /// Independent, from-scratch Wilder reference ADX over an OHLC series, with
    /// `α = 1/period` (SMA-seeded), computed entirely in-test — the AC-6 oracle.
    /// `None` while warming. NOT read back from ta-rs `AverageTrueRange`.
    fn wilder_reference_adx(series: &[(f64, f64, f64)], period: u32) -> Vec<Option<f64>> {
        let n = series.len();
        let period_f = f64::from(period);
        // Per-bar TR / +DM / −DM (bar 0 has no predecessor → absent).
        let mut tr = vec![None; n];
        let mut pdm = vec![None; n];
        let mut ndm = vec![None; n];
        for t in 1..n {
            let (high, low, _close) = series[t];
            let (prev_high, prev_low, prev_close) = series[t - 1];
            let up = high - prev_high;
            let down = prev_low - low;
            tr[t] = Some(
                (high - low)
                    .max((high - prev_close).abs())
                    .max((low - prev_close).abs()),
            );
            pdm[t] = Some(if up > down && up > 0.0 { up } else { 0.0 });
            ndm[t] = Some(if down > up && down > 0.0 { down } else { 0.0 });
        }

        // Wilder RMA (SMA-seeded, α = 1/period) over a sparse series of values.
        let rma = |vals: &[Option<f64>]| -> Vec<Option<f64>> {
            let mut out = vec![None; vals.len()];
            let mut smoothed: Option<f64> = None;
            let mut seed_sum = 0.0;
            let mut count = 0u32;
            for (i, v) in vals.iter().enumerate() {
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
        };

        let atr = rma(&tr);
        let smooth_pos = rma(&pdm);
        let smooth_neg = rma(&ndm);

        // DX per bar (where ATR/DI are defined).
        let mut dx = vec![None; n];
        for i in 0..n {
            if let (Some(a), Some(p), Some(m)) = (atr[i], smooth_pos[i], smooth_neg[i]) {
                if a == 0.0 {
                    dx[i] = Some(0.0);
                    continue;
                }
                let plus_di = 100.0 * p / a;
                let minus_di = 100.0 * m / a;
                let denom = plus_di + minus_di;
                dx[i] = Some(if denom == 0.0 {
                    0.0
                } else {
                    100.0 * (plus_di - minus_di).abs() / denom
                });
            }
        }
        rma(&dx)
    }

    #[test]
    fn adx_returns_none_during_warmup_then_some() {
        // ADX(period) is `None` for the first `2·period − 1` candles and `Some`
        // on the `2·period`-th candle — pinned against pandas-ta's first non-NaN
        // ADX row (0-based index `2·period − 1`). Uses a non-degenerate series so
        // the first emission is genuinely a value, not a guard-zero.
        let period = 3u32;
        let warmup = 2 * period; // first Some on the `warmup`-th candle
        let mut adx = Adx::new(period).expect("period >= 1");

        // A trending series (highs and lows rising) so DX is non-trivial.
        let mut h = 10.0_f64;
        for candle_idx in 1..warmup {
            // Before feeding candle `candle_idx`, `seen == candle_idx - 1`, so
            // `is_ready()` (true iff the NEXT call emits) stays false throughout
            // warmup: it can only flip true once `seen >= warmup - 1`, which first
            // holds AFTER the final warmup candle (candle `warmup - 1`) is fed.
            assert!(
                !adx.is_ready(),
                "not ready before feeding warmup candle {candle_idx}"
            );
            let out = adx.next(&ohlc(h + 1.0, h - 1.0, h));
            assert_eq!(out, None, "candle {candle_idx} is warmup → None");
            h += 1.0;
        }
        // Having fed `warmup - 1` candles, the NEXT call is the `warmup`-th candle.
        assert!(adx.is_ready(), "ready right before the 2·period-th candle");

        let out = adx.next(&ohlc(h + 1.0, h - 1.0, h));
        assert!(out.is_some(), "the 2·period-th candle → Some");
        assert!(adx.is_ready(), "stays ready once warm");
    }

    #[test]
    fn adx_matches_reference_within_epsilon() {
        // Emitted ADX equals the independently hand-computed Wilder reference
        // (α = 1/period, SMA-seeded) within 1e-6 — the smoothing constant locked
        // by a real oracle, not a tautology and not ta-rs AverageTrueRange.
        let period = 2u32;
        let series = fixture();
        let reference = wilder_reference_adx(&series, period);

        let mut adx = Adx::new(period).expect("period >= 1");
        let mut emitted_count = 0;
        for (i, &(h, l, c)) in series.iter().enumerate() {
            let out = adx.next(&ohlc(h, l, c));
            match reference[i] {
                None => assert_eq!(out, None, "candle {} reference is None", i + 1),
                Some(expected) => {
                    let got: f64 = out
                        .expect("warm candle → Some")
                        .to_string()
                        .parse()
                        .unwrap();
                    assert!(
                        (got - expected).abs() < 1e-6,
                        "candle {}: got {got}, expected {expected}",
                        i + 1
                    );
                    emitted_count += 1;
                }
            }
        }
        assert!(
            emitted_count >= 2,
            "the fixture must exercise at least two emitted ADX values (got {emitted_count})"
        );
    }

    #[test]
    fn adx_deterministic_across_repeated_runs() {
        // NFR-2: a fresh `Adx` streamed twice over the same OHLC series yields
        // byte-identical `Vec<Option<Decimal>>` (exact `Decimal` equality, the
        // sub-epsilon `f64` jitter erased by the scale-8 conversion rule).
        let period = 3u32;
        let series = fixture();

        let run = || -> Vec<Option<Decimal>> {
            let mut adx = Adx::new(period).expect("period >= 1");
            series
                .iter()
                .map(|&(h, l, c)| adx.next(&ohlc(h, l, c)))
                .collect()
        };

        let first = run();
        let second = run();
        assert_eq!(
            first, second,
            "repeated runs yield identical Vec<Option<Decimal>> (NFR-2)"
        );
    }

    #[test]
    fn adx_new_rejects_zero_period() {
        // Panic-free degenerate handling: period 0 → None (no unwrap/expect).
        assert!(Adx::new(0).is_none(), "period 0 is a degenerate ADX → None");
        assert!(Adx::new(1).is_some(), "period 1 is constructible");
    }
}
