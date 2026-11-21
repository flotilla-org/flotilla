//! The Forge and Repository ports over a [`ResourceBackend`], for a home with no engine behind
//! its backend: the readers that take the ports run unchanged over a bare backend's records.
//! Each read is the same keyed or indexed read the engine's port makes.

use std::collections::BTreeMap;

use flotilla_resources::{
    engine_records::Record,
    forge_records::ForgeRecords,
    repository::{forge_service_url_key, FORGE_SERVICE_URL_FIELD},
    repository_records::RepositoryRecords,
    Forge, ForgeSpec, Repository, ResourceError, ResourceObject,
};
use futures::stream::BoxStream;

use crate::{ResourceBackend, StoreQuery};

/// The Forge port over `backend`'s definitions in `namespace`.
pub struct BackendForges {
    backend: ResourceBackend,
    namespace: String,
}

impl BackendForges {
    pub fn new(backend: ResourceBackend, namespace: &str) -> Self {
        Self { backend, namespace: namespace.to_owned() }
    }
}

#[async_trait::async_trait]
impl ForgeRecords for BackendForges {
    async fn forge(&self, name: &str) -> Result<Option<ForgeSpec>, String> {
        match self.backend.definitions::<Forge>(&self.namespace).get(name).await {
            Ok(forge) => Ok(Some(forge.spec)),
            Err(ResourceError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn at_host(&self, host: &str) -> Result<Vec<ForgeSpec>, String> {
        let query = StoreQuery {
            fields: BTreeMap::from([("spec.remote_hosts.*".to_owned(), host.to_ascii_lowercase())]),
            include_replicas: true,
            ..Default::default()
        };
        let read = self.backend.query_declared::<Forge>(&self.namespace, &query).await.map_err(|error| error.to_string())?;
        let mut forges: BTreeMap<String, ForgeSpec> = BTreeMap::new();
        for forge in read.items {
            forges.entry(forge.object.metadata.name).or_insert(forge.object.spec);
        }
        Ok(forges.into_values().collect())
    }

    async fn changes(&self) -> Result<BoxStream<'_, ForgeSpec>, String> {
        use flotilla_resources::ReadWatchEvent;
        use futures::StreamExt;
        let events = self.backend.including_replicas::<Forge>(&self.namespace).watch().await.map_err(|error| error.to_string())?;
        Ok(events
            .filter_map(|event| {
                futures::future::ready(match event {
                    Ok(ReadWatchEvent::Added(forge) | ReadWatchEvent::Modified(forge) | ReadWatchEvent::Deleted(forge)) => {
                        Some(forge.object.spec)
                    }
                    _ => None,
                })
            })
            .boxed())
    }
}

/// The Repository port over `backend`'s repositories in `namespace`.
pub struct BackendRepositories {
    backend: ResourceBackend,
    namespace: String,
}

impl BackendRepositories {
    pub fn new(backend: ResourceBackend, namespace: &str) -> Self {
        Self { backend, namespace: namespace.to_owned() }
    }

    fn record(object: ResourceObject<Repository>) -> Record<Repository> {
        Record {
            name: object.metadata.name,
            home: String::new(),
            version: object.metadata.resource_version.parse().unwrap_or_default(),
            created_at: object.metadata.creation_timestamp,
            labels: object.metadata.labels,
            annotations: object.metadata.annotations,
            spec: object.spec,
            status: object.status,
        }
    }

    async fn indexed(&self, path: &str, value: &str) -> Result<Vec<Record<Repository>>, String> {
        let query =
            StoreQuery { fields: BTreeMap::from([(path.to_owned(), value.to_owned())]), include_replicas: true, ..StoreQuery::default() };
        let read = self.backend.using::<Repository>(&self.namespace).query_declared(&query).await.map_err(|error| error.to_string())?;
        Ok(read.items.into_iter().map(|read| Self::record(read.object)).collect())
    }
}

#[async_trait::async_trait]
impl RepositoryRecords for BackendRepositories {
    async fn repository(&self, key: &str) -> Result<Option<Record<Repository>>, String> {
        match self.backend.using::<Repository>(&self.namespace).get(key).await {
            Ok(object) => Ok(Some(Self::record(object))),
            Err(ResourceError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn on_forge(&self, service_url: &str) -> Result<Vec<Record<Repository>>, String> {
        self.indexed(FORGE_SERVICE_URL_FIELD, forge_service_url_key(service_url)).await
    }

    async fn with_catalog_target(&self, target: &str) -> Result<Vec<Record<Repository>>, String> {
        self.indexed("spec.catalog_targets.*", target).await
    }

    async fn changes(&self) -> Result<BoxStream<'_, ()>, String> {
        use futures::StreamExt;
        let events = self.backend.including_replicas::<Repository>(&self.namespace).watch().await.map_err(|error| error.to_string())?;
        Ok(events.filter_map(|event| futures::future::ready(event.ok().map(|_| ()))).boxed())
    }
}
