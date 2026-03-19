use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    data::{GroupEntry, GroupedWorkItems},
};
use flotilla_protocol::{
    Change, Command, DaemonEvent, HostListResponse, HostName, HostProvidersResponse, HostStatusResponse, ProviderData, ProviderError,
    RepoDelta, RepoDetailResponse, RepoIdentity, RepoInfo, RepoLabels, RepoProvidersResponse, RepoSnapshot, RepoWorkResponse,
    StatusResponse, StreamKey, TopologyResponse, WorkItem,
};
use tokio::sync::broadcast;
use tui_input::Input;

// Re-export shared builders so unit tests can use `test_support::checkout_item` etc.
pub(crate) use super::test_builders::*;
use super::{App, CommandQueue, DirEntry, InFlightCommand, RepoUiState, TuiModel, UiMode};
use crate::{keymap::Keymap, widgets::WidgetContext};

pub(crate) struct StubDaemon {
    tx: broadcast::Sender<DaemonEvent>,
}

static STUB_APP_CONFIG_COUNTER: AtomicUsize = AtomicUsize::new(0);

impl StubDaemon {
    pub(crate) fn new() -> Self {
        let (tx, _) = broadcast::channel(1);
        Self { tx }
    }
}

#[async_trait::async_trait]
impl DaemonHandle for StubDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.tx.subscribe()
    }

    async fn get_state(&self, _repo: &flotilla_protocol::RepoSelector) -> Result<RepoSnapshot, String> {
        Err("stub".into())
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        Ok(vec![])
    }

    async fn execute(&self, _command: Command) -> Result<u64, String> {
        Ok(1)
    }

    async fn cancel(&self, _command_id: u64) -> Result<(), String> {
        Ok(())
    }

    async fn replay_since(&self, _last_seen: &HashMap<StreamKey, u64>) -> Result<Vec<DaemonEvent>, String> {
        Ok(vec![])
    }

    async fn get_status(&self) -> Result<StatusResponse, String> {
        Ok(StatusResponse { repos: vec![] })
    }

    async fn get_repo_detail(&self, _repo: &flotilla_protocol::RepoSelector) -> Result<RepoDetailResponse, String> {
        Err("stub".into())
    }

    async fn get_repo_providers(&self, _repo: &flotilla_protocol::RepoSelector) -> Result<RepoProvidersResponse, String> {
        Err("stub".into())
    }

    async fn get_repo_work(&self, _repo: &flotilla_protocol::RepoSelector) -> Result<RepoWorkResponse, String> {
        Err("stub".into())
    }

    async fn list_hosts(&self) -> Result<HostListResponse, String> {
        Err("stub".into())
    }

    async fn get_host_status(&self, _host: &str) -> Result<HostStatusResponse, String> {
        Err("stub".into())
    }

    async fn get_host_providers(&self, _host: &str) -> Result<HostProvidersResponse, String> {
        Err("stub".into())
    }

    async fn get_topology(&self) -> Result<TopologyResponse, String> {
        Err("stub".into())
    }
}

pub(crate) fn stub_app() -> App {
    stub_app_with_repo_info(default_repo_info())
}

pub(crate) fn stub_app_with_repos(count: usize) -> App {
    let repos_info = (0..count).map(|i| repo_info(format!("/tmp/repo-{i}"), format!("repo-{i}"), RepoLabels::default())).collect();
    stub_app_with_repo_infos(repos_info)
}

pub(crate) fn active_repo_path(app: &App) -> PathBuf {
    app.model.active_repo_root().clone()
}

pub(crate) fn provider_error(category: &str, provider: &str, message: &str) -> ProviderError {
    ProviderError { category: category.into(), provider: provider.into(), message: message.into() }
}

pub(crate) fn snapshot(repo: &Path) -> RepoSnapshot {
    RepoSnapshot {
        seq: 1,
        repo_identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: repo.display().to_string() },
        repo: repo.to_path_buf(),
        host_name: flotilla_protocol::HostName::local(),
        work_items: vec![],
        providers: ProviderData::default(),
        provider_health: HashMap::new(),
        errors: vec![],
        issue_total: None,
        issue_has_more: false,
        issue_search_results: None,
    }
}

pub(crate) fn delta(repo: &Path, changes: Vec<Change>) -> RepoDelta {
    RepoDelta {
        seq: 2,
        prev_seq: 1,
        repo_identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: repo.display().to_string() },
        repo: repo.to_path_buf(),
        changes,
        work_items: vec![],
        issue_total: None,
        issue_has_more: false,
        issue_search_results: None,
    }
}

pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(crate) fn grouped_items(items: Vec<WorkItem>) -> GroupedWorkItems {
    let selectable_indices = (0..items.len()).collect();
    let table_entries = items.into_iter().map(|item| GroupEntry::Item(Box::new(item))).collect();
    GroupedWorkItems { table_entries, selectable_indices }
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
    app.widget_stack.push(Box::new(crate::widgets::file_picker::FilePickerWidget::new(Input::from(path), entries)));
}

pub(crate) fn dir_entry(name: &str, is_git_repo: bool, is_added: bool) -> DirEntry {
    DirEntry { name: name.to_string(), is_dir: true, is_git_repo, is_added }
}

fn default_repo_info() -> RepoInfo {
    repo_info("/tmp/test-repo", "test-repo", RepoLabels::default())
}

fn stub_app_with_repo_info(repo_info: RepoInfo) -> App {
    stub_app_with_repo_infos(vec![repo_info])
}

fn stub_app_with_repo_infos(repos_info: Vec<RepoInfo>) -> App {
    let daemon: Arc<dyn DaemonHandle> = Arc::new(StubDaemon::new());
    let config_id = STUB_APP_CONFIG_COUNTER.fetch_add(1, Ordering::Relaxed);
    let config_base = std::env::temp_dir().join(format!("flotilla-test-{config_id}"));
    let _ = std::fs::remove_dir_all(&config_base);
    let config = Arc::new(ConfigStore::with_base(config_base));
    App::new(daemon, repos_info, config, crate::theme::Theme::classic())
}

/// Test harness that owns the state needed to construct a `WidgetContext`.
///
/// Use `new()` to build from a default `stub_app()`, then call `ctx()` to
/// get a `WidgetContext` suitable for driving widget event handlers in tests.
pub(crate) struct TestWidgetHarness {
    pub model: TuiModel,
    pub keymap: Keymap,
    pub config: Arc<ConfigStore>,
    pub in_flight: HashMap<u64, InFlightCommand>,
    pub commands: CommandQueue,
    pub repo_ui: HashMap<RepoIdentity, RepoUiState>,
    pub target_host: Option<HostName>,
    pub mode: UiMode,
}

impl TestWidgetHarness {
    pub fn new() -> Self {
        let app = stub_app();
        Self {
            model: app.model,
            keymap: app.keymap,
            config: app.config,
            in_flight: app.in_flight,
            commands: app.proto_commands,
            repo_ui: app.ui.repo_ui,
            target_host: app.ui.target_host,
            mode: UiMode::Normal,
        }
    }

    pub fn ctx(&mut self) -> WidgetContext<'_> {
        WidgetContext {
            model: &self.model,
            keymap: &self.keymap,
            config: &self.config,
            in_flight: &self.in_flight,
            target_host: self.target_host.as_ref(),
            active_repo: self.model.active_repo,
            repo_order: &self.model.repo_order,
            commands: &mut self.commands,
            repo_ui: &mut self.repo_ui,
            mode: &mut self.mode,
            app_actions: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_widget_harness_builds_context() {
        let mut harness = TestWidgetHarness::new();
        let ctx = harness.ctx();
        assert!(ctx.app_actions.is_empty());
    }
}
