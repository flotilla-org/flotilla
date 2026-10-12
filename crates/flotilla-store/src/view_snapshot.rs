//! Immutable request snapshot composed from watch views. Existing typed
//! resolvers can consume it without changing their query or routing contracts.
use crate::{ReadObserver, ReadOperation, ResourceKind, SerializedResource, Store};
use flotilla_protocol::NodeId;
use flotilla_resources::*;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Debug, Clone)]
pub(crate) struct SnapshotPartition {
    pub(crate) items: BTreeMap<String, Arc<ReadResourceObject<SerializedResource>>>,
    pub(crate) index: Arc<crate::QueryIndex>,
    pub(crate) local_names: BTreeSet<String>,
}
type SnapshotObjects = BTreeMap<(String, String), Arc<SnapshotPartition>>;

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
        let mut items = BTreeMap::new();
        let mut local_names = BTreeSet::new();
        for (identity, item) in listed.items.into_iter().enumerate() {
            // Cold request snapshots still own their selected evidence, but
            // serialize each typed body only once for both storage and indexing.
            let mut value = serde_json::to_value(item.object.to_k8s_object()).map_err(|error| ResourceError::decode(error.to_string()))?;
            let identity = identity.to_string();
            index.update(&identity, Some(&value));
            if matches!(item.provenance, ResourceProvenance::Local) {
                local_names.insert(item.object.metadata.name.clone());
            }
            let fields = value.as_object_mut().expect("resource object");
            let object = ResourceObject::<SerializedResource> {
                metadata: crate::transcode(item.object.metadata)?,
                spec: fields.remove("spec").expect("resource spec"),
                status: fields.remove("status").filter(|value| !value.is_null()),
            };
            items.insert(identity, Arc::new(ReadResourceObject { object, provenance: item.provenance }));
        }
        Arc::make_mut(&mut self.objects).insert(
            (T::API_PATHS.kind.into(), namespace.into()),
            Arc::new(SnapshotPartition { items, index: Arc::new(index), local_names }),
        );
        Ok(())
    }
    pub(crate) fn insert_partition<T: Resource>(&mut self, namespace: &str, partition: Arc<SnapshotPartition>) {
        Arc::make_mut(&mut self.objects).insert((T::API_PATHS.kind.into(), namespace.into()), partition);
    }
    fn partition(&self, kind: ResourceKind, namespace: &str) -> Result<&SnapshotPartition, ResourceError> {
        self.objects
            .get(&(kind.paths.kind.into(), namespace.into()))
            .map(Arc::as_ref)
            .ok_or_else(|| ResourceError::invalid(format!("kind {} missing from view snapshot", kind.paths.kind)))
    }
    fn observe(&self, kind: ResourceKind, operation: ReadOperation, objects: usize) {
        if let Some(observer) = &self.observer {
            observer.read(kind.paths.kind, operation, objects);
        }
    }
    /// A whole-kind list: visits every object in the partition.
    fn items(&self, kind: ResourceKind, namespace: &str) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        let partition = self.partition(kind, namespace)?;
        self.observe(kind, ReadOperation::List, partition.items.len());
        Ok(self.visible(partition))
    }
    fn visible(&self, partition: &SnapshotPartition) -> Vec<ReadResourceObject<SerializedResource>> {
        let mut items: Vec<_> =
            partition.items.values().filter(|item| !self.shadowed(item, partition)).map(|item| item.as_ref().clone()).collect();
        crate::sort_query_items(&mut items);
        items
    }
    /// Visible provenances of one name, without cloning the partition.
    fn named(
        &self,
        kind: ResourceKind,
        namespace: &str,
        name: &str,
        local_only: bool,
    ) -> Result<Option<ReadResourceObject<SerializedResource>>, ResourceError> {
        let partition = self.partition(kind, namespace)?;
        let found = partition
            .items
            .values()
            .filter(|item| {
                item.object.metadata.name == name
                    && !self.shadowed(item, partition)
                    && (!local_only || matches!(item.provenance, ResourceProvenance::Local))
            })
            .min_by_key(|item| match &item.provenance {
                ResourceProvenance::Local => (0, None),
                ResourceProvenance::Replica { origin_root, .. } => (1, Some(origin_root)),
            })
            .map(|item| item.as_ref().clone());
        self.observe(kind, ReadOperation::Get, usize::from(found.is_some()));
        Ok(found)
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
        let resolved = crate::resolve_semijoins(self, kind, namespace, query).await?;
        let query = &resolved;
        let partition = self
            .objects
            .get(&(kind.paths.kind.into(), namespace.into()))
            .map(Arc::as_ref)
            .ok_or_else(|| ResourceError::invalid(format!("kind {} missing from view snapshot", kind.paths.kind)))?;
        let mut items: Vec<_> = partition
            .index
            .names(query)
            .into_iter()
            .map(|identity| partition.items[&identity].as_ref().clone())
            .filter(|item| !query.suppress_self_origin || !self.shadowed(item, partition))
            .filter(|item| {
                if query.replicas_only {
                    matches!(item.provenance, ResourceProvenance::Replica { .. })
                } else {
                    query.include_replicas || matches!(item.provenance, ResourceProvenance::Local)
                }
            })
            .collect();
        self.observe(kind, query.read_operation(), items.len());
        crate::sort_query_items(&mut items);
        Ok(crate::QueryResult {
            local_root: self.root.clone(),
            items,
            position: ResourcePosition { resource_version: "0".into(), generation: None },
        })
    }
    async fn query_values_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        query: &crate::StoreQuery,
        field: &str,
    ) -> Result<std::collections::BTreeSet<String>, ResourceError> {
        query.validate(kind)?;
        if !kind.queryable_fields.contains(&field) {
            return Err(ResourceError::invalid("undeclared field projection"));
        }
        if query.message.is_some() {
            return Err(ResourceError::invalid("snapshot does not index Message history"));
        }
        let query = crate::resolve_semijoins(self, kind, namespace, query).await?;
        let partition = self
            .objects
            .get(&(kind.paths.kind.into(), namespace.into()))
            .map(Arc::as_ref)
            .ok_or_else(|| ResourceError::invalid("source kind missing from view snapshot"))?;
        let names = partition
            .index
            .names(&query)
            .into_iter()
            .filter(|identity| {
                let item = &partition.items[identity];
                (!query.suppress_self_origin || !self.shadowed(item, partition))
                    && (query.include_replicas || matches!(item.provenance, ResourceProvenance::Local))
                    && (!query.replicas_only || matches!(item.provenance, ResourceProvenance::Replica { .. }))
            })
            .collect();
        Ok(partition.index.values(field, &names))
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
        self.named(resource_kind, namespace, name, true)?.map(|item| item.object).ok_or_else(|| ResourceError::not_found(name))
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
        // A label selector is a query in every adapter, counted by its matches.
        // Select by reference and clone only the matches.
        let partition = self.partition(resource_kind, namespace)?;
        let items = partition
            .items
            .values()
            .filter(|item| matches!(item.provenance, ResourceProvenance::Local) && !self.shadowed(item, partition))
            .filter(|item| required.iter().all(|(key, value)| item.object.metadata.labels.get(key) == Some(value)))
            .map(|item| item.object.clone())
            .collect::<Vec<_>>();
        self.observe(resource_kind, ReadOperation::Query, items.len());
        Ok(ResourceList { items, resource_version: String::new(), generation: None })
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
        self.named(kind, namespace, name, false)?.ok_or_else(|| ResourceError::not_found(name))
    }
}
