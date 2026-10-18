//! Random numbers.
//!
//! `lb.random` in the reference implementation drew from PHP's `mt_rand()`
//! (`mt_rand() / mt_getrandmax()`, resolution 2^31-1). Games never depend on
//! the exact stream, so the port uses the process-wide generator by default.
//! Set `LuaRandomness::Deterministic` to draw from a `ChaCha8Rng` seeded per
//! match instead (useful for replays and tests).

use std::sync::Mutex;

use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;

/// Global generator for guest ids, token bytes and the default `lb.random`.
static GLOBAL: Mutex<Option<StdRng>> = Mutex::new(None);

/// PHP `mt_getrandmax()`.
pub const MAX_FRACTION: f64 = 2_147_483_647.0;

/// A fraction in `[0, 1]` with the reference implementation's resolution.
pub fn fraction() -> f64 {
    (global_int(0, MAX_FRACTION as i64) as f64) / MAX_FRACTION
}

/// Uniform integer in `[min, max]` (inclusive).
pub fn global_int(min: i64, max: i64) -> i64 {
    if max <= min {
        return min;
    }

    let mut guard = GLOBAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let rng = guard.get_or_insert_with(StdRng::from_os_rng);
    rng.random_range(min..=max)
}

/// Random alphanumeric string of `len` characters (`a-z0-9`), used for guest
/// player ids (`p-<10 random>` in the reference implementation).
pub fn alphanumeric(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

    let mut guard = GLOBAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let rng = guard.get_or_insert_with(StdRng::from_os_rng);

    (0..len)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

/// Deterministic per-match generator for `LuaRandomness::Deterministic`.
#[derive(Debug, Clone)]
pub struct MatchRng {
    rng: StdRng,
}

impl MatchRng {
    /// Seeds from the match id (stable across restarts).
    pub fn for_match(match_id: &str) -> Self {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(match_id.as_bytes());
        let digest = hasher.finalize();

        let mut seed = [0u8; 32];
        seed.copy_from_slice(&digest);

        Self {
            rng: StdRng::from_seed(seed),
        }
    }

    pub fn fraction(&mut self) -> f64 {
        (self.rng.random_range(0..=MAX_FRACTION as i64) as f64) / MAX_FRACTION
    }

    pub fn int(&mut self, min: i64, max: i64) -> i64 {
        if max <= min {
            return min;
        }

        let fraction = self.fraction();
        let span = (max - min) as f64;
        min + ((span + 1.0) * fraction).floor().min(span) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_stay_within_bounds_and_are_swapped_when_reversed() {
        for _ in 0..100 {
            let value = global_int(3, 7);
            assert!((3..=7).contains(&value));
            let value = global_int(7, 3);
            assert!((3..=7).contains(&value));
            assert_eq!(global_int(5, 5), 5);
        }
    }

    #[test]
    fn fractions_are_in_unit_range() {
        for _ in 0..100 {
            let value = fraction();
            assert!((0.0..=1.0).contains(&value));
        }
    }

    #[test]
    fn match_rng_is_deterministic_per_match() {
        let mut first = MatchRng::for_match("coin_rush");
        let mut second = MatchRng::for_match("coin_rush");

        for _ in 0..20 {
            let left = first.int(0, 10_000);
            let right = second.int(0, 10_000);
            assert_eq!(left, right);
            assert!((0..=10_000).contains(&left));
        }
    }

    #[test]
    fn guest_ids_have_the_expected_shape() {
        let id = alphanumeric(10);
        assert_eq!(id.len(), 10);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }
}
