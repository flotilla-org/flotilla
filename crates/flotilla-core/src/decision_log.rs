//! Bounded per-key decision memory. Timestamps belong in emitted fields, not keys.
use std::{collections::BTreeMap, fmt::Debug, sync::Mutex};

#[derive(Default)]
pub struct DecisionLog {
    decisions: Mutex<BTreeMap<String, String>>,
}

impl DecisionLog {
    pub fn changed(&self, key: String, decision: impl Debug) -> bool {
        let decision = format!("{decision:?}");
        let mut decisions = self.decisions.lock().expect("decision log lock");
        if decisions.get(&key) == Some(&decision) {
            return false;
        }
        // Bound memory even when deleted resources never reconcile again.
        if decisions.len() >= 4096 && !decisions.contains_key(&key) {
            let _ = decisions.pop_first();
        }
        decisions.insert(key, decision);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
