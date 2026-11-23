//! Repositories as the engine holds them: the port every Repository reader uses, served over the
//! decided store.
//!
//! A reader asks for the repository it needs, by key or through an index, and is never handed the
//! declared set to search. There is no listing.

use std::collections::{BTreeMap, BTreeSet};

use futures::stream::BoxStream;

use crate::{engine_records::Record, Repository, RepositorySpec};

/// The Repository port. A key several homes hold reads as this home's record, else the
/// lexically first home's.
#[async_trait::async_trait]
pub trait RepositoryRecords: Send + Sync {
    /// The repository with `key`.
    async fn repository(&self, key: &str) -> Result<Option<Record<Repository>>, String>;
    /// Every home's copy of every repository on the forge whose service URL is `service_url`
    /// ([`crate::repository::FORGE_SERVICE_URL_FIELD`]), through the index of it.
    async fn on_forge(&self, service_url: &str) -> Result<Vec<Record<Repository>>, String>;
    /// Every home's copy of every repository holding `slug`, a normalized workspace slug
    /// ([`crate::repository::WORKSPACE_SLUGS_FIELD`]), through the index of it.
    async fn with_workspace_slug(&self, slug: &str) -> Result<Vec<Record<Repository>>, String>;
    /// Ticks as repositories change, from now on.
    async fn changes(&self) -> Result<BoxStream<'_, ()>, String>;

    /// The repositories, other than `spec`'s own, that can change the workspace slug it is
    /// allocated: those holding one of its slugs, and those holding one of theirs in turn. Slug
    /// allocation over `spec` and these alone gives `spec` the slug it gets over every repository.
    /// Each step reads by slug; the declared set is never listed.
    async fn workspace_slug_peers(&self, spec: &RepositorySpec) -> Result<Vec<Record<Repository>>, String> {
        let own = spec.key();
        let mut asked = BTreeSet::new();
        let mut frontier = crate::repository::workspace_slugs(spec);
        let mut peers: BTreeMap<(String, String), Record<Repository>> = BTreeMap::new();
        while let Some(slug) = frontier.pop_first() {
            if !asked.insert(slug.clone()) {
                continue;
            }
            for record in self.with_workspace_slug(&slug).await? {
                if record.spec.key() == own {
                    continue;
                }
                frontier.extend(crate::repository::workspace_slugs(&record.spec));
                peers.insert((record.name.clone(), record.home.clone()), record);
            }
        }
        Ok(peers.into_values().collect())
    }
}
