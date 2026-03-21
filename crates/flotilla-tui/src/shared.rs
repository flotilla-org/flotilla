use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, MutexGuard,
};

/// Shared mutable state with generation-based change detection.
///
/// The event loop calls `mutate()` to update data and bump the generation.
/// Widgets call `changed()` to detect updates since their last read.
/// The generation counter is an `AtomicU64` outside the mutex, so
/// `generation()` is lock-free.
#[derive(Debug)]
pub struct Shared<T> {
    inner: Arc<SharedInner<T>>,
}

#[derive(Debug)]
struct SharedInner<T> {
    generation: AtomicU64,
    data: Mutex<T>,
}

impl<T> Shared<T> {
    pub fn new(data: T) -> Self {
        Self {
            inner: Arc::new(SharedInner {
                generation: AtomicU64::new(1), // start at 1 so 0 means "never seen"
                data: Mutex::new(data),
            }),
        }
    }

    /// Lock and return the data unconditionally.
    pub fn read(&self) -> MutexGuard<'_, T> {
        self.inner.data.lock().expect("shared data poisoned")
    }

    /// Current generation (lock-free).
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    /// If the generation advanced since `*since`, lock the data, update
    /// `*since`, and return the guard. Otherwise return `None`.
    pub fn changed(&self, since: &mut u64) -> Option<MutexGuard<'_, T>> {
        let current = self.generation();
        if current > *since {
            *since = current;
            Some(self.read())
        } else {
            None
        }
    }

    /// Lock the data, apply `f`, and bump the generation.
    pub fn mutate(&self, f: impl FnOnce(&mut T)) {
        let mut guard = self.inner.data.lock().expect("shared data poisoned");
        f(&mut *guard);
        self.inner.generation.fetch_add(1, Ordering::Release);
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_at_generation_1() {
        let s = Shared::new(42);
        assert_eq!(s.generation(), 1);
    }

    #[test]
    fn read_returns_initial_data() {
        let s = Shared::new("hello");
        assert_eq!(*s.read(), "hello");
    }

    #[test]
    fn mutate_updates_data_and_bumps_generation() {
        let s = Shared::new(0);
        s.mutate(|v| *v = 99);
        assert_eq!(*s.read(), 99);
        assert_eq!(s.generation(), 2);
    }

    #[test]
    fn changed_returns_data_on_first_call() {
        let s = Shared::new(42);
        let mut seen = 0u64;
        let guard = s.changed(&mut seen);
        assert!(guard.is_some());
        assert_eq!(*guard.expect("expected changed data"), 42);
        assert_eq!(seen, 1);
    }

    #[test]
    fn changed_returns_none_when_unchanged() {
        let s = Shared::new(42);
        let mut seen = 0u64;
        let _ = s.changed(&mut seen); // consume initial
        assert!(s.changed(&mut seen).is_none());
    }

    #[test]
    fn changed_returns_data_after_mutate() {
        let s = Shared::new(0);
        let mut seen = 0u64;
        let _ = s.changed(&mut seen); // consume initial
        s.mutate(|v| *v = 5);
        let guard = s.changed(&mut seen);
        assert!(guard.is_some());
        assert_eq!(*guard.expect("expected changed data"), 5);
        assert_eq!(seen, 2);
    }

    #[test]
    fn clone_shares_state() {
        let s1 = Shared::new(0);
        let s2 = s1.clone();
        s1.mutate(|v| *v = 7);
        assert_eq!(*s2.read(), 7);
        assert_eq!(s2.generation(), 2);
    }

    #[test]
    fn multiple_mutates_accumulate_generation() {
        let s = Shared::new(0);
        for i in 1..=5 {
            s.mutate(|v| *v = i);
        }
        assert_eq!(s.generation(), 6); // 1 initial + 5 mutates
        assert_eq!(*s.read(), 5);
    }
}
