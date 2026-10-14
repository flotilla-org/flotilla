//! What a load run does: how many convoys to keep running, of which mixes,
//! how fast to ramp, and how long to measure.

use std::time::Duration;

use flotilla_dummy_fleet::catalog::CrewMix;

#[derive(Debug, Clone, PartialEq)]
pub struct LoadPlan {
    /// Convoys to keep open at once; each that ends is replaced.
    pub running: usize,
    /// Relative weights of each crew mix; dispatches cycle through them.
    pub mix: Vec<(CrewMix, u32)>,
    /// Dispatch rate while ramping and replacing, in convoys per second.
    pub ramp_per_second: f64,
    /// Admissions in flight at once.
    pub dispatch_concurrency: usize,
    /// Measurement window, starting when `running` convoys are first open.
    pub window: Duration,
    /// Measure anyway if the ramp has not reached `running` by then.
    pub max_ramp: Duration,
    /// Abandon a convoy still open this long after its dispatch.
    pub convoy_deadline: Duration,
    /// How often the driver observes outcomes and tops up.
    pub tick: Duration,
    /// How often the time series records a point.
    pub series_every: Duration,
}

impl LoadPlan {
    /// The mix of the `index`-th dispatch: a deterministic weighted rotation,
    /// so any window of `total weight` dispatches has exactly the weights.
    pub fn mix_for(&self, index: u64) -> CrewMix {
        let total = self.mix.iter().map(|(_, weight)| u64::from(*weight)).sum::<u64>();
        if total == 0 {
            return CrewMix::Coder;
        }
        let mut slot = index % total;
        for (mix, weight) in &self.mix {
            let weight = u64::from(*weight);
            if slot < weight {
                return *mix;
            }
            slot -= weight;
        }
        unreachable!("slot is below the total weight")
    }
}

impl Default for LoadPlan {
    fn default() -> Self {
        Self {
            running: 100,
            mix: vec![(CrewMix::Coder, 6), (CrewMix::CoderReviewer, 3), (CrewMix::LongRunner, 1)],
            ramp_per_second: 20.0,
            dispatch_concurrency: 16,
            window: Duration::from_secs(120),
            max_ramp: Duration::from_secs(600),
            convoy_deadline: Duration::from_secs(600),
            tick: Duration::from_millis(250),
            series_every: Duration::from_secs(5),
        }
    }
}

#[cfg(test)]
mod tests {
    use hegel::generators as gs;

    use super::*;

    // Behaviour: every run of `total weight` consecutive dispatches contains
    // each mix exactly its weight times. Weights from zero to five, including
    // all-zero (which falls back to coders), for any starting index.
    #[hegel::test]
    fn the_rotation_honours_the_weights_in_every_window(tc: hegel::TestCase) {
        let weights = CrewMix::ALL.map(|_| tc.draw(gs::integers::<u32>().min_value(0).max_value(5)));
        let plan = LoadPlan { mix: CrewMix::ALL.into_iter().zip(weights).collect(), ..LoadPlan::default() };
        let total = u64::from(weights.iter().sum::<u32>());
        let start = tc.draw(gs::integers::<u64>().min_value(0).max_value(1_000_000));
        if total == 0 {
            assert_eq!(plan.mix_for(start), CrewMix::Coder);
            return;
        }
        for (mix, weight) in CrewMix::ALL.into_iter().zip(weights) {
            let seen = (start..start + total).filter(|index| plan.mix_for(*index) == mix).count() as u32;
            assert_eq!(seen, weight, "{mix:?}");
        }
    }
}
