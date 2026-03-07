use super::App;
use flotilla_protocol::{Command, RepoLabels, WorkItem, WorkItemKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    SwitchToWorkspace,
    CreateWorkspace,
    RemoveWorktree,
    CreateWorktreeAndWorkspace,
    GenerateBranchName,
    OpenPr,
    OpenIssue,
    LinkIssuesToPr,
    TeleportSession,
    ArchiveSession,
}

impl Intent {
    pub fn label(&self, labels: &RepoLabels) -> String {
        match self {
            Intent::SwitchToWorkspace => "Switch to workspace".into(),
            Intent::CreateWorkspace => "Create workspace".into(),
            Intent::RemoveWorktree => format!("Remove {}", labels.checkouts.noun),
            Intent::CreateWorktreeAndWorkspace => {
                format!("Create {} + workspace", labels.checkouts.noun)
            }
            Intent::GenerateBranchName => "Generate branch name".into(),
            Intent::OpenPr => format!("Open {} in browser", labels.code_review.noun),
            Intent::OpenIssue => "Open issue in browser".into(),
            Intent::LinkIssuesToPr => format!("Link issues to {}", labels.code_review.noun),
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
            Intent::RemoveWorktree => item.checkout_key().is_some() && !item.is_main_worktree,
            Intent::CreateWorktreeAndWorkspace => {
                item.checkout_key().is_none() && item.branch.is_some()
            }
            Intent::GenerateBranchName => item.branch.is_none() && !item.issue_keys.is_empty(),
            Intent::OpenPr => item.pr_key.is_some(),
            Intent::OpenIssue => !item.issue_keys.is_empty(),
            Intent::LinkIssuesToPr => {
                item.pr_key.is_some()
                    && item.checkout_key().is_some()
                    && !item.issue_keys.is_empty()
            }
            Intent::TeleportSession => item.session_key.is_some(),
            Intent::ArchiveSession => item.session_key.is_some(),
        }
    }

    pub fn shortcut_hint(&self, labels: &RepoLabels) -> Option<String> {
        match self {
            Intent::RemoveWorktree => Some(format!("d:remove {}", labels.checkouts.noun)),
            Intent::OpenPr => Some(format!("p:show {}", labels.code_review.abbr)),
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
            Intent::CreateWorkspace => item.checkout_key().map(|p| Command::SwitchWorktree {
                path: p.to_path_buf(),
            }),
            Intent::RemoveWorktree => {
                if item.kind != WorkItemKind::Checkout || item.is_main_worktree {
                    return None;
                }
                let branch = item.branch.as_ref()?.to_string();
                let worktree_path = item.checkout_key().map(|p| p.to_path_buf());
                let pr_number = item.pr_key.clone();
                Some(Command::FetchDeleteInfo {
                    branch,
                    worktree_path,
                    pr_number,
                })
            }
            Intent::CreateWorktreeAndWorkspace => {
                item.branch.as_ref().map(|branch| Command::CreateWorktree {
                    branch: branch.to_string(),
                    create_branch: item.kind != WorkItemKind::RemoteBranch
                        && item.kind != WorkItemKind::Pr,
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
            Intent::OpenPr => item
                .pr_key
                .as_ref()
                .map(|k| Command::OpenPr { id: k.clone() }),
            Intent::OpenIssue => item
                .issue_keys
                .first()
                .map(|k| Command::OpenIssueBrowser { id: k.clone() }),
            Intent::LinkIssuesToPr => {
                let pr_key = item.pr_key.as_ref()?;
                let co_key = item.checkout_key()?;
                let providers = &app.model.active().providers;
                let cr = providers.change_requests.get(pr_key.as_str())?;
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
                Some(Command::LinkIssuesToPr {
                    pr_id: cr.id.clone(),
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
            Intent::RemoveWorktree,
            Intent::CreateWorktreeAndWorkspace,
            Intent::GenerateBranchName,
            Intent::OpenPr,
            Intent::OpenIssue,
            Intent::LinkIssuesToPr,
            Intent::TeleportSession,
            Intent::ArchiveSession,
        ]
    }

    pub fn enter_priority() -> &'static [Intent] {
        &[
            Intent::SwitchToWorkspace,
            Intent::TeleportSession,
            Intent::CreateWorkspace,
            Intent::CreateWorktreeAndWorkspace,
            Intent::GenerateBranchName,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_protocol::{
        CategoryLabels, CheckoutRef, RepoLabels, WorkItem, WorkItemIdentity, WorkItemKind,
    };
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

    /// Bare work item with no associated data — standalone issue-like item.
    fn bare_item() -> WorkItem {
        WorkItem {
            kind: WorkItemKind::Issue,
            identity: WorkItemIdentity::Issue("1".into()),
            branch: None,
            description: String::new(),
            checkout: None,
            pr_key: None,
            session_key: None,
            issue_keys: Vec::new(),
            workspace_refs: Vec::new(),
            is_main_worktree: false,
            debug_group: Vec::new(),
        }
    }

    /// A checkout work item with a branch and checkout path.
    fn checkout_item(branch: &str, path: &str, is_main: bool) -> WorkItem {
        WorkItem {
            kind: WorkItemKind::Checkout,
            identity: WorkItemIdentity::Checkout(PathBuf::from(path)),
            branch: Some(branch.into()),
            description: format!("checkout {branch}"),
            checkout: Some(CheckoutRef {
                key: PathBuf::from(path),
                is_main_worktree: is_main,
            }),
            pr_key: None,
            session_key: None,
            issue_keys: Vec::new(),
            workspace_refs: Vec::new(),
            is_main_worktree: is_main,
            debug_group: Vec::new(),
        }
    }

    /// A PR work item with optional checkout.
    fn pr_item(pr_id: &str) -> WorkItem {
        WorkItem {
            kind: WorkItemKind::Pr,
            identity: WorkItemIdentity::ChangeRequest(pr_id.into()),
            branch: Some("feat/pr-branch".into()),
            description: format!("PR #{pr_id}"),
            checkout: None,
            pr_key: Some(pr_id.into()),
            session_key: None,
            issue_keys: Vec::new(),
            workspace_refs: Vec::new(),
            is_main_worktree: false,
            debug_group: Vec::new(),
        }
    }

    /// A session work item.
    fn session_item(session_id: &str) -> WorkItem {
        WorkItem {
            kind: WorkItemKind::Session,
            identity: WorkItemIdentity::Session(session_id.into()),
            branch: Some("feat/session-branch".into()),
            description: format!("session {session_id}"),
            checkout: None,
            pr_key: None,
            session_key: Some(session_id.into()),
            issue_keys: Vec::new(),
            workspace_refs: Vec::new(),
            is_main_worktree: false,
            debug_group: Vec::new(),
        }
    }

    /// A remote-branch work item (no checkout, has branch).
    fn remote_branch_item(branch: &str) -> WorkItem {
        WorkItem {
            kind: WorkItemKind::RemoteBranch,
            identity: WorkItemIdentity::RemoteBranch(branch.into()),
            branch: Some(branch.into()),
            description: format!("remote {branch}"),
            checkout: None,
            pr_key: None,
            session_key: None,
            issue_keys: Vec::new(),
            workspace_refs: Vec::new(),
            is_main_worktree: false,
            debug_group: Vec::new(),
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
        assert!(Intent::RemoveWorktree.is_available(&item));

        // Main worktree -> not available
        let main_item = checkout_item("main", "/tmp/main", true);
        assert!(!Intent::RemoveWorktree.is_available(&main_item));

        // No checkout -> not available
        let no_co = bare_item();
        assert!(!Intent::RemoveWorktree.is_available(&no_co));
    }

    #[test]
    fn create_worktree_and_workspace_needs_no_checkout_and_has_branch() {
        // Remote branch: no checkout, has branch -> available
        let item = remote_branch_item("feat/remote");
        assert!(Intent::CreateWorktreeAndWorkspace.is_available(&item));

        // Has checkout -> not available
        let co_item = checkout_item("feat/x", "/tmp/feat-x", false);
        assert!(!Intent::CreateWorktreeAndWorkspace.is_available(&co_item));

        // No branch -> not available
        let no_branch = bare_item();
        assert!(!Intent::CreateWorktreeAndWorkspace.is_available(&no_branch));
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
    fn open_pr_needs_pr_key() {
        let pr = pr_item("123");
        assert!(Intent::OpenPr.is_available(&pr));

        let no_pr = bare_item();
        assert!(!Intent::OpenPr.is_available(&no_pr));
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
        item.pr_key = Some("42".into());
        item.issue_keys = vec!["7".into()];
        assert!(Intent::LinkIssuesToPr.is_available(&item));

        // Missing PR -> not available
        let mut no_pr = item.clone();
        no_pr.pr_key = None;
        assert!(!Intent::LinkIssuesToPr.is_available(&no_pr));

        // Missing checkout -> not available
        let mut no_co = item.clone();
        no_co.checkout = None;
        assert!(!Intent::LinkIssuesToPr.is_available(&no_co));

        // Missing issues -> not available
        let mut no_issues = item.clone();
        no_issues.issue_keys = Vec::new();
        assert!(!Intent::LinkIssuesToPr.is_available(&no_issues));
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
        assert_eq!(Intent::RemoveWorktree.label(&labels), "Remove item");
        assert_eq!(
            Intent::CreateWorktreeAndWorkspace.label(&labels),
            "Create item + workspace"
        );
        assert_eq!(
            Intent::GenerateBranchName.label(&labels),
            "Generate branch name"
        );
        assert_eq!(Intent::OpenPr.label(&labels), "Open item in browser");
        assert_eq!(Intent::OpenIssue.label(&labels), "Open issue in browser");
        assert_eq!(Intent::LinkIssuesToPr.label(&labels), "Link issues to item");
        assert_eq!(Intent::TeleportSession.label(&labels), "Teleport session");
        assert_eq!(Intent::ArchiveSession.label(&labels), "Archive session");
    }

    #[test]
    fn label_with_custom_labels() {
        let labels = custom_labels();
        assert_eq!(Intent::RemoveWorktree.label(&labels), "Remove worktree");
        assert_eq!(
            Intent::CreateWorktreeAndWorkspace.label(&labels),
            "Create worktree + workspace"
        );
        assert_eq!(Intent::OpenPr.label(&labels), "Open PR in browser");
        assert_eq!(Intent::LinkIssuesToPr.label(&labels), "Link issues to PR");
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
            Intent::RemoveWorktree.shortcut_hint(&labels),
            Some("d:remove item".into())
        );
    }

    #[test]
    fn shortcut_hint_open_pr() {
        let labels = default_labels();
        assert_eq!(
            Intent::OpenPr.shortcut_hint(&labels),
            Some("p:show ".into())
        );
    }

    #[test]
    fn shortcut_hint_with_custom_labels() {
        let labels = custom_labels();
        assert_eq!(
            Intent::RemoveWorktree.shortcut_hint(&labels),
            Some("d:remove worktree".into())
        );
        assert_eq!(
            Intent::OpenPr.shortcut_hint(&labels),
            Some("p:show pr".into())
        );
    }

    #[test]
    fn shortcut_hint_none_for_other_intents() {
        let labels = default_labels();
        assert!(Intent::SwitchToWorkspace.shortcut_hint(&labels).is_none());
        assert!(Intent::CreateWorkspace.shortcut_hint(&labels).is_none());
        assert!(Intent::CreateWorktreeAndWorkspace
            .shortcut_hint(&labels)
            .is_none());
        assert!(Intent::GenerateBranchName.shortcut_hint(&labels).is_none());
        assert!(Intent::OpenIssue.shortcut_hint(&labels).is_none());
        assert!(Intent::LinkIssuesToPr.shortcut_hint(&labels).is_none());
        assert!(Intent::TeleportSession.shortcut_hint(&labels).is_none());
        assert!(Intent::ArchiveSession.shortcut_hint(&labels).is_none());
    }

    // ── all_in_menu_order tests ──

    #[test]
    fn all_in_menu_order_matches_expected_sequence() {
        assert_eq!(
            Intent::all_in_menu_order(),
            &[
                Intent::SwitchToWorkspace,
                Intent::CreateWorkspace,
                Intent::RemoveWorktree,
                Intent::CreateWorktreeAndWorkspace,
                Intent::GenerateBranchName,
                Intent::OpenPr,
                Intent::OpenIssue,
                Intent::LinkIssuesToPr,
                Intent::TeleportSession,
                Intent::ArchiveSession,
            ]
        );
    }

    // ── enter_priority tests ──

    #[test]
    fn enter_priority_matches_expected_sequence_and_is_subset() {
        let expected = [
            Intent::SwitchToWorkspace,
            Intent::TeleportSession,
            Intent::CreateWorkspace,
            Intent::CreateWorktreeAndWorkspace,
            Intent::GenerateBranchName,
        ];
        assert_eq!(Intent::enter_priority(), &expected);
        for intent in expected {
            assert!(Intent::all_in_menu_order().contains(&intent));
        }
    }

    // ── resolve tests ──
    //
    // resolve() requires &App which needs a DaemonHandle trait object. Since
    // async_trait is not a direct dependency of flotilla-tui, we build a stub
    // using manual Pin<Box<dyn Future>> desugaring to match the #[async_trait]
    // expansion. Most resolve() variants only read from the WorkItem and ignore
    // App entirely; only LinkIssuesToPr reads app.model.active().providers.

    use flotilla_core::daemon::DaemonHandle;
    use flotilla_protocol::{CommandResult, DaemonEvent, ProviderData, RepoInfo, Snapshot};
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    struct StubDaemon {
        tx: broadcast::Sender<DaemonEvent>,
    }

    impl StubDaemon {
        fn new() -> Self {
            let (tx, _) = broadcast::channel(1);
            Self { tx }
        }
    }

    impl DaemonHandle for StubDaemon {
        fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
            self.tx.subscribe()
        }

        fn get_state<'life0, 'life1, 'async_trait>(
            &'life0 self,
            _repo: &'life1 Path,
        ) -> Pin<
            Box<dyn std::future::Future<Output = Result<Snapshot, String>> + Send + 'async_trait>,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async { Err("stub".into()) })
        }

        fn list_repos<'life0, 'async_trait>(
            &'life0 self,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<Vec<RepoInfo>, String>>
                    + Send
                    + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async { Ok(vec![]) })
        }

        fn execute<'life0, 'life1, 'async_trait>(
            &'life0 self,
            _repo: &'life1 Path,
            _command: Command,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<CommandResult, String>>
                    + Send
                    + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async { Ok(CommandResult::Ok) })
        }

        fn refresh<'life0, 'life1, 'async_trait>(
            &'life0 self,
            _repo: &'life1 Path,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async { Ok(()) })
        }

        fn add_repo<'life0, 'life1, 'async_trait>(
            &'life0 self,
            _path: &'life1 Path,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async { Ok(()) })
        }

        fn remove_repo<'life0, 'life1, 'async_trait>(
            &'life0 self,
            _path: &'life1 Path,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async { Ok(()) })
        }
    }

    fn stub_app() -> App {
        let daemon: Arc<dyn DaemonHandle> = Arc::new(StubDaemon::new());
        let repo_path = PathBuf::from("/tmp/test-repo");
        let repos_info = vec![RepoInfo {
            path: repo_path,
            name: "test-repo".into(),
            labels: default_labels(),
            provider_names: std::collections::HashMap::new(),
            provider_health: std::collections::HashMap::new(),
            loading: false,
        }];
        App::new(daemon, repos_info)
    }

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
            Command::SwitchWorktree { path } => {
                assert_eq!(path, PathBuf::from("/tmp/feat-x"))
            }
            other => panic!("expected SwitchWorktree, got {other:?}"),
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
        item.pr_key = Some("99".into());
        let cmd = Intent::RemoveWorktree.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::FetchDeleteInfo {
                branch,
                worktree_path,
                pr_number,
            } => {
                assert_eq!(branch, "feat/x");
                assert_eq!(worktree_path, Some(PathBuf::from("/tmp/feat-x")));
                assert_eq!(pr_number, Some("99".into()));
            }
            other => panic!("expected FetchDeleteInfo, got {other:?}"),
        }
    }

    #[test]
    fn resolve_remove_worktree_none_for_main() {
        let app = stub_app();
        let item = checkout_item("main", "/tmp/main", true);
        assert!(Intent::RemoveWorktree.resolve(&item, &app).is_none());
    }

    #[test]
    fn resolve_remove_worktree_none_for_non_checkout_kind() {
        let app = stub_app();
        let mut item = pr_item("42");
        // Even with a checkout path, the kind check prevents resolve
        item.checkout = Some(CheckoutRef {
            key: PathBuf::from("/tmp/pr-co"),
            is_main_worktree: false,
        });
        assert!(Intent::RemoveWorktree.resolve(&item, &app).is_none());
    }

    #[test]
    fn resolve_remove_worktree_none_without_branch() {
        let app = stub_app();
        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.branch = None; // branch required for FetchDeleteInfo
        assert!(Intent::RemoveWorktree.resolve(&item, &app).is_none());
    }

    #[test]
    fn resolve_remove_worktree_without_pr_key() {
        let app = stub_app();
        let item = checkout_item("feat/x", "/tmp/feat-x", false);
        let cmd = Intent::RemoveWorktree.resolve(&item, &app).unwrap();
        match cmd {
            Command::FetchDeleteInfo { pr_number, .. } => {
                assert_eq!(pr_number, None);
            }
            other => panic!("expected FetchDeleteInfo, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_worktree_and_workspace_remote_branch() {
        let app = stub_app();
        let item = remote_branch_item("feat/remote");
        let cmd = Intent::CreateWorktreeAndWorkspace.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::CreateWorktree {
                branch,
                create_branch,
                issue_ids,
            } => {
                assert_eq!(branch, "feat/remote");
                // RemoteBranch kind -> create_branch = false
                assert!(!create_branch);
                assert!(issue_ids.is_empty());
            }
            other => panic!("expected CreateWorktree, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_worktree_and_workspace_pr_item() {
        let app = stub_app();
        let item = pr_item("42");
        let cmd = Intent::CreateWorktreeAndWorkspace.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::CreateWorktree {
                branch,
                create_branch,
                ..
            } => {
                assert_eq!(branch, "feat/pr-branch");
                // Pr kind -> create_branch = false
                assert!(!create_branch);
            }
            other => panic!("expected CreateWorktree, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_worktree_and_workspace_session_item() {
        let app = stub_app();
        let item = session_item("sess-1");
        let cmd = Intent::CreateWorktreeAndWorkspace.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::CreateWorktree {
                branch,
                create_branch,
                ..
            } => {
                assert_eq!(branch, "feat/session-branch");
                // Session kind -> create_branch = true (not RemoteBranch or Pr)
                assert!(create_branch);
            }
            other => panic!("expected CreateWorktree, got {other:?}"),
        }
    }

    #[test]
    fn resolve_create_worktree_and_workspace_none_without_branch() {
        let app = stub_app();
        let item = bare_item(); // no branch
        assert!(Intent::CreateWorktreeAndWorkspace
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
        let cmd = Intent::OpenPr.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::OpenPr { id } => assert_eq!(id, "123"),
            other => panic!("expected OpenPr, got {other:?}"),
        }
    }

    #[test]
    fn resolve_open_pr_none_without_pr_key() {
        let app = stub_app();
        let item = bare_item();
        assert!(Intent::OpenPr.resolve(&item, &app).is_none());
    }

    #[test]
    fn resolve_open_issue() {
        let app = stub_app();
        let mut item = bare_item();
        item.issue_keys = vec!["7".into(), "8".into()];
        let cmd = Intent::OpenIssue.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::OpenIssueBrowser { id } => {
                // Opens the first issue
                assert_eq!(id, "7");
            }
            other => panic!("expected OpenIssueBrowser, got {other:?}"),
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
            is_main_worktree: false,
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

        let daemon: Arc<dyn DaemonHandle> = Arc::new(StubDaemon::new());
        let repo_path = PathBuf::from("/tmp/test-repo");
        let repos_info = vec![RepoInfo {
            path: repo_path.clone(),
            name: "test-repo".into(),
            labels: default_labels(),
            provider_names: std::collections::HashMap::new(),
            provider_health: std::collections::HashMap::new(),
            loading: false,
        }];
        let mut app = App::new(daemon, repos_info);

        let mut providers = ProviderData::default();
        providers.change_requests.insert(
            "42".into(),
            ChangeRequest {
                id: "42".into(),
                title: "Fix bug".into(),
                branch: "feat/x".into(),
                status: ChangeRequestStatus::Open,
                body: None,
                correlation_keys: vec![],
                // PR already has issue "10" linked
                association_keys: vec![AssociationKey::IssueRef("gh".into(), "10".into())],
            },
        );
        let co_path = PathBuf::from("/tmp/feat-x");
        providers.checkouts.insert(
            co_path.clone(),
            Checkout {
                branch: "feat/x".into(),
                path: co_path.clone(),
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
        item.pr_key = Some("42".into());
        item.issue_keys = vec!["7".into()];
        let cmd = Intent::LinkIssuesToPr.resolve(&item, &app);
        assert!(cmd.is_none());
    }

    #[test]
    fn resolve_link_issues_to_pr_with_provider_data() {
        // Checkout has issues "10" (already on PR) and "20" (missing)
        let app = app_with_pr_and_issues(&["10", "20"]);

        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.pr_key = Some("42".into());
        item.issue_keys = vec!["10".into(), "20".into()];

        let cmd = Intent::LinkIssuesToPr.resolve(&item, &app);
        assert!(cmd.is_some());
        match cmd.unwrap() {
            Command::LinkIssuesToPr { pr_id, issue_ids } => {
                assert_eq!(pr_id, "42");
                // Only issue "20" should be linked ("10" is already on the PR)
                assert_eq!(issue_ids, vec!["20".to_string()]);
            }
            other => panic!("expected LinkIssuesToPr, got {other:?}"),
        }
    }

    #[test]
    fn resolve_link_issues_to_pr_none_when_all_issues_already_linked() {
        // Checkout has only issue "10", which is already on the PR
        let app = app_with_pr_and_issues(&["10"]);

        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.pr_key = Some("42".into());
        item.issue_keys = vec!["10".into()];

        // All issues already linked -> returns None
        let cmd = Intent::LinkIssuesToPr.resolve(&item, &app);
        assert!(cmd.is_none());
    }

    // ── Combined availability scenario ──

    #[test]
    fn rich_item_has_multiple_intents_available() {
        // A checkout with PR, session, issues, and workspace
        let mut item = checkout_item("feat/x", "/tmp/feat-x", false);
        item.pr_key = Some("42".into());
        item.session_key = Some("sess-1".into());
        item.issue_keys = vec!["7".into()];
        item.workspace_refs = vec!["ws-1".into()];

        let available: Vec<_> = Intent::all_in_menu_order()
            .iter()
            .filter(|i| i.is_available(&item))
            .collect();

        assert!(available.contains(&&Intent::SwitchToWorkspace));
        assert!(available.contains(&&Intent::OpenPr));
        assert!(available.contains(&&Intent::OpenIssue));
        assert!(available.contains(&&Intent::LinkIssuesToPr));
        assert!(available.contains(&&Intent::TeleportSession));
        assert!(available.contains(&&Intent::ArchiveSession));
        assert!(available.contains(&&Intent::RemoveWorktree));

        // These should NOT be available
        assert!(!available.contains(&&Intent::CreateWorkspace)); // has workspace
        assert!(!available.contains(&&Intent::CreateWorktreeAndWorkspace)); // has checkout
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
