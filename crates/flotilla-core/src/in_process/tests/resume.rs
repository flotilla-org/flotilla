use flotilla_resources::ResumeIntent;
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_resources::{
    external_patches as convoy_external_patches,
    terminal_records::{AgentLaunch, TerminalLaunchFacts, TerminalLaunchRequest},
    Clock, Convoy as ResourceConvoy, ConvoySpec, CrewWorkPhase, TerminalAttention, TerminalAttentionSource, TerminalAttentionState,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

use flotilla_state_machines::machines::agent_session::Activity;

use super::support::{
    crew_messages, ensure_vessel, observe_attention, resume_staging_fixture, resume_staging_fixture_open, resume_staging_fixture_shaped,
    test_meta,
};
use flotilla_crew::crew_ops::{ConvoyResumeOutcome, CrewSupervisionRequest};

// Working crews already own staged credentials. Operator input is admitted as
// durable intent; ordinary attention changes cannot fabricate its delivery receipt.
#[tokio::test]
async fn working_resume_uses_message_delivery_instead_of_pending_brief_release() {
    let (daemon, backend, probe) = resume_staging_fixture_open().await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    // The coder's session runs: it is at work.
    let launch = probe.engine.terminals.record("resume-staging-session").name;
    probe.engine.terminals.ready(&launch, "resume-staging-pool-session", &Default::default());
    let status = convoys.get(&probe.convoy).await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    daemon
        .convoy_resume_internal("flotilla", &probe.convoy, "operator guidance", Some("work"), Some("coder"), ResumeIntent::Reminder)
        .await
        .expect("queue intent");
    daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect("migration scan");
    assert!(convoys.get(&probe.convoy).await.expect("convoy").status.expect("status").pending_brief().is_none());
    let records = crew_messages(&daemon, "flotilla").await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].spec.body, "operator guidance");
    // Admitted and not yet received: the engine's pending Message.
    assert_eq!(records[0].status.as_ref().expect("status").phase, flotilla_resources::MessagePhase::Deliverable);
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
    let records = crew_messages(&daemon, "flotilla").await;
    assert_eq!(records.len(), 1);
    // Native Message attribution is separate from its unframed body.
    assert_eq!(records[0].spec.body, "continue");
    // The engine addresses a principal no charter declares as the operator role (#3513).
    assert_eq!(records[0].spec.sender, "principal:operator");
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

    // The daemon's wall clock runs ahead on demand, so attention debounces elapse without real
    // waiting; the test stamps from the same clock. The source is the project's graphql-budget
    // role, its coder at work under the standard exit table.
    let clock = Arc::new(SkippingClock::default());
    let (daemon, backend, probe) =
        resume_staging_fixture_shaped(ResourceBackend::new(InMemoryBackend::default()), clock.clone(), false, |spec, snapshot| {
            spec.project_ref = Some("project".to_string());
            spec.role = "graphql-budget".to_string();
            snapshot.exit = Some(flotilla_resources::ExitDeclaration::standard_table());
        })
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
    let engine_path = daemon.convoy_engine_path();
    let note = |patch: flotilla_resources::ConvoyStatusPatch| {
        let (engine_path, convoys, convoy) = (engine_path.clone(), convoys.clone(), probe.convoy.clone());
        async move { flotilla_store::note_convoy(&engine_path, &convoys, &convoy, &patch).await }
    };
    // The project's governor: its work `watch` crewed by the governor agent.
    let governor = super::admit_snapshot(
        &daemon,
        "governor",
        ConvoySpec::builder().workflow_ref("workflow".to_string()).role("governor".to_string()).project_ref("project".to_string()).build(),
        flotilla_resources::WorkflowSnapshot {
            cascade: None,
            stall_nudges: Default::default(),
            supervision: None,
            exit: None,
            turn_delivery: Default::default(),
            vessels: vec![flotilla_resources::VesselRequirement::builder()
                .name("watch".to_string())
                .crew(vec![flotilla_engine_convoys_testkit::agent("governor")])
                .build()],
        },
    )
    .await;
    let terminals = &probe.engine.terminals;
    let source_vessel = ensure_vessel(&probe.engine, &probe.convoy, "work");
    probe.engine.vessels.ready(&source_vessel, "resume-env");
    let source_launch = terminals.record("resume-staging-session").name;
    terminals.ready(&source_launch, "resume-staging-pool-session", &Default::default());
    let working = |as_of| TerminalAttention { state: TerminalAttentionState::Working, as_of, source: TerminalAttentionSource::Hook };
    observe_attention(&probe.engine, &source_launch, working(clock.now()));
    let governor_vessel = ensure_vessel(&probe.engine, &governor, "watch");
    probe.engine.vessels.ready(&governor_vessel, "resume-env");
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
    observe_attention(&probe.engine, &governor_launch, working(clock.now()));
    let (tx, _rx) = flotilla_store::controller::WorkQueueSender::channel();
    let task = tokio::spawn(daemon.reconciler_wake_watch().spawn(backend.clone(), "flotilla".to_string(), tx));
    note(convoy_external_patches::mark_crew_stalled(
        probe.convoy.clone(),
        "work".to_string(),
        "coder".to_string(),
        clock.now(),
        flotilla_resources::StallReason::Access,
        Some(flotilla_resources::StallProposedDisposition::ReduceScope),
        "repository permission missing".to_string(),
    ))
    .await
    .expect("declare stall");
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
    assert_eq!(condition.supervisor.as_ref().expect("governor").convoy, governor);
    // The stall is the coder's: the seat reads Stalled once its supervision opens a stall record
    // about it, which the fake actuator here does not.
    assert!(condition.leaves.iter().any(|leaf| leaf.field_path == ".crew.coder.phase"), "{condition:?}");
    {
        let requests = supervision.requests.lock().expect("supervision requests");
        let escalation = requests.iter().find(|request| request.vessel == "watch").expect("governor escalation");
        // #2592: the supervisor must identify the convoy and act on the exact stalled crew.
        let coder =
            flotilla_crew::leaf_engine::Party::Seat { project: "project".into(), convoy: probe.convoy.clone(), role: "coder".into() };
        assert_eq!(escalation.sender, coder);
        assert_eq!(escalation.relation, flotilla_resources::MessageRelation::Supervisor);
        let framed = &escalation.brief;
        assert!(framed.starts_with("Escalated from coder@work in graphql-budget@project:"));
        assert!(framed.contains(&format!("convoy graphql-budget@project (resource ref: {})", probe.convoy)), "{framed}");
        assert!(framed.contains("Reason: access. Evidence:"), "{framed}");
        assert!(framed.contains("repository permission missing"), "{framed}");
        for action in ["resume", "convert-to-failed", "escalate"] {
            assert!(
                framed.contains(&format!("flotilla crew supervise --convoy '{}' --vessel 'work' --role 'coder' {action}", probe.convoy)),
                "{framed}"
            );
        }
        assert!(framed.contains("Proposed disposition: reduce-scope."), "{framed}");
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if daemon.crew.subscription_diagnostics().rows().await.iter().any(|row| {
                matches!(&row.maker,
                flotilla_resources::LeafMaker::Supervisor { convoy, role, .. } if *convoy == governor && role == "governor")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("governor row");
    // The stalled coder waits on its supervisor: its turn has ended, so the resume is not held
    // for a turn boundary.
    observe_attention(
        &probe.engine,
        &source_launch,
        TerminalAttention { state: TerminalAttentionState::Idle, as_of: clock.now(), source: TerminalAttentionSource::Hook },
    );
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
    let guidance = crew_messages(&daemon, "flotilla")
        .await
        .into_iter()
        .find(|message| message.spec.body.contains("continue with access"))
        .expect("guidance Message");
    assert_eq!(guidance.spec.sender, format!("project/{governor}/watch/governor"));
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
    note(convoy_external_patches::mark_crew_stalled(
        probe.convoy.clone(),
        "work".to_string(),
        "coder".to_string(),
        clock.now(),
        flotilla_resources::StallReason::Access,
        None,
        "repository permission still missing".to_string(),
    ))
    .await
    .expect("stall again");
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
    note(convoy_external_patches::resume_crew_work(
        "work".to_string(),
        "coder".to_string(),
        clock.now(),
        "implicit stall test".to_string(),
        None,
        ResumeIntent::Reopen,
    ))
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
            // The reopen is the home's note of the resumed brief. The engine has no edge that
            // reopens a kept promise, so the kept work stays Landing; the audit is a log line.
            assert!(status.crew_work["work"]["coder"].resume_brief_id.is_some());
            assert_eq!(status.phase, flotilla_resources::ConvoyPhase::Landing);
        } else {
            assert_eq!(
                result,
                CommandValue::ConvoyBriefInformational { message: "crew already claimed; delivered without reopening".into() }
            );
            assert_eq!(status.phase, flotilla_resources::ConvoyPhase::Landing);
        }
    }
}
