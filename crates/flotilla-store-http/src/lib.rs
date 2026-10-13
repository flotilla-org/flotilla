use flotilla_protocol::NodeId;
use flotilla_resources::*;
use flotilla_resources::{error, resource};
use flotilla_store::*;
use flotilla_store::{replica, watch};
use serde_json::Value;
use std::collections::BTreeMap;
mod http;
pub use http::*;

#[async_trait::async_trait]
impl Store for HttpBackend {
    fn unsupported_operation(&self, operation: &str) -> ResourceError {
        let message = match operation {
            "replicas" => "HTTP backends cannot hold replicas".to_string(),
            "authoring definitions" => "HTTP backends cannot author definitions".to_string(),
            "enumerating local resource namespaces" => "HTTP backends cannot enumerate local resource namespaces".to_string(),
            "enumerating stored resource namespaces" => "HTTP backends cannot enumerate stored resource namespaces".to_string(),
            other => format!("this store does not support {other}"),
        };
        ResourceError::invalid(message)
    }
    fn with_local_root(&self, _root: NodeId) -> std::sync::Arc<dyn Store> {
        std::sync::Arc::new(self.clone())
    }
    fn local_root(&self) -> Result<NodeId, ResourceError> {
        Err(ResourceError::invalid("HTTP backends cannot author definitions"))
    }
    fn serves_read_views(&self) -> bool {
        true
    }
    fn with_read_observer(&self, observer: std::sync::Arc<dyn ReadObserver>) -> std::sync::Arc<dyn Store> {
        std::sync::Arc::new(self.clone().with_read_observer(observer))
    }
    fn read_observer(&self) -> Option<&dyn ReadObserver> {
        self.read_observer()
    }
    async fn diagnostics(&self) -> Result<Option<ResourceStoreDiagnostics>, ResourceError> {
        Ok(None)
    }
    async fn record_field_ownership_violation(&self, _violation: FieldOwnershipViolation) -> Result<(), ResourceError> {
        Ok(())
    }
    async fn query_raw(&self, kind: ResourceKind, namespace: &str, query: &StoreQuery) -> Result<QueryResult, ResourceError> {
        self.query_declared(kind, namespace, query).await
    }
    async fn query_messages_raw(
        &self,
        namespace: &str,
        query: &MessageQuery,
        include_replicas: bool,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        let result = self.query_messages(namespace, query, include_replicas).await?;
        if let Some(observer) = self.read_observer() {
            observer.read("Message", ReadOperation::Query, result.len());
        }
        result.into_iter().map(convert_read).collect()
    }
    async fn get_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        let result = HttpBackend::get_raw(self, resource_kind, namespace, name).await;
        if let Some(observer) = self.read_observer() {
            // Missing, transport and decode errors still cost a crossing, but
            // return zero objects to the caller.
            observer.read(resource_kind.paths.kind, ReadOperation::Get, usize::from(result.is_ok()));
        }
        result
    }
    async fn current_position_raw(&self, resource_kind: ResourceKind, namespace: &str) -> Result<ResourcePosition, ResourceError> {
        HttpBackend::current_position_raw(self, resource_kind, namespace).await
    }
    async fn digest_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        query: &DigestQuery,
    ) -> Result<PartitionDigest, ResourceError> {
        HttpBackend::digest_raw(self, resource_kind, namespace, query).await
    }
    async fn list_raw(&self, resource_kind: ResourceKind, namespace: &str) -> Result<ResourceList<SerializedResource>, ResourceError> {
        let result = HttpBackend::list_raw(self, resource_kind, namespace).await?;
        if let Some(observer) = self.read_observer() {
            observer.read(resource_kind.paths.kind, ReadOperation::List, result.items.len());
        }
        Ok(result)
    }
    async fn list_raw_matching_labels(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ResourceList<SerializedResource>, ResourceError> {
        let result = HttpBackend::list_raw_matching_labels(self, resource_kind, namespace, required).await?;
        if let Some(observer) = self.read_observer() {
            observer.read(resource_kind.paths.kind, ReadOperation::Query, result.items.len());
        }
        Ok(result)
    }
    async fn create_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        HttpBackend::create_raw(self, resource_kind, namespace, meta, spec).await
    }
    async fn update_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        HttpBackend::update_raw(self, resource_kind, namespace, meta, resource_version, spec).await
    }
    async fn update_status_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        HttpBackend::update_status_raw(self, resource_kind, namespace, name, resource_version, status).await
    }
    async fn delete_raw(&self, resource_kind: ResourceKind, namespace: &str, name: &str) -> Result<(), ResourceError> {
        HttpBackend::delete_raw(self, resource_kind, namespace, name).await
    }
    async fn tombstone_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        minimum_resource_version: Option<&str>,
    ) -> Result<watch::TombstoneWrite, ResourceError> {
        HttpBackend::tombstone_raw(self, resource_kind, namespace, name, minimum_resource_version).await
    }
    async fn watch_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        start: WatchStart,
    ) -> Result<SerializedWatchStream, ResourceError> {
        {
            use futures::StreamExt;
            let stream = HttpBackend::watch_raw(self, resource_kind, namespace, start).await?;
            let generation = stream.generation().map(str::to_owned);
            Ok(SerializedWatchStream::new(
                generation,
                stream.map(move |event| event.and_then(|event| SerializedWatchEvent::from_raw(resource_kind, event))).boxed(),
            ))
        }
    }
    async fn list_including_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        let result = HttpBackend::list_including_replicas_raw(self, resource_kind, namespace).await?;
        if let Some(observer) = self.read_observer() {
            observer.read(resource_kind.paths.kind, ReadOperation::List, result.items.len());
        }
        Ok(result)
    }
    async fn list_including_replicas_raw_matching_labels(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        let result = HttpBackend::list_including_replicas_raw_matching_labels(self, resource_kind, namespace, required).await?;
        if let Some(observer) = self.read_observer() {
            observer.read(resource_kind.paths.kind, ReadOperation::Query, result.items.len());
        }
        Ok(result)
    }
    async fn get_including_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ReadResourceObject<SerializedResource>, ResourceError> {
        let result = HttpBackend::get_including_replicas_raw(self, resource_kind, namespace, name).await;
        if let Some(observer) = self.read_observer() {
            // Count error responses as crossings with zero returned objects,
            // just like a local get (including transport and decode errors).
            observer.read(resource_kind.paths.kind, ReadOperation::Get, usize::from(result.is_ok()));
        }
        result
    }
    async fn get_all_including_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        let result = HttpBackend::get_all_including_replicas_raw(self, resource_kind, namespace, name).await?;
        if let Some(observer) = self.read_observer() {
            observer.read(resource_kind.paths.kind, ReadOperation::Get, result.items.len());
        }
        Ok(result)
    }
    async fn list_replica_sources_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        let result = HttpBackend::list_replica_sources_raw(self, resource_kind, namespace).await?;
        if let Some(observer) = self.read_observer() {
            observer.read(resource_kind.paths.kind, ReadOperation::List, result.items.len());
        }
        Ok(result)
    }
    async fn list_read_view_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        query_key: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        let result = HttpBackend::list_read_view_raw(self, resource_kind, namespace, query_key).await?;
        if let Some(observer) = self.read_observer() {
            observer.read(resource_kind.paths.kind, ReadOperation::Query, result.items.len());
        }
        Ok(result)
    }
    async fn watch_including_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<SerializedResource>, ResourceError>>, ResourceError> {
        HttpBackend::watch_including_replicas_raw(self, resource_kind, namespace).await
    }
    async fn watch_replica_sources_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<SerializedResource>, ResourceError>>, ResourceError> {
        HttpBackend::watch_replica_sources_raw(self, resource_kind, namespace).await
    }
    async fn watch_read_view_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        query_key: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<SerializedResource>, ResourceError>>, ResourceError> {
        HttpBackend::watch_read_view_raw(self, resource_kind, namespace, query_key).await
    }
}
