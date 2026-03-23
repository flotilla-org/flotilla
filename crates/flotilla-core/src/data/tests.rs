use std::path::PathBuf;

use flotilla_protocol::test_support::{hp, TestChangeRequest, TestCheckout, TestIssue, TestSession};

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
        ..correlated(CorrelatedAnchor::Checkout(CheckoutRef { key: hp(path), is_main_checkout: is_main }))
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

fn make_workspace(_ws_ref: &str, name: &str, directories: Vec<PathBuf>, correlation_keys: Vec<CorrelationKey>) -> Workspace {
    Workspace { name: name.to_string(), directories, correlation_keys, attachable_set_id: None }
}

fn make_attachable_set(id: &str, path: &str) -> flotilla_protocol::AttachableSet {
    flotilla_protocol::AttachableSet {
        id: flotilla_protocol::AttachableSetId::new(id),
        host_affinity: Some(flotilla_protocol::HostName::new("test-host")),
        checkout: Some(flotilla_protocol::HostPath::new(flotilla_protocol::HostName::new("test-host"), PathBuf::from(path))),
        template_identity: None,
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

fn header_titles(entries: &[GroupEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|e| match e {
            GroupEntry::Header(h) => Some(h.0.clone()),
            GroupEntry::Item(_) => None,
        })
        .collect()
}

fn item_branches(entries: &[GroupEntry]) -> Vec<Option<String>> {
    entries
        .iter()
        .filter_map(|e| match e {
            GroupEntry::Header(_) => None,
            GroupEntry::Item(item) => Some(item.branch.clone()),
        })
        .collect()
}

fn item_change_request_keys(entries: &[GroupEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|e| match e {
            GroupEntry::Header(_) => None,
            GroupEntry::Item(item) => item.change_request_key.clone(),
        })
        .collect()
}

fn issue_key_groups(entries: &[GroupEntry]) -> Vec<Vec<String>> {
    entries
        .iter()
        .filter_map(|e| match e {
            GroupEntry::Header(_) => None,
            GroupEntry::Item(item) => {
                if item.kind == WorkItemKind::Issue {
                    Some(item.issue_keys.clone())
                } else {
                    None
                }
            }
        })
        .collect()
}

fn session_descriptions(entries: &[GroupEntry]) -> Vec<&str> {
    entries
        .iter()
        .filter_map(|e| match e {
            GroupEntry::Header(_) => None,
            GroupEntry::Item(item) => {
                if item.kind == WorkItemKind::Session {
                    Some(item.description.as_str())
                } else {
                    None
                }
            }
        })
        .collect()
}

// -----------------------------------------------------------------------
// Display / formatting tests
// -----------------------------------------------------------------------

#[test]
fn refresh_error_display() {
    let err = RefreshError { category: "github", provider: "GitHub".to_string(), message: "rate limited".to_string() };
    assert_eq!(format!("{err}"), "github/GitHub: rate limited");
}

#[test]
fn section_header_display() {
    let hdr = SectionHeader("Checkouts".to_string());
    assert_eq!(format!("{hdr}"), "Checkouts");
}

// -----------------------------------------------------------------------
// WorkItemKind classification tests
// -----------------------------------------------------------------------

#[test]
fn kind_checkout() {
    let wi = checkout_item("/tmp/foo", None, false);
    assert_eq!(wi.kind(), WorkItemKind::Checkout);
}

#[test]
fn kind_change_request() {
    let wi = cr_item("42", "PR title");
    assert_eq!(wi.kind(), WorkItemKind::ChangeRequest);
}

#[test]
fn kind_session() {
    let wi = session_item("sess-1", "Session title");
    assert_eq!(wi.kind(), WorkItemKind::Session);
}

#[test]
fn kind_issue() {
    let wi = issue_item("7", "Fix bug");
    assert_eq!(wi.kind(), WorkItemKind::Issue);
}

#[test]
fn kind_remote_branch() {
    let wi = remote_branch_item("feature/x");
    assert_eq!(wi.kind(), WorkItemKind::RemoteBranch);
}

// -----------------------------------------------------------------------
// Accessor tests: branch()
// -----------------------------------------------------------------------

#[test]
fn branch_from_checkout_with_branch() {
    let wi = checkout_item("/tmp/wt", Some("feat-x"), false);
    assert_eq!(wi.branch(), Some("feat-x"));
}

#[test]
fn branch_from_checkout_without_branch() {
    let wi = checkout_item("/tmp/wt", None, false);
    assert_eq!(wi.branch(), None);
}

#[test]
fn branch_from_remote_branch() {
    let wi = remote_branch_item("origin/develop");
    assert_eq!(wi.branch(), Some("origin/develop"));
}

#[test]
fn branch_from_issue_is_none() {
    let wi = issue_item("1", "desc");
    assert_eq!(wi.branch(), None);
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
fn description_from_correlated() {
    let wi = cr_item("1", "Fix login flow");
    assert_eq!(wi.description(), "Fix login flow");
}

#[test]
fn description_from_standalone_issue() {
    let wi = issue_item("5", "Add caching");
    assert_eq!(wi.description(), "Add caching");
}

#[test]
fn description_from_remote_branch_is_branch_name() {
    let wi = remote_branch_item("feature/auth");
    assert_eq!(wi.description(), "feature/auth");
}

// -----------------------------------------------------------------------
// Accessor tests: checkout(), checkout_key(), is_main_checkout()
// -----------------------------------------------------------------------

#[test]
fn checkout_returns_some_for_checkout_anchor() {
    let wi = checkout_item("/tmp/wt", Some("main"), true);
    let co = wi.checkout().expect("should return checkout");
    assert_eq!(co.key, hp("/tmp/wt"));
    assert!(co.is_main_checkout);
}

#[test]
fn checkout_returns_none_for_non_checkout() {
    assert!(cr_item("1", "d").checkout().is_none());
    assert!(session_item("s", "d").checkout().is_none());
    assert!(issue_item("i", "d").checkout().is_none());
    assert!(remote_branch_item("b").checkout().is_none());
}

#[test]
fn checkout_key_returns_path() {
    let wi = checkout_item("/repos/proj", None, false);
    assert_eq!(wi.checkout_key(), Some(&hp("/repos/proj")));
}

#[test]
fn checkout_key_none_for_standalone() {
    assert!(issue_item("1", "d").checkout_key().is_none());
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

#[test]
fn is_main_checkout_false_for_non_checkout() {
    assert!(!cr_item("1", "d").is_main_checkout());
}

// -----------------------------------------------------------------------
// Accessor tests: change_request_key()
// -----------------------------------------------------------------------

#[test]
fn change_request_key_from_cr_anchor() {
    let wi = cr_item("42", "PR");
    assert_eq!(wi.change_request_key(), Some("42"));
}

#[test]
fn change_request_key_from_linked_on_checkout() {
    let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
        linked_change_request: Some("99".to_string()),
        ..correlated(CorrelatedAnchor::Checkout(CheckoutRef { key: hp("/tmp/wt"), is_main_checkout: false }))
    });
    assert_eq!(wi.change_request_key(), Some("99"));
}

#[test]
fn change_request_key_none_for_standalone() {
    assert!(issue_item("1", "d").change_request_key().is_none());
    assert!(remote_branch_item("b").change_request_key().is_none());
}

// -----------------------------------------------------------------------
// Accessor tests: session_key()
// -----------------------------------------------------------------------

#[test]
fn session_key_from_session_anchor() {
    let wi = session_item("sess-x", "title");
    assert_eq!(wi.session_key(), Some("sess-x"));
}

#[test]
fn session_key_from_linked_on_checkout() {
    let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
        linked_session: Some("linked-sess".to_string()),
        ..correlated(CorrelatedAnchor::Checkout(CheckoutRef { key: hp("/tmp/wt"), is_main_checkout: false }))
    });
    assert_eq!(wi.session_key(), Some("linked-sess"));
}

#[test]
fn session_key_none_for_standalone() {
    assert!(issue_item("1", "d").session_key().is_none());
}

// -----------------------------------------------------------------------
// Accessor tests: issue_keys()
// -----------------------------------------------------------------------

#[test]
fn issue_keys_from_correlated_with_linked_issues() {
    let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
        linked_issues: vec!["10".to_string(), "20".to_string()],
        ..correlated(CorrelatedAnchor::Checkout(CheckoutRef { key: hp("/tmp/wt"), is_main_checkout: false }))
    });
    assert_eq!(wi.issue_keys(), &["10".to_string(), "20".to_string()]);
}

#[test]
fn issue_keys_from_standalone_issue_returns_single() {
    let wi = issue_item("42", "desc");
    assert_eq!(wi.issue_keys(), &["42".to_string()]);
}

#[test]
fn issue_keys_empty_for_remote_branch() {
    let wi = remote_branch_item("b");
    assert!(wi.issue_keys().is_empty());
}

// -----------------------------------------------------------------------
// Accessor tests: workspace_refs()
// -----------------------------------------------------------------------

#[test]
fn workspace_refs_from_correlated() {
    let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
        workspace_refs: vec!["ws-1".to_string()],
        ..correlated(CorrelatedAnchor::Checkout(CheckoutRef { key: hp("/tmp/wt"), is_main_checkout: false }))
    });
    assert_eq!(wi.workspace_refs(), &["ws-1".to_string()]);
}

#[test]
fn workspace_refs_empty_for_standalone() {
    assert!(issue_item("1", "d").workspace_refs().is_empty());
    assert!(remote_branch_item("b").workspace_refs().is_empty());
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
    assert!(issue_item("1", "d").correlation_group_idx().is_none());
    assert!(remote_branch_item("b").correlation_group_idx().is_none());
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
// Identity tests (existing)
// -----------------------------------------------------------------------

#[test]
fn identity_checkout() {
    let wi = checkout_item("/tmp/foo", None, false);
    assert_eq!(wi.identity(), WorkItemIdentity::Checkout(hp("/tmp/foo")));
}

#[test]
fn identity_pr() {
    let wi = cr_item("42", "PR");
    assert_eq!(wi.identity(), WorkItemIdentity::ChangeRequest("42".to_string()));
}

#[test]
fn identity_session() {
    let wi = session_item("sess-1", "title");
    assert_eq!(wi.identity(), WorkItemIdentity::Session("sess-1".to_string()));
}

#[test]
fn identity_issue() {
    let wi = issue_item("7", "desc");
    assert_eq!(wi.identity(), WorkItemIdentity::Issue("7".to_string()));
}

#[test]
fn identity_remote_branch() {
    let wi = remote_branch_item("feature/x");
    assert_eq!(wi.identity(), WorkItemIdentity::RemoteBranch("feature/x".to_string()));
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
    providers.checkouts.insert(hp("/tmp/feat"), TestCheckout::new("feat").at("/tmp/feat").is_main(false).with_branch_key().build());

    let (items, groups) = correlate(&providers);
    assert_eq!(items.len(), 1);
    assert_eq!(groups.len(), 1);
    assert_eq!(items[0].kind(), WorkItemKind::Checkout);
    assert_eq!(items[0].branch(), Some("feat"));
}

#[test]
fn correlate_trunk_checkout_marked_as_main() {
    let mut providers = new_providers();
    providers.checkouts.insert(hp("/tmp/main"), TestCheckout::new("main").at("/tmp/main").is_main(true).with_branch_key().build());

    let (items, _) = correlate(&providers);
    assert_eq!(items.len(), 1);
    assert!(items[0].is_main_checkout());
}

#[test]
fn correlate_checkout_and_pr_merge_on_branch() {
    let mut providers = new_providers();
    providers.checkouts.insert(hp("/tmp/feat-x"), TestCheckout::new("feat-x").at("/tmp/feat-x").is_main(false).with_branch_key().build());
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
        .insert(hp("/repo/feat"), TestCheckout::new("feat-branch").at("/repo/feat").is_main(false).with_branch_key().build());

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
    providers.checkouts.insert(hp("/tmp/feat-y"), TestCheckout::new("feat-y").at("/tmp/feat-y").is_main(false).with_branch_key().build());
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
    providers.checkouts.insert(hp("/tmp/feat-z"), TestCheckout::new("feat-z").at("/tmp/feat-z").is_main(false).with_branch_key().build());
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
    providers.checkouts.insert(hp("/tmp/feat"), TestCheckout::new("feat").at("/tmp/feat").is_main(false).with_branch_key().build());
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

    let co_path = hp("/tmp/feat-x");
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
    providers.workspaces.insert("ws-orphan".to_string(), make_workspace("ws-orphan", "orphan", vec![], vec![]));

    let (items, _) = correlate(&providers);
    assert!(items.is_empty(), "workspace-only group should be skipped");
}

#[test]
fn correlate_workspace_without_attachable_set_is_not_linked_to_checkout() {
    let mut providers = new_providers();
    let co_path = hp("/tmp/feat-ws");
    providers.checkouts.insert(co_path.clone(), TestCheckout::new("feat-ws").at("/tmp/feat-ws").is_main(false).with_branch_key().build());
    providers.workspaces.insert(
        "ws-1".to_string(),
        make_workspace("ws-1", "dev-session", vec![co_path.path.clone()], vec![CorrelationKey::CheckoutPath(co_path)]),
    );

    let (items, _) = correlate(&providers);
    assert_eq!(items.len(), 1);
    assert!(items[0].workspace_refs().is_empty());
}

#[test]
fn correlate_description_prefers_pr_title() {
    let mut providers = new_providers();
    providers.checkouts.insert(hp("/tmp/feat"), TestCheckout::new("feat").at("/tmp/feat").is_main(false).with_branch_key().build());
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
    providers.checkouts.insert(hp("/tmp/feat"), TestCheckout::new("feat").at("/tmp/feat").is_main(false).with_branch_key().build());
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
        .insert(hp("/tmp/my-branch"), TestCheckout::new("my-branch").at("/tmp/my-branch").is_main(false).with_branch_key().build());

    let (items, _) = correlate(&providers);
    assert_eq!(items[0].description(), "my-branch");
}

#[test]
fn correlate_multiple_items_sharing_branch_merge() {
    let mut providers = new_providers();
    providers
        .checkouts
        .insert(hp("/tmp/shared"), TestCheckout::new("shared-branch").at("/tmp/shared").is_main(false).with_branch_key().build());
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
    providers.checkouts.insert(hp("/tmp/a"), TestCheckout::new("branch-a").at("/tmp/a").is_main(false).with_branch_key().build());
    providers.checkouts.insert(hp("/tmp/b"), TestCheckout::new("branch-b").at("/tmp/b").is_main(false).with_branch_key().build());

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
// group_work_items() tests
// -----------------------------------------------------------------------

#[test]
fn group_work_items_empty_input() {
    let providers = new_providers();
    let labels = default_labels();
    let result = group_work_items(&[], &providers, &labels, Path::new("/tmp"));
    assert!(result.table_entries.is_empty());
    assert!(result.selectable_indices.is_empty());
}

#[test]
fn group_work_items_single_checkout() {
    let providers = new_providers();
    let labels = default_labels();
    let items = vec![to_proto(&checkout_item("/tmp/wt", Some("feat"), false))];
    let result = group_work_items(&items, &providers, &labels, Path::new("/tmp"));

    // Should have 1 header + 1 item
    assert_eq!(result.table_entries.len(), 2);
    assert!(matches!(result.table_entries[0], GroupEntry::Header(_)));
    assert!(matches!(result.table_entries[1], GroupEntry::Item(_)));
    assert_eq!(result.selectable_indices, vec![1]);
}

#[test]
fn group_work_items_sections_appear_in_order() {
    // checkouts, sessions, PRs, remote branches, issues
    let providers = new_providers();
    let labels = default_labels();
    let items = vec![
        to_proto(&checkout_item("/tmp/wt", Some("feat"), false)),
        to_proto(&session_item("s1", "Session")),
        to_proto(&cr_item("10", "PR")),
        to_proto(&remote_branch_item("origin/dev")),
        to_proto(&issue_item("1", "Bug")),
    ];
    let result = group_work_items(&items, &providers, &labels, Path::new("/tmp"));

    // Expect 5 headers + 5 items = 10 entries
    assert_eq!(result.table_entries.len(), 10);

    let headers = header_titles(&result.table_entries);
    assert_eq!(headers, vec!["Checkouts", "Sessions", "Change Requests", "Remote Branches", "Issues",]);
}

#[test]
fn group_work_items_checkouts_sorted_by_path_main_first() {
    let providers = new_providers();
    let labels = default_labels();
    let items = vec![
        to_proto(&checkout_item("/tmp/z", Some("z-branch"), false)),
        to_proto(&checkout_item("/tmp/a", Some("a-branch"), false)),
        to_proto(&checkout_item("/tmp/main", Some("main"), true)),
        to_proto(&checkout_item("/tmp/m", Some("m-branch"), false)),
    ];
    let result = group_work_items(&items, &providers, &labels, Path::new("/tmp"));

    let branches = item_branches(&result.table_entries);
    assert_eq!(branches, vec![
        Some("main".to_string()),     // main always first
        Some("a-branch".to_string()), // then by path ascending
        Some("m-branch".to_string()),
        Some("z-branch".to_string()),
    ]);
}

#[test]
fn group_work_items_codex_worktree_sorts_after_siblings() {
    // Real scenario: main at ~/dev/flotilla, sibling worktrees at
    // ~/dev/flotilla.checkout-order etc., and a Codex auto-worktree at
    // ~/.codex/worktrees/0cf6/flotilla.  The Codex path currently sorts
    // between main and siblings because raw "/Users/x/.codex" < "/Users/x/dev".
    let providers = new_providers();
    let labels = default_labels();
    let items = vec![
        to_proto(&checkout_item("/Users/robert/dev/flotilla", Some("main"), true)),
        to_proto(&checkout_item("/Users/robert/.codex/worktrees/0cf6/flotilla", Some("codex-detached"), false)),
        to_proto(&checkout_item("/Users/robert/dev/flotilla.checkout-order", Some("checkout-order"), false)),
        to_proto(&checkout_item("/Users/robert/dev/flotilla.low-hang-13", Some("low-hang-13"), false)),
    ];
    let repo_root = Path::new("/Users/robert/dev/flotilla");
    let result = group_work_items(&items, &providers, &labels, repo_root);

    let branches = item_branches(&result.table_entries);
    assert_eq!(branches, vec![
        Some("main".to_string()),           // main always first
        Some("checkout-order".to_string()), // siblings next
        Some("low-hang-13".to_string()),
        Some("codex-detached".to_string()), // external worktrees last
    ]);
}

#[test]
fn group_work_items_prs_sorted_by_id_descending() {
    let providers = new_providers();
    let labels = default_labels();
    let pr1 = to_proto(&cr_item("1", "PR one"));
    let pr5 = to_proto(&cr_item("5", "PR five"));
    let pr3 = to_proto(&cr_item("3", "PR three"));

    let items = vec![pr1, pr5, pr3];
    let result = group_work_items(&items, &providers, &labels, Path::new("/tmp"));

    let cr_keys = item_change_request_keys(&result.table_entries);
    assert_eq!(cr_keys, vec!["5", "3", "1"]);
}

#[test]
fn group_work_items_issues_sorted_by_id_descending() {
    let providers = new_providers();
    let labels = default_labels();
    let items =
        vec![to_proto(&issue_item("3", "Issue three")), to_proto(&issue_item("10", "Issue ten")), to_proto(&issue_item("1", "Issue one"))];
    let result = group_work_items(&items, &providers, &labels, Path::new("/tmp"));

    let issue_keys = issue_key_groups(&result.table_entries);
    assert_eq!(issue_keys, vec![vec!["10".to_string()], vec!["3".to_string()], vec!["1".to_string()]]);
}

#[test]
fn group_work_items_remote_branches_sorted_by_name() {
    let providers = new_providers();
    let labels = default_labels();
    let items = vec![to_proto(&remote_branch_item("z-remote")), to_proto(&remote_branch_item("a-remote"))];
    let result = group_work_items(&items, &providers, &labels, Path::new("/tmp"));

    let branches = item_branches(&result.table_entries);
    assert_eq!(branches, vec![Some("a-remote".to_string()), Some("z-remote".to_string()),]);
}

#[test]
fn group_work_items_selectable_indices_skip_headers() {
    let providers = new_providers();
    let labels = default_labels();
    let items = vec![
        to_proto(&checkout_item("/tmp/a", Some("a"), false)),
        to_proto(&checkout_item("/tmp/b", Some("b"), false)),
        to_proto(&issue_item("1", "Bug")),
    ];
    let result = group_work_items(&items, &providers, &labels, Path::new("/tmp"));

    // Layout: Header(0), Item(1), Item(2), Header(3), Item(4)
    assert_eq!(result.selectable_indices, vec![1, 2, 4]);
}

#[test]
fn group_work_items_empty_sections_omitted() {
    let providers = new_providers();
    let labels = default_labels();
    // Only issues, no checkouts/sessions/PRs/remote
    let items = vec![to_proto(&issue_item("1", "Bug"))];
    let result = group_work_items(&items, &providers, &labels, Path::new("/tmp"));

    assert_eq!(result.table_entries.len(), 2); // 1 header + 1 item
    let headers = header_titles(&result.table_entries);
    assert_eq!(headers, vec!["Issues"]);
}

#[test]
fn group_work_items_uses_custom_labels() {
    let providers = new_providers();
    let labels = SectionLabels {
        checkouts: "Checkouts".into(),
        change_requests: "Pull Requests".into(),
        issues: "Tickets".into(),
        sessions: "Agents".into(),
    };
    let items = vec![
        to_proto(&checkout_item("/tmp/wt", Some("feat"), false)),
        to_proto(&session_item("s1", "Agent")),
        to_proto(&cr_item("1", "PR")),
        to_proto(&issue_item("1", "Ticket")),
    ];
    let result = group_work_items(&items, &providers, &labels, Path::new("/tmp"));

    let headers = header_titles(&result.table_entries);
    assert_eq!(headers, vec!["Checkouts", "Agents", "Pull Requests", "Tickets"]);
}

#[test]
fn group_work_items_sessions_sorted_by_updated_at_descending() {
    let mut providers = new_providers();
    // Populate providers with sessions that have updated_at
    providers.sessions.insert("s-old".to_string(), CloudAgentSession {
        title: "Old".to_string(),
        status: SessionStatus::Idle,
        model: None,
        updated_at: Some("2026-01-01T00:00:00Z".to_string()),
        correlation_keys: vec![],
        provider_name: String::new(),
        provider_display_name: String::new(),
        item_noun: String::new(),
    });
    providers.sessions.insert("s-new".to_string(), CloudAgentSession {
        title: "New".to_string(),
        status: SessionStatus::Running,
        model: None,
        updated_at: Some("2026-03-01T00:00:00Z".to_string()),
        correlation_keys: vec![],
        provider_name: String::new(),
        provider_display_name: String::new(),
        item_noun: String::new(),
    });
    providers.sessions.insert("s-mid".to_string(), CloudAgentSession {
        title: "Mid".to_string(),
        status: SessionStatus::Running,
        model: None,
        updated_at: Some("2026-02-01T00:00:00Z".to_string()),
        correlation_keys: vec![],
        provider_name: String::new(),
        provider_display_name: String::new(),
        item_noun: String::new(),
    });

    let labels = default_labels();
    let si1 = to_proto(&session_item("s-old", "Old"));
    let si2 = to_proto(&session_item("s-new", "New"));
    let si3 = to_proto(&session_item("s-mid", "Mid"));

    let items = vec![si1, si2, si3];
    let result = group_work_items(&items, &providers, &labels, Path::new("/tmp"));

    let session_descs = session_descriptions(&result.table_entries);
    assert_eq!(session_descs, vec!["New", "Mid", "Old"]);
}

#[test]
fn group_work_items_sessions_grouped_by_provider_then_time() {
    let mut providers = new_providers();
    providers.sessions.insert("s-claude-old".to_string(), CloudAgentSession {
        title: "Claude Old".to_string(),
        status: SessionStatus::Idle,
        model: None,
        updated_at: Some("2026-01-01T00:00:00Z".to_string()),
        correlation_keys: vec![],
        provider_name: "claude".to_string(),
        provider_display_name: "Claude".to_string(),
        item_noun: "Agent".to_string(),
    });
    providers.sessions.insert("s-codex-new".to_string(), CloudAgentSession {
        title: "Codex New".to_string(),
        status: SessionStatus::Running,
        model: None,
        updated_at: Some("2026-03-01T00:00:00Z".to_string()),
        correlation_keys: vec![],
        provider_name: "codex".to_string(),
        provider_display_name: "Codex".to_string(),
        item_noun: "Task".to_string(),
    });
    providers.sessions.insert("s-claude-new".to_string(), CloudAgentSession {
        title: "Claude New".to_string(),
        status: SessionStatus::Running,
        model: None,
        updated_at: Some("2026-02-01T00:00:00Z".to_string()),
        correlation_keys: vec![],
        provider_name: "claude".to_string(),
        provider_display_name: "Claude".to_string(),
        item_noun: "Agent".to_string(),
    });

    let labels = default_labels();
    let items = vec![
        to_proto(&session_item("s-claude-old", "Claude Old")),
        to_proto(&session_item("s-codex-new", "Codex New")),
        to_proto(&session_item("s-claude-new", "Claude New")),
    ];
    let result = group_work_items(&items, &providers, &labels, Path::new("/tmp"));

    let session_descs = session_descriptions(&result.table_entries);
    // claude sessions grouped first (alphabetically), newest first within group
    // then codex sessions
    assert_eq!(session_descs, vec!["Claude New", "Claude Old", "Codex New"]);
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

// -----------------------------------------------------------------------
// GroupedWorkItems default test
// -----------------------------------------------------------------------

#[test]
fn grouped_work_items_default_is_empty() {
    let g = GroupedWorkItems::default();
    assert!(g.table_entries.is_empty());
    assert!(g.selectable_indices.is_empty());
}

// -----------------------------------------------------------------------
// GroupedWorkItems::filter_archived_sessions tests
// -----------------------------------------------------------------------

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

#[test]
fn filter_archived_sessions_removes_archived_and_expired() {
    use flotilla_protocol::SessionStatus;

    let active = test_session_work_item("s1");
    let archived = test_session_work_item("s2");

    let checkout = flotilla_protocol::WorkItem {
        kind: WorkItemKind::Checkout,
        identity: WorkItemIdentity::Checkout(flotilla_protocol::HostPath::new(
            flotilla_protocol::HostName::local(),
            std::path::PathBuf::from("/tmp/co"),
        )),
        host: flotilla_protocol::HostName::local(),
        branch: Some("main".into()),
        description: "checkout".into(),
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
        agent_keys: Vec::new(),
    };

    let mut grouped = GroupedWorkItems::default();
    grouped.table_entries.push(GroupEntry::Header(SectionHeader("Sessions".into())));
    grouped.selectable_indices.push(1);
    grouped.table_entries.push(GroupEntry::Item(Box::new(active)));
    grouped.selectable_indices.push(2);
    grouped.table_entries.push(GroupEntry::Item(Box::new(archived)));
    grouped.table_entries.push(GroupEntry::Header(SectionHeader("Checkouts".into())));
    grouped.selectable_indices.push(4);
    grouped.table_entries.push(GroupEntry::Item(Box::new(checkout)));

    let mut providers = ProviderData::default();
    providers.sessions.insert("s1".into(), test_cloud_agent_session(SessionStatus::Running));
    providers.sessions.insert("s2".into(), test_cloud_agent_session(SessionStatus::Archived));

    let filtered = grouped.filter_archived_sessions(&providers);

    assert_eq!(filtered.selectable_indices.len(), 2);
    let header_count = filtered.table_entries.iter().filter(|e| matches!(e, GroupEntry::Header(_))).count();
    assert_eq!(header_count, 2);
}

#[test]
fn filter_archived_sessions_removes_orphaned_headers() {
    use flotilla_protocol::SessionStatus;

    let archived = test_session_work_item("s1");

    let mut grouped = GroupedWorkItems::default();
    grouped.table_entries.push(GroupEntry::Header(SectionHeader("Sessions".into())));
    grouped.selectable_indices.push(1);
    grouped.table_entries.push(GroupEntry::Item(Box::new(archived)));

    let mut providers = ProviderData::default();
    providers.sessions.insert("s1".into(), test_cloud_agent_session(SessionStatus::Archived));

    let filtered = grouped.filter_archived_sessions(&providers);
    assert!(filtered.table_entries.is_empty());
    assert!(filtered.selectable_indices.is_empty());
}

#[test]
fn filter_archived_sessions_keeps_agent_items() {
    let agent = flotilla_protocol::WorkItem {
        kind: WorkItemKind::Agent,
        identity: WorkItemIdentity::Agent("a1".into()),
        host: flotilla_protocol::HostName::local(),
        branch: None,
        description: "agent".into(),
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
        agent_keys: vec!["a1".into()],
    };

    let mut grouped = GroupedWorkItems::default();
    grouped.table_entries.push(GroupEntry::Header(SectionHeader("Agents".into())));
    grouped.selectable_indices.push(1);
    grouped.table_entries.push(GroupEntry::Item(Box::new(agent)));

    let providers = ProviderData::default();
    let filtered = grouped.filter_archived_sessions(&providers);

    assert_eq!(filtered.selectable_indices.len(), 1);
    assert_eq!(filtered.table_entries.len(), 2);
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
    providers.checkouts.insert(hp("/repo"), TestCheckout::new("main").at("/repo").is_main(true).with_branch_key().build());
    // feature checkout + PR
    providers.checkouts.insert(hp("/repo.feat"), TestCheckout::new("feat-login").at("/repo.feat").is_main(false).with_branch_key().build());
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

    // Now group them
    let labels = default_labels();
    let proto_items: Vec<_> = work_items.iter().map(to_proto).collect();
    let grouped = group_work_items(&proto_items, &providers, &labels, Path::new("/tmp"));

    // Should have sections for checkouts, sessions, remote, issues
    let header_count = grouped.table_entries.iter().filter(|e| matches!(e, GroupEntry::Header(_))).count();
    assert_eq!(header_count, 4, "should have exactly 4 section headers");

    // All items should be selectable
    assert_eq!(grouped.selectable_indices.len(), 5);
}

#[test]
fn workspace_only_joins_checkout_through_attachable_set() {
    let mut providers = new_providers();

    let remote_checkout = flotilla_protocol::HostPath::new(flotilla_protocol::HostName::new("feta"), "/remote/feat-set");
    let local_checkout = flotilla_protocol::HostPath::new(flotilla_protocol::HostName::new("kiwi"), "/Users/robert/dev/project");
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
        checkout: Some(remote_checkout.clone()),
        template_identity: None,
        members: vec![],
    });
    providers.workspaces.insert("ws-1".to_string(), Workspace {
        name: "feat-set@feta".to_string(),
        directories: vec![PathBuf::from("/Users/robert/dev/project")],
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

    let co_path = flotilla_protocol::HostPath::new(flotilla_protocol::HostName::local(), "/tmp/feat-set");
    let set_id = flotilla_protocol::AttachableSetId::new("set-1");

    providers.checkouts.insert(co_path.clone(), TestCheckout::new("feat-set").at("/tmp/feat-set").is_main(false).with_branch_key().build());
    providers.attachable_sets.insert(set_id.clone(), make_attachable_set("set-1", "/tmp/feat-set"));
    providers.workspaces.insert("ws-1".to_string(), Workspace {
        name: "feat-set".to_string(),
        directories: vec![PathBuf::from("/tmp/feat-set")],
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
