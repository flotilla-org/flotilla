//! Immutable request snapshot composed from watch views. Existing typed
//! resolvers can consume it without changing their query or routing contracts.
use crate::{ReadObserver, ResourceKind, SerializedResource, Store};
use flotilla_protocol::NodeId;
use flotilla_resources::*;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Debug, Clone)]
struct SnapshotPartition {
    items: Vec<ReadResourceObject<SerializedResource>>,
    index: Arc<crate::QueryIndex>,
    local_names: BTreeSet<String>,
}
type SnapshotObjects = BTreeMap<(String, String), SnapshotPartition>;

#[derive(Debug, Clone, Default)]
pub struct ViewSnapshot {
    objects: Arc<SnapshotObjects>,
    root: Option<NodeId>,
    observer: Option<Arc<dyn ReadObserver>>,
}
impl ViewSnapshot {
    pub fn new(root: Option<NodeId>) -> Self {
        Self { root, ..Self::default() }
    }
    pub fn insert<T: Resource>(&mut self, namespace: &str, listed: ReadResourceList<T>) -> Result<(), ResourceError> {
        let mut index = crate::QueryIndex::default();
        for (identity, item) in listed.items.iter().enumerate() {
            let value = serde_json::to_value(item.object.to_k8s_object()).map_err(|error| ResourceError::decode(error.to_string()))?;
            index.update(&identity.to_string(), Some(&value));
        }
        let local_names = listed
            .items
            .iter()
            .filter(|item| matches!(item.provenance, ResourceProvenance::Local))
            .map(|item| item.object.metadata.name.clone())
            .collect();
        let listed = crate::convert_read_list(listed)?;
        Arc::make_mut(&mut self.objects).insert(
            (T::API_PATHS.kind.into(), namespace.into()),
            SnapshotPartition { items: listed.items, index: Arc::new(index), local_names },
        );
        Ok(())
    }
    fn items(&self, kind: ResourceKind, namespace: &str) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        self.objects
            .get(&(kind.paths.kind.into(), namespace.into()))
            .map(|partition| {
                if let Some(observer) = &self.observer {
                    observer.read(kind.paths.kind, partition.items.len());
                }
                partition.items.iter().filter(|item| !self.shadowed(item, partition)).cloned().collect()
            })
            .ok_or_else(|| ResourceError::invalid(format!("kind {} missing from view snapshot", kind.paths.kind)))
    }
    fn shadowed(&self, item: &ReadResourceObject<SerializedResource>, partition: &SnapshotPartition) -> bool {
        matches!(&item.provenance, ResourceProvenance::Replica { origin_root, .. }
            if Some(origin_root) == self.root.as_ref() && partition.local_names.contains(&item.object.metadata.name))
    }
}
#[async_trait::async_trait]
impl Store for ViewSnapshot {
    fn serves_read_views(&self) -> bool {
        true
    }
    fn with_local_root(&self, root: NodeId) -> Arc<dyn Store> {
        let mut snapshot = self.clone();
        snapshot.root = Some(root);
        Arc::new(snapshot)
    }
    fn local_root(&self) -> Result<NodeId, ResourceError> {
        self.root.clone().ok_or_else(|| ResourceError::invalid("view snapshot has no local root"))
    }
    fn with_read_observer(&self, observer: Arc<dyn ReadObserver>) -> Arc<dyn Store> {
        let mut snapshot = self.clone();
        snapshot.observer = Some(observer);
        Arc::new(snapshot)
    }
    fn read_observer(&self) -> Option<&dyn ReadObserver> {
        self.observer.as_deref()
    }
    async fn diagnostics(&self) -> Result<Option<ResourceStoreDiagnostics>, ResourceError> {
        Ok(None)
    }
    async fn record_field_ownership_violation(&self, _violation: FieldOwnershipViolation) -> Result<(), ResourceError> {
        Err(self.unsupported_operation("writing view snapshots"))
    }
    async fn query_raw(&self, kind: ResourceKind, namespace: &str, query: &crate::StoreQuery) -> Result<crate::QueryResult, ResourceError> {
        query.validate(kind)?;
        if query.message.is_some() || query.replica_shadowing.is_some() {
            return Err(ResourceError::invalid("snapshot queries support labels, declared fields and owners"));
        }
        let partition = self
            .objects
            .get(&(kind.paths.kind.into(), namespace.into()))
            .ok_or_else(|| ResourceError::invalid(format!("kind {} missing from view snapshot", kind.paths.kind)))?;
        let mut items: Vec<_> = partition
            .index
            .names(query)
            .into_iter()
            .map(|identity| partition.items[identity.parse::<usize>().expect("snapshot index identity")].clone())
            .filter(|item| !query.suppress_self_origin || !self.shadowed(item, partition))
            .filter(|item| {
                if query.replicas_only {
                    matches!(item.provenance, ResourceProvenance::Replica { .. })
                } else {
                    query.include_replicas || matches!(item.provenance, ResourceProvenance::Local)
                }
            })
            .collect();
        if let Some(observer) = &self.observer {
            observer.read(kind.paths.kind, items.len());
        }
        crate::sort_query_items(&mut items);
        Ok(crate::QueryResult {
            local_root: self.root.clone(),
            items,
            position: ResourcePosition { resource_version: "0".into(), generation: None },
        })
    }
    async fn query_messages_raw(
        &self,
        _namespace: &str,
        _query: &crate::MessageQuery,
        _include_replicas: bool,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        Err(self.unsupported_operation("mutating or watching view snapshots"))
    }
    async fn get_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.items(resource_kind, namespace)?
            .into_iter()
            .find(|item| item.object.metadata.name == name && matches!(item.provenance, ResourceProvenance::Local))
            .map(|item| item.object)
            .ok_or_else(|| ResourceError::not_found(name))
    }
    async fn current_position_raw(&self, _resource_kind: ResourceKind, _namespace: &str) -> Result<ResourcePosition, ResourceError> {
        Err(self.unsupported_operation("mutating or watching view snapshots"))
    }
    async fn digest_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _query: &DigestQuery,
    ) -> Result<PartitionDigest, ResourceError> {
        Err(self.unsupported_operation("mutating or watching view snapshots"))
    }
    async fn list_raw(&self, resource_kind: ResourceKind, namespace: &str) -> Result<ResourceList<SerializedResource>, ResourceError> {
        Ok(ResourceList {
            items: self
                .items(resource_kind, namespace)?
                .into_iter()
                .filter(|item| matches!(item.provenance, ResourceProvenance::Local))
                .map(|item| item.object)
                .collect(),
            resource_version: String::new(),
            generation: None,
        })
    }
    async fn list_raw_matching_labels(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ResourceList<SerializedResource>, ResourceError> {
        let mut listed = self.list_raw(resource_kind, namespace).await?;
        listed.items.retain(|item| required.iter().all(|(key, value)| item.metadata.labels.get(key) == Some(value)));
        Ok(listed)
    }
    async fn create_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _meta: &InputMeta,
        _spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        Err(self.unsupported_operation("mutating or watching view snapshots"))
    }
    async fn update_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _meta: &InputMeta,
        _resource_version: &str,
        _spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        Err(self.unsupported_operation("mutating or watching view snapshots"))
    }
    async fn update_status_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _name: &str,
        _resource_version: &str,
        _status: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        Err(self.unsupported_operation("mutating or watching view snapshots"))
    }
    async fn delete_raw(&self, _resource_kind: ResourceKind, _namespace: &str, _name: &str) -> Result<(), ResourceError> {
        Err(self.unsupported_operation("mutating or watching view snapshots"))
    }
    async fn tombstone_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _name: &str,
        _minimum_resource_version: Option<&str>,
    ) -> Result<crate::watch::TombstoneWrite, ResourceError> {
        Err(self.unsupported_operation("mutating or watching view snapshots"))
    }
    async fn watch_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _start: WatchStart,
    ) -> Result<crate::SerializedWatchStream, ResourceError> {
        Err(self.unsupported_operation("mutating or watching view snapshots"))
    }
    async fn list_including_replicas_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        Ok(ReadResourceList { items: self.items(kind, namespace)? })
    }
    async fn get_including_replicas_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ReadResourceObject<SerializedResource>, ResourceError> {
        self.items(kind, namespace)?
            .into_iter()
            .find(|item| item.object.metadata.name == name)
            .ok_or_else(|| ResourceError::not_found(name))
    }
}
