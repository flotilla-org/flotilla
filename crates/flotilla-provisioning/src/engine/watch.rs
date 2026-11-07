//! Following the decided log from async code: a pass runs when the store commits a write of a
//! followed kind, never on a timer, and reads the log on a blocking thread, so the store's lock
//! is never taken on an async worker. A commit of other kinds, or a keepalive, wakes nothing.

use std::{collections::BTreeSet, sync::Arc};

use flotilla_engine_store::follow::{Followed, Follower};
use flotilla_schema::KindCode;
use flotilla_schema_sqlite::{
    stream::{CommitListener, Committed, Service},
    Read, StoreError,
};
use tokio::sync::Notify;

/// A [`Follower`] of some kinds over every home's log, woken by the store's commits.
pub struct EngineWatch {
    service: Arc<Service>,
    follower: Follower,
    wake: Arc<Notify>,
    // The store holds the listener weakly: it lapses with this watch.
    _listener: Arc<CommitListener>,
}

impl EngineWatch {
    /// Follows `kinds` from each home's current sequence, woken by commits that write them. The
    /// listener is installed first, so a commit between it and the start still wakes the first
    /// pass.
    pub fn start(service: Arc<Service>, kinds: impl IntoIterator<Item = KindCode>) -> Result<Self, StoreError> {
        Self::start_waking(service, kinds, false)
    }

    /// As [`EngineWatch::start`], also woken when another home is heard from, keepalives
    /// included, for a reader of each home's heard time.
    pub fn start_hearing(service: Arc<Service>, kinds: impl IntoIterator<Item = KindCode>) -> Result<Self, StoreError> {
        Self::start_waking(service, kinds, true)
    }

    fn start_waking(service: Arc<Service>, kinds: impl IntoIterator<Item = KindCode>, hearing: bool) -> Result<Self, StoreError> {
        let kinds: BTreeSet<KindCode> = kinds.into_iter().collect();
        let wake = Arc::new(Notify::new());
        let listener: Arc<CommitListener> = {
            let (wake, kinds) = (Arc::clone(&wake), kinds.clone());
            Arc::new(move |committed: &Committed<'_>| {
                if committed.touches(&kinds) || (hearing && !committed.heard.is_empty()) {
                    wake.notify_one()
                }
            })
        };
        service.on_commit(Arc::downgrade(&listener));
        let mut follower = Follower::new(kinds);
        let homes = service.homes();
        service.with_store(|store| follower.start(&mut store.read()?, &homes))?;
        Ok(Self { service, follower, wake, _listener: listener })
    }

    /// Waits for the next commit, then passes over the log and hands what it found to `read`,
    /// in the same read. A pass that fails is tried again at the next commit.
    pub async fn next<T: Send + 'static>(
        &mut self,
        read: impl FnOnce(&mut Read<'_>, Vec<Followed>) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        self.woken().await;
        self.pass(read).await
    }

    /// Resolves once a commit of a followed kind has landed since the last wake. Safe to drop
    /// unresolved: the wake stays for the next call.
    pub async fn woken(&self) {
        self.wake.notified().await;
    }

    /// Passes over the log now. Dropped unresolved, it leaves the cursors where they were.
    pub async fn pass<T: Send + 'static>(
        &mut self,
        read: impl FnOnce(&mut Read<'_>, Vec<Followed>) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let service = Arc::clone(&self.service);
        let mut follower = self.follower.clone();
        let (follower, result) = tokio::task::spawn_blocking(move || {
            let homes = service.homes();
            let result = service.with_store(|store| {
                let mut store_read = store.read()?;
                let followed = follower.pass(&mut store_read, &homes)?;
                read(&mut store_read, followed)
            });
            (follower, result)
        })
        .await
        .map_err(|error| StoreError::Stream(format!("log pass did not finish: {error}")))?;
        if result.is_ok() {
            self.follower = follower;
        }
        result
    }

    /// Reads the store now on a blocking thread, leaving the cursors where they are.
    pub async fn read<T: Send + 'static>(
        &self,
        read: impl FnOnce(&mut Read<'_>) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let service = Arc::clone(&self.service);
        tokio::task::spawn_blocking(move || service.with_store(|store| read(&mut store.read()?)))
            .await
            .map_err(|error| StoreError::Stream(format!("read did not finish: {error}")))?
    }
}

/// Whether the store has committed a write of some kinds since a reader of them last brought
/// itself up to the log: a reader that finds nothing new skips its pass, and so the store's lock,
/// entirely.
pub struct CommitsSeen {
    dirty: Arc<std::sync::atomic::AtomicBool>,
    // The store holds the listener weakly: it lapses with this value.
    _listener: Arc<CommitListener>,
}

impl CommitsSeen {
    /// Follows commits that write `kinds`. Starts as seen, so the first pass always runs.
    pub fn follow(service: &Service, kinds: impl IntoIterator<Item = KindCode>) -> Self {
        let kinds: BTreeSet<KindCode> = kinds.into_iter().collect();
        let dirty = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let listener: Arc<CommitListener> = {
            let dirty = Arc::clone(&dirty);
            Arc::new(move |committed: &Committed<'_>| {
                if committed.touches(&kinds) {
                    dirty.store(true, std::sync::atomic::Ordering::SeqCst)
                }
            })
        };
        service.on_commit(Arc::downgrade(&listener));
        Self { dirty, _listener: listener }
    }

    /// Whether a commit of the kinds has landed since the last take. A commit landing after this call sets it
    /// again, so the pass that follows never misses one.
    pub fn take(&self) -> bool {
        self.dirty.swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    /// Puts back a commit taken by a pass that failed, so the next pass runs.
    pub fn give_back(&self) {
        self.dirty.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
