use std::sync::{Arc, Mutex};

use super::*;
use crate::peer::{
    test_support::{ensure_test_connection_generation, handle_test_peer_data, MockPeerSender, MockTransport},
    PeerConnectionStatus,
};

fn test_repo() -> RepoIdentity {
    RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() }
}

fn snapshot_msg(origin: &str, seq: u64) -> PeerDataMessage {
    let mut clock = VectorClock::default();
    for _ in 0..seq {
        clock.tick(&HostName::new(origin));
    }
    PeerDataMessage {
        origin_host: HostName::new(origin),
        repo_identity: test_repo(),
        repo_path: PathBuf::from("/home/dev/repo"),
        clock,
        kind: PeerDataKind::Snapshot { data: Box::new(ProviderData::default()), seq },
    }
}

fn sample_host_summary_for(name: &str) -> flotilla_protocol::HostSummary {
    flotilla_protocol::HostSummary {
        host_name: HostName::new(name),
        system: flotilla_protocol::SystemInfo {
            home_dir: Some(PathBuf::from("/home/dev")),
            os: Some("linux".into()),
            arch: Some("x86_64".into()),
            cpu_count: Some(8),
            memory_total_mb: Some(16384),
            environment: flotilla_protocol::HostEnvironment::Unknown,
        },
        inventory: flotilla_protocol::ToolInventory::default(),
        providers: vec![],
        environments: vec![],
    }
}

fn accepted_generation(result: ActivationResult) -> u64 {
    match result {
        ActivationResult::Accepted { generation, .. } => generation,
        ActivationResult::Rejected { reason } => {
            panic!("expected accepted connection, got rejection: {:?}", reason)
        }
    }
}

#[tokio::test]
async fn handle_snapshot_stores_data() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let msg = snapshot_msg("remote", 1);

    let result = handle_test_peer_data(&mut mgr, msg, MockPeerSender::discard).await;
    assert_eq!(result, HandleResult::Updated(test_repo()));

    let peer_data = mgr.get_peer_data();
    let remote_host = HostName::new("remote");
    assert!(peer_data.contains_key(&remote_host));
    let repo_state = &peer_data[&remote_host][&test_repo()];
    assert_eq!(repo_state.seq, 1);
    assert_eq!(repo_state.repo_path, PathBuf::from("/home/dev/repo"));
}

#[tokio::test]
async fn handle_snapshot_updates_existing_data() {
    let mut mgr = PeerManager::new(HostName::new("local"));

    // First snapshot
    let msg1 = snapshot_msg("remote", 1);
    handle_test_peer_data(&mut mgr, msg1, MockPeerSender::discard).await;

    // Second snapshot with higher seq
    let msg2 = snapshot_msg("remote", 5);
    let result = handle_test_peer_data(&mut mgr, msg2, MockPeerSender::discard).await;
    assert_eq!(result, HandleResult::Updated(test_repo()));

    let peer_data = mgr.get_peer_data();
    let repo_state = &peer_data[&HostName::new("remote")][&test_repo()];
    assert_eq!(repo_state.seq, 5);
}

#[tokio::test]
async fn legacy_direct_request_resync_is_ignored() {
    let mut mgr = PeerManager::new(HostName::new("local"));

    let msg = PeerDataMessage {
        origin_host: HostName::new("remote"),
        repo_identity: test_repo(),
        repo_path: PathBuf::from("/home/dev/repo"),
        clock: VectorClock::default(),
        kind: PeerDataKind::RequestResync { since_seq: 3 },
    };

    let result = handle_test_peer_data(&mut mgr, msg, MockPeerSender::discard).await;
    assert_eq!(result, HandleResult::Ignored);
}

#[tokio::test]
async fn handle_delta_returns_needs_resync() {
    use flotilla_protocol::{
        delta::{Branch, BranchStatus, EntryOp},
        Change,
    };

    let mut mgr = PeerManager::new(HostName::new("local"));

    let msg = PeerDataMessage {
        origin_host: HostName::new("remote"),
        repo_identity: test_repo(),
        repo_path: PathBuf::from("/home/dev/repo"),
        clock: VectorClock::default(),
        kind: PeerDataKind::Delta {
            changes: vec![Change::Branch { key: "feat-x".into(), op: EntryOp::Added(Branch { status: BranchStatus::Remote }) }],
            seq: 2,
            prev_seq: 1,
        },
    };

    let result = handle_test_peer_data(&mut mgr, msg, MockPeerSender::discard).await;
    assert_eq!(result, HandleResult::NeedsResync { from: HostName::new("remote"), repo: test_repo() });
}

#[tokio::test]
async fn handle_ignores_messages_from_self() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let msg = snapshot_msg("local", 1);

    let result = handle_test_peer_data(&mut mgr, msg, MockPeerSender::discard).await;
    assert_eq!(result, HandleResult::Ignored);
    assert!(mgr.get_peer_data().is_empty());
}

#[tokio::test]
async fn relay_sends_to_all_except_origin() {
    let mut mgr = PeerManager::new(HostName::new("local"));

    let (transport_a, sent_a) = MockTransport::with_sender();
    let (transport_b, sent_b) = MockTransport::with_sender();
    let (transport_c, sent_c) = MockTransport::with_sender();
    let sender_a = transport_a.sender().expect("sender");
    let sender_b = transport_b.sender().expect("sender");
    let sender_c = transport_c.sender().expect("sender");

    mgr.add_peer(HostName::new("peer-a"), Box::new(transport_a));
    mgr.add_peer(HostName::new("peer-b"), Box::new(transport_b));
    mgr.add_peer(HostName::new("peer-c"), Box::new(transport_c));
    mgr.register_sender(HostName::new("peer-a"), sender_a);
    mgr.register_sender(HostName::new("peer-b"), sender_b);
    mgr.register_sender(HostName::new("peer-c"), sender_c);

    let msg = snapshot_msg("peer-a", 1);
    mgr.relay(&HostName::new("peer-a"), &msg).await;

    // peer-a is origin, so it should NOT receive the relay
    assert!(sent_a.lock().expect("lock").is_empty());
    // peer-b and peer-c should each get exactly one message
    assert_eq!(sent_b.lock().expect("lock").len(), 1);
    assert_eq!(sent_c.lock().expect("lock").len(), 1);
}

#[tokio::test]
async fn relay_does_not_send_to_self() {
    let mut mgr = PeerManager::new(HostName::new("local"));

    let (transport, sent) = MockTransport::with_sender();
    let sender = transport.sender().expect("sender");
    mgr.add_peer(HostName::new("local"), Box::new(transport));
    mgr.register_sender(HostName::new("local"), sender);

    let msg = snapshot_msg("remote", 1);
    mgr.relay(&HostName::new("remote"), &msg).await;

    // Should not send to self even if registered as a peer
    assert!(sent.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn relay_skips_peers_already_in_clock() {
    // Star topology: leader has peers [F1, F2].
    // F1 sends a message that leader relays to F2 (stamping leader into clock).
    // If F2 then tried to relay, it should NOT send back to leader
    // because leader is already in the clock.
    let mut mgr = PeerManager::new(HostName::new("F2"));

    let (transport_leader, sent_leader) = MockTransport::with_sender();
    let sender_leader = transport_leader.sender().expect("sender");
    mgr.add_peer(HostName::new("leader"), Box::new(transport_leader));
    mgr.register_sender(HostName::new("leader"), sender_leader);

    // Simulate a message that was relayed through leader:
    // origin=F1, clock={F1:1, leader:1}
    let mut clock = VectorClock::default();
    clock.tick(&HostName::new("F1"));
    clock.tick(&HostName::new("leader"));
    let msg = PeerDataMessage {
        origin_host: HostName::new("F1"),
        repo_identity: test_repo(),
        repo_path: PathBuf::from("/home/dev/repo"),
        clock,
        kind: PeerDataKind::Snapshot { data: Box::new(ProviderData::default()), seq: 1 },
    };

    mgr.relay(&HostName::new("F1"), &msg).await;

    // Leader is already in the clock, so relay should skip it
    assert!(sent_leader.lock().expect("lock").is_empty(), "should not relay back to a peer already in the clock");
}

#[tokio::test]
async fn get_peer_data_returns_stored_data() {
    let mut mgr = PeerManager::new(HostName::new("local"));

    // Initially empty
    assert!(mgr.get_peer_data().is_empty());

    // After storing data from two hosts
    handle_test_peer_data(&mut mgr, snapshot_msg("desktop", 1), MockPeerSender::discard).await;
    handle_test_peer_data(&mut mgr, snapshot_msg("server", 2), MockPeerSender::discard).await;

    let data = mgr.get_peer_data();
    assert_eq!(data.len(), 2);
    assert!(data.contains_key(&HostName::new("desktop")));
    assert!(data.contains_key(&HostName::new("server")));
}

#[tokio::test]
async fn host_summary_handle_inbound_stores_for_connection_peer() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let connection_peer = HostName::new("remote");
    let generation = ensure_test_connection_generation(&mut mgr, &connection_peer, MockPeerSender::discard);

    let result = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::HostSummary(sample_host_summary_for("spoofed-name")),
            connection_generation: generation,
            connection_peer: connection_peer.clone(),
        })
        .await;

    assert_eq!(result, HandleResult::Ignored);
    let stored = mgr.get_peer_host_summaries().get(&connection_peer).expect("stored host summary");
    assert_eq!(stored.host_name, connection_peer);
}

#[test]
fn remove_peer_data_clears_host_summary() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    mgr.store_host_summary(sample_host_summary_for("remote"));

    mgr.remove_peer_data(&HostName::new("remote"));

    assert!(mgr.get_peer_host_summaries().is_empty());
}

#[test]
fn clear_peer_data_for_restart_clears_host_summary() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    mgr.store_host_summary(sample_host_summary_for("remote"));

    mgr.clear_peer_data_for_restart(&HostName::new("remote"));

    assert!(mgr.get_peer_host_summaries().is_empty());
}

#[tokio::test]
async fn connect_all_connects_peers() {
    let mut mgr = PeerManager::new(HostName::new("local"));

    let transport = MockTransport::new();
    // Start disconnected
    let mut transport = transport;
    transport.status = PeerConnectionStatus::Disconnected;

    mgr.add_peer(HostName::new("peer"), Box::new(transport));
    mgr.connect_all().await;

    // After connect_all, the mock transport's connect() sets status to Connected
    let peer_transport = mgr.peers.get(&HostName::new("peer")).expect("peer exists");
    assert_eq!(peer_transport.status(), PeerConnectionStatus::Connected);
}

#[tokio::test]
async fn disconnect_all_disconnects_peers() {
    let mut mgr = PeerManager::new(HostName::new("local"));

    let transport = MockTransport::new();
    mgr.add_peer(HostName::new("peer"), Box::new(transport));
    mgr.disconnect_all().await;

    let peer_transport = mgr.peers.get(&HostName::new("peer")).expect("peer exists");
    assert_eq!(peer_transport.status(), PeerConnectionStatus::Disconnected);
}

#[test]
fn synthetic_repo_path_format() {
    let host = HostName::new("desktop");
    let repo_path = std::path::Path::new("/home/dev/repo");
    let path = super::synthetic_repo_path(&host, repo_path);
    assert_eq!(path, PathBuf::from("<remote>/desktop/home/dev/repo"));
}

#[test]
fn synthetic_repo_path_different_hosts_produce_different_paths() {
    let repo_path = std::path::Path::new("/home/dev/repo");
    let path_a = super::synthetic_repo_path(&HostName::new("host-a"), repo_path);
    let path_b = super::synthetic_repo_path(&HostName::new("host-b"), repo_path);
    assert_ne!(path_a, path_b);
}

#[test]
fn register_and_query_remote_repos() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let repo = test_repo();
    let synthetic = PathBuf::from("<remote>/desktop/home/dev/repo");

    assert!(!mgr.is_remote_repo(&repo));
    assert!(mgr.known_remote_repos().is_empty());

    mgr.register_remote_repo(repo.clone(), synthetic.clone());

    assert!(mgr.is_remote_repo(&repo));
    assert_eq!(mgr.known_remote_repos().len(), 1);
    assert_eq!(mgr.known_remote_repos()[&repo], synthetic);
}

#[tokio::test]
async fn send_to_reaches_registered_sender() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&sent) });
    mgr.register_sender(HostName::new("peer"), sender);

    mgr.send_to(&HostName::new("peer"), PeerWireMessage::Data(snapshot_msg("local", 1))).await.expect("send succeeds");

    assert_eq!(sent.lock().expect("lock").len(), 1);
}

#[tokio::test]
async fn activate_connection_rejects_same_direction_duplicate_sender() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let first_sent = Arc::new(Mutex::new(Vec::new()));
    let second_sent = Arc::new(Mutex::new(Vec::new()));
    let first_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&first_sent) });
    let second_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&second_sent) });

    let gen1 = accepted_generation(mgr.activate_connection(HostName::new("peer"), first_sender, ConnectionMeta {
        direction: ConnectionDirection::Inbound,
        config_label: None,
        expected_peer: None,
        config_backed: false,
    }));
    let second = mgr.activate_connection(HostName::new("peer"), second_sender, ConnectionMeta {
        direction: ConnectionDirection::Inbound,
        config_label: None,
        expected_peer: None,
        config_backed: false,
    });

    assert_eq!(gen1, 1);
    assert_eq!(second, ActivationResult::Rejected { reason: GoodbyeReason::Superseded });
    mgr.send_to(&HostName::new("peer"), PeerWireMessage::Data(snapshot_msg("local", 1))).await.expect("send succeeds");

    assert_eq!(first_sent.lock().expect("lock").len(), 1);
    assert!(second_sent.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn configured_outbound_beats_unsolicited_inbound() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let outbound_sent = Arc::new(Mutex::new(Vec::new()));
    let inbound_sent = Arc::new(Mutex::new(Vec::new()));
    let outbound_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&outbound_sent) });
    let inbound_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&inbound_sent) });

    let _ = accepted_generation(mgr.activate_connection(HostName::new("peer"), outbound_sender, ConnectionMeta {
        direction: ConnectionDirection::Outbound,
        config_label: Some(ConfigLabel("peer".into())),
        expected_peer: Some(HostName::new("peer")),
        config_backed: true,
    }));
    let duplicate = mgr.activate_connection(HostName::new("peer"), inbound_sender, ConnectionMeta {
        direction: ConnectionDirection::Inbound,
        config_label: None,
        expected_peer: None,
        config_backed: false,
    });
    assert_eq!(duplicate, ActivationResult::Rejected { reason: GoodbyeReason::Superseded });

    mgr.send_to(&HostName::new("peer"), PeerWireMessage::Data(snapshot_msg("local", 1))).await.expect("send succeeds");

    assert_eq!(outbound_sent.lock().expect("lock").len(), 1);
    assert!(inbound_sent.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn displaced_connection_can_be_retired_after_replacement() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let first_sent = Arc::new(Mutex::new(Vec::new()));
    let second_sent = Arc::new(Mutex::new(Vec::new()));
    let first_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&first_sent) });
    let second_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&second_sent) });

    let first_generation = accepted_generation(mgr.activate_connection(HostName::new("peer"), first_sender, ConnectionMeta {
        direction: ConnectionDirection::Inbound,
        config_label: None,
        expected_peer: None,
        config_backed: false,
    }));
    let replacement = mgr.activate_connection(HostName::new("peer"), second_sender, ConnectionMeta {
        direction: ConnectionDirection::Outbound,
        config_label: Some(ConfigLabel("peer".into())),
        expected_peer: Some(HostName::new("peer")),
        config_backed: true,
    });

    let displaced_generation = match replacement {
        ActivationResult::Accepted { generation, displaced: Some(displaced) } => {
            assert_eq!(generation, 2);
            displaced
        }
        other => panic!("expected accepted replacement, got {other:?}"),
    };
    assert_eq!(displaced_generation, first_generation);

    let displaced = mgr.take_displaced_sender(&HostName::new("peer"), displaced_generation).expect("displaced sender should be tracked");
    displaced.retire(GoodbyeReason::Superseded).await.expect("retire displaced sender");

    let sent = first_sent.lock().expect("lock");
    assert_eq!(sent.len(), 1);
    match &sent[0] {
        PeerWireMessage::Goodbye { reason: GoodbyeReason::Superseded } => {}
        other => panic!("expected superseded goodbye, got {other:?}"),
    }
    assert!(second_sent.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn stale_generation_inbound_message_is_dropped() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let generation = accepted_generation(mgr.activate_connection(HostName::new("peer"), MockPeerSender::discard(), ConnectionMeta {
        direction: ConnectionDirection::Inbound,
        config_label: None,
        expected_peer: None,
        config_backed: false,
    }));
    assert_eq!(generation, 1);
    let replacement_generation =
        accepted_generation(mgr.activate_connection(HostName::new("peer"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Outbound,
            config_label: None,
            expected_peer: Some(HostName::new("peer")),
            config_backed: true,
        }));
    assert_eq!(replacement_generation, 2);

    let result = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::Data(snapshot_msg("peer", 1)),
            connection_generation: generation,
            connection_peer: HostName::new("peer"),
        })
        .await;

    assert_eq!(result, HandleResult::Ignored);
    assert!(mgr.get_peer_data().is_empty());
}

#[tokio::test]
async fn send_to_uses_route_primary_when_no_direct_sender() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let via_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&sent) });
    mgr.register_sender(HostName::new("relay"), via_sender);
    mgr.generations.insert(HostName::new("relay"), 1);
    mgr.routes.insert(HostName::new("target"), RouteState {
        primary: RouteHop { next_hop: HostName::new("relay"), next_hop_generation: 1, learned_epoch: 1 },
        fallbacks: Vec::new(),
        candidates: Vec::new(),
    });

    mgr.send_to(
        &HostName::new("target"),
        PeerWireMessage::Routed(RoutedPeerMessage::RequestResync {
            request_id: 1,
            requester_host: HostName::new("local"),
            target_host: HostName::new("target"),
            remaining_hops: 3,
            repo_identity: test_repo(),
            since_seq: 0,
        }),
    )
    .await
    .expect("send succeeds");

    assert_eq!(sent.lock().expect("lock").len(), 1);
}

#[tokio::test]
async fn send_to_returns_error_when_no_direct_sender_or_route() {
    let mgr = PeerManager::new(HostName::new("local"));
    let err = mgr
        .send_to(&HostName::new("missing"), PeerWireMessage::Data(snapshot_msg("local", 1)))
        .await
        .expect_err("missing route should error");
    assert!(err.contains("unknown peer"));
}

#[tokio::test]
async fn configured_peer_names_include_all_configured_peers() {
    let mut mgr = PeerManager::new(HostName::new("m"));
    mgr.add_peer(HostName::new("z"), Box::new(MockTransport::new()));
    mgr.add_peer(HostName::new("a"), Box::new(MockTransport::new()));

    let mut configured = mgr.configured_peer_names();
    configured.sort();

    assert_eq!(configured, vec![HostName::new("a"), HostName::new("z")]);
}

#[tokio::test]
async fn reconnect_peer_allows_configured_peer_regardless_of_host_order() {
    let mut mgr = PeerManager::new(HostName::new("z"));
    mgr.add_peer(HostName::new("a"), Box::new(MockTransport::new()));

    let (generation, _rx) = mgr.reconnect_peer(&HostName::new("a")).await.expect("reconnect should succeed for configured peer");

    assert_eq!(generation, 0);
}

#[tokio::test]
async fn reconnect_peer_retires_displaced_connection() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let displaced_sent = Arc::new(Mutex::new(Vec::new()));
    let displaced_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&displaced_sent) });

    let _ = accepted_generation(mgr.activate_connection(HostName::new("peer"), displaced_sender, ConnectionMeta {
        direction: ConnectionDirection::Inbound,
        config_label: None,
        expected_peer: None,
        config_backed: false,
    }));

    let (transport, _new_sent) = MockTransport::with_sender();
    mgr.add_peer(HostName::new("peer"), Box::new(transport));

    let _ = mgr.reconnect_peer(&HostName::new("peer")).await.expect("reconnect should succeed");

    let sent = displaced_sent.lock().expect("lock");
    assert_eq!(sent.len(), 1);
    match &sent[0] {
        PeerWireMessage::Goodbye { reason: GoodbyeReason::Superseded } => {}
        other => panic!("expected superseded goodbye, got {other:?}"),
    }
}

#[tokio::test]
async fn late_resync_snapshot_is_dropped_without_pending_request() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let generation = accepted_generation(mgr.activate_connection(HostName::new("relay"), MockPeerSender::discard(), ConnectionMeta {
        direction: ConnectionDirection::Inbound,
        config_label: None,
        expected_peer: None,
        config_backed: false,
    }));

    let result = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::Routed(RoutedPeerMessage::ResyncSnapshot {
                request_id: 1,
                requester_host: HostName::new("local"),
                responder_host: HostName::new("target"),
                remaining_hops: 3,
                repo_identity: test_repo(),
                repo_path: PathBuf::from("/home/dev/repo"),
                clock: VectorClock::default(),
                seq: 1,
                data: Box::new(ProviderData::default()),
            }),
            connection_generation: generation,
            connection_peer: HostName::new("relay"),
        })
        .await;

    assert_eq!(result, HandleResult::Ignored);
    assert!(mgr.get_peer_data().is_empty());
}

#[tokio::test]
async fn goodbye_superseded_suppresses_reconnect_for_peer() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let generation = accepted_generation(mgr.activate_connection(HostName::new("peer"), MockPeerSender::discard(), ConnectionMeta {
        direction: ConnectionDirection::Outbound,
        config_label: Some(ConfigLabel("peer".into())),
        expected_peer: Some(HostName::new("peer")),
        config_backed: true,
    }));
    mgr.add_peer(HostName::new("peer"), Box::new(MockTransport::new()));

    let result = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::Goodbye { reason: GoodbyeReason::Superseded },
            connection_generation: generation,
            connection_peer: HostName::new("peer"),
        })
        .await;

    assert_eq!(result, HandleResult::ReconnectSuppressed { peer: HostName::new("peer") });
    let err = mgr.reconnect_peer(&HostName::new("peer")).await.expect_err("reconnect should be suppressed");
    assert!(err.contains("suppressed"));
}

#[tokio::test]
async fn routed_request_resync_is_dropped_when_hop_budget_exhausted() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&sent) });
    let generation = accepted_generation(mgr.activate_connection(HostName::new("relay"), sender, ConnectionMeta {
        direction: ConnectionDirection::Inbound,
        config_label: None,
        expected_peer: None,
        config_backed: false,
    }));

    let result = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::Routed(RoutedPeerMessage::RequestResync {
                request_id: 1,
                requester_host: HostName::new("requester"),
                target_host: HostName::new("target"),
                remaining_hops: 0,
                repo_identity: test_repo(),
                since_seq: 0,
            }),
            connection_generation: generation,
            connection_peer: HostName::new("relay"),
        })
        .await;

    assert_eq!(result, HandleResult::Ignored);
    assert!(sent.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn routed_request_resync_to_local_preserves_request_id() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let generation = accepted_generation(mgr.activate_connection(HostName::new("relay"), MockPeerSender::discard(), ConnectionMeta {
        direction: ConnectionDirection::Inbound,
        config_label: None,
        expected_peer: None,
        config_backed: false,
    }));

    let result = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::Routed(RoutedPeerMessage::RequestResync {
                request_id: 41,
                requester_host: HostName::new("requester"),
                target_host: HostName::new("local"),
                remaining_hops: 3,
                repo_identity: test_repo(),
                since_seq: 7,
            }),
            connection_generation: generation,
            connection_peer: HostName::new("relay"),
        })
        .await;

    assert_eq!(result, HandleResult::ResyncRequested {
        request_id: 41,
        requester_host: HostName::new("requester"),
        reply_via: HostName::new("relay"),
        repo: test_repo(),
        since_seq: 7,
    });
}

#[tokio::test]
async fn disconnect_peer_keeps_snapshot_stale_when_fallback_exists() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let direct_generation =
        accepted_generation(mgr.activate_connection(HostName::new("target"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Outbound,
            config_label: None,
            expected_peer: Some(HostName::new("target")),
            config_backed: true,
        }));
    let relay_generation =
        accepted_generation(mgr.activate_connection(HostName::new("relay"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Outbound,
            config_label: None,
            expected_peer: Some(HostName::new("relay")),
            config_backed: true,
        }));
    let _ = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::Data(snapshot_msg("target", 1)),
            connection_generation: direct_generation,
            connection_peer: HostName::new("target"),
        })
        .await;

    mgr.routes.get_mut(&HostName::new("target")).expect("route exists").fallbacks.push(RouteHop {
        next_hop: HostName::new("relay"),
        next_hop_generation: relay_generation,
        learned_epoch: 10,
    });

    let plan = mgr.disconnect_peer(&HostName::new("target"), direct_generation);

    assert_eq!(plan.affected_repos, vec![test_repo()]);
    assert_eq!(plan.resync_requests.len(), 1);
    let state = &mgr.get_peer_data()[&HostName::new("target")][&test_repo()];
    assert!(state.stale, "snapshot should be retained as stale");
    assert_eq!(mgr.routes[&HostName::new("target")].primary.next_hop, HostName::new("relay"));
}

#[tokio::test]
async fn accepted_snapshot_refreshes_route_primary_to_live_hop() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let relay_a_generation =
        accepted_generation(mgr.activate_connection(HostName::new("relay-a"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));
    let relay_b_generation =
        accepted_generation(mgr.activate_connection(HostName::new("relay-b"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

    let _ = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::Data(snapshot_msg("target", 1)),
            connection_generation: relay_a_generation,
            connection_peer: HostName::new("relay-a"),
        })
        .await;
    let _ = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::Data(snapshot_msg("target", 2)),
            connection_generation: relay_b_generation,
            connection_peer: HostName::new("relay-b"),
        })
        .await;

    assert_eq!(mgr.routes[&HostName::new("target")].primary.next_hop, HostName::new("relay-b"));
    assert_eq!(mgr.routes[&HostName::new("target")].fallbacks[0].next_hop, HostName::new("relay-a"));
}

#[tokio::test]
async fn disconnect_peer_keeps_unrelated_pending_resync_requests() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let _ = accepted_generation(mgr.activate_connection(HostName::new("target"), MockPeerSender::discard(), ConnectionMeta {
        direction: ConnectionDirection::Inbound,
        config_label: None,
        expected_peer: None,
        config_backed: false,
    }));
    let other_generation =
        accepted_generation(mgr.activate_connection(HostName::new("other"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

    let kept_request_id = mgr.note_pending_resync_request(HostName::new("target"), test_repo());
    let dropped_request_id = mgr.note_pending_resync_request(HostName::new("other"), test_repo());

    let _ = mgr.disconnect_peer(&HostName::new("other"), other_generation);

    let kept_key = ReversePathKey {
        request_id: kept_request_id,
        requester_host: HostName::new("local"),
        target_host: HostName::new("target"),
        repo_identity: test_repo(),
    };
    let dropped_key = ReversePathKey {
        request_id: dropped_request_id,
        requester_host: HostName::new("local"),
        target_host: HostName::new("other"),
        repo_identity: test_repo(),
    };

    assert!(mgr.pending_resync_requests.contains_key(&kept_key));
    assert!(!mgr.pending_resync_requests.contains_key(&dropped_key));
}

#[tokio::test]
async fn disconnect_peer_reports_stale_generation_as_inactive() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let stale_generation = accepted_generation(mgr.activate_connection(HostName::new("peer"), MockPeerSender::discard(), ConnectionMeta {
        direction: ConnectionDirection::Inbound,
        config_label: None,
        expected_peer: None,
        config_backed: false,
    }));

    let _current_generation =
        accepted_generation(mgr.activate_connection(HostName::new("peer"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Outbound,
            config_label: None,
            expected_peer: Some(HostName::new("peer")),
            config_backed: true,
        }));

    let plan = mgr.disconnect_peer(&HostName::new("peer"), stale_generation);

    assert!(!plan.was_active);
    assert!(mgr.current_generation(&HostName::new("peer")).is_some());
}

#[tokio::test]
async fn failover_resync_for_relayed_origin_accepts_same_clock_snapshot() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let relay_a_generation =
        accepted_generation(mgr.activate_connection(HostName::new("relay-a"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));
    let relay_b_generation =
        accepted_generation(mgr.activate_connection(HostName::new("relay-b"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

    let baseline = snapshot_msg("target", 1);
    let _ = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::Data(baseline.clone()),
            connection_generation: relay_a_generation,
            connection_peer: HostName::new("relay-a"),
        })
        .await;
    mgr.routes.get_mut(&HostName::new("target")).expect("route exists").fallbacks.push(RouteHop {
        next_hop: HostName::new("relay-b"),
        next_hop_generation: relay_b_generation,
        learned_epoch: 10,
    });

    let plan = mgr.disconnect_peer(&HostName::new("relay-a"), relay_a_generation);
    let request_id = match &plan.resync_requests[0] {
        RoutedPeerMessage::RequestResync { request_id, .. } => *request_id,
        other => panic!("expected request_resync, got {:?}", other),
    };

    let result = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::Routed(RoutedPeerMessage::ResyncSnapshot {
                request_id,
                requester_host: HostName::new("local"),
                responder_host: HostName::new("target"),
                remaining_hops: 4,
                repo_identity: baseline.repo_identity.clone(),
                repo_path: baseline.repo_path.clone(),
                clock: baseline.clock.clone(),
                seq: 1,
                data: Box::new(ProviderData::default()),
            }),
            connection_generation: relay_b_generation,
            connection_peer: HostName::new("relay-b"),
        })
        .await;

    assert_eq!(result, HandleResult::Updated(test_repo()));
    let state = &mgr.get_peer_data()[&HostName::new("target")][&test_repo()];
    assert!(!state.stale);
    assert_eq!(state.via_peer, HostName::new("relay-b"));
}

#[tokio::test]
async fn consecutive_failovers_reissue_resync_for_stale_snapshot() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let relay_a_generation =
        accepted_generation(mgr.activate_connection(HostName::new("relay-a"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));
    let relay_b_generation =
        accepted_generation(mgr.activate_connection(HostName::new("relay-b"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));
    let relay_c_generation =
        accepted_generation(mgr.activate_connection(HostName::new("relay-c"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

    let baseline = snapshot_msg("target", 1);
    let _ = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::Data(baseline.clone()),
            connection_generation: relay_a_generation,
            connection_peer: HostName::new("relay-a"),
        })
        .await;

    mgr.routes.get_mut(&HostName::new("target")).expect("route exists").fallbacks =
        vec![RouteHop { next_hop: HostName::new("relay-b"), next_hop_generation: relay_b_generation, learned_epoch: 10 }, RouteHop {
            next_hop: HostName::new("relay-c"),
            next_hop_generation: relay_c_generation,
            learned_epoch: 20,
        }];

    let first_plan = mgr.disconnect_peer(&HostName::new("relay-a"), relay_a_generation);
    assert_eq!(first_plan.resync_requests.len(), 1);
    let state = &mgr.get_peer_data()[&HostName::new("target")][&test_repo()];
    assert!(state.stale);

    let second_plan = mgr.disconnect_peer(&HostName::new("relay-c"), relay_c_generation);

    assert_eq!(second_plan.resync_requests.len(), 1);
    match &second_plan.resync_requests[0] {
        RoutedPeerMessage::RequestResync { target_host, .. } => {
            assert_eq!(target_host, &HostName::new("target"));
        }
        other => panic!("expected request_resync, got {:?}", other),
    }
    assert_eq!(mgr.routes[&HostName::new("target")].primary.next_hop, HostName::new("relay-b"));
}

#[tokio::test]
async fn failover_resync_clears_stale_and_rebinds_provenance() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let direct_generation =
        accepted_generation(mgr.activate_connection(HostName::new("target"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Outbound,
            config_label: None,
            expected_peer: Some(HostName::new("target")),
            config_backed: true,
        }));
    let relay_generation =
        accepted_generation(mgr.activate_connection(HostName::new("relay"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Outbound,
            config_label: None,
            expected_peer: Some(HostName::new("relay")),
            config_backed: true,
        }));
    let baseline = snapshot_msg("target", 1);
    let _ = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::Data(baseline.clone()),
            connection_generation: direct_generation,
            connection_peer: HostName::new("target"),
        })
        .await;

    mgr.routes.get_mut(&HostName::new("target")).expect("route exists").fallbacks.push(RouteHop {
        next_hop: HostName::new("relay"),
        next_hop_generation: relay_generation,
        learned_epoch: 10,
    });

    let plan = mgr.disconnect_peer(&HostName::new("target"), direct_generation);
    let request = match &plan.resync_requests[0] {
        RoutedPeerMessage::RequestResync { request_id, .. } => *request_id,
        other => panic!("expected request_resync, got {:?}", other),
    };

    let result = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::Routed(RoutedPeerMessage::ResyncSnapshot {
                request_id: request,
                requester_host: HostName::new("local"),
                responder_host: HostName::new("target"),
                remaining_hops: 4,
                repo_identity: baseline.repo_identity.clone(),
                repo_path: baseline.repo_path.clone(),
                clock: baseline.clock.clone(),
                seq: 1,
                data: Box::new(ProviderData::default()),
            }),
            connection_generation: relay_generation,
            connection_peer: HostName::new("relay"),
        })
        .await;

    assert_eq!(result, HandleResult::Updated(test_repo()));
    let state = &mgr.get_peer_data()[&HostName::new("target")][&test_repo()];
    assert!(!state.stale, "failover resync should clear stale");
    assert_eq!(state.via_peer, HostName::new("relay"));
    assert_eq!(state.via_generation, relay_generation);
}

#[tokio::test]
async fn expired_resync_request_removes_stale_snapshot() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let relay_generation =
        accepted_generation(mgr.activate_connection(HostName::new("relay"), MockPeerSender::discard(), ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

    let _ = mgr
        .handle_inbound(InboundPeerEnvelope {
            msg: PeerWireMessage::Data(snapshot_msg("target", 1)),
            connection_generation: relay_generation,
            connection_peer: HostName::new("relay"),
        })
        .await;

    let state = mgr.peer_data.get_mut(&HostName::new("target")).and_then(|repos| repos.get_mut(&test_repo())).expect("repo state");
    state.stale = true;

    mgr.pending_resync_requests.insert(
        ReversePathKey {
            request_id: 7,
            requester_host: HostName::new("local"),
            target_host: HostName::new("target"),
            repo_identity: test_repo(),
        },
        PendingResyncRequest { deadline_at: Instant::now() - Duration::from_secs(1) },
    );

    let affected = mgr.sweep_expired_resyncs(Instant::now());

    assert_eq!(affected, vec![test_repo()]);
    assert!(!mgr.pending_resync_requests.iter().any(|(key, _)| key.request_id == 7));
    assert!(!mgr.peer_data.get(&HostName::new("target")).is_some_and(|repos| repos.contains_key(&test_repo())));
}

#[tokio::test]
async fn disconnect_peer_returns_overlay_updates_for_remaining_peers() {
    let mut mgr = PeerManager::new(HostName::new("local"));

    handle_test_peer_data(&mut mgr, snapshot_msg("desktop", 1), MockPeerSender::discard).await;
    handle_test_peer_data(&mut mgr, snapshot_msg("laptop", 1), MockPeerSender::discard).await;

    let desktop_generation = mgr.current_generation(&HostName::new("desktop")).expect("desktop connected");

    let plan = mgr.disconnect_peer(&HostName::new("desktop"), desktop_generation);

    assert!(plan.was_active);
    assert_eq!(plan.overlay_updates.len(), 1);
    match &plan.overlay_updates[0] {
        OverlayUpdate::SetProviders { identity, peers, overlay_version } => {
            assert_eq!(identity, &test_repo());
            assert_eq!(peers.len(), 1);
            assert_eq!(peers[0].0, HostName::new("laptop"));
            assert!(*overlay_version > 0, "overlay_version should be bumped on disconnect");
        }
        other => panic!("expected SetProviders, got {:?}", other),
    }
}

#[tokio::test]
async fn disconnect_peer_returns_remove_repo_for_remote_only_with_no_remaining_peers() {
    let mut mgr = PeerManager::new(HostName::new("local"));

    handle_test_peer_data(&mut mgr, snapshot_msg("desktop", 1), MockPeerSender::discard).await;

    let desktop_generation = mgr.current_generation(&HostName::new("desktop")).expect("desktop connected");

    let synthetic_path = PathBuf::from("/virtual/github.com/owner/repo");
    mgr.register_remote_repo(test_repo(), synthetic_path.clone());

    let plan = mgr.disconnect_peer(&HostName::new("desktop"), desktop_generation);

    assert!(plan.was_active);
    assert_eq!(plan.overlay_updates.len(), 1);
    match &plan.overlay_updates[0] {
        OverlayUpdate::RemoveRepo { identity, path } => {
            assert_eq!(identity, &test_repo());
            assert_eq!(path, &synthetic_path);
        }
        other => panic!("expected RemoveRepo, got {:?}", other),
    }
    assert!(!mgr.is_remote_repo(&test_repo()));
}

#[tokio::test]
async fn get_sender_if_current_returns_sender_for_matching_generation() {
    let mut mgr = PeerManager::new(HostName::new("local"));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&sent) });
    let generation = accepted_generation(mgr.activate_connection(HostName::new("peer"), sender, ConnectionMeta {
        direction: ConnectionDirection::Inbound,
        config_label: None,
        expected_peer: None,
        config_backed: false,
    }));

    assert!(mgr.get_sender_if_current(&HostName::new("peer"), generation).is_some());
    assert!(mgr.get_sender_if_current(&HostName::new("peer"), generation + 1).is_none());
    assert!(mgr.get_sender_if_current(&HostName::new("unknown"), 1).is_none());
}
