//! Seedable, dependency-free randomness for the personas (SplitMix64).
//!
//! Persona randomness — think times, window jitter, presence, variant and
//! time-control draws, search seeds — is driven by a per-bot seedable
//! generator (ADR-0014 §4): a fixed seed in tests makes a bot's whole
//! behavior reproducible. No ambient randomness outside key generation
//! (which the bot never does — keys are operator-supplied).

/// SplitMix64 — the same tiny generator the player crate uses for its
/// tie-breaks.
#[derive(Debug, Clone, Copy)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// Seed the generator.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next pseudo-random value.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform draw in `[0, 1)`.
    pub fn next_unit(&mut self) -> f64 {
        // 53 significand bits — the standard uniform-double construction.
        let bits = self.next_u64() >> 11;
        (bits as f64) / ((1_u64 << 53) as f64)
    }

    /// A uniform index in `0..bound` (`0` when `bound == 0`).
    pub fn next_index(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        usize::try_from(self.next_u64().checked_rem(bound as u64).unwrap_or(0)).unwrap_or(0)
    }

    /// A standard-normal draw (Box–Muller; the second value is discarded —
    /// simplicity over throughput, persona sampling is not hot).
    pub fn next_normal(&mut self) -> f64 {
        // Guard the log: u1 ∈ (0, 1].
        let u1 = 1.0 - self.next_unit();
        let u2 = self.next_unit();
        (-2.0 * u1.ln()).sqrt() * (core::f64::consts::TAU * u2).cos()
    }

    /// A log-normal draw parameterized the persona way: `median · e^(σ·z)`
    /// (the median is exp(μ), so μ never appears in configuration).
    pub fn next_lognormal(&mut self, median: f64, sigma: f64) -> f64 {
        median * (sigma * self.next_normal()).exp()
    }

    /// A weighted index over `weights` (uniform fallback when the sum is not
    /// positive). Deterministic for a given generator state.
    pub fn next_weighted(&mut self, weights: &[f64]) -> usize {
        let total: f64 = weights.iter().copied().filter(|w| *w > 0.0).sum();
        if total <= 0.0 {
            return self.next_index(weights.len());
        }
        let mut draw = self.next_unit() * total;
        for (index, weight) in weights.iter().enumerate() {
            if *weight <= 0.0 {
                continue;
            }
            if draw < *weight {
                return index;
            }
            draw -= *weight;
        }
        weights.len().saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::SplitMix64;

    #[test]
    fn deterministic_under_a_fixed_seed() {
        let mut a = SplitMix64::new(9);
        let mut b = SplitMix64::new(9);
        for _ in 0..8 {
            assert!((a.next_unit() - b.next_unit()).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn lognormal_median_is_roughly_the_median() {
        let mut rng = SplitMix64::new(42);
        let mut below = 0_u32;
        for _ in 0..2_000 {
            if rng.next_lognormal(3.0, 0.6) < 3.0 {
                below += 1;
            }
        }
        // The median splits the mass; allow a generous band.
        assert!(
            (800..=1_200).contains(&below),
            "below-median count: {below}"
        );
    }

    #[test]
    fn weighted_draw_respects_zero_weights() {
        let mut rng = SplitMix64::new(7);
        for _ in 0..64 {
            let pick = rng.next_weighted(&[0.0, 1.0, 0.0]);
            assert_eq!(pick, 1);
        }
    }
}
