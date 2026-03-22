use std::collections::HashMap;

use flotilla_protocol::HostPath;

use super::types::CorrelationKey;

/// The kind of item being correlated (identity-keyed items only).
/// Issues and remote branches are not correlated — they use association
/// keys or are handled separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemKind {
    Checkout,
    AttachableSet,
    ChangeRequest,
    CloudSession,
    Workspace,
    ManagedTerminal,
    Agent,
}

/// A key that uniquely identifies a provider item by its natural identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProviderItemKey {
    Checkout(HostPath),
    AttachableSet(flotilla_protocol::AttachableSetId),
    ChangeRequest(String),
    Session(String),
    Workspace(String),
    ManagedTerminal(flotilla_protocol::AttachableId),
    Agent(String),
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
    pub source_key: ProviderItemKey,
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
        Self { parent: (0..n).collect(), rank: vec![0; n] }
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

/// Item kinds that must be unique within a correlated group.
/// A union that would produce two items of a singleton kind is refused.
fn is_singleton_kind(kind: &ItemKind) -> bool {
    matches!(kind, ItemKind::Checkout | ItemKind::ChangeRequest)
}

/// Groups items that share any `CorrelationKey` value, transitively.
///
/// If item A shares a key with item B, and B shares a *different* key with C,
/// then A, B and C all end up in the same group — unless the merge would
/// combine two items of a singleton kind (e.g. two Checkouts).
pub fn correlate(items: Vec<CorrelatedItem>) -> Vec<CorrelatedGroup> {
    if items.is_empty() {
        return Vec::new();
    }

    let n = items.len();
    let mut uf = UnionFind::new(n);

    // Track which singleton kinds are present in each group (by root).
    // Value is a set of singleton ItemKinds in that group.
    let mut group_singletons: HashMap<usize, Vec<ItemKind>> = HashMap::new();
    for (idx, item) in items.iter().enumerate() {
        if is_singleton_kind(&item.kind) {
            group_singletons.entry(idx).or_default().push(item.kind.clone());
        }
    }

    // Map each correlation key to the first item index that carried it.
    let mut key_to_item: HashMap<CorrelationKey, usize> = HashMap::new();

    for (idx, item) in items.iter().enumerate() {
        for key in &item.correlation_keys {
            match key_to_item.get(key) {
                Some(&first_idx) => {
                    let root_a = uf.find(first_idx);
                    let root_b = uf.find(idx);
                    if root_a == root_b {
                        continue; // already in the same group
                    }
                    // Check if merging would combine two singleton kinds
                    let singletons_a = group_singletons.get(&root_a);
                    let singletons_b = group_singletons.get(&root_b);
                    let would_conflict =
                        if let (Some(sa), Some(sb)) = (singletons_a, singletons_b) { sa.iter().any(|k| sb.contains(k)) } else { false };
                    if would_conflict {
                        continue; // refuse the union
                    }
                    uf.union(root_a, root_b);
                    // Merge singleton tracking under the new root
                    let new_root = uf.find(root_a);
                    let other = if new_root == root_a { root_b } else { root_a };
                    if let Some(moved) = group_singletons.remove(&other) {
                        group_singletons.entry(new_root).or_default().extend(moved);
                    }
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

    roots.into_iter().map(|root| CorrelatedGroup { items: groups.remove(&root).unwrap() }).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn hp(path: &str) -> HostPath {
        HostPath::new(flotilla_protocol::HostName::new("test-host"), PathBuf::from(path))
    }

    fn item(provider: &str, kind: ItemKind, title: &str, keys: Vec<CorrelationKey>, source_key: ProviderItemKey) -> CorrelatedItem {
        CorrelatedItem { provider_name: provider.to_string(), kind, title: title.to_string(), correlation_keys: keys, source_key }
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
            ProviderItemKey::Checkout(hp("/code/feat-x")),
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
                ProviderItemKey::Checkout(hp("/code/feat-x")),
            ),
            item(
                "github",
                ItemKind::ChangeRequest,
                "PR #42: feat-x",
                vec![CorrelationKey::Branch("feat-x".into())],
                ProviderItemKey::ChangeRequest("42".into()),
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
        // checkout --[Branch]--> PR, session --[Branch]--> same group
        let items = vec![
            item(
                "git",
                ItemKind::Checkout,
                "feat-x checkout",
                vec![CorrelationKey::Branch("feat-x".into()), CorrelationKey::CheckoutPath(hp("/code/feat-x"))],
                ProviderItemKey::Checkout(hp("/code/feat-x")),
            ),
            item(
                "github",
                ItemKind::ChangeRequest,
                "PR #42",
                vec![CorrelationKey::Branch("feat-x".into())],
                ProviderItemKey::ChangeRequest("42".into()),
            ),
            item(
                "cmux",
                ItemKind::Workspace,
                "my-workspace",
                vec![CorrelationKey::CheckoutPath(hp("/code/feat-x"))],
                ProviderItemKey::Workspace("cmux:my-workspace".into()),
            ),
        ];

        let groups = correlate(items);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].items.len(), 3);
        assert!(groups[0].has(&ItemKind::Checkout));
        assert!(groups[0].has(&ItemKind::ChangeRequest));
        assert!(groups[0].has(&ItemKind::Workspace));
    }

    #[test]
    fn unrelated_items_stay_separate() {
        let items = vec![
            item(
                "git",
                ItemKind::Checkout,
                "branch-a",
                vec![CorrelationKey::Branch("branch-a".into())],
                ProviderItemKey::Checkout(hp("/code/branch-a")),
            ),
            item(
                "git",
                ItemKind::Checkout,
                "branch-b",
                vec![CorrelationKey::Branch("branch-b".into())],
                ProviderItemKey::Checkout(hp("/code/branch-b")),
            ),
            item(
                "claude",
                ItemKind::CloudSession,
                "session-1",
                vec![CorrelationKey::SessionRef("claude".into(), "s1".into())],
                ProviderItemKey::Session("s1".into()),
            ),
        ];

        let groups = correlate(items);
        assert_eq!(groups.len(), 3);
    }

    #[test]
    fn no_correlation_keys_each_item_separate() {
        let items = vec![
            item("git", ItemKind::Checkout, "orphan-a", vec![], ProviderItemKey::Checkout(hp("/code/orphan-a"))),
            item("github", ItemKind::ChangeRequest, "orphan-b", vec![], ProviderItemKey::ChangeRequest("orphan-b".into())),
            item("claude", ItemKind::CloudSession, "orphan-c", vec![], ProviderItemKey::Session("orphan-c".into())),
        ];

        let groups = correlate(items);
        assert_eq!(groups.len(), 3);
    }

    #[test]
    fn workspace_correlates_via_checkout_path() {
        let repo = hp("/home/user/project");
        let items = vec![
            item(
                "git",
                ItemKind::Checkout,
                "main checkout",
                vec![CorrelationKey::CheckoutPath(repo.clone())],
                ProviderItemKey::Checkout(repo.clone()),
            ),
            item(
                "tmux",
                ItemKind::Workspace,
                "my-workspace",
                vec![CorrelationKey::CheckoutPath(repo)],
                ProviderItemKey::Workspace("tmux:my-workspace".into()),
            ),
        ];

        let groups = correlate(items);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].items.len(), 2);
        assert!(groups[0].has(&ItemKind::Checkout));
        assert!(groups[0].has(&ItemKind::Workspace));
    }

    #[test]
    fn two_checkouts_never_merge() {
        // A workspace with paths matching two different checkouts should NOT
        // cause those checkouts to merge into one group.
        let main_path = hp("/code/project");
        let feat_path = hp("/code/project.feat-x");
        let items = vec![
            item(
                "git",
                ItemKind::Checkout,
                "main",
                vec![CorrelationKey::Branch("main".into()), CorrelationKey::CheckoutPath(main_path.clone())],
                ProviderItemKey::Checkout(main_path.clone()),
            ),
            item(
                "git",
                ItemKind::Checkout,
                "feat-x",
                vec![CorrelationKey::Branch("feat-x".into()), CorrelationKey::CheckoutPath(feat_path.clone())],
                ProviderItemKey::Checkout(feat_path.clone()),
            ),
            item(
                "cmux",
                ItemKind::Workspace,
                "buggy-workspace",
                // Workspace reports both paths (buggy multiplexer)
                vec![CorrelationKey::CheckoutPath(feat_path), CorrelationKey::CheckoutPath(main_path)],
                ProviderItemKey::Workspace("cmux:buggy-workspace".into()),
            ),
        ];

        let groups = correlate(items);
        // The workspace should attach to one checkout, not bridge them
        assert_eq!(groups.len(), 2, "two checkouts must stay in separate groups");
        // One group has 2 items (checkout + workspace), the other has 1 (checkout alone)
        let mut sizes: Vec<usize> = groups.iter().map(|g| g.items.len()).collect();
        sizes.sort();
        assert_eq!(sizes, vec![1, 2]);
    }

    #[test]
    fn two_change_requests_never_merge() {
        let items = vec![
            item(
                "github",
                ItemKind::ChangeRequest,
                "PR #1",
                vec![CorrelationKey::Branch("shared-branch".into())],
                ProviderItemKey::ChangeRequest("1".into()),
            ),
            item(
                "github",
                ItemKind::ChangeRequest,
                "PR #2",
                vec![CorrelationKey::Branch("shared-branch".into())],
                ProviderItemKey::ChangeRequest("2".into()),
            ),
        ];

        let groups = correlate(items);
        assert_eq!(groups.len(), 2, "two change requests must stay separate");
    }

    #[test]
    fn multiple_sessions_can_merge() {
        let items = vec![
            item(
                "git",
                ItemKind::Checkout,
                "feat-x",
                vec![CorrelationKey::Branch("feat-x".into())],
                ProviderItemKey::Checkout(hp("/code/feat-x")),
            ),
            item(
                "claude",
                ItemKind::CloudSession,
                "session-1",
                vec![CorrelationKey::Branch("feat-x".into())],
                ProviderItemKey::Session("sess-1".into()),
            ),
            item(
                "claude",
                ItemKind::CloudSession,
                "session-2",
                vec![CorrelationKey::Branch("feat-x".into())],
                ProviderItemKey::Session("sess-2".into()),
            ),
        ];

        let groups = correlate(items);
        assert_eq!(groups.len(), 1, "multiple sessions can share a group");
        assert_eq!(groups[0].items.len(), 3);
    }
}
