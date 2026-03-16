//! Integration tests for multi-host peer data flow.
//!
//! These tests verify:
//! - PeerManager stores and retrieves peer snapshot data
//! - merge_provider_data combines checkouts from multiple hosts
//! - Follower mode skips external providers (verified in flotilla-core)
//! - Host attribution appears correctly on work items via InProcessDaemon
//! - Peer data relay excludes the origin host

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::discovery::test_support::{fake_discovery, git_process_discovery, init_git_repo},
};
use flotilla_daemon::peer::{
    channel_transport_pair, merge::merge_provider_data, test_support::handle_test_peer_data, HandleResult, PeerConnectionStatus,
    PeerManager, PeerSender, PeerTransport,
};
use flotilla_protocol::{
    Checkout, CheckoutTarget, Command, CommandAction, CommandResult, DaemonEvent, GoodbyeReason, HostName, HostPath, PeerDataKind,
    PeerDataMessage, PeerWireMessage, ProviderData, RepoIdentity, RepoSelector, VectorClock,
};
use indexmap::IndexMap;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Mock transport
// ---------------------------------------------------------------------------

struct MockTransport {
    status: PeerConnectionStatus,
    sender: Option<Arc<dyn PeerSender>>,
}

impl MockTransport {
    fn with_sender() -> (Self, Arc<Mutex<Vec<PeerWireMessage>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&sent) });
        let transport = Self { status: PeerConnectionStatus::Connected, sender: Some(sender) };
        (transport, sent)
    }
}

struct MockPeerSender {
    sent: Arc<Mutex<Vec<PeerWireMessage>>>,
}

#[async_trait]
impl PeerSender for MockPeerSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        self.sent.lock().expect("lock poisoned").push(msg);
        Ok(())
    }

    async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
        self.sent.lock().expect("lock poisoned").push(PeerWireMessage::Goodbye { reason });
        Ok(())
    }
}

#[async_trait]
impl PeerTransport for MockTransport {
    async fn connect(&mut self) -> Result<(), String> {
        self.status = PeerConnectionStatus::Connected;
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), String> {
        self.status = PeerConnectionStatus::Disconnected;
        Ok(())
    }

    fn status(&self) -> PeerConnectionStatus {
        self.status.clone()
    }

    async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerWireMessage>, String> {
        let (_tx, rx) = mpsc::channel(1);
        Ok(rx)
    }

    fn sender(&self) -> Option<Arc<dyn PeerSender>> {
        self.sender.clone()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_repo() -> RepoIdentity {
    RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() }
}

fn make_checkout(branch: &str) -> Checkout {
    Checkout {
        branch: branch.to_string(),
        is_main: false,
        trunk_ahead_behind: None,
        remote_ahead_behind: None,
        working_tree: None,
        last_commit: None,
        correlation_keys: vec![],
        association_keys: vec![],
    }
}

fn snapshot_msg(origin: &str, seq: u64, data: ProviderData) -> PeerDataMessage {
    let mut clock = VectorClock::default();
    for _ in 0..seq {
        clock.tick(&HostName::new(origin));
    }
    PeerDataMessage {
        origin_host: HostName::new(origin),
        repo_identity: test_repo(),
        repo_path: PathBuf::from("/home/dev/repo"),
        clock,
        kind: PeerDataKind::Snapshot { data: Box::new(data), seq },
    }
}

async fn wait_for_command_result(rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>, command_id: u64) -> CommandResult {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: finished_id, result, .. }) if finished_id == command_id => return result,
                Ok(_) => {}
                Err(e) => panic!("recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for command result")
}

async fn wait_for_local_checkout(daemon: &Arc<InProcessDaemon>, repo: &std::path::Path, branch: &str) -> ProviderData {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            daemon.refresh(repo).await.expect("refresh");
            if let Some((providers, _)) = daemon.get_local_providers(repo).await {
                if providers.checkouts.values().any(|checkout| checkout.branch == branch) {
                    return providers;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("timeout waiting for local checkout providers")
}
async fn empty_daemon_with_host(temp: &tempfile::TempDir, host: &str) -> Arc<InProcessDaemon> {
    let config = Arc::new(ConfigStore::with_base(temp.path().join(format!("config-{host}"))));
    InProcessDaemon::new(vec![], config, fake_discovery(false), HostName::new(host)).await
}
// ---------------------------------------------------------------------------
// Test 1: PeerManager stores snapshot data and returns Updated
// ---------------------------------------------------------------------------

#[tokio::test]
async fn peer_manager_stores_snapshot_and_returns_updated() {
    let mut mgr = PeerManager::new(HostName::new("leader"));

    // Build provider data with a checkout from the follower
    let mut follower_data = ProviderData::default();
    follower_data.checkouts.insert(HostPath::new(HostName::new("follower"), "/home/dev/repo"), make_checkout("feature-branch"));

    let msg = snapshot_msg("follower", 1, follower_data);
    let result =
        handle_test_peer_data(&mut mgr, msg, || Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>)
            .await;

    // Should return Updated with the repo identity
    assert_eq!(result, HandleResult::Updated(test_repo()));

    // Verify stored data is accessible
    let peer_data = mgr.get_peer_data();
    let follower_host = HostName::new("follower");
    assert!(peer_data.contains_key(&follower_host), "peer data should contain follower host");

    let repo_state = &peer_data[&follower_host][&test_repo()];
    assert_eq!(repo_state.seq, 1);
    assert_eq!(repo_state.repo_path, PathBuf::from("/home/dev/repo"));

    // Verify the checkout is in the stored provider data
    let hp = HostPath::new(HostName::new("follower"), "/home/dev/repo");
    assert!(repo_state.provider_data.checkouts.contains_key(&hp), "stored provider data should contain the follower's checkout");
    assert_eq!(repo_state.provider_data.checkouts[&hp].branch, "feature-branch");
}

// ---------------------------------------------------------------------------
// Test 2: Merge combines local and peer checkouts from different hosts
// ---------------------------------------------------------------------------

#[test]
fn merge_combines_checkouts_from_leader_and_follower() {
    // Leader has a checkout on "laptop"
    let local_host = HostName::new("laptop");
    let local = ProviderData {
        checkouts: IndexMap::from([(HostPath::new(local_host.clone(), "/home/dev/repo/main"), make_checkout("main"))]),
        ..Default::default()
    };

    // Follower has a checkout on "desktop"
    let peer_host = HostName::new("desktop");
    let peer_data = ProviderData {
        checkouts: IndexMap::from([(HostPath::new(peer_host.clone(), "/home/dev/repo/feature"), make_checkout("feature-x"))]),
        ..Default::default()
    };

    let merged = merge_provider_data(&local, &local_host, &[(peer_host.clone(), &peer_data)]);

    // Both checkouts should be present
    assert_eq!(merged.checkouts.len(), 2);
    assert!(merged.checkouts.contains_key(&HostPath::new(local_host, "/home/dev/repo/main")));
    assert!(merged.checkouts.contains_key(&HostPath::new(peer_host, "/home/dev/repo/feature")));

    // Verify branch names are correct
    let laptop_checkout = &merged.checkouts[&HostPath::new(HostName::new("laptop"), "/home/dev/repo/main")];
    assert_eq!(laptop_checkout.branch, "main");

    let desktop_checkout = &merged.checkouts[&HostPath::new(HostName::new("desktop"), "/home/dev/repo/feature")];
    assert_eq!(desktop_checkout.branch, "feature-x");
}

// ---------------------------------------------------------------------------
// Test 3: PeerManager + merge end-to-end flow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn peer_manager_to_merge_end_to_end() {
    // Simulate the full flow: follower sends data -> PeerManager stores it ->
    // merge combines it with leader's local data.

    let leader_host = HostName::new("leader");
    let mut mgr = PeerManager::new(leader_host.clone());

    // Follower sends its checkout data
    let mut follower_data = ProviderData::default();
    follower_data.checkouts.insert(HostPath::new(HostName::new("follower"), "/opt/code/repo"), make_checkout("experiment"));

    let msg = snapshot_msg("follower", 1, follower_data);
    let result =
        handle_test_peer_data(&mut mgr, msg, || Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>)
            .await;
    assert_eq!(result, HandleResult::Updated(test_repo()));

    // Leader has its own local data
    let mut local_data = ProviderData::default();
    local_data.checkouts.insert(HostPath::new(leader_host.clone(), "/home/dev/repo"), make_checkout("main"));

    // Collect peer data in the format merge_provider_data expects
    let peer_data = mgr.get_peer_data();
    let peers: Vec<(HostName, &ProviderData)> =
        peer_data.iter().flat_map(|(host, repos)| repos.values().map(move |state| (host.clone(), &state.provider_data))).collect();

    let merged = merge_provider_data(&local_data, &leader_host, &peers);

    // Should contain checkouts from both hosts
    assert_eq!(merged.checkouts.len(), 2);
    assert!(merged.checkouts.contains_key(&HostPath::new(leader_host, "/home/dev/repo")));
    assert!(merged.checkouts.contains_key(&HostPath::new(HostName::new("follower"), "/opt/code/repo")));
}

// ---------------------------------------------------------------------------
// Test 4: Host attribution on work items via InProcessDaemon snapshot
// ---------------------------------------------------------------------------

#[tokio::test]
async fn daemon_snapshot_has_correct_host_attribution() {
    // Create a temp dir with .git so VCS detection finds a checkout
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().to_path_buf();
    std::fs::create_dir_all(repo.join(".git")).expect("create .git dir");

    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, fake_discovery(false), HostName::local()).await;

    // Subscribe first, then trigger a refresh so the snapshot cannot race ahead.
    let mut rx = daemon.subscribe();
    daemon.refresh(&repo).await.expect("refresh");
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(flotilla_protocol::DaemonEvent::RepoSnapshot(snap)) => return *snap,
                Ok(flotilla_protocol::DaemonEvent::RepoDelta(_)) => {
                    // Get full state instead
                    return daemon.get_state(&repo).await.expect("get_state");
                }
                Ok(_) => continue,
                Err(e) => panic!("recv error: {e:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for snapshot");

    // The snapshot should carry the local machine's host name
    assert_eq!(snapshot.host_name, HostName::local(), "snapshot host_name should be the local machine's hostname");

    // If there are work items (there should be at least a main checkout),
    // they should all carry the local host name.
    for item in &snapshot.work_items {
        assert_eq!(item.host, HostName::local(), "work item {:?} should have local host attribution", item.identity);
    }
}

#[tokio::test]
async fn remote_checkout_replication_attributes_checkout_to_follower_host() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let leader_repo = temp.path().join("leader-repo");
    let follower_repo = temp.path().join("follower-repo");
    init_git_repo(&leader_repo);
    init_git_repo(&follower_repo);

    let leader = InProcessDaemon::new(
        vec![leader_repo.clone()],
        Arc::new(ConfigStore::with_base(temp.path().join("leader-config"))),
        git_process_discovery(false),
        HostName::new("leader"),
    )
    .await;
    let follower = InProcessDaemon::new(
        vec![follower_repo.clone()],
        Arc::new(ConfigStore::with_base(temp.path().join("follower-config"))),
        git_process_discovery(false),
        HostName::new("follower"),
    )
    .await;

    leader.refresh(&leader_repo).await.expect("refresh leader");

    let mut follower_rx = follower.subscribe();
    let command_id = follower
        .execute(Command {
            host: None,
            context_repo: None,
            action: CommandAction::Checkout {
                repo: RepoSelector::Path(follower_repo.clone()),
                target: CheckoutTarget::FreshBranch("feat-remote".into()),
                issue_ids: vec![],
            },
        })
        .await
        .expect("dispatch follower checkout");

    let result = wait_for_command_result(&mut follower_rx, command_id).await;
    assert!(
        matches!(result, CommandResult::CheckoutCreated { ref branch, .. } if branch == "feat-remote"),
        "expected checkout creation on follower, got {result:?}"
    );

    let follower_providers = wait_for_local_checkout(&follower, &follower_repo, "feat-remote").await;

    leader.set_peer_providers(&leader_repo, vec![(HostName::new("follower"), follower_providers)], 0).await;

    let snapshot = leader.get_state(&leader_repo).await.expect("leader state");
    assert!(
        snapshot
            .providers
            .checkouts
            .iter()
            .any(|(path, checkout)| path.host == HostName::new("follower") && checkout.branch == "feat-remote"),
        "leader snapshot providers should include the follower checkout"
    );
    let checkout_item = snapshot
        .work_items
        .iter()
        .find(|item| {
            item.checkout_key().is_some_and(|path| path.host == HostName::new("follower")) && item.branch.as_deref() == Some("feat-remote")
        })
        .expect("replicated remote checkout");

    assert_eq!(checkout_item.host, HostName::new("follower"));
}

// ---------------------------------------------------------------------------
// Test 4b: Leader snapshot rebuild includes follower checkout overlay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn daemon_snapshot_includes_follower_checkout_overlay() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().to_path_buf();
    std::fs::create_dir_all(repo.join(".git")).expect("create .git dir");

    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo.clone()], config, fake_discovery(false), HostName::local()).await;

    let follower_host = HostName::new("follower");
    let follower_checkout = HostPath::new(follower_host.clone(), "/remote/repo");

    daemon.refresh(&repo).await.expect("refresh");
    let baseline = daemon.get_state(&repo).await.expect("baseline get_state");
    assert!(
        baseline.work_items.iter().all(|item| item.checkout_key() != Some(&follower_checkout)),
        "baseline snapshot should not already contain follower overlay data"
    );

    let mut follower_data = ProviderData::default();
    follower_data.checkouts.insert(follower_checkout.clone(), make_checkout("feature-x"));

    // `set_peer_providers` updates the overlay and rebuilds the snapshot synchronously,
    // so `get_state` can assert on the merged view immediately.
    daemon.set_peer_providers(&repo, vec![(follower_host.clone(), follower_data)], 0).await;

    let snapshot = daemon.get_state(&repo).await.expect("get_state");

    assert!(
        snapshot.providers.checkouts.contains_key(&follower_checkout),
        "rebuilt snapshot should include the follower checkout in provider data"
    );

    let checkout_items: Vec<_> = snapshot.work_items.iter().filter_map(|item| item.checkout_key().map(|key| (item, key))).collect();
    assert!(
        checkout_items.iter().any(|(_, key)| *key == &follower_checkout),
        "expected follower checkout work item in snapshot: {:?}",
        snapshot.work_items
    );

    let item = checkout_items.iter().find_map(|(item, key)| (*key == &follower_checkout).then_some(*item)).expect("follower checkout item");
    assert_eq!(item.host, follower_host);
}

// ---------------------------------------------------------------------------
// Test 5: Peer data relay excludes origin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn host_summary_round_trip_between_connected_peers() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let leader_daemon = empty_daemon_with_host(&temp, "leader").await;
    let follower_daemon = empty_daemon_with_host(&temp, "follower").await;

    let (leader_transport, follower_transport) = channel_transport_pair(HostName::new("leader"), HostName::new("follower"));
    let mut leader_mgr = PeerManager::new(HostName::new("leader"));
    let mut follower_mgr = PeerManager::new(HostName::new("follower"));
    leader_mgr.add_peer(HostName::new("follower"), Box::new(leader_transport));
    follower_mgr.add_peer(HostName::new("leader"), Box::new(follower_transport));

    let mut leader_receivers = leader_mgr.connect_all().await;
    let _follower_receivers = follower_mgr.connect_all().await;
    let (_peer, generation, mut leader_rx) = leader_receivers.pop().expect("leader receiver");

    follower_mgr
        .send_to(&HostName::new("leader"), PeerWireMessage::HostSummary(follower_daemon.local_host_summary().clone()))
        .await
        .expect("send host summary");

    let inbound = tokio::time::timeout(std::time::Duration::from_secs(2), leader_rx.recv())
        .await
        .expect("timeout waiting for host summary")
        .expect("host summary message");

    let result = leader_mgr
        .handle_inbound(flotilla_daemon::peer::InboundPeerEnvelope {
            msg: inbound,
            connection_generation: generation,
            connection_peer: HostName::new("follower"),
        })
        .await;

    assert_eq!(result, HandleResult::Ignored);
    let stored = leader_mgr.get_peer_host_summaries().get(&HostName::new("follower")).expect("leader stored follower summary");
    assert_eq!(stored.host_name, HostName::new("follower"));
    assert_eq!(stored, follower_daemon.local_host_summary());

    let plan = leader_mgr.disconnect_peer(&HostName::new("follower"), generation);
    assert!(plan.was_active, "disconnect should clear active peer state");
    assert!(
        !leader_mgr.get_peer_host_summaries().contains_key(&HostName::new("follower")),
        "disconnect should clear the stored host summary"
    );

    let _ = leader_daemon;
}

#[tokio::test]
async fn relay_excludes_origin_and_sends_to_other_peers() {
    let mut mgr = PeerManager::new(HostName::new("leader"));

    let (transport_a, sent_a) = MockTransport::with_sender();
    let (transport_b, sent_b) = MockTransport::with_sender();
    let (transport_c, sent_c) = MockTransport::with_sender();
    let sender_a = transport_a.sender().expect("sender");
    let sender_b = transport_b.sender().expect("sender");
    let sender_c = transport_c.sender().expect("sender");

    mgr.add_peer(HostName::new("follower-a"), Box::new(transport_a));
    mgr.add_peer(HostName::new("follower-b"), Box::new(transport_b));
    mgr.add_peer(HostName::new("follower-c"), Box::new(transport_c));
    mgr.register_sender(HostName::new("follower-a"), sender_a);
    mgr.register_sender(HostName::new("follower-b"), sender_b);
    mgr.register_sender(HostName::new("follower-c"), sender_c);

    // Data arrives from follower-a
    let mut data = ProviderData::default();
    data.checkouts.insert(HostPath::new(HostName::new("follower-a"), "/home/dev/repo"), make_checkout("feature"));
    let msg = snapshot_msg("follower-a", 1, data);

    // prepare_relay should return targets for b and c, but not a
    let targets = mgr.prepare_relay(&HostName::new("follower-a"), &msg);
    for (name, sender, relayed_msg) in targets {
        sender.send(PeerWireMessage::Data(relayed_msg)).await.unwrap_or_else(|e| panic!("send to {name} failed: {e}"));
    }

    assert!(sent_a.lock().expect("lock").is_empty(), "origin (follower-a) should NOT receive relayed message");
    assert_eq!(sent_b.lock().expect("lock").len(), 1, "follower-b should receive exactly one relayed message");
    assert_eq!(sent_c.lock().expect("lock").len(), 1, "follower-c should receive exactly one relayed message");
}

// ---------------------------------------------------------------------------
// Test 6: Relay also excludes self (the leader)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn relay_excludes_self_even_if_registered_as_peer() {
    let mut mgr = PeerManager::new(HostName::new("leader"));

    // Register self as a peer (shouldn't happen in practice, but test the guard)
    let (self_transport, sent_self) = MockTransport::with_sender();
    let (other_transport, sent_other) = MockTransport::with_sender();
    let self_sender = self_transport.sender().expect("sender");
    let other_sender = other_transport.sender().expect("sender");

    mgr.add_peer(HostName::new("leader"), Box::new(self_transport));
    mgr.add_peer(HostName::new("follower"), Box::new(other_transport));
    mgr.register_sender(HostName::new("leader"), self_sender);
    mgr.register_sender(HostName::new("follower"), other_sender);

    let msg = snapshot_msg("remote", 1, ProviderData::default());
    let targets = mgr.prepare_relay(&HostName::new("remote"), &msg);
    for (name, sender, relayed_msg) in targets {
        sender.send(PeerWireMessage::Data(relayed_msg)).await.unwrap_or_else(|e| panic!("send to {name} failed: {e}"));
    }

    assert!(sent_self.lock().expect("lock").is_empty(), "self (leader) should NOT receive relayed message");
    assert_eq!(sent_other.lock().expect("lock").len(), 1, "follower should receive the relayed message");
}

// ---------------------------------------------------------------------------
// Test 7: PeerManager ignores messages from self
// ---------------------------------------------------------------------------

#[tokio::test]
async fn peer_manager_ignores_messages_from_self() {
    let mut mgr = PeerManager::new(HostName::new("leader"));

    let msg = snapshot_msg("leader", 1, ProviderData::default());
    let result =
        handle_test_peer_data(&mut mgr, msg, || Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>)
            .await;

    assert_eq!(result, HandleResult::Ignored);
    assert!(mgr.get_peer_data().is_empty(), "no data should be stored for messages from self");
}

// ---------------------------------------------------------------------------
// Test 8: Multiple peers with different repos
// ---------------------------------------------------------------------------

#[tokio::test]
async fn peer_manager_handles_multiple_peers_and_repos() {
    let mut mgr = PeerManager::new(HostName::new("leader"));

    // Follower A sends data
    let mut data_a = ProviderData::default();
    data_a.checkouts.insert(HostPath::new(HostName::new("follower-a"), "/home/a/repo"), make_checkout("branch-a"));
    let msg_a = snapshot_msg("follower-a", 1, data_a);

    // Follower B sends data
    let mut data_b = ProviderData::default();
    data_b.checkouts.insert(HostPath::new(HostName::new("follower-b"), "/home/b/repo"), make_checkout("branch-b"));
    let msg_b = snapshot_msg("follower-b", 2, data_b);

    assert_eq!(
        handle_test_peer_data(&mut mgr, msg_a, || {
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
        })
        .await,
        HandleResult::Updated(test_repo())
    );
    assert_eq!(
        handle_test_peer_data(&mut mgr, msg_b, || {
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
        })
        .await,
        HandleResult::Updated(test_repo())
    );

    let peer_data = mgr.get_peer_data();
    assert_eq!(peer_data.len(), 2, "should have data from two peers");

    // Verify each peer's checkout is accessible
    let a_data = &peer_data[&HostName::new("follower-a")][&test_repo()].provider_data;
    assert_eq!(a_data.checkouts[&HostPath::new(HostName::new("follower-a"), "/home/a/repo")].branch, "branch-a");

    let b_data = &peer_data[&HostName::new("follower-b")][&test_repo()].provider_data;
    assert_eq!(b_data.checkouts[&HostPath::new(HostName::new("follower-b"), "/home/b/repo")].branch, "branch-b");
}

// ---------------------------------------------------------------------------
// Test 9: Merge preserves local service data when peers have none
// ---------------------------------------------------------------------------

#[test]
fn merge_preserves_local_service_data_with_peer_checkouts() {
    use flotilla_protocol::{ChangeRequest, ChangeRequestStatus};

    let local_host = HostName::new("leader");
    let mut local = ProviderData::default();
    local.checkouts.insert(HostPath::new(local_host.clone(), "/home/dev/repo"), make_checkout("main"));
    local.change_requests.insert("PR-42".into(), ChangeRequest {
        title: "Add feature".into(),
        branch: "feature".into(),
        status: ChangeRequestStatus::Open,
        body: None,
        correlation_keys: vec![],
        association_keys: vec![],
        provider_name: String::new(),
        provider_display_name: String::new(),
    });

    // Follower only has checkouts (no service data — as expected in follower mode)
    let peer_host = HostName::new("follower");
    let peer_data = ProviderData {
        checkouts: IndexMap::from([(HostPath::new(peer_host.clone(), "/opt/repo"), make_checkout("feature"))]),
        ..Default::default()
    };

    let merged = merge_provider_data(&local, &local_host, &[(peer_host, &peer_data)]);

    // Both checkouts present
    assert_eq!(merged.checkouts.len(), 2);
    // Leader's PR is preserved
    assert_eq!(merged.change_requests.len(), 1);
    assert!(merged.change_requests.contains_key("PR-42"));
}

// ---------------------------------------------------------------------------
// Test 10: Delta message returns NeedsResync (Phase 1 behavior)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delta_message_returns_needs_resync() {
    use flotilla_protocol::{
        delta::{Branch, BranchStatus, EntryOp},
        Change,
    };

    let mut mgr = PeerManager::new(HostName::new("leader"));

    let msg = PeerDataMessage {
        origin_host: HostName::new("follower"),
        repo_identity: test_repo(),
        repo_path: PathBuf::from("/home/dev/repo"),
        clock: VectorClock::default(),
        kind: PeerDataKind::Delta {
            changes: vec![Change::Branch { key: "feat-x".into(), op: EntryOp::Added(Branch { status: BranchStatus::Remote }) }],
            seq: 2,
            prev_seq: 1,
        },
    };

    let result =
        handle_test_peer_data(&mut mgr, msg, || Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>)
            .await;
    assert_eq!(
        result,
        HandleResult::NeedsResync { from: HostName::new("follower"), repo: test_repo() },
        "Phase 1: deltas should trigger NeedsResync"
    );
}

// ---------------------------------------------------------------------------
// Test 11: Follower mode skips external providers (cross-crate verification)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn follower_mode_has_only_local_providers() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().to_path_buf();
    std::fs::create_dir_all(repo.join(".git")).expect("create .git dir");

    let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
    let daemon = InProcessDaemon::new(vec![repo], config, fake_discovery(true), HostName::local()).await;

    assert!(daemon.is_follower(), "daemon should be in follower mode");

    let repos = daemon.list_repos().await.expect("list_repos");
    assert_eq!(repos.len(), 1);

    let provider_names = &repos[0].provider_names;

    // Local providers should be present
    assert!(provider_names.contains_key("vcs"), "follower should have VCS provider");

    // External providers should be absent
    assert!(!provider_names.contains_key("change_request"), "follower should NOT have change_request");
    assert!(!provider_names.contains_key("issue_tracker"), "follower should NOT have issue_tracker");
    assert!(!provider_names.contains_key("cloud_agent"), "follower should NOT have cloud_agent");
    assert!(!provider_names.contains_key("ai_utility"), "follower should NOT have ai_utility");
}

// ---------------------------------------------------------------------------
// Test 12: Snapshot update overwrites previous peer data
// ---------------------------------------------------------------------------

#[tokio::test]
async fn peer_snapshot_update_overwrites_previous() {
    let mut mgr = PeerManager::new(HostName::new("leader"));

    // First snapshot from follower with branch "old-branch"
    let mut data1 = ProviderData::default();
    data1.checkouts.insert(HostPath::new(HostName::new("follower"), "/repo"), make_checkout("old-branch"));
    handle_test_peer_data(&mut mgr, snapshot_msg("follower", 1, data1), || {
        Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
    })
    .await;

    // Second snapshot with branch "new-branch"
    let mut data2 = ProviderData::default();
    data2.checkouts.insert(HostPath::new(HostName::new("follower"), "/repo"), make_checkout("new-branch"));
    let result = handle_test_peer_data(&mut mgr, snapshot_msg("follower", 2, data2), || {
        Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
    })
    .await;
    assert_eq!(result, HandleResult::Updated(test_repo()));

    // Verify the data was updated
    let peer_data = mgr.get_peer_data();
    let state = &peer_data[&HostName::new("follower")][&test_repo()];
    assert_eq!(state.seq, 2);
    assert_eq!(
        state.provider_data.checkouts[&HostPath::new(HostName::new("follower"), "/repo")].branch,
        "new-branch",
        "second snapshot should overwrite the first"
    );
}
