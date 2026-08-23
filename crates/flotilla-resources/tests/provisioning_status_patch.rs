use chrono::{TimeZone, Utc};
use flotilla_protocol::{CanonicalHostId, PlacementDecision, PlacementTargetHost, SleepInhibitionHealth};
use flotilla_resources::{
    CheckoutBranchProvenance, CheckoutIntegrationStatus, CheckoutPhase, CheckoutStatus, CheckoutStatusPatch, ClonePhase, CloneStatus,
    CloneStatusPatch, ConditionValue, EnvironmentPhase, EnvironmentStatus, EnvironmentStatusPatch, HostStatus, HostStatusPatch,
    InnerCommandStatus, IntegrationCondition, LandedEvidence, PresentationPhase, PresentationStatus, PresentationStatusPatch, Stance,
    StatusPatch, TerminalSessionPhase, TerminalSessionStatus, TerminalSessionStatusPatch, VesselPhase, VesselStatus, VesselStatusPatch,
};

#[test]
fn host_status_patch_updates_heartbeat_snapshot() {
    let mut status = HostStatus::default();
    let observed_at = Utc.with_ymd_and_hms(2026, 8, 3, 12, 40, 0).single().expect("valid timestamp");
    HostStatusPatch::Heartbeat {
        capabilities: [("docker".to_string(), serde_json::Value::Bool(true))].into_iter().collect(),
        heartbeat_at: Utc::now(),
        ready: true,
        daemon_generation: None,
        daemon_version: None,
        daemon_started_at: None,
        disk_free_bytes: None,
        admission_free_space_floor_bytes: None,
    }
    .apply(&mut status);

    assert_eq!(status.capabilities.get("docker"), Some(&serde_json::Value::Bool(true)));
    assert!(status.heartbeat_at.is_some());
    assert!(status.ready);

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

#[test]
fn environment_status_patch_marks_ready_and_failed() {
    let mut status = EnvironmentStatus::default();
    EnvironmentStatusPatch::MarkReady {
        docker_container_id: Some("container-123".to_string()),
        image_ref: Some("registry.example/crew:latest".to_string()),
        image_digest: Some("sha256:first".to_string()),
    }
    .apply(&mut status);
    assert_eq!(status.phase, EnvironmentPhase::Ready);
    assert!(status.ready);
    assert_eq!(status.docker_container_id.as_deref(), Some("container-123"));
    assert_eq!(status.image_ref.as_deref(), Some("registry.example/crew:latest"));
    assert_eq!(status.image_digest.as_deref(), Some("sha256:first"));

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
    CheckoutStatusPatch::MarkPreparing.apply(&mut status);
    assert_eq!(status.phase, CheckoutPhase::Preparing);

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
            clean: IntegrationCondition::builder().value(ConditionValue::True).build(),
            pushed: IntegrationCondition::builder().value(ConditionValue::False).details(vec!["2 unpushed commits".to_string()]).build(),
            landed: IntegrationCondition::builder().value(ConditionValue::True).build(),
            landed_evidence: Some(
                LandedEvidence::builder().change_request_id("815".to_string()).merged_at("2026-07-21T23:15:00Z".to_string()).build(),
            ),
            change_request: None,
        }),
    }
    .apply(&mut status);

    assert_eq!(status.integration.clean.value, ConditionValue::True);
    assert_eq!(status.integration.pushed.value, ConditionValue::False);
    assert_eq!(status.integration.landed.value, ConditionValue::True);
    assert_eq!(status.integration.landed_evidence.as_ref().map(|evidence| evidence.change_request_id.as_str()), Some("815"));

    CheckoutStatusPatch::UpdateIntegration {
        integration: Box::new(CheckoutIntegrationStatus {
            clean: IntegrationCondition::builder().value(ConditionValue::True).build(),
            pushed: IntegrationCondition::builder().value(ConditionValue::True).build(),
            landed: IntegrationCondition::builder().value(ConditionValue::False).details(vec!["no PR found".to_string()]).build(),
            landed_evidence: None,
            change_request: None,
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
        session_id: "abc123".to_string(),
        pid: Some(12345),
        started_at,
        crew: None,
        launch_command: "bash".to_string(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    TerminalSessionStatusPatch::MarkRunning {
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
        policy_name: "docker-on-01HXYZ".to_string(),
        target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("01HXYZ"), display_name: "kiwi".to_string() },
        refused_candidates: Vec::new(),
        viable_not_selected: Vec::new(),
    };

    VesselStatusPatch::MarkProvisioning {
        placement_decision: Some(placement_decision.clone()),
        observed_policy_ref: "docker-on-01HXYZ".to_string(),
        observed_policy_version: "12".to_string(),
        started_at,
        message: None,
        wait_reason: Some(flotilla_resources::EnvironmentWaitReason::MaterialPoolExhausted { pool_ref: "codex-login".to_string() }),
    }
    .apply(&mut status);
    assert_eq!(status.phase, VesselPhase::Provisioning);
    assert_eq!(status.observed_policy_ref.as_deref(), Some("docker-on-01HXYZ"));
    assert_eq!(status.placement_decision.as_ref(), Some(&placement_decision));
    assert!(matches!(
        status.wait_reason,
        Some(flotilla_resources::EnvironmentWaitReason::MaterialPoolExhausted { ref pool_ref }) if pool_ref == "codex-login"
    ));

    VesselStatusPatch::MarkProvisioning {
        placement_decision: Some(PlacementDecision {
            policy_name: "replacement-must-not-win".to_string(),
            target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("other"), display_name: "feta".to_string() },
            refused_candidates: Vec::new(),
            viable_not_selected: Vec::new(),
        }),
        observed_policy_ref: "docker-on-01HXYZ".to_string(),
        observed_policy_version: "13".to_string(),
        started_at: Utc.timestamp_opt(11, 0).single().expect("timestamp"),
        wait_reason: None,
        message: Some("still waiting".to_string()),
    }
    .apply(&mut status);
    assert_eq!(status.started_at, Some(started_at), "reconcile must not restamp an in-progress transition");
    assert_eq!(status.message.as_deref(), Some("still waiting"));
    assert_eq!(status.placement_decision.as_ref(), Some(&placement_decision), "placement decision is write-once");

    VesselStatusPatch::MarkReady {
        placement_decision: None,
        environment_ref: Some("env-a".to_string()),
        image_ref: Some("registry.example/crew:latest".to_string()),
        image_digest: Some("sha256:test-image".to_string()),
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
    assert_eq!(status.image_digest.as_deref(), Some("sha256:test-image"));

    VesselStatusPatch::MarkReady {
        placement_decision: None,
        environment_ref: Some("env-a".to_string()),
        image_ref: Some("registry.example/crew:latest".to_string()),
        image_digest: Some("sha256:test-image".to_string()),
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

#[test]
fn presentation_status_patch_marks_active_torn_down_and_failed() {
    let mut status = PresentationStatus::default();
    let ready_at = Utc.timestamp_opt(10, 0).single().expect("timestamp");

    PresentationStatusPatch::MarkActive {
        presentation_manager: "tmux".to_string(),
        workspace_ref: "ws-123".to_string(),
        spec_hash: "hash-abc".to_string(),
        ready_at,
    }
    .apply(&mut status);
    PresentationStatusPatch::MarkActive {
        presentation_manager: "tmux".to_string(),
        workspace_ref: "ws-456".to_string(),
        spec_hash: "hash-def".to_string(),
        ready_at: Utc.timestamp_opt(11, 0).single().expect("timestamp"),
    }
    .apply(&mut status);
    assert_eq!(status.phase, PresentationPhase::Active);
    assert_eq!(status.observed_presentation_manager.as_deref(), Some("tmux"));
    assert_eq!(status.observed_workspace_ref.as_deref(), Some("ws-456"));
    assert_eq!(status.observed_spec_hash.as_deref(), Some("hash-def"));
    assert_eq!(status.ready_at, Some(ready_at), "an in-place presentation refresh is not a new Active transition");
    assert_eq!(status.message, None);

    PresentationStatusPatch::MarkTornDown { message: Some("create failed after replace".to_string()) }.apply(&mut status);
    assert_eq!(status.phase, PresentationPhase::TornDown);
    assert_eq!(status.observed_presentation_manager, None);
    assert_eq!(status.observed_workspace_ref, None);
    assert_eq!(status.observed_spec_hash, None);
    assert_eq!(status.message.as_deref(), Some("create failed after replace"));

    let reactivated_at = Utc.timestamp_opt(12, 0).single().expect("timestamp");
    PresentationStatusPatch::MarkActive {
        presentation_manager: "tmux".to_string(),
        workspace_ref: "ws-789".to_string(),
        spec_hash: "hash-ghi".to_string(),
        ready_at: reactivated_at,
    }
    .apply(&mut status);
    assert_eq!(status.ready_at, Some(reactivated_at), "TornDown to Active is a new transition");

    PresentationStatusPatch::MarkFailed { message: "unknown policy".to_string() }.apply(&mut status);
    assert_eq!(status.phase, PresentationPhase::Failed);
    assert_eq!(status.message.as_deref(), Some("unknown policy"));
}
