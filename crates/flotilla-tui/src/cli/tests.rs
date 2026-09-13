use std::collections::HashMap;

use flotilla_protocol::{
    CommandValue, ConvoyExplanation, DaemonEvent, EnvironmentId, ExplainedDecisionLedger, ExplainedMaterialLease, ExplainedSettlement,
    HostName, HostSnapshot, HostSummary, NodeId, NodeInfo, PeerConnectionState, StreamKey,
};

use super::{event_stream_seq, format_command_result, format_convoy_explanation_human, format_event_human};

#[test]
fn convoy_explanation_renders_linked_and_missing_decision_ledgers() {
    let explanation = ConvoyExplanation {
        recent_events: Vec::new(),
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
        material_leases: vec![ExplainedMaterialLease {
            environment: "ledger-work".into(),
            pool_ref: "codex-login".into(),
            state: "waiting".into(),
            reason: Some("all login units are leased".into()),
        }],
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
    assert!(output.contains("ledger-work pool=codex-login state=waiting reason=all login units are leased"));
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
