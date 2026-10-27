use crate::store::transcode_request;
use crate::StoreExt;
use flotilla_resources::lifecycle_reads::LifecycleReads;
use std::{
    collections::{BTreeMap, HashSet},
    marker::PhantomData,
};

use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;
use flotilla_resources::{ReadResourceList, ReadResourceObject, ReadWatchEvent, ReplicaCursor, ResourceProvenance};
use flotilla_resources::{ResourceList, WatchStart, WatchStream};
use futures::{stream, StreamExt};

use crate::{
    definition::DefinitionResolver,
    error::ResourceError,
    field_ownership::merge_owned_spec,
    replica::StoredReplicaEventKind,
    resource::{InputMeta, Resource, ResourceObject},
    retention::ResourceStoreDiagnostics,
    FieldOwnedResource, FieldOwnershipViolation, OwnershipEnforcement, WriterIdentity,
};

macro_rules! dispatch_backend {
    ($self:expr, $method:ident $(, $args:expr)*) => {
        $self.backend.$method::<T>(&$self.namespace $(, $args)*).await
    };
}

/// The engine ports installed behind `[primitives]`, by the resource kinds they serve: shared by
/// every handle cloned or derived from one backend (#3824 layers 2b-ii and 3).
type EngineSlot = std::sync::Arc<
    std::sync::RwLock<std::collections::BTreeMap<&'static str, std::sync::Arc<dyn flotilla_resources::engine_records::EngineRecords>>>,
>;

/// Cloneable handle chosen by the composition root.
#[derive(Debug, Clone)]
pub struct ResourceBackend {
    store: std::sync::Arc<dyn crate::Store>,
    engines: EngineSlot,
}
impl ResourceBackend {
    pub fn new(store: impl Into<Self>) -> Self {
        store.into()
    }
    pub fn from_arc(store: std::sync::Arc<dyn crate::Store>) -> Self {
        Self { store, engines: EngineSlot::default() }
    }
    /// Serves the kinds `records` names from it, the engine's, behind `[primitives]`: every handle
    /// cloned or derived from this backend reads and writes them there. A daemon runtime restarted
    /// over the same backend installs its reopened ports in place of the last.
    pub fn install_engine_records(&self, records: std::sync::Arc<dyn flotilla_resources::engine_records::EngineRecords>) {
        let mut engines = self.engines.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        for kind in records.kinds() {
            engines.insert(kind, std::sync::Arc::clone(&records));
        }
    }
    /// The engine port serving `kind`, when one is installed.
    pub(crate) fn engine_records(&self, kind: &str) -> Option<std::sync::Arc<dyn flotilla_resources::engine_records::EngineRecords>> {
        self.engines.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(kind).cloned()
    }
    pub fn store(&self) -> &dyn crate::Store {
        &*self.store
    }
    pub fn with_read_observer(self, observer: std::sync::Arc<dyn crate::ReadObserver>) -> Self {
        Self { store: self.store.with_read_observer(observer), engines: self.engines.clone() }
    }
    pub fn read_observer(&self) -> Option<&dyn crate::ReadObserver> {
        self.store.read_observer()
    }
    pub(crate) async fn hierarchy_admission(&self, kind: &str) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        if !crate::project_hierarchy::is_hierarchy_kind(kind) {
            return None;
        }
        Some(self.store.hierarchy_admission()?.lock_owned().await)
    }
    pub fn with_local_root(self, root: NodeId) -> Self {
        Self { store: self.store.with_local_root(root), engines: self.engines.clone() }
    }
    pub fn local_root(&self) -> Result<NodeId, ResourceError> {
        self.store.local_root()
    }
    pub fn serves_read_views(&self) -> bool {
        self.store.serves_read_views()
    }
    pub fn using<T: Resource>(&self, namespace: &str) -> TypedResolver<T> {
        TypedResolver { backend: self.clone(), namespace: namespace.to_string(), lifecycle_status_reads: None, _marker: PhantomData }
    }

    /// Namespaces containing locally-authored objects of this kind.
    pub async fn local_namespaces<T: Resource>(&self) -> Result<Vec<String>, ResourceError> {
        self.local_namespaces_typed::<T>().await
    }

    /// Namespaces containing a local or replicated source of this kind.
    pub async fn stored_namespaces<T: Resource>(&self) -> Result<Vec<String>, ResourceError> {
        self.stored_namespaces_typed::<T>().await
    }

    pub fn definitions<T: Resource>(&self, namespace: &str) -> DefinitionResolver<T> {
        DefinitionResolver::new(self.clone(), namespace.to_string())
    }

    /// A read-only union of locally-authored objects and durable replicas.
    ///
    /// This deliberately returns a different resolver type with no mutation
    /// methods, so controllers cannot accidentally reconcile replica rows.
    pub fn including_replicas<T: Resource>(&self, namespace: &str) -> ReplicaReadResolver<T> {
        ReplicaReadResolver { backend: self.clone(), namespace: namespace.to_string(), suppress_self_origin: true, _marker: PhantomData }
    }

    pub fn replica_writer<T: Resource>(&self, origin_root: NodeId, namespace: &str) -> ReplicaWriter<T> {
        ReplicaWriter { backend: self.clone(), origin_root, namespace: namespace.to_string(), _marker: PhantomData }
    }

    pub async fn diagnostics(&self) -> Result<Option<ResourceStoreDiagnostics>, ResourceError> {
        self.store.diagnostics().await
    }
    pub(crate) async fn delete_decode_quarantine<T: Resource>(&self, namespace: &str, name: &str) -> Result<bool, ResourceError> {
        self.store.delete_decode_quarantine_typed::<T>(namespace, name).await
    }
    async fn record_field_ownership_violation(&self, violation: FieldOwnershipViolation) -> Result<(), ResourceError> {
        self.store.record_field_ownership_violation(violation).await
    }
}

#[derive(Debug)]
pub struct ReplicaReadResolver<T: Resource> {
    backend: ResourceBackend,
    namespace: String,
    suppress_self_origin: bool,
    _marker: PhantomData<T>,
}

impl<T: Resource> std::clone::Clone for ReplicaReadResolver<T> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            namespace: self.namespace.clone(),
            suppress_self_origin: self.suppress_self_origin,
            _marker: PhantomData,
        }
    }
}

impl<T: Resource> ReplicaReadResolver<T> {
    /// Fault-injection seam for convergence tests. Production callers must
    /// leave self-origin suppression enabled.
    #[doc(hidden)]
    pub fn with_self_origin_suppression(mut self, suppress: bool) -> Self {
        self.suppress_self_origin = suppress;
        self
    }

    pub async fn get(&self, name: &str) -> Result<ReadResourceObject<T>, ResourceError> {
        if !T::RANKED_REPLICA_READS {
            return self.get_local_first(name).await;
        }
        // Demanded placeholders must not hide observed replicas. Read each copy
        // once and share selection with leaf evaluation and observer ownership.
        flotilla_resources::pick_replica_read(self.get_all(name).await?.items, |item| {
            (&item.object, matches!(item.provenance, ResourceProvenance::Local))
        })
        .ok_or_else(|| ResourceError::not_found(name))
    }

    async fn get_local_first(&self, name: &str) -> Result<ReadResourceObject<T>, ResourceError> {
        if let Some(reads) = self.engine_copies(name).await {
            // This home's copy first, then the lexically first home's, as the port resolves.
            let mut reads = reads?;
            let local = reads.iter().position(|read| matches!(read.provenance, ResourceProvenance::Local));
            return match local {
                Some(index) => Ok(reads.swap_remove(index)),
                None => reads.into_iter().next().ok_or_else(|| ResourceError::not_found(name)),
            };
        }
        if T::REPLICATION_CLASS == crate::ReplicationClass::None {
            return self
                .backend
                .using::<T>(&self.namespace)
                .get(name)
                .await
                .map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local });
        }
        ensure_replication_enabled::<T>()?;
        if self.backend.serves_read_views() {
            let backend = &self.backend;
            return backend.get_including_replicas_typed::<T>(&self.namespace, name).await;
        }
        if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            return self
                .backend
                .definitions::<T>(&self.namespace)
                .get(name)
                .await
                .map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local });
        }
        match self.backend.using::<T>(&self.namespace).get(name).await {
            Ok(object) => Ok(ReadResourceObject { object, provenance: ResourceProvenance::Local }),
            Err(ResourceError::NotFound { .. }) => {
                let replicas = { self.backend.get_replicas_typed::<T>(&self.namespace, name).await? };
                replicas.into_iter().next().ok_or_else(|| ResourceError::not_found(name))
            }
            Err(error) => Err(error),
        }
    }

    /// Read every visible provenance for one name without listing the namespace.
    pub async fn get_all(&self, name: &str) -> Result<ReadResourceList<T>, ResourceError> {
        if let Some(reads) = self.engine_copies(name).await {
            return reads.map(|items| ReadResourceList { items });
        }
        // These classes have at most one visible copy for a name.
        if T::REPLICATION_CLASS == crate::ReplicationClass::None || T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            return match self.get_local_first(name).await {
                Ok(item) => Ok(ReadResourceList { items: vec![item] }),
                Err(ResourceError::NotFound { .. }) => Ok(ReadResourceList { items: Vec::new() }),
                Err(error) => Err(error),
            };
        }
        ensure_replication_enabled::<T>()?;
        if self.backend.serves_read_views() {
            let backend = &self.backend;
            return backend.get_all_including_replicas_typed::<T>(&self.namespace, name).await;
        }
        let mut items = match self.backend.using::<T>(&self.namespace).get(name).await {
            Ok(object) => vec![ReadResourceObject { object, provenance: ResourceProvenance::Local }],
            Err(ResourceError::NotFound { .. }) => Vec::new(),
            Err(error) => return Err(error),
        };
        let replicas = { self.backend.get_replicas_typed::<T>(&self.namespace, name).await? };
        items.extend(replicas);
        let listed = ReadResourceList { items };
        if self.suppress_self_origin {
            self.suppress_shadowed_self_origin_sources(listed)
        } else {
            Ok(listed)
        }
    }

    pub async fn list(&self) -> Result<ReadResourceList<T>, ResourceError> {
        if T::REPLICATION_CLASS == crate::ReplicationClass::None {
            let items = self
                .backend
                .using::<T>(&self.namespace)
                .list()
                .await?
                .items
                .into_iter()
                .map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local })
                .collect();
            return Ok(ReadResourceList { items });
        }
        ensure_replication_enabled::<T>()?;
        if self.backend.serves_read_views() {
            let backend = &self.backend;
            return backend.list_including_replicas_typed::<T>(&self.namespace).await;
        }
        if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            let items = self
                .backend
                .definitions::<T>(&self.namespace)
                .list()
                .await?
                .into_iter()
                .map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local })
                .collect();
            return Ok(ReadResourceList { items });
        }
        if !self.suppress_self_origin {
            return self.list_sources().await;
        }
        let listed = self.list_sources().await?;
        self.suppress_shadowed_self_origin_sources(listed)
    }

    fn suppress_shadowed_self_origin_sources(&self, mut listed: ReadResourceList<T>) -> Result<ReadResourceList<T>, ResourceError> {
        let local_root = self.backend.local_root()?;
        let local_names = listed
            .items
            .iter()
            .filter(|item| matches!(item.provenance, ResourceProvenance::Local))
            .map(|item| item.object.metadata.name.clone())
            .collect::<HashSet<_>>();
        listed.items.retain(|item| {
            !matches!(
                &item.provenance,
                ResourceProvenance::Replica { origin_root, .. }
                    if origin_root == &local_root && local_names.contains(&item.object.metadata.name)
            )
        });
        Ok(listed)
    }

    pub async fn list_matching_labels(&self, required: &BTreeMap<String, String>) -> Result<ReadResourceList<T>, ResourceError> {
        if required.is_empty() {
            return self.list().await;
        }
        if self.backend.serves_read_views() && self.suppress_self_origin {
            return self.backend.list_including_replicas_typed_matching_labels::<T>(&self.namespace, required).await;
        }
        self.backend.query_declared::<T>(&self.namespace, &crate::StoreQuery::labels(required, true, self.suppress_self_origin)).await
    }

    /// Every home's engine copy of `name`, for a kind the engine serves other than a definition
    /// kind (definitions resolve through their resolver).
    async fn engine_copies(&self, name: &str) -> Option<Result<Vec<ReadResourceObject<T>>, ResourceError>> {
        if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            return None;
        }
        let reads = self.backend.definitions::<T>(&self.namespace).engine_reads().await?;
        Some(reads.map(|reads| reads.into_iter().filter(|read| read.object.metadata.name == name).collect()))
    }

    pub(crate) async fn list_sources(&self) -> Result<ReadResourceList<T>, ResourceError> {
        ensure_replication_enabled::<T>()?;
        if let Some(reads) = self.backend.definitions::<T>(&self.namespace).engine_reads().await {
            return reads.map(|items| ReadResourceList { items });
        }
        if self.backend.serves_read_views() {
            return Err(ResourceError::invalid("HTTP replica-source lists use the resource API"));
        }
        let local = self.backend.using::<T>(&self.namespace).list().await?;
        let mut items =
            local.items.into_iter().map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local }).collect::<Vec<_>>();
        let replicas = { self.backend.list_replicas_typed::<T>(&self.namespace).await? };
        items.extend(replicas);
        items.sort_by(|left, right| {
            left.object.metadata.name.cmp(&right.object.metadata.name).then_with(|| match (&left.provenance, &right.provenance) {
                (ResourceProvenance::Local, ResourceProvenance::Local) => std::cmp::Ordering::Equal,
                (ResourceProvenance::Local, ResourceProvenance::Replica { .. }) => std::cmp::Ordering::Less,
                (ResourceProvenance::Replica { .. }, ResourceProvenance::Local) => std::cmp::Ordering::Greater,
                (
                    ResourceProvenance::Replica { origin_root: left_origin, .. },
                    ResourceProvenance::Replica { origin_root: right_origin, .. },
                ) => left_origin.cmp(right_origin),
            })
        });
        Ok(ReadResourceList { items })
    }

    /// Lists every local and replicated source object without collapsing
    /// same-name replicas behind the local object.
    ///
    /// Consumers that combine host-local status need the individual sources;
    /// ordinary resource reads should continue to use [`Self::list`].
    pub async fn list_replica_sources(&self) -> Result<ReadResourceList<T>, ResourceError> {
        let listed = self.list_sources().await?;
        if self.suppress_self_origin {
            self.suppress_shadowed_self_origin_sources(listed)
        } else {
            Ok(listed)
        }
    }

    pub async fn watch(&self) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>, ResourceError> {
        ensure_replication_enabled::<T>()?;
        if let Some(watch) = self.backend.definitions::<T>(&self.namespace).engine_watch().await {
            // The engine's records stream their changes; the current ones are read by `list`.
            return watch.map(|(_, stream)| stream);
        }
        if self.backend.serves_read_views() {
            let backend = &self.backend;
            return backend.watch_including_replicas_typed::<T>(&self.namespace).await;
        }
        let raw = self.watch_sources().await?;
        if T::REPLICATION_CLASS != crate::ReplicationClass::Definitions && self.suppress_self_origin {
            let backend = self.backend.clone();
            let namespace = self.namespace.clone();
            let local_root = backend.local_root()?;
            return Ok(raw
                .filter_map(move |event| {
                    let backend = backend.clone();
                    let namespace = namespace.clone();
                    let local_root = local_root.clone();
                    async move {
                        crate::watch::complete_watch_lookup(async move {
                            let event = match event {
                                Ok(event) => event,
                                Err(error) => return Some(Err(error)),
                            };
                            let self_origin_name = match &event {
                                ReadWatchEvent::Added(item) | ReadWatchEvent::Modified(item) | ReadWatchEvent::Deleted(item) => matches!(
                                    &item.provenance,
                                    ResourceProvenance::Replica { origin_root, .. } if origin_root == &local_root
                                )
                                .then_some(item.object.metadata.name.as_str()),
                                ReadWatchEvent::DeletedByName { tombstone, provenance } => matches!(
                                    provenance,
                                    ResourceProvenance::Replica { origin_root, .. } if origin_root == &local_root
                                )
                                .then_some(tombstone.name.as_str()),
                            };
                            let Some(name) = self_origin_name else {
                                return Some(Ok(event));
                            };
                            match backend.using::<T>(&namespace).get(name).await {
                                Ok(_) => None,
                                Err(ResourceError::NotFound { .. }) => Some(Ok(event)),
                                Err(error) => Some(Err(error)),
                            }
                        })
                        .await
                        .unwrap_or_else(|error| Some(Err(error)))
                    }
                })
                .boxed());
        }
        let backend = self.backend.clone();
        let namespace = self.namespace.clone();
        Ok(raw
            .then(move |event| {
                let backend = backend.clone();
                let namespace = namespace.clone();
                async move {
                    crate::watch::complete_watch_lookup(async move {
                        let event = event?;
                        if let ReadWatchEvent::DeletedByName { tombstone, .. } = event {
                            return match backend.definitions::<T>(&namespace).get(&tombstone.name).await {
                                Ok(object) => {
                                    Ok(ReadWatchEvent::Modified(ReadResourceObject { object, provenance: ResourceProvenance::Local }))
                                }
                                Err(ResourceError::NotFound { .. }) => {
                                    Ok(ReadWatchEvent::DeletedByName { tombstone, provenance: ResourceProvenance::Local })
                                }
                                Err(error) => Err(error),
                            };
                        }
                        let fallback = match event {
                            ReadWatchEvent::Added(object) | ReadWatchEvent::Modified(object) | ReadWatchEvent::Deleted(object) => object,
                            ReadWatchEvent::DeletedByName { .. } => unreachable!("handled above"),
                        };
                        match backend.definitions::<T>(&namespace).get(&fallback.object.metadata.name).await {
                            Ok(object) => {
                                Ok(ReadWatchEvent::Modified(ReadResourceObject { object, provenance: ResourceProvenance::Local }))
                            }
                            // Definitions are one merged logical member, whose initial
                            // list uses Local provenance even when entirely replicated.
                            Err(ResourceError::NotFound { .. }) => Ok(ReadWatchEvent::Deleted(ReadResourceObject {
                                object: fallback.object,
                                provenance: ResourceProvenance::Local,
                            })),
                            Err(error) => Err(error),
                        }
                    })
                    .await?
                }
            })
            .boxed())
    }

    pub(crate) async fn watch_sources(
        &self,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>, ResourceError> {
        ensure_replication_enabled::<T>()?;
        if self.backend.serves_read_views() {
            return Err(ResourceError::invalid("HTTP replica-source watches use the resource API"));
        }
        let replicas = { self.backend.watch_replicas_typed::<T>(&self.namespace).await? };
        let local = self.backend.using::<T>(&self.namespace).watch(WatchStart::Now).await?.map(|event| event.map(ReadWatchEvent::local));
        Ok(stream::select(local, replicas).boxed())
    }
}

#[derive(Debug, Clone, bon::Builder)]
#[builder(builder_type(vis = "pub(in crate::backend)"))]
pub struct ReplicaWriter<T: Resource> {
    backend: ResourceBackend,
    origin_root: NodeId,
    namespace: String,
    #[builder(skip)]
    _marker: PhantomData<T>,
}

impl<T: Resource> ReplicaWriter<T> {
    pub async fn replace(&self, listed: &ResourceList<T>, synced_at: DateTime<Utc>) -> Result<(), ResourceError> {
        ensure_replication_enabled::<T>()?;
        self.backend.replace_replicas_typed(&self.origin_root, &self.namespace, listed, synced_at, None).await
    }

    /// Replace only one complete authoritative hash bucket; never move the log cursor.
    pub async fn replace_bucket(&self, bucket: u8, listed: &ResourceList<T>, synced_at: DateTime<Utc>) -> Result<(), ResourceError> {
        ensure_replication_enabled::<T>()?;
        if listed
            .items
            .iter()
            .any(|object| crate::digest_bucket(&object.metadata.name) != bucket || object.metadata.namespace != self.namespace)
        {
            return Err(ResourceError::invalid("snapshot contains keys outside its partition"));
        }
        self.backend.replace_replicas_typed(&self.origin_root, &self.namespace, listed, synced_at, Some(bucket)).await
    }

    /// A repaired, complete hierarchy proves a log cut. Confirm it atomically
    /// with the cached set before advancing the direct-origin prefix.
    pub async fn confirm_digest_position(&self, proof: &crate::PartitionDigest, previous: &ReplicaCursor) -> Result<(), ResourceError> {
        if proof.origin != self.origin_root
            || proof.kind != T::API_PATHS.kind
            || proof.namespace != self.namespace
            || proof.generation != previous.generation
        {
            return Err(ResourceError::invalid("digest prefix identity mismatch"));
        }
        self.backend.confirm_replica_digest_typed::<T>(&self.origin_root, &self.namespace, proof, previous).await
    }

    pub async fn digest(&self, generation: Option<String>) -> Result<crate::PartitionDigest, ResourceError> {
        ensure_replication_enabled::<T>()?;
        self.backend.replica_digest_typed::<T>(&self.origin_root, &self.namespace, generation).await
    }

    pub async fn apply(&self, event: crate::WatchEvent<T>, synced_at: DateTime<Utc>) -> Result<(), ResourceError> {
        ensure_replication_enabled::<T>()?;
        let (kind, object) = match event {
            crate::WatchEvent::Added(object) => (StoredReplicaEventKind::Added, object),
            crate::WatchEvent::Modified(object) => (StoredReplicaEventKind::Modified, object),
            crate::WatchEvent::Deleted(object) => (StoredReplicaEventKind::Deleted, object),
            crate::WatchEvent::DeletedByName(tombstone) => {
                return self.backend.apply_replica_tombstone_typed::<T>(&self.origin_root, &self.namespace, &tombstone, synced_at).await;
            }
        };
        self.backend.apply_replica_typed(&self.origin_root, &self.namespace, kind, &object, synced_at).await
    }

    pub async fn apply_batch(&self, events: &[crate::WatchEvent<T>], synced_at: DateTime<Utc>) -> Result<(), ResourceError> {
        ensure_replication_enabled::<T>()?;
        for batch in events.chunks(crate::replica::REPLICA_BATCH_SIZE) {
            let serialized = crate::transcode::<_, Vec<crate::WatchEvent<crate::SerializedResource>>>(batch)?;
            let stamped = serialized.into_iter().map(|event| (event, synced_at)).collect::<Vec<_>>();
            self.backend.apply_replica_batch_raw(crate::ResourceKind::of::<T>(), &self.origin_root, &self.namespace, &stamped).await?;
        }
        Ok(())
    }

    /// Relay batches retain each event's originating sync time for conflict resolution.
    pub async fn apply_stamped_batch(&self, events: &[(crate::WatchEvent<T>, DateTime<Utc>)]) -> Result<(), ResourceError> {
        ensure_replication_enabled::<T>()?;
        for batch in events.chunks(crate::replica::REPLICA_BATCH_SIZE) {
            let serialized = crate::transcode::<_, Vec<(crate::WatchEvent<crate::SerializedResource>, DateTime<Utc>)>>(batch)?;
            self.backend.apply_replica_batch_raw(crate::ResourceKind::of::<T>(), &self.origin_root, &self.namespace, &serialized).await?;
        }
        Ok(())
    }

    /// Prove the entire contiguous batch before applying any of it. A crash
    /// before cursor advancement replays idempotent events rather than skipping them.
    pub async fn apply_direct_batch(&self, events: &[crate::WatchEvent<T>], synced_at: DateTime<Utc>) -> Result<(), ResourceError> {
        if events.is_empty() {
            return Ok(());
        }
        let previous = self.cursor().await?.ok_or_else(|| ResourceError::invalid("replica prefix missing; snapshot required"))?;
        let mut version = previous.resource_version.parse::<u64>().map_err(|err| ResourceError::invalid(err.to_string()))?;
        let mut next = previous.resource_version.clone();
        for event in events {
            next = match event {
                crate::WatchEvent::Added(object) | crate::WatchEvent::Modified(object) | crate::WatchEvent::Deleted(object) => {
                    object.metadata.resource_version.clone()
                }
                crate::WatchEvent::DeletedByName(tombstone) => tombstone.resource_version.clone(),
            };
            let next_version = next.parse::<u64>().map_err(|err| ResourceError::invalid(err.to_string()))?;
            if version.checked_add(1) != Some(next_version) {
                return Err(ResourceError::invalid("replica sequence gap; snapshot required"));
            }
            version = next_version;
        }
        self.apply_batch(events, synced_at).await?;
        self.backend.advance_replica_cursor_typed::<T>(&self.origin_root, &self.namespace, &previous, &next).await
    }

    /// Apply a direct origin event, then commit its contiguous prefix. Relay
    /// `apply` never advances this cursor. A crash between these operations only
    /// replays an already-applied event; it cannot skip a missing event.
    pub async fn apply_direct(&self, event: crate::WatchEvent<T>, synced_at: DateTime<Utc>) -> Result<(), ResourceError> {
        self.apply_direct_batch(&[event], synced_at).await
    }

    /// Require a new authoritative snapshot without removing cached objects.
    pub async fn invalidate_cursor(&self) -> Result<(), ResourceError> {
        ensure_replication_enabled::<T>()?;
        self.backend.invalidate_replica_cursor_typed::<T>(&self.origin_root, &self.namespace).await
    }

    pub async fn cursor(&self) -> Result<Option<ReplicaCursor>, ResourceError> {
        ensure_replication_enabled::<T>()?;
        self.backend.replica_cursor_typed::<T>(&self.origin_root, &self.namespace).await
    }
}

fn ensure_replication_enabled<T: Resource>() -> Result<(), ResourceError> {
    if T::REPLICATION_CLASS != crate::ReplicationClass::None {
        Ok(())
    } else {
        Err(ResourceError::invalid(format!("{} is not enabled for overlay replication", T::API_PATHS.kind)))
    }
}

pub struct TypedResolver<T: Resource> {
    lifecycle_status_reads: Option<std::sync::Arc<dyn LifecycleReads>>,
    pub(crate) backend: ResourceBackend,
    pub(crate) namespace: String,
    pub(crate) _marker: PhantomData<T>,
}

impl<T: Resource> std::fmt::Debug for TypedResolver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypedResolver").field("backend", &self.backend).field("namespace", &self.namespace).finish()
    }
}

impl<T: Resource> std::clone::Clone for TypedResolver<T> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            namespace: self.namespace.clone(),
            lifecycle_status_reads: self.lifecycle_status_reads.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T: Resource> TypedResolver<T> {
    /// Local controller writes select engine validation only after a checked ownership read.
    pub fn with_lifecycle_status_reads(mut self, reads: std::sync::Arc<dyn LifecycleReads>) -> Self {
        self.lifecycle_status_reads = Some(reads);
        self
    }

    pub async fn get(&self, name: &str) -> Result<ResourceObject<T>, ResourceError> {
        if let Some(local) = self.engine_local().await {
            return local?.into_iter().find(|object| object.metadata.name == name).ok_or_else(|| ResourceError::not_found(name));
        }
        dispatch_backend!(self, get_typed, name)
    }

    /// This home's own records of an engine-served definition kind (#3824 layer 2b-ii), as the
    /// old store's local rows: no built-ins and no other home's records.
    async fn engine_local(&self) -> Option<Result<Vec<ResourceObject<T>>, ResourceError>> {
        let reads = self.backend.definitions::<T>(&self.namespace).engine_reads().await?;
        Some(reads.map(|reads| {
            reads
                .into_iter()
                .filter(|read| {
                    matches!(read.provenance, ResourceProvenance::Local)
                        && !flotilla_resources::builtins::is_builtin(T::API_PATHS.kind, &read.object.metadata.name)
                })
                .map(|read| read.object)
                .collect()
        }))
    }

    /// Read the local collection's list/watch position without decoding objects.
    /// HTTP uses collection metadata from the standard list endpoint.
    pub async fn current_position(&self) -> Result<crate::ResourcePosition, ResourceError> {
        dispatch_backend!(self, current_position_typed)
    }

    pub async fn digest(&self, query: &crate::DigestQuery) -> Result<crate::PartitionDigest, ResourceError> {
        ensure_replication_enabled::<T>()?;
        dispatch_backend!(self, digest_typed, query)
    }

    pub async fn list(&self) -> Result<ResourceList<T>, ResourceError> {
        if let Some(local) = self.engine_local().await {
            return Ok(ResourceList { items: local?, resource_version: String::new(), generation: None });
        }
        dispatch_backend!(self, list_typed)
    }

    pub async fn list_matching_labels(&self, required: &BTreeMap<String, String>) -> Result<ResourceList<T>, ResourceError> {
        if let Some(local) = self.engine_local().await {
            let items = local?
                .into_iter()
                .filter(|object| required.iter().all(|(key, value)| object.metadata.labels.get(key) == Some(value)))
                .collect();
            return Ok(ResourceList { items, resource_version: String::new(), generation: None });
        }
        dispatch_backend!(self, list_typed_matching_labels, required)
    }

    pub async fn create(&self, meta: &InputMeta, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
        if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            return self.backend.definitions::<T>(&self.namespace).create(meta, spec).await;
        }
        if let Some(records) = self.engine_owned() {
            T::validate_spec(meta, spec)?;
            if self.engine_home_record(&*records, &meta.name).await?.is_some() {
                return Err(ResourceError::conflict(&meta.name, "resource already exists"));
            }
            return self.engine_apply(&*records, meta, spec).await;
        }
        dispatch_backend!(self, create_typed, meta, spec)
    }

    /// The engine port serving this kind, for a kind the old store keeps a record of per home
    /// (#3824 layer 3): definitions are served through their resolver instead.
    fn engine_owned(&self) -> Option<std::sync::Arc<dyn flotilla_resources::engine_records::EngineRecords>> {
        (T::REPLICATION_CLASS != crate::ReplicationClass::Definitions).then(|| self.backend.engine_records(T::API_PATHS.kind)).flatten()
    }

    /// This home's own engine record `name`, if any.
    async fn engine_home_record(
        &self,
        records: &dyn flotilla_resources::engine_records::EngineRecords,
        name: &str,
    ) -> Result<Option<flotilla_resources::engine_records::EngineObject>, ResourceError> {
        let home = records.home();
        let sources = records.sources(T::API_PATHS.kind).await.map_err(ResourceError::other)?;
        Ok(sources.into_iter().find(|record| record.name == name && record.home == home))
    }

    /// The engine record `name` at this home, refused unless it is at `resource_version`.
    async fn engine_current(
        &self,
        records: &dyn flotilla_resources::engine_records::EngineRecords,
        name: &str,
        resource_version: &str,
    ) -> Result<(), ResourceError> {
        match self.engine_home_record(records, name).await? {
            None => Err(ResourceError::not_found(name)),
            Some(current) if current.version.to_string() != resource_version => Err(ResourceError::conflict(name, "stale resourceVersion")),
            Some(_) => Ok(()),
        }
    }

    async fn engine_apply(
        &self,
        records: &dyn flotilla_resources::engine_records::EngineRecords,
        meta: &InputMeta,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let value = serde_json::to_value(spec).map_err(|error| ResourceError::decode(error.to_string()))?;
        let record =
            records.apply(T::API_PATHS.kind, &meta.name, &meta.labels, &meta.annotations, value).await.map_err(ResourceError::other)?;
        crate::definition::engine_object(&self.namespace, record)
    }

    pub async fn update(&self, meta: &InputMeta, resource_version: &str, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
        if let Some(records) = self.engine_owned() {
            self.engine_current(&*records, &meta.name, resource_version).await?;
            let current = self.get(&meta.name).await?;
            T::validate_spec_update(&current.spec, spec)?;
            T::validate_spec(meta, spec)?;
            return self.engine_apply(&*records, meta, spec).await;
        }
        if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            let definitions = self.backend.definitions::<T>(&self.namespace);
            let current = definitions.get(&meta.name).await?;
            let local_version_matches = self.get(&meta.name).await.is_ok_and(|local| local.metadata.resource_version == resource_version);
            if current.metadata.resource_version != resource_version && !local_version_matches {
                return Err(ResourceError::conflict(&meta.name, "stale resourceVersion"));
            }
            return definitions.apply(meta, spec).await;
        }
        dispatch_backend!(self, update_typed, meta, resource_version, spec)
    }

    /// Ownership can be acquired after the read below. A legacy validation refusal is
    /// retried by reconciliation with the newly held lifecycle; it cannot change engine state.
    /// HTTP validates on its server and does not transmit this local validator selection.
    pub async fn update_status(&self, name: &str, resource_version: &str, status: &T::Status) -> Result<ResourceObject<T>, ResourceError> {
        let engine_owned = match (T::STATUS_LIFECYCLE_KIND, self.lifecycle_status_reads.as_ref()) {
            (Some(lifecycle), Some(reads)) => {
                let reads = std::sync::Arc::clone(reads);
                let name = name.to_owned();
                tokio::task::spawn_blocking(move || reads.try_reading(lifecycle, &name, None))
                    .await
                    .map_err(|error| ResourceError::other(error.to_string()))?
                    .map_err(ResourceError::other)?
                    .is_some()
            }
            _ => false,
        };
        // A definition kind with a status (Project, #3824 layer 3b) writes it through its port too.
        if let Some(records) = self.backend.engine_records(T::API_PATHS.kind) {
            self.engine_current(&*records, name, resource_version).await?;
            let value = serde_json::to_value(status).map_err(|error| ResourceError::decode(error.to_string()))?;
            let record = records.set_status(T::API_PATHS.kind, name, value).await.map_err(ResourceError::other)?;
            return crate::definition::engine_object(&self.namespace, record);
        }
        if !engine_owned {
            return dispatch_backend!(self, update_status_typed, name, resource_version, status);
        }
        // Ownership is checked before the resource transaction. Endings do not revoke it:
        // the engine retains ended resources, so lifecycle progress cannot select the old validator again.
        let kind = crate::ResourceKind::of::<T>().with_engine_status_validator::<T>();
        let status = transcode_request(self.backend.blocking_request_encoding(), status.clone()).await?;
        crate::typed_object(self.backend.update_status_raw(kind, &self.namespace, name, resource_version, &status).await?)
    }

    pub async fn delete(&self, name: &str) -> Result<(), ResourceError> {
        if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            return self.backend.definitions::<T>(&self.namespace).delete(name).await;
        }
        if let Some(records) = self.engine_owned() {
            if self.engine_home_record(&*records, name).await?.is_none() {
                return Err(ResourceError::not_found(name));
            }
            return records.delete(T::API_PATHS.kind, name).await.map_err(ResourceError::other);
        }
        dispatch_backend!(self, delete_typed, name)
    }

    pub(crate) async fn tombstone(&self, name: &str) -> Result<crate::watch::TombstoneWrite, ResourceError> {
        ensure_replication_enabled::<T>()?;
        let origin_root = self.backend.local_root()?;
        let replica_cursor = self.backend.replica_writer::<T>(origin_root, &self.namespace).cursor().await?;
        let minimum_resource_version = replica_cursor.as_ref().map(|cursor| cursor.resource_version.as_str());
        dispatch_backend!(self, tombstone_typed, name, minimum_resource_version)
    }

    pub async fn watch(&self, start: WatchStart) -> Result<WatchStream<T>, ResourceError> {
        if let Some(watch) = self.backend.definitions::<T>(&self.namespace).engine_watch().await {
            // An engine-served kind streams every home's changes; there is no position to resume
            // from, so a watcher lists again on each start.
            let (_, stream) = watch?;
            let events = stream.map(|event| {
                event.map(|event| match event {
                    ReadWatchEvent::Added(read) => crate::WatchEvent::Added(read.object),
                    ReadWatchEvent::Modified(read) => crate::WatchEvent::Modified(read.object),
                    ReadWatchEvent::Deleted(read) => crate::WatchEvent::Deleted(read.object),
                    ReadWatchEvent::DeletedByName { tombstone, .. } => crate::WatchEvent::DeletedByName(tombstone),
                })
            });
            return Ok(WatchStream::new(None, events.boxed()));
        }
        dispatch_backend!(self, watch_typed, start)
    }
}

impl<T: FieldOwnedResource> TypedResolver<T> {
    /// The only update path for an enrolled resource kind.
    ///
    /// Observe mode records attempted ownership violations and applies the
    /// writer's owned fields while preserving protected stored values. Enforce
    /// mode records the same event and refuses the complete write.
    pub async fn write_spec(
        &self,
        writer: &WriterIdentity,
        meta: &InputMeta,
        resource_version: &str,
        requested: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        self.write_spec_with_enforcement(writer, meta, resource_version, requested, T::OWNERSHIP_ENFORCEMENT).await
    }

    pub(crate) async fn write_spec_rejecting_violations(
        &self,
        writer: &WriterIdentity,
        meta: &InputMeta,
        resource_version: &str,
        requested: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        self.write_spec_with_enforcement(writer, meta, resource_version, requested, OwnershipEnforcement::Enforce).await
    }

    async fn write_spec_with_enforcement(
        &self,
        writer: &WriterIdentity,
        meta: &InputMeta,
        resource_version: &str,
        requested: &T::Spec,
        enforcement: OwnershipEnforcement,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let current = self.get(&meta.name).await?;
        let (merged, violations) = merge_owned_spec::<T>(&current.spec, requested, writer, &self.namespace, &meta.name)?;
        self.record_field_ownership_violations(&violations).await?;
        if !violations.is_empty() && enforcement == OwnershipEnforcement::Enforce {
            return Err(ResourceError::FieldOwnership { violations });
        }
        self.update(meta, resource_version, &merged).await
    }

    pub(crate) async fn record_field_ownership_violations(&self, violations: &[FieldOwnershipViolation]) -> Result<(), ResourceError> {
        for violation in violations {
            tracing::warn!(
                kind = %violation.kind,
                namespace = %violation.namespace,
                name = %violation.name,
                writer_role = ?violation.writer.role,
                field = %violation.field,
                attempted_value = %violation.attempted_value,
                rule = %violation.rule,
                "resource field ownership violation"
            );
            self.backend.record_field_ownership_violation(violation.clone()).await?;
        }
        Ok(())
    }
}

impl<T: Resource> TypedResolver<T> {
    /// The backing store and namespace used by this typed resolver.
    pub fn context(&self) -> (&ResourceBackend, &str) {
        (&self.backend, &self.namespace)
    }
}

impl std::ops::Deref for ResourceBackend {
    type Target = dyn crate::Store;
    fn deref(&self) -> &Self::Target {
        &*self.store
    }
}

impl<S: crate::Store + 'static> From<S> for ResourceBackend {
    fn from(store: S) -> Self {
        Self::from_arc(std::sync::Arc::new(store))
    }
}
