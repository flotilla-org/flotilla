use std::collections::HashMap;

use flotilla_protocol::{
    DaemonEvent, EnvironmentId, HostName, HostSnapshot, HostSummary, NodeId, NodeInfo, PeerConnectionState, StreamKey, TopologyResponse,
    TopologyRoute,
};

use super::{event_stream_seq, format_event_human, format_topology_dot};

#[test]
fn host_snapshot_formats_and_exposes_its_stream() {
    let environment_id = EnvironmentId::new("env-1");
    let event = DaemonEvent::HostSnapshot(Box::new(HostSnapshot {
        seq: 3,
        environment_id: environment_id.clone(),
        node: NodeInfo::new(NodeId::new("node-1"), "host-1"),
        is_local: false,
        connection_status: PeerConnectionState::Connected,
        summary: HostSummary {
            environment_id: environment_id.clone(),
            host_name: Some(HostName::new("host-1")),
            node: NodeInfo::new(NodeId::new("node-1"), "host-1"),
            system: Default::default(),
            inventory: Default::default(),
            providers: vec![],
            environments: vec![],
        },
    }));
    assert!(format_event_human(&event).contains("host-1"));
    assert_eq!(event_stream_seq(&event), Some((StreamKey::Host { environment_id }, 3)));
}

#[test]
fn repo_tracked_has_no_replay_stream() {
    let info = flotilla_protocol::RepoInfo {
        identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        repository_key: None,
        path: None,
        name: "repo".into(),
        labels: Default::default(),
        provider_names: HashMap::new(),
        provider_health: HashMap::new(),
        loading: false,
    };
    let event = DaemonEvent::RepoTracked(Box::new(info));
    assert_eq!(event_stream_seq(&event), None);
}

#[test]
fn topology_dot_renders_hosts_and_route_semantics_deterministically() {
    let response = TopologyResponse {
        local_node: NodeInfo::new(NodeId::new("local-node"), "workstation"),
        routes: vec![
            TopologyRoute {
                target: NodeInfo::new(NodeId::new("remote-b"), "build host"),
                next_hop: NodeInfo::new(NodeId::new("remote-b"), "build host"),
                direct: true,
                connected: true,
                fallbacks: vec![],
                last_attempt: None,
                last_error: None,
            },
            TopologyRoute {
                target: NodeInfo::new(NodeId::new("remote-c"), "cloud runner"),
                next_hop: NodeInfo::new(NodeId::new("remote-b"), "build host"),
                direct: false,
                connected: false,
                fallbacks: vec![NodeInfo::new(NodeId::new("remote-d"), "backup")],
                last_attempt: None,
                last_error: None,
            },
        ],
    };

    assert_eq!(
        format_topology_dot(&response),
        concat!(
            "digraph topology {\n",
            "  graph [rankdir=LR];\n",
            "  node [shape=ellipse];\n",
            "  \"local-node\" [label=\"workstation\", shape=doublecircle];\n",
            "  \"remote-b\" [label=\"build host\"];\n",
            "  \"remote-c\" [label=\"cloud runner\"];\n",
            "  \"remote-d\" [label=\"backup\"];\n",
            "  \"local-node\" -> \"remote-b\" [label=\"direct\"];\n",
            "  \"remote-b\" -> \"remote-c\" [label=\"route, disconnected\", color=red, style=dashed];\n",
            "  \"remote-d\" -> \"remote-c\" [label=\"fallback\", style=dotted];\n",
            "}\n",
        )
    );
}

#[test]
fn topology_dot_uses_stable_ids_and_escapes_labels() {
    let response = TopologyResponse { local_node: NodeInfo::new(NodeId::new("node \"one"), "desk\\office\nprimary"), routes: vec![] };

    assert!(format_topology_dot(&response).contains(r#"  "node \"one" [label="desk\\office\nprimary", shape=doublecircle];"#));
}
