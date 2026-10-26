use std::sync::Mutex;

use rand::{rngs::StdRng, Rng, SeedableRng};

/// Supplies the uniform draw that selects a requester's account (#3884): tests fix the seed,
/// production seeds from the operating system.
pub trait RandomSource: Send + Sync {
    /// A uniform draw from `0..bound`. `bound` is never zero.
    fn below(&self, bound: u32) -> u32;
}

#[derive(Debug)]
pub struct SeededRandom(Mutex<StdRng>);

impl SeededRandom {
    pub fn new(seed: u64) -> Self {
        Self(Mutex::new(StdRng::seed_from_u64(seed)))
    }

    pub fn from_entropy() -> Self {
        Self(Mutex::new(StdRng::from_os_rng()))
    }
}

impl RandomSource for SeededRandom {
    fn below(&self, bound: u32) -> u32 {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).random_range(0..bound)
    }
}
