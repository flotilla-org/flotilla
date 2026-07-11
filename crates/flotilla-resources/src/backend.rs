use std::{collections::BTreeMap, marker::PhantomData};

use crate::{
    error::ResourceError,
    http::HttpBackend,
    in_memory::InMemoryBackend,
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
    pub fn using<T: Resource>(&self, namespace: &str) -> TypedResolver<T> {
        TypedResolver { backend: self.clone(), namespace: namespace.to_string(), _marker: PhantomData }
    }

    pub async fn diagnostics(&self) -> Result<Option<ResourceStoreDiagnostics>, ResourceError> {
        match self {
            Self::InMemory(backend) => backend.diagnostics().await.map(Some),
            Self::Http(_) => Ok(None),
            Self::Sqlite(backend) => backend.diagnostics().await.map(Some),
        }
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
        dispatch_backend!(self, create_typed, meta, spec)
    }

    pub async fn update(&self, meta: &InputMeta, resource_version: &str, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
        dispatch_backend!(self, update_typed, meta, resource_version, spec)
    }

    pub async fn update_status(&self, name: &str, resource_version: &str, status: &T::Status) -> Result<ResourceObject<T>, ResourceError> {
        dispatch_backend!(self, update_status_typed, name, resource_version, status)
    }

    pub async fn delete(&self, name: &str) -> Result<(), ResourceError> {
        dispatch_backend!(self, delete_typed, name)
    }

    pub async fn watch(&self, start: WatchStart) -> Result<WatchStream<T>, ResourceError> {
        dispatch_backend!(self, watch_typed, start)
    }
}
