use std::collections::HashMap;

use flotilla_protocol::{
    CommandValue, DaemonEvent, EnvironmentId, HostName, HostSnapshot, HostSummary, NodeId, NodeInfo, PeerConnectionState, StreamKey,
};

use super::{event_stream_seq, format_command_result, format_event_human};

#[test]
fn replaced_pending_brief_echoes_the_displaced_text() {
    let output = format_command_result(&CommandValue::ConvoyBriefQueued { displaced: Some("older instruction".to_string()) });
    assert_eq!(output, "pending brief replaced; displaced brief:\nolder instruction");
}

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
