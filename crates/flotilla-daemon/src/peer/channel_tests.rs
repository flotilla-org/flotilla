use std::path::PathBuf;

use flotilla_protocol::{
    Command, CommandAction, CommandPeerEvent, CommandResult, GoodbyeReason, HostName, PeerDataKind, PeerDataMessage, PeerWireMessage,
    ProviderData, RepoIdentity, RepoSelector, RoutedPeerMessage, StepStatus, VectorClock,
};

use crate::peer::{test_support::TestNetwork, HandleResult, PeerManager};

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

#[tokio::test]
async fn routed_command_request_reaches_target_through_relay() {
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    let c = net.add_peer("host-c");
    net.connect(a, b);
    net.connect(b, c);
    net.start().await;

    let repo = test_repo();
    net.inject_local_data(c, snapshot_msg("host-c", &repo, 1)).await;
    net.inject_local_data(a, snapshot_msg("host-a", &repo, 1)).await;
    net.settle().await;

    let request = RoutedPeerMessage::CommandRequest {
        request_id: 42,
        requester_host: HostName::new("host-a"),
        target_host: HostName::new("host-c"),
        remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
        command: Box::new(Command {
            host: Some(HostName::new("host-c")),
            context_repo: None,
            action: CommandAction::Refresh { repo: Some(RepoSelector::Query("owner/repo".into())) },
        }),
    };

    net.manager(a).send_to(&HostName::new("host-c"), PeerWireMessage::Routed(request)).await.expect("send command request");

    let b_results = net.process_peer_with_results(b).await;
    assert!(b_results.iter().all(|result| matches!(result, HandleResult::Ignored)));

    let c_results = net.process_peer_with_results(c).await;
    assert!(matches!(
        c_results.as_slice(),
        [HandleResult::CommandRequested { request_id: 42, requester_host, reply_via, command }]
            if requester_host == &HostName::new("host-a")
                && reply_via == &HostName::new("host-b")
                && *command
                    == Command {
                        host: Some(HostName::new("host-c")),
                        context_repo: None,
                        action: CommandAction::Refresh { repo: Some(RepoSelector::Query("owner/repo".into())) },
                    }
    ));
}

#[tokio::test]
async fn routed_command_event_and_response_reach_requester_through_relay() {
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    let c = net.add_peer("host-c");
    net.connect(a, b);
    net.connect(b, c);
    net.start().await;

    let repo = test_repo();
    net.inject_local_data(c, snapshot_msg("host-c", &repo, 1)).await;
    net.settle().await;

    net.manager(a)
        .send_to(
            &HostName::new("host-c"),
            PeerWireMessage::Routed(RoutedPeerMessage::CommandRequest {
                request_id: 77,
                requester_host: HostName::new("host-a"),
                target_host: HostName::new("host-c"),
                remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                command: Box::new(Command {
                    host: Some(HostName::new("host-c")),
                    context_repo: None,
                    action: CommandAction::Refresh { repo: None },
                }),
            }),
        )
        .await
        .expect("send command request");
    net.process_peer_with_results(b).await;
    net.process_peer_with_results(c).await;

    net.manager(c)
        .send_to(
            &HostName::new("host-b"),
            PeerWireMessage::Routed(RoutedPeerMessage::CommandEvent {
                request_id: 77,
                requester_host: HostName::new("host-a"),
                responder_host: HostName::new("host-c"),
                remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                event: Box::new(CommandPeerEvent::StepUpdate {
                    repo: PathBuf::from("/repo"),
                    step_index: 0,
                    step_count: 2,
                    description: "Refreshing".into(),
                    status: StepStatus::Started,
                }),
            }),
        )
        .await
        .expect("send command event");
    net.process_peer_with_results(b).await;
    let a_event_results = net.process_peer_with_results(a).await;
    assert!(matches!(
        a_event_results.as_slice(),
        [HandleResult::CommandEventReceived { request_id: 77, responder_host, event }]
            if responder_host == &HostName::new("host-c")
                && *event
                    == CommandPeerEvent::StepUpdate {
                        repo: PathBuf::from("/repo"),
                        step_index: 0,
                        step_count: 2,
                        description: "Refreshing".into(),
                        status: StepStatus::Started,
                    }
    ));

    net.manager(c)
        .send_to(
            &HostName::new("host-b"),
            PeerWireMessage::Routed(RoutedPeerMessage::CommandResponse {
                request_id: 77,
                requester_host: HostName::new("host-a"),
                responder_host: HostName::new("host-c"),
                remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                result: Box::new(CommandResult::Refreshed { repos: vec![PathBuf::from("/repo")] }),
            }),
        )
        .await
        .expect("send command response");
    net.process_peer_with_results(b).await;
    let a_response_results = net.process_peer_with_results(a).await;
    assert!(matches!(
        a_response_results.as_slice(),
        [HandleResult::CommandResponseReceived { request_id: 77, responder_host, result }]
            if responder_host == &HostName::new("host-c")
                && *result == CommandResult::Refreshed { repos: vec![PathBuf::from("/repo")] }
    ));
}

#[tokio::test]
async fn routed_command_returns_clear_error_for_unknown_target() {
    let mut net = TestNetwork::new();
    let a = net.add_peer("host-a");
    let b = net.add_peer("host-b");
    net.connect(a, b);
    net.start().await;

    let err = net
        .manager(a)
        .send_to(
            &HostName::new("host-z"),
            PeerWireMessage::Routed(RoutedPeerMessage::CommandRequest {
                request_id: 1,
                requester_host: HostName::new("host-a"),
                target_host: HostName::new("host-z"),
                remaining_hops: PeerManager::DEFAULT_ROUTED_HOPS,
                command: Box::new(Command {
                    host: Some(HostName::new("host-z")),
                    context_repo: None,
                    action: CommandAction::Refresh { repo: None },
                }),
            }),
        )
        .await
        .expect_err("unknown target should fail");

    assert!(err.contains("unknown peer: host-z"));
}
