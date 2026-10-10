use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_protocol::HostName;
use flotilla_resources::{
    external_patches as convoy_external_patches, Convoy as ResourceConvoy, ConvoySpec, ConvoyStatus, CrewWorkPhase, CrewWorkState,
    InputMeta, TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalSession as ResourceTerminalSession,
    TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource, TerminalSessionStatus as ResourceTerminalSessionStatus,
    CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
};
use flotilla_store::{apply_status_patch as apply_resource_status_patch, ResourceBackend};

use super::support::{resume_staging_fixture, resume_staging_fixture_with_backend, test_meta};
use crate::config::ConfigStore;
use crate::in_process::crew_ops::{ConvoyResumeOutcome, CrewSupervisionRequest};
use crate::in_process::{input_meta_from_resource, InProcessDaemon};
use crate::testkits::discovery::fake_discovery;

// Working crews already own staged credentials. Operator input is admitted as
// durable intent; ordinary attention changes cannot fabricate its delivery receipt.
#[tokio::test]
async fn working_resume_uses_message_delivery_instead_of_pending_brief_release() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let current = convoys.get("resume-staging").await.expect("convoy");
    let mut status = current.status.expect("status");
    status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("coder").phase = CrewWorkPhase::Working;
    convoys.update_status(&current.metadata.name, &current.metadata.resource_version, &status).await.expect("working crew");
    daemon
        .convoy_resume_internal("flotilla", "resume-staging", "operator guidance", Some("work"), Some("coder"))
        .await
        .expect("queue intent");
    daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect("migration scan");
    assert!(convoys.get("resume-staging").await.expect("convoy").status.expect("status").pending_brief().is_none());
    let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].spec.body, "operator guidance");
    assert_eq!(records[0].status.as_ref().expect("status").phase, flotilla_resources::MessagePhase::Accepted);
    assert!(records[0].status.as_ref().expect("status").resolved_receiver.is_none());
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn resume_stages_credentials_before_message_and_retries_failure() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    let resume = || daemon.convoy_resume_internal("flotilla", "resume-staging", "continue", Some("work"), Some("coder"));
    assert!(resume().await.expect_err("staging should fail").contains("credential staging failed"));
    assert_eq!(
        convoys.get("resume-staging").await.expect("convoy").status.expect("status").crew_work["work"]["coder"].phase,
        CrewWorkPhase::Done
    );
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 0);
    let TerminalSessionSource::Agent { message, .. } = sessions.get("resume-staging-session").await.expect("session").spec.source else {
        panic!("agent")
    };
    assert!(message.is_none());
    resume().await.expect("retry resume");
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
    let TerminalSessionSource::Agent { message, .. } = sessions.get("resume-staging-session").await.expect("session").spec.source else {
        panic!("agent")
    };
    assert!(message.is_none());
    let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
    assert_eq!(records.len(), 1);
    // Native Message attribution is separate from its unframed body.
    assert_eq!(records[0].spec.body, "continue");
    assert_eq!(records[0].spec.sender, "principal:implicit");
}

#[tokio::test]
async fn resume_retries_a_racing_session_write_after_staging() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    probe.invalidate_next.store(true, std::sync::atomic::Ordering::SeqCst);
    let outcome = daemon
        .convoy_resume_internal("flotilla", "resume-staging", "continue", Some("work"), Some("coder"))
        .await
        .expect("stale session write retried");
    assert_eq!(outcome, ConvoyResumeOutcome::Queued { displaced: None });
    let status = backend.using::<ResourceConvoy>("flotilla").get("resume-staging").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
    let session = backend.using::<ResourceTerminalSession>("flotilla").get("resume-staging-session").await.expect("session");
    assert!(matches!(session.spec.source, TerminalSessionSource::Agent { message: None, .. }));
    assert!(backend
        .using::<flotilla_resources::Message>("flotilla")
        .list()
        .await
        .expect("inbox")
        .items
        .iter()
        .any(|message| message.spec.body.contains("continue")));
}

#[tokio::test]
async fn declared_access_stall_routes_to_project_governor_and_resumes() {
    #[derive(Default)]
    struct AcceptSupervision {
        requests: std::sync::Mutex<Vec<crate::leaf_engine::CrewTurnIntent>>,
    }
    #[async_trait]
    impl crate::leaf_engine::TurnDeliveryActuator for AcceptSupervision {
        async fn deliver(&self, request: &crate::leaf_engine::CrewTurnIntent) -> Result<crate::leaf_engine::CrewTurnAdmission, String> {
            self.requests.lock().expect("supervision requests").push(request.clone());
            Ok(crate::leaf_engine::CrewTurnAdmission {
                new_turn: true,
                rung: flotilla_resources::TurnDeliveryRung::WarmSession,
                message: flotilla_protocol::ResourceRef::new(
                    "flotilla.work/v1",
                    "Message",
                    &request.namespace,
                    format!("fake-{}-{}", request.source, request.subject_revision),
                ),
            })
        }
        async fn hold(
            &self,
            _request: &crate::leaf_engine::CrewTurnIntent,
            _act: &flotilla_resources::HoldAct,
            _reason: &str,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    let database_dir = tempfile::tempdir().expect("resource database");
    let database_path = database_dir.path().join("resources.db");
    let (mut daemon, mut backend, probe) = resume_staging_fixture_with_backend(ResourceBackend::Sqlite(
        flotilla_store::SqliteBackend::open(&database_path).expect("resource store"),
    ))
    .await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let supervision = Arc::new(AcceptSupervision::default());
    daemon.crew_ops.set_turn_delivery_actuator(supervision.clone()).await;
    backend
        .clone()
        .using::<flotilla_resources::Project>("flotilla")
        .create(
            &test_meta("project"),
            &flotilla_resources::ProjectSpec::builder()
                .display_name("Project".to_string())
                .default_workflow_ref("workflow".to_string())
                .build(),
        )
        .await
        .expect("project");
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let source = convoys.get("resume-staging").await.expect("source convoy");
    let mut spec = source.spec.clone();
    spec.project_ref = Some("project".to_string());
    spec.role = "graphql-budget".to_string();
    let source =
        convoys.update(&input_meta_from_resource(&source), &source.metadata.resource_version, &spec).await.expect("project source");
    let mut status = source.status.expect("source status");
    status.phase = flotilla_resources::ConvoyPhase::Active;
    status.work.get_mut("work").expect("work").phase = flotilla_resources::WorkPhase::Running;
    status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Working;
    convoys.update_status("resume-staging", &source.metadata.resource_version, &status).await.expect("activate source");
    let governor = convoys
        .create(
            &test_meta("governor"),
            &ConvoySpec::builder()
                .workflow_ref("workflow".to_string())
                .role("governor".to_string())
                .project_ref("project".to_string())
                .build(),
        )
        .await
        .expect("governor convoy");
    convoys
        .update_status(
            "governor",
            &governor.metadata.resource_version,
            &ConvoyStatus {
                crew_work: BTreeMap::from([(
                    "watch".to_string(),
                    BTreeMap::from([("governor".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("governor status");
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    let source_session = sessions.get("resume-staging-session").await.expect("source session");
    let mut session_status = ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Running, ..Default::default() };
    session_status.attention = Some(TerminalAttention {
        state: TerminalAttentionState::Working,
        as_of: chrono::Utc::now(),
        source: TerminalAttentionSource::Hook,
    });
    sessions
        .update_status("resume-staging-session", &source_session.metadata.resource_version, &session_status)
        .await
        .expect("source working");
    let mut governor_spec = source_session.spec.clone();
    governor_spec.role = "governor".to_string();
    let TerminalSessionSource::Agent { context, .. } = &mut governor_spec.source else { panic!("agent session") };
    context.convoy = "governor".to_string();
    let governor_session = sessions
        .create(
            &InputMeta::builder()
                .name("governor-session".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "governor".to_string()),
                    (VESSEL_LABEL.to_string(), "watch".to_string()),
                    (ROLE_LABEL.to_string(), "governor".to_string()),
                ]))
                .build(),
            &governor_spec,
        )
        .await
        .expect("governor session");
    sessions
        .update_status(
            "governor-session",
            &governor_session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                crew: Some(flotilla_resources::CrewSessionStatus {
                    id: "governor-crew".to_string(),
                    adapter: "codex".to_string(),
                    model: None,
                    stance: "governor".to_string(),
                }),
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::Working,
                    as_of: chrono::Utc::now(),
                    source: TerminalAttentionSource::Hook,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("governor working");
    let (tx, _rx) = flotilla_store::controller::WorkQueueSender::channel();
    let mut task = tokio::spawn(daemon.reconciler_wake_watch().spawn(backend.clone(), "flotilla".to_string(), tx));
    let patch = convoy_external_patches::mark_crew_stalled(
        "resume-staging".to_string(),
        "work".to_string(),
        "coder".to_string(),
        chrono::Utc::now(),
        flotilla_resources::StallReason::Access,
        Some(flotilla_resources::StallProposedDisposition::ReduceScope),
        "repository permission missing".to_string(),
    );
    apply_resource_status_patch(&convoys, "resume-staging", &patch).await.expect("declare stall");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = convoys.get("resume-staging").await.expect("source").status.expect("status");
            if status.stalled.as_ref().is_some_and(|stalled| stalled.rung == flotilla_resources::StallRung::Governor) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("governor rung");
    let stalled = convoys.get("resume-staging").await.expect("source").status.expect("status");
    let condition = stalled.stalled.expect("stall condition");
    assert_eq!(condition.reason, Some(flotilla_resources::StallReason::Access));
    assert_eq!(condition.proposed_disposition, Some(flotilla_resources::StallProposedDisposition::ReduceScope));
    assert_eq!(condition.supervisor.expect("governor").convoy, "governor");
    assert_eq!(stalled.crew_work["work"]["coder"].phase, CrewWorkPhase::Stalled);
    {
        let requests = supervision.requests.lock().expect("supervision requests");
        let escalation = requests.iter().find(|request| request.vessel == "watch").expect("governor escalation");
        // #2592: the supervisor must identify the convoy and act on the exact stalled crew.
        assert_eq!(escalation.sender, "project/resume-staging/work/coder");
        assert_eq!(escalation.relation, flotilla_resources::MessageRelation::Supervisor);
        let framed = &escalation.brief;
        assert!(framed.starts_with("Escalated from coder@work in graphql-budget@project:"));
        assert!(framed.contains("convoy graphql-budget@project (resource ref: resume-staging)"), "{framed}");
        assert!(framed.contains("Reason: access. Evidence:"), "{framed}");
        assert!(framed.contains("repository permission missing"), "{framed}");
        for action in ["resume", "convert-to-failed", "escalate"] {
            assert!(
                framed.contains(&format!("flotilla crew supervise --convoy 'resume-staging' --vessel 'work' --role 'coder' {action}")),
                "{framed}"
            );
        }
        assert!(framed.contains("Proposed disposition: reduce-scope."), "{framed}");
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if daemon.crew_ops.subscription_rows().await.iter().any(|row| {
                matches!(&row.maker,
                flotilla_resources::LeafMaker::Supervisor { convoy, role, .. } if convoy == "governor" && role == "governor")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("governor row");
    // #2488: restarting the daemon rebuilds its leaf rows from stored status.
    // The already assigned governor retains authority to resume this same stall.
    task.abort();
    let _ = task.await;
    drop(daemon);
    backend = ResourceBackend::Sqlite(flotilla_store::SqliteBackend::open(&database_path).expect("reopen stored resources"));
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let restart_config = tempfile::tempdir().expect("restart config");
    std::fs::write(restart_config.path().join("daemon.toml"), "machine_id = \"resume-staging-test\"\n").expect("daemon config");
    daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(restart_config.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    daemon.set_work_credential_reconciler(probe.clone()).await;
    daemon.crew_ops.set_turn_delivery_actuator(supervision.clone()).await;
    let (tx, _rx) = flotilla_store::controller::WorkQueueSender::channel();
    task = tokio::spawn(daemon.reconciler_wake_watch().spawn(backend.clone(), "flotilla".to_string(), tx));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if daemon.crew_ops.subscription_rows().await.iter().any(|row| {
                matches!(&row.maker, flotilla_resources::LeafMaker::Supervisor { convoy, role, .. }
                    if convoy == "governor" && role == "governor")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("supervisor row restored after restart");
    daemon
        .crew_supervise_internal(
            CrewSupervisionRequest::builder()
                .namespace("flotilla")
                .convoy_name("resume-staging")
                .vessel("work")
                .role("coder")
                .operation(flotilla_protocol::CrewSupervisionAction::Resume)
                .message("continue with access")
                .actor_crew_id("governor-crew")
                .build(),
        )
        .await
        .expect("governor resume");
    let resumed = convoys.get("resume-staging").await.expect("source").status.expect("status");
    assert_eq!(resumed.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert!(resumed.stalled.is_none());
    let source_session = sessions.get("resume-staging-session").await.expect("resumed source session");
    assert!(matches!(source_session.spec.source, TerminalSessionSource::Agent { message: None, .. }));
    let guidance = backend
        .using::<flotilla_resources::Message>("flotilla")
        .list()
        .await
        .expect("guidance inbox")
        .items
        .into_iter()
        .find(|message| message.spec.body.contains("continue with access"))
        .expect("guidance Message");
    assert_eq!(guidance.spec.sender, "project/governor/watch/governor");
    assert_eq!(guidance.spec.relation, flotilla_resources::MessageRelation::Supervisor);
    let governor_session = sessions.get("governor-session").await.expect("governor session");
    sessions
        .update_status(
            "governor-session",
            &governor_session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::Idle,
                    as_of: chrono::Utc::now(),
                    source: TerminalAttentionSource::Hook,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("governor idle");
    let patch = convoy_external_patches::mark_crew_stalled(
        "resume-staging".to_string(),
        "work".to_string(),
        "coder".to_string(),
        chrono::Utc::now(),
        flotilla_resources::StallReason::Access,
        None,
        "repository permission still missing".to_string(),
    );
    apply_resource_status_patch(&convoys, "resume-staging", &patch).await.expect("stall again");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let status = convoys.get("resume-staging").await.expect("source").status.expect("status");
            if status
                .stalled
                .as_ref()
                .is_some_and(|stalled| stalled.rung == flotilla_resources::StallRung::Operator && stalled.supervision_exhausted)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("idle governor escalates to operator");
    apply_resource_status_patch(
        &convoys,
        "resume-staging",
        &convoy_external_patches::resume_crew_work(
            "work".to_string(),
            "coder".to_string(),
            chrono::Utc::now(),
            "implicit stall test".to_string(),
            None,
        ),
    )
    .await
    .expect("reset crew to working");
    let governor_session = sessions.get("governor-session").await.expect("governor session");
    sessions
        .update_status(
            "governor-session",
            &governor_session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::Working,
                    as_of: chrono::Utc::now(),
                    source: TerminalAttentionSource::Hook,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("governor working");
    let source_session = sessions.get("resume-staging-session").await.expect("source session");
    sessions
        .update_status(
            "resume-staging-session",
            &source_session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::NeedsInput,
                    as_of: chrono::Utc::now(),
                    source: TerminalAttentionSource::Hook,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("source needs input");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = convoys.get("resume-staging").await.expect("source").status.expect("status");
            if status
                .stalled
                .as_ref()
                .is_some_and(|stalled| stalled.rung == flotilla_resources::StallRung::Governor && stalled.evidence == "NeedsInput")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("NeedsInput routes to governor");
    let source_session = sessions.get("resume-staging-session").await.expect("source session");
    sessions
        .update_status(
            "resume-staging-session",
            &source_session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::Working,
                    as_of: chrono::Utc::now(),
                    source: TerminalAttentionSource::Hook,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("source resumes work");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if convoys.get("resume-staging").await.expect("source").status.expect("status").stalled.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("implicit stall cleared");
    task.abort();
}
