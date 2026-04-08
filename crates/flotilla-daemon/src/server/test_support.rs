use std::{sync::Arc, time::Duration};

use flotilla_client::SocketDaemon;
use flotilla_core::{daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{HostName, NodeInfo};
use tokio::sync::{mpsc, watch, Mutex, Notify};

use super::{build_remote_command_router, handle_client_session, spawn_peer_networking_runtime};
use crate::{
    peer::{channel_transport::channel_transport_pair_with_nodes, PeerManager},
    server::PeerConnectedNotice,
};

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

    let (leader_peer_data_tx, leader_peer_data_rx) = mpsc::channel(256);
    let (follower_peer_data_tx, follower_peer_data_rx) = mpsc::channel(256);
    let leader_remote_router = build_remote_command_router(&leader, &leader_peer_manager);
    let follower_remote_router = build_remote_command_router(&follower, &follower_peer_manager);

    let (leader_runtime_handle, _leader_peer_connected_tx): (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectedNotice>) =
        spawn_peer_networking_runtime(
            Arc::clone(&leader),
            Arc::clone(&leader_peer_manager),
            Some(leader_peer_data_rx),
            leader_peer_data_tx.clone(),
            leader_remote_router.clone(),
        );
    let (follower_runtime_handle, _follower_peer_connected_tx): (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectedNotice>) =
        spawn_peer_networking_runtime(
            Arc::clone(&follower),
            Arc::clone(&follower_peer_manager),
            Some(follower_peer_data_rx),
            follower_peer_data_tx,
            follower_remote_router,
        );

    let (client_session, server_session) = flotilla_transport::message::message_session_pair();
    let client = SocketDaemon::from_session(client_session)?;
    spawn_topology_with_client(
        leader,
        follower,
        leader_host,
        follower_host,
        leader_peer_manager,
        leader_peer_data_tx,
        leader_remote_router,
        leader_runtime_handle,
        follower_runtime_handle,
        server_session,
        client,
    )
    .await
}

/// Like [`spawn_in_memory_request_topology`] but performs a Hello handshake so
/// the client gets a server-assigned `session_id` for cursor ownership.
pub async fn spawn_in_memory_request_topology_stateful(
    leader: Arc<InProcessDaemon>,
    follower: Arc<InProcessDaemon>,
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

    let (leader_peer_data_tx, leader_peer_data_rx) = mpsc::channel(256);
    let (follower_peer_data_tx, follower_peer_data_rx) = mpsc::channel(256);
    let leader_remote_router = build_remote_command_router(&leader, &leader_peer_manager);
    let follower_remote_router = build_remote_command_router(&follower, &follower_peer_manager);

    let (leader_runtime_handle, _leader_peer_connected_tx): (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectedNotice>) =
        spawn_peer_networking_runtime(
            Arc::clone(&leader),
            Arc::clone(&leader_peer_manager),
            Some(leader_peer_data_rx),
            leader_peer_data_tx.clone(),
            leader_remote_router.clone(),
        );
    let (follower_runtime_handle, _follower_peer_connected_tx): (tokio::task::JoinHandle<()>, mpsc::UnboundedSender<PeerConnectedNotice>) =
        spawn_peer_networking_runtime(
            Arc::clone(&follower),
            Arc::clone(&follower_peer_manager),
            Some(follower_peer_data_rx),
            follower_peer_data_tx,
            follower_remote_router,
        );

    // Spawn the server-side client session handler BEFORE the client handshake,
    // because from_session_stateful sends Hello and blocks waiting for the reply.
    let (client_session, server_session) = flotilla_transport::message::message_session_pair();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let client_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let client_notify = Arc::new(Notify::new());
    let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();
    let leader_for_client = Arc::clone(&leader);
    let leader_peer_manager_for_client = Arc::clone(&leader_peer_manager);
    let leader_peer_data_tx_for_client = leader_peer_data_tx;
    let leader_remote_router_for_client = leader_remote_router;
    let client_count_for_task = Arc::clone(&client_count);
    let client_notify_for_task = Arc::clone(&client_notify);
    let client_session_handle = tokio::spawn(async move {
        handle_client_session(
            server_session,
            leader_for_client,
            shutdown_rx,
            leader_peer_data_tx_for_client,
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
    let client = SocketDaemon::from_session_stateful(client_session).await?;

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

#[allow(clippy::too_many_arguments)]
async fn spawn_topology_with_client(
    leader: Arc<InProcessDaemon>,
    follower: Arc<InProcessDaemon>,
    leader_host: HostName,
    follower_host: HostName,
    leader_peer_manager: Arc<Mutex<PeerManager>>,
    leader_peer_data_tx: mpsc::Sender<super::InboundPeerEnvelope>,
    leader_remote_router: super::remote_commands::RemoteCommandRouter,
    leader_runtime_handle: tokio::task::JoinHandle<()>,
    follower_runtime_handle: tokio::task::JoinHandle<()>,
    server_session: flotilla_transport::message::MessageSession,
    client: Arc<SocketDaemon>,
) -> Result<InMemoryRequestTopology, String> {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let client_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let client_notify = Arc::new(Notify::new());
    // The in-memory request topology only needs the sender side because readiness is polled from
    // each daemon's topology view below; peer-connected notices are intentionally ignored here.
    let (peer_connected_tx, _peer_connected_rx) = mpsc::unbounded_channel::<PeerConnectedNotice>();
    let leader_for_client = Arc::clone(&leader);
    let leader_peer_manager_for_client = Arc::clone(&leader_peer_manager);
    let leader_peer_data_tx_for_client = leader_peer_data_tx;
    let leader_remote_router_for_client = leader_remote_router;
    let client_count_for_task = Arc::clone(&client_count);
    let client_notify_for_task = Arc::clone(&client_notify);
    let client_session_handle = tokio::spawn(async move {
        handle_client_session(
            server_session,
            leader_for_client,
            shutdown_rx,
            leader_peer_data_tx_for_client,
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
