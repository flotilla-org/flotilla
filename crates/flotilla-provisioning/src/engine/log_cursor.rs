//! Reading the decided log's changes on the caller's thread (#3108): a view passes its cursors
//! under the store's lock at each listing, so it reads every commit made before it.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use flotilla_engine_store::follow::{Followed, Follower};
use flotilla_schema::KindCode;
use flotilla_schema_sqlite::stream::{CommitListener, Committed};

/// A [`ChangeCursor`](flotilla_resources::lifecycle_reads::ChangeCursor) over some kinds in the
/// decided log at every home the store holds, passed on the caller's thread under the store's lock.
pub struct LogCursor<C> {
    reads: super::reads::StoreReads,
    homes: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    follower: Follower,
    change: fn(Followed) -> C,
    resets: Arc<AtomicBool>,
    // The store holds the listener weakly: it lapses with this cursor.
    _listener: Arc<CommitListener>,
}

impl<C> LogCursor<C> {
    /// Follows `kinds` from each home's current sequence, each followed write told as `change`.
    pub fn start(
        reads: super::reads::StoreReads,
        homes: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
        kinds: impl IntoIterator<Item = KindCode>,
        change: fn(Followed) -> C,
    ) -> Result<Self, String> {
        let resets = Arc::new(AtomicBool::new(false));
        let listener: Arc<CommitListener> = {
            let resets = Arc::clone(&resets);
            Arc::new(move |committed: &Committed<'_>| {
                if committed.rescan {
                    resets.store(true, Ordering::SeqCst);
                }
            })
        };
        reads.on_commit(Arc::downgrade(&listener)).map_err(|error| error.to_string())?;
        let mut follower = Follower::new(kinds);
        let held = homes();
        reads.read(|read| Ok(follower.start(read, &held)?)).map_err(|error| error.to_string())?;
        Ok(Self { reads, homes, follower, change, resets, _listener: listener })
    }
}

impl<C: Send> flotilla_resources::lifecycle_reads::ChangeCursor<C> for LogCursor<C> {
    fn pass(&mut self) -> Result<Vec<C>, String> {
        let homes = (self.homes)();
        let reset = self.resets.swap(false, Ordering::SeqCst);
        let follower = &mut self.follower;
        let followed = self.reads.read(|read| Ok(flotilla_engine_store::watch::reset_or_pass(follower, read, &homes, reset)?));
        if followed.is_err() && reset {
            self.resets.store(true, Ordering::SeqCst);
        }
        Ok(followed.map_err(|error| error.to_string())?.into_iter().map(self.change).collect())
    }
}
