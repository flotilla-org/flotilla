//! One reading of an engine kind's records shared by every watcher of that kind on a backend.
//!
//! The feed lists the kind once, at a position, and then follows it: each batch of changes asks
//! the port what changed since the feed's position and reads only those names again, by name. A
//! position the port can no longer resume from is listed afresh. Watchers take the names each
//! batch changed and convert only those records.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use flotilla_resources::engine_records::{KindRecords, LogPosition, Record, Since};
use flotilla_schema::reads::{attributed, caller};
use futures::{FutureExt, StreamExt};

use crate::{port_reads, Resource};

/// The caller the feeds' reads after the first listing count for: the first counts for the
/// watcher that started the feed.
const FEED: &str = "engine feed";

/// How many batches a feed keeps for watchers that have not yet taken them; one that falls further
/// behind compares every record again.
const BATCHES: usize = 256;

/// Every home's records of a kind, by name, in the order the port gives them.
pub(crate) type Copies<R> = BTreeMap<String, Vec<Record<R>>>;

/// What one pass of the feed did.
#[derive(Debug)]
pub(crate) enum Batch {
    /// These names' records were read again.
    Changed(Arc<BTreeSet<String>>),
    /// Every record was listed afresh.
    Relisted,
    /// The pass could not read what changed; the records stand as they were.
    Failed(String),
}

/// The feed's records as of its position, and the batches that brought them there.
#[derive(Debug)]
pub(crate) struct FeedState<R: Resource> {
    /// The records, every home's copies by name.
    pub(crate) records: Copies<R>,
    /// Where the records were read: a position the port resumes from.
    pub(crate) position: LogPosition,
    /// The serial of the last batch: 0 for the first listing.
    pub(crate) serial: u64,
    /// The latest batches, each with its serial.
    batches: VecDeque<(u64, Batch)>,
}

impl<R: Resource> FeedState<R> {
    /// The batches after `serial`; `None` where some are no longer held.
    pub(crate) fn after(&self, serial: u64) -> Option<impl Iterator<Item = &Batch>> {
        let oldest = self.batches.front().map_or(self.serial + 1, |(first, _)| *first);
        (serial + 1 >= oldest).then(|| self.batches.iter().filter(move |(held, _)| *held > serial).map(|(_, batch)| batch))
    }

    fn push(&mut self, batch: Batch) {
        self.serial += 1;
        self.batches.push_back((self.serial, batch));
        if self.batches.len() > BATCHES {
            self.batches.pop_front();
        }
    }
}

/// A kind's records, followed for each batch of changes and published to its watchers. The
/// following task stops when the last watcher drops the feed.
#[derive(Debug)]
pub(crate) struct EngineFeed<R: Resource> {
    state: Arc<Mutex<FeedState<R>>>,
    serials: tokio::sync::watch::Receiver<u64>,
    catch_ups: tokio::sync::mpsc::Sender<CatchUp>,
    task: tokio::task::JoinHandle<()>,
}

impl<R: Resource> Drop for EngineFeed<R> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl<R: Resource> EngineFeed<R> {
    /// Subscribes to `records`' changes, then lists them with their position, so no change
    /// between the two is lost.
    pub(crate) async fn start(records: Arc<dyn KindRecords<R>>) -> Result<Self, String> {
        let mut changes = records.changes().await?;
        let (listed, position) = port_reads::positioned(&*records).await?;
        let state = Arc::new(Mutex::new(FeedState { records: by_name(listed), position, serial: 0, batches: VecDeque::new() }));
        let (publish, serials) = tokio::sync::watch::channel(0);
        let (catch_ups, mut requests) = tokio::sync::mpsc::channel::<CatchUp>(16);
        let following = Arc::clone(&state);
        let task = tokio::spawn(attributed(caller(FEED), async move {
            loop {
                let (caught_up, forced) = tokio::select! {
                    tick = changes.next() => match tick {
                        Some(()) => (None, false),
                        None => return,
                    },
                    request = requests.recv() => match request {
                        Some(CatchUp { done, forced }) => (Some(done), forced),
                        None => return,
                    },
                };
                // A tick says only that something changed: the ticks already queued behind it are
                // answered by the same pass, not one pass each. A catch-up passes only when a tick
                // is queued, unless it is forced.
                let mut changed = caught_up.is_none() || forced;
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
                    if let Some(serial) = follow(&*records, &following).await {
                        publish.send_replace(serial);
                    }
                }
                if let Some(caught_up) = caught_up {
                    caught_up.send(()).ok();
                }
                if ended {
                    return;
                }
            }
        }));
        Ok(Self { state, serials, catch_ups, task })
    }

    /// The feed's state, for a watcher to read its records and batches.
    pub(crate) fn state(&self) -> MutexGuard<'_, FeedState<R>> {
        locked(&self.state)
    }

    /// A receiver of each batch's serial, the latest marked seen.
    pub(crate) fn serials(&self) -> tokio::sync::watch::Receiver<u64> {
        let mut serials = self.serials.clone();
        serials.borrow_and_update();
        serials
    }

    /// Returns once the published records hold every change ticked before the call: a write's
    /// tick is queued when the write commits, so a reader that saw the write reads it. Passes
    /// again only when a tick is queued.
    pub(crate) async fn catch_up(&self) {
        self.request(false).await;
    }

    /// Returns once the published records hold every change committed before the call, whether
    /// or not its tick has reached the feed: a pass reads the log from the feed's position.
    pub(crate) async fn read_through_now(&self) {
        self.request(true).await;
    }

    async fn request(&self, forced: bool) {
        let (done, caught_up) = tokio::sync::oneshot::channel();
        if self.catch_ups.send(CatchUp { done, forced }).await.is_ok() {
            caught_up.await.ok();
        }
    }
}

/// A reader's request for the feed to hold every change before it.
#[derive(Debug)]
struct CatchUp {
    done: tokio::sync::oneshot::Sender<()>,
    /// Pass though no tick is queued.
    forced: bool,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `listed` by name, each name's copies in the order listed.
fn by_name<R: Resource>(listed: Vec<Record<R>>) -> Copies<R> {
    let mut copies = Copies::<R>::new();
    for record in listed {
        copies.entry(record.name.clone()).or_default().push(record);
    }
    copies
}

/// Reads what changed since `state`'s position into it: each changed name's copies by name, or
/// every record where the position cannot be resumed from. Returns the batch's serial; none
/// where nothing changed.
async fn follow<R: Resource>(records: &dyn KindRecords<R>, state: &Mutex<FeedState<R>>) -> Option<u64> {
    let position = locked(state).position.clone();
    let batch = match records.changed_since(&position).await {
        Ok(Since::Changed { names, through }) if names.is_empty() => {
            locked(state).position = through;
            return None;
        }
        Ok(Since::Changed { names, through }) => {
            let mut read = Vec::with_capacity(names.len());
            let mut failed = None;
            for name in &names {
                match port_reads::sources_named(records, name).await {
                    Ok(copies) => read.push((name.clone(), copies)),
                    Err(error) => {
                        failed = Some(error);
                        break;
                    }
                }
            }
            match failed {
                Some(error) => Batch::Failed(error),
                None => {
                    let mut state = locked(state);
                    for (name, copies) in read {
                        if copies.is_empty() {
                            state.records.remove(&name);
                        } else {
                            state.records.insert(name, copies);
                        }
                    }
                    state.position = through;
                    state.push(Batch::Changed(Arc::new(names)));
                    return Some(state.serial);
                }
            }
        }
        Ok(Since::Expired(reason)) => {
            tracing::info!(kind = R::API_PATHS.kind, %reason, "engine feed: its position expired; listing afresh");
            match port_reads::positioned(records).await {
                Ok((listed, position)) => {
                    let mut state = locked(state);
                    state.records = by_name(listed);
                    state.position = position;
                    state.push(Batch::Relisted);
                    return Some(state.serial);
                }
                Err(error) => Batch::Failed(error),
            }
        }
        Err(error) => Batch::Failed(error),
    };
    let mut state = locked(state);
    state.push(batch);
    Some(state.serial)
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

    use flotilla_resources::{
        engine_records::{LogPosition, Since},
        FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, ReadWatchEvent, Vessel, VesselSpec,
    };
    use futures::stream::BoxStream;

    use super::*;

    /// Records served as the engine serves them, each write moving a log position, counting each
    /// listing and each subscription to their changes.
    struct Counted<R: Resource> {
        listings: AtomicUsize,
        /// How many times each listing yields before it answers, as a listing read from storage does.
        yields: AtomicUsize,
        records: Mutex<Vec<Record<R>>>,
        /// Each write's name, in order: the log the position counts.
        written: Mutex<Vec<String>>,
        subscribers: Mutex<Vec<futures::channel::mpsc::UnboundedSender<()>>>,
    }

    impl<R: Resource> std::fmt::Debug for Counted<R> {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.debug_struct("Counted").field("written", &self.written).finish_non_exhaustive()
        }
    }

    impl<R: Resource> Default for Counted<R> {
        fn default() -> Self {
            Self {
                listings: AtomicUsize::default(),
                yields: AtomicUsize::default(),
                records: Mutex::default(),
                written: Mutex::default(),
                subscribers: Mutex::default(),
            }
        }
    }

    impl Counted<Vessel> {
        fn put(&self, name: &str, version: u64) {
            self.put_spec(
                name,
                version,
                VesselSpec {
                    convoy_ref: "c".into(),
                    vessel_name: "w".into(),
                    fulfilment_kind_ref: "k".into(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
            );
        }
    }

    impl<R: Resource> Counted<R> {
        fn put_spec(&self, name: &str, version: u64, spec: R::Spec) {
            let record = Record::<R> {
                name: name.into(),
                home: "kiwi".into(),
                version,
                created_at: chrono::DateTime::UNIX_EPOCH,
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                spec,
                status: None,
            };
            let mut records = self.records.lock().expect("records");
            records.retain(|held| held.name != name);
            records.push(record);
            self.written.lock().expect("written").push(name.into());
        }

        fn tick(&self) {
            for subscriber in self.subscribers.lock().expect("subscribers").iter() {
                subscriber.unbounded_send(()).expect("tick");
            }
        }

        fn at(&self) -> LogPosition {
            LogPosition::logged(BTreeMap::from([("kiwi".to_owned(), self.written.lock().expect("written").len() as u64)]))
        }
    }

    #[async_trait::async_trait]
    impl<R: Resource> KindRecords<R> for Counted<R> {
        fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
            Vec::new()
        }
        fn home(&self) -> String {
            "kiwi".into()
        }
        async fn get(&self, name: &str) -> Result<Option<Record<R>>, String> {
            Ok(self.records.lock().expect("records").iter().find(|record| record.name == name).cloned())
        }
        async fn list(&self) -> Result<Vec<Record<R>>, String> {
            self.sources().await
        }
        async fn sources(&self) -> Result<Vec<Record<R>>, String> {
            self.listings.fetch_add(1, Ordering::SeqCst);
            for _ in 0..self.yields.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            Ok(self.records.lock().expect("records").clone())
        }
        async fn positioned(&self) -> Result<(Vec<Record<R>>, LogPosition), String> {
            let at = self.at();
            Ok((self.sources().await?, at))
        }
        async fn position(&self) -> Result<LogPosition, String> {
            Ok(self.at())
        }
        async fn changed_since(&self, position: &LogPosition) -> Result<Since, String> {
            let from = position.homes.get("kiwi").copied().unwrap_or(0) as usize;
            let written = self.written.lock().expect("written");
            let names = written.get(from..).ok_or("ahead of the log")?.iter().cloned().collect();
            Ok(Since::Changed { names, through: LogPosition::logged(BTreeMap::from([("kiwi".to_owned(), written.len() as u64)])) })
        }
        async fn copies(&self, name: &str) -> Result<Vec<Record<R>>, String> {
            Ok(self.get(name).await?.into_iter().collect())
        }
        async fn apply(
            &self,
            _name: &str,
            _labels: &BTreeMap<String, String>,
            _annotations: &BTreeMap<String, String>,
            _spec: &R::Spec,
        ) -> Result<Record<R>, String> {
            Err("read only".into())
        }
        async fn set_status(&self, _name: &str, _status: &R::Status) -> Result<Record<R>, String> {
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

    // Behaviour: every watcher of a kind on one backend shares one subscription and one listing,
    // after which each batch of changes is read by name; each watcher still sees every change as
    // its own events.
    #[tokio::test]
    async fn watchers_of_a_kind_share_one_listing_and_read_each_batch_by_name() {
        let records = Arc::new(Counted::<Vessel>::default());
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

        // Three commits' ticks queued together are answered by one pass, which reads only the
        // names written.
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
        assert_eq!(records.listings.load(Ordering::SeqCst), 1, "the batch is read by name, not listed");

        // The feed ends with its last watcher; a later watcher starts its own.
        drop((first, second));
        tokio::task::yield_now().await;
        let (later, _later) = watch().await;
        assert_eq!(later.len(), 2);
        assert_eq!(records.listings.load(Ordering::SeqCst), 2);
    }

    // Behaviour: a view of an engine kind reads every change committed before the read, without
    // waiting for the feed's own pass; a read with nothing changed reads nothing again.
    #[tokio::test(flavor = "current_thread")]
    async fn a_view_reads_the_engine_changes_committed_before_the_read() {
        let records = Arc::new(Counted::<Vessel>::default());
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

    // Behaviour (#3108): the feed lists its kind once, at its position; one change among many
    // records then costs a keyed read of the changed name, not a listing of every record. No
    // other test of this crate feeds FulfilmentKinds, so the feed's counts of them are this test's.
    #[tokio::test]
    async fn one_change_among_many_records_costs_a_keyed_read() {
        const RECORDS: usize = 50;
        let kind = |host: &str| {
            FulfilmentKindSpec::builder()
                .host_ref(host.into())
                .pool("passthrough".into())
                .realisation(FulfilmentRealisation::HostDirect)
                .build()
        };
        let records = Arc::new(Counted::<FulfilmentKind>::default());
        for index in 0..RECORDS {
            records.put_spec(&format!("kind-{index}"), 1, kind("kiwi"));
        }
        let fed = || {
            flotilla_schema::reads::callers()
                .get(FEED)
                .and_then(|counts| counts.get(&format!("FulfilmentKind{}", port_reads::PORT)).copied())
                .unwrap_or_default()
        };
        let before = fed();
        let feed = EngineFeed::start(records.clone() as Arc<dyn KindRecords<FulfilmentKind>>).await.expect("feed");
        // The opening listing runs on the caller here, not the feed's task.
        let mut serials = feed.serials();
        let opened = fed();

        records.put_spec("kind-7", 2, kind("feta"));
        records.tick();
        tokio::time::timeout(std::time::Duration::from_secs(5), serials.changed()).await.expect("a batch").expect("feed open");
        let change = fed().since(&opened);
        assert_eq!(opened.since(&before).whole_kind, 0, "the opening listing is the starter's");
        assert_eq!((change.keyed, change.whole_kind, change.rows), (1, 0, 1), "one change reads one name: {change:?}");
        assert_eq!(feed.state().records["kind-7"][0].spec.host_ref, "feta");
    }
}
