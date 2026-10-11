use crate::{convert_read, transcode, ReadObserver, ResourceKind, SerializedResource};
use crate::{watch, DigestQuery, MessageQuery, PartitionDigest, ResourcePosition};
use crate::{SerializedReadWatchEvent, SerializedWatchEvent, SerializedWatchStream, WatchAction, WatchObject, WatchPayload};
use crate::{Store, StoreExt};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use chrono::Utc;
use flotilla_protocol::NodeId;
use flotilla_resources::{ReadResourceObject, ReplicaCursor, ResourceProvenance};
use flotilla_resources::{ResourceList, ResourceTombstone, WatchStart};
use futures::{stream, StreamExt};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{
    error::ResourceError,
    field_ownership::FieldOwnershipViolation,
    replica::{StoredReplicaEvent, StoredReplicaEventKind},
    resource::{InputMeta, MergeMetadata, ObjectMeta, ResourceObject},
    retention::{EventRetention, ResourceStoreDiagnostics, FIELD_OWNERSHIP_VIOLATION_TTL_HOURS, MAX_FIELD_OWNERSHIP_VIOLATIONS},
    watch::WatchChannel,
};

type StoreKey = (String, String, String, String);

#[derive(Debug, Clone, Default, bon::Builder)]
#[builder(builder_type(vis = "pub(in crate::in_memory)"))]
pub struct InMemoryBackend {
    stores: Arc<Mutex<HashMap<StoreKey, ResourceStore>>>,
    read_observer: Option<Arc<dyn ReadObserver>>,
    replicas: Arc<Mutex<ReplicaState>>,
    durable_replicas: Option<crate::ResourceBackend>,
    generation: Option<String>,
    event_retention: EventRetention,
    local_root: Option<NodeId>,
    // Serialize cross-kind Project/FleetDesignation admission across clones.
    pub(crate) hierarchy_admission: Arc<tokio::sync::Mutex<()>>,
    ownership_violations: Arc<Mutex<Vec<FieldOwnershipViolation>>>,
}

type ReplicaKey = (NodeId, StoreKey);

#[derive(Debug, Default)]
struct ReplicaState {
    partitions: HashMap<ReplicaKey, ReplicaPartition>,
    watchers: HashMap<StoreKey, WatchChannel<StoredReplicaEvent>>,
}

#[derive(Debug, Default)]
struct ReplicaPartition {
    queries: crate::QueryIndex,
    messages: crate::message_query::MessageIndex,
    objects: HashMap<String, Value>,
    digest: flotilla_resources::digest::DigestIndex,
    // Retained for deleted names as a tombstone so an older relayed write
    // cannot resurrect an object after a newer delete.
    synced_at_by_name: HashMap<String, chrono::DateTime<Utc>>,
    cursor: Option<ReplicaCursor>,
}

#[derive(Debug)]
struct ResourceStore {
    queries: crate::QueryIndex,
    messages: crate::message_query::MessageIndex,
    objects: HashMap<String, Value>,
    digest: flotilla_resources::digest::DigestIndex,
    tombstones: HashMap<String, ResourceTombstone>,
    next_version: u64,
    watchers: WatchChannel<StoredEvent>,
    event_log: Vec<StoredEvent>,
    compacted_through: u64,
}

#[derive(Debug, Clone)]
struct StoredEvent {
    version: u64,
    kind: StoredEventKind,
    object: Value,
}

#[derive(Debug, Clone, Copy)]
enum StoredEventKind {
    Added,
    Modified,
    Deleted,
}

impl ResourceStore {
    fn current_version(&self) -> u64 {
        self.next_version.saturating_sub(1)
    }

    fn allocate_version(&mut self) -> u64 {
        let version = self.next_version;
        self.next_version += 1;
        version
    }

    fn push_event(&mut self, event: StoredEvent, max_events: usize) {
        let name = event.object["metadata"]["name"].as_str().expect("encoded event name");
        self.queries.update(name, (!matches!(event.kind, StoredEventKind::Deleted)).then_some(&event.object));
        self.messages.update(name, (!matches!(event.kind, StoredEventKind::Deleted)).then_some(&event.object));
        let version = event.version.to_string();
        self.digest.set(name, (!matches!(event.kind, StoredEventKind::Deleted)).then_some(version.as_str()));
        let excess = self.event_log.len().saturating_add(1).saturating_sub(max_events);
        if excess > 0 {
            if let Some(last_removed) = self.event_log.get(excess - 1) {
                self.compacted_through = last_removed.version;
            }
            self.event_log.drain(..excess);
        }
        self.event_log.push(event.clone());
        self.watchers.send(event);
    }
}

impl Default for ResourceStore {
    fn default() -> Self {
        Self {
            queries: Default::default(),
            messages: Default::default(),
            objects: HashMap::new(),
            digest: flotilla_resources::digest::DigestIndex::default(),
            tombstones: HashMap::new(),
            next_version: 1,
            watchers: WatchChannel::default(),
            event_log: Vec::new(),
            compacted_through: 0,
        }
    }
}

impl InMemoryBackend {
    /// Observe decoded objects without changing query results or store ownership.
    pub fn with_read_observer(mut self, observer: Arc<dyn ReadObserver>) -> Self {
        self.read_observer = Some(observer);
        self
    }

    pub fn read_observer(&self) -> Option<&dyn ReadObserver> {
        self.read_observer.as_deref()
    }

    fn count_read(&self, resource_kind: ResourceKind, count: usize) {
        if let Some(observer) = &self.read_observer {
            observer.read(resource_kind.paths.kind, count);
        }
    }

    pub async fn query_messages(
        &self,
        namespace: &str,
        query: &crate::MessageQuery,
        include_replicas: bool,
    ) -> Result<Vec<ReadResourceObject<crate::Message>>, ResourceError> {
        let resource_kind = ResourceKind::of::<crate::Message>();
        let key = Self::store_key(ResourceKind::of::<crate::Message>(), namespace);
        let mut items = Vec::new();
        {
            let stores = self.stores.lock().await;
            if let Some(store) = stores.get(&key) {
                for name in store.messages.names(query) {
                    items.push(ReadResourceObject {
                        object: transcode(Self::decode_object(resource_kind, store.objects[&name].clone())?)?,
                        provenance: ResourceProvenance::Local,
                    });
                }
            }
        }
        if include_replicas {
            if let Some(backend) = &self.durable_replicas {
                items.extend(backend.query_message_replicas(namespace, query).await?);
            } else {
                let replicas = self.replicas.lock().await;
                for ((origin_root, partition_key), partition) in &replicas.partitions {
                    if partition_key != &key {
                        continue;
                    }
                    for name in partition.messages.names(query) {
                        items.push(ReadResourceObject {
                            object: transcode(Self::decode_object(resource_kind, partition.objects[&name].clone())?)?,
                            provenance: ResourceProvenance::Replica {
                                origin_root: origin_root.clone(),
                                last_synced_at: partition.synced_at_by_name[&name],
                            },
                        });
                    }
                }
            }
        }
        self.count_read(ResourceKind::of::<crate::Message>(), items.len());
        Ok(items)
    }

    pub fn observed() -> Self {
        Self {
            stores: Arc::default(),
            read_observer: None,
            replicas: Arc::default(),
            durable_replicas: None,
            generation: Some(uuid::Uuid::new_v4().to_string()),
            event_retention: EventRetention::default(),
            local_root: None,
            hierarchy_admission: Arc::default(),
            ownership_violations: Arc::default(),
        }
    }

    /// Local observations have a new generation per process, while received
    /// facts remain durable across holder restarts (ADR 0016). Only the replica
    /// operations use this store; local writes always remain in memory.
    pub fn observed_with_durable_replicas(replicas: impl Into<crate::ResourceBackend>) -> Self {
        Self { durable_replicas: Some(replicas.into()), ..Self::observed() }
    }

    pub async fn delete_decode_quarantine_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<bool, ResourceError> {
        match &self.durable_replicas {
            Some(backend) => backend.delete_decode_quarantine_raw(resource_kind, namespace, name).await,
            None => Ok(false),
        }
    }

    pub fn with_event_retention(event_retention: EventRetention) -> Self {
        Self {
            stores: Arc::default(),
            read_observer: None,
            replicas: Arc::default(),
            durable_replicas: None,
            generation: None,
            event_retention,
            local_root: None,
            hierarchy_admission: Arc::default(),
            ownership_violations: Arc::default(),
        }
    }

    pub fn observed_with_event_retention(event_retention: EventRetention) -> Self {
        Self {
            stores: Arc::default(),
            read_observer: None,
            replicas: Arc::default(),
            durable_replicas: None,
            generation: Some(uuid::Uuid::new_v4().to_string()),
            event_retention,
            local_root: None,
            hierarchy_admission: Arc::default(),
            ownership_violations: Arc::default(),
        }
    }

    pub(crate) fn with_local_root(mut self, local_root: NodeId) -> Self {
        if let Some(backend) = self.durable_replicas.take() {
            self.durable_replicas = Some(backend.with_local_root(local_root.clone()));
        }
        self.local_root = Some(local_root);
        self
    }

    pub(crate) fn local_root(&self) -> NodeId {
        self.local_root.clone().unwrap_or_else(|| NodeId::new("local"))
    }

    pub async fn diagnostics(&self) -> Result<ResourceStoreDiagnostics, ResourceError> {
        let stores = self.stores.lock().await;
        let object_count = stores.values().map(|store| store.objects.len() as u64).sum();
        let event_count = stores.values().map(|store| store.event_log.len() as u64).sum();
        let resource_stream_count = stores.values().filter(|store| store.current_version() > 0).count() as u64;
        let mut diagnostics = ResourceStoreDiagnostics::new(object_count, event_count, resource_stream_count, self.event_retention);
        let mut violations = self.ownership_violations.lock().await;
        let cutoff = Utc::now() - chrono::Duration::hours(FIELD_OWNERSHIP_VIOLATION_TTL_HOURS);
        violations.retain(|violation| violation.observed_at >= cutoff);
        diagnostics.field_ownership_violations = violations.clone();
        Ok(diagnostics)
    }

    pub async fn record_field_ownership_violation(&self, violation: FieldOwnershipViolation) {
        let mut violations = self.ownership_violations.lock().await;
        violations.push(violation);
        let excess = violations.len().saturating_sub(MAX_FIELD_OWNERSHIP_VIOLATIONS);
        if excess > 0 {
            violations.drain(..excess);
        }
    }

    fn store_key(resource_kind: ResourceKind, namespace: &str) -> StoreKey {
        (
            resource_kind.paths.group.to_string(),
            resource_kind.paths.version.to_string(),
            resource_kind.paths.plural.to_string(),
            namespace.to_string(),
        )
    }

    pub async fn local_namespaces_raw(&self, resource_kind: ResourceKind) -> Result<Vec<String>, ResourceError> {
        let stores = self.stores.lock().await;
        let mut namespaces = stores
            .iter()
            .filter(|(key, store)| {
                key.0 == resource_kind.paths.group
                    && key.1 == resource_kind.paths.version
                    && key.2 == resource_kind.paths.plural
                    && !store.objects.is_empty()
            })
            .map(|(key, _)| key.3.clone())
            .collect::<Vec<_>>();
        namespaces.sort();
        Ok(namespaces)
    }

    pub async fn stored_namespaces_raw(&self, resource_kind: ResourceKind) -> Result<Vec<String>, ResourceError> {
        let mut namespaces = self.local_namespaces_raw(resource_kind).await?.into_iter().collect::<std::collections::BTreeSet<_>>();
        if let Some(backend) = &self.durable_replicas {
            namespaces.extend(backend.stored_namespaces_raw(resource_kind).await?);
            return Ok(namespaces.into_iter().collect());
        }
        let replicas = self.replicas.lock().await;
        for ((_, key), partition) in &replicas.partitions {
            if key.0 == resource_kind.paths.group
                && key.1 == resource_kind.paths.version
                && key.2 == resource_kind.paths.plural
                && !partition.objects.is_empty()
            {
                namespaces.insert(key.3.clone());
            }
        }
        Ok(namespaces.into_iter().collect())
    }

    fn clone_through_serde<T>(value: &T) -> Result<T, ResourceError>
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        serde_json::from_value(serde_json::to_value(value).map_err(|err| ResourceError::decode(format!("serialize value: {err}")))?)
            .map_err(|err| ResourceError::decode(format!("deserialize value: {err}")))
    }

    async fn with_store_mut<R>(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        f: impl FnOnce(&mut ResourceStore) -> Result<R, ResourceError>,
    ) -> Result<R, ResourceError> {
        let mut stores = self.stores.lock().await;
        let store = stores.entry(Self::store_key(resource_kind, namespace)).or_default();
        f(store)
    }

    async fn with_store<R>(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        f: impl FnOnce(&ResourceStore) -> Result<R, ResourceError>,
    ) -> Result<R, ResourceError> {
        let stores = self.stores.lock().await;
        let empty = ResourceStore::default();
        let store = stores.get(&Self::store_key(resource_kind, namespace)).unwrap_or(&empty);
        f(store)
    }

    fn decode_object(resource_kind: ResourceKind, value: Value) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        resource_kind.decode_object(value)
    }

    fn encode_object(resource_kind: ResourceKind, object: &ResourceObject<SerializedResource>) -> Result<Value, ResourceError> {
        resource_kind.encode_object(object)
    }

    fn notify_replica_watchers(state: &mut ReplicaState, key: &StoreKey, event: StoredReplicaEvent) {
        if let Some(watchers) = state.watchers.get_mut(key) {
            watchers.send(event);
        }
    }

    pub async fn list_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.list_replicas_raw(resource_kind, namespace).await;
        }
        let key = Self::store_key(resource_kind, namespace);
        let replicas = self.replicas.lock().await;
        let mut items = Vec::new();
        for ((origin_root, partition_key), partition) in &replicas.partitions {
            if partition_key != &key {
                continue;
            }
            for (name, value) in &partition.objects {
                let last_synced_at = partition
                    .synced_at_by_name
                    .get(name)
                    .copied()
                    .ok_or_else(|| ResourceError::other(format!("replica row '{name}' has no sync timestamp")))?;
                items.push(ReadResourceObject {
                    object: Self::decode_object(resource_kind, value.clone())?,
                    provenance: ResourceProvenance::Replica { origin_root: origin_root.clone(), last_synced_at },
                });
            }
        }
        self.count_read(resource_kind, items.len());
        Ok(items)
    }

    pub async fn get_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.get_replicas_raw(resource_kind, namespace, name).await;
        }
        let key = Self::store_key(resource_kind, namespace);
        let replicas = self.replicas.lock().await;
        let mut items = Vec::new();
        for ((origin_root, partition_key), partition) in &replicas.partitions {
            if partition_key != &key {
                continue;
            }
            let Some(value) = partition.objects.get(name) else {
                continue;
            };
            let last_synced_at = partition
                .synced_at_by_name
                .get(name)
                .copied()
                .ok_or_else(|| ResourceError::other(format!("replica row '{name}' has no sync timestamp")))?;
            items.push((
                origin_root.clone(),
                ReadResourceObject {
                    object: Self::decode_object(resource_kind, value.clone())?,
                    provenance: ResourceProvenance::Replica { origin_root: origin_root.clone(), last_synced_at },
                },
            ));
        }
        self.count_read(resource_kind, items.len());
        items.sort_by(|(left, _), (right, _)| left.cmp(right));
        Ok(items.into_iter().map(|(_, item)| item).collect())
    }

    pub async fn watch_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<SerializedReadWatchEvent, ResourceError>>, ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.watch_replicas_raw(resource_kind, namespace).await;
        }
        let key = Self::store_key(resource_kind, namespace);
        let rx = self.replicas.lock().await.watchers.entry(key).or_default().subscribe(resource_kind.paths.kind, namespace);
        Ok(stream::unfold(rx, move |mut rx| async move {
            let event = match rx.next().await? {
                Ok(event) => event,
                Err(error) => return Some((Err(error), rx)),
            };
            let provenance = ResourceProvenance::Replica { origin_root: event.origin_root.clone(), last_synced_at: event.synced_at };
            let action = WatchAction::from(event.kind);
            let decoded =
                Ok(SerializedReadWatchEvent { event: SerializedWatchEvent { action, payload: WatchPayload::Shared(event) }, provenance });
            Some((decoded, rx))
        })
        .boxed())
    }

    pub async fn replace_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        listed: &ResourceList<SerializedResource>,
        synced_at: chrono::DateTime<Utc>,
        bucket: Option<u8>,
    ) -> Result<(), ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.replace_replicas_raw(resource_kind, origin_root, namespace, listed, synced_at, bucket).await;
        }
        let store_key = Self::store_key(resource_kind, namespace);
        let replica_key = (origin_root.clone(), store_key.clone());
        let mut state = self.replicas.lock().await;
        let mut old = state.partitions.remove(&replica_key).unwrap_or_default();
        let mut digest = std::mem::take(&mut old.digest);
        let mut queries = std::mem::take(&mut old.queries);
        let mut messages = std::mem::take(&mut old.messages);
        // Move the unaffected map intact. Only the selected leaf is compared,
        // cloned, decoded or notified; sparse repair never scans other bodies.
        let mut objects = if let Some(bucket) = bucket {
            let mut unaffected = std::mem::take(&mut old.objects);
            for name in digest.names(bucket) {
                if let Some(value) = unaffected.remove(name) {
                    old.objects.insert(name.clone(), value);
                }
            }
            unaffected
        } else {
            HashMap::new()
        };
        // Absent names retain deletion fences against delayed relays. A recreated
        // key is absent from old.objects, so insertion below stamps it afresh.
        let mut synced_at_by_name = old.synced_at_by_name;
        for object in &listed.items {
            let name = &object.metadata.name;
            let encoded = Self::encode_object(resource_kind, object)?;
            let unchanged = old.objects.get(name) == Some(&encoded);
            queries.update(name, Some(&encoded));
            messages.update(name, Some(&encoded));
            objects.insert(name.clone(), encoded);
            digest.set(name, Some(&object.metadata.resource_version));
            if !unchanged {
                synced_at_by_name.insert(name.clone(), synced_at);
            }
        }
        let mut events = Vec::new();
        for object in &listed.items {
            let name = &object.metadata.name;
            let value = objects.get(name).expect("encoded snapshot object");
            if old.objects.get(name) == Some(value) {
                continue;
            }
            events.push(StoredReplicaEvent {
                origin_root: origin_root.clone(),
                synced_at,
                kind: if old.objects.contains_key(name) { StoredReplicaEventKind::Modified } else { StoredReplicaEventKind::Added },
                object: value.clone(),
            });
        }
        for (name, value) in old.objects {
            if !objects.contains_key(&name) {
                // Fence stale relays as well as removing the visible replica.
                queries.update(&name, None);
                messages.update(&name, None);
                digest.set(&name, None);
                synced_at_by_name.insert(name, synced_at);
                events.push(StoredReplicaEvent {
                    origin_root: origin_root.clone(),
                    synced_at,
                    kind: StoredReplicaEventKind::Deleted,
                    object: value,
                });
            }
        }
        state.partitions.insert(
            replica_key,
            ReplicaPartition {
                queries,
                messages,
                objects,
                digest,
                synced_at_by_name,
                cursor: if bucket.is_some() {
                    old.cursor
                } else {
                    Some(ReplicaCursor { resource_version: listed.resource_version.clone(), generation: listed.generation.clone() })
                },
            },
        );
        for event in events {
            Self::notify_replica_watchers(&mut state, &store_key, event);
        }
        Ok(())
    }

    pub async fn apply_replica_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        kind: StoredReplicaEventKind,
        object: &ResourceObject<SerializedResource>,
        synced_at: chrono::DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.apply_replica_raw(resource_kind, origin_root, namespace, kind, object, synced_at).await;
        }
        let store_key = Self::store_key(resource_kind, namespace);
        let replica_key = (origin_root.clone(), store_key.clone());
        let encoded = Self::encode_object(resource_kind, object)?;
        let mut state = self.replicas.lock().await;
        let partition = state.partitions.entry(replica_key).or_default();
        let unchanged = match kind {
            StoredReplicaEventKind::Added | StoredReplicaEventKind::Modified => {
                match (partition.objects.get(&object.metadata.name), partition.synced_at_by_name.get(&object.metadata.name)) {
                    (_, Some(existing_synced_at)) if existing_synced_at > &synced_at => true,
                    (Some(existing), Some(existing_synced_at)) if existing_synced_at == &synced_at => {
                        serde_json::to_string(existing)
                            .map_err(|error| ResourceError::decode(format!("encode existing replica: {error}")))?
                            >= serde_json::to_string(&encoded)
                                .map_err(|error| ResourceError::decode(format!("encode incoming replica: {error}")))?
                    }
                    (None, Some(existing_synced_at)) if existing_synced_at == &synced_at => true,
                    _ => false,
                }
            }
            StoredReplicaEventKind::Deleted => match partition.synced_at_by_name.get(&object.metadata.name) {
                Some(existing_synced_at) if existing_synced_at > &synced_at => true,
                Some(existing_synced_at) if existing_synced_at == &synced_at => !partition.objects.contains_key(&object.metadata.name),
                _ => false,
            },
        };
        if unchanged {
            return Ok(());
        }
        match kind {
            StoredReplicaEventKind::Added | StoredReplicaEventKind::Modified => {
                partition.queries.update(&object.metadata.name, Some(&encoded));
                partition.messages.update(&object.metadata.name, Some(&encoded));
                partition.objects.insert(object.metadata.name.clone(), encoded.clone());
                partition.digest.set(&object.metadata.name, Some(&object.metadata.resource_version));
                partition.synced_at_by_name.insert(object.metadata.name.clone(), synced_at);
            }
            StoredReplicaEventKind::Deleted => {
                partition.queries.update(&object.metadata.name, None);
                partition.messages.update(&object.metadata.name, None);
                partition.objects.remove(&object.metadata.name);
                partition.digest.set(&object.metadata.name, None);
                partition.synced_at_by_name.insert(object.metadata.name.clone(), synced_at);
            }
        }
        Self::notify_replica_watchers(
            &mut state,
            &store_key,
            StoredReplicaEvent { origin_root: origin_root.clone(), synced_at, kind, object: encoded },
        );
        Ok(())
    }

    pub async fn apply_replica_tombstone_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        tombstone: &ResourceTombstone,
        synced_at: chrono::DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.apply_replica_tombstone_raw(resource_kind, origin_root, namespace, tombstone, synced_at).await;
        }
        let store_key = Self::store_key(resource_kind, namespace);
        let replica_key = (origin_root.clone(), store_key.clone());
        let encoded = Self::encode_tombstone(resource_kind, tombstone);
        let mut state = self.replicas.lock().await;
        let partition = state.partitions.entry(replica_key).or_default();
        let unchanged = match partition.synced_at_by_name.get(&tombstone.name) {
            Some(existing_synced_at) if existing_synced_at > &synced_at => true,
            Some(existing_synced_at) if existing_synced_at == &synced_at => !partition.objects.contains_key(&tombstone.name),
            _ => false,
        };
        if unchanged {
            return Ok(());
        }
        partition.queries.update(&tombstone.name, None);
        partition.messages.update(&tombstone.name, None);
        partition.objects.remove(&tombstone.name);
        partition.digest.set(&tombstone.name, None);
        partition.synced_at_by_name.insert(tombstone.name.clone(), synced_at);
        Self::notify_replica_watchers(
            &mut state,
            &store_key,
            StoredReplicaEvent { origin_root: origin_root.clone(), synced_at, kind: StoredReplicaEventKind::Deleted, object: encoded },
        );
        Ok(())
    }

    pub async fn invalidate_replica_cursor_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
    ) -> Result<(), ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.invalidate_replica_cursor_raw(resource_kind, origin_root, namespace).await;
        }
        let key = (origin_root.clone(), Self::store_key(resource_kind, namespace));
        if let Some(partition) = self.replicas.lock().await.partitions.get_mut(&key) {
            partition.cursor = None;
        }
        Ok(())
    }

    pub async fn advance_replica_cursor_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        previous: &ReplicaCursor,
        next: &str,
    ) -> Result<(), ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.advance_replica_cursor_raw(resource_kind, origin_root, namespace, previous, next).await;
        }
        let key = (origin_root.clone(), Self::store_key(resource_kind, namespace));
        let mut replicas = self.replicas.lock().await;
        let partition = replicas.partitions.get_mut(&key).ok_or_else(|| ResourceError::invalid("replica prefix missing"))?;
        if partition.cursor.as_ref() != Some(previous) {
            return Err(ResourceError::invalid("replica prefix changed concurrently"));
        }
        partition.cursor = Some(ReplicaCursor { resource_version: next.to_string(), generation: previous.generation.clone() });
        Ok(())
    }

    pub async fn replica_cursor_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
    ) -> Result<Option<ReplicaCursor>, ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.replica_cursor_raw(resource_kind, origin_root, namespace).await;
        }
        let key = (origin_root.clone(), Self::store_key(resource_kind, namespace));
        Ok(self.replicas.lock().await.partitions.get(&key).and_then(|partition| partition.cursor.clone()))
    }

    fn encode_tombstone(resource_kind: ResourceKind, tombstone: &ResourceTombstone) -> Value {
        serde_json::json!({
            "apiVersion": format!("{}/{}", resource_kind.paths.group, resource_kind.paths.version),
            "kind": resource_kind.paths.kind,
            "metadata": {
                "name": tombstone.name,
                "namespace": tombstone.namespace,
                "resourceVersion": tombstone.resource_version,
                "annotations": tombstone.annotations,
            },
        })
    }

    pub async fn get_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.with_store(resource_kind, namespace, |store| {
            let value = store.objects.get(name).cloned().ok_or_else(|| ResourceError::not_found(name))?;
            self.count_read(resource_kind, 1);
            Self::decode_object(resource_kind, value)
        })
        .await
    }

    pub async fn current_position_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<crate::ResourcePosition, ResourceError> {
        self.with_store(resource_kind, namespace, |store| {
            Ok(crate::ResourcePosition { resource_version: store.current_version().to_string(), generation: self.generation.clone() })
        })
        .await
    }

    pub async fn digest_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        query: &crate::DigestQuery,
    ) -> Result<crate::PartitionDigest, ResourceError> {
        self.with_store(resource_kind, namespace, |store| {
            let mut response = resource_kind
                .partition_digest(
                    self.local_root(),
                    namespace,
                    self.generation.clone(),
                    store.current_version().to_string(),
                    store.digest.hashes(),
                )
                .select(query)?;
            if let crate::DigestQuery::Snapshot { bucket, .. } = query {
                let items = store
                    .digest
                    .names(*bucket)
                    .map(|name| {
                        let value = store.objects.get(name).ok_or_else(|| ResourceError::invalid("incomplete digest index"))?;
                        Self::decode_object(resource_kind, value.clone())?;
                        Ok(value.clone())
                    })
                    .collect::<Result<Vec<_>, ResourceError>>()?;
                response.bucket = Some(*bucket);
                response.items = Some(items);
            }
            Ok(response)
        })
        .await
    }

    pub async fn confirm_replica_digest_raw(
        &self,
        resource_kind: ResourceKind,
        origin: &NodeId,
        namespace: &str,
        proof: &crate::PartitionDigest,
        previous: &ReplicaCursor,
    ) -> Result<(), ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.confirm_replica_digest_raw(resource_kind, origin, namespace, proof, previous).await;
        }
        let mut replicas = self.replicas.lock().await;
        let partition = replicas
            .partitions
            .get_mut(&(origin.clone(), Self::store_key(resource_kind, namespace)))
            .ok_or_else(|| ResourceError::invalid("missing replica prefix"))?;
        let digest = resource_kind.partition_digest(
            origin.clone(),
            namespace,
            previous.generation.clone(),
            String::new(),
            partition.digest.hashes(),
        );
        if digest.root != proof.root || partition.cursor.as_ref() != Some(previous) {
            return Err(ResourceError::invalid("replica set changed during digest repair"));
        }
        partition.cursor = Some(ReplicaCursor { resource_version: proof.resource_version.clone(), generation: proof.generation.clone() });
        Ok(())
    }

    pub async fn replica_digest_raw(
        &self,
        resource_kind: ResourceKind,
        origin: &NodeId,
        namespace: &str,
        generation: Option<String>,
    ) -> Result<crate::PartitionDigest, ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.replica_digest_raw(resource_kind, origin, namespace, generation).await;
        }
        let replicas = self.replicas.lock().await;
        let empty = ReplicaPartition::default();
        let partition = replicas.partitions.get(&(origin.clone(), Self::store_key(resource_kind, namespace))).unwrap_or(&empty);
        Ok(resource_kind.partition_digest(origin.clone(), namespace, generation, String::new(), partition.digest.hashes()))
    }

    pub async fn list_raw(&self, resource_kind: ResourceKind, namespace: &str) -> Result<ResourceList<SerializedResource>, ResourceError> {
        self.with_store(resource_kind, namespace, |store| {
            self.count_read(resource_kind, store.objects.len());
            let mut items = Vec::with_capacity(store.objects.len());
            for value in store.objects.values().cloned() {
                items.push(Self::decode_object(resource_kind, value)?);
            }
            items.sort_by(|left, right| left.metadata.name.cmp(&right.metadata.name));
            Ok(ResourceList { items, resource_version: store.current_version().to_string(), generation: self.generation.clone() })
        })
        .await
    }

    pub async fn list_raw_matching_labels(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ResourceList<SerializedResource>, ResourceError> {
        if required.is_empty() {
            return self.list_raw(resource_kind, namespace).await;
        }

        self.with_store(resource_kind, namespace, |store| {
            let names = store.queries.names(&crate::StoreQuery::labels(required, false, true));
            self.count_read(resource_kind, names.len());
            let items =
                names.iter().map(|name| Self::decode_object(resource_kind, store.objects[name].clone())).collect::<Result<Vec<_>, _>>()?;
            Ok(ResourceList { items, resource_version: store.current_version().to_string(), generation: self.generation.clone() })
        })
        .await
    }

    pub async fn create_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.create_typed_with_merge(resource_kind, namespace, meta, spec, None).await
    }

    pub async fn create_definition_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
        merge: MergeMetadata,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.create_typed_with_merge(resource_kind, namespace, meta, spec, Some(merge)).await
    }

    fn validate_namespace_spec(
        resource_kind: ResourceKind,
        store: &ResourceStore,
        meta: &InputMeta,
        spec: &Value,
    ) -> Result<(), ResourceError> {
        if !resource_kind.validate_namespace {
            return Ok(());
        }
        let siblings = store
            .objects
            .values()
            .cloned()
            .map(|value| resource_kind.decode_object(value))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|object| object.metadata.name != meta.name && object.metadata.deletion_timestamp.is_none())
            .collect::<Vec<_>>();
        resource_kind.validate_spec_with_named_siblings(meta, spec, &siblings)
    }

    async fn create_typed_with_merge(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
        merge: Option<MergeMetadata>,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        resource_kind.validate_spec(meta, spec)?;
        self.with_store_mut(resource_kind, namespace, |store| {
            if store.objects.contains_key(&meta.name) {
                return Err(ResourceError::conflict(&meta.name, "resource already exists"));
            }

            Self::validate_namespace_spec(resource_kind, store, meta, spec)?;
            let version = store.allocate_version();
            let object = ResourceObject::<SerializedResource> {
                metadata: ObjectMeta {
                    name: meta.name.clone(),
                    namespace: namespace.to_string(),
                    resource_version: version.to_string(),
                    labels: meta.labels.clone(),
                    annotations: meta.annotations.clone(),
                    owner_references: meta.owner_references.clone(),
                    finalizers: meta.finalizers.clone(),
                    deletion_timestamp: meta.deletion_timestamp,
                    creation_timestamp: Utc::now(),
                    merge,
                },
                spec: Self::clone_through_serde(spec)?,
                status: None,
            };

            let encoded = Self::encode_object(resource_kind, &object)?;
            store.tombstones.remove(&meta.name);
            store.objects.insert(meta.name.clone(), encoded.clone());
            store.push_event(
                StoredEvent { version, kind: StoredEventKind::Added, object: encoded },
                resource_kind.replication.event_retention(self.event_retention.max_events_per_resource_stream()),
            );
            Ok(object)
        })
        .await
    }

    pub async fn update_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.update_typed_with_merge(resource_kind, namespace, meta, resource_version, spec, None).await
    }

    pub async fn update_definition_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
        merge: MergeMetadata,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.update_typed_with_merge(resource_kind, namespace, meta, resource_version, spec, Some(merge)).await
    }

    async fn update_typed_with_merge(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
        admitted_merge: Option<MergeMetadata>,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.with_store_mut(resource_kind, namespace, |store| {
            let existing = store.objects.get(&meta.name).cloned().ok_or_else(|| ResourceError::not_found(&meta.name))?;
            let mut object = Self::decode_object(resource_kind, existing)?;
            if object.metadata.resource_version != resource_version {
                return Err(ResourceError::conflict(&meta.name, "stale resourceVersion"));
            }
            resource_kind.validate_spec_update(&object.spec, spec)?;
            resource_kind.validate_spec(meta, spec)?;
            Self::validate_namespace_spec(resource_kind, store, meta, spec)?;
            if object.matches_update(meta, spec)?
                && admitted_merge.as_ref().is_none_or(|merge| object.metadata.merge.as_ref() == Some(merge))
            {
                return Ok(object);
            }

            let version = store.allocate_version();
            object.metadata.resource_version = version.to_string();
            object.metadata.labels = meta.labels.clone();
            object.metadata.annotations = meta.annotations.clone();
            object.metadata.owner_references = meta.owner_references.clone();
            object.metadata.finalizers = meta.finalizers.clone();
            object.metadata.deletion_timestamp = meta.deletion_timestamp;
            if let Some(merge) = admitted_merge {
                object.metadata.merge = Some(merge);
            }
            object.spec = Self::clone_through_serde(spec)?;

            let encoded = Self::encode_object(resource_kind, &object)?;
            if resource_kind.replication != crate::ReplicationClass::Definitions
                && object.metadata.deletion_timestamp.is_some()
                && object.metadata.finalizers.is_empty()
            {
                store.objects.remove(&meta.name);
                store.push_event(
                    StoredEvent { version, kind: StoredEventKind::Deleted, object: encoded },
                    resource_kind.replication.event_retention(self.event_retention.max_events_per_resource_stream()),
                );
            } else {
                store.objects.insert(meta.name.clone(), encoded.clone());
                store.push_event(
                    StoredEvent { version, kind: StoredEventKind::Modified, object: encoded },
                    resource_kind.replication.event_retention(self.event_retention.max_events_per_resource_stream()),
                );
            }
            Ok(object)
        })
        .await
    }

    pub async fn update_status_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.with_store_mut(resource_kind, namespace, |store| {
            let existing = store.objects.get(name).cloned().ok_or_else(|| ResourceError::not_found(name))?;
            let mut object = Self::decode_object(resource_kind, existing)?;
            if object.metadata.resource_version != resource_version {
                return Err(ResourceError::conflict(name, "stale resourceVersion"));
            }
            resource_kind.validate_status_update(object.status.as_ref(), status)?;
            if object.matches_status(status)? {
                return Ok(object);
            }

            let version = store.allocate_version();
            object.metadata.resource_version = version.to_string();
            object.status = Some(Self::clone_through_serde(status)?);

            let encoded = Self::encode_object(resource_kind, &object)?;
            store.objects.insert(name.to_string(), encoded.clone());
            store.push_event(
                StoredEvent { version, kind: StoredEventKind::Modified, object: encoded },
                resource_kind.replication.event_retention(self.event_retention.max_events_per_resource_stream()),
            );
            Ok(object)
        })
        .await
    }

    pub async fn delete_raw(&self, resource_kind: ResourceKind, namespace: &str, name: &str) -> Result<(), ResourceError> {
        self.with_store_mut(resource_kind, namespace, |store| {
            let existing = store.objects.get(name).cloned().ok_or_else(|| ResourceError::not_found(name))?;
            let mut object = Self::decode_object(resource_kind, existing)?;
            if object.metadata.is_pending_finalization() {
                return Ok(());
            }
            let version = store.allocate_version();
            object.metadata.resource_version = version.to_string();
            if !object.metadata.finalizers.is_empty() && object.metadata.deletion_timestamp.is_none() {
                object.metadata.deletion_timestamp = Some(Utc::now());
                let encoded = Self::encode_object(resource_kind, &object)?;
                store.objects.insert(name.to_string(), encoded.clone());
                store.push_event(
                    StoredEvent { version, kind: StoredEventKind::Modified, object: encoded },
                    resource_kind.replication.event_retention(self.event_retention.max_events_per_resource_stream()),
                );
                return Ok(());
            }

            let encoded = Self::encode_object(resource_kind, &object)?;
            store.objects.remove(name);
            store.push_event(
                StoredEvent { version, kind: StoredEventKind::Deleted, object: encoded },
                resource_kind.replication.event_retention(self.event_retention.max_events_per_resource_stream()),
            );
            Ok(())
        })
        .await
    }

    pub async fn tombstone_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        minimum_resource_version: Option<&str>,
    ) -> Result<crate::watch::TombstoneWrite, ResourceError> {
        let minimum_resource_version = minimum_resource_version
            .map(|version| {
                version.parse::<u64>().map_err(|error| {
                    ResourceError::invalid(format!("invalid replica cursor resourceVersion '{version}' while tombstoning: {error}"))
                })
            })
            .transpose()?;
        self.with_store_mut(resource_kind, namespace, |store| {
            if store.objects.contains_key(name) {
                return Err(ResourceError::conflict(name, "cannot tombstone an existing resource by name"));
            }
            if let Some(tombstone) = store.tombstones.get(name) {
                return Ok(crate::watch::TombstoneWrite { tombstone: tombstone.clone(), created: false });
            }
            if let Some(minimum_resource_version) = minimum_resource_version {
                store.next_version = store.next_version.max(minimum_resource_version.saturating_add(1));
            }
            let version = store.allocate_version();
            let tombstone = ResourceTombstone {
                name: name.to_string(),
                namespace: namespace.to_string(),
                resource_version: version.to_string(),
                annotations: BTreeMap::new(),
            };
            store.push_event(
                StoredEvent { version, kind: StoredEventKind::Deleted, object: Self::encode_tombstone(resource_kind, &tombstone) },
                resource_kind.replication.event_retention(self.event_retention.max_events_per_resource_stream()),
            );
            store.tombstones.insert(name.to_string(), tombstone.clone());
            Ok(crate::watch::TombstoneWrite { tombstone, created: true })
        })
        .await
    }

    pub async fn watch_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        start: WatchStart,
    ) -> Result<SerializedWatchStream, ResourceError> {
        let generation = self.generation.clone();
        let (replay, receiver) = {
            let mut stores = self.stores.lock().await;
            let store = stores.entry(Self::store_key(resource_kind, namespace)).or_default();
            let replay_from = match &start {
                WatchStart::Now => None,
                WatchStart::FromVersion(version) => {
                    if generation.is_some() {
                        return Err(ResourceError::invalid("generational in-memory watches require a generation"));
                    }
                    Some(
                        version
                            .parse::<u64>()
                            .map_err(|err| ResourceError::invalid(format!("invalid resourceVersion '{version}': {err}")))?,
                    )
                }
                WatchStart::FromVersionInGeneration { generation: requested_generation, resource_version } => {
                    let Some(current_generation) = &generation else {
                        return Err(ResourceError::invalid("in-memory resource watches are not generational"));
                    };
                    if requested_generation != current_generation {
                        return Err(ResourceError::invalid(format!(
                            "resourceVersion belongs to generation '{requested_generation}', current generation is '{current_generation}'"
                        )));
                    }
                    Some(
                        resource_version
                            .parse::<u64>()
                            .map_err(|err| ResourceError::invalid(format!("invalid resourceVersion '{resource_version}': {err}")))?,
                    )
                }
            };
            if replay_from.is_some_and(|version| version < store.compacted_through) {
                return Err(ResourceError::WatchExpired {
                    requested_version: replay_from.expect("checked replay version").to_string(),
                    compacted_through: Some(store.compacted_through.to_string()),
                });
            }
            if replay_from.is_some_and(|version| version > store.current_version()) {
                return Err(ResourceError::invalid(format!(
                    "resourceVersion {} is ahead of current version {}",
                    replay_from.expect("checked replay version"),
                    store.current_version()
                )));
            }
            let replay = match replay_from {
                Some(version) => store.event_log.iter().filter(|event| event.version > version).cloned().collect(),
                None => Vec::new(),
            };
            let receiver = store.watchers.subscribe(resource_kind.paths.kind, namespace);
            (replay, receiver)
        };

        let replay_stream = stream::iter(
            replay
                .into_iter()
                .map(move |event| Ok(SerializedWatchEvent { action: event.kind.into(), payload: WatchPayload::Owned(event.object) })),
        );
        let live_stream = stream::unfold(receiver, move |mut receiver| async move {
            receiver.next().await.map(|event| {
                (event.map(|event| SerializedWatchEvent { action: event.kind.into(), payload: WatchPayload::Shared(event) }), receiver)
            })
        });
        Ok(SerializedWatchStream::new(generation, Box::pin(replay_stream.chain(live_stream))))
    }
}

#[async_trait::async_trait]
impl Store for InMemoryBackend {
    fn with_local_root(&self, root: NodeId) -> std::sync::Arc<dyn Store> {
        std::sync::Arc::new(self.clone().with_local_root(root))
    }
    fn local_root(&self) -> Result<NodeId, ResourceError> {
        Ok(self.local_root())
    }
    fn hierarchy_admission(&self) -> Option<std::sync::Arc<tokio::sync::Mutex<()>>> {
        Some(self.hierarchy_admission.clone())
    }
    fn with_read_observer(&self, observer: std::sync::Arc<dyn ReadObserver>) -> std::sync::Arc<dyn Store> {
        std::sync::Arc::new(self.clone().with_read_observer(observer))
    }
    fn read_observer(&self) -> Option<&dyn ReadObserver> {
        self.read_observer()
    }
    async fn diagnostics(&self) -> Result<Option<ResourceStoreDiagnostics>, ResourceError> {
        self.diagnostics().await.map(Some)
    }
    async fn record_field_ownership_violation(&self, violation: FieldOwnershipViolation) -> Result<(), ResourceError> {
        self.record_field_ownership_violation(violation).await;
        Ok(())
    }
    async fn query_raw(&self, kind: ResourceKind, namespace: &str, query: &crate::StoreQuery) -> Result<crate::QueryResult, ResourceError> {
        query.validate(kind)?;
        let stores = self.stores.lock().await;
        let key = Self::store_key(kind, namespace);
        let local = stores.get(&key);
        let mut items = Vec::new();
        let mut hydration_names = local.map(|store| store.queries.names(query)).unwrap_or_default();
        if query.all_sources_for_matches {
            if let Some(durable) = &self.durable_replicas {
                let mut selection = query.clone();
                selection.replicas_only = true;
                selection.any_of = Some(vec![
                    crate::StoreQuery { all_sources_for_matches: false, ..query.clone() },
                    crate::StoreQuery {
                        field_values: BTreeMap::from([("metadata.name".into(), hydration_names.iter().cloned().collect())]),
                        ..Default::default()
                    },
                ]);
                selection.labels.clear();
                selection.fields.clear();
                selection.field_values.clear();
                selection.present.clear();
                selection.field_prefixes.clear();
                selection.label_prefixes.clear();
                selection.owner = None;
                items = durable.store().query_raw(kind, namespace, &selection).await?.items;
                hydration_names.extend(items.iter().map(|source| source.object.metadata.name.clone()));
            } else {
                let replicas = self.replicas.lock().await;
                for ((_, partition_key), partition) in &replicas.partitions {
                    if partition_key == &key {
                        hydration_names.extend(partition.queries.names(query));
                    }
                }
            }
        }
        if let Some(store) = local.filter(|_| !query.replicas_only) {
            let mut names = if query.all_sources_for_matches {
                hydration_names.intersection(&store.objects.keys().cloned().collect()).cloned().collect()
            } else {
                store.queries.names(query)
            };
            if let Some(message) = &query.message {
                let message_names = store.messages.names(message).into_iter().collect();
                names = names.intersection(&message_names).cloned().collect();
            }
            for name in names {
                items.push(ReadResourceObject {
                    object: kind.decode_object(
                        store
                            .objects
                            .get(&name)
                            .ok_or_else(|| ResourceError::other("query index references missing local object"))?
                            .clone(),
                    )?,
                    provenance: ResourceProvenance::Local,
                });
            }
        }
        if query.include_replicas && kind.replication != crate::ReplicationClass::None {
            if let Some(durable) = &self.durable_replicas {
                // The composing memory store defines shadowing, not the durable
                // store's unrelated local rows. Pass names before any decoding.
                let mut replica_query = query.clone();
                replica_query.replicas_only = true;
                replica_query.replica_shadowing = Some(query.replica_shadowing.clone().unwrap_or_else(|| crate::ReplicaShadowing {
                    origin_root: self.local_root(),
                    local_names: local.map(|store| store.objects.keys().cloned().collect()).unwrap_or_default(),
                }));
                if !query.all_sources_for_matches {
                    items.extend(durable.store().query_raw(kind, namespace, &replica_query).await?.items);
                }
            } else {
                let replicas = self.replicas.lock().await;
                for ((origin, partition_key), partition) in &replicas.partitions {
                    if partition_key != &key {
                        continue;
                    }
                    let mut names = if query.all_sources_for_matches {
                        hydration_names.intersection(&partition.objects.keys().cloned().collect()).cloned().collect()
                    } else {
                        partition.queries.names(query)
                    };
                    if let Some(message) = &query.message {
                        let message_names = partition.messages.names(message).into_iter().collect();
                        names = names.intersection(&message_names).cloned().collect();
                    }
                    for name in names {
                        let shadowed = if let Some(scope) = &query.replica_shadowing {
                            origin == &scope.origin_root && scope.local_names.contains(&name)
                        } else {
                            origin == &self.local_root() && local.is_some_and(|store| store.objects.contains_key(&name))
                        };
                        if query.suppress_self_origin && shadowed {
                            continue;
                        }
                        items.push(ReadResourceObject {
                            object: kind.decode_object(
                                partition
                                    .objects
                                    .get(&name)
                                    .ok_or_else(|| ResourceError::other("query index references missing replica object"))?
                                    .clone(),
                            )?,
                            provenance: ResourceProvenance::Replica {
                                origin_root: origin.clone(),
                                last_synced_at: *partition
                                    .synced_at_by_name
                                    .get(&name)
                                    .ok_or_else(|| ResourceError::other("replica query object has no sync timestamp"))?,
                            },
                        });
                    }
                }
            }
        }
        self.count_read(kind, items.len());
        crate::sort_query_items(&mut items);
        Ok(crate::QueryResult {
            local_root: Some(self.local_root()),
            items,
            position: crate::ResourcePosition {
                resource_version: local.map(|store| store.current_version()).unwrap_or_default().to_string(),
                generation: self.generation.clone(),
            },
        })
    }
    async fn query_messages_raw(
        &self,
        namespace: &str,
        query: &MessageQuery,
        include_replicas: bool,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        self.query_messages(namespace, query, include_replicas).await?.into_iter().map(convert_read).collect()
    }
    async fn delete_decode_quarantine_raw(&self, resource_kind: ResourceKind, namespace: &str, name: &str) -> Result<bool, ResourceError> {
        InMemoryBackend::delete_decode_quarantine_raw(self, resource_kind, namespace, name).await
    }
    async fn local_namespaces_raw(&self, resource_kind: ResourceKind) -> Result<Vec<String>, ResourceError> {
        InMemoryBackend::local_namespaces_raw(self, resource_kind).await
    }
    async fn stored_namespaces_raw(&self, resource_kind: ResourceKind) -> Result<Vec<String>, ResourceError> {
        InMemoryBackend::stored_namespaces_raw(self, resource_kind).await
    }
    async fn list_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        InMemoryBackend::list_replicas_raw(self, resource_kind, namespace).await
    }
    async fn get_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        InMemoryBackend::get_replicas_raw(self, resource_kind, namespace, name).await
    }
    async fn watch_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<SerializedReadWatchEvent, ResourceError>>, ResourceError> {
        InMemoryBackend::watch_replicas_raw(self, resource_kind, namespace).await
    }
    async fn replace_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        listed: &ResourceList<SerializedResource>,
        synced_at: chrono::DateTime<Utc>,
        bucket: Option<u8>,
    ) -> Result<(), ResourceError> {
        InMemoryBackend::replace_replicas_raw(self, resource_kind, origin_root, namespace, listed, synced_at, bucket).await
    }
    async fn apply_replica_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        kind: StoredReplicaEventKind,
        object: &ResourceObject<SerializedResource>,
        synced_at: chrono::DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        InMemoryBackend::apply_replica_raw(self, resource_kind, origin_root, namespace, kind, object, synced_at).await
    }
    async fn apply_replica_tombstone_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        tombstone: &ResourceTombstone,
        synced_at: chrono::DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        InMemoryBackend::apply_replica_tombstone_raw(self, resource_kind, origin_root, namespace, tombstone, synced_at).await
    }
    async fn invalidate_replica_cursor_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
    ) -> Result<(), ResourceError> {
        InMemoryBackend::invalidate_replica_cursor_raw(self, resource_kind, origin_root, namespace).await
    }
    async fn advance_replica_cursor_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        previous: &ReplicaCursor,
        next: &str,
    ) -> Result<(), ResourceError> {
        InMemoryBackend::advance_replica_cursor_raw(self, resource_kind, origin_root, namespace, previous, next).await
    }
    async fn replica_cursor_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
    ) -> Result<Option<ReplicaCursor>, ResourceError> {
        InMemoryBackend::replica_cursor_raw(self, resource_kind, origin_root, namespace).await
    }
    async fn get_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        InMemoryBackend::get_raw(self, resource_kind, namespace, name).await
    }
    async fn current_position_raw(&self, resource_kind: ResourceKind, namespace: &str) -> Result<ResourcePosition, ResourceError> {
        InMemoryBackend::current_position_raw(self, resource_kind, namespace).await
    }
    async fn digest_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        query: &DigestQuery,
    ) -> Result<PartitionDigest, ResourceError> {
        InMemoryBackend::digest_raw(self, resource_kind, namespace, query).await
    }
    async fn confirm_replica_digest_raw(
        &self,
        resource_kind: ResourceKind,
        origin: &NodeId,
        namespace: &str,
        proof: &PartitionDigest,
        previous: &ReplicaCursor,
    ) -> Result<(), ResourceError> {
        InMemoryBackend::confirm_replica_digest_raw(self, resource_kind, origin, namespace, proof, previous).await
    }
    async fn replica_digest_raw(
        &self,
        resource_kind: ResourceKind,
        origin: &NodeId,
        namespace: &str,
        generation: Option<String>,
    ) -> Result<PartitionDigest, ResourceError> {
        InMemoryBackend::replica_digest_raw(self, resource_kind, origin, namespace, generation).await
    }
    async fn list_raw(&self, resource_kind: ResourceKind, namespace: &str) -> Result<ResourceList<SerializedResource>, ResourceError> {
        InMemoryBackend::list_raw(self, resource_kind, namespace).await
    }
    async fn list_raw_matching_labels(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ResourceList<SerializedResource>, ResourceError> {
        InMemoryBackend::list_raw_matching_labels(self, resource_kind, namespace, required).await
    }
    async fn create_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        InMemoryBackend::create_raw(self, resource_kind, namespace, meta, spec).await
    }
    async fn create_definition_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
        merge: MergeMetadata,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        InMemoryBackend::create_definition_raw(self, resource_kind, namespace, meta, spec, merge).await
    }
    async fn update_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        InMemoryBackend::update_raw(self, resource_kind, namespace, meta, resource_version, spec).await
    }
    async fn update_definition_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
        merge: MergeMetadata,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        InMemoryBackend::update_definition_raw(self, resource_kind, namespace, meta, resource_version, spec, merge).await
    }
    async fn update_status_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        InMemoryBackend::update_status_raw(self, resource_kind, namespace, name, resource_version, status).await
    }
    async fn delete_raw(&self, resource_kind: ResourceKind, namespace: &str, name: &str) -> Result<(), ResourceError> {
        InMemoryBackend::delete_raw(self, resource_kind, namespace, name).await
    }
    async fn tombstone_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        minimum_resource_version: Option<&str>,
    ) -> Result<watch::TombstoneWrite, ResourceError> {
        InMemoryBackend::tombstone_raw(self, resource_kind, namespace, name, minimum_resource_version).await
    }
    async fn watch_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        start: WatchStart,
    ) -> Result<SerializedWatchStream, ResourceError> {
        InMemoryBackend::watch_raw(self, resource_kind, namespace, start).await
    }
}

impl WatchObject for StoredEvent {
    fn object(&self) -> &Value {
        &self.object
    }
}
impl From<StoredEventKind> for WatchAction {
    fn from(kind: StoredEventKind) -> Self {
        match kind {
            StoredEventKind::Added => Self::Added,
            StoredEventKind::Modified => Self::Modified,
            StoredEventKind::Deleted => Self::Deleted,
        }
    }
}
