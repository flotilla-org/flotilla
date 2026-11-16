//! Engine reads counted for the caller that made them (#3108 load measurement).
//!
//! A caller is a controller loop, a watch, or another named task. It enters its scope around its
//! work: [`attributed`] around async work, [`within`] on the current thread, [`carry`] to take
//! the current scope onto a blocking thread and [`inherit`] onto a spawned task. Each read inside
//! the scope counts against that caller
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
}

/// Puts the scope back as it was when dropped, unwinding included.
struct Restore(Option<Arc<CallerReads>>);

impl Drop for Restore {
    fn drop(&mut self) {
        let previous = self.0.take();
        CURRENT.with(|current| *current.borrow_mut() = previous);
    }
}

/// Runs `work` on this thread with its reads counted for `caller`.
pub fn within<T>(caller: &Arc<CallerReads>, work: impl FnOnce() -> T) -> T {
    let _restore = Restore(CURRENT.with(|current| current.replace(Some(Arc::clone(caller)))));
    work()
}

/// `work`, run wherever it is sent (a blocking thread), counted for the scope current here.
pub fn carry<T>(work: impl FnOnce() -> T) -> impl FnOnce() -> T {
    let scope = CURRENT.with(|current| current.borrow().clone());
    move || match scope {
        Some(caller) => within(&caller, work),
        None => work(),
    }
}

/// `work` with its reads counted for `caller` each time it is polled.
pub fn attributed<F: Future>(caller: Arc<CallerReads>, work: F) -> Attributed<F> {
    Attributed { caller, work: Box::pin(work) }
}

/// `work`, to be spawned, counted for the caller current where it is made: spawned work keeps
/// its spawner's caller.
pub fn inherit<F: Future>(work: F) -> Attributed<F> {
    let caller = CURRENT.with(|current| current.borrow().clone()).unwrap_or_else(|| caller(UNATTRIBUTED));
    attributed(caller, work)
}

/// `work`'s output and the reads it made, counted for a caller of its own. Work it spawns is
/// not its own: that counts where it runs.
pub async fn measure<F: Future>(work: F) -> (F::Output, ReadCounts) {
    let reads = Arc::new(CallerReads::default());
    let output = attributed(Arc::clone(&reads), work).await;
    (output, reads.snapshot())
}

/// `work`'s output and the reads it made on this thread, counted for a caller of its own.
pub fn measure_now<T>(work: impl FnOnce() -> T) -> (T, ReadCounts) {
    let reads = Arc::new(CallerReads::default());
    let output = within(&reads, work);
    (output, reads.snapshot())
}

/// A future whose reads count for one caller: see [`attributed`].
pub struct Attributed<F> {
    caller: Arc<CallerReads>,
    work: Pin<Box<F>>,
}

impl<F: Future> Future for Attributed<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        within(&this.caller, || this.work.as_mut().poll(context))
    }
}

fn counting(count: impl FnOnce(&CallerReads)) {
    let mut count = Some(count);
    CURRENT.with(|current| {
        if let Some(caller) = current.borrow().as_deref() {
            (count.take().expect("counted once"))(caller);
        }
    });
    if let Some(count) = count {
        count(&caller(UNATTRIBUTED));
    }
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
}
