//! The shared handle every writer, gate and follower of one store locks. Once a timer is
//! installed, each hold reports how long its lock waited and how long it was held, as a write
//! when the hold began a write transaction and as a read otherwise.

use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, LockResult, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError},
    time::{Duration, Instant},
};

use crate::Store;

/// One released hold of a [`StoreHandle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold {
    /// Whether the hold began a write transaction.
    pub write: bool,
    /// From asking for the lock until holding it; zero when it was free.
    pub wait: Duration,
    /// From taking the lock until releasing it.
    pub held: Duration,
}

/// Called as each hold is released; must not block or lock the store.
pub type HoldTimer = dyn Fn(Hold) + Send + Sync;

/// A [`Store`] behind its one lock.
pub struct StoreHandle {
    store: Mutex<Store>,
    timer: OnceLock<Arc<HoldTimer>>,
}

impl StoreHandle {
    pub fn new(store: Store) -> Self {
        Self { store: Mutex::new(store), timer: OnceLock::new() }
    }

    /// Times every later hold with `timer`; a handle takes one timer.
    pub fn time_holds(&self, timer: Arc<HoldTimer>) -> Result<(), String> {
        self.timer.set(timer).map_err(|_| "store holds are already timed".to_owned())
    }

    /// Locks the store, as [`Mutex::lock`] does. A timed lock reads the clock as it takes the
    /// lock and as it releases it, and once more before waiting for a held lock.
    pub fn lock(&self) -> LockResult<StoreGuard<'_>> {
        let Some(timer) = self.timer.get() else {
            return match self.store.lock() {
                Ok(guard) => Ok(StoreGuard { guard, timed: None }),
                Err(poisoned) => Err(PoisonError::new(StoreGuard { guard: poisoned.into_inner(), timed: None })),
            };
        };
        let (locked, asked) = match self.store.try_lock() {
            Ok(guard) => (Ok(guard), None),
            Err(TryLockError::Poisoned(poisoned)) => (Err(poisoned), None),
            Err(TryLockError::WouldBlock) => {
                let asked = Instant::now();
                (self.store.lock(), Some(asked))
            }
        };
        let taken = Instant::now();
        let timed = Some(Timed { timer: timer.as_ref(), wait: asked.map_or(Duration::ZERO, |asked| taken - asked), taken });
        match locked {
            Ok(guard) => Ok(StoreGuard { guard, timed }),
            Err(poisoned) => Err(PoisonError::new(StoreGuard { guard: poisoned.into_inner(), timed })),
        }
    }

    pub fn is_poisoned(&self) -> bool {
        self.store.is_poisoned()
    }

    /// The store, even from a poisoned lock: a write that panicked rolled its transaction back.
    pub fn into_inner(self) -> Store {
        self.store.into_inner().unwrap_or_else(PoisonError::into_inner)
    }
}

struct Timed<'a> {
    timer: &'a HoldTimer,
    wait: Duration,
    taken: Instant,
}

/// A held [`StoreHandle`]; releasing it reports the hold to the handle's timer.
pub struct StoreGuard<'a> {
    guard: MutexGuard<'a, Store>,
    timed: Option<Timed<'a>>,
}

impl Deref for StoreGuard<'_> {
    type Target = Store;

    fn deref(&self) -> &Store {
        &self.guard
    }
}

impl DerefMut for StoreGuard<'_> {
    fn deref_mut(&mut self) -> &mut Store {
        &mut self.guard
    }
}

impl Drop for StoreGuard<'_> {
    fn drop(&mut self) {
        let write = self.guard.take_wrote();
        if let Some(timed) = &self.timed {
            (timed.timer)(Hold { write, wait: timed.wait, held: timed.taken.elapsed() });
        }
    }
}
