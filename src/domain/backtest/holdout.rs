//! The C1 holdout test (r4.s1.w3, ADR-0028): a candidate passes its holdout
//! when the **one-sided lower confidence bound** of its holdout expectancy is
//! strictly above zero, at a family-wise 5% split over the hypothesis budget
//! `H` — `z = z(1 − 0.05/H)` (≈ 2.64 at H = 12). Measuring the test's power is
//! this item's job; the certification step that USES it is w5.
//!
//! The mean and the variance accumulate exactly as [`FoldVerdict::from_rs`]
//! does — `Decimal` up to one `var/n` conversion to `f64`, one `sqrt` (the
//! `stats.rs` quarantine). The quantile is computed in-crate by a documented
//! inverse-normal approximation (no new crate).
//!
//! [`FoldVerdict::from_rs`]: super::FoldVerdict::from_rs

use rust_decimal::Decimal;

use super::stats::decimal_to_f64;

/// The C1 test's family-wise alpha: the hypothesis budget `H` splits 0.05
/// across every attempt, so each holdout is judged at `1 − 0.05/H`.
pub const HOLDOUT_ALPHA: f64 = 0.05;

/// One holdout's verdict under the C1 test: the trade count and mean the rule
/// saw, the split's quantile `z`, the one-sided lower bound on the expectancy
/// in R, and whether it passes. `PartialEq` (not `Eq`): `z` and `lower_bound`
/// are `f64`, exactly as `FoldVerdict`'s bound is.
#[derive(Debug, Clone, PartialEq)]
pub struct HoldoutVerdict {
    /// The number of holdout trades the verdict saw.
    pub n: usize,
    /// `Σ rᵢ / n` in `Decimal` (byte-exact; `0` when `n < 2`).
    pub mean_r: Decimal,
    /// The one-sided normal quantile the split gives: `z(1 − 0.05/H)`.
    pub z: f64,
    /// `mean − z · sqrt(var / n)` — the one-sided lower bound on the holdout
    /// expectancy in R. `0.0` when `n < 2` (the bound is undefined, and
    /// undefined does not pass).
    pub lower_bound: f64,
    /// `n >= 2 && lower_bound > 0.0` — a bound of exactly zero does not pass
    /// (it must be STRICTLY above zero).
    pub passes: bool,
}

/// The C1 holdout test (ADR-0028) over one holdout's `realized_r` series and
/// the hypothesis budget `H`.
///
/// The mean and the **sample variance (Bessel `N−1`)** accumulate in `Decimal`
/// exactly as [`FoldVerdict::from_rs`] does; `variance / n` converts to `f64`
/// once and is `sqrt`ed once — `lower_bound = mean − z · sqrt(var/n)` with
/// `z = z(1 − 0.05/H)`. `n < 2` yields `mean_r = 0`, `lower_bound = 0.0`,
/// `passes = false` (the verdict is defined, it simply cannot pass).
///
/// `H = 0` is not a legal budget — the split `0.05/0` is undefined — so it
/// yields the strictest possible verdict (`z = +∞`, `lower_bound = −∞`,
/// `passes = false`) rather than a fabricated finite bound. `H` is validated
/// upstream: the freeze record (w4/w5) owns the budget's legal range.
///
/// [`FoldVerdict::from_rs`]: super::FoldVerdict::from_rs
#[must_use]
pub fn holdout_test(rs: &[Decimal], h: u8) -> HoldoutVerdict {
    let n = rs.len();
    if h == 0 {
        return HoldoutVerdict {
            n,
            mean_r: Decimal::ZERO,
            z: f64::INFINITY,
            lower_bound: f64::NEG_INFINITY,
            passes: false,
        };
    }
    let z = inverse_normal(1.0 - HOLDOUT_ALPHA / f64::from(h));
    if n < 2 {
        return HoldoutVerdict {
            n,
            mean_r: Decimal::ZERO,
            z,
            lower_bound: 0.0,
            passes: false,
        };
    }
    let n_dec = Decimal::from(n);
    let sum: Decimal = rs.iter().copied().sum();
    let mean = sum / n_dec;
    let mut variance_num = Decimal::ZERO;
    for r in rs {
        let dev = *r - mean;
        variance_num += dev * dev;
    }
    let sample_var = variance_num / (n_dec - Decimal::ONE);
    let var_over_n = sample_var / n_dec;
    let standard_error = decimal_to_f64(var_over_n).sqrt();
    let lower_bound = decimal_to_f64(mean) - z * standard_error;
    HoldoutVerdict {
        n,
        mean_r: mean,
        z,
        lower_bound,
        passes: n >= 2 && lower_bound > 0.0,
    }
}

/// The inverse standard-normal CDF (the `z`-quantile), by Acklam's rational
/// approximation — |absolute error| < 1.15e-9, comfortably inside the 1e-6 the
/// C1 pins require; the coefficients are the published ones (P. J. Acklam,
/// "An algorithm for computing the inverse normal cumulative distribution
/// function"), so no new crate is added for one quantile.
///
/// `p <= 0` and `p >= 1` answer the mathematical limits; callers reach here
/// only through `1 − 0.05/H` with `H >= 1`, i.e. `p ∈ [0.95, 1)`.
fn inverse_normal(p: f64) -> f64 {
    // The coefficients of the central and tail rational approximations.
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239e0,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838e0,
        -2.549_732_539_343_734e0,
        4.374_664_141_464_968e0,
        2.938_163_982_698_783e0,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996e0,
        3.754_408_661_907_416e0,
    ];
    /// The central region's lower bound; the upper is `1 - P_LOW`.
    const P_LOW: f64 = 0.024_25;

    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }
    if p < P_LOW {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= 1.0 - P_LOW {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{HOLDOUT_ALPHA, HoldoutVerdict, holdout_test};
    use rust_decimal::Decimal;

    fn r(v: i64) -> Decimal {
        Decimal::new(v, 0)
    }

    /// The C1 family-wise alpha and the quantiles the rule splits on, pinned:
    /// H = 1 is the plain one-sided 95% z (the wf-v1 constant's neighbour),
    /// H = 6 and H = 12 the split values the item's ADR measures at.
    #[test]
    fn holdout_test_z_is_pinned_at_h_1_6_12() {
        assert_eq!(HOLDOUT_ALPHA.to_bits(), 0.05f64.to_bits());
        let expected = [
            (1_u8, 1.644_853_626_951_471_5_f64),
            (6, 2.393_979_799_818_510_4),
            (12, 2.638_257_273_476_751),
        ];
        for (h, z) in expected {
            let verdict = holdout_test(&[r(1), r(1)], h);
            assert!(
                (verdict.z - z).abs() < 1e-6,
                "H={h}: z={} vs {z}",
                verdict.z
            );
        }
        // H = 12 reads 2.638 to three decimals (the spec's pin).
        assert!(
            (holdout_test(&[r(1), r(1)], 12).z - 2.638).abs() < 5e-4,
            "H=12 z must render 2.638 to three decimals"
        );
    }

    /// `n < 2` is defined, not undefined: no mean, no bound, no pass — the
    /// `FoldVerdict` convention — while `z` still reports the H-derived split.
    #[test]
    fn holdout_test_under_two_trades_never_passes() {
        for rs in [&[][..], &[r(5)][..]] {
            let v: HoldoutVerdict = holdout_test(rs, 12);
            assert_eq!(v.n, rs.len());
            assert_eq!(v.mean_r, Decimal::ZERO);
            assert_eq!(v.lower_bound.to_bits(), 0.0f64.to_bits());
            assert!(!v.passes);
            assert!((v.z - 2.638_257_273_476_751).abs() < 1e-6);
        }
    }

    /// The bound is `mean − z·sqrt(var/n)` (Bessel variance), and a bound of
    /// exactly zero does not pass — strictly above is the rule.
    #[test]
    fn holdout_test_bound_matches_the_pinned_formula() {
        // Classic sample vector: mean 5, sample variance 32/7, n = 8.
        let rs: Vec<Decimal> = [2, 4, 4, 4, 5, 5, 7, 9].into_iter().map(r).collect();
        let v = holdout_test(&rs, 12);
        assert_eq!(v.n, 8);
        assert_eq!(v.mean_r, r(5));
        let expected = 5.0 - v.z * (4.0_f64 / 7.0).sqrt();
        assert!(
            (v.lower_bound - expected).abs() < 1e-9,
            "lb {} vs expected {expected}",
            v.lower_bound
        );
        assert!(v.passes, "5 − 2.638·0.756 is comfortably above zero");

        // Zero-edge and zero-variance: lb = 0.0 exactly, and that does not pass.
        let flat = holdout_test(&vec![r(0); 250], 12);
        assert_eq!(flat.lower_bound.to_bits(), 0.0f64.to_bits());
        assert!(!flat.passes);

        // n = 2 with a positive mean and zero variance passes: lb = mean > 0.
        assert!(holdout_test(&[r(1), r(1)], 6).passes);
    }

    /// H = 0 is not a legal budget (the split is undefined): the verdict is the
    /// strictest possible shape — no candidate can pass — rather than a
    /// fabricated finite bound. H is validated upstream (w4's freeze record).
    #[test]
    fn holdout_test_at_zero_hypotheses_never_passes() {
        let v = holdout_test(&vec![r(1); 100], 0);
        assert!(!v.passes);
        assert!(v.z.is_infinite() && v.z > 0.0, "z={}", v.z);
        assert!(
            v.lower_bound.is_infinite() && v.lower_bound < 0.0,
            "lb={}",
            v.lower_bound
        );
    }
}
