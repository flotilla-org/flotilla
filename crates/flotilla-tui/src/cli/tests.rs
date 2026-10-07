use std::collections::{BTreeMap, HashMap};

use flotilla_protocol::{
    commands::{ExplainedSubjectFact, ExplainedSubjectObservation},
    result_set::ConvoySubjectRow,
    CanonicalHostId, CommandValue, ConvoyExplanation, DaemonEvent, EnvironmentId, EvidenceFreshness, ExplainedDecisionLedger,
    ExplainedSettlement, ExplainedUnclaimedWork, FulfilmentAllocation, HostName, HostSnapshot, HostSummary, IssueSource, NodeId, NodeInfo,
    PeerConnectionState, PlacementDecision, PlacementTargetHost, Relationship, StreamKey, Subject, SubjectKind, TopologyResponse,
    TopologyRoute,
};

use super::{event_stream_seq, format_command_result, format_convoy_explanation_human, format_event_human, format_topology_dot};

#[test]
fn project_list_displays_zero_one_and_multiple_resolved_issue_sources() {
    use flotilla_protocol::{IssueSource, ProjectListEntry, ProjectListResponse, ViewAddress};

    let source = IssueSource { service: "https://github.com".into(), scope: "team/one".into() };
    let entry = |name: &str, issue_sources: Vec<IssueSource>| {
        ProjectListEntry::builder()
            .namespace("flotilla".to_string())
            .name(name.to_string())
            .display_name(name.to_string())
            .address(ViewAddress::Project { namespace: "flotilla".into(), name: name.into() })
            .repositories(vec![])
            .issue_sources(issue_sources)
            .default_workflow_ref("single-agent".to_string())
            .build()
    };
    let response = ProjectListResponse {
        projects: vec![
            entry("none", vec![]),
            entry("one", vec![source.clone()]),
            entry("many", vec![source, IssueSource { service: "https://gitlab.com".into(), scope: "team/two".into() }]),
        ],
    };

    let output = super::format_project_list_human(&response);
    assert!(output.lines().any(|line| line.contains("flotilla/none") && line.contains("┆ - ")), "{output}");
    assert!(output.contains("https://github.com / team/one"), "{output}");
    assert!(output.contains("2 sources"), "{output}");
}

#[test]
fn fleet_list_displays_replication_failure_and_last_successful_sync() {
    use chrono::{TimeZone, Utc};
    use flotilla_protocol::{FleetListResponse, FleetReplicaStatus};

    let last_sync = Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 0).single().expect("timestamp");
    let response = FleetListResponse {
        fleet_project: None,
        declaration_attention: Vec::new(),
        rows: vec![],
        replicas: vec![FleetReplicaStatus {
            host: HostName::new("remote"),
            reachable: false,
            last_sync: Some(last_sync),
            generation: Some("generation-1".into()),
            message: Some("resource replication failed: convoys: connection lost".into()),
        }],
    };

    let output = super::format_fleet_list_human(&response);
    assert!(output.contains("resource replication failed: convoys: connection lost"), "{output}");
    assert!(output.contains("2026-09-30T12:00:00+00:00"), "{output}");
    assert!(!output.contains("skipped"), "{output}");
    let json = serde_json::to_value(&response).expect("serialize fleet list");
    assert!(json["replicas"][0].get("skipped_records").is_none());
    assert!(json["replicas"][0].get("first_parse_error").is_none());
}

#[test]
fn provider_lists_show_active_sessions_and_workspaces() {
    use flotilla_protocol::{CliListKind, CliListResponse, CliListRow};

    let row = CliListRow {
        repo: Some("team/repo".into()),
        reference: "session-42".into(),
        name: "Implement feature".into(),
        status: "running".into(),
        provider: Some("Claude".into()),
    };
    let agents = CliListResponse { list_kind: CliListKind::Agent, items: vec![row] };
    let value = CommandValue::CliList(Box::new(agents.clone()));
    let json = serde_json::to_value(&value).expect("serialize list");
    assert_eq!(json["kind"], "cli_list");
    assert_eq!(json["list_kind"], "agent");
    assert_eq!(serde_json::from_value::<CommandValue>(json).expect("decode list"), value);
    let output = format_command_result(&CommandValue::CliList(Box::new(agents)));
    assert!(output.contains("session-42") && output.contains("Implement feature") && output.contains("running"), "{output}");

    let workspaces = CliListResponse { list_kind: CliListKind::Workspace, items: vec![] };
    assert_eq!(format_command_result(&CommandValue::CliList(Box::new(workspaces))), "No active workspaces found.\n");
}

#[test]
fn issue_list_shows_open_issue_details() {
    use chrono::Utc;
    use flotilla_protocol::{issue_query::IssueResultPage, Issue, IssueRef, IssueSource, IssueState};

    let issue = Issue {
        reference: IssueRef { source: IssueSource { service: "https://github.com".into(), scope: "team/repo".into() }, id: "42".into() },
        title: "Repair the parser".into(),
        body: None,
        state: IssueState::Open,
        labels: vec!["bug".into()],
        assignees: vec![],
        as_of: Utc::now(),
        observed_at: None,
        provider_name: "github".into(),
        provider_display_name: "GitHub".into(),
    };
    let page = IssueResultPage { items: vec![issue], total: Some(1), has_more: false };
    let output = format_command_result(&CommandValue::IssuePage(page));
    assert!(output.contains("team/repo#42") && output.contains("Repair the parser") && output.contains("bug"), "{output}");
}

#[test]
fn crew_follow_up_result_tells_the_crew_to_complete_again() {
    let output = format_command_result(&CommandValue::CrewFollowUpDelivered);
    assert!(output.contains("Completion received"));
    assert!(output.contains("follow-up brief was delivered"));
    assert!(output.contains("run `flotilla crew complete` again"));
}

#[test]
fn resource_human_output_names_each_record_origin() {
    use flotilla_protocol::{ResourceCursor, ResourceReadEnvelope, ResourceReadRecord, ResourceRecordProvenance, ResourceRecordType};

    let response = ResourceReadEnvelope {
        api_version: "flotilla.work/v1".to_string(),
        resource_kind: "Convoy".to_string(),
        plural: "convoys".to_string(),
        namespace: "flotilla".to_string(),
        cursor: ResourceCursor::from_position("1", None),
        records: vec![
            ResourceReadRecord {
                record_type: ResourceRecordType::Current,
                provenance: ResourceRecordProvenance::Local { node_id: NodeId::new("kiwi-root") },
                object: Some(serde_json::json!({"metadata": {"name": "local"}})),
            },
            ResourceReadRecord {
                record_type: ResourceRecordType::Current,
                provenance: ResourceRecordProvenance::Replica {
                    origin_root: NodeId::new("feta-root"),
                    last_synced_at: "2026-09-30T00:00:00Z".to_string(),
                },
                object: Some(serde_json::json!({"metadata": {"name": "remote"}})),
            },
        ],
    };
    let output = format_command_result(&CommandValue::ResourceRead(Box::new(response)));
    assert!(output.contains("Convoy/flotilla/local origin: kiwi-root"), "{output}");
    assert!(output.contains("Convoy/flotilla/remote origin: feta-root"), "{output}");
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
        cascade: None,
        environment_observations: Default::default(),
        stalled: None,
        recent_events: Vec::new(),
        lifecycle_mutations: Vec::new(),
        namespace: "flotilla".into(),
        convoy: "ledger".into(),
        phase: "Landing".into(),
        message: Some("waiting for review evidence".into()),
        role_needs: Default::default(),
        skills: BTreeMap::new(),
        allocation: Vec::new(),
        vessel_placements: Default::default(),
        placement: None,
        evidence_ttl_seconds: 30,
        change_request_stale_after_seconds: 30,
        checkouts: Vec::new(),
        subjects: Vec::new(),
        subject_observations: Vec::new(),
        change_requests: Vec::new(),
        subscriptions: Vec::new(),
        crew_deliveries: Vec::new(),
        messages: Vec::new(),
        queued_turns: Vec::new(),
        unclaimed_work: vec![ExplainedUnclaimedWork { vessel: "work".into(), role: "coder".into(), evidence: "turn_idle".into() }],
        artifacts: vec![flotilla_protocol::ExplainedArtifact {
            kind: "explainer".into(),
            address: "artifact/example".into(),
            view_url: Some("https://artifacts.example.test/fleet/digest".into()),
        }],
        decision_ledgers: vec![
            ExplainedDecisionLedger {
                artifact_address: None,
                projection_missing: false,
                projection_error: None,
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
                artifact_address: Some("artifact/ledger".into()),
                projection_missing: false,
                projection_error: None,
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
                artifact_address: None,
                projection_missing: false,
                projection_error: None,
                vessel: "review".into(),
                role: "reviewer".into(),
                claimed_at: Some("2026-08-21T12:01:00Z".into()),
                comment_url: Some("https://example.test/pull/1#orphaned".into()),
                missing: true,
                override_principal: None,
                completed_while_crew_active: false,
                message: None,
                superseded: false,
            },
            ExplainedDecisionLedger {
                artifact_address: None,
                projection_missing: false,
                projection_error: None,
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
    assert!(output
        .contains("work/coder claimed_at=2026-08-21T12:00:00Z artifact=artifact/ledger comment=https://example.test/pull/1#comment-2"));
    assert!(output.contains("review/reviewer claimed_at=2026-08-21T12:01:00Z MISSING (PR comment exists, decision-ledger artifact not found) comment=https://example.test/pull/1#orphaned"));
    assert!(output.contains(
        "research/researcher claimed_at=2026-08-21T12:02:00Z MISSING (completed by flotilla/operator with --force) — completed while crew active"
    ));
    // #2677: the human display names durable artifact evidence with no PR comment;
    // a failed bound projection is a separate flag, never a missing ledger.
    let mut artifact_only = explanation;
    artifact_only.decision_ledgers = vec![artifact_only.decision_ledgers[1].clone()];
    artifact_only.decision_ledgers[0].comment_url = None;
    let output = format_convoy_explanation_human(&artifact_only);
    assert!(output.contains("artifact=artifact/ledger comment=-"));
    assert!(!output.contains("MISSING"));
    artifact_only.decision_ledgers[0].projection_missing = true;
    artifact_only.decision_ledgers[0].projection_error = Some("forge unavailable".into());
    let output = format_convoy_explanation_human(&artifact_only);
    assert!(output.contains("artifact=artifact/ledger comment=-"));
    assert!(output.contains("PR projection MISSING (forge unavailable)"));
    assert!(!output.contains("decision-ledger artifact not found"));
}

#[test]
fn convoy_explanation_shows_reserved_platform_fallback_without_escalation() {
    let explanation = ConvoyExplanation {
        cascade: None,
        environment_observations: Default::default(),
        namespace: "flotilla".into(),
        convoy: "reserved-only".into(),
        phase: "Running".into(),
        stalled: None,
        message: None,
        role_needs: BTreeMap::new(),
        skills: BTreeMap::new(),
        allocation: Vec::new(),
        placement: Some(PlacementDecision {
            policy_name: "macos-scarce".into(),
            target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("comte"), display_name: "comte".into() },
            minimal_alternatives: Vec::new(),
            escalation_reason: None,
            refused_candidates: Vec::new(),
            viable_not_selected: Vec::new(),
            allocation: Some(FulfilmentAllocation {
                chosen_kind: "macos-scarce".into(),
                candidates: Vec::new(),
                reservation_reason: Some("no unreserved capacity covers the needs".into()),
            }),
        }),
        vessel_placements: BTreeMap::new(),
        evidence_ttl_seconds: 30,
        change_request_stale_after_seconds: 30,
        checkouts: Vec::new(),
        subjects: Vec::new(),
        subject_observations: Vec::new(),
        change_requests: Vec::new(),
        subscriptions: Vec::new(),
        crew_deliveries: Vec::new(),
        messages: Vec::new(),
        queued_turns: Vec::new(),
        unclaimed_work: Vec::new(),
        decision_ledgers: Vec::new(),
        artifacts: Vec::new(),
        settlement: ExplainedSettlement { mode: "world_terminal".into(), satisfied: false, unmet: Vec::new() },
        recent_events: Vec::new(),
        lifecycle_mutations: Vec::new(),
    };

    let output = format_convoy_explanation_human(&explanation);
    assert!(output.contains("Fulfilment: macos-scarce on comte"), "{output}");
    assert!(output.contains("Reserved platform capacity used: no unreserved capacity covers the needs"), "{output}");
    assert!(!output.contains("Escalation:"), "{output}");
}

// #2684: human explain must make queued/unsubmitted age and reason visible.
// Glue: a single serialized explanation exercises the display contract.
#[test]
fn convoy_explain_formats_queued_turn_age_and_blocker() {
    let explanation: ConvoyExplanation = serde_json::from_value(serde_json::json!({
        "namespace": "flotilla", "convoy": "queued", "phase": "Active", "evidence_ttl_seconds": 30,
        "change_request_stale_after_seconds": 30, "checkouts": [], "change_requests": [], "subscriptions": [],
        "crew_deliveries": [], "decision_ledgers": [], "settlement": {"mode": "no_exit", "satisfied": false, "unmet": []},
        "queued_turns": [{"source": "review", "subject_revision": "head", "vessel": "work", "role": "coder", "message_id": "turn",
            "rung": "warm-session", "queued_at": "2026-10-05T13:42:35+00:00", "age_seconds": 301,
            "blocking_reason": "attention Unobservable; waiting for turn readiness or submission evidence", "overdue": true}]
    }))
    .unwrap();
    let output = format_convoy_explanation_human(&explanation);
    assert!(output.contains("Queued turns (not submitted):"), "{output}");
    assert!(output.contains("review work/coder revision=head"), "{output}");
    assert!(output.contains("rung=warm-session"), "{output}");
    assert!(output.contains("age=301s OVERDUE: attention Unobservable"), "{output}");
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
            .daemon_rss_bytes(128 * 1024 * 1024)
            .blob_sync(sync.clone())
            .build()],
        ..Default::default()
    };
    let list = super::format_fleet_health_human(&fleet);
    assert!(list.contains("Daemon RSS"));
    assert!(list.contains("128.0 MiB"));
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

// Behaviour: convoy explain keeps the plural layout and shows differing states,
// unknown observations and stale evidence beside the corresponding subject.
// Glue: a single formatting scenario covers the fixed presentation layout.
#[test]
fn convoy_explanation_subject_observations_snapshot() {
    let mut explanation: ConvoyExplanation = serde_json::from_value(serde_json::json!({
        "namespace": "flotilla", "convoy": "plural", "phase": "Landing",
        "evidence_ttl_seconds": 30, "change_request_stale_after_seconds": 60,
        "checkouts": [], "change_requests": [], "subscriptions": [], "crew_deliveries": [],
        "decision_ledgers": [], "settlement": {"mode": "world_terminal", "satisfied": false, "unmet": []}
    }))
    .expect("explanation");
    let fact = |value: Option<&str>, freshness| ExplainedSubjectFact {
        value: value.map(str::to_owned),
        observed_at: (freshness != EvidenceFreshness::Missing).then(|| "2026-10-02T12:00:00Z".into()),
        freshness,
    };
    for (number, state, checks, review, readiness, freshness) in [
        (1, Some("open"), Some("pass"), Some("approved"), "ready_to_merge", EvidenceFreshness::Fresh),
        (2, Some("merged"), Some("pending"), Some("none"), "merged_not_landed", EvidenceFreshness::Stale),
        (3, None, None, None, "awaiting_review_response", EvidenceFreshness::Missing),
        (4, None, None, None, "awaiting_review_response", EvidenceFreshness::Fresh),
    ] {
        let subject = Subject {
            kind: SubjectKind::ChangeRequest,
            source: IssueSource { service: "github.com".into(), scope: "owner/repo".into() },
            id: number.to_string(),
        };
        explanation.subjects.push(ConvoySubjectRow {
            subject: subject.clone(),
            relationship: Relationship::Produces,
            declared: false,
            short: format!("!{number}"),
            url: Some(format!("https://github.com/owner/repo/pull/{number}")),
            repository_key: None,
        });
        explanation.subject_observations.push(ExplainedSubjectObservation {
            subject,
            state: fact(state, freshness),
            checks: fact(checks, freshness),
            review: fact(review, freshness),
            review_actionable_at_head: fact(state.map(|_| "false"), freshness),
            readiness: fact(Some(readiness), freshness),
        });
    }
    // An observation outside the convoy's subject set cannot introduce a subject.
    let mut unrelated = explanation.subject_observations[0].clone();
    unrelated.subject.id = "999".into();
    explanation.subject_observations.insert(0, unrelated);
    let output = format_convoy_explanation_human(&explanation);
    let subjects = output.split("Subjects:\n").nth(1).expect("subjects").split("\nChange requests:").next().expect("section");
    insta::assert_snapshot!(subjects, @r###"
      produces !1, !2, !3, !4
        !1 state=open checks=pass review=approved actionable_at_head=false readiness=ready_to_merge
        !1 https://github.com/owner/repo/pull/1
        !2 state=merged (stale) checks=pending (stale) review=none (stale) actionable_at_head=false (stale) readiness=merged_not_landed (stale)
        !2 https://github.com/owner/repo/pull/2
        !3 state=unknown (missing) checks=unknown (missing) review=unknown (missing) actionable_at_head=unknown (missing) readiness=awaiting_review_response (missing)
        !3 https://github.com/owner/repo/pull/3
        !4 state=unknown checks=unknown review=unknown actionable_at_head=unknown readiness=awaiting_review_response
        !4 https://github.com/owner/repo/pull/4
    "###);
}

// A completion wait must keep the caller pending, state the deadline, and
// retry the original claim only when GitHub's advertised cooldown has elapsed.
#[tokio::test(start_paused = true)]
async fn crew_completion_wait_retries_same_claim_after_deadline() {
    completion_wait_case(false).await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_completion_wait_stops_future_claims() {
    completion_wait_case(true).await;
}

async fn completion_wait_case(cancel: bool) {
    use std::sync::{Arc, Mutex};

    use flotilla_protocol::{Command, CommandAction, CrewCommandContext};

    use crate::app::test_support::StubDaemon;
    let (tx, _) = tokio::sync::broadcast::channel(8);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let daemon = StubDaemon::builder().tx(tx.clone()).execute_calls(calls.clone()).build();
    let command = Command {
        node_id: None,
        provisioning_target: None,
        context_repo: None,
        action: CommandAction::CrewComplete {
            context: CrewCommandContext {
                crew_id: None,
                namespace: Some("flotilla".into()),
                convoy: Some("convoy".into()),
                vessel_ref: Some("vessel".into()),
                role: Some("coder".into()),
            },
            message: Some("PR delivered".into()),
            disposition: None,
            decision_ledger_ref: Some("ledger".into()),
            force: false,
        },
    };
    let mut run = Box::pin(super::run_command(&daemon, command.clone(), super::OutputFormat::Json));
    // Drive subscription/dispatch before emitting the fake daemon event.
    assert!(futures::poll!(&mut run).is_pending());
    let event = |result| DaemonEvent::CommandFinished {
        command_id: 1,
        node_id: NodeId::new("node-local-test"),
        repo_identity: flotilla_protocol::RepoIdentity { authority: "local".into(), path: "".into() },
        repo: None,
        result,
    };
    tx.send(event(CommandValue::CrewCompletionWaiting {
        reason: "GitHub secondary limit".into(),
        retry_at: chrono::Utc::now() + chrono::Duration::seconds(60),
    }))
    .expect("wait event");
    assert!(futures::poll!(&mut run).is_pending());
    tokio::time::advance(std::time::Duration::from_secs(59)).await;
    assert!(futures::poll!(&mut run).is_pending());
    assert_eq!(calls.lock().expect("calls").len(), 1);
    if cancel {
        drop(run);
        tokio::time::advance(std::time::Duration::from_secs(120)).await;
        assert_eq!(calls.lock().expect("calls").len(), 1, "cancelled caller must not submit another claim");
        return;
    }
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert!(futures::poll!(&mut run).is_pending());
    assert_eq!(*calls.lock().expect("calls"), vec![command.clone(), command]);
    tx.send(event(CommandValue::Ok)).expect("ready event");
    assert_eq!(run.await.expect("completed claim"), CommandValue::Ok);
}

// #2498: human output abbreviates Unicode evidence; --full and JSON retain it.
#[test]
fn crew_stalls_format_preserves_full_evidence() {
    let evidence = "限".repeat(161);
    let response = flotilla_protocol::CrewStallsResponse {
        observed_at: chrono::Utc::now(),
        full: false,
        rows: vec![flotilla_protocol::CrewStallRow::builder()
            .namespace("flotilla".into())
            .project("project".into())
            .project_display_name("Project".into())
            .convoy("convoy-id".into())
            .convoy_display_name("implement #2".into())
            .vessel("work".into())
            .role("coder".into())
            .rung(flotilla_protocol::StallRung::Operator)
            .supervisor_absence_reason("no live governor for project".into())
            .age_seconds(300)
            .proposed_disposition(flotilla_protocol::StallProposedDisposition::Resume)
            .evidence(evidence.clone())
            .cause_group("rate-limit".into())
            .shared_cause_count(2)
            .artifacts(vec!["artifact/evidence".into()])
            .build()],
    };
    let human = format_command_result(&CommandValue::CrewStalls(Box::new(response.clone())));
    assert!(human.contains("Project / implement #2"));
    assert!(human.contains("none: no live governor for project"));
    assert!(human.contains(&format!("{}…", "限".repeat(160))));
    assert!(!human.contains(&evidence));
    let json = serde_json::to_value(&response).expect("JSON");
    assert_eq!(json["rows"][0]["evidence"], evidence);
    let full = format_command_result(&CommandValue::CrewStalls(Box::new(flotilla_protocol::CrewStallsResponse { full: true, ..response })));
    assert!(full.contains(&evidence));
    assert!(full.contains("artifact/evidence"));
}

// Glue: the crew query's live charter must be visible in text as well as JSON (#1986).
#[test]
fn crew_list_renders_live_repository_roles() {
    use flotilla_protocol::{CrewListResponse, CrewProject, CrewProjectRepository, ProjectRepositoryRole, RepositoryKey};
    let response = CrewListResponse::builder()
        .convoy("governor".into())
        .vessel("work".into())
        .vessel_ref("vessel".into())
        .members(vec![])
        .project(
            CrewProject::builder()
                .namespace("flotilla".into())
                .name("island".into())
                .display_name("Island".into())
                .repositories(vec![CrewProjectRepository::builder()
                    .key(RepositoryKey("live-repo".into()))
                    .alias("ops".into())
                    .roles([ProjectRepositoryRole::Code, ProjectRepositoryRole::Ops].into())
                    .subpath("part".into())
                    .default_branch("main".into())
                    .remotes(vec!["https://github.com/example/live".into()])
                    .build()])
                .build(),
        )
        .build();
    let output = super::format_crew_list_human(&response);
    assert!(output.contains("Live Project: flotilla/island (Island)"));
    assert!(
        output.contains("live-repo  alias=ops  roles=[code, ops]  subpath=part  branch=main  remotes=[https://github.com/example/live]")
    );
    let json = serde_json::to_value(response).expect("JSON");
    assert_eq!(json["project"]["repositories"][0]["roles"], serde_json::json!(["code", "ops"]));
}

// A missing live charter preserves process state and credential alerts (#2643 review).
#[test]
fn crew_list_renders_charter_error_alongside_state_and_alerts() {
    let response = flotilla_protocol::CrewListResponse::builder()
        .convoy("governor".into())
        .vessel("work".into())
        .vessel_ref("vessel".into())
        .members(vec![flotilla_protocol::CrewListMember::builder()
            .role("governor".into())
            .kind("agent".into())
            .state("active".into())
            .build()])
        .project_error("live Project `island` unavailable".into())
        .credential_alerts(vec!["refresh delayed".into()])
        .build();
    let output = super::format_crew_list_human(&response);
    assert!(output.contains("active"));
    assert!(output.contains("Credential attention: refresh delayed"));
    assert!(output.contains("Live Project unavailable: live Project `island` unavailable"));
    let json = serde_json::to_value(response).expect("JSON");
    assert_eq!(json["project_error"], "live Project `island` unavailable");
    assert!(json.get("project").is_none());
}

// #2718: human output identifies the fleet even when it has no crew sessions,
// and project listings expose its designation and resolved parent.
#[test]
fn fleet_root_is_visible_in_project_and_fleet_lists() {
    use flotilla_protocol::{FleetListResponse, ProjectListEntry, ProjectListResponse, ResourceRef, ViewAddress};
    let response = FleetListResponse {
        fleet_project: Some(ResourceRef::new("flotilla.work/v1", "Project", "flotilla", "project-map")),
        rows: vec![],
        replicas: vec![],
        declaration_attention: vec![],
    };
    assert!(super::format_fleet_list_human(&response).contains("Fleet Project: flotilla/project-map"));
    let entry = |name: &str| {
        ProjectListEntry::builder()
            .namespace("flotilla".into())
            .name(name.into())
            .display_name(name.into())
            .address(ViewAddress::Project { namespace: "flotilla".into(), name: name.into() })
            .repositories(vec![])
            .default_workflow_ref("work".into())
            .build()
    };
    let mut root = entry("project-map");
    root.is_fleet = true;
    let mut child = entry("product");
    child.parent = Some("project-map".into());
    let output = super::format_project_list_human(&ProjectListResponse { projects: vec![root, child] });
    assert!(output.lines().any(|line| line.contains("flotilla/project-map") && line.contains("fleet")), "{output}");
    assert!(output.lines().any(|line| line.contains("flotilla/product") && line.contains("project-map")), "{output}");
}
