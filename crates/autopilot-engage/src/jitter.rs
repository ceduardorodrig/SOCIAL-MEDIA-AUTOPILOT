//! Deterministic, seedable pseudo-random pacing.
//!
//! Pacing must look human, but it must also be reproducible in tests. A tiny
//! xorshift64* generator gives both: a fixed seed yields a fixed sequence, so
//! the jitter bounds and the sporadic-skip decision are unit-testable.

use std::time::{SystemTime, UNIX_EPOCH};

/// A seedable pseudo-random generator (xorshift64*).
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Build a generator from an explicit seed (tests use a fixed seed).
    pub fn from_seed(seed: u64) -> Self {
        // A zero state would be a fixed point, so force a non-zero bit.
        Self { state: seed | 1 }
    }

    /// Build a generator seeded from the clock and the process id.
    pub fn from_clock() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0);
        Self::from_seed(nanos ^ (u64::from(std::process::id()).rotate_left(32)))
    }

    /// Next raw 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Next value in `[0, 1)`.
    pub fn fraction(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Next value in `[min, max]` (inclusive); a degenerate range returns `min`.
    pub fn between(&mut self, min: u64, max: u64) -> u64 {
        if max <= min {
            return min;
        }
        min + self.next_u64() % (max - min + 1)
    }
}

/// A jittered delay in seconds within `[min, max]`.
pub fn jitter_secs(rng: &mut Rng, min: u64, max: u64) -> u64 {
    rng.between(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn between_stays_within_bounds() {
        let mut rng = Rng::from_seed(42);
        for _ in 0..10_000 {
            let value = rng.between(45, 240);
            assert!((45..=240).contains(&value), "out of range: {value}");
        }
    }

    #[test]
    fn a_degenerate_range_returns_the_bound() {
        let mut rng = Rng::from_seed(7);
        assert_eq!(rng.between(120, 120), 120);
        assert_eq!(rng.between(120, 10), 120);
    }

    #[test]
    fn the_same_seed_is_reproducible() {
        let mut first = Rng::from_seed(2026);
        let mut second = Rng::from_seed(2026);
        for _ in 0..100 {
            assert_eq!(first.next_u64(), second.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut first = Rng::from_seed(1);
        let mut second = Rng::from_seed(2);
        assert_ne!(first.next_u64(), second.next_u64());
    }

    #[test]
    fn fraction_is_in_the_unit_interval() {
        let mut rng = Rng::from_seed(99);
        for _ in 0..10_000 {
            let value = rng.fraction();
            assert!((0.0..1.0).contains(&value), "out of range: {value}");
        }
    }

    #[test]
    fn jitter_is_bounded() {
        let mut rng = Rng::from_seed(5);
        for _ in 0..1_000 {
            let value = jitter_secs(&mut rng, 600, 1800);
            assert!((600..=1800).contains(&value), "out of range: {value}");
        }
    }
}
