use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use flotilla_core::config::ConfigStore;
use flotilla_core::daemon::DaemonHandle;
use flotilla_core::data::{GroupEntry, GroupedWorkItems};
use flotilla_protocol::{
    CheckoutRef, Command, DaemonEvent, RepoInfo, RepoLabels, Snapshot, WorkItem, WorkItemIdentity,
    WorkItemKind,
};
use tokio::sync::broadcast;
use tui_input::Input;

use super::{App, DirEntry, TuiRepoModel, UiMode};

struct StubDaemon {
    tx: broadcast::Sender<DaemonEvent>,
}

impl StubDaemon {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(1);
        Self { tx }
    }
}

#[async_trait::async_trait]
impl DaemonHandle for StubDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.tx.subscribe()
    }

    async fn get_state(&self, _repo: &Path) -> Result<Snapshot, String> {
        Err("stub".into())
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        Ok(vec![])
    }

    async fn execute(&self, _repo: &Path, _command: Command) -> Result<u64, String> {
        Ok(1)
    }

    async fn refresh(&self, _repo: &Path) -> Result<(), String> {
        Ok(())
    }

    async fn add_repo(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }

    async fn remove_repo(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }

    async fn replay_since(
        &self,
        _last_seen: &HashMap<PathBuf, u64>,
    ) -> Result<Vec<DaemonEvent>, String> {
        Ok(vec![])
    }
}

pub(crate) fn stub_app() -> App {
    stub_app_with_repo_info(default_repo_info())
}

pub(crate) fn stub_app_with_repos(count: usize) -> App {
    let repos_info = (0..count)
        .map(|i| {
            repo_info(
                format!("/tmp/repo-{i}"),
                format!("repo-{i}"),
                RepoLabels::default(),
            )
        })
        .collect();
    stub_app_with_repo_infos(repos_info)
}

pub(crate) fn default_repo_model(labels: RepoLabels) -> TuiRepoModel {
    TuiRepoModel {
        providers: Arc::new(flotilla_protocol::ProviderData::default()),
        labels,
        provider_names: HashMap::new(),
        provider_health: HashMap::new(),
        loading: false,
        issue_has_more: false,
        issue_total: None,
        issue_search_active: false,
        issue_fetch_pending: false,
        issue_initial_requested: false,
    }
}

pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(crate) fn bare_item() -> WorkItem {
    WorkItem {
        kind: WorkItemKind::Issue,
        identity: WorkItemIdentity::Issue("1".into()),
        branch: None,
        description: String::new(),
        checkout: None,
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
    }
}

pub(crate) fn issue_item(id: impl Into<String>) -> WorkItem {
    let id = id.into();
    WorkItem {
        kind: WorkItemKind::Issue,
        identity: WorkItemIdentity::Issue(id.clone()),
        branch: None,
        description: format!("Item {id}"),
        checkout: None,
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
    }
}

pub(crate) fn checkout_item(branch: &str, path: &str, is_main: bool) -> WorkItem {
    WorkItem {
        kind: WorkItemKind::Checkout,
        identity: WorkItemIdentity::Checkout(PathBuf::from(path)),
        branch: Some(branch.into()),
        description: format!("checkout {branch}"),
        checkout: Some(CheckoutRef {
            key: PathBuf::from(path),
            is_main_checkout: is_main,
        }),
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: is_main,
        debug_group: Vec::new(),
    }
}

pub(crate) fn pr_item(pr_id: &str) -> WorkItem {
    WorkItem {
        kind: WorkItemKind::ChangeRequest,
        identity: WorkItemIdentity::ChangeRequest(pr_id.into()),
        branch: Some("feat/pr-branch".into()),
        description: format!("PR #{pr_id}"),
        checkout: None,
        change_request_key: Some(pr_id.into()),
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
    }
}

pub(crate) fn session_item(session_id: &str) -> WorkItem {
    WorkItem {
        kind: WorkItemKind::Session,
        identity: WorkItemIdentity::Session(session_id.into()),
        branch: Some("feat/session-branch".into()),
        description: format!("session {session_id}"),
        checkout: None,
        change_request_key: None,
        session_key: Some(session_id.into()),
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
    }
}

pub(crate) fn remote_branch_item(branch: &str) -> WorkItem {
    WorkItem {
        kind: WorkItemKind::RemoteBranch,
        identity: WorkItemIdentity::RemoteBranch(branch.into()),
        branch: Some(branch.into()),
        description: format!("remote {branch}"),
        checkout: None,
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
    }
}

pub(crate) fn grouped_items(items: Vec<WorkItem>) -> GroupedWorkItems {
    let selectable_indices = (0..items.len()).collect();
    let table_entries = items
        .into_iter()
        .map(|item| GroupEntry::Item(Box::new(item)))
        .collect();
    GroupedWorkItems {
        table_entries,
        selectable_indices,
    }
}

pub(crate) fn issue_table_entries(count: usize) -> GroupedWorkItems {
    grouped_items((0..count).map(|i| issue_item(i.to_string())).collect())
}

pub(crate) fn set_active_table_view(app: &mut App, table_view: GroupedWorkItems) {
    let repo_key = app.model.repo_order[app.model.active_repo].clone();
    app.ui.repo_ui.get_mut(&repo_key).unwrap().table_view = table_view;
}

pub(crate) fn setup_selectable_table(app: &mut App, items: Vec<WorkItem>) {
    set_active_table_view(app, grouped_items(items));
    if app.active_ui().table_view.selectable_indices.is_empty() {
        app.active_ui_mut().selected_selectable_idx = None;
        app.active_ui_mut().table_state.select(None);
    } else {
        app.active_ui_mut().selected_selectable_idx = Some(0);
        app.active_ui_mut().table_state.select(Some(0));
    }
}

pub(crate) fn enter_file_picker(app: &mut App, path: &str, entries: Vec<DirEntry>) {
    app.ui.mode = UiMode::FilePicker {
        input: Input::from(path),
        dir_entries: entries,
        selected: 0,
    };
}

pub(crate) fn dir_entry(name: &str, is_git_repo: bool, is_added: bool) -> DirEntry {
    DirEntry {
        name: name.to_string(),
        is_dir: true,
        is_git_repo,
        is_added,
    }
}

fn default_repo_info() -> RepoInfo {
    repo_info("/tmp/test-repo", "test-repo", RepoLabels::default())
}

fn stub_app_with_repo_info(repo_info: RepoInfo) -> App {
    stub_app_with_repo_infos(vec![repo_info])
}

fn stub_app_with_repo_infos(repos_info: Vec<RepoInfo>) -> App {
    let daemon: Arc<dyn DaemonHandle> = Arc::new(StubDaemon::new());
    let config = Arc::new(ConfigStore::with_base("/tmp/flotilla-test"));
    App::new(daemon, repos_info, config)
}

fn repo_info(path: impl Into<PathBuf>, name: impl Into<String>, labels: RepoLabels) -> RepoInfo {
    RepoInfo {
        path: path.into(),
        name: name.into(),
        labels,
        provider_names: HashMap::new(),
        provider_health: HashMap::new(),
        loading: false,
    }
}
