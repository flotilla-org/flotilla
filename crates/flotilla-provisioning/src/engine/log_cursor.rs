//! Reading the decided log's changes on the caller's thread (#3108): a view passes its cursors
//! under the store's lock at each listing, so it reads every commit made before it.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use flotilla_engine_store::follow::{self, Followed, Follower, Resumed};
use flotilla_resources::lifecycle_reads::CursorSince;
use flotilla_schema::KindCode;
use flotilla_schema_sqlite::stream::{CommitListener, Committed};

use super::resume::WrittenSince;

/// A [`ChangeCursor`](flotilla_resources::lifecycle_reads::ChangeCursor) over some kinds in the
/// decided log at every home the store holds, passed on the caller's thread under the store's lock.
pub struct LogCursor<C> {
    reads: super::reads::StoreReads,
    homes: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    kinds: Vec<KindCode>,
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
        let kinds: Vec<KindCode> = kinds.into_iter().collect();
        let mut follower = Follower::new(kinds.iter().copied());
        let held = homes();
        reads.read(|read| Ok(follower.start(read, &held)?)).map_err(|error| error.to_string())?;
        Ok(Self { reads, homes, kinds, follower, change, resets, _listener: listener })
    }

    /// The resources of its kinds written since `at`, by identity, and each home's sequence they
    /// were read through; leaves the cursor where it is.
    pub fn written_since(&self, at: &BTreeMap<String, u64>) -> Result<WrittenSince, String> {
        let homes = (self.homes)();
        let kinds = self.kinds.clone();
        let resumed = self.reads.read(|read| Ok(follow::since(read, kinds, &homes, at)?)).map_err(|error| error.to_string())?;
        Ok(match resumed {
            Resumed::Followed { changed, through } => Ok((changed, through)),
            Resumed::Expired(reason) => Err(reason),
        })
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

    fn position(&self) -> u64 {
        self.follower.position()
    }

    fn read_through(&self) -> BTreeMap<String, u64> {
        self.follower.cursors().clone()
    }

    fn since(&self, at: &BTreeMap<String, u64>) -> Result<CursorSince, String> {
        Ok(match self.written_since(at)? {
            Ok((changed, through)) => {
                CursorSince::Changed { written: changed.into_iter().map(|identity| (identity.name, identity.home)).collect(), through }
            }
            Err(reason) => CursorSince::Expired(reason),
        })
    }
}
