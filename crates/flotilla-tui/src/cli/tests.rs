use std::collections::HashMap;

use flotilla_protocol::{
    CommandValue, ConvoyExplanation, DaemonEvent, EnvironmentId, ExplainedDecisionLedger, ExplainedSettlement, ExplainedUnclaimedWork,
    HostName, HostSnapshot, HostSummary, NodeId, NodeInfo, PeerConnectionState, StreamKey, TopologyResponse, TopologyRoute,
};

use super::{event_stream_seq, format_command_result, format_convoy_explanation_human, format_event_human, format_topology_dot};

#[test]
fn convoy_explanation_renders_linked_and_missing_decision_ledgers() {
    let explanation = ConvoyExplanation {
        recent_events: Vec::new(),
        lifecycle_mutations: Vec::new(),
        namespace: "flotilla".into(),
        convoy: "ledger".into(),
        phase: "Landing".into(),
        message: Some("waiting for review evidence".into()),
        evidence_ttl_seconds: 30,
        change_request_stale_after_seconds: 30,
        checkouts: Vec::new(),
        change_requests: Vec::new(),
        subscriptions: Vec::new(),
        crew_deliveries: Vec::new(),
        unclaimed_work: vec![ExplainedUnclaimedWork { vessel: "work".into(), role: "coder".into(), evidence: "turn_idle".into() }],
        decision_ledgers: vec![
            ExplainedDecisionLedger {
                vessel: "work".into(),
                role: "coder".into(),
                claimed_at: Some("2026-08-21T12:00:00Z".into()),
                comment_url: Some("https://example.test/pull/1#comment-2".into()),
                missing: false,
                override_principal: None,
                completed_while_crew_active: false,
            },
            ExplainedDecisionLedger {
                vessel: "review".into(),
                role: "reviewer".into(),
                claimed_at: Some("2026-08-21T12:01:00Z".into()),
                comment_url: None,
                missing: true,
                override_principal: None,
                completed_while_crew_active: false,
            },
            ExplainedDecisionLedger {
                vessel: "research".into(),
                role: "researcher".into(),
                claimed_at: Some("2026-08-21T12:02:00Z".into()),
                comment_url: None,
                missing: true,
                override_principal: Some(flotilla_protocol::PrincipalRef { namespace: "flotilla".into(), name: "operator".into() }),
                completed_while_crew_active: true,
            },
        ],
        settlement: ExplainedSettlement { mode: "world_terminal".into(), satisfied: false, unmet: Vec::new() },
    };

    let output = format_convoy_explanation_human(&explanation);
    assert!(output.contains("Message: waiting for review evidence"));
    assert!(output.contains("Crew work needing a settlement claim:\n  - work/coder (turn idle)"));
    assert!(output.contains("work/coder claimed_at=2026-08-21T12:00:00Z comment=https://example.test/pull/1#comment-2"));
    assert!(output.contains("review/reviewer claimed_at=2026-08-21T12:01:00Z MISSING (crew completed without a decision ledger)"));
    assert!(output.contains(
        "research/researcher claimed_at=2026-08-21T12:02:00Z MISSING (completed by flotilla/operator with --force) — completed while crew active"
    ));
}

#[test]
fn replaced_pending_brief_echoes_the_displaced_text() {
    let output = format_command_result(&CommandValue::ConvoyBriefQueued { displaced: Some("older instruction".to_string()) });
    assert_eq!(output, "brief queued for turn end; displaced brief:\nolder instruction");
}

#[test]
fn convoy_resume_reports_immediate_and_deferred_delivery_distinctly() {
    assert_eq!(format_command_result(&CommandValue::ConvoyBriefDelivered { displaced: None }), "brief delivered now");
    assert_eq!(format_command_result(&CommandValue::ConvoyBriefQueued { displaced: None }), "brief queued for turn end");
}

#[test]
fn immediately_delivered_brief_echoes_displaced_pending_text() {
    let output = format_command_result(&CommandValue::ConvoyBriefDelivered { displaced: Some("older instruction".to_string()) });
    assert_eq!(output, "brief delivered now; displaced pending brief:\nolder instruction");
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
            TopologyRoute {
                target: NodeInfo::new(NodeId::new("remote-e"), "test host"),
                next_hop: NodeInfo::new(NodeId::new("remote-b"), "build host"),
                direct: false,
                connected: true,
                fallbacks: vec![],
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
            "  \"remote-e\" [label=\"test host\"];\n",
            "  \"local-node\" -> \"remote-b\" [label=\"direct\"];\n",
            "  \"remote-b\" -> \"remote-c\" [label=\"route, disconnected\", color=red, style=dashed];\n",
            "  \"remote-b\" -> \"remote-e\" [label=\"route\"];\n",
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
