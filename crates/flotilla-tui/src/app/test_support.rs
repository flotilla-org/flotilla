use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use flotilla_core::{config::ConfigStore, daemon::DaemonHandle};
use flotilla_protocol::{
    qualified_path::HostId, Command, CommandValue, DaemonEvent, EnvironmentId, HostName, HostSummary, NodeId, NodeInfo, ProvisioningTarget,
    RepoInfo, RepoLabels, StatusResponse, StreamKey, TopologyResponse,
};
use tokio::sync::{broadcast, Semaphore};
use tui_input::Input;

use super::{App, CommandQueue, DirEntry, InFlightCommand, OpenViews, TuiHostState, TuiModel};
use crate::{keymap::Keymap, widgets::WidgetContext};

type FocusObservations = Arc<Mutex<Vec<(uuid::Uuid, Vec<flotilla_protocol::ResourceRef>)>>>;

#[derive(bon::Builder)]
pub(crate) struct StubDaemon {
    #[builder(default = broadcast::channel(1).0)]
    tx: broadcast::Sender<DaemonEvent>,
    #[builder(default = Mutex::new(None), with = |result: Result<CommandValue, String>| Mutex::new(Some(result)))]
    query_result: Mutex<Option<Result<CommandValue, String>>>,
    execute_gate: Option<Arc<Semaphore>>,
    #[builder(default = Ok(1))]
    execute_result: Result<u64, String>,
    #[builder(default = Arc::new(Mutex::new(Vec::new())))]
    observations: FocusObservations,
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
        Self::builder().build()
    }
}

#[async_trait::async_trait]
impl DaemonHandle for StubDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.tx.subscribe()
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        Ok(vec![])
    }

    async fn execute(&self, _command: Command) -> Result<u64, String> {
        if let Some(gate) = &self.execute_gate {
            gate.acquire().await.expect("execute gate should remain open").forget();
        }
        self.execute_result.clone()
    }

    async fn execute_query(&self, _command: Command, _session_id: uuid::Uuid) -> Result<flotilla_protocol::CommandValue, String> {
        self.query_result.lock().expect("query result lock").take().unwrap_or_else(|| Err("stub".into()))
    }

    async fn cancel(&self, _command_id: u64) -> Result<(), String> {
        Ok(())
    }

    async fn replay_since(&self, _last_seen: &HashMap<StreamKey, u64>) -> Result<Vec<DaemonEvent>, String> {
        Ok(vec![])
    }

    async fn subscribe_queries(
        &self,
        _subscriber_id: uuid::Uuid,
        _queries: &[flotilla_protocol::QueryCursor],
    ) -> Result<Vec<DaemonEvent>, String> {
        Ok(vec![])
    }

    async fn get_status(&self) -> Result<StatusResponse, String> {
        Ok(StatusResponse { repos: vec![] })
    }

    async fn get_topology(&self) -> Result<TopologyResponse, String> {
        Err("stub".into())
    }

    async fn observe_focus(&self, surface_id: uuid::Uuid, targets: Vec<flotilla_protocol::ResourceRef>) -> Result<(), String> {
        self.observations.lock().expect("observations lock").push((surface_id, targets));
        Ok(())
    }
}

pub(crate) fn stub_app() -> App {
    stub_app_with_repo_info(default_repo_info())
}

pub(crate) fn stub_app_with_repos(count: usize) -> App {
    let repos_info = (0..count).map(|i| repo_info(format!("/tmp/repo-{i}"), format!("repo-{i}"), RepoLabels::default())).collect();
    stub_app_with_repo_infos(repos_info)
}

pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(crate) fn enter_file_picker(app: &mut App, path: &str, entries: Vec<DirEntry>) {
    app.screen.modal_stack.push(Box::new(crate::widgets::file_picker::FilePickerWidget::new(Input::from(path), entries)));
}

pub(crate) fn dir_entry(name: &str, is_git_repo: bool, is_added: bool) -> DirEntry {
    DirEntry { name: name.to_string(), is_dir: true, is_git_repo, is_added }
}

pub(crate) fn repo_info(path: impl Into<PathBuf>, name: impl Into<String>, labels: RepoLabels) -> RepoInfo {
    let path = path.into();
    RepoInfo {
        identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: path.display().to_string() },
        repository_key: None,
        path: Some(path),
        name: name.into(),
        labels,
        provider_names: HashMap::new(),
        provider_health: HashMap::new(),
        loading: false,
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
    stub_app_with_daemon(daemon, repos_info)
}

pub(crate) fn stub_app_with_daemon(daemon: Arc<dyn DaemonHandle>, repos_info: Vec<RepoInfo>) -> App {
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
    pub views: OpenViews,
    pub keymap: Keymap,
    pub config: Arc<ConfigStore>,
    pub in_flight: HashMap<u64, InFlightCommand>,
    pub commands: CommandQueue,
    pub provisioning_target: ProvisioningTarget,
    pub my_host: Option<HostName>,
    pub my_node_id: Option<NodeId>,
    pub active_repo_is_remote_only: bool,
    pub namespaces: crate::app::NamespaceMap,
    pub query_tables: crate::app::QueryTableCache,
}

impl TestWidgetHarness {
    pub fn new() -> Self {
        let app = stub_app();
        Self {
            model: app.model,
            views: app.views,
            keymap: app.keymap,
            config: app.config,
            in_flight: app.in_flight,
            commands: app.proto_commands,
            provisioning_target: app.ui.provisioning_target.clone(),
            my_host: None,
            my_node_id: None,
            active_repo_is_remote_only: false,
            namespaces: Default::default(),
            query_tables: Default::default(),
        }
    }

    /// Make the overview the active tab (the old `is_config = true`).
    pub fn activate_overview(&mut self) {
        self.views.switch_to(0);
        self.model.active_repo = self.views.active_repo_identity().cloned();
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
            views: &mut self.views,
            commands: &mut self.commands,
            active_repo_is_remote_only: self.active_repo_is_remote_only,
            namespaces: &self.namespaces,
            query_tables: &self.query_tables,
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
