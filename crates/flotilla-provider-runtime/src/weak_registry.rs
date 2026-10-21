//! Weakly held, keyed handles. Each registry instance owns a separate namespace.

use std::{
    borrow::Borrow,
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex, Weak},
};

/// Shares a handle while any caller holds it, pruning unused entries on lookup.
/// The registry mutex protects lookup and creation only; callers own the lifetime
/// and synchronization policy of the returned value.
pub struct WeakRegistry<K, V> {
    entries: Mutex<HashMap<K, Weak<V>>>,
}

impl<K, V> Default for WeakRegistry<K, V> {
    fn default() -> Self {
        Self { entries: Mutex::default() }
    }
}

impl<K: Eq + Hash, V: Default> WeakRegistry<K, V> {
    /// Reuse the live handle for `key`, or create one after its last owner retires.
    pub fn get<Q>(&self, key: &Q) -> Arc<V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ToOwned<Owned = K> + ?Sized,
    {
        let mut entries = self.entries.lock().expect("weak registry poisoned");
        entries.retain(|_, entry| entry.strong_count() > 0);
        if let Some(value) = entries.get(key).and_then(Weak::upgrade) {
            return value;
        }
        let value = Arc::new(V::default());
        entries.insert(key.to_owned(), Arc::downgrade(&value));
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    // Concurrent lookups for one key must return the same live lock, and holding
    // it excludes that key without preventing another key or namespace progressing.
    #[test]
    fn concurrent_keys_and_namespaces() {
        let registry = WeakRegistry::<String, Mutex<()>>::default();
        let barrier = Barrier::new(8);
        let handles = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        registry.get("same")
                    })
                })
                .collect();
            threads.into_iter().map(|thread| thread.join().unwrap()).collect::<Vec<_>>()
        });
        for handle in &handles {
            assert!(Arc::ptr_eq(&handles[0], handle));
        }
        let _guard = handles[0].lock().unwrap();
        assert!(handles[1].try_lock().is_err());
        let other = registry.get("other");
        assert!(other.try_lock().is_ok());
        let separate = WeakRegistry::<String, Mutex<()>>::default().get("same");
        assert!(separate.try_lock().is_ok());
    }

    // After every lookup/drop, live keys share identity; unused keys are pruned.
    // Generated keys include empty and duplicates; sequences cover retirement,
    // recreation, and lookups while another key is still live.
    #[hegel::test]
    fn lifecycle(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let registry = WeakRegistry::<String, Mutex<()>>::default();
        let mut held = [None, None, None];
        let keys = ["", "a", "b"];
        let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(32));
        for _ in 0..steps {
            let index = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            if tc.draw(gs::booleans()) {
                held[index] = None;
            } else {
                let handle = registry.get(keys[index]);
                if let Some(previous) = &held[index] {
                    assert!(Arc::ptr_eq(previous, &handle));
                }
                held[index] = Some(handle);
                let entries = registry.entries.lock().unwrap();
                assert!(entries.values().all(|entry| entry.strong_count() > 0));
            }
        }
        drop(held);
        let old = registry.get("retired");
        let weak = Arc::downgrade(&old);
        drop(old);
        assert!(weak.upgrade().is_none());
        let recreated = registry.get("retired");
        assert!(!Weak::ptr_eq(&weak, &Arc::downgrade(&recreated)));
        assert_eq!(registry.entries.lock().unwrap().len(), 1);
    }
}
