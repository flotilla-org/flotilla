//! Behaviour every [`IssueProvider`](super::IssueProvider) must satisfy.
//!
//! This crate's provider tests and the discovery testkit's fake compile
//! against different builds of the trait, so each includes this file as a
//! child module of a parent that has `IssueProvider` in scope.
use flotilla_protocol::{issue_query::IssueQuery, IssueRef, IssueSource, IssueState};

use super::IssueProvider;

pub async fn assert_provider_contract(provider: &dyn IssueProvider, source: &IssueSource, known_id: &str, since: &str) {
    let page = provider.query(source, &IssueQuery::default(), 1, 30).await.expect("provider contract: initial query");
    assert!(!page.items.is_empty(), "provider contract requires seeded open issues");
    assert!(page.items.iter().all(|issue| issue.state == IssueState::Open));
    assert!(page.items.iter().all(|issue| issue.reference.source == *source));
    assert!(page.items.windows(2).all(|rows| rows[0].as_of >= rows[1].as_of), "initial query must be updated-descending");

    let reference = IssueRef { source: source.clone(), id: known_id.to_string() };
    let fetched = provider.fetch_by_id(&reference).await.expect("provider contract: opaque fetch-by-id");
    assert_eq!(fetched.reference, reference);

    let changes = provider.list_changed_since(source, since, 30).await.expect("provider contract: changed-since");
    assert!(changes.updated.iter().all(|issue| issue.state == IssueState::Open));
    assert!(changes.updated.iter().all(|issue| issue.reference.source == *source));
    assert!(changes.closed.iter().all(|reference| reference.source == *source));
    assert!(changes.updated.iter().all(|issue| !changes.closed.iter().any(|reference| reference == &issue.reference)));
}
