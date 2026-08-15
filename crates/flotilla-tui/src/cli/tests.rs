use std::{collections::HashMap, io::Write};

use flotilla_protocol::{
    DaemonEvent, EnvironmentId, HostName, HostSnapshot, HostSummary, NodeId, NodeInfo, PeerConnectionState, StreamKey,
};

use super::{event_stream_seq, format_event_human, write_finished_command};

#[derive(Default)]
struct FlushRecordingWriter {
    bytes: Vec<u8>,
    flushes: usize,
}

impl Write for FlushRecordingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

#[test]
fn convoy_started_confirmation_is_flushed_before_auto_attach() {
    let result = flotilla_protocol::CommandValue::ConvoyStarted {
        name: "demo".to_string(),
        attach_plan: Some(flotilla_protocol::ResolvedAttachPlan(Vec::new())),
        binding: None,
    };
    let event = DaemonEvent::CommandFinished {
        command_id: 7,
        node_id: NodeId::new("local"),
        repo_identity: flotilla_protocol::RepoIdentity { authority: String::new(), path: String::new() },
        repo: None,
        result: result.clone(),
    };
    let mut output = FlushRecordingWriter::default();

    write_finished_command(&mut output, &event, &result, flotilla_protocol::output::OutputFormat::Human).expect("write confirmation");

    assert_eq!(String::from_utf8(output.bytes).expect("utf-8 output"), "convoy started: demo (crew ready)\n");
    assert_eq!(output.flushes, 1);
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
