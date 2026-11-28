//! Following the decided log from async code: a pass runs when the store commits a write of a
//! followed kind, never on a timer, and reads the log on a blocking thread, so the store's lock
//! is never taken on an async worker. A commit of other kinds, or a keepalive, wakes nothing.

use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use crate::follow::{Followed, Follower};
use flotilla_schema::{reads::carry, KindCode};
use flotilla_schema_sqlite::{
    stream::{CommitListener, Committed, HeardListener, Service},
    Read, StoreError,
};
use tokio::sync::Notify;

/// A [`Follower`] of some kinds over every home's log, woken by the store's commits.
pub struct EngineWatch {
    service: Arc<Service>,
    follower: Follower,
    wake: Arc<Notify>,
    resets: Arc<AtomicBool>,
    // The store holds the listeners weakly: they lapse with this watch.
    _listener: Arc<CommitListener>,
    _hearing: Option<Arc<HeardListener>>,
}

impl EngineWatch {
    /// Follows `kinds` from each home's current sequence, woken by commits that write them. The
    /// listener is installed first, so a commit between it and the start still wakes the first
    /// pass.
    pub fn start(service: Arc<Service>, kinds: impl IntoIterator<Item = KindCode>) -> Result<Self, StoreError> {
        Self::start_waking(service, kinds, false)
    }

    /// As [`EngineWatch::start`], also woken when another home is heard from, a keepalive that
    /// writes nothing included, for a reader of each home's heard time.
    pub fn start_hearing(service: Arc<Service>, kinds: impl IntoIterator<Item = KindCode>) -> Result<Self, StoreError> {
        Self::start_waking(service, kinds, true)
    }

    fn start_waking(service: Arc<Service>, kinds: impl IntoIterator<Item = KindCode>, hearing: bool) -> Result<Self, StoreError> {
        let kinds: BTreeSet<KindCode> = kinds.into_iter().collect();
        let wake = Arc::new(Notify::new());
        let resets = Arc::new(AtomicBool::new(false));
        let listener: Arc<CommitListener> = {
            let (wake, kinds, resets) = (Arc::clone(&wake), kinds.clone(), Arc::clone(&resets));
            Arc::new(move |committed: &Committed<'_>| {
                if committed.rescan {
                    resets.store(true, Ordering::SeqCst);
                }
                if committed.touches(&kinds) || (hearing && !committed.heard.is_empty()) {
                    wake.notify_one()
                }
            })
        };
        service.on_commit(Arc::downgrade(&listener));
        let hearing = hearing.then(|| {
            let wake = Arc::clone(&wake);
            let heard: Arc<HeardListener> = Arc::new(move |_: &str| wake.notify_one());
            service.on_heard(Arc::downgrade(&heard));
            heard
        });
        let mut follower = Follower::new(kinds);
        let homes = service.homes();
        service.with_store(|store| follower.start(&mut store.read()?, &homes))?;
        Ok(Self { service, follower, wake, resets, _listener: listener, _hearing: hearing })
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
        let reset = self.resets.swap(false, Ordering::SeqCst);
        let (follower, result) = tokio::task::spawn_blocking(carry(move || {
            let homes = service.homes();
            let result = service.with_store(|store| {
                let mut store_read = store.read()?;
                let followed = reset_or_pass(&mut follower, &mut store_read, &homes, reset)?;
                read(&mut store_read, followed)
            });
            (follower, result)
        }))
        .await
        .map_err(|error| StoreError::Stream(format!("log pass did not finish: {error}")))?;
        if result.is_ok() {
            self.follower = follower;
        } else if reset {
            self.resets.store(true, Ordering::SeqCst);
        }
        result
    }

    /// Reads the store now on a blocking thread, leaving the cursors where they are.
    pub async fn read<T: Send + 'static>(
        &self,
        read: impl FnOnce(&mut Read<'_>) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let service = Arc::clone(&self.service);
        tokio::task::spawn_blocking(carry(move || service.with_store(|store| read(&mut store.read()?))))
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

    /// The flag and the listener that sets it, for a reader that holds them itself.
    pub fn into_parts(self) -> (Arc<std::sync::atomic::AtomicBool>, Arc<CommitListener>) {
        (self.dirty, self._listener)
    }
}

/// One follower of some kinds shared by every watcher of them on this store: each watcher gets a
/// tick for each pass that found the kinds changed, from the moment it subscribed, and reads the
/// records again on it. Ticks a watcher has not yet taken fold into one. The follower runs while
/// any watcher holds its stream, and starts again for the next one after the last has gone.
pub struct SharedChanges {
    service: Arc<Service>,
    kinds: Vec<KindCode>,
    what: &'static str,
    running: Running,
}

/// The shared follower's sender while it runs; it clears this itself, under the lock, as it stops.
type Running = Arc<std::sync::Mutex<Option<tokio::sync::watch::Sender<u64>>>>;

impl std::fmt::Debug for SharedChanges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedChanges").field("kinds", &self.kinds).field("what", &self.what).finish()
    }
}

impl SharedChanges {
    /// Shares changes to `kinds`, naming them `what` in the warning a failed pass logs.
    pub fn new(service: Arc<Service>, kinds: impl IntoIterator<Item = KindCode>, what: &'static str) -> Self {
        Self { service, kinds: kinds.into_iter().collect(), what, running: Running::default() }
    }

    /// A tick for each pass that finds the kinds changed after this call.
    pub fn subscribe(&self) -> Result<futures::stream::BoxStream<'static, ()>, StoreError> {
        let mut running = self.running.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let receiver = match running.as_ref() {
            Some(sender) => sender.subscribe(),
            None => {
                // Started before it is shared: a commit from here on wakes its first pass.
                let watch = EngineWatch::start(Arc::clone(&self.service), self.kinds.iter().copied())?;
                let (sender, receiver) = tokio::sync::watch::channel(0u64);
                tokio::spawn(follow(watch, sender.clone(), Arc::clone(&self.running), self.what));
                *running = Some(sender);
                receiver
            }
        };
        Ok(Box::pin(futures::stream::unfold(receiver, |mut receiver| async move {
            receiver.changed().await.ok()?;
            Some(((), receiver))
        })))
    }
}

async fn follow(mut watch: EngineWatch, sender: tokio::sync::watch::Sender<u64>, running: Running, what: &'static str) {
    loop {
        tokio::select! {
            () = sender.closed() => {
                // A watcher subscribing as the last one went keeps it running.
                let mut running = running.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if sender.receiver_count() == 0 {
                    *running = None;
                    return;
                }
            }
            // A pass that fails is tried again at the next commit; the watchers see no tick.
            passed = async {
                watch.woken().await;
                tick(&mut watch, &sender).await
            } => if let Err(error) = passed {
                tracing::warn!(%error, what, "change stream could not read the log; retrying")
            },
        }
    }
}

/// One pass of `watch`, counted on `sender` only when it found the followed kinds changed.
async fn tick(watch: &mut EngineWatch, sender: &tokio::sync::watch::Sender<u64>) -> Result<(), StoreError> {
    if watch.pass(|_, followed| Ok(!followed.is_empty())).await? {
        sender.send_modify(|passes| *passes += 1);
    }
    Ok(())
}

/// The pass of `follower` over `homes`, or, where a home was `reset` since it last passed (a
/// snapshot cut or a replica reset, which can leave the home's sequence where it was), a rescan of
/// every home, followed from now.
pub fn reset_or_pass(follower: &mut Follower, read: &mut Read<'_>, homes: &[String], reset: bool) -> Result<Vec<Followed>, StoreError> {
    if !reset {
        return follower.pass(read, homes);
    }
    follower.start(read, homes)?;
    Ok(homes.iter().cloned().map(Followed::Rescan).collect())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flotilla_schema::{Address, Kind, Timestamp};
    use flotilla_schema_sqlite::stream::Service;
    use flotilla_state_machines::{
        machines::{
            agent_session::{AgentSession, MemoryAgentSessionWorld},
            message::{MemoryMessageWorld, Message, MessageBody, MessageSpec, Relation},
        },
        Home, Resource,
    };

    use super::{tick, EngineWatch};
    use crate::{registry, MessageLog, StoreLog};

    const HOME: &str = "alpha";

    // Behaviour: a change stream counts a pass only when it finds its kinds changed. Each pass is
    // driven here by hand, so whether a pass over another kind's entries ticks is read directly,
    // with no wait on the follower task.
    #[tokio::test]
    async fn a_pass_over_another_kinds_entries_does_not_tick() {
        let service = Arc::new(Service::open(std::path::Path::new(":memory:"), HOME, registry().expect("registry")).expect("service"));
        let mut watch = EngineWatch::start(Arc::clone(&service), [Resource::<Message>::CODE]).expect("watch");
        let sender = tokio::sync::watch::channel(0u64).0;

        let mut sessions = Home::<AgentSession, StoreLog>::with_log(HOME, StoreLog::new(service.store())).expect("session home");
        sessions
            .create(
                "session",
                flotilla_state_machines::machines::agent_session::AgentSessionRequest::of(
                    flotilla_schema::Ref::new("convoy", "home"),
                    "coder",
                ),
                &MemoryAgentSessionWorld::default(),
                Timestamp(1),
            )
            .expect("session");
        tick(&mut watch, &sender).await.expect("pass");
        assert_eq!(*sender.borrow(), 0, "an AgentSession's entries are no Message: the pass does not tick");

        let inbox: Address = "project:fleet/convoy:demo/role:coder".parse().expect("inbox");
        let name = Address::message(&inbox, "first").expect("message address").as_str().to_owned();
        let spec = MessageSpec::new(
            "project:fleet/role:turn-rules".parse().expect("sender"),
            inbox,
            Relation::System,
            MessageBody::text("take a turn").expect("small body"),
        );
        let mut messages = Home::<Message, MessageLog>::with_log(HOME, MessageLog::new(service.store())).expect("message home");
        messages.create(&name, spec, &MemoryMessageWorld::default(), Timestamp(2)).expect("message");
        tick(&mut watch, &sender).await.expect("pass");
        assert_eq!(*sender.borrow(), 1, "a pass over a Message's entries ticks");
    }
}
