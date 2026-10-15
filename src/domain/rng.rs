//! The in-tree deterministic PRNG (r5.s4.w1): splitmix64, shared by the paper
//! fixture's synthetic series and the streak card's Monte Carlo draws.
//!
//! In-tree and tiny on purpose — a consumer that pulled `rand` would make its
//! output depend on a dependency's stream stability, and both consumers pin
//! their output byte-for-byte (the fixture through its data versions, the card
//! through its frozen goldens). The algorithm is the published splitmix64
//! (Steele et al.'s reference: the `state += 0x9E37…` mixer); the unit test at
//! the bottom pins seed 0's first three outputs against that published stream.

/// splitmix64 (Steele et al.): the shared fixed-seed PRNG. In-tree and tiny on
/// purpose — a consumer that pulled `rand` would make its pinned output depend
/// on a dependency's stream stability.
pub(crate) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Seed the stream.
    pub(crate) const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next 64 bits of the stream.
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform draw from `0..=span` (`span` inclusive), where `span` is
    /// non-negative.
    #[allow(clippy::cast_possible_wrap)]
    pub(crate) fn below_inclusive(&mut self, span: i64) -> i64 {
        let width = u64::try_from(span).unwrap_or(0).saturating_add(1);
        (self.next_u64() % width) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::SplitMix64;

    /// Seed 0's first three outputs, as published for splitmix64. Pinned so an
    /// edit to the mixing constants or the shift widths fails HERE, naming the
    /// algorithm, rather than silently moving the paper fixture's data versions
    /// or the streak card's goldens.
    #[test]
    fn splitmix64_seed_zero_matches_the_published_reference() {
        let mut rng = SplitMix64::new(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(rng.next_u64(), 0x06C4_5D18_8009_454F);
    }
}
