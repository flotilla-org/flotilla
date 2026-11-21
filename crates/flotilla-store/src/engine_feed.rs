//! One reading of an engine kind's records shared by every watcher of that kind on a backend.
//!
//! The feed lists the kind once, at a position, and then follows it: each batch of changes asks
//! the port what changed since the feed's position and reads only those names again, by name. A
//! position the port can no longer resume from is listed afresh. Watchers take the names each
//! batch changed and convert only those records, as that batch read them: a watcher that takes
//! several batches at once still reads each state they passed through, such as a retired
//! launch's ending, which its port lets go once the feed has read past it.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use flotilla_resources::engine_records::{CommitMark, KindRecords, LogPosition, Record, Since, Ticks};
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
pub(crate) enum Batch<R: Resource> {
    /// These names' records were read again: `read` holds their copies as this pass read them,
    /// a name read gone holding none.
    Changed { names: Arc<BTreeSet<String>>, read: Arc<Copies<R>> },
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
    batches: VecDeque<(u64, Batch<R>)>,
}

impl<R: Resource> FeedState<R> {
    /// The batches after `serial`; `None` where some are no longer held.
    pub(crate) fn after(&self, serial: u64) -> Option<impl Iterator<Item = &Batch<R>>> {
        let oldest = self.batches.front().map_or(self.serial + 1, |(first, _)| *first);
        (serial + 1 >= oldest).then(|| self.batches.iter().filter(move |(held, _)| *held > serial).map(|(_, batch)| batch))
    }

    fn push(&mut self, batch: Batch<R>) {
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
    /// When the port's ticks arrive: a catch-up over a followed port reads what changed itself.
    ticks: Ticks,
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
        let ticks = records.ticks();
        // Taken before the listing, so a commit after the mark is in the next pass.
        let mark = records.commit_mark();
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
                let mut ticked = caught_up.is_none();
                let mut ended = false;
                while let Some(next) = changes.next().now_or_never() {
                    match next {
                        Some(()) => ticked = true,
                        None => {
                            ended = true;
                            break;
                        }
                    }
                }
                // A pass reads every commit made before it, so it takes the port's mark; a forced
                // catch-up with no tick queued passes only where the mark shows a commit since.
                let changed = (ticked || forced) && (mark.as_ref().is_none_or(CommitMark::take) || ticked);
                if changed {
                    match follow(&*records, &following).await {
                        Passed::Unchanged => {}
                        Passed::Read(serial) => {
                            publish.send_replace(serial);
                        }
                        Passed::Failed(serial) => {
                            publish.send_replace(serial);
                            if let Some(mark) = &mark {
                                mark.give_back();
                            }
                        }
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
        Ok(Self { state, serials, catch_ups, ticks, task })
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

    /// Returns once the published records hold every change committed before the call. A port
    /// that ticks at commit has a write's tick queued by then, so this passes only when a tick
    /// is queued; over a followed port a tick may still be on its way, so this passes unless the
    /// port's commit mark shows nothing committed since the last pass.
    pub(crate) async fn catch_up(&self) {
        self.request(self.ticks == Ticks::Followed).await;
    }

    /// Returns once the published records hold every change committed before the call, whether
    /// or not its tick has reached the feed: a pass reads the log from the feed's position,
    /// unless the port's commit mark shows nothing committed since the last.
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

/// What one pass of the feed read.
enum Passed {
    /// Nothing changed since the feed's position.
    Unchanged,
    /// The batch with this serial.
    Read(u64),
    /// The failed batch with this serial; the records stand as they were.
    Failed(u64),
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
/// every record where the position cannot be resumed from.
async fn follow<R: Resource>(records: &dyn KindRecords<R>, state: &Mutex<FeedState<R>>) -> Passed {
    let position = locked(state).position.clone();
    let batch = match records.changed_since(&position).await {
        Ok(Since::Changed { names, through }) if names.is_empty() => {
            locked(state).position = through;
            return Passed::Unchanged;
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
                    let mut batch = Copies::<R>::new();
                    for (name, copies) in read {
                        if copies.is_empty() {
                            state.records.remove(&name);
                        } else {
                            state.records.insert(name.clone(), copies.clone());
                            batch.insert(name, copies);
                        }
                    }
                    state.position = through;
                    state.push(Batch::Changed { names: Arc::new(names), read: Arc::new(batch) });
                    return Passed::Read(state.serial);
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
                    return Passed::Read(state.serial);
                }
                Err(error) => Batch::Failed(error),
            }
        }
        Err(error) => Batch::Failed(error),
    };
    let mut state = locked(state);
    state.push(batch);
    Passed::Failed(state.serial)
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
        engine_records::{LogPosition, Since, WriteError},
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
        /// When the records say their ticks arrive; each test sends them itself.
        ticks: Ticks,
        /// Whether the records keep commit marks, each set as a write lands.
        marked: bool,
        marks: Mutex<Vec<Arc<std::sync::atomic::AtomicBool>>>,
        /// How many times a reader asked what changed: each a read of the log.
        passes: AtomicUsize,
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
                ticks: Ticks::AtCommit,
                marked: false,
                marks: Mutex::default(),
                passes: AtomicUsize::default(),
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
            for mark in self.marks.lock().expect("marks").iter() {
                mark.store(true, Ordering::SeqCst);
            }
        }

        /// Removes `name`'s record, as its port lets a retired one go.
        fn remove(&self, name: &str) {
            self.records.lock().expect("records").retain(|held| held.name != name);
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
        fn ticks(&self) -> Ticks {
            self.ticks
        }
        fn commit_mark(&self) -> Option<CommitMark> {
            self.marked.then(|| {
                let mark = Arc::new(std::sync::atomic::AtomicBool::new(true));
                self.marks.lock().expect("marks").push(Arc::clone(&mark));
                CommitMark::new(mark, ())
            })
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
            self.passes.fetch_add(1, Ordering::SeqCst);
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
            _version: Option<u64>,
            _labels: &BTreeMap<String, String>,
            _annotations: &BTreeMap<String, String>,
            _spec: &R::Spec,
        ) -> Result<Record<R>, WriteError> {
            Err("read only".to_owned().into())
        }
        async fn set_status(
            &self,
            _name: &str,
            _version: u64,
            _status: &R::Status,
        ) -> Result<Record<R>, flotilla_resources::engine_records::WriteError> {
            Err(String::from("read only").into())
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

    // Behaviour: a watcher that takes several of the feed's batches at once reads each as the feed
    // read it: a record one batch changed and the next read gone is told changed, then gone, as a
    // retired launch's ending is told before it leaves.
    #[tokio::test(flavor = "current_thread")]
    async fn a_watcher_behind_the_feed_reads_each_batch_as_it_was_read() {
        let records = Arc::new(Counted::<Vessel>::default());
        records.put("v#1", 1);
        let backend = crate::test_store::memory_backend();
        backend.install_engine_kind::<Vessel>(records.clone());
        let watch = || async { backend.definitions::<Vessel>("flotilla").engine_watch().await.expect("served").expect("watch") };
        let (_, mut behind) = watch().await;
        let (_, mut ahead) = watch().await;

        // The watcher ahead takes each batch as it is read; the one behind takes none until both
        // are read.
        records.put("v#1", 2);
        records.tick();
        assert_eq!(named(&ahead.next().await.expect("event").expect("read")), ("modified", "v#1".to_owned()));
        records.remove("v#1");
        records.tick();
        assert_eq!(named(&ahead.next().await.expect("event").expect("read")), ("deleted", "v#1".to_owned()));

        let first = behind.next().await.expect("event").expect("read");
        assert_eq!(named(&first), ("modified", "v#1".to_owned()), "the change is told before the record leaves");
        let ReadWatchEvent::Modified(read) = first else { unreachable!() };
        assert_eq!(read.object.metadata.resource_version, "2");
        assert_eq!(named(&behind.next().await.expect("event").expect("read")), ("deleted", "v#1".to_owned()));
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

    // Behaviour: over records whose ticks come from a follower, a view's read holds every write
    // committed before it, whether that write's tick has reached the feed yet, comes later or
    // never comes. Over records that keep commit marks, a read with nothing written or ticked
    // since the last reads no log. Generated: an interleaving of writes, late ticks and reads.
    #[hegel::test]
    fn a_view_of_followed_records_reads_each_write_before_its_tick(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let steps = tc.draw(gs::vecs(gs::integers::<u8>().min_value(0).max_value(2)).min_size(1).max_size(24));
        let names = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
        let marked = tc.draw(gs::booleans());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let records = Arc::new(Counted::<Vessel> { ticks: Ticks::Followed, marked, ..Counted::default() });
            let spec = |version: u64| VesselSpec {
                convoy_ref: "c".into(),
                vessel_name: format!("w{version}"),
                fulfilment_kind_ref: "k".into(),
                adopted_checkout_refs: BTreeMap::new(),
            };
            records.put_spec("v#0", 0, spec(0));
            let backend = crate::test_store::memory_backend();
            backend.install_engine_kind::<Vessel>(records.clone());
            let view = crate::WatchView::<Vessel>::start(backend, "flotilla").await.expect("view");
            let mut written = BTreeMap::from([("v#0".to_owned(), "w0".to_owned())]);
            let (mut version, mut unticked) = (0, 0);
            // Whether nothing was written or ticked since the last read.
            let mut quiet = false;
            for (index, step) in steps.into_iter().chain([2]).enumerate() {
                match step {
                    0 => {
                        version += 1;
                        let name = format!("v#{}", index % names);
                        records.put_spec(&name, version, spec(version));
                        written.insert(name, format!("w{version}"));
                        unticked += 1;
                        quiet = false;
                    }
                    1 if unticked > 0 => {
                        unticked -= 1;
                        records.tick();
                        quiet = false;
                    }
                    1 => {}
                    _ => {
                        let passes = records.passes.load(Ordering::SeqCst);
                        let read: BTreeMap<_, _> = view
                            .list()
                            .await
                            .expect("list")
                            .items
                            .into_iter()
                            .map(|read| (read.object.metadata.name, read.object.spec.vessel_name))
                            .collect();
                        assert_eq!(read, written, "a read holds every write before it");
                        if marked && quiet {
                            assert_eq!(records.passes.load(Ordering::SeqCst), passes, "a read with nothing committed reads no log");
                        }
                        quiet = true;
                    }
                }
            }
        });
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
