//! Engine reads counted for the caller that made them (#3108 load measurement).
//!
//! A caller is a controller loop, a watch, or another named task. It enters its scope around its
//! work: [`attributed`] around async work, [`within`] on the current thread, [`carry`] to take
//! the current scope onto a blocking thread and [`inherit`] onto a spawned task. A [`measure`]
//! also counts the shared work it waits on, such as a held view's follower ([`Waiters`]). Each
//! read inside the scope counts against that caller
//! by kind: the store's reads by shape and the rows they returned, the records decoded, and the
//! records converted to the old store's shape. A read made outside every scope counts for
//! [`UNATTRIBUTED`]. Counting is an increment on a counter the caller already holds, after one
//! map lookup by kind; nothing switches it off.
//!
//! Named callers ([`caller`]) are process-wide: one daemon runs per process, and tests that run
//! several in one process see the named callers summed over them. A controller keeps its own
//! [`CallerReads`], so its counts stay with its daemon.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock, Mutex, PoisonError, RwLock,
    },
    task::{Context, Poll},
};

use serde::Serialize;

/// The caller that counts reads made outside every scope.
pub const UNATTRIBUTED: &str = "unattributed";

/// How a read finds its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadShape {
    /// One record by its key: a store identity lookup, or a get by name.
    Keyed,
    /// A declared index query, counted once for all its pages.
    Index,
    /// Every record of a kind (at a home), counted once for all its pages.
    WholeKind,
}

/// What one caller's reads of one kind cost, cumulative since the caller was first counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct KindReadCounts {
    pub keyed: u64,
    pub index: u64,
    pub whole_kind: u64,
    /// Rows the reads returned, over every page.
    pub rows: u64,
    /// Records decoded from the store's rows.
    pub decoded: u64,
    /// Records converted to the old store's shape.
    pub converted: u64,
}

impl KindReadCounts {
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            keyed: self.keyed.saturating_sub(earlier.keyed),
            index: self.index.saturating_sub(earlier.index),
            whole_kind: self.whole_kind.saturating_sub(earlier.whole_kind),
            rows: self.rows.saturating_sub(earlier.rows),
            decoded: self.decoded.saturating_sub(earlier.decoded),
            converted: self.converted.saturating_sub(earlier.converted),
        }
    }
}

/// One caller's counts, by kind name.
pub type ReadCounts = BTreeMap<String, KindReadCounts>;

/// `later` less `earlier`, kind by kind; kinds with nothing counted between them are left out.
pub fn counts_since(later: &ReadCounts, earlier: &ReadCounts) -> ReadCounts {
    later
        .iter()
        .map(|(kind, counts)| (kind.clone(), earlier.get(kind).map_or(*counts, |earlier| counts.since(earlier))))
        .filter(|(_, counts)| *counts != KindReadCounts::default())
        .collect()
}

#[derive(Debug, Default)]
struct KindCounters {
    keyed: AtomicU64,
    index: AtomicU64,
    whole_kind: AtomicU64,
    rows: AtomicU64,
    decoded: AtomicU64,
    converted: AtomicU64,
}

impl KindCounters {
    fn snapshot(&self) -> KindReadCounts {
        KindReadCounts {
            keyed: self.keyed.load(Ordering::Relaxed),
            index: self.index.load(Ordering::Relaxed),
            whole_kind: self.whole_kind.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            decoded: self.decoded.load(Ordering::Relaxed),
            converted: self.converted.load(Ordering::Relaxed),
        }
    }
}

/// One caller's reads, by kind.
#[derive(Debug, Default)]
pub struct CallerReads {
    kinds: RwLock<HashMap<String, Arc<KindCounters>>>,
    /// A [`measure`]: it also counts the shared work it waits on ([`Waiters`]), which a caller
    /// otherwise leaves to the caller doing it.
    measure: bool,
}

impl CallerReads {
    fn kind(&self, kind: &str) -> Arc<KindCounters> {
        if let Some(counters) = self.kinds.read().unwrap_or_else(PoisonError::into_inner).get(kind) {
            return Arc::clone(counters);
        }
        Arc::clone(self.kinds.write().unwrap_or_else(PoisonError::into_inner).entry(kind.to_owned()).or_default())
    }

    /// A read of `kind` by `shape`. A paged read counts here once, before its first page.
    pub fn read(&self, kind: &str, shape: ReadShape) {
        let counters = self.kind(kind);
        match shape {
            ReadShape::Keyed => &counters.keyed,
            ReadShape::Index => &counters.index,
            ReadShape::WholeKind => &counters.whole_kind,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    /// Rows a read of `kind` returned, page by page.
    pub fn rows(&self, kind: &str, rows: usize) {
        self.kind(kind).rows.fetch_add(rows as u64, Ordering::Relaxed);
    }

    pub fn decoded(&self, kind: &str, records: usize) {
        self.kind(kind).decoded.fetch_add(records as u64, Ordering::Relaxed);
    }

    pub fn converted(&self, kind: &str, records: usize) {
        self.kind(kind).converted.fetch_add(records as u64, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ReadCounts {
        let kinds = self.kinds.read().unwrap_or_else(PoisonError::into_inner);
        kinds.iter().map(|(kind, counters)| (kind.clone(), counters.snapshot())).collect()
    }
}

static CALLERS: LazyLock<Mutex<BTreeMap<&'static str, Arc<CallerReads>>>> = LazyLock::new(Mutex::default);

/// The process-wide caller named `name`.
pub fn caller(name: &'static str) -> Arc<CallerReads> {
    Arc::clone(CALLERS.lock().unwrap_or_else(PoisonError::into_inner).entry(name).or_default())
}

/// Every process-wide caller's counts, [`UNATTRIBUTED`] among them once anything is counted
/// outside a scope.
pub fn callers() -> BTreeMap<String, ReadCounts> {
    let callers = CALLERS.lock().unwrap_or_else(PoisonError::into_inner);
    callers.iter().map(|(name, reads)| ((*name).to_owned(), reads.snapshot())).collect()
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<CallerReads>>> = const { RefCell::new(None) };
    /// The measures waiting on the work running here: each counts its reads too.
    static WAITING: RefCell<Vec<Arc<CallerReads>>> = const { RefCell::new(Vec::new()) };
}

/// Puts the scope back as it was when dropped, unwinding included.
struct Restore(Option<Arc<CallerReads>>);

impl Drop for Restore {
    fn drop(&mut self) {
        let previous = self.0.take();
        CURRENT.with(|current| *current.borrow_mut() = previous);
    }
}

/// Puts the waiting measures back as they were when dropped, unwinding included.
struct RestoreWaiting(Vec<Arc<CallerReads>>);

impl Drop for RestoreWaiting {
    fn drop(&mut self) {
        let previous = std::mem::take(&mut self.0);
        WAITING.with(|waiting| *waiting.borrow_mut() = previous);
    }
}

/// Runs `work` on this thread with its reads counted for `caller`.
pub fn within<T>(caller: &Arc<CallerReads>, work: impl FnOnce() -> T) -> T {
    let _restore = Restore(CURRENT.with(|current| current.replace(Some(Arc::clone(caller)))));
    work()
}

/// Runs `work` on this thread with its reads counted for `waiting` as well as its caller.
fn waited_on<T>(waiting: Vec<Arc<CallerReads>>, work: impl FnOnce() -> T) -> T {
    let _restore = RestoreWaiting(WAITING.with(|current| current.replace(waiting)));
    work()
}

/// `work`, run wherever it is sent (a blocking thread), counted for the scope current here, and
/// for the measures waiting on it here.
pub fn carry<T>(work: impl FnOnce() -> T) -> impl FnOnce() -> T {
    let scope = CURRENT.with(|current| current.borrow().clone());
    let waiting = WAITING.with(|waiting| waiting.borrow().clone());
    move || {
        waited_on(waiting, || match scope {
            Some(caller) => within(&caller, work),
            None => work(),
        })
    }
}

/// The measures in scope here: the caller, when it is one, and those waiting on this work.
fn measures_here() -> Vec<Arc<CallerReads>> {
    let mut measures = WAITING.with(|waiting| waiting.borrow().clone());
    if let Some(caller) = CURRENT.with(|current| current.borrow().clone()).filter(|caller| caller.measure) {
        if !measures.iter().any(|measure| Arc::ptr_eq(measure, &caller)) {
            measures.push(caller);
        }
    }
    measures
}

/// The callers waiting on one piece of shared work, such as a held view's follower: a measured
/// operation that waits ([`Waiters::wait`]) for work another task does ([`Waiters::serve`])
/// counts that work's reads as its own. Only a [`measure`] counts them; a named caller leaves
/// them to the caller doing the work, so the named callers' counts still sum to the reads made.
#[derive(Debug, Default)]
pub struct Waiters {
    waiting: Mutex<Vec<Arc<CallerReads>>>,
}

impl Waiters {
    /// `work`, which waits on the shared work, with the measures in scope here counting what that
    /// work reads while it waits.
    pub async fn wait<F: Future>(&self, work: F) -> F::Output {
        let measures = measures_here();
        if measures.is_empty() {
            return work.await;
        }
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner).extend(measures.iter().cloned());
        let _waited = Waited { waiters: self, measures };
        work.await
    }

    /// `work`, the shared work, counted at each poll for the measures then waiting on it as well
    /// as for its caller.
    pub fn serve<'a, F: Future>(&'a self, work: F) -> Served<'a, F> {
        Served { waiters: self, work: Box::pin(work) }
    }

    fn waiting(&self) -> Vec<Arc<CallerReads>> {
        let mut waiting = WAITING.with(|waiting| waiting.borrow().clone());
        for measure in self.waiting.lock().unwrap_or_else(PoisonError::into_inner).iter() {
            if !waiting.iter().any(|held| Arc::ptr_eq(held, measure)) {
                waiting.push(Arc::clone(measure));
            }
        }
        waiting
    }
}

/// Takes a wait's measures off its [`Waiters`] when it ends, dropped or finished.
struct Waited<'a> {
    waiters: &'a Waiters,
    measures: Vec<Arc<CallerReads>>,
}

impl Drop for Waited<'_> {
    fn drop(&mut self) {
        let mut waiting = self.waiters.waiting.lock().unwrap_or_else(PoisonError::into_inner);
        for measure in &self.measures {
            if let Some(at) = waiting.iter().position(|held| Arc::ptr_eq(held, measure)) {
                waiting.swap_remove(at);
            }
        }
    }
}

/// Shared work counted for the measures waiting on it: see [`Waiters::serve`].
pub struct Served<'a, F> {
    waiters: &'a Waiters,
    work: Pin<Box<F>>,
}

impl<F: Future> Future for Served<'_, F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        waited_on(this.waiters.waiting(), || this.work.as_mut().poll(context))
    }
}

/// `work` with its reads counted for `caller` each time it is polled.
pub fn attributed<F: Future>(caller: Arc<CallerReads>, work: F) -> Attributed<F> {
    Attributed { caller, waiting: None, work: Box::pin(work) }
}

/// `work`, to be spawned, counted for the caller current where it is made, and for the measures
/// waiting there: spawned work keeps its spawner's caller, and a measure counts what the work it
/// measures spawns.
pub fn inherit<F: Future>(work: F) -> Attributed<F> {
    let caller = CURRENT.with(|current| current.borrow().clone()).unwrap_or_else(|| caller(UNATTRIBUTED));
    let waiting = WAITING.with(|waiting| waiting.borrow().clone());
    Attributed { caller, waiting: Some(waiting), work: Box::pin(work) }
}

/// `work`, counted as [`inherit`] counts it where a caller is current, and for `fallback` where
/// none is: an entry point many callers share, such as command execution, names the reads of
/// callers that name none.
pub fn inherit_or<F: Future>(fallback: &'static str, work: F) -> Attributed<F> {
    let current = CURRENT.with(|current| current.borrow().clone());
    let caller = current.unwrap_or_else(|| caller(fallback));
    let waiting = WAITING.with(|waiting| waiting.borrow().clone());
    Attributed { caller, waiting: Some(waiting), work: Box::pin(work) }
}

/// `work`'s output and the reads it made, counted for a caller of its own: the reads of the work
/// it spawns through [`inherit`], and of shared work it waits on through [`Waiters`], count too.
pub async fn measure<F: Future>(work: F) -> (F::Output, ReadCounts) {
    let reads = Arc::new(CallerReads { measure: true, ..CallerReads::default() });
    let output = attributed(Arc::clone(&reads), work).await;
    (output, reads.snapshot())
}

/// `work`'s output and the reads it made on this thread, counted for a caller of its own.
pub fn measure_now<T>(work: impl FnOnce() -> T) -> (T, ReadCounts) {
    let reads = Arc::new(CallerReads { measure: true, ..CallerReads::default() });
    let output = within(&reads, work);
    (output, reads.snapshot())
}

/// A future whose reads count for one caller: see [`attributed`].
pub struct Attributed<F> {
    caller: Arc<CallerReads>,
    /// The measures waiting where it was spawned; none keeps those where it is polled.
    waiting: Option<Vec<Arc<CallerReads>>>,
    work: Pin<Box<F>>,
}

impl<F: Future> Future for Attributed<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        match &this.waiting {
            Some(waiting) => waited_on(waiting.clone(), || within(&this.caller, || this.work.as_mut().poll(context))),
            None => within(&this.caller, || this.work.as_mut().poll(context)),
        }
    }
}

fn counting(count: impl Fn(&CallerReads)) {
    CURRENT.with(|current| {
        let current = current.borrow();
        match current.as_ref() {
            Some(caller) => count(caller),
            None => count(&caller(UNATTRIBUTED)),
        }
        WAITING.with(|waiting| {
            for measure in waiting.borrow().iter() {
                if !current.as_ref().is_some_and(|caller| Arc::ptr_eq(caller, measure)) {
                    count(measure);
                }
            }
        });
    });
}

/// Counts a read of `kind` by `shape` for the current caller.
pub fn read(kind: &str, shape: ReadShape) {
    counting(|caller| caller.read(kind, shape));
}

/// Counts rows a read of `kind` returned for the current caller.
pub fn rows(kind: &str, rows: usize) {
    counting(|caller| caller.rows(kind, rows));
}

/// Counts records of `kind` decoded for the current caller.
pub fn decoded(kind: &str, records: usize) {
    counting(|caller| caller.decoded(kind, records));
}

/// Counts records of `kind` converted to the old store's shape for the current caller.
pub fn converted(kind: &str, records: usize) {
    counting(|caller| caller.converted(kind, records));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poll_once<F: Future>(work: Pin<&mut F>) -> Poll<F::Output> {
        work.poll(&mut Context::from_waker(std::task::Waker::noop()))
    }

    // Behaviour: reads count for the innermost scope, the scope is restored when it ends, and a
    // read outside every scope counts as unattributed.
    #[test]
    fn reads_count_for_the_scope_they_are_made_in() {
        let (outer, inner) = (Arc::new(CallerReads::default()), Arc::new(CallerReads::default()));
        let unattributed = caller(UNATTRIBUTED).snapshot();
        within(&outer, || {
            read("Convoy", ReadShape::WholeKind);
            rows("Convoy", 3);
            within(&inner, || {
                read("Convoy", ReadShape::Keyed);
                decoded("Convoy", 1);
            });
            converted("Message", 2);
        });
        read("Vessel", ReadShape::Index);
        let outer = outer.snapshot();
        assert_eq!(outer["Convoy"], KindReadCounts { whole_kind: 1, rows: 3, ..KindReadCounts::default() });
        assert_eq!(outer["Message"], KindReadCounts { converted: 2, ..KindReadCounts::default() });
        assert_eq!(inner.snapshot()["Convoy"], KindReadCounts { keyed: 1, decoded: 1, ..KindReadCounts::default() });
        let since = counts_since(&caller(UNATTRIBUTED).snapshot(), &unattributed);
        assert_eq!(since.get("Vessel").map(|counts| counts.index), Some(1));
    }

    // Behaviour: an attributed future counts each poll's reads for its caller, and carried work
    // counts for the scope it was carried from, on whichever thread runs it.
    #[test]
    fn futures_and_carried_work_keep_their_caller() {
        let polled = Arc::new(CallerReads::default());
        let mut first = true;
        let mut work = attributed(
            Arc::clone(&polled),
            std::future::poll_fn(move |_| {
                read("Convoy", ReadShape::Keyed);
                if std::mem::take(&mut first) {
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            }),
        );
        assert!(poll_once(Pin::new(&mut work)).is_pending());
        assert!(poll_once(Pin::new(&mut work)).is_ready());
        assert_eq!(polled.snapshot()["Convoy"].keyed, 2);

        let carrier = Arc::new(CallerReads::default());
        let carried = within(&carrier, || carry(|| read("Vessel", ReadShape::WholeKind)));
        std::thread::spawn(carried).join().expect("carried work");
        assert_eq!(carrier.snapshot()["Vessel"].whole_kind, 1);
    }

    /// A future pending at its first poll and ready at its second.
    fn pending_once() -> impl Future<Output = ()> {
        let mut first = true;
        std::future::poll_fn(move |_| if std::mem::take(&mut first) { Poll::Pending } else { Poll::Ready(()) })
    }

    // Behaviour: shared work counts for its own caller, and also for each measure waiting on it
    // while it waits, on the work's blocking threads too; a named caller waiting counts none of it.
    #[test]
    fn a_measure_counts_the_shared_work_it_waits_on() {
        let waiters = Waiters::default();
        let (measured, named, worker) = (
            Arc::new(CallerReads { measure: true, ..CallerReads::default() }),
            Arc::new(CallerReads::default()),
            Arc::new(CallerReads::default()),
        );
        let mut measured_wait = attributed(Arc::clone(&measured), waiters.wait(pending_once()));
        let mut named_wait = attributed(Arc::clone(&named), waiters.wait(pending_once()));
        assert!(poll_once(Pin::new(&mut measured_wait)).is_pending());
        assert!(poll_once(Pin::new(&mut named_wait)).is_pending());

        let mut served = attributed(
            Arc::clone(&worker),
            waiters.serve(std::future::poll_fn(|_| {
                read("Convoy", ReadShape::WholeKind);
                std::thread::spawn(carry(|| read("Vessel", ReadShape::Keyed))).join().expect("blocking work");
                Poll::Ready(())
            })),
        );
        assert!(poll_once(Pin::new(&mut served)).is_ready());
        assert!(poll_once(Pin::new(&mut measured_wait)).is_ready());
        drop(measured_wait);
        let mut after = attributed(
            Arc::clone(&worker),
            waiters.serve(std::future::poll_fn(|_| {
                read("Host", ReadShape::Keyed);
                Poll::Ready(())
            })),
        );
        assert!(poll_once(Pin::new(&mut after)).is_ready());

        let (measured, named, worker) = (measured.snapshot(), named.snapshot(), worker.snapshot());
        assert_eq!((measured["Convoy"].whole_kind, measured["Vessel"].keyed), (1, 1), "the waiting measure counts the work: {measured:?}");
        assert!(!measured.contains_key("Host"), "work after the wait is not the measure's: {measured:?}");
        assert!(named.is_empty(), "a named caller leaves the work to its doer: {named:?}");
        assert_eq!((worker["Convoy"].whole_kind, worker["Vessel"].keyed, worker["Host"].keyed), (1, 1, 1));
    }

    // Behaviour: work spawned through inherit counts for the measure that spawned it.
    #[test]
    fn a_measure_counts_the_work_it_spawns() {
        let measured = Arc::new(CallerReads { measure: true, ..CallerReads::default() });
        let mut spawned = within(&measured, || {
            inherit(std::future::poll_fn(|_| {
                read("Convoy", ReadShape::Keyed);
                Poll::Ready(())
            }))
        });
        std::thread::spawn(move || assert!(poll_once(Pin::new(&mut spawned)).is_ready())).join().expect("spawned work");
        assert_eq!(measured.snapshot()["Convoy"].keyed, 1);
    }
}
