use chrono::{TimeZone, Utc};
use flotilla_resources::{
    CheckoutBranchProvenance, CheckoutIntegrationStatus, CheckoutPhase, CheckoutStatus, CheckoutStatusPatch, ClonePhase, CloneStatus,
    CloneStatusPatch, ConditionValue, EnvironmentPhase, EnvironmentStatus, EnvironmentStatusPatch, HostStatus, HostStatusPatch,
    InnerCommandStatus, IntegrationCondition, LandedEvidence, PresentationPhase, PresentationStatus, PresentationStatusPatch, Stance,
    StatusPatch, TerminalSessionPhase, TerminalSessionStatus, TerminalSessionStatusPatch, VesselPhase, VesselStatus, VesselStatusPatch,
};

#[test]
fn host_status_patch_updates_heartbeat_snapshot() {
    let mut status = HostStatus::default();
    HostStatusPatch::Heartbeat {
        capabilities: [("docker".to_string(), serde_json::Value::Bool(true))].into_iter().collect(),
        heartbeat_at: Utc::now(),
        ready: true,
    }
    .apply(&mut status);

    assert_eq!(status.capabilities.get("docker"), Some(&serde_json::Value::Bool(true)));
    assert!(status.heartbeat_at.is_some());
    assert!(status.ready);
}

#[test]
fn environment_status_patch_marks_ready_and_failed() {
    let mut status = EnvironmentStatus::default();
    EnvironmentStatusPatch::MarkReady { docker_container_id: Some("container-123".to_string()) }.apply(&mut status);
    assert_eq!(status.phase, EnvironmentPhase::Ready);
    assert!(status.ready);
    assert_eq!(status.docker_container_id.as_deref(), Some("container-123"));

    EnvironmentStatusPatch::MarkFailed { message: "docker run failed".to_string() }.apply(&mut status);
    assert_eq!(status.phase, EnvironmentPhase::Failed);
    assert_eq!(status.message.as_deref(), Some("docker run failed"));
}

#[test]
fn clone_status_patch_marks_cloning_and_ready() {
    let mut status = CloneStatus::default();
    CloneStatusPatch::MarkCloning.apply(&mut status);
    assert_eq!(status.phase, ClonePhase::Cloning);

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
fn checkout_integration_patch_updates_conditions_and_latches_landed() {
    let mut status = CheckoutStatus::default();

    CheckoutStatusPatch::UpdateIntegration {
        integration: CheckoutIntegrationStatus {
            clean: IntegrationCondition::builder().value(ConditionValue::True).build(),
            pushed: IntegrationCondition::builder().value(ConditionValue::False).details(vec!["2 unpushed commits".to_string()]).build(),
            landed: IntegrationCondition::builder().value(ConditionValue::True).build(),
            landed_evidence: Some(
                LandedEvidence::builder().change_request_id("815".to_string()).merged_at("2026-07-21T23:15:00Z".to_string()).build(),
            ),
        },
    }
    .apply(&mut status);

    assert_eq!(status.integration.clean.value, ConditionValue::True);
    assert_eq!(status.integration.pushed.value, ConditionValue::False);
    assert_eq!(status.integration.landed.value, ConditionValue::True);
    assert_eq!(status.integration.landed_evidence.as_ref().map(|evidence| evidence.change_request_id.as_str()), Some("815"));

    CheckoutStatusPatch::UpdateIntegration {
        integration: CheckoutIntegrationStatus {
            clean: IntegrationCondition::builder().value(ConditionValue::True).build(),
            pushed: IntegrationCondition::builder().value(ConditionValue::True).build(),
            landed: IntegrationCondition::builder().value(ConditionValue::False).details(vec!["no PR found".to_string()]).build(),
            landed_evidence: None,
        },
    }
    .apply(&mut status);

    assert_eq!(status.integration.landed.value, ConditionValue::True);
    assert_eq!(status.integration.landed_evidence.as_ref().map(|evidence| evidence.change_request_id.as_str()), Some("815"));
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

    VesselStatusPatch::MarkProvisioning {
        observed_policy_ref: "docker-on-01HXYZ".to_string(),
        observed_policy_version: "12".to_string(),
        started_at,
    }
    .apply(&mut status);
    assert_eq!(status.phase, VesselPhase::Provisioning);
    assert_eq!(status.observed_policy_ref.as_deref(), Some("docker-on-01HXYZ"));

    VesselStatusPatch::MarkProvisioning {
        observed_policy_ref: "docker-on-01HXYZ".to_string(),
        observed_policy_version: "13".to_string(),
        started_at: Utc.timestamp_opt(11, 0).single().expect("timestamp"),
    }
    .apply(&mut status);
    assert_eq!(status.started_at, Some(started_at), "reconcile must not restamp an in-progress transition");

    VesselStatusPatch::MarkReady {
        environment_ref: Some("env-a".to_string()),
        checkout_refs: Default::default(),
        terminal_session_refs: vec!["term-a".to_string(), "term-b".to_string()],
        requested_stance: Stance::WorkspaceWrite,
        effective_stance: Stance::Contained,
        ready_at,
    }
    .apply(&mut status);
    assert_eq!(status.phase, VesselPhase::Ready);
    assert_eq!(status.terminal_session_refs.len(), 2);

    VesselStatusPatch::MarkReady {
        environment_ref: Some("env-a".to_string()),
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
