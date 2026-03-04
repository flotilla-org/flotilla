use std::collections::HashMap;

use super::types::CorrelationKey;

/// The kind of item being correlated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemKind {
    Checkout,
    ChangeRequest,
    #[allow(dead_code)]
    Issue,
    CloudSession,
    Workspace,
    #[allow(dead_code)]
    RemoteBranch,
}

/// A single item submitted for correlation.
#[derive(Debug, Clone)]
pub struct CorrelatedItem {
    #[allow(dead_code)]
    pub provider_name: String,
    pub kind: ItemKind,
    #[allow(dead_code)]
    pub title: String,
    pub correlation_keys: Vec<CorrelationKey>,
    pub source_index: usize,
}

/// A group of items that are transitively related via shared correlation keys.
#[derive(Debug, Clone)]
pub struct CorrelatedGroup {
    pub items: Vec<CorrelatedItem>,
}

impl CorrelatedGroup {
    /// Returns the branch name if any item in the group has a `Branch` correlation key.
    pub fn branch(&self) -> Option<&str> {
        for item in &self.items {
            for key in &item.correlation_keys {
                if let CorrelationKey::Branch(ref b) = key {
                    return Some(b.as_str());
                }
            }
        }
        None
    }

    /// Returns true if the group contains an item of the given kind.
    #[allow(dead_code)]
    pub fn has(&self, kind: &ItemKind) -> bool {
        self.items.iter().any(|item| item.kind == *kind)
    }
}

// ---------------------------------------------------------------------------
// Union-Find (disjoint set) with path compression and union by rank
// ---------------------------------------------------------------------------

struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]); // path compression
        }
        self.parent[x]
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        // union by rank
        match self.rank[ra].cmp(&self.rank[rb]) {
            std::cmp::Ordering::Less => self.parent[ra] = rb,
            std::cmp::Ordering::Greater => self.parent[rb] = ra,
            std::cmp::Ordering::Equal => {
                self.parent[rb] = ra;
                self.rank[ra] += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Groups items that share any `CorrelationKey` value, transitively.
///
/// If item A shares a key with item B, and B shares a *different* key with C,
/// then A, B and C all end up in the same group.
pub fn correlate(items: Vec<CorrelatedItem>) -> Vec<CorrelatedGroup> {
    if items.is_empty() {
        return Vec::new();
    }

    let n = items.len();
    let mut uf = UnionFind::new(n);

    // Map each correlation key to the first item index that carried it.
    let mut key_to_item: HashMap<CorrelationKey, usize> = HashMap::new();

    for (idx, item) in items.iter().enumerate() {
        for key in &item.correlation_keys {
            match key_to_item.get(key) {
                Some(&first_idx) => {
                    uf.union(first_idx, idx);
                }
                None => {
                    key_to_item.insert(key.clone(), idx);
                }
            }
        }
    }

    // Collect items into groups keyed by their root representative.
    let mut groups: HashMap<usize, Vec<CorrelatedItem>> = HashMap::new();
    for (idx, item) in items.into_iter().enumerate() {
        let root = uf.find(idx);
        groups.entry(root).or_default().push(item);
    }

    // Return groups in a deterministic (sorted by smallest original index) order.
    let mut roots: Vec<usize> = groups.keys().copied().collect();
    roots.sort_unstable();

    roots
        .into_iter()
        .map(|root| CorrelatedGroup {
            items: groups.remove(&root).unwrap(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn item(
        provider: &str,
        kind: ItemKind,
        title: &str,
        keys: Vec<CorrelationKey>,
    ) -> CorrelatedItem {
        CorrelatedItem {
            provider_name: provider.to_string(),
            kind,
            title: title.to_string(),
            correlation_keys: keys,
            source_index: 0,
        }
    }

    #[test]
    fn empty_input() {
        let groups = correlate(vec![]);
        assert!(groups.is_empty());
    }

    #[test]
    fn single_item_forms_own_group() {
        let items = vec![item(
            "git",
            ItemKind::Checkout,
            "feat-x",
            vec![CorrelationKey::Branch("feat-x".into())],
        )];
        let groups = correlate(items);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].items.len(), 1);
        assert_eq!(groups[0].items[0].title, "feat-x");
    }

    #[test]
    fn items_sharing_branch_are_grouped() {
        let items = vec![
            item(
                "git",
                ItemKind::Checkout,
                "feat-x checkout",
                vec![CorrelationKey::Branch("feat-x".into())],
            ),
            item(
                "github",
                ItemKind::ChangeRequest,
                "PR #42: feat-x",
                vec![CorrelationKey::Branch("feat-x".into())],
            ),
        ];

        let groups = correlate(items);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].items.len(), 2);
        assert!(groups[0].has(&ItemKind::Checkout));
        assert!(groups[0].has(&ItemKind::ChangeRequest));
        assert_eq!(groups[0].branch(), Some("feat-x"));
    }

    #[test]
    fn transitive_correlation() {
        // checkout --[Branch]--> PR --[IssueRef]--> issue
        let items = vec![
            item(
                "git",
                ItemKind::Checkout,
                "feat-x checkout",
                vec![CorrelationKey::Branch("feat-x".into())],
            ),
            item(
                "github",
                ItemKind::ChangeRequest,
                "PR #42",
                vec![
                    CorrelationKey::Branch("feat-x".into()),
                    CorrelationKey::IssueRef("github".into(), "99".into()),
                ],
            ),
            item(
                "github",
                ItemKind::Issue,
                "Issue #99",
                vec![CorrelationKey::IssueRef("github".into(), "99".into())],
            ),
        ];

        let groups = correlate(items);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].items.len(), 3);
        assert!(groups[0].has(&ItemKind::Checkout));
        assert!(groups[0].has(&ItemKind::ChangeRequest));
        assert!(groups[0].has(&ItemKind::Issue));
    }

    #[test]
    fn unrelated_items_stay_separate() {
        let items = vec![
            item(
                "git",
                ItemKind::Checkout,
                "branch-a",
                vec![CorrelationKey::Branch("branch-a".into())],
            ),
            item(
                "git",
                ItemKind::Checkout,
                "branch-b",
                vec![CorrelationKey::Branch("branch-b".into())],
            ),
            item(
                "github",
                ItemKind::Issue,
                "Issue #1",
                vec![CorrelationKey::IssueRef("github".into(), "1".into())],
            ),
        ];

        let groups = correlate(items);
        assert_eq!(groups.len(), 3);
    }

    #[test]
    fn no_correlation_keys_each_item_separate() {
        let items = vec![
            item("git", ItemKind::Checkout, "orphan-a", vec![]),
            item("github", ItemKind::ChangeRequest, "orphan-b", vec![]),
            item("github", ItemKind::Issue, "orphan-c", vec![]),
        ];

        let groups = correlate(items);
        assert_eq!(groups.len(), 3);
    }

    #[test]
    fn workspace_correlates_via_repo_path() {
        let repo = PathBuf::from("/home/user/project");
        let items = vec![
            item(
                "git",
                ItemKind::Checkout,
                "main checkout",
                vec![CorrelationKey::CheckoutPath(repo.clone())],
            ),
            item(
                "tmux",
                ItemKind::Workspace,
                "my-workspace",
                vec![CorrelationKey::CheckoutPath(repo)],
            ),
        ];

        let groups = correlate(items);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].items.len(), 2);
        assert!(groups[0].has(&ItemKind::Checkout));
        assert!(groups[0].has(&ItemKind::Workspace));
    }
}
