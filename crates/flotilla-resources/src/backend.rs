use std::marker::PhantomData;

use crate::{
    error::ResourceError,
    http::HttpBackend,
    in_memory::InMemoryBackend,
    resource::{InputMeta, Resource, ResourceObject},
    watch::{ResourceList, WatchStart, WatchStream},
};

#[derive(Debug, Clone)]
pub enum ResourceBackend {
    InMemory(InMemoryBackend),
    Http(HttpBackend),
}

impl ResourceBackend {
    pub fn using<T: Resource>(&self, namespace: &str) -> TypedResolver<T> {
        TypedResolver { backend: self.clone(), namespace: namespace.to_string(), _marker: PhantomData }
    }
}

#[derive(Debug, Clone)]
pub struct TypedResolver<T: Resource> {
    pub(crate) backend: ResourceBackend,
    pub(crate) namespace: String,
    pub(crate) _marker: PhantomData<T>,
}

impl<T: Resource> TypedResolver<T> {
    pub async fn get(&self, name: &str) -> Result<ResourceObject<T>, ResourceError> {
        match &self.backend {
            ResourceBackend::InMemory(backend) => backend.get_typed::<T>(&self.namespace, name).await,
            ResourceBackend::Http(backend) => backend.get_typed::<T>(&self.namespace, name).await,
        }
    }

    pub async fn list(&self) -> Result<ResourceList<T>, ResourceError> {
        match &self.backend {
            ResourceBackend::InMemory(backend) => backend.list_typed::<T>(&self.namespace).await,
            ResourceBackend::Http(backend) => backend.list_typed::<T>(&self.namespace).await,
        }
    }

    pub async fn create(&self, meta: &InputMeta, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
        match &self.backend {
            ResourceBackend::InMemory(backend) => backend.create_typed::<T>(&self.namespace, meta, spec).await,
            ResourceBackend::Http(backend) => backend.create_typed::<T>(&self.namespace, meta, spec).await,
        }
    }

    pub async fn update(&self, meta: &InputMeta, resource_version: &str, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
        match &self.backend {
            ResourceBackend::InMemory(backend) => backend.update_typed::<T>(&self.namespace, meta, resource_version, spec).await,
            ResourceBackend::Http(backend) => backend.update_typed::<T>(&self.namespace, meta, resource_version, spec).await,
        }
    }

    pub async fn update_status(&self, name: &str, resource_version: &str, status: &T::Status) -> Result<ResourceObject<T>, ResourceError> {
        match &self.backend {
            ResourceBackend::InMemory(backend) => backend.update_status_typed::<T>(&self.namespace, name, resource_version, status).await,
            ResourceBackend::Http(backend) => backend.update_status_typed::<T>(&self.namespace, name, resource_version, status).await,
        }
    }

    pub async fn delete(&self, name: &str) -> Result<(), ResourceError> {
        match &self.backend {
            ResourceBackend::InMemory(backend) => backend.delete_typed::<T>(&self.namespace, name).await,
            ResourceBackend::Http(backend) => backend.delete_typed::<T>(&self.namespace, name).await,
        }
    }

    pub async fn watch(&self, start: WatchStart) -> Result<WatchStream<T>, ResourceError> {
        match &self.backend {
            ResourceBackend::InMemory(backend) => backend.watch_typed::<T>(&self.namespace, start).await,
            ResourceBackend::Http(backend) => backend.watch_typed::<T>(&self.namespace, start).await,
        }
    }
}
