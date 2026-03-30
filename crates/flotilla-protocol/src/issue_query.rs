//! Issue query service types shared between core and protocol.

use serde::{Deserialize, Serialize};

use crate::provider_data::Issue;

/// Opaque identifier for a paginated query cursor.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CursorId(String);

impl CursorId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Parameters for an issue query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueQuery {
    pub search: Option<String>,
}

/// A single page of query results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueResultPage {
    pub items: Vec<(String, Issue)>,
    pub total: Option<u32>,
    pub has_more: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_id_equality() {
        let a = CursorId::new("abc");
        let b = CursorId::new("abc");
        let c = CursorId::new("def");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn issue_query_default_has_no_search() {
        let q = IssueQuery::default();
        assert!(q.search.is_none());
    }

    #[test]
    fn cursor_id_serde_roundtrip() {
        let id = CursorId::new("test-cursor-1");
        let json = serde_json::to_string(&id).expect("serialize");
        let decoded: CursorId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, id);
    }

    #[test]
    fn issue_result_page_serde_roundtrip() {
        let page = IssueResultPage {
            items: vec![("1".into(), Issue {
                title: "Bug".into(),
                labels: vec![],
                association_keys: vec![],
                provider_name: "github".into(),
                provider_display_name: "GitHub".into(),
            })],
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
