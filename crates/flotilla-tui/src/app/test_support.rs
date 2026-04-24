use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use flotilla_core::{config::ConfigStore, daemon::DaemonHandle, data::SectionLabels};
use flotilla_protocol::{
    qualified_path::HostId, Change, Command, DaemonEvent, EnvironmentId, HostName, HostSummary, NodeId, NodeInfo, ProviderData,
    ProviderError, ProvisioningTarget, RepoDelta, RepoInfo, RepoLabels, RepoSnapshot, StatusResponse, StreamKey, TopologyResponse,
    WorkItem,
};
use tokio::sync::broadcast;
use tui_input::Input;

// Re-export shared builders so unit tests can use `test_support::checkout_item` etc.
pub(crate) use super::test_builders::*;
use super::{App, CommandQueue, DirEntry, InFlightCommand, TuiHostState, TuiModel};
use crate::{keymap::Keymap, widgets::WidgetContext};

pub(crate) struct StubDaemon {
    tx: broadcast::Sender<DaemonEvent>,
}

static STUB_APP_CONFIG_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn local_node_id() -> NodeId {
    NodeId::new("node-local-test")
}

fn insert_stub_local_host(model: &mut TuiModel) {
    let host_name = HostName::local();
    let environment_id = EnvironmentId::host(HostId::new("local-test-host"));
    model.hosts.insert(environment_id.clone(), TuiHostState {
        environment_id: environment_id.clone(),
        host_name: host_name.clone(),
        is_local: true,
        status: super::PeerStatus::Connected,
        summary: HostSummary {
            environment_id,
            host_name: Some(host_name.clone()),
            node: NodeInfo::new(local_node_id(), host_name.as_str()),
            system: flotilla_protocol::SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        },
    });
}

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

    async fn execute_query(&self, _command: Command, _session_id: uuid::Uuid) -> Result<flotilla_protocol::CommandValue, String> {
        Err("stub".into())
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
        repo: Some(repo.to_path_buf()),
        node_id: local_node_id(),
        work_items: vec![],
        providers: ProviderData::default(),
        provider_health: HashMap::new(),
        errors: vec![],
    }
}

pub(crate) fn delta(repo: &Path, changes: Vec<Change>) -> RepoDelta {
    RepoDelta {
        seq: 2,
        prev_seq: 1,
        repo_identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: repo.display().to_string() },
        repo: Some(repo.to_path_buf()),
        changes,
        work_items: vec![],
    }
}

pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(crate) fn set_active_table_items(app: &mut App, items: Vec<WorkItem>) {
    let repo_key = app.model.repo_order[app.model.active_repo].clone();
    if let Some(page) = app.screen.repo_pages.get_mut(&repo_key) {
        let providers = flotilla_protocol::ProviderData::default();
        let labels = SectionLabels::default();
        let sections = flotilla_core::data::group_work_items_split(&items, &providers, &labels, std::path::Path::new("/tmp"));
        page.table.update_sections(sections);
        // Clear the auto-selection so tests retain None until they explicitly navigate.
        page.table.clear_selection();
    }
}

pub(crate) fn setup_selectable_table(app: &mut App, items: Vec<WorkItem>) {
    // Populate Shared<RepoData> so the RepoPage can reconcile the items.
    let repo_key = app.model.repo_order[app.model.active_repo].clone();
    if let Some(handle) = app.repo_data.get(&repo_key) {
        handle.mutate(|d| {
            d.work_items = items;
        });
    }
    // Trigger reconciliation on the RepoPage so its table is populated.
    if let Some(page) = app.screen.repo_pages.get_mut(&repo_key) {
        page.reconcile_if_changed();
    }
}

pub(crate) fn enter_file_picker(app: &mut App, path: &str, entries: Vec<DirEntry>) {
    app.screen.modal_stack.push(Box::new(crate::widgets::file_picker::FilePickerWidget::new(Input::from(path), entries)));
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
    let mut app = App::new(daemon, repos_info, config, crate::theme::Theme::classic());
    insert_stub_local_host(&mut app.model);
    app
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
    pub provisioning_target: ProvisioningTarget,
    pub my_host: Option<HostName>,
    pub my_node_id: Option<NodeId>,
    pub is_config: bool,
    pub active_repo_is_remote_only: bool,
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
            provisioning_target: app.ui.provisioning_target.clone(),
            my_host: None,
            my_node_id: None,
            is_config: false,
            active_repo_is_remote_only: false,
        }
    }

    pub fn ctx(&mut self) -> WidgetContext<'_> {
        WidgetContext {
            model: &self.model,
            keymap: &self.keymap,
            config: &self.config,
            in_flight: &self.in_flight,
            provisioning_target: &self.provisioning_target,
            my_host: self.my_host.clone(),
            my_node_id: self.my_node_id.clone(),
            active_repo: self.model.active_repo,
            repo_order: &self.model.repo_order,
            commands: &mut self.commands,
            is_config: &mut self.is_config,
            is_convoys: false,
            active_repo_is_remote_only: self.active_repo_is_remote_only,
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
