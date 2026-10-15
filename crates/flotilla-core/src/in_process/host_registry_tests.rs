#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap};

    use flotilla_protocol::{
        qualified_path::HostId, DaemonEvent, EnvironmentId, HostName, HostSnapshot, HostSummary, NodeId, NodeInfo, PeerConnectionState,
        StreamKey, SystemInfo, ToolInventory,
    };

    use flotilla_projections::host_registry::{HostCounts, HostRegistry};

    fn local_node() -> NodeInfo {
        NodeInfo::new(NodeId::new("local-node"), "local-host")
    }

    fn peer_node() -> NodeInfo {
        NodeInfo::new(NodeId::new("peer-node"), "peer-host")
    }

    fn minimal_summary(node: &NodeInfo) -> HostSummary {
        HostSummary {
            environment_id: EnvironmentId::host(HostId::new(format!("{}-host", node.node_id))),
            host_name: Some(HostName::new(node.display_name.clone())),
            node: node.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        }
    }

    #[tokio::test]
    async fn publish_peer_connection_status_without_environment_mapping_does_not_synthesize_host_snapshot() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let events = RefCell::new(Vec::new());
        let emit = |event| events.borrow_mut().push(event);

        registry.publish_peer_connection_status(&peer_node(), PeerConnectionState::Connected, &HashMap::new(), &emit).await;

        assert!(events.borrow().is_empty(), "peer status without a canonical environment id should not emit host state");
    }

    #[tokio::test]
    async fn connected_peer_summaries_only_returns_live_routes() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer = peer_node();
        let summary = minimal_summary(&peer);
        registry.publish_peer_summary(summary.clone(), &|_| {}).await;

        assert!(registry.connected_peer_summaries().await.is_empty(), "a disconnected peer must not receive heartbeat refreshes");

        registry.publish_peer_connection_status(&peer, PeerConnectionState::Connected, &HashMap::new(), &|_| {}).await;
        assert_eq!(registry.connected_peer_summaries().await, vec![summary]);

        registry.publish_peer_connection_status(&peer, PeerConnectionState::Disconnected, &HashMap::new(), &|_| {}).await;
        assert!(registry.connected_peer_summaries().await.is_empty(), "a disconnected peer must stop receiving heartbeat refreshes");
    }

    #[tokio::test]
    async fn replay_uses_environment_id_stream_keys() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        registry.publish_peer_summary(minimal_summary(&peer_node()), &|_| {}).await;

        let replay = registry.replay_host_events(&HashMap::new()).await;
        assert!(replay
            .iter()
            .any(|event| matches!(event, DaemonEvent::HostSnapshot(snapshot) if snapshot.node.node_id == peer_node().node_id)));

        let replay = registry
            .replay_host_events(&HashMap::from([(
                StreamKey::Host { environment_id: EnvironmentId::host(HostId::new("peer-node-host")) },
                2,
            )]))
            .await;
        assert!(!replay
            .iter()
            .any(|event| matches!(event, DaemonEvent::HostSnapshot(snapshot) if snapshot.node.node_id == peer_node().node_id)));
    }

    #[tokio::test]
    async fn configured_peers_without_environment_mapping_do_not_create_host_entries() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        registry.set_configured_peers(vec![NodeInfo::new(NodeId::new("peer-node"), "Build Box")], &HashMap::new(), &|_| {}).await;

        let hosts = registry.list_hosts(&HashMap::new()).await;
        assert!(hosts.hosts.iter().all(|entry| entry.node.as_ref().is_none_or(|node| node.node_id != peer_node().node_id)));
    }

    #[tokio::test]
    async fn overlay_placeholder_does_not_clobber_existing_real_summary() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer = NodeInfo::new(NodeId::new("peer-node-42"), "Build Box");
        let real_summary = HostSummary {
            environment_id: EnvironmentId::host(HostId::new("peer-node-42-host")),
            host_name: Some(HostName::new("build-box")),
            node: peer.clone(),
            system: SystemInfo { os: Some("linux".into()), arch: Some("x86_64".into()), ..SystemInfo::default() },
            inventory: ToolInventory::default(),
            providers: vec![flotilla_protocol::HostProviderStatus {
                category: "workspace".into(),
                name: "cmux".into(),
                implementation: "cmux".into(),
                healthy: true,
                disabled_reason: None,
            }],
            environments: vec![],
        };

        registry.publish_peer_summary(real_summary.clone(), &|_| {}).await;
        registry.publish_peer_summary(minimal_summary(&peer), &|_| {}).await;

        let status = registry.get_host_status(&real_summary.environment_id, &HashMap::new()).await.expect("host status");
        let summary = status.summary.expect("summary should remain available");
        assert_eq!(summary.node.display_name, "Build Box");
        assert_eq!(summary.providers, real_summary.providers);
        assert_eq!(summary.system, real_summary.system);
    }

    #[tokio::test]
    async fn overlay_placeholder_for_second_environment_on_same_node_is_not_dropped() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer = NodeInfo::new(NodeId::new("peer-node-43"), "Build Box");
        let first_environment_id = EnvironmentId::host(HostId::new("peer-node-43-host-a"));
        let second_environment_id = EnvironmentId::host(HostId::new("peer-node-43-host-b"));

        let real_summary = HostSummary {
            environment_id: first_environment_id.clone(),
            host_name: Some(HostName::new("build-box-a")),
            node: peer.clone(),
            system: SystemInfo { os: Some("linux".into()), arch: Some("x86_64".into()), ..SystemInfo::default() },
            inventory: ToolInventory::default(),
            providers: vec![flotilla_protocol::HostProviderStatus {
                category: "workspace".into(),
                name: "cmux".into(),
                implementation: "cmux".into(),
                healthy: true,
                disabled_reason: None,
            }],
            environments: vec![],
        };
        let placeholder_summary = HostSummary {
            environment_id: second_environment_id.clone(),
            host_name: Some(HostName::new("build-box-b")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };

        registry.publish_peer_summary(real_summary, &|_| {}).await;
        registry.publish_peer_summary(placeholder_summary.clone(), &|_| {}).await;

        let status = registry.get_host_status(&second_environment_id, &HashMap::new()).await.expect("status for second environment");

        assert_eq!(status.environment_id, second_environment_id);
        assert_eq!(status.summary.expect("placeholder summary should be visible").environment_id, placeholder_summary.environment_id);

        let hosts = registry.list_hosts(&HashMap::new()).await;
        assert!(
            hosts.hosts.iter().any(|entry| entry.environment_id.as_ref() == Some(&first_environment_id)),
            "the first live environment should remain visible"
        );
        assert!(
            hosts.hosts.iter().any(|entry| entry.environment_id.as_ref() == Some(&second_environment_id)),
            "the second live environment should be visible even with a placeholder summary"
        );
    }

    #[tokio::test]
    async fn set_peer_host_summaries_removes_missing_environment_for_connected_node() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer = NodeInfo::new(NodeId::new("peer-node-44"), "Build Box");
        let first_environment_id = EnvironmentId::host(HostId::new("peer-node-44-host-a"));
        let second_environment_id = EnvironmentId::host(HostId::new("peer-node-44-host-b"));

        let first_summary = HostSummary {
            environment_id: first_environment_id.clone(),
            host_name: Some(HostName::new("build-box-a")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };
        let second_summary = HostSummary {
            environment_id: second_environment_id.clone(),
            host_name: Some(HostName::new("build-box-b")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };

        registry.publish_peer_connection_status(&peer, PeerConnectionState::Connected, &HashMap::new(), &|_| {}).await;
        registry.publish_peer_summary(first_summary.clone(), &|_| {}).await;
        registry.publish_peer_summary(second_summary.clone(), &|_| {}).await;

        let events = RefCell::new(Vec::new());
        let emit = |event: DaemonEvent| events.borrow_mut().push(event);
        registry
            .set_peer_host_summaries(HashMap::from([(second_environment_id.clone(), second_summary.clone())]), &HashMap::new(), &emit)
            .await;

        {
            let captured = events.borrow();
            assert!(
                captured.iter().any(
                    |event| matches!(event, DaemonEvent::HostRemoved { environment_id, .. } if *environment_id == first_environment_id)
                ),
                "missing environment should be removed when it disappears from the peer summary set"
            );
        }

        let err = registry.get_host_status(&first_environment_id, &HashMap::new()).await.expect_err("removed environment should disappear");
        assert!(err.contains("host not found"), "unexpected error: {err}");

        let status =
            registry.get_host_status(&second_environment_id, &HashMap::new()).await.expect("remaining environment should stay live");
        assert_eq!(status.environment_id, second_environment_id);
        assert_eq!(registry.environment_id_for_node(&peer.node_id).await, Some(second_environment_id));
    }

    #[tokio::test]
    async fn host_entries_are_not_synthesized_from_node_identity_without_environment_mapping() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );

        registry.publish_peer_connection_status(&peer_node(), PeerConnectionState::Connected, &HashMap::new(), &|_| {}).await;

        let hosts = registry.list_hosts(&HashMap::new()).await;
        assert!(hosts.hosts.iter().all(|entry| entry.node.as_ref().is_none_or(|node| node.node_id != peer_node().node_id)));
    }

    #[tokio::test]
    async fn out_of_order_status_remove_and_reconnect_recover_cached_peer_state() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer = peer_node();
        let first_environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));
        let second_environment_id = EnvironmentId::host(HostId::new("peer-node-host-b"));
        let first_summary = HostSummary {
            environment_id: first_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-a")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };
        let second_summary = HostSummary {
            environment_id: second_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-b")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };

        let events = RefCell::new(Vec::new());
        let emit = |event: DaemonEvent| events.borrow_mut().push(event);

        registry.publish_peer_connection_status(&peer, PeerConnectionState::Connected, &HashMap::new(), &emit).await;
        assert!(events.borrow().is_empty(), "status before mapping should be cached, not emitted");

        registry.publish_peer_summary(first_summary.clone(), &emit).await;
        let first_snapshot_seq = {
            let captured = events.borrow();
            let first_snapshot = captured
                .iter()
                .find_map(|event| match event {
                    DaemonEvent::HostSnapshot(snapshot) if snapshot.node.node_id == peer.node_id => Some(snapshot),
                    _ => None,
                })
                .expect("expected first host snapshot");
            assert_eq!(first_snapshot.environment_id, first_environment_id);
            assert_eq!(first_snapshot.connection_status, PeerConnectionState::Connected);
            first_snapshot.seq
        };
        assert_eq!(registry.environment_id_for_node(&peer.node_id).await, Some(first_environment_id.clone()));

        events.borrow_mut().clear();
        registry.apply_event(&DaemonEvent::HostRemoved { environment_id: first_environment_id.clone(), seq: first_snapshot_seq + 1 });
        assert_eq!(registry.environment_id_for_node(&peer.node_id).await, None, "removing the host should clear the node mapping");

        registry.publish_peer_connection_status(&peer, PeerConnectionState::Connected, &HashMap::new(), &emit).await;
        assert!(events.borrow().is_empty(), "reconnect before the replacement summary should be cached");

        registry.publish_peer_summary(second_summary.clone(), &emit).await;
        {
            let captured = events.borrow();
            let second_snapshot = captured
                .iter()
                .find_map(|event| match event {
                    DaemonEvent::HostSnapshot(snapshot) if snapshot.node.node_id == peer.node_id => Some(snapshot),
                    _ => None,
                })
                .expect("expected second host snapshot");
            assert_eq!(second_snapshot.environment_id, second_environment_id);
            assert_eq!(second_snapshot.connection_status, PeerConnectionState::Connected);
        }
        assert_eq!(registry.environment_id_for_node(&peer.node_id).await, Some(second_environment_id));
    }

    #[tokio::test]
    async fn host_list_retains_multiple_environments_for_the_same_node() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer = peer_node();

        let first_summary = HostSummary {
            environment_id: EnvironmentId::host(HostId::new("peer-node-host-a")),
            host_name: Some(HostName::new("peer-host-a")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };
        let second_summary = HostSummary {
            environment_id: EnvironmentId::host(HostId::new("peer-node-host-b")),
            host_name: Some(HostName::new("peer-host-b")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };

        registry.publish_peer_summary(first_summary.clone(), &|_| {}).await;
        registry.publish_peer_summary(second_summary.clone(), &|_| {}).await;

        let hosts = registry.list_hosts(&HashMap::new()).await;
        let peer_entries: Vec<_> =
            hosts.hosts.iter().filter(|entry| entry.node.as_ref().is_some_and(|node| node.node_id == peer.node_id)).collect();

        assert_eq!(peer_entries.len(), 2, "a single node should be able to expose multiple host environments");
        assert!(peer_entries.iter().any(|entry| entry.environment_id.as_ref() == Some(&first_summary.environment_id)));
        assert!(peer_entries.iter().any(|entry| entry.environment_id.as_ref() == Some(&second_summary.environment_id)));

        let status_a =
            registry.get_host_status(&first_summary.environment_id, &HashMap::new()).await.expect("status for first environment");
        let status_b =
            registry.get_host_status(&second_summary.environment_id, &HashMap::new()).await.expect("status for second environment");

        assert_eq!(status_a.environment_id, first_summary.environment_id);
        assert_eq!(status_b.environment_id, second_summary.environment_id);
    }

    #[tokio::test]
    async fn peer_connection_status_updates_all_live_environments_for_the_same_node() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer = peer_node();
        let first_environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));
        let second_environment_id = EnvironmentId::host(HostId::new("peer-node-host-b"));

        registry
            .publish_peer_summary(
                HostSummary {
                    environment_id: first_environment_id.clone(),
                    host_name: Some(HostName::new("peer-host-a")),
                    node: peer.clone(),
                    system: SystemInfo::default(),
                    inventory: ToolInventory::default(),
                    providers: vec![],
                    environments: vec![],
                },
                &|_| {},
            )
            .await;
        registry
            .publish_peer_summary(
                HostSummary {
                    environment_id: second_environment_id.clone(),
                    host_name: Some(HostName::new("peer-host-b")),
                    node: peer.clone(),
                    system: SystemInfo::default(),
                    inventory: ToolInventory::default(),
                    providers: vec![],
                    environments: vec![],
                },
                &|_| {},
            )
            .await;

        registry.publish_peer_connection_status(&peer, PeerConnectionState::Connected, &HashMap::new(), &|_| {}).await;

        let first_status = registry.get_host_status(&first_environment_id, &HashMap::new()).await.expect("first host status");
        let second_status = registry.get_host_status(&second_environment_id, &HashMap::new()).await.expect("second host status");
        assert_eq!(first_status.connection_status, PeerConnectionState::Connected);
        assert_eq!(second_status.connection_status, PeerConnectionState::Connected);
    }

    #[tokio::test]
    async fn visible_host_status_changes_emit_live_events_for_existing_hosts() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer = peer_node();
        let environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));

        registry
            .publish_peer_summary(
                HostSummary {
                    environment_id: environment_id.clone(),
                    host_name: Some(HostName::new("peer-host-a")),
                    node: peer.clone(),
                    system: SystemInfo::default(),
                    inventory: ToolInventory::default(),
                    providers: vec![],
                    environments: vec![],
                },
                &|_| {},
            )
            .await;

        let events = RefCell::new(Vec::new());
        let emit = |event: DaemonEvent| events.borrow_mut().push(event);

        registry.publish_peer_connection_status(&peer, PeerConnectionState::Connected, &HashMap::new(), &emit).await;

        let captured = events.borrow();
        assert!(
            captured.iter().any(|event| matches!(event, DaemonEvent::PeerStatusChanged { node_id, status } if node_id == &peer.node_id && *status == PeerConnectionState::Connected)),
            "a visible host status transition should emit a peer status event"
        );
        let snapshot = captured
            .iter()
            .find_map(|event| match event {
                DaemonEvent::HostSnapshot(snapshot) if snapshot.environment_id == environment_id => Some(snapshot),
                _ => None,
            })
            .expect("a visible host should receive a refreshed snapshot");
        assert_eq!(snapshot.connection_status, PeerConnectionState::Connected);
    }

    #[tokio::test]
    async fn host_list_uses_environment_scoped_counts_for_shared_node() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer = peer_node();
        let first_environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));
        let second_environment_id = EnvironmentId::host(HostId::new("peer-node-host-b"));

        registry
            .publish_peer_summary(
                HostSummary {
                    environment_id: first_environment_id.clone(),
                    host_name: Some(HostName::new("peer-host-a")),
                    node: peer.clone(),
                    system: SystemInfo::default(),
                    inventory: ToolInventory::default(),
                    providers: vec![],
                    environments: vec![],
                },
                &|_| {},
            )
            .await;
        registry
            .publish_peer_summary(
                HostSummary {
                    environment_id: second_environment_id.clone(),
                    host_name: Some(HostName::new("peer-host-b")),
                    node: peer.clone(),
                    system: SystemInfo::default(),
                    inventory: ToolInventory::default(),
                    providers: vec![],
                    environments: vec![],
                },
                &|_| {},
            )
            .await;

        let counts = HashMap::from([
            (first_environment_id.clone(), HostCounts { repo_count: 1 }),
            (second_environment_id.clone(), HostCounts { repo_count: 3 }),
        ]);
        let hosts = registry.list_hosts(&counts).await;

        let first_entry =
            hosts.hosts.iter().find(|entry| entry.environment_id.as_ref() == Some(&first_environment_id)).expect("first entry");
        let second_entry =
            hosts.hosts.iter().find(|entry| entry.environment_id.as_ref() == Some(&second_environment_id)).expect("second entry");
        assert_eq!(first_entry.repo_count, 1);
        assert_eq!(second_entry.repo_count, 3);
    }

    #[tokio::test]
    async fn host_snapshot_replay_does_not_rollback_newer_node_connectivity() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer = peer_node();
        let environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));

        registry
            .publish_peer_summary(
                HostSummary {
                    environment_id: environment_id.clone(),
                    host_name: Some(HostName::new("peer-host-a")),
                    node: peer.clone(),
                    system: SystemInfo::default(),
                    inventory: ToolInventory::default(),
                    providers: vec![],
                    environments: vec![],
                },
                &|_| {},
            )
            .await;
        registry.apply_event(&DaemonEvent::PeerStatusChanged { node_id: peer.node_id.clone(), status: PeerConnectionState::Connected });

        registry.apply_event(&DaemonEvent::HostSnapshot(Box::new(HostSnapshot {
            seq: 2,
            environment_id: environment_id.clone(),
            node: peer.clone(),
            is_local: false,
            connection_status: PeerConnectionState::Disconnected,
            summary: HostSummary {
                environment_id: environment_id.clone(),
                host_name: Some(HostName::new("peer-host-a")),
                node: peer.clone(),
                system: SystemInfo::default(),
                inventory: ToolInventory::default(),
                providers: vec![],
                environments: vec![],
            },
        })));

        assert_eq!(registry.peer_connection_status(&peer.node_id).await, PeerConnectionState::Connected);
        let status = registry.get_host_status(&environment_id, &HashMap::new()).await.expect("host status");
        assert_eq!(status.connection_status, PeerConnectionState::Connected);
    }

    #[tokio::test]
    async fn stale_host_snapshot_from_another_environment_does_not_repoint_canonical_mapping_even_with_higher_seq() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer = peer_node();
        let current_environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));
        let stale_environment_id = EnvironmentId::host(HostId::new("peer-node-host-b"));

        let current_summary = HostSummary {
            environment_id: current_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-a")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![flotilla_protocol::HostProviderStatus {
                category: "workspace".into(),
                name: "cmux".into(),
                implementation: "cmux".into(),
                healthy: true,
                disabled_reason: None,
            }],
            environments: vec![],
        };
        registry.publish_peer_summary(current_summary, &|_| {}).await;
        assert_eq!(registry.environment_id_for_node(&peer.node_id).await, Some(current_environment_id.clone()));

        let stale_snapshot = HostSnapshot {
            seq: 9,
            environment_id: stale_environment_id.clone(),
            node: peer.clone(),
            is_local: false,
            connection_status: PeerConnectionState::Disconnected,
            summary: HostSummary {
                environment_id: stale_environment_id.clone(),
                host_name: Some(HostName::new("peer-host-b")),
                node: peer.clone(),
                system: SystemInfo::default(),
                inventory: ToolInventory::default(),
                providers: vec![],
                environments: vec![],
            },
        };

        registry.apply_event(&DaemonEvent::HostSnapshot(Box::new(stale_snapshot)));

        assert_eq!(
            registry.environment_id_for_node(&peer.node_id).await,
            Some(current_environment_id),
            "cross-environment snapshots must not move the canonical node-to-environment mapping based on seq alone"
        );
    }

    #[tokio::test]
    async fn removing_one_environment_keeps_another_live_environment_reachable() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer = peer_node();
        let first_environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));
        let second_environment_id = EnvironmentId::host(HostId::new("peer-node-host-b"));

        let first_summary = HostSummary {
            environment_id: first_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-a")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };
        let second_summary = HostSummary {
            environment_id: second_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-b")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };

        registry.publish_peer_summary(first_summary, &|_| {}).await;
        registry.publish_peer_summary(second_summary, &|_| {}).await;

        registry.apply_event(&DaemonEvent::HostRemoved { environment_id: first_environment_id.clone(), seq: 2 });

        assert_eq!(
            registry.environment_id_for_node(&peer.node_id).await,
            Some(second_environment_id.clone()),
            "removing one environment should leave another live environment addressable"
        );

        let status = registry.get_host_status(&second_environment_id, &HashMap::new()).await.expect("status for remaining environment");
        assert_eq!(status.environment_id, second_environment_id);
    }

    #[tokio::test]
    async fn removing_the_canonical_environment_reassigns_to_the_most_recent_remaining_environment() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer = peer_node();
        let canonical_environment_id = EnvironmentId::host(HostId::new("peer-node-host-a"));
        let older_remaining_environment_id = EnvironmentId::host(HostId::new("peer-node-host-b"));
        let newer_remaining_environment_id = EnvironmentId::host(HostId::new("peer-node-host-c"));

        let canonical_summary = HostSummary {
            environment_id: canonical_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-a")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };
        let older_remaining_summary = HostSummary {
            environment_id: older_remaining_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-b")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };
        let newer_remaining_summary = HostSummary {
            environment_id: newer_remaining_environment_id.clone(),
            host_name: Some(HostName::new("peer-host-c")),
            node: peer.clone(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![flotilla_protocol::HostProviderStatus {
                category: "workspace".into(),
                name: "cmux".into(),
                implementation: "cmux".into(),
                healthy: true,
                disabled_reason: None,
            }],
            environments: vec![],
        };

        registry.publish_peer_summary(canonical_summary, &|_| {}).await;
        registry.publish_peer_summary(older_remaining_summary, &|_| {}).await;
        registry.publish_peer_summary(newer_remaining_summary.clone(), &|_| {}).await;

        registry.apply_event(&DaemonEvent::HostRemoved { environment_id: canonical_environment_id.clone(), seq: 2 });

        assert_eq!(
            registry.environment_id_for_node(&peer.node_id).await,
            Some(newer_remaining_environment_id.clone()),
            "the canonical mapping should follow the most recently updated remaining environment"
        );

        let status =
            registry.get_host_status(&newer_remaining_environment_id, &HashMap::new()).await.expect("status for remaining environment");
        assert_eq!(status.environment_id, newer_remaining_environment_id);
        assert!(status.summary.is_some());
    }

    #[tokio::test]
    async fn removed_environment_status_lookup_returns_not_found() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer_summary = HostSummary {
            environment_id: EnvironmentId::host(HostId::new("peer-node-host-a")),
            host_name: Some(HostName::new("peer-host-a")),
            node: peer_node(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };

        registry.publish_peer_summary(peer_summary.clone(), &|_| {}).await;
        registry.apply_event(&DaemonEvent::HostRemoved { environment_id: peer_summary.environment_id.clone(), seq: 2 });

        let err = registry.get_host_status(&peer_summary.environment_id, &HashMap::new()).await.expect_err("removed host should be absent");
        assert!(err.contains("host not found"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn removed_environment_providers_lookup_returns_not_found() {
        let registry = HostRegistry::new(
            local_node(),
            minimal_summary(&local_node()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        );
        let peer_summary = HostSummary {
            environment_id: EnvironmentId::host(HostId::new("peer-node-host-b")),
            host_name: Some(HostName::new("peer-host-b")),
            node: peer_node(),
            system: SystemInfo::default(),
            inventory: ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        };

        registry.publish_peer_summary(peer_summary.clone(), &|_| {}).await;
        registry.apply_event(&DaemonEvent::HostRemoved { environment_id: peer_summary.environment_id.clone(), seq: 2 });

        let err =
            registry.get_host_providers(&peer_summary.environment_id, &HashMap::new()).await.expect_err("removed host should be absent");
        assert!(err.contains("host not found"), "unexpected error: {err}");
    }
}
