use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use chrono::Utc;
use flotilla_protocol::NodeId;
use futures::{stream, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};

use crate::{
    error::ResourceError,
    replica::{ReadResourceObject, ReadWatchEvent, ReplicaCursor, ResourceProvenance, StoredReplicaEvent, StoredReplicaEventKind},
    resource::{InputMeta, K8sResourceObject, ObjectMeta, Resource, ResourceObject},
    retention::{EventRetention, ResourceStoreDiagnostics},
    watch::{ResourceList, WatchEvent, WatchStart, WatchStream},
};

type StoreKey = (String, String, String, String);

#[derive(Debug, Clone, Default)]
pub struct InMemoryBackend {
    stores: Arc<Mutex<HashMap<StoreKey, ResourceStore>>>,
    replicas: Arc<Mutex<ReplicaState>>,
    generation: Option<String>,
    event_retention: EventRetention,
}

type ReplicaKey = (NodeId, StoreKey);

#[derive(Debug, Default)]
struct ReplicaState {
    partitions: HashMap<ReplicaKey, ReplicaPartition>,
    watchers: HashMap<StoreKey, Vec<mpsc::UnboundedSender<StoredReplicaEvent>>>,
}

#[derive(Debug, Default)]
struct ReplicaPartition {
    objects: HashMap<String, Value>,
    synced_at_by_name: HashMap<String, chrono::DateTime<Utc>>,
    cursor: Option<ReplicaCursor>,
}

#[derive(Debug)]
struct ResourceStore {
    objects: HashMap<String, Value>,
    next_version: u64,
    watchers: Vec<mpsc::UnboundedSender<StoredEvent>>,
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

    fn push_event(&mut self, event: StoredEvent, retention: EventRetention) {
        let excess = self.event_log.len().saturating_add(1).saturating_sub(retention.max_events_per_resource_stream());
        if excess > 0 {
            if let Some(last_removed) = self.event_log.get(excess - 1) {
                self.compacted_through = last_removed.version;
            }
            self.event_log.drain(..excess);
        }
        self.event_log.push(event.clone());
        self.watchers.retain(|watcher| watcher.send(event.clone()).is_ok());
    }
}

impl Default for ResourceStore {
    fn default() -> Self {
        Self { objects: HashMap::new(), next_version: 1, watchers: Vec::new(), event_log: Vec::new(), compacted_through: 0 }
    }
}

impl InMemoryBackend {
    pub fn observed() -> Self {
        Self {
            stores: Arc::default(),
            replicas: Arc::default(),
            generation: Some(uuid::Uuid::new_v4().to_string()),
            event_retention: EventRetention::default(),
        }
    }

    pub fn with_event_retention(event_retention: EventRetention) -> Self {
        Self { stores: Arc::default(), replicas: Arc::default(), generation: None, event_retention }
    }

    pub fn observed_with_event_retention(event_retention: EventRetention) -> Self {
        Self { stores: Arc::default(), replicas: Arc::default(), generation: Some(uuid::Uuid::new_v4().to_string()), event_retention }
    }

    pub(crate) async fn diagnostics(&self) -> Result<ResourceStoreDiagnostics, ResourceError> {
        let stores = self.stores.lock().await;
        let object_count = stores.values().map(|store| store.objects.len() as u64).sum();
        let event_count = stores.values().map(|store| store.event_log.len() as u64).sum();
        let resource_stream_count = stores.values().filter(|store| store.current_version() > 0).count() as u64;
        Ok(ResourceStoreDiagnostics::new(object_count, event_count, resource_stream_count, self.event_retention))
    }

    fn store_key<T: Resource>(namespace: &str) -> StoreKey {
        (T::API_PATHS.group.to_string(), T::API_PATHS.version.to_string(), T::API_PATHS.plural.to_string(), namespace.to_string())
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
            watchers.retain(|watcher| watcher.send(event.clone()).is_ok());
        }
    }

    pub(crate) async fn list_replicas_typed<T: Resource>(&self, namespace: &str) -> Result<Vec<ReadResourceObject<T>>, ResourceError> {
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

    pub(crate) async fn watch_replicas_typed<T: Resource>(
        &self,
        namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let (tx, rx) = mpsc::unbounded_channel();
        self.replicas.lock().await.watchers.entry(key).or_default().push(tx);
        Ok(stream::unfold(rx, |mut rx| async {
            let event = rx.recv().await?;
            let decoded = Self::decode_object::<T>(event.object).map(|object| {
                let object = ReadResourceObject {
                    object,
                    provenance: ResourceProvenance::Replica { origin_root: event.origin_root, last_synced_at: event.synced_at },
                };
                match event.kind {
                    StoredReplicaEventKind::Added => ReadWatchEvent::Added(object),
                    StoredReplicaEventKind::Modified => ReadWatchEvent::Modified(object),
                    StoredReplicaEventKind::Deleted => ReadWatchEvent::Deleted(object),
                }
            });
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
        let store_key = Self::store_key::<T>(namespace);
        let replica_key = (origin_root.clone(), store_key.clone());
        let mut state = self.replicas.lock().await;
        let old = state.partitions.remove(&replica_key).unwrap_or_default();
        let mut objects = HashMap::new();
        let mut synced_at_by_name = HashMap::new();
        for object in &listed.items {
            objects.insert(object.metadata.name.clone(), Self::encode_object(object)?);
            synced_at_by_name.insert(object.metadata.name.clone(), synced_at);
        }
        let mut events = Vec::new();
        for (name, value) in &objects {
            events.push(StoredReplicaEvent {
                origin_root: origin_root.clone(),
                synced_at,
                kind: if old.objects.contains_key(name) { StoredReplicaEventKind::Modified } else { StoredReplicaEventKind::Added },
                object: value.clone(),
            });
        }
        for (name, value) in old.objects {
            if !objects.contains_key(&name) {
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
        let store_key = Self::store_key::<T>(namespace);
        let replica_key = (origin_root.clone(), store_key.clone());
        let encoded = Self::encode_object(object)?;
        let mut state = self.replicas.lock().await;
        let partition = state.partitions.entry(replica_key).or_default();
        match kind {
            StoredReplicaEventKind::Added | StoredReplicaEventKind::Modified => {
                partition.objects.insert(object.metadata.name.clone(), encoded.clone());
                partition.synced_at_by_name.insert(object.metadata.name.clone(), synced_at);
            }
            StoredReplicaEventKind::Deleted => {
                partition.objects.remove(&object.metadata.name);
                partition.synced_at_by_name.remove(&object.metadata.name);
            }
        }
        partition.cursor = Some(ReplicaCursor {
            resource_version: object.metadata.resource_version.clone(),
            generation: partition.cursor.as_ref().and_then(|cursor| cursor.generation.clone()),
        });
        Self::notify_replica_watchers(&mut state, &store_key, StoredReplicaEvent {
            origin_root: origin_root.clone(),
            synced_at,
            kind,
            object: encoded,
        });
        Ok(())
    }

    pub(crate) async fn replica_cursor_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
    ) -> Result<Option<ReplicaCursor>, ResourceError> {
        let key = (origin_root.clone(), Self::store_key::<T>(namespace));
        Ok(self.replicas.lock().await.partitions.get(&key).and_then(|partition| partition.cursor.clone()))
    }

    fn decode_event<T: Resource>(event: StoredEvent) -> Result<WatchEvent<T>, ResourceError> {
        let object = Self::decode_object::<T>(event.object)?;
        Ok(match event.kind {
            StoredEventKind::Added => WatchEvent::Added(object),
            StoredEventKind::Modified => WatchEvent::Modified(object),
            StoredEventKind::Deleted => WatchEvent::Deleted(object),
        })
    }

    pub(crate) async fn get_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<ResourceObject<T>, ResourceError> {
        self.with_store::<T, _>(namespace, |store| {
            let value = store.objects.get(name).cloned().ok_or_else(|| ResourceError::not_found(name))?;
            Self::decode_object::<T>(value)
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
                let matches = required.iter().all(|(key, expected)| object.metadata.labels.get(key) == Some(expected));
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
        self.with_store_mut::<T, _>(namespace, |store| {
            if store.objects.contains_key(&meta.name) {
                return Err(ResourceError::conflict(&meta.name, "resource already exists"));
            }

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
                },
                spec: Self::clone_through_serde(spec)?,
                status: None,
            };

            let encoded = Self::encode_object(&object)?;
            store.objects.insert(meta.name.clone(), encoded.clone());
            store.push_event(StoredEvent { version, kind: StoredEventKind::Added, object: encoded }, self.event_retention);
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
        self.with_store_mut::<T, _>(namespace, |store| {
            let existing = store.objects.get(&meta.name).cloned().ok_or_else(|| ResourceError::not_found(&meta.name))?;
            let mut object = Self::decode_object::<T>(existing)?;
            if object.metadata.resource_version != resource_version {
                return Err(ResourceError::conflict(&meta.name, "stale resourceVersion"));
            }
            T::validate_spec_update(&object.spec, spec)?;
            if object.matches_update(meta, spec)? {
                return Ok(object);
            }

            let version = store.allocate_version();
            object.metadata.resource_version = version.to_string();
            object.metadata.labels = meta.labels.clone();
            object.metadata.annotations = meta.annotations.clone();
            object.metadata.owner_references = meta.owner_references.clone();
            object.metadata.finalizers = meta.finalizers.clone();
            object.metadata.deletion_timestamp = meta.deletion_timestamp;
            object.spec = Self::clone_through_serde(spec)?;

            let encoded = Self::encode_object(&object)?;
            if object.metadata.deletion_timestamp.is_some() && object.metadata.finalizers.is_empty() {
                store.objects.remove(&meta.name);
                store.push_event(StoredEvent { version, kind: StoredEventKind::Deleted, object: encoded }, self.event_retention);
            } else {
                store.objects.insert(meta.name.clone(), encoded.clone());
                store.push_event(StoredEvent { version, kind: StoredEventKind::Modified, object: encoded }, self.event_retention);
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
            if object.matches_status(status)? {
                return Ok(object);
            }

            let version = store.allocate_version();
            object.metadata.resource_version = version.to_string();
            object.status = Some(Self::clone_through_serde(status)?);

            let encoded = Self::encode_object(&object)?;
            store.objects.insert(name.to_string(), encoded.clone());
            store.push_event(StoredEvent { version, kind: StoredEventKind::Modified, object: encoded }, self.event_retention);
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
                store.push_event(StoredEvent { version, kind: StoredEventKind::Modified, object: encoded }, self.event_retention);
                return Ok(());
            }

            let encoded = Self::encode_object(&object)?;
            store.objects.remove(name);
            store.push_event(StoredEvent { version, kind: StoredEventKind::Deleted, object: encoded }, self.event_retention);
            Ok(())
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
            let replay = match replay_from {
                Some(version) => store.event_log.iter().filter(|event| event.version > version).cloned().collect(),
                None => Vec::new(),
            };
            let (sender, receiver) = mpsc::unbounded_channel();
            store.watchers.push(sender);
            (replay, receiver)
        };

        let replay_stream = stream::iter(replay.into_iter().map(Self::decode_event::<T>));
        let live_stream = stream::unfold(receiver, |mut receiver| async {
            receiver.recv().await.map(|event| (Self::decode_event::<T>(event), receiver))
        });
        Ok(WatchStream::new(generation, Box::pin(replay_stream.chain(live_stream))))
    }
}
