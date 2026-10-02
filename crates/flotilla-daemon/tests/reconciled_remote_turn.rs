use std::{collections::BTreeMap, sync::Arc};

use chrono::Utc;
use flotilla_controllers::reconcilers::VesselReconciler;
use flotilla_core::{
    config::ConfigStore,
    in_process::{ConvoyResumeOutcome, InProcessDaemon},
    leaf_engine::TurnDeliveryRequest,
    providers::discovery::test_support::fake_discovery,
};
use flotilla_protocol::{HostName, NodeId};
use flotilla_resources::{
    controller::{Actuation, Reconciler},
    ClaimExit, Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewMessageSender, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState,
    Environment, EnvironmentPhase, EnvironmentSpec, EnvironmentStatus, ExitDeclaration, HostDirectEnvironmentSpec,
    HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, InputMeta, PlacementPolicy, PlacementPolicySpec, Resource,
    ResourceBackend, ResourceObject, Selector, SqliteBackend, TerminalCrewMessage, TerminalSession, TerminalSessionPhase,
    TerminalSessionSource, TerminalSessionStatus, Vessel, VesselRequirement, VesselSpec, WorkPhase, WorkState, WorkflowSnapshot,
    ACTUATOR_SOURCE_ROOT_ANNOTATION,
};

fn meta(name: &str) -> InputMeta {
    InputMeta::builder().name(name.to_string()).build()
}

async fn replicate<T: Resource>(source: &ResourceBackend, destination: &ResourceBackend, source_root: &str) {
    let objects = source.clone().using::<T>("flotilla").list().await.expect("source list");
    destination.replica_writer::<T>(NodeId::new(source_root), "flotilla").replace(&objects, Utc::now()).await.expect("replicate resources");
}

fn agent_message(session: ResourceObject<TerminalSession>) -> TerminalCrewMessage {
    let TerminalSessionSource::Agent { message: Some(message), .. } = session.spec.source else { panic!("agent message queued") };
    message
}

#[tokio::test]
async fn reconciled_remote_session_receives_nudge_and_resume_from_convoy_home() {
    let home = ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("home store")).with_local_root(NodeId::new("home"));
    let placement =
        ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("placement store")).with_local_root(NodeId::new("placement"));
    let home_dir = tempfile::tempdir().expect("home config");
    std::fs::write(home_dir.path().join("daemon.toml"), "machine_id = \"home\"\n").expect("home identity");
    let home_daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(home_dir.path())),
        fake_discovery(false),
        HostName::local(),
        home.clone(),
    )
    .await;
    let placement_dir = tempfile::tempdir().expect("placement config");
    std::fs::write(placement_dir.path().join("daemon.toml"), "machine_id = \"placement\"\n").expect("placement identity");
    let placement_daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(placement_dir.path())),
        fake_discovery(false),
        HostName::local(),
        placement.clone(),
    )
    .await;

    let convoys = home.clone().using::<Convoy>("flotilla");
    let created =
        convoys.create(&meta("nudge-convoy"), &ConvoySpec::builder().workflow_ref("wf".to_string()).build()).await.expect("convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Active,
            workflow_snapshot: Some(WorkflowSnapshot {
                exit: Some(ExitDeclaration::Claim(ClaimExit)),
                turn_delivery: Default::default(),
                stall_nudges: Default::default(),
                supervision: None,
                vessels: vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .crew(vec![CrewSpec::builder()
                        .role("coder".to_string())
                        .source(CrewSource::Agent {
                            selector: Selector::for_capability("coding"),
                            prompt: Some("Initial".to_string()),
                            brief_template: None,
                        })
                        .build()])
                    .build()],
            }),
            work: BTreeMap::from([("work".to_string(), WorkState::builder().phase(WorkPhase::Running).build())]),
            crew_work: BTreeMap::from([(
                "work".to_string(),
                BTreeMap::from([("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
            )]),
            ..Default::default()
        })
        .await
        .expect("working crew");
    replicate::<Convoy>(&home, &placement, "home").await;
    home.using::<PlacementPolicy>("flotilla")
        .create(
            &meta("placement-policy"),
            &PlacementPolicySpec::builder()
                .pool("cleat".to_string())
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: "placement-host".to_string(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
        )
        .await
        .expect("placement policy");
    replicate::<PlacementPolicy>(&home, &placement, "home").await;
    let environments = placement.clone().using::<Environment>("flotilla");
    let environment = environments
        .create(&meta("host-direct-placement-host"), &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec {
                host_ref: "placement-host".to_string(),
                repo_default_dir: "/workspace".to_string(),
            }),
            docker: None,
        })
        .await
        .expect("placement environment");
    environments
        .update_status(&environment.metadata.name, &environment.metadata.resource_version, &EnvironmentStatus {
            phase: EnvironmentPhase::Ready,
            ready: true,
            ..Default::default()
        })
        .await
        .expect("ready environment");
    let vessel = placement
        .using::<Vessel>("flotilla")
        .create(
            &InputMeta::builder()
                .name("nudge-convoy-work".to_string())
                .annotations(BTreeMap::from([(ACTUATOR_SOURCE_ROOT_ANNOTATION.to_string(), "home".to_string())]))
                .build(),
            &VesselSpec {
                convoy_ref: "nudge-convoy".to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "placement-policy".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("placement vessel");
    let reconciler = VesselReconciler::new(placement.clone(), "flotilla")
        .with_federated_dependencies(&placement, flotilla_protocol::CanonicalHostId::resolved("placement-host"));
    let dependencies = reconciler.prepare(&vessel).await.expect("vessel dependencies");
    let (session_meta, session_spec) = reconciler
        .reconcile(&vessel, &dependencies, Utc::now())
        .actuations
        .into_iter()
        .find_map(|actuation| match actuation {
            Actuation::CreateTerminalSession { meta, spec } => Some((meta, spec)),
            _ => None,
        })
        .expect("vessel reconciler creates crew terminal");
    let sessions = placement.clone().using::<TerminalSession>("flotilla");
    let session = sessions.create(&session_meta, &session_spec).await.expect("persist reconciled terminal");
    sessions
        .update_status(&session.metadata.name, &session.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            ..Default::default()
        })
        .await
        .expect("running terminal");
    let convoy = convoys.get("nudge-convoy").await.expect("working convoy");
    let mut status = convoy.status.expect("working status");
    status.crew_work.get_mut("work").expect("work crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Stalled;
    convoys.update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &status).await.expect("stall crew");
    replicate::<TerminalSession>(&placement, &home, "placement").await;

    let request = TurnDeliveryRequest::builder()
        .namespace("flotilla".to_string())
        .convoy("nudge-convoy".to_string())
        .source("stall-nudge-1".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .brief("Please continue".to_string())
        .subject_revision("stall-1".to_string())
        .sender(CrewMessageSender::FlotillaNudge)
        .build();
    let mut disallowed = request.clone();
    disallowed.sender = CrewMessageSender::OperatorResume { principal: None };
    let error = home_daemon.deliver_standing_turn(&disallowed).await.expect_err("operator cannot queue a remote turn");
    assert!(error.contains("sender is not permitted"), "{error}");
    let mut missing = request.clone();
    missing.role = "missing".to_string();
    let error = home_daemon.deliver_standing_turn(&missing).await.expect_err("nudge requires a remote session");
    assert!(error.contains("has no durable terminal-session record"), "{error}");
    home_daemon.deliver_standing_turn(&request).await.expect("nudge accepted at convoy home");
    let queued = convoys.get("nudge-convoy").await.expect("queued convoy");
    assert!(queued.status.expect("status").turn_deliveries.values().any(|delivery| delivery.pending_supervisor_turn.is_some()));
    replicate::<Convoy>(&home, &placement, "home").await;
    placement_daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect("deliver at placement");
    let delivered = sessions.get(&session_meta.name).await.expect("delivered session");
    let nudge = agent_message(delivered.clone());
    assert!(nudge.text.contains("Your stall is recorded. What changed since your report?"));
    assert!(matches!(nudge.sender, CrewMessageSender::FlotillaNudge));
    let nudge_id = nudge.id.clone();
    let mut delivered_status = delivered.status.expect("running terminal status");
    delivered_status.delivered_message_id = Some(nudge_id.clone());
    sessions
        .update_status(&session_meta.name, &delivered.metadata.resource_version, &delivered_status)
        .await
        .expect("confirm nudge delivery");
    replicate::<TerminalSession>(&placement, &home, "placement").await;
    home_daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect("acknowledge nudge at convoy home");

    let convoy = convoys.get("nudge-convoy").await.expect("convoy after nudge");
    let mut status = convoy.status.expect("convoy status");
    status.crew_work.get_mut("work").expect("work crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Stalled;
    convoys.update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &status).await.expect("stall crew again");
    let outcome = home_daemon
        .convoy_resume_internal("flotilla", "nudge-convoy", "Resume this turn", Some("work"), Some("coder"))
        .await
        .expect("resume reconciled remote session");
    assert!(matches!(outcome, ConvoyResumeOutcome::Queued { .. }));
    let resumed_convoy = convoys.get("nudge-convoy").await.expect("resumed convoy");
    assert!(resumed_convoy.status.as_ref().and_then(|status| status.crew_work["work"]["coder"].resumed_at).is_some());
    let mut racing_nudge = request.clone();
    racing_nudge.source = "stall-nudge-2".to_string();
    racing_nudge.subject_revision = "stall-2".to_string();
    home_daemon.deliver_standing_turn(&racing_nudge).await.expect("racing nudge accepted");
    replicate::<Convoy>(&home, &placement, "home").await;
    placement_daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect("deliver resume at placement");
    let resumed_session = sessions.get(&session_meta.name).await.expect("resumed session");
    let resumed = agent_message(resumed_session.clone());
    let resume = resumed.next_after(Some(&nudge_id)).expect("resume follows nudge");
    assert!(resume.text.contains("Resume this turn"));
    assert!(matches!(resume.sender, CrewMessageSender::OperatorResume { .. }));
    let resume_id = resume.id.clone();
    let mut status = resumed_session.status.expect("terminal status");
    status.delivered_message_id = Some(resume_id);
    sessions.update_status(&session_meta.name, &resumed_session.metadata.resource_version, &status).await.expect("resume delivered");
    placement_daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect("queue nudge after resume");
    let after_race = sessions.get(&session_meta.name).await.expect("session after race");
    let messages = agent_message(after_race.clone());
    assert_eq!(messages.following.len(), 2);
    assert!(messages.following[0].text.contains("Resume this turn"));
    assert!(matches!(messages.following[1].sender, CrewMessageSender::FlotillaNudge));
    let mut status = after_race.status.expect("terminal status");
    status.delivered_message_id = Some(messages.following[1].id.clone());
    sessions.update_status(&session_meta.name, &after_race.metadata.resource_version, &status).await.expect("both turns delivered");
    replicate::<TerminalSession>(&placement, &home, "placement").await;
    home_daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect("acknowledge both turns");
    let convoy = convoys.get("nudge-convoy").await.expect("convoy after acknowledgments");
    assert!(!convoy.status.expect("status").turn_deliveries.values().any(|delivery| delivery.pending_supervisor_turn.is_some()));
}
