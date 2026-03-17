//! Delta computation: diff two IndexMaps to produce keyed entry operations,
//! and full `ProviderData` snapshot diffing.

use std::hash::Hash;

use flotilla_protocol::{Change, EntryOp, ProviderData, ProviderError, WorkItem, WorkItemIdentity};
use indexmap::IndexMap;

/// Diff two IndexMaps, producing `(key, EntryOp)` pairs for all differences.
///
/// - Keys in `curr` but not `prev` → `Added`
/// - Keys in both with different values → `Updated`
/// - Keys in `prev` but not `curr` → `Removed`
/// - Keys in both with equal values → omitted
pub fn diff_indexmap<K, V>(prev: &IndexMap<K, V>, curr: &IndexMap<K, V>) -> Vec<(K, EntryOp<V>)>
where
    K: Clone + Eq + Hash,
    V: Clone + PartialEq,
{
    let mut changes = Vec::new();

    for (key, curr_val) in curr {
        match prev.get(key) {
            Some(prev_val) if prev_val == curr_val => {}
            Some(_) => changes.push((key.clone(), EntryOp::Updated(curr_val.clone()))),
            None => changes.push((key.clone(), EntryOp::Added(curr_val.clone()))),
        }
    }

    for key in prev.keys() {
        if !curr.contains_key(key) {
            changes.push((key.clone(), EntryOp::Removed));
        }
    }

    changes
}

/// Diff two `ProviderData` snapshots, producing a `Vec<Change>` covering all collections.
pub fn diff_provider_data(prev: &ProviderData, curr: &ProviderData) -> Vec<Change> {
    let mut changes = Vec::new();

    for (key, op) in diff_indexmap(&prev.checkouts, &curr.checkouts) {
        changes.push(Change::Checkout { key, op });
    }
    for (key, op) in diff_indexmap(&prev.change_requests, &curr.change_requests) {
        changes.push(Change::ChangeRequest { key, op });
    }
    for (key, op) in diff_indexmap(&prev.issues, &curr.issues) {
        changes.push(Change::Issue { key, op });
    }
    for (key, op) in diff_indexmap(&prev.sessions, &curr.sessions) {
        changes.push(Change::Session { key, op });
    }
    for (key, op) in diff_indexmap(&prev.workspaces, &curr.workspaces) {
        changes.push(Change::Workspace { key, op });
    }
    for (key, op) in diff_indexmap(&prev.attachable_sets, &curr.attachable_sets) {
        changes.push(Change::AttachableSet { key, op });
    }
    for (key, op) in diff_indexmap(&prev.branches, &curr.branches) {
        changes.push(Change::Branch { key, op });
    }

    changes
}

/// Apply an `EntryOp` to an IndexMap entry.
fn apply_op<K, V>(map: &mut IndexMap<K, V>, key: K, op: EntryOp<V>)
where
    K: Eq + Hash,
{
    match op {
        EntryOp::Added(v) | EntryOp::Updated(v) => {
            map.insert(key, v);
        }
        EntryOp::Removed => {
            map.shift_remove(&key);
        }
    }
}

/// Apply a list of `Change`s to a `ProviderData` snapshot, mutating it in place.
pub fn apply_changes(pd: &mut ProviderData, changes: Vec<Change>) {
    for change in changes {
        match change {
            Change::Checkout { key, op } => apply_op(&mut pd.checkouts, key, op),
            Change::ChangeRequest { key, op } => apply_op(&mut pd.change_requests, key, op),
            Change::Issue { key, op } => apply_op(&mut pd.issues, key, op),
            Change::Session { key, op } => apply_op(&mut pd.sessions, key, op),
            Change::Workspace { key, op } => apply_op(&mut pd.workspaces, key, op),
            Change::AttachableSet { key, op } => apply_op(&mut pd.attachable_sets, key, op),
            Change::Branch { key, op } => apply_op(&mut pd.branches, key, op),
            // WorkItem and ProviderHealth are snapshot-level, not ProviderData-level.
            // They'll be handled at a higher layer.
            Change::WorkItem { .. } | Change::ProviderHealth { .. } | Change::ErrorsChanged(_) => {}
        }
    }
}

/// Compare two error lists — if different, return `Change::ErrorsChanged` with the new errors.
///
/// Errors lack stable identity, so this is a full replacement rather than keyed diffing.
pub fn diff_errors(prev: &[ProviderError], curr: &[ProviderError]) -> Option<Change> {
    if prev == curr {
        None
    } else {
        Some(Change::ErrorsChanged(curr.to_vec()))
    }
}

/// Diff two work item lists, producing `Change::WorkItem` entries.
///
/// Work items are keyed by `WorkItemIdentity`. The input slices are converted
/// to IndexMaps internally for O(n) diffing.
///
/// Not currently called in production — kept for the client-side materialization
/// path (delta replay on reconnect) planned in later PRs.
pub fn diff_work_items(prev: &[WorkItem], curr: &[WorkItem]) -> Vec<Change> {
    let prev_map: IndexMap<WorkItemIdentity, WorkItem> = prev.iter().map(|wi| (wi.identity.clone(), wi.clone())).collect();
    let curr_map: IndexMap<WorkItemIdentity, WorkItem> = curr.iter().map(|wi| (wi.identity.clone(), wi.clone())).collect();
    diff_indexmap(&prev_map, &curr_map).into_iter().map(|(identity, op)| Change::WorkItem { identity, op }).collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use flotilla_protocol::{
        delta::{Branch, BranchStatus},
        ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession, HostName, HostPath, Issue, ProviderError, SessionStatus,
        Workspace,
    };

    use super::*;

    fn hp(path: &str) -> HostPath {
        HostPath::new(HostName::new("test-host"), PathBuf::from(path))
    }

    fn checkout(branch: &str) -> Checkout {
        Checkout {
            branch: branch.into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        }
    }

    fn change_request(title: &str) -> ChangeRequest {
        ChangeRequest {
            title: title.into(),
            branch: "main".into(),
            status: ChangeRequestStatus::Open,
            body: None,
            correlation_keys: vec![],
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        }
    }

    fn issue(title: &str) -> Issue {
        Issue {
            title: title.into(),
            labels: vec![],
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        }
    }

    fn session(title: &str) -> CloudAgentSession {
        CloudAgentSession {
            title: title.into(),
            status: SessionStatus::Idle,
            model: None,
            updated_at: None,
            correlation_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
            item_noun: String::new(),
        }
    }

    fn workspace(name: &str) -> Workspace {
        Workspace { name: name.into(), directories: vec![], correlation_keys: vec![], attachable_set_id: None }
    }

    // --- diff_indexmap tests ---

    #[test]
    fn empty_to_empty() {
        let prev: IndexMap<String, i32> = IndexMap::new();
        let curr: IndexMap<String, i32> = IndexMap::new();
        assert!(diff_indexmap(&prev, &curr).is_empty());
    }

    #[test]
    fn empty_to_entries_all_added() {
        let prev: IndexMap<String, i32> = IndexMap::new();
        let curr: IndexMap<String, i32> = IndexMap::from([("a".into(), 1), ("b".into(), 2)]);
        let changes = diff_indexmap(&prev, &curr);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0], ("a".into(), EntryOp::Added(1)));
        assert_eq!(changes[1], ("b".into(), EntryOp::Added(2)));
    }

    #[test]
    fn entries_to_empty_all_removed() {
        let prev: IndexMap<String, i32> = IndexMap::from([("a".into(), 1), ("b".into(), 2)]);
        let curr: IndexMap<String, i32> = IndexMap::new();
        let changes = diff_indexmap(&prev, &curr);
        assert_eq!(changes.len(), 2);
        assert!(changes.contains(&("a".into(), EntryOp::Removed)));
        assert!(changes.contains(&("b".into(), EntryOp::Removed)));
    }

    #[test]
    fn identical_no_changes() {
        let map: IndexMap<String, i32> = IndexMap::from([("a".into(), 1), ("b".into(), 2)]);
        assert!(diff_indexmap(&map, &map).is_empty());
    }

    #[test]
    fn mixed_add_update_remove() {
        let prev: IndexMap<String, i32> = IndexMap::from([("a".into(), 1), ("b".into(), 2)]);
        let curr: IndexMap<String, i32> = IndexMap::from([("a".into(), 10), ("c".into(), 3)]);
        let changes = diff_indexmap(&prev, &curr);
        assert_eq!(changes.len(), 3);
        assert!(changes.contains(&("a".into(), EntryOp::Updated(10))));
        assert!(changes.contains(&("c".into(), EntryOp::Added(3))));
        assert!(changes.contains(&("b".into(), EntryOp::Removed)));
    }

    #[test]
    fn value_unchanged_key_not_emitted() {
        let prev: IndexMap<String, i32> = IndexMap::from([("a".into(), 1), ("b".into(), 2), ("c".into(), 3)]);
        let curr: IndexMap<String, i32> = IndexMap::from([("a".into(), 1), ("b".into(), 99), ("c".into(), 3)]);
        let changes = diff_indexmap(&prev, &curr);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0], ("b".into(), EntryOp::Updated(99)));
    }

    // --- diff_provider_data tests ---

    #[test]
    fn diff_empty_snapshots() {
        let prev = ProviderData::default();
        let curr = ProviderData::default();
        assert!(diff_provider_data(&prev, &curr).is_empty());
    }

    #[test]
    fn diff_checkout_added() {
        let prev = ProviderData::default();
        let mut curr = ProviderData::default();
        curr.checkouts.insert(hp("/wt/feat"), checkout("feat"));
        let changes = diff_provider_data(&prev, &curr);
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            Change::Checkout { key, op: EntryOp::Added(co) } => {
                assert_eq!(key, &hp("/wt/feat"));
                assert_eq!(co.branch, "feat");
            }
            other => panic!("unexpected change: {other:?}"),
        }
    }

    #[test]
    fn diff_checkout_removed() {
        let mut prev = ProviderData::default();
        prev.checkouts.insert(hp("/wt/old"), checkout("old"));
        let curr = ProviderData::default();
        let changes = diff_provider_data(&prev, &curr);
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            Change::Checkout { key, op: EntryOp::Removed } => {
                assert_eq!(key, &hp("/wt/old"));
            }
            other => panic!("unexpected change: {other:?}"),
        }
    }

    #[test]
    fn diff_change_request_updated() {
        let mut prev = ProviderData::default();
        prev.change_requests.insert("42".into(), change_request("old title"));
        let mut curr = ProviderData::default();
        curr.change_requests.insert("42".into(), change_request("new title"));
        let changes = diff_provider_data(&prev, &curr);
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            Change::ChangeRequest { key, op: EntryOp::Updated(cr) } => {
                assert_eq!(key, "42");
                assert_eq!(cr.title, "new title");
            }
            other => panic!("unexpected change: {other:?}"),
        }
    }

    #[test]
    fn diff_branch_added_and_removed() {
        let mut prev = ProviderData::default();
        prev.branches.insert("old-branch".into(), Branch { status: BranchStatus::Remote });
        let mut curr = ProviderData::default();
        curr.branches.insert("new-branch".into(), Branch { status: BranchStatus::Merged });
        let changes = diff_provider_data(&prev, &curr);
        assert_eq!(changes.len(), 2);
        // One added, one removed
        let added = changes.iter().any(|c| matches!(c, Change::Branch { key, op: EntryOp::Added(_) } if key == "new-branch"));
        let removed = changes.iter().any(|c| matches!(c, Change::Branch { key, op: EntryOp::Removed } if key == "old-branch"));
        assert!(added, "expected new-branch Added");
        assert!(removed, "expected old-branch Removed");
    }

    #[test]
    fn diff_mixed_collections() {
        let mut prev = ProviderData::default();
        prev.checkouts.insert(hp("/wt/main"), checkout("main"));
        prev.issues.insert("1".into(), issue("bug"));
        prev.sessions.insert("s1".into(), session("session 1"));

        let mut curr = ProviderData::default();
        curr.checkouts.insert(hp("/wt/main"), checkout("main")); // unchanged
        curr.issues.insert("1".into(), issue("bug fix")); // updated title
        curr.workspaces.insert("w1".into(), workspace("dev")); // added
                                                               // sessions removed

        let changes = diff_provider_data(&prev, &curr);
        assert_eq!(changes.len(), 3);
        let has_issue_update = changes.iter().any(|c| matches!(c, Change::Issue { key, op: EntryOp::Updated(_) } if key == "1"));
        let has_ws_add = changes.iter().any(|c| matches!(c, Change::Workspace { key, op: EntryOp::Added(_) } if key == "w1"));
        let has_session_remove = changes.iter().any(|c| matches!(c, Change::Session { key, op: EntryOp::Removed } if key == "s1"));
        assert!(has_issue_update, "expected issue Updated");
        assert!(has_ws_add, "expected workspace Added");
        assert!(has_session_remove, "expected session Removed");
    }

    fn work_item(identity: WorkItemIdentity, desc: &str) -> flotilla_protocol::WorkItem {
        flotilla_protocol::WorkItem {
            kind: flotilla_protocol::WorkItemKind::Checkout,
            identity,
            host: flotilla_protocol::HostName::new("test-host"),
            branch: None,
            description: desc.into(),
            checkout: None,
            change_request_key: None,
            session_key: None,
            issue_keys: vec![],
            workspace_refs: vec![],
            is_main_checkout: false,
            debug_group: vec![],
            source: None,
            terminal_keys: vec![],
            attachable_set_id: None,
        }
    }

    // --- diff_work_items tests ---

    #[test]
    fn diff_work_items_empty() {
        assert!(diff_work_items(&[], &[]).is_empty());
    }

    #[test]
    fn diff_work_items_added() {
        let curr = vec![work_item(flotilla_protocol::WorkItemIdentity::Session("s1".into()), "new session")];
        let changes = diff_work_items(&[], &curr);
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            Change::WorkItem { identity, op: EntryOp::Added(wi) } => {
                assert_eq!(identity, &flotilla_protocol::WorkItemIdentity::Session("s1".into()));
                assert_eq!(wi.description, "new session");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn diff_work_items_removed() {
        let prev = vec![work_item(flotilla_protocol::WorkItemIdentity::Issue("i1".into()), "old issue")];
        let changes = diff_work_items(&prev, &[]);
        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], Change::WorkItem { identity, op: EntryOp::Removed }
            if *identity == flotilla_protocol::WorkItemIdentity::Issue("i1".into())));
    }

    #[test]
    fn diff_work_items_updated() {
        let id = flotilla_protocol::WorkItemIdentity::Checkout(hp("/wt"));
        let prev = vec![work_item(id.clone(), "old desc")];
        let curr = vec![work_item(id.clone(), "new desc")];
        let changes = diff_work_items(&prev, &curr);
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            Change::WorkItem { identity, op: EntryOp::Updated(wi) } => {
                assert_eq!(identity, &id);
                assert_eq!(wi.description, "new desc");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn diff_work_items_unchanged() {
        let items = vec![work_item(flotilla_protocol::WorkItemIdentity::RemoteBranch("feat".into()), "remote branch")];
        assert!(diff_work_items(&items, &items).is_empty());
    }

    #[test]
    fn diff_identical_snapshots_no_changes() {
        let mut pd = ProviderData::default();
        pd.checkouts.insert(hp("/wt/main"), checkout("main"));
        pd.change_requests.insert("1".into(), change_request("pr 1"));
        pd.issues.insert("10".into(), issue("task"));
        pd.sessions.insert("s1".into(), session("sess"));
        pd.workspaces.insert("w1".into(), workspace("dev"));
        pd.branches.insert("feat".into(), Branch { status: BranchStatus::Remote });
        assert!(diff_provider_data(&pd, &pd).is_empty());
    }

    // --- apply_changes roundtrip tests ---

    /// Helper: verify diff + apply roundtrip.
    fn assert_roundtrip(prev: &ProviderData, curr: &ProviderData) {
        let changes = diff_provider_data(prev, curr);
        let mut result = prev.clone();
        apply_changes(&mut result, changes);
        assert_eq!(&result, curr);
    }

    #[test]
    fn roundtrip_empty_to_populated() {
        let prev = ProviderData::default();
        let mut curr = ProviderData::default();
        curr.checkouts.insert(hp("/wt/feat"), checkout("feat"));
        curr.change_requests.insert("1".into(), change_request("pr"));
        curr.issues.insert("10".into(), issue("bug"));
        curr.sessions.insert("s1".into(), session("sess"));
        curr.workspaces.insert("w1".into(), workspace("dev"));
        curr.branches.insert("main".into(), Branch { status: BranchStatus::Remote });
        assert_roundtrip(&prev, &curr);
    }

    #[test]
    fn roundtrip_populated_to_empty() {
        let mut prev = ProviderData::default();
        prev.checkouts.insert(hp("/wt/feat"), checkout("feat"));
        prev.issues.insert("10".into(), issue("bug"));
        prev.sessions.insert("s1".into(), session("sess"));
        let curr = ProviderData::default();
        assert_roundtrip(&prev, &curr);
    }

    #[test]
    fn roundtrip_mixed_changes() {
        let mut prev = ProviderData::default();
        prev.checkouts.insert(hp("/wt/main"), checkout("main"));
        prev.issues.insert("1".into(), issue("old bug"));
        prev.sessions.insert("s1".into(), session("session 1"));

        let mut curr = ProviderData::default();
        curr.checkouts.insert(hp("/wt/main"), checkout("main")); // unchanged
        curr.issues.insert("1".into(), issue("new bug")); // updated
        curr.workspaces.insert("w1".into(), workspace("dev")); // added
                                                               // sessions removed

        assert_roundtrip(&prev, &curr);
    }

    #[test]
    fn roundtrip_identical() {
        let mut pd = ProviderData::default();
        pd.checkouts.insert(hp("/wt/main"), checkout("main"));
        pd.branches.insert("feat".into(), Branch { status: BranchStatus::Merged });
        assert_roundtrip(&pd, &pd);
    }

    #[test]
    fn apply_added_then_removed() {
        let mut pd = ProviderData::default();
        let changes = vec![Change::Issue { key: "1".into(), op: EntryOp::Added(issue("task")) }, Change::Issue {
            key: "1".into(),
            op: EntryOp::Removed,
        }];
        apply_changes(&mut pd, changes);
        assert!(pd.issues.is_empty());
    }

    // --- diff_errors tests ---

    fn provider_error(category: &str, message: &str) -> ProviderError {
        ProviderError { category: category.into(), provider: String::new(), message: message.into() }
    }

    #[test]
    fn diff_errors_both_empty_no_change() {
        let prev: Vec<ProviderError> = vec![];
        let curr: Vec<ProviderError> = vec![];
        assert!(diff_errors(&prev, &curr).is_none());
    }

    #[test]
    fn diff_errors_empty_to_errors() {
        let prev: Vec<ProviderError> = vec![];
        let curr = vec![provider_error("git", "not found")];
        let change = diff_errors(&prev, &curr).expect("should produce ErrorsChanged");
        match change {
            Change::ErrorsChanged(errors) => {
                assert_eq!(errors.len(), 1);
                assert_eq!(errors[0].category, "git");
                assert_eq!(errors[0].message, "not found");
            }
            other => panic!("expected ErrorsChanged, got {other:?}"),
        }
    }

    #[test]
    fn diff_errors_errors_to_empty() {
        let prev = vec![provider_error("github", "rate limited")];
        let curr: Vec<ProviderError> = vec![];
        let change = diff_errors(&prev, &curr).expect("should produce ErrorsChanged");
        match change {
            Change::ErrorsChanged(errors) => assert!(errors.is_empty()),
            other => panic!("expected ErrorsChanged, got {other:?}"),
        }
    }

    #[test]
    fn diff_errors_same_no_change() {
        let errors = vec![provider_error("git", "error 1"), provider_error("github", "error 2")];
        assert!(diff_errors(&errors, &errors).is_none());
    }

    #[test]
    fn diff_errors_different_produces_change() {
        let prev = vec![provider_error("git", "old error")];
        let curr = vec![provider_error("git", "new error")];
        let change = diff_errors(&prev, &curr).expect("should produce ErrorsChanged");
        match change {
            Change::ErrorsChanged(errors) => {
                assert_eq!(errors.len(), 1);
                assert_eq!(errors[0].message, "new error");
            }
            other => panic!("expected ErrorsChanged, got {other:?}"),
        }
    }
}
