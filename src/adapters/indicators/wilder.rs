//! Wilder RMA — the shared smoothing core behind the Wilder-family adapters.
//!
//! r2.s2.w2 extracted this from `adx.rs` so `atr.rs` can reuse the identical
//! recursion rather than grow a second copy. A **Wilder RMA** uses
//! `α = 1/period` (`S_t = S_{t−1} + (x_t − S_{t−1})/period`), seeded by the
//! simple average of the first `period` values — deliberately NOT the ta-rs /
//! pandas `ewm` recursion (`α = 2/(period+1)`, seeded with the first value),
//! which is a different smoothing constant and a different seed.
//!
//! `f64` is confined to the `adapters::indicators` module; this file is pure
//! `f64` state + `+ - * /` only (the determinism guard scans it).

/// A hand-rolled **Wilder RMA** (`α = 1/period`), seeded by the simple average
/// of the first `period` values, then `S_t = S_{t−1} + (x_t − S_{t−1})/period`.
///
/// Returns `None` until it has accumulated `period` real values; from the
/// `period`-th value onward it returns `Some(smoothed)`. This is the Wilder
/// smoothing constant (NOT the EMA `α = 2/(period+1)` ta-rs uses), shared by
/// `ATR`, `S+DM`, `S−DM`, and the `DX → ADX` step.
pub(crate) struct WilderRma {
    period: u32,
    /// Running smoothed value once seeded.
    smoothed: Option<f64>,
    /// Sum of the first `period` values while still seeding.
    seed_sum: f64,
    /// Count of real values fed so far.
    seen: u32,
}

impl WilderRma {
    pub(crate) fn new(period: u32) -> Self {
        Self {
            period,
            smoothed: None,
            seed_sum: 0.0,
            seen: 0,
        }
    }

    /// Feed one real value; returns the current smoothed value, or `None` while
    /// still seeding (before `period` values have accrued).
    pub(crate) fn next(&mut self, value: f64) -> Option<f64> {
        self.seen = self.seen.saturating_add(1);
        if let Some(prev) = self.smoothed {
            let updated = prev + (value - prev) / f64::from(self.period);
            self.smoothed = Some(updated);
            Some(updated)
        } else {
            self.seed_sum += value;
            if self.seen < self.period {
                None
            } else {
                // `period`-th value: seed with the simple average.
                let seed = self.seed_sum / f64::from(self.period);
                self.smoothed = Some(seed);
                Some(seed)
            }
        }
    }
}

/// Wilder's three-term true range: `max(high − low, |high − prev_close|,
/// |low − prev_close|)`. Shared by the ATR adapter (directly) and ADX (its TR
/// leg); pure `f64` `-`/`abs`/`max` — no banned call-forms.
pub(crate) fn true_range(high: f64, low: f64, prev_close: f64) -> f64 {
    (high - low)
        .max((high - prev_close).abs())
        .max((low - prev_close).abs())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::WilderRma;

    #[test]
    fn seeds_with_the_sma_of_the_first_period_values() {
        // period = 3 over [2, 4, 6, 8]: seed at the 3rd value = (2+4+6)/3 = 4,
        // then S = 4 + (8 − 4)/3 = 16/3.
        let mut rma = WilderRma::new(3);
        assert_eq!(rma.next(2.0), None);
        assert_eq!(rma.next(4.0), None);
        assert_eq!(rma.next(6.0), Some(4.0));
        let next = rma.next(8.0).expect("seeded → Some");
        assert!((next - 16.0 / 3.0).abs() < 1e-12, "got {next}");
    }

    #[test]
    fn is_deterministic_across_repeated_runs() {
        let values = [1.0_f64, 3.0, 2.0, 5.0, 4.0, 8.0, 7.0, 6.0];
        let run = || {
            let mut rma = WilderRma::new(3);
            values.iter().map(|&v| rma.next(v)).collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }
}
