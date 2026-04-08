//! Integration test for peer connect / reconnect local state send flow (#306).
//!
//! Verifies the end-to-end path: peer connects → outbound task receives
//! PeerConnectedNotice → send_local_to_peer sends data → peer receives it.
//!
//! Uses a `NotifyPeerSender` that signals a `tokio::sync::Notify` on each
//! message, replacing timing-dependent sleeps with deterministic waits.

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use flotilla_core::{
    config::ConfigStore,
    in_process::InProcessDaemon,
    providers::discovery::test_support::{fake_discovery, init_git_repo},
};
use flotilla_daemon::{
    peer::{test_support::ensure_test_connection_generation, PeerManager, PeerSender},
    server::PeerConnectedNotice,
};
use flotilla_protocol::{GoodbyeReason, HostName, NodeId, PeerWireMessage, RepoSelector};
use tokio::sync::{Mutex, Notify};

/// Peer sender that captures messages and signals a `Notify` on each send,
/// allowing tests to wait deterministically instead of sleeping.
struct NotifyPeerSender {
    sent: Arc<StdMutex<Vec<PeerWireMessage>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl PeerSender for NotifyPeerSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        self.sent.lock().expect("lock").push(msg);
        self.notify.notify_waiters();
        Ok(())
    }

    async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
        self.sent.lock().expect("lock").push(PeerWireMessage::Goodbye { reason });
        self.notify.notify_waiters();
        Ok(())
    }
}

/// Wait until the captured message count reaches `target`, or timeout.
async fn wait_for_messages(sent: &Arc<StdMutex<Vec<PeerWireMessage>>>, notify: &Notify, target: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            // Register interest BEFORE checking the condition so a
            // notify_waiters() that fires between the check and the
            // await is not lost.
            let notified = notify.notified();
            if sent.lock().expect("lock").len() >= target {
                return;
            }
            notified.await;
        }
    })
    .await
    .expect("timed out waiting for peer messages");
}

#[tokio::test]
async fn peer_connect_triggers_local_state_send() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_path = tmp.path().join("repo");
    init_git_repo(&repo_path);
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let host_a = HostName::new("host-a");
    let _host_b = HostName::new("host-b");
    let node_b = NodeId::new("host-b");

    let daemon = InProcessDaemon::new(vec![repo_path.clone()], config, fake_discovery(false), host_a.clone()).await;
    let node_a = daemon.node_id().clone();
    daemon.refresh(&RepoSelector::Path(repo_path.clone())).await.expect("refresh");

    let sent = Arc::new(StdMutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let sender: Arc<dyn PeerSender> = Arc::new(NotifyPeerSender { sent: Arc::clone(&sent), notify: Arc::clone(&notify) });
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(node_a.clone())));
    let generation = {
        let mut pm = peer_manager.lock().await;
        ensure_test_connection_generation(&mut pm, &node_b, || Arc::clone(&sender))
    };

    let (_handle, peer_connected_tx) = flotilla_daemon::server::spawn_test_peer_networking(Arc::clone(&daemon), Arc::clone(&peer_manager));

    peer_connected_tx.send(PeerConnectedNotice { peer: node_b.clone(), generation }).expect("send notice");

    // Expect at least 2 messages: HostSummary + Data for the repo
    wait_for_messages(&sent, &notify, 2).await;

    let messages = sent.lock().expect("lock");
    assert!(
        messages.iter().any(|m| matches!(m, PeerWireMessage::HostSummary(s) if s.node.node_id == node_a)),
        "peer should receive HostSummary from host-a, got: {messages:?}"
    );
    assert!(
        messages.iter().any(|m| matches!(m, PeerWireMessage::Data(d) if d.origin_node_id == node_a)),
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
    let _host_b = HostName::new("host-b");
    let node_b = NodeId::new("host-b");

    let daemon = InProcessDaemon::new(vec![repo_path.clone()], config, fake_discovery(false), host_a.clone()).await;
    let node_a = daemon.node_id().clone();
    daemon.refresh(&RepoSelector::Path(repo_path.clone())).await.expect("refresh");

    let sent = Arc::new(StdMutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let sender: Arc<dyn PeerSender> = Arc::new(NotifyPeerSender { sent: Arc::clone(&sent), notify: Arc::clone(&notify) });
    let peer_manager = Arc::new(Mutex::new(PeerManager::new(node_a.clone())));
    let gen1 = {
        let mut pm = peer_manager.lock().await;
        ensure_test_connection_generation(&mut pm, &node_b, || Arc::clone(&sender))
    };

    let (_handle, peer_connected_tx) = flotilla_daemon::server::spawn_test_peer_networking(Arc::clone(&daemon), Arc::clone(&peer_manager));

    // First connection
    peer_connected_tx.send(PeerConnectedNotice { peer: node_b.clone(), generation: gen1 }).expect("send notice 1");
    wait_for_messages(&sent, &notify, 2).await;

    let first_count = sent.lock().expect("lock").len();
    assert!(first_count > 0, "first connect should have sent messages");

    // Disconnect + reconnect
    let gen2 = {
        let mut pm = peer_manager.lock().await;
        pm.disconnect_peer(&node_b, gen1);
        ensure_test_connection_generation(&mut pm, &node_b, || Arc::clone(&sender))
    };

    // Second connection — expect at least first_count + 2 more messages
    peer_connected_tx.send(PeerConnectedNotice { peer: node_b.clone(), generation: gen2 }).expect("send notice 2");
    wait_for_messages(&sent, &notify, first_count + 2).await;

    let total_count = sent.lock().expect("lock").len();
    assert!(total_count > first_count, "reconnect should resend local state (first: {first_count}, total: {total_count})");
}
