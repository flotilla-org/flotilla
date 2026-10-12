//! Store-read budgets for request and steady-state scenarios.
//!
//! A counting [`ReadObserver`] records every adapter read as a call and a
//! number of decoded objects, per kind and [`ReadOperation`]. Scenarios compare
//! [`ReadSnapshot`]s: take a delta with [`ReadSnapshot::since`] or
//! [`measure_reads`], bound it with a [`ReadBudget`], and run the same scenario
//! at several sizes of unrelated data with [`assert_flat_reads`].
use flotilla_store::{ReadObserver, ResourceBackend};
use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

pub use flotilla_store::ReadOperation;

/// Calls and decoded objects for one kind and operation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadTally {
    pub calls: usize,
    pub objects: usize,
}

/// Store reads keyed by kind and operation: cumulative from a counted backend,
/// or a delta between two points of a scenario.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadSnapshot(BTreeMap<(String, ReadOperation), ReadTally>);

impl ReadSnapshot {
    fn record(&mut self, kind: &str, operation: ReadOperation, objects: usize) {
        let tally = self.0.entry((kind.into(), operation)).or_default();
        tally.calls += 1;
        tally.objects += objects;
    }

    /// Reads made after `earlier` was taken from the same counter. Entries
    /// without new calls are omitted, so an idle interval is empty.
    pub fn since(&self, earlier: &ReadSnapshot) -> ReadSnapshot {
        ReadSnapshot(
            self.0
                .iter()
                .filter_map(|(key, tally)| {
                    let before = earlier.0.get(key).copied().unwrap_or_default();
                    let calls = tally.calls.checked_sub(before.calls).expect("snapshots from one counter, in order");
                    let objects = tally.objects.checked_sub(before.objects).expect("snapshots from one counter, in order");
                    (calls != 0).then(|| (key.clone(), ReadTally { calls, objects }))
                })
                .collect(),
        )
    }

    pub fn tally(&self, kind: &str, operation: ReadOperation) -> ReadTally {
        self.0.get(&(kind.to_string(), operation)).copied().unwrap_or_default()
    }

    pub fn calls(&self, kind: &str, operation: ReadOperation) -> usize {
        self.tally(kind, operation).calls
    }

    pub fn objects(&self, kind: &str, operation: ReadOperation) -> usize {
        self.tally(kind, operation).objects
    }

    /// Every kind and operation with at least one call.
    pub fn entries(&self) -> impl Iterator<Item = (&str, ReadOperation, ReadTally)> {
        self.0.iter().map(|((kind, operation), tally)| (kind.as_str(), *operation, *tally))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Only whole-kind lists.
    pub fn lists(&self) -> ReadSnapshot {
        ReadSnapshot(self.0.iter().filter(|((_, operation), _)| *operation == ReadOperation::List).map(|(k, v)| (k.clone(), *v)).collect())
    }

    /// Calls per kind across operations; kinds read without objects are kept.
    pub fn calls_by_kind(&self) -> BTreeMap<String, usize> {
        self.by_kind(|tally| tally.calls)
    }

    /// Decoded objects per kind across operations; kinds read without objects are kept.
    pub fn objects_by_kind(&self) -> BTreeMap<String, usize> {
        self.by_kind(|tally| tally.objects)
    }

    fn by_kind(&self, field: impl Fn(&ReadTally) -> usize) -> BTreeMap<String, usize> {
        let mut totals = BTreeMap::new();
        for ((kind, _), tally) in &self.0 {
            *totals.entry(kind.clone()).or_default() += field(tally);
        }
        totals
    }

    /// No whole-kind list: steady-state work is served from views and watches,
    /// and request paths read through declared queries and named gets.
    #[track_caller]
    pub fn assert_no_lists(&self, scenario: &str) {
        let lists = self.lists();
        assert!(lists.is_empty(), "{scenario}: must not list a whole kind\n{lists}");
    }

    /// Every read is within the budget. A kind and operation the budget does
    /// not name may not be read at all.
    #[track_caller]
    pub fn assert_within(&self, budget: &ReadBudget, scenario: &str) {
        let over = self
            .entries()
            .filter_map(|(kind, operation, tally)| {
                let limit = budget.0.get(&(kind.to_string(), operation));
                let within = limit.is_some_and(|limit| tally.calls <= limit.calls && limit.objects.is_none_or(|max| tally.objects <= max));
                (!within).then(|| match limit {
                    Some(limit) => format!(
                        "  {kind} {operation:?}: {} calls / {} objects exceeds {} calls / {} objects",
                        tally.calls,
                        tally.objects,
                        limit.calls,
                        limit.objects.map_or_else(|| "any".to_string(), |max| max.to_string())
                    ),
                    None => format!("  {kind} {operation:?}: {} calls / {} objects outside the budget", tally.calls, tally.objects),
                })
            })
            .collect::<Vec<_>>();
        assert!(over.is_empty(), "{scenario}: reads exceed their budget\n{}\nobserved:\n{self}", over.join("\n"));
    }
}

impl fmt::Display for ReadSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return writeln!(f, "  (no reads)");
        }
        for (kind, operation, tally) in self.entries() {
            writeln!(f, "  {kind} {operation:?}: {} calls / {} objects", tally.calls, tally.objects)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct ReadLimit {
    calls: usize,
    objects: Option<usize>,
}

/// Upper bounds on a scenario's reads per kind and operation. Unnamed reads
/// are refused, so a new collaborator read fails until the budget names it.
#[derive(Debug, Clone, Default)]
pub struct ReadBudget(BTreeMap<(String, ReadOperation), ReadLimit>);

impl ReadBudget {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allow up to `calls` reads of this kind and operation, decoding any number of objects.
    pub fn calls(mut self, kind: &str, operation: ReadOperation, calls: usize) -> Self {
        self.0.insert((kind.into(), operation), ReadLimit { calls, objects: None });
        self
    }

    /// Allow up to `calls` reads decoding at most `objects` objects in total.
    pub fn objects(mut self, kind: &str, operation: ReadOperation, calls: usize, objects: usize) -> Self {
        self.0.insert((kind.into(), operation), ReadLimit { calls, objects: Some(objects) });
        self
    }
}

static NEXT_METER: AtomicU64 = AtomicU64::new(0);

tokio::task_local! {
    static READ_SCOPES: Vec<Arc<Mutex<BTreeMap<u64, ReadSnapshot>>>>;
}

#[derive(Debug)]
struct ReadCounts {
    id: u64,
    totals: Mutex<ReadSnapshot>,
}

impl Default for ReadCounts {
    fn default() -> Self {
        Self { id: NEXT_METER.fetch_add(1, Ordering::Relaxed), totals: Mutex::default() }
    }
}

impl ReadObserver for ReadCounts {
    fn read(&self, kind: &str, operation: ReadOperation, objects: usize) {
        self.totals.lock().expect("read counts").record(kind, operation, objects);
        // Adapters report after awaiting their storage, on the reading task.
        let _ = READ_SCOPES.try_with(|scopes| {
            for scope in scopes {
                scope.lock().expect("read scope").entry(self.id).or_default().record(kind, operation, objects);
            }
        });
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn read_counts(observer: Option<&dyn ReadObserver>) -> Option<&ReadCounts> {
    observer.and_then(|observer| observer.as_any().downcast_ref::<ReadCounts>())
}

/// Reads made by one future's own task, per counted backend. Background tasks
/// sharing a counted backend are excluded; so are tasks the future spawns.
#[derive(Debug, Clone, Default)]
pub struct ScopedReads(BTreeMap<u64, ReadSnapshot>);

impl ScopedReads {
    /// This backend's reads in the scope.
    #[track_caller]
    pub fn of(&self, backend: &ResourceBackend) -> ReadSnapshot {
        let counts = read_counts(backend.read_observer()).expect("backend counts reads; use with_read_counts");
        self.0.get(&counts.id).cloned().unwrap_or_default()
    }
}

/// Run a future and return its reads through counted backends. Reads that its
/// own task makes, directly or through `join`/`select`, are attributed even when
/// other tasks read the same backends concurrently.
pub async fn measure_reads<F: Future>(future: F) -> (F::Output, ScopedReads) {
    let scope = Arc::new(Mutex::new(BTreeMap::new()));
    let mut scopes = READ_SCOPES.try_with(Clone::clone).unwrap_or_default();
    scopes.push(scope.clone());
    let output = READ_SCOPES.scope(scopes, future).await;
    let reads = scope.lock().expect("read scope").clone();
    (output, ScopedReads(reads))
}

/// Run a scenario at each size of unrelated data and require identical
/// results (typically [`ReadSnapshot`]s), so request reads stay flat as
/// unrelated objects grow. Returns the result for the first size.
#[track_caller]
pub fn assert_flat_reads<R, F, Fut>(sizes: impl IntoIterator<Item = usize>, mut scenario: F) -> impl Future<Output = R>
where
    R: PartialEq + fmt::Debug,
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = R>,
{
    let caller = std::panic::Location::caller();
    let sizes = sizes.into_iter().collect::<Vec<_>>();
    async move {
        assert!(sizes.len() >= 2, "flat reads compare at least two sizes");
        let mut samples = Vec::new();
        for &size in &sizes {
            samples.push((size, scenario(size).await));
        }
        let (first_size, first) = samples.remove(0);
        for (size, sample) in &samples {
            assert!(
                *sample == first,
                "{caller}: reads changed with unrelated objects\nat {first_size}: {first:#?}\nat {size}: {sample:#?}"
            );
        }
        first
    }
}

pub trait ReadCountsBackendExt {
    fn with_read_counts(self) -> ResourceBackend
    where
        Self: Sized;
    /// Cumulative reads per kind and operation.
    fn read_snapshot(&self) -> ReadSnapshot;
    /// Number of objects decoded by store reads, per kind.
    fn read_counts(&self) -> BTreeMap<String, usize> {
        self.read_snapshot().objects_by_kind()
    }
    /// Number of store read operations per kind, including reads returning no objects.
    fn read_calls(&self) -> BTreeMap<String, usize> {
        self.read_snapshot().calls_by_kind()
    }
}

fn snapshot(observer: Option<&dyn ReadObserver>) -> ReadSnapshot {
    read_counts(observer).map(|counts| counts.totals.lock().expect("read counts").clone()).unwrap_or_default()
}

impl ReadCountsBackendExt for ResourceBackend {
    fn with_read_counts(self) -> ResourceBackend
    where
        Self: Sized,
    {
        self.with_read_observer(Arc::new(ReadCounts::default()))
    }
    fn read_snapshot(&self) -> ReadSnapshot {
        snapshot(self.read_observer())
    }
}

macro_rules! observed_backend {
    ($backend:ty) => {
        impl ReadCountsBackendExt for $backend {
            fn with_read_counts(self) -> ResourceBackend {
                ResourceBackend::new(self.with_read_observer(Arc::new(ReadCounts::default())))
            }
            fn read_snapshot(&self) -> ReadSnapshot {
                snapshot(self.read_observer())
            }
        }
    };
}
observed_backend!(flotilla_store_memory::InMemoryBackend);
observed_backend!(flotilla_store_sqlite::SqliteBackend);
observed_backend!(flotilla_store_http::HttpBackend);

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::generators as gs;

    const KINDS: [&str; 3] = ["Convoy", "Message", "TerminalSession"];
    const OPERATIONS: [ReadOperation; 3] = [ReadOperation::List, ReadOperation::Query, ReadOperation::Get];

    fn counted() -> ResourceBackend {
        crate::memory_backend().with_read_counts()
    }

    fn read(backend: &ResourceBackend, kind: &str, operation: ReadOperation, objects: usize) {
        backend.read_observer().expect("counted backend").read(kind, operation, objects);
    }

    // A delta is exactly the reads after its base: replaying any read sequence
    // split at any point (empty prefix and suffix included, repeated kinds,
    // every operation, zero-object reads) gives `since` == the suffix alone,
    // and per-kind views sum operations without dropping zero-object reads.
    #[hegel::test]
    fn deltas_are_the_reads_after_their_base(tc: hegel::TestCase) {
        let length = tc.draw(gs::integers::<usize>().min_value(0).max_value(12));
        let reads = (0..length)
            .map(|_| {
                (
                    KINDS[tc.draw(gs::integers::<usize>().min_value(0).max_value(2))],
                    OPERATIONS[tc.draw(gs::integers::<usize>().min_value(0).max_value(2))],
                    tc.draw(gs::integers::<usize>().min_value(0).max_value(3)),
                )
            })
            .collect::<Vec<_>>();
        let split = tc.draw(gs::integers::<usize>().min_value(0).max_value(length));
        let (all, suffix) = (counted(), counted());
        for (index, (kind, operation, objects)) in reads.iter().enumerate() {
            read(&all, kind, *operation, *objects);
            if index >= split {
                read(&suffix, kind, *operation, *objects);
            }
        }
        let base = counted();
        for (kind, operation, objects) in &reads[..split] {
            read(&base, kind, *operation, *objects);
        }
        let delta = all.read_snapshot().since(&base.read_snapshot());
        assert_eq!(delta, suffix.read_snapshot());
        assert!(all.read_snapshot().since(&all.read_snapshot()).is_empty());
        for kind in KINDS {
            let tail = &reads[split..];
            let calls = tail.iter().filter(|read| read.0 == kind).count();
            let objects = tail.iter().filter(|read| read.0 == kind).map(|read| read.2).sum::<usize>();
            assert_eq!(delta.calls_by_kind().get(kind).copied().unwrap_or_default(), calls);
            assert_eq!(delta.objects_by_kind().get(kind).copied(), (calls > 0).then_some(objects));
            let lists = tail.iter().filter(|read| read.0 == kind && read.1 == ReadOperation::List).count();
            assert_eq!(delta.lists().calls(kind, ReadOperation::List), lists);
        }
    }

    // A budget bounds calls and, when given, objects per kind and operation;
    // an unnamed read fails even with zero objects.
    #[test]
    fn budgets_bound_calls_objects_and_unnamed_reads() {
        let backend = counted();
        read(&backend, "Convoy", ReadOperation::Get, 1);
        read(&backend, "Convoy", ReadOperation::Get, 1);
        let reads = backend.read_snapshot();
        let fits = |budget: ReadBudget| std::panic::catch_unwind(|| reads.assert_within(&budget, "case")).is_ok();
        assert!(fits(ReadBudget::new().calls("Convoy", ReadOperation::Get, 2)));
        assert!(fits(ReadBudget::new().objects("Convoy", ReadOperation::Get, 2, 2)));
        assert!(!fits(ReadBudget::new().calls("Convoy", ReadOperation::Get, 1)));
        assert!(!fits(ReadBudget::new().objects("Convoy", ReadOperation::Get, 2, 1)));
        assert!(!fits(ReadBudget::new().calls("Convoy", ReadOperation::Query, 2)));
        read(&backend, "Message", ReadOperation::List, 0);
        let reads = backend.read_snapshot();
        assert!(
            std::panic::catch_unwind(|| reads.assert_within(&ReadBudget::new().calls("Convoy", ReadOperation::Get, 2), "case")).is_err()
        );
        assert!(std::panic::catch_unwind(|| reads.assert_no_lists("case")).is_err());
        assert!(std::panic::catch_unwind(|| ReadSnapshot::default().assert_no_lists("case")).is_ok());
    }

    // Flatness compares every size against the first and reports a change.
    #[tokio::test]
    async fn flat_reads_require_identical_samples() {
        assert_eq!(assert_flat_reads([0, 10, 200], |_| async { 7 }).await, 7);
        let grows = tokio::spawn(assert_flat_reads([0, 10], |size| async move { size.min(5) }));
        assert!(grows.await.is_err(), "a read that grows with unrelated objects fails");
    }

    // A scope attributes the measured future's reads, including concurrent
    // reads it joins, per backend, and nests; other tasks' reads are excluded.
    #[tokio::test]
    async fn scopes_measure_their_own_task() {
        let (first, second) = (counted(), counted());
        let background = first.clone();
        let (gate, wait) = tokio::sync::oneshot::channel::<()>();
        let other = tokio::spawn(async move {
            wait.await.expect("gate");
            read(&background, "Convoy", ReadOperation::List, 9);
        });
        let ((), outer) = measure_reads(async {
            read(&first, "Convoy", ReadOperation::Get, 1);
            gate.send(()).expect("gate");
            other.await.expect("other task");
            let ((), inner) = measure_reads(async {
                futures::join!(async { read(&first, "Message", ReadOperation::Query, 2) }, async {
                    read(&second, "Convoy", ReadOperation::Get, 0)
                });
            })
            .await;
            assert_eq!(inner.of(&first).calls_by_kind(), BTreeMap::from([("Message".to_string(), 1)]));
            assert_eq!(inner.of(&second).calls("Convoy", ReadOperation::Get), 1);
        })
        .await;
        let first_reads = outer.of(&first);
        assert_eq!(first_reads.calls("Convoy", ReadOperation::Get), 1);
        assert_eq!(first_reads.calls("Message", ReadOperation::Query), 1);
        assert!(first_reads.lists().is_empty(), "another task's list is not attributed");
        assert_eq!(first.read_snapshot().calls("Convoy", ReadOperation::List), 1, "totals still count every task");
        assert_eq!(outer.of(&second).calls("Convoy", ReadOperation::Get), 1);
    }
}
