//! A bounded two-generation map: lookups promote entries into the young generation, and when the
//! young generation fills it becomes the old one, dropping the previous old generation. Recently
//! used entries survive at a cost of at most twice the capacity, with no per-entry bookkeeping.

use std::{borrow::Borrow, collections::HashMap, hash::Hash};

pub(crate) struct TwoGen<K, V> {
    young: HashMap<K, V>,
    old: HashMap<K, V>,
    capacity: usize,
}

impl<K: Eq + Hash + Clone, V: Clone> TwoGen<K, V> {
    pub(crate) fn new(capacity: usize) -> TwoGen<K, V> {
        TwoGen { young: HashMap::new(), old: HashMap::new(), capacity: capacity.max(1) }
    }

    pub(crate) fn get<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ToOwned<Owned = K> + ?Sized,
    {
        if let Some(value) = self.young.get(key) {
            return Some(value.clone());
        }
        let value = self.old.remove(key)?;
        self.insert(key.to_owned(), value.clone());
        Some(value)
    }

    pub(crate) fn insert(&mut self, key: K, value: V) {
        if self.young.len() >= self.capacity {
            self.old = std::mem::take(&mut self.young);
        }
        self.young.insert(key, value);
    }

    pub(crate) fn remove<Q>(&mut self, key: &Q)
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.young.remove(key);
        self.old.remove(key);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.young.len() + self.old.len()
    }
}

#[cfg(test)]
mod tests {
    use super::TwoGen;

    // Behaviour: the cache never holds more than twice its capacity, and an entry looked up since
    // the last generation turn survives the next one.
    #[test]
    fn bounded_and_keeps_recent_entries() {
        let mut cache = TwoGen::new(2);
        for key in 0..10 {
            cache.insert(key, key * 10);
            assert!(cache.len() <= 4);
        }
        assert_eq!(cache.get(&0), None);
        assert_eq!(cache.get(&8), Some(80));
        cache.insert(10, 100);
        cache.insert(11, 110);
        assert_eq!(cache.get(&8), Some(80), "promoted on lookup");
    }
}
