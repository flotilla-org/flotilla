use std::{collections::BTreeMap, marker::PhantomData};

use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;
use futures::{stream, StreamExt};

use crate::{
    definition::DefinitionResolver,
    error::ResourceError,
    http::HttpBackend,
    in_memory::InMemoryBackend,
    replica::{ReadResourceList, ReadResourceObject, ReadWatchEvent, ReplicaCursor, ResourceProvenance, StoredReplicaEventKind},
    resource::{InputMeta, Resource, ResourceObject},
    retention::ResourceStoreDiagnostics,
    sqlite::SqliteBackend,
    watch::{ResourceList, WatchStart, WatchStream},
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

    pub fn definitions<T: Resource>(&self, namespace: &str) -> DefinitionResolver<T> {
        DefinitionResolver::new(self.clone(), namespace.to_string())
    }

    /// A read-only union of locally-authored objects and durable replicas.
    ///
    /// This deliberately returns a different resolver type with no mutation
    /// methods, so controllers cannot accidentally reconcile replica rows.
    pub fn including_replicas<T: Resource>(&self, namespace: &str) -> ReplicaReadResolver<T> {
        ReplicaReadResolver { backend: self.clone(), namespace: namespace.to_string(), _marker: PhantomData }
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
}

#[derive(Debug, Clone)]
pub struct ReplicaReadResolver<T: Resource> {
    backend: ResourceBackend,
    namespace: String,
    _marker: PhantomData<T>,
}

impl<T: Resource> ReplicaReadResolver<T> {
    pub async fn list(&self) -> Result<ReadResourceList<T>, ResourceError> {
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
        self.list_sources().await
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

    pub async fn watch(&self) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>, ResourceError> {
        ensure_replication_enabled::<T>()?;
        if let ResourceBackend::Http(backend) = &self.backend {
            return backend.watch_including_replicas_typed::<T>(&self.namespace).await;
        }
        let raw = self.watch_sources().await?;
        if T::REPLICATION_CLASS != crate::ReplicationClass::Definitions {
            return Ok(raw);
        }
        let backend = self.backend.clone();
        let namespace = self.namespace.clone();
        Ok(raw
            .then(move |event| {
                let backend = backend.clone();
                let namespace = namespace.clone();
                async move {
                    let event = event?;
                    let fallback = match event {
                        ReadWatchEvent::Added(object) | ReadWatchEvent::Modified(object) | ReadWatchEvent::Deleted(object) => object,
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

    pub async fn watch(&self, start: WatchStart) -> Result<WatchStream<T>, ResourceError> {
        dispatch_backend!(self, watch_typed, start)
    }
}
