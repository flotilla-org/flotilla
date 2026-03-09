use std::collections::HashSet;
use std::sync::Arc;

use indexmap::IndexMap;

use flotilla_protocol::{Issue, IssueChangeset, IssuePage};

pub struct IssueCache {
    entries: Arc<IndexMap<String, Issue>>,
    pub next_page: u32,
    pub has_more: bool,
    pub pinned: HashSet<String>,
    pub total_count: Option<u32>,
}

impl Default for IssueCache {
    fn default() -> Self {
        Self::new()
    }
}

impl IssueCache {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(IndexMap::new()),
            next_page: 1,
            has_more: true,
            pinned: HashSet::new(),
            total_count: None,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn merge_page(&mut self, page: IssuePage) {
        let entries = Arc::make_mut(&mut self.entries);
        for issue in page.issues {
            entries.insert(issue.id.clone(), issue);
        }
        self.next_page += 1;
        self.has_more = page.has_more;
        if page.total_count.is_some() {
            self.total_count = page.total_count;
        }
    }

    pub fn pin(&mut self, ids: &[String]) {
        for id in ids {
            self.pinned.insert(id.clone());
        }
    }

    pub fn missing_ids(&self, ids: &[String]) -> Vec<String> {
        ids.iter()
            .filter(|id| !self.entries.contains_key(id.as_str()))
            .cloned()
            .collect()
    }

    pub fn add_pinned(&mut self, issues: Vec<Issue>) {
        let entries = Arc::make_mut(&mut self.entries);
        for issue in issues {
            self.pinned.insert(issue.id.clone());
            entries.insert(issue.id.clone(), issue);
        }
    }

    /// Cheap Arc clone — avoids copying the full map on every snapshot build.
    pub fn to_index_map(&self) -> Arc<IndexMap<String, Issue>> {
        Arc::clone(&self.entries)
    }

    /// Apply an incremental changeset: upsert open issues, evict closed ones.
    /// Pinned issues are never evicted (they're linked to PRs via correlation).
    pub fn apply_changeset(&mut self, changeset: IssueChangeset) {
        let entries = Arc::make_mut(&mut self.entries);
        for issue in changeset.updated {
            entries.insert(issue.id.clone(), issue);
        }
        for id in &changeset.closed_ids {
            if !self.pinned.contains(id) {
                entries.shift_remove(id);
            }
        }
    }

    /// Reset pagination state for a full re-fetch. Pinned issues and the
    /// pinned set are preserved; everything else is cleared.
    pub fn reset(&mut self) {
        let pinned_issues: Vec<Issue> = self
            .pinned
            .iter()
            .filter_map(|id| self.entries.get(id).cloned())
            .collect();

        let entries = Arc::make_mut(&mut self.entries);
        entries.clear();
        for issue in pinned_issues {
            entries.insert(issue.id.clone(), issue);
        }
        self.next_page = 1;
        self.has_more = true;
        self.total_count = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(id: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: format!("Issue {}", id),
            labels: vec![],
            association_keys: vec![],
        }
    }

    #[test]
    fn merge_page_appends_issues() {
        let mut cache = IssueCache::new();
        let page = IssuePage {
            issues: vec![issue("1"), issue("2")],
            total_count: Some(10),
            has_more: true,
        };
        cache.merge_page(page);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.total_count, Some(10));
        assert!(cache.has_more);
    }

    #[test]
    fn pin_issues_marks_as_pinned() {
        let mut cache = IssueCache::new();
        cache.merge_page(IssuePage {
            issues: vec![issue("1"), issue("2")],
            total_count: None,
            has_more: false,
        });
        cache.pin(&["1".to_string()]);
        assert!(cache.pinned.contains("1"));
        assert!(!cache.pinned.contains("2"));
    }

    #[test]
    fn missing_ids_returns_unpinned_absent_ids() {
        let mut cache = IssueCache::new();
        cache.merge_page(IssuePage {
            issues: vec![issue("1")],
            total_count: None,
            has_more: false,
        });
        let missing = cache.missing_ids(&["1".to_string(), "3".to_string(), "5".to_string()]);
        assert_eq!(missing, vec!["3", "5"]);
    }

    #[test]
    fn add_pinned_inserts_and_pins() {
        let mut cache = IssueCache::new();
        cache.add_pinned(vec![issue("99")]);
        assert!(cache.to_index_map().contains_key("99"));
        assert!(cache.pinned.contains("99"));
    }

    #[test]
    fn to_index_map_returns_all_entries() {
        let mut cache = IssueCache::new();
        cache.merge_page(IssuePage {
            issues: vec![issue("1"), issue("2")],
            total_count: None,
            has_more: false,
        });
        cache.add_pinned(vec![issue("99")]);
        let map = cache.to_index_map();
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn apply_changeset_upserts_and_evicts() {
        let mut cache = IssueCache::new();
        cache.merge_page(IssuePage {
            issues: vec![issue("1"), issue("2"), issue("3")],
            total_count: None,
            has_more: false,
        });

        let changeset = IssueChangeset {
            updated: vec![Issue {
                id: "2".to_string(),
                title: "Updated Issue 2".to_string(),
                labels: vec!["changed".to_string()],
                association_keys: vec![],
            }, issue("4")],
            closed_ids: vec!["3".to_string()],
            has_more: false,
        };
        cache.apply_changeset(changeset);

        let map = cache.to_index_map();
        assert_eq!(map.len(), 3); // 1, updated-2, 4 (3 evicted)
        assert_eq!(map["2"].title, "Updated Issue 2");
        assert!(map.contains_key("4"));
        assert!(!map.contains_key("3"));
    }

    #[test]
    fn apply_changeset_preserves_pinned_on_close() {
        let mut cache = IssueCache::new();
        cache.add_pinned(vec![issue("99")]);
        cache.merge_page(IssuePage {
            issues: vec![issue("1")],
            total_count: None,
            has_more: false,
        });

        let changeset = IssueChangeset {
            updated: vec![],
            closed_ids: vec!["99".to_string(), "1".to_string()],
            has_more: false,
        };
        cache.apply_changeset(changeset);

        let map = cache.to_index_map();
        assert!(map.contains_key("99"), "pinned issues survive eviction");
        assert!(!map.contains_key("1"), "non-pinned issues are evicted");
    }

    #[test]
    fn reset_clears_non_pinned_entries() {
        let mut cache = IssueCache::new();
        cache.merge_page(IssuePage {
            issues: vec![issue("1"), issue("2")],
            total_count: Some(10),
            has_more: true,
        });
        cache.add_pinned(vec![issue("99")]);
        assert_eq!(cache.next_page, 2);

        cache.reset();

        assert_eq!(cache.len(), 1, "only pinned issue remains");
        assert!(cache.to_index_map().contains_key("99"));
        assert_eq!(cache.next_page, 1);
        assert!(cache.has_more);
        assert!(cache.pinned.contains("99"), "pinned set preserved");
        assert_eq!(cache.total_count, None);
    }
}
