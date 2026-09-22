use std::{
    collections::{BTreeMap, HashSet},
    marker::PhantomData,
};

use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;
use futures::{stream, StreamExt};

use crate::{
    definition::DefinitionResolver,
    error::ResourceError,
    field_ownership::merge_owned_spec,
    http::HttpBackend,
    in_memory::InMemoryBackend,
    replica::{ReadResourceList, ReadResourceObject, ReadWatchEvent, ReplicaCursor, ResourceProvenance, StoredReplicaEventKind},
    resource::{InputMeta, Resource, ResourceObject},
    retention::ResourceStoreDiagnostics,
    sqlite::SqliteBackend,
    watch::{ResourceList, WatchStart, WatchStream},
    FieldOwnedResource, FieldOwnershipViolation, OwnershipEnforcement, WriterIdentity,
};

macro_rules! dispatch_backend {
    ($self:expr, $method:ident $(, $args:expr)*) => {
        match &$self.backend {
            ResourceBackend::InMemory(backend) => backend.$method::<T>(&$self.namespace $(, $args)*).await,
            ResourceBackend::Http(backend) => backend.$method::<T>(&$self.namespace $(, $args)*).await,
            ResourceBackend::Sqlite(backend) => backend.$method::<T>(&$self.namespace $(, $args)*).await,
        }
    };
}

#[derive(Debug, Clone)]
pub enum ResourceBackend {
    InMemory(InMemoryBackend),
    Http(HttpBackend),
    Sqlite(SqliteBackend),
}

impl ResourceBackend {
    pub fn with_local_root(self, local_root: NodeId) -> Self {
        match self {
            Self::InMemory(backend) => Self::InMemory(backend.with_local_root(local_root)),
            Self::Http(backend) => Self::Http(backend),
            Self::Sqlite(backend) => Self::Sqlite(backend.with_local_root(local_root)),
        }
    }

    pub(crate) fn local_root(&self) -> Result<NodeId, ResourceError> {
        match self {
            Self::InMemory(backend) => Ok(backend.local_root()),
            Self::Sqlite(backend) => Ok(backend.local_root()),
            Self::Http(_) => Err(ResourceError::invalid("HTTP backends cannot author definitions")),
        }
    }

    pub fn using<T: Resource>(&self, namespace: &str) -> TypedResolver<T> {
        TypedResolver { backend: self.clone(), namespace: namespace.to_string(), _marker: PhantomData }
    }

    /// Namespaces containing locally-authored objects of this kind.
    pub async fn local_namespaces<T: Resource>(&self) -> Result<Vec<String>, ResourceError> {
        match self {
            Self::InMemory(backend) => backend.local_namespaces_typed::<T>().await,
            Self::Sqlite(backend) => backend.local_namespaces_typed::<T>().await,
            Self::Http(_) => Err(ResourceError::invalid("HTTP backends cannot enumerate local resource namespaces")),
        }
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
        match self {
            Self::InMemory(backend) => backend.diagnostics().await.map(Some),
            Self::Http(_) => Ok(None),
            Self::Sqlite(backend) => backend.diagnostics().await.map(Some),
        }
    }

    pub(crate) async fn delete_decode_quarantine<T: Resource>(&self, namespace: &str, name: &str) -> Result<bool, ResourceError> {
        match self {
            Self::Sqlite(backend) => backend.delete_decode_quarantine_typed::<T>(namespace, name).await,
            Self::InMemory(_) | Self::Http(_) => Ok(false),
        }
    }

    async fn record_field_ownership_violation(&self, violation: FieldOwnershipViolation) -> Result<(), ResourceError> {
        match self {
            Self::InMemory(backend) => {
                backend.record_field_ownership_violation(violation).await;
                Ok(())
            }
            Self::Sqlite(backend) => backend.record_field_ownership_violation(violation).await,
            // HTTP clients cannot publish authoritative host diagnostics. The
            // enrolled write paths currently execute against embedded stores.
            Self::Http(_) => Ok(()),
        }
    }
}

#[derive(Debug)]
pub struct ReplicaReadResolver<T: Resource> {
    backend: ResourceBackend,
    namespace: String,
    suppress_self_origin: bool,
    _marker: PhantomData<T>,
}

impl<T: Resource> Clone for ReplicaReadResolver<T> {
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
    #[cfg(feature = "test-support")]
    pub fn with_self_origin_suppression_disabled_for_test(mut self) -> Self {
        self.suppress_self_origin = false;
        self
    }

    pub async fn get(&self, name: &str) -> Result<ReadResourceObject<T>, ResourceError> {
        if T::REPLICATION_CLASS == crate::ReplicationClass::None {
            return self
                .backend
                .using::<T>(&self.namespace)
                .get(name)
                .await
                .map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local });
        }
        ensure_replication_enabled::<T>()?;
        if let ResourceBackend::Http(backend) = &self.backend {
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
                let replicas = match &self.backend {
                    ResourceBackend::InMemory(backend) => backend.get_replicas_typed::<T>(&self.namespace, name).await?,
                    ResourceBackend::Sqlite(backend) => backend.get_replicas_typed::<T>(&self.namespace, name).await?,
                    ResourceBackend::Http(_) => unreachable!("HTTP handled above"),
                };
                replicas.into_iter().next().ok_or_else(|| ResourceError::not_found(name))
            }
            Err(error) => Err(error),
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
        if let ResourceBackend::Http(backend) = &self.backend {
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
        if let ResourceBackend::Http(backend) = &self.backend {
            return backend.list_including_replicas_typed_matching_labels::<T>(&self.namespace, required).await;
        }
        let local = self.backend.using::<T>(&self.namespace).list_matching_labels(required).await?;
        let mut items =
            local.items.into_iter().map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local }).collect::<Vec<_>>();
        let mut replicas = match &self.backend {
            ResourceBackend::InMemory(backend) => backend.list_replicas_typed::<T>(&self.namespace).await?,
            ResourceBackend::Sqlite(backend) => backend.list_replicas_typed::<T>(&self.namespace).await?,
            ResourceBackend::Http(_) => unreachable!("HTTP handled above"),
        };
        replicas.retain(|item| required.iter().all(|(key, expected)| item.object.metadata.labels.get(key) == Some(expected)));
        items.extend(replicas);
        if self.suppress_self_origin {
            let local_root = self.backend.local_root()?;
            let local_names = items
                .iter()
                .filter(|item| matches!(item.provenance, ResourceProvenance::Local))
                .map(|item| item.object.metadata.name.clone())
                .collect::<HashSet<_>>();
            items.retain(|item| {
                !matches!(
                    &item.provenance,
                    ResourceProvenance::Replica { origin_root, .. }
                        if origin_root == &local_root && local_names.contains(&item.object.metadata.name)
                )
            });
        }
        Ok(ReadResourceList { items })
    }

    pub(crate) async fn list_sources(&self) -> Result<ReadResourceList<T>, ResourceError> {
        ensure_replication_enabled::<T>()?;
        if matches!(&self.backend, ResourceBackend::Http(_)) {
            return Err(ResourceError::invalid("HTTP replica-source lists use the resource API"));
        }
        let local = self.backend.using::<T>(&self.namespace).list().await?;
        let mut items =
            local.items.into_iter().map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local }).collect::<Vec<_>>();
        let replicas = match &self.backend {
            ResourceBackend::InMemory(backend) => backend.list_replicas_typed::<T>(&self.namespace).await?,
            ResourceBackend::Sqlite(backend) => backend.list_replicas_typed::<T>(&self.namespace).await?,
            ResourceBackend::Http(_) => unreachable!("HTTP handled above"),
        };
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
        if let ResourceBackend::Http(backend) = &self.backend {
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
                    let event = event?;
                    if let ReadWatchEvent::DeletedByName { tombstone, provenance } = event {
                        return match backend.definitions::<T>(&namespace).get(&tombstone.name).await {
                            Ok(object) => {
                                Ok(ReadWatchEvent::Modified(ReadResourceObject { object, provenance: ResourceProvenance::Local }))
                            }
                            Err(ResourceError::NotFound { .. }) => Ok(ReadWatchEvent::DeletedByName { tombstone, provenance }),
                            Err(error) => Err(error),
                        };
                    }
                    let fallback = match event {
                        ReadWatchEvent::Added(object) | ReadWatchEvent::Modified(object) | ReadWatchEvent::Deleted(object) => object,
                        ReadWatchEvent::DeletedByName { .. } => unreachable!("handled above"),
                    };
                    match backend.definitions::<T>(&namespace).get(&fallback.object.metadata.name).await {
                        Ok(object) => Ok(ReadWatchEvent::Modified(ReadResourceObject { object, provenance: ResourceProvenance::Local })),
                        Err(ResourceError::NotFound { .. }) => Ok(ReadWatchEvent::Deleted(fallback)),
                        Err(error) => Err(error),
                    }
                }
            })
            .boxed())
    }

    pub(crate) async fn watch_sources(
        &self,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>, ResourceError> {
        ensure_replication_enabled::<T>()?;
        if matches!(&self.backend, ResourceBackend::Http(_)) {
            return Err(ResourceError::invalid("HTTP replica-source watches use the resource API"));
        }
        let replicas = match &self.backend {
            ResourceBackend::InMemory(backend) => backend.watch_replicas_typed::<T>(&self.namespace).await?,
            ResourceBackend::Sqlite(backend) => backend.watch_replicas_typed::<T>(&self.namespace).await?,
            ResourceBackend::Http(_) => unreachable!("HTTP handled above"),
        };
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
        match &self.backend {
            ResourceBackend::InMemory(backend) => {
                backend.replace_replicas_typed(&self.origin_root, &self.namespace, listed, synced_at).await
            }
            ResourceBackend::Sqlite(backend) => backend.replace_replicas_typed(&self.origin_root, &self.namespace, listed, synced_at).await,
            ResourceBackend::Http(_) => Err(ResourceError::invalid("HTTP backends cannot hold replicas")),
        }
    }

    pub async fn apply(&self, event: crate::WatchEvent<T>, synced_at: DateTime<Utc>) -> Result<(), ResourceError> {
        ensure_replication_enabled::<T>()?;
        let (kind, object) = match event {
            crate::WatchEvent::Added(object) => (StoredReplicaEventKind::Added, object),
            crate::WatchEvent::Modified(object) => (StoredReplicaEventKind::Modified, object),
            crate::WatchEvent::Deleted(object) => (StoredReplicaEventKind::Deleted, object),
            crate::WatchEvent::DeletedByName(tombstone) => {
                return match &self.backend {
                    ResourceBackend::InMemory(backend) => {
                        backend.apply_replica_tombstone_typed::<T>(&self.origin_root, &self.namespace, &tombstone, synced_at).await
                    }
                    ResourceBackend::Sqlite(backend) => {
                        backend.apply_replica_tombstone_typed::<T>(&self.origin_root, &self.namespace, &tombstone, synced_at).await
                    }
                    ResourceBackend::Http(_) => Err(ResourceError::invalid("HTTP backends cannot hold replicas")),
                };
            }
        };
        match &self.backend {
            ResourceBackend::InMemory(backend) => {
                backend.apply_replica_typed(&self.origin_root, &self.namespace, kind, &object, synced_at).await
            }
            ResourceBackend::Sqlite(backend) => {
                backend.apply_replica_typed(&self.origin_root, &self.namespace, kind, &object, synced_at).await
            }
            ResourceBackend::Http(_) => Err(ResourceError::invalid("HTTP backends cannot hold replicas")),
        }
    }

    pub async fn cursor(&self) -> Result<Option<ReplicaCursor>, ResourceError> {
        ensure_replication_enabled::<T>()?;
        match &self.backend {
            ResourceBackend::InMemory(backend) => backend.replica_cursor_typed::<T>(&self.origin_root, &self.namespace).await,
            ResourceBackend::Sqlite(backend) => backend.replica_cursor_typed::<T>(&self.origin_root, &self.namespace).await,
            ResourceBackend::Http(_) => Err(ResourceError::invalid("HTTP backends cannot hold replicas")),
        }
    }
}

fn ensure_replication_enabled<T: Resource>() -> Result<(), ResourceError> {
    if T::REPLICATION_CLASS != crate::ReplicationClass::None {
        Ok(())
    } else {
        Err(ResourceError::invalid(format!("{} is not enabled for overlay replication", T::API_PATHS.kind)))
    }
}

#[derive(Debug)]
pub struct TypedResolver<T: Resource> {
    pub(crate) backend: ResourceBackend,
    pub(crate) namespace: String,
    pub(crate) _marker: PhantomData<T>,
}

impl<T: Resource> Clone for TypedResolver<T> {
    fn clone(&self) -> Self {
        Self { backend: self.backend.clone(), namespace: self.namespace.clone(), _marker: PhantomData }
    }
}

impl<T: Resource> TypedResolver<T> {
    pub async fn get(&self, name: &str) -> Result<ResourceObject<T>, ResourceError> {
        dispatch_backend!(self, get_typed, name)
    }

    pub async fn list(&self) -> Result<ResourceList<T>, ResourceError> {
        dispatch_backend!(self, list_typed)
    }

    pub async fn list_matching_labels(&self, required: &BTreeMap<String, String>) -> Result<ResourceList<T>, ResourceError> {
        dispatch_backend!(self, list_typed_matching_labels, required)
    }

    pub async fn create(&self, meta: &InputMeta, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
        if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            return self.backend.definitions::<T>(&self.namespace).create(meta, spec).await;
        }
        dispatch_backend!(self, create_typed, meta, spec)
    }

    pub async fn update(&self, meta: &InputMeta, resource_version: &str, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
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

    pub async fn update_status(&self, name: &str, resource_version: &str, status: &T::Status) -> Result<ResourceObject<T>, ResourceError> {
        dispatch_backend!(self, update_status_typed, name, resource_version, status)
    }

    pub async fn delete(&self, name: &str) -> Result<(), ResourceError> {
        if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            return self.backend.definitions::<T>(&self.namespace).delete(name).await;
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
        let current = self.get(&meta.name).await?;
        let (merged, violations) = merge_owned_spec::<T>(&current.spec, requested, writer, &self.namespace, &meta.name)?;
        for violation in &violations {
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
        if !violations.is_empty() && T::OWNERSHIP_ENFORCEMENT == OwnershipEnforcement::Enforce {
            return Err(ResourceError::FieldOwnership { violations });
        }
        self.update(meta, resource_version, &merged).await
    }
}
