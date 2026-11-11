use flotilla_resources::ResumeIntent;
use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_config::ConfigStore;
use flotilla_protocol::HostName;
use flotilla_resources::{
    external_patches as convoy_external_patches,
    terminal_records::{AgentLaunch, TerminalLaunchFacts, TerminalLaunchRequest},
    Clock, Convoy as ResourceConvoy, ConvoySpec, ConvoyStatus, CrewWorkPhase, CrewWorkState, TerminalAttention, TerminalAttentionSource,
    TerminalAttentionState,
};
use flotilla_store::{apply_status_patch as apply_resource_status_patch, ResourceBackend};

use flotilla_state_machines::machines::agent_session::Activity;

use super::support::{ensure_vessel, observe_attention, resume_staging_fixture, resume_staging_fixture_with_backend, test_meta};
use crate::in_process::{input_meta_from_resource, InProcessDaemon};
use crate::testkits::discovery::fake_discovery;
use flotilla_crew::crew_ops::{ConvoyResumeOutcome, CrewSupervisionRequest};

// Working crews already own staged credentials. Operator input is admitted as
// durable intent; ordinary attention changes cannot fabricate its delivery receipt.
#[tokio::test]
async fn working_resume_uses_message_delivery_instead_of_pending_brief_release() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let current = convoys.get(&probe.convoy).await.expect("convoy");
    let mut status = current.status.expect("status");
    status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("coder").phase = CrewWorkPhase::Working;
    convoys.update_status(&current.metadata.name, &current.metadata.resource_version, &status).await.expect("working crew");
    daemon
        .convoy_resume_internal("flotilla", &probe.convoy, "operator guidance", Some("work"), Some("coder"), ResumeIntent::Reminder)
        .await
        .expect("queue intent");
    daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect("migration scan");
    assert!(convoys.get(&probe.convoy).await.expect("convoy").status.expect("status").pending_brief().is_none());
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
    let resume = || daemon.convoy_resume_internal("flotilla", &probe.convoy, "continue", Some("work"), Some("coder"), ResumeIntent::Reopen);
    assert!(resume().await.expect_err("staging should fail").contains("credential staging failed"));
    assert_eq!(
        convoys.get(&probe.convoy).await.expect("convoy").status.expect("status").crew_work["work"]["coder"].phase,
        CrewWorkPhase::Done
    );
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 0);
    resume().await.expect("retry resume");
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
    let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
    assert_eq!(records.len(), 1);
    // Native Message attribution is separate from its unframed body.
    assert_eq!(records[0].spec.body, "continue");
    assert_eq!(records[0].spec.sender, "principal:implicit");
}

/// System time plus an offset the test only ever moves forward.
#[derive(Default)]
struct SkippingClock(std::sync::Mutex<chrono::Duration>);
impl SkippingClock {
    fn skip(&self, by: chrono::Duration) {
        *self.0.lock().expect("clock offset") += by;
    }
}
impl Clock for SkippingClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now() + *self.0.lock().expect("clock offset")
    }
}

#[tokio::test]
async fn declared_access_stall_routes_to_project_governor_and_resumes() {
    #[derive(Default)]
    struct AcceptSupervision {
        requests: std::sync::Mutex<Vec<flotilla_crew::leaf_engine::CrewTurnIntent>>,
    }
    #[async_trait]
    impl flotilla_crew::leaf_engine::TurnDeliveryActuator for AcceptSupervision {
        async fn deliver(
            &self,
            request: &flotilla_crew::leaf_engine::CrewTurnIntent,
        ) -> Result<flotilla_crew::leaf_engine::CrewTurnAdmission, String> {
            self.requests.lock().expect("supervision requests").push(request.clone());
            Ok(flotilla_crew::leaf_engine::CrewTurnAdmission {
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
            _request: &flotilla_crew::leaf_engine::CrewTurnIntent,
            _act: &flotilla_resources::HoldAct,
            _reason: &str,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    let database_dir = tempfile::tempdir().expect("resource database");
    let database_path = database_dir.path().join("resources.db");
    let (mut daemon, mut backend, probe) = resume_staging_fixture_with_backend(ResourceBackend::new(
        flotilla_store_sqlite::SqliteBackend::open(&database_path).expect("resource store"),
    ))
    .await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let supervision = Arc::new(AcceptSupervision::default());
    daemon.crew.set_turn_delivery_actuator(supervision.clone()).await;
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
    let source = convoys.get(&probe.convoy).await.expect("source convoy");
    let mut spec = source.spec.clone();
    spec.project_ref = Some("project".to_string());
    spec.role = "graphql-budget".to_string();
    let source =
        convoys.update(&input_meta_from_resource(&source), &source.metadata.resource_version, &spec).await.expect("project source");
    let mut status = source.status.expect("source status");
    status.phase = flotilla_resources::ConvoyPhase::Active;
    status.workflow_snapshot.as_mut().expect("workflow").exit = Some(flotilla_resources::ExitDeclaration::standard_table());
    status.work.get_mut("work").expect("work").phase = flotilla_resources::WorkPhase::Running;
    status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Working;
    convoys.update_status(&probe.convoy, &source.metadata.resource_version, &status).await.expect("activate source");
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
    let terminals = &probe.engine.terminals;
    let source_launch = terminals.record("resume-staging-session").name;
    terminals.ready(&source_launch, "resume-staging-pool-session", &Default::default());
    let working = |as_of| TerminalAttention { state: TerminalAttentionState::Working, as_of, source: TerminalAttentionSource::Hook };
    observe_attention(&probe.engine, &source_launch, working(chrono::Utc::now()));
    let governor_vessel = ensure_vessel(&probe.engine, "governor", "watch");
    let mut governor_request = terminals.record(&source_launch);
    governor_request.role = "governor".to_string();
    let governor_launch = terminals.request(&TerminalLaunchRequest {
        session: "governor-session".to_string(),
        environment: governor_request.environment,
        vessel: Some(governor_vessel),
        role: governor_request.role,
        pool: governor_request.pool,
        cwd: governor_request.cwd,
        env: governor_request.env,
        source: governor_request.source,
    });
    let governor_facts = TerminalLaunchFacts {
        agent: Some(AgentLaunch { adapter: "codex".to_string(), model: None, stance: "governor".to_string() }),
        ..Default::default()
    };
    terminals.ready(&governor_launch, "governor-pool-session", &governor_facts);
    observe_attention(&probe.engine, &governor_launch, working(chrono::Utc::now()));
    let (tx, _rx) = flotilla_store::controller::WorkQueueSender::channel();
    let mut task = tokio::spawn(daemon.reconciler_wake_watch().spawn(backend.clone(), "flotilla".to_string(), tx));
    let patch = convoy_external_patches::mark_crew_stalled(
        probe.convoy.clone(),
        "work".to_string(),
        "coder".to_string(),
        chrono::Utc::now(),
        flotilla_resources::StallReason::Access,
        Some(flotilla_resources::StallProposedDisposition::ReduceScope),
        "repository permission missing".to_string(),
    );
    apply_resource_status_patch(&convoys, &probe.convoy, &patch).await.expect("declare stall");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = convoys.get(&probe.convoy).await.expect("source").status.expect("status");
            if status.stalled.as_ref().is_some_and(|stalled| stalled.rung == flotilla_resources::StallRung::Governor) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("governor rung");
    let stalled = convoys.get(&probe.convoy).await.expect("source").status.expect("status");
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
            if daemon.crew.subscription_diagnostics().rows().await.iter().any(|row| {
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
    backend = ResourceBackend::new(flotilla_store_sqlite::SqliteBackend::open(&database_path).expect("reopen stored resources"));
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let restart_config = tempfile::tempdir().expect("restart config");
    std::fs::write(restart_config.path().join("daemon.toml"), "machine_id = \"resume-staging-test\"\n").expect("daemon config");
    // The restarted daemon's wall clock runs ahead on demand, so attention
    // debounces elapse without real waiting; the test stamps from the same clock.
    let clock = Arc::new(SkippingClock::default());
    daemon = InProcessDaemon::new_with_resource_backend_and_clock(
        Vec::new(),
        Arc::new(ConfigStore::with_base(restart_config.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
        clock.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    // The restarted daemon reads the same engine's launches.
    daemon.install_lifecycle_reads(probe.engine.reads()).expect("engine reads");
    daemon.set_work_credential_reconciler(probe.clone()).await;
    daemon.crew.set_turn_delivery_actuator(supervision.clone()).await;
    let (tx, _rx) = flotilla_store::controller::WorkQueueSender::channel();
    task = tokio::spawn(daemon.reconciler_wake_watch().spawn(backend.clone(), "flotilla".to_string(), tx));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if daemon.crew.subscription_diagnostics().rows().await.iter().any(|row| {
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
        .crew
        .supervise(
            CrewSupervisionRequest::builder()
                .namespace("flotilla")
                .convoy_name(&probe.convoy)
                .vessel("work")
                .role("coder")
                .operation(flotilla_protocol::CrewSupervisionAction::Resume)
                .message("continue with access")
                // The governor's crew id is its launch's name.
                .actor_crew_id(&governor_launch)
                .build(),
        )
        .await
        .expect("governor resume");
    let resumed = convoys.get(&probe.convoy).await.expect("source").status.expect("status");
    assert_eq!(resumed.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert!(resumed.stalled.is_none());
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
    observe_attention(
        &probe.engine,
        &governor_launch,
        TerminalAttention { state: TerminalAttentionState::Idle, as_of: clock.now(), source: TerminalAttentionSource::Hook },
    );
    // Each hook is recorded on the launch's AgentSession too, which stall judgement reads.
    let agents = super::support::agent_sessions(&daemon);
    let hook = |session: &str, activity, cause: &str| {
        agents.observe(session, activity, cause.into(), super::support::engine_at(clock.now())).expect("hook activity");
    };
    hook(&governor_launch, Activity::Idle, "Stop");
    let patch = convoy_external_patches::mark_crew_stalled(
        probe.convoy.clone(),
        "work".to_string(),
        "coder".to_string(),
        clock.now(),
        flotilla_resources::StallReason::Access,
        None,
        "repository permission still missing".to_string(),
    );
    apply_resource_status_patch(&convoys, &probe.convoy, &patch).await.expect("stall again");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            // Skip through the idle governor's attention debounce at ten times real speed.
            clock.skip(chrono::Duration::milliseconds(500));
            let status = convoys.get(&probe.convoy).await.expect("source").status.expect("status");
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
        &probe.convoy,
        &convoy_external_patches::resume_crew_work(
            "work".to_string(),
            "coder".to_string(),
            clock.now(),
            "implicit stall test".to_string(),
            None,
            ResumeIntent::Reopen,
        ),
    )
    .await
    .expect("reset crew to working");
    observe_attention(&probe.engine, &governor_launch, working(clock.now()));
    hook(&governor_launch, Activity::Working, "UserPromptSubmit");
    observe_attention(
        &probe.engine,
        &source_launch,
        TerminalAttention { state: TerminalAttentionState::NeedsInput, as_of: clock.now(), source: TerminalAttentionSource::Hook },
    );
    hook(&source_launch, Activity::NeedsInput, "PermissionRequest");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = convoys.get(&probe.convoy).await.expect("source").status.expect("status");
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
    observe_attention(&probe.engine, &source_launch, working(clock.now()));
    hook(&source_launch, Activity::Working, "PostToolUse");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if convoys.get(&probe.convoy).await.expect("source").status.expect("status").stalled.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("implicit stall cleared");
    task.abort();
}

// #3145: information after a claim neither stages credentials nor changes the
// completion, even if no terminal session remains intact. Explicit reopen does.
#[tokio::test]
async fn claimed_resume_delivers_information_without_reopening() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let claimed = convoys.get(&probe.convoy).await.unwrap().status.unwrap();
    let outcome =
        daemon.convoy_resume_internal("flotilla", &probe.convoy, "please complete", None, None, ResumeIntent::Reminder).await.unwrap();
    assert_eq!(outcome, ConvoyResumeOutcome::Informational);
    assert_eq!(convoys.get(&probe.convoy).await.unwrap().status.unwrap(), claimed);
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 0);
    let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.unwrap().items;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.body, "please complete");
    assert_eq!(messages[0].spec.expectation, flotilla_resources::MessageExpectation::None);
    // No launch of its session remains intact.
    let terminals = &probe.engine.terminals;
    let launch = terminals.record("resume-staging-session").name;
    terminals.ready(&launch, "resume-staging-pool-session", &Default::default());
    terminals.failed(&launch, "pool lost the session");
    daemon.convoy_resume_internal("flotilla", &probe.convoy, "duplicate reminder", None, None, ResumeIntent::Reminder).await.unwrap();
    assert_eq!(convoys.get(&probe.convoy).await.unwrap().status.unwrap(), claimed);
}

// #3145: the command reports information-only delivery without a lifecycle
// mutation; --reopen records the principal and time through the existing audit.
#[tokio::test]
async fn claimed_resume_command_reports_outcome_and_audits_explicit_reopen() {
    use flotilla_daemon_api::daemon::DaemonHandle;
    use flotilla_protocol::{Command, CommandAction, CommandCaller, CommandValue, DaemonEvent, PrincipalRef};
    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let caller = CommandCaller { principal_ref: PrincipalRef::implicit_for_namespace("flotilla"), process: None, crew: None };
    let mut events = daemon.subscribe();
    for reopen in [false, true] {
        let id = daemon
            .execute_for_caller(
                Command::builder()
                    .action(CommandAction::ConvoyResume {
                        namespace: Some("flotilla".into()),
                        name: probe.convoy.clone(),
                        prompt: "please complete".into(),
                        vessel: None,
                        role: None,
                        reopen,
                    })
                    .build(),
                Some(caller.clone()),
            )
            .await
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let DaemonEvent::CommandFinished { command_id, result, .. } = events.recv().await.unwrap() {
                    if command_id == id {
                        break result;
                    }
                }
            }
        })
        .await
        .unwrap();
        let status = backend.using::<ResourceConvoy>("flotilla").get(&probe.convoy).await.unwrap().status.unwrap();
        if reopen {
            assert!(matches!(result, CommandValue::ConvoyBriefQueued { .. }), "{result:?}");
            assert_eq!(status.phase, flotilla_resources::ConvoyPhase::Active);
            assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
            assert_eq!(status.lifecycle_mutations.len(), 1);
            assert_eq!(status.lifecycle_mutations[0].action, "reopened");
            assert_eq!(status.lifecycle_mutations[0].caller, caller);
        } else {
            assert_eq!(
                result,
                CommandValue::ConvoyBriefInformational { message: "crew already claimed; delivered without reopening".into() }
            );
            assert_eq!(status.phase, flotilla_resources::ConvoyPhase::Landing);
            assert!(status.lifecycle_mutations.is_empty());
        }
    }
}
