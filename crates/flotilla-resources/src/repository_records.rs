//! Repositories as the engine holds them: the port every Repository reader uses, served over the
//! decided store.
//!
//! A reader asks for the repository it needs, by key or through an index, and is never handed the
//! declared set to search. There is no listing.

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
    /// The repositories, other than `spec`'s own, that can change the workspace slug it is
    /// allocated ([`crate::repository::workspace_slug_component`]), as several homes' resolve.
    /// Slugs collide once normalized and the indexes hold them as written, so the engine answers
    /// from its resident records.
    async fn workspace_slug_peers(&self, spec: &RepositorySpec) -> Result<Vec<Record<Repository>>, String>;
    /// Ticks as repositories change, from now on.
    async fn changes(&self) -> Result<BoxStream<'_, ()>, String>;
}

/// The records among `listed` that are [`crate::repository::workspace_slug_component`] peers of
/// `spec`: the repositories that can change the workspace slug it is allocated, `spec`'s own
/// excluded.
pub fn workspace_slug_peer_records(spec: &RepositorySpec, listed: Vec<Record<Repository>>) -> Vec<Record<Repository>> {
    let own = spec.key();
    let others: Vec<Record<Repository>> = listed.into_iter().filter(|record| record.spec.key() != own).collect();
    let peers: std::collections::BTreeSet<String> =
        crate::repository::workspace_slug_component(spec, others.iter().map(|record| &record.spec))
            .into_iter()
            .map(|peer| peer.key().to_string())
            .collect();
    others.into_iter().filter(|record| peers.contains(&record.spec.key().to_string())).collect()
}
