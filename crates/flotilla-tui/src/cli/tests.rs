use std::collections::{BTreeMap, HashMap};

use flotilla_protocol::{
    CommandValue, ConvoyExplanation, DaemonEvent, EnvironmentId, ExplainedDecisionLedger, ExplainedSettlement, ExplainedUnclaimedWork,
    HostName, HostSnapshot, HostSummary, NodeId, NodeInfo, PeerConnectionState, StreamKey, TopologyResponse, TopologyRoute,
};

use super::{event_stream_seq, format_command_result, format_convoy_explanation_human, format_event_human, format_topology_dot};

#[test]
fn crew_follow_up_result_tells_the_crew_to_complete_again() {
    let output = format_command_result(&CommandValue::CrewFollowUpDelivered);
    assert!(output.contains("Completion received"));
    assert!(output.contains("follow-up brief was delivered"));
    assert!(output.contains("run `flotilla crew complete` again"));
}

#[test]
fn fulfilment_list_distinguishes_image_and_host_model_support() {
    let make = |name: &str, realisation: &str, version: &str, usable: bool| flotilla_protocol::FulfilmentRow {
        name: name.to_string(),
        host_ref: "kiwi".to_string(),
        pool: "cleat".to_string(),
        realisation: realisation.to_string(),
        grants: vec!["platform:linux".to_string()],
        harnesses: BTreeMap::from([("claude-code".to_string(), flotilla_protocol::FulfilmentHarness {
            version: version.to_string(),
            models: BTreeMap::from([("claude-new-model".to_string(), flotilla_protocol::FulfilmentModel {
                usable,
                source: "probe".to_string(),
            })]),
        })]),
        toolchains: BTreeMap::new(),
        gui_session_logged_in: Some(false),
        free_vessel_slots: None,
        image: None,
    };
    let response = flotilla_protocol::FulfilmentListResponse {
        kinds: vec![
            make("docker-crew-image-kiwi", "docker_per_vessel", "2.1.280", false),
            make("host-direct-kiwi", "host_direct", "2.1.282", true),
        ],
    };
    let output = super::format_fulfilment_list_human(&response);
    assert!(output.contains("claude-code 2.1.280: claude-new-model: no (probe)"), "{output}");
    assert!(output.contains("claude-code 2.1.282: claude-new-model: yes (probe)"), "{output}");
}

#[test]
fn convoy_explanation_renders_linked_and_missing_decision_ledgers() {
    let explanation = ConvoyExplanation {
        stalled: None,
        recent_events: Vec::new(),
        lifecycle_mutations: Vec::new(),
        namespace: "flotilla".into(),
        convoy: "ledger".into(),
        phase: "Landing".into(),
        message: Some("waiting for review evidence".into()),
        role_needs: Default::default(),
        allocation: Vec::new(),
        vessel_placements: Default::default(),
        placement: None,
        evidence_ttl_seconds: 30,
        change_request_stale_after_seconds: 30,
        checkouts: Vec::new(),
        change_requests: Vec::new(),
        subscriptions: Vec::new(),
        crew_deliveries: Vec::new(),
        unclaimed_work: vec![ExplainedUnclaimedWork { vessel: "work".into(), role: "coder".into(), evidence: "turn_idle".into() }],
        artifacts: vec![flotilla_protocol::ExplainedArtifact {
            kind: "explainer".into(),
            address: "artifact/example".into(),
            view_url: Some("https://artifacts.example.test/fleet/digest".into()),
        }],
        decision_ledgers: vec![
            ExplainedDecisionLedger {
                vessel: "work".into(),
                role: "coder".into(),
                claimed_at: Some("2026-08-21T11:59:00Z".into()),
                comment_url: Some("https://example.test/pull/1#prior".into()),
                missing: false,
                override_principal: None,
                completed_while_crew_active: false,
                message: Some("first turn complete".into()),
                superseded: true,
            },
            ExplainedDecisionLedger {
                vessel: "work".into(),
                role: "coder".into(),
                claimed_at: Some("2026-08-21T12:00:00Z".into()),
                comment_url: Some("https://example.test/pull/1#comment-2".into()),
                missing: false,
                override_principal: None,
                completed_while_crew_active: false,
                message: None,
                superseded: false,
            },
            ExplainedDecisionLedger {
                vessel: "review".into(),
                role: "reviewer".into(),
                claimed_at: Some("2026-08-21T12:01:00Z".into()),
                comment_url: None,
                missing: true,
                override_principal: None,
                completed_while_crew_active: false,
                message: None,
                superseded: false,
            },
            ExplainedDecisionLedger {
                vessel: "research".into(),
                role: "researcher".into(),
                claimed_at: Some("2026-08-21T12:02:00Z".into()),
                comment_url: None,
                missing: true,
                override_principal: Some(flotilla_protocol::PrincipalRef { namespace: "flotilla".into(), name: "operator".into() }),
                completed_while_crew_active: true,
                message: None,
                superseded: false,
            },
        ],
        settlement: ExplainedSettlement { mode: "world_terminal".into(), satisfied: false, unmet: Vec::new() },
    };

    let output = format_convoy_explanation_human(&explanation);
    assert!(output.contains("Artifacts:\n  - explainer: artifact/example https://artifacts.example.test/fleet/digest"));
    assert!(
        output.contains("superseded claim at=2026-08-21T11:59:00Z comment=https://example.test/pull/1#prior message=first turn complete")
    );
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

#[test]
fn blob_sync_diagnostics_render_in_host_list_and_status() {
    let sync = flotilla_protocol::BlobSyncStatus { pending_count: 2, last_error: Some("endpoint unavailable".to_string()) };
    let fleet = flotilla_protocol::FleetHealthResponse {
        hosts: vec![flotilla_protocol::FleetHostRow::builder()
            .host(HostName::new("local"))
            .is_local(true)
            .configured(false)
            .link(PeerConnectionState::Connected)
            .crew_count(0)
            .convoy_count(0)
            .staleness(flotilla_protocol::FleetHostStaleness::Current)
            .observation_agreement(flotilla_protocol::FleetObservationAgreement::Agree)
            .blob_sync(sync.clone())
            .build()],
        ..Default::default()
    };
    let list = super::format_fleet_health_human(&fleet);
    assert!(list.contains("Blob Sync"));
    assert!(list.contains("2 pending; endpoint unavailable"));

    let status = flotilla_protocol::HostStatusResponse {
        environment_id: EnvironmentId::host(flotilla_protocol::qualified_path::HostId::new("local")),
        host_name: HostName::new("local"),
        node: NodeInfo::new(NodeId::new("local-node"), "Local"),
        is_local: true,
        configured: false,
        connection_status: PeerConnectionState::Connected,
        summary: None,
        visible_environments: Vec::new(),
        repo_count: 0,
        blob_sync: Some(sync),
    };
    let output = super::format_host_status_human(&status);
    assert!(output.contains("Blob sync: 2 pending"));
    assert!(output.contains("Blob sync error: endpoint unavailable"));
}
