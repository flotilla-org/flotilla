//! Per-repo issue query state driven by `IssueQueryService` cursors.
//!
//! `IssueViewState` replaces the old snapshot-driven issue display. Each repo
//! can have a *default* cursor (open issues, no search filter) and an optional
//! *search* cursor that overlays the default while active.

use flotilla_protocol::{
    issue_query::{CursorId, IssueResultPage},
    provider_data::Issue,
    HostName, WorkItem, WorkItemIdentity, WorkItemKind,
};

/// State for a single paginated cursor.
pub struct IssueCursorState {
    pub cursor: CursorId,
    pub items: Vec<(String, Issue)>,
    pub total: Option<u32>,
    pub has_more: bool,
    pub fetch_pending: bool,
}

impl IssueCursorState {
    /// Append a fetched page of results and update pagination metadata.
    pub fn append_page(&mut self, page: IssueResultPage) {
        self.total = page.total;
        self.has_more = page.has_more;
        self.fetch_pending = false;
        self.items.extend(page.items);
    }

    /// Convert the cursor's issue items into protocol `WorkItem` values
    /// suitable for the `SplitTable` issue section.
    pub fn to_work_items(&self, host_name: &HostName) -> Vec<WorkItem> {
        self.items
            .iter()
            .map(|(id, issue)| WorkItem {
                kind: WorkItemKind::Issue,
                identity: WorkItemIdentity::Issue(id.clone()),
                host: host_name.clone(),
                branch: None,
                description: issue.title.clone(),
                checkout: None,
                change_request_key: None,
                session_key: None,
                issue_keys: vec![id.clone()],
                workspace_refs: Vec::new(),
                is_main_checkout: false,
                debug_group: Vec::new(),
                source: Some(issue.provider_display_name.clone()),
                terminal_keys: Vec::new(),
                attachable_set_id: None,
                agent_keys: Vec::new(),
            })
            .collect()
    }
}

/// Per-repo issue view state, managing default and search cursors.
#[derive(Default)]
pub struct IssueViewState {
    /// Default listing cursor (open issues, no search filter).
    pub default: Option<IssueCursorState>,
    /// Active search cursor, overlays the default when present.
    pub search: Option<IssueCursorState>,
    pub search_query: Option<String>,
}

impl IssueViewState {
    pub fn new() -> Self {
        Self { default: None, search: None, search_query: None }
    }

    /// The cursor state currently displayed — search if active, else default.
    pub fn active(&self) -> Option<&IssueCursorState> {
        self.search.as_ref().or(self.default.as_ref())
    }

    pub fn active_mut(&mut self) -> Option<&mut IssueCursorState> {
        if self.search.is_some() {
            self.search.as_mut()
        } else {
            self.default.as_mut()
        }
    }

    /// Convert the active cursor's items into `WorkItem` values for display.
    pub fn active_work_items(&self, host_name: &HostName) -> Vec<WorkItem> {
        self.active().map(|c| c.to_work_items(host_name)).unwrap_or_default()
    }
}

/// Background update messages from spawned query tasks back to the event loop.
pub enum IssueQueryUpdate {
    /// A default cursor was opened for a repo.
    DefaultCursorOpened { repo: flotilla_protocol::RepoIdentity, cursor: CursorId },
    /// A search cursor was opened for a repo.
    SearchCursorOpened { repo: flotilla_protocol::RepoIdentity, cursor: CursorId, query: String },
    /// A page of results arrived for a cursor.
    PageFetched { repo: flotilla_protocol::RepoIdentity, cursor: CursorId, page: IssueResultPage },
    /// A query command failed.
    QueryFailed { repo: flotilla_protocol::RepoIdentity, message: String },
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{provider_data::Issue, HostName};

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
        state.default = Some(IssueCursorState {
            cursor: CursorId::new("c1"),
            items: vec![test_issue("1", "Bug")],
            total: Some(1),
            has_more: false,
            fetch_pending: false,
        });
        let active = state.active().expect("should have active");
        assert_eq!(active.items.len(), 1);
        assert_eq!(active.cursor, CursorId::new("c1"));
    }

    #[test]
    fn active_returns_search_when_present() {
        let mut state = IssueViewState::new();
        state.default = Some(IssueCursorState {
            cursor: CursorId::new("default"),
            items: vec![test_issue("1", "Default issue")],
            total: Some(1),
            has_more: false,
            fetch_pending: false,
        });
        state.search = Some(IssueCursorState {
            cursor: CursorId::new("search"),
            items: vec![test_issue("2", "Search result")],
            total: Some(1),
            has_more: false,
            fetch_pending: false,
        });
        let active = state.active().expect("should have active");
        assert_eq!(active.cursor, CursorId::new("search"));
        assert_eq!(active.items[0].0, "2");
    }

    #[test]
    fn append_page_extends_items() {
        let mut cursor = IssueCursorState {
            cursor: CursorId::new("c1"),
            items: vec![test_issue("1", "First")],
            total: None,
            has_more: true,
            fetch_pending: true,
        };
        cursor.append_page(IssueResultPage {
            items: vec![test_issue("2", "Second"), test_issue("3", "Third")],
            total: Some(10),
            has_more: true,
        });
        assert_eq!(cursor.items.len(), 3);
        assert_eq!(cursor.total, Some(10));
        assert!(cursor.has_more);
        assert!(!cursor.fetch_pending);
    }

    #[test]
    fn to_work_items_converts_correctly() {
        let cursor = IssueCursorState {
            cursor: CursorId::new("c1"),
            items: vec![test_issue("42", "Fix login bug"), test_issue("99", "Add dark mode")],
            total: Some(2),
            has_more: false,
            fetch_pending: false,
        };
        let host = HostName::local();
        let work_items = cursor.to_work_items(&host);
        assert_eq!(work_items.len(), 2);

        assert_eq!(work_items[0].kind, WorkItemKind::Issue);
        assert_eq!(work_items[0].identity, WorkItemIdentity::Issue("42".into()));
        assert_eq!(work_items[0].description, "Fix login bug");
        assert_eq!(work_items[0].issue_keys, vec!["42"]);
        assert_eq!(work_items[0].source.as_deref(), Some("GitHub"));

        assert_eq!(work_items[1].identity, WorkItemIdentity::Issue("99".into()));
        assert_eq!(work_items[1].description, "Add dark mode");
    }

    #[test]
    fn active_work_items_returns_empty_when_no_cursor() {
        let state = IssueViewState::new();
        let items = state.active_work_items(&HostName::local());
        assert!(items.is_empty());
    }

    #[test]
    fn active_mut_returns_search_when_present() {
        let mut state = IssueViewState::new();
        state.default =
            Some(IssueCursorState { cursor: CursorId::new("default"), items: vec![], total: None, has_more: false, fetch_pending: false });
        state.search =
            Some(IssueCursorState { cursor: CursorId::new("search"), items: vec![], total: None, has_more: true, fetch_pending: false });
        let active = state.active_mut().expect("should have active");
        assert_eq!(active.cursor, CursorId::new("search"));
    }
}
