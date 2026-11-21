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
//!   runtime connects to one;
//! - the engine's tables, through [`TestDaemon::rows`], as an operator reads
//!   them over the main socket.
//!

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use flotilla_client::{
    resource::{ResourceClient, ResourceGetRequest, ResourceListRequest, ResourceWatch, ResourceWatchRequest},
    SocketDaemon,
};
use flotilla_config::{ConfigStore, DaemonConfig};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon::{
    dummy_fleet::{dummy_fleet_component, install_dummy_fleet},
    peer::PeerManager,
    runtime::{engine_opener, DaemonRuntime, RuntimeOptions},
    server::{build_remote_command_router, EnvironmentFrontDoor, HookEndpoint, PeerConnectionEvent},
};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_discovery_api::detectors::{parse_first_dotted_version, CommandDetector};
use flotilla_discovery_testkit::fake_discovery_with_host_directories;
use flotilla_dummy_fleet::{
    catalog::{self, CrewMix, DUMMY_CLOUD_FULFILMENT_KIND, DUMMY_FULFILMENT_KIND},
    DummyFleet, DummyFleetSnapshot, DummyVendor, DUMMY_AGENT_ADAPTER,
};
use flotilla_forgejo_providers::factories::{change_request::ForgejoChangeRequestFactory, issue_tracker::ForgejoIssueProviderFactory};
use flotilla_github_providers::factories::{change_request::GitHubChangeRequestFactory, issue_tracker::GitHubIssueProviderFactory};
use flotilla_protocol::{
    commands::CommandValue,
    table::{Message as TableMessage, Row},
    CommandAction, CommandCaller, DaemonEvent, HostName, PrincipalRef, QueryCursor, QueryId, RepoSelector, ResultSet,
};
use flotilla_provider_runtime::{CommandRunner, HttpClient};
use flotilla_resources::{
    Clock, Convoy, FulfilmentKind, Host, K8sResourceObject, Repository, RepositoryKey, Resource, ResourceObject, SystemClock,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use flotilla_transport::traffic::Tracked;
use futures::{FutureExt, StreamExt};
use tokio::sync::{mpsc, watch, Notify};

/// The namespace every test daemon provisions in.
pub const NAMESPACE: &str = "flotilla";
/// Long runners' turns unless `.agent(..)` sets `long_runner_turns`.
const LONG_RUNNER_TURNS: u32 = 2;
/// How long a wait lasts before it fails, in virtual time: a scenario's real time is what its
/// work takes, so the bound is generous, and a wait the system can never satisfy reaches it as
/// soon as nothing nearer is due.
const WAIT: Duration = Duration::from_secs(10 * 60);
const HARNESS_RESYNC: Duration = Duration::from_secs(1);

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
    /// Ends the environment sockets' connections, as the daemon process's end does.
    front_door: watch::Sender<bool>,
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
        /// `[dummy_fleet.terminal]`, such as `refusal_rate = 0.3`.
        #[builder(default)]
        terminal: &str,
        /// `[dummy_fleet.vcs]`, such as `dirty_rate = 1.0`.
        #[builder(default)]
        vcs: &str,
        /// The rest of `daemon.toml`, such as `host_name`.
        #[builder(default)]
        settings: &str,
        /// The controllers' resync interval; the harness's is short, so
        /// scenarios observe seeded kinds and changes within a virtual second.
        #[builder(default = HARNESS_RESYNC)]
        resync: Duration,
        /// Install the GitHub and Forgejo providers behind these transports,
        /// as on a host with `gh` and forge credentials. The dummy fleet makes
        /// them read-only, as it does on such a host.
        forges: Option<ForgeTransport>,
    ) -> Self {
        assert_virtual_time();
        let dir = tempfile::TempDir::new().expect("tempdir");
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(config_dir.join("daemon.toml"), daemon_toml(settings, agent, environment, change_request, terminal, vcs))
            .expect("daemon.toml");
        let config = Arc::new(ConfigStore::with_base(config_dir.clone()));
        let daemon_config: DaemonConfig = config.load_daemon_config().expect("daemon config");
        let composition = Composition {
            host: HostName::new(daemon_config.host_name.as_deref().expect("a host name")),
            // The wall clock as the runtime measures it: the test's virtual time.
            clock: Arc::new(SystemClock),
            backend: ResourceBackend::from_arc(Arc::new(InMemoryBackend::default()) as Arc<dyn flotilla_store::Store>),
            resync,
            forges,
        };
        let mut discovery = discovery(&dir, composition.forges.as_ref());
        let fleet = install_dummy_fleet(&daemon_config, config.state_dir().as_path(), &mut discovery)
            .expect("dummy fleet state")
            .expect("dummy fleet configured");
        fleet.resume();
        let (daemon, runtime, front_door) = compose(&composition, config, discovery, &dir).await;
        let operator = Operator::connect(&daemon).await;
        let started = Self { daemon, fleet, runtime: Some(runtime), front_door, operator, composition, dispatched: AtomicU64::new(0), dir };
        started.seed().await;
        started
    }
}

/// Fails unless the test runs on virtual time: tokio's paused time, on the one thread of a
/// current-thread runtime, where the clock moves only when nothing can run. Paused time stands
/// still across real work, which shows it.
fn assert_virtual_time() {
    let flavor = tokio::runtime::Handle::current().runtime_flavor();
    let before = tokio::time::Instant::now();
    std::thread::sleep(Duration::from_millis(1));
    let paused = tokio::time::Instant::now() == before;
    assert!(
        flavor == tokio::runtime::RuntimeFlavor::CurrentThread && paused,
        "a TestDaemon runs on virtual time: start its test with #[tokio::test(start_paused = true)]"
    );
}

/// `daemon.toml`: `settings`, with the harness's host name unless it names
/// one, then the dummy fleet's sections.
fn daemon_toml(settings: &str, agent: &str, environment: &str, change_request: &str, terminal: &str, vcs: &str) -> String {
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
    dummy_fleet.insert("terminal".into(), table("terminal", terminal).into());
    dummy_fleet.insert("vcs".into(), table("vcs", vcs).into());
    document.insert("dummy_fleet".into(), dummy_fleet.into());
    toml::to_string(&document).expect("daemon.toml")
}

/// Cadences shorter than a host's, so scenarios observe seeded kinds and changes within a few
/// virtual seconds. Each pass is real work, so a scenario's real time grows with its virtual time
/// over the shortest of them.
fn harness_options(resync: Duration) -> RuntimeOptions {
    RuntimeOptions {
        namespace: NAMESPACE.into(),
        heartbeat_interval: Duration::from_secs(5),
        controller_resync_interval: resync,
        // The dummy kinds are seeded after startup; observe them on the next pass.
        fulfilment_probe_interval: Duration::from_secs(1),
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

/// The daemon and its runtime, serving its environment sockets and hook routes as `flotillad`'s
/// server does; the sender ends those sockets' connections.
async fn compose(
    composition: &Composition,
    config: Arc<ConfigStore>,
    discovery: flotilla_discovery::DiscoveryRuntime,
    dir: &tempfile::TempDir,
) -> (Arc<InProcessDaemon>, DaemonRuntime, watch::Sender<bool>) {
    let engine = engine_opener(&config);
    let state_dir = config.state_dir();
    let daemon = InProcessDaemon::new_composed(
        vec![],
        Arc::clone(&config),
        discovery,
        composition.host.clone(),
        composition.backend.clone(),
        Arc::clone(&composition.clock),
        Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        engine,
    )
    .await
    .expect("compose the daemon");
    let hooks = HookEndpoint::load(state_dir.as_path()).expect("hook key");
    let (shutdown, shutdown_rx) = watch::channel(false);
    let directory = flotilla_protocol::DaemonHostPath::new(dir.path().join("env"));
    let front_door = EnvironmentFrontDoor::for_daemon(&daemon, hooks.clone(), directory, shutdown_rx);
    let options = RuntimeOptions {
        environment_sockets: Some(front_door.socket_service()),
        hook_endpoint: Some(hooks),
        ..harness_options(composition.resync)
    };
    let runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), config, None, options)
        .await
        .unwrap_or_else(|error| panic!("runtime: {error:?}"));
    front_door.install_blob_store(Arc::clone(&runtime.blob_store)).expect("install the blob store");
    (daemon, runtime, shutdown)
}

impl TestDaemon {
    /// Restart the daemon over the same store, as a process restart does.
    /// Vendor backings and crew processes survive; the new daemon adopts them.
    pub async fn restart(self) -> Self {
        self.stop().await.start().await
    }

    /// Stop the daemon, as a process restart's first half does, leaving its store in the
    /// directory for an operator's command to read while no daemon holds it.
    pub async fn stop(self) -> StoppedDaemon {
        let Self { daemon, fleet, runtime, front_door, operator, composition, dispatched, dir } = self;
        drop(operator);
        let _ = front_door.send(true);
        let node = daemon.node_id().clone();
        if let Some(runtime) = runtime {
            runtime.shutdown().await;
        }
        drop(daemon);
        tokio::task::yield_now().await;
        StoppedDaemon { fleet, composition, dispatched, dir, node }
    }

    /// Serve this daemon's main socket at `socket`, as `flotillad` serves the operator's process,
    /// for a command run with `--socket`. Dropping the returned guard stops serving it.
    pub fn serve_socket(&self, socket: &std::path::Path) -> MainSocket {
        let listener = tokio::net::UnixListener::bind(socket).unwrap_or_else(|error| panic!("bind {}: {error}", socket.display()));
        let daemon = Arc::clone(&self.daemon);
        MainSocket(socket.to_path_buf(), tokio::spawn(flotilla_daemon::stopped_store::serve_resource_socket(listener, daemon)))
    }

    /// The directory that holds the daemon's configuration and its state, as `--config-dir` and
    /// `--state-dir` name it.
    pub fn config_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("config")
    }

    /// Restart the daemon's process, as `flotillad` exiting and starting again does. The dummy
    /// fleet runs in that process, so it ends with it: its crews stop where they are. The fleet
    /// the new process opens restores, from its state file, what outlives a real process: the
    /// vendors' backings, the sessions with their crews, the checkouts and the forge.
    pub async fn restart_process(self) -> Self {
        let Self { daemon, fleet, runtime, front_door, operator, composition, dispatched, dir } = self;
        drop(operator);
        let node = daemon.node_id().clone();
        // The fleet stops with the runtime, as flotillad's graceful stop stops it.
        if let Some(mut runtime) = runtime {
            runtime.stop_with_runtime(dummy_fleet_component(&fleet));
            runtime.shutdown().await;
        } else {
            fleet.stop();
        }
        let _ = front_door.send(true);
        let ended = fleet.snapshot();
        drop(daemon);
        drop(fleet);
        tokio::task::yield_now().await;
        let config = Arc::new(ConfigStore::with_base(dir.path().join("config")));
        let daemon_config: DaemonConfig = config.load_daemon_config().expect("daemon config");
        let mut discovery = discovery(&dir, composition.forges.as_ref());
        let fleet = install_dummy_fleet(&daemon_config, config.state_dir().as_path(), &mut discovery)
            .expect("dummy fleet state")
            .expect("dummy fleet configured");
        assert_eq!(fleet.snapshot(), ended, "a graceful stop leaves every change the fleet recorded in its state file");
        fleet.resume();
        let (daemon, runtime, front_door) = compose(&composition, config, discovery, &dir).await;
        assert_eq!(daemon.node_id(), &node, "restart preserves the daemon's identity");
        let operator = Operator::connect(&daemon).await;
        Self { daemon, fleet, runtime: Some(runtime), front_door, operator, composition, dispatched, dir }
    }

    /// Execute `action` as an operator: through the request dispatcher, as the
    /// CLI's commands do. A refusal or failure is a [`CommandValue::Error`].
    pub async fn execute(&self, action: CommandAction) -> CommandValue {
        let value = self.operator.execute(action, None).await;
        value
    }

    /// Deliver a harness hook event, as `flotilla hook` does from inside a crew's terminal.
    pub async fn agent_hook(&self, event: flotilla_protocol::AgentHookEvent) {
        self.operator.client.send_agent_hook(event).await.expect("the daemon accepts the hook event");
    }

    /// [`TestDaemon::execute`] in `repository`, as the CLI sends a command
    /// with its `context_repo`.
    pub async fn execute_in(&self, repository: &RepositoryKey, action: CommandAction) -> CommandValue {
        self.operator.execute(action, Some(RepoSelector::Repository(repository.clone()))).await
    }

    /// Dispatch a convoy of `mix` onto the fulfilment kind `kind`, and return
    /// the convoy's resource name.
    pub async fn dispatch(&self, mix: CrewMix, kind: &str) -> String {
        let role = self.request_convoy(mix, kind).await;
        let convoys = self.list::<Convoy>().await;
        convoys.into_iter().find(|convoy| convoy.spec.role == role).unwrap_or_else(|| panic!("no convoy {role}")).metadata.name
    }

    /// Request a convoy of `mix` onto the fulfilment kind `kind` in the Fleet
    /// project, as `flotilla convoy ROLE create --project fleet` does, and
    /// return its role.
    pub async fn request_convoy(&self, mix: CrewMix, kind: &str) -> String {
        let role = format!("{}-{}", mix.workflow_name(), self.dispatched.fetch_add(1, Ordering::Relaxed));
        let created = self.execute(catalog::dispatch(&role, mix.workflow_name(), kind)).await;
        assert!(matches!(created, CommandValue::ConvoyStarted { .. }), "dispatch {role}: {created:?}");
        role
    }

    /// The `T` named `name`, as `flotilla get` reads it.
    pub async fn get<T: Resource>(&self, name: &str) -> Option<ResourceObject<T>> {
        let request = ResourceGetRequest::builder().kind(T::API_PATHS.kind.into()).name(name.into()).namespace(NAMESPACE.into()).build();
        let read = self.operator.resources.get(request).await;
        match read {
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
        let read = self.operator.resources.list(request).await;
        let read = read.unwrap_or_else(|message| panic!("list {}: {message}", T::API_PATHS.kind));
        read.records.into_iter().filter_map(|record| record.object).map(decode).collect()
    }

    /// The operator's resource client, as `flotilla watch` uses it.
    pub fn resources(&self) -> &ResourceClient {
        &self.operator.resources
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

    /// Every `T` listed and, for a Convoy, every other the operator has seen, read by name: one
    /// that ended and retired has left the listing, and reads with how it ended.
    pub async fn every<T: Resource>(&self) -> Vec<ResourceObject<T>> {
        let mut items = self.list::<T>().await;
        if T::API_PATHS.kind == Convoy::API_PATHS.kind {
            let listed: BTreeSet<String> = items.iter().map(|item| item.metadata.name.clone()).collect();
            let unlisted: Vec<String> = self.operator.convoys_seen.lock().expect("convoys seen").difference(&listed).cloned().collect();
            for name in unlisted {
                items.extend(self.get::<T>(&name).await);
            }
        }
        items
    }

    /// Wait until some `T` satisfies `done`, and return the first that does. It reads every `T`
    /// ([`Self::every`], so a Convoy that ended and retired is found), then tries each state a
    /// watch of them streams, and each one's last state again at every change the fleet records
    /// (`done` may read [`Self::counts`]). It fails with every `T`'s last state.
    pub fn wait<'a, T: Resource>(&'a self, what: &'a str, done: impl Fn(&ResourceObject<T>) -> bool + 'a) -> Wait<'a, ResourceObject<T>> {
        Wait::new(async move |deadline| {
            let mut recorded = self.fleet.changes();
            let mut watch = watch_kind::<T>(&self.operator.resources).await;
            // Each `T` by name, as last read: one that ended and retired keeps how it ended.
            let mut last = BTreeMap::new();
            for item in self.every::<T>().await {
                last.insert(item.metadata.name.clone(), item);
            }
            loop {
                if let Some(found) = last.iter().find_map(|(name, item)| done(item).then(|| name.clone())) {
                    return last.remove(&found).expect("the found state");
                }
                let streamed = async {
                    match watch.as_mut() {
                        Some(watch) => watch.next().await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    changed = recorded.changed() => if changed.is_err() {
                        panic!("the fleet ended while waiting for a {} to reach {what}", T::API_PATHS.kind);
                    },
                    streamed = streamed => match streamed {
                        Ok(Some(envelope)) => {
                            for item in envelope.records.into_iter().filter_map(|record| record.object).map(decode::<T>) {
                                // A state that passes is found, though a later one in the same
                                // envelope moves on from it.
                                if done(&item) {
                                    return item;
                                }
                                last.insert(item.metadata.name.clone(), item);
                            }
                        }
                        // An ended watch opens again, and every `T` is read anew.
                        Ok(None) | Err(_) => {
                            watch = watch_kind::<T>(&self.operator.resources).await;
                            for item in self.every::<T>().await {
                                last.insert(item.metadata.name.clone(), item);
                            }
                        }
                    },
                    () = tokio::time::sleep_until(deadline) => {
                        let states: Vec<_> = last.values().collect();
                        panic!("no {} reached {what}; last states: {}; fleet: {:?}", T::API_PATHS.kind, shown(&states), self.counts());
                    }
                }
            }
        })
    }

    /// Wait until the `T` named `name` is gone.
    pub fn wait_gone<'a, T: Resource>(&'a self, name: &'a str) -> Wait<'a, ()> {
        Wait::new(async move |deadline| {
            let mut changes = self.changes().kind::<T>().await;
            loop {
                let Some(last) = self.get::<T>(name).await else { return };
                if !changes.next(deadline).await {
                    panic!("{} {name} was never removed; last state: {}", T::API_PATHS.kind, shown(&last));
                }
            }
        })
    }

    /// Wait until the client query `query` has a result set that satisfies
    /// `done`, subscribing to it as a client does. The wait fails with the
    /// last result set.
    pub fn wait_query<'a>(&'a self, query: QueryId, done: impl Fn(&ResultSet) -> bool + 'a) -> Wait<'a, ResultSet> {
        Wait::new(async move |deadline| {
            let mut events = self.operator.client.subscribe();
            let mut last = self.result_set(&query).await;
            loop {
                if let Some(result) = last.take_if(|result| done(result)) {
                    return result;
                }
                let next = match tokio::time::timeout_at(deadline, events.recv()).await {
                    Ok(next) => next,
                    Err(_) => panic!("no {query:?} result set satisfied the wait; last: {}", shown(&last)),
                };
                // A delta carries only what changed, and a lag loses changes: read the whole set
                // again, as a client that resubscribes does.
                let reread = match next {
                    Ok(DaemonEvent::ResultSet(result)) if result.query() == query => {
                        last = Some(*result);
                        false
                    }
                    Ok(DaemonEvent::ResultDelta(delta)) => delta.query() == query,
                    Ok(_) => false,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => panic!("the daemon closed while waiting on {query:?}"),
                };
                if reread {
                    last = self.result_set(&query).await.or(last);
                }
            }
        })
    }

    /// The client query `query`'s whole result set, subscribing to it as a client does.
    async fn result_set(&self, query: &QueryId) -> Option<ResultSet> {
        let cursors = [QueryCursor { query: query.clone(), since: None }];
        let current = self.operator.client.subscribe_queries(uuid::Uuid::new_v4(), &cursors).await.expect("subscribe");
        current.into_iter().rev().find_map(|event| match event {
            DaemonEvent::ResultSet(result) if result.query() == *query => Some(*result),
            _ => None,
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

    /// The rows of the table `table` that satisfy `condition` (the tables
    /// API's `where`, none when empty), as an operator reads them with `curl
    /// --unix-socket` from `/tables/{table}/rows`: this home's, for an engine
    /// kind.
    pub async fn rows(&self, table: &str, condition: &str) -> Vec<Row> {
        let mut target = format!("/tables/{table}/rows?limit=10000");
        if !condition.is_empty() {
            target.push_str("&where=");
            target.extend(condition.bytes().map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => char::from(byte).to_string(),
                _ => format!("%{byte:02X}"),
            }));
        }
        let (status, body) = local_get(&self.daemon, &target).await;
        assert_eq!(status, 200, "GET {target}: {}", String::from_utf8_lossy(&body));
        let mut rows = Vec::new();
        for line in body.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
            match serde_json::from_slice::<TableMessage>(line).unwrap_or_else(|error| panic!("GET {target}: {error}")) {
                TableMessage::Rows { rows: page, partial, .. } => {
                    assert!(!partial, "GET {target}: more than one page");
                    rows.extend(page);
                }
                TableMessage::Resync { reason } => panic!("GET {target}: resync: {reason}"),
                _ => {}
            }
        }
        rows
    }

    /// A watch of the table `table`, as an operator streams it from
    /// `/tables/{table}/rows?watch=true`.
    pub async fn watch_table(&self, table: &str) -> TableWatch {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let (client, server) = tokio::net::UnixStream::pair().expect("socket pair");
        let mut client = Tracked::new(client);
        let serving = tokio::spawn(flotilla_daemon::server::serve_local_http(server, Arc::clone(&self.daemon)));
        let request = format!(
            "GET /tables/{table}/rows?namespace={NAMESPACE}&limit=10000&watch=true HTTP/1.1\r\nHost: flotilla.local\r\nAccept: application/x-ndjson\r\n\r\n"
        );
        client.write_all(request.as_bytes()).await.expect("send request");
        let mut body = tokio::io::BufReader::new(client);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            tokio::time::timeout(WAIT, body.read_line(&mut line)).await.expect("response head in time").expect("read response head");
            if line == "\r\n" || line.is_empty() {
                break;
            }
            head.push_str(&line.to_ascii_lowercase());
        }
        assert!(head.starts_with("http/1.1 200"), "GET /tables/{table}/rows?watch=true: {head}");
        let chunked = head.lines().any(|line| line.starts_with("transfer-encoding:") && line.contains("chunked"));
        TableWatch { body, chunked, buffer: Vec::new(), serving }
    }

    /// Wait until some row of `table` satisfying `condition` also satisfies
    /// `done`, and return it. It reads again at each change the table's watch streams and each
    /// change the fleet records, and fails with the table's last rows.
    pub fn wait_row<'a>(&'a self, what: &'a str, table: &'a str, condition: &'a str, done: impl Fn(&Row) -> bool + 'a) -> Wait<'a, Row> {
        Wait::new(async move |deadline| {
            let mut changes = self.changes().table(table).await;
            loop {
                let mut rows = self.rows(table, condition).await;
                if let Some(found) = rows.iter().position(&done) {
                    return rows.swap_remove(found);
                }
                if !changes.next(deadline).await {
                    panic!("no {table} row reached {what}; last rows where {condition:?}: {}; fleet: {:?}", shown(&rows), self.counts());
                }
            }
        })
    }

    /// Wait until some row of engine table `table` satisfying `held` has reached
    /// `state`, read from the kind's log rather than its current rows: the fleet
    /// can move a record past a state between two reads. Returns the row as it
    /// now is. It reads again at each change the table's watch streams, as each entry its log
    /// gains changes its rows, and fails with the held rows and the log's last entries.
    pub fn wait_reached<'a>(&'a self, what: &'a str, table: &'a str, state: &'a str, held: impl Fn(&Row) -> bool + 'a) -> Wait<'a, Row> {
        let name = |row: &Row| row.get("resource.name").or_else(|| row.get("name")).and_then(serde_json::Value::as_str).map(str::to_owned);
        Wait::new(async move |deadline| {
            let mut changes = self.changes().table(table).await;
            loop {
                let mut rows: Vec<Row> = self.rows(table, "").await.into_iter().filter(&held).collect();
                let log = self.rows(&format!("{table}.log"), "").await;
                let reached = |row: &Row| {
                    log.iter().any(|entry| name(entry) == name(row) && entry.get("to").and_then(serde_json::Value::as_str) == Some(state))
                };
                if let Some(found) = rows.iter().position(reached) {
                    return rows.swap_remove(found);
                }
                if !changes.next(deadline).await {
                    let last = &log[log.len().saturating_sub(20)..];
                    panic!(
                        "no {table} row reached {what}; held rows: {}; last log entries: {}; fleet: {:?}",
                        shown(&rows),
                        shown(last),
                        self.counts()
                    );
                }
            }
        })
    }

    /// What wakes a wait: the changes the fleet records, and those of what the wait reads.
    fn changes(&self) -> ChangesBuilder<'_> {
        ChangesBuilder { daemon: self }
    }

    /// Each engine kind's root table, as the tables API lists them.
    pub async fn engine_tables(&self) -> std::collections::BTreeMap<String, String> {
        let (status, body) = local_get_as(&self.daemon, "/tables", "application/json").await;
        assert_eq!(status, 200, "GET /tables: {}", String::from_utf8_lossy(&body));
        let listed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|error| panic!("GET /tables: {error}"));
        let mut kinds = std::collections::BTreeMap::new();
        for entry in listed["kinds"].as_array().expect("the listed kinds") {
            // An engine kind's tables end with its log; the old store's kinds, derived tables
            // and query families have none.
            let tables = entry["tables"].as_array().into_iter().flatten().filter_map(serde_json::Value::as_str);
            let engine = tables.into_iter().any(|table| table.ends_with(".log"));
            if let (true, Some(kind), Some(root)) = (engine, entry["kind"].as_str(), entry["root"].as_str()) {
                kinds.insert(kind.to_owned(), root.to_owned());
            }
        }
        kinds
    }

    /// The environment vendors: what a test does as the vendor is all the
    /// daemon learns of it.
    pub fn vendor(&self) -> &DummyVendor {
        self.fleet.vendor()
    }

    /// End the process of `role`'s crew in convoy `convoy`, as a crashed harness's ends. False
    /// when no such crew runs.
    pub fn end_crew(&self, convoy: &str, role: &str) -> bool {
        self.fleet.end_crew(convoy, role)
    }

    /// Lose the session of `role`'s crew in convoy `convoy` from its pool, as a cleat daemon that
    /// no longer has it does. False when no such crew runs.
    pub fn lose_crew(&self, convoy: &str, role: &str) -> bool {
        self.fleet.lose_crew(convoy, role)
    }

    /// What the fakes counted.
    pub fn counts(&self) -> DummyFleetSnapshot {
        self.fleet.snapshot()
    }

    /// The first refusals the crews' commands met, as the fleet kept them.
    pub fn command_error_samples(&self) -> Vec<String> {
        self.fleet.command_error_samples()
    }

    /// The dummy catalog, applied as an operator applies one: a Fleet project
    /// and its designation, the forge, repository, kinds and workflows.
    async fn seed(&self) {
        let host = self.list::<Host>().await.into_iter().next().expect("local host").metadata.name;
        let config = self.fleet.config();
        // The documents an operator applies to a real daemon (`dummy-fleet-catalog`).
        let documents = catalog::documents(&config.forge_service_url, &host, config.agent.long_runner_turns).expect("dummy catalog");
        for catalog::CatalogDocument { kind, name, document } in documents {
            let applied = self.execute(CommandAction::ResourceApply { namespace: NAMESPACE.into(), document }).await;
            assert!(matches!(applied, CommandValue::ResourceObject(_)), "apply {kind} {name}: {applied:?}");
        }
        self.apply::<FulfilmentKind>(DUMMY_CLOUD_FULFILMENT_KIND, catalog::cloud_fulfilment_kind(&host)).await;
        let key = catalog::repository(&catalog::forge(&config.forge_service_url)).expect("dummy repository").key().to_string();
        let resolved = |repository: &ResourceObject<Repository>| {
            repository.metadata.name == key && repository.status.as_ref().is_some_and(|status| status.default_branch.is_some())
        };
        self.wait("a resolved default branch", resolved).await;
        // Admission places a literal image only once its host has probed the image's harnesses.
        let probed = |host: &ResourceObject<Host>| {
            host.status.as_ref().is_some_and(|status| {
                status.fulfilment_facts.get(DUMMY_FULFILMENT_KIND).is_some_and(|facts| facts.harnesses.contains_key(DUMMY_AGENT_ADAPTER))
            })
        };
        self.wait("the dummy kind's harness", probed).await;
    }
}

/// A daemon that was stopped over its store: [`TestDaemon::stop`] without the restart's second half.
pub struct StoppedDaemon {
    fleet: Arc<DummyFleet>,
    composition: Composition,
    dispatched: AtomicU64,
    dir: tempfile::TempDir,
    node: flotilla_protocol::NodeId,
}

impl StoppedDaemon {
    /// Start the daemon again over the same store, as a process restart does.
    pub async fn start(self) -> TestDaemon {
        let Self { fleet, composition, dispatched, dir, node } = self;
        let config = Arc::new(ConfigStore::with_base(dir.path().join("config")));
        let mut discovery = discovery(&dir, composition.forges.as_ref());
        fleet.install(&mut discovery);
        let (daemon, runtime, front_door) = compose(&composition, config, discovery, &dir).await;
        assert_eq!(daemon.node_id(), &node, "restart preserves the daemon's identity");
        let operator = Operator::connect(&daemon).await;
        TestDaemon { daemon, fleet, runtime: Some(runtime), front_door, operator, composition, dispatched, dir }
    }
}

/// A daemon's main socket, served until dropped.
pub struct MainSocket(std::path::PathBuf, tokio::task::JoinHandle<()>);

impl Drop for MainSocket {
    fn drop(&mut self) {
        self.1.abort();
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Two peered daemons; dropping it disconnects them.
pub struct Peering {
    _topology: super::server::InMemoryRequestTopology,
}

/// A table watch over the daemon's local HTTP surface; dropping it disconnects.
pub struct TableWatch {
    body: tokio::io::BufReader<Tracked<tokio::net::UnixStream>>,
    chunked: bool,
    /// Bytes read past the last whole message.
    buffer: Vec<u8>,
    serving: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Drop for TableWatch {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

impl TableWatch {
    /// The next message, within the usual wait.
    pub async fn next(&mut self) -> TableMessage {
        let next = tokio::time::timeout(WAIT, self.read()).await.expect("a table message in time");
        next.expect("the table watch ended")
    }

    /// The next message; none once the watch ends.
    async fn read(&mut self) -> Option<TableMessage> {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        loop {
            if let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=end).collect();
                if line.len() > 1 {
                    return Some(serde_json::from_slice(&line).unwrap_or_else(|error| panic!("table message: {error}")));
                }
                continue;
            }
            let mut data = Vec::new();
            if self.chunked {
                let mut size = String::new();
                if self.body.read_line(&mut size).await.ok()? == 0 {
                    return None;
                }
                let size = usize::from_str_radix(size.split(';').next().unwrap_or_default().trim(), 16).expect("chunk size");
                if size == 0 {
                    return None;
                }
                data.resize(size + 2, 0);
                self.body.read_exact(&mut data).await.ok()?;
                data.truncate(size);
            } else if self.body.read_until(b'\n', &mut data).await.ok()? == 0 {
                return None;
            }
            self.buffer.extend(data);
        }
    }
}

/// Opens what wakes a wait: each change the fleet records, and, once opened, each change to what
/// the wait reads, as the daemon publishes it.
struct ChangesBuilder<'a> {
    daemon: &'a TestDaemon,
}

impl<'a> ChangesBuilder<'a> {
    /// And each change to a `T`, as `flotilla watch` streams them.
    async fn kind<T: Resource>(self) -> Changes<'a> {
        let resources = self.daemon.operator.resources.clone();
        let watched = watch_kind::<T>(&resources).await;
        let wakes = futures::stream::unfold((resources, watched), |(resources, mut watched)| async move {
            loop {
                let Some(watch) = watched.as_mut() else {
                    // An ended watch opens again, and its opening reads anew what changed between.
                    watched = Some(watch_kind::<T>(&resources).await?);
                    return Some(((), (resources, watched)));
                };
                match watch.next().await {
                    Ok(Some(_)) => return Some(((), (resources, watched))),
                    Ok(None) | Err(_) => watched = None,
                }
            }
        });
        self.with(wakes.boxed())
    }

    /// And each change to the rows of `table`, as its watch streams them.
    async fn table(self, table: &str) -> Changes<'a> {
        let watch = self.daemon.watch_table(table).await;
        let wakes = futures::stream::unfold(watch, |mut watch| async move { watch.read().await.map(|_| ((), watch)) });
        self.with(wakes.boxed())
    }

    fn with(self, read: futures::stream::BoxStream<'a, ()>) -> Changes<'a> {
        Changes { fleet: self.daemon.fleet.changes(), read }
    }
}

/// A watch of every `T`, as `flotilla watch` opens one; none when the daemon refuses it.
async fn watch_kind<T: Resource>(resources: &ResourceClient) -> Option<ResourceWatch> {
    let request = ResourceWatchRequest::builder().kind(T::API_PATHS.kind.into()).namespace(NAMESPACE.into()).include_replicas(true).build();
    resources.watch(request).await.ok()
}

/// The changes a wait wakes at.
struct Changes<'a> {
    fleet: watch::Receiver<u64>,
    read: futures::stream::BoxStream<'a, ()>,
}

impl Changes<'_> {
    /// Waits for the next change, taking every other already made with it. False when the
    /// virtual `deadline` comes first.
    async fn next(&mut self, deadline: tokio::time::Instant) -> bool {
        let Self { fleet, read } = self;
        let recorded = async {
            if fleet.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            () = recorded => {}
            Some(()) = read.next() => {}
            () = tokio::time::sleep_until(deadline) => return false,
        }
        while let Some(Some(())) = read.next().now_or_never() {}
        fleet.mark_unchanged();
        true
    }
}

fn shown(value: &(impl serde::Serialize + ?Sized)) -> String {
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
}

impl<'a, T: 'a> Wait<'a, T> {
    fn new(run: impl AsyncFnOnce(tokio::time::Instant) -> T + 'a) -> Self {
        Self { run: Box::new(move |deadline| Box::pin(run(deadline))) }
    }
}

impl<'a, T: 'a> std::future::IntoFuture for Wait<'a, T> {
    type Output = T;
    type IntoFuture = WaitFuture<'a, T>;

    fn into_future(self) -> Self::IntoFuture {
        (self.run)(tokio::time::Instant::now() + WAIT)
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
    /// Every Convoy the operator's watch has read, by name.
    convoys_seen: Arc<Mutex<BTreeSet<String>>>,
    seeing: tokio::task::JoinHandle<()>,
}

impl Drop for Operator {
    fn drop(&mut self) {
        self.seeing.abort();
        self.session.abort();
    }
}

/// Records the name of every Convoy `resources` watches, opening the watch again whenever it
/// ends, until the daemon is gone.
async fn see_convoys(resources: ResourceClient, seen: Arc<Mutex<BTreeSet<String>>>) {
    loop {
        let request =
            ResourceWatchRequest::builder().kind(Convoy::API_PATHS.kind.into()).namespace(NAMESPACE.into()).include_replicas(true).build();
        let Ok(mut watch) = resources.watch(request).await else { return };
        while let Ok(Some(envelope)) = watch.next().await {
            let names = envelope.records.into_iter().filter_map(|record| Some(record.object?["metadata"]["name"].as_str()?.to_owned()));
            seen.lock().expect("convoys seen").extend(names);
        }
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
        let convoys_seen = Arc::default();
        let seeing = tokio::spawn(see_convoys(resources.clone(), Arc::clone(&convoys_seen)));
        Self { client, resources, _shutdown: shutdown, session, convoys_seen, seeing }
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

/// One GET over a connection the daemon serves as its main socket serves the
/// operator's process: the status and the de-chunked body.
async fn local_get(daemon: &Arc<InProcessDaemon>, target: &str) -> (u16, Vec<u8>) {
    local_get_as(daemon, target, "application/x-ndjson").await
}

/// [`local_get`], accepting `accept`.
async fn local_get_as(daemon: &Arc<InProcessDaemon>, target: &str, accept: &str) -> (u16, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (client, server) = tokio::net::UnixStream::pair().expect("socket pair");
    let mut client = Tracked::new(client);
    let serving = tokio::spawn(flotilla_daemon::server::serve_local_http(server, Arc::clone(daemon)));
    let request = format!("GET {target} HTTP/1.1\r\nHost: flotilla.local\r\nAccept: {accept}\r\nConnection: close\r\n\r\n");
    client.write_all(request.as_bytes()).await.expect("send request");
    let mut response = Vec::new();
    tokio::time::timeout(WAIT, client.read_to_end(&mut response)).await.expect("response in time").expect("read response");
    serving.abort();
    let split = response.windows(4).position(|window| window == b"\r\n\r\n").expect("response head");
    let head = String::from_utf8_lossy(&response[..split]).to_ascii_lowercase();
    let mut body = response[split + 4..].to_vec();
    let status = head.split_whitespace().nth(1).and_then(|code| code.parse().ok()).expect("status code");
    if head.lines().any(|line| line.starts_with("transfer-encoding:") && line.contains("chunked")) {
        let mut chunked = body.as_slice();
        let mut plain = Vec::new();
        loop {
            let end = chunked.windows(2).position(|window| window == b"\r\n").expect("chunk size");
            let size =
                String::from_utf8_lossy(&chunked[..end]).split(';').next().map(str::trim).map(|size| usize::from_str_radix(size, 16));
            let size = size.expect("chunk size").expect("chunk size");
            chunked = &chunked[end + 2..];
            if size == 0 {
                break;
            }
            plain.extend_from_slice(&chunked[..size]);
            chunked = &chunked[size + 2..];
        }
        body = plain;
    }
    (status, body)
}
