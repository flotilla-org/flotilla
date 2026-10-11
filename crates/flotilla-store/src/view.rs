//! Indexed list/watch state shared by continuous store consumers.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_protocol::NodeId;
use flotilla_resources::{
    ReadResourceList, ReadResourceObject, ReadWatchEvent, ReplicationClass, Resource, ResourceError, ResourceList, ResourceObject,
    ResourceProvenance, WatchStart, WatchStream,
};
use futures::{stream::BoxStream, FutureExt, StreamExt};
use tokio::{
    sync::{watch, RwLock},
    time::Instant,
};

use crate::{QueryIndex, ResourceBackend, ResourceKind, StoreExt, StoreQuery};

type Events<T> = BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>;
const VIEW_RESTART_BACKOFF: Duration = Duration::from_millis(100);

/// A snapshot and its race-free watch handoff. Replica collections have no
/// single cursor: subscribe to replicas before listing and resume local events
/// from the snapshot's authoritative position.
#[async_trait]
pub trait ViewSource<T: Resource>: Send + Sync {
    async fn list_and_watch(&self) -> Result<(ReadResourceList<T>, Events<T>), ResourceError>;
}

pub struct StoreViewSource<T: Resource> {
    backend: ResourceBackend,
    namespace: String,
    marker: std::marker::PhantomData<T>,
}
impl<T: Resource> StoreViewSource<T> {
    pub fn new(backend: ResourceBackend, namespace: &str) -> Self {
        Self { backend, namespace: namespace.into(), marker: std::marker::PhantomData }
    }
}
#[async_trait]
impl<T: Resource> ViewSource<T> for StoreViewSource<T> {
    async fn list_and_watch(&self) -> Result<(ReadResourceList<T>, Events<T>), ResourceError> {
        let store = self.backend.store();
        let replicas = if T::REPLICATION_CLASS == ReplicationClass::None {
            futures::stream::pending().boxed()
        } else if self.backend.serves_read_views() {
            store
                .watch_replica_sources_typed::<T>(&self.namespace)
                .await?
                .filter_map(|event| async move {
                    match event {
                        Ok(ReadWatchEvent::Added(item) | ReadWatchEvent::Modified(item) | ReadWatchEvent::Deleted(item))
                            if matches!(item.provenance, ResourceProvenance::Local) =>
                        {
                            None
                        }
                        Ok(ReadWatchEvent::DeletedByName { provenance: ResourceProvenance::Local, .. }) => None,
                        event => Some(event),
                    }
                })
                .boxed()
        } else {
            store.watch_replicas_typed::<T>(&self.namespace).await?
        };
        let query = StoreQuery { include_replicas: true, suppress_self_origin: false, ..StoreQuery::default() };
        let listed = store.query_raw(ResourceKind::of::<T>(), &self.namespace, &query).await?;
        let position = listed.position.clone();
        let start = match position.generation {
            Some(generation) => WatchStart::FromVersionInGeneration { generation, resource_version: position.resource_version },
            None => WatchStart::FromVersion(position.resource_version),
        };
        let local = store.watch_typed::<T>(&self.namespace, start).await?.map(|event| event.map(ReadWatchEvent::local));
        let items = crate::convert_read_list(listed.into_list())?;
        Ok((items, futures::stream::select(local, replicas).boxed()))
    }
}

type ViewObjects<T> = BTreeMap<String, Arc<ReadResourceObject<T>>>;
type DefinitionIdentity = (String, String);

/// Current objects keyed by namespace/name/origin, with declared equality
/// indexes. Updating the map and indexes is one operation under the caller's lock.
pub struct ViewState<T: Resource> {
    objects: ViewObjects<T>,
    sources: BTreeMap<DefinitionIdentity, BTreeSet<String>>,
    index: QueryIndex,
    definitions: Option<(ViewObjects<T>, QueryIndex)>,
    local_root: Option<NodeId>,
}
impl<T: Resource> ViewState<T> {
    pub fn new(local_root: Option<NodeId>) -> Self {
        Self { objects: BTreeMap::new(), sources: BTreeMap::new(), index: QueryIndex::default(), definitions: None, local_root }
    }
    fn key(namespace: &str, name: &str, provenance: &ResourceProvenance) -> String {
        let origin = match provenance {
            ResourceProvenance::Local => None,
            ResourceProvenance::Replica { origin_root, .. } => Some(origin_root),
        };
        serde_json::json!([namespace, name, origin]).to_string()
    }
    pub fn replace(&mut self, listed: ReadResourceList<T>) -> Result<(), ResourceError> {
        let mut next = Self::new(self.local_root.clone());
        for item in listed.items {
            next.apply_source(ReadWatchEvent::Added(item))?;
        }
        for identity in next.sources.keys().cloned().collect::<Vec<_>>() {
            next.refresh_definition(&identity)?;
        }
        *self = next;
        Ok(())
    }
    pub fn apply(&mut self, event: ReadWatchEvent<T>) -> Result<(), ResourceError> {
        let identity = match &event {
            ReadWatchEvent::Added(item) | ReadWatchEvent::Modified(item) | ReadWatchEvent::Deleted(item) => {
                (item.object.metadata.namespace.clone(), item.object.metadata.name.clone())
            }
            ReadWatchEvent::DeletedByName { tombstone, .. } => (tombstone.namespace.clone(), tombstone.name.clone()),
        };
        self.apply_source(event)?;
        self.refresh_definition(&identity)
    }
    fn refresh_definition(&mut self, identity: &DefinitionIdentity) -> Result<(), ResourceError> {
        if T::REPLICATION_CLASS != ReplicationClass::Definitions {
            return Ok(());
        }
        let Some(root) = self.local_root.as_ref() else {
            return Ok(());
        };
        // Group membership is maintained with the source index. Re-merging one
        // identity never clones or serializes unrelated definition bodies.
        let sources = self
            .sources
            .get(identity)
            .into_iter()
            .flatten()
            .filter_map(|key| self.objects.get(key))
            .map(|item| clone_read_object(item))
            .collect();
        let merged = crate::definition::merge_cached_definitions(sources, root)?;
        let (objects, index) = self.definitions.get_or_insert_with(|| (BTreeMap::new(), QueryIndex::default()));
        let key = Self::key(&identity.0, &identity.1, &ResourceProvenance::Local);
        if let Some(object) = merged.into_iter().next() {
            let value = serde_json::to_value(object.to_k8s_object()).map_err(|error| ResourceError::decode(error.to_string()))?;
            index.update(&key, Some(&value));
            objects.insert(key, Arc::new(ReadResourceObject { object, provenance: ResourceProvenance::Local }));
        } else {
            index.update(&key, None);
            objects.remove(&key);
        }
        Ok(())
    }
    fn remove_source(&mut self, namespace: &str, name: &str, provenance: &ResourceProvenance) {
        let key = Self::key(namespace, name, provenance);
        self.index.update(&key, None);
        self.objects.remove(&key);
        let identity = (namespace.to_owned(), name.to_owned());
        if let Some(keys) = self.sources.get_mut(&identity) {
            keys.remove(&key);
            if keys.is_empty() {
                self.sources.remove(&identity);
            }
        }
    }
    fn apply_source(&mut self, event: ReadWatchEvent<T>) -> Result<(), ResourceError> {
        match event {
            ReadWatchEvent::Added(item) | ReadWatchEvent::Modified(item) => {
                let key = Self::key(&item.object.metadata.namespace, &item.object.metadata.name, &item.provenance);
                let value = serde_json::to_value(item.object.to_k8s_object()).map_err(|error| ResourceError::decode(error.to_string()))?;
                self.index.update(&key, Some(&value));
                if T::REPLICATION_CLASS == ReplicationClass::Definitions {
                    self.sources
                        .entry((item.object.metadata.namespace.clone(), item.object.metadata.name.clone()))
                        .or_default()
                        .insert(key.clone());
                }
                self.objects.insert(key, Arc::new(item));
            }
            ReadWatchEvent::Deleted(item) => {
                self.remove_source(&item.object.metadata.namespace, &item.object.metadata.name, &item.provenance);
            }
            ReadWatchEvent::DeletedByName { tombstone, provenance } => {
                self.remove_source(&tombstone.namespace, &tombstone.name, &provenance);
            }
        }
        Ok(())
    }
    pub fn query(&self, query: &StoreQuery) -> Result<ReadResourceList<T>, ResourceError> {
        Ok(materialize(self.select(query)?))
    }
    fn select(&self, query: &StoreQuery) -> Result<Vec<Arc<ReadResourceObject<T>>>, ResourceError> {
        query.validate(ResourceKind::of::<T>())?;
        if query.message.is_some() {
            return Err(ResourceError::invalid("view queries support labels, declared fields and owners"));
        }
        if query.replica_shadowing.is_some() {
            return Err(ResourceError::invalid("view owns its replica shadowing scope"));
        }
        let (objects, index) = self.definitions.as_ref().map(|(objects, index)| (objects, index)).unwrap_or((&self.objects, &self.index));
        let items = index
            .names(query)
            .into_iter()
            .filter_map(|key| objects.get(&key))
            .filter(|item| {
                if query.replicas_only && matches!(item.provenance, ResourceProvenance::Local) {
                    return false;
                }
                if !query.include_replicas && !matches!(item.provenance, ResourceProvenance::Local) {
                    return false;
                }
                if query.suppress_self_origin {
                    if let ResourceProvenance::Replica { origin_root, .. } = &item.provenance {
                        if Some(origin_root) == self.local_root.as_ref() {
                            return !self.objects.contains_key(&Self::key(
                                &item.object.metadata.namespace,
                                &item.object.metadata.name,
                                &ResourceProvenance::Local,
                            ));
                        }
                    }
                }
                true
            })
            .cloned()
            .collect();
        Ok(items)
    }
}

// The fixed public list shape owns its bodies. Selection only clones Arc
// handles under the view lock; ordering and owned clones happen after release.
fn materialize<T: Resource>(mut items: Vec<Arc<ReadResourceObject<T>>>) -> ReadResourceList<T> {
    items.sort_by(|a, b| {
        a.object
            .metadata
            .namespace
            .cmp(&b.object.metadata.namespace)
            .then_with(|| a.object.metadata.name.cmp(&b.object.metadata.name))
            .then_with(|| match (&a.provenance, &b.provenance) {
                (ResourceProvenance::Local, ResourceProvenance::Local) => std::cmp::Ordering::Equal,
                (ResourceProvenance::Local, _) => std::cmp::Ordering::Less,
                (_, ResourceProvenance::Local) => std::cmp::Ordering::Greater,
                (ResourceProvenance::Replica { origin_root: a, .. }, ResourceProvenance::Replica { origin_root: b, .. }) => a.cmp(b),
            })
    });
    ReadResourceList { items: items.iter().map(|item| clone_read_object(item)).collect() }
}

/// Shared recovery policy. Callers install the complete snapshot before
/// delivering subsequent events; expired handoffs retry without exposing it.
pub async fn recover_view<T: Resource>(source: &dyn ViewSource<T>) -> Result<(ReadResourceList<T>, Events<T>), ResourceError> {
    let mut warnings = RecoveryWarnings::default();
    loop {
        match source.list_and_watch().await {
            Err(error @ ResourceError::WatchExpired { .. }) => {
                warnings.warn::<T>(&error);
                tokio::time::sleep(VIEW_RESTART_BACKOFF).await;
            }
            result => return result,
        }
    }
}

#[derive(Default)]
struct RecoveryWarnings {
    last: Option<Instant>,
}
impl RecoveryWarnings {
    fn warn<T: Resource>(&mut self, error: &ResourceError) {
        let now = Instant::now();
        if self.last.is_none_or(|last| now.duration_since(last) >= Duration::from_secs(30)) {
            tracing::warn!(%error, kind = T::API_PATHS.kind, "watch view resync retry");
            self.last = Some(now);
        }
    }
}

// Both normal consumption and a flush drain use this path. Any lost event or
// failed application invalidates the cache: retry a complete snapshot/watch
// handoff with backoff, never acknowledge a read using the incomplete state.
// Return true after replacement so a flush stops at that coherent snapshot.
async fn handle_view_event<T: Resource>(
    event: Option<Result<ReadWatchEvent<T>, ResourceError>>,
    source: &dyn ViewSource<T>,
    state: &RwLock<ViewState<T>>,
    events: &mut Events<T>,
    warnings: &mut RecoveryWarnings,
) -> bool {
    let error = match event {
        Some(Ok(event)) => match state.write().await.apply(event) {
            Ok(()) => return false,
            Err(error) => error,
        },
        Some(Err(error)) => error,
        None => ResourceError::other("watch stream ended"),
    };
    warnings.warn::<T>(&error);
    loop {
        tokio::time::sleep(VIEW_RESTART_BACKOFF).await;
        let error = match source.list_and_watch().await {
            Ok((listed, watch)) => match state.write().await.replace(listed) {
                Ok(()) => {
                    *events = watch;
                    return true;
                }
                Err(error) => error,
            },
            Err(error) => error,
        };
        warnings.warn::<T>(&error);
    }
}

/// A running informer. Clones share one task, watch and indexed state. Dropping
/// the final handle cancels the task, including a pending recovery.
pub struct WatchView<T: Resource> {
    state: Arc<RwLock<ViewState<T>>>,
    changes: watch::Receiver<u64>,
    task: Arc<ViewTask>,
    flush: tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<()>>,
}
impl<T: Resource> Clone for WatchView<T> {
    fn clone(&self) -> Self {
        Self { state: self.state.clone(), changes: self.changes.clone(), task: self.task.clone(), flush: self.flush.clone() }
    }
}
struct ViewTask(tokio::task::JoinHandle<()>);
impl Drop for ViewTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl<T: Resource> WatchView<T> {
    pub async fn start(backend: ResourceBackend, namespace: &str) -> Result<Self, ResourceError> {
        let root = backend.local_root()?;
        Self::start_at_root(backend, namespace, root).await
    }
    /// HTTP stores do not expose the serving root through Store::local_root;
    /// callers supply that identity to preserve self-origin replica shadowing.
    pub async fn start_at_root(backend: ResourceBackend, namespace: &str, root: NodeId) -> Result<Self, ResourceError> {
        Self::from_source(Arc::new(StoreViewSource::<T>::new(backend, namespace)), Some(root)).await
    }
    /// Initial setup retries expired handoffs but fails fast on other errors,
    /// so startup can report refusal or unavailable storage. Once running, the
    /// informer retries resync failures to keep existing consumers recoverable.
    pub async fn from_source(source: Arc<dyn ViewSource<T>>, root: Option<NodeId>) -> Result<Self, ResourceError> {
        let (listed, mut events) = recover_view(source.as_ref()).await?;
        let mut initial = ViewState::new(root);
        initial.replace(listed)?;
        let state = Arc::new(RwLock::new(initial));
        let (tx, changes) = watch::channel(0_u64);
        let worker_state = state.clone();
        let (flush, mut flush_rx) = tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<()>>(16);
        let task = tokio::spawn(async move {
            let mut warnings = RecoveryWarnings::default();
            loop {
                tokio::select! {
                    event = events.next() => {
                        handle_view_event(event, source.as_ref(), &worker_state, &mut events, &mut warnings).await;
                    }
                    request = flush_rx.recv() => {
                        let Some(request) = request else { break };
                        let mut changed = false;
                        while let Some(event) = events.next().now_or_never() {
                            let recovered = handle_view_event(event, source.as_ref(), &worker_state, &mut events, &mut warnings).await;
                            changed = true;
                            // A fresh snapshot is a coherent read boundary. Do
                            // not chase immediately ended replacement streams.
                            if recovered { break; }
                        }
                        // Coalesce drained events into one invalidation.
                        if changed { tx.send_modify(|revision| *revision = revision.wrapping_add(1)); }
                        request.send(()).ok();
                        continue;
                    }
                }
                tx.send_modify(|revision| *revision = revision.wrapping_add(1));
            }
        });
        Ok(Self { state, changes, task: Arc::new(ViewTask(task)), flush })
    }
    /// Drain queued events before reading, stopping at a fresh resync snapshot.
    /// This avoids scheduler-dependent
    /// read-your-writes behavior, at the cost of a worker round-trip; reads wait
    /// behind event handling and resync rather than returning an incomplete view.
    pub async fn flush(&self) -> Result<(), ResourceError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.flush.send(tx).await.map_err(|_| ResourceError::other("watch view stopped"))?;
        rx.await.map_err(|_| ResourceError::other("watch view stopped"))
    }
    /// Kind-wide invalidations, including changes irrelevant to a selector.
    /// Subscribers re-query; revisions are not a resource event or wire cursor.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.clone()
    }
    pub async fn query(&self, query: &StoreQuery) -> Result<ReadResourceList<T>, ResourceError> {
        self.flush().await?;
        // Retain the task for the duration of a snapshot read.
        let _task = &self.task;
        if self.changes.has_changed().is_err() {
            return Err(ResourceError::other("watch view stopped"));
        }
        let selected = self.state.read().await.select(query)?;
        Ok(materialize(selected))
    }
    pub async fn definitions(&self) -> Result<Vec<ResourceObject<T>>, ResourceError> {
        if T::REPLICATION_CLASS != ReplicationClass::Definitions {
            return Err(ResourceError::invalid("definition view requires a definitions kind"));
        }
        Ok(self.list().await?.items.into_iter().map(|item| item.object).collect())
    }
    pub async fn list(&self) -> Result<ReadResourceList<T>, ResourceError> {
        self.query(&StoreQuery { include_replicas: true, ..StoreQuery::default() }).await
    }
}

/// Local-only source seam, also useful for enforcing list/watch interleavings
/// without replacing the storage collaborator.
#[async_trait]
pub trait LocalViewSource<T: Resource>: Send + Sync {
    async fn list(&self) -> Result<ResourceList<T>, ResourceError>;
    async fn watch(&self, start: WatchStart) -> Result<WatchStream<T>, ResourceError>;
}
#[async_trait]
impl<T: Resource> LocalViewSource<T> for crate::TypedResolver<T> {
    async fn list(&self) -> Result<ResourceList<T>, ResourceError> {
        self.list().await
    }
    async fn watch(&self, start: WatchStart) -> Result<WatchStream<T>, ResourceError> {
        self.watch(start).await
    }
}
#[async_trait]
impl<T: Resource> ViewSource<T> for crate::ReplicaReadResolver<T> {
    async fn list_and_watch(&self) -> Result<(ReadResourceList<T>, Events<T>), ResourceError> {
        let watch = self.watch().await?;
        let listed = self.list().await?;
        Ok((listed, watch))
    }
}

/// Shared local snapshot/watch handoff for consumers that maintain their own
/// projection state. No duplicate index or event serialization is needed.
pub async fn recover_local_view<T: Resource>(source: &dyn LocalViewSource<T>) -> Result<(ResourceList<T>, WatchStream<T>), ResourceError> {
    let mut warnings = RecoveryWarnings::default();
    loop {
        let result = async {
            let listed = source.list().await?;
            let events = source.watch(WatchStart::resuming_from(&listed)).await?;
            Ok((listed, events))
        }
        .await;
        match result {
            Err(error @ ResourceError::WatchExpired { .. }) => {
                warnings.warn::<T>(&error);
                tokio::time::sleep(VIEW_RESTART_BACKOFF).await;
            }
            result => return result,
        }
    }
}
fn clone_read_object<T: Resource>(item: &ReadResourceObject<T>) -> ReadResourceObject<T> {
    ReadResourceObject {
        object: ResourceObject {
            metadata: item.object.metadata.clone(),
            spec: item.object.spec.clone(),
            status: item.object.status.clone(),
        },
        provenance: item.provenance.clone(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_resources::{Convoy, ConvoySpec, K8sResourceObject, ResourceObject};

    // This resource instruments serialization through an injected spec counter;
    // it observes definition merge work without a clock or production test hook.
    struct CountedDefinition;
    #[derive(Debug, Clone, serde::Deserialize)]
    struct CountedSpec {
        value: String,
        #[serde(skip)]
        fail: bool,
        #[serde(skip)]
        serializations: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl serde::Serialize for CountedSpec {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            if self.fail {
                return Err(serde::ser::Error::custom("injected encoding failure"));
            }
            self.serializations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            BTreeMap::from([("value", &self.value)]).serialize(serializer)
        }
    }
    impl Resource for CountedDefinition {
        type Spec = CountedSpec;
        type Status = <flotilla_resources::Project as Resource>::Status;
        type StatusPatch = <flotilla_resources::Project as Resource>::StatusPatch;
        const API_PATHS: flotilla_resources::ApiPaths = flotilla_resources::Project::API_PATHS;
        const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Definitions;
    }
    struct ApplyFailureSource(std::sync::atomic::AtomicUsize);
    #[async_trait]
    impl ViewSource<CountedDefinition> for ApplyFailureSource {
        async fn list_and_watch(&self) -> Result<(ReadResourceList<CountedDefinition>, Events<CountedDefinition>), ResourceError> {
            let attempt = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let value = serde_json::json!({"apiVersion":"flotilla.work/v1", "kind":"Project",
                "metadata":{"namespace":"test", "name":"project", "resourceVersion":"1",
                    "creationTimestamp":"2026-01-01T00:00:00Z"}, "spec":{"value":"resynced"}});
            let mut object = ResourceObject::<CountedDefinition>::from_k8s_object(serde_json::from_value(value).unwrap()).unwrap();
            object.spec.fail = attempt == 0;
            let item = ReadResourceObject { object, provenance: ResourceProvenance::Local };
            if attempt == 0 {
                let events =
                    futures::stream::once(async move { Ok(ReadWatchEvent::Added(item)) }).chain(futures::stream::pending()).boxed();
                Ok((ReadResourceList { items: vec![] }, events))
            } else {
                Ok((ReadResourceList { items: vec![item] }, futures::stream::pending().boxed()))
            }
        }
    }
    // A failed event application invalidates the cached state just like a lost
    // transport event. Reads wait for a successful replacement, not stale data.
    #[tokio::test]
    async fn event_application_failure_resyncs_before_read() {
        let source = Arc::new(ApplyFailureSource(std::sync::atomic::AtomicUsize::new(0)));
        let view = WatchView::from_source(source.clone(), Some(NodeId::new("root"))).await.unwrap();
        let items = tokio::time::timeout(Duration::from_secs(5), view.list()).await.unwrap().unwrap().items;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].object.spec.value, "resynced");
        assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    // Transport boundary source that repeatedly supplies a valid snapshot but
    // immediately ends its watch. Flush must return after one resync, rather
    // than draining and recovering each replacement stream forever.
    struct EndedSource(ApplyFailureSource);
    #[async_trait]
    impl ViewSource<CountedDefinition> for EndedSource {
        async fn list_and_watch(&self) -> Result<(ReadResourceList<CountedDefinition>, Events<CountedDefinition>), ResourceError> {
            let (listed, _) = self.0.list_and_watch().await?;
            Ok((listed, futures::stream::empty().boxed()))
        }
    }
    #[tokio::test]
    async fn flush_returns_snapshot_when_replacement_watch_ends() {
        let source = Arc::new(EndedSource(ApplyFailureSource(std::sync::atomic::AtomicUsize::new(1))));
        let view = WatchView::from_source(source, Some(NodeId::new("root"))).await.unwrap();
        let listed = tokio::time::timeout(Duration::from_secs(5), view.list()).await.expect("flush must terminate").unwrap();
        assert_eq!(listed.items.len(), 1);
        assert_eq!(listed.items[0].object.spec.value, "resynced");
    }

    // Generate different collection sizes and churned identities. An event for
    // one definition must never serialize unrelated definitions, and old labels
    // must disappear from the merged equality index after every update/delete.
    #[hegel::test]
    fn definition_churn_only_recomputes_affected_identity(tc: hegel::TestCase) {
        use hegel::generators as gs;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let size = tc.draw(gs::integers::<usize>().min_value(2).max_value(24));
        let selected = tc.draw(gs::integers::<usize>().min_value(0).max_value(size - 1));
        let counters: Vec<_> = (0..size).map(|_| Arc::new(AtomicUsize::new(0))).collect();
        let item = |index: usize, team: &str| {
            let value = serde_json::json!({"apiVersion":"flotilla.work/v1", "kind":"Project",
                "metadata":{"namespace":"test", "name":format!("project-{index}"), "resourceVersion":"1",
                    "creationTimestamp":"2026-01-01T00:00:00Z", "labels":{"team":team}},
                "spec":{"value":team}});
            let mut object = ResourceObject::<CountedDefinition>::from_k8s_object(serde_json::from_value(value).unwrap()).unwrap();
            object.spec.serializations = counters[index].clone();
            ReadResourceObject { object, provenance: ResourceProvenance::Local }
        };
        let mut view = ViewState::new(Some(NodeId::new("root")));
        view.replace(ReadResourceList { items: (0..size).map(|index| item(index, "old")).collect() }).unwrap();
        for round in 0..8 {
            for counter in &counters {
                counter.store(0, Ordering::SeqCst);
            }
            let team = format!("team-{round}");
            view.apply(ReadWatchEvent::Modified(item(selected, &team))).unwrap();
            for (index, counter) in counters.iter().enumerate() {
                if index != selected {
                    assert_eq!(counter.load(Ordering::SeqCst), 0, "unrelated definition {index}");
                }
            }
            let query = StoreQuery { labels: BTreeMap::from([("team".into(), team)]), ..StoreQuery::default() };
            assert_eq!(view.query(&query).unwrap().items.len(), 1);
            let old = StoreQuery { labels: BTreeMap::from([("team".into(), "old".into())]), ..StoreQuery::default() };
            assert_eq!(view.query(&old).unwrap().items.len(), size - 1);
            view.apply(ReadWatchEvent::Deleted(item(selected, "deleted"))).unwrap();
            assert!(view.query(&query).unwrap().items.is_empty());
        }
    }

    // Generate duplicate identities, local and two replica origins, missing and
    // changing labels/owners, optional phases, and both delete event shapes.
    // Every operation must leave indexed results equal to a direct predicate
    // over the independently maintained identity map.
    #[hegel::test]
    fn indexed_view_tracks_every_mutation(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let mut view = ViewState::<Convoy>::new(Some(NodeId::new("root-1")));
        let mut expected = BTreeMap::<(usize, usize), ReadResourceObject<Convoy>>::new();
        let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(80));
        for _ in 0..steps {
            let name = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
            let origin = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            let operation = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
            let team = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            let phase = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            let owner = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            let provenance = if origin == 0 {
                ResourceProvenance::Local
            } else {
                ResourceProvenance::Replica { origin_root: NodeId::new(format!("root-{origin}")), last_synced_at: chrono::Utc::now() }
            };
            let mut value = serde_json::json!({
                "apiVersion": "flotilla.work/v1", "kind": "Convoy",
                "metadata": {"name": format!("object-{name}"), "namespace": "test", "resourceVersion": "1",
                    "creationTimestamp": "2026-01-01T00:00:00Z", "labels": {"team": format!("team-{team}")},
                    "ownerReferences": [{"apiVersion":"flotilla.work/v1", "kind":"Project", "name":format!("owner-{owner}"), "controller":false}]},
                "spec": ConvoySpec::builder().workflow_ref("test".into()).build(),
                "status": {"phase": if phase == 0 { "Active" } else { "Pending" }},
            });
            if team == 2 {
                value["metadata"]["labels"] = serde_json::json!({});
            }
            if owner == 2 {
                value["metadata"]["ownerReferences"] = serde_json::json!([]);
            }
            if phase == 2 {
                value.as_object_mut().unwrap().remove("status");
            }
            let object = ResourceObject::from_k8s_object(serde_json::from_value::<K8sResourceObject<Convoy>>(value).unwrap()).unwrap();
            let item = ReadResourceObject { object, provenance };
            match operation {
                0 | 1 => {
                    view.apply(if operation == 0 { ReadWatchEvent::Added(item.clone()) } else { ReadWatchEvent::Modified(item.clone()) })
                        .unwrap();
                    expected.insert((name, origin), item);
                }
                2 => {
                    view.apply(ReadWatchEvent::Deleted(item)).unwrap();
                    expected.remove(&(name, origin));
                }
                _ => {
                    view.apply(ReadWatchEvent::DeletedByName {
                        tombstone: flotilla_resources::ResourceTombstone {
                            namespace: "test".into(),
                            name: format!("object-{name}"),
                            resource_version: "1".into(),
                            annotations: BTreeMap::new(),
                        },
                        provenance: item.provenance,
                    })
                    .unwrap();
                    expected.remove(&(name, origin));
                }
            }
            for (selected_team, variant) in (0..3).flat_map(|team| (0..5).map(move |variant| (team, variant))) {
                let phase =
                    if selected_team == 0 { flotilla_resources::ConvoyPhase::Active } else { flotilla_resources::ConvoyPhase::Pending };
                let query = StoreQuery {
                    labels: if variant == 1 || variant == 4 {
                        BTreeMap::from([("team".into(), format!("team-{selected_team}"))])
                    } else {
                        BTreeMap::new()
                    },
                    fields: if variant == 2 || variant == 4 {
                        BTreeMap::from([("status.phase".into(), if selected_team == 0 { "Active" } else { "Pending" }.into())])
                    } else {
                        BTreeMap::new()
                    },
                    owner: (variant == 3 || variant == 4).then(|| crate::OwnerQuery {
                        api_version: "flotilla.work/v1".into(),
                        kind: "Project".into(),
                        name: format!("owner-{selected_team}"),
                    }),
                    include_replicas: true,
                    ..StoreQuery::default()
                };
                let mut want: Vec<_> = expected
                    .iter()
                    .filter(|((name, origin), item)| {
                        !(*origin == 1 && expected.contains_key(&(*name, 0)))
                            && ((variant != 1 && variant != 4)
                                || item.object.metadata.labels.get("team") == Some(&format!("team-{selected_team}")))
                            && ((variant != 2 && variant != 4) || item.object.status.as_ref().is_some_and(|status| status.phase == phase))
                            && ((variant != 3 && variant != 4)
                                || item.object.metadata.owner_references.iter().any(|owner| {
                                    owner.api_version == "flotilla.work/v1"
                                        && owner.kind == "Project"
                                        && owner.name == format!("owner-{selected_team}")
                                }))
                    })
                    .map(|((name, origin), _)| (*name, *origin))
                    .collect();
                want.sort();
                let mut got: Vec<_> = view
                    .query(&query)
                    .unwrap()
                    .items
                    .into_iter()
                    .map(|item| {
                        let name = item.object.metadata.name.strip_prefix("object-").unwrap().parse::<usize>().unwrap();
                        let origin = match item.provenance {
                            ResourceProvenance::Local => 0,
                            ResourceProvenance::Replica { origin_root, .. } => {
                                origin_root.as_str().strip_prefix("root-").unwrap().parse().unwrap()
                            }
                        };
                        (name, origin)
                    })
                    .collect();
                got.sort();
                assert_eq!(got, want);
            }
        }
        let bad = StoreQuery { fields: BTreeMap::from([("undeclared".into(), "value".into())]), ..StoreQuery::default() };
        assert!(view.query(&bad).is_err());
    }
}
