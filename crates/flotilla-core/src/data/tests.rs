use std::path::PathBuf;

use flotilla_protocol::test_support::{hp, qp, TestChangeRequest, TestCheckout, TestIssue, TestSession};

use super::*;
use crate::{provider_data::ProviderData, providers::types::*};

// -----------------------------------------------------------------------
// Helper: build a minimal CorrelatedWorkItem with sensible defaults
// -----------------------------------------------------------------------

fn correlated(anchor: CorrelatedAnchor) -> CorrelatedWorkItem {
    let checkout_ref = match &anchor {
        CorrelatedAnchor::Checkout(co) => Some(co.clone()),
        _ => None,
    };
    let attachable_set_id = match &anchor {
        CorrelatedAnchor::AttachableSet(id) => Some(id.clone()),
        _ => None,
    };
    CorrelatedWorkItem {
        anchor,
        checkout_ref,
        attachable_set_id,
        branch: None,
        description: String::new(),
        linked_change_request: None,
        linked_session: None,
        linked_issues: Vec::new(),
        workspace_refs: Vec::new(),
        correlation_group_idx: 0,
        host: None,
        source: None,
        terminal_ids: vec![],
        agent_keys: vec![],
    }
}

fn checkout_item(path: &str, branch: Option<&str>, is_main: bool) -> CorrelationResult {
    CorrelationResult::Correlated(CorrelatedWorkItem {
        branch: branch.map(|s| s.to_string()),
        description: branch.unwrap_or("").to_string(),
        ..correlated(CorrelatedAnchor::Checkout(CheckoutRef::from_host_path(hp(path), is_main)))
    })
}

fn cr_item(key: &str, desc: &str) -> CorrelationResult {
    CorrelationResult::Correlated(CorrelatedWorkItem {
        description: desc.to_string(),
        ..correlated(CorrelatedAnchor::ChangeRequest(key.to_string()))
    })
}

fn session_item(key: &str, desc: &str) -> CorrelationResult {
    CorrelationResult::Correlated(CorrelatedWorkItem {
        description: desc.to_string(),
        ..correlated(CorrelatedAnchor::Session(key.to_string()))
    })
}

fn issue_item(key: &str, desc: &str) -> CorrelationResult {
    CorrelationResult::Standalone(StandaloneResult::Issue { key: key.to_string(), description: desc.to_string(), source: String::new() })
}

fn remote_branch_item(branch: &str) -> CorrelationResult {
    CorrelationResult::Standalone(StandaloneResult::RemoteBranch { branch: branch.to_string() })
}

fn make_workspace(_ws_ref: &str, name: &str, correlation_keys: Vec<CorrelationKey>) -> Workspace {
    Workspace { name: name.to_string(), correlation_keys, attachable_set_id: None }
}

fn make_attachable_set(id: &str, path: &str) -> flotilla_protocol::AttachableSet {
    flotilla_protocol::AttachableSet {
        id: flotilla_protocol::AttachableSetId::new(id),
        host_affinity: Some(flotilla_protocol::HostName::new("test-host")),
        checkout: Some(flotilla_protocol::HostPath::new(flotilla_protocol::HostName::new("test-host"), PathBuf::from(path)).into()),
        template_identity: None,
        environment_id: None,
        members: vec![],
    }
}

// Convert CorrelationResult to protocol WorkItem for group_work_items tests
fn to_proto(item: &CorrelationResult) -> flotilla_protocol::WorkItem {
    crate::convert::correlation_result_to_work_item(item, &[], &flotilla_protocol::HostName::new("test-host"))
}

fn new_providers() -> ProviderData {
    ProviderData::default()
}

fn default_labels() -> SectionLabels {
    SectionLabels::default()
}

// -----------------------------------------------------------------------
// Display / formatting tests
// -----------------------------------------------------------------------

#[test]
fn refresh_error_display() {
    let err = RefreshError { category: "github", provider: "GitHub".to_string(), message: "rate limited".to_string() };
    assert_eq!(format!("{err}"), "github/GitHub: rate limited");
}

// -----------------------------------------------------------------------
// WorkItemKind classification tests
// -----------------------------------------------------------------------

#[test]
fn kind_returns_correct_variant() {
    let cases = [
        ("checkout", checkout_item("/tmp/foo", None, false), WorkItemKind::Checkout),
        ("change_request", cr_item("42", "PR title"), WorkItemKind::ChangeRequest),
        ("session", session_item("sess-1", "Session title"), WorkItemKind::Session),
        ("issue", issue_item("7", "Fix bug"), WorkItemKind::Issue),
        ("remote_branch", remote_branch_item("feature/x"), WorkItemKind::RemoteBranch),
    ];
    for (label, item, expected) in cases {
        assert_eq!(item.kind(), expected, "failed for {label}");
    }
}

// -----------------------------------------------------------------------
// Accessor tests: branch()
// -----------------------------------------------------------------------

#[test]
fn branch_returns_expected_value() {
    let cases: [(&str, CorrelationResult, Option<&str>); 4] = [
        ("checkout_with_branch", checkout_item("/tmp/wt", Some("feat-x"), false), Some("feat-x")),
        ("checkout_without_branch", checkout_item("/tmp/wt", None, false), None),
        ("remote_branch", remote_branch_item("origin/develop"), Some("origin/develop")),
        ("issue", issue_item("1", "desc"), None),
    ];
    for (label, item, expected) in cases {
        assert_eq!(item.branch(), expected, "failed for {label}");
    }
}

#[test]
fn branch_from_change_request_correlated() {
    let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
        branch: Some("cr-branch".to_string()),
        ..correlated(CorrelatedAnchor::ChangeRequest("10".to_string()))
    });
    assert_eq!(wi.branch(), Some("cr-branch"));
}

// -----------------------------------------------------------------------
// Accessor tests: description()
// -----------------------------------------------------------------------

#[test]
fn description_returns_expected_value() {
    let cases = [
        ("correlated", cr_item("1", "Fix login flow"), "Fix login flow"),
        ("standalone_issue", issue_item("5", "Add caching"), "Add caching"),
        ("remote_branch", remote_branch_item("feature/auth"), "feature/auth"),
    ];
    for (label, item, expected) in cases {
        assert_eq!(item.description(), expected, "failed for {label}");
    }
}

// -----------------------------------------------------------------------
// Accessor tests: checkout(), checkout_key(), is_main_checkout()
// -----------------------------------------------------------------------

#[test]
fn checkout_returns_some_for_checkout_anchor() {
    let wi = checkout_item("/tmp/wt", Some("main"), true);
    let co = wi.checkout().expect("should return checkout");
    assert_eq!(co.host_path(), Some(&hp("/tmp/wt")));
    assert!(co.is_main_checkout);
}

#[test]
fn checkout_returns_none_for_non_checkout() {
    let cases: [(&str, CorrelationResult); 4] = [
        ("change_request", cr_item("1", "d")),
        ("session", session_item("s", "d")),
        ("issue", issue_item("i", "d")),
        ("remote_branch", remote_branch_item("b")),
    ];
    for (label, item) in cases {
        assert!(item.checkout().is_none(), "checkout() should be None for {label}");
        assert!(item.checkout_key().is_none(), "checkout_key() should be None for {label}");
        assert!(!item.is_main_checkout(), "is_main_checkout() should be false for {label}");
    }
}

#[test]
fn checkout_key_returns_path() {
    let wi = checkout_item("/repos/proj", None, false);
    assert_eq!(wi.checkout_key(), Some(&qp("/repos/proj")));
}

#[test]
fn host_falls_back_to_checkout_host_provenance_when_key_has_host_id() {
    let checkout_key = flotilla_protocol::qualified_path::QualifiedPath::host(
        flotilla_protocol::qualified_path::HostId::new("peer-host-id"),
        "/repos/feat-remote",
    );
    let mut providers = ProviderData::default();
    providers.checkouts.insert(checkout_key.clone(), Checkout {
        branch: "feat-remote".into(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![CorrelationKey::Branch("feat-remote".into()), CorrelationKey::CheckoutPath(checkout_key.clone())],
        association_keys: vec![],
        host_name: Some(flotilla_protocol::HostName::new("follower")),
        environment_id: None,
    });

    let grouped = crate::data::correlate(&providers).0;
    let checkout = grouped.iter().find(|item| item.branch() == Some("feat-remote")).expect("correlated checkout");

    assert_eq!(checkout.host(&flotilla_protocol::HostName::new("leader")), flotilla_protocol::HostName::new("follower"));
    let proto = crate::convert::correlation_result_to_work_item(checkout, &[], &flotilla_protocol::HostName::new("leader"));
    assert_eq!(proto.host, flotilla_protocol::HostName::new("follower"));
}

#[test]
fn is_main_checkout_true() {
    let wi = checkout_item("/repos/main", Some("main"), true);
    assert!(wi.is_main_checkout());
}

#[test]
fn is_main_checkout_false_for_non_main() {
    let wi = checkout_item("/repos/feat", Some("feat"), false);
    assert!(!wi.is_main_checkout());
}

// -----------------------------------------------------------------------
// Accessor tests: change_request_key()
// -----------------------------------------------------------------------

#[test]
fn change_request_key_returns_expected_value() {
    let cases: [(&str, CorrelationResult, Option<&str>); 3] = [
        ("cr_anchor", cr_item("42", "PR"), Some("42")),
        ("issue", issue_item("1", "d"), None),
        ("remote_branch", remote_branch_item("b"), None),
    ];
    for (label, item, expected) in cases {
        assert_eq!(item.change_request_key(), expected, "failed for {label}");
    }
}

#[test]
fn change_request_key_from_linked_on_checkout() {
    let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
        linked_change_request: Some("99".to_string()),
        ..correlated(CorrelatedAnchor::Checkout(CheckoutRef::from_host_path(hp("/tmp/wt"), false)))
    });
    assert_eq!(wi.change_request_key(), Some("99"));
}

// -----------------------------------------------------------------------
// Accessor tests: session_key()
// -----------------------------------------------------------------------

#[test]
fn session_key_returns_expected_value() {
    let cases: [(&str, CorrelationResult, Option<&str>); 2] =
        [("session_anchor", session_item("sess-x", "title"), Some("sess-x")), ("issue", issue_item("1", "d"), None)];
    for (label, item, expected) in cases {
        assert_eq!(item.session_key(), expected, "failed for {label}");
    }
}

#[test]
fn session_key_from_linked_on_checkout() {
    let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
        linked_session: Some("linked-sess".to_string()),
        ..correlated(CorrelatedAnchor::Checkout(CheckoutRef::from_host_path(hp("/tmp/wt"), false)))
    });
    assert_eq!(wi.session_key(), Some("linked-sess"));
}

// -----------------------------------------------------------------------
// Accessor tests: issue_keys()
// -----------------------------------------------------------------------

#[test]
fn issue_keys_from_correlated_with_linked_issues() {
    let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
        linked_issues: vec!["10".to_string(), "20".to_string()],
        ..correlated(CorrelatedAnchor::Checkout(CheckoutRef::from_host_path(hp("/tmp/wt"), false)))
    });
    assert_eq!(wi.issue_keys(), &["10".to_string(), "20".to_string()]);
}

#[test]
fn issue_keys_returns_expected_value() {
    let cases: [(&str, CorrelationResult, &[String]); 2] =
        [("standalone_issue", issue_item("42", "desc"), &["42".to_string()]), ("remote_branch", remote_branch_item("b"), &[])];
    for (label, item, expected) in cases {
        assert_eq!(item.issue_keys(), expected, "failed for {label}");
    }
}

// -----------------------------------------------------------------------
// Accessor tests: workspace_refs()
// -----------------------------------------------------------------------

#[test]
fn workspace_refs_from_correlated() {
    let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
        workspace_refs: vec!["ws-1".to_string()],
        ..correlated(CorrelatedAnchor::Checkout(CheckoutRef::from_host_path(hp("/tmp/wt"), false)))
    });
    assert_eq!(wi.workspace_refs(), &["ws-1".to_string()]);
}

#[test]
fn workspace_refs_empty_for_standalone() {
    let cases: [(&str, CorrelationResult); 2] = [("issue", issue_item("1", "d")), ("remote_branch", remote_branch_item("b"))];
    for (label, item) in cases {
        assert!(item.workspace_refs().is_empty(), "workspace_refs() should be empty for {label}");
    }
}

// -----------------------------------------------------------------------
// Accessor tests: correlation_group_idx()
// -----------------------------------------------------------------------

#[test]
fn correlation_group_idx_from_correlated() {
    let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
        correlation_group_idx: 7,
        ..correlated(CorrelatedAnchor::Session("s".to_string()))
    });
    assert_eq!(wi.correlation_group_idx(), Some(7));
}

#[test]
fn correlation_group_idx_none_for_standalone() {
    let cases: [(&str, CorrelationResult); 2] = [("issue", issue_item("1", "d")), ("remote_branch", remote_branch_item("b"))];
    for (label, item) in cases {
        assert!(item.correlation_group_idx().is_none(), "correlation_group_idx() should be None for {label}");
    }
}

// -----------------------------------------------------------------------
// Accessor tests: as_correlated_mut()
// -----------------------------------------------------------------------

#[test]
fn as_correlated_mut_returns_some_for_correlated() {
    let mut wi = checkout_item("/tmp/wt", Some("feat"), false);
    let inner = wi.as_correlated_mut().expect("should be Some");
    inner.linked_issues.push("99".to_string());
    assert_eq!(wi.issue_keys(), &["99".to_string()]);
}

#[test]
fn as_correlated_mut_returns_none_for_standalone() {
    let mut wi = issue_item("1", "d");
    assert!(wi.as_correlated_mut().is_none());
}

// -----------------------------------------------------------------------
// Identity tests
// -----------------------------------------------------------------------

#[test]
fn identity_returns_correct_variant() {
    let cases = [
        ("checkout", checkout_item("/tmp/foo", None, false), WorkItemIdentity::Checkout(qp("/tmp/foo"))),
        ("change_request", cr_item("42", "PR"), WorkItemIdentity::ChangeRequest("42".to_string())),
        ("session", session_item("sess-1", "title"), WorkItemIdentity::Session("sess-1".to_string())),
        ("issue", issue_item("7", "desc"), WorkItemIdentity::Issue("7".to_string())),
        ("remote_branch", remote_branch_item("feature/x"), WorkItemIdentity::RemoteBranch("feature/x".to_string())),
    ];
    for (label, item, expected) in cases {
        assert_eq!(item.identity(), expected, "failed for {label}");
    }
}

// -----------------------------------------------------------------------
// correlate() tests
// -----------------------------------------------------------------------

#[test]
fn correlate_empty_provider_data() {
    let providers = new_providers();
    let (items, groups) = correlate(&providers);
    assert!(items.is_empty());
    assert!(groups.is_empty());
}

#[test]
fn correlate_single_checkout() {
    let mut providers = new_providers();
    providers.checkouts.insert(qp("/tmp/feat"), TestCheckout::new("feat").at("/tmp/feat").is_main(false).with_branch_key().build());

    let (items, groups) = correlate(&providers);
    assert_eq!(items.len(), 1);
    assert_eq!(groups.len(), 1);
    assert_eq!(items[0].kind(), WorkItemKind::Checkout);
    assert_eq!(items[0].branch(), Some("feat"));
}

#[test]
fn correlate_trunk_checkout_marked_as_main() {
    let mut providers = new_providers();
    providers.checkouts.insert(qp("/tmp/main"), TestCheckout::new("main").at("/tmp/main").is_main(true).with_branch_key().build());

    let (items, _) = correlate(&providers);
    assert_eq!(items.len(), 1);
    assert!(items[0].is_main_checkout());
}

#[test]
fn correlate_checkout_and_pr_merge_on_branch() {
    let mut providers = new_providers();
    providers.checkouts.insert(qp("/tmp/feat-x"), TestCheckout::new("feat-x").at("/tmp/feat-x").is_main(false).with_branch_key().build());
    providers.change_requests.insert("10".to_string(), TestChangeRequest::new("Add auth", "feat-x").with_branch_key().build());

    let (items, _) = correlate(&providers);
    // Should merge into one work item
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind(), WorkItemKind::Checkout); // checkout is preferred anchor
    assert_eq!(items[0].change_request_key(), Some("10"));
    // Description comes from PR title since it's non-empty
    assert_eq!(items[0].description(), "Add auth");
}

#[test]
fn correlate_agent_only_becomes_agent_anchor() {
    let mut providers = new_providers();
    providers.agents.insert("att-1".to_string(), flotilla_protocol::Agent {
        harness: flotilla_protocol::AgentHarness::ClaudeCode,
        status: flotilla_protocol::AgentStatus::Active,
        model: Some("opus-4".into()),
        context: flotilla_protocol::AgentContext::Local { attachable_id: flotilla_protocol::AttachableId::new("att-1") },
        correlation_keys: vec![],
        provider_name: "cli-agent".into(),
        provider_display_name: "CLI Agent".into(),
        item_noun: "agent".into(),
    });

    let (items, _) = correlate(&providers);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind(), WorkItemKind::Agent);
}

#[test]
fn correlate_agent_with_terminal_via_attachable_set() {
    let mut providers = new_providers();
    let set_id = flotilla_protocol::AttachableSetId::new("set-1");

    // AttachableSet
    providers.attachable_sets.insert(set_id.clone(), make_attachable_set("set-1", "/repo/feat"));

    // Checkout in same set (via CheckoutPath)
    providers
        .checkouts
        .insert(qp("/repo/feat"), TestCheckout::new("feat-branch").at("/repo/feat").is_main(false).with_branch_key().build());

    // Agent with same attachable set key
    providers.agents.insert("att-1".to_string(), flotilla_protocol::Agent {
        harness: flotilla_protocol::AgentHarness::ClaudeCode,
        status: flotilla_protocol::AgentStatus::Active,
        model: None,
        context: flotilla_protocol::AgentContext::Local { attachable_id: flotilla_protocol::AttachableId::new("att-1") },
        correlation_keys: vec![CorrelationKey::AttachableSet(set_id)],
        provider_name: "cli-agent".into(),
        provider_display_name: "CLI Agent".into(),
        item_noun: "agent".into(),
    });

    let (items, _) = correlate(&providers);
    // Checkout is the anchor (higher priority than agent), both correlated together
    let checkout_items: Vec<_> = items.iter().filter(|wi| wi.kind() == WorkItemKind::Checkout).collect();
    assert_eq!(checkout_items.len(), 1, "agent and checkout should merge into one work item");
    // The standalone agent shouldn't appear since it merged with the checkout
    let agent_items: Vec<_> = items.iter().filter(|wi| wi.kind() == WorkItemKind::Agent).collect();
    assert_eq!(agent_items.len(), 0, "agent should merge with checkout, not appear standalone");
}

#[test]
fn correlate_checkout_pr_session_merge_on_branch() {
    let mut providers = new_providers();
    providers.checkouts.insert(qp("/tmp/feat-y"), TestCheckout::new("feat-y").at("/tmp/feat-y").is_main(false).with_branch_key().build());
    providers.change_requests.insert("20".to_string(), TestChangeRequest::new("Improve perf", "feat-y").with_branch_key().build());
    providers.sessions.insert(
        "sess-a".to_string(),
        TestSession::new("Debug perf").with_session_ref("claude", "sess-a").with_branch_key("feat-y").build(),
    );

    let (items, _) = correlate(&providers);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind(), WorkItemKind::Checkout);
    assert_eq!(items[0].change_request_key(), Some("20"));
    assert_eq!(items[0].session_key(), Some("sess-a"));
}

#[test]
fn correlate_session_only_becomes_session_anchor() {
    let mut providers = new_providers();
    providers
        .sessions
        .insert("sess-lonely".to_string(), TestSession::new("Solo session").with_session_ref("claude", "sess-lonely").build());

    let (items, _) = correlate(&providers);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind(), WorkItemKind::Session);
    assert_eq!(items[0].session_key(), Some("sess-lonely"));
}

#[test]
fn correlate_pr_only_becomes_cr_anchor() {
    let mut providers = new_providers();
    providers.change_requests.insert("50".to_string(), TestChangeRequest::new("Orphan PR", "no-checkout-branch").with_branch_key().build());

    let (items, _) = correlate(&providers);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind(), WorkItemKind::ChangeRequest);
    assert_eq!(items[0].change_request_key(), Some("50"));
}

#[test]
fn correlate_standalone_issue_appears_as_issue() {
    let mut providers = new_providers();
    providers.issues.insert("100".to_string(), TestIssue::new("Standalone bug").build());

    let (items, _) = correlate(&providers);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind(), WorkItemKind::Issue);
    assert_eq!(items[0].description(), "Standalone bug");
}

#[test]
fn correlate_remote_branches_appear_as_standalone() {
    let mut providers = new_providers();
    providers
        .branches
        .insert("feature/remote-only".to_string(), flotilla_protocol::delta::Branch { status: flotilla_protocol::BranchStatus::Remote });

    let (items, _) = correlate(&providers);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind(), WorkItemKind::RemoteBranch);
    assert_eq!(items[0].branch(), Some("feature/remote-only"));
}

#[test]
fn correlate_remote_branches_excludes_head_main_master() {
    let mut providers = new_providers();
    let remote = flotilla_protocol::delta::Branch { status: flotilla_protocol::BranchStatus::Remote };
    providers.branches.insert("HEAD".to_string(), remote.clone());
    providers.branches.insert("main".to_string(), remote.clone());
    providers.branches.insert("master".to_string(), remote.clone());
    providers.branches.insert("feature/visible".to_string(), remote);

    let (items, _) = correlate(&providers);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].branch(), Some("feature/visible"));
}

#[test]
fn correlate_remote_branches_excludes_already_known() {
    let mut providers = new_providers();
    // A checkout on branch "feat-z"
    providers.checkouts.insert(qp("/tmp/feat-z"), TestCheckout::new("feat-z").at("/tmp/feat-z").is_main(false).with_branch_key().build());
    // Same branch also in remote
    providers.branches.insert("feat-z".to_string(), flotilla_protocol::delta::Branch { status: flotilla_protocol::BranchStatus::Remote });

    let (items, _) = correlate(&providers);
    // Should only have the checkout, not a duplicate remote
    let remote_items: Vec<_> = items.iter().filter(|wi| wi.kind() == WorkItemKind::RemoteBranch).collect();
    assert!(remote_items.is_empty());
}

#[test]
fn correlate_merged_branches_excluded() {
    let mut providers = new_providers();
    providers
        .branches
        .insert("already-merged".to_string(), flotilla_protocol::delta::Branch { status: flotilla_protocol::BranchStatus::Merged });

    let (items, _) = correlate(&providers);
    assert!(items.is_empty());
}

#[test]
fn correlate_pr_links_issue_via_association_key() {
    let mut providers = new_providers();
    providers.checkouts.insert(qp("/tmp/feat"), TestCheckout::new("feat").at("/tmp/feat").is_main(false).with_branch_key().build());
    let mut cr = TestChangeRequest::new("Impl feature", "feat").with_branch_key().build();
    cr.association_keys.push(AssociationKey::IssueRef("gh".to_string(), "77".to_string()));
    providers.change_requests.insert("5".to_string(), cr);
    providers.issues.insert("77".to_string(), TestIssue::new("Feature request").build());

    let (items, _) = correlate(&providers);
    let checkout = items.iter().find(|wi| wi.kind() == WorkItemKind::Checkout).expect("should have checkout");
    assert!(checkout.issue_keys().contains(&"77".to_string()));
    // Issue should not appear standalone
    assert!(!items.iter().any(|wi| wi.kind() == WorkItemKind::Issue));
}

#[test]
fn checkout_association_keys_link_issues() {
    let mut providers = new_providers();

    let co_path = qp("/tmp/feat-x");
    let mut co = TestCheckout::new("feat-x").at("/tmp/feat-x").is_main(false).with_branch_key().build();
    co.association_keys.push(AssociationKey::IssueRef("github".into(), "42".into()));
    providers.checkouts.insert(co_path, co);
    providers.issues.insert("42".to_string(), TestIssue::new("Fix the thing").build());

    let (work_items, _groups) = correlate(&providers);
    let checkout_wi = work_items.iter().find(|wi| wi.kind() == WorkItemKind::Checkout).expect("should have a checkout work item");
    assert!(
        checkout_wi.issue_keys().contains(&"42".to_string()),
        "checkout should link issue 42 via association key, got: {:?}",
        checkout_wi.issue_keys()
    );
    let standalone_issues: Vec<_> = work_items.iter().filter(|wi| wi.kind() == WorkItemKind::Issue).collect();
    assert!(standalone_issues.is_empty(), "issue 42 should be linked, not standalone");
}

#[test]
fn correlate_workspace_only_group_is_skipped() {
    // A workspace with no checkout/PR/session should be excluded
    let mut providers = new_providers();
    providers.workspaces.insert("ws-orphan".to_string(), make_workspace("ws-orphan", "orphan", vec![]));

    let (items, _) = correlate(&providers);
    assert!(items.is_empty(), "workspace-only group should be skipped");
}

#[test]
fn correlate_workspace_without_attachable_set_is_not_linked_to_checkout() {
    let mut providers = new_providers();
    let co_path = qp("/tmp/feat-ws");
    providers.checkouts.insert(co_path.clone(), TestCheckout::new("feat-ws").at("/tmp/feat-ws").is_main(false).with_branch_key().build());
    providers.workspaces.insert("ws-1".to_string(), make_workspace("ws-1", "dev-session", vec![CorrelationKey::CheckoutPath(co_path)]));

    let (items, _) = correlate(&providers);
    assert_eq!(items.len(), 1);
    assert!(items[0].workspace_refs().is_empty());
}

#[test]
fn correlate_description_prefers_pr_title() {
    let mut providers = new_providers();
    providers.checkouts.insert(qp("/tmp/feat"), TestCheckout::new("feat").at("/tmp/feat").is_main(false).with_branch_key().build());
    providers.change_requests.insert("1".to_string(), TestChangeRequest::new("My PR Title", "feat").with_branch_key().build());
    providers
        .sessions
        .insert("s1".to_string(), TestSession::new("My Session Title").with_session_ref("claude", "s1").with_branch_key("feat").build());

    let (items, _) = correlate(&providers);
    assert_eq!(items[0].description(), "My PR Title");
}

#[test]
fn correlate_description_falls_back_to_session_title() {
    let mut providers = new_providers();
    providers.checkouts.insert(qp("/tmp/feat"), TestCheckout::new("feat").at("/tmp/feat").is_main(false).with_branch_key().build());
    providers
        .sessions
        .insert("s1".to_string(), TestSession::new("Session Title").with_session_ref("claude", "s1").with_branch_key("feat").build());

    let (items, _) = correlate(&providers);
    assert_eq!(items[0].description(), "Session Title");
}

#[test]
fn correlate_description_falls_back_to_branch() {
    let mut providers = new_providers();
    providers
        .checkouts
        .insert(qp("/tmp/my-branch"), TestCheckout::new("my-branch").at("/tmp/my-branch").is_main(false).with_branch_key().build());

    let (items, _) = correlate(&providers);
    assert_eq!(items[0].description(), "my-branch");
}

#[test]
fn correlate_multiple_items_sharing_branch_merge() {
    let mut providers = new_providers();
    providers
        .checkouts
        .insert(qp("/tmp/shared"), TestCheckout::new("shared-branch").at("/tmp/shared").is_main(false).with_branch_key().build());
    providers.change_requests.insert("1".to_string(), TestChangeRequest::new("PR on shared", "shared-branch").with_branch_key().build());
    providers.sessions.insert(
        "s1".to_string(),
        TestSession::new("Session on shared").with_session_ref("claude", "s1").with_branch_key("shared-branch").build(),
    );

    let (items, _) = correlate(&providers);
    assert_eq!(items.len(), 1, "all items should merge into one");
    assert_eq!(items[0].kind(), WorkItemKind::Checkout);
    assert_eq!(items[0].change_request_key(), Some("1"));
    assert_eq!(items[0].session_key(), Some("s1"));
    assert_eq!(items[0].branch(), Some("shared-branch"));
}

#[test]
fn correlate_two_checkouts_stay_separate() {
    let mut providers = new_providers();
    providers.checkouts.insert(qp("/tmp/a"), TestCheckout::new("branch-a").at("/tmp/a").is_main(false).with_branch_key().build());
    providers.checkouts.insert(qp("/tmp/b"), TestCheckout::new("branch-b").at("/tmp/b").is_main(false).with_branch_key().build());

    let (items, _) = correlate(&providers);
    assert_eq!(items.len(), 2);
    let branches: HashSet<_> = items.iter().filter_map(|wi| wi.branch()).collect();
    assert!(branches.contains("branch-a"));
    assert!(branches.contains("branch-b"));
}

#[test]
fn correlate_issue_not_in_provider_data_ignored_by_association() {
    // An association key pointing to a non-existent issue should be ignored
    let mut providers = new_providers();
    let mut cr = TestChangeRequest::new("PR", "feat").with_branch_key().build();
    cr.association_keys.push(AssociationKey::IssueRef("gh".into(), "999".into()));
    providers.change_requests.insert("5".to_string(), cr);
    // Note: no issue "999" in providers.issues

    let (items, _) = correlate(&providers);
    let cr_item = items.iter().find(|wi| wi.kind() == WorkItemKind::ChangeRequest).unwrap();
    assert!(cr_item.issue_keys().is_empty());
}

// -----------------------------------------------------------------------
// SectionLabels default test
// -----------------------------------------------------------------------

#[test]
fn section_labels_default_values() {
    let labels = default_labels();
    assert_eq!(labels.checkouts, "Checkouts");
    assert_eq!(labels.change_requests, "Change Requests");
    assert_eq!(labels.issues, "Issues");
    assert_eq!(labels.sessions, "Sessions");
}

fn test_session_work_item(id: &str) -> flotilla_protocol::WorkItem {
    flotilla_protocol::WorkItem {
        kind: WorkItemKind::Session,
        identity: WorkItemIdentity::Session(id.into()),
        host: flotilla_protocol::HostName::local(),
        branch: None,
        description: format!("session {id}"),
        checkout: None,
        change_request_key: None,
        session_key: Some(id.into()),
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
        source: None,
        terminal_keys: Vec::new(),
        attachable_set_id: None,
        agent_keys: Vec::new(),
    }
}

fn test_cloud_agent_session(status: flotilla_protocol::SessionStatus) -> flotilla_protocol::CloudAgentSession {
    flotilla_protocol::CloudAgentSession {
        title: String::new(),
        status,
        model: None,
        updated_at: None,
        correlation_keys: Vec::new(),
        provider_name: String::new(),
        provider_display_name: String::new(),
        item_noun: String::new(),
    }
}

// -----------------------------------------------------------------------
// DataStore default test
// -----------------------------------------------------------------------

#[test]
fn data_store_default() {
    let ds = DataStore::default();
    assert!(!ds.loading);
    assert!(ds.correlation_groups.is_empty());
    assert!(ds.provider_health.is_empty());
}

// -----------------------------------------------------------------------
// Integration-style: end-to-end correlate + group
// -----------------------------------------------------------------------

#[test]
fn end_to_end_mixed_providers() {
    let mut providers = new_providers();

    // trunk checkout
    providers.checkouts.insert(qp("/repo"), TestCheckout::new("main").at("/repo").is_main(true).with_branch_key().build());
    // feature checkout + PR
    providers.checkouts.insert(qp("/repo.feat"), TestCheckout::new("feat-login").at("/repo.feat").is_main(false).with_branch_key().build());
    providers.change_requests.insert("10".to_string(), TestChangeRequest::new("Add login", "feat-login").with_branch_key().build());
    // standalone session
    providers.sessions.insert("s-solo".to_string(), TestSession::new("Solo work").with_session_ref("claude", "s-solo").build());
    // standalone issue
    providers.issues.insert("55".to_string(), TestIssue::new("Improve docs").build());
    // remote-only branch
    providers
        .branches
        .insert("experiment/alpha".to_string(), flotilla_protocol::delta::Branch { status: flotilla_protocol::BranchStatus::Remote });

    let (work_items, _) = correlate(&providers);

    // Expected: main checkout, feat checkout (with PR), solo session, issue, remote branch
    assert_eq!(work_items.len(), 5);

    let kinds: Vec<WorkItemKind> = work_items.iter().map(|wi| wi.kind()).collect();
    assert!(kinds.contains(&WorkItemKind::Checkout));
    assert!(kinds.contains(&WorkItemKind::Session));
    assert!(kinds.contains(&WorkItemKind::Issue));
    assert!(kinds.contains(&WorkItemKind::RemoteBranch));

    // The feat checkout should have the PR linked
    let feat = work_items.iter().find(|wi| wi.branch() == Some("feat-login")).expect("should have feat-login");
    assert_eq!(feat.change_request_key(), Some("10"));
    assert!(!feat.is_main_checkout());

    // main checkout should be flagged as main
    let main_item = work_items.iter().find(|wi| wi.branch() == Some("main")).expect("should have main");
    assert!(main_item.is_main_checkout());

    // Now group them into sections
    let labels = default_labels();
    let proto_items: Vec<_> = work_items.iter().map(to_proto).collect();
    let sections = group_work_items_split(&proto_items, &providers, &labels, Path::new("/tmp"));

    // Should have sections for checkouts, sessions, remote, issues
    assert_eq!(sections.len(), 4, "should have exactly 4 sections");

    // All items should be present
    let total: usize = sections.iter().map(|s| s.items.len()).sum();
    assert_eq!(total, 5);
}

#[test]
fn workspace_only_joins_checkout_through_attachable_set() {
    let mut providers = new_providers();

    let remote_checkout =
        flotilla_protocol::qualified_path::QualifiedPath::from_host_name(&flotilla_protocol::HostName::new("feta"), "/remote/feat-set");
    let local_checkout = flotilla_protocol::qualified_path::QualifiedPath::from_host_name(
        &flotilla_protocol::HostName::new("kiwi"),
        "/Users/robert/dev/project",
    );
    let set_id = flotilla_protocol::AttachableSetId::new("set-remote");

    let mut remote_checkout_data = TestCheckout::new("feat-set").at("/remote/feat-set").is_main(false).with_branch_key().build();
    remote_checkout_data.correlation_keys =
        vec![CorrelationKey::Branch("feat-set".to_string()), CorrelationKey::CheckoutPath(remote_checkout.clone())];
    providers.checkouts.insert(remote_checkout.clone(), remote_checkout_data);

    let mut local_checkout_data = TestCheckout::new("feat-set").at("/Users/robert/dev/project").is_main(false).with_branch_key().build();
    local_checkout_data.correlation_keys =
        vec![CorrelationKey::Branch("feat-set".to_string()), CorrelationKey::CheckoutPath(local_checkout.clone())];
    providers.checkouts.insert(local_checkout.clone(), local_checkout_data);
    providers.attachable_sets.insert(set_id.clone(), flotilla_protocol::AttachableSet {
        id: set_id.clone(),
        host_affinity: Some(flotilla_protocol::HostName::new("feta")),
        checkout: Some(
            flotilla_protocol::HostPath::try_from(&remote_checkout).expect("hostname-qualified path should convert to HostPath").into(),
        ),
        template_identity: None,
        environment_id: None,
        members: vec![],
    });
    providers.workspaces.insert("ws-1".to_string(), Workspace {
        name: "feat-set@feta".to_string(),
        correlation_keys: vec![CorrelationKey::CheckoutPath(local_checkout.clone())],
        attachable_set_id: Some(set_id.clone()),
    });

    let (items, _) = correlate(&providers);

    assert_eq!(items.len(), 2);
    let remote_checkout_item = items
        .iter()
        .find(|item| item.kind() == WorkItemKind::Checkout && item.checkout_key() == Some(&remote_checkout))
        .expect("remote checkout item");
    assert_eq!(remote_checkout_item.attachable_set_id(), Some(&set_id));
    assert_eq!(remote_checkout_item.workspace_refs(), &["ws-1".to_string()]);

    let local_checkout_item = items
        .iter()
        .find(|item| item.kind() == WorkItemKind::Checkout && item.checkout_key() == Some(&local_checkout))
        .expect("local checkout item");
    assert!(local_checkout_item.workspace_refs().is_empty(), "workspace should not correlate directly to local checkout");
    assert_eq!(local_checkout_item.attachable_set_id(), None);
}

#[test]
fn correlate_checkout_remains_anchor_when_attachable_set_present() {
    let mut providers = new_providers();

    let co_path = flotilla_protocol::qualified_path::QualifiedPath::from_host_name(&flotilla_protocol::HostName::local(), "/tmp/feat-set");
    let set_id = flotilla_protocol::AttachableSetId::new("set-1");

    providers.checkouts.insert(co_path.clone(), TestCheckout::new("feat-set").at("/tmp/feat-set").is_main(false).with_branch_key().build());
    providers.attachable_sets.insert(set_id.clone(), make_attachable_set("set-1", "/tmp/feat-set"));
    providers.workspaces.insert("ws-1".to_string(), Workspace {
        name: "feat-set".to_string(),
        correlation_keys: vec![],
        attachable_set_id: Some(set_id.clone()),
    });
    let (items, _) = correlate(&providers);

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind(), WorkItemKind::Checkout);
    assert_eq!(items[0].attachable_set_id(), Some(&set_id));
    assert_eq!(items[0].checkout_key(), Some(&co_path));
    assert_eq!(items[0].workspace_refs(), &["ws-1".to_string()]);
}

// -----------------------------------------------------------------------
// group_work_items_split() test helpers
// -----------------------------------------------------------------------

fn test_attachable_set_work_item(id: &str) -> flotilla_protocol::WorkItem {
    flotilla_protocol::WorkItem {
        kind: WorkItemKind::AttachableSet,
        identity: WorkItemIdentity::AttachableSet(flotilla_protocol::AttachableSetId::new(id)),
        host: flotilla_protocol::HostName::local(),
        branch: None,
        description: format!("attachable set {id}"),
        checkout: None,
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
        source: None,
        terminal_keys: Vec::new(),
        attachable_set_id: Some(flotilla_protocol::AttachableSetId::new(id)),
        agent_keys: Vec::new(),
    }
}

fn test_agent_work_item(id: &str) -> flotilla_protocol::WorkItem {
    flotilla_protocol::WorkItem {
        kind: WorkItemKind::Agent,
        identity: WorkItemIdentity::Agent(id.into()),
        host: flotilla_protocol::HostName::local(),
        branch: None,
        description: format!("agent {id}"),
        checkout: None,
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
        source: None,
        terminal_keys: Vec::new(),
        attachable_set_id: None,
        agent_keys: vec![id.into()],
    }
}

// -----------------------------------------------------------------------
// group_work_items_split() tests
// -----------------------------------------------------------------------

#[test]
fn group_work_items_split_empty_input() {
    let providers = new_providers();
    let labels = default_labels();
    let sections = group_work_items_split(&[], &providers, &labels, Path::new("/tmp"));
    assert!(sections.is_empty());
}

#[test]
fn group_work_items_split_produces_correct_sections() {
    let providers = new_providers();
    let labels = default_labels();
    let items = vec![
        to_proto(&checkout_item("/tmp/wt", Some("feat"), false)),
        to_proto(&cr_item("5", "PR five")),
        to_proto(&issue_item("10", "Bug ten")),
        to_proto(&session_item("s1", "Session one")),
        to_proto(&remote_branch_item("origin/dev")),
    ];
    let sections = group_work_items_split(&items, &providers, &labels, Path::new("/tmp"));

    // All 5 kinds present → 5 sections
    assert_eq!(sections.len(), 5);

    // Verify each section contains only its own kind
    for section in &sections {
        for item in &section.items {
            match section.kind {
                SectionKind::Checkouts => assert_eq!(item.kind, WorkItemKind::Checkout),
                SectionKind::AttachableSets => assert_eq!(item.kind, WorkItemKind::AttachableSet),
                SectionKind::CloudAgents => {
                    assert!(item.kind == WorkItemKind::Session || item.kind == WorkItemKind::Agent)
                }
                SectionKind::ChangeRequests => assert_eq!(item.kind, WorkItemKind::ChangeRequest),
                SectionKind::RemoteBranches => assert_eq!(item.kind, WorkItemKind::RemoteBranch),
                SectionKind::Issues => assert_eq!(item.kind, WorkItemKind::Issue),
            }
        }
    }

    // Verify display order: Checkouts, CloudAgents, ChangeRequests, RemoteBranches, Issues
    // (no AttachableSets in this input)
    let kinds: Vec<SectionKind> = sections.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, vec![
        SectionKind::Checkouts,
        SectionKind::CloudAgents,
        SectionKind::ChangeRequests,
        SectionKind::RemoteBranches,
        SectionKind::Issues,
    ]);
}

#[test]
fn group_work_items_split_empty_sections_omitted() {
    let providers = new_providers();
    let labels = default_labels();
    // Only issues — all other sections should be absent
    let items = vec![to_proto(&issue_item("1", "Bug"))];
    let sections = group_work_items_split(&items, &providers, &labels, Path::new("/tmp"));

    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].kind, SectionKind::Issues);
    assert_eq!(sections[0].items.len(), 1);
}

#[test]
fn group_work_items_split_agents_in_cloud_agents_section() {
    let providers = new_providers();
    let labels = default_labels();
    let items = vec![test_agent_work_item("a1"), test_agent_work_item("a2")];
    let sections = group_work_items_split(&items, &providers, &labels, Path::new("/tmp"));

    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].kind, SectionKind::CloudAgents);
    assert_eq!(sections[0].items.len(), 2);
    for item in &sections[0].items {
        assert_eq!(item.kind, WorkItemKind::Agent);
    }
}

#[test]
fn group_work_items_split_sessions_and_agents_share_cloud_agents_section() {
    let providers = new_providers();
    let labels = default_labels();
    let items = vec![test_session_work_item("s1"), test_agent_work_item("a1")];
    let sections = group_work_items_split(&items, &providers, &labels, Path::new("/tmp"));

    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].kind, SectionKind::CloudAgents);
    assert_eq!(sections[0].items.len(), 2);
}

#[test]
fn group_work_items_split_section_order_with_all_kinds() {
    let providers = new_providers();
    let labels = default_labels();
    let items = vec![
        to_proto(&issue_item("1", "Bug")),
        to_proto(&remote_branch_item("origin/x")),
        to_proto(&cr_item("2", "PR")),
        test_session_work_item("s1"),
        test_attachable_set_work_item("set-1"),
        to_proto(&checkout_item("/tmp/wt", Some("feat"), false)),
    ];
    let sections = group_work_items_split(&items, &providers, &labels, Path::new("/tmp"));

    assert_eq!(sections.len(), 6);
    let kinds: Vec<SectionKind> = sections.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, vec![
        SectionKind::Checkouts,
        SectionKind::AttachableSets,
        SectionKind::CloudAgents,
        SectionKind::ChangeRequests,
        SectionKind::RemoteBranches,
        SectionKind::Issues,
    ]);
}

// -----------------------------------------------------------------------
// filter_archived_sections() tests
// -----------------------------------------------------------------------

#[test]
fn filter_archived_sessions_from_section_data() {
    use flotilla_protocol::SessionStatus;

    let mut providers = new_providers();
    providers.sessions.insert("s-active".into(), test_cloud_agent_session(SessionStatus::Running));
    providers.sessions.insert("s-archived".into(), test_cloud_agent_session(SessionStatus::Archived));

    let items = vec![test_session_work_item("s-active"), test_session_work_item("s-archived"), to_proto(&issue_item("1", "Bug"))];
    let labels = default_labels();
    let sections = group_work_items_split(&items, &providers, &labels, Path::new("/tmp"));

    let filtered = filter_archived_sections(sections, &providers);

    // Issues section should be untouched; CloudAgents should have only the active session
    let cloud_section = filtered.iter().find(|s| s.kind == SectionKind::CloudAgents).expect("CloudAgents section should exist");
    assert_eq!(cloud_section.items.len(), 1);
    assert_eq!(cloud_section.items[0].session_key.as_deref(), Some("s-active"));

    let issue_section = filtered.iter().find(|s| s.kind == SectionKind::Issues).expect("Issues section should exist");
    assert_eq!(issue_section.items.len(), 1);
}

#[test]
fn filter_archived_removes_empty_section() {
    use flotilla_protocol::SessionStatus;

    let mut providers = new_providers();
    providers.sessions.insert("s1".into(), test_cloud_agent_session(SessionStatus::Archived));
    providers.sessions.insert("s2".into(), test_cloud_agent_session(SessionStatus::Expired));

    let items = vec![test_session_work_item("s1"), test_session_work_item("s2")];
    let labels = default_labels();
    let sections = group_work_items_split(&items, &providers, &labels, Path::new("/tmp"));
    assert_eq!(sections.len(), 1, "should have one section before filtering");

    let filtered = filter_archived_sections(sections, &providers);
    assert!(filtered.is_empty(), "section should be dropped when all sessions are archived");
}

#[test]
fn filter_archived_keeps_agents() {
    let providers = new_providers();
    // No session entries → agent items have no session_key and are never filtered

    let items = vec![test_agent_work_item("a1"), test_agent_work_item("a2")];
    let labels = default_labels();
    let sections = group_work_items_split(&items, &providers, &labels, Path::new("/tmp"));

    let filtered = filter_archived_sections(sections, &providers);

    assert_eq!(filtered.len(), 1);
    let cloud_section = &filtered[0];
    assert_eq!(cloud_section.kind, SectionKind::CloudAgents);
    assert_eq!(cloud_section.items.len(), 2, "agent items should never be filtered");
}
