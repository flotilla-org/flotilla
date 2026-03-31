//! Per-repo issue query state driven by stateless paged queries.
//!
//! `IssueViewState` replaces the old snapshot-driven issue display. Each repo
//! can have a *default* listing (open issues, no search filter) and an optional
//! *search* listing that overlays the default while active.

use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    provider_data::Issue,
};

use crate::widgets::section_table::IssueRow;

/// State for a single paginated query — tracks accumulated items and next page.
pub struct IssuePagingState {
    pub params: IssueQuery,
    pub items: Vec<(String, Issue)>,
    pub next_page: u32,
    pub total: Option<u32>,
    pub has_more: bool,
    pub fetch_pending: bool,
}

impl IssuePagingState {
    pub fn new(params: IssueQuery) -> Self {
        Self { params, items: Vec::new(), next_page: 1, total: None, has_more: true, fetch_pending: false }
    }

    /// Append a fetched page of results and update pagination metadata.
    pub fn append_page(&mut self, page: IssueResultPage) {
        self.total = page.total;
        self.has_more = page.has_more;
        self.fetch_pending = false;
        self.next_page += 1;
        self.items.extend(page.items);
    }

    /// Convert the paging state's issue items into native `IssueRow` values
    /// for the `SectionTable<IssueRow>` issue section.
    pub fn to_issue_rows(&self) -> Vec<IssueRow> {
        self.items.iter().map(|(id, issue)| IssueRow { id: id.clone(), issue: issue.clone() }).collect()
    }
}

/// Per-repo issue view state, managing default and search listings.
#[derive(Default)]
pub struct IssueViewState {
    /// Default listing (open issues, no search filter).
    pub default: Option<IssuePagingState>,
    /// Active search listing, overlays the default when present.
    pub search: Option<IssuePagingState>,
    pub search_query: Option<String>,
}

impl IssueViewState {
    pub fn new() -> Self {
        Self { default: None, search: None, search_query: None }
    }

    /// The paging state currently displayed — search if active, else default.
    pub fn active(&self) -> Option<&IssuePagingState> {
        self.search.as_ref().or(self.default.as_ref())
    }

    pub fn active_mut(&mut self) -> Option<&mut IssuePagingState> {
        if self.search.is_some() {
            self.search.as_mut()
        } else {
            self.default.as_mut()
        }
    }

    /// Convert the active listing's items into native `IssueRow` values for display.
    pub fn active_issue_rows(&self) -> Vec<IssueRow> {
        self.active().map(|c| c.to_issue_rows()).unwrap_or_default()
    }
}

/// Background update messages from spawned query tasks back to the event loop.
pub enum IssueQueryUpdate {
    /// A page of results arrived.
    PageFetched { repo: flotilla_protocol::RepoIdentity, params: IssueQuery, page: IssueResultPage },
    /// A query request failed.
    QueryFailed { repo: flotilla_protocol::RepoIdentity, message: String, is_search: bool },
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{issue_query::IssueQuery, provider_data::Issue};

    use super::*;

    fn test_issue(id: &str, title: &str) -> (String, Issue) {
        (id.to_string(), Issue {
            title: title.to_string(),
            labels: vec![],
            association_keys: vec![],
            provider_name: "github".to_string(),
            provider_display_name: "GitHub".to_string(),
        })
    }

    #[test]
    fn new_state_has_no_active() {
        let state = IssueViewState::new();
        assert!(state.active().is_none());
        assert!(state.search_query.is_none());
    }

    #[test]
    fn active_returns_default_when_no_search() {
        let mut state = IssueViewState::new();
        state.default = Some(IssuePagingState {
            params: IssueQuery::default(),
            items: vec![test_issue("1", "Bug")],
            next_page: 2,
            total: Some(1),
            has_more: false,
            fetch_pending: false,
        });
        let active = state.active().expect("should have active");
        assert_eq!(active.items.len(), 1);
        assert_eq!(active.params, IssueQuery::default());
    }

    #[test]
    fn active_returns_search_when_present() {
        let mut state = IssueViewState::new();
        state.default = Some(IssuePagingState {
            params: IssueQuery::default(),
            items: vec![test_issue("1", "Default issue")],
            next_page: 2,
            total: Some(1),
            has_more: false,
            fetch_pending: false,
        });
        state.search = Some(IssuePagingState {
            params: IssueQuery { search: Some("search".into()) },
            items: vec![test_issue("2", "Search result")],
            next_page: 2,
            total: Some(1),
            has_more: false,
            fetch_pending: false,
        });
        let active = state.active().expect("should have active");
        assert!(active.params.search.is_some());
        assert_eq!(active.items[0].0, "2");
    }

    #[test]
    fn append_page_extends_items() {
        let mut paging = IssuePagingState {
            params: IssueQuery::default(),
            items: vec![test_issue("1", "First")],
            next_page: 2,
            total: None,
            has_more: true,
            fetch_pending: true,
        };
        paging.append_page(IssueResultPage {
            items: vec![test_issue("2", "Second"), test_issue("3", "Third")],
            total: Some(10),
            has_more: true,
        });
        assert_eq!(paging.items.len(), 3);
        assert_eq!(paging.total, Some(10));
        assert!(paging.has_more);
        assert!(!paging.fetch_pending);
        assert_eq!(paging.next_page, 3);
    }

    #[test]
    fn to_issue_rows_converts_correctly() {
        let paging = IssuePagingState {
            params: IssueQuery::default(),
            items: vec![test_issue("42", "Fix login bug"), test_issue("99", "Add dark mode")],
            next_page: 1,
            total: Some(2),
            has_more: false,
            fetch_pending: false,
        };
        let rows = paging.to_issue_rows();
        assert_eq!(rows.len(), 2);

        assert_eq!(rows[0].id, "42");
        assert_eq!(rows[0].issue.title, "Fix login bug");
        assert_eq!(rows[0].issue.provider_display_name, "GitHub");

        assert_eq!(rows[1].id, "99");
        assert_eq!(rows[1].issue.title, "Add dark mode");
    }

    #[test]
    fn active_issue_rows_returns_empty_when_no_cursor() {
        let state = IssueViewState::new();
        let rows = state.active_issue_rows();
        assert!(rows.is_empty());
    }

    #[test]
    fn active_mut_returns_search_when_present() {
        let mut state = IssueViewState::new();
        state.default = Some(IssuePagingState {
            params: IssueQuery::default(),
            items: vec![],
            next_page: 1,
            total: None,
            has_more: false,
            fetch_pending: false,
        });
        state.search = Some(IssuePagingState {
            params: IssueQuery { search: Some("test".into()) },
            items: vec![],
            next_page: 1,
            total: None,
            has_more: true,
            fetch_pending: false,
        });
        let active = state.active_mut().expect("should have active");
        assert!(active.params.search.is_some());
    }
}
