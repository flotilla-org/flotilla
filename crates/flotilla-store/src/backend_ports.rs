//! The Forge and Repository ports over a [`ResourceBackend`], for a home with no engine behind
//! its backend: the readers that take the ports run unchanged over a bare backend's records.
//! Each read is the same keyed or indexed read the engine's port makes.

use std::collections::BTreeMap;

use flotilla_resources::{
    engine_records::Record,
    repository::{forge_service_url_key, FORGE_SERVICE_URL_FIELD},
    repository_records::RepositoryRecords,
    Repository, RepositorySpec, ResourceError, ResourceObject,
};
use futures::stream::BoxStream;

use crate::{ResourceBackend, StoreQuery};

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

    async fn workspace_slug_peers(&self, spec: &RepositorySpec) -> Result<Vec<Record<Repository>>, String> {
        let all = self.backend.using::<Repository>(&self.namespace).list().await.map_err(|error| error.to_string())?;
        let listed = all.items.into_iter().map(Self::record).collect();
        Ok(flotilla_resources::repository_records::workspace_slug_peer_records(spec, listed))
    }

    async fn changes(&self) -> Result<BoxStream<'_, ()>, String> {
        use futures::StreamExt;
        let events = self.backend.including_replicas::<Repository>(&self.namespace).watch().await.map_err(|error| error.to_string())?;
        Ok(events.filter_map(|event| futures::future::ready(event.ok().map(|_| ()))).boxed())
    }
}
