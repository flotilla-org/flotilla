//! Following the decided log from async code: a pass runs when the store commits, never on a
//! timer, and reads the log on a blocking thread, so the store's lock is never taken on an async
//! worker.

use std::sync::Arc;

use flotilla_engine_store::follow::{Followed, Follower};
use flotilla_schema::KindCode;
use flotilla_schema_sqlite::{
    stream::{CommitListener, Service},
    Read, StoreError,
};
use tokio::sync::Notify;

/// A [`Follower`] of some kinds over every home's log, woken by the store's commits.
pub(crate) struct LogWatch {
    service: Arc<Service>,
    follower: Follower,
    wake: Arc<Notify>,
    // The store holds the listener weakly: it lapses with this watch.
    _listener: Arc<CommitListener>,
}

impl LogWatch {
    /// Follows `kinds` from each home's current sequence. The listener is installed first, so a
    /// commit between it and the start still wakes the first pass.
    pub(crate) fn start(service: Arc<Service>, kinds: impl IntoIterator<Item = KindCode>) -> Result<Self, StoreError> {
        let wake = Arc::new(Notify::new());
        let listener: Arc<CommitListener> = {
            let wake = Arc::clone(&wake);
            Arc::new(move || wake.notify_one())
        };
        service.on_commit(Arc::downgrade(&listener));
        let mut follower = Follower::new(kinds);
        let homes = service.homes();
        service.with_store(|store| follower.start(&mut store.read()?, &homes))?;
        Ok(Self { service, follower, wake, _listener: listener })
    }

    /// Waits for the next commit, then passes over the log and hands what it found to `read`,
    /// in the same read. A pass that fails is tried again at the next commit.
    pub(crate) async fn next<T: Send + 'static>(
        &mut self,
        read: impl FnOnce(&mut Read<'_>, Vec<Followed>) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        self.woken().await;
        self.pass(read).await
    }

    /// Resolves once a commit has landed since the last wake. Safe to drop unresolved: the wake
    /// stays for the next call.
    pub(crate) async fn woken(&self) {
        self.wake.notified().await;
    }

    /// Passes over the log now. Dropped unresolved, it leaves the cursors where they were.
    pub(crate) async fn pass<T: Send + 'static>(
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
}
