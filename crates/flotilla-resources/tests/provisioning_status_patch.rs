use chrono::Utc;
use flotilla_resources::{
    CheckoutPhase, CheckoutStatus, CheckoutStatusPatch, ClonePhase, CloneStatus, CloneStatusPatch, EnvironmentPhase, EnvironmentStatus,
    EnvironmentStatusPatch, HostStatus, HostStatusPatch, InnerCommandStatus, PresentationPhase, PresentationStatus,
    PresentationStatusPatch, StatusPatch, TaskWorkspacePhase, TaskWorkspaceStatus, TaskWorkspaceStatusPatch, TerminalSessionPhase,
    TerminalSessionStatus, TerminalSessionStatusPatch,
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

    CheckoutStatusPatch::MarkReady { path: "/workspace".to_string(), commit: Some("44982740".to_string()) }.apply(&mut status);
    assert_eq!(status.phase, CheckoutPhase::Ready);
    assert_eq!(status.path.as_deref(), Some("/workspace"));
    assert_eq!(status.commit.as_deref(), Some("44982740"));

    CheckoutStatusPatch::MarkFailed { message: "worktree add failed".to_string() }.apply(&mut status);
    assert_eq!(status.phase, CheckoutPhase::Failed);
}

#[test]
fn terminal_session_status_patch_marks_running_and_stopped() {
    let mut status = TerminalSessionStatus::default();
    let started_at = Utc::now();
    let stopped_at = Utc::now();

    TerminalSessionStatusPatch::MarkRunning { session_id: "abc123".to_string(), pid: Some(12345), started_at }.apply(&mut status);
    assert_eq!(status.phase, TerminalSessionPhase::Running);
    assert_eq!(status.session_id.as_deref(), Some("abc123"));
    assert_eq!(status.pid, Some(12345));

    TerminalSessionStatusPatch::MarkStopped {
        stopped_at,
        inner_command_status: Some(InnerCommandStatus::Exited),
        inner_exit_code: Some(1),
        message: Some("process exited".to_string()),
    }
    .apply(&mut status);
    assert_eq!(status.phase, TerminalSessionPhase::Stopped);
    assert_eq!(status.inner_command_status, Some(InnerCommandStatus::Exited));
    assert_eq!(status.inner_exit_code, Some(1));
}

#[test]
fn task_workspace_status_patch_marks_provisioning_ready_and_failed() {
    let mut status = TaskWorkspaceStatus::default();
    let started_at = Utc::now();
    let ready_at = Utc::now();

    TaskWorkspaceStatusPatch::MarkProvisioning {
        observed_policy_ref: "docker-on-01HXYZ".to_string(),
        observed_policy_version: "12".to_string(),
        started_at,
    }
    .apply(&mut status);
    assert_eq!(status.phase, TaskWorkspacePhase::Provisioning);
    assert_eq!(status.observed_policy_ref.as_deref(), Some("docker-on-01HXYZ"));

    TaskWorkspaceStatusPatch::MarkReady {
        environment_ref: Some("env-a".to_string()),
        checkout_ref: Some("checkout-a".to_string()),
        terminal_session_refs: vec!["term-a".to_string(), "term-b".to_string()],
        ready_at,
    }
    .apply(&mut status);
    assert_eq!(status.phase, TaskWorkspacePhase::Ready);
    assert_eq!(status.terminal_session_refs.len(), 2);

    TaskWorkspaceStatusPatch::MarkFailed { message: "clone failed".to_string() }.apply(&mut status);
    assert_eq!(status.phase, TaskWorkspacePhase::Failed);
    assert_eq!(status.message.as_deref(), Some("clone failed"));
}

#[test]
fn presentation_status_patch_marks_active_torn_down_and_failed() {
    let mut status = PresentationStatus::default();
    let ready_at = Utc::now();

    PresentationStatusPatch::MarkActive {
        presentation_manager: "tmux".to_string(),
        workspace_ref: "ws-123".to_string(),
        spec_hash: "hash-abc".to_string(),
        ready_at,
    }
    .apply(&mut status);
    assert_eq!(status.phase, PresentationPhase::Active);
    assert_eq!(status.observed_presentation_manager.as_deref(), Some("tmux"));
    assert_eq!(status.observed_workspace_ref.as_deref(), Some("ws-123"));
    assert_eq!(status.observed_spec_hash.as_deref(), Some("hash-abc"));
    assert_eq!(status.message, None);

    PresentationStatusPatch::MarkTornDown { message: Some("create failed after replace".to_string()) }.apply(&mut status);
    assert_eq!(status.phase, PresentationPhase::TornDown);
    assert_eq!(status.observed_presentation_manager, None);
    assert_eq!(status.observed_workspace_ref, None);
    assert_eq!(status.observed_spec_hash, None);
    assert_eq!(status.message.as_deref(), Some("create failed after replace"));

    PresentationStatusPatch::MarkFailed { message: "unknown policy".to_string() }.apply(&mut status);
    assert_eq!(status.phase, PresentationPhase::Failed);
    assert_eq!(status.message.as_deref(), Some("unknown policy"));
}
