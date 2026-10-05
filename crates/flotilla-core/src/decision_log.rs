//! Bounded per-key decision memory. Timestamps belong in emitted fields, not keys.
use std::{collections::BTreeMap, fmt::Debug, sync::Mutex};

#[derive(Default)]
pub struct DecisionLog {
    decisions: Mutex<DecisionMemory>,
}

#[derive(Default)]
struct DecisionMemory {
    entries: BTreeMap<String, (String, u64)>,
    clock: u64,
}

impl DecisionLog {
    pub fn changed(&self, key: String, decision: impl Debug) -> bool {
        let decision = format!("{decision:?}");
        let mut decisions = self.decisions.lock().expect("decision log lock");
        decisions.clock += 1;
        let clock = decisions.clock;
        if let Some((prior, used_at)) = decisions.entries.get_mut(&key) {
            *used_at = clock;
            if prior == &decision {
                return false;
            }
        }
        // Evict least recently observed, regardless of key prefix. Deleted
        // resources naturally age out while live unchanged decisions stay warm.
        if decisions.entries.len() >= 4096 && !decisions.entries.contains_key(&key) {
            let oldest = decisions.entries.iter().min_by_key(|(_, (_, used_at))| used_at).map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                decisions.entries.remove(&oldest);
            }
        }
        decisions.entries.insert(key, (decision, clock));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn eviction_retains_recently_observed_early_prefix() {
        let log = DecisionLog::default();
        assert!(log.changed("attention/live".into(), true));
        for index in 0..4095 {
            assert!(log.changed(format!("terminal/{index}"), true));
        }
        assert!(!log.changed("attention/live".into(), true));
        assert!(log.changed("terminal/new".into(), true));
        assert!(!log.changed("attention/live".into(), true));
        assert!(log.changed("terminal/0".into(), true));
    }

    // Decisions emit exactly at per-key transitions. Draw duplicate decisions,
    // interleaved keys, and empty sequences; observation timestamps are excluded.
    #[hegel::test]
    fn decision_emission_follows_per_key_changes(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let log = DecisionLog::default();
        let mut expected = BTreeMap::new();
        let steps = tc.draw(gs::integers::<usize>().min_value(0).max_value(30));
        for _ in 0..steps {
            let key = tc.draw(gs::integers::<usize>().min_value(0).max_value(2)).to_string();
            let decision = tc.draw(gs::integers::<u8>().min_value(0).max_value(2));
            let changed = expected.insert(key.clone(), decision) != Some(decision);
            assert_eq!(log.changed(key, decision), changed);
        }
    }
}
