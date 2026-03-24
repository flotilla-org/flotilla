use std::path::PathBuf;

use flotilla_protocol::{
    delta::{Branch, BranchStatus},
    test_support::hp,
    AttachableId, AttachableSetId, ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession, Issue, ManagedTerminal,
    ProviderError, SessionStatus, TerminalStatus, Workspace,
};

use super::*;

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
        agent_keys: vec![],
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

// --- managed terminal diff/apply tests ---

fn terminal(role: &str, status: TerminalStatus) -> ManagedTerminal {
    ManagedTerminal {
        set_id: AttachableSetId::new("set-1"),
        role: role.into(),
        command: "bash".into(),
        working_directory: PathBuf::from("/repo"),
        status,
    }
}

#[test]
fn diff_terminal_added() {
    let prev = ProviderData::default();
    let mut curr = ProviderData::default();
    curr.managed_terminals.insert(AttachableId::new("t1"), terminal("editor", TerminalStatus::Running));
    let changes = diff_provider_data(&prev, &curr);
    assert_eq!(changes.len(), 1);
    assert!(matches!(&changes[0], Change::ManagedTerminal { key, op: EntryOp::Added(_) } if key.as_str() == "t1"));
}

#[test]
fn diff_terminal_removed() {
    let mut prev = ProviderData::default();
    prev.managed_terminals.insert(AttachableId::new("t1"), terminal("editor", TerminalStatus::Running));
    let curr = ProviderData::default();
    let changes = diff_provider_data(&prev, &curr);
    assert_eq!(changes.len(), 1);
    assert!(matches!(&changes[0], Change::ManagedTerminal { key, op: EntryOp::Removed } if key.as_str() == "t1"));
}

#[test]
fn diff_terminal_status_changed() {
    let mut prev = ProviderData::default();
    prev.managed_terminals.insert(AttachableId::new("t1"), terminal("editor", TerminalStatus::Running));
    let mut curr = ProviderData::default();
    curr.managed_terminals.insert(AttachableId::new("t1"), terminal("editor", TerminalStatus::Disconnected));
    let changes = diff_provider_data(&prev, &curr);
    assert_eq!(changes.len(), 1);
    assert!(matches!(&changes[0], Change::ManagedTerminal { key, op: EntryOp::Updated(_) } if key.as_str() == "t1"));
}

#[test]
fn roundtrip_terminal_changes() {
    let mut prev = ProviderData::default();
    prev.managed_terminals.insert(AttachableId::new("t1"), terminal("editor", TerminalStatus::Running));

    let mut curr = ProviderData::default();
    curr.managed_terminals.insert(AttachableId::new("t1"), terminal("editor", TerminalStatus::Exited(0)));
    curr.managed_terminals.insert(AttachableId::new("t2"), terminal("shell", TerminalStatus::Running));

    let changes = diff_provider_data(&prev, &curr);
    assert_eq!(changes.len(), 2);

    let mut applied = prev.clone();
    apply_changes(&mut applied, changes);
    assert_eq!(applied.managed_terminals, curr.managed_terminals);
}
