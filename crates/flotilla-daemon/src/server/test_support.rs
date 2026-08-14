use std::{collections::HashMap, sync::Arc, time::Duration};

use flotilla_client::SocketDaemon;
use flotilla_core::{daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{
    result_set::{ConvoyPhase, ConvoyRow},
    HostName, NodeInfo, ResourceRef, SurfaceDeclaration,
};
use flotilla_resources::{api_version, Convoy, InputMeta, Project, ProjectSpec, Resource, Stance, WorkflowTemplate};
use tokio::sync::{mpsc, watch, Mutex, Notify};

use super::{build_remote_command_router, handle_client_session, spawn_peer_networking_runtime};
use crate::{
    peer::{channel_transport::channel_transport_pair_with_nodes, PeerManager},
    server::PeerConnectionEvent,
};

pub async fn apply_convoy_replica_feed(daemon: &InProcessDaemon, namespace: &str, name: &str, home: HostName) {
    let resource = ResourceRef::new(api_version(Convoy::API_PATHS), Convoy::API_PATHS.kind, namespace, name).on_host(home.clone());
    let row = ConvoyRow::builder().resource(resource).name(name).workflow_ref("scratch").phase(ConvoyPhase::Pending).build();
    let state = daemon.aggregator_projection_state().await;
    state.write().await.replace_replica_rows(HashMap::from([(home, HashMap::from([(row.resource.clone(), row)]))]));
}

pub async fn seed_trusted_remote_convoy_project(daemon: &InProcessDaemon, namespace: &str) {
    let mut workflow = flotilla_resources::single_agent_contained_workflow_spec();
    workflow.vessels[0].stance = Stance::Trusted;
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
    spawn_in_memory_request_topology_stateful_with_optional_surface(leader, follower, None).await
}

/// Stateful in-memory topology whose client declares an explicit attention
/// character during the Hello handshake.
pub async fn spawn_in_memory_request_topology_stateful_with_surface(
    leader: Arc<InProcessDaemon>,
    follower: Arc<InProcessDaemon>,
    surface: SurfaceDeclaration,
) -> Result<InMemoryRequestTopology, String> {
    spawn_in_memory_request_topology_stateful_with_optional_surface(leader, follower, Some(surface)).await
}

async fn spawn_in_memory_request_topology_stateful_with_optional_surface(
    leader: Arc<InProcessDaemon>,
    follower: Arc<InProcessDaemon>,
    surface: Option<SurfaceDeclaration>,
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
        handle_client_session(
            server_session,
            leader_for_client,
            shutdown_rx,
            leader_inbound_peer_tx_for_client,
            leader_peer_manager_for_client,
            leader_remote_router_for_client,
            client_count_for_task,
            client_notify_for_task,
            peer_connected_tx,
            flotilla_core::agents::shared_in_memory_agent_state_store(),
            None,
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
