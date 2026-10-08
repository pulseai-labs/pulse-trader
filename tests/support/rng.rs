//! The wf-v2 suites' deterministic generator (r4.s1.w3), shared so the
//! calibration test (`tests/wf_v2_calibration.rs`) and the measurement harness
//! (`tests/wf_v2_measurement.rs`) draw from the SAME stream: a count pinned in
//! one is a pin on the other, and re-deriving the ADR-0028 tables runs the code
//! the calibration proves.
//!
//! This module compiles into every suite that declares `mod support;`, so its
//! items read as dead code in the suites that do not draw from the stream — a
//! named allowance, the `support/mcp.rs` precedent.
#![allow(dead_code)]

use rust_decimal::Decimal;

/// splitmix64 — the in-test deterministic PRNG (no new crate; the mixing
/// constants are the published ones), advanced one `u64` at a time so
/// seed → series is a pure function of the seed.
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)` from the top 53 bits.
    ///
    /// The 53-bit integer is split into two exact halves: each converts through
    /// the lossless `f64::from(u32)`, so no lossy integer→float cast is
    /// involved, and the combination is exact (53 significant bits fit an
    /// `f64` mantissa).
    pub fn next_f64(&mut self) -> f64 {
        /// 2³² — the weight of the high half.
        const TWO_POW_32: f64 = 4_294_967_296.0;
        /// 2⁻⁵³ — the scale from 53 random bits to `[0, 1)`.
        const TWO_POW_MINUS_53: f64 = 1.0 / 9_007_199_254_740_992.0;
        let bits = self.next_u64() >> 11;
        let hi = u32::try_from(bits >> 32).expect("53 bits >> 32 fits a u32");
        let lo = u32::try_from(bits & 0xFFFF_FFFF).expect("32 bits fit a u32");
        (f64::from(hi) * TWO_POW_32 + f64::from(lo)) * TWO_POW_MINUS_53
    }

    /// One standard normal draw, Box–Muller (the `u1 = 0` guard keeps `ln`
    /// finite; the cosine branch means the sine branch is never needed).
    pub fn next_normal(&mut self) -> f64 {
        let u1 = self.next_f64().max(f64::MIN_POSITIVE);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

/// One `N(mean_r, sigma_r²)` draw as the `Decimal` the verdict rules accumulate
/// in — rounded to 4 decimal places, a documented and deterministic
/// quantization of an `f64`.
pub fn normal_decimal(rng: &mut SplitMix64, mean_r: f64, sigma_r: f64) -> Decimal {
    let draw = mean_r + sigma_r * rng.next_normal();
    Decimal::from_f64_retain(draw)
        .expect("a finite normal draw")
        .round_dp(4)
}
