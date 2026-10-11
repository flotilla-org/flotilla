//! Indexed list/watch state shared by continuous store consumers.
use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::{stream::BoxStream, FutureExt, StreamExt};
use tokio::sync::{watch, RwLock};

use crate::{QueryIndex, ResourceBackend, ResourceKind, StoreExt, StoreQuery};
use flotilla_resources::{ReadResourceList, ReadResourceObject, ReadWatchEvent, Resource, ResourceError, ResourceProvenance, WatchStart};

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
        let replicas = if T::REPLICATION_CLASS == flotilla_resources::ReplicationClass::None {
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

/// Current objects keyed by namespace/name/origin, with declared equality
/// indexes. Updating the map and indexes is one operation under the caller's lock.
pub struct ViewState<T: Resource> {
    objects: BTreeMap<String, ReadResourceObject<T>>,
    index: QueryIndex,
    definitions: Option<(BTreeMap<String, ReadResourceObject<T>>, QueryIndex)>,
    local_root: Option<flotilla_protocol::NodeId>,
}
impl<T: Resource> ViewState<T> {
    pub fn new(local_root: Option<flotilla_protocol::NodeId>) -> Self {
        Self { objects: BTreeMap::new(), index: QueryIndex::default(), definitions: None, local_root }
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
        next.refresh_definitions()?;
        *self = next;
        Ok(())
    }
    pub fn apply(&mut self, event: ReadWatchEvent<T>) -> Result<(), ResourceError> {
        self.apply_source(event)?;
        self.refresh_definitions()
    }
    fn refresh_definitions(&mut self) -> Result<(), ResourceError> {
        if T::REPLICATION_CLASS != flotilla_resources::ReplicationClass::Definitions {
            return Ok(());
        }
        let Some(root) = self.local_root.as_ref() else {
            return Ok(());
        };
        let merged = crate::definition::merge_cached_definitions(self.objects.values().map(clone_read_object).collect(), root)?;
        let mut objects = BTreeMap::new();
        let mut index = QueryIndex::default();
        for object in merged {
            let key = Self::key(&object.metadata.namespace, &object.metadata.name, &ResourceProvenance::Local);
            let value = serde_json::to_value(object.to_k8s_object()).map_err(|error| ResourceError::decode(error.to_string()))?;
            index.update(&key, Some(&value));
            objects.insert(key, ReadResourceObject { object, provenance: ResourceProvenance::Local });
        }
        self.definitions = Some((objects, index));
        Ok(())
    }
    fn apply_source(&mut self, event: ReadWatchEvent<T>) -> Result<(), ResourceError> {
        match event {
            ReadWatchEvent::Added(item) | ReadWatchEvent::Modified(item) => {
                let key = Self::key(&item.object.metadata.namespace, &item.object.metadata.name, &item.provenance);
                let value = serde_json::to_value(item.object.to_k8s_object()).map_err(|error| ResourceError::decode(error.to_string()))?;
                self.index.update(&key, Some(&value));
                self.objects.insert(key, item);
            }
            ReadWatchEvent::Deleted(item) => {
                let key = Self::key(&item.object.metadata.namespace, &item.object.metadata.name, &item.provenance);
                self.index.update(&key, None);
                self.objects.remove(&key);
            }
            ReadWatchEvent::DeletedByName { tombstone, provenance } => {
                let key = Self::key(&tombstone.namespace, &tombstone.name, &provenance);
                self.index.update(&key, None);
                self.objects.remove(&key);
            }
        }
        Ok(())
    }
    pub fn query(&self, query: &StoreQuery) -> Result<ReadResourceList<T>, ResourceError> {
        query.validate(ResourceKind::of::<T>())?;
        if query.message.is_some() {
            return Err(ResourceError::invalid("view queries support labels, declared fields and owners"));
        }
        if query.replica_shadowing.is_some() {
            return Err(ResourceError::invalid("view owns its replica shadowing scope"));
        }
        let (objects, index) = self.definitions.as_ref().map(|(objects, index)| (objects, index)).unwrap_or((&self.objects, &self.index));
        let mut items: Vec<ReadResourceObject<T>> = index
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
            .map(|item| ReadResourceObject {
                object: flotilla_resources::ResourceObject {
                    metadata: item.object.metadata.clone(),
                    spec: item.object.spec.clone(),
                    status: item.object.status.clone(),
                },
                provenance: item.provenance.clone(),
            })
            .collect();
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
        Ok(ReadResourceList { items })
    }
}

/// Shared recovery policy. Callers install the complete snapshot before
/// delivering subsequent events; expired handoffs retry without exposing it.
pub async fn recover_view<T: Resource>(source: &dyn ViewSource<T>) -> Result<(ReadResourceList<T>, Events<T>), ResourceError> {
    loop {
        match source.list_and_watch().await {
            Err(ResourceError::WatchExpired { .. }) => tokio::time::sleep(VIEW_RESTART_BACKOFF).await,
            result => return result,
        }
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
    pub async fn start_at_root(backend: ResourceBackend, namespace: &str, root: flotilla_protocol::NodeId) -> Result<Self, ResourceError> {
        Self::from_source(Arc::new(StoreViewSource::<T>::new(backend, namespace)), Some(root)).await
    }
    pub async fn from_source(source: Arc<dyn ViewSource<T>>, root: Option<flotilla_protocol::NodeId>) -> Result<Self, ResourceError> {
        let (listed, mut events) = recover_view(source.as_ref()).await?;
        let mut initial = ViewState::new(root);
        initial.replace(listed)?;
        let state = Arc::new(RwLock::new(initial));
        let (tx, changes) = watch::channel(0_u64);
        let worker_state = state.clone();
        let (flush, mut flush_rx) = tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<()>>(16);
        let task = tokio::spawn(async move {
            loop {
                let mut acknowledgement = None;
                let event = tokio::select! {
                    event = events.next() => event,
                    request = flush_rx.recv() => {
                        let Some(request) = request else { break };
                        acknowledgement = Some(request);
                        match events.next().now_or_never() {
                            Some(event) => event,
                            None => { acknowledgement.take().expect("request").send(()).ok(); continue; }
                        }
                    }
                };
                if event.is_none() {
                    tokio::time::sleep(VIEW_RESTART_BACKOFF).await;
                }
                let result = match event {
                    Some(Ok(event)) => worker_state.write().await.apply(event),
                    Some(Err(ResourceError::WatchExpired { .. })) | None => match recover_view(source.as_ref()).await {
                        Ok((listed, watch)) => {
                            events = watch;
                            worker_state.write().await.replace(listed)
                        }
                        Err(error) => Err(error),
                    },
                    Some(Err(error)) => Err(error),
                };
                if let Err(error) = result {
                    tracing::error!(%error, kind = T::API_PATHS.kind, "watch view stopped");
                    break;
                }
                tx.send_modify(|revision| *revision = revision.wrapping_add(1));
                if let Some(acknowledgement) = acknowledgement {
                    while let Some(Some(event)) = events.next().now_or_never() {
                        match event {
                            Ok(event) => {
                                if let Err(error) = worker_state.write().await.apply(event) {
                                    tracing::error!(%error, "view drain failed");
                                    return;
                                }
                            }
                            Err(ResourceError::WatchExpired { .. }) => match recover_view(source.as_ref()).await {
                                Ok((listed, watch)) => {
                                    events = watch;
                                    if worker_state.write().await.replace(listed).is_err() {
                                        return;
                                    }
                                }
                                Err(_) => return,
                            },
                            Err(_) => return,
                        }
                        tx.send_modify(|revision| *revision = revision.wrapping_add(1));
                    }
                    acknowledgement.send(()).ok();
                }
            }
        });
        Ok(Self { state, changes, task: Arc::new(ViewTask(task)), flush })
    }
    pub async fn flush(&self) -> Result<(), ResourceError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.flush.send(tx).await.map_err(|_| ResourceError::other("watch view stopped"))?;
        rx.await.map_err(|_| ResourceError::other("watch view stopped"))
    }
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
        self.state.read().await.query(query)
    }
    pub async fn definitions(&self) -> Result<Vec<flotilla_resources::ResourceObject<T>>, ResourceError> {
        if T::REPLICATION_CLASS != flotilla_resources::ReplicationClass::Definitions {
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
    async fn list(&self) -> Result<flotilla_resources::ResourceList<T>, ResourceError>;
    async fn watch(&self, start: WatchStart) -> Result<flotilla_resources::WatchStream<T>, ResourceError>;
}
#[async_trait]
impl<T: Resource> LocalViewSource<T> for crate::TypedResolver<T> {
    async fn list(&self) -> Result<flotilla_resources::ResourceList<T>, ResourceError> {
        self.list().await
    }
    async fn watch(&self, start: WatchStart) -> Result<flotilla_resources::WatchStream<T>, ResourceError> {
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

/// Install a snapshot and maintain its indexes while a projection consumes
/// events. Projection-specific gap replacement stays with the consumer; the
/// snapshot/watch handoff and retry policy are shared with the informer.
pub async fn recover_indexed_view<T: Resource>(source: &dyn ViewSource<T>) -> Result<(ReadResourceList<T>, Events<T>), ResourceError> {
    let (listed, events) = recover_view(source).await?;
    let mut state = ViewState::new(None);
    state.replace(ReadResourceList { items: listed.items.iter().map(clone_read_object).collect() })?;
    let events = events
        .map(move |event| {
            let event = event?;
            state.apply(clone_read_event(&event))?;
            Ok(event)
        })
        .boxed();
    Ok((listed, events))
}

pub async fn recover_local_view<T: Resource>(
    source: &dyn LocalViewSource<T>,
) -> Result<(flotilla_resources::ResourceList<T>, flotilla_resources::WatchStream<T>), ResourceError> {
    loop {
        let result = async {
            let listed = source.list().await?;
            let events = source.watch(WatchStart::resuming_from(&listed)).await?;
            let generation = events.generation().map(str::to_owned);
            let mut state = ViewState::new(None);
            state.replace(ReadResourceList {
                items: listed
                    .items
                    .iter()
                    .map(|object| ReadResourceObject {
                        object: flotilla_resources::ResourceObject {
                            metadata: object.metadata.clone(),
                            spec: object.spec.clone(),
                            status: object.status.clone(),
                        },
                        provenance: ResourceProvenance::Local,
                    })
                    .collect(),
            })?;
            let events = events
                .map(move |event| {
                    let event = event?;
                    state.apply(ReadWatchEvent::local(clone_local_event(&event)))?;
                    Ok(event)
                })
                .boxed();
            Ok((listed, flotilla_resources::WatchStream::new(generation, events)))
        }
        .await;
        match result {
            Err(ResourceError::WatchExpired { .. }) => tokio::time::sleep(VIEW_RESTART_BACKOFF).await,
            result => return result,
        }
    }
}
fn clone_read_object<T: Resource>(item: &ReadResourceObject<T>) -> ReadResourceObject<T> {
    ReadResourceObject {
        object: flotilla_resources::ResourceObject {
            metadata: item.object.metadata.clone(),
            spec: item.object.spec.clone(),
            status: item.object.status.clone(),
        },
        provenance: item.provenance.clone(),
    }
}
fn clone_read_event<T: Resource>(event: &ReadWatchEvent<T>) -> ReadWatchEvent<T> {
    match event {
        ReadWatchEvent::Added(item) => ReadWatchEvent::Added(clone_read_object(item)),
        ReadWatchEvent::Modified(item) => ReadWatchEvent::Modified(clone_read_object(item)),
        ReadWatchEvent::Deleted(item) => ReadWatchEvent::Deleted(clone_read_object(item)),
        ReadWatchEvent::DeletedByName { tombstone, provenance } => {
            ReadWatchEvent::DeletedByName { tombstone: tombstone.clone(), provenance: provenance.clone() }
        }
    }
}
fn clone_local_event<T: Resource>(event: &flotilla_resources::WatchEvent<T>) -> flotilla_resources::WatchEvent<T> {
    use flotilla_resources::{ResourceObject, WatchEvent};
    let clone = |object: &ResourceObject<T>| ResourceObject {
        metadata: object.metadata.clone(),
        spec: object.spec.clone(),
        status: object.status.clone(),
    };
    match event {
        WatchEvent::Added(item) => WatchEvent::Added(clone(item)),
        WatchEvent::Modified(item) => WatchEvent::Modified(clone(item)),
        WatchEvent::Deleted(item) => WatchEvent::Deleted(clone(item)),
        WatchEvent::DeletedByName(tombstone) => WatchEvent::DeletedByName(tombstone.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_resources::{Convoy, ConvoySpec, K8sResourceObject, ResourceObject};

    // Generate duplicate identities, local and two replica origins, missing and
    // changing labels/owners, optional phases, and both delete event shapes.
    // Every operation must leave indexed results equal to a direct predicate
    // over the independently maintained identity map.
    #[hegel::test]
    fn indexed_view_tracks_every_mutation(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let mut view = ViewState::<Convoy>::new(Some(flotilla_protocol::NodeId::new("root-1")));
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
                ResourceProvenance::Replica {
                    origin_root: flotilla_protocol::NodeId::new(format!("root-{origin}")),
                    last_synced_at: chrono::Utc::now(),
                }
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
