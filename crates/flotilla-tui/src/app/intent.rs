use super::App;
use flotilla_protocol::{Command, RepoLabels, WorkItem, WorkItemKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    SwitchToWorkspace,
    CreateWorkspace,
    RemoveCheckout,
    CreateCheckoutAndWorkspace,
    GenerateBranchName,
    OpenChangeRequest,
    OpenIssue,
    LinkIssuesToChangeRequest,
    TeleportSession,
    ArchiveSession,
}

impl Intent {
    pub fn label(&self, labels: &RepoLabels) -> String {
        match self {
            Intent::SwitchToWorkspace => "Switch to workspace".into(),
            Intent::CreateWorkspace => "Create workspace".into(),
            Intent::RemoveCheckout => format!("Remove {}", labels.checkouts.noun),
            Intent::CreateCheckoutAndWorkspace => {
                format!("Create {} + workspace", labels.checkouts.noun)
            }
            Intent::GenerateBranchName => "Generate branch name".into(),
            Intent::OpenChangeRequest => format!("Open {} in browser", labels.code_review.noun),
            Intent::OpenIssue => "Open issue in browser".into(),
            Intent::LinkIssuesToChangeRequest => {
                format!("Link issues to {}", labels.code_review.noun)
            }
            Intent::TeleportSession => "Teleport session".into(),
            Intent::ArchiveSession => "Archive session".into(),
        }
    }

    pub fn is_available(&self, item: &WorkItem) -> bool {
        match self {
            Intent::SwitchToWorkspace => !item.workspace_refs.is_empty(),
            Intent::CreateWorkspace => {
                item.checkout_key().is_some() && item.workspace_refs.is_empty()
            }
            Intent::RemoveCheckout => item.checkout_key().is_some() && !item.is_main_checkout,
            Intent::CreateCheckoutAndWorkspace => {
                item.checkout_key().is_none() && item.branch.is_some()
            }
            Intent::GenerateBranchName => item.branch.is_none() && !item.issue_keys.is_empty(),
            Intent::OpenChangeRequest => item.change_request_key.is_some(),
            Intent::OpenIssue => !item.issue_keys.is_empty(),
            Intent::LinkIssuesToChangeRequest => {
                item.change_request_key.is_some()
                    && item.checkout_key().is_some()
                    && !item.issue_keys.is_empty()
            }
            Intent::TeleportSession => item.session_key.is_some(),
            Intent::ArchiveSession => item.session_key.is_some(),
        }
    }

    pub fn shortcut_hint(&self, labels: &RepoLabels) -> Option<String> {
        match self {
            Intent::RemoveCheckout => Some(format!("d:remove {}", labels.checkouts.noun)),
            Intent::OpenChangeRequest => {
                if labels.code_review.abbr.is_empty() {
                    Some("p:show".into())
                } else {
                    Some(format!("p:show {}", labels.code_review.abbr))
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
                item.workspace_refs
                    .first()
                    .map(|ws_ref| Command::SelectWorkspace {
                        ws_ref: ws_ref.clone(),
                    })
            }
            Intent::CreateWorkspace => {
                item.checkout_key()
                    .map(|p| Command::CreateWorkspaceForCheckout {
                        checkout_path: p.to_path_buf(),
                    })
            }
            Intent::RemoveCheckout => {
                if item.kind != WorkItemKind::Checkout || item.is_main_checkout {
                    return None;
                }
                let branch = item.branch.as_ref()?.to_string();
                let checkout_path = item.checkout_key().map(|p| p.to_path_buf());
                let change_request_id = item.change_request_key.clone();
                Some(Command::FetchCheckoutStatus {
                    branch,
                    checkout_path,
                    change_request_id,
                })
            }
            Intent::CreateCheckoutAndWorkspace => {
                item.branch.as_ref().map(|branch| Command::CreateCheckout {
                    branch: branch.to_string(),
                    create_branch: item.kind != WorkItemKind::RemoteBranch
                        && item.kind != WorkItemKind::ChangeRequest,
                    issue_ids: Vec::new(),
                })
            }
            Intent::GenerateBranchName => {
                if !item.issue_keys.is_empty() {
                    Some(Command::GenerateBranchName {
                        issue_keys: item.issue_keys.clone(),
                    })
                } else {
                    None
                }
            }
            Intent::OpenChangeRequest => item
                .change_request_key
                .as_ref()
                .map(|k| Command::OpenChangeRequest { id: k.clone() }),
            Intent::OpenIssue => item
                .issue_keys
                .first()
                .map(|k| Command::OpenIssue { id: k.clone() }),
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
                Some(Command::LinkIssuesToChangeRequest {
                    change_request_id: change_request_key.clone(),
                    issue_ids: missing,
                })
            }
            Intent::TeleportSession => {
                item.session_key.as_ref().map(|k| Command::TeleportSession {
                    session_id: k.clone(),
                    branch: item.branch.clone(),
                    checkout_key: item.checkout_key().map(|p| p.to_path_buf()),
                })
            }
            Intent::ArchiveSession => item.session_key.as_ref().map(|k| Command::ArchiveSession {
                session_id: k.clone(),
            }),
        }
    }

    pub fn all_in_menu_order() -> &'static [Intent] {
        &[
            Intent::SwitchToWorkspace,
            Intent::CreateWorkspace,
            Intent::RemoveCheckout,
            Intent::CreateCheckoutAndWorkspace,
            Intent::GenerateBranchName,
            Intent::OpenChangeRequest,
            Intent::OpenIssue,
            Intent::LinkIssuesToChangeRequest,
            Intent::TeleportSession,
            Intent::ArchiveSession,
        ]
    }

    pub fn enter_priority() -> &'static [Intent] {
        &[
            Intent::SwitchToWorkspace,
            Intent::TeleportSession,
            Intent::CreateWorkspace,
            Intent::CreateCheckoutAndWorkspace,
            Intent::GenerateBranchName,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{
        bare_item, checkout_item, pr_item, remote_branch_item, session_item, stub_app,
    };
    use flotilla_protocol::{CategoryLabels, CheckoutRef, RepoLabels};
    use std::path::PathBuf;

    // ── Helpers ──

    fn default_labels() -> RepoLabels {
        RepoLabels::default()
    }

    fn custom_labels() -> RepoLabels {
        RepoLabels {
            checkouts: CategoryLabels {
                section: "Worktrees".into(),
                noun: "worktree".into(),
                abbr: "wt".into(),
            },
            code_review: CategoryLabels {
                section: "Pull Requests".into(),
                noun: "PR".into(),
                abbr: "pr".into(),
            },
            issues: CategoryLabels {
                section: "Issues".into(),
                noun: "issue".into(),
                abbr: "iss".into(),
            },
            sessions: CategoryLabels {
                section: "Sessions".into(),
                noun: "session".into(),
                abbr: "sess".into(),
            },
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
        assert!(Intent::CreateCheckoutAndWorkspace.is_available(&item));

        // Has checkout -> not available
        let co_item = checkout_item("feat/x", "/tmp/feat-x", false);
        assert!(!Intent::CreateCheckoutAndWorkspace.is_available(&co_item));

        // No branch -> not available
        let no_branch = bare_item();
        assert!(!Intent::CreateCheckoutAndWorkspace.is_available(&no_branch));
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
        assert_eq!(
            Intent::SwitchToWorkspace.label(&labels),
            "Switch to workspace"
        );
        assert_eq!(Intent::CreateWorkspace.label(&labels), "Create workspace");
        assert_eq!(Intent::RemoveCheckout.label(&labels), "Remove item");
        assert_eq!(
            Intent::CreateCheckoutAndWorkspace.label(&labels),
            "Create item + workspace"
        );
        assert_eq!(
            Intent::GenerateBranchName.label(&labels),
            "Generate branch name"
        );
        assert_eq!(
            Intent::OpenChangeRequest.label(&labels),
            "Open item in browser"
        );
        assert_eq!(Intent::OpenIssue.label(&labels), "Open issue in browser");
        assert_eq!(
            Intent::LinkIssuesToChangeRequest.label(&labels),
            "Link issues to item"
        );
        assert_eq!(Intent::TeleportSession.label(&labels), "Teleport session");
        assert_eq!(Intent::ArchiveSession.label(&labels), "Archive session");
    }

    #[test]
    fn label_with_custom_labels() {
        let labels = custom_labels();
        assert_eq!(Intent::RemoveCheckout.label(&labels), "Remove worktree");
        assert_eq!(
            Intent::CreateCheckoutAndWorkspace.label(&labels),
            "Create worktree + workspace"
        );
        assert_eq!(
            Intent::OpenChangeRequest.label(&labels),
            "Open PR in browser"
        );
        assert_eq!(
            Intent::LinkIssuesToChangeRequest.label(&labels),
            "Link issues to PR"
        );
    }

    #[test]
    fn label_static_strings_unaffected_by_labels() {
        let labels = custom_labels();
        assert_eq!(
            Intent::SwitchToWorkspace.label(&labels),
            "Switch to workspace"
        );
        assert_eq!(Intent::CreateWorkspace.label(&labels), "Create workspace");
        assert_eq!(
            Intent::GenerateBranchName.label(&labels),
            "Generate branch name"
        );
        assert_eq!(Intent::OpenIssue.label(&labels), "Open issue in browser");
        assert_eq!(Intent::TeleportSession.label(&labels), "Teleport session");
        assert_eq!(Intent::ArchiveSession.label(&labels), "Archive session");
    }

    // ── shortcut_hint tests ──

    #[test]
    fn shortcut_hint_remove_worktree() {
        let labels = default_labels();
        assert_eq!(
            Intent::RemoveCheckout.shortcut_hint(&labels),
            Some("d:remove item".into())
        );
    }

    #[test]
    fn shortcut_hint_open_pr() {
        let labels = default_labels();
        assert_eq!(
            Intent::OpenChangeRequest.shortcut_hint(&labels),
            Some("p:show".into())
        );
    }

    #[test]
    fn shortcut_hint_with_custom_labels() {
        let labels = custom_labels();
        assert_eq!(
            Intent::RemoveCheckout.shortcut_hint(&labels),
            Some("d:remove worktree".into())
        );
        assert_eq!(
            Intent::OpenChangeRequest.shortcut_hint(&labels),
            Some("p:show pr".into())
        );
    }

    #[test]
    fn shortcut_hint_none_for_other_intents() {
        let labels = default_labels();
        assert!(Intent::SwitchToWorkspace.shortcut_hint(&labels).is_none());
        assert!(Intent::CreateWorkspace.shortcut_hint(&labels).is_none());
        assert!(Intent::CreateCheckoutAndWorkspace
            .shortcut_hint(&labels)
            .is_none());
        assert!(Intent::GenerateBranchName.shortcut_hint(&labels).is_none());
        assert!(Intent::OpenIssue.shortcut_hint(&labels).is_none());
        assert!(Intent::LinkIssuesToChangeRequest
            .shortcut_hint(&labels)
            .is_none());
        assert!(Intent::TeleportSession.shortcut_hint(&labels).is_none());
        assert!(Intent::ArchiveSession.shortcut_hint(&labels).is_none());
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
            Intent::CreateCheckoutAndWorkspace,
            Intent::GenerateBranchName,
            Intent::OpenChangeRequest,
            Intent::OpenIssue,
            Intent::LinkIssuesToChangeRequest,
            Intent::TeleportSession,
            Intent::ArchiveSession,
        ]
        .into_iter()
        .map(|v| match v {
            Intent::SwitchToWorkspace => v,
            Intent::CreateWorkspace => v,
            Intent::RemoveCheckout => v,
            Intent::CreateCheckoutAndWorkspace => v,
            Intent::GenerateBranchName => v,
            Intent::OpenChangeRequest => v,
            Intent::OpenIssue => v,
            Intent::LinkIssuesToChangeRequest => v,
            Intent::TeleportSession => v,
            Intent::ArchiveSession => v,
        })
        .collect()
    }

    #[test]
    fn all_in_menu_order_contains_every_variant() {
        let all_variants = all_intent_variants();
        let menu = Intent::all_in_menu_order();
        for variant in &all_variants {
            assert!(
                menu.contains(variant),
                "{variant:?} missing from all_in_menu_order()"
            );
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
            Intent::CreateCheckoutAndWorkspace,
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
    use std::sync::Arc;

    #[test]
    fn resolve_switch_to_workspace() {
        let app = stub_app();
        let mut item = bare_item();
        item.workspace_refs = vec!["ws-abc".into()];
        let cmd = Intent::SwitchToWorkspace.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::SelectWorkspace { ws_ref } => assert_eq!(ws_ref, "ws-abc"),
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
            Command::SelectWorkspace { ws_ref } => assert_eq!(ws_ref, "ws-first"),
            other => panic!("expected SelectWorkspace, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_workspace() {
        let app = stub_app();
        let item = checkout_item("feat/x", "/tmp/feat-x", false);
        let cmd = Intent::CreateWorkspace.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::CreateWorkspaceForCheckout { checkout_path } => {
                assert_eq!(checkout_path, PathBuf::from("/tmp/feat-x"))
            }
            other => panic!("expected CreateWorkspaceForCheckout, got {other:?}"),
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
            Command::FetchCheckoutStatus {
                branch,
                checkout_path,
                change_request_id,
            } => {
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
        item.checkout = Some(CheckoutRef {
            key: PathBuf::from("/tmp/pr-co"),
            is_main_checkout: false,
        });
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
            Command::FetchCheckoutStatus {
                change_request_id, ..
            } => {
                assert_eq!(change_request_id, None);
            }
            other => panic!("expected FetchCheckoutStatus, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_worktree_and_workspace_remote_branch() {
        let app = stub_app();
        let item = remote_branch_item("feat/remote");
        let cmd = Intent::CreateCheckoutAndWorkspace.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::CreateCheckout {
                branch,
                create_branch,
                issue_ids,
            } => {
                assert_eq!(branch, "feat/remote");
                // RemoteBranch kind -> create_branch = false
                assert!(!create_branch);
                assert!(issue_ids.is_empty());
            }
            other => panic!("expected CreateCheckout, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_worktree_and_workspace_pr_item() {
        let app = stub_app();
        let item = pr_item("42");
        let cmd = Intent::CreateCheckoutAndWorkspace.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::CreateCheckout {
                branch,
                create_branch,
                ..
            } => {
                assert_eq!(branch, "feat/pr-branch");
                // Pr kind -> create_branch = false
                assert!(!create_branch);
            }
            other => panic!("expected CreateCheckout, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_worktree_and_workspace_session_item() {
        let app = stub_app();
        let item = session_item("sess-1");
        let cmd = Intent::CreateCheckoutAndWorkspace.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::CreateCheckout {
                branch,
                create_branch,
                ..
            } => {
                assert_eq!(branch, "feat/session-branch");
                // Session kind -> create_branch = true (not RemoteBranch or Pr)
                assert!(create_branch);
            }
            other => panic!("expected CreateCheckout, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_worktree_and_workspace_none_without_branch() {
        let app = stub_app();
        let item = bare_item(); // no branch
        assert!(Intent::CreateCheckoutAndWorkspace
            .resolve(&item, &app)
            .is_none());
    }

    #[test]
    fn resolve_generate_branch_name_with_issues() {
        let app = stub_app();
        let mut item = bare_item();
        item.issue_keys = vec!["42".into(), "43".into()];
        let cmd = Intent::GenerateBranchName.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::GenerateBranchName { issue_keys } => {
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
        let app = stub_app();
        let item = pr_item("123");
        let cmd = Intent::OpenChangeRequest.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::OpenChangeRequest { id } => assert_eq!(id, "123"),
            other => panic!("expected OpenChangeRequest, got {other:?}"),
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
            Command::OpenIssue { id } => {
                // Opens the first issue
                assert_eq!(id, "7");
            }
            other => panic!("expected OpenIssue, got {other:?}"),
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
        item.checkout = Some(CheckoutRef {
            key: PathBuf::from("/tmp/co"),
            is_main_checkout: false,
        });
        let cmd = Intent::TeleportSession.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::TeleportSession {
                session_id,
                branch,
                checkout_key,
            } => {
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
            Command::TeleportSession {
                checkout_key,
                branch,
                ..
            } => {
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
            Command::ArchiveSession { session_id } => assert_eq!(session_id, "sess-99"),
            other => panic!("expected ArchiveSession, got {other:?}"),
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
        use flotilla_protocol::{
            AssociationKey, ChangeRequest, ChangeRequestStatus, Checkout, CorrelationKey,
        };

        let mut app = stub_app();

        let mut providers = ProviderData::default();
        providers.change_requests.insert(
            "42".into(),
            ChangeRequest {
                title: "Fix bug".into(),
                branch: "feat/x".into(),
                status: ChangeRequestStatus::Open,
                body: None,
                correlation_keys: vec![],
                // PR already has issue "10" linked
                association_keys: vec![AssociationKey::IssueRef("gh".into(), "10".into())],
                provider_name: String::new(),
            },
        );
        let co_path = PathBuf::from("/tmp/feat-x");
        providers.checkouts.insert(
            co_path.clone(),
            Checkout {
                branch: "feat/x".into(),
                is_trunk: false,
                trunk_ahead_behind: None,
                remote_ahead_behind: None,
                working_tree: None,
                last_commit: None,
                correlation_keys: vec![CorrelationKey::CheckoutPath(co_path.clone())],
                association_keys: checkout_issue_ids
                    .iter()
                    .map(|id| AssociationKey::IssueRef("gh".into(), (*id).into()))
                    .collect(),
            },
        );

        app.model
            .repos
            .get_mut(&PathBuf::from("/tmp/test-repo"))
            .unwrap()
            .providers = Arc::new(providers);

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
            Command::LinkIssuesToChangeRequest {
                change_request_id,
                issue_ids,
            } => {
                assert_eq!(change_request_id, "42");
                // Only issue "20" should be linked ("10" is already on the PR)
                assert_eq!(issue_ids, vec!["20".to_string()]);
            }
            other => panic!("expected LinkIssuesToChangeRequest, got {other:?}"),
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

    // ── Combined availability scenario ──

    #[test]
    fn rich_item_has_multiple_intents_available() {
        // A checkout with PR, session, issues, and workspace
        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.change_request_key = Some("42".into());
        item.session_key = Some("sess-1".into());
        item.issue_keys = vec!["7".into()];
        item.workspace_refs = vec!["ws-1".into()];

        let available: Vec<_> = Intent::all_in_menu_order()
            .iter()
            .filter(|i| i.is_available(&item))
            .collect();

        assert!(available.contains(&&Intent::SwitchToWorkspace));
        assert!(available.contains(&&Intent::OpenChangeRequest));
        assert!(available.contains(&&Intent::OpenIssue));
        assert!(available.contains(&&Intent::LinkIssuesToChangeRequest));
        assert!(available.contains(&&Intent::TeleportSession));
        assert!(available.contains(&&Intent::ArchiveSession));
        assert!(available.contains(&&Intent::RemoveCheckout));

        // These should NOT be available
        assert!(!available.contains(&&Intent::CreateWorkspace)); // has workspace
        assert!(!available.contains(&&Intent::CreateCheckoutAndWorkspace)); // has checkout
        assert!(!available.contains(&&Intent::GenerateBranchName)); // has branch
    }

    #[test]
    fn bare_item_has_no_intents_available() {
        let item = bare_item();
        let available: Vec<_> = Intent::all_in_menu_order()
            .iter()
            .filter(|i| i.is_available(&item))
            .collect();
        assert!(
            available.is_empty(),
            "bare item should have no intents, got {available:?}"
        );
    }
}
