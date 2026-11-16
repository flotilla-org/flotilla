//! One listing of an engine kind's records shared by every watcher of that kind on a backend.

use std::sync::Arc;

use flotilla_resources::engine_records::{KindRecords, Record};
use flotilla_schema::reads::{attributed, caller};
use futures::{FutureExt, StreamExt};

use crate::{port_reads, Resource};

/// The caller the feeds' listings after the first count for: the first counts for the watcher
/// that started the feed.
const FEED: &str = "engine feed";

/// Every home's records of one kind as of a pass, or why the pass could not list them.
pub(crate) type EngineSnapshot<R> = Arc<Result<Vec<Record<R>>, String>>;

/// A kind's records, listed again once for each batch of changes and published to its watchers.
/// The listing task stops when the last watcher drops the feed.
#[derive(Debug)]
pub(crate) struct EngineFeed<R: Resource> {
    snapshots: tokio::sync::watch::Receiver<EngineSnapshot<R>>,
    catch_ups: tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl<R: Resource> Drop for EngineFeed<R> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl<R: Resource> EngineFeed<R> {
    /// Subscribes to `records`' changes, then lists them, so no change between the two is lost.
    pub(crate) async fn start(records: Arc<dyn KindRecords<R>>) -> Result<Self, String> {
        let mut changes = records.changes().await?;
        let initial = port_reads::sources(&*records).await?;
        let (publish, snapshots) = tokio::sync::watch::channel(Arc::new(Ok(initial)));
        let (catch_ups, mut requests) = tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<()>>(16);
        let task = tokio::spawn(attributed(caller(FEED), async move {
            loop {
                let caught_up = tokio::select! {
                    tick = changes.next() => match tick {
                        Some(()) => None,
                        None => return,
                    },
                    request = requests.recv() => match request {
                        Some(caught_up) => Some(caught_up),
                        None => return,
                    },
                };
                // A tick says only that something changed: the ticks already queued behind it are
                // answered by the same listing, not one listing each. A catch-up lists only when
                // a tick is queued.
                let mut changed = caught_up.is_none();
                let mut ended = false;
                while let Some(next) = changes.next().now_or_never() {
                    match next {
                        Some(()) => changed = true,
                        None => {
                            ended = true;
                            break;
                        }
                    }
                }
                if changed {
                    publish.send_replace(Arc::new(port_reads::sources(&*records).await));
                }
                if let Some(caught_up) = caught_up {
                    caught_up.send(()).ok();
                }
                if ended {
                    return;
                }
            }
        }));
        Ok(Self { snapshots, catch_ups, task })
    }

    /// The latest listing, marked seen, and a receiver of the ones after it.
    pub(crate) fn subscribe(&self) -> (EngineSnapshot<R>, tokio::sync::watch::Receiver<EngineSnapshot<R>>) {
        let mut snapshots = self.snapshots.clone();
        let current = snapshots.borrow_and_update().clone();
        (current, snapshots)
    }

    /// Returns once the published listing holds every change ticked before the call: a write's
    /// tick is queued when the write commits, so a reader that saw the write reads it. Lists
    /// again only when a tick is queued.
    pub(crate) async fn catch_up(&self) {
        let (caught_up, done) = tokio::sync::oneshot::channel();
        if self.catch_ups.send(caught_up).await.is_ok() {
            done.await.ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };

    use flotilla_resources::{ReadWatchEvent, Vessel, VesselSpec, VesselStatus};
    use futures::stream::BoxStream;

    use super::*;

    /// Vessel records served as the engine serves them, counting each listing and each
    /// subscription to their changes.
    #[derive(Debug, Default)]
    struct Counted {
        listings: AtomicUsize,
        /// How many times each listing yields before it answers, as a listing read from storage does.
        yields: AtomicUsize,
        records: Mutex<Vec<Record<Vessel>>>,
        subscribers: Mutex<Vec<futures::channel::mpsc::UnboundedSender<()>>>,
    }

    impl Counted {
        fn put(&self, name: &str, version: u64) {
            let record = Record::<Vessel> {
                name: name.into(),
                home: "kiwi".into(),
                version,
                created_at: chrono::DateTime::UNIX_EPOCH,
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                spec: VesselSpec {
                    convoy_ref: "c".into(),
                    vessel_name: "w".into(),
                    fulfilment_kind_ref: "k".into(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
                status: None,
            };
            let mut records = self.records.lock().expect("records");
            records.retain(|held| held.name != name);
            records.push(record);
        }

        fn tick(&self) {
            for subscriber in self.subscribers.lock().expect("subscribers").iter() {
                subscriber.unbounded_send(()).expect("tick");
            }
        }
    }

    #[async_trait::async_trait]
    impl KindRecords<Vessel> for Counted {
        fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
            Vec::new()
        }
        fn home(&self) -> String {
            "kiwi".into()
        }
        async fn get(&self, name: &str) -> Result<Option<Record<Vessel>>, String> {
            Ok(self.records.lock().expect("records").iter().find(|record| record.name == name).cloned())
        }
        async fn list(&self) -> Result<Vec<Record<Vessel>>, String> {
            self.sources().await
        }
        async fn sources(&self) -> Result<Vec<Record<Vessel>>, String> {
            self.listings.fetch_add(1, Ordering::SeqCst);
            for _ in 0..self.yields.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            Ok(self.records.lock().expect("records").clone())
        }
        async fn apply(
            &self,
            _name: &str,
            _labels: &BTreeMap<String, String>,
            _annotations: &BTreeMap<String, String>,
            _spec: &VesselSpec,
        ) -> Result<Record<Vessel>, String> {
            Err("read only".into())
        }
        async fn set_status(&self, _name: &str, _status: &VesselStatus) -> Result<Record<Vessel>, String> {
            Err("read only".into())
        }
        async fn delete(&self, _name: &str) -> Result<(), String> {
            Err("read only".into())
        }
        async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
            let (sender, receiver) = futures::channel::mpsc::unbounded();
            self.subscribers.lock().expect("subscribers").push(sender);
            Ok(receiver.boxed())
        }
    }

    fn named(event: &ReadWatchEvent<Vessel>) -> (&'static str, String) {
        match event {
            ReadWatchEvent::Added(read) => ("added", read.object.metadata.name.clone()),
            ReadWatchEvent::Modified(read) => ("modified", read.object.metadata.name.clone()),
            ReadWatchEvent::Deleted(read) => ("deleted", read.object.metadata.name.clone()),
            ReadWatchEvent::DeletedByName { .. } => ("deleted by name", String::new()),
        }
    }

    // Behaviour: every watcher of a kind on one backend shares one subscription and one listing
    // per batch of changes, and each still sees every change as its own events.
    #[tokio::test]
    async fn watchers_of_a_kind_share_one_listing_per_batch_of_changes() {
        let records = Arc::new(Counted::default());
        records.put("v#1", 1);
        let backend = crate::test_store::memory_backend();
        backend.install_engine_kind::<Vessel>(records.clone());
        let watch = || async { backend.definitions::<Vessel>("flotilla").engine_watch().await.expect("served").expect("watch") };
        let (first_initial, mut first) = watch().await;
        let (second_initial, mut second) = watch().await;
        assert_eq!(first_initial.len(), 1);
        assert_eq!(second_initial.len(), 1);
        assert_eq!(records.subscribers.lock().expect("subscribers").len(), 1, "one subscription for both watchers");
        assert_eq!(records.listings.load(Ordering::SeqCst), 1, "one listing for both watchers");

        // Three commits' ticks queued together are answered by one listing.
        records.put("v#1", 2);
        records.put("v#2", 1);
        records.tick();
        records.tick();
        records.tick();
        for watch in [&mut first, &mut second] {
            let mut seen =
                vec![named(&watch.next().await.expect("event").expect("read")), named(&watch.next().await.expect("event").expect("read"))];
            seen.sort();
            assert_eq!(seen, vec![("added", "v#2".to_owned()), ("modified", "v#1".to_owned())]);
        }
        assert_eq!(records.listings.load(Ordering::SeqCst), 2);

        // The feed ends with its last watcher; a later watcher starts its own.
        drop((first, second));
        tokio::task::yield_now().await;
        let (later, _later) = watch().await;
        assert_eq!(later.len(), 2);
        assert_eq!(records.listings.load(Ordering::SeqCst), 3);
    }

    // Behaviour: a view of an engine kind reads every change committed before the read, without
    // waiting for the feed's own pass; a read with nothing changed lists nothing again.
    #[tokio::test(flavor = "current_thread")]
    async fn a_view_reads_the_engine_changes_committed_before_the_read() {
        let records = Arc::new(Counted::default());
        records.put("v#1", 1);
        let backend = crate::test_store::memory_backend();
        backend.install_engine_kind::<Vessel>(records.clone());
        let view = crate::WatchView::<Vessel>::start(backend, "flotilla").await.expect("view");
        records.yields.store(8, Ordering::SeqCst);
        let names = || async {
            let mut names = view.list().await.expect("list").items.into_iter().map(|read| read.object.metadata.name).collect::<Vec<_>>();
            names.sort();
            names
        };
        records.put("v#2", 1);
        records.tick();
        assert_eq!(names().await, ["v#1", "v#2"]);
        let listings = records.listings.load(Ordering::SeqCst);
        assert_eq!(names().await, ["v#1", "v#2"]);
        assert_eq!(records.listings.load(Ordering::SeqCst), listings, "an unchanged kind is not listed again");
    }
}
