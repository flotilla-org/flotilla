use std::sync::Mutex;

/// Supplies uniform draws to decisions that must be reproducible under test:
/// tests fix the seed, production seeds from real randomness (#3884).
pub trait RandomSource: Send + Sync {
    /// A uniform draw from `0..bound`. `bound` is never zero.
    fn below(&self, bound: u32) -> u32;
}

/// A splitmix64 stream. `SeededRandom::from_entropy` seeds it from the
/// operating system's randomness; `SeededRandom::new` fixes the seed.
#[derive(Debug)]
pub struct SeededRandom {
    state: Mutex<u64>,
}

impl SeededRandom {
    pub fn new(seed: u64) -> Self {
        Self { state: Mutex::new(seed) }
    }

    pub fn from_entropy() -> Self {
        let bytes = uuid::Uuid::new_v4().into_bytes();
        Self::new(u64::from_le_bytes(bytes[..8].try_into().expect("eight bytes")))
    }

    fn next(&self) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

impl RandomSource for SeededRandom {
    fn below(&self, bound: u32) -> u32 {
        assert!(bound > 0, "a draw needs at least one candidate");
        // Rejection sampling keeps the draw uniform for every bound.
        let bound = u64::from(bound);
        let zone = u64::MAX - (u64::MAX % bound);
        loop {
            let value = self.next();
            if value < zone {
                return (value % bound) as u32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_seed_repeats_its_draws_and_stays_in_bounds() {
        let draws = |seed| {
            let random = SeededRandom::new(seed);
            (0..32).map(|_| random.below(7)).collect::<Vec<_>>()
        };
        assert_eq!(draws(11), draws(11));
        assert_ne!(draws(11), draws(12));
        assert!(draws(11).iter().all(|draw| *draw < 7));
        let seen = draws(11).into_iter().collect::<std::collections::BTreeSet<_>>();
        assert!(seen.len() > 3, "{seen:?}");
    }
}
