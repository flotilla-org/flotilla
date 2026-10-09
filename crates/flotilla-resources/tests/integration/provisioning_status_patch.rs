use chrono::{TimeZone, Utc};
use flotilla_protocol::{CanonicalHostId, PlacementDecision, PlacementTargetHost, SleepInhibitionHealth};
use flotilla_resources::{
    CheckoutBranchProvenance, CheckoutIntegrationStatus, CheckoutPhase, CheckoutStatus, CheckoutStatusPatch, ClonePhase, CloneStatus,
    CloneStatusPatch, ConditionValue, EnvironmentPhase, EnvironmentStatus, EnvironmentStatusPatch, Host, HostCondition, HostSpec,
    HostStatus, HostStatusPatch, InMemoryBackend, InnerCommandStatus, InputMeta, IntegrationCondition, LandedEvidence, ResourceBackend,
    ResourceError, Stance, StatusPatch, TerminalSessionPhase, TerminalSessionStatus, TerminalSessionStatusPatch, VesselPhase, VesselStatus,
    VesselStatusPatch,
};

#[test]
fn host_status_patch_updates_heartbeat_snapshot() {
    // #2729: heartbeat cannot overwrite the image inventory's independent observation.
    let mut status = HostStatus::default();
    let inventory = serde_json::json!([format!("sha256:{}", "3".repeat(64))]);
    status.capabilities.insert(flotilla_resources::IMAGE_DIGESTS_CAPABILITY.into(), inventory.clone());
    let observed_at = Utc.with_ymd_and_hms(2026, 8, 3, 12, 40, 0).single().expect("valid timestamp");
    HostStatusPatch::Heartbeat {
        description: None,
        capabilities: [("docker".to_string(), serde_json::Value::Bool(true))].into_iter().collect(),
        heartbeat_at: Utc::now(),
        ready: true,
        daemon_generation: None,
        protocol_fingerprint: Some("wire-fingerprint".to_string()),
        daemon_version: None,
        daemon_started_at: None,
        disk_free_bytes: None,
        daemon_rss_bytes: Some(123 * 1024 * 1024),
        admission_free_space_floor_bytes: None,
        agent_adapter_baseline: None,
        resource_store: None,
        conditions: Vec::new(),
    }
    .apply(&mut status);

    assert_eq!(status.capabilities.get("docker"), Some(&serde_json::Value::Bool(true)));
    assert_eq!(status.capabilities.get(flotilla_resources::IMAGE_DIGESTS_CAPABILITY), Some(&inventory));
    assert!(status.heartbeat_at.is_some());
    assert!(status.ready);
    // Heartbeat telemetry is in bytes and persists on the host status.
    assert_eq!(status.daemon_rss_bytes, Some(123 * 1024 * 1024));
    // Heartbeats persist wire identity separately from the daemon instance id.
    assert_eq!(status.protocol_fingerprint.as_deref(), Some("wire-fingerprint"));

    HostStatusPatch::SleepInhibition {
        health: SleepInhibitionHealth::Failed { consecutive_failures: 3, message: "polkit denied".to_string() },
        observed_at,
    }
    .apply(&mut status);

    assert_eq!(status.sleep_inhibition, SleepInhibitionHealth::Failed { consecutive_failures: 3, message: "polkit denied".to_string() });
    assert_eq!(status.conditions.len(), 1);
    assert_eq!(status.conditions[0].condition_type, "SleepInhibition");
    assert_eq!(status.conditions[0].reason, "InhibitorNotHeld");
    assert_eq!(status.conditions[0].observed_at, observed_at);
    assert!(status.conditions[0].message.contains("polkit denied"));

    HostStatusPatch::SleepInhibition {
        health: SleepInhibitionHealth::Acquiring { consecutive_failures: 1, message: "polkit denied again".to_string() },
        observed_at: observed_at + chrono::Duration::minutes(1),
    }
    .apply(&mut status);
    assert_eq!(status.conditions.len(), 1, "a retry alone must not clear a persisted failure");

    HostStatusPatch::SleepInhibition { health: SleepInhibitionHealth::Held, observed_at: observed_at + chrono::Duration::minutes(2) }
        .apply(&mut status);
    assert!(status.conditions.is_empty(), "successful acquisition should clear the condition");
}

#[tokio::test]
async fn heartbeat_patch_preserves_independent_status_after_another_writer_updates_it() {
    let observed_at = Utc.with_ymd_and_hms(2026, 9, 30, 12, 40, 0).single().expect("valid timestamp");
    let hosts = ResourceBackend::InMemory(InMemoryBackend::default()).using::<Host>("flotilla");
    let meta = InputMeta::builder().name("host-a".to_string()).build();
    let stale = hosts.create(&meta, &HostSpec::default()).await.expect("create host");
    let sleep_patch = HostStatusPatch::SleepInhibition {
        health: SleepInhibitionHealth::Failed { consecutive_failures: 1, message: "inhibitor unavailable".to_string() },
        observed_at,
    };
    let status = flotilla_resources::apply_status_patch(&hosts, "host-a", &sleep_patch)
        .await
        .expect("concurrent sleep writer")
        .status
        .expect("status");
    let sleep_condition = status.conditions[0].clone();
    assert!(matches!(
        hosts.update_status("host-a", &stale.metadata.resource_version, &HostStatus::default()).await,
        Err(ResourceError::Conflict { .. })
    ));
    let heartbeat_condition = HostCondition::builder()
        .condition_type("ResourceStore/DecodeQuarantine")
        .value(ConditionValue::False)
        .reason("DecodeFailed")
        .message("one record failed to decode")
        .observed_at(observed_at)
        .build();
    let patch = HostStatusPatch::Heartbeat {
        description: None,
        capabilities: Default::default(),
        heartbeat_at: observed_at,
        ready: true,
        daemon_generation: None,
        protocol_fingerprint: None,
        daemon_version: None,
        daemon_started_at: None,
        disk_free_bytes: None,
        daemon_rss_bytes: None,
        admission_free_space_floor_bytes: None,
        agent_adapter_baseline: Some(["codex".to_string()].into()),
        resource_store: None,
        conditions: vec![heartbeat_condition.clone()],
    };
    let status = flotilla_resources::apply_status_patch(&hosts, "host-a", &patch)
        .await
        .expect("heartbeat applies to current status")
        .status
        .expect("status");

    assert_eq!(status.conditions, vec![heartbeat_condition, sleep_condition]);
    assert!(!status.ready, "concurrent sleep-inhibition failure still blocks readiness");
    assert_eq!(status.agent_adapter_baseline, Some(["codex".to_string()].into()));
    assert!(matches!(status.sleep_inhibition, SleepInhibitionHealth::Failed { .. }));
}

#[test]
fn environment_status_patch_marks_ready_and_failed() {
    let mut status = EnvironmentStatus::default();
    EnvironmentStatusPatch::MarkReady {
        configured_limits: None,
        docker_container_id: Some("container-123".to_string()),
        image_ref: Some("registry.example/crew:latest".to_string()),
        local_image_id: Some("sha256:first".to_string()),
        registry_digest: None,
    }
    .apply(&mut status);
    assert_eq!(status.phase, EnvironmentPhase::Ready);
    assert!(status.ready);
    assert_eq!(status.docker_container_id.as_deref(), Some("container-123"));
    assert_eq!(status.image_ref.as_deref(), Some("registry.example/crew:latest"));
    assert_eq!(status.local_image_id.as_deref(), Some("sha256:first"));

    EnvironmentStatusPatch::MarkFailed { message: "docker run failed".to_string() }.apply(&mut status);
    assert_eq!(status.phase, EnvironmentPhase::Failed);
    assert_eq!(status.message.as_deref(), Some("docker run failed"));
}

#[test]
fn clone_status_patch_marks_cloning_and_ready() {
    let mut status = CloneStatus::default();
    CloneStatusPatch::MarkCloning.apply(&mut status);
    assert_eq!(status.phase, ClonePhase::Cloning);

    CloneStatusPatch::MarkRetrying { message: "clone interrupted".to_string() }.apply(&mut status);
    assert_eq!(status.phase, ClonePhase::Cloning);
    assert_eq!(status.message.as_deref(), Some("clone interrupted"));

    CloneStatusPatch::MarkReady { default_branch: Some("main".to_string()) }.apply(&mut status);
    assert_eq!(status.phase, ClonePhase::Ready);
    assert_eq!(status.default_branch.as_deref(), Some("main"));
}

#[test]
fn checkout_status_patch_marks_ready_and_failed() {
    let mut status = CheckoutStatus::default();

    CheckoutStatusPatch::MarkReady {
        path: "/workspace".to_string(),
        commit: Some("44982740".to_string()),
        branch_provenance: CheckoutBranchProvenance::CreatedForConvoy,
    }
    .apply(&mut status);
    assert_eq!(status.phase, CheckoutPhase::Ready);
    assert_eq!(status.path.as_deref(), Some("/workspace"));
    assert_eq!(status.commit.as_deref(), Some("44982740"));
    assert_eq!(status.branch_provenance, CheckoutBranchProvenance::CreatedForConvoy);

    CheckoutStatusPatch::MarkFailed { message: "worktree add failed".to_string() }.apply(&mut status);
    assert_eq!(status.phase, CheckoutPhase::Failed);
}

#[test]
fn checkout_integration_patch_replaces_conditions_without_latching() {
    let mut status = CheckoutStatus::default();

    CheckoutStatusPatch::UpdateIntegration {
        integration: Box::new(CheckoutIntegrationStatus {
            head_revision: None,
            clean: IntegrationCondition::builder().value(ConditionValue::True).build(),
            pushed: IntegrationCondition::builder().value(ConditionValue::False).details(vec!["2 unpushed commits".to_string()]).build(),
            landed: IntegrationCondition::builder().value(ConditionValue::True).build(),
            landed_evidence: Some(
                LandedEvidence::builder().change_request_id("815".to_string()).merged_at("2026-07-21T23:15:00Z".to_string()).build(),
            ),
            change_request: None,
            remote_refs: Default::default(),
        }),
    }
    .apply(&mut status);

    assert_eq!(status.integration.clean.value, ConditionValue::True);
    assert_eq!(status.integration.pushed.value, ConditionValue::False);
    assert_eq!(status.integration.landed.value, ConditionValue::True);
    assert_eq!(status.integration.landed_evidence.as_ref().map(|evidence| evidence.change_request_id.as_str()), Some("815"));

    CheckoutStatusPatch::UpdateIntegration {
        integration: Box::new(CheckoutIntegrationStatus {
            head_revision: None,
            clean: IntegrationCondition::builder().value(ConditionValue::True).build(),
            pushed: IntegrationCondition::builder().value(ConditionValue::True).build(),
            landed: IntegrationCondition::builder().value(ConditionValue::False).details(vec!["no PR found".to_string()]).build(),
            landed_evidence: None,
            change_request: None,
            remote_refs: Default::default(),
        }),
    }
    .apply(&mut status);

    assert_eq!(status.integration.landed.value, ConditionValue::False);
    assert_eq!(status.integration.landed_evidence, None);
}

#[test]
fn checkout_integration_patch_absorbs_unknown_with_landed_evidence() {
    let evidence = LandedEvidence::builder().change_request_id("815".to_string()).build();
    let mut status = CheckoutStatus {
        integration: CheckoutIntegrationStatus {
            landed: IntegrationCondition::builder().value(ConditionValue::True).build(),
            landed_evidence: Some(evidence.clone()),
            ..Default::default()
        },
        ..Default::default()
    };

    CheckoutStatusPatch::UpdateIntegration {
        integration: Box::new(CheckoutIntegrationStatus {
            landed: IntegrationCondition::builder().value(ConditionValue::Unknown).build(),
            ..Default::default()
        }),
    }
    .apply(&mut status);

    assert_eq!(status.integration.landed.value, ConditionValue::True);
    assert_eq!(status.integration.landed_evidence, Some(evidence));
}

#[test]
fn terminal_session_status_patch_marks_running_and_stopped() {
    let mut status = TerminalSessionStatus::default();
    let started_at = Utc.timestamp_opt(10, 0).single().expect("timestamp");
    let stopped_at = Utc.timestamp_opt(20, 0).single().expect("timestamp");

    TerminalSessionStatusPatch::MarkRunning {
        configured_limits: None,
        session_id: "abc123".to_string(),
        pid: Some(12345),
        started_at,
        crew: None,
        launch_command: "bash".to_string(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    TerminalSessionStatusPatch::MarkRunning {
        configured_limits: None,
        session_id: "abc123".to_string(),
        pid: Some(12345),
        started_at: Utc.timestamp_opt(11, 0).single().expect("timestamp"),
        crew: None,
        launch_command: "bash".to_string(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    assert_eq!(status.phase, TerminalSessionPhase::Running);
    assert_eq!(status.session_id.as_deref(), Some("abc123"));
    assert_eq!(status.pid, Some(12345));
    assert_eq!(status.started_at, Some(started_at));

    TerminalSessionStatusPatch::MarkStopped {
        stopped_at,
        inner_command_status: Some(InnerCommandStatus::Exited),
        inner_exit_code: Some(1),
        message: Some("process exited".to_string()),
    }
    .apply(&mut status);
    TerminalSessionStatusPatch::MarkStopped {
        stopped_at: Utc.timestamp_opt(21, 0).single().expect("timestamp"),
        inner_command_status: Some(InnerCommandStatus::Exited),
        inner_exit_code: Some(1),
        message: Some("process exited".to_string()),
    }
    .apply(&mut status);
    assert_eq!(status.phase, TerminalSessionPhase::Stopped);
    assert_eq!(status.inner_command_status, Some(InnerCommandStatus::Exited));
    assert_eq!(status.inner_exit_code, Some(1));
    assert_eq!(status.stopped_at, Some(stopped_at));

    TerminalSessionStatusPatch::MarkFailed { message: "failed after stop".to_string(), stopped_at: None }.apply(&mut status);
    assert_eq!(status.phase, TerminalSessionPhase::Failed);
    assert_eq!(status.stopped_at, Some(stopped_at), "a later patch without a timestamp must not erase the transition time");
}

#[test]
fn terminal_session_failure_is_distinct_from_a_stopped_crew_member_and_can_restart() {
    let mut status = TerminalSessionStatus::default();

    TerminalSessionStatusPatch::MarkFailed { message: "unknown agent capability `architect`".to_string(), stopped_at: Some(Utc::now()) }
        .apply(&mut status);
    assert_eq!(status.phase, TerminalSessionPhase::Failed);
    assert_eq!(status.message.as_deref(), Some("unknown agent capability `architect`"));

    TerminalSessionStatusPatch::MarkStarting.apply(&mut status);
    assert_eq!(status.phase, TerminalSessionPhase::Starting);
    assert_eq!(status.session_id, None);
    assert_eq!(status.started_at, None);
    assert_eq!(status.stopped_at, None);
    assert_eq!(status.message, None);
}

#[test]
fn vessel_status_patch_marks_provisioning_ready_and_failed() {
    let mut status = VesselStatus::default();
    let started_at = Utc.timestamp_opt(10, 0).single().expect("timestamp");
    let ready_at = Utc.timestamp_opt(20, 0).single().expect("timestamp");
    let placement_decision = PlacementDecision {
        minimal_alternatives: Vec::new(),
        escalation_reason: None,
        policy_name: "docker-on-01HXYZ".to_string(),
        target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("01HXYZ"), display_name: "kiwi".to_string() },
        refused_candidates: Vec::new(),
        viable_not_selected: Vec::new(),
        allocation: None,
    };

    VesselStatusPatch::MarkProvisioning {
        placement_decision: Some(placement_decision.clone()),
        observed_policy_ref: "docker-on-01HXYZ".to_string(),
        observed_policy_version: "12".to_string(),
        started_at,
        message: None,
    }
    .apply(&mut status);
    assert_eq!(status.phase, VesselPhase::Provisioning);
    assert_eq!(status.observed_policy_ref.as_deref(), Some("docker-on-01HXYZ"));
    assert_eq!(status.placement_decision.as_ref(), Some(&placement_decision));

    VesselStatusPatch::MarkProvisioning {
        placement_decision: Some(PlacementDecision {
            minimal_alternatives: Vec::new(),
            escalation_reason: None,
            policy_name: "replacement-must-not-win".to_string(),
            target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("other"), display_name: "feta".to_string() },
            refused_candidates: Vec::new(),
            viable_not_selected: Vec::new(),
            allocation: None,
        }),
        observed_policy_ref: "docker-on-01HXYZ".to_string(),
        observed_policy_version: "13".to_string(),
        started_at: Utc.timestamp_opt(11, 0).single().expect("timestamp"),
        message: Some("still waiting".to_string()),
    }
    .apply(&mut status);
    assert_eq!(status.started_at, Some(started_at), "reconcile must not restamp an in-progress transition");
    assert_eq!(status.message.as_deref(), Some("still waiting"));
    assert_eq!(status.placement_decision.as_ref(), Some(&placement_decision), "placement decision is write-once");

    VesselStatusPatch::MarkReady {
        configured_limits: None,
        placement_decision: None,
        environment_ref: Some("env-a".to_string()),
        image_ref: Some("registry.example/crew:latest".to_string()),
        local_image_id: Some("sha256:test-image".to_string()),
        registry_digest: None,
        checkout_refs: Default::default(),
        terminal_session_refs: vec!["term-a".to_string(), "term-b".to_string()],
        requested_stance: Stance::WorkspaceWrite,
        effective_stance: Stance::Contained,
        ready_at,
    }
    .apply(&mut status);
    assert_eq!(status.phase, VesselPhase::Ready);
    assert_eq!(status.terminal_session_refs.len(), 2);
    assert_eq!(status.image_ref.as_deref(), Some("registry.example/crew:latest"));
    assert_eq!(status.local_image_id.as_deref(), Some("sha256:test-image"));

    VesselStatusPatch::MarkReady {
        configured_limits: None,
        placement_decision: None,
        environment_ref: Some("env-a".to_string()),
        image_ref: Some("registry.example/crew:latest".to_string()),
        local_image_id: Some("sha256:test-image".to_string()),
        registry_digest: None,
        checkout_refs: Default::default(),
        terminal_session_refs: vec!["term-a".to_string(), "term-b".to_string()],
        requested_stance: Stance::WorkspaceWrite,
        effective_stance: Stance::Contained,
        ready_at: Utc.timestamp_opt(21, 0).single().expect("timestamp"),
    }
    .apply(&mut status);
    assert_eq!(status.ready_at, Some(ready_at), "reconcile must not restamp an established Ready transition");
    assert_eq!(status.requested_stance, Some(Stance::WorkspaceWrite));
    assert_eq!(status.effective_stance, Some(Stance::Contained));

    VesselStatusPatch::MarkFailed { message: "clone failed".to_string() }.apply(&mut status);
    assert_eq!(status.phase, VesselPhase::Failed);
    assert_eq!(status.message.as_deref(), Some("clone failed"));
}

// #1496: heartbeat descriptions persist with ADR 0047 defaults. Placement keys
// supply availability; the stored description must not double-bookkeep it.
#[hegel::test]
fn heartbeat_description_has_one_availability_authority(tc: hegel::TestCase) {
    use flotilla_protocol::{qualified_path::HostId, EnvironmentId, HostProviderStatus, HostSummary, NodeId, NodeInfo, SystemInfo};
    use hegel::generators as gs;
    // Include absent/empty capability sets and both availability categories.
    let adapter = tc.draw(gs::booleans());
    let pool = tc.draw(gs::booleans());
    let cpu = tc.draw(gs::integers::<u16>().min_value(0).max_value(u16::MAX));
    let summary = HostSummary::builder()
        .environment_id(EnvironmentId::host(HostId::new("host")))
        .node(NodeInfo::new(NodeId::new("node"), "host"))
        .system(SystemInfo { cpu_count: Some(cpu), ..Default::default() })
        .providers(vec![
            HostProviderStatus::available("agent_adapter", "stale-adapter"),
            HostProviderStatus::available("terminal_pool", "stale-pool"),
            HostProviderStatus::disabled("vcs", "git", "missing binary"),
        ])
        .build();
    let mut capabilities = std::collections::BTreeMap::new();
    if adapter {
        capabilities.insert("agent_adapters".into(), serde_json::json!(["codex"]));
    }
    if pool {
        capabilities.insert("terminal_pools".into(), serde_json::json!(["cleat"]));
    }
    let patch = HostStatusPatch::Heartbeat {
        description: Some(Box::new(summary.clone())),
        capabilities,
        heartbeat_at: Utc::now(),
        ready: true,
        daemon_generation: None,
        protocol_fingerprint: None,
        daemon_version: None,
        daemon_started_at: None,
        disk_free_bytes: None,
        daemon_rss_bytes: Some(u64::MAX),
        admission_free_space_floor_bytes: None,
        agent_adapter_baseline: None,
        resource_store: None,
        conditions: vec![],
    };
    let mut status = HostStatus::default();
    patch.apply(&mut status);
    let stored = status.description.as_ref().expect("description");
    assert_eq!(stored.system, summary.system);
    assert_eq!(stored.providers, vec![summary.providers[2].clone()]);
    let mut expected = stored.clone();
    if adapter {
        expected.providers.push(HostProviderStatus::available("agent_adapter", "codex"));
    }
    if pool {
        expected.providers.push(HostProviderStatus::available("terminal_pool", "cleat"));
    }
    expected.providers.sort_by(|a, b| (&a.category, &a.name).cmp(&(&b.category, &b.name)));
    assert_eq!(status.host_summary(), Some(expected));
    assert_eq!(status.agent_adapters().expect("adapters"), if adapter { ["codex".into()].into() } else { Default::default() });
    let decoded: HostStatus = serde_json::from_value(serde_json::to_value(&status).expect("encode status")).expect("decode status");
    assert_eq!(decoded, status);
    // Previous-generation stored records have no descriptive field.
    let mut previous = serde_json::to_value(&status).expect("encode previous generation");
    previous.as_object_mut().expect("object").remove("description");
    let previous: HostStatus = serde_json::from_value(previous).expect("previous generation still decodes");
    assert!(previous.description.is_none());
    assert_eq!(previous.capabilities, status.capabilities);
    assert_eq!(previous.daemon_rss_bytes, status.daemon_rss_bytes);
}

// ADR 0047: previous-generation statuses lack configured limits and must decode
// as unknown. Exercise each stored owner without changing the golden corpus.
#[test]
fn previous_generation_statuses_decode_without_configured_limits() {
    let environment: EnvironmentStatus =
        serde_json::from_value(serde_json::json!({"phase": "Ready"})).expect("previous-generation status decodes");
    let vessel: VesselStatus = serde_json::from_value(serde_json::json!({"phase": "Ready"})).expect("previous-generation status decodes");
    let terminal: TerminalSessionStatus =
        serde_json::from_value(serde_json::json!({"phase": "Running"})).expect("previous-generation status decodes");
    assert_eq!(environment.configured_limits, None);
    assert_eq!(vessel.configured_limits, None);
    assert_eq!(terminal.configured_limits, None);
}
