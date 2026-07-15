//! Issue query service types shared between core and protocol.

use serde::{Deserialize, Serialize};

use crate::provider_data::Issue;

/// Parameters for an issue query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueQuery {
    pub search: Option<String>,
}

/// A single page of query results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueResultPage {
    pub items: Vec<Issue>,
    pub total: Option<u32>,
    pub has_more: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_data::{IssueRef, IssueSource, IssueState};

    #[test]
    fn issue_query_default_has_no_search() {
        let q = IssueQuery::default();
        assert!(q.search.is_none());
    }

    #[test]
    fn issue_result_page_serde_roundtrip() {
        let page = IssueResultPage {
            items: vec![Issue {
                reference: IssueRef {
                    source: IssueSource { service: "https://github.com".into(), scope: "owner/repo".into() },
                    id: "1".into(),
                },
                title: "Bug".into(),
                body: None,
                state: IssueState::Open,
                labels: vec![],
                as_of: "2026-07-15T09:30:00Z".parse().expect("valid timestamp"),
                association_keys: vec![],
                provider_name: "github".into(),
                provider_display_name: "GitHub".into(),
            }],
            total: Some(42),
            has_more: true,
        };
        let json = serde_json::to_string(&page).expect("serialize");
        let decoded: IssueResultPage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.items.len(), 1);
        assert_eq!(decoded.total, Some(42));
        assert!(decoded.has_more);
    }
}
