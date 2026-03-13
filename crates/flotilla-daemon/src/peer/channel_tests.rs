use std::path::PathBuf;

use flotilla_protocol::{GoodbyeReason, HostName, PeerDataKind, PeerDataMessage, ProviderData, RepoIdentity, VectorClock};

use crate::peer::test_support::TestNetwork;

fn test_repo() -> RepoIdentity {
    RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() }
}

/// Create a snapshot message with the origin host's clock pre-ticked to `seq`.
fn snapshot_msg(origin: &str, repo: &RepoIdentity, seq: u64) -> PeerDataMessage {
    let mut clock = VectorClock::default();
    for _ in 0..seq {
        clock.tick(&HostName::new(origin));
    }
    PeerDataMessage {
        origin_host: HostName::new(origin),
        repo_identity: repo.clone(),
        repo_path: PathBuf::from("/repo"),
        clock,
        kind: PeerDataKind::Snapshot { data: Box::new(ProviderData::default()), seq },
    }
}

/// Helper: check if a peer's manager has stored data from a given origin for a repo.
fn has_peer_data(net: &TestNetwork, peer_idx: usize, origin: &str, repo: &RepoIdentity) -> bool {
    net.manager(peer_idx).get_peer_data().get(&HostName::new(origin)).and_then(|repos| repos.get(repo)).is_some()
}

// ---------------------------------------------------------------------------
// Level A: 2-peer tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_peer_snapshot_exchange() {
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    net.connect(a, b);
    net.start().await;

    let repo = test_repo();
    let msg = snapshot_msg("host-a", &repo, 1);
    net.inject_local_data(a, msg).await;
    net.settle().await;

    assert!(has_peer_data(&net, b, "host-a", &repo), "host-b should have host-a's data");
}

#[tokio::test]
async fn bidirectional_snapshot_exchange() {
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    net.connect(a, b);
    net.start().await;

    let repo = test_repo();

    let msg_a = snapshot_msg("host-a", &repo, 1);
    net.inject_local_data(a, msg_a).await;

    let msg_b = snapshot_msg("host-b", &repo, 1);
    net.inject_local_data(b, msg_b).await;

    net.settle().await;

    assert!(has_peer_data(&net, b, "host-a", &repo), "host-b should have host-a's data");
    assert!(has_peer_data(&net, a, "host-b", &repo), "host-a should have host-b's data");
}

#[tokio::test]
async fn vector_clock_dedup_drops_duplicate() {
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    net.connect(a, b);
    net.start().await;

    let repo = test_repo();

    // Send seq 1
    let msg1 = snapshot_msg("host-a", &repo, 1);
    net.inject_local_data(a, msg1).await;
    net.settle().await;

    // Send seq 1 again (duplicate — same clock, should be deduped)
    let msg1_dup = snapshot_msg("host-a", &repo, 1);
    net.inject_local_data(a, msg1_dup).await;
    net.settle().await;

    // Send seq 2 (new clock value, should be accepted)
    let msg2 = snapshot_msg("host-a", &repo, 2);
    net.inject_local_data(a, msg2).await;
    net.settle().await;

    let peer_data = net.manager(b).get_peer_data();
    let state = peer_data.get(&HostName::new("host-a")).and_then(|repos| repos.get(&repo)).expect("should have data");
    assert_eq!(state.seq, 2, "seq should be 2 — duplicate seq 1 was dropped, seq 2 was accepted");
}

#[tokio::test]
async fn goodbye_flow_through_channel() {
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    net.connect(a, b);
    net.start().await;

    // Get A's sender for B and retire it (sends Goodbye)
    let sender = net.manager(a).resolve_sender(&HostName::new("host-b")).expect("sender");
    sender.retire(GoodbyeReason::Superseded).await.expect("retire should succeed");

    // Process B to receive the Goodbye
    let count = net.process_peer(b).await;
    assert!(count > 0, "host-b should have received the Goodbye message");
}

// ---------------------------------------------------------------------------
// Level B: 3-peer tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn three_peer_relay_propagation() {
    // Linear topology: A — B — C
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    let c = net.add_peer("host-c");
    net.connect(a, b);
    net.connect(b, c);
    net.start().await;

    let repo = test_repo();
    let msg = snapshot_msg("host-a", &repo, 1);
    net.inject_local_data(a, msg).await;
    net.settle().await;

    assert!(has_peer_data(&net, b, "host-a", &repo), "host-b should have host-a's data");
    assert!(has_peer_data(&net, c, "host-a", &repo), "host-c should have host-a's data via relay through host-b");
}

#[tokio::test]
async fn three_peer_mesh_dedup() {
    // Full mesh: A — B, B — C, A — C
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    let c = net.add_peer("host-c");
    net.connect(a, b);
    net.connect(b, c);
    net.connect(a, c);
    net.start().await;

    let repo = test_repo();
    let msg = snapshot_msg("host-a", &repo, 1);
    net.inject_local_data(a, msg).await;
    net.settle().await;

    assert!(has_peer_data(&net, b, "host-a", &repo), "host-b should have host-a's data");
    assert!(has_peer_data(&net, c, "host-a", &repo), "host-c should have host-a's data");
}

#[tokio::test]
async fn reverse_direction_snapshot_in_chain() {
    // Linear topology: A — B — C, but C sends the snapshot
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    let c = net.add_peer("host-c");
    net.connect(a, b);
    net.connect(b, c);
    net.start().await;

    let repo = test_repo();
    let msg = snapshot_msg("host-c", &repo, 1);
    net.inject_local_data(c, msg).await;
    net.settle().await;

    assert!(has_peer_data(&net, b, "host-c", &repo), "host-b should have host-c's data");
    assert!(has_peer_data(&net, a, "host-c", &repo), "host-a should have host-c's data via relay through host-b");
}
