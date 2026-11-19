use std::{collections::HashMap, sync::Arc, time::Duration};

use flotilla_client::SocketDaemon;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{
    result_set::{ConvoyPhase, ConvoyRow},
    CommandCaller, GoodbyeReason, HostName, NodeInfo, PeerWireMessage, ResourceRef, SurfaceDeclaration,
};
use flotilla_resources::{api_version, Convoy, InputMeta, Project, ProjectSpec, Resource, WorkflowTemplate};
use tokio::sync::{mpsc, watch, Mutex, Notify};

use flotilla_daemon::server::{build_remote_command_router, change_stream::StreamHolds, peer_runtime::PeerRuntime};
use flotilla_daemon::{
    blob_store::TieredBlobStore,
    peer::{
        channel_transport::{channel_transport_pair_with_nodes, ChannelTransport},
        transport::{PeerConnectionStatus, PeerSender, PeerTransport},
        PeerManager,
    },
    server::PeerConnectionEvent,
};

pub async fn apply_convoy_replica_feed(daemon: &InProcessDaemon, namespace: &str, name: &str, home: HostName) {
    let resource = ResourceRef::new(api_version(Convoy::API_PATHS), Convoy::API_PATHS.kind, namespace, name).on_host(home.clone());
    let row = ConvoyRow::builder()
        .resource(resource)
        .name(name)
        .workflow_ref("scratch")
        .project_ref(namespace)
        .phase(ConvoyPhase::Pending)
        .build();
    let state = daemon.aggregator_projection_state().await;
    state.write().await.replace_replica_rows(HashMap::from([(home, HashMap::from([(row.resource.clone(), row)]))]));
}

pub async fn seed_trusted_remote_convoy_project(daemon: &InProcessDaemon, namespace: &str) {
    let workflow = flotilla_resources::single_agent_workflow_spec();

    let backend = daemon.resource_backend();
    backend
        .clone()
        .using::<WorkflowTemplate>(namespace)
        .create(&InputMeta::builder().name("remote-workflow".to_string()).build(), &workflow)
        .await
        .expect("create workflow");
    backend
        .using::<Project>(namespace)
        .create(
            &InputMeta::builder().name("flotilla".to_string()).build(),
            &ProjectSpec::builder().display_name("Flotilla".to_string()).default_workflow_ref("remote-workflow".to_string()).build(),
        )
        .await
        .expect("create project");
}

pub struct InMemoryRequestTopology {
    pub leader: Arc<InProcessDaemon>,
    pub follower: Arc<InProcessDaemon>,
    pub client: Arc<SocketDaemon>,
    pub leader_host: HostName,
    pub follower_host: HostName,
    pub shutdown_tx: watch::Sender<bool>,
    /// The homes whose change streams the leader holds back.
    pub leader_stream_holds: StreamHolds,
    /// The homes whose change streams the follower holds back.
    pub follower_stream_holds: StreamHolds,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
    /// The daemons' temporary homes, removed after everything above drops.
    _homes: Vec<tempfile::TempDir>,
}

/// A harness daemon and the temporary home it was made in, which goes when this does, or when
/// the topology it joins does. A daemon made elsewhere comes with no home to remove.
pub struct HomedDaemon {
    pub daemon: Arc<InProcessDaemon>,
    pub home: Option<tempfile::TempDir>,
}

impl HomedDaemon {
    pub fn new(daemon: Arc<InProcessDaemon>, home: tempfile::TempDir) -> Self {
        Self { daemon, home: Some(home) }
    }
}

impl std::ops::Deref for HomedDaemon {
    type Target = Arc<InProcessDaemon>;

    fn deref(&self) -> &Arc<InProcessDaemon> {
        &self.daemon
    }
}

impl From<Arc<InProcessDaemon>> for HomedDaemon {
    fn from(daemon: Arc<InProcessDaemon>) -> Self {
        Self { daemon, home: None }
    }
}

impl InMemoryRequestTopology {
    /// Holds the follower's change stream at the leader once the leader has installed everything
    /// the follower has committed so far. The leader keeps what came before and installs nothing
    /// the follower commits after, so a scenario can prove an outcome needs none of it there.
    pub async fn hold_follower_stream_at_leader(&self) {
        let follower = self.follower.decided_store();
        let leader = self.leader.decided_store();
        let home = follower.home().to_owned();
        tokio::time::timeout(Duration::from_secs(5), async {
            while leader.watermark(&home).expect("leader watermark").home_seq < follower.last_sequence(&home).expect("follower log") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("leader should catch up with the follower's change stream");
        self.leader_stream_holds.hold(&home);
    }
}

impl Drop for InMemoryRequestTopology {
    fn drop(&mut self) {
        for task in &self._tasks {
            task.abort();
        }
    }
}

/// A full mesh of in-memory peer sessions with a request client on every host.
/// The clients exercise the same request dispatcher and remote router as a
/// socket client, while tests may inspect each host's authoritative store.
pub struct InMemoryRequestMesh {
    pub hosts: Vec<Arc<InProcessDaemon>>,
    pub clients: Vec<Arc<SocketDaemon>>,
    pub shutdown_txs: Vec<watch::Sender<bool>>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
    /// The hosts' temporary homes, removed after everything above drops.
    _homes: Vec<tempfile::TempDir>,
}

impl Drop for InMemoryRequestMesh {
    fn drop(&mut self) {
        for task in &self._tasks {
            task.abort();
        }
    }
}

/// `hosts`' daemons, and their homes for the mesh to hold.
fn homed(hosts: Vec<impl Into<HomedDaemon>>) -> (Vec<Arc<InProcessDaemon>>, Vec<tempfile::TempDir>) {
    let (mut daemons, mut homes) = (Vec::new(), Vec::new());
    for host in hosts {
        let HomedDaemon { daemon, home } = host.into();
        daemons.push(daemon);
        homes.extend(home);
    }
    (daemons, homes)
}

impl InMemoryRequestMesh {
    fn holding(mut self, homes: Vec<tempfile::TempDir>) -> Self {
        self._homes = homes;
        self
    }
}

pub async fn spawn_in_memory_request_mesh(hosts: Vec<impl Into<HomedDaemon>>) -> Result<InMemoryRequestMesh, String> {
    spawn_in_memory_request_mesh_with_filter(hosts, Arc::new(Some)).await
}

/// A network-boundary fault injector for replication scenarios. Returning None
/// drops one envelope; changing a request can simulate a failed stream round.
pub async fn spawn_in_memory_request_mesh_with_filter(
    hosts: Vec<impl Into<HomedDaemon>>,
    filter: EnvelopeFilter,
) -> Result<InMemoryRequestMesh, String> {
    let (hosts, homes) = homed(hosts);
    Ok(spawn_request_mesh(hosts, filter).await?.holding(homes))
}

/// A full mesh whose hosts record replication into the given metrics, one per
/// host in order, for load measurement (#3206).
pub async fn spawn_in_memory_request_mesh_with_replication_metrics(
    hosts: Vec<impl Into<HomedDaemon>>,
    replication_metrics: Vec<Arc<flotilla_store::load_metrics::ReplicationLoadMetrics>>,
) -> Result<InMemoryRequestMesh, String> {
    let (hosts, homes) = homed(hosts);
    if replication_metrics.len() != hosts.len() {
        return Err("replication metrics must name one entry per host".into());
    }
    Ok(spawn_request_graph(hosts, Arc::new(Some), replication_metrics, |_, _| true).await?.holding(homes))
}

/// Hosts linked in a line, each only to the next: the first and last are not directly connected,
/// so what crosses between them is passed on by the hosts between.
pub async fn spawn_in_memory_request_line(hosts: Vec<impl Into<HomedDaemon>>) -> Result<InMemoryRequestMesh, String> {
    let (hosts, homes) = homed(hosts);
    let replication_metrics = hosts.iter().map(|_| Arc::default()).collect();
    Ok(spawn_request_graph(hosts, Arc::new(Some), replication_metrics, |left, right| right == left + 1).await?.holding(homes))
}

async fn spawn_request_mesh(hosts: Vec<Arc<InProcessDaemon>>, filter: EnvelopeFilter) -> Result<InMemoryRequestMesh, String> {
    let replication_metrics = hosts.iter().map(|_| Arc::default()).collect();
    spawn_request_graph(hosts, filter, replication_metrics, |_, _| true).await
}

/// Hosts linked wherever `linked(left, right)` holds, for `left < right`.
async fn spawn_request_graph(
    hosts: Vec<Arc<InProcessDaemon>>,
    filter: EnvelopeFilter,
    replication_metrics: Vec<Arc<flotilla_store::load_metrics::ReplicationLoadMetrics>>,
    linked: fn(usize, usize) -> bool,
) -> Result<InMemoryRequestMesh, String> {
    if hosts.is_empty() {
        return Err("request mesh needs at least one host".into());
    }
    let peer_managers: Vec<_> = hosts.iter().map(|host| Arc::new(Mutex::new(PeerManager::new(host.node_id().clone())))).collect();
    for (index, peer_manager) in peer_managers.iter().enumerate() {
        for (other_index, other) in hosts.iter().enumerate() {
            if index != other_index {
                peer_manager.lock().await.store_host_identity(other.local_host_summary().await);
            }
        }
    }
    for left in 0..hosts.len() {
        for right in (left + 1)..hosts.len() {
            if !linked(left, right) {
                continue;
            }
            let (left_transport, right_transport) = channel_transport_pair_with_nodes(
                NodeInfo::new(hosts[left].node_id().clone(), hosts[left].host_name().to_string()),
                NodeInfo::new(hosts[right].node_id().clone(), hosts[right].host_name().to_string()),
            );
            peer_managers[left].lock().await.add_configured_target(
                flotilla_protocol::ConfigLabel(hosts[right].host_name().to_string()),
                hosts[right].host_name().clone(),
                None,
                Box::new(FilteredTransport { inner: left_transport, filter: Arc::clone(&filter) }),
            );
            peer_managers[right].lock().await.add_configured_target(
                flotilla_protocol::ConfigLabel(hosts[left].host_name().to_string()),
                hosts[left].host_name().clone(),
                None,
                Box::new(FilteredTransport { inner: right_transport, filter: Arc::clone(&filter) }),
            );
        }
    }

    let mut clients = Vec::with_capacity(hosts.len());
    let mut shutdown_txs = Vec::with_capacity(hosts.len());
    let mut tasks = Vec::with_capacity(hosts.len() * 2);
    for ((host, peer_manager), replication_metrics) in hosts.iter().zip(&peer_managers).zip(replication_metrics) {
        let (inbound_peer_tx, inbound_peer_rx) = mpsc::channel(256);
        let router = build_remote_command_router(host, peer_manager);
        let (runtime, _connected_tx) = PeerRuntime::new(
            Arc::clone(host),
            Arc::clone(peer_manager),
            Some(inbound_peer_rx),
            inbound_peer_tx.clone(),
            router.clone(),
            None,
        )
        .with_replication_metrics(replication_metrics)
        .spawn();
        tasks.push(runtime);

        let (client_session, server_session) = flotilla_transport::message::message_session_pair();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (shutdown_request_tx, _shutdown_request_rx) = mpsc::unbounded_channel();
        let client_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let client_notify = Arc::new(Notify::new());
        let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectionEvent>();
        let host = Arc::clone(host);
        let peer_manager = Arc::clone(peer_manager);
        tasks.push(tokio::spawn(async move {
            flotilla_daemon::server::handle_client_session_with_caller(
                server_session,
                host,
                shutdown_request_tx,
                shutdown_rx,
                inbound_peer_tx,
                peer_manager,
                router,
                client_count,
                client_notify,
                peer_connected_tx,
                flotilla_agent::shared_in_memory_agent_state_store(),
                None,
                None,
            )
            .await;
        }));
        clients.push(SocketDaemon::from_session_stateful(client_session).await?);
        shutdown_txs.push(shutdown_tx);
    }

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut ready = true;
            for (index, host) in hosts.iter().enumerate() {
                let topology = host.get_topology().await.map_err(|error| error.to_string())?;
                let links = (0..hosts.len()).filter(|other| *other != index && linked(index.min(*other), index.max(*other))).count();
                ready &= topology.routes.iter().filter(|route| route.connected).count() >= links;
            }
            if ready {
                return Ok::<(), String>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| "timed out waiting for in-memory request mesh to connect".to_string())??;

    Ok(InMemoryRequestMesh { hosts, clients, shutdown_txs, _tasks: tasks, _homes: Vec::new() })
}

pub async fn spawn_in_memory_request_topology(
    leader: impl Into<HomedDaemon>,
    follower: impl Into<HomedDaemon>,
) -> Result<InMemoryRequestTopology, String> {
    spawn_in_memory_request_topology_stateful(leader, follower).await
}

/// Like [`spawn_in_memory_request_topology`] but performs a Hello handshake so
/// the client gets a server-assigned `session_id` for cursor ownership.
pub async fn spawn_in_memory_request_topology_stateful(
    leader: impl Into<HomedDaemon>,
    follower: impl Into<HomedDaemon>,
) -> Result<InMemoryRequestTopology, String> {
    spawn_in_memory_request_topology_stateful_with_options(leader, follower, None, None, None).await
}

/// Stateful in-memory topology whose client declares an explicit attention
/// character during the Hello handshake.
pub async fn spawn_in_memory_request_topology_stateful_with_surface(
    leader: impl Into<HomedDaemon>,
    follower: impl Into<HomedDaemon>,
    surface: SurfaceDeclaration,
) -> Result<InMemoryRequestTopology, String> {
    spawn_in_memory_request_topology_stateful_with_options(leader, follower, Some(surface), None, None).await
}

pub async fn spawn_in_memory_request_topology_stateful_with_caller(
    leader: impl Into<HomedDaemon>,
    follower: impl Into<HomedDaemon>,
    surface: SurfaceDeclaration,
    caller: CommandCaller,
) -> Result<InMemoryRequestTopology, String> {
    spawn_in_memory_request_topology_stateful_with_options(leader, follower, Some(surface), Some(caller), None).await
}

pub async fn spawn_in_memory_request_topology_stateful_with_caller_and_blob_store(
    leader: impl Into<HomedDaemon>,
    follower: impl Into<HomedDaemon>,
    surface: SurfaceDeclaration,
    caller: CommandCaller,
    blob_store: Arc<TieredBlobStore>,
) -> Result<InMemoryRequestTopology, String> {
    spawn_in_memory_request_topology_stateful_with_options(leader, follower, Some(surface), Some(caller), Some(blob_store)).await
}

async fn spawn_in_memory_request_topology_stateful_with_options(
    leader: impl Into<HomedDaemon>,
    follower: impl Into<HomedDaemon>,
    surface: Option<SurfaceDeclaration>,
    caller: Option<CommandCaller>,
    blob_store: Option<Arc<TieredBlobStore>>,
) -> Result<InMemoryRequestTopology, String> {
    let HomedDaemon { daemon: leader, home: leader_home } = leader.into();
    let HomedDaemon { daemon: follower, home: follower_home } = follower.into();
    let leader_host = leader.host_name().clone();
    let follower_host = follower.host_name().clone();

    let leader_peer_manager = Arc::new(Mutex::new(PeerManager::new(leader.node_id().clone())));
    let follower_peer_manager = Arc::new(Mutex::new(PeerManager::new(follower.node_id().clone())));

    let (leader_transport, follower_transport) = channel_transport_pair_with_nodes(
        NodeInfo::new(leader.node_id().clone(), leader_host.to_string()),
        NodeInfo::new(follower.node_id().clone(), follower_host.to_string()),
    );
    {
        let mut pm = leader_peer_manager.lock().await;
        pm.add_configured_target(
            flotilla_protocol::ConfigLabel("follower".into()),
            follower_host.clone(),
            None,
            Box::new(leader_transport),
        );
    }
    {
        let mut pm = follower_peer_manager.lock().await;
        pm.add_configured_target(flotilla_protocol::ConfigLabel("leader".into()), leader_host.clone(), None, Box::new(follower_transport));
    }

    let (leader_inbound_peer_tx, leader_inbound_peer_rx) = mpsc::channel(256);
    let (follower_inbound_peer_tx, follower_inbound_peer_rx) = mpsc::channel(256);
    let leader_remote_router = build_remote_command_router(&leader, &leader_peer_manager);
    if let Some(blob_store) = blob_store {
        leader_remote_router.install_blob_store(blob_store)?;
    }
    let follower_remote_router = build_remote_command_router(&follower, &follower_peer_manager);
    let leader_stream_holds = StreamHolds::default();
    let follower_stream_holds = StreamHolds::default();

    let (leader_runtime_handle, _leader_peer_connected_tx): (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectionEvent>) =
        PeerRuntime::new(
            Arc::clone(&leader),
            Arc::clone(&leader_peer_manager),
            Some(leader_inbound_peer_rx),
            leader_inbound_peer_tx.clone(),
            leader_remote_router.clone(),
            None,
        )
        .with_stream_holds(leader_stream_holds.clone())
        .spawn();
    let (follower_runtime_handle, _follower_peer_connected_tx): (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectionEvent>) =
        PeerRuntime::new(
            Arc::clone(&follower),
            Arc::clone(&follower_peer_manager),
            Some(follower_inbound_peer_rx),
            follower_inbound_peer_tx,
            follower_remote_router,
            None,
        )
        .with_stream_holds(follower_stream_holds.clone())
        .spawn();

    // Spawn the server-side client session handler BEFORE the client handshake,
    // because from_session_stateful sends Hello and blocks waiting for the reply.
    let (client_session, server_session) = flotilla_transport::message::message_session_pair();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (shutdown_request_tx, _shutdown_request_rx) = mpsc::unbounded_channel();
    let client_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let client_notify = Arc::new(Notify::new());
    let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectionEvent>();
    let leader_for_client = Arc::clone(&leader);
    let leader_peer_manager_for_client = Arc::clone(&leader_peer_manager);
    let leader_inbound_peer_tx_for_client = leader_inbound_peer_tx;
    let leader_remote_router_for_client = leader_remote_router;
    let client_count_for_task = Arc::clone(&client_count);
    let client_notify_for_task = Arc::clone(&client_notify);
    let client_session_handle = tokio::spawn(async move {
        flotilla_daemon::server::handle_client_session_with_caller(
            server_session,
            leader_for_client,
            shutdown_request_tx,
            shutdown_rx,
            leader_inbound_peer_tx_for_client,
            leader_peer_manager_for_client,
            leader_remote_router_for_client,
            client_count_for_task,
            client_notify_for_task,
            peer_connected_tx,
            flotilla_agent::shared_in_memory_agent_state_store(),
            None,
            caller,
        )
        .await;
    });

    // Now the server is listening — the handshake can proceed.
    let client = match surface {
        Some(surface) => SocketDaemon::from_session_stateful_with_surface(client_session, surface).await?,
        None => SocketDaemon::from_session_stateful(client_session).await?,
    };

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let leader_topology = leader.get_topology().await.map_err(|e| e.to_string())?;
            let follower_topology = follower.get_topology().await.map_err(|e| e.to_string())?;
            let leader_ready =
                leader_topology.routes.iter().any(|route| route.target.node_id == follower.node_id().clone() && route.connected);
            let follower_ready =
                follower_topology.routes.iter().any(|route| route.target.node_id == leader.node_id().clone() && route.connected);
            if leader_ready && follower_ready {
                return Ok::<(), String>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| "timed out waiting for in-memory request topology to connect".to_string())??;

    Ok(InMemoryRequestTopology {
        leader,
        follower,
        client,
        leader_host,
        follower_host,
        shutdown_tx,
        leader_stream_holds,
        follower_stream_holds,
        _tasks: vec![leader_runtime_handle, follower_runtime_handle, client_session_handle],
        _homes: leader_home.into_iter().chain(follower_home).collect(),
    })
}

pub type EnvelopeFilter = Arc<dyn Fn(PeerWireMessage) -> Option<PeerWireMessage> + Send + Sync>;

struct FilteredTransport {
    inner: ChannelTransport,
    filter: EnvelopeFilter,
}

struct FilteredSender {
    inner: Arc<dyn PeerSender>,
    filter: EnvelopeFilter,
}

#[async_trait::async_trait]
impl PeerSender for FilteredSender {
    async fn send(&self, message: PeerWireMessage) -> Result<(), String> {
        match (self.filter)(message) {
            Some(message) => self.inner.send(message).await,
            None => Ok(()),
        }
    }

    async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
        self.inner.retire(reason).await
    }
}

#[async_trait::async_trait]
impl PeerTransport for FilteredTransport {
    async fn connect(&mut self) -> Result<(), String> {
        self.inner.connect().await
    }
    async fn disconnect(&mut self) -> Result<(), String> {
        self.inner.disconnect().await
    }
    fn status(&self) -> PeerConnectionStatus {
        self.inner.status()
    }
    fn connection_address(&self) -> String {
        self.inner.connection_address()
    }
    async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerWireMessage>, String> {
        self.inner.subscribe().await
    }
    fn sender(&self) -> Option<Arc<dyn PeerSender>> {
        self.inner.sender().map(|inner| Arc::new(FilteredSender { inner, filter: Arc::clone(&self.filter) }) as Arc<dyn PeerSender>)
    }
    fn remote_session_id(&self) -> Option<uuid::Uuid> {
        self.inner.remote_session_id()
    }
    fn remote_node_info(&self) -> Option<NodeInfo> {
        self.inner.remote_node_info()
    }
}

/// Spawn the peer networking runtime with pre-built components.
///
/// Test-only entry point: callers provide a PeerManager with pre-configured
/// senders (e.g. CapturePeerSender). Passes `None` for `inbound_peer_rx` to skip
/// the inbound connection task — tests drive the outbound task via the returned
/// `PeerConnectionEvent` sender.
pub fn spawn_test_peer_networking(
    daemon: Arc<InProcessDaemon>,
    peer_manager: Arc<Mutex<PeerManager>>,
) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectionEvent>) {
    // Receiver dropped intentionally — None is passed for the inbound task,
    // so no messages are forwarded; the sender satisfies the runtime signature.
    let (inbound_peer_tx, _inbound_peer_rx) = mpsc::channel(256);
    let remote_command_router = build_remote_command_router(&daemon, &peer_manager);
    PeerRuntime::new(
        daemon,
        peer_manager,
        None, // No inbound task — test drives outbound via PeerConnectionEvent
        inbound_peer_tx,
        remote_command_router,
        None,
    )
    .spawn()
}
