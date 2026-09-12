//! A seeded generator, so every number in `BENCHMARKS.md` can be reproduced.
//!
//! Not `rand`: a benchmark that cannot be replayed exactly is a benchmark whose
//! regressions cannot be bisected, and pulling in a dependency to get *less*
//! reproducibility would be a strange trade. Plan §6 asks that every failing run
//! print its seed; the same discipline is what makes a 3% slowdown attributable
//! to a commit rather than to the weather.

/// SplitMix64 — the generator `rand` itself uses to seed others.
///
/// Small enough to read in one sitting, and its output distribution is well
/// studied, which is more than can be said for a hand-rolled xorshift.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n`.
    ///
    /// Lemire's multiply-shift rather than `%`: the modulo version is biased
    /// towards small values whenever `n` does not divide 2^64, and the whole
    /// point of the workload generator is that its distribution is the one
    /// described in the docs.
    pub fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0, "below(0) has no answer");
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }

    /// A number in `lo..=hi`.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(lo <= hi);
        lo + self.below((hi - lo + 1) as u64) as i64
    }

    /// True with probability `numerator / 100`.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_replays_the_same_sequence() {
        let a: Vec<u64> = (0..8).map(|_| Rng::new(7).next_u64()).collect();
        let mut r = Rng::new(7);
        assert_eq!(a[0], r.next_u64());

        let first: Vec<u64> = {
            let mut r = Rng::new(99);
            (0..16).map(|_| r.next_u64()).collect()
        };
        let second: Vec<u64> = {
            let mut r = Rng::new(99);
            (0..16).map(|_| r.next_u64()).collect()
        };
        assert_eq!(first, second);
    }

    #[test]
    fn below_stays_in_range_and_covers_it() {
        let mut r = Rng::new(1);
        let mut seen = [false; 5];
        for _ in 0..500 {
            let v = r.below(5);
            assert!(v < 5);
            seen[v as usize] = true;
        }
        assert!(seen.iter().all(|s| *s), "every bucket should be reachable");
    }

    #[test]
    fn range_includes_both_endpoints() {
        let mut r = Rng::new(2);
        let (mut lo, mut hi) = (false, false);
        for _ in 0..500 {
            let v = r.range(-3, 3);
            assert!((-3..=3).contains(&v));
            lo |= v == -3;
            hi |= v == 3;
        }
        assert!(lo && hi, "range is inclusive at both ends");
    }
}
