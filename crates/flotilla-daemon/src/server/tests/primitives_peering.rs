use std::{
    sync::{atomic::AtomicUsize, Arc},
    time::Duration,
};

use flotilla_config::RemoteHostConfig;
use flotilla_protocol::ConfigLabel;
use flotilla_test_support::TestSocketDir;
use tokio::{
    net::UnixListener,
    sync::{mpsc, watch, Mutex, Notify},
};

use super::{empty_daemon_named, empty_remote_command_router, handle_client};
use crate::{
    peer::{peer_resource_socket_path, PeerManager, SshTransport, SshTransportPaths},
    server::PeerConnectionEvent,
};

fn reason(peer: &str, peer_on: bool, local_on: bool) -> String {
    let word = |on| if on { "on" } else { "off" };
    format!("peer {peer} has [primitives] {}, this host has it {}", word(peer_on), word(local_on))
}

struct Pair {
    dialer: Result<(), String>,
    listener_admitted: bool,
}

/// feta listens as the inbound side; kiwi dials it as the outbound side.
async fn connect(feta_on: bool, kiwi_on: bool) -> Pair {
    let (_feta_tmp, feta) = empty_daemon_named("feta").await;
    let (_kiwi_tmp, kiwi) = empty_daemon_named("kiwi").await;
    feta.set_primitives_switch(feta_on);
    kiwi.set_primitives_switch(kiwi_on);

    let dir = TestSocketDir::new();
    let label = ConfigLabel("feta".into());
    let state = dir.socket_path("state");
    let peers = state.join("peers");
    std::fs::create_dir_all(&peers).expect("peers dir");
    let socket = peer_resource_socket_path(&peers, &label).expect("socket path");
    let listener = UnixListener::bind(&socket).expect("bind");

    let peer_manager = Arc::new(Mutex::new(PeerManager::new(feta.node_id().clone())));
    let (inbound_tx, _inbound_rx) = mpsc::channel(16);
    let (request_tx, _request_rx) = mpsc::unbounded_channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let (connected_tx, _connected_rx) = mpsc::unbounded_channel::<PeerConnectionEvent>();
    let server = {
        let (feta, peer_manager) = (Arc::clone(&feta), Arc::clone(&peer_manager));
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let router = empty_remote_command_router(&feta, &peer_manager);
            handle_client(
                stream,
                feta,
                request_tx,
                shutdown_rx,
                inbound_tx,
                peer_manager,
                router,
                Arc::new(AtomicUsize::new(0)),
                Arc::new(Notify::new()),
                connected_tx,
                flotilla_agent::shared_in_memory_agent_state_store(),
                None,
            )
            .await;
        })
    };

    let config = RemoteHostConfig {
        hostname: "feta.example".into(),
        expected_host_name: "feta".into(),
        expected_node_id: None,
        user: None,
        ssh_multiplex: None,
        ssh_host: false,
        supplies: Vec::new(),
        max_vessels: None,
        single_tenant: false,
        vessel_agent: None,
    };
    let mut transport = SshTransport::new(
        kiwi.node_id().clone(),
        "kiwi".into(),
        label,
        config,
        None,
        kiwi.session_id(),
        Arc::new(flotilla_provider_runtime::ProcessCommandRunner),
        SshTransportPaths { state_dir: state.as_path(), daemon_socket: dir.socket_path("kiwi.sock").as_path() },
    )
    .expect("transport")
    .with_local_primitives(kiwi_on);
    let dialer = transport.connect_socket().await.map(|_| ());

    let dialer_node = kiwi.node_id().clone();
    let mut listener_admitted = false;
    if dialer.is_err() {
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("listener finishes the refused connection")
            .expect("listener task");
    } else {
        for _ in 0..50 {
            listener_admitted = peer_manager.lock().await.current_generation(&dialer_node).is_some();
            if listener_admitted {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        server.abort();
    }
    drop(transport);
    Pair { dialer, listener_admitted }
}

#[tokio::test]
async fn peers_with_the_same_primitives_switch_connect() {
    for on in [true, false] {
        let pair = connect(on, on).await;
        pair.dialer.expect("dialer is admitted");
        assert!(pair.listener_admitted);
    }
}

#[tokio::test]
async fn peers_with_different_primitives_switches_refuse_each_other() {
    for (feta_on, kiwi_on) in [(true, false), (false, true)] {
        let pair = connect(feta_on, kiwi_on).await;
        assert_eq!(pair.dialer, Err(reason("feta", feta_on, kiwi_on)));
        assert!(!pair.listener_admitted);
    }
}
