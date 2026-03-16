use flotilla_protocol::{CheckoutTarget, Command, CommandAction, HostName, RepoLabels, WorkItem, WorkItemKind};

use super::App;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    SwitchToWorkspace,
    CreateWorkspace,
    RemoveCheckout,
    CreateCheckout,
    GenerateBranchName,
    OpenChangeRequest,
    OpenIssue,
    LinkIssuesToChangeRequest,
    TeleportSession,
    ArchiveSession,
    CloseChangeRequest,
}

impl Intent {
    pub fn label(&self, labels: &RepoLabels) -> String {
        match self {
            Intent::SwitchToWorkspace => "Switch to workspace".into(),
            Intent::CreateWorkspace => "Create workspace".into(),
            Intent::RemoveCheckout => format!("Remove {}", labels.checkouts.noun),
            Intent::CreateCheckout => format!("Create {}", labels.checkouts.noun),
            Intent::GenerateBranchName => "Generate branch name".into(),
            Intent::OpenChangeRequest => format!("Open {} in browser", labels.change_requests.noun),
            Intent::OpenIssue => "Open issue in browser".into(),
            Intent::LinkIssuesToChangeRequest => {
                format!("Link issues to {}", labels.change_requests.noun)
            }
            Intent::TeleportSession => "Teleport session".into(),
            Intent::ArchiveSession => "Archive session".into(),
            Intent::CloseChangeRequest => format!("Close {}", labels.change_requests.noun),
        }
    }

    pub fn is_available(&self, item: &WorkItem) -> bool {
        match self {
            Intent::SwitchToWorkspace => !item.workspace_refs.is_empty(),
            Intent::CreateWorkspace => item.checkout_key().is_some() && item.workspace_refs.is_empty(),
            Intent::RemoveCheckout => item.checkout_key().is_some() && !item.is_main_checkout,
            Intent::CreateCheckout => item.checkout_key().is_none() && item.branch.is_some(),
            Intent::GenerateBranchName => item.branch.is_none() && !item.issue_keys.is_empty(),
            Intent::OpenChangeRequest => item.change_request_key.is_some(),
            Intent::OpenIssue => !item.issue_keys.is_empty(),
            Intent::LinkIssuesToChangeRequest => {
                item.change_request_key.is_some() && item.checkout_key().is_some() && !item.issue_keys.is_empty()
            }
            Intent::TeleportSession => item.session_key.is_some(),
            Intent::ArchiveSession => item.session_key.is_some(),
            Intent::CloseChangeRequest => item.change_request_key.is_some(),
        }
    }

    /// Whether this intent requires local filesystem access.
    ///
    /// Returns `true` for actions that operate on the local filesystem
    /// (switch workspace, remove checkout, teleport session). These should be
    /// hidden for work items from remote hosts.
    pub fn requires_local_host(&self) -> bool {
        matches!(self, Intent::SwitchToWorkspace | Intent::RemoveCheckout | Intent::TeleportSession)
    }

    /// Whether this intent is allowed given the item's host provenance.
    ///
    /// Remote items (where `item.host != my_host`) cannot use intents that
    /// require local filesystem access. If `my_host` is `None`, all items
    /// are treated as local (pre-multi-host compatibility).
    pub fn is_allowed_for_host(&self, item: &WorkItem, my_host: &Option<HostName>) -> bool {
        if !self.requires_local_host() {
            return true;
        }
        match my_host {
            Some(host) => item.host == *host,
            None => true,
        }
    }

    pub fn shortcut_hint(&self, labels: &RepoLabels) -> Option<String> {
        match self {
            Intent::RemoveCheckout => Some(format!("d:remove {}", labels.checkouts.noun)),
            Intent::OpenChangeRequest => {
                if labels.change_requests.abbr.is_empty() {
                    Some("p:show".into())
                } else {
                    Some(format!("p:show {}", labels.change_requests.abbr))
                }
            }
            _ => None,
        }
    }

    /// Resolve an intent into a concrete Command, given the current item and app state.
    /// Returns None if the intent can't be resolved (missing data).
    pub fn resolve(&self, item: &WorkItem, app: &App) -> Option<Command> {
        match self {
            Intent::SwitchToWorkspace => {
                item.workspace_refs.first().map(|ws_ref| app.repo_command(CommandAction::SelectWorkspace { ws_ref: ws_ref.clone() }))
            }
            Intent::CreateWorkspace => item.checkout_key().map(|p| {
                let label =
                    item.branch.clone().unwrap_or_else(|| p.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                let command = app.item_host_repo_command(
                    CommandAction::PrepareTerminalForCheckout { checkout_path: p.path.clone(), commands: app.local_template_commands() },
                    item,
                );
                if command.host.is_some() {
                    command
                } else {
                    app.repo_command(CommandAction::CreateWorkspaceForCheckout { checkout_path: p.path.clone(), label })
                }
            }),
            Intent::RemoveCheckout => {
                if item.kind != WorkItemKind::Checkout || item.is_main_checkout {
                    return None;
                }
                let branch = item.branch.as_ref()?.to_string();
                let checkout_path = item.checkout_key().map(|p| p.path.clone());
                let change_request_id = item.change_request_key.clone();
                Some(app.repo_command(CommandAction::FetchCheckoutStatus { branch, checkout_path, change_request_id }))
            }
            Intent::CreateCheckout => item.branch.as_ref().map(|branch| {
                let target = if item.kind == WorkItemKind::RemoteBranch || item.kind == WorkItemKind::ChangeRequest {
                    CheckoutTarget::Branch(branch.to_string())
                } else {
                    CheckoutTarget::FreshBranch(branch.to_string())
                };
                app.targeted_command(CommandAction::Checkout {
                    repo: flotilla_protocol::RepoSelector::Identity(app.model.active_repo_identity().clone()),
                    target,
                    issue_ids: Vec::new(),
                })
            }),
            Intent::GenerateBranchName => {
                if !item.issue_keys.is_empty() {
                    Some(app.targeted_repo_command(CommandAction::GenerateBranchName { issue_keys: item.issue_keys.clone() }))
                } else {
                    None
                }
            }
            Intent::OpenChangeRequest => item
                .change_request_key
                .as_ref()
                .map(|k| app.provider_repo_command(CommandAction::OpenChangeRequest { id: k.clone() }, item)),
            Intent::OpenIssue => {
                item.issue_keys.first().map(|k| app.provider_repo_command(CommandAction::OpenIssue { id: k.clone() }, item))
            }
            Intent::LinkIssuesToChangeRequest => {
                let change_request_key = item.change_request_key.as_ref()?;
                let co_key = item.checkout_key()?;
                let providers = &app.model.active().providers;
                let cr = providers.change_requests.get(change_request_key.as_str())?;
                let co = providers.checkouts.get(co_key)?;

                // Find issue IDs from checkout that aren't already on the PR
                let pr_issue_ids: std::collections::HashSet<&str> = cr
                    .association_keys
                    .iter()
                    .map(|k| {
                        let flotilla_protocol::AssociationKey::IssueRef(_, id) = k;
                        id.as_str()
                    })
                    .collect();
                let missing: Vec<String> = co
                    .association_keys
                    .iter()
                    .filter_map(|k| {
                        let flotilla_protocol::AssociationKey::IssueRef(_, id) = k;
                        if !pr_issue_ids.contains(id.as_str()) {
                            Some(id.clone())
                        } else {
                            None
                        }
                    })
                    .collect();

                if missing.is_empty() {
                    return None;
                }
                Some(app.provider_repo_command(
                    CommandAction::LinkIssuesToChangeRequest { change_request_id: change_request_key.clone(), issue_ids: missing },
                    item,
                ))
            }
            Intent::TeleportSession => item.session_key.as_ref().map(|k| {
                app.repo_command(CommandAction::TeleportSession {
                    session_id: k.clone(),
                    branch: item.branch.clone(),
                    checkout_key: item.checkout_key().map(|p| p.path.clone()),
                })
            }),
            Intent::ArchiveSession => {
                item.session_key.as_ref().map(|k| app.provider_repo_command(CommandAction::ArchiveSession { session_id: k.clone() }, item))
            }
            Intent::CloseChangeRequest => {
                let cr_key = item.change_request_key.as_ref()?;
                let providers = &app.model.active().providers;
                let cr = providers.change_requests.get(cr_key.as_str())?;
                if cr.status != flotilla_protocol::ChangeRequestStatus::Open {
                    return None;
                }
                Some(app.provider_repo_command(CommandAction::CloseChangeRequest { id: cr_key.clone() }, item))
            }
        }
    }

    pub fn all_in_menu_order() -> &'static [Intent] {
        &[
            Intent::SwitchToWorkspace,
            Intent::CreateWorkspace,
            Intent::RemoveCheckout,
            Intent::CreateCheckout,
            Intent::GenerateBranchName,
            Intent::OpenChangeRequest,
            Intent::OpenIssue,
            Intent::LinkIssuesToChangeRequest,
            Intent::TeleportSession,
            Intent::ArchiveSession,
            Intent::CloseChangeRequest,
        ]
    }

    pub fn enter_priority() -> &'static [Intent] {
        &[Intent::SwitchToWorkspace, Intent::TeleportSession, Intent::CreateWorkspace, Intent::CreateCheckout, Intent::GenerateBranchName]
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use flotilla_protocol::{
        CategoryLabels, ChangeRequest, ChangeRequestStatus, Checkout, CheckoutRef, CorrelationKey, HostName, HostPath, RepoLabels,
        RepoSelector,
    };

    use super::*;
    use crate::app::{
        test_support::{bare_item, checkout_item, pr_item, remote_branch_item, session_item, stub_app},
        PeerStatus, TuiHostState, TuiRepoModel,
    };

    // ── Helpers ──

    fn insert_local_host(model: &mut super::super::TuiModel, name: &str) {
        let host_name = HostName::new(name);
        model.hosts.insert(host_name.clone(), TuiHostState {
            host_name: host_name.clone(),
            is_local: true,
            status: PeerStatus::Connected,
            summary: flotilla_protocol::HostSummary {
                host_name,
                system: flotilla_protocol::SystemInfo::default(),
                inventory: flotilla_protocol::ToolInventory::default(),
                providers: vec![],
            },
        });
    }

    fn default_labels() -> RepoLabels {
        RepoLabels::default()
    }

    fn custom_labels() -> RepoLabels {
        RepoLabels {
            checkouts: CategoryLabels { section: "Worktrees".into(), noun: "worktree".into(), abbr: "wt".into() },
            change_requests: CategoryLabels { section: "Pull Requests".into(), noun: "PR".into(), abbr: "pr".into() },
            issues: CategoryLabels { section: "Issues".into(), noun: "issue".into(), abbr: "iss".into() },
            cloud_agents: CategoryLabels { section: "Sessions".into(), noun: "session".into(), abbr: "sess".into() },
        }
    }

    // ── is_available tests ──

    #[test]
    fn switch_to_workspace_available_when_workspace_refs_present() {
        let mut item = bare_item();
        assert!(!Intent::SwitchToWorkspace.is_available(&item));
        item.workspace_refs = vec!["ws-1".into()];
        assert!(Intent::SwitchToWorkspace.is_available(&item));
    }

    #[test]
    fn create_workspace_needs_checkout_and_no_workspace() {
        let item = checkout_item("feat/x", "/tmp/feat-x", false);
        // Has checkout, no workspace -> available
        assert!(Intent::CreateWorkspace.is_available(&item));

        // Has checkout AND workspace -> not available
        let mut with_ws = item.clone();
        with_ws.workspace_refs = vec!["ws-1".into()];
        assert!(!Intent::CreateWorkspace.is_available(&with_ws));

        // No checkout -> not available
        let no_co = bare_item();
        assert!(!Intent::CreateWorkspace.is_available(&no_co));
    }

    #[test]
    fn remove_worktree_needs_checkout_and_not_main() {
        let item = checkout_item("feat/x", "/tmp/feat-x", false);
        assert!(Intent::RemoveCheckout.is_available(&item));

        // Main worktree -> not available
        let main_item = checkout_item("main", "/tmp/main", true);
        assert!(!Intent::RemoveCheckout.is_available(&main_item));

        // No checkout -> not available
        let no_co = bare_item();
        assert!(!Intent::RemoveCheckout.is_available(&no_co));
    }

    #[test]
    fn create_worktree_and_workspace_needs_no_checkout_and_has_branch() {
        // Remote branch: no checkout, has branch -> available
        let item = remote_branch_item("feat/remote");
        assert!(Intent::CreateCheckout.is_available(&item));

        // Has checkout -> not available
        let co_item = checkout_item("feat/x", "/tmp/feat-x", false);
        assert!(!Intent::CreateCheckout.is_available(&co_item));

        // No branch -> not available
        let no_branch = bare_item();
        assert!(!Intent::CreateCheckout.is_available(&no_branch));
    }

    #[test]
    fn generate_branch_name_needs_no_branch_and_has_issues() {
        let mut item = bare_item();
        item.branch = None;
        item.issue_keys = vec!["42".into()];
        assert!(Intent::GenerateBranchName.is_available(&item));

        // Has a branch -> not available
        let mut with_branch = item.clone();
        with_branch.branch = Some("feat/x".into());
        assert!(!Intent::GenerateBranchName.is_available(&with_branch));

        // No issues -> not available
        let mut no_issues = item.clone();
        no_issues.issue_keys = Vec::new();
        assert!(!Intent::GenerateBranchName.is_available(&no_issues));
    }

    #[test]
    fn open_pr_needs_change_request_key() {
        let pr = pr_item("123");
        assert!(Intent::OpenChangeRequest.is_available(&pr));

        let no_pr = bare_item();
        assert!(!Intent::OpenChangeRequest.is_available(&no_pr));
    }

    #[test]
    fn open_issue_needs_issue_keys() {
        let mut item = bare_item();
        assert!(!Intent::OpenIssue.is_available(&item));

        item.issue_keys = vec!["7".into()];
        assert!(Intent::OpenIssue.is_available(&item));
    }

    #[test]
    fn link_issues_to_pr_needs_pr_checkout_and_issues() {
        // Has PR, checkout, and issues -> available
        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.change_request_key = Some("42".into());
        item.issue_keys = vec!["7".into()];
        assert!(Intent::LinkIssuesToChangeRequest.is_available(&item));

        // Missing PR -> not available
        let mut no_pr = item.clone();
        no_pr.change_request_key = None;
        assert!(!Intent::LinkIssuesToChangeRequest.is_available(&no_pr));

        // Missing checkout -> not available
        let mut no_co = item.clone();
        no_co.checkout = None;
        assert!(!Intent::LinkIssuesToChangeRequest.is_available(&no_co));

        // Missing issues -> not available
        let mut no_issues = item.clone();
        no_issues.issue_keys = Vec::new();
        assert!(!Intent::LinkIssuesToChangeRequest.is_available(&no_issues));
    }

    #[test]
    fn teleport_session_needs_session_key() {
        let sess = session_item("sess-1");
        assert!(Intent::TeleportSession.is_available(&sess));

        let no_sess = bare_item();
        assert!(!Intent::TeleportSession.is_available(&no_sess));
    }

    #[test]
    fn archive_session_needs_session_key() {
        let sess = session_item("sess-1");
        assert!(Intent::ArchiveSession.is_available(&sess));

        let no_sess = bare_item();
        assert!(!Intent::ArchiveSession.is_available(&no_sess));
    }

    #[test]
    fn close_pr_needs_change_request_key() {
        let pr = pr_item("123");
        assert!(Intent::CloseChangeRequest.is_available(&pr));

        let no_pr = bare_item();
        assert!(!Intent::CloseChangeRequest.is_available(&no_pr));
    }

    // ── is_available: edge cases ──

    #[test]
    fn switch_to_workspace_with_multiple_refs() {
        let mut item = bare_item();
        item.workspace_refs = vec!["ws-1".into(), "ws-2".into()];
        assert!(Intent::SwitchToWorkspace.is_available(&item));
    }

    // ── label tests ──

    #[test]
    fn label_with_default_labels() {
        let labels = default_labels();
        assert_eq!(Intent::SwitchToWorkspace.label(&labels), "Switch to workspace");
        assert_eq!(Intent::CreateWorkspace.label(&labels), "Create workspace");
        assert_eq!(Intent::RemoveCheckout.label(&labels), "Remove item");
        assert_eq!(Intent::CreateCheckout.label(&labels), "Create item");
        assert_eq!(Intent::GenerateBranchName.label(&labels), "Generate branch name");
        assert_eq!(Intent::OpenChangeRequest.label(&labels), "Open item in browser");
        assert_eq!(Intent::OpenIssue.label(&labels), "Open issue in browser");
        assert_eq!(Intent::LinkIssuesToChangeRequest.label(&labels), "Link issues to item");
        assert_eq!(Intent::TeleportSession.label(&labels), "Teleport session");
        assert_eq!(Intent::ArchiveSession.label(&labels), "Archive session");
        assert_eq!(Intent::CloseChangeRequest.label(&labels), "Close item");
    }

    #[test]
    fn label_with_custom_labels() {
        let labels = custom_labels();
        assert_eq!(Intent::RemoveCheckout.label(&labels), "Remove worktree");
        assert_eq!(Intent::CreateCheckout.label(&labels), "Create worktree");
        assert_eq!(Intent::OpenChangeRequest.label(&labels), "Open PR in browser");
        assert_eq!(Intent::LinkIssuesToChangeRequest.label(&labels), "Link issues to PR");
        assert_eq!(Intent::CloseChangeRequest.label(&labels), "Close PR");
    }

    #[test]
    fn label_static_strings_unaffected_by_labels() {
        let labels = custom_labels();
        assert_eq!(Intent::SwitchToWorkspace.label(&labels), "Switch to workspace");
        assert_eq!(Intent::CreateWorkspace.label(&labels), "Create workspace");
        assert_eq!(Intent::GenerateBranchName.label(&labels), "Generate branch name");
        assert_eq!(Intent::OpenIssue.label(&labels), "Open issue in browser");
        assert_eq!(Intent::TeleportSession.label(&labels), "Teleport session");
        assert_eq!(Intent::ArchiveSession.label(&labels), "Archive session");
    }

    // ── shortcut_hint tests ──

    #[test]
    fn shortcut_hint_remove_worktree() {
        let labels = default_labels();
        assert_eq!(Intent::RemoveCheckout.shortcut_hint(&labels), Some("d:remove item".into()));
    }

    #[test]
    fn shortcut_hint_open_pr() {
        let labels = default_labels();
        assert_eq!(Intent::OpenChangeRequest.shortcut_hint(&labels), Some("p:show".into()));
    }

    #[test]
    fn shortcut_hint_with_custom_labels() {
        let labels = custom_labels();
        assert_eq!(Intent::RemoveCheckout.shortcut_hint(&labels), Some("d:remove worktree".into()));
        assert_eq!(Intent::OpenChangeRequest.shortcut_hint(&labels), Some("p:show pr".into()));
    }

    #[test]
    fn shortcut_hint_none_for_other_intents() {
        let labels = default_labels();
        assert!(Intent::SwitchToWorkspace.shortcut_hint(&labels).is_none());
        assert!(Intent::CreateWorkspace.shortcut_hint(&labels).is_none());
        assert!(Intent::CreateCheckout.shortcut_hint(&labels).is_none());
        assert!(Intent::GenerateBranchName.shortcut_hint(&labels).is_none());
        assert!(Intent::OpenIssue.shortcut_hint(&labels).is_none());
        assert!(Intent::LinkIssuesToChangeRequest.shortcut_hint(&labels).is_none());
        assert!(Intent::TeleportSession.shortcut_hint(&labels).is_none());
        assert!(Intent::ArchiveSession.shortcut_hint(&labels).is_none());
        assert!(Intent::CloseChangeRequest.shortcut_hint(&labels).is_none());
    }

    // ── all_in_menu_order tests ──

    /// Exhaustive match — if a new Intent variant is added this will fail to
    /// compile, reminding the author to add it to `all_in_menu_order()` (and
    /// `enter_priority()` if appropriate).
    fn all_intent_variants() -> Vec<Intent> {
        // Intentionally no wildcard — forces a compile error on new variants.
        [
            Intent::SwitchToWorkspace,
            Intent::CreateWorkspace,
            Intent::RemoveCheckout,
            Intent::CreateCheckout,
            Intent::GenerateBranchName,
            Intent::OpenChangeRequest,
            Intent::OpenIssue,
            Intent::LinkIssuesToChangeRequest,
            Intent::TeleportSession,
            Intent::ArchiveSession,
            Intent::CloseChangeRequest,
        ]
        .into_iter()
        .map(|v| match v {
            Intent::SwitchToWorkspace => v,
            Intent::CreateWorkspace => v,
            Intent::RemoveCheckout => v,
            Intent::CreateCheckout => v,
            Intent::GenerateBranchName => v,
            Intent::OpenChangeRequest => v,
            Intent::OpenIssue => v,
            Intent::LinkIssuesToChangeRequest => v,
            Intent::TeleportSession => v,
            Intent::ArchiveSession => v,
            Intent::CloseChangeRequest => v,
        })
        .collect()
    }

    #[test]
    fn all_in_menu_order_contains_every_variant() {
        let all_variants = all_intent_variants();
        let menu = Intent::all_in_menu_order();
        for variant in &all_variants {
            assert!(menu.contains(variant), "{variant:?} missing from all_in_menu_order()");
        }
        assert_eq!(menu.len(), all_variants.len());
    }

    // ── enter_priority tests ──

    #[test]
    fn enter_priority_matches_expected_sequence_and_is_subset() {
        let expected = [
            Intent::SwitchToWorkspace,
            Intent::TeleportSession,
            Intent::CreateWorkspace,
            Intent::CreateCheckout,
            Intent::GenerateBranchName,
        ];
        assert_eq!(Intent::enter_priority(), &expected);
        for intent in expected {
            assert!(Intent::all_in_menu_order().contains(&intent));
        }
    }

    // ── resolve tests ──
    //
    // resolve() requires &App so we use the shared TUI test harness.

    use flotilla_protocol::ProviderData;

    #[test]
    fn resolve_switch_to_workspace() {
        let app = stub_app();
        let mut item = bare_item();
        item.workspace_refs = vec!["ws-abc".into()];
        let cmd = Intent::SwitchToWorkspace.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command { action: CommandAction::SelectWorkspace { ws_ref }, .. } => assert_eq!(ws_ref, "ws-abc"),
            other => panic!("expected SelectWorkspace, got {other:?}"),
        }
    }

    #[test]
    fn resolve_switch_to_workspace_none_when_empty() {
        let app = stub_app();
        let item = bare_item();
        assert!(Intent::SwitchToWorkspace.resolve(&item, &app).is_none());
    }

    #[test]
    fn resolve_switch_to_workspace_picks_first_ref() {
        let app = stub_app();
        let mut item = bare_item();
        item.workspace_refs = vec!["ws-first".into(), "ws-second".into()];
        match Intent::SwitchToWorkspace.resolve(&item, &app).unwrap() {
            Command { action: CommandAction::SelectWorkspace { ws_ref }, .. } => assert_eq!(ws_ref, "ws-first"),
            other => panic!("expected SelectWorkspace, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_workspace() {
        let mut app = stub_app();
        app.ui.target_host = Some(HostName::new("remote-a"));
        let item = checkout_item("feat/x", "/tmp/feat-x", false);
        let cmd = Intent::CreateWorkspace.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command { host, action: CommandAction::CreateWorkspaceForCheckout { checkout_path, label }, .. } => {
                assert_eq!(host, None);
                assert_eq!(checkout_path, PathBuf::from("/tmp/feat-x"));
                assert_eq!(label, "feat/x");
            }
            other => panic!("expected CreateWorkspaceForCheckout, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_workspace_on_remote_checkout_prepares_remote_terminal() {
        let mut app = stub_app();
        insert_local_host(&mut app.model, &HostName::local().to_string());
        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.host = HostName::new("remote-a");
        item.checkout =
            Some(CheckoutRef { key: HostPath::new(HostName::new("remote-a"), PathBuf::from("/remote/feat-x")), is_main_checkout: false });

        let cmd = Intent::CreateWorkspace.resolve(&item, &app).unwrap();

        match cmd {
            Command { host, action: CommandAction::PrepareTerminalForCheckout { checkout_path, .. }, .. } => {
                assert_eq!(host, Some(HostName::new("remote-a")));
                assert_eq!(checkout_path, PathBuf::from("/remote/feat-x"));
            }
            other => panic!("expected PrepareTerminalForCheckout, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_workspace_none_without_checkout() {
        let app = stub_app();
        let item = bare_item();
        assert!(Intent::CreateWorkspace.resolve(&item, &app).is_none());
    }

    #[test]
    fn resolve_remove_worktree_checkout_item() {
        let app = stub_app();
        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.change_request_key = Some("99".into());
        let cmd = Intent::RemoveCheckout.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command { action: CommandAction::FetchCheckoutStatus { branch, checkout_path, change_request_id }, .. } => {
                assert_eq!(branch, "feat/x");
                assert_eq!(checkout_path, Some(PathBuf::from("/tmp/feat-x")));
                assert_eq!(change_request_id, Some("99".into()));
            }
            other => panic!("expected FetchCheckoutStatus, got {other:?}"),
        }
    }

    #[test]
    fn resolve_remove_worktree_none_for_main() {
        let app = stub_app();
        let item = checkout_item("main", "/tmp/main", true);
        assert!(Intent::RemoveCheckout.resolve(&item, &app).is_none());
    }

    #[test]
    fn resolve_remove_worktree_none_for_non_checkout_kind() {
        let app = stub_app();
        let mut item = pr_item("42");
        // Even with a checkout path, the kind check prevents resolve
        item.checkout =
            Some(CheckoutRef { key: HostPath::new(HostName::new("test-host"), PathBuf::from("/tmp/pr-co")), is_main_checkout: false });
        assert!(Intent::RemoveCheckout.resolve(&item, &app).is_none());
    }

    #[test]
    fn resolve_remove_worktree_none_without_branch() {
        let app = stub_app();
        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.branch = None; // branch required for FetchDeleteInfo
        assert!(Intent::RemoveCheckout.resolve(&item, &app).is_none());
    }

    #[test]
    fn resolve_remove_worktree_without_change_request_key() {
        let app = stub_app();
        let item = checkout_item("feat/x", "/tmp/feat-x", false);
        let cmd = Intent::RemoveCheckout.resolve(&item, &app).unwrap();
        match cmd {
            Command { action: CommandAction::FetchCheckoutStatus { change_request_id, .. }, .. } => {
                assert_eq!(change_request_id, None);
            }
            other => panic!("expected FetchCheckoutStatus, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_worktree_and_workspace_remote_branch() {
        let app = stub_app();
        let item = remote_branch_item("feat/remote");
        let cmd = Intent::CreateCheckout.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command { action: CommandAction::Checkout { repo, target, issue_ids }, .. } => {
                assert_eq!(repo, flotilla_protocol::RepoSelector::Identity(app.model.active_repo_identity().clone()));
                assert_eq!(target, CheckoutTarget::Branch("feat/remote".into()));
                assert!(issue_ids.is_empty());
            }
            other => panic!("expected CreateCheckout, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_worktree_and_workspace_pr_item() {
        let app = stub_app();
        let item = pr_item("42");
        let cmd = Intent::CreateCheckout.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command { action: CommandAction::Checkout { repo, target, .. }, .. } => {
                assert_eq!(repo, flotilla_protocol::RepoSelector::Identity(app.model.active_repo_identity().clone()));
                assert_eq!(target, CheckoutTarget::Branch("feat/pr-branch".into()));
            }
            other => panic!("expected CreateCheckout, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_worktree_and_workspace_uses_selected_target_host() {
        let mut app = stub_app();
        app.ui.target_host = Some(HostName::new("remote-a"));
        let item = remote_branch_item("feat/remote");

        let cmd = Intent::CreateCheckout.resolve(&item, &app).unwrap();

        assert_eq!(cmd.host, Some(HostName::new("remote-a")));
    }

    #[test]
    fn resolve_create_worktree_and_workspace_session_item() {
        let app = stub_app();
        let item = session_item("sess-1");
        let cmd = Intent::CreateCheckout.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command { action: CommandAction::Checkout { target, .. }, .. } => {
                assert_eq!(target, CheckoutTarget::FreshBranch("feat/session-branch".into()));
            }
            other => panic!("expected CreateCheckout, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_worktree_and_workspace_none_without_branch() {
        let app = stub_app();
        let item = bare_item(); // no branch
        assert!(Intent::CreateCheckout.resolve(&item, &app).is_none());
    }

    #[test]
    fn resolve_generate_branch_name_with_issues() {
        let app = stub_app();
        let mut item = bare_item();
        item.issue_keys = vec!["42".into(), "43".into()];
        let cmd = Intent::GenerateBranchName.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command { context_repo, action: CommandAction::GenerateBranchName { issue_keys }, .. } => {
                assert_eq!(context_repo, Some(flotilla_protocol::RepoSelector::Identity(app.model.active_repo_identity().clone())));
                assert_eq!(issue_keys, vec!["42".to_string(), "43".to_string()]);
            }
            other => panic!("expected GenerateBranchName, got {other:?}"),
        }
    }

    #[test]
    fn resolve_generate_branch_name_none_without_issues() {
        let app = stub_app();
        let item = bare_item();
        assert!(Intent::GenerateBranchName.resolve(&item, &app).is_none());
    }

    #[test]
    fn resolve_open_pr() {
        let mut app = stub_app();
        insert_local_host(&mut app.model, &HostName::local().to_string());
        app.ui.target_host = Some(HostName::new("remote-a"));
        let mut item = pr_item("123");
        item.host = HostName::new("remote-b");
        let cmd = Intent::OpenChangeRequest.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command { host, action: CommandAction::OpenChangeRequest { id }, .. } => {
                assert_eq!(host, None);
                assert_eq!(id, "123");
            }
            other => panic!("expected OpenChangeRequest, got {other:?}"),
        }
    }

    #[test]
    fn resolve_open_pr_on_remote_only_repo_routes_to_remote_host_by_identity() {
        let app = remote_only_app();
        let mut item = pr_item("123");
        item.host = HostName::new("desktop");
        let cmd = Intent::OpenChangeRequest.resolve(&item, &app).expect("command");
        match cmd {
            Command { host, context_repo, action: CommandAction::OpenChangeRequest { id } } => {
                assert_eq!(host, Some(HostName::new("desktop")));
                assert_eq!(context_repo, Some(RepoSelector::Identity(app.model.active_repo_identity().clone())));
                assert_eq!(id, "123");
            }
            other => panic!("expected remote OpenChangeRequest, got {other:?}"),
        }
    }

    #[test]
    fn resolve_open_pr_none_without_change_request_key() {
        let app = stub_app();
        let item = bare_item();
        assert!(Intent::OpenChangeRequest.resolve(&item, &app).is_none());
    }

    #[test]
    fn resolve_open_issue() {
        let app = stub_app();
        let mut item = bare_item();
        item.issue_keys = vec!["7".into(), "8".into()];
        let cmd = Intent::OpenIssue.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command { action: CommandAction::OpenIssue { id }, .. } => {
                // Opens the first issue
                assert_eq!(id, "7");
            }
            other => panic!("expected OpenIssue, got {other:?}"),
        }
    }

    #[test]
    fn resolve_open_issue_on_remote_only_repo_routes_to_remote_host_by_identity() {
        let app = remote_only_app();
        let mut item = bare_item();
        item.host = HostName::new("desktop");
        item.issue_keys = vec!["7".into(), "8".into()];
        let cmd = Intent::OpenIssue.resolve(&item, &app).expect("command");
        match cmd {
            Command { host, context_repo, action: CommandAction::OpenIssue { id } } => {
                assert_eq!(host, Some(HostName::new("desktop")));
                assert_eq!(context_repo, Some(RepoSelector::Identity(app.model.active_repo_identity().clone())));
                assert_eq!(id, "7");
            }
            other => panic!("expected remote OpenIssue, got {other:?}"),
        }
    }

    #[test]
    fn resolve_open_issue_none_without_issues() {
        let app = stub_app();
        let item = bare_item();
        assert!(Intent::OpenIssue.resolve(&item, &app).is_none());
    }

    #[test]
    fn resolve_teleport_session() {
        let app = stub_app();
        let mut item = session_item("sess-42");
        item.checkout =
            Some(CheckoutRef { key: HostPath::new(HostName::new("test-host"), PathBuf::from("/tmp/co")), is_main_checkout: false });
        let cmd = Intent::TeleportSession.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command { action: CommandAction::TeleportSession { session_id, branch, checkout_key }, .. } => {
                assert_eq!(session_id, "sess-42");
                assert_eq!(branch, Some("feat/session-branch".into()));
                assert_eq!(checkout_key, Some(PathBuf::from("/tmp/co")));
            }
            other => panic!("expected TeleportSession, got {other:?}"),
        }
    }

    #[test]
    fn resolve_teleport_session_without_checkout() {
        let app = stub_app();
        let item = session_item("sess-42");
        match Intent::TeleportSession.resolve(&item, &app).unwrap() {
            Command { action: CommandAction::TeleportSession { checkout_key, branch, .. }, .. } => {
                assert!(checkout_key.is_none());
                assert_eq!(branch, Some("feat/session-branch".into()));
            }
            other => panic!("expected TeleportSession, got {other:?}"),
        }
    }

    #[test]
    fn resolve_teleport_session_none_without_session() {
        let app = stub_app();
        let item = bare_item();
        assert!(Intent::TeleportSession.resolve(&item, &app).is_none());
    }

    #[test]
    fn resolve_archive_session() {
        let app = stub_app();
        let item = session_item("sess-99");
        let cmd = Intent::ArchiveSession.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command { host, action: CommandAction::ArchiveSession { session_id }, .. } => {
                assert_eq!(host, None);
                assert_eq!(session_id, "sess-99");
            }
            other => panic!("expected ArchiveSession, got {other:?}"),
        }
    }

    #[test]
    fn resolve_archive_session_on_remote_only_repo_routes_to_remote_host_by_identity() {
        let app = remote_only_app();
        let mut item = session_item("sess-99");
        item.host = HostName::new("desktop");
        let cmd = Intent::ArchiveSession.resolve(&item, &app).expect("command");
        match cmd {
            Command { host, context_repo, action: CommandAction::ArchiveSession { session_id } } => {
                assert_eq!(host, Some(HostName::new("desktop")));
                assert_eq!(context_repo, Some(RepoSelector::Identity(app.model.active_repo_identity().clone())));
                assert_eq!(session_id, "sess-99");
            }
            other => panic!("expected remote ArchiveSession, got {other:?}"),
        }
    }

    #[test]
    fn resolve_archive_session_none_without_session() {
        let app = stub_app();
        let item = bare_item();
        assert!(Intent::ArchiveSession.resolve(&item, &app).is_none());
    }

    // ── resolve: LinkIssuesToPr (requires App with provider data) ──

    /// Build an App whose active repo has a PR "42" (with issue "10" already
    /// linked) and a checkout at `/tmp/feat-x` whose association keys reference
    /// the given `checkout_issue_ids`.
    fn app_with_pr_and_issues(checkout_issue_ids: &[&str]) -> App {
        use flotilla_protocol::{AssociationKey, ChangeRequest, ChangeRequestStatus, Checkout, CorrelationKey};

        let mut app = stub_app();

        let mut providers = ProviderData::default();
        providers.change_requests.insert("42".into(), ChangeRequest {
            title: "Fix bug".into(),
            branch: "feat/x".into(),
            status: ChangeRequestStatus::Open,
            body: None,
            correlation_keys: vec![],
            // PR already has issue "10" linked
            association_keys: vec![AssociationKey::IssueRef("gh".into(), "10".into())],
            provider_name: String::new(),
            provider_display_name: String::new(),
        });
        let co_path = HostPath::new(HostName::local(), PathBuf::from("/tmp/feat-x"));
        providers.checkouts.insert(co_path.clone(), Checkout {
            branch: "feat/x".into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![CorrelationKey::CheckoutPath(co_path.clone())],
            association_keys: checkout_issue_ids.iter().map(|id| AssociationKey::IssueRef("gh".into(), (*id).into())).collect(),
        });

        let repo_identity = app.model.active_repo_identity().clone();
        app.model.repos.get_mut(&repo_identity).unwrap().providers = Arc::new(providers);

        app
    }

    fn remote_only_app() -> App {
        let mut app = stub_app();
        let old_identity = app.model.active_repo_identity().clone();
        let synthetic_path = PathBuf::from("<remote>/desktop/home/dev/repo");
        let old = app.model.repos.remove(&old_identity).expect("default repo");
        let remote_identity = flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() };
        let model = TuiRepoModel {
            identity: remote_identity.clone(),
            path: synthetic_path.clone(),
            providers: old.providers,
            labels: old.labels,
            provider_names: old.provider_names,
            provider_health: old.provider_health,
            loading: old.loading,
            issue_has_more: old.issue_has_more,
            issue_total: old.issue_total,
            issue_search_active: old.issue_search_active,
            issue_fetch_pending: old.issue_fetch_pending,
            issue_initial_requested: old.issue_initial_requested,
        };
        app.model.repo_order[0] = remote_identity.clone();
        app.model.repos.insert(remote_identity, model);
        insert_local_host(&mut app.model, &HostName::local().to_string());
        app
    }

    fn remote_only_app_with_providers() -> App {
        use flotilla_protocol::AssociationKey;

        let mut app = remote_only_app();
        let remote_host = HostName::new("desktop");
        let checkout_path = HostPath::new(remote_host.clone(), PathBuf::from("/srv/repo.feat-x"));

        let mut providers = flotilla_protocol::ProviderData::default();
        providers.change_requests.insert("42".into(), ChangeRequest {
            title: "Fix bug".into(),
            branch: "feat/x".into(),
            status: ChangeRequestStatus::Open,
            body: None,
            correlation_keys: vec![],
            association_keys: vec![AssociationKey::IssueRef("gh".into(), "10".into())],
            provider_name: String::new(),
            provider_display_name: String::new(),
        });
        providers.checkouts.insert(checkout_path.clone(), Checkout {
            branch: "feat/x".into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![CorrelationKey::CheckoutPath(checkout_path)],
            association_keys: vec![AssociationKey::IssueRef("gh".into(), "10".into()), AssociationKey::IssueRef("gh".into(), "20".into())],
        });

        let repo_identity = app.model.active_repo_identity().clone();
        app.model.repos.get_mut(&repo_identity).expect("remote repo").providers = Arc::new(providers);
        app
    }

    #[test]
    fn resolve_link_issues_to_pr_returns_none_when_provider_data_missing() {
        // Even if the WorkItem has the right keys, resolve needs matching
        // provider data in app.model. With empty ProviderData it returns None.
        let app = stub_app();
        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.change_request_key = Some("42".into());
        item.issue_keys = vec!["7".into()];
        let cmd = Intent::LinkIssuesToChangeRequest.resolve(&item, &app);
        assert!(cmd.is_none());
    }

    #[test]
    fn resolve_link_issues_to_pr_with_provider_data() {
        // Checkout has issues "10" (already on PR) and "20" (missing)
        let app = app_with_pr_and_issues(&["10", "20"]);

        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.change_request_key = Some("42".into());
        item.issue_keys = vec!["10".into(), "20".into()];

        let cmd = Intent::LinkIssuesToChangeRequest.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command { action: CommandAction::LinkIssuesToChangeRequest { change_request_id, issue_ids }, .. } => {
                assert_eq!(change_request_id, "42");
                // Only issue "20" should be linked ("10" is already on the PR)
                assert_eq!(issue_ids, vec!["20".to_string()]);
            }
            other => panic!("expected LinkIssuesToChangeRequest, got {other:?}"),
        }
    }

    #[test]
    fn resolve_link_issues_to_pr_on_remote_only_repo_routes_to_remote_host_by_identity() {
        let app = remote_only_app_with_providers();

        let mut item = checkout_item("feat/x", "/srv/repo.feat-x", false);
        item.host = HostName::new("desktop");
        item.checkout =
            Some(CheckoutRef { key: HostPath::new(HostName::new("desktop"), PathBuf::from("/srv/repo.feat-x")), is_main_checkout: false });
        item.change_request_key = Some("42".into());
        item.issue_keys = vec!["10".into(), "20".into()];

        let cmd = Intent::LinkIssuesToChangeRequest.resolve(&item, &app).expect("command");
        match cmd {
            Command { host, context_repo, action: CommandAction::LinkIssuesToChangeRequest { change_request_id, issue_ids } } => {
                assert_eq!(host, Some(HostName::new("desktop")));
                assert_eq!(context_repo, Some(RepoSelector::Identity(app.model.active_repo_identity().clone())));
                assert_eq!(change_request_id, "42");
                assert_eq!(issue_ids, vec!["20".to_string()]);
            }
            other => panic!("expected remote LinkIssuesToChangeRequest, got {other:?}"),
        }
    }

    #[test]
    fn resolve_link_issues_to_pr_none_when_all_issues_already_linked() {
        // Checkout has only issue "10", which is already on the PR
        let app = app_with_pr_and_issues(&["10"]);

        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.change_request_key = Some("42".into());
        item.issue_keys = vec!["10".into()];

        // All issues already linked -> returns None
        let cmd = Intent::LinkIssuesToChangeRequest.resolve(&item, &app);
        assert!(cmd.is_none());
    }

    #[test]
    fn resolve_close_change_request_on_remote_only_repo_routes_to_remote_host_by_identity() {
        let app = remote_only_app_with_providers();
        let mut item = pr_item("42");
        item.host = HostName::new("desktop");

        let cmd = Intent::CloseChangeRequest.resolve(&item, &app).expect("command");
        match cmd {
            Command { host, context_repo, action: CommandAction::CloseChangeRequest { id } } => {
                assert_eq!(host, Some(HostName::new("desktop")));
                assert_eq!(context_repo, Some(RepoSelector::Identity(app.model.active_repo_identity().clone())));
                assert_eq!(id, "42");
            }
            other => panic!("expected remote CloseChangeRequest, got {other:?}"),
        }
    }

    // ── Combined availability scenario ──

    #[test]
    fn rich_item_has_multiple_intents_available() {
        // A checkout with PR, session, issues, and workspace
        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.change_request_key = Some("42".into());
        item.session_key = Some("sess-1".into());
        item.issue_keys = vec!["7".into()];
        item.workspace_refs = vec!["ws-1".into()];

        let available: Vec<_> = Intent::all_in_menu_order().iter().filter(|i| i.is_available(&item)).collect();

        assert!(available.contains(&&Intent::SwitchToWorkspace));
        assert!(available.contains(&&Intent::OpenChangeRequest));
        assert!(available.contains(&&Intent::OpenIssue));
        assert!(available.contains(&&Intent::LinkIssuesToChangeRequest));
        assert!(available.contains(&&Intent::TeleportSession));
        assert!(available.contains(&&Intent::ArchiveSession));
        assert!(available.contains(&&Intent::RemoveCheckout));

        // These should NOT be available
        assert!(!available.contains(&&Intent::CreateWorkspace)); // has workspace
        assert!(!available.contains(&&Intent::CreateCheckout)); // has checkout
        assert!(!available.contains(&&Intent::GenerateBranchName)); // has branch
    }

    #[test]
    fn bare_item_has_no_intents_available() {
        let item = bare_item();
        let available: Vec<_> = Intent::all_in_menu_order().iter().filter(|i| i.is_available(&item)).collect();
        assert!(available.is_empty(), "bare item should have no intents, got {available:?}");
    }

    // ── requires_local_host tests ──

    #[test]
    fn requires_local_host_true_for_filesystem_intents() {
        assert!(Intent::SwitchToWorkspace.requires_local_host());
        assert!(Intent::RemoveCheckout.requires_local_host());
        assert!(Intent::TeleportSession.requires_local_host());
    }

    #[test]
    fn requires_local_host_false_for_non_filesystem_intents() {
        assert!(!Intent::CreateWorkspace.requires_local_host());
        assert!(!Intent::CreateCheckout.requires_local_host());
        assert!(!Intent::GenerateBranchName.requires_local_host());
        assert!(!Intent::OpenChangeRequest.requires_local_host());
        assert!(!Intent::OpenIssue.requires_local_host());
        assert!(!Intent::LinkIssuesToChangeRequest.requires_local_host());
        assert!(!Intent::ArchiveSession.requires_local_host());
        assert!(!Intent::CloseChangeRequest.requires_local_host());
    }

    // ── is_allowed_for_host tests ──

    #[test]
    fn allowed_for_host_local_item_with_known_host() {
        let item = checkout_item("feat/x", "/tmp/feat-x", false);
        let my_host = Some(HostName::local());
        // Local item, local host -> all intents allowed
        for intent in Intent::all_in_menu_order() {
            assert!(intent.is_allowed_for_host(&item, &my_host), "{intent:?} should be allowed for local item");
        }
    }

    #[test]
    fn allowed_for_host_remote_item_blocks_filesystem_intents() {
        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.host = HostName::new("remote-host");
        let my_host = Some(HostName::local());

        // Local-only filesystem intents should be blocked
        assert!(!Intent::SwitchToWorkspace.is_allowed_for_host(&item, &my_host));
        assert!(!Intent::RemoveCheckout.is_allowed_for_host(&item, &my_host));
        assert!(!Intent::TeleportSession.is_allowed_for_host(&item, &my_host));

        // Remote-executable intents should remain allowed
        assert!(Intent::CreateWorkspace.is_allowed_for_host(&item, &my_host));
        assert!(Intent::CreateCheckout.is_allowed_for_host(&item, &my_host));
        assert!(Intent::OpenChangeRequest.is_allowed_for_host(&item, &my_host));
        assert!(Intent::OpenIssue.is_allowed_for_host(&item, &my_host));
        assert!(Intent::GenerateBranchName.is_allowed_for_host(&item, &my_host));
        assert!(Intent::LinkIssuesToChangeRequest.is_allowed_for_host(&item, &my_host));
        assert!(Intent::ArchiveSession.is_allowed_for_host(&item, &my_host));
        assert!(Intent::CloseChangeRequest.is_allowed_for_host(&item, &my_host));
    }

    #[test]
    fn allowed_for_host_unknown_host_treats_all_as_local() {
        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.host = HostName::new("remote-host");
        let my_host: Option<HostName> = None;

        // When my_host is unknown, treat everything as local
        for intent in Intent::all_in_menu_order() {
            assert!(intent.is_allowed_for_host(&item, &my_host), "{intent:?} should be allowed when my_host is unknown");
        }
    }

    #[test]
    fn remote_item_action_menu_excludes_local_only_intents() {
        // A rich remote item that would normally have many intents
        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.host = HostName::new("remote-host");
        item.change_request_key = Some("42".into());
        item.session_key = Some("sess-1".into());
        item.issue_keys = vec!["7".into()];
        item.workspace_refs = vec!["ws-1".into()];

        let my_host = Some(HostName::local());

        let available: Vec<_> =
            Intent::all_in_menu_order().iter().filter(|i| i.is_available(&item) && i.is_allowed_for_host(&item, &my_host)).collect();

        // Local-only intents should be excluded
        assert!(!available.contains(&&Intent::SwitchToWorkspace));
        assert!(!available.contains(&&Intent::RemoveCheckout));
        assert!(!available.contains(&&Intent::CreateCheckout));
        assert!(!available.contains(&&Intent::TeleportSession));

        // Remote-executable intents should remain. CreateWorkspace is not
        // available here because the item already has a workspace.
        assert!(available.contains(&&Intent::OpenChangeRequest));
        assert!(available.contains(&&Intent::OpenIssue));
        assert!(available.contains(&&Intent::LinkIssuesToChangeRequest));
        assert!(available.contains(&&Intent::ArchiveSession));
        assert!(available.contains(&&Intent::CloseChangeRequest));
    }

    // ── resolve: CloseChangeRequest ──

    #[test]
    fn resolve_close_change_request_open_pr() {
        let mut app = stub_app();
        let repo = app.model.repo_order[0].clone();
        let rm = app.model.repos.get_mut(&repo).unwrap();
        let mut providers = ProviderData::default();
        providers.change_requests.insert("55".to_string(), flotilla_protocol::ChangeRequest {
            title: "My PR".into(),
            branch: "feat/x".into(),
            status: flotilla_protocol::ChangeRequestStatus::Open,
            body: None,
            correlation_keys: vec![],
            association_keys: vec![],
            provider_name: "github".into(),
            provider_display_name: "GitHub".into(),
        });
        rm.providers = Arc::new(providers);

        let item = pr_item("55");
        let cmd = Intent::CloseChangeRequest.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command { action: CommandAction::CloseChangeRequest { id }, .. } => assert_eq!(id, "55"),
            other => panic!("expected CloseChangeRequest, got {other:?}"),
        }
    }

    #[test]
    fn resolve_close_change_request_none_for_merged() {
        let mut app = stub_app();
        let repo = app.model.repo_order[0].clone();
        let rm = app.model.repos.get_mut(&repo).unwrap();
        let mut providers = ProviderData::default();
        providers.change_requests.insert("56".to_string(), flotilla_protocol::ChangeRequest {
            title: "Done PR".into(),
            branch: "feat/done".into(),
            status: flotilla_protocol::ChangeRequestStatus::Merged,
            body: None,
            correlation_keys: vec![],
            association_keys: vec![],
            provider_name: "github".into(),
            provider_display_name: "GitHub".into(),
        });
        rm.providers = Arc::new(providers);

        let item = pr_item("56");
        assert!(Intent::CloseChangeRequest.resolve(&item, &app).is_none());
    }
}
