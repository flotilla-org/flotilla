use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_resources::{WatchEvent, WatchStart};
use flotilla_schema::reads::attributed;
use futures::StreamExt;
use tokio::{
    sync::{mpsc, Mutex},
    task::JoinHandle,
    time::Instant,
};
use tracing::{error, warn, Instrument};

use crate::{
    apply_status_patch,
    backend::{ReplicaReadResolver, ResourceBackend, TypedResolver},
    error::ResourceError,
    labels::LifecycleAuthority,
    load_metrics::ControllerLoadMetrics,
    resource::{InputMeta, Resource, ResourceObject},
};

#[allow(async_fn_in_trait)]
pub trait Reconciler: Send + Sync + 'static {
    type Resource: Resource;
    type Prepared;

    /// Prepare the state needed for `reconcile()`; this method may perform effects and must be idempotent, while `reconcile()` must be pure.
    async fn prepare(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Prepared, ResourceError>;

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        prepared: &Self::Prepared,
        now: DateTime<Utc>,
    ) -> ReconcileOutcome<Self::Resource>;

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError>;

    fn finalizer_name(&self) -> Option<&'static str>;

    /// Opt an object reconciler into a budget for repeated identical errors.
    /// Controllers without a policy are retried at a backoff capped at a
    /// minute, and parked after ten identical failures until a change wakes
    /// the object (see [`WorkQueueSender::parked`]).
    fn reconcile_error_policy(&self) -> Option<ReconcileErrorPolicy> {
        None
    }

    /// A durable retry wake suppresses attempts until its due time, when the
    /// loop wakes the resource itself. Explicit reconcile-now clears the
    /// resource's retry record.
    fn retry_disposition(&self, _obj: &ResourceObject<Self::Resource>) -> Option<crate::ControllerRetryDisposition> {
        None
    }

    /// Persist a resource-specific degraded condition once the reconcile
    /// error threshold is reached.
    fn reconcile_degraded_patch(
        &self,
        _obj: &ResourceObject<Self::Resource>,
        _failure: &ReconcileFailure,
    ) -> Option<<Self::Resource as Resource>::StatusPatch> {
        None
    }

    /// Report whether a persisted degraded condition is present. A `Park`
    /// policy uses this to survive controller restarts; a `Retry` policy keeps
    /// reconciling the object with backoff.
    fn is_reconcile_degraded(&self, _obj: &ResourceObject<Self::Resource>) -> bool {
        false
    }

    /// Allow a lifecycle change to wake an otherwise parked degraded object.
    /// This is consulted only for `Park` policies.
    async fn degraded_object_needs_reconcile(&self, _obj: &ResourceObject<Self::Resource>) -> Result<bool, ResourceError> {
        Ok(false)
    }

    /// Map a finalizer failure to a status update.
    ///
    /// Other object-scoped errors remain isolated and are retried with backoff
    /// without forcing the resource into a terminal state.
    fn finalizer_error_patch(
        &self,
        _obj: &ResourceObject<Self::Resource>,
        _error: &ResourceError,
    ) -> Option<<Self::Resource as Resource>::StatusPatch> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileErrorPolicy {
    /// Number of identical consecutive failures before the degraded condition
    /// is persisted and `exhaustion` takes effect.
    pub max_consecutive_failures: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub exhaustion: ReconcileErrorExhaustion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileErrorExhaustion {
    /// Persist the degraded condition and stop reconciling until a lifecycle
    /// change explicitly wakes the resource.
    Park,
    /// Persist the degraded condition but keep retrying at the capped backoff.
    Retry,
}

impl ReconcileErrorPolicy {
    /// What a reconciler without its own policy gets, and what a failed read of an object gets:
    /// retried at a backoff capped at a minute, then parked after ten identical failures, with
    /// the error told by [`WorkQueueSender::parked`]. Any change to the object wakes it at once
    /// and starts its count again.
    const UNBUDGETED: Self = Self {
        max_consecutive_failures: 10,
        initial_backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(60),
        exhaustion: ReconcileErrorExhaustion::Park,
    };

    fn backoff(&self, consecutive_failures: u32) -> Duration {
        let exponent = consecutive_failures.saturating_sub(1).min(31);
        self.initial_backoff.saturating_mul(1_u32 << exponent).min(self.max_backoff)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileFailure {
    /// Stable error identity for one object and failure cause. Reconcilers
    /// opting into a budget should avoid errors containing volatile data.
    pub message: String,
    pub consecutive_failures: u32,
}

struct ObjectFailure {
    failure: ReconcileFailure,
    creation_timestamp: DateTime<Utc>,
    retry_at: Instant,
    terminal: bool,
}

struct FinalizerFailure {
    creation_timestamp: DateTime<Utc>,
    consecutive_failures: u32,
    retry_at: Instant,
}

impl FinalizerFailure {
    fn delay(consecutive_failures: u32) -> Duration {
        Duration::from_secs(1).saturating_mul(1_u32 << consecutive_failures.saturating_sub(1).min(5))
    }
}

pub struct ReconcileOutcome<T: Resource> {
    pub patch: Option<T::StatusPatch>,
    pub actuations: Vec<Actuation>,
    pub requeue_after: Option<Duration>,
}

impl<T: Resource> ReconcileOutcome<T> {
    pub fn new(patch: Option<T::StatusPatch>) -> Self {
        Self { patch, actuations: Vec::new(), requeue_after: None }
    }

    pub fn with_actuations(patch: Option<T::StatusPatch>, actuations: Vec<Actuation>) -> Self {
        Self { patch, actuations, requeue_after: None }
    }
}

use flotilla_resources::Actuation;

pub trait SecondaryWatch: Send + Sync {
    type Primary: Resource;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>>;

    fn spawn(
        self: Box<Self>,
        backend: ResourceBackend,
        namespace: String,
        sender: WorkQueueSender,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>>;
}

/// Names a controller is asked to reconcile now (an operator's reconcile-now), as a secondary
/// watch that queues each name sent once it has started. A wake writes nothing, so it serves
/// records admitted on the engine, which no generic write may touch.
pub struct NamedWakes<R> {
    names: tokio::sync::broadcast::Sender<String>,
    kind: PhantomData<fn() -> R>,
}

impl<R> Clone for NamedWakes<R> {
    fn clone(&self) -> Self {
        Self { names: self.names.clone(), kind: PhantomData }
    }
}

impl<R> Default for NamedWakes<R> {
    fn default() -> Self {
        Self { names: tokio::sync::broadcast::channel(64).0, kind: PhantomData }
    }
}

impl<R> NamedWakes<R> {
    /// Queues `name` at every controller watching these wakes; false when none is.
    pub fn wake(&self, name: &str) -> bool {
        self.names.send(name.to_owned()).is_ok()
    }
}

impl<R: Resource> SecondaryWatch for NamedWakes<R> {
    type Primary = R;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        _backend: ResourceBackend,
        _namespace: String,
        sender: WorkQueueSender,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
        // Subscribed as the watch is spawned, so a wake sent after that is queued.
        let mut names = self.names.subscribe();
        Box::pin(async move {
            loop {
                match names.recv().await {
                    Ok(name) => {
                        if sender.send(name).await.is_err() {
                            return Ok(());
                        }
                    }
                    // A missed wake is covered by the controller's resync.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        warn!(missed, kind = R::API_PATHS.kind, "a controller fell behind its reconcile-now wakes");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        })
    }
}

/// Nonblocking, deduplicating producer for a controller's work queue.
/// A name remains queued until the controller starts processing it.
#[derive(Clone)]
pub struct WorkQueueSender {
    sender: mpsc::UnboundedSender<String>,
    progress: Arc<StdMutex<QueueProgress>>,
    every: Arc<tokio::sync::Notify>,
    metrics: Arc<ControllerLoadMetrics>,
}

#[derive(Default)]
struct QueueProgress {
    queued: BTreeMap<String, Instant>,
    processing: Option<String>,
    last_progress: Option<DateTime<Utc>>,
    /// Objects that stopped being retried, with the error that parked them.
    parked: BTreeMap<String, String>,
}

impl WorkQueueSender {
    /// Queue progress uses wall time, like the production judge. A judge with
    /// an injected virtual clock must compare against these recorded wall-time
    /// instants (or advance relative to them); advancing only its clock does not
    /// simulate progress in the real controller loop.
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<String>) {
        Self::metered(Arc::default())
    }

    /// A queue whose sends are counted into `metrics`.
    fn metered(metrics: Arc<ControllerLoadMetrics>) -> (Self, mpsc::UnboundedReceiver<String>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self { sender, progress: Arc::default(), every: Arc::default(), metrics }, receiver)
    }

    /// Enqueue completes without suspension; the async signature keeps watch producers uniform.
    pub async fn send(&self, name: String) -> Result<(), mpsc::error::SendError<String>> {
        self.metrics.enqueued.increment();
        let mut progress = self.progress.lock().expect("controller work queue lock poisoned");
        if progress.queued.is_empty() && progress.processing.is_none() {
            progress.last_progress = Some(flotilla_resources::clock::now());
        }
        if progress.queued.contains_key(&name) {
            self.metrics.enqueued_coalesced.increment();
        } else {
            progress.queued.insert(name.clone(), Instant::now());
            if let Err(error) = self.sender.send(name.clone()) {
                progress.queued.remove(&name);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Asks the controller to queue every resource it holds, for a change that names none of them.
    /// Requests made before the controller lists coalesce into one listing.
    pub fn send_every(&self) {
        self.every.notify_one();
    }

    fn start(&self, name: &str) -> Duration {
        let mut progress = self.progress.lock().expect("controller work queue lock poisoned");
        let wait = progress.queued.remove(name).map_or(Duration::ZERO, |at| at.elapsed());
        progress.processing = Some(name.into());
        progress.last_progress = Some(flotilla_resources::clock::now());
        wait
    }

    fn made_progress(&self) {
        let mut progress = self.progress.lock().expect("controller work queue lock poisoned");
        progress.processing = None;
        progress.last_progress = Some(flotilla_resources::clock::now());
    }

    /// Why the controller stopped retrying `name`, if it has: its last error, repeated until the
    /// retries ran out. A change to the object wakes it again.
    pub fn parked(&self, name: &str) -> Option<String> {
        self.progress.lock().expect("controller work queue lock poisoned").parked.get(name).cloned()
    }

    /// Returns how many objects are parked now.
    fn park(&self, name: &str, error: String) -> usize {
        let mut progress = self.progress.lock().expect("controller work queue lock poisoned");
        progress.parked.insert(name.to_owned(), error);
        progress.parked.len()
    }

    /// Returns how many objects are parked now.
    fn unpark(&self, name: &str) -> usize {
        let mut progress = self.progress.lock().expect("controller work queue lock poisoned");
        progress.parked.remove(name);
        progress.parked.len()
    }

    /// When outstanding work without loop progress reaches `bound`, unless the loop progresses
    /// first; none while the queue is idle.
    pub fn stall_due(&self, bound: Duration) -> Option<DateTime<Utc>> {
        let progress = self.progress.lock().expect("controller work queue lock poisoned");
        if progress.queued.is_empty() && progress.processing.is_none() {
            return None;
        }
        Some(progress.last_progress? + chrono::Duration::from_std(bound).expect("controller bound fits"))
    }

    /// Queue age alone is not a failure: a busy loop making progress is able.
    /// An idle loop owes nothing. Only outstanding work without loop progress
    /// for the bound is evidence that the owning controller is stuck.
    pub fn stall_reason(&self, now: DateTime<Utc>, bound: Duration) -> Option<String> {
        let progress = self.progress.lock().expect("controller work queue lock poisoned");
        if progress.queued.is_empty() && progress.processing.is_none() {
            return None;
        }
        let age = now.signed_duration_since(progress.last_progress?);
        (age >= chrono::Duration::from_std(bound).expect("controller bound fits")).then(|| {
            format!(
                "controller has made no progress for {}s; processing {}; {} queued",
                age.num_seconds(),
                progress.processing.as_deref().unwrap_or("no object"),
                progress.queued.len()
            )
        })
    }
}

impl<P: Resource> std::clone::Clone for Box<dyn SecondaryWatch<Primary = P>> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

#[derive(Clone)]
pub struct LabelMappedWatch<W: Resource, P: Resource> {
    pub label_key: &'static str,
    pub _marker: PhantomData<(W, P)>,
}

impl<W: Resource, P: Resource> LabelMappedWatch<W, P> {
    async fn enqueue_from_object(
        label_key: &'static str,
        sender: &WorkQueueSender,
        object: &ResourceObject<W>,
    ) -> Result<(), ResourceError> {
        if let Some(primary) = crate::label_value(&object.metadata.labels, label_key) {
            sender
                .send(primary.clone())
                .await
                .map_err(|_| ResourceError::other("controller queue closed while forwarding secondary event"))?;
        }
        Ok(())
    }
}

impl<W: Resource, P: Resource> SecondaryWatch for LabelMappedWatch<W, P> {
    type Primary = P;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(Self { label_key: self.label_key, _marker: PhantomData })
    }

    fn spawn(
        self: Box<Self>,
        backend: ResourceBackend,
        namespace: String,
        sender: WorkQueueSender,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            let resolver = backend.using::<W>(&namespace);
            // Watch before listing (see `spawn_primary_watch`).
            let mut watch = resolver.watch(WatchStart::Now).await?;
            let listed = resolver.list().await?;
            for object in &listed.items {
                Self::enqueue_from_object(self.label_key, &sender, object).await?;
            }
            while let Some(event) = watch.next().await {
                match event? {
                    WatchEvent::Added(object) | WatchEvent::Modified(object) | WatchEvent::Deleted(object) => {
                        Self::enqueue_from_object(self.label_key, &sender, &object).await?;
                    }
                    WatchEvent::DeletedByName(_) => {}
                }
            }
            Ok(())
        })
    }
}

#[derive(Clone)]
pub struct ResolverLabelMappedWatch<W: Resource, P: Resource> {
    pub label_key: &'static str,
    pub resolver: TypedResolver<W>,
    pub _marker: PhantomData<P>,
}

impl<W: Resource, P: Resource> SecondaryWatch for ResolverLabelMappedWatch<W, P> {
    type Primary = P;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(Self { label_key: self.label_key, resolver: self.resolver.clone(), _marker: PhantomData })
    }

    fn spawn(
        self: Box<Self>,
        _backend: ResourceBackend,
        _namespace: String,
        sender: WorkQueueSender,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            // Watch before listing (see `spawn_primary_watch`).
            let mut watch = self.resolver.watch(WatchStart::Now).await?;
            let listed = self.resolver.list().await?;
            for object in &listed.items {
                LabelMappedWatch::<W, P>::enqueue_from_object(self.label_key, &sender, object).await?;
            }
            while let Some(event) = watch.next().await {
                match event? {
                    WatchEvent::Added(object) | WatchEvent::Modified(object) | WatchEvent::Deleted(object) => {
                        LabelMappedWatch::<W, P>::enqueue_from_object(self.label_key, &sender, &object).await?;
                    }
                    WatchEvent::DeletedByName(_) => {}
                }
            }
            Ok(())
        })
    }
}

#[derive(Clone)]
pub struct ReplicaLabelMappedWatch<W: Resource, P: Resource> {
    pub label_key: &'static str,
    pub resolver: ReplicaReadResolver<W>,
    pub _marker: PhantomData<P>,
}

impl<W: Resource, P: Resource> SecondaryWatch for ReplicaLabelMappedWatch<W, P> {
    type Primary = P;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(Self { label_key: self.label_key, resolver: self.resolver.clone(), _marker: PhantomData })
    }

    fn spawn(
        self: Box<Self>,
        _backend: ResourceBackend,
        _namespace: String,
        sender: WorkQueueSender,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            let mut watch = self.resolver.watch().await?;
            let listed = self.resolver.list().await?;
            for source in &listed.items {
                LabelMappedWatch::<W, P>::enqueue_from_object(self.label_key, &sender, &source.object).await?;
            }
            while let Some(event) = watch.next().await {
                let source = match event? {
                    crate::ReadWatchEvent::Added(source)
                    | crate::ReadWatchEvent::Modified(source)
                    | crate::ReadWatchEvent::Deleted(source) => source,
                    crate::ReadWatchEvent::DeletedByName { .. } => continue,
                };
                LabelMappedWatch::<W, P>::enqueue_from_object(self.label_key, &sender, &source.object).await?;
            }
            Ok(())
        })
    }
}

#[derive(Clone)]
pub struct LabelJoinWatch<W: Resource, P: Resource> {
    pub label_key: &'static str,
    pub _marker: PhantomData<(W, P)>,
}

impl<W: Resource, P: Resource> LabelJoinWatch<W, P> {
    async fn enqueue_matching_primaries(
        label_key: &'static str,
        sender: &WorkQueueSender,
        watched: &ResourceObject<W>,
        primaries: &TypedResolver<P>,
    ) -> Result<(), ResourceError> {
        let Some(value) = crate::label_value(&watched.metadata.labels, label_key) else {
            return Ok(());
        };
        let selector = BTreeMap::from([(label_key.to_string(), value.clone())]);
        let listed = primaries.list_matching_labels(&selector).await?;
        for object in listed.items {
            sender
                .send(object.metadata.name)
                .await
                .map_err(|_| ResourceError::other("controller queue closed while forwarding joined secondary event"))?;
        }
        Ok(())
    }
}

impl<W: Resource, P: Resource> SecondaryWatch for LabelJoinWatch<W, P> {
    type Primary = P;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(Self { label_key: self.label_key, _marker: PhantomData })
    }

    fn spawn(
        self: Box<Self>,
        backend: ResourceBackend,
        namespace: String,
        sender: WorkQueueSender,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            let watched = backend.clone().using::<W>(&namespace);
            let primaries = backend.using::<P>(&namespace);
            // Watch before listing (see `spawn_primary_watch`).
            let mut watch = watched.watch(WatchStart::Now).await?;
            let listed = watched.list().await?;
            for object in &listed.items {
                Self::enqueue_matching_primaries(self.label_key, &sender, object, &primaries).await?;
            }
            while let Some(event) = watch.next().await {
                match event? {
                    WatchEvent::Added(object) | WatchEvent::Modified(object) | WatchEvent::Deleted(object) => {
                        Self::enqueue_matching_primaries(self.label_key, &sender, &object, &primaries).await?;
                    }
                    WatchEvent::DeletedByName(_) => {}
                }
            }
            Ok(())
        })
    }
}

enum WatchExited {
    Primary(Result<(), ResourceError>),
    Secondary { index: usize, result: Result<(), ResourceError> },
}

// Tokio handles detach on drop. Keep every current watch, including respawns,
// owned by the loop so shutdown also cancels a watch's in-flight read/backoff.
struct ControllerWatches {
    primary: JoinHandle<()>,
    secondaries: Vec<JoinHandle<()>>,
}
impl Drop for ControllerWatches {
    fn drop(&mut self) {
        self.primary.abort();
        for watch in &self.secondaries {
            watch.abort();
        }
    }
}

pub struct ControllerLoop<R: Reconciler> {
    pub primary: TypedResolver<R::Resource>,
    pub secondaries: Vec<Box<dyn SecondaryWatch<Primary = R::Resource>>>,
    pub reconciler: R,
    pub backend: ResourceBackend,
}

const WATCH_RESTART_BACKOFF: Duration = Duration::from_millis(100);
const SECONDARY_BACKOFF_CAP: Duration = Duration::from_secs(30);
const SECONDARY_STABLE_RUN: Duration = Duration::from_secs(60);

/// Doubles per early exit up to a cap; a watch that ran for a while starts over.
fn next_secondary_backoff(previous: Duration, ran_for: Duration) -> Duration {
    if ran_for >= SECONDARY_STABLE_RUN {
        WATCH_RESTART_BACKOFF
    } else {
        previous.saturating_mul(2).min(SECONDARY_BACKOFF_CAP)
    }
}

impl<R: Reconciler> ControllerLoop<R> {
    async fn write_tolerating_not_found<T>(write: impl Future<Output = Result<T, ResourceError>>) -> Result<(), ResourceError> {
        match write.await {
            Ok(_) | Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(err) => Err(err),
        }
    }

    async fn apply_actuation(backend: &ResourceBackend, namespace: &str, actuation: Actuation) -> Result<(), ResourceError> {
        match actuation {
            Actuation::CreateRepository { key, spec } => {
                let resolver = backend.using::<crate::Repository>(namespace);
                crate::ensure_repository(&resolver, &key, &spec).await.map(|_| ())
            }
            Actuation::RaiseAttention { attention } => {
                backend.attentions().raise(namespace, &attention).await.map_err(ResourceError::other)
            }
            Actuation::ClearAttention { sender, subject } => {
                backend.attentions().clear(namespace, sender, &subject).await.map_err(ResourceError::other)
            }
        }
    }

    fn spawn_primary_watch(
        primary: TypedResolver<R::Resource>,
        sender: WorkQueueSender,
        watch_exited: mpsc::UnboundedSender<WatchExited>,
        restart_backoff: Option<Duration>,
    ) -> JoinHandle<()> {
        let reads = Arc::clone(&sender.metrics.watch_reads);
        tokio::spawn(attributed(reads, async move {
            if let Some(backoff) = restart_backoff {
                tokio::time::sleep(backoff).await;
            }
            let result = async {
                // Watch before listing: an engine-served kind's watch cannot resume from a listing,
                // so a change between the two is in the list, the watch or both. Every event only
                // queues a name for a reconcile of its current state, so one seen twice costs a
                // repeated reconcile, never a wrong one.
                let mut watch = primary.watch(WatchStart::Now).await?;
                let listed = primary.list().await?;
                for object in &listed.items {
                    sender
                        .send(object.metadata.name.clone())
                        .await
                        .map_err(|_| ResourceError::other("controller queue closed while forwarding initial primary list"))?;
                }
                while let Some(event) = watch.next().await {
                    match event? {
                        WatchEvent::Added(object) | WatchEvent::Modified(object) | WatchEvent::Deleted(object) => {
                            sender
                                .send(object.metadata.name)
                                .await
                                .map_err(|_| ResourceError::other("controller queue closed while forwarding primary watch event"))?;
                        }
                        WatchEvent::DeletedByName(tombstone) => {
                            sender
                                .send(tombstone.name)
                                .await
                                .map_err(|_| ResourceError::other("controller queue closed while forwarding primary tombstone"))?;
                        }
                    }
                }
                Ok(())
            }
            .await;

            let _ = watch_exited.send(WatchExited::Primary(result));
        }))
    }

    fn spawn_secondary_watch(
        index: usize,
        watch: Box<dyn SecondaryWatch<Primary = R::Resource>>,
        backend: ResourceBackend,
        namespace: String,
        sender: WorkQueueSender,
        watch_exited: mpsc::UnboundedSender<WatchExited>,
        restart_backoff: Option<Duration>,
    ) -> JoinHandle<()> {
        let reads = Arc::clone(&sender.metrics.watch_reads);
        tokio::spawn(attributed(reads, async move {
            if let Some(backoff) = restart_backoff {
                tokio::time::sleep(backoff).await;
            }
            let result = watch.spawn(backend, namespace, sender).await;
            let _ = watch_exited.send(WatchExited::Secondary { index, result });
        }))
    }

    /// Queues `name` again after `delay`, unless a requeue of it is already scheduled.
    async fn requeue_after(
        scheduled_requeues: &Arc<Mutex<HashSet<String>>>,
        sender: &WorkQueueSender,
        name: &str,
        delay: Duration,
    ) -> bool {
        let mut scheduled = scheduled_requeues.lock().await;
        if !scheduled.insert(name.to_owned()) {
            return false;
        }
        let scheduled_requeues = Arc::clone(scheduled_requeues);
        let sender = sender.clone();
        let name = name.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            scheduled_requeues.lock().await.remove(&name);
            let _ = sender.send(name).await;
        });
        true
    }

    async fn resync_all(primary: &TypedResolver<R::Resource>, sender: &WorkQueueSender) -> Result<HashSet<String>, ResourceError> {
        let listed = primary.list().await?;
        let mut live_names = HashSet::with_capacity(listed.items.len());
        for object in listed.items {
            live_names.insert(object.metadata.name.clone());
            sender
                .send(object.metadata.name)
                .await
                .map_err(|_| ResourceError::other("controller queue closed while forwarding resync item"))?;
        }
        Ok(live_names)
    }

    fn accept_restartable_watch_exit(result: Result<(), ResourceError>) -> Result<(), ResourceError> {
        match result {
            Ok(()) | Err(ResourceError::WatchExpired { .. }) => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub async fn run(self) -> Result<(), ResourceError>
    where
        <R::Resource as Resource>::Status: Default,
    {
        self.run_instrumented(Arc::default()).await
    }

    /// [`Self::run`], also recording wakes, reconcile latency, requeues,
    /// resyncs and the engine reads of its passes and watches into `metrics`
    /// for load measurement (#3206).
    pub async fn run_observed(self, metrics: Arc<ControllerLoadMetrics>) -> Result<(), ResourceError>
    where
        <R::Resource as Resource>::Status: Default,
    {
        self.run_instrumented(metrics).await
    }

    async fn run_instrumented(self, metrics: Arc<ControllerLoadMetrics>) -> Result<(), ResourceError>
    where
        <R::Resource as Resource>::Status: Default,
    {
        attributed(Arc::clone(&metrics.reads), self.run_passes(metrics)).await
    }

    async fn run_passes(self, metrics: Arc<ControllerLoadMetrics>) -> Result<(), ResourceError>
    where
        <R::Resource as Resource>::Status: Default,
    {
        let ControllerLoop { primary, secondaries, reconciler, backend } = self;
        let (sender, mut receiver) = WorkQueueSender::metered(Arc::clone(&metrics));
        let every = Arc::clone(&sender.every);
        let (watch_exited_tx, mut watch_exited_rx) = mpsc::unbounded_channel();
        let primary_watch = Self::spawn_primary_watch(primary.clone(), sender.clone(), watch_exited_tx.clone(), None);
        let secondary_templates = secondaries;
        let secondary_watches: Vec<JoinHandle<()>> = secondary_templates
            .iter()
            .enumerate()
            .map(|(index, watch)| {
                Self::spawn_secondary_watch(
                    index,
                    watch.clone(),
                    backend.clone(),
                    primary.namespace.clone(),
                    sender.clone(),
                    watch_exited_tx.clone(),
                    None,
                )
            })
            .collect();
        let mut watches = ControllerWatches { primary: primary_watch, secondaries: secondary_watches };
        let mut secondary_started = vec![Instant::now(); secondary_templates.len()];
        let mut secondary_backoff = vec![WATCH_RESTART_BACKOFF / 2; secondary_templates.len()];
        let mut pending: VecDeque<String> = VecDeque::new();
        let scheduled_requeues = Arc::new(Mutex::new(HashSet::new()));
        let mut object_failures = BTreeMap::<String, ObjectFailure>::new();
        let mut finalizer_failures = BTreeMap::<String, FinalizerFailure>::new();
        let mut read_failures = BTreeMap::<String, u32>::new();

        loop {
            sender.made_progress();
            if let Some(name) = pending.pop_front() {
                metrics.wakes.increment();
                metrics.woke(&name);
                metrics.queue_peak.observe(pending.len() as u64 + 1);
                let woke_at = Instant::now();
                metrics.queue_wait.record(sender.start(&name));
                let _busy = metrics.busy();
                let object = match primary.get(&name).await {
                    Ok(object) => {
                        read_failures.remove(&name);
                        object
                    }
                    Err(ResourceError::NotFound { .. }) => {
                        read_failures.remove(&name);
                        object_failures.remove(&name);
                        finalizer_failures.remove(&name);
                        metrics.parked.set(sender.unpark(&name) as u64);
                        continue;
                    }
                    Err(err) => {
                        let limit = ReconcileErrorPolicy::UNBUDGETED.max_consecutive_failures;
                        let failures = read_failures.entry(name.clone()).or_insert(0);
                        // A parked object is read again only when a change wakes it: count afresh.
                        *failures = if *failures >= limit { 1 } else { failures.saturating_add(1) };
                        if *failures >= limit {
                            error!(
                                resource_kind = R::Resource::API_PATHS.kind,
                                resource = %name,
                                %err,
                                failures = *failures,
                                "controller could not read object; parked until it changes",
                            );
                            metrics.parked.set(sender.park(&name, format!("reading it failed {} times: {err}", *failures)) as u64);
                            continue;
                        }
                        let delay = ReconcileErrorPolicy::UNBUDGETED.backoff(*failures);
                        warn!(
                            resource_kind = R::Resource::API_PATHS.kind,
                            resource = %name,
                            %err,
                            retry_in = ?delay,
                            "controller failed to read object; retrying it",
                        );
                        Self::requeue_after(&scheduled_requeues, &sender, &name, delay).await;
                        continue;
                    }
                };
                if finalizer_failures.get(&name).is_some_and(|failure| failure.creation_timestamp != object.metadata.creation_timestamp) {
                    finalizer_failures.remove(&name);
                }
                let mut attempted_reconcile = false;
                let mut attempted_finalizer = false;
                // One span per reconcile, named by kind: what it logs carries its trace.
                let span = tracing::info_span!("reconcile", kind = R::Resource::API_PATHS.kind);
                let result = async {
                    let lifecycle_owned = is_lifecycle_owned(object.metadata.lifecycle_authority()?);
                    if !lifecycle_owned
                        || object.metadata.deletion_timestamp.is_none()
                        || reconciler
                            .finalizer_name()
                            .is_none_or(|finalizer| object.metadata.finalizers.iter().all(|existing| existing != finalizer))
                    {
                        finalizer_failures.remove(&name);
                    }
                    if let Some(finalizer_name) = reconciler.finalizer_name() {
                        if object.metadata.deletion_timestamp.is_none()
                            && object.metadata.finalizers.iter().all(|finalizer| finalizer != finalizer_name)
                            && lifecycle_owned
                        {
                            let meta = InputMeta::from(&object.metadata).with_added_finalizer(finalizer_name);
                            Self::write_tolerating_not_found(primary.update(&meta, &object.metadata.resource_version, &object.spec))
                                .await?;
                            return Ok(());
                        }
                        if object.metadata.deletion_timestamp.is_some()
                            && object.metadata.finalizers.iter().any(|finalizer| finalizer == finalizer_name)
                        {
                            if lifecycle_owned {
                                if finalizer_failures.get(&name).is_some_and(|failure| Instant::now() < failure.retry_at) {
                                    return Ok(());
                                }
                                attempted_finalizer = true;
                                if let Err(err) = reconciler.run_finalizer(&object).await {
                                    if matches!(err, ResourceError::FinalizerPending) {
                                        return Err(err);
                                    }
                                    if let Some(patch) = reconciler.finalizer_error_patch(&object, &err) {
                                        if let Err(patch_error) =
                                            Self::write_tolerating_not_found(apply_status_patch(&primary, &name, &patch)).await
                                        {
                                            warn!(
                                                resource_kind = R::Resource::API_PATHS.kind,
                                                resource = %name,
                                                %patch_error,
                                                "controller failed to record object finalizer error",
                                            );
                                        }
                                    }
                                    return Err(err);
                                }
                            }
                            let meta = InputMeta::from(&object.metadata).without_finalizer(finalizer_name);
                            Self::write_tolerating_not_found(primary.update(&meta, &object.metadata.resource_version, &object.spec))
                                .await?;
                            return Ok(());
                        }
                    }
                    if object.metadata.deletion_timestamp.is_some() {
                        return Ok(());
                    }
                    if !lifecycle_owned {
                        return Ok(());
                    }
                    let degraded_is_parked = reconciler.is_reconcile_degraded(&object)
                        && reconciler.reconcile_error_policy().is_none_or(|policy| policy.exhaustion == ReconcileErrorExhaustion::Park);
                    let degraded_needs_reconcile = degraded_is_parked && reconciler.degraded_object_needs_reconcile(&object).await?;
                    if degraded_is_parked && !degraded_needs_reconcile {
                        return Ok(());
                    }
                    if object_failures.get(&name).is_some_and(|failure| failure.creation_timestamp != object.metadata.creation_timestamp) {
                        object_failures.remove(&name);
                    }
                    if let Some(disposition) = reconciler.retry_disposition(&object) {
                        match disposition {
                            crate::ControllerRetryDisposition::Retryable { next_attempt_at }
                                if flotilla_resources::clock::now() < next_attempt_at =>
                            {
                                let delay = (next_attempt_at - flotilla_resources::clock::now()).to_std().unwrap_or_default();
                                Self::requeue_after(&scheduled_requeues, &sender, &name, delay).await;
                                return Ok(());
                            }
                            crate::ControllerRetryDisposition::Terminal { .. } => return Ok(()),
                            _ => {}
                        }
                    }
                    // A budgeted failure holds the object until its retry; an unbudgeted one is only
                    // retried, and any change still wakes it at once.
                    if let Some(failure) = object_failures.get(&name).filter(|_| reconciler.reconcile_error_policy().is_some()) {
                        if !degraded_needs_reconcile && (failure.terminal || Instant::now() < failure.retry_at) {
                            return Ok(());
                        }
                    }
                    attempted_reconcile = true;
                    let prepared = reconciler.prepare(&object).await?;
                    let outcome = reconciler.reconcile(&object, &prepared, flotilla_resources::clock::now());
                    for actuation in outcome.actuations {
                        Self::apply_actuation(&primary.backend, &primary.namespace, actuation).await?;
                    }
                    if let Some(patch) = outcome.patch {
                        Self::write_tolerating_not_found(apply_status_patch(&primary, &name, &patch)).await?;
                    }
                    if let Some(delay) = outcome.requeue_after {
                        metrics.requeues.increment();
                        if !Self::requeue_after(&scheduled_requeues, &sender, &name, delay).await {
                            metrics.requeues_coalesced.increment();
                        }
                    }
                    Ok(())
                }
                .instrument(span.clone())
                .await;
                if attempted_reconcile {
                    metrics.reconcile.record(woke_at.elapsed());
                }
                if matches!(&result, Err(error) if !matches!(error, ResourceError::FinalizerPending)) {
                    metrics.failures.increment();
                }
                match result {
                    Err(ResourceError::FinalizerPending) => {
                        Self::requeue_after(&scheduled_requeues, &sender, &name, Duration::from_secs(1)).await;
                    }
                    Ok(()) if attempted_reconcile => {
                        object_failures.remove(&name);
                        metrics.parked.set(sender.unpark(&name) as u64);
                    }
                    Ok(()) if attempted_finalizer => {
                        finalizer_failures.remove(&name);
                    }
                    Ok(()) => {}
                    Err(err) => {
                        if attempted_reconcile {
                            // Counted with the kind's controller failures (#3206); told here, not stored.
                            span.in_scope(
                                || warn!(resource_kind = R::Resource::API_PATHS.kind, resource = %name, error = %err, "reconcile failed"),
                            );
                        }
                        let mut terminal = false;
                        // A finalizer or its removal write may fail without
                        // changing status. Retry it with bounded backoff instead
                        // of spinning on a watch event.
                        let mut retry_after = attempted_finalizer.then(|| {
                            let consecutive_failures =
                                finalizer_failures.get(&name).map_or(1, |failure| failure.consecutive_failures.saturating_add(1));
                            let delay = FinalizerFailure::delay(consecutive_failures);
                            finalizer_failures.insert(
                                name.clone(),
                                FinalizerFailure {
                                    creation_timestamp: object.metadata.creation_timestamp,
                                    consecutive_failures,
                                    retry_at: Instant::now() + delay,
                                },
                            );
                            delay
                        });
                        if attempted_reconcile {
                            let unbudgeted = reconciler.reconcile_error_policy().is_none();
                            let policy = reconciler.reconcile_error_policy().unwrap_or(ReconcileErrorPolicy::UNBUDGETED);
                            let message = err.to_string();
                            let previous = object_failures.get(&name);
                            // A parked unbudgeted object is reconciled again only when a change
                            // wakes it: count afresh.
                            let consecutive_failures = previous
                                .filter(|previous| previous.failure.message == message && !(unbudgeted && previous.terminal))
                                .map_or(1, |previous| previous.failure.consecutive_failures.saturating_add(1));
                            let failure = ReconcileFailure { message, consecutive_failures };
                            let exhausted = consecutive_failures >= policy.max_consecutive_failures;
                            terminal = exhausted && policy.exhaustion == ReconcileErrorExhaustion::Park;
                            let delay = policy.backoff(consecutive_failures);
                            if exhausted {
                                if let Some(patch) = reconciler.reconcile_degraded_patch(&object, &failure) {
                                    match apply_status_patch(&primary, &name, &patch).await {
                                        Ok(_) => {}
                                        Err(ResourceError::NotFound { .. }) => {
                                            object_failures.remove(&name);
                                            continue;
                                        }
                                        Err(patch_error) => {
                                            terminal = false;
                                            warn!(
                                                resource_kind = R::Resource::API_PATHS.kind,
                                                resource = %name,
                                                %patch_error,
                                                "controller failed to record degraded reconcile condition; retrying",
                                            );
                                        }
                                    }
                                } else if !unbudgeted {
                                    terminal = false;
                                }
                            }
                            if !terminal {
                                retry_after = Some(delay);
                            } else if unbudgeted {
                                error!(
                                    resource_kind = R::Resource::API_PATHS.kind,
                                    resource = %name,
                                    %err,
                                    failures = failure.consecutive_failures,
                                    "controller parked object until it changes",
                                );
                                let parked = sender.park(
                                    &name,
                                    format!("reconciling it failed {} times: {}", failure.consecutive_failures, failure.message),
                                );
                                metrics.parked.set(parked as u64);
                            }
                            object_failures.insert(
                                name.clone(),
                                ObjectFailure {
                                    failure,
                                    creation_timestamp: object.metadata.creation_timestamp,
                                    retry_at: Instant::now() + delay,
                                    terminal,
                                },
                            );
                        }
                        span.in_scope(|| {
                            warn!(
                                resource_kind = R::Resource::API_PATHS.kind,
                                resource = %name,
                                %err,
                                terminal,
                                "controller failed to reconcile object; other objects will continue",
                            )
                        });
                        if let Some(delay) = retry_after {
                            Self::requeue_after(&scheduled_requeues, &sender, &name, delay).await;
                        }
                    }
                }
                continue;
            }

            tokio::select! {
                maybe_name = receiver.recv() => {
                    let Some(name) = maybe_name else {
                        return Ok(());
                    };
                    while let Ok(next) = receiver.try_recv() {
                        pending.push_back(next);
                    }
                    pending.push_front(name);
                }
                _ = every.notified() => {
                    let _busy = metrics.busy();
                    let live_names = Self::resync_all(&primary, &sender).await?;
                    metrics.resyncs.increment();
                    metrics.resync_enqueued.add(live_names.len() as u64);
                    finalizer_failures.retain(|name, _| live_names.contains(name));
                }
                Some(exited) = watch_exited_rx.recv() => {
                    let _busy = metrics.busy();
                    match exited {
                        WatchExited::Primary(result) => {
                            Self::accept_restartable_watch_exit(result)?;
                            watches.primary = Self::spawn_primary_watch(
                                primary.clone(),
                                sender.clone(),
                                watch_exited_tx.clone(),
                                Some(WATCH_RESTART_BACKOFF),
                            );
                        }
                        WatchExited::Secondary { index, result } => {
                            Self::accept_restartable_watch_exit(result)?;
                            // Relisting a secondary can enqueue objects that still exist, but
                            // cannot reveal a deletion missed while its watch was expired.
                            let live_names = Self::resync_all(&primary, &sender).await?;
                            metrics.resyncs.increment();
                            metrics.resync_enqueued.add(live_names.len() as u64);
                            finalizer_failures.retain(|name, _| live_names.contains(name));
                            let backoff = next_secondary_backoff(secondary_backoff[index], secondary_started[index].elapsed());
                            secondary_backoff[index] = backoff;
                            // Counts from when the delayed spawn runs, so `elapsed()` is zero until then.
                            secondary_started[index] = Instant::now() + backoff;
                            watches.secondaries[index] = Self::spawn_secondary_watch(
                                index,
                                secondary_templates[index].clone(),
                                backend.clone(),
                                primary.namespace.clone(),
                                sender.clone(),
                                watch_exited_tx.clone(),
                                Some(backoff),
                            );
                        }
                    }
                }
            }
        }
    }
}

fn is_lifecycle_owned(authority: Option<LifecycleAuthority>) -> bool {
    !matches!(authority, Some(LifecycleAuthority::Observed | LifecycleAuthority::Adopted))
}

/// List every resource matching `selector` and delete only lifecycle-owned children.
///
/// `Observed` and `Adopted` children may be referenced by a managed parent but their
/// lifecycle belongs outside the parent reconciler, so cascading teardown must not
/// delete them.
pub async fn delete_lifecycle_owned_matching<T: Resource>(
    resolver: &TypedResolver<T>,
    selector: &BTreeMap<String, String>,
) -> Result<(), ResourceError> {
    let listed = resolver.list_matching_labels(selector).await?;
    for object in listed.items {
        if !is_lifecycle_owned(object.metadata.lifecycle_authority()?) {
            continue;
        }
        match resolver.delete(&object.metadata.name).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    #[test]
    fn secondary_backoff_doubles_to_the_cap_and_resets_after_a_stable_run() {
        let mut delay = WATCH_RESTART_BACKOFF / 2;
        let mut seen = Vec::new();
        for _ in 0..12 {
            delay = next_secondary_backoff(delay, Duration::ZERO);
            seen.push(delay);
        }
        assert_eq!(seen[..3], [Duration::from_millis(100), Duration::from_millis(200), Duration::from_millis(400)]);
        assert_eq!(seen[11], SECONDARY_BACKOFF_CAP);
        assert_eq!(next_secondary_backoff(delay, SECONDARY_STABLE_RUN - Duration::from_millis(1)), SECONDARY_BACKOFF_CAP);
        assert_eq!(next_secondary_backoff(delay, SECONDARY_STABLE_RUN), WATCH_RESTART_BACKOFF);
    }

    // #3412: only outstanding work without progress can stall a controller.
    // Generate duplicate queue entries, backlog sizes, and ages on both sides
    // of the bound; check after every drain step and after the queue is idle.
    // A duplicate wake preserves the earliest enqueue instant, including time
    // in the channel before the controller moves names to its local queue.
    #[tokio::test(start_paused = true)]
    async fn queue_wait_includes_channel_delay_and_preserves_first_wake() {
        let (sender, mut receiver) = WorkQueueSender::channel();
        sender.send("one".into()).await.expect("wake");
        tokio::time::advance(Duration::from_secs(2)).await;
        sender.send("one".into()).await.expect("coalesced wake");
        let name = receiver.recv().await.expect("name");
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(sender.start(&name), Duration::from_secs(3));
        assert!(receiver.try_recv().is_err());
        sender.made_progress();
        sender.send(name.clone()).await.expect("next wake");
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(sender.start(&name), Duration::from_secs(1));
    }

    #[hegel::test]
    fn controller_queue_progress_distinguishes_idle_busy_and_stuck(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(30));
        let age = tc.draw(gs::integers::<i64>().min_value(0).max_value(120));
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let (sender, mut receiver) = WorkQueueSender::channel();
            let bound = Duration::from_secs(60);
            assert!(sender.stall_reason(Utc::now() + chrono::Duration::hours(2), bound).is_none());
            for index in 0..count {
                sender.send(index.to_string()).await.unwrap();
                sender.send(index.to_string()).await.unwrap();
            }
            let baseline = sender.progress.lock().unwrap().last_progress.unwrap();
            for age in [0, 59, 60, 61, age] {
                assert_eq!(sender.stall_reason(baseline + chrono::Duration::seconds(age), bound).is_some(), age >= 60);
            }
            for _ in 0..count {
                let name = receiver.try_recv().unwrap();
                sender.start(&name);
                let baseline = sender.progress.lock().unwrap().last_progress.unwrap();
                assert!(sender.stall_reason(baseline + chrono::Duration::seconds(59), bound).is_none());
                assert!(sender
                    .stall_reason(baseline + chrono::Duration::seconds(60), bound)
                    .unwrap()
                    .contains(&format!("processing {name}")));
                sender.made_progress();
                assert!(sender.stall_reason(Utc::now(), bound).is_none());
            }
            assert!(receiver.try_recv().is_err(), "duplicates are deduplicated");
            assert!(sender.stall_reason(Utc::now() + chrono::Duration::hours(2), bound).is_none());
        });
    }
}
