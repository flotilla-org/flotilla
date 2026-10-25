// Storage boundary: a real memory store that refuses reads until virtual time advances.
use async_trait::async_trait;
use flotilla_protocol::NodeId;
use flotilla_resources::*;
use flotilla_store::*;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, OnceLock,
    },
};

#[derive(Debug)]
pub(crate) struct BusyStore {
    pub inner: Arc<dyn Store>,
    pub armed: Arc<AtomicBool>,
    pub busy_since: Arc<OnceLock<tokio::time::Instant>>,
    pub refusals: Arc<AtomicUsize>,
}
impl BusyStore {
    fn busy(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
            && self.busy_since.get_or_init(tokio::time::Instant::now).elapsed() < std::time::Duration::from_secs(360)
    }
}
#[async_trait]
impl Store for BusyStore {
    fn with_local_root(&self, root: NodeId) -> Arc<dyn Store> {
        Arc::new(Self {
            inner: self.inner.with_local_root(root),
            armed: Arc::clone(&self.armed),
            busy_since: Arc::clone(&self.busy_since),
            refusals: Arc::clone(&self.refusals),
        })
    }
    fn local_root(&self) -> Result<NodeId, ResourceError> {
        self.inner.local_root()
    }
    fn with_read_observer(&self, observer: Arc<dyn ReadObserver>) -> Arc<dyn Store> {
        Arc::new(Self {
            inner: self.inner.with_read_observer(observer),
            armed: Arc::clone(&self.armed),
            busy_since: Arc::clone(&self.busy_since),
            refusals: Arc::clone(&self.refusals),
        })
    }
    fn read_observer(&self) -> Option<&dyn ReadObserver> {
        self.inner.read_observer()
    }
    async fn diagnostics(&self) -> Result<Option<ResourceStoreDiagnostics>, ResourceError> {
        self.inner.diagnostics().await
    }
    async fn record_field_ownership_violation(&self, violation: FieldOwnershipViolation) -> Result<(), ResourceError> {
        self.inner.record_field_ownership_violation(violation).await
    }
    async fn query_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _query: &flotilla_store::StoreQuery,
    ) -> Result<flotilla_store::QueryResult, ResourceError> {
        self.inner.query_raw(_resource_kind, _namespace, _query).await
    }
    async fn query_messages_raw(
        &self,
        namespace: &str,
        query: &flotilla_store::MessageQuery,
        include_replicas: bool,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        self.inner.query_messages_raw(namespace, query, include_replicas).await
    }
    async fn list_replicas_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        self.inner.list_replicas_raw(_resource_kind, _namespace).await
    }
    async fn get_replicas_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _name: &str,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        self.inner.get_replicas_raw(_resource_kind, _namespace, _name).await
    }
    async fn get_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        if self.busy() {
            self.refusals.fetch_add(1, Ordering::Relaxed);
            return Err(ResourceError::store_busy_read_deadline());
        }
        self.inner.get_raw(resource_kind, namespace, name).await
    }
    async fn current_position_raw(&self, resource_kind: ResourceKind, namespace: &str) -> Result<ResourcePosition, ResourceError> {
        self.inner.current_position_raw(resource_kind, namespace).await
    }
    async fn digest_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        query: &DigestQuery,
    ) -> Result<PartitionDigest, ResourceError> {
        self.inner.digest_raw(resource_kind, namespace, query).await
    }
    async fn list_raw(&self, resource_kind: ResourceKind, namespace: &str) -> Result<ResourceList<SerializedResource>, ResourceError> {
        if self.busy() {
            self.refusals.fetch_add(1, Ordering::Relaxed);
            return Err(ResourceError::store_busy_read_deadline());
        }
        self.inner.list_raw(resource_kind, namespace).await
    }
    async fn list_raw_matching_labels(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ResourceList<SerializedResource>, ResourceError> {
        if self.busy() {
            self.refusals.fetch_add(1, Ordering::Relaxed);
            return Err(ResourceError::store_busy_read_deadline());
        }
        self.inner.list_raw_matching_labels(resource_kind, namespace, required).await
    }
    async fn create_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.inner.create_raw(resource_kind, namespace, meta, spec).await
    }
    async fn create_definition_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _meta: &InputMeta,
        _spec: &Value,
        _merge: MergeMetadata,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.inner.create_definition_raw(_resource_kind, _namespace, _meta, _spec, _merge).await
    }
    async fn update_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.inner.update_raw(resource_kind, namespace, meta, resource_version, spec).await
    }
    async fn update_definition_raw(
        &self,
        _resource_kind: ResourceKind,
        _namespace: &str,
        _meta: &InputMeta,
        _resource_version: &str,
        _spec: &Value,
        _merge: MergeMetadata,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.inner.update_definition_raw(_resource_kind, _namespace, _meta, _resource_version, _spec, _merge).await
    }
    async fn update_status_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.inner.update_status_raw(resource_kind, namespace, name, resource_version, status).await
    }
    async fn delete_raw(&self, resource_kind: ResourceKind, namespace: &str, name: &str) -> Result<(), ResourceError> {
        self.inner.delete_raw(resource_kind, namespace, name).await
    }
    async fn tombstone_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        minimum_resource_version: Option<&str>,
    ) -> Result<flotilla_store::watch::TombstoneWrite, ResourceError> {
        self.inner.tombstone_raw(resource_kind, namespace, name, minimum_resource_version).await
    }
    async fn watch_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        start: WatchStart,
    ) -> Result<flotilla_store::SerializedWatchStream, ResourceError> {
        self.inner.watch_raw(resource_kind, namespace, start).await
    }
}
