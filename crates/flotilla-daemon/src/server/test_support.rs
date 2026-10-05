use std::{collections::HashMap, sync::Arc, time::Duration};

use flotilla_client::SocketDaemon;
use flotilla_core::{daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{
    result_set::{ConvoyPhase, ConvoyRow},
    CommandCaller, GoodbyeReason, HostName, NodeId, NodeInfo, PeerWireMessage, ResourceRef, SurfaceDeclaration,
};
use flotilla_resources::{api_version, Convoy, InputMeta, Project, ProjectSpec, Resource, WorkflowTemplate};
use tokio::sync::{mpsc, watch, Mutex, Notify};

use super::{build_remote_command_router, peer_runtime::PeerRuntime, spawn_peer_networking_runtime};
use crate::{
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
    let row = ConvoyRow::builder().resource(resource).name(name).workflow_ref("scratch").phase(ConvoyPhase::Pending).build();
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
    _tasks: Vec<tokio::task::JoinHandle<()>>,
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
    pub digest_driver: Option<DigestDriver>,
    pub hosts: Vec<Arc<InProcessDaemon>>,
    pub clients: Vec<Arc<SocketDaemon>>,
    pub shutdown_txs: Vec<watch::Sender<bool>>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for InMemoryRequestMesh {
    fn drop(&mut self) {
        for task in &self._tasks {
            task.abort();
        }
    }
}

type DigestControls = HashMap<(NodeId, NodeId, String), Arc<DigestControl>>;

/// Explicit digest rounds for routed test sessions. Production keeps its periodic timer.
#[derive(Clone, Default)]
pub struct DigestDriver {
    controls: Arc<std::sync::Mutex<DigestControls>>,
}

pub(super) struct DigestControl {
    sender: mpsc::UnboundedSender<tokio::sync::oneshot::Sender<Result<bool, String>>>,
    pub(super) requests: Mutex<mpsc::UnboundedReceiver<tokio::sync::oneshot::Sender<Result<bool, String>>>>,
    pub(super) ready: watch::Sender<bool>,
}

impl DigestDriver {
    pub(super) fn control(&self, holder: &NodeId, origin: &NodeId, kind: &str) -> Arc<DigestControl> {
        self.controls
            .lock()
            .expect("digest controls")
            .entry((holder.clone(), origin.clone(), kind.to_string()))
            .or_insert_with(|| {
                let (sender, requests) = mpsc::unbounded_channel();
                let (ready, _) = watch::channel(false);
                Arc::new(DigestControl { sender, requests: Mutex::new(requests), ready })
            })
            .clone()
    }

    /// Wait until the current primary watch has applied a bookmark.
    /// Readiness resets when it restarts; later bookmarks keep it ready.
    pub async fn watch_ready(&self, holder: &NodeId, origin: &NodeId, kind: &str) -> Result<(), String> {
        let control = self.control(holder, origin, kind);
        let mut ready = control.ready.subscribe();
        ready.wait_for(|ready| *ready).await.map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Returns after reconciliation, including persisted cursor confirmation.
    /// Queued requests survive unrelated watch errors and run on its replacement.
    /// `true` means repair; errors preserve the previous replica set and cursor.
    pub async fn round(&self, holder: &NodeId, origin: &NodeId, kind: &str) -> Result<bool, String> {
        self.watch_ready(holder, origin, kind).await?;
        let control = self.control(holder, origin, kind);
        let (completion, result) = tokio::sync::oneshot::channel();
        control.sender.send(completion).map_err(|error| error.to_string())?;
        result.await.map_err(|error| error.to_string())?
    }
}

pub async fn spawn_in_memory_request_mesh(hosts: Vec<Arc<InProcessDaemon>>) -> Result<InMemoryRequestMesh, String> {
    spawn_in_memory_request_mesh_with_replication_kinds(hosts, None).await
}

/// Exercise the production router and replicators for the kinds a scenario authors.
/// `None` preserves full-fleet replication; a selected set avoids starting unrelated
/// watch commands on every connection in generated, repeatedly rebuilt meshes.
pub async fn spawn_in_memory_request_mesh_with_replication_kinds(
    hosts: Vec<Arc<InProcessDaemon>>,
    replication_kinds: Option<&'static [&'static str]>,
) -> Result<InMemoryRequestMesh, String> {
    spawn_in_memory_request_mesh_with_filter(hosts, replication_kinds, Arc::new(Some)).await
}

/// A network-boundary fault injector for replication scenarios. Returning None
/// drops one envelope; changing a request can simulate a failed authoritative list.
pub async fn spawn_in_memory_request_mesh_with_filter(
    hosts: Vec<Arc<InProcessDaemon>>,
    replication_kinds: Option<&'static [&'static str]>,
    filter: EnvelopeFilter,
) -> Result<InMemoryRequestMesh, String> {
    spawn_request_mesh(hosts, replication_kinds, filter, None).await
}

/// Like the filtered mesh, with explicit digest rounds replacing test-only timers.
pub async fn spawn_in_memory_request_mesh_with_digest_driver(
    hosts: Vec<Arc<InProcessDaemon>>,
    replication_kinds: Option<&'static [&'static str]>,
    filter: EnvelopeFilter,
) -> Result<InMemoryRequestMesh, String> {
    spawn_request_mesh(hosts, replication_kinds, filter, Some(DigestDriver::default())).await
}

async fn spawn_request_mesh(
    hosts: Vec<Arc<InProcessDaemon>>,
    replication_kinds: Option<&'static [&'static str]>,
    filter: EnvelopeFilter,
    digest_driver: Option<DigestDriver>,
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
    for (host, peer_manager) in hosts.iter().zip(&peer_managers) {
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
        .with_replication_kinds(replication_kinds)
        .with_digest_driver(digest_driver.clone())
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
            super::handle_client_session_with_caller(
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
                flotilla_core::agents::shared_in_memory_agent_state_store(),
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
            for host in &hosts {
                let topology = host.get_topology().await.map_err(|error| error.to_string())?;
                ready &= topology.routes.iter().filter(|route| route.connected).count() == hosts.len() - 1;
            }
            if ready {
                return Ok::<(), String>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| "timed out waiting for in-memory request mesh to connect".to_string())??;

    Ok(InMemoryRequestMesh { digest_driver, hosts, clients, shutdown_txs, _tasks: tasks })
}

pub async fn spawn_in_memory_request_topology(
    leader: Arc<InProcessDaemon>,
    follower: Arc<InProcessDaemon>,
) -> Result<InMemoryRequestTopology, String> {
    spawn_in_memory_request_topology_stateful(leader, follower).await
}

/// Like [`spawn_in_memory_request_topology`] but performs a Hello handshake so
/// the client gets a server-assigned `session_id` for cursor ownership.
pub async fn spawn_in_memory_request_topology_stateful(
    leader: Arc<InProcessDaemon>,
    follower: Arc<InProcessDaemon>,
) -> Result<InMemoryRequestTopology, String> {
    spawn_in_memory_request_topology_stateful_with_options(leader, follower, None, None, None).await
}

/// Stateful in-memory topology whose client declares an explicit attention
/// character during the Hello handshake.
pub async fn spawn_in_memory_request_topology_stateful_with_surface(
    leader: Arc<InProcessDaemon>,
    follower: Arc<InProcessDaemon>,
    surface: SurfaceDeclaration,
) -> Result<InMemoryRequestTopology, String> {
    spawn_in_memory_request_topology_stateful_with_options(leader, follower, Some(surface), None, None).await
}

pub async fn spawn_in_memory_request_topology_stateful_with_caller(
    leader: Arc<InProcessDaemon>,
    follower: Arc<InProcessDaemon>,
    surface: SurfaceDeclaration,
    caller: CommandCaller,
) -> Result<InMemoryRequestTopology, String> {
    spawn_in_memory_request_topology_stateful_with_options(leader, follower, Some(surface), Some(caller), None).await
}

pub async fn spawn_in_memory_request_topology_stateful_with_caller_and_blob_store(
    leader: Arc<InProcessDaemon>,
    follower: Arc<InProcessDaemon>,
    surface: SurfaceDeclaration,
    caller: CommandCaller,
    blob_store: Arc<TieredBlobStore>,
) -> Result<InMemoryRequestTopology, String> {
    spawn_in_memory_request_topology_stateful_with_options(leader, follower, Some(surface), Some(caller), Some(blob_store)).await
}

async fn spawn_in_memory_request_topology_stateful_with_options(
    leader: Arc<InProcessDaemon>,
    follower: Arc<InProcessDaemon>,
    surface: Option<SurfaceDeclaration>,
    caller: Option<CommandCaller>,
    blob_store: Option<Arc<TieredBlobStore>>,
) -> Result<InMemoryRequestTopology, String> {
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

    let (leader_runtime_handle, _leader_peer_connected_tx): (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectionEvent>) =
        spawn_peer_networking_runtime(
            Arc::clone(&leader),
            Arc::clone(&leader_peer_manager),
            Some(leader_inbound_peer_rx),
            leader_inbound_peer_tx.clone(),
            leader_remote_router.clone(),
            None,
        );
    let (follower_runtime_handle, _follower_peer_connected_tx): (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectionEvent>) =
        spawn_peer_networking_runtime(
            Arc::clone(&follower),
            Arc::clone(&follower_peer_manager),
            Some(follower_inbound_peer_rx),
            follower_inbound_peer_tx,
            follower_remote_router,
            None,
        );

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
        super::handle_client_session_with_caller(
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
            flotilla_core::agents::shared_in_memory_agent_state_store(),
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
        _tasks: vec![leader_runtime_handle, follower_runtime_handle, client_session_handle],
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
