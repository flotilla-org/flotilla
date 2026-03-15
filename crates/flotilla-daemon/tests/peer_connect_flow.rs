//! Integration test for peer connect / reconnect local state send flow (#306).

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::discovery::test_support::{fake_discovery, init_git_repo},
};
use flotilla_daemon::{
    peer::{test_support::ensure_test_connection_generation, PeerManager, PeerSender},
    server::PeerConnectedNotice,
};
use flotilla_protocol::{GoodbyeReason, HostName, PeerWireMessage};
use tokio::sync::Mutex;

struct CapturePeerSender {
    sent: Arc<StdMutex<Vec<PeerWireMessage>>>,
}

#[async_trait]
impl PeerSender for CapturePeerSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        self.sent.lock().expect("lock").push(msg);
        Ok(())
    }

    async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
        self.sent.lock().expect("lock").push(PeerWireMessage::Goodbye { reason });
        Ok(())
    }
}

#[tokio::test]
async fn peer_connect_triggers_local_state_send() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_path = tmp.path().join("repo");
    init_git_repo(&repo_path);
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let host_a = HostName::new("host-a");
    let host_b = HostName::new("host-b");

    let daemon = InProcessDaemon::new(vec![repo_path.clone()], config, fake_discovery(false), host_a.clone()).await;

    // Refresh so local_data_version > 0
    daemon.refresh(&repo_path).await.expect("refresh");

    let sent = Arc::new(StdMutex::new(Vec::new()));
    let sender: Arc<dyn PeerSender> = Arc::new(CapturePeerSender { sent: Arc::clone(&sent) });
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(host_a.clone())));
    let generation = {
        let mut pm = peer_manager.lock().await;
        ensure_test_connection_generation(&mut pm, &host_b, || Arc::clone(&sender))
    };

    let (_handle, peer_connected_tx) = flotilla_daemon::server::spawn_test_peer_networking(Arc::clone(&daemon), Arc::clone(&peer_manager));

    peer_connected_tx.send(PeerConnectedNotice { peer: host_b.clone(), generation }).expect("send notice");

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let messages = sent.lock().expect("lock");
    assert!(
        messages.iter().any(|m| matches!(m, PeerWireMessage::HostSummary(s) if s.host_name == host_a)),
        "peer should receive HostSummary from host-a, got: {messages:?}"
    );
    assert!(
        messages.iter().any(|m| matches!(m, PeerWireMessage::Data(d) if d.origin_host == host_a)),
        "peer should receive repo data from host-a, got: {messages:?}"
    );
}

#[tokio::test]
async fn peer_reconnect_resends_local_state() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_path = tmp.path().join("repo");
    init_git_repo(&repo_path);
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let host_a = HostName::new("host-a");
    let host_b = HostName::new("host-b");

    let daemon = InProcessDaemon::new(vec![repo_path.clone()], config, fake_discovery(false), host_a.clone()).await;
    daemon.refresh(&repo_path).await.expect("refresh");

    let sent = Arc::new(StdMutex::new(Vec::new()));
    let sender: Arc<dyn PeerSender> = Arc::new(CapturePeerSender { sent: Arc::clone(&sent) });
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(host_a.clone())));
    let gen1 = {
        let mut pm = peer_manager.lock().await;
        ensure_test_connection_generation(&mut pm, &host_b, || Arc::clone(&sender))
    };

    let (_handle, peer_connected_tx) = flotilla_daemon::server::spawn_test_peer_networking(Arc::clone(&daemon), Arc::clone(&peer_manager));

    peer_connected_tx.send(PeerConnectedNotice { peer: host_b.clone(), generation: gen1 }).expect("send notice 1");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let first_count = sent.lock().expect("lock").len();
    assert!(first_count > 0, "first connect should have sent messages");

    let gen2 = {
        let mut pm = peer_manager.lock().await;
        pm.disconnect_peer(&host_b, gen1);
        ensure_test_connection_generation(&mut pm, &host_b, || Arc::clone(&sender))
    };

    peer_connected_tx.send(PeerConnectedNotice { peer: host_b.clone(), generation: gen2 }).expect("send notice 2");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let total_count = sent.lock().expect("lock").len();
    assert!(total_count > first_count, "reconnect should resend local state (first: {first_count}, total: {total_count})");
}
