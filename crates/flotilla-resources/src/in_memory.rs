use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use chrono::Utc;
use futures::{stream, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};

use crate::{
    error::ResourceError,
    resource::{InputMeta, K8sResourceObject, ObjectMeta, Resource, ResourceObject},
    watch::{ResourceList, WatchEvent, WatchStart, WatchStream},
};

type StoreKey = (String, String, String, String);

#[derive(Debug, Clone, Default)]
pub struct InMemoryBackend {
    stores: Arc<Mutex<HashMap<StoreKey, ResourceStore>>>,
    generation: Option<String>,
}

#[derive(Debug)]
struct ResourceStore {
    objects: HashMap<String, Value>,
    next_version: u64,
    watchers: Vec<mpsc::UnboundedSender<StoredEvent>>,
    // TODO: compact the event log if this backend starts serving long-lived scenarios.
    event_log: Vec<StoredEvent>,
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

    fn push_event(&mut self, event: StoredEvent) {
        self.event_log.push(event.clone());
        self.watchers.retain(|watcher| watcher.send(event.clone()).is_ok());
    }
}

impl Default for ResourceStore {
    fn default() -> Self {
        Self { objects: HashMap::new(), next_version: 1, watchers: Vec::new(), event_log: Vec::new() }
    }
}

impl InMemoryBackend {
    pub fn observed() -> Self {
        Self { stores: Arc::default(), generation: Some(uuid::Uuid::new_v4().to_string()) }
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
            store.push_event(StoredEvent { version, kind: StoredEventKind::Added, object: encoded });
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
                store.push_event(StoredEvent { version, kind: StoredEventKind::Deleted, object: encoded });
            } else {
                store.objects.insert(meta.name.clone(), encoded.clone());
                store.push_event(StoredEvent { version, kind: StoredEventKind::Modified, object: encoded });
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

            let version = store.allocate_version();
            object.metadata.resource_version = version.to_string();
            object.status = Some(Self::clone_through_serde(status)?);

            let encoded = Self::encode_object(&object)?;
            store.objects.insert(name.to_string(), encoded.clone());
            store.push_event(StoredEvent { version, kind: StoredEventKind::Modified, object: encoded });
            Ok(object)
        })
        .await
    }

    pub(crate) async fn delete_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<(), ResourceError> {
        self.with_store_mut::<T, _>(namespace, |store| {
            let existing = store.objects.get(name).cloned().ok_or_else(|| ResourceError::not_found(name))?;
            let mut object = Self::decode_object::<T>(existing)?;
            let version = store.allocate_version();
            object.metadata.resource_version = version.to_string();
            if !object.metadata.finalizers.is_empty() && object.metadata.deletion_timestamp.is_none() {
                object.metadata.deletion_timestamp = Some(Utc::now());
                let encoded = Self::encode_object(&object)?;
                store.objects.insert(name.to_string(), encoded.clone());
                store.push_event(StoredEvent { version, kind: StoredEventKind::Modified, object: encoded });
                return Ok(());
            }

            let encoded = Self::encode_object(&object)?;
            store.objects.remove(name);
            store.push_event(StoredEvent { version, kind: StoredEventKind::Deleted, object: encoded });
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
