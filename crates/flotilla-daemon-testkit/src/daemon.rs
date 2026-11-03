//! A daemon composed as `flotillad` composes one, for behaviour tests.
//!
//! [`TestDaemon`] writes a `daemon.toml` whose `[dummy_fleet]` section selects
//! the fakes, then starts the production composition through the
//! composition-root switch: the same controllers, startup restoration and
//! periodic tasks as `flotillad`. The only fakes are the dummy fleet's
//! environment vendors, forge, VCS, terminal pool and agent. Tests touch what
//! an operator, a crew or a vendor touches:
//!
//! - commands, as an operator through the request dispatcher a socket client
//!   uses ([`TestDaemon::execute_as_operator`], [`TestDaemon::client`]);
//! - the environment vendor, through [`TestDaemon::fleet`]
//!   ([`DummyFleet::vanish_environment`] and its siblings);
//! - the daemon process, through [`TestDaemon::restart`] over the same store,
//!   while vendor backings and running crews survive.
//!
//! Waits read any resource kind and fail with the record's last state.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_client::SocketDaemon;
use flotilla_config::{ConfigStore, DaemonConfig};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon::{
    dummy_fleet::{daemon_crew_port, install_dummy_fleet},
    peer::PeerManager,
    runtime::{DaemonRuntime, RuntimeOptions},
    server::{build_remote_command_router, PeerConnectionEvent},
    supervisor::ControllerSupervision,
};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_discovery::DiscoveryRuntime;
use flotilla_discovery_testkit::fake_discovery_with_host_directories;
use flotilla_dummy_fleet::{
    catalog::{self, CrewMix, DUMMY_CLOUD_FULFILMENT_KIND, DUMMY_FULFILMENT_KIND},
    CrewCommandPort, DummyFleet, DummyFleetSnapshot, DUMMY_AGENT_ADAPTER,
};
use flotilla_protocol::{commands::CommandValue, CommandAction, CommandCaller, DaemonEvent, HostName, PrincipalRef};
use flotilla_resources::{
    Clock, Convoy, ConvoyPhase, CrewWorkPhase, FleetDesignation, FleetDesignationSpec, Forge, FulfilmentKind, Host, InputMeta, Project,
    ProjectSpec, Repository, Resource, ResourceError, ResourceObject, SystemClock, WorkflowTemplate, FLEET_DESIGNATION_NAME,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_sqlite::SqliteBackend;
use tokio::sync::{mpsc, watch, Notify};

/// The namespace every test daemon provisions in.
pub const NAMESPACE: &str = "flotilla";
/// Long runners' turns when a test sets none: a middle turn and the last one.
pub const LONG_RUNNER_TURNS: u32 = 2;
/// How long a wait lasts before it fails with the record's last state.
pub const WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(100);
const SEED_WAIT: Duration = Duration::from_secs(60);

/// Builds the daemon's discovery from its home directory. The dummy fleet is
/// installed into whatever it returns, on start and on every restart.
pub type DiscoveryFactory = Arc<dyn Fn(&Path) -> DiscoveryRuntime + Send + Sync>;

/// Where a test daemon keeps its resource store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TestStore {
    #[default]
    Memory,
    /// SQLite in the daemon's state directory, where `flotillad` opens it.
    Sqlite,
}

/// What a restart rebuilds the daemon from.
struct Composition {
    host: HostName,
    options: RuntimeOptions,
    clock: Arc<dyn Clock>,
    discovery: DiscoveryFactory,
    backend: ResourceBackend,
}

pub struct TestDaemon {
    daemon: Arc<InProcessDaemon>,
    fleet: Arc<DummyFleet>,
    runtime: Option<DaemonRuntime>,
    relay: Arc<RestartRelay>,
    composition: Composition,
    sqlite: Option<SqliteBackend>,
    operator: Mutex<Option<Arc<TestClient>>>,
    dir: tempfile::TempDir,
}

#[bon::bon]
impl TestDaemon {
    /// Start a daemon whose `daemon.toml` enables the dummy fleet with these
    /// settings, then seed the dummy catalog and wait until its kinds can admit.
    #[builder(start_fn = builder, finish_fn = start)]
    pub async fn start_daemon(
        /// Lines of `[dummy_fleet.agent]`, such as `stall_rate = 1.0`.
        #[builder(default)]
        agent: &str,
        /// Lines of `[dummy_fleet.environment]`, such as `failure_rate = 1.0`.
        #[builder(default)]
        environment: &str,
        /// Lines of `[dummy_fleet.change_request]`.
        #[builder(default)]
        change_request: &str,
        #[builder(default = LONG_RUNNER_TURNS)] long_runner_turns: u32,
        /// Other `daemon.toml` content, ahead of the dummy fleet's sections.
        #[builder(default)]
        settings: &str,
        /// `[primitives]`: Message and the provisioning kinds on the engine.
        #[builder(default)]
        primitives: bool,
        #[builder(default = "dummy-load")] host: &str,
        /// Record the fleet designation the engine resolves its roles from.
        #[builder(default = true)]
        designated: bool,
        /// Seed the dummy forge, repository, kinds and workflows.
        #[builder(default = true)]
        catalog: bool,
        #[builder(default)] store: TestStore,
        /// Runtime options before the overrides below; harness cadences by default.
        options: Option<RuntimeOptions>,
        resync: Option<Duration>,
        heartbeat: Option<Duration>,
        supervision: Option<ControllerSupervision>,
        clock: Option<Arc<dyn Clock>>,
        discovery: Option<DiscoveryFactory>,
    ) -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        let switch = if primitives { "[primitives]\n" } else { "" };
        std::fs::write(
            config_dir.join("daemon.toml"),
            format!(
                "machine_id = \"{host}\"\n\n{switch}{settings}\n[dummy_fleet]\n[dummy_fleet.agent]\nlong_runner_turns = {long_runner_turns}\n{agent}\n[dummy_fleet.environment]\n{environment}\n[dummy_fleet.change_request]\n{change_request}\n"
            ),
        )
        .expect("daemon.toml");
        let mut options = options.unwrap_or_else(harness_options);
        options.namespace = NAMESPACE.into();
        if let Some(resync) = resync {
            options.controller_resync_interval = resync;
        }
        if let Some(heartbeat) = heartbeat {
            options.heartbeat_interval = heartbeat;
        }
        if let Some(supervision) = supervision {
            options.controller_supervision = supervision;
        }
        let config = ConfigStore::with_base(config_dir);
        let (backend, sqlite) = match store {
            TestStore::Memory => (ResourceBackend::new(InMemoryBackend::default()), None),
            TestStore::Sqlite => {
                let database = config.state_dir().as_path().join("resources.sqlite");
                std::fs::create_dir_all(database.parent().expect("state dir")).expect("state dir");
                let sqlite = SqliteBackend::open_async(&database).await.expect("sqlite store");
                (ResourceBackend::new(sqlite.clone()), Some(sqlite))
            }
        };
        let composition = Composition {
            host: HostName::new(host),
            options,
            clock: clock.unwrap_or_else(|| Arc::new(SystemClock)),
            discovery: discovery.unwrap_or_else(|| Arc::new(fake_discovery_with_host_directories)),
            backend,
        };
        let relay = Arc::new(RestartRelay::default());
        let config_store = Arc::new(ConfigStore::with_base(dir.path().join("config")));
        let daemon_config: DaemonConfig = config_store.load_daemon_config().expect("daemon config");
        let mut discovery = (composition.discovery)(dir.path());
        let fleet = install_dummy_fleet(&daemon_config, &mut discovery).expect("dummy fleet configured");
        fleet.attach(Arc::clone(&relay) as Arc<dyn CrewCommandPort>).expect("attach the crews' command relay");
        let (daemon, runtime) = compose(&composition, config_store, discovery, &relay).await;
        let started = Self { daemon, fleet, runtime: Some(runtime), relay, composition, sqlite, operator: Mutex::new(None), dir };
        if catalog {
            started.seed(designated).await;
        }
        started
    }
}

/// Short cadences, so scenarios observe seeded kinds and changes promptly.
fn harness_options() -> RuntimeOptions {
    RuntimeOptions {
        heartbeat_interval: Duration::from_secs(5),
        controller_resync_interval: Duration::from_millis(200),
        // The dummy kinds are seeded after startup; observe them on the next pass.
        fulfilment_probe_interval: Duration::from_millis(200),
        ..RuntimeOptions::default()
    }
}

async fn compose(
    composition: &Composition,
    config: Arc<ConfigStore>,
    discovery: DiscoveryRuntime,
    relay: &RestartRelay,
) -> (Arc<InProcessDaemon>, DaemonRuntime) {
    let daemon = InProcessDaemon::new_with_resource_backend_and_clock(
        vec![],
        Arc::clone(&config),
        discovery,
        composition.host.clone(),
        composition.backend.clone(),
        Arc::clone(&composition.clock),
        Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    relay.connect(&daemon);
    let runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), config, None, composition.options.clone()).await.expect("runtime");
    (daemon, runtime)
}

impl TestDaemon {
    pub fn daemon(&self) -> &Arc<InProcessDaemon> {
        &self.daemon
    }

    pub fn fleet(&self) -> &Arc<DummyFleet> {
        &self.fleet
    }

    pub fn snapshot(&self) -> DummyFleetSnapshot {
        self.fleet.snapshot()
    }

    pub fn backend(&self) -> ResourceBackend {
        self.daemon.resource_backend()
    }

    pub fn runtime(&self) -> &DaemonRuntime {
        self.runtime.as_ref().expect("a running daemon")
    }

    /// The SQLite store, for a daemon started with [`TestStore::Sqlite`].
    pub fn sqlite(&self) -> Option<&SqliteBackend> {
        self.sqlite.as_ref()
    }

    /// The SQLite store's file, for a daemon started with [`TestStore::Sqlite`].
    pub fn database_path(&self) -> Option<PathBuf> {
        self.sqlite.as_ref().map(|_| ConfigStore::with_base(self.dir.path().join("config")).state_dir().as_path().join("resources.sqlite"))
    }

    /// Restart the daemon over the same store, as a process restart does.
    /// Vendor backings and crew processes survive; the new daemon adopts them.
    pub async fn restart(self) -> Self {
        let Self { daemon, fleet, runtime, relay, composition, sqlite, operator, dir } = self;
        drop(operator);
        relay.disconnect();
        let node = daemon.node_id().clone();
        if let Some(runtime) = runtime {
            runtime.shutdown();
        }
        drop(daemon);
        tokio::task::yield_now().await;
        let config = Arc::new(ConfigStore::with_base(dir.path().join("config")));
        let mut discovery = (composition.discovery)(dir.path());
        fleet.install(&mut discovery);
        let (daemon, runtime) = compose(&composition, config, discovery, &relay).await;
        assert_eq!(daemon.node_id(), &node, "restart preserves the daemon's identity");
        Self { daemon, fleet, runtime: Some(runtime), relay, composition, sqlite, operator: Mutex::new(None), dir }
    }

    /// Record the fleet designation the engine resolves its roles from.
    pub async fn designate(&self) {
        self.backend()
            .definitions::<FleetDesignation>(NAMESPACE)
            .create(
                &InputMeta::builder().name(FLEET_DESIGNATION_NAME.to_string()).build(),
                &FleetDesignationSpec { fleet: Some("lab".into()), project: "fleet".into(), image_cache: None },
            )
            .await
            .expect("fleet designation");
    }

    async fn seed(&self, designated: bool) {
        let backend = self.backend();
        if self.daemon.message_primitives().is_some() {
            // Fleet identity and charter Project are declared independently.
            backend
                .clone()
                .using::<Project>(NAMESPACE)
                .create(
                    &InputMeta::builder().name("fleet".to_string()).build(),
                    &ProjectSpec::builder()
                        .display_name("Fleet".to_string())
                        .default_workflow_ref(CrewMix::Coder.workflow_name().to_string())
                        .repositories(Vec::new())
                        .build(),
                )
                .await
                .expect("fleet project");
            if designated {
                self.designate().await;
            }
        }
        let host = backend.clone().using::<Host>(NAMESPACE).list().await.expect("hosts").items.into_iter().next().expect("local host");
        let forge = catalog::forge(&self.fleet.config().forge_service_url);
        backend
            .clone()
            .using::<Forge>(NAMESPACE)
            .create(&InputMeta::builder().name(forge.forge_id.clone()).build(), &forge)
            .await
            .expect("dummy forge");
        // A dispatched repository is tracked first; admission reads its resolved default branch.
        let repository = catalog::repository(&forge).expect("dummy repository");
        let repositories = backend.clone().using::<Repository>(NAMESPACE);
        repositories.create(&InputMeta::builder().name(repository.key().to_string()).build(), &repository).await.expect("repository");
        let kinds = backend.clone().using::<FulfilmentKind>(NAMESPACE);
        kinds
            .create(&InputMeta::builder().name(DUMMY_FULFILMENT_KIND.to_string()).build(), &catalog::fulfilment_kind(&host.metadata.name))
            .await
            .expect("dummy fulfilment kind");
        kinds
            .create(
                &InputMeta::builder().name(DUMMY_CLOUD_FULFILMENT_KIND.to_string()).build(),
                &catalog::cloud_fulfilment_kind(&host.metadata.name),
            )
            .await
            .expect("dummy cloud fulfilment kind");
        let turns = self.fleet.config().agent.long_runner_turns;
        for mix in CrewMix::ALL {
            backend
                .clone()
                .using::<WorkflowTemplate>(NAMESPACE)
                .create(&InputMeta::builder().name(mix.workflow_name().to_string()).build(), &mix.workflow(turns))
                .await
                .expect("dummy workflow");
        }
        let key = repository.key().to_string();
        self.wait_for_resource::<Repository>(&key, "a resolved default branch", |repository| {
            repository.status.as_ref().is_some_and(|status| status.default_branch.is_some())
        })
        .within(SEED_WAIT)
        .await;
        // Admission places a literal image only once its host has probed the image's harnesses.
        self.wait_for_resource::<Host>(&host.metadata.name, "the dummy kind's harness", |host| {
            host.status.as_ref().is_some_and(|status| {
                status.fulfilment_facts.get(DUMMY_FULFILMENT_KIND).is_some_and(|facts| facts.harnesses.contains_key(DUMMY_AGENT_ADAPTER))
            })
        })
        .within(SEED_WAIT)
        .await;
    }

    /// Execute as the daemon's own process does, with no calling principal.
    pub async fn execute(&self, action: CommandAction) -> CommandValue {
        self.execute_as(action, None).await
    }

    /// Execute as an operator, through the request dispatcher a socket client uses.
    pub async fn execute_as_operator(&self, action: CommandAction) -> CommandValue {
        self.client().await.execute(action).await
    }

    /// Execute `command` in process, with its own targeting and no calling principal.
    pub async fn execute_command(&self, command: flotilla_protocol::Command) -> CommandValue {
        let mut events = self.daemon.subscribe();
        let id = self.daemon.execute(command).await.expect("execute");
        command_result(&mut events, id).await
    }

    /// Execute in process for `principal`, bypassing the request dispatcher.
    pub async fn execute_as(&self, action: CommandAction, principal: Option<PrincipalRef>) -> CommandValue {
        let mut events = self.daemon.subscribe();
        let command = flotilla_protocol::Command::builder().action(action).build();
        let id = self.daemon.execute_for_principal(command, principal).await.expect("execute");
        command_result(&mut events, id).await
    }

    /// An operator's client of this daemon: the framed request protocol a
    /// socket client speaks, served by the daemon's request dispatcher. The
    /// same client serves every call until the daemon restarts.
    pub async fn client(&self) -> Arc<TestClient> {
        if let Some(client) = self.operator.lock().expect("operator client").clone() {
            return client;
        }
        let client = Arc::new(TestClient::connect(&self.daemon).await);
        Arc::clone(self.operator.lock().expect("operator client").get_or_insert(client))
    }

    /// Dispatch a convoy of `mix` onto the dummy kind, as an operator.
    pub async fn dispatch(&self, name: &str, mix: CrewMix) {
        self.dispatch_onto(name, mix, DUMMY_FULFILMENT_KIND).await;
    }

    /// Dispatch a convoy of `mix` onto the dummy cloud sandbox kind.
    pub async fn dispatch_to_cloud(&self, name: &str, mix: CrewMix) {
        self.dispatch_onto(name, mix, DUMMY_CLOUD_FULFILMENT_KIND).await;
    }

    async fn dispatch_onto(&self, name: &str, mix: CrewMix, kind: &str) {
        let forge = catalog::forge(&self.fleet.config().forge_service_url);
        let created = self
            .execute(CommandAction::ConvoyCreate {
                name: name.into(),
                workflow_ref: mix.workflow_name().into(),
                inputs: Vec::new(),
                repository_url: Some(catalog::repository_url(&forge)),
                r#ref: Some("main".into()),
                project_ref: None,
                placement_policy: Some(kind.into()),
                adopted_checkout: None,
            })
            .await;
        assert_eq!(created, CommandValue::ConvoyCreated { name: name.into() });
    }

    /// The convoy dispatched as `name`; its resource name differs.
    pub async fn convoy(&self, name: &str) -> Option<ResourceObject<Convoy>> {
        let convoys = self.backend().using::<Convoy>(NAMESPACE).list().await.expect("convoys");
        convoys.items.into_iter().find(|convoy| convoy.metadata.labels.values().any(|value| value == name))
    }

    /// Wait until `done` holds for the convoy dispatched as `name`, reporting where it stopped.
    pub async fn wait_for(&self, name: &str, what: &str, done: impl Fn(&ResourceObject<Convoy>) -> bool) -> ResourceObject<Convoy> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let convoy = self.convoy(name).await;
            match convoy {
                Some(convoy) if done(&convoy) => return convoy,
                convoy if tokio::time::Instant::now() > deadline => {
                    let status = convoy.and_then(|convoy| convoy.status);
                    panic!(
                        "convoy {name} never reached {what}: phase {:?}, message {:?}, crew {:?}, stall {:?}, supervision {:?}, fleet {:?}",
                        status.as_ref().map(|status| status.phase),
                        status.as_ref().and_then(|status| status.message.clone()),
                        status.as_ref().map(|status| &status.crew_work),
                        status.as_ref().and_then(|status| status.stalled.as_ref()),
                        status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).and_then(|w| w.supervision.as_ref()),
                        self.fleet.snapshot()
                    );
                }
                _ => tokio::time::sleep(POLL).await,
            }
        }
    }

    pub async fn wait_for_phase(&self, name: &str, phase: ConvoyPhase) -> ResourceObject<Convoy> {
        self.wait_for(name, &format!("{phase:?}"), |convoy| convoy.status.as_ref().is_some_and(|status| status.phase == phase)).await
    }

    /// Wait for the lone `coder` of the `implement` vessel to reach `phase`.
    pub async fn wait_for_crew(&self, name: &str, phase: CrewWorkPhase) -> ResourceObject<Convoy> {
        self.wait_for(name, &format!("coder {phase:?}"), |convoy| {
            convoy.status.as_ref().is_some_and(|status| {
                status.crew_work.get("implement").and_then(|roles| roles.get("coder")).is_some_and(|work| work.phase == phase)
            })
        })
        .await
    }

    /// A crew stall a supervisor can act on: the crew's work and the convoy's
    /// stall condition, both published by the crew and maintained by the leaf engine.
    pub async fn wait_for_stall(&self, name: &str) -> ResourceObject<Convoy> {
        self.wait_for(name, "a supervisable stall", |convoy| {
            convoy.status.as_ref().is_some_and(|status| {
                status.stalled.is_some()
                    && status
                        .crew_work
                        .get("implement")
                        .and_then(|roles| roles.get("coder"))
                        .is_some_and(|work| work.phase == CrewWorkPhase::Stalled)
            })
        })
        .await
    }

    /// Dispatch `name` and wait for it to land.
    pub async fn deliver(&self, name: &str, mix: CrewMix) -> ResourceObject<Convoy> {
        self.dispatch(name, mix).await;
        self.wait_for_phase(name, ConvoyPhase::Landed).await
    }

    /// Wait until the `T` named `name` exists and `done` holds for it.
    pub fn wait_for_resource<'a, T: Resource + std::fmt::Debug>(
        &'a self,
        name: &'a str,
        what: &'a str,
        done: impl Fn(&ResourceObject<T>) -> bool + 'a,
    ) -> Wait<'a, ResourceObject<T>> {
        let backend = self.backend();
        Wait::new(move |deadline| async move {
            let resources = backend.using::<T>(NAMESPACE);
            loop {
                let last = match resources.get(name).await {
                    Ok(resource) if done(&resource) => return resource,
                    Ok(resource) => Some(resource),
                    Err(ResourceError::NotFound { .. }) => None,
                    Err(error) => panic!("read {} {name}: {error}", T::API_PATHS.kind),
                };
                if tokio::time::Instant::now() > deadline {
                    panic!("{} {name} never reached {what}; last state: {last:#?}", T::API_PATHS.kind);
                }
                tokio::time::sleep(POLL).await;
            }
        })
    }

    /// Wait until some `T` satisfies `done`, returning the first that does.
    pub fn wait_for_any<'a, T: Resource + std::fmt::Debug>(
        &'a self,
        what: &'a str,
        done: impl Fn(&ResourceObject<T>) -> bool + 'a,
    ) -> Wait<'a, ResourceObject<T>> {
        let backend = self.backend();
        Wait::new(move |deadline| async move {
            let resources = backend.using::<T>(NAMESPACE);
            loop {
                let mut items = resources.list().await.unwrap_or_else(|error| panic!("list {}: {error}", T::API_PATHS.kind)).items;
                if let Some(found) = items.iter().position(&done) {
                    return items.swap_remove(found);
                }
                if tokio::time::Instant::now() > deadline {
                    panic!("no {} reached {what}; last states: {items:#?}", T::API_PATHS.kind);
                }
                tokio::time::sleep(POLL).await;
            }
        })
    }

    /// Wait until the `T` named `name` is gone from the store.
    pub fn wait_for_absence<'a, T: Resource + std::fmt::Debug>(&'a self, name: &'a str) -> Wait<'a, ()> {
        let backend = self.backend();
        Wait::new(move |deadline| async move {
            let resources = backend.using::<T>(NAMESPACE);
            loop {
                let last = match resources.get(name).await {
                    Err(ResourceError::NotFound { .. }) => return,
                    Ok(resource) => resource,
                    Err(error) => panic!("read {} {name}: {error}", T::API_PATHS.kind),
                };
                if tokio::time::Instant::now() > deadline {
                    panic!("{} {name} was never removed; last state: {last:#?}", T::API_PATHS.kind);
                }
                tokio::time::sleep(POLL).await;
            }
        })
    }
}

/// A wait, [`WAIT`] long unless [`Wait::within`] says otherwise. Awaiting it
/// returns what it waited for, or panics with the last state it saw.
type WaitFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'a>>;

pub struct Wait<'a, T> {
    run: Box<dyn FnOnce(tokio::time::Instant) -> WaitFuture<'a, T> + 'a>,
    within: Duration,
}

impl<'a, T: 'a> Wait<'a, T> {
    fn new<F>(run: impl FnOnce(tokio::time::Instant) -> F + 'a) -> Self
    where
        F: std::future::Future<Output = T> + 'a,
    {
        Self { run: Box::new(move |deadline| Box::pin(run(deadline))), within: WAIT }
    }

    pub fn within(mut self, within: Duration) -> Self {
        self.within = within;
        self
    }
}

impl<'a, T: 'a> std::future::IntoFuture for Wait<'a, T> {
    type Output = T;
    type IntoFuture = WaitFuture<'a, T>;

    fn into_future(self) -> Self::IntoFuture {
        (self.run)(tokio::time::Instant::now() + self.within)
    }
}

async fn command_result(events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>, id: u64) -> CommandValue {
    tokio::time::timeout(WAIT, async {
        loop {
            match events.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id, result, .. }) if command_id == id => return result,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => panic!("the daemon closed before command {id} finished"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("command {id} never finished"))
}

/// An operator's connection to a test daemon: a framed client session served
/// by the daemon's request dispatcher, as its socket serves one.
pub struct TestClient {
    client: Arc<SocketDaemon>,
    _shutdown: watch::Sender<bool>,
    session: tokio::task::JoinHandle<()>,
}

impl Drop for TestClient {
    fn drop(&mut self) {
        self.session.abort();
    }
}

impl TestClient {
    async fn connect(daemon: &Arc<InProcessDaemon>) -> Self {
        let peer_manager = Arc::new(tokio::sync::Mutex::new(PeerManager::new(daemon.node_id().clone())));
        let router = build_remote_command_router(daemon, &peer_manager);
        let (client_session, server_session) = flotilla_transport::message::message_session_pair();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (shutdown_request_tx, _shutdown_requests) = mpsc::unbounded_channel();
        let (inbound_peer_tx, _inbound_peers) = mpsc::channel(16);
        let (peer_connected_tx, _peer_connections) = mpsc::unbounded_channel::<PeerConnectionEvent>();
        let namespace = daemon.provisioning_namespace().await;
        // The principal the main socket resolves for an operator's process.
        let caller = CommandCaller { principal_ref: PrincipalRef::implicit_for_namespace(&namespace), process: None, crew: None };
        let daemon = Arc::clone(daemon);
        let session = tokio::spawn(async move {
            flotilla_daemon::server::handle_client_session_with_caller(
                server_session,
                daemon,
                shutdown_request_tx,
                shutdown_rx,
                inbound_peer_tx,
                peer_manager,
                router,
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                Arc::new(Notify::new()),
                peer_connected_tx,
                flotilla_agent::shared_in_memory_agent_state_store(),
                None,
                Some(caller),
            )
            .await;
        });
        let client = SocketDaemon::from_session_stateful(client_session).await.expect("client handshake");
        Self { client, _shutdown: shutdown, session }
    }

    /// Send `action` and return its value, as `flotilla` does for each command.
    pub async fn execute(&self, action: CommandAction) -> CommandValue {
        let command = flotilla_protocol::Command::builder().action(action).build();
        if command.action.is_query() {
            return self
                .client
                .execute_query(command, uuid::Uuid::new_v4())
                .await
                .unwrap_or_else(|message| CommandValue::Error { message });
        }
        let mut events = self.client.subscribe();
        match self.client.execute(command).await {
            Ok(id) => command_result(&mut events, id).await,
            Err(message) => CommandValue::Error { message },
        }
    }
}

/// Stands in for the reconnecting command socket at the process boundary: the
/// dummy crews' processes survive a restart, while their daemon endpoint is
/// replaced. Commands sent while no daemon runs wait for the next one.
#[derive(Default)]
struct RestartRelay {
    port: std::sync::Mutex<Option<Arc<dyn CrewCommandPort>>>,
    ready: Notify,
}

impl RestartRelay {
    fn connect(&self, daemon: &Arc<InProcessDaemon>) {
        *self.port.lock().expect("relay") = Some(daemon_crew_port(daemon));
        self.ready.notify_waiters();
    }

    fn disconnect(&self) {
        *self.port.lock().expect("relay") = None;
    }

    async fn port(&self) -> Arc<dyn CrewCommandPort> {
        loop {
            let ready = self.ready.notified();
            if let Some(port) = self.port.lock().expect("relay").clone() {
                return port;
            }
            ready.await;
        }
    }
}

#[async_trait]
impl CrewCommandPort for RestartRelay {
    async fn execute(&self, action: CommandAction) -> Result<CommandValue, String> {
        self.port().await.execute(action).await
    }

    async fn report_attention(&self, namespace: &str, session: &str, working: bool) -> Result<(), String> {
        self.port().await.report_attention(namespace, session, working).await
    }
}
