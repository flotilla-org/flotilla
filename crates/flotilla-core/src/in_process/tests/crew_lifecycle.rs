use flotilla_resources::ResumeIntent;
use flotilla_store_memory::engine_home_backend;
use std::sync::Arc;

use chrono::Utc;
use flotilla_config::ConfigStore;
use flotilla_protocol::{CrewCommandContext, DaemonEvent, HostName};
use flotilla_resources::{
    Convoy as ResourceConvoy, ConvoyPhase, ConvoySpec, CrewSource, CrewWorkPhase, Selector, TerminalAttention, TerminalAttentionSource,
    TerminalAttentionState, TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase,
};

use flotilla_state_machines::machines::agent_session::Activity;

use super::support::{
    admit_snapshot, admit_standing, agent_sessions, claim_crew, complete_work, engine_at, ensure_vessel, observe_attention,
    resume_staging_fixture_shaped, stall_test_daemon, stall_test_session, stall_workflow_snapshot, test_meta, wait_for_stall,
};
use crate::in_process::InProcessDaemon;
use crate::testkits::discovery::fake_discovery;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_store_testkit::cr_name;

#[tokio::test]
async fn crew_fail_requires_operator_force() {
    let (daemon, backend, _temp, watch, engine) = stall_test_daemon().await;
    let error = daemon
        .crew_fail_internal(&CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() }, "blocked".into(), false, None)
        .await
        .expect_err("crew failure must be refused");
    assert!(error.contains("crew stall"), "{error}");
    let principal = flotilla_protocol::PrincipalRef::implicit_for_namespace("flotilla");
    let error = daemon
        .crew_fail_internal(
            &CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
            "blocked".into(),
            true,
            Some(&principal),
        )
        .await
        .expect_err("crew identity cannot force failure");
    assert!(error.contains("crew stall"), "{error}");

    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let failure = admit_snapshot(
        &daemon,
        "operator-failure",
        ConvoySpec::builder().workflow_ref("test".into()).project_ref("flotilla".into()).build(),
        stall_workflow_snapshot(vec![claim_crew("coder")]),
    )
    .await;
    let vessel = super::support::ensure_vessel(&engine, &failure, "work");
    daemon
        .crew_fail_internal(
            &CrewCommandContext {
                namespace: Some("flotilla".into()),
                convoy: Some(failure.clone()),
                vessel_ref: Some(vessel),
                role: Some("coder".into()),
                ..Default::default()
            },
            "supervisor ruling".into(),
            true,
            Some(&principal),
        )
        .await
        .expect("operator force failure");
    let status = convoys.get(&failure).await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Failed);
    watch.abort();
}

#[tokio::test]
async fn active_idle_crew_stalls_and_working_crew_clears() {
    let (daemon, backend, _temp, watch, engine) = stall_test_daemon().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let mut snapshot = stall_workflow_snapshot(vec![claim_crew("coder")]);
    snapshot.exit = Some(flotilla_resources::ExitDeclaration::standard_table());
    // No nudges: the idle agent's stall escalates at once.
    snapshot.stall_nudges.insert(
        "work/coder".into(),
        flotilla_resources::StallNudgePolicy { max_per_episode: 0, max_refusals: None, idle_grace_seconds: Some(3) },
    );
    let idle_crew = admit_snapshot(
        &daemon,
        "idle-crew",
        ConvoySpec::builder().workflow_ref("test".into()).project_ref("flotilla".into()).build(),
        snapshot,
    )
    .await;
    // The screen read Idle six seconds ago: quiet from a second ago, after the screen debounce,
    // so the three-second grace runs out two seconds from now.
    let idle_at = Utc::now() - chrono::Duration::seconds(6);
    let launch = stall_test_session(
        &engine,
        &idle_crew,
        "idle-session",
        "coder",
        TerminalAttention { state: TerminalAttentionState::Idle, as_of: idle_at, source: TerminalAttentionSource::Screen },
    )
    .await;
    let agents = agent_sessions(&daemon, &[&launch]);
    agents.observe_screen(&launch, Activity::Idle, engine_at(idle_at)).expect("idle activity");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(convoys.get(&idle_crew).await.expect("convoy").status.expect("status").stalled.is_none());
    let status = wait_for_stall(&backend, &idle_crew, true).await;
    let stalled = status.stalled.expect("stalled");
    assert!(matches!(stalled.maker, Some(flotilla_resources::LeafMaker::Actor { ref role, .. }) if role == "coder"));
    // The idle evidence explains why the default ProjectCrew rung cannot supply a supervisor
    // (#2522): the fixture's convoy is project flotilla's, which has no governor.
    assert_eq!(stalled.evidence, "idle; no live governor for project flotilla (no matching convoy in replica view)");
    assert_eq!(stalled.source, flotilla_resources::StallEvidenceSource::Screen);
    let mut events = daemon.subscribe();
    let subscription = daemon
        .crew
        .subscribe_wait(
            uuid::Uuid::new_v4(),
            flotilla_protocol::WaitSubscriptionRequest {
                namespace: "flotilla".into(),
                leaves: vec![format!("convoy/{idle_crew} .status.stalled == true").parse().expect("stall leaf")],
                freshness_demand: None,
            },
        )
        .await
        .expect("subscribe to stalled condition");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let DaemonEvent::LeafFired(fire) = events.recv().await.expect("leaf event") {
                if fire.subscription_id == subscription {
                    break;
                }
            }
        }
    })
    .await
    .expect("stalled leaf fires");
    observe_attention(
        &engine,
        &launch,
        TerminalAttention { state: TerminalAttentionState::Working, as_of: Utc::now(), source: TerminalAttentionSource::Screen },
    );
    agents.observe_screen(&launch, Activity::Working, engine_at(Utc::now())).expect("working activity");
    assert!(wait_for_stall(&backend, &idle_crew, false).await.stalled.is_none());
    watch.abort();
}

// The stall judge hears an agent's activity from its AgentSession's writes (#3108): an idle crew
// working again clears its stall though nothing else about its session changed.
#[tokio::test]
async fn an_agent_working_again_clears_its_stall_from_its_session_alone() {
    let (daemon, backend, _temp, watch, engine) = stall_test_daemon().await;
    let mut snapshot = stall_workflow_snapshot(vec![claim_crew("coder")]);
    snapshot.exit = Some(flotilla_resources::ExitDeclaration::standard_table());
    snapshot.stall_nudges.insert(
        "work/coder".into(),
        flotilla_resources::StallNudgePolicy { max_per_episode: 2, max_refusals: None, idle_grace_seconds: Some(0) },
    );
    let idle_crew = admit_snapshot(
        &daemon,
        "idle-crew",
        ConvoySpec::builder().workflow_ref("test".into()).project_ref("flotilla".into()).build(),
        snapshot,
    )
    .await;
    let idle_at = Utc::now() - chrono::Duration::seconds(6);
    let launch = stall_test_session(
        &engine,
        &idle_crew,
        "idle-session",
        "coder",
        TerminalAttention { state: TerminalAttentionState::Idle, as_of: idle_at, source: TerminalAttentionSource::Screen },
    )
    .await;
    let agents = agent_sessions(&daemon, &[&launch]);
    agents.observe_screen(&launch, Activity::Idle, engine_at(idle_at)).expect("idle activity");
    assert!(wait_for_stall(&backend, &idle_crew, true).await.stalled.is_some());
    agents.observe_screen(&launch, Activity::Working, engine_at(Utc::now())).expect("working activity");
    assert!(wait_for_stall(&backend, &idle_crew, false).await.stalled.is_none());
    watch.abort();
}

#[tokio::test]
async fn stale_working_attention_does_not_stall_crew() {
    let (daemon, backend, _temp, watch, engine) = stall_test_daemon().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let stale_crew = admit_snapshot(
        &daemon,
        "stale-crew",
        ConvoySpec::builder().workflow_ref("test".into()).project_ref("flotilla".into()).build(),
        stall_workflow_snapshot(vec![claim_crew("coder")]),
    )
    .await;
    stall_test_session(
        &engine,
        &stale_crew,
        "stale-session",
        "coder",
        TerminalAttention {
            state: TerminalAttentionState::Working,
            as_of: Utc::now() - chrono::Duration::minutes(3),
            source: TerminalAttentionSource::Hook,
        },
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1250)).await;
    assert!(convoys.get(&stale_crew).await.expect("convoy").status.expect("status").stalled.is_none());
    watch.abort();
}

#[tokio::test]
async fn landing_done_crew_with_idle_session_has_no_actor_stall() {
    let (daemon, backend, _temp, watch, engine) = stall_test_daemon().await;
    let repository = flotilla_resources::RepositoryKey("repo".into());
    let spec = ConvoySpec::builder()
        .workflow_ref("test".into())
        .project_ref("flotilla".into())
        .repositories(vec![flotilla_resources::ConvoyRepositorySpec::builder()
            .url("https://github.com/flotilla-org/flotilla".into())
            .repo_ref(repository.clone())
            .source_ref("feature/landing".into())
            .target_ref("main".into())
            .workspace_slug("flotilla".into())
            .subpaths(Vec::new())
            .build()])
        .change_request(
            flotilla_resources::BoundChangeRequest::builder().id("1392".into()).repository_ref(repository).title("test".into()).build(),
        )
        .build();
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let mut snapshot = stall_workflow_snapshot(vec![claim_crew("coder")]);
    snapshot.exit = Some(flotilla_resources::ExitDeclaration::Table(indexmap::IndexMap::from([(
        "shipped".into(),
        flotilla_resources::parse_leaf_template("$cr.state == merged").expect("leaf template"),
    )])));
    let landed = admit_snapshot(&daemon, "landed-claim", spec, snapshot).await;
    // The coder's work is complete: the convoy is Landing.
    complete_work(&daemon, &landed, "work").await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if daemon
                .crew
                .subscription_diagnostics()
                .rows()
                .await
                .iter()
                .any(|row| matches!(&row.watcher, flotilla_resources::LeafWatcher::ReconcilerWake { convoy } if *convoy == landed))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("exit row armed");
    let records = backend.clone().using::<flotilla_resources::ChangeRequest>("flotilla");
    let record_name = cr_name("github.com", "flotilla-org/flotilla", 1392);
    let record = records.get(&record_name).await.expect("demanded observation record");
    let fresh = Utc::now();
    records
        .update_status(
            &record_name,
            &record.metadata.resource_version,
            &flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, fresh),
                head_sha: flotilla_resources::Observation::known("head".into(), fresh),
                checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pending, fresh),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::known(false, fresh),
                },
                mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, fresh),
            },
        )
        .await
        .expect("fresh observation");
    stall_test_session(
        &engine,
        &landed,
        "idle-landing-session",
        "coder",
        TerminalAttention { state: TerminalAttentionState::Idle, as_of: Utc::now(), source: TerminalAttentionSource::Hook },
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1250)).await;
    assert!(convoys.get(&landed).await.expect("convoy").status.expect("status").stalled.is_none());
    watch.abort();
}

#[tokio::test]
async fn landing_without_armed_exit_rows_stalls() {
    let (daemon, backend, _temp, watch, _engine) = stall_test_daemon().await;
    // Its coder's work is complete, so it is Landing, and it declares no exit.
    let unarmed = admit_snapshot(
        &daemon,
        "unarmed-landing",
        ConvoySpec::builder().workflow_ref("test".into()).project_ref("flotilla".into()).build(),
        stall_workflow_snapshot(vec![claim_crew("coder")]),
    )
    .await;
    complete_work(&daemon, &unarmed, "work").await;
    let stalled = wait_for_stall(&backend, &unarmed, true).await.stalled.expect("stalled");
    assert_eq!(stalled.maker, None);
    assert_eq!(stalled.evidence, flotilla_crew::leaf_engine::NO_EXIT_STALL_EVIDENCE);
    watch.abort();
}

#[tokio::test]
// #3412: the real daemon keeps stale evidence unknown during observer grace,
// then attributes unrecovered evidence to observer lag after its deadline.
async fn stale_change_request_observation_stalls_landing_convoy() {
    let clock = Arc::new(flotilla_store_testkit::VirtualClock::new(Utc::now()));
    let (daemon, backend, _temp, watch, _engine) = super::support::stall_test_daemon_with_clock(clock.clone()).await;
    // The test judges by hand: the daemon's own judge would sync the rows beside it from its
    // own convoy revision, and arm the change request's observer before the test writes it.
    // It has not run yet, so it never does.
    watch.abort();
    let repository = flotilla_resources::RepositoryKey("repo".into());
    let spec = ConvoySpec::builder()
        .workflow_ref("test".into())
        .project_ref("flotilla".into())
        .repositories(vec![flotilla_resources::ConvoyRepositorySpec::builder()
            .url("https://github.com/flotilla-org/flotilla".into())
            .repo_ref(repository.clone())
            .source_ref("feature/stall".into())
            .target_ref("main".into())
            .workspace_slug("flotilla".into())
            .subpaths(Vec::new())
            .build()])
        .change_request(
            flotilla_resources::BoundChangeRequest::builder().id("1391".into()).repository_ref(repository).title("test".into()).build(),
        )
        .build();
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let mut snapshot = stall_workflow_snapshot(vec![claim_crew("coder")]);
    snapshot.exit = Some(flotilla_resources::ExitDeclaration::Table(indexmap::IndexMap::from([(
        "shipped".into(),
        flotilla_resources::parse_leaf_template("$cr.state == merged").expect("leaf template"),
    )])));
    let stale = admit_snapshot(&daemon, "stale-observation", spec, snapshot).await;
    // The coder's work is complete: the convoy is Landing.
    complete_work(&daemon, &stale, "work").await;
    let records = backend.clone().using::<flotilla_resources::ChangeRequest>("flotilla");
    let record_name = cr_name("github.com", "flotilla-org/flotilla", 1391);
    let record = records
        .create(
            &test_meta(&record_name),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".into())
                .scope("flotilla-org/flotilla".into())
                .number(1391)
                .observing_authority("test".into())
                .build(),
        )
        .await
        .expect("observation record");
    let old = Utc::now() - chrono::Duration::hours(1);
    records
        .update_status(
            &record_name,
            &record.metadata.resource_version,
            &flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, old),
                head_sha: flotilla_resources::Observation::known("old".into(), old),
                checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pending, old),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::known(false, old),
                },
                mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, old),
            },
        )
        .await
        .expect("stale observation");
    // The decided store keeps no observation times: the record's values age from when its
    // observer's reads began failing.
    let failures =
        flotilla_provisioning::engine::observations::EngineObservations::open(daemon.decided_store(), clock.clone()).expect("observations");
    flotilla_resources::engine_records::ObservationFailures::failing(&failures, "ChangeRequest", &record_name)
        .await
        .expect("observer reads failing");
    daemon.reconcile_crew_stalls_once("flotilla").await.expect("judge inside observer grace");
    let stalled = convoys.get(&stale).await.expect("convoy").status.expect("status").stalled;
    assert!(stalled.is_none(), "stalled inside observer grace: {stalled:?}");
    clock.advance(chrono::Duration::hours(1));
    daemon.reconcile_crew_stalls_once("flotilla").await.expect("judge expired observer grace");
    let stalled = convoys.get(&stale).await.expect("convoy").status.expect("status").stalled.expect("stalled");
    assert!(matches!(stalled.maker, Some(flotilla_resources::LeafMaker::Observed { .. })));
    assert!(stalled.evidence.contains("stale"), "{}", stalled.evidence);
    assert!(stalled.evidence.contains("observer behind scheduled refresh grace"));
    assert_eq!(stalled.source, flotilla_resources::StallEvidenceSource::Observation);
}

#[tokio::test]
async fn idle_standing_role_without_obligation_never_stalls_convoy() {
    let clock = Arc::new(flotilla_store_testkit::VirtualClock::new(Utc::now()));
    let (daemon, backend, _temp, watch, engine) = super::support::stall_test_daemon_with_clock(clock.clone()).await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    // A standing governor owes only its lifetime duty, no promise.
    let standing =
        admit_standing(&daemon, "flotilla", "governor", stall_workflow_snapshot(vec![flotilla_engine_convoys_testkit::agent("governor")]))
            .await;
    stall_test_session(
        &engine,
        &standing,
        "governor-session",
        "governor",
        TerminalAttention {
            state: TerminalAttentionState::Idle,
            as_of: Utc::now() - chrono::Duration::seconds(10),
            source: TerminalAttentionSource::Screen,
        },
    )
    .await;
    daemon.reconcile_crew_stalls_once("flotilla").await.expect("initial judgement");
    clock.advance(chrono::Duration::hours(1));
    daemon.reconcile_crew_stalls_once("flotilla").await.expect("past idle grace");
    assert!(convoys.get(&standing).await.expect("convoy").status.expect("status").stalled.is_none());
    watch.abort();
}

#[tokio::test]
async fn replica_wake_engine_does_not_write_stalled_condition() {
    let authority = flotilla_engine_convoys_testkit::EngineHome::open("authority");
    let convoys = authority.backend().clone().using::<ResourceConvoy>("flotilla");
    let launch = authority
        .admit(
            "replicated",
            ConvoySpec::builder().workflow_ref("test".into()).project_ref("flotilla".into()).build(),
            flotilla_engine_convoys_testkit::single_agent("coder"),
        )
        .await;
    flotilla_engine_convoys_testkit::keep_all(&authority.convoys.path(), &authority.convoy(&launch).await).await;
    assert_eq!(convoys.get(&launch).await.expect("convoy").status.expect("status").phase, flotilla_resources::ConvoyPhase::Landing);
    let replica = engine_home_backend();
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"replica-stall-test\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("replica"),
        replica.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    super::test_engine(&daemon);
    flotilla_provisioning_testkit::replicate_home(authority.service(), "authority", &daemon.decided_store());
    let (sender, _receiver) = flotilla_store::controller::WorkQueueSender::channel();
    let watch = daemon.reconciler_wake_watch();
    let replica_check = replica.clone();
    let task = tokio::spawn(async move { watch.spawn(replica.clone(), "flotilla".into(), sender).await.expect("replica watch") });
    tokio::time::sleep(std::time::Duration::from_millis(1250)).await;
    assert!(!task.is_finished(), "replica watch must remain healthy");
    assert!(replica_check.using::<ResourceConvoy>("flotilla").list().await.expect("local convoys").items.is_empty());
    assert!(replica_check
        .including_replicas::<ResourceConvoy>("flotilla")
        .get(&launch)
        .await
        .expect("replicated convoy")
        .object
        .status
        .expect("replica status")
        .stalled
        .is_none());
    assert!(convoys.get(&launch).await.expect("authority convoy").status.expect("status").stalled.is_none());
    task.abort();
}

// Process exit leaves unfinished work resumable in its existing checkout. An
// operator follow-up relaunches a stopped session instead of waiting for a hook
// that an exited agent cannot send; both observed and not-yet-observed exits work.
#[tokio::test]
async fn resume_relaunches_exited_active_and_interrupted_crew() {
    // The coder is an agent, the shape its agent session is launched for: only an agent's exit
    // interrupts its seat.
    let (daemon, backend, probe) =
        resume_staging_fixture_shaped(engine_home_backend(), Arc::new(flotilla_resources::SystemClock), false, |_, snapshot| {
            snapshot.vessels[0].crew[0].source = CrewSource::Agent {
                selector: Selector { capability: "code".to_string(), adapter: None, model: None },
                prompt: None,
                brief_template: None,
            };
        })
        .await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("resume-staging-session").await.expect("session");
    let original_cwd = session.spec.cwd.clone();
    // The session runs in a realised vessel, so its exit interrupts the seat.
    probe.engine.vessels.ready(&ensure_vessel(&probe.engine, &probe.convoy, "work"), "resume-env");
    let terminals = &probe.engine.terminals;
    let exited = terminals.record("resume-staging-session").name;
    terminals.ready(&exited, "resume-staging-pool-session", &Default::default());
    terminals.exited(&exited, "exit 0");
    let status = convoys.get(&probe.convoy).await.expect("convoy").status.expect("status");
    assert_eq!(status.phase, ConvoyPhase::Interrupted, "its work is unfinished, interrupted by the exit");
    daemon
        .convoy_resume_internal(
            "flotilla",
            &probe.convoy,
            "Recover the unfinished review",
            Some("work"),
            Some("coder"),
            ResumeIntent::Reminder,
        )
        .await
        .expect("resume exited agent");
    let session = sessions.get("resume-staging-session").await.expect("same session");
    assert_eq!(session.spec.cwd, original_cwd);
    assert_eq!(session.status.expect("session status").phase, ResourceTerminalSessionPhase::Starting);
    let relaunched = terminals.record("resume-staging-session").name;
    assert_ne!(relaunched, exited, "the resume starts the session's next launch");
    assert!(backend
        .using::<flotilla_resources::Message>("flotilla")
        .list()
        .await
        .expect("inbox")
        .items
        .iter()
        .any(|message| message.spec.body.contains("Recover the unfinished review")));
    // The pool starts the next launch: the coder is at work again.
    terminals.ready(&relaunched, "resume-staging-pool-session-2", &Default::default());
    let status = convoys.get(&probe.convoy).await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
}

// ADR 0061: crew retraction is immediate only for crew-sourced promises;
// template/dispatch retractions stall with the promise and reason preserved.
#[tokio::test]
async fn promise_retractions_stall_unless_crew_sourced() {
    use flotilla_protocol::commands::CrewPromiseOperation;
    use flotilla_resources::promises::{PromiseKind, PromiseState};
    let (daemon, backend, _temp, watch, engine) = stall_test_daemon().await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    // The template and the dispatch each declare a PR promise for the coder; the coder declares
    // its own.
    let crew = flotilla_resources::CrewSpec::builder()
        .role("coder".into())
        .source(flotilla_resources::CrewSource::Tool { command: "test".into() })
        .promises(vec![flotilla_resources::TemplatePromise { kind: PromiseKind::Pr, owner: "coder".into() }])
        .build();
    let spec = ConvoySpec::builder()
        .workflow_ref("test".into())
        .project_ref("flotilla".into())
        .promises(vec![flotilla_resources::promises::DispatchPromise {
            label: "dispatch".into(),
            kind: PromiseKind::Pr,
            vessel: "work".into(),
            role: "coder".into(),
        }])
        .build();
    let promise_crew = admit_snapshot(&daemon, "promise-crew", spec, stall_workflow_snapshot(vec![crew])).await;
    let vessel = super::support::ensure_vessel(&engine, &promise_crew, "work");
    let context = CrewCommandContext {
        namespace: Some("flotilla".into()),
        convoy: Some(promise_crew.clone()),
        vessel_ref: Some(vessel),
        role: Some("coder".into()),
        ..Default::default()
    };
    daemon.crew.promise(&context, CrewPromiseOperation::Promise { id: "own".into(), kind: "pr".into() }).await.unwrap();
    let status = convoys.get(&promise_crew).await.unwrap().status.unwrap();
    for id in ["template/pr", "dispatch", "own"] {
        assert!(status.promises["work"]["coder"].iter().any(|p| p.id == id && p.state == PromiseState::Open), "{id}");
    }
    daemon.crew.promise(&context, CrewPromiseOperation::Retract { id: "own".into(), reason: "scope reduced".into() }).await.unwrap();
    for id in ["template/pr", "dispatch"] {
        daemon.crew.promise(&context, CrewPromiseOperation::Retract { id: id.into(), reason: "not achievable".into() }).await.unwrap();
        let status = convoys.get(&promise_crew).await.unwrap().status.unwrap();
        let promise = status.promises["work"]["coder"].iter().find(|p| p.id == id).unwrap();
        assert_eq!(promise.state, PromiseState::Open);
        let stalled = status.stalled.as_ref().unwrap();
        assert!(stalled.evidence.contains("retraction-proposed"));
        assert!(stalled.evidence.contains(id));
        assert!(stalled.evidence.contains("not achievable"));
        // The proposal is the home's stall judgement, made by the coder; the seat reads Stalled
        // once its supervision opens a stall record about it.
        assert_eq!(stalled.maker, Some(flotilla_resources::LeafMaker::Actor { vessel: "work".into(), role: "coder".into() }));
    }
    let status = convoys.get(&promise_crew).await.unwrap().status.unwrap();
    assert_eq!(status.promises["work"]["coder"][2].state, PromiseState::Retracted);
    watch.abort();
}
