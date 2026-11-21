//! Repositories as the engine holds them: the port every Repository reader uses, served over the
//! decided store.
//!
//! A reader asks for the repository it needs, by key or through an index, and is never handed the
//! declared set to search. There is no listing.

use futures::stream::BoxStream;

use crate::{engine_records::Record, Repository};

/// The Repository port. A key several homes hold reads as this home's record, else the
/// lexically first home's.
#[async_trait::async_trait]
pub trait RepositoryRecords: Send + Sync {
    /// The repository with `key`.
    async fn repository(&self, key: &str) -> Result<Option<Record<Repository>>, String>;
    /// Every home's copy of every repository on the forge whose service URL is `service_url`
    /// ([`crate::repository::FORGE_SERVICE_URL_FIELD`]), through the index of it.
    async fn on_forge(&self, service_url: &str) -> Result<Vec<Record<Repository>>, String>;
    /// The repositories one of whose catalog targets is `target` (`spec.catalog_targets.*`: a leaf
    /// or catalog slug, or an `owner/name`), as several homes' resolve, through the index of it.
    async fn with_catalog_target(&self, target: &str) -> Result<Vec<Record<Repository>>, String>;
    /// Ticks as repositories change, from now on.
    async fn changes(&self) -> Result<BoxStream<'_, ()>, String>;
}
