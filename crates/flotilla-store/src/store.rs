//! Untyped storage seam. Kind descriptors retain resource validation at the
//! authoritative write boundary, including while a backend holds its transaction.
use crate::replica::StoredReplicaEventKind;
use crate::{
    typed_object, typed_read, typed_read_list, typed_read_watch, DecodedList, DecodedObject, DecodedQueryResult, DecodedReadObject,
};
use chrono::Utc;
use flotilla_protocol::NodeId;
use flotilla_resources::{
    api_version, ApiPaths, DigestQuery, FieldOwnershipViolation, InputMeta, K8sObjectMeta, K8sResourceList, K8sResourceObject,
    MergeMetadata, Message, PartitionDigest, ReadResourceList, ReadResourceObject, ReadWatchEvent, ReplicaCursor, ReplicationClass,
    Resource, ResourceError, ResourceList, ResourceObject, ResourcePosition, ResourceProvenance, ResourceStoreDiagnostics,
    ResourceTombstone, StatusPatch, WatchEvent, WatchStart, WatchStream,
};
use futures::StreamExt;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Clone)]
pub struct SerializedResource;
pub enum SerializedStatusPatch {}
impl StatusPatch<Value> for SerializedStatusPatch {
    fn apply(&self, _: &mut Value) {
        match *self {}
    }
}
impl Resource for SerializedResource {
    type Spec = Value;
    type Status = Value;
    type StatusPatch = SerializedStatusPatch;
    const API_PATHS: ApiPaths = ApiPaths { group: "", version: "", plural: "", kind: "" };
}

pub fn transcode<A: Serialize, B: DeserializeOwned>(value: A) -> Result<B, ResourceError> {
    let value = serde_json::to_value(value).map_err(|e| ResourceError::decode(e.to_string()))?;
    serde_json::from_value(value).map_err(|e| ResourceError::decode(e.to_string()))
}

type ValidateSiblings = fn(&InputMeta, &Value, &[ResourceObject<SerializedResource>]) -> Result<(), ResourceError>;

#[derive(Clone, Copy)]
pub struct ResourceKind {
    pub paths: ApiPaths,
    pub replication: ReplicationClass,
    pub event_history: flotilla_resources::EventHistory,
    pub queryable_fields: &'static [&'static str],
    pub queryable_numeric_fields: &'static [&'static str],
    pub validate_namespace: bool,
    validate_stored_fn: fn(&K8sResourceObject<SerializedResource>) -> Result<(), ResourceError>,
    decode_typed_text_fn: fn(&str) -> Result<DecodedObject, ResourceError>,
    decode_typed_value_fn: fn(&Value) -> Result<DecodedObject, ResourceError>,
    initial_status_fn: fn(&InputMeta) -> Result<Option<Value>, ResourceError>,
    validate_spec_fn: fn(&InputMeta, &Value) -> Result<(), ResourceError>,
    validate_spec_update_fn: fn(&Value, &Value) -> Result<(), ResourceError>,
    validate_status_update_fn: fn(Option<&Value>, &Value) -> Result<(), ResourceError>,
    validate_siblings_fn: ValidateSiblings,
    digest_fn: fn(NodeId, &str, Option<String>, String, Vec<String>) -> PartitionDigest,
}
impl std::fmt::Debug for ResourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceKind").field("paths", &self.paths).finish()
    }
}
fn stored_decode_error(error: impl std::fmt::Display) -> ResourceError {
    ResourceError::decode(format!("decode stored object: {error}"))
}
/// The registered type's view of a serialized object: its apiVersion and kind,
/// and a typed deserialization of its spec and status. Metadata is already
/// typed in the serialized form. This is the same acceptance as deserializing
/// the whole object into `K8sResourceObject<T>`, without building a `T`-shaped
/// copy of the tree it borrows.
fn validate_stored<T: Resource>(object: &K8sResourceObject<SerializedResource>) -> Result<(), ResourceError> {
    let expected_api_version = api_version(T::API_PATHS);
    if object.api_version != expected_api_version {
        return Err(ResourceError::decode(format!("unexpected apiVersion '{}', expected '{}'", object.api_version, expected_api_version)));
    }
    if object.kind != T::API_PATHS.kind {
        return Err(ResourceError::decode(format!("unexpected kind '{}', expected '{}'", object.kind, T::API_PATHS.kind)));
    }
    T::Spec::deserialize(&object.spec).map_err(stored_decode_error)?;
    if let Some(status) = &object.status {
        Option::<T::Status>::deserialize(status).map_err(stored_decode_error)?;
    }
    Ok(())
}
fn decode_typed_text<T: Resource>(text: &str) -> Result<DecodedObject, ResourceError> {
    let object: K8sResourceObject<T> = serde_json::from_str(text).map_err(stored_decode_error)?;
    Ok(DecodedObject::typed(ResourceObject::from_k8s_object(object)?))
}
fn decode_typed_value<T: Resource>(value: &Value) -> Result<DecodedObject, ResourceError> {
    let object: K8sResourceObject<T> = serde::Deserialize::deserialize(value).map_err(stored_decode_error)?;
    Ok(DecodedObject::typed(ResourceObject::from_k8s_object(object)?))
}
impl ResourceKind {
    pub fn of<T: Resource>() -> Self {
        Self {
            paths: T::API_PATHS,
            replication: T::REPLICATION_CLASS,
            event_history: T::EVENT_HISTORY,
            queryable_fields: T::QUERYABLE_FIELDS,
            queryable_numeric_fields: T::QUERYABLE_NUMERIC_FIELDS,
            validate_namespace: T::VALIDATE_NAMESPACE_SPEC,
            validate_stored_fn: validate_stored::<T>,
            decode_typed_text_fn: decode_typed_text::<T>,
            decode_typed_value_fn: decode_typed_value::<T>,
            initial_status_fn: |meta| T::initial_status(meta)?.map(transcode).transpose(),
            validate_spec_fn: |meta, spec| T::validate_spec(meta, &transcode(spec)?),
            validate_spec_update_fn: |current, requested| T::validate_spec_update(&transcode(current)?, &transcode(requested)?),
            validate_status_update_fn: |current, requested| {
                T::validate_status_update(current.map(transcode).transpose()?.as_ref(), &transcode(requested)?)
            },
            validate_siblings_fn: |meta, spec, siblings| {
                T::validate_spec_with_named_siblings(
                    meta,
                    &transcode(spec)?,
                    &siblings.iter().map(transcode).collect::<Result<Vec<_>, _>>()?,
                )
            },
            digest_fn: PartitionDigest::new::<T>,
        }
    }
    /// Validate a stored tree against the registered type and expose it as
    /// serialized data. The spec and status trees are kept as stored, so
    /// defaults and unknown fields are not rewritten for raw callers.
    /// Validate replica admission by borrowing the raw body.
    pub fn validate_stored_object(self, value: &Value) -> Result<(), ResourceError> {
        self.decode_typed_value(value).map(|_| ())
    }
    pub fn decode_object(self, value: Value) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.from_k8s(serde_json::from_value(value).map_err(stored_decode_error)?)
    }
    /// [`Self::decode_object`] for a stored body: one parse and one typed
    /// validation pass.
    pub fn decode_object_text(self, text: &str) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.from_k8s(serde_json::from_str(text).map_err(stored_decode_error)?)
    }
    pub fn from_k8s(self, object: K8sResourceObject<SerializedResource>) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        (self.validate_stored_fn)(&object)?;
        Ok(ResourceObject { metadata: object.metadata.into(), spec: object.spec, status: object.status })
    }
    /// Decode a stored body once, straight into the registered type. A typed
    /// read uses this as its only decode, so it is also the quarantine check.
    pub fn decode_typed_text(self, text: &str) -> Result<DecodedObject, ResourceError> {
        (self.decode_typed_text_fn)(text)
    }
    /// Decode a stored tree once into the registered type, borrowing the tree.
    pub fn decode_typed_value(self, value: &Value) -> Result<DecodedObject, ResourceError> {
        (self.decode_typed_value_fn)(value)
    }
    pub fn encode_object(self, object: &ResourceObject<SerializedResource>) -> Result<Value, ResourceError> {
        serde_json::to_value(K8sResourceObject::<SerializedResource> {
            api_version: api_version(self.paths),
            kind: self.paths.kind.into(),
            metadata: K8sObjectMeta::from(&object.metadata),
            spec: object.spec.clone(),
            status: object.status.clone(),
        })
        .map_err(|e| ResourceError::decode(e.to_string()))
    }
    pub fn initial_status(self, meta: &InputMeta) -> Result<Option<Value>, ResourceError> {
        let status = (self.initial_status_fn)(meta)?;
        if let Some(status) = &status {
            self.validate_status_update(None, status)?;
        }
        Ok(status)
    }

    pub fn validate_spec(self, meta: &InputMeta, spec: &Value) -> Result<(), ResourceError> {
        (self.validate_spec_fn)(meta, spec)
    }
    pub fn validate_spec_update(self, current: &Value, requested: &Value) -> Result<(), ResourceError> {
        (self.validate_spec_update_fn)(current, requested)
    }
    pub fn validate_status_update(self, current: Option<&Value>, requested: &Value) -> Result<(), ResourceError> {
        (self.validate_status_update_fn)(current, requested)
    }
    pub fn validate_spec_with_named_siblings(
        self,
        meta: &InputMeta,
        spec: &Value,
        siblings: &[ResourceObject<SerializedResource>],
    ) -> Result<(), ResourceError> {
        (self.validate_siblings_fn)(meta, spec, siblings)
    }
    pub fn partition_digest(
        self,
        origin: NodeId,
        namespace: &str,
        generation: Option<String>,
        version: String,
        hashes: Vec<String>,
    ) -> PartitionDigest {
        (self.digest_fn)(origin, namespace, generation, version, hashes)
    }
    pub fn resource_list(self, list: K8sResourceList<SerializedResource>) -> Result<ResourceList<SerializedResource>, ResourceError> {
        Ok(ResourceList {
            items: list.items.into_iter().map(|o| self.from_k8s(o)).collect::<Result<_, _>>()?,
            resource_version: list.metadata.resource_version,
            generation: list.metadata.generation,
        })
    }
}

/// The shape of one store read, so per-request read budgets can tell an
/// indexed lookup from a scan of a whole kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReadOperation {
    /// Every object of a kind in one namespace: local rows, replica rows or
    /// both (`list_raw`, `list_replicas_raw`, `list_including_replicas_raw`,
    /// `list_replica_sources_raw`). Its cost grows with the whole kind.
    List,
    /// Rows selected by a declared query: indexed fields, labels, owners,
    /// names, Message history or a named read view.
    Query,
    /// Every visible provenance of one name.
    Get,
}

/// Observes reads at the adapter boundary: one call per store operation, with
/// the number of objects it decoded (or, for in-memory snapshots, visited).
pub trait ReadObserver: std::fmt::Debug + Send + Sync {
    fn read(&self, kind: &str, operation: ReadOperation, objects: usize);
    fn as_any(&self) -> &dyn std::any::Any;
}
pub fn convert_read<A: Resource, B: Resource>(item: ReadResourceObject<A>) -> Result<ReadResourceObject<B>, ResourceError> {
    Ok(ReadResourceObject { object: transcode(item.object)?, provenance: item.provenance })
}
pub fn convert_read_list<A: Resource, B: Resource>(list: ReadResourceList<A>) -> Result<ReadResourceList<B>, ResourceError> {
    Ok(ReadResourceList { items: list.items.into_iter().map(convert_read).collect::<Result<_, _>>()? })
}
pub fn convert_watch<A: Resource, B: Resource>(event: WatchEvent<A>) -> Result<WatchEvent<B>, ResourceError> {
    Ok(match event {
        WatchEvent::Added(o) => WatchEvent::Added(transcode(o)?),
        WatchEvent::Modified(o) => WatchEvent::Modified(transcode(o)?),
        WatchEvent::Deleted(o) => WatchEvent::Deleted(transcode(o)?),
        WatchEvent::DeletedByName(t) => WatchEvent::DeletedByName(t),
    })
}
pub fn convert_read_watch<A: Resource, B: Resource>(event: ReadWatchEvent<A>) -> Result<ReadWatchEvent<B>, ResourceError> {
    Ok(match event {
        ReadWatchEvent::Added(o) => ReadWatchEvent::Added(convert_read(o)?),
        ReadWatchEvent::Modified(o) => ReadWatchEvent::Modified(convert_read(o)?),
        ReadWatchEvent::Deleted(o) => ReadWatchEvent::Deleted(convert_read(o)?),
        ReadWatchEvent::DeletedByName { tombstone, provenance } => ReadWatchEvent::DeletedByName { tombstone, provenance },
    })
}

#[async_trait::async_trait]
/// Object-safe storage boundary. Requests carry serialized values and a kind
/// descriptor; adapters apply its validators under their authoritative lock or
/// transaction. Typed callers use [`StoreExt`] or [`crate::ResourceBackend`].
/// Observer and local-root builders return isolated handles sharing the data.
pub trait Store: std::fmt::Debug + Send + Sync {
    /// Adapter-specific wording may preserve an existing public refusal contract.
    fn unsupported_operation(&self, operation: &str) -> ResourceError {
        ResourceError::invalid(format!("this store does not support {operation}"))
    }
    fn with_local_root(&self, root: NodeId) -> Arc<dyn Store>;
    fn local_root(&self) -> Result<NodeId, ResourceError>;
    /// Remote views already merge definitions and provenance at their origin.
    fn serves_read_views(&self) -> bool {
        false
    }
    /// Preserve adapters whose serializers previously ran on a storage worker.
    fn blocking_request_encoding(&self) -> bool {
        false
    }
    fn durable_local(&self) -> bool {
        false
    }
    fn hierarchy_admission(&self) -> Option<Arc<tokio::sync::Mutex<()>>> {
        None
    }
    fn with_read_observer(&self, observer: Arc<dyn ReadObserver>) -> Arc<dyn Store>;
    fn read_observer(&self) -> Option<&dyn ReadObserver>;
    async fn diagnostics(&self) -> Result<Option<ResourceStoreDiagnostics>, ResourceError>;
    async fn record_field_ownership_violation(&self, violation: FieldOwnershipViolation) -> Result<(), ResourceError>;
    async fn query_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _query: &crate::StoreQuery,
    ) -> Result<crate::QueryResult, ResourceError> {
        Err(self.unsupported_operation("declared queries"))
    }
    /// Internal indexed projection used by composed stores to resolve semijoins.
    /// It returns field values without reading or decoding resource bodies.
    async fn query_values_raw(
        &self,
        _kind: ResourceKind,
        _namespace: &str,
        _query: &crate::StoreQuery,
        _field: &str,
    ) -> Result<std::collections::BTreeSet<String>, ResourceError> {
        Err(self.unsupported_operation("indexed field projections"))
    }
    async fn query_messages_raw(
        &self,
        namespace: &str,
        query: &crate::MessageQuery,
        include_replicas: bool,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError>;
    async fn query_message_replicas_raw(
        &self,
        namespace: &str,
        query: &crate::MessageQuery,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        Ok(self
            .query_messages_raw(namespace, query, true)
            .await?
            .into_iter()
            .filter(|item| matches!(item.provenance, ResourceProvenance::Replica { .. }))
            .collect())
    }

    async fn delete_decode_quarantine_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _name: &str,
    ) -> Result<bool, ResourceError> {
        Ok(false)
    }
    /// One store inventory for discovery; keys are registered plural names.
    async fn namespace_inventory(&self) -> Result<BTreeMap<String, Vec<String>>, ResourceError> {
        let mut inventory = BTreeMap::new();
        for kind in crate::declared_resource_kinds() {
            inventory.insert(kind.paths.plural.to_string(), self.stored_namespaces_raw(kind).await?);
        }
        Ok(inventory)
    }
    async fn local_namespaces_raw(&self, _resource_kind: ResourceKind) -> Result<Vec<String>, ResourceError> {
        Err(self.unsupported_operation("enumerating local resource namespaces"))
    }
    async fn stored_namespaces_raw(&self, _resource_kind: ResourceKind) -> Result<Vec<String>, ResourceError> {
        Err(self.unsupported_operation("enumerating stored resource namespaces"))
    }
    async fn list_replicas_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn get_replicas_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _name: &str,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn watch_replicas_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<crate::SerializedReadWatchEvent, ResourceError>>, ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn replace_replicas_raw(
        &self,
        _resource_kind: ResourceKind,
        _origin_root: &NodeId,
        _namespace: &str,
        _listed: &ResourceList<SerializedResource>,
        _synced_at: chrono::DateTime<Utc>,
        _bucket: Option<u8>,
    ) -> Result<(), ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    /// Apply a bounded group of replica events. Persistent backends commit once
    /// per batch; the default preserves the same event semantics in memory.
    async fn apply_replica_batch_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        events: &[(WatchEvent<SerializedResource>, chrono::DateTime<Utc>)],
    ) -> Result<(), ResourceError> {
        for (event, synced_at) in events {
            match event {
                WatchEvent::Added(object) => {
                    self.apply_replica_raw(resource_kind, origin_root, namespace, StoredReplicaEventKind::Added, object, *synced_at).await?
                }
                WatchEvent::Modified(object) => {
                    self.apply_replica_raw(resource_kind, origin_root, namespace, StoredReplicaEventKind::Modified, object, *synced_at)
                        .await?
                }
                WatchEvent::Deleted(object) => {
                    self.apply_replica_raw(resource_kind, origin_root, namespace, StoredReplicaEventKind::Deleted, object, *synced_at)
                        .await?
                }
                WatchEvent::DeletedByName(tombstone) => {
                    self.apply_replica_tombstone_raw(resource_kind, origin_root, namespace, tombstone, *synced_at).await?
                }
            }
        }
        Ok(())
    }
    async fn apply_replica_raw(
        &self,
        _resource_kind: ResourceKind,
        _origin_root: &NodeId,
        _namespace: &str,
        _kind: StoredReplicaEventKind,
        _object: &ResourceObject<SerializedResource>,
        _synced_at: chrono::DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn apply_replica_tombstone_raw(
        &self,
        _resource_kind: ResourceKind,
        _origin_root: &NodeId,
        _namespace: &str,
        _tombstone: &ResourceTombstone,
        _synced_at: chrono::DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn invalidate_replica_cursor_raw(
        &self,
        _resource_kind: ResourceKind,
        _origin_root: &NodeId,
        _namespace: &str,
    ) -> Result<(), ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn advance_replica_cursor_raw(
        &self,
        _resource_kind: ResourceKind,
        _origin_root: &NodeId,
        _namespace: &str,
        _previous: &ReplicaCursor,
        _next: &str,
    ) -> Result<(), ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn replica_cursor_raw(
        &self,
        _resource_kind: ResourceKind,
        _origin_root: &NodeId,
        _namespace: &str,
    ) -> Result<Option<ReplicaCursor>, ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn get_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError>;
    async fn current_position_raw(&self, resource_kind: ResourceKind, namespace: &str) -> Result<ResourcePosition, ResourceError>;
    async fn digest_raw(&self, resource_kind: ResourceKind, namespace: &str, query: &DigestQuery)
        -> Result<PartitionDigest, ResourceError>;
    async fn confirm_replica_digest_raw(
        &self,
        _resource_kind: ResourceKind,
        _origin: &NodeId,
        _namespace: &str,
        _proof: &PartitionDigest,
        _previous: &ReplicaCursor,
    ) -> Result<(), ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn replica_digest_raw(
        &self,
        _resource_kind: ResourceKind,
        _origin: &NodeId,
        _namespace: &str,
        _generation: Option<String>,
    ) -> Result<PartitionDigest, ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn list_raw(&self, resource_kind: ResourceKind, namespace: &str) -> Result<ResourceList<SerializedResource>, ResourceError>;
    async fn list_raw_matching_labels(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ResourceList<SerializedResource>, ResourceError>;
    async fn create_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError>;
    async fn create_definition_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _meta: &InputMeta,
        _spec: &Value,
        _merge: MergeMetadata,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        Err(self.unsupported_operation("authoring definitions"))
    }
    async fn update_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError>;
    async fn update_definition_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _meta: &InputMeta,
        _resource_version: &str,
        _spec: &Value,
        _merge: MergeMetadata,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        Err(self.unsupported_operation("authoring definitions"))
    }
    async fn update_status_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError>;
    async fn delete_raw(&self, resource_kind: ResourceKind, namespace: &str, name: &str) -> Result<(), ResourceError>;
    async fn tombstone_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        minimum_resource_version: Option<&str>,
    ) -> Result<crate::watch::TombstoneWrite, ResourceError>;
    async fn watch_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        start: WatchStart,
    ) -> Result<crate::SerializedWatchStream, ResourceError>;
    async fn list_including_replicas_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn list_including_replicas_raw_matching_labels(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _required: &BTreeMap<String, String>,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn get_including_replicas_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _name: &str,
    ) -> Result<ReadResourceObject<SerializedResource>, ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn get_all_including_replicas_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _name: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn list_replica_sources_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn list_read_view_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _query_key: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn watch_including_replicas_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<SerializedResource>, ResourceError>>, ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    /// Retained replica deletion fences for a relay's initial snapshot.
    /// Ordinary list/query APIs continue to return only live objects.
    async fn list_replica_deletions_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
    ) -> Result<Vec<crate::replica::ReplicaDeletion>, ResourceError> {
        Err(self.unsupported_operation("replica deletions"))
    }
    async fn watch_replica_sources_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<SerializedResource>, ResourceError>>, ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }
    async fn watch_read_view_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _query_key: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<SerializedResource>, ResourceError>>, ResourceError> {
        Err(self.unsupported_operation("replicas"))
    }

    // Typed reads. Each returns rows decoded once by `resource_kind`'s typed
    // decoder; [`StoreExt`] unwraps them for `T`. The defaults read the raw
    // form, which the kind has already validated, and leave one typed
    // deserialization of spec and status to the caller. Adapters that hold
    // stored text override them to decode each body once, straight into `T`.

    async fn get_decoded(&self, resource_kind: ResourceKind, namespace: &str, name: &str) -> Result<DecodedObject, ResourceError> {
        Ok(DecodedObject::serialized(self.get_raw(resource_kind, namespace, name).await?))
    }
    async fn list_decoded(&self, resource_kind: ResourceKind, namespace: &str) -> Result<DecodedList, ResourceError> {
        Ok(DecodedList::serialized(self.list_raw(resource_kind, namespace).await?))
    }
    async fn list_decoded_matching_labels(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<DecodedList, ResourceError> {
        Ok(DecodedList::serialized(self.list_raw_matching_labels(resource_kind, namespace, required).await?))
    }
    async fn list_replicas_decoded(&self, resource_kind: ResourceKind, namespace: &str) -> Result<Vec<DecodedReadObject>, ResourceError> {
        Ok(self.list_replicas_raw(resource_kind, namespace).await?.into_iter().map(DecodedReadObject::serialized).collect())
    }
    async fn get_replicas_decoded(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<DecodedReadObject>, ResourceError> {
        Ok(self.get_replicas_raw(resource_kind, namespace, name).await?.into_iter().map(DecodedReadObject::serialized).collect())
    }
    async fn query_decoded(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        query: &crate::StoreQuery,
    ) -> Result<DecodedQueryResult, ResourceError> {
        Ok(DecodedQueryResult::serialized(self.query_raw(resource_kind, namespace, query).await?))
    }
    async fn query_messages_decoded(
        &self,
        namespace: &str,
        query: &crate::MessageQuery,
        include_replicas: bool,
    ) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        self.query_messages_raw(namespace, query, include_replicas).await?.into_iter().map(typed_read).collect()
    }
    async fn query_message_replicas_decoded(
        &self,
        namespace: &str,
        query: &crate::MessageQuery,
    ) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        self.query_message_replicas_raw(namespace, query).await?.into_iter().map(typed_read).collect()
    }
    /// A watch whose replayed events may arrive as [`crate::WatchPayload::Decoded`]
    /// rows for `resource_kind`'s registered type. Only typed consumers
    /// ([`crate::SerializedWatchEvent::decode`]) may read it.
    async fn watch_decoded(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        start: WatchStart,
    ) -> Result<crate::SerializedWatchStream, ResourceError> {
        self.watch_raw(resource_kind, namespace, start).await
    }
}

/// Typed convenience layer over the untyped store. Backend implementations do
/// not need to enumerate resource types; the descriptor supplies validation.
#[async_trait::async_trait]
pub trait StoreExt: Store {
    async fn query_messages_typed(
        &self,
        namespace: &str,
        query: &crate::MessageQuery,
        include_replicas: bool,
    ) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        self.query_messages_decoded(namespace, query, include_replicas).await
    }
    async fn query_message_replicas(
        &self,
        namespace: &str,
        query: &crate::MessageQuery,
    ) -> Result<Vec<ReadResourceObject<Message>>, ResourceError> {
        self.query_message_replicas_decoded(namespace, query).await
    }
    async fn delete_decode_quarantine_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<bool, ResourceError> {
        Ok(self.delete_decode_quarantine_raw(ResourceKind::of::<T>(), namespace, name).await?)
    }
    async fn local_namespaces_typed<T: Resource>(&self) -> Result<Vec<String>, ResourceError> {
        Ok(self.local_namespaces_raw(ResourceKind::of::<T>()).await?)
    }
    async fn stored_namespaces_typed<T: Resource>(&self) -> Result<Vec<String>, ResourceError> {
        Ok(self.stored_namespaces_raw(ResourceKind::of::<T>()).await?)
    }
    async fn list_replicas_typed<T: Resource>(&self, namespace: &str) -> Result<Vec<ReadResourceObject<T>>, ResourceError> {
        self.list_replicas_decoded(ResourceKind::of::<T>(), namespace).await?.into_iter().map(DecodedReadObject::into_typed).collect()
    }
    async fn get_replicas_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<Vec<ReadResourceObject<T>>, ResourceError> {
        self.get_replicas_decoded(ResourceKind::of::<T>(), namespace, name).await?.into_iter().map(DecodedReadObject::into_typed).collect()
    }
    async fn watch_replicas_typed<T: Resource>(
        &self,
        namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>, ResourceError> {
        Ok(self
            .watch_replicas_raw(ResourceKind::of::<T>(), namespace)
            .await?
            .map(|e| e.and_then(crate::SerializedReadWatchEvent::decode))
            .boxed())
    }
    async fn replace_replicas_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
        listed: &ResourceList<T>,
        synced_at: chrono::DateTime<Utc>,
        bucket: Option<u8>,
    ) -> Result<(), ResourceError> {
        let listed = ResourceList::<T> {
            items: listed.items.iter().map(clone_object).collect(),
            resource_version: listed.resource_version.clone(),
            generation: listed.generation.clone(),
        };
        let listed = transcode_request(self.blocking_request_encoding(), listed).await?;
        Ok(self.replace_replicas_raw(ResourceKind::of::<T>(), origin_root, namespace, &listed, synced_at, bucket).await?)
    }
    async fn apply_replica_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
        kind: StoredReplicaEventKind,
        object: &ResourceObject<T>,
        synced_at: chrono::DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        let object = transcode_request(self.blocking_request_encoding(), clone_object(object)).await?;
        Ok(self.apply_replica_raw(ResourceKind::of::<T>(), origin_root, namespace, kind, &object, synced_at).await?)
    }
    async fn apply_replica_tombstone_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
        tombstone: &ResourceTombstone,
        synced_at: chrono::DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        Ok(self.apply_replica_tombstone_raw(ResourceKind::of::<T>(), origin_root, namespace, tombstone, synced_at).await?)
    }
    async fn invalidate_replica_cursor_typed<T: Resource>(&self, origin_root: &NodeId, namespace: &str) -> Result<(), ResourceError> {
        Ok(self.invalidate_replica_cursor_raw(ResourceKind::of::<T>(), origin_root, namespace).await?)
    }
    async fn advance_replica_cursor_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
        previous: &ReplicaCursor,
        next: &str,
    ) -> Result<(), ResourceError> {
        Ok(self.advance_replica_cursor_raw(ResourceKind::of::<T>(), origin_root, namespace, previous, next).await?)
    }
    async fn replica_cursor_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
    ) -> Result<Option<ReplicaCursor>, ResourceError> {
        Ok(self.replica_cursor_raw(ResourceKind::of::<T>(), origin_root, namespace).await?)
    }
    async fn get_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<ResourceObject<T>, ResourceError> {
        self.get_decoded(ResourceKind::of::<T>(), namespace, name).await?.into_typed()
    }
    async fn current_position_typed<T: Resource>(&self, namespace: &str) -> Result<crate::ResourcePosition, ResourceError> {
        Ok(self.current_position_raw(ResourceKind::of::<T>(), namespace).await?)
    }
    async fn digest_typed<T: Resource>(
        &self,
        namespace: &str,
        query: &crate::DigestQuery,
    ) -> Result<crate::PartitionDigest, ResourceError> {
        Ok(self.digest_raw(ResourceKind::of::<T>(), namespace, query).await?)
    }
    async fn confirm_replica_digest_typed<T: Resource>(
        &self,
        origin: &NodeId,
        namespace: &str,
        proof: &crate::PartitionDigest,
        previous: &ReplicaCursor,
    ) -> Result<(), ResourceError> {
        Ok(self.confirm_replica_digest_raw(ResourceKind::of::<T>(), origin, namespace, proof, previous).await?)
    }
    async fn replica_digest_typed<T: Resource>(
        &self,
        origin: &NodeId,
        namespace: &str,
        generation: Option<String>,
    ) -> Result<crate::PartitionDigest, ResourceError> {
        Ok(self.replica_digest_raw(ResourceKind::of::<T>(), origin, namespace, generation).await?)
    }
    async fn list_typed<T: Resource>(&self, namespace: &str) -> Result<ResourceList<T>, ResourceError> {
        self.list_decoded(ResourceKind::of::<T>(), namespace).await?.into_typed()
    }
    async fn list_typed_matching_labels<T: Resource>(
        &self,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ResourceList<T>, ResourceError> {
        self.list_decoded_matching_labels(ResourceKind::of::<T>(), namespace, required).await?.into_typed()
    }
    async fn create_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let spec = transcode_request(self.blocking_request_encoding(), spec.clone()).await?;
        typed_object(self.create_raw(ResourceKind::of::<T>(), namespace, meta, &spec).await?)
    }
    async fn create_definition_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        spec: &T::Spec,
        merge: MergeMetadata,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let spec = transcode_request(self.blocking_request_encoding(), spec.clone()).await?;
        typed_object(self.create_definition_raw(ResourceKind::of::<T>(), namespace, meta, &spec, merge).await?)
    }
    async fn update_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let spec = transcode_request(self.blocking_request_encoding(), spec.clone()).await?;
        typed_object(self.update_raw(ResourceKind::of::<T>(), namespace, meta, resource_version, &spec).await?)
    }
    async fn update_definition_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &T::Spec,
        merge: MergeMetadata,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let spec = transcode_request(self.blocking_request_encoding(), spec.clone()).await?;
        typed_object(self.update_definition_raw(ResourceKind::of::<T>(), namespace, meta, resource_version, &spec, merge).await?)
    }
    async fn update_status_typed<T: Resource>(
        &self,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &T::Status,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let status = transcode_request(self.blocking_request_encoding(), status.clone()).await?;
        typed_object(self.update_status_raw(ResourceKind::of::<T>(), namespace, name, resource_version, &status).await?)
    }
    async fn delete_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<(), ResourceError> {
        Ok(self.delete_raw(ResourceKind::of::<T>(), namespace, name).await?)
    }
    async fn tombstone_typed<T: Resource>(
        &self,
        namespace: &str,
        name: &str,
        minimum_resource_version: Option<&str>,
    ) -> Result<crate::watch::TombstoneWrite, ResourceError> {
        Ok(self.tombstone_raw(ResourceKind::of::<T>(), namespace, name, minimum_resource_version).await?)
    }
    async fn watch_typed<T: Resource>(&self, namespace: &str, start: WatchStart) -> Result<WatchStream<T>, ResourceError> {
        let stream = self.watch_decoded(ResourceKind::of::<T>(), namespace, start).await?;
        let generation = stream.generation().map(str::to_owned);
        Ok(WatchStream::new(generation, stream.map(|e| e.and_then(crate::SerializedWatchEvent::decode)).boxed()))
    }
    async fn list_including_replicas_typed<T: Resource>(&self, namespace: &str) -> Result<ReadResourceList<T>, ResourceError> {
        typed_read_list(self.list_including_replicas_raw(ResourceKind::of::<T>(), namespace).await?)
    }
    async fn list_including_replicas_typed_matching_labels<T: Resource>(
        &self,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ReadResourceList<T>, ResourceError> {
        typed_read_list(self.list_including_replicas_raw_matching_labels(ResourceKind::of::<T>(), namespace, required).await?)
    }
    async fn get_including_replicas_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<ReadResourceObject<T>, ResourceError> {
        typed_read(self.get_including_replicas_raw(ResourceKind::of::<T>(), namespace, name).await?)
    }
    async fn get_all_including_replicas_typed<T: Resource>(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<ReadResourceList<T>, ResourceError> {
        typed_read_list(self.get_all_including_replicas_raw(ResourceKind::of::<T>(), namespace, name).await?)
    }
    async fn list_replica_sources_typed<T: Resource>(&self, namespace: &str) -> Result<ReadResourceList<T>, ResourceError> {
        typed_read_list(self.list_replica_sources_raw(ResourceKind::of::<T>(), namespace).await?)
    }
    async fn list_read_view_typed<T: Resource>(&self, namespace: &str, query_key: &str) -> Result<ReadResourceList<T>, ResourceError> {
        typed_read_list(self.list_read_view_raw(ResourceKind::of::<T>(), namespace, query_key).await?)
    }
    async fn watch_including_replicas_typed<T: Resource>(
        &self,
        namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>, ResourceError> {
        Ok(self.watch_including_replicas_raw(ResourceKind::of::<T>(), namespace).await?.map(|e| e.and_then(typed_read_watch)).boxed())
    }
    async fn watch_replica_sources_typed<T: Resource>(
        &self,
        namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>, ResourceError> {
        Ok(self.watch_replica_sources_raw(ResourceKind::of::<T>(), namespace).await?.map(|e| e.and_then(typed_read_watch)).boxed())
    }
    async fn watch_read_view_typed<T: Resource>(
        &self,
        namespace: &str,
        query_key: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>, ResourceError> {
        Ok(self.watch_read_view_raw(ResourceKind::of::<T>(), namespace, query_key).await?.map(|e| e.and_then(typed_read_watch)).boxed())
    }
}
#[async_trait::async_trait]
impl<S: Store + ?Sized> StoreExt for S {}

fn clone_object<T: Resource>(object: &ResourceObject<T>) -> ResourceObject<T> {
    ResourceObject { metadata: object.metadata.clone(), spec: object.spec.clone(), status: object.status.clone() }
}
/// Preserve synchronous memory/HTTP encoding and SQLite worker-thread encoding.
async fn transcode_request<A: Serialize + Send + 'static, B: DeserializeOwned + Send + 'static>(
    blocking: bool,
    value: A,
) -> Result<B, ResourceError> {
    if !blocking {
        return transcode(value);
    }
    tokio::task::spawn_blocking(move || transcode(value)).await.map_err(|e| ResourceError::other(format!("encode store request: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RefusedInitialStatus;
    impl Resource for RefusedInitialStatus {
        type Spec = ();
        type Status = ();
        type StatusPatch = flotilla_resources::NoStatusPatch;
        const API_PATHS: ApiPaths = ApiPaths { group: "test", version: "v1", plural: "initialized", kind: "Initialized" };

        fn initial_status(_: &InputMeta) -> Result<Option<Self::Status>, ResourceError> {
            Ok(Some(()))
        }

        fn validate_status_update(current: Option<&Self::Status>, _: &Self::Status) -> Result<(), ResourceError> {
            assert!(current.is_none(), "initialization must validate as the first write");
            Err(ResourceError::invalid("initial status refused"))
        }
    }

    // A kind's initial status is subject to its status-write validation, even
    // though creation commits it together with the spec rather than via a patch.
    #[test]
    fn initial_status_cannot_bypass_status_validation() {
        let meta = InputMeta::builder().name("initialized".into()).build();
        assert!(matches!(ResourceKind::of::<RefusedInitialStatus>().initial_status(&meta),
            Err(ResourceError::Invalid { message }) if message == "initial status refused"));
    }

    struct PanickingSerializer;
    impl Serialize for PanickingSerializer {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            panic!("serializer panic fixture");
        }
    }

    // A serializer panic becomes a regular store error; it does not poison the
    // blocking pool or prevent a subsequent request from being encoded.
    #[tokio::test]
    async fn encoding_panic_is_an_error_and_the_next_request_succeeds() {
        let result = transcode_request::<_, Value>(true, PanickingSerializer).await;
        assert!(
            matches!(result, Err(ResourceError::Other { message }) if message.contains("encode store request") && message.contains("serializer panic fixture"))
        );
        let value = serde_json::json!({"spec":{"value":"next"}});
        assert_eq!(transcode_request::<_, Value>(true, value.clone()).await.expect("next request"), value);
    }
}
