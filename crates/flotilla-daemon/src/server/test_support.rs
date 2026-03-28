use std::{sync::Arc, time::Duration};

use flotilla_client::SocketDaemon;
use flotilla_core::{daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::HostName;
use tokio::sync::{mpsc, watch, Mutex, Notify};

use super::{build_remote_command_router, handle_client_session, spawn_peer_networking_runtime};
use crate::{
    peer::{channel_transport::channel_transport_pair, PeerManager},
    server::PeerConnectedNotice,
};

pub struct InMemoryRequestTopology {
    pub leader: Arc<InProcessDaemon>,
    pub follower: Arc<InProcessDaemon>,
    pub client: Arc<SocketDaemon>,
    pub leader_host: HostName,
    pub follower_host: HostName,
    _shutdown_tx: watch::Sender<bool>,
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

    let leader_peer_manager = Arc::new(Mutex::new(PeerManager::new(leader_host.clone())));
    let follower_peer_manager = Arc::new(Mutex::new(PeerManager::new(follower_host.clone())));

    let (leader_transport, follower_transport) = channel_transport_pair(leader_host.clone(), follower_host.clone());
    {
        let mut pm = leader_peer_manager.lock().await;
        pm.add_peer(follower_host.clone(), Box::new(leader_transport));
    }
    {
        let mut pm = follower_peer_manager.lock().await;
        pm.add_peer(leader_host.clone(), Box::new(follower_transport));
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
            let leader_ready = leader_topology.routes.iter().any(|route| route.target == follower_host && route.connected);
            let follower_ready = follower_topology.routes.iter().any(|route| route.target == leader_host && route.connected);
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
        _shutdown_tx: shutdown_tx,
        _tasks: vec![leader_runtime_handle, follower_runtime_handle, client_session_handle],
    })
}
