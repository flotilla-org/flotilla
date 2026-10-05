use std::{
    borrow::Borrow,
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use chrono::Utc;
use flotilla_protocol::NodeId;
use futures::{stream, StreamExt};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{
    error::ResourceError,
    field_ownership::FieldOwnershipViolation,
    replica::{ReadResourceObject, ReadWatchEvent, ReplicaCursor, ResourceProvenance, StoredReplicaEvent, StoredReplicaEventKind},
    resource::{InputMeta, K8sResourceObject, MergeMetadata, ObjectMeta, Resource, ResourceObject},
    retention::{EventRetention, ResourceStoreDiagnostics, FIELD_OWNERSHIP_VIOLATION_TTL_HOURS, MAX_FIELD_OWNERSHIP_VIOLATIONS},
    watch::{
        decode_watch_object, decode_watch_tombstone, ResourceList, ResourceTombstone, WatchChannel, WatchEvent, WatchStart, WatchStream,
    },
};

type StoreKey = (String, String, String, String);

#[derive(Debug, Clone, Default, bon::Builder)]
#[builder(builder_type(vis = "pub(in crate::in_memory)"))]
pub struct InMemoryBackend {
    stores: Arc<Mutex<HashMap<StoreKey, ResourceStore>>>,
    replicas: Arc<Mutex<ReplicaState>>,
    durable_replicas: Option<crate::SqliteBackend>,
    generation: Option<String>,
    event_retention: EventRetention,
    local_root: Option<NodeId>,
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
    objects: HashMap<String, Value>,
    // Retained for deleted names as a tombstone so an older relayed write
    // cannot resurrect an object after a newer delete.
    synced_at_by_name: HashMap<String, chrono::DateTime<Utc>>,
    cursor: Option<ReplicaCursor>,
}

#[derive(Debug)]
struct ResourceStore {
    objects: HashMap<String, Value>,
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
            objects: HashMap::new(),
            tombstones: HashMap::new(),
            next_version: 1,
            watchers: WatchChannel::default(),
            event_log: Vec::new(),
            compacted_through: 0,
        }
    }
}

impl InMemoryBackend {
    pub fn observed() -> Self {
        Self {
            stores: Arc::default(),
            replicas: Arc::default(),
            durable_replicas: None,
            generation: Some(uuid::Uuid::new_v4().to_string()),
            event_retention: EventRetention::default(),
            local_root: None,
            ownership_violations: Arc::default(),
        }
    }

    /// Local observations have a new generation per process, while received
    /// facts remain durable across holder restarts (ADR 0016). Only the replica
    /// operations use this store; local writes always remain in memory.
    pub fn observed_with_durable_replicas(replicas: crate::SqliteBackend) -> Self {
        Self { durable_replicas: Some(replicas), ..Self::observed() }
    }

    pub(crate) async fn delete_decode_quarantine_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<bool, ResourceError> {
        match &self.durable_replicas {
            Some(backend) => backend.delete_decode_quarantine_typed::<T>(namespace, name).await,
            None => Ok(false),
        }
    }

    pub fn with_event_retention(event_retention: EventRetention) -> Self {
        Self {
            stores: Arc::default(),
            replicas: Arc::default(),
            durable_replicas: None,
            generation: None,
            event_retention,
            local_root: None,
            ownership_violations: Arc::default(),
        }
    }

    pub fn observed_with_event_retention(event_retention: EventRetention) -> Self {
        Self {
            stores: Arc::default(),
            replicas: Arc::default(),
            durable_replicas: None,
            generation: Some(uuid::Uuid::new_v4().to_string()),
            event_retention,
            local_root: None,
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

    pub(crate) async fn diagnostics(&self) -> Result<ResourceStoreDiagnostics, ResourceError> {
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

    pub(crate) async fn record_field_ownership_violation(&self, violation: FieldOwnershipViolation) {
        let mut violations = self.ownership_violations.lock().await;
        violations.push(violation);
        let excess = violations.len().saturating_sub(MAX_FIELD_OWNERSHIP_VIOLATIONS);
        if excess > 0 {
            violations.drain(..excess);
        }
    }

    fn store_key<T: Resource>(namespace: &str) -> StoreKey {
        (T::API_PATHS.group.to_string(), T::API_PATHS.version.to_string(), T::API_PATHS.plural.to_string(), namespace.to_string())
    }

    pub(crate) async fn local_namespaces_typed<T: Resource>(&self) -> Result<Vec<String>, ResourceError> {
        let stores = self.stores.lock().await;
        let mut namespaces = stores
            .iter()
            .filter(|(key, store)| {
                key.0 == T::API_PATHS.group && key.1 == T::API_PATHS.version && key.2 == T::API_PATHS.plural && !store.objects.is_empty()
            })
            .map(|(key, _)| key.3.clone())
            .collect::<Vec<_>>();
        namespaces.sort();
        Ok(namespaces)
    }

    pub(crate) async fn stored_namespaces_typed<T: Resource>(&self) -> Result<Vec<String>, ResourceError> {
        let mut namespaces = self.local_namespaces_typed::<T>().await?.into_iter().collect::<std::collections::BTreeSet<_>>();
        if let Some(backend) = &self.durable_replicas {
            namespaces.extend(backend.stored_namespaces_typed::<T>().await?);
            return Ok(namespaces.into_iter().collect());
        }
        let replicas = self.replicas.lock().await;
        for ((_, key), partition) in &replicas.partitions {
            if key.0 == T::API_PATHS.group && key.1 == T::API_PATHS.version && key.2 == T::API_PATHS.plural && !partition.objects.is_empty()
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

    async fn with_store_mut<T: Resource, R>(
        &self,
        namespace: &str,
        f: impl FnOnce(&mut ResourceStore) -> Result<R, ResourceError>,
    ) -> Result<R, ResourceError> {
        let mut stores = self.stores.lock().await;
        let store = stores.entry(Self::store_key::<T>(namespace)).or_default();
        f(store)
    }

    async fn with_store<T: Resource, R>(
        &self,
        namespace: &str,
        f: impl FnOnce(&ResourceStore) -> Result<R, ResourceError>,
    ) -> Result<R, ResourceError> {
        let stores = self.stores.lock().await;
        let empty = ResourceStore::default();
        let store = stores.get(&Self::store_key::<T>(namespace)).unwrap_or(&empty);
        f(store)
    }

    fn decode_object<T: Resource>(value: Value) -> Result<ResourceObject<T>, ResourceError> {
        let object: K8sResourceObject<T> =
            serde_json::from_value(value).map_err(|err| ResourceError::decode(format!("decode stored object: {err}")))?;
        ResourceObject::from_k8s_object(object)
    }

    fn encode_object<T: Resource>(object: &ResourceObject<T>) -> Result<Value, ResourceError> {
        serde_json::to_value(object.to_k8s_object()).map_err(|err| ResourceError::decode(format!("encode object: {err}")))
    }

    fn notify_replica_watchers(state: &mut ReplicaState, key: &StoreKey, event: StoredReplicaEvent) {
        if let Some(watchers) = state.watchers.get_mut(key) {
            watchers.send(event);
        }
    }

    pub(crate) async fn list_replicas_typed<T: Resource>(&self, namespace: &str) -> Result<Vec<ReadResourceObject<T>>, ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.list_replicas_typed::<T>(namespace).await;
        }
        let key = Self::store_key::<T>(namespace);
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
                    object: Self::decode_object(value.clone())?,
                    provenance: ResourceProvenance::Replica { origin_root: origin_root.clone(), last_synced_at },
                });
            }
        }
        Ok(items)
    }

    pub(crate) async fn get_replicas_typed<T: Resource>(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<ReadResourceObject<T>>, ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.get_replicas_typed::<T>(namespace, name).await;
        }
        let key = Self::store_key::<T>(namespace);
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
            items.push((origin_root.clone(), ReadResourceObject {
                object: Self::decode_object(value.clone())?,
                provenance: ResourceProvenance::Replica { origin_root: origin_root.clone(), last_synced_at },
            }));
        }
        items.sort_by(|(left, _), (right, _)| left.cmp(right));
        Ok(items.into_iter().map(|(_, item)| item).collect())
    }

    pub(crate) async fn watch_replicas_typed<T: Resource>(
        &self,
        namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>, ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.watch_replicas_typed::<T>(namespace).await;
        }
        let key = Self::store_key::<T>(namespace);
        let rx = self.replicas.lock().await.watchers.entry(key).or_default().subscribe(T::API_PATHS.kind, namespace);
        Ok(stream::unfold(rx, |mut rx| async {
            let event = match rx.next().await? {
                Ok(event) => event,
                Err(error) => return Some((Err(error), rx)),
            };
            let provenance = ResourceProvenance::Replica { origin_root: event.origin_root.clone(), last_synced_at: event.synced_at };
            let decoded = if matches!(event.kind, StoredReplicaEventKind::Deleted) {
                // Name-only tombstones deliberately omit `spec`; a present but
                // malformed spec is a corrupt object and must remain an error.
                if event.object.get("spec").is_some() {
                    decode_watch_object::<T>(&event.object).map(|object| ReadWatchEvent::Deleted(ReadResourceObject { object, provenance }))
                } else {
                    decode_watch_tombstone(&event.object).map(|tombstone| ReadWatchEvent::DeletedByName { tombstone, provenance })
                }
            } else {
                decode_watch_object::<T>(&event.object).map(|object| {
                    let object = ReadResourceObject { object, provenance };
                    match event.kind {
                        StoredReplicaEventKind::Added => ReadWatchEvent::Added(object),
                        StoredReplicaEventKind::Modified => ReadWatchEvent::Modified(object),
                        StoredReplicaEventKind::Deleted => unreachable!("deleted replica events handled above"),
                    }
                })
            };
            Some((decoded, rx))
        })
        .boxed())
    }

    pub(crate) async fn replace_replicas_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
        listed: &ResourceList<T>,
        synced_at: chrono::DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.replace_replicas_typed::<T>(origin_root, namespace, listed, synced_at).await;
        }
        let store_key = Self::store_key::<T>(namespace);
        let replica_key = (origin_root.clone(), store_key.clone());
        let mut state = self.replicas.lock().await;
        let old = state.partitions.remove(&replica_key).unwrap_or_default();
        let mut objects = HashMap::new();
        // Absent names retain deletion fences against delayed relays. A recreated
        // key is absent from old.objects, so insertion below stamps it afresh.
        let mut synced_at_by_name = old.synced_at_by_name;
        for object in &listed.items {
            let name = &object.metadata.name;
            let encoded = Self::encode_object(object)?;
            let unchanged = old.objects.get(name) == Some(&encoded);
            objects.insert(name.clone(), encoded);
            if !unchanged {
                synced_at_by_name.insert(name.clone(), synced_at);
            }
        }
        let mut events = Vec::new();
        for (name, value) in &objects {
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
                synced_at_by_name.insert(name, synced_at);
                events.push(StoredReplicaEvent {
                    origin_root: origin_root.clone(),
                    synced_at,
                    kind: StoredReplicaEventKind::Deleted,
                    object: value,
                });
            }
        }
        state.partitions.insert(replica_key, ReplicaPartition {
            objects,
            synced_at_by_name,
            cursor: Some(ReplicaCursor { resource_version: listed.resource_version.clone(), generation: listed.generation.clone() }),
        });
        for event in events {
            Self::notify_replica_watchers(&mut state, &store_key, event);
        }
        Ok(())
    }

    pub(crate) async fn apply_replica_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
        kind: StoredReplicaEventKind,
        object: &ResourceObject<T>,
        synced_at: chrono::DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.apply_replica_typed::<T>(origin_root, namespace, kind, object, synced_at).await;
        }
        let store_key = Self::store_key::<T>(namespace);
        let replica_key = (origin_root.clone(), store_key.clone());
        let encoded = Self::encode_object(object)?;
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
                partition.objects.insert(object.metadata.name.clone(), encoded.clone());
                partition.synced_at_by_name.insert(object.metadata.name.clone(), synced_at);
            }
            StoredReplicaEventKind::Deleted => {
                partition.objects.remove(&object.metadata.name);
                partition.synced_at_by_name.insert(object.metadata.name.clone(), synced_at);
            }
        }
        Self::notify_replica_watchers(&mut state, &store_key, StoredReplicaEvent {
            origin_root: origin_root.clone(),
            synced_at,
            kind,
            object: encoded,
        });
        Ok(())
    }

    pub(crate) async fn apply_replica_tombstone_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
        tombstone: &ResourceTombstone,
        synced_at: chrono::DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.apply_replica_tombstone_typed::<T>(origin_root, namespace, tombstone, synced_at).await;
        }
        let store_key = Self::store_key::<T>(namespace);
        let replica_key = (origin_root.clone(), store_key.clone());
        let encoded = Self::encode_tombstone::<T>(tombstone);
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
        partition.objects.remove(&tombstone.name);
        partition.synced_at_by_name.insert(tombstone.name.clone(), synced_at);
        Self::notify_replica_watchers(&mut state, &store_key, StoredReplicaEvent {
            origin_root: origin_root.clone(),
            synced_at,
            kind: StoredReplicaEventKind::Deleted,
            object: encoded,
        });
        Ok(())
    }

    pub(crate) async fn invalidate_replica_cursor_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
    ) -> Result<(), ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.invalidate_replica_cursor_typed::<T>(origin_root, namespace).await;
        }
        let key = (origin_root.clone(), Self::store_key::<T>(namespace));
        if let Some(partition) = self.replicas.lock().await.partitions.get_mut(&key) {
            partition.cursor = None;
        }
        Ok(())
    }

    pub(crate) async fn advance_replica_cursor_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
        previous: &ReplicaCursor,
        next: &str,
    ) -> Result<(), ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.advance_replica_cursor_typed::<T>(origin_root, namespace, previous, next).await;
        }
        let key = (origin_root.clone(), Self::store_key::<T>(namespace));
        let mut replicas = self.replicas.lock().await;
        let partition = replicas.partitions.get_mut(&key).ok_or_else(|| ResourceError::invalid("replica prefix missing"))?;
        if partition.cursor.as_ref() != Some(previous) {
            return Err(ResourceError::invalid("replica prefix changed concurrently"));
        }
        partition.cursor = Some(ReplicaCursor { resource_version: next.to_string(), generation: previous.generation.clone() });
        Ok(())
    }

    pub(crate) async fn replica_cursor_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
    ) -> Result<Option<ReplicaCursor>, ResourceError> {
        if let Some(backend) = &self.durable_replicas {
            return backend.replica_cursor_typed::<T>(origin_root, namespace).await;
        }
        let key = (origin_root.clone(), Self::store_key::<T>(namespace));
        Ok(self.replicas.lock().await.partitions.get(&key).and_then(|partition| partition.cursor.clone()))
    }

    fn decode_event<'de, T: Resource>(
        kind: StoredEventKind,
        object: impl Borrow<Value> + serde::Deserializer<'de, Error = serde_json::Error>,
    ) -> Result<WatchEvent<T>, ResourceError> {
        match kind {
            StoredEventKind::Added => decode_watch_object::<T>(object).map(WatchEvent::Added),
            StoredEventKind::Modified => decode_watch_object::<T>(object).map(WatchEvent::Modified),
            // Name-only tombstones deliberately omit `spec`; a present but
            // malformed spec is a corrupt object and must remain an error.
            StoredEventKind::Deleted if object.borrow().get("spec").is_none() => {
                decode_watch_tombstone(object.borrow()).map(WatchEvent::DeletedByName)
            }
            StoredEventKind::Deleted => decode_watch_object::<T>(object).map(WatchEvent::Deleted),
        }
    }

    fn encode_tombstone<T: Resource>(tombstone: &ResourceTombstone) -> Value {
        serde_json::json!({
            "apiVersion": format!("{}/{}", T::API_PATHS.group, T::API_PATHS.version),
            "kind": T::API_PATHS.kind,
            "metadata": {
                "name": tombstone.name,
                "namespace": tombstone.namespace,
                "resourceVersion": tombstone.resource_version,
                "annotations": tombstone.annotations,
            },
        })
    }

    pub(crate) async fn get_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<ResourceObject<T>, ResourceError> {
        self.with_store::<T, _>(namespace, |store| {
            let value = store.objects.get(name).cloned().ok_or_else(|| ResourceError::not_found(name))?;
            Self::decode_object::<T>(value)
        })
        .await
    }

    pub(crate) async fn current_position_typed<T: Resource>(&self, namespace: &str) -> Result<crate::ResourcePosition, ResourceError> {
        self.with_store::<T, _>(namespace, |store| {
            Ok(crate::ResourcePosition { resource_version: store.current_version().to_string(), generation: self.generation.clone() })
        })
        .await
    }

    pub(crate) async fn list_typed<T: Resource>(&self, namespace: &str) -> Result<ResourceList<T>, ResourceError> {
        self.with_store::<T, _>(namespace, |store| {
            let mut items = Vec::with_capacity(store.objects.len());
            for value in store.objects.values().cloned() {
                items.push(Self::decode_object::<T>(value)?);
            }
            items.sort_by(|left, right| left.metadata.name.cmp(&right.metadata.name));
            Ok(ResourceList { items, resource_version: store.current_version().to_string(), generation: self.generation.clone() })
        })
        .await
    }

    pub(crate) async fn list_typed_matching_labels<T: Resource>(
        &self,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ResourceList<T>, ResourceError> {
        if required.is_empty() {
            return self.list_typed::<T>(namespace).await;
        }

        self.with_store::<T, _>(namespace, |store| {
            let mut items = Vec::new();
            for value in store.objects.values().cloned() {
                let object = Self::decode_object::<T>(value)?;
                let matches = crate::labels_match(&object.metadata.labels, required);
                if matches {
                    items.push(object);
                }
            }
            items.sort_by(|left, right| left.metadata.name.cmp(&right.metadata.name));
            Ok(ResourceList { items, resource_version: store.current_version().to_string(), generation: self.generation.clone() })
        })
        .await
    }

    pub(crate) async fn create_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        self.create_typed_with_merge(namespace, meta, spec, None).await
    }

    pub(crate) async fn create_definition_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        spec: &T::Spec,
        merge: MergeMetadata,
    ) -> Result<ResourceObject<T>, ResourceError> {
        self.create_typed_with_merge(namespace, meta, spec, Some(merge)).await
    }

    fn validate_namespace_spec<T: Resource>(store: &ResourceStore, meta: &InputMeta, spec: &T::Spec) -> Result<(), ResourceError> {
        if !T::VALIDATE_NAMESPACE_SPEC {
            return Ok(());
        }
        let siblings = store
            .objects
            .values()
            .cloned()
            .map(Self::decode_object::<T>)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|object| object.metadata.name != meta.name && object.metadata.deletion_timestamp.is_none())
            .map(|object| object.spec)
            .collect::<Vec<_>>();
        T::validate_spec_with_siblings(spec, &siblings)
    }

    async fn create_typed_with_merge<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        spec: &T::Spec,
        merge: Option<MergeMetadata>,
    ) -> Result<ResourceObject<T>, ResourceError> {
        T::validate_spec(meta, spec)?;
        self.with_store_mut::<T, _>(namespace, |store| {
            if store.objects.contains_key(&meta.name) {
                return Err(ResourceError::conflict(&meta.name, "resource already exists"));
            }

            Self::validate_namespace_spec::<T>(store, meta, spec)?;
            let version = store.allocate_version();
            let object = ResourceObject::<T> {
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

            let encoded = Self::encode_object(&object)?;
            store.tombstones.remove(&meta.name);
            store.objects.insert(meta.name.clone(), encoded.clone());
            store.push_event(
                StoredEvent { version, kind: StoredEventKind::Added, object: encoded },
                T::REPLICATION_CLASS.event_retention(self.event_retention.max_events_per_resource_stream()),
            );
            Ok(object)
        })
        .await
    }

    pub(crate) async fn update_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        self.update_typed_with_merge(namespace, meta, resource_version, spec, None).await
    }

    pub(crate) async fn update_definition_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &T::Spec,
        merge: MergeMetadata,
    ) -> Result<ResourceObject<T>, ResourceError> {
        self.update_typed_with_merge(namespace, meta, resource_version, spec, Some(merge)).await
    }

    async fn update_typed_with_merge<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &T::Spec,
        admitted_merge: Option<MergeMetadata>,
    ) -> Result<ResourceObject<T>, ResourceError> {
        self.with_store_mut::<T, _>(namespace, |store| {
            let existing = store.objects.get(&meta.name).cloned().ok_or_else(|| ResourceError::not_found(&meta.name))?;
            let mut object = Self::decode_object::<T>(existing)?;
            if object.metadata.resource_version != resource_version {
                return Err(ResourceError::conflict(&meta.name, "stale resourceVersion"));
            }
            T::validate_spec_update(&object.spec, spec)?;
            T::validate_spec(meta, spec)?;
            Self::validate_namespace_spec::<T>(store, meta, spec)?;
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

            let encoded = Self::encode_object(&object)?;
            if T::REPLICATION_CLASS != crate::ReplicationClass::Definitions
                && object.metadata.deletion_timestamp.is_some()
                && object.metadata.finalizers.is_empty()
            {
                store.objects.remove(&meta.name);
                store.push_event(
                    StoredEvent { version, kind: StoredEventKind::Deleted, object: encoded },
                    T::REPLICATION_CLASS.event_retention(self.event_retention.max_events_per_resource_stream()),
                );
            } else {
                store.objects.insert(meta.name.clone(), encoded.clone());
                store.push_event(
                    StoredEvent { version, kind: StoredEventKind::Modified, object: encoded },
                    T::REPLICATION_CLASS.event_retention(self.event_retention.max_events_per_resource_stream()),
                );
            }
            Ok(object)
        })
        .await
    }

    pub(crate) async fn update_status_typed<T: Resource>(
        &self,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &T::Status,
    ) -> Result<ResourceObject<T>, ResourceError> {
        self.with_store_mut::<T, _>(namespace, |store| {
            let existing = store.objects.get(name).cloned().ok_or_else(|| ResourceError::not_found(name))?;
            let mut object = Self::decode_object::<T>(existing)?;
            if object.metadata.resource_version != resource_version {
                return Err(ResourceError::conflict(name, "stale resourceVersion"));
            }
            T::validate_status_update(object.status.as_ref(), status)?;
            if object.matches_status(status)? {
                return Ok(object);
            }

            let version = store.allocate_version();
            object.metadata.resource_version = version.to_string();
            object.status = Some(Self::clone_through_serde(status)?);

            let encoded = Self::encode_object(&object)?;
            store.objects.insert(name.to_string(), encoded.clone());
            store.push_event(
                StoredEvent { version, kind: StoredEventKind::Modified, object: encoded },
                T::REPLICATION_CLASS.event_retention(self.event_retention.max_events_per_resource_stream()),
            );
            Ok(object)
        })
        .await
    }

    pub(crate) async fn delete_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<(), ResourceError> {
        self.with_store_mut::<T, _>(namespace, |store| {
            let existing = store.objects.get(name).cloned().ok_or_else(|| ResourceError::not_found(name))?;
            let mut object = Self::decode_object::<T>(existing)?;
            if object.metadata.is_pending_finalization() {
                return Ok(());
            }
            let version = store.allocate_version();
            object.metadata.resource_version = version.to_string();
            if !object.metadata.finalizers.is_empty() && object.metadata.deletion_timestamp.is_none() {
                object.metadata.deletion_timestamp = Some(Utc::now());
                let encoded = Self::encode_object(&object)?;
                store.objects.insert(name.to_string(), encoded.clone());
                store.push_event(
                    StoredEvent { version, kind: StoredEventKind::Modified, object: encoded },
                    T::REPLICATION_CLASS.event_retention(self.event_retention.max_events_per_resource_stream()),
                );
                return Ok(());
            }

            let encoded = Self::encode_object(&object)?;
            store.objects.remove(name);
            store.push_event(
                StoredEvent { version, kind: StoredEventKind::Deleted, object: encoded },
                T::REPLICATION_CLASS.event_retention(self.event_retention.max_events_per_resource_stream()),
            );
            Ok(())
        })
        .await
    }

    pub(crate) async fn tombstone_typed<T: Resource>(
        &self,
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
        self.with_store_mut::<T, _>(namespace, |store| {
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
                StoredEvent { version, kind: StoredEventKind::Deleted, object: Self::encode_tombstone::<T>(&tombstone) },
                T::REPLICATION_CLASS.event_retention(self.event_retention.max_events_per_resource_stream()),
            );
            store.tombstones.insert(name.to_string(), tombstone.clone());
            Ok(crate::watch::TombstoneWrite { tombstone, created: true })
        })
        .await
    }

    pub(crate) async fn watch_typed<T: Resource>(&self, namespace: &str, start: WatchStart) -> Result<WatchStream<T>, ResourceError> {
        let generation = self.generation.clone();
        let (replay, receiver) = {
            let mut stores = self.stores.lock().await;
            let store = stores.entry(Self::store_key::<T>(namespace)).or_default();
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
            let receiver = store.watchers.subscribe(T::API_PATHS.kind, namespace);
            (replay, receiver)
        };

        let replay_stream = stream::iter(replay.into_iter().map(|event| Self::decode_event::<T>(event.kind, event.object)));
        let live_stream = stream::unfold(receiver, |mut receiver| async {
            receiver.next().await.map(|event| (event.and_then(|event| Self::decode_event::<T>(event.kind, &event.object)), receiver))
        });
        Ok(WatchStream::new(generation, Box::pin(replay_stream.chain(live_stream))))
    }
}
