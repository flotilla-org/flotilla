pub mod github;

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    Issue, IssueChangeset, IssueRef, IssueSource, RepoIdentity,
};
use futures::{stream, StreamExt};

const MAX_CONCURRENT_ISSUE_FETCHES: usize = 10;

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
        let fetches = ids.iter().cloned().map(|id| {
            let reference = IssueRef { source: source.clone(), id };
            async move {
                let result = self.fetch_by_id(&reference).await;
                (reference, result)
            }
        });
        let results = stream::iter(fetches).buffered(MAX_CONCURRENT_ISSUE_FETCHES).collect::<Vec<_>>().await;

        let mut issues = Vec::with_capacity(results.len());
        for (reference, result) in results {
            match result {
                Ok(issue) => issues.push(issue),
                Err(error) => tracing::warn!(%error, id = %reference.id, "failed to fetch issue by id"),
            }
        }
        Ok(issues)
    }

    async fn list_changed_since(&self, source: &IssueSource, since: &str, count: usize) -> Result<IssueChangeset, String>;

    async fn open_in_browser(&self, reference: &IssueRef) -> Result<(), String>;
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use chrono::Utc;
    use flotilla_protocol::{AssociationKey, IssueState};

    use super::*;

    struct ConcurrentFetchProvider {
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    #[async_trait]
    impl IssueProvider for ConcurrentFetchProvider {
        fn supports(&self, _source: &IssueSource) -> bool {
            true
        }

        async fn query(&self, _source: &IssueSource, _params: &IssueQuery, _page: u32, _count: usize) -> Result<IssueResultPage, String> {
            unreachable!("query is not used by this test")
        }

        async fn fetch_by_id(&self, reference: &IssueRef) -> Result<Issue, String> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(10)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(Issue::builder()
                .reference(reference.clone())
                .title(reference.id.clone())
                .state(IssueState::Open)
                .labels(vec![])
                .as_of(Utc::now())
                .association_keys(Vec::<AssociationKey>::new())
                .provider_name("test".into())
                .provider_display_name("Test".into())
                .build())
        }

        async fn list_changed_since(&self, _source: &IssueSource, _since: &str, _count: usize) -> Result<IssueChangeset, String> {
            unreachable!("changed-since is not used by this test")
        }

        async fn open_in_browser(&self, _reference: &IssueRef) -> Result<(), String> {
            unreachable!("open-in-browser is not used by this test")
        }
    }

    #[tokio::test]
    async fn default_batch_fetch_is_bounded_and_concurrent() {
        let provider = Arc::new(ConcurrentFetchProvider { active: AtomicUsize::new(0), max_active: AtomicUsize::new(0) });
        let source = IssueSource { service: "test".into(), scope: "owner/repo".into() };
        let ids = (0..25).map(|id| id.to_string()).collect::<Vec<_>>();

        let issues = provider.fetch_by_ids(&source, &ids).await.expect("batch fetch should succeed");

        assert_eq!(issues.iter().map(|issue| issue.reference.id.as_str()).collect::<Vec<_>>(), ids);
        assert_eq!(provider.max_active.load(Ordering::SeqCst), MAX_CONCURRENT_ISSUE_FETCHES);
    }
}
