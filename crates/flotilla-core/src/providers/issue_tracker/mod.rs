pub mod github;

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    Issue, IssueChangeset, IssueRef, IssueSource, RepoIdentity,
};

pub fn forge_issue_source(identity: &RepoIdentity) -> IssueSource {
    let service = if identity.authority.contains("://") { identity.authority.clone() } else { format!("https://{}", identity.authority) };
    IssueSource { service, scope: identity.path.clone() }
}

pub(crate) fn provider_for_source<'a>(
    mut providers: impl Iterator<Item = &'a Arc<dyn IssueProvider>>,
    source: &IssueSource,
) -> Option<Arc<dyn IssueProvider>> {
    providers.find_map(|provider| provider.supports(source).then(|| Arc::clone(provider)))
}

/// Source-addressed access to external issue systems.
///
/// Implementations are host capabilities: the source selects the external
/// service and scope on every call, while credentials and the concrete
/// adapter remain local to the host.
#[async_trait]
pub trait IssueProvider: Send + Sync {
    fn supports(&self, source: &IssueSource) -> bool;

    async fn query(&self, source: &IssueSource, params: &IssueQuery, page: u32, count: usize) -> Result<IssueResultPage, String>;

    async fn fetch_by_id(&self, reference: &IssueRef) -> Result<Issue, String>;

    async fn fetch_by_ids(&self, source: &IssueSource, ids: &[String]) -> Result<Vec<Issue>, String> {
        let mut issues = Vec::with_capacity(ids.len());
        for id in ids {
            let reference = IssueRef { source: source.clone(), id: id.clone() };
            match self.fetch_by_id(&reference).await {
                Ok(issue) => issues.push(issue),
                Err(error) => tracing::warn!(%error, %id, "failed to fetch issue by id"),
            }
        }
        Ok(issues)
    }

    async fn list_changed_since(&self, source: &IssueSource, since: &str, count: usize) -> Result<IssueChangeset, String>;

    async fn open_in_browser(&self, reference: &IssueRef) -> Result<(), String>;
}
