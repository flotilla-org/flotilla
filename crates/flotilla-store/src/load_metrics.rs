//! Load instrumentation (#3206): counters and latency histograms that
//! the store, controller loops and replication record into, and that a load
//! driver samples. Most recording uses relaxed atomics; read-retry lifecycle
//! tracking takes a short mutex only on refusal and completion. Metrics stay
//! on in production builds.
//!
//! Counters are cumulative since start. Where supported, subtract an earlier
//! snapshot to measure one phase (`since`); peaks are high-water marks over
//! the whole run. Read-retry snapshots also expose explicitly current gauges.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use serde::Serialize;

/// Sixteen sub-buckets per power of two keep every quantile within 1/16
/// (6.25%) of the recorded value, over the whole `u64` range of microseconds.
const SUB_BITS: u32 = 4;
const SUB_BUCKETS: u32 = 1 << SUB_BITS;
const BUCKETS: usize = (SUB_BUCKETS + (64 - SUB_BITS) * SUB_BUCKETS) as usize;

fn bucket(micros: u64) -> usize {
    if micros < u64::from(SUB_BUCKETS) {
        return micros as usize;
    }
    let exponent = 63 - micros.leading_zeros();
    let sub = (micros >> (exponent - SUB_BITS)) & u64::from(SUB_BUCKETS - 1);
    (SUB_BUCKETS + (exponent - SUB_BITS) * SUB_BUCKETS) as usize + sub as usize
}

/// The largest value a bucket holds: quantiles report this conservative bound.
fn bucket_upper(index: usize) -> u64 {
    if index < SUB_BUCKETS as usize {
        return index as u64;
    }
    let exponent = (index as u32 - SUB_BUCKETS) / SUB_BUCKETS + SUB_BITS;
    let sub = (index as u64 - u64::from(SUB_BUCKETS)) % u64::from(SUB_BUCKETS);
    // The top bucket's bound is 2^64 - 1: compute in u128 so its shift cannot wrap.
    let upper = ((u128::from(SUB_BUCKETS) + u128::from(sub) + 1) << (exponent - SUB_BITS)) - 1;
    u64::try_from(upper).unwrap_or(u64::MAX)
}

/// A latency distribution recorded in microseconds.
pub struct LatencyHistogram {
    buckets: Box<[AtomicU64]>,
    sum_micros: AtomicU64,
    max_micros: AtomicU64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self { buckets: (0..BUCKETS).map(|_| AtomicU64::new(0)).collect(), sum_micros: AtomicU64::new(0), max_micros: AtomicU64::new(0) }
    }
}

impl std::fmt::Debug for LatencyHistogram {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_tuple("LatencyHistogram").field(&self.snapshot().summary()).finish()
    }
}

impl LatencyHistogram {
    pub fn record(&self, latency: Duration) {
        let micros = u64::try_from(latency.as_micros()).unwrap_or(u64::MAX);
        self.buckets[bucket(micros)].fetch_add(1, Ordering::Relaxed);
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
        self.max_micros.fetch_max(micros, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            buckets: self.buckets.iter().map(|bucket| bucket.load(Ordering::Relaxed)).collect(),
            sum_micros: self.sum_micros.load(Ordering::Relaxed),
            max_micros: self.max_micros.load(Ordering::Relaxed),
        }
    }
}

/// Cumulative bucket counts; [`HistogramSnapshot::summary`] reads quantiles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistogramSnapshot {
    buckets: Vec<u64>,
    sum_micros: u64,
    max_micros: u64,
}

impl HistogramSnapshot {
    /// Inclusive bucket bounds in microseconds and cumulative counts.
    pub fn cumulative_buckets(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        let mut total = 0;
        self.buckets.iter().enumerate().map(move |(index, count)| {
            total += count;
            (bucket_upper(index), total)
        })
    }

    pub fn sum_micros(&self) -> u64 {
        self.sum_micros
    }

    pub fn count(&self) -> u64 {
        self.buckets.iter().sum()
    }

    /// Records made after `earlier`. Their maximum is bounded by the highest
    /// bucket they occupy (within 1/16 above the true value) and by the run's
    /// maximum, since individual values are not kept.
    pub fn since(&self, earlier: &Self) -> Self {
        let buckets = self
            .buckets
            .iter()
            .enumerate()
            .map(|(index, count)| count.saturating_sub(earlier.buckets.get(index).copied().unwrap_or(0)))
            .collect::<Vec<_>>();
        let highest = buckets.iter().rposition(|count| *count > 0).map_or(0, bucket_upper);
        Self { buckets, sum_micros: self.sum_micros.saturating_sub(earlier.sum_micros), max_micros: highest.min(self.max_micros) }
    }

    /// The smallest bucket bound below which `quantile` of the records fall.
    pub fn quantile_micros(&self, quantile: f64) -> u64 {
        let count = self.count();
        if count == 0 {
            return 0;
        }
        let rank = ((quantile.clamp(0.0, 1.0) * count as f64).ceil() as u64).max(1);
        let mut seen = 0;
        for (index, bucket) in self.buckets.iter().enumerate() {
            seen += bucket;
            if seen >= rank {
                return bucket_upper(index).min(self.max_micros);
            }
        }
        self.max_micros
    }

    pub fn summary(&self) -> LatencySummary {
        let count = self.count();
        LatencySummary {
            count,
            mean_micros: self.sum_micros.checked_div(count).unwrap_or(0),
            p50_micros: self.quantile_micros(0.50),
            p90_micros: self.quantile_micros(0.90),
            p99_micros: self.quantile_micros(0.99),
            max_micros: self.max_micros,
        }
    }
}

/// Quantiles report the bound of the bucket they fall in, within 1/16 above
/// the recorded values. For a phase (`since`), `max_micros` is bounded the same way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LatencySummary {
    pub count: u64,
    pub mean_micros: u64,
    pub p50_micros: u64,
    pub p90_micros: u64,
    pub p99_micros: u64,
    pub max_micros: u64,
}

#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn increment(&self) {
        self.add(1);
    }

    pub fn add(&self, value: u64) {
        self.0.fetch_add(value, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// The highest value observed.
#[derive(Debug, Default)]
pub struct Peak(AtomicU64);

impl Peak {
    pub fn observe(&self, value: u64) {
        self.0.fetch_max(value, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Retry labels are a fixed process-wide set: no resource, peer or view IDs.
#[derive(Debug, Clone, Copy)]
#[repr(usize)]
pub enum ReadRetryConsumer {
    Controller,
    WatchView,
    Replication,
}

impl ReadRetryConsumer {
    pub const ALL: [Self; 3] = [Self::Controller, Self::WatchView, Self::Replication];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Controller => "controller",
            Self::WatchView => "watch_view",
            Self::Replication => "replication",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// A consumer becomes prolonged after 30 seconds since its first busy refusal,
/// including time spent waiting on its next read. Finishing or dropping the
/// retry future removes its active signal; only successful reads count as recovery.
pub const READ_RETRY_PROLONGED: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub struct ReadRetryMetrics {
    state: Mutex<ReadRetryState>,
}

#[derive(Debug, Default)]
struct ReadRetryState {
    next_id: u64,
    retries: u64,
    recoveries: u64,
    failures: u64,
    cancellations: u64,
    peak_consecutive: u64,
    active: BTreeMap<u64, (tokio::time::Instant, u64)>,
}

/// Counters are cumulative; active/prolonged/consecutive/oldest are current
/// gauges. Storage is proportional only to currently retrying calls and is
/// released on completion or cancellation, never retained by consumer identity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ReadRetrySnapshot {
    pub retries: u64,
    pub recoveries: u64,
    pub failures: u64,
    pub cancellations: u64,
    /// Highest consecutive refusal count of any single call since start.
    pub peak_consecutive: u64,
    pub active: u64,
    pub prolonged: u64,
    /// Sum of consecutive refusal counts across currently active calls.
    pub consecutive: u64,
    pub oldest_millis: u64,
}

impl ReadRetryMetrics {
    // Instrumentation must remain usable in Drop and health reporting even if
    // a prior panic poisoned the lock; metrics must not cause a second panic.
    fn state(&self) -> std::sync::MutexGuard<'_, ReadRetryState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn snapshot(&self) -> ReadRetrySnapshot {
        let state = self.state();
        let now = tokio::time::Instant::now();
        let (mut prolonged, mut consecutive, mut oldest) = (0, 0, Duration::ZERO);
        for (at, count) in state.active.values() {
            let age = now.saturating_duration_since(*at);
            prolonged += u64::from(age >= READ_RETRY_PROLONGED);
            consecutive += count;
            oldest = oldest.max(age);
        }
        ReadRetrySnapshot {
            retries: state.retries,
            recoveries: state.recoveries,
            failures: state.failures,
            cancellations: state.cancellations,
            peak_consecutive: state.peak_consecutive,
            active: state.active.len() as u64,
            prolonged,
            consecutive,
            oldest_millis: u64::try_from(oldest.as_millis()).unwrap_or(u64::MAX),
        }
    }

    pub(crate) fn retrying(&self) -> ReadRetryGuard<'_> {
        ReadRetryGuard { metrics: self, id: None }
    }
}

pub(crate) struct ReadRetryGuard<'a> {
    metrics: &'a ReadRetryMetrics,
    id: Option<u64>,
}

impl ReadRetryGuard<'_> {
    pub(crate) fn busy(&mut self) {
        let mut state = self.metrics.state();
        let id = *self.id.get_or_insert_with(|| {
            let id = state.next_id;
            state.next_id += 1;
            id
        });
        let count = {
            let entry = state.active.entry(id).or_insert_with(|| (tokio::time::Instant::now(), 0));
            entry.1 += 1;
            entry.1
        };
        state.retries += 1;
        state.peak_consecutive = state.peak_consecutive.max(count);
    }

    pub(crate) fn finish(&mut self, recovered: bool) {
        if let Some(id) = self.id.take() {
            let mut state = self.metrics.state();
            state.active.remove(&id);
            if recovered {
                state.recoveries += 1;
            } else {
                state.failures += 1;
            }
        }
    }
}

impl Drop for ReadRetryGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let mut state = self.metrics.state();
            state.active.remove(&id);
            state.cancellations += 1;
        }
    }
}

fn read_retry_metrics() -> &'static [ReadRetryMetrics; ReadRetryConsumer::ALL.len()] {
    static METRICS: std::sync::OnceLock<[ReadRetryMetrics; ReadRetryConsumer::ALL.len()]> = std::sync::OnceLock::new();
    METRICS.get_or_init(Default::default)
}

pub(crate) fn read_retry_metrics_for(consumer: ReadRetryConsumer) -> &'static ReadRetryMetrics {
    &read_retry_metrics()[consumer.index()]
}

/// All retries in this process, including remote replication reads. These are
/// process metrics, independent of which backend a read uses.
pub fn read_retry_snapshot() -> BTreeMap<&'static str, ReadRetrySnapshot> {
    ReadRetryConsumer::ALL.into_iter().map(|consumer| (consumer.label(), read_retry_metrics_for(consumer).snapshot())).collect()
}

/// One durable store's single writer and its reader pool.
#[derive(Debug, Default)]
pub struct StoreLoadMetrics {
    /// Reads refused as busy, once per refusal returned to a caller.
    pub read_contention: Counter,
    /// Time from a write's submission until the writer thread starts it.
    pub write_wait: LatencyHistogram,
    pub write_execution: LatencyHistogram,
    /// Writes queued behind the running one, observed as each write starts;
    /// a burst that drains between starts is not seen.
    pub write_queue_peak: Peak,
    pub read_wait: LatencyHistogram,
    pub read_execution: LatencyHistogram,
    /// Reads queued behind those running, observed as each read starts.
    pub read_queue_peak: Peak,
    /// Reads the reader skipped because their caller gave up or their
    /// deadline passed while they queued; counted once, when skipped.
    pub reads_shed: Counter,
}

impl StoreLoadMetrics {
    pub fn snapshot(&self) -> StoreLoadSnapshot {
        StoreLoadSnapshot {
            write_wait: self.write_wait.snapshot(),
            write_execution: self.write_execution.snapshot(),
            write_queue_peak: self.write_queue_peak.get(),
            read_wait: self.read_wait.snapshot(),
            read_execution: self.read_execution.snapshot(),
            read_queue_peak: self.read_queue_peak.get(),
            reads_shed: self.reads_shed.get(),
            read_contention: self.read_contention.get(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreLoadSnapshot {
    pub write_wait: HistogramSnapshot,
    pub write_execution: HistogramSnapshot,
    pub write_queue_peak: u64,
    pub read_wait: HistogramSnapshot,
    pub read_execution: HistogramSnapshot,
    pub read_queue_peak: u64,
    pub reads_shed: u64,
    pub read_contention: u64,
}

impl StoreLoadSnapshot {
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            write_wait: self.write_wait.since(&earlier.write_wait),
            write_execution: self.write_execution.since(&earlier.write_execution),
            write_queue_peak: self.write_queue_peak,
            read_wait: self.read_wait.since(&earlier.read_wait),
            read_execution: self.read_execution.since(&earlier.read_execution),
            read_queue_peak: self.read_queue_peak,
            reads_shed: self.reads_shed.saturating_sub(earlier.reads_shed),
            read_contention: self.read_contention.saturating_sub(earlier.read_contention),
        }
    }
}

/// One controller loop (one resource kind).
#[derive(Debug, Default)]
pub struct ControllerLoadMetrics {
    /// Objects taken from the work queue.
    pub wakes: Counter,
    /// Prepare plus reconcile and its writes, per reconciled object.
    pub reconcile: LatencyHistogram,
    pub queue_wait: LatencyHistogram,
    busy: Mutex<BusyTime>,
    /// Reconciles that asked to run again after a delay, and requeues
    /// absorbed because one was already scheduled for the object.
    pub requeues: Counter,
    pub requeues_coalesced: Counter,
    pub failures: Counter,
    /// Periodic full resyncs and the objects each enqueued.
    pub resyncs: Counter,
    pub resync_enqueued: Counter,
    /// Work queue length, observed as each wake starts.
    pub queue_peak: Peak,
}

#[derive(Debug, Default)]
struct BusyTime {
    elapsed: Duration,
    active: Option<tokio::time::Instant>,
}

/// Counts all work, including reads, finalizers and failed attempts. Dropping
/// on cancellation closes the interval; snapshots include a running interval.
pub struct ControllerBusyGuard<'a>(&'a ControllerLoadMetrics);
impl Drop for ControllerBusyGuard<'_> {
    fn drop(&mut self) {
        let mut busy = self.0.busy.lock().expect("busy time");
        if let Some(at) = busy.active.take() {
            busy.elapsed += at.elapsed();
        }
    }
}

impl ControllerLoadMetrics {
    pub fn busy(&self) -> ControllerBusyGuard<'_> {
        let mut busy = self.busy.lock().expect("busy time");
        assert!(busy.active.is_none(), "one loop per controller metrics object");
        busy.active = Some(tokio::time::Instant::now());
        ControllerBusyGuard(self)
    }

    fn busy_micros(&self) -> u64 {
        let busy = self.busy.lock().expect("busy time");
        u64::try_from((busy.elapsed + busy.active.map_or(Duration::ZERO, |at| at.elapsed())).as_micros()).unwrap_or(u64::MAX)
    }
    pub fn snapshot(&self) -> ControllerLoadSnapshot {
        ControllerLoadSnapshot {
            wakes: self.wakes.get(),
            queue_wait: self.queue_wait.snapshot(),
            busy_micros: self.busy_micros(),
            reconcile: self.reconcile.snapshot(),
            requeues: self.requeues.get(),
            requeues_coalesced: self.requeues_coalesced.get(),
            failures: self.failures.get(),
            resyncs: self.resyncs.get(),
            resync_enqueued: self.resync_enqueued.get(),
            queue_peak: self.queue_peak.get(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControllerLoadSnapshot {
    pub wakes: u64,
    pub queue_wait: HistogramSnapshot,
    pub busy_micros: u64,
    pub reconcile: HistogramSnapshot,
    pub requeues: u64,
    pub requeues_coalesced: u64,
    pub failures: u64,
    pub resyncs: u64,
    pub resync_enqueued: u64,
    pub queue_peak: u64,
}

impl ControllerLoadSnapshot {
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            wakes: self.wakes.saturating_sub(earlier.wakes),
            queue_wait: self.queue_wait.since(&earlier.queue_wait),
            busy_micros: self.busy_micros.saturating_sub(earlier.busy_micros),
            reconcile: self.reconcile.since(&earlier.reconcile),
            requeues: self.requeues.saturating_sub(earlier.requeues),
            requeues_coalesced: self.requeues_coalesced.saturating_sub(earlier.requeues_coalesced),
            failures: self.failures.saturating_sub(earlier.failures),
            resyncs: self.resyncs.saturating_sub(earlier.resyncs),
            resync_enqueued: self.resync_enqueued.saturating_sub(earlier.resync_enqueued),
            queue_peak: self.queue_peak,
        }
    }
}

/// Every controller loop of one daemon, by resource kind.
#[derive(Debug, Default)]
pub struct ControllerMetricsRegistry {
    kinds: Mutex<BTreeMap<String, Arc<ControllerLoadMetrics>>>,
}

impl ControllerMetricsRegistry {
    /// The kind's metrics, shared by every restart of its loop.
    pub fn for_kind(&self, kind: &str) -> Arc<ControllerLoadMetrics> {
        let mut kinds = self.kinds.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(kinds.entry(kind.to_string()).or_default())
    }

    pub fn snapshot(&self) -> BTreeMap<String, ControllerLoadSnapshot> {
        let kinds = self.kinds.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        kinds.iter().map(|(kind, metrics)| (kind.clone(), metrics.snapshot())).collect()
    }
}

/// One daemon's replication of its peers' stores.
#[derive(Debug, Default)]
pub struct ReplicationLoadMetrics {
    /// Full snapshots taken, at first connect and after every resnapshot.
    pub snapshots: Counter,
    /// Watches dropped because the replicator fell behind its peer's stream.
    pub lag_resets: Counter,
    /// Watch streams that ended in an error and were restarted.
    pub stream_failures: Counter,
    /// Live watch events applied after the snapshot.
    pub events_applied: Counter,
}

impl ReplicationLoadMetrics {
    pub fn snapshot(&self) -> ReplicationLoadSnapshot {
        ReplicationLoadSnapshot {
            snapshots: self.snapshots.get(),
            lag_resets: self.lag_resets.get(),
            stream_failures: self.stream_failures.get(),
            events_applied: self.events_applied.get(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ReplicationLoadSnapshot {
    pub snapshots: u64,
    pub lag_resets: u64,
    pub stream_failures: u64,
    pub events_applied: u64,
}

impl ReplicationLoadSnapshot {
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            snapshots: self.snapshots.saturating_sub(earlier.snapshots),
            lag_resets: self.lag_resets.saturating_sub(earlier.lag_resets),
            stream_failures: self.stream_failures.saturating_sub(earlier.stream_failures),
            events_applied: self.events_applied.saturating_sub(earlier.events_applied),
        }
    }
}

#[cfg(test)]
mod tests {
    use hegel::generators as gs;

    use super::*;

    // Behaviour: every value lands in a bucket whose bound is at least the
    // value and within 1/16 of it, and buckets are ordered. The generator spans
    // zero, the exact sub-bucket boundaries and the whole u64 range.
    #[hegel::test]
    fn buckets_bound_their_values_within_a_sixteenth(tc: hegel::TestCase) {
        let exponent = tc.draw(gs::integers::<u32>().min_value(0).max_value(63));
        let offset = tc.draw(gs::integers::<u64>().min_value(0).max_value(u64::MAX));
        let value = if exponent == 0 { offset % u64::from(SUB_BUCKETS) } else { (1u64 << exponent) | (offset >> (64 - exponent)) };
        let index = bucket(value);
        let upper = bucket_upper(index);
        assert!(index < BUCKETS);
        assert!(upper >= value, "bucket {index} bound {upper} below {value}");
        assert!(upper - value <= value / 16 + 1, "bucket {index} bound {upper} too far above {value}");
        if index > 0 {
            assert!(bucket_upper(index - 1) < value, "{value} belongs in an earlier bucket than {index}");
        }
    }

    // The top bucket's bound must not wrap: values near u64::MAX microseconds
    // (as_micros saturating) land there.
    #[test]
    fn the_top_bucket_bounds_the_largest_values() {
        for value in [u64::MAX, u64::MAX - 1, (1 << 63) | (15 << 59)] {
            assert_eq!(bucket(value), BUCKETS - 1, "{value}");
            assert_eq!(bucket_upper(bucket(value)), u64::MAX);
        }
        let histogram = LatencyHistogram::default();
        histogram.record(Duration::MAX);
        assert_eq!(histogram.snapshot().summary().p99_micros, u64::MAX);
    }

    // Behaviour: quantiles of recorded latencies are bounded by the recorded
    // values (within the bucket precision), and `since` keeps only later records.
    #[hegel::test]
    fn quantiles_and_phases_follow_the_records(tc: hegel::TestCase) {
        // Latencies from zero to an hour, as lists that may be empty.
        let before = tc.draw(gs::vecs(gs::integers::<u64>().min_value(0).max_value(3_600_000_000)).max_size(40));
        let during = tc.draw(gs::vecs(gs::integers::<u64>().min_value(0).max_value(3_600_000_000)).max_size(40));
        let histogram = LatencyHistogram::default();
        for micros in &before {
            histogram.record(Duration::from_micros(*micros));
        }
        let earlier = histogram.snapshot();
        for micros in &during {
            histogram.record(Duration::from_micros(*micros));
        }
        let phase = histogram.snapshot().since(&earlier).summary();
        assert_eq!(phase.count, during.len() as u64);
        let mut sorted = during.clone();
        sorted.sort_unstable();
        if let (Some(first), Some(last)) = (sorted.first(), sorted.last()) {
            let median = sorted[sorted.len().div_ceil(2) - 1];
            assert!(
                phase.p50_micros >= median && phase.p50_micros <= median + median / 4 + 1,
                "p50 {} for median {median}",
                phase.p50_micros
            );
            assert!(phase.p99_micros >= phase.p50_micros && phase.p99_micros <= phase.max_micros);
            assert!(phase.max_micros >= *last && phase.max_micros <= last + last / 16 + 1, "phase max {} for {last}", phase.max_micros);
            assert_eq!(phase.mean_micros, during.iter().sum::<u64>() / during.len() as u64);
            assert!(phase.p50_micros >= *first);
        } else {
            assert_eq!((phase.p50_micros, phase.mean_micros), (0, 0));
        }
    }

    #[test]
    fn a_registry_shares_one_metrics_object_per_kind() {
        let registry = ControllerMetricsRegistry::default();
        registry.for_kind("Convoy").wakes.increment();
        registry.for_kind("Convoy").requeues.add(2);
        registry.for_kind("Vessel").resyncs.increment();
        let snapshot = registry.snapshot();
        assert_eq!((snapshot["Convoy"].wakes, snapshot["Convoy"].requeues), (1, 2));
        assert_eq!(snapshot["Vessel"].resyncs, 1);
        let later = registry.snapshot();
        assert_eq!(later["Convoy"].since(&snapshot["Convoy"]).wakes, 0);
    }
}

#[cfg(test)]
mod busy_tests {
    use super::*;

    // Snapshots clip active work at both window boundaries; idle time is excluded,
    // including after a cancelled/failed reconcile drops its guard.
    #[tokio::test(start_paused = true)]
    async fn busy_fraction_clips_work_crossing_the_window() {
        let metrics = ControllerLoadMetrics::default();
        let guard = metrics.busy();
        tokio::time::advance(Duration::from_secs(2)).await;
        let start = metrics.snapshot();
        tokio::time::advance(Duration::from_secs(3)).await;
        let end = metrics.snapshot();
        assert_eq!(end.since(&start).busy_micros, 3_000_000);
        drop(guard);
        tokio::time::advance(Duration::from_secs(7)).await;
        assert_eq!(metrics.snapshot().since(&end).busy_micros, 0);
        let _guard = metrics.busy();
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(metrics.snapshot().since(&end).busy_micros, 1_000_000);
    }
}

#[cfg(test)]
mod read_retry_tests {
    use super::*;

    // Generated interleavings cover idle calls, duplicate busy refusals, success,
    // error and cancellation. Current gauges equal the remaining live consumers;
    // cumulative retries and outcomes survive their removal.
    #[hegel::test]
    fn retry_lifecycle_tracks_only_live_consumers(tc: hegel::TestCase) {
        // Always exercise every outcome and overlapping consumers, then vary
        // the interleavings. Even an empty generated suffix covers all branches.
        let mut operations = vec![0, 3, 0, 1, 1, 0, 1, 3, 4, 0, 1, 5];
        operations.extend(tc.draw(hegel::generators::vecs(hegel::generators::integers::<u8>().min_value(0).max_value(5)).max_size(40)));
        tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap().block_on(async {
            let metrics = ReadRetryMetrics::default();
            assert_eq!(metrics.snapshot(), ReadRetrySnapshot::default());
            let mut guards = Vec::new();
            let mut counts = Vec::new();
            let (mut retries, mut recoveries, mut failures, mut cancellations, mut peak) = (0, 0, 0, 0, 0);
            for op in operations {
                match op {
                    0 => {
                        guards.push(metrics.retrying());
                        counts.push(0);
                    }
                    1 | 2 => {
                        if let Some(guard) = guards.last_mut() {
                            guard.busy();
                            *counts.last_mut().unwrap() += 1;
                            retries += 1;
                            peak = peak.max(*counts.last().unwrap());
                        }
                    }
                    _ => {
                        if let Some(mut guard) = guards.pop() {
                            let count = counts.pop().unwrap();
                            match op {
                                3 => {
                                    guard.finish(true);
                                    recoveries += u64::from(count > 0);
                                }
                                4 => {
                                    guard.finish(false);
                                    failures += u64::from(count > 0);
                                }
                                _ => {
                                    cancellations += u64::from(count > 0);
                                }
                            }
                        }
                    }
                }
                let snapshot = metrics.snapshot();
                assert_eq!(snapshot.retries, retries);
                assert_eq!(snapshot.recoveries, recoveries);
                assert_eq!(snapshot.failures, failures);
                assert_eq!(snapshot.cancellations, cancellations);
                assert_eq!(snapshot.peak_consecutive, peak);
                assert_eq!(snapshot.active, counts.iter().filter(|count| **count > 0).count() as u64);
                assert_eq!(snapshot.consecutive, counts.iter().sum::<u64>());
            }
            drop(guards);
            assert_eq!(metrics.snapshot().active, 0);
        });
    }

    // A poisoned instrumentation lock must not disable snapshots, retry
    // accounting or cancellation cleanup. One example covers the lock adapter.
    #[test]
    fn read_retry_poisoned_lock_remains_observable() {
        let metrics = ReadRetryMetrics::default();
        let poisoned = std::panic::catch_unwind(|| {
            let _state = metrics.state.lock().unwrap();
            panic!("poison instrumentation lock");
        });
        assert!(poisoned.is_err());
        assert_eq!(metrics.snapshot(), ReadRetrySnapshot::default());
        let mut recovered = metrics.retrying();
        recovered.busy();
        recovered.finish(true);
        let mut cancelled = metrics.retrying();
        cancelled.busy();
        drop(cancelled);
        let end = metrics.snapshot();
        assert_eq!((end.retries, end.recoveries, end.cancellations, end.active), (2, 1, 1, 0));
    }

    // The 30-second boundary includes exactly 30s, even when the next read is
    // still pending; finishing one consumer must not clear another one's signal.
    #[tokio::test(start_paused = true)]
    async fn prolonged_boundary_and_independent_recovery() {
        let metrics = ReadRetryMetrics::default();
        let mut first = metrics.retrying();
        first.busy();
        tokio::time::advance(READ_RETRY_PROLONGED - Duration::from_millis(1)).await;
        assert_eq!(metrics.snapshot().prolonged, 0);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(metrics.snapshot().prolonged, 1);
        let mut second = metrics.retrying();
        second.busy();
        first.finish(true);
        assert_eq!(metrics.snapshot().prolonged, 0);
        assert_eq!(metrics.snapshot().active, 1);
        tokio::time::advance(READ_RETRY_PROLONGED).await;
        assert_eq!(metrics.snapshot().prolonged, 1);
        drop(second);
        let end = metrics.snapshot();
        assert_eq!((end.active, end.prolonged, end.consecutive, end.oldest_millis), (0, 0, 0, 0));
        assert_eq!((end.retries, end.recoveries, end.cancellations), (2, 1, 1));
    }
}
