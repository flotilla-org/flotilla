//! A daemon composed as `flotillad` composes one, for behaviour tests.
//!
//! [`TestDaemon`] writes a `daemon.toml` whose `[dummy_fleet]` section selects
//! the fakes, then starts the production composition through the
//! composition-root switch: the same controllers, startup restoration and
//! periodic tasks as `flotillad`. The only fakes are the dummy fleet's
//! environment vendors, forge, VCS, terminal pool and agent. Tests touch what
//! an operator, a crew or a vendor touches:
//!
//! - commands and resources, as an operator through the request dispatcher a
//!   socket client uses ([`TestDaemon::execute`], [`TestDaemon::get`],
//!   [`TestDaemon::apply`] and the waits);
//! - the environment vendor, through [`TestDaemon::vendor`];
//! - real forges, when `.forges(..)` installs the GitHub and Forgejo
//!   providers behind a test's transports;
//! - the daemon process, through [`TestDaemon::restart`] over the same store,
//!   while vendor backings and running crews survive;
//! - another daemon, through [`TestDaemon::peer`], as the daemon's own peer
//!   runtime connects to one.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_client::{
    resource::{ResourceClient, ResourceGetRequest, ResourceListRequest},
    SocketDaemon,
};
use flotilla_config::{ConfigStore, DaemonConfig};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon::{
    dummy_fleet::{daemon_crew_port, install_dummy_fleet},
    peer::PeerManager,
    runtime::{DaemonRuntime, RuntimeOptions},
    server::{build_remote_command_router, PeerConnectionEvent},
};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_discovery_api::detectors::{parse_first_dotted_version, CommandDetector};
use flotilla_discovery_testkit::fake_discovery_with_host_directories;
use flotilla_dummy_fleet::{
    catalog::{self, CrewMix, DUMMY_CLOUD_FULFILMENT_KIND, DUMMY_FULFILMENT_KIND},
    CrewCommandPort, DummyFleet, DummyFleetSnapshot, DummyVendor, DUMMY_AGENT_ADAPTER,
};
use flotilla_forgejo_providers::factories::{change_request::ForgejoChangeRequestFactory, issue_tracker::ForgejoIssueProviderFactory};
use flotilla_github_providers::factories::{change_request::GitHubChangeRequestFactory, issue_tracker::GitHubIssueProviderFactory};
use flotilla_protocol::{
    commands::CommandValue, CommandAction, CommandCaller, DaemonEvent, HostName, PrincipalRef, QueryCursor, QueryId, RepoSelector,
    ResultSet,
};
use flotilla_provider_runtime::{CommandRunner, HttpClient};
use flotilla_resources::{
    Clock, Convoy, FleetDesignation, FleetDesignationSpec, Forge, FulfilmentKind, Host, K8sResourceObject, Project, ProjectSpec,
    Repository, RepositoryKey, Resource, ResourceObject, SystemClock, WorkflowTemplate, FLEET_DESIGNATION_NAME,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use tokio::sync::{mpsc, watch, Notify};

/// The namespace every test daemon provisions in.
pub const NAMESPACE: &str = "flotilla";
/// Long runners' turns unless `.agent(..)` sets `long_runner_turns`.
const LONG_RUNNER_TURNS: u32 = 2;
const WAIT: Duration = Duration::from_secs(30);
const SEED_WAIT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(100);
const HARNESS_RESYNC: Duration = Duration::from_millis(200);

/// What a restart rebuilds the daemon from.
struct Composition {
    host: HostName,
    clock: Arc<dyn Clock>,
    backend: ResourceBackend,
    resync: Duration,
    forges: Option<ForgeTransport>,
}

/// The transports the real forge providers reach their forges through:
/// Forgejo's HTTP requests and the host's `gh` and `git` commands.
pub struct ForgeTransport {
    pub http: Arc<dyn HttpClient>,
    pub commands: Arc<dyn CommandRunner>,
}

pub struct TestDaemon {
    daemon: Arc<InProcessDaemon>,
    fleet: Arc<DummyFleet>,
    runtime: Option<DaemonRuntime>,
    relay: Arc<RestartRelay>,
    operator: Operator,
    composition: Composition,
    dispatched: AtomicU64,
    dir: tempfile::TempDir,
}

#[bon::bon]
impl TestDaemon {
    /// Start a daemon whose `daemon.toml` enables the dummy fleet with these
    /// sections, then seed the dummy catalog and wait until its kinds can admit.
    #[builder(start_fn = builder, finish_fn = start)]
    pub async fn start_daemon(
        /// `[dummy_fleet.agent]`, such as `stall_rate = 1.0`.
        #[builder(default)]
        agent: &str,
        /// `[dummy_fleet.environment]`, such as `failure_rate = 1.0`.
        #[builder(default)]
        environment: &str,
        /// `[dummy_fleet.change_request]`.
        #[builder(default)]
        change_request: &str,
        /// The rest of `daemon.toml`, such as `[primitives]` or `host_name`.
        #[builder(default)]
        settings: &str,
        clock: Option<Arc<dyn Clock>>,
        /// The controllers' resync interval; the harness's is short, so
        /// scenarios observe seeded kinds and changes promptly.
        #[builder(default = HARNESS_RESYNC)]
        resync: Duration,
        /// Install the GitHub and Forgejo providers behind these transports,
        /// as on a host with `gh` and forge credentials. The dummy fleet makes
        /// them read-only, as it does on such a host.
        forges: Option<ForgeTransport>,
    ) -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(config_dir.join("daemon.toml"), daemon_toml(settings, agent, environment, change_request)).expect("daemon.toml");
        let config = Arc::new(ConfigStore::with_base(config_dir));
        let daemon_config: DaemonConfig = config.load_daemon_config().expect("daemon config");
        let composition = Composition {
            host: HostName::new(daemon_config.host_name.as_deref().expect("a host name")),
            clock: clock.unwrap_or_else(|| Arc::new(SystemClock)),
            backend: ResourceBackend::new(InMemoryBackend::default()),
            resync,
            forges,
        };
        let relay = Arc::new(RestartRelay::default());
        let mut discovery = discovery(&dir, composition.forges.as_ref());
        let fleet = install_dummy_fleet(&daemon_config, &mut discovery).expect("dummy fleet configured");
        fleet.attach(Arc::clone(&relay) as Arc<dyn CrewCommandPort>).expect("attach the crews' command relay");
        let (daemon, runtime) = compose(&composition, config, discovery, &relay).await;
        let operator = Operator::connect(&daemon).await;
        let started = Self { daemon, fleet, runtime: Some(runtime), relay, operator, composition, dispatched: AtomicU64::new(0), dir };
        started.seed(daemon_config.primitives.is_some()).await;
        started
    }
}

/// `daemon.toml`: `settings`, with the harness's host name unless it names
/// one, then the dummy fleet's sections.
fn daemon_toml(settings: &str, agent: &str, environment: &str, change_request: &str) -> String {
    let table = |section: &str, lines: &str| -> toml::Table { lines.parse().unwrap_or_else(|error| panic!("{section}: {error}")) };
    let mut document = table("settings", settings);
    let host = document.get("host_name").and_then(toml::Value::as_str).unwrap_or("dummy-load").to_string();
    document.entry("host_name").or_insert_with(|| host.clone().into());
    document.entry("machine_id").or_insert_with(|| host.into());
    let mut agent = table("agent", agent);
    agent.entry("long_runner_turns").or_insert_with(|| i64::from(LONG_RUNNER_TURNS).into());
    let mut dummy_fleet = toml::Table::new();
    dummy_fleet.insert("agent".into(), agent.into());
    dummy_fleet.insert("environment".into(), table("environment", environment).into());
    dummy_fleet.insert("change_request".into(), table("change_request", change_request).into());
    document.insert("dummy_fleet".into(), dummy_fleet.into());
    toml::to_string(&document).expect("daemon.toml")
}

/// Short cadences, so scenarios observe seeded kinds and changes promptly.
fn harness_options(resync: Duration) -> RuntimeOptions {
    RuntimeOptions {
        namespace: NAMESPACE.into(),
        heartbeat_interval: Duration::from_secs(5),
        controller_resync_interval: resync,
        // The dummy kinds are seeded after startup; observe them on the next pass.
        fulfilment_probe_interval: Duration::from_millis(200),
        ..RuntimeOptions::default()
    }
}

/// The host's discovery: its directories under `dir`, and the real forge
/// providers behind `forges`.
fn discovery(dir: &tempfile::TempDir, forges: Option<&ForgeTransport>) -> flotilla_discovery::DiscoveryRuntime {
    let mut discovery = fake_discovery_with_host_directories(dir.path());
    let Some(forges) = forges else { return discovery };
    discovery.runner = Arc::clone(&forges.commands);
    discovery.http = Arc::clone(&forges.http);
    let detectors = Arc::get_mut(&mut discovery.host_detectors).expect("unshared detectors");
    detectors.push(Box::new(CommandDetector::new("gh", &["--version"], parse_first_dotted_version)));
    let factories = &mut discovery.factories;
    factories.change_requests =
        vec![Box::new(GitHubChangeRequestFactory), Box::new(ForgejoChangeRequestFactory::new(Arc::clone(&forges.http)))];
    factories.issue_trackers =
        vec![Box::new(GitHubIssueProviderFactory), Box::new(ForgejoIssueProviderFactory::new(Arc::clone(&forges.http)))];
    discovery
}

async fn compose(
    composition: &Composition,
    config: Arc<ConfigStore>,
    discovery: flotilla_discovery::DiscoveryRuntime,
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
    let runtime =
        DaemonRuntime::start_with_options(Arc::clone(&daemon), config, None, harness_options(composition.resync)).await.expect("runtime");
    (daemon, runtime)
}

impl TestDaemon {
    /// Restart the daemon over the same store, as a process restart does.
    /// Vendor backings and crew processes survive; the new daemon adopts them.
    pub async fn restart(self) -> Self {
        let Self { daemon, fleet, runtime, relay, operator, composition, dispatched, dir } = self;
        drop(operator);
        relay.disconnect();
        let node = daemon.node_id().clone();
        if let Some(runtime) = runtime {
            runtime.shutdown();
        }
        drop(daemon);
        tokio::task::yield_now().await;
        let config = Arc::new(ConfigStore::with_base(dir.path().join("config")));
        let mut discovery = discovery(&dir, composition.forges.as_ref());
        fleet.install(&mut discovery);
        let (daemon, runtime) = compose(&composition, config, discovery, &relay).await;
        assert_eq!(daemon.node_id(), &node, "restart preserves the daemon's identity");
        let operator = Operator::connect(&daemon).await;
        Self { daemon, fleet, runtime: Some(runtime), relay, operator, composition, dispatched, dir }
    }

    /// Execute `action` as an operator: through the request dispatcher, as the
    /// CLI's commands do. A refusal or failure is a [`CommandValue::Error`].
    pub async fn execute(&self, action: CommandAction) -> CommandValue {
        self.operator.execute(action, None).await
    }

    /// [`TestDaemon::execute`] in `repository`, as the CLI sends a command
    /// with its `context_repo`.
    pub async fn execute_in(&self, repository: &RepositoryKey, action: CommandAction) -> CommandValue {
        self.operator.execute(action, Some(RepoSelector::Repository(repository.clone()))).await
    }

    /// Dispatch a convoy of `mix` onto the fulfilment kind `kind`, and return
    /// the convoy's resource name.
    pub async fn dispatch(&self, mix: CrewMix, kind: &str) -> String {
        let role = format!("{}-{}", mix.workflow_name(), self.dispatched.fetch_add(1, Ordering::Relaxed));
        let forge = catalog::forge(&self.fleet.config().forge_service_url);
        let created = self
            .execute(CommandAction::ConvoyCreate {
                name: role.clone(),
                workflow_ref: mix.workflow_name().into(),
                inputs: Vec::new(),
                repository_url: Some(catalog::repository_url(&forge)),
                r#ref: Some("main".into()),
                project_ref: None,
                placement_policy: Some(kind.into()),
                adopted_checkout: None,
            })
            .await;
        assert!(matches!(created, CommandValue::ConvoyCreated { .. }), "dispatch {role}: {created:?}");
        let convoys = self.list::<Convoy>().await;
        convoys.into_iter().find(|convoy| convoy.spec.role == role).unwrap_or_else(|| panic!("no convoy {role}")).metadata.name
    }

    /// The `T` named `name`, as `flotilla get` reads it.
    pub async fn get<T: Resource>(&self, name: &str) -> Option<ResourceObject<T>> {
        let request = ResourceGetRequest::builder().kind(T::API_PATHS.kind.into()).name(name.into()).namespace(NAMESPACE.into()).build();
        match self.operator.resources.get(request).await {
            Ok(read) => read.records.into_iter().find_map(|record| record.object).map(decode),
            Err(message) if message.contains("not found") => None,
            Err(message) => panic!("get {} {name}: {message}", T::API_PATHS.kind),
        }
    }

    /// Every `T`, as `flotilla get` lists them: with the replicas of every
    /// peer's.
    pub async fn list<T: Resource>(&self) -> Vec<ResourceObject<T>> {
        let request =
            ResourceListRequest::builder().kind(T::API_PATHS.kind.into()).namespace(NAMESPACE.into()).include_replicas(true).build();
        let read = self.operator.resources.list(request).await.unwrap_or_else(|message| panic!("list {}: {message}", T::API_PATHS.kind));
        read.records.into_iter().filter_map(|record| record.object).map(decode).collect()
    }

    /// Create or update the `T` named `name`, as `flotilla apply` does.
    pub async fn apply<T: Resource>(&self, name: &str, spec: T::Spec) {
        let document = serde_json::json!({
            "apiVersion": format!("{}/{}", T::API_PATHS.group, T::API_PATHS.version),
            "kind": T::API_PATHS.kind,
            "metadata": { "name": name, "namespace": NAMESPACE },
            "spec": spec,
        });
        let applied = self.execute(CommandAction::ResourceApply { namespace: NAMESPACE.into(), document }).await;
        assert!(matches!(applied, CommandValue::ResourceObject(_)), "apply {} {name}: {applied:?}", T::API_PATHS.kind);
    }

    /// Wait until some `T` satisfies `done`, and return the first that does.
    /// The wait fails with every `T`'s last state.
    pub fn wait<'a, T: Resource>(&'a self, what: &'a str, done: impl Fn(&ResourceObject<T>) -> bool + 'a) -> Wait<'a, ResourceObject<T>> {
        Wait::new(WAIT, async move |deadline| loop {
            let mut items = self.list::<T>().await;
            if let Some(found) = items.iter().position(&done) {
                return items.swap_remove(found);
            }
            if tokio::time::Instant::now() > deadline {
                panic!("no {} reached {what}; last states: {}; fleet: {:?}", T::API_PATHS.kind, shown(&items), self.counts());
            }
            tokio::time::sleep(POLL).await;
        })
    }

    /// Wait until the `T` named `name` is gone.
    pub fn wait_gone<'a, T: Resource>(&'a self, name: &'a str) -> Wait<'a, ()> {
        Wait::new(WAIT, async move |deadline| loop {
            let Some(last) = self.get::<T>(name).await else { return };
            if tokio::time::Instant::now() > deadline {
                panic!("{} {name} was never removed; last state: {}", T::API_PATHS.kind, shown(&last));
            }
            tokio::time::sleep(POLL).await;
        })
    }

    /// Wait until the client query `query` has a result set that satisfies
    /// `done`, subscribing to it as a client does. The wait fails with the
    /// last result set.
    pub fn wait_query<'a>(&'a self, query: QueryId, done: impl Fn(&ResultSet) -> bool + 'a) -> Wait<'a, ResultSet> {
        Wait::new(WAIT, async move |deadline| {
            let mut events = self.operator.client.subscribe();
            let cursors = [QueryCursor { query: query.clone(), since: None }];
            let current = self.operator.client.subscribe_queries(uuid::Uuid::new_v4(), &cursors).await.expect("subscribe");
            let mut last = None;
            for event in current {
                if let DaemonEvent::ResultSet(result) = event {
                    if result.query() == query {
                        last = Some(*result);
                    }
                }
            }
            loop {
                if let Some(result) = last.take_if(|result| done(result)) {
                    return result;
                }
                let next = match tokio::time::timeout_at(deadline, events.recv()).await {
                    Ok(next) => next,
                    Err(_) => panic!("no {query:?} result set satisfied the wait; last: {}", shown(&last)),
                };
                match next {
                    Ok(DaemonEvent::ResultSet(result)) if result.query() == query => last = Some(*result),
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => panic!("the daemon closed while waiting on {query:?}"),
                }
            }
        })
    }

    /// Connect `a` and `b` as the daemon's peer runtime connects two hosts.
    /// Dropping the [`Peering`] disconnects them.
    pub async fn peer(a: &TestDaemon, b: &TestDaemon) -> Peering {
        let topology = super::server::spawn_in_memory_request_topology_stateful(Arc::clone(&a.daemon), Arc::clone(&b.daemon))
            .await
            .expect("peer runtimes");
        Peering { _topology: topology }
    }

    /// The environment vendors: what a test does as the vendor is all the
    /// daemon learns of it.
    pub fn vendor(&self) -> &DummyVendor {
        self.fleet.vendor()
    }

    /// What the fakes counted.
    pub fn counts(&self) -> DummyFleetSnapshot {
        self.fleet.snapshot()
    }

    /// The dummy catalog, applied as an operator applies one: a Fleet project
    /// and its designation, the forge, repository, kinds and workflows.
    async fn seed(&self, primitives: bool) {
        if primitives {
            // Fleet identity and charter Project are declared independently.
            let fleet = ProjectSpec::builder()
                .display_name("Fleet".to_string())
                .default_workflow_ref(CrewMix::Coder.workflow_name().to_string())
                .repositories(Vec::new())
                .build();
            self.apply::<Project>("fleet", fleet).await;
            let designation = FleetDesignationSpec { fleet: Some("lab".into()), project: "fleet".into(), image_cache: None };
            self.apply::<FleetDesignation>(FLEET_DESIGNATION_NAME, designation).await;
        }
        let host = self.list::<Host>().await.into_iter().next().expect("local host").metadata.name;
        let forge = catalog::forge(&self.fleet.config().forge_service_url);
        self.apply::<Forge>(&forge.forge_id.clone(), forge.clone()).await;
        // A dispatched repository is tracked first; admission reads its resolved default branch.
        let repository = catalog::repository(&forge).expect("dummy repository");
        let key = repository.key().to_string();
        self.apply::<Repository>(&key, repository).await;
        self.apply::<FulfilmentKind>(DUMMY_FULFILMENT_KIND, catalog::fulfilment_kind(&host)).await;
        self.apply::<FulfilmentKind>(DUMMY_CLOUD_FULFILMENT_KIND, catalog::cloud_fulfilment_kind(&host)).await;
        let turns = self.fleet.config().agent.long_runner_turns;
        for mix in CrewMix::ALL {
            self.apply::<WorkflowTemplate>(mix.workflow_name(), mix.workflow(turns)).await;
        }
        let resolved = |repository: &ResourceObject<Repository>| {
            repository.metadata.name == key && repository.status.as_ref().is_some_and(|status| status.default_branch.is_some())
        };
        self.wait("a resolved default branch", resolved).within(SEED_WAIT).await;
        // Admission places a literal image only once its host has probed the image's harnesses.
        let probed = |host: &ResourceObject<Host>| {
            host.status.as_ref().is_some_and(|status| {
                status.fulfilment_facts.get(DUMMY_FULFILMENT_KIND).is_some_and(|facts| facts.harnesses.contains_key(DUMMY_AGENT_ADAPTER))
            })
        };
        self.wait("the dummy kind's harness", probed).within(SEED_WAIT).await;
    }
}

/// Two peered daemons; dropping it disconnects them.
pub struct Peering {
    _topology: super::server::InMemoryRequestTopology,
}

fn shown(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|error| format!("<{error}>"))
}

/// A resource as the wire carries it, in its `apiVersion`/`kind` form.
fn decode<T: Resource>(object: serde_json::Value) -> ResourceObject<T> {
    let object: K8sResourceObject<T> =
        serde_json::from_value(object).unwrap_or_else(|error| panic!("decode {}: {error}", T::API_PATHS.kind));
    ResourceObject { metadata: object.metadata.into(), spec: object.spec, status: object.status }
}

/// A wait. Awaiting it returns what it waited for, or panics with the last
/// state it saw.
type WaitFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'a>>;

pub struct Wait<'a, T> {
    run: Box<dyn FnOnce(tokio::time::Instant) -> WaitFuture<'a, T> + 'a>,
    within: Duration,
}

impl<'a, T: 'a> Wait<'a, T> {
    fn new(within: Duration, run: impl AsyncFnOnce(tokio::time::Instant) -> T + 'a) -> Self {
        Self { run: Box::new(move |deadline| Box::pin(run(deadline))), within }
    }

    fn within(mut self, within: Duration) -> Self {
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

/// The operator's connection: a framed client session served by the
/// daemon's request dispatcher, as its socket serves one, with the principal
/// the socket resolves for an operator's process.
struct Operator {
    client: Arc<SocketDaemon>,
    resources: ResourceClient,
    _shutdown: watch::Sender<bool>,
    session: tokio::task::JoinHandle<()>,
}

impl Drop for Operator {
    fn drop(&mut self) {
        self.session.abort();
    }
}

impl Operator {
    async fn connect(daemon: &Arc<InProcessDaemon>) -> Self {
        let peer_manager = Arc::new(tokio::sync::Mutex::new(PeerManager::new(daemon.node_id().clone())));
        let router = build_remote_command_router(daemon, &peer_manager);
        let (client_session, server_session) = flotilla_transport::message::message_session_pair();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (shutdown_request_tx, _shutdown_requests) = mpsc::unbounded_channel();
        let (inbound_peer_tx, _inbound_peers) = mpsc::channel(16);
        let (peer_connected_tx, _peer_connections) = mpsc::unbounded_channel::<PeerConnectionEvent>();
        let namespace = daemon.provisioning_namespace().await;
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
        let resources = ResourceClient::new(Arc::clone(&client) as Arc<dyn DaemonHandle>);
        Self { client, resources, _shutdown: shutdown, session }
    }

    /// Send `action` and return its value, as `flotilla` does for each command.
    async fn execute(&self, action: CommandAction, repository: Option<RepoSelector>) -> CommandValue {
        let command = flotilla_protocol::Command::builder().action(action).maybe_context_repo(repository).build();
        if command.action.is_query() {
            return self
                .client
                .execute_query(command, uuid::Uuid::new_v4())
                .await
                .unwrap_or_else(|message| CommandValue::Error { message });
        }
        let mut events = self.client.subscribe();
        let id = match self.client.execute(command).await {
            Ok(id) => id,
            Err(message) => return CommandValue::Error { message },
        };
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
}

/// Stands in for the reconnecting command socket at the process boundary: the
/// dummy crews' processes survive a restart, while their daemon endpoint is
/// replaced. Commands sent while no daemon runs wait for the next one.
#[derive(Default)]
struct RestartRelay {
    port: Mutex<Option<Arc<dyn CrewCommandPort>>>,
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
