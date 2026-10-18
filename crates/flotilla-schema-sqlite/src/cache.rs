//! A bounded two-generation map: lookups promote entries into the young generation, and when the
//! young generation fills it becomes the old one, dropping the previous old generation. Recently
//! used entries survive at a cost of at most twice the capacity, with no per-entry bookkeeping.

use std::{
    borrow::Borrow,
    collections::HashMap,
    hash::{Hash, Hasher},
};

use flotilla_schema::Code;

/// Owned dictionary key; its borrowed view hashes and compares the same domain and text.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct StringKey {
    pub domain: Code,
    pub text: String,
}

impl StringKey {
    pub(crate) fn new(domain: Code, text: &str) -> Self {
        Self { domain, text: text.to_owned() }
    }
}

/// A composite borrowed key for std::HashMap, without allocating text on cache hits or misses.
/// Both implementations deliberately hash the same (Code, str) parts as StringKey's fields.
pub(crate) trait StringKeyRef {
    fn parts(&self) -> (Code, &str);
}

impl StringKeyRef for StringKey {
    fn parts(&self) -> (Code, &str) {
        (self.domain, &self.text)
    }
}

impl StringKeyRef for (Code, &str) {
    fn parts(&self) -> (Code, &str) {
        *self
    }
}

impl Hash for dyn StringKeyRef + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.parts().hash(state);
    }
}

impl PartialEq for dyn StringKeyRef + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.parts() == other.parts()
    }
}

impl Eq for dyn StringKeyRef + '_ {}

impl<'a> Borrow<dyn StringKeyRef + 'a> for StringKey {
    fn borrow(&self) -> &(dyn StringKeyRef + 'a) {
        self
    }
}

pub(crate) struct TwoGen<K, V> {
    young: HashMap<K, V>,
    old: HashMap<K, V>,
    capacity: usize,
}

impl<K: Eq + Hash, V: Clone> TwoGen<K, V> {
    pub(crate) fn new(capacity: usize) -> TwoGen<K, V> {
        TwoGen { young: HashMap::new(), old: HashMap::new(), capacity: capacity.max(1) }
    }

    pub(crate) fn get<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        if let Some(value) = self.young.get(key) {
            return Some(value.clone());
        }
        let (key, value) = self.old.remove_entry(key)?;
        self.insert(key, value.clone());
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
    // Property (#3422 review): borrowed and owned keys are interchangeable across the pending
    // map and both cache generations. Domains, empty/Unicode text and eviction all participate.
    #[hegel::test]
    fn borrowed_domain_keys_match_owned_keys(tc: hegel::TestCase) {
        use super::{StringKey, StringKeyRef};
        use flotilla_schema::Code;
        use hegel::generators as gs;
        let text = tc.draw(gs::text().alphabet("abé '").max_size(4));
        let mut cache = TwoGen::new(2);
        let mut pending = std::collections::HashMap::new();
        for (index, domain) in [Code::new("ad"), Code::new("wr")].into_iter().enumerate() {
            pending.insert(StringKey::new(domain, &text), index);
            cache.insert(StringKey::new(domain, &text), index);
        }
        cache.insert(StringKey::new(Code::new("hm"), &text), 2); // rotate ad/wr into old
        for (index, domain) in [Code::new("ad"), Code::new("wr")].into_iter().enumerate() {
            let key = (domain, text.as_str());
            let key: &dyn StringKeyRef = &key;
            assert_eq!(pending.get(key).copied(), Some(index));
            assert_eq!(cache.get(key), Some(index));
            cache.remove(key);
            assert_eq!(cache.get(key), None);
        }
        assert!(cache.len() <= 4);
    }
}
