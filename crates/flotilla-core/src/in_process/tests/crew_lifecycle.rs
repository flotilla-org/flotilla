use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use flotilla_protocol::{CrewCommandContext, DaemonEvent, HostName, NodeId};
use flotilla_resources::{
    Convoy as ResourceConvoy, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewWorkPhase, CrewWorkState, InMemoryBackend, InputMeta,
    ResourceBackend, TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalSession as ResourceTerminalSession,
    TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec as ResourceTerminalSessionSpec,
    TerminalSessionStatus as ResourceTerminalSessionStatus, Vessel, VesselSpec, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
};

use super::support::{
    claim_crew, resume_staging_fixture, stall_test_daemon, stall_test_session, stall_workflow_snapshot, test_meta, wait_for_stall,
};
use crate::config::ConfigStore;
use crate::daemon::DaemonHandle;
use crate::in_process::InProcessDaemon;
use crate::providers::discovery::test_support::fake_discovery;

#[tokio::test]
async fn crew_fail_requires_operator_force() {
    let (daemon, backend, _temp, watch) = stall_test_daemon().await;
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
    let convoy =
        convoys.create(&test_meta("operator-failure"), &ConvoySpec::builder().workflow_ref("test".into()).build()).await.expect("convoy");
    convoys
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("crew status");
    backend
        .clone()
        .using::<Vessel>("flotilla")
        .create(
            &test_meta("operator-failure-vessel"),
            &VesselSpec {
                convoy_ref: "operator-failure".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "test".into(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("vessel");
    daemon
        .crew_fail_internal(
            &CrewCommandContext {
                namespace: Some("flotilla".into()),
                convoy: Some("operator-failure".into()),
                vessel_ref: Some("operator-failure-vessel".into()),
                role: Some("coder".into()),
                ..Default::default()
            },
            "supervisor ruling".into(),
            true,
            Some(&principal),
        )
        .await
        .expect("operator force failure");
    let status = convoys.get("operator-failure").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Failed);
    watch.abort();
}

#[tokio::test]
async fn active_idle_crew_stalls_and_working_crew_clears() {
    let (daemon, backend, _temp, watch) = stall_test_daemon().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created =
        convoys.create(&test_meta("idle-crew"), &ConvoySpec::builder().workflow_ref("test".into()).build()).await.expect("convoy");
    convoys
        .update_status(
            "idle-crew",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                workflow_snapshot: Some({
                    let mut snapshot = stall_workflow_snapshot(vec![claim_crew("coder")]);
                    snapshot.stall_nudges.insert(
                        "work/coder".into(),
                        flotilla_resources::StallNudgePolicy { max_per_episode: 2, max_refusals: None, idle_grace_seconds: Some(3) },
                    );
                    snapshot
                }),
                work: BTreeMap::from([(
                    "work".into(),
                    flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Running).build(),
                )]),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("active status");
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    let session = sessions
        .create(
            &InputMeta::builder()
                .name("idle-session".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "idle-crew".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                    (ROLE_LABEL.into(), "coder".into()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "env".into(),
                role: "coder".into(),
                source: TerminalSessionSource::Tool { command: "test".into() },
                cwd: "/tmp".into(),
                env: Default::default(),
                pool: "test".into(),
            },
        )
        .await
        .expect("session");
    let mut idle = sessions
        .update_status(
            "idle-session",
            &session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::Idle,
                    as_of: Utc::now() - chrono::Duration::seconds(3),
                    source: TerminalAttentionSource::Screen,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("idle attention");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(convoys.get("idle-crew").await.expect("convoy").status.expect("status").stalled.is_none());
    let mut refreshed = idle.status.clone().expect("session status");
    refreshed.attention.as_mut().expect("attention").as_of = Utc::now();
    idle = sessions.update_status("idle-session", &idle.metadata.resource_version, &refreshed).await.expect("refreshed idle attention");
    let status = wait_for_stall(&backend, "idle-crew", true).await;
    let stalled = status.stalled.expect("stalled");
    assert!(matches!(stalled.maker, Some(flotilla_resources::LeafMaker::Actor { ref role, .. }) if role == "coder"));
    // The projectless fixture keeps its idle evidence and explains why the
    // default ProjectCrew rung cannot supply a supervisor (#2522).
    assert_eq!(stalled.evidence, "idle; cannot find governor: convoy has no project_ref");
    assert_eq!(stalled.source, flotilla_resources::StallEvidenceSource::Screen);
    let mut events = daemon.subscribe();
    let subscription = daemon
        .crew_ops
        .subscribe_wait(
            uuid::Uuid::new_v4(),
            flotilla_protocol::WaitSubscriptionRequest {
                namespace: "flotilla".into(),
                leaves: vec!["convoy/idle-crew .status.stalled == true".parse().expect("stall leaf")],
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
    let mut working = idle.status.expect("session status");
    working.attention =
        Some(TerminalAttention { state: TerminalAttentionState::Working, as_of: Utc::now(), source: TerminalAttentionSource::Screen });
    sessions.update_status("idle-session", &idle.metadata.resource_version, &working).await.expect("working attention");
    assert!(wait_for_stall(&backend, "idle-crew", false).await.stalled.is_none());
    watch.abort();
}

#[tokio::test]
async fn stale_working_attention_does_not_stall_crew() {
    let (_daemon, backend, _temp, watch) = stall_test_daemon().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created =
        convoys.create(&test_meta("stale-crew"), &ConvoySpec::builder().workflow_ref("test".into()).build()).await.expect("convoy");
    convoys
        .update_status(
            "stale-crew",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                workflow_snapshot: Some(stall_workflow_snapshot(vec![claim_crew("coder")])),
                work: BTreeMap::from([(
                    "work".into(),
                    flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Running).build(),
                )]),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("active status");
    stall_test_session(
        &backend,
        "stale-crew",
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
    assert!(convoys.get("stale-crew").await.expect("convoy").status.expect("status").stalled.is_none());
    watch.abort();
}

#[tokio::test]
async fn landing_done_crew_with_idle_session_has_no_actor_stall() {
    let (daemon, backend, _temp, watch) = stall_test_daemon().await;
    let repository = flotilla_resources::RepositoryKey("repo".into());
    let spec = ConvoySpec::builder()
        .workflow_ref("test".into())
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
    let created = convoys.create(&test_meta("landed-claim"), &spec).await.expect("convoy");
    let mut snapshot = stall_workflow_snapshot(vec![claim_crew("coder")]);
    snapshot.exit = Some(flotilla_resources::ExitDeclaration::Table(indexmap::IndexMap::from([(
        "shipped".into(),
        "$cr.state == merged".parse().expect("leaf template"),
    )])));
    convoys
        .update_status(
            "landed-claim",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Landing,
                workflow_snapshot: Some(snapshot),
                work: BTreeMap::from([(
                    "work".into(),
                    flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Complete).build(),
                )]),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Done).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("landing status");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if daemon
                .crew_ops
                .subscription_rows()
                .await
                .iter()
                .any(|row| matches!(&row.watcher, crate::leaf_engine::LeafWatcher::ReconcilerWake { convoy } if convoy == "landed-claim"))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("exit row armed");
    let records = backend.clone().using::<flotilla_resources::ChangeRequest>("flotilla");
    let record_name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 1392);
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
        &backend,
        "landed-claim",
        "idle-landing-session",
        "coder",
        TerminalAttention { state: TerminalAttentionState::Idle, as_of: Utc::now(), source: TerminalAttentionSource::Hook },
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1250)).await;
    assert!(convoys.get("landed-claim").await.expect("convoy").status.expect("status").stalled.is_none());
    watch.abort();
}

#[tokio::test]
async fn landing_without_armed_exit_rows_stalls() {
    let (_daemon, backend, _temp, watch) = stall_test_daemon().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created =
        convoys.create(&test_meta("unarmed-landing"), &ConvoySpec::builder().workflow_ref("test".into()).build()).await.expect("convoy");
    convoys
        .update_status(
            "unarmed-landing",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Landing,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: Vec::new(),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("landing status");
    let stalled = wait_for_stall(&backend, "unarmed-landing", true).await.stalled.expect("stalled");
    assert_eq!(stalled.maker, None);
    assert_eq!(stalled.evidence, "no armed row with an able maker");
    watch.abort();
}

#[tokio::test]
async fn stale_change_request_observation_stalls_landing_convoy() {
    let (_daemon, backend, _temp, watch) = stall_test_daemon().await;
    let repository = flotilla_resources::RepositoryKey("repo".into());
    let spec = ConvoySpec::builder()
        .workflow_ref("test".into())
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
    let created = convoys.create(&test_meta("stale-observation"), &spec).await.expect("convoy");
    convoys
        .update_status(
            "stale-observation",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Landing,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: Some(flotilla_resources::ExitDeclaration::Table(indexmap::IndexMap::from([(
                        "shipped".into(),
                        "$cr.state == merged".parse().expect("leaf template"),
                    )]))),
                    turn_delivery: Default::default(),
                    vessels: Vec::new(),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("landing status");
    let records = backend.clone().using::<flotilla_resources::ChangeRequest>("flotilla");
    let record_name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 1391);
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
    let stalled = wait_for_stall(&backend, "stale-observation", true).await.stalled.expect("stalled");
    assert!(matches!(stalled.maker, Some(flotilla_resources::LeafMaker::Observed { .. })));
    assert!(stalled.evidence.starts_with("stale"));
    assert_eq!(stalled.source, flotilla_resources::StallEvidenceSource::Observation);
    watch.abort();
}

#[tokio::test]
async fn idle_standing_role_without_obligation_never_stalls_convoy() {
    let (_daemon, backend, _temp, watch) = stall_test_daemon().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys.create(&test_meta("standing"), &ConvoySpec::builder().workflow_ref("test".into()).build()).await.expect("convoy");
    convoys
        .update_status(
            "standing",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                workflow_snapshot: Some(stall_workflow_snapshot(vec![flotilla_resources::CrewSpec::builder()
                    .role("governor".into())
                    .source(flotilla_resources::CrewSource::Tool { command: "test".into() })
                    .build()])),
                work: BTreeMap::from([(
                    "work".into(),
                    flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Running).build(),
                )]),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("governor".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("active status");
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    for (name, role, state) in [("governor-session", "governor", TerminalAttentionState::Idle)] {
        let session = sessions
            .create(
                &InputMeta::builder()
                    .name(name.into())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.into(), "standing".into()),
                        (VESSEL_LABEL.into(), "work".into()),
                        (ROLE_LABEL.into(), role.into()),
                    ]))
                    .build(),
                &ResourceTerminalSessionSpec {
                    env_ref: "env".into(),
                    role: role.into(),
                    source: TerminalSessionSource::Tool { command: "test".into() },
                    cwd: "/tmp".into(),
                    env: Default::default(),
                    pool: "test".into(),
                },
            )
            .await
            .expect("session");
        sessions
            .update_status(
                name,
                &session.metadata.resource_version,
                &ResourceTerminalSessionStatus {
                    phase: ResourceTerminalSessionPhase::Running,
                    attention: Some(TerminalAttention {
                        state,
                        as_of: Utc::now() - chrono::Duration::seconds(10),
                        source: TerminalAttentionSource::Screen,
                    }),
                    ..Default::default()
                },
            )
            .await
            .expect("attention");
    }
    tokio::time::sleep(std::time::Duration::from_millis(1250)).await;
    assert!(convoys.get("standing").await.expect("convoy").status.expect("status").stalled.is_none());
    watch.abort();
}

#[tokio::test]
async fn replica_wake_engine_does_not_write_stalled_condition() {
    let authority = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("authority"));
    let convoys = authority.clone().using::<ResourceConvoy>("flotilla");
    let created =
        convoys.create(&test_meta("replicated"), &ConvoySpec::builder().workflow_ref("test".into()).build()).await.expect("convoy");
    convoys
        .update_status(
            "replicated",
            &created.metadata.resource_version,
            &ConvoyStatus { phase: flotilla_resources::ConvoyPhase::Landing, ..Default::default() },
        )
        .await
        .expect("landing status");
    let replica = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("replica"));
    replica
        .replica_writer::<ResourceConvoy>(NodeId::new("authority"), "flotilla")
        .replace(&convoys.list().await.expect("authority list"), Utc::now())
        .await
        .expect("replicate");
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"replica-stall-test\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("replica"),
        replica.clone(),
    )
    .await;
    let (sender, _receiver) = flotilla_resources::controller::WorkQueueSender::channel();
    let watch = daemon.reconciler_wake_watch();
    let replica_check = replica.clone();
    let task = tokio::spawn(async move { watch.spawn(replica.clone(), "flotilla".into(), sender).await.expect("replica watch") });
    tokio::time::sleep(std::time::Duration::from_millis(1250)).await;
    assert!(!task.is_finished(), "replica watch must remain healthy");
    assert!(replica_check.using::<ResourceConvoy>("flotilla").list().await.expect("local convoys").items.is_empty());
    assert!(replica_check
        .including_replicas::<ResourceConvoy>("flotilla")
        .get("replicated")
        .await
        .expect("replicated convoy")
        .object
        .status
        .expect("replica status")
        .stalled
        .is_none());
    assert!(convoys.get("replicated").await.expect("authority convoy").status.expect("status").stalled.is_none());
    task.abort();
}

// Process exit leaves unfinished work resumable in its existing checkout. An
// operator follow-up relaunches a stopped session instead of waiting for a hook
// that an exited agent cannot send; both observed and not-yet-observed exits work.
#[tokio::test]
async fn resume_relaunches_exited_active_and_interrupted_crew() {
    for phase in [CrewWorkPhase::Working, CrewWorkPhase::Interrupted] {
        let (daemon, backend, probe) = resume_staging_fixture().await;
        probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
        let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
        let convoy = convoys.get("resume-staging").await.expect("convoy");
        let mut status = convoy.status.expect("status");
        status.phase = ConvoyPhase::Active;
        status.work.get_mut("work").expect("work").phase = flotilla_resources::WorkPhase::Interrupted;
        status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = phase;
        convoys.update_status("resume-staging", &convoy.metadata.resource_version, &status).await.expect("unfinished work");
        let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
        let session = sessions.get("resume-staging-session").await.expect("session");
        let original_cwd = session.spec.cwd.clone();
        sessions
            .update_status(
                &session.metadata.name,
                &session.metadata.resource_version,
                &ResourceTerminalSessionStatus {
                    phase: ResourceTerminalSessionPhase::Stopped,
                    inner_command_status: Some(flotilla_resources::InnerCommandStatus::Exited),
                    ..Default::default()
                },
            )
            .await
            .expect("agent exited");
        daemon
            .convoy_resume_internal("flotilla", "resume-staging", "Recover the unfinished review", Some("work"), Some("coder"))
            .await
            .expect("resume exited agent");
        let session = sessions.get("resume-staging-session").await.expect("same session");
        assert_eq!(session.spec.cwd, original_cwd);
        assert_eq!(session.status.expect("session status").phase, ResourceTerminalSessionPhase::Starting);
        assert!(matches!(session.spec.source, TerminalSessionSource::Agent { message: None, .. }));
        assert!(backend
            .using::<flotilla_resources::Message>("flotilla")
            .list()
            .await
            .expect("inbox")
            .items
            .iter()
            .any(|message| message.spec.body.contains("Recover the unfinished review")));
        let status = convoys.get("resume-staging").await.expect("convoy").status.expect("status");
        assert!(status.pending_brief().is_none(), "an exited process cannot consume a pending brief");
        assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    }
}
