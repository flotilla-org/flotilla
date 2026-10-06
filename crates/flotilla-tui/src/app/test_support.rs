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
use crate::{
    keymap::Keymap,
    widgets::{file_picker::FilePickerWidget, WidgetContext},
};

type FocusObservations = Arc<Mutex<Vec<(uuid::Uuid, Vec<flotilla_protocol::ResourceRef>)>>>;
pub(crate) type ExecuteCalls = Arc<Mutex<Vec<Command>>>;
pub(crate) type QueryCalls = Arc<Mutex<Vec<(Command, uuid::Uuid)>>>;

#[derive(bon::Builder)]
pub(crate) struct StubDaemon {
    #[builder(default = broadcast::channel(1).0)]
    tx: broadcast::Sender<DaemonEvent>,
    #[builder(default = Mutex::new(None), with = |result: Result<CommandValue, String>| Mutex::new(Some(result)))]
    query_result: Mutex<Option<Result<CommandValue, String>>>,
    query_gate: Option<Arc<Semaphore>>,
    #[builder(default)]
    query_panics: bool,
    #[builder(default = Mutex::new(Ok(vec![])), with = |result: Result<Vec<DaemonEvent>, String>| Mutex::new(result))]
    pub(crate) subscribe_result: Mutex<Result<Vec<DaemonEvent>, String>>,
    #[builder(default = Arc::new(AtomicUsize::new(0)))]
    pub(crate) subscribe_calls: Arc<AtomicUsize>,
    execute_gate: Option<Arc<Semaphore>>,
    #[builder(default = Ok(1))]
    execute_result: Result<u64, String>,
    #[builder(default = Arc::new(Mutex::new(Vec::new())))]
    execute_calls: ExecuteCalls,
    #[builder(default = Arc::new(Mutex::new(Vec::new())))]
    query_calls: QueryCalls,
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

    async fn execute(&self, command: Command) -> Result<u64, String> {
        self.execute_calls.lock().expect("execute calls lock").push(command);
        if let Some(gate) = &self.execute_gate {
            gate.acquire().await.expect("execute gate should remain open").forget();
        }
        self.execute_result.clone()
    }

    async fn execute_query(&self, command: Command, session_id: uuid::Uuid) -> Result<flotilla_protocol::CommandValue, String> {
        self.query_calls.lock().expect("query calls lock").push((command, session_id));
        assert!(!self.query_panics, "simulated project query panic");
        if let Some(gate) = &self.query_gate {
            gate.acquire().await.expect("query gate should remain open").forget();
        }
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
        self.subscribe_calls.fetch_add(1, Ordering::SeqCst);
        self.subscribe_result.lock().expect("subscribe result lock").clone()
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
    app.screen.modal_stack.push(Box::new(FilePickerWidget::new(Input::from(path), entries)));
}

pub(crate) fn dir_entry(name: &str, is_git_repo: bool, is_added: bool) -> DirEntry {
    DirEntry::builder().name(name.to_string()).path(PathBuf::from(name)).is_dir(true).is_git_repo(is_git_repo).is_added(is_added).build()
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
            namespaces: Default::default(),
            query_tables: Default::default(),
        }
    }

    /// Make the overview the active tab (the old `is_config = true`).
    pub fn activate_overview(&mut self) {
        self.views.switch_to(0);
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
            namespaces: &self.namespaces,
            query_tables: &self.query_tables,
            app_actions: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{CommandAction, CommandValue, ProjectListResponse};

    use super::*;
    use crate::app::{ProjectAddressState, ProviderStatus};

    #[test]
    fn provider_health_remains_visible_after_daemon_reconnect() {
        let mut repo = repo_info("/tmp/repo", "repo", RepoLabels::default());
        repo.provider_names.insert("vcs".into(), vec!["git".into()]);
        repo.provider_health.insert("vcs".into(), HashMap::from([("git".into(), false)]));
        let identity = repo.identity.clone();
        let mut app = stub_app_with_repo_infos(vec![repo.clone()]);
        assert_eq!(app.model.provider_status(&identity, "vcs", "git"), Some(ProviderStatus::Error));

        app.reconnect_daemon(Arc::new(StubDaemon::new()), vec![repo]);
        assert_eq!(app.model.provider_status(&identity, "vcs", "git"), Some(ProviderStatus::Error));

        let mut updated = repo_info("/tmp/repo", "repo", RepoLabels::default());
        updated.provider_names.insert("vcs".into(), vec!["git".into()]);
        updated.provider_health.insert("vcs".into(), HashMap::from([("git".into(), true)]));
        app.reconnect_daemon(Arc::new(StubDaemon::new()), vec![updated]);
        assert_eq!(app.model.provider_status(&identity, "vcs", "git"), Some(ProviderStatus::Ok));
    }

    #[test]
    fn project_addresses_load_once_and_fill_the_palette_cache() {
        let mut app = stub_app();
        app.process_app_actions(vec![crate::widgets::AppAction::LoadProjectAddresses, crate::widgets::AppAction::LoadProjectAddresses]);
        let (command, _) = app.proto_commands.take_next().expect("project-list query");
        assert!(matches!(command.action, CommandAction::QueryProjectList {}));
        assert!(app.proto_commands.take_next().is_none(), "in-flight query is not duplicated");

        let session_id = app.session_id;
        app.handle_project_addresses_loaded(session_id, Ok(CommandValue::ProjectList(Box::new(ProjectListResponse { projects: vec![] }))));
        assert_eq!(app.model.project_address_state, ProjectAddressState::Loaded(vec![]));
    }

    #[test]
    fn failed_project_address_query_can_be_retried_after_reopening_palette() {
        let mut app = stub_app();
        app.process_app_actions(vec![crate::widgets::AppAction::LoadProjectAddresses]);
        let _ = app.proto_commands.take_next().expect("first project-list query");
        app.handle_project_addresses_loaded(app.session_id, Err("offline".into()));
        assert_eq!(app.model.project_address_state, ProjectAddressState::Failed);

        app.process_app_actions(vec![crate::widgets::AppAction::LoadProjectAddresses]);
        let (command, _) = app.proto_commands.take_next().expect("retry project-list query");
        assert!(matches!(command.action, CommandAction::QueryProjectList {}));
    }

    #[test]
    fn refreshing_project_addresses_keeps_cached_choices_until_the_new_list_arrives() {
        let mut app = stub_app();
        let cached: flotilla_protocol::ViewAddress = "project/flotilla/roadmap".parse().expect("project address");
        app.model.project_address_state = ProjectAddressState::Loaded(vec![cached.clone()]);
        app.process_app_actions(vec![crate::widgets::AppAction::LoadProjectAddresses]);
        assert_eq!(app.model.project_address_state, ProjectAddressState::Refreshing(vec![cached]));
        assert!(matches!(app.proto_commands.take_next(), Some((Command { action: CommandAction::QueryProjectList {}, .. }, _))));
        app.handle_project_addresses_loaded(
            app.session_id,
            Ok(CommandValue::ProjectList(Box::new(ProjectListResponse { projects: vec![] }))),
        );
        assert_eq!(app.model.project_address_state, ProjectAddressState::Loaded(vec![]));
    }

    #[test]
    fn failed_refresh_keeps_cached_project_addresses() {
        let mut app = stub_app();
        let cached: flotilla_protocol::ViewAddress = "project/flotilla/roadmap".parse().expect("project address");
        app.model.project_address_state = ProjectAddressState::Loaded(vec![cached.clone()]);
        app.process_app_actions(vec![crate::widgets::AppAction::LoadProjectAddresses]);
        let _ = app.proto_commands.take_next().expect("refresh query");
        app.handle_project_addresses_loaded(app.session_id, Err("offline".into()));
        assert_eq!(app.model.project_address_state, ProjectAddressState::Loaded(vec![cached]));
    }

    #[test]
    fn test_widget_harness_builds_context() {
        let mut harness = TestWidgetHarness::new();
        let ctx = harness.ctx();
        assert!(ctx.app_actions.is_empty());
    }
}
