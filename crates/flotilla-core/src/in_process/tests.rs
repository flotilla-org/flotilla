use std::collections::{BTreeMap, BTreeSet};

use chrono::TimeZone;
use flotilla_resources::{
    controller_patches, ConvoyEnsureStatus, ConvoyProvisioningState, ConvoyStatus, CredentialConsumer, CredentialExpiry, CredentialGrant,
    CredentialGrantSelector, CredentialGrantSpec, CredentialLifecycle, CredentialPlacementRequirements, CredentialSource, CredentialSpec,
    CredentialSpecSpec, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, DemandStatusPatch, Environment as ResourceEnvironment,
    EnvironmentPhase, EnvironmentSpec as ResourceEnvironmentSpec, EnvironmentStatus as ResourceEnvironmentStatus, Event, FulfilmentFacts,
    FulfilmentKindSpec, FulfilmentRealisation, HarnessFacts, HostCondition, HostDirectEnvironmentSpec, HostDirectPlacementPolicyCheckout,
    HostDirectPlacementPolicySpec, HostSpec, HostStatus, PlacementPolicy, PlacementPolicySpec, ProjectRepositoryRole,
    ProjectRepositorySpec, RepositoryStatus, Selector, TerminalAttention, TerminalAttentionSource, TerminalAttentionState,
    TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource,
    TerminalSessionSpec as ResourceTerminalSessionSpec, TerminalSessionStatus as ResourceTerminalSessionStatus, VesselRequirement,
    VesselSpec, VirtualClock, WorkflowTemplateSpec, AGENT_ADAPTERS_CAPABILITY, AUTHORITY_LABEL, CONVOY_LABEL, GENERATION_LABEL,
    PROJECT_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_REF_LABEL,
};

use super::*;

#[test]
fn bound_change_request_identity_uses_matching_declared_or_discovered_subject() {
    let requested = ChangeRequestRef { namespace: "flotilla".into(), service: "github.com".into(), scope: "team/repo".into(), number: 42 };
    let subject = flotilla_protocol::Subject {
        kind: flotilla_protocol::SubjectKind::ChangeRequest,
        source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "team/repo".into() },
        id: "42".into(),
    };
    let spec = ConvoySpec::builder().workflow_ref("review".to_string()).build();
    let mut status = ConvoyStatus::default();
    status.discover_subject(
        subject.clone(),
        flotilla_protocol::Relationship::Produces,
        flotilla_resources::SubjectDiscoverySource::Claim,
        Utc::now(),
    );
    status.workflow_snapshot = Some(flotilla_resources::WorkflowSnapshot {
        exit: None,
        turn_delivery: Default::default(),
        stall_nudges: Default::default(),
        supervision: None,
        vessels: vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(Vec::new())
            .credential_refs(BTreeSet::from(["github-crew-pr".to_string()]))
            .build()],
    });
    let mut convoy = ResourceObject::<ResourceConvoy> {
        metadata: ObjectMeta {
            name: "review-convoy".to_string(),
            namespace: "flotilla".to_string(),
            resource_version: "1".to_string(),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: Utc::now(),
            merge: None,
        },
        spec,
        status: Some(status),
    };
    let bound = convoy_change_request_credential_refs(&convoy, &requested).expect("active PR subjects");
    assert_eq!(bound.numbers, BTreeSet::from([42]));
    assert_eq!(bound.credentials_by_number[&42], BTreeSet::from(["github-crew-pr".to_string()]));
    let mut unrelated = requested.clone();
    unrelated.scope = "team/other".into();
    assert!(convoy_change_request_credential_refs(&convoy, &unrelated).expect("other scope").numbers.is_empty());
    convoy.status.as_mut().expect("status").discover_subject(
        subject,
        flotilla_protocol::Relationship::Supersedes,
        flotilla_resources::SubjectDiscoverySource::Operator,
        Utc::now(),
    );
    assert!(convoy_change_request_credential_refs(&convoy, &requested).expect("superseded PR").numbers.is_empty());
}

#[tokio::test]
async fn operator_brief_survives_a_racing_nudge_until_delivery() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions
        .create(&test_meta("racing-messages"), &ResourceTerminalSessionSpec {
            env_ref: "env".to_string(),
            role: "coder".to_string(),
            source: TerminalSessionSource::Agent {
                selector: Selector::for_capability("coding"),
                brief: flotilla_resources::TerminalBrief {
                    path: "brief.md".to_string(),
                    content: "Initial".to_string(),
                    artifact_digest: None,
                    copies: Vec::new(),
                },
                context: Box::new(flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".to_string(),
                    convoy: "convoy".to_string(),
                    vessel_ref: "work".to_string(),
                }),
                message: None,
            },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("session");
    queue_pending_crew_message(&sessions, &session, CrewMessageSender::OperatorResume { principal: None }, "Continue the work")
        .await
        .expect("operator brief queued");
    let queued = sessions.get("racing-messages").await.expect("queued operator brief");
    queue_pending_crew_message(&sessions, &queued, CrewMessageSender::FlotillaNudge, "Please settle").await.expect("nudge queued");
    let after_race = sessions.get("racing-messages").await.expect("session after race");
    let TerminalSessionSource::Agent { message: Some(message), .. } = after_race.spec.source else { panic!("operator brief was lost") };
    assert_eq!(message.text, "[operator (unattributed) · via convoy resume]\n\nContinue the work");
    assert_eq!(message.following.len(), 1);
    assert!(matches!(message.following[0].sender, CrewMessageSender::FlotillaNudge));
    assert_eq!(message.next_after(Some(&message.id)).map(|next| next.id.as_str()), Some(message.following[0].id.as_str()));

    let concurrent = sessions.create(&test_meta("concurrent-messages"), &session.spec).await.expect("concurrent session");
    let (operator, nudge) = tokio::join!(
        queue_pending_crew_message(&sessions, &concurrent, CrewMessageSender::OperatorResume { principal: None }, "New guidance"),
        queue_pending_crew_message(&sessions, &concurrent, CrewMessageSender::FlotillaNudge, "Please settle"),
    );
    operator.expect("operator brief survives concurrent write");
    nudge.expect("nudge does not erase concurrent brief");
    let concurrent = sessions.get("concurrent-messages").await.expect("session after concurrent writes");
    let TerminalSessionSource::Agent { message: Some(head), .. } = concurrent.spec.source else { panic!("concurrent messages lost") };
    assert_eq!(head.following.len(), 1);
    assert!(std::iter::once(&head).chain(head.following.iter()).any(|message| message.text.contains("New guidance")));
}

#[test]
fn turn_delivery_restarts_a_lost_session() {
    assert_eq!(
        turn_delivery_session_plan(Some(ResourceTerminalSessionPhase::Lost), "work", "coder").expect("delivery plan"),
        TurnDeliverySessionPlan::RestartFresh
    );
}

async fn replicate_turn_delivery_resources<T: Resource>(
    source: &ResourceBackend,
    destination: &ResourceBackend,
    source_root: &str,
    context: &str,
) {
    let objects = source.clone().using::<T>("flotilla").list().await.expect(context);
    destination.replica_writer::<T>(NodeId::new(source_root), "flotilla").replace(&objects, Utc::now()).await.expect(context);
}

#[tokio::test]
async fn standing_governor_on_another_host_receives_a_stalled_crew_turn() {
    let home = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("home"));
    let placement = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("placement"));
    let temp = tempfile::tempdir().expect("config dir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"home\"\n").expect("machine identity");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::local(),
        home.clone(),
    )
    .await;
    let placement_config = tempfile::tempdir().expect("placement config dir");
    std::fs::write(placement_config.path().join("daemon.toml"), "machine_id = \"placement\"\n").expect("placement identity");
    let placement_daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(placement_config.path())),
        fake_discovery(false),
        HostName::local(),
        placement.clone(),
    )
    .await;
    let convoys = home.clone().using::<ResourceConvoy>("flotilla");
    let governor = convoys
        .create(
            &test_meta("governor-convoy"),
            &ConvoySpec::builder().workflow_ref("governor".to_string()).role("governor".to_string()).build(),
        )
        .await
        .expect("governor convoy");
    let existing_attention = flotilla_resources::ConvoyAttention {
        source: "existing-attention".to_string(),
        reason: "Existing concern".to_string(),
        raised_at: Utc::now(),
    };
    convoys
        .update_status(&governor.metadata.name, &governor.metadata.resource_version, &ConvoyStatus {
            phase: flotilla_resources::ConvoyPhase::Active,
            work: BTreeMap::from([(
                "govern".to_string(),
                flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Running).build(),
            )]),
            crew_work: BTreeMap::from([(
                "govern".to_string(),
                BTreeMap::from([("governor".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
            )]),
            attention: Some(existing_attention.clone()),
            ..Default::default()
        })
        .await
        .expect("running governor");
    let sessions = placement.clone().using::<ResourceTerminalSession>("flotilla");
    let session = sessions
        .create(
            &InputMeta::builder()
                .name("governor-terminal".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "governor-convoy".to_string()),
                    (VESSEL_LABEL.to_string(), "govern".to_string()),
                    (ROLE_LABEL.to_string(), "governor".to_string()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "remote-env".to_string(),
                role: "governor".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector::for_capability("governor"),
                    brief: flotilla_resources::TerminalBrief {
                        path: "brief.md".to_string(),
                        content: "standing brief".to_string(),
                        artifact_digest: None,
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "governor-convoy".to_string(),
                        vessel_ref: "governor-convoy-govern".to_string(),
                    }),
                    message: None,
                },
                cwd: "/workspace".to_string(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("governor terminal on placement host");
    sessions
        .update_status(&session.metadata.name, &session.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            ..Default::default()
        })
        .await
        .expect("running terminal");
    let request = crate::leaf_engine::TurnDeliveryRequest::builder()
        .namespace("flotilla".to_string())
        .convoy("governor-convoy".to_string())
        .source("supervision-stalled-crew".to_string())
        .vessel("govern".to_string())
        .role("governor".to_string())
        .brief("Supervise the stalled crew".to_string())
        .subject_revision("stall-1".to_string())
        .sender(CrewMessageSender::FlotillaEscalation { from: "coder@work".to_string() })
        .build();
    daemon.deliver_standing_turn(&request).await.expect("remote governor turn accepted");
    let queued = convoys.get("governor-convoy").await.expect("governor convoy after turn");
    assert_eq!(queued.status.as_ref().and_then(|status| status.attention.as_ref()), Some(&existing_attention));
    assert!(queued
        .status
        .as_ref()
        .is_some_and(|status| status.turn_deliveries.values().any(|delivery| delivery.pending_supervisor_turn.is_some())));
    placement
        .replica_writer::<ResourceConvoy>(NodeId::new("home"), "flotilla")
        .replace(&convoys.list().await.expect("home convoys"), Utc::now())
        .await
        .expect("replicate queued turn to placement host");
    placement_daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect("placement consumes turn");
    let delivered = sessions.get("governor-terminal").await.expect("governor terminal after turn");
    let TerminalSessionSource::Agent { message: Some(message), .. } = delivered.spec.source else { panic!("governor turn queued") };
    assert_eq!(message.text, "[flotilla · escalated from coder@work · supervise the stalled crew]\n\nSupervise the stalled crew");
    let delivered = sessions.get("governor-terminal").await.expect("governor terminal for acknowledgment");
    let mut delivered_status = delivered.status.expect("running terminal status");
    delivered_status.delivered_message_id = Some(message.id);
    sessions
        .update_status("governor-terminal", &delivered.metadata.resource_version, &delivered_status)
        .await
        .expect("confirm governor delivery");
    home.replica_writer::<ResourceTerminalSession>(NodeId::new("placement"), "flotilla")
        .replace(&sessions.list().await.expect("confirmed placement terminals"), Utc::now())
        .await
        .expect("replicate delivery confirmation");
    let bad_sessions = home.clone().using::<ResourceTerminalSession>("flotilla");
    let bad = bad_sessions
        .create(&InputMeta::builder().name("bad-local-terminal".to_string()).labels(session.metadata.labels.clone()).build(), &session.spec)
        .await
        .expect("failed local session");
    bad_sessions
        .update_status(&bad.metadata.name, &bad.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Failed,
            ..Default::default()
        })
        .await
        .expect("mark local session failed");
    let error = daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect_err("failed session is reported");
    assert!(error.contains("bad-local-terminal"));
    let acknowledged = convoys.get("governor-convoy").await.expect("governor convoy after acknowledgment");
    let status = acknowledged.status.expect("governor status");
    assert!(!status.turn_deliveries.values().any(|delivery| delivery.pending_supervisor_turn.is_some()));
    assert_eq!(status.attention, Some(existing_attention));
}

#[tokio::test]
async fn remote_session_receives_nudge_and_resume_from_convoy_home() {
    let home = ResourceBackend::Sqlite(flotilla_resources::SqliteBackend::open_in_memory().expect("home store"))
        .with_local_root(NodeId::new("home"));
    let placement = ResourceBackend::Sqlite(flotilla_resources::SqliteBackend::open_in_memory().expect("placement store"))
        .with_local_root(NodeId::new("placement"));
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
    let convoys = home.clone().using::<ResourceConvoy>("flotilla");
    let created =
        convoys.create(&test_meta("nudge-convoy"), &ConvoySpec::builder().workflow_ref("wf".to_string()).build()).await.expect("convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
            phase: flotilla_resources::ConvoyPhase::Active,
            workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                exit: Some(flotilla_resources::ExitDeclaration::Claim(flotilla_resources::ClaimExit)),
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
            work: BTreeMap::from([(
                "work".to_string(),
                flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Running).build(),
            )]),
            crew_work: BTreeMap::from([(
                "work".to_string(),
                BTreeMap::from([("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
            )]),
            ..Default::default()
        })
        .await
        .expect("working crew");
    let meta = InputMeta::builder()
        .name("nudge-convoy-work-coder".to_string())
        .labels(BTreeMap::from([
            (CONVOY_LABEL.to_string(), "nudge-convoy".to_string()),
            (VESSEL_LABEL.to_string(), "work".to_string()),
            (ROLE_LABEL.to_string(), "coder".to_string()),
        ]))
        .build();
    // Turn delivery reads the session's labels and source without resolving its environment.
    let spec = ResourceTerminalSessionSpec {
        env_ref: "placement-environment".to_string(),
        role: "coder".to_string(),
        source: TerminalSessionSource::Agent {
            selector: Selector::for_capability("coding"),
            brief: flotilla_resources::TerminalBrief {
                path: "brief.md".to_string(),
                content: "Initial".to_string(),
                artifact_digest: None,
                copies: Vec::new(),
            },
            context: Box::new(flotilla_resources::TerminalCrewContext {
                namespace: "flotilla".to_string(),
                convoy: "nudge-convoy".to_string(),
                vessel_ref: "nudge-convoy-work".to_string(),
            }),
            message: None,
        },
        cwd: "/workspace".to_string(),
        pool: "cleat".to_string(),
    };
    let sessions = placement.clone().using::<ResourceTerminalSession>("flotilla");
    let session = sessions.create(&meta, &spec).await.expect("persist remote terminal");
    let convoy = convoys.get("nudge-convoy").await.expect("working convoy");
    let mut status = convoy.status.expect("working status");
    status.crew_work.get_mut("work").expect("work crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Stalled;
    convoys.update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &status).await.expect("stall crew");
    sessions
        .update_status(&session.metadata.name, &session.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            ..Default::default()
        })
        .await
        .expect("running terminal");
    replicate_turn_delivery_resources::<ResourceTerminalSession>(&placement, &home, "placement", "replicate session").await;

    let request = crate::leaf_engine::TurnDeliveryRequest::builder()
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
    let convoy = convoys.get("nudge-convoy").await.expect("queued convoy");
    assert!(convoy.status.expect("status").turn_deliveries.values().any(|delivery| delivery.pending_supervisor_turn.is_some()));
    replicate_turn_delivery_resources::<ResourceConvoy>(&home, &placement, "home", "replicate turn").await;
    placement_daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect("deliver at placement");
    let delivered = sessions.get(&meta.name).await.expect("delivered session");
    let TerminalSessionSource::Agent { message: Some(message), .. } = delivered.spec.source else { panic!("nudge queued") };
    assert!(message.text.contains("Your stall is recorded. What changed since your report?"));
    let nudge_id = message.id.clone();
    assert!(matches!(message.sender, CrewMessageSender::FlotillaNudge));
    let mut delivered_status = delivered.status.expect("running terminal status");
    delivered_status.delivered_message_id = Some(nudge_id.clone());
    sessions.update_status(&meta.name, &delivered.metadata.resource_version, &delivered_status).await.expect("confirm nudge delivery");
    replicate_turn_delivery_resources::<ResourceTerminalSession>(&placement, &home, "placement", "replicate nudge confirmation").await;
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
    replicate_turn_delivery_resources::<ResourceConvoy>(&home, &placement, "home", "replicate resume").await;
    placement_daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect("deliver resume at placement");
    let resumed = sessions.get(&meta.name).await.expect("resumed session");
    let TerminalSessionSource::Agent { message: Some(head), .. } = resumed.spec.source else { panic!("resume queued") };
    let resumed_message = head.next_after(Some(&nudge_id)).expect("resume follows delivered nudge");
    assert!(resumed_message.text.contains("Resume this turn"));
    assert!(matches!(resumed_message.sender, CrewMessageSender::OperatorResume { .. }));
    let resume_id = resumed_message.id.clone();
    let mut status = resumed.status.expect("terminal status");
    status.delivered_message_id = Some(resume_id);
    sessions.update_status(&meta.name, &resumed.metadata.resource_version, &status).await.expect("resume delivered");
    placement_daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect("queue nudge after resume");
    let after_race = sessions.get(&meta.name).await.expect("session after race");
    let TerminalSessionSource::Agent { message: Some(head), .. } = after_race.spec.source else { panic!("resume lost") };
    assert_eq!(head.following.len(), 2);
    assert!(head.following[0].text.contains("Resume this turn"));
    assert!(matches!(head.following[1].sender, CrewMessageSender::FlotillaNudge));
    let last_id = head.following[1].id.clone();
    let mut status = after_race.status.expect("terminal status");
    status.delivered_message_id = Some(last_id);
    sessions.update_status(&meta.name, &after_race.metadata.resource_version, &status).await.expect("both turns delivered");
    replicate_turn_delivery_resources::<ResourceTerminalSession>(&placement, &home, "placement", "replicate both deliveries").await;
    home_daemon.reconcile_pending_supervisor_turns_once("flotilla").await.expect("acknowledge both turns");
    let convoy = convoys.get("nudge-convoy").await.expect("convoy after acknowledgments");
    assert!(!convoy.status.expect("status").turn_deliveries.values().any(|delivery| delivery.pending_supervisor_turn.is_some()));
}

#[test]
fn placement_tiebreak_orders_live_minimal_candidates_by_availability_then_cost() {
    let now = chrono::Utc::now();
    let needs = BTreeSet::new();
    let placement_tiebreak = PlacementTieBreak { needs: &needs, now };
    let candidate = |name: &str, cost_class, ready, sleeping_until, slots| {
        let metadata = flotilla_resources::ObjectMeta {
            name: name.to_string(),
            namespace: "flotilla".to_string(),
            resource_version: "1".to_string(),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: now,
            merge: None,
        };
        let policy = ResourceObject::<PlacementPolicy> {
            metadata: metadata.clone(),
            spec: PlacementPolicySpec::builder()
                .pool("test".to_string())
                .priority(0)
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: "host".to_string(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
            status: None,
        };
        KindCandidate {
            kind: ResourceObject::<FulfilmentKind> {
                metadata,
                spec: FulfilmentKindSpec::builder()
                    .host_ref("host".to_string())
                    .pool("test".to_string())
                    .cost_class(cost_class)
                    .realisation(FulfilmentRealisation::HostDirect)
                    .build(),
                status: None,
            },
            placement: PlacementResolution {
                selected: Some(policy),
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
                allocation: None,
            },
            free_slots: slots,
            host_ready: ready,
            sleeping_until,
        }
    };
    let cases = [
        (
            "owned available beats metered",
            (true, None, Some(1), FulfilmentCostClass::OwnedIdle),
            (true, None, Some(1), FulfilmentCostClass::Metered),
            true,
        ),
        (
            "available metered beats full owned",
            (true, None, Some(0), FulfilmentCostClass::OwnedIdle),
            (true, None, Some(1), FulfilmentCostClass::Metered),
            false,
        ),
        (
            "awake beats sleeping",
            (true, Some(now + ChronoDuration::hours(1)), Some(1), FulfilmentCostClass::OwnedIdle),
            (true, None, Some(1), FulfilmentCostClass::Metered),
            false,
        ),
        (
            "ready beats unready",
            (false, None, Some(1), FulfilmentCostClass::OwnedIdle),
            (true, None, Some(1), FulfilmentCostClass::SubscriptionIncluded),
            false,
        ),
        (
            "subscription beats metered",
            (true, None, None, FulfilmentCostClass::SubscriptionIncluded),
            (true, None, None, FulfilmentCostClass::Metered),
            true,
        ),
    ];
    for (label, left, right, left_wins) in cases {
        let left = candidate("left", left.3, left.0, left.1, left.2);
        let right = candidate("right", right.3, right.0, right.1, right.2);
        assert_eq!(placement_tiebreak.compare(&left, &right).is_lt(), left_wins, "{label}");
    }
}

#[test]
fn placement_tiebreak_reserves_scarce_platforms_for_named_needs() {
    let now = chrono::Utc::now();
    for platform in ["macos", "windows"] {
        let kind = ResourceObject::<FulfilmentKind> {
            metadata: flotilla_resources::ObjectMeta {
                name: platform.to_string(),
                namespace: "flotilla".to_string(),
                resource_version: "1".to_string(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: now,
                merge: None,
            },
            spec: FulfilmentKindSpec::builder()
                .host_ref("host".to_string())
                .pool("test".to_string())
                .grants(BTreeSet::from([FulfilmentGrant::Platform(platform.to_string())]))
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
            status: None,
        };
        let candidate = KindCandidate {
            kind,
            placement: PlacementResolution {
                selected: None,
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
                allocation: None,
            },
            free_slots: Some(1),
            host_ready: true,
            sleeping_until: None,
        };
        let no_need = BTreeSet::new();
        assert!(PlacementTieBreak { needs: &no_need, now }.reserved(&candidate));
        let named = BTreeSet::from([CapabilityNeed::Platform(platform.to_string())]);
        assert!(!PlacementTieBreak { needs: &named, now }.reserved(&candidate));
    }
}
use crate::providers::{
    discovery::test_support::{
        fake_discovery, fake_discovery_with_provider_set, fake_discovery_with_runner, FakeChangeRequest, FakeDiscoveryProviders,
        FakeTerminalPool, FakeVcsFactory, FakeVcsState,
    },
    terminal::{managed_session_name, ManagedSessionMetadata, TerminalSession},
    testing::MockRunner,
};

#[test]
fn crew_message_sender_headers_snapshot() {
    let principal = Some(flotilla_protocol::PrincipalRef { namespace: "flotilla".into(), name: "robert".into() });
    let cases = [
        ("nudge", CrewMessageSender::FlotillaNudge),
        ("turn", CrewMessageSender::FlotillaTurn { source: "conflicting".into() }),
        ("escalation", CrewMessageSender::FlotillaEscalation { from: "coder@work".into() }),
        ("resume", CrewMessageSender::OperatorResume { principal: principal.clone() }),
        ("follow_up", CrewMessageSender::OperatorFollowUp { principal }),
        ("governor", CrewMessageSender::Governor { name: "wheelhouse".into() }),
        ("bosun", CrewMessageSender::Bosun { name: "reviewer@implement".into() }),
        ("handoff", CrewMessageSender::Handoff { from: "coder@implement".into() }),
    ];
    for (name, sender) in cases {
        insta::assert_snapshot!(name, frame_crew_message(&sender, "Example message."));
    }
}

#[test]
fn crew_message_header_escapes_sender_supplied_delimiters() {
    let sender = CrewMessageSender::OperatorResume {
        principal: Some(flotilla_protocol::PrincipalRef { namespace: "flotilla".into(), name: "robert]\n[flotilla · nudge".into() }),
    };
    assert_eq!(crew_message_header(&sender), "operator robert) (flotilla - nudge · via convoy resume");
    assert_eq!(crew_message_header(&CrewMessageSender::Unknown), "unknown sender · message");
}

#[test]
fn allocation_groups_by_needs_and_credential_environment() {
    let cases = [
        (vec![("coder", vec!["platform:linux"], "write"), ("reviewer", vec!["platform:linux"], "write")], 1),
        (vec![("coder", vec!["platform:linux"], "write"), ("verifier", vec!["gui_session"], "write")], 2),
        (vec![("coder", vec![], "write"), ("reviewer", vec!["host_account_reach"], "write")], 2),
        (vec![("coder", vec![], "write"), ("reviewer", vec![], "read")], 2),
    ];
    for (case, expected_count) in cases {
        let roles = case
            .into_iter()
            .map(|(name, needs, grants)| AllocationRole {
                crew: CrewSpec::builder()
                    .role(name.to_string())
                    .needs(needs.into_iter().map(|need| need.parse().expect("valid need")).collect())
                    .source(CrewSource::Tool { command: "true".to_string() })
                    .build(),
                hint: name.to_string(),
                repository_refs: None,
                depends_on: Vec::new(),
                credential_signature: grants.to_string(),
            })
            .collect::<Vec<_>>();
        let mut workflow = WorkflowTemplateSpec::builder()
            .handoffs(vec![RoleHandoff { from: "coder".to_string(), to: roles[1].crew.role.clone() }])
            .build();
        allocate_roles(&mut workflow, &roles).expect("allocation");
        assert_eq!(workflow.vessels.len(), expected_count);
        assert_eq!(workflow.allocation.len(), expected_count);
        if expected_count == 1 {
            assert!(workflow.allocation[0].crossed_handoffs.is_empty());
        } else {
            assert!(workflow.allocation.iter().any(|decision| !decision.crossed_handoffs.is_empty()));
        }
    }
}

#[test]
fn platform_matrix_expands_into_named_vessels() {
    let project = ProjectSpec::builder()
        .display_name("example".to_string())
        .default_workflow_ref("verify".to_string())
        .platform_matrix(vec!["macos".to_string(), "windows".to_string()])
        .build();
    let verifier = CrewSpec::builder()
        .role("verify".to_string())
        .needs(BTreeSet::from(["platform:$matrix".parse().expect("matrix need")]))
        .source(CrewSource::Tool { command: "true".to_string() })
        .build();
    let mut workflow = WorkflowTemplateSpec::builder().roles(vec![verifier]).build();
    workflow.repository_refs = Some(vec![RepositoryKey("scoped-repository".to_string())]);
    let roles = expand_allocation_roles(&mut workflow, &project).expect("expand matrix");
    assert_eq!(roles.iter().map(|role| role.hint.as_str()).collect::<Vec<_>>(), ["verify[macos]", "verify[windows]"]);
    assert_eq!(roles[0].crew.needs.iter().map(ToString::to_string).collect::<Vec<_>>(), ["platform:macos"]);
    assert_eq!(roles[0].repository_refs, workflow.repository_refs);
}

#[test]
fn dual_authored_template_refuses_a_role_only_in_the_vessel_hint() {
    let project = ProjectSpec::builder().display_name("example".to_string()).default_workflow_ref("work".to_string()).build();
    let crew = |role: &str| CrewSpec::builder().role(role.to_string()).source(CrewSource::Tool { command: "true".to_string() }).build();
    let mut workflow = WorkflowTemplateSpec::builder()
        .roles(vec![crew("coder")])
        .vessels(vec![VesselRequirement::builder().name("work".to_string()).crew(vec![crew("reviewer")]).build()])
        .build();
    let error = expand_allocation_roles(&mut workflow, &project).expect_err("missing role must refuse");
    assert!(error.contains("reviewer"), "{error}");
}

#[test]
fn matrix_turn_delivery_requires_a_concrete_vessel() {
    let roles = ["macos", "windows"]
        .into_iter()
        .map(|platform| AllocationRole {
            crew: CrewSpec::builder()
                .role("verify".to_string())
                .needs(BTreeSet::from([CapabilityNeed::Platform(platform.to_string())]))
                .source(CrewSource::Tool { command: "true".to_string() })
                .build(),
            hint: format!("verify[{platform}]"),
            repository_refs: None,
            depends_on: Vec::new(),
            credential_signature: String::new(),
        })
        .collect::<Vec<_>>();
    let rule = flotilla_resources::TurnDeliveryRule::builder()
        .on("$cr.mergeable == conflicting".parse().expect("leaf"))
        .to(flotilla_resources::TurnDeliveryTarget::builder().vessel("verify".to_string()).role("verify".to_string()).build())
        .brief("Verify the result".to_string())
        .hold(HoldAct::ChangeRequestComment { body: "Verification needed".to_string() })
        .build();
    let mut workflow = WorkflowTemplateSpec::builder().turn_delivery(indexmap::IndexMap::from([("verify".to_string(), rule)])).build();
    let error = allocate_roles(&mut workflow, &roles).expect_err("ambiguous delivery must refuse");
    assert!(error.contains("multiple vessels"), "{error}");
    workflow.turn_delivery["verify"].to.vessel = "verify[macos]".to_string();
    allocate_roles(&mut workflow, &roles).expect("explicit concrete target");
    assert_eq!(workflow.turn_delivery["verify"].to.vessel, "verify[macos]");
}

#[test]
fn standalone_issue_source_lookup_round_trips_installation_identity() {
    let lab = flotilla_resources::ForgeSpec::builder()
        .forge_id("lab".into())
        .kind(flotilla_resources::ForgeKind::Forgejo)
        .hosts(BTreeSet::from(["forgejo.example".into()]))
        .https_url("https://forgejo.example/lab".into())
        .git_ssh_host("forgejo.example".into())
        .build();
    let mut stage = lab.clone();
    stage.forge_id = "stage".into();
    stage.https_url = "https://forgejo.example/stage".into();
    let forges = [lab, stage];
    for (service, expected) in [
        ("lab", "https://forgejo.example/lab"),
        ("stage", "https://forgejo.example/stage"),
        ("forgejo.example%2flab", "https://forgejo.example/lab"),
        ("forgejo.example%2fstage", "https://forgejo.example/stage"),
        ("http%3a%2f%2fforgejo.example%2flab", "http://forgejo.example/lab"),
        ("host%3aforgejo", "https://forgejo"),
    ] {
        let subject = crate::issue_observer::IssueRef {
            namespace: "flotilla".into(),
            service: service.into(),
            scope: "Team/Repo".into(),
            number: 12,
        };
        let source = issue_source_for_subject(&subject, &forges).expect("source");
        assert_eq!(source.service, expected);
        let reference = flotilla_protocol::IssueRef { source, id: "12".into() };
        let declarations = if service.contains("%2f") { &[][..] } else { &forges[..] };
        assert_eq!(
            flotilla_resources::issue_address_with_forges(&reference, declarations).expect("round trip").to_string(),
            format!("issue/{service}/Team/Repo/12")
        );
    }
    let orphaned =
        crate::issue_observer::IssueRef { namespace: "flotilla".into(), service: "lab".into(), scope: "Team/Repo".into(), number: 12 };
    assert!(issue_source_for_subject(&orphaned, &[]).expect_err("missing Forge").contains("no Forge declaration"));
}

struct RecordingWorkCredentials {
    backend: ResourceBackend,
    delivered: tokio::sync::Mutex<BTreeSet<String>>,
    fail_next: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl WorkCredentialReconciler for RecordingWorkCredentials {
    async fn reconcile(&self, namespace: &str, environment_ref: &str) -> Result<(), String> {
        assert_eq!(environment_ref, "credential-env");
        let convoy =
            self.backend.clone().using::<ResourceConvoy>(namespace).get("turn-credential-work").await.map_err(|error| error.to_string())?;
        let status = convoy.status.ok_or_else(|| "missing convoy status".to_string())?;
        if status.phase != flotilla_resources::ConvoyPhase::Active
            || status.work.get("work").is_none_or(|work| work.phase != flotilla_resources::WorkPhase::Running)
            || status.crew_work.get("work").and_then(|crew| crew.get("coder")).is_none_or(|crew| crew.phase != CrewWorkPhase::Working)
        {
            return Err("credentials reconciled before work reopened".to_string());
        }
        let refs = status
            .workflow_snapshot
            .ok_or_else(|| "missing workflow snapshot".to_string())?
            .vessels
            .into_iter()
            .find(|vessel| vessel.name == "work")
            .ok_or_else(|| "missing work vessel".to_string())?
            .credential_refs;
        if self.fail_next.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err("credential staging failed".to_string());
        }
        self.delivered.lock().await.extend(refs);
        Ok(())
    }
}

struct SessionStagingProbe {
    backend: ResourceBackend,
    session: String,
    environment: String,
    fail_next: std::sync::atomic::AtomicBool,
    invalidate_next: std::sync::atomic::AtomicBool,
    staged: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl WorkCredentialReconciler for SessionStagingProbe {
    async fn reconcile(&self, namespace: &str, environment_ref: &str) -> Result<(), String> {
        assert_eq!(environment_ref, self.environment);
        let sessions = self.backend.clone().using::<ResourceTerminalSession>(namespace);
        let session = sessions.get(&self.session).await.map_err(|error| error.to_string())?;
        let TerminalSessionSource::Agent { message, .. } = &session.spec.source else { return Err("expected agent session".to_string()) };
        assert!(message.is_none(), "message became deliverable before credential staging");
        if self.fail_next.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err("credential staging failed".to_string());
        }
        self.staged.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.invalidate_next.swap(false, std::sync::atomic::Ordering::SeqCst) {
            let mut changed_spec = session.spec.clone();
            let TerminalSessionSource::Agent { brief, .. } = &mut changed_spec.source else { unreachable!("checked above") };
            brief.content.push_str(" (concurrent edit)");
            sessions
                .update(&input_meta_from_resource(&session), &session.metadata.resource_version, &changed_spec)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

async fn resume_staging_fixture() -> (Arc<InProcessDaemon>, ResourceBackend, Arc<SessionStagingProbe>) {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"resume-staging-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .create(&test_meta("resume-staging"), &ConvoySpec::builder().workflow_ref("implement-review".to_string()).build())
        .await
        .expect("convoy");
    convoys
        .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus {
            phase: flotilla_resources::ConvoyPhase::Landing,
            workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                stall_nudges: Default::default(),
                supervision: None,
                exit: None,
                turn_delivery: Default::default(),
                vessels: vec![VesselRequirement::builder().name("work".to_string()).crew(vec![claim_crew("coder")]).build()],
            }),
            work: BTreeMap::from([(
                "work".to_string(),
                flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Complete).build(),
            )]),
            crew_work: BTreeMap::from([(
                "work".to_string(),
                BTreeMap::from([("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Done).build())]),
            )]),
            ..Default::default()
        })
        .await
        .expect("convoy status");
    backend
        .clone()
        .using::<ResourceTerminalSession>("flotilla")
        .create(
            &InputMeta::builder()
                .name("resume-staging-session".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "resume-staging".to_string()),
                    (VESSEL_LABEL.to_string(), "work".to_string()),
                    (ROLE_LABEL.to_string(), "coder".to_string()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "resume-env".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: None, model: None },
                    brief: flotilla_resources::TerminalBrief {
                        artifact_digest: None,
                        path: "brief.md".to_string(),
                        content: "original".to_string(),
                        copies: vec![],
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "resume-staging".to_string(),
                        vessel_ref: "resume-vessel".to_string(),
                    }),
                    message: None,
                },
                cwd: "/repo".to_string(),
                pool: "passthrough".to_string(),
            },
        )
        .await
        .expect("session");
    let probe = Arc::new(SessionStagingProbe {
        backend: backend.clone(),
        session: "resume-staging-session".to_string(),
        environment: "resume-env".to_string(),
        fail_next: std::sync::atomic::AtomicBool::new(true),
        invalidate_next: std::sync::atomic::AtomicBool::new(false),
        staged: std::sync::atomic::AtomicUsize::new(0),
    });
    daemon.set_work_credential_reconciler(probe.clone()).await;
    (daemon, backend, probe)
}

#[tokio::test]
async fn resume_stages_credentials_before_message_and_retries_failure() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
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
    assert_eq!(message.expect("queued message").text, "[operator (unattributed) · via convoy resume]\n\ncontinue");
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
    let TerminalSessionSource::Agent { message: Some(message), .. } = session.spec.source else { panic!("resume brief missing") };
    assert!(message.text.contains("continue"));
}

#[tokio::test]
async fn declared_access_stall_routes_to_project_governor_and_resumes() {
    #[derive(Default)]
    struct AcceptSupervision {
        requests: std::sync::Mutex<Vec<crate::leaf_engine::TurnDeliveryRequest>>,
    }
    #[async_trait]
    impl crate::leaf_engine::TurnDeliveryActuator for AcceptSupervision {
        async fn deliver(&self, request: &crate::leaf_engine::TurnDeliveryRequest) -> Result<flotilla_resources::TurnDeliveryRung, String> {
            self.requests.lock().expect("supervision requests").push(request.clone());
            Ok(flotilla_resources::TurnDeliveryRung::WarmSession)
        }
        async fn hold(
            &self,
            _request: &crate::leaf_engine::TurnDeliveryRequest,
            _act: &flotilla_resources::HoldAct,
            _reason: &str,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let supervision = Arc::new(AcceptSupervision::default());
    daemon.leaf_subscriptions.set_turn_delivery_actuator(supervision.clone()).await;
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
        .update_status("governor", &governor.metadata.resource_version, &ConvoyStatus {
            crew_work: BTreeMap::from([(
                "watch".to_string(),
                BTreeMap::from([("governor".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
            )]),
            ..Default::default()
        })
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
        .update_status("governor-session", &governor_session.metadata.resource_version, &ResourceTerminalSessionStatus {
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
        })
        .await
        .expect("governor working");
    let (tx, _rx) = flotilla_resources::controller::WorkQueueSender::channel();
    let task = tokio::spawn(daemon.reconciler_wake_watch().spawn(backend.clone(), "flotilla".to_string(), tx));
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
        assert_eq!(escalation.sender, CrewMessageSender::FlotillaEscalation { from: "coder@work".to_string() });
        let framed = frame_crew_message(&escalation.sender, &escalation.brief);
        assert!(framed.starts_with("[flotilla · escalated from coder@work · supervise the stalled crew]"));
        assert!(framed.contains("Proposed disposition: reduce-scope."), "{framed}");
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if daemon.leaf_subscriptions.rows().await.iter().any(|row| {
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
    let TerminalSessionSource::Agent { message: Some(guidance), .. } = source_session.spec.source else {
        panic!("governor guidance should be queued")
    };
    assert_eq!(guidance.sender, CrewMessageSender::Governor { name: "governor".to_string() });
    assert!(guidance.text.starts_with("[governor governor · guidance for your stalled work · reply by running `crew complete`]"));
    let governor_session = sessions.get("governor-session").await.expect("governor session");
    sessions
        .update_status("governor-session", &governor_session.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            attention: Some(TerminalAttention {
                state: TerminalAttentionState::Idle,
                as_of: chrono::Utc::now(),
                source: TerminalAttentionSource::Hook,
            }),
            ..Default::default()
        })
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
        .update_status("governor-session", &governor_session.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            attention: Some(TerminalAttention {
                state: TerminalAttentionState::Working,
                as_of: chrono::Utc::now(),
                source: TerminalAttentionSource::Hook,
            }),
            ..Default::default()
        })
        .await
        .expect("governor working");
    let source_session = sessions.get("resume-staging-session").await.expect("source session");
    sessions
        .update_status("resume-staging-session", &source_session.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            attention: Some(TerminalAttention {
                state: TerminalAttentionState::NeedsInput,
                as_of: chrono::Utc::now(),
                source: TerminalAttentionSource::Hook,
            }),
            ..Default::default()
        })
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
        .update_status("resume-staging-session", &source_session.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            attention: Some(TerminalAttention {
                state: TerminalAttentionState::Working,
                as_of: chrono::Utc::now(),
                source: TerminalAttentionSource::Hook,
            }),
            ..Default::default()
        })
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

#[tokio::test]
async fn turn_delivery_restores_convoy_when_session_write_fails_after_staging() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    probe.invalidate_next.store(true, std::sync::atomic::Ordering::SeqCst);
    let request = crate::leaf_engine::TurnDeliveryRequest::builder()
        .namespace("flotilla".to_string())
        .convoy("resume-staging".to_string())
        .source("review".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .brief("continue".to_string())
        .subject_revision("new-head".to_string())
        .sender(CrewMessageSender::FlotillaTurn { source: "review".to_string() })
        .build();
    daemon.deliver_standing_turn(&request).await.expect_err("stale session write");
    let status = backend.using::<ResourceConvoy>("flotilla").get("resume-staging").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Done);
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn fresh_turn_replaces_the_old_brief_digest() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("resume-staging-session").await.expect("session");
    let mut spec = session.spec.clone();
    let TerminalSessionSource::Agent { brief, .. } = &mut spec.source else { panic!("agent session") };
    brief.artifact_digest = Some("a".repeat(64));
    brief.content.clear();
    let session = sessions
        .update(&input_meta_from_resource(&session), &session.metadata.resource_version, &spec)
        .await
        .expect("digest-backed session");
    sessions
        .update_status(&session.metadata.name, &session.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Stopped,
            ..Default::default()
        })
        .await
        .expect("stopped session");
    let request = crate::leaf_engine::TurnDeliveryRequest::builder()
        .namespace("flotilla".to_string())
        .convoy("resume-staging".to_string())
        .source("review".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .brief("fresh turn".to_string())
        .subject_revision("next-head".to_string())
        .sender(CrewMessageSender::FlotillaTurn { source: "review".to_string() })
        .build();
    daemon.deliver_standing_turn(&request).await.expect("deliver fresh turn");
    let session = sessions.get("resume-staging-session").await.expect("updated session");
    let TerminalSessionSource::Agent { brief, message, .. } = session.spec.source else { panic!("agent session") };
    assert_eq!(brief.content, "[flotilla · turn: review · reply by running `crew complete`]\n\nfresh turn");
    assert_eq!(brief.artifact_digest, None);
    assert_eq!(message.expect("fresh message record").sender, CrewMessageSender::FlotillaTurn { source: "review".to_string() });
}

#[tokio::test]
async fn repeated_standing_turn_does_not_restart_a_lost_session_after_delivery() {
    let (daemon, backend, probe) = resume_staging_fixture().await;
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("resume-staging-session").await.expect("session");
    let mut spec = session.spec.clone();
    let TerminalSessionSource::Agent { message, .. } = &mut spec.source else { panic!("agent session") };
    *message = Some(flotilla_resources::TerminalCrewMessage {
        id: "turn-delivery:review:same-head".into(),
        text: "previously delivered".into(),
        sender: CrewMessageSender::FlotillaTurn { source: "review".into() },
        delivery: flotilla_resources::CrewMessageDelivery::Queued,
        following: Vec::new(),
    });
    let session =
        sessions.update(&input_meta_from_resource(&session), &session.metadata.resource_version, &spec).await.expect("stored turn");
    sessions
        .update_status(&session.metadata.name, &session.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Lost,
            delivered_message_id: Some("turn-delivery:review:same-head".into()),
            ..Default::default()
        })
        .await
        .expect("lost after delivery");
    let request = crate::leaf_engine::TurnDeliveryRequest::builder()
        .namespace("flotilla".to_string())
        .convoy("resume-staging".to_string())
        .source("review".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .brief("same turn".to_string())
        .subject_revision("same-head".to_string())
        .sender(CrewMessageSender::FlotillaTurn { source: "review".to_string() })
        .build();
    assert_eq!(daemon.deliver_standing_turn(&request).await.expect("duplicate is already delivered"), TurnDeliveryRung::FreshAgent);
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 0);
    let session = sessions.get("resume-staging-session").await.expect("session");
    assert_eq!(session.spec, spec);
    assert_eq!(session.status.expect("status").phase, ResourceTerminalSessionPhase::Lost);
}

#[tokio::test]
async fn turn_delivery_reopens_work_and_stages_credentials_before_queuing_every_rung() {
    for phase in [ResourceTerminalSessionPhase::Running, ResourceTerminalSessionPhase::Starting, ResourceTerminalSessionPhase::Stopped] {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"turn-credential-test\"\n").expect("daemon config");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery(false),
            HostName::new("test-host"),
            backend.clone(),
        )
        .await;
        let credentials = Arc::new(RecordingWorkCredentials {
            backend: backend.clone(),
            delivered: tokio::sync::Mutex::new(BTreeSet::new()),
            fail_next: std::sync::atomic::AtomicBool::new(true),
        });
        daemon.set_work_credential_reconciler(credentials.clone()).await;
        let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
        let convoy = convoys
            .create(&test_meta("turn-credential-work"), &ConvoySpec::builder().workflow_ref("implement-review".to_string()).build())
            .await
            .expect("convoy");
        convoys
            .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Landing,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![VesselRequirement::builder()
                        .name("work".to_string())
                        .credential_refs(BTreeSet::from(["github-crew-pr".to_string()]))
                        .crew(Vec::new())
                        .build()],
                }),
                work: BTreeMap::from([(
                    "work".to_string(),
                    flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Complete).build(),
                )]),
                crew_work: BTreeMap::from([(
                    "work".to_string(),
                    BTreeMap::from([("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Done).build())]),
                )]),
                ..Default::default()
            })
            .await
            .expect("admitted claim");
        let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
        let session = sessions
            .create(
                &InputMeta::builder()
                    .name("turn-credential-session".to_string())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.to_string(), "turn-credential-work".to_string()),
                        (VESSEL_LABEL.to_string(), "work".to_string()),
                        (ROLE_LABEL.to_string(), "coder".to_string()),
                    ]))
                    .build(),
                &ResourceTerminalSessionSpec {
                    env_ref: "credential-env".to_string(),
                    role: "coder".to_string(),
                    source: TerminalSessionSource::Agent {
                        selector: Selector { capability: "code".to_string(), adapter: None, model: None },
                        brief: flotilla_resources::TerminalBrief {
                            artifact_digest: None,
                            path: "brief.md".to_string(),
                            content: "original".to_string(),
                            copies: vec![],
                        },
                        context: Box::new(flotilla_resources::TerminalCrewContext {
                            namespace: "flotilla".to_string(),
                            convoy: "turn-credential-work".to_string(),
                            vessel_ref: "turn-credential-vessel".to_string(),
                        }),
                        message: None,
                    },
                    cwd: "/repo".to_string(),
                    pool: "passthrough".to_string(),
                },
            )
            .await
            .expect("session");
        sessions
            .update_status(&session.metadata.name, &session.metadata.resource_version, &ResourceTerminalSessionStatus {
                phase,
                ..Default::default()
            })
            .await
            .expect("session phase");
        let request = crate::leaf_engine::TurnDeliveryRequest::builder()
            .namespace("flotilla".to_string())
            .convoy("turn-credential-work".to_string())
            .source("conflicting".to_string())
            .vessel("work".to_string())
            .role("coder".to_string())
            .brief("rebase the PR".to_string())
            .subject_revision("new-head".to_string())
            .sender(CrewMessageSender::FlotillaTurn { source: "conflicting".to_string() })
            .build();
        assert!(daemon.deliver_standing_turn(&request).await.is_err(), "failed staging must prevent delivery");
        let after_failure = convoys.get("turn-credential-work").await.expect("convoy after failed staging").status.expect("status");
        assert_eq!(after_failure.phase, flotilla_resources::ConvoyPhase::Landing);
        assert_eq!(after_failure.work["work"].phase, flotilla_resources::WorkPhase::Complete);
        assert_eq!(after_failure.crew_work["work"]["coder"].phase, CrewWorkPhase::Done);
        assert!(credentials.delivered.lock().await.is_empty());
        let undelivered = sessions.get("turn-credential-session").await.expect("undelivered session");
        let TerminalSessionSource::Agent { message, .. } = undelivered.spec.source else { panic!("agent session expected") };
        assert!(message.is_none());
        daemon.deliver_standing_turn(&request).await.expect("retry conflicting turn");
        let delivered = sessions.get("turn-credential-session").await.expect("delivered session");
        assert_eq!(*credentials.delivered.lock().await, BTreeSet::from(["github-crew-pr".to_string()]));
        let TerminalSessionSource::Agent { brief, message, .. } = delivered.spec.source else { panic!("agent session expected") };
        if phase == ResourceTerminalSessionPhase::Stopped {
            assert_eq!(brief.content, "[flotilla · turn: conflicting · reply by running `crew complete`]\n\nrebase the PR");
            assert_eq!(message.expect("fresh message record").sender, CrewMessageSender::FlotillaTurn {
                source: "conflicting".to_string()
            });
        } else {
            assert_eq!(
                message.expect("queued message").text,
                "[flotilla · turn: conflicting · reply by running `crew complete`]\n\nrebase the PR"
            );
        }
    }
}

#[tokio::test]
async fn idle_crew_nudges_are_bounded_and_credential_staged() {
    use flotilla_resources::StallRung;

    for limit in [0, 2] {
        let (daemon, backend, probe) = resume_staging_fixture().await;
        probe.fail_next.store(false, std::sync::atomic::Ordering::SeqCst);
        let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
        let convoy = convoys.get("resume-staging").await.expect("convoy");
        let mut status = convoy.status.expect("status");
        status.phase = flotilla_resources::ConvoyPhase::Active;
        status.work.get_mut("work").expect("work").phase = flotilla_resources::WorkPhase::Running;
        status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Working;
        status
            .workflow_snapshot
            .as_mut()
            .expect("workflow")
            .stall_nudges
            .insert("work/coder".to_string(), flotilla_resources::StallNudgePolicy { max_per_episode: limit, max_refusals: None });
        convoys.update_status("resume-staging", &convoy.metadata.resource_version, &status).await.expect("active convoy");
        let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
        let session = sessions.get("resume-staging-session").await.expect("session");
        let mut session_status = ResourceTerminalSessionStatus { phase: ResourceTerminalSessionPhase::Running, ..Default::default() };
        session_status.attention = Some(TerminalAttention {
            state: TerminalAttentionState::Idle,
            as_of: chrono::Utc::now(),
            source: TerminalAttentionSource::Hook,
        });
        sessions.update_status("resume-staging-session", &session.metadata.resource_version, &session_status).await.expect("idle session");
        let (tx, _rx) = flotilla_resources::controller::WorkQueueSender::channel();
        let watcher = daemon.reconciler_wake_watch();
        let task = tokio::spawn(watcher.spawn(backend.clone(), "flotilla".to_string(), tx));
        let expected_rung = if limit == 0 { StallRung::Operator } else { StallRung::Nudge };
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let stalled = convoys.get("resume-staging").await.expect("convoy").status.expect("status").stalled;
                if stalled.as_ref().is_some_and(|stall| stall.rung == expected_rung && stall.nudge_history.len() == limit.min(1) as usize) {
                    break stalled.expect("stalled");
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first stall judgement");
        assert_eq!(first.nudge_history.len(), limit.min(1) as usize);
        let session = sessions.get("resume-staging-session").await.expect("session");
        let TerminalSessionSource::Agent { message, .. } = session.spec.source else { panic!("agent") };
        if limit == 0 {
            assert!(message.is_none());
            assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 0);
        } else {
            assert_eq!(message.expect("nudge").text, "[flotilla · nudge · reply by running `crew complete` or `crew stall`]\n\nYou owe a settlement claim for work/coder: finish, put the decision-ledger artifact, then run `flotilla crew complete`, or `crew stall --reason <infra|scope|decision|access|other> --message …` if blocked.");
            assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);
            for (offset, desired_rung) in [(1, StallRung::Nudge), (2, StallRung::Operator)] {
                let session = sessions.get("resume-staging-session").await.expect("session");
                let mut spec = session.spec.clone();
                let TerminalSessionSource::Agent { message, .. } = &mut spec.source else { panic!("agent") };
                *message = None;
                sessions
                    .update(&input_meta_from_resource(&session), &session.metadata.resource_version, &spec)
                    .await
                    .expect("consume nudge");
                let session = sessions.get("resume-staging-session").await.expect("session");
                session_status.attention.as_mut().expect("attention").as_of = chrono::Utc::now() + chrono::Duration::seconds(offset);
                sessions
                    .update_status("resume-staging-session", &session.metadata.resource_version, &session_status)
                    .await
                    .expect("idle again");
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let stalled =
                            convoys.get("resume-staging").await.expect("convoy").status.expect("status").stalled.expect("stalled");
                        if stalled.nudge_history.len() == 2 && stalled.rung == desired_rung {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("next idle episode judgement");
            }
            let stalled = convoys.get("resume-staging").await.expect("convoy").status.expect("status").stalled.expect("stalled");
            assert_eq!(stalled.nudge_history.len(), 2);
        }
        let convoy = convoys.get("resume-staging").await.expect("convoy");
        let mut status = convoy.status.expect("status");
        status.phase = flotilla_resources::ConvoyPhase::Landed;
        status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Done;
        convoys.update_status("resume-staging", &convoy.metadata.resource_version, &status).await.expect("complete");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if convoys.get("resume-staging").await.expect("convoy").status.expect("status").stalled.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stall cleared");
        task.abort();
    }
}

struct ForgeAwareTestChangeRequestFactory(Arc<dyn ChangeRequestTracker>);

#[async_trait]
impl crate::providers::discovery::Factory for ForgeAwareTestChangeRequestFactory {
    type Descriptor = crate::providers::discovery::ProviderDescriptor;
    type Output = dyn ChangeRequestTracker;

    fn descriptor(&self) -> Self::Descriptor {
        crate::providers::discovery::ProviderDescriptor::named(crate::providers::discovery::ProviderCategory::ChangeRequest, "test-forgejo")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &ExecutionEnvironmentPath,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<crate::providers::discovery::UnmetRequirement>> {
        assert_eq!(env.find_origin_forge().map(|forge| forge.kind), Some(flotilla_resources::ForgeKind::Forgejo));
        assert!(env.find_auth_path("forgejo").is_some());
        Ok(Arc::clone(&self.0))
    }
}

#[tokio::test]
async fn convoy_change_request_resolution_uses_forge_aware_factory_and_credential() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"forgejo-cr-test\"\n").expect("daemon config");
    let token_path = temp.path().join("forgejo-token");
    std::fs::write(&token_path, "test-token").expect("token file");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let forge = flotilla_resources::ForgeSpec::builder()
        .forge_id("lab".to_string())
        .kind(flotilla_resources::ForgeKind::Forgejo)
        .hosts(BTreeSet::from(["forgejo.lab.flotilla.work".to_string()]))
        .https_url("https://forgejo.lab.flotilla.work".to_string())
        .git_ssh_host("forgejo.lab.flotilla.work".to_string())
        .build();
    backend.definitions::<flotilla_resources::Forge>("flotilla").create(&test_meta("lab"), &forge).await.expect("Forge");
    let repository = RepositorySpec::remote("https://forgejo.lab.flotilla.work/robert/ghostty-ops")
        .expect("repository")
        .on_forge(&forge)
        .expect("forge identity");
    let repository_key = repository.key();
    backend.clone().using::<Repository>("flotilla").create(&test_meta(&repository_key.to_string()), &repository).await.expect("repository");
    let remote_repository = RepositorySpec::remote("https://forgejo.lab.flotilla.work/robert/ghostty-ops").expect("remote repository");
    let remote_key = remote_repository.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&test_meta(&remote_key.to_string()), &remote_repository)
        .await
        .expect("repository awaiting forge identity migration");
    backend
        .definitions::<CredentialSpec>("flotilla")
        .create(
            &test_meta("lab-forgejo-crew-pr"),
            &CredentialSpecSpec::builder()
                .consumer(CredentialConsumer::Forgejo { forge_ref: "lab".to_string(), username: "crew".to_string() })
                .source(CredentialSource::File { path: token_path.to_string_lossy().into_owned() })
                .lifecycle(CredentialLifecycle::Static)
                .build(),
        )
        .await
        .expect("credential");
    let provider = Arc::new(FakeChangeRequest::new());
    provider
        .add_change_requests(vec![("17".to_string(), crate::providers::types::ChangeRequest {
            title: "Fix ghostty".to_string(),
            branch: "governor".to_string(),
            status: flotilla_protocol::ChangeRequestStatus::Open,
            body: None,
            provider_name: "forgejo".to_string(),
            provider_display_name: "Forgejo".to_string(),
        })])
        .await;
    let mut discovery = fake_discovery(false);
    discovery.factories.change_requests = vec![Box::new(ForgeAwareTestChangeRequestFactory(provider))];
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        discovery,
        HostName::new("test-host"),
        backend,
    )
    .await;
    daemon.set_provisioning_namespace("flotilla".to_string()).await;
    let resolved =
        daemon.resolve_convoy_change_request(std::slice::from_ref(&repository_key), "governor", None).await.expect("resolve Forgejo PR");
    assert_eq!(
        resolved,
        Some(ConvoyChangeRequest {
            id: "17".to_string(),
            status: flotilla_protocol::ChangeRequestStatus::Open,
            repository_key: repository_key.clone(),
        })
    );
    let resolved =
        daemon.resolve_convoy_change_request(std::slice::from_ref(&remote_key), "governor", None).await.expect("resolve remote by host");
    assert_eq!(
        resolved,
        Some(ConvoyChangeRequest {
            id: "17".to_string(),
            status: flotilla_protocol::ChangeRequestStatus::Open,
            repository_key: remote_key
        })
    );

    let second_url = "https://forgejo.lab.flotilla.work/robert/other";
    let second_repository = RepositorySpec::remote(second_url).expect("second repository").on_forge(&forge).expect("forge identity");
    let second_key = second_repository.key();
    daemon
        .resource_backend()
        .using::<Repository>("flotilla")
        .create(&test_meta(&second_key.to_string()), &second_repository)
        .await
        .expect("second repository");
    let convoy_spec = ConvoySpec::builder()
        .workflow_ref("workflow".to_string())
        .repositories(vec![
            ConvoyRepositorySpec {
                url: "https://forgejo.lab.flotilla.work/robert/ghostty-ops".into(),
                repo_ref: repository_key.clone(),
                source_ref: "main".into(),
                target_ref: "main".into(),
                workspace_slug: "ghostty-ops".into(),
                subpaths: Vec::new(),
            },
            ConvoyRepositorySpec {
                url: second_url.into(),
                repo_ref: second_key,
                source_ref: "main".into(),
                target_ref: "main".into(),
                workspace_slug: "other".into(),
                subpaths: Vec::new(),
            },
        ])
        .r#ref("governor".to_string())
        .build();
    let convoys = daemon.resource_backend().using::<ResourceConvoy>("flotilla");
    convoys.create(&test_meta("multi-repo"), &convoy_spec).await.expect("convoy");
    daemon.discover_convoy_branch_subjects("flotilla", "multi-repo", "governor").await.expect("branch discovery");
    let convoy = convoys.get("multi-repo").await.expect("convoy after discovery");
    assert_eq!(convoy.status.as_ref().expect("status").subjects.len(), 2);
    assert!(convoy
        .status
        .as_ref()
        .expect("status")
        .subjects
        .iter()
        .all(|entry| entry.relationship == flotilla_protocol::Relationship::Produces));

    let claim = crate::checkout_integration::change_request_subjects_from_claim(
        "https://forgejo.lab.flotilla.work/robert/ghostty-ops/pulls/18",
        &convoy.spec.repositories,
        &[forge],
    );
    assert_eq!(claim.len(), 1);
    flotilla_resources::apply_status_patch(&convoys, "multi-repo", &ConvoyStatusPatch::DiscoverSubjects {
        subjects: vec![(claim[0].clone(), flotilla_protocol::Relationship::Produces)],
        source: flotilla_resources::SubjectDiscoverySource::Claim,
        at: Utc::now(),
    })
    .await
    .expect("claim discovery");
    let with_followup = convoys.get("multi-repo").await.expect("convoy with follow-up");
    let leaves = flotilla_resources::expected_change_request_leaves(&with_followup, &BTreeMap::new()).expect("subject leaves");
    assert_eq!(leaves.len(), 6, "all three produced change requests require terminal observations");
    assert!(daemon
        .link_convoy_subject("flotilla", "multi-repo", "wheelhouze/cleat!12", Some(flotilla_protocol::Relationship::Produces))
        .await
        .expect_err("unknown GitHub repository must be rejected")
        .contains("outside this convoy's repositories"));
    assert!(daemon
        .link_convoy_subject(
            "flotilla",
            "multi-repo",
            "https://forgejo.lab.flotilla.work/robert/foreign/pulls/12",
            Some(flotilla_protocol::Relationship::Produces)
        )
        .await
        .expect_err("foreign Forgejo repository must be rejected")
        .contains("outside this convoy's repositories"));
    daemon
        .link_convoy_subject("flotilla", "multi-repo", "lab:robert/ghostty-ops!18", Some(flotilla_protocol::Relationship::Supersedes))
        .await
        .expect("operator resolution");
    let superseded = convoys.get("multi-repo").await.expect("convoy with superseded request");
    let leaves = flotilla_resources::expected_change_request_leaves(&superseded, &BTreeMap::new()).expect("subject leaves");
    assert_eq!(leaves.len(), 4, "supersedes releases only the replaced change request");
}

#[tokio::test]
async fn prepared_workflow_snapshot_reuses_an_identical_replica() {
    let home_root = NodeId::new("snapshot-home");
    let driver_root = NodeId::new("snapshot-driver");
    let home = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(home_root.clone());
    let driver = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(driver_root);
    let spec = flotilla_resources::single_agent_workflow_spec();
    let name = prepared_snapshot_name("workflow", &serde_json::to_value(&spec).expect("serialize workflow")).expect("snapshot name");

    ensure_prepared_workflow_snapshot(&home, "flotilla", &name, &spec).await.expect("author snapshot on home");
    driver
        .replica_writer::<WorkflowTemplate>(home_root, "flotilla")
        .replace(&home.using::<WorkflowTemplate>("flotilla").list().await.expect("home workflow log"), Utc::now())
        .await
        .expect("replicate snapshot to driver");

    ensure_prepared_workflow_snapshot(&driver, "flotilla", &name, &spec).await.expect("reuse identical replicated snapshot");
    assert!(driver.using::<WorkflowTemplate>("flotilla").list().await.expect("driver local workflow log").items.is_empty());
}

#[tokio::test]
async fn prepared_placement_snapshot_reuses_an_identical_replica() {
    let home_root = NodeId::new("snapshot-home");
    let driver_root = NodeId::new("snapshot-driver");
    let home = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(home_root.clone());
    let driver = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(driver_root);
    let spec = PlacementPolicySpec::builder().pool("passthrough".to_string()).build();
    let name = prepared_snapshot_name("placement", &serde_json::to_value(&spec).expect("serialize placement")).expect("snapshot name");

    ensure_prepared_placement_snapshot(&home, "flotilla", &name, &spec).await.expect("author snapshot on home");
    driver
        .replica_writer::<PlacementPolicy>(home_root, "flotilla")
        .replace(&home.using::<PlacementPolicy>("flotilla").list().await.expect("home placement log"), Utc::now())
        .await
        .expect("replicate snapshot to driver");

    ensure_prepared_placement_snapshot(&driver, "flotilla", &name, &spec).await.expect("reuse identical replicated snapshot");
    assert!(driver.using::<PlacementPolicy>("flotilla").list().await.expect("driver local placement log").items.is_empty());
}

#[tokio::test]
async fn prepared_placement_snapshot_rejects_a_different_replica_spec() {
    let home_root = NodeId::new("snapshot-home");
    let home = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(home_root.clone());
    let driver = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("snapshot-driver"));
    let name = "placement-snapshot-collision";
    let authored = PlacementPolicySpec::builder().pool("passthrough".to_string()).build();
    let requested = PlacementPolicySpec::builder().pool("cleat".to_string()).build();
    home.using::<PlacementPolicy>("flotilla").create(&test_meta(name), &authored).await.expect("author snapshot on home");
    driver
        .replica_writer::<PlacementPolicy>(home_root, "flotilla")
        .replace(&home.using::<PlacementPolicy>("flotilla").list().await.expect("home placement log"), Utc::now())
        .await
        .expect("replicate snapshot to driver");

    let error =
        ensure_prepared_placement_snapshot(&driver, "flotilla", name, &requested).await.expect_err("mismatched replica must be refused");
    assert_eq!(error, format!("prepared placement snapshot {name} already exists with different contents"));
    assert!(driver.using::<PlacementPolicy>("flotilla").list().await.expect("driver local placement log").items.is_empty());
}

#[tokio::test]
async fn two_origins_admit_identical_placements_without_authorship_collision() {
    let (first, first_backend, _first_clock, _first_temp) = standing_ensure_fixture_for("feta", true).await;
    let (second, second_backend, _second_clock, _second_temp) = standing_ensure_fixture_for("udder", true).await;
    for backend in [&first_backend, &second_backend] {
        configure_standing_ensure_agent(backend, Vec::new()).await;
    }

    first.reconcile_convoy_ensures_once("flotilla").await.expect("first origin admits convoy");
    let first_store = first.resource_backend();
    let second_store = second.resource_backend();
    let first_snapshot = first_store
        .using::<PlacementPolicy>("flotilla")
        .list()
        .await
        .expect("first placement policies")
        .items
        .into_iter()
        .find(|policy| policy.metadata.name.starts_with("placement-snapshot-"))
        .expect("first placement snapshot");
    let first_root = first.node_id().clone();
    let mut snapshots = first_store.using::<PlacementPolicy>("flotilla").list().await.expect("first placement log");
    snapshots.items.retain(|policy| policy.metadata.name == first_snapshot.metadata.name);
    second_store
        .replica_writer::<PlacementPolicy>(first_root, "flotilla")
        .replace(&snapshots, Utc::now())
        .await
        .expect("replicate first origin's placement policies");

    second.reconcile_convoy_ensures_once("flotilla").await.expect("second origin admits same placement");
    let second_convoy = second_store.using::<ResourceConvoy>("flotilla").list().await.expect("second convoys").items;
    assert_eq!(second_convoy.len(), 1);
    assert_eq!(
        second_convoy[0].metadata.annotations.get(flotilla_resources::PLACEMENT_SNAPSHOT_ANNOTATION),
        Some(&first_snapshot.metadata.name)
    );
    assert!(!second_store
        .using::<PlacementPolicy>("flotilla")
        .list()
        .await
        .expect("second local placements")
        .items
        .iter()
        .any(|policy| policy.metadata.name == first_snapshot.metadata.name));
    assert!(flotilla_resources::home_bound_authorship_collisions(&first_store, "flotilla").await.expect("first diagnostics").is_empty());
    assert!(flotilla_resources::home_bound_authorship_collisions(&second_store, "flotilla").await.expect("second diagnostics").is_empty());
}

#[tokio::test]
async fn abandon_archive_skips_pushed_head_pushes_unpushed_head_and_reports_push_failure() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"archive-test\"\n").expect("daemon config");
    let runner = Arc::new(MockRunner::new(vec![
        Ok("git version 2.43.0".to_string()),
        Ok("git version 2.43.0".to_string()),
        Ok("archived".to_string()),
        Err("remote rejected".to_string()),
        Ok("archived stale head".to_string()),
    ]));
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let mut discovery = fake_discovery_with_runner(false, runner.clone());
    discovery.repo_detectors.clear();
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        discovery,
        HostName::local(),
        backend.clone(),
    )
    .await;
    let repository = RepositorySpec::remote("https://github.com/acme/archive").expect("repository spec").key();
    let checkouts = backend.using::<ResourceCheckout>("flotilla");
    let fresh = Utc::now().to_rfc3339();
    for (name, pushed, observed_at) in [
        ("already-pushed", ConditionValue::True, fresh.as_str()),
        ("needs-push", ConditionValue::False, fresh.as_str()),
        ("push-fails", ConditionValue::False, fresh.as_str()),
        ("stale-pushed", ConditionValue::True, "2020-01-01T00:00:00Z"),
    ] {
        let checkout = checkouts
            .create(
                &InputMeta::builder()
                    .name(name.to_string())
                    .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "archive-convoy".to_string())]))
                    .build(),
                &ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
                    r#ref: name.to_string(),
                    path: format!("/checkouts/{name}"),
                    repo_ref: repository.clone(),
                    host_ref: "archive-test".to_string(),
                    is_main: false,
                }),
            )
            .await
            .expect("checkout");
        checkouts
            .update_status(
                name,
                &checkout.metadata.resource_version,
                &ResourceCheckoutStatus::builder()
                    .phase(ResourceCheckoutPhase::Ready)
                    .path(format!("/checkouts/{name}"))
                    .integration(CheckoutIntegrationStatus {
                        pushed: IntegrationCondition::builder().value(pushed).observed_at(observed_at.to_string()).build(),
                        ..CheckoutIntegrationStatus::default()
                    })
                    .build(),
            )
            .await
            .expect("checkout status");
    }

    let outcomes = daemon.archive_convoy_checkouts_best_effort("flotilla", "archive-convoy").await.expect("best-effort archive");

    assert_eq!(outcomes.iter().map(|outcome| (outcome.checkout.as_str(), outcome.status)).collect::<Vec<_>>(), vec![
        ("already-pushed", CheckoutArchiveStatus::NothingToArchive),
        ("needs-push", CheckoutArchiveStatus::Archived),
        ("push-fails", CheckoutArchiveStatus::Failed),
        ("stale-pushed", CheckoutArchiveStatus::Archived),
    ]);
    assert_eq!(outcomes[2].detail.as_deref(), Some("remote rejected"));
    assert_eq!(
        runner.calls().iter().filter(|(command, args)| command == "git" && args.first().is_some_and(|arg| arg == "push")).count(),
        3
    );
}

#[tokio::test]
async fn bound_change_request_resolution_uses_durable_observation_for_a_mirror_checkout() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"bound-pr-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla")
        .expect("repository spec")
        .with_remotes(["https://github.com/flotilla-org/flotilla".to_string(), "https://forgejo.example/flotilla/flotilla".to_string()])
        .expect("mirror declaration");
    let repository_key = repository_spec.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&test_meta(&repository_key.to_string()), &repository_spec)
        .await
        .expect("repository");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend,
    )
    .await;
    daemon.set_provisioning_namespace("flotilla".to_string()).await;
    let change_requests = daemon.resource_backend().using::<ResourceChangeRequest>("flotilla");
    let change_request_name = change_request_record_name("github.com", "flotilla-org/flotilla", 1696);
    let observation = change_requests
        .create(
            &test_meta(&change_request_name),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("flotilla-org/flotilla".to_string())
                .number(1696)
                .observing_authority("github-observer".to_string())
                .build(),
        )
        .await
        .expect("change request observation");
    let observed_at = Utc::now();
    change_requests
        .update_status(&observation.metadata.name, &observation.metadata.resource_version, &flotilla_resources::ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: flotilla_resources::Observation::known(ObservedChangeRequestState::Open, observed_at),
            head_sha: flotilla_resources::Observation::unknown(observed_at),
            checks: flotilla_resources::Observation::unknown(observed_at),
            review: flotilla_resources::ChangeRequestReviewObservation {
                actionable_at_head: flotilla_resources::Observation::unknown(observed_at),
            },
            mergeable: flotilla_resources::Observation::unknown(observed_at),
        })
        .await
        .expect("change request status");

    let resolved = daemon
        .resolve_convoy_change_request(std::slice::from_ref(&repository_key), "fix/convoy-pr-linkage", Some("1696"))
        .await
        .expect("bound change request lookup")
        .expect("durable observation should resolve the bound change request");

    assert_eq!(resolved.id, "1696");
    assert_eq!(resolved.repository_key, repository_key);
    assert_eq!(resolved.status, flotilla_protocol::ChangeRequestStatus::Open);
}

struct BatchedObservationRunner {
    calls: std::sync::Mutex<Vec<String>>,
    rate_limit_two: std::sync::atomic::AtomicBool,
    block_one: std::sync::atomic::AtomicBool,
    one_started: tokio::sync::Notify,
    release_one: tokio::sync::Notify,
    conflicting: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl CommandRunner for BatchedObservationRunner {
    async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        match (cmd, args) {
            ("gh", ["--version"]) => Ok("gh version 2.49.0\n".into()),
            ("git", ["--version"]) => Ok("git version 2.43.0\n".into()),
            _ => Err(format!("unexpected command: {cmd} {args:?}")),
        }
    }

    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
    ) -> Result<crate::providers::CommandOutput, String> {
        if cmd != "gh" || args.first() != Some(&"api") || args.get(1) != Some(&"graphql") {
            return self.run(cmd, args, cwd, label).await.map(|stdout| crate::providers::CommandOutput {
                stdout,
                stderr: String::new(),
                success: true,
            });
        }
        let query = args.iter().find_map(|arg| arg.strip_prefix("query=")).ok_or("missing GraphQL query")?;
        self.calls.lock().expect("calls").push(query.to_string());
        if query.contains("name:\"one\"") && self.block_one.load(std::sync::atomic::Ordering::SeqCst) {
            self.one_started.notify_one();
            self.release_one.notified().await;
        }
        if query.contains("name:\"two\"") && self.rate_limit_two.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(crate::providers::CommandOutput {
                stdout: "HTTP/2 403 Forbidden\r\nX-RateLimit-Reset: 1893456000\r\n\r\n{\"message\":\"API rate limit exceeded\"}".into(),
                stderr: "gh: API rate limit exceeded".into(),
                success: false,
            });
        }
        let requests = query
            .split("pullRequest(number:")
            .skip(1)
            .filter_map(|tail| {
                let number = tail.split(')').next()?.parse::<u64>().ok()?;
                Some((
                    format!("pr{number}"),
                    serde_json::json!({
                        "state": "OPEN", "isDraft": false, "headRefOid": "abc", "reviewDecision": null,
                        "mergeable": if self.conflicting.load(std::sync::atomic::Ordering::SeqCst) { "CONFLICTING" } else { "MERGEABLE" },
                        "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": []}}}}]}
                    }),
                ))
            })
            .collect::<serde_json::Map<_, _>>();
        Ok(crate::providers::CommandOutput {
            stdout: format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": requests}})),
            stderr: String::new(),
            success: true,
        })
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }
}

#[tokio::test]
async fn live_bound_observation_batches_two_repositories_and_caches_rate_limit() {
    let runner = Arc::new(BatchedObservationRunner {
        calls: std::sync::Mutex::new(Vec::new()),
        rate_limit_two: std::sync::atomic::AtomicBool::new(false),
        block_one: std::sync::atomic::AtomicBool::new(false),
        one_started: tokio::sync::Notify::new(),
        release_one: tokio::sync::Notify::new(),
        conflicting: std::sync::atomic::AtomicBool::new(false),
    });
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"batch-observation-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery_with_runner(false, runner.clone()),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh")))
        .expect("gh discovery");
    let mut subjects = Vec::new();
    for (scope, ids) in [("team/one", vec![1, 2, 3]), ("team/two", vec![4, 5])] {
        let scheme = if scope == "team/one" { "http" } else { "https" };
        let repository = RepositorySpec::remote(format!("{scheme}://github.com/{scope}")).expect("repository");
        let repository_key = repository.key();
        backend.using::<Repository>("flotilla").create(&test_meta(&repository_key.to_string()), &repository).await.expect("repository");
        for id in ids {
            let mut spec = ConvoySpec::builder()
                .workflow_ref("test".to_string())
                .repositories(vec![ConvoyRepositorySpec {
                    url: format!("{scheme}://github.com/{scope}"),
                    repo_ref: repository_key.clone(),
                    source_ref: "main".into(),
                    target_ref: "main".into(),
                    workspace_slug: scope.replace('/', "-"),
                    subpaths: Vec::new(),
                }])
                .build();
            spec.change_request =
                Some(BoundChangeRequest { id: id.to_string(), repository_ref: repository_key.clone(), title: format!("PR {id}") });
            backend.using::<ResourceConvoy>("flotilla").create(&test_meta(&format!("convoy-{id}")), &spec).await.expect("convoy");
            subjects.push(ChangeRequestRef { namespace: "flotilla".into(), service: "github.com".into(), scope: scope.into(), number: id });
        }
    }
    for subject in &subjects {
        daemon.change_request_observation_source.observe(subject).await.expect("live observation");
    }
    assert_eq!(runner.calls.lock().expect("calls").len(), 2, "one GitHub request per repository for five bound CRs");

    let first = &subjects[0];
    let repository_key = backend
        .using::<Repository>("flotilla")
        .list()
        .await
        .expect("repositories")
        .items
        .into_iter()
        .find(|repository| repository.spec.forge().is_some_and(|forge| forge.repository == "team/one"))
        .expect("first repository")
        .spec
        .key();
    let mut spec = ConvoySpec::builder()
        .workflow_ref("test".to_string())
        .repositories(vec![ConvoyRepositorySpec {
            url: "http://github.com/team/one".into(),
            repo_ref: repository_key.clone(),
            source_ref: "main".into(),
            target_ref: "main".into(),
            workspace_slug: "team-one".into(),
            subpaths: Vec::new(),
        }])
        .build();
    spec.change_request = Some(BoundChangeRequest { id: "6".into(), repository_ref: repository_key, title: "PR 6".into() });
    backend.using::<ResourceConvoy>("flotilla").create(&test_meta("convoy-6"), &spec).await.expect("new convoy");
    daemon.change_request_observation_source.observe(first).await.expect("expanded batch");
    assert_eq!(runner.calls.lock().expect("calls").len(), 3, "a new bound CR invalidates its repository batch");

    runner.rate_limit_two.store(true, std::sync::atomic::Ordering::SeqCst);
    let limited = &subjects[3];
    let error = daemon.change_request_observation_source.observe_for_completion(limited).await.expect_err("fresh read is rate limited");
    assert!(error.contains("budget=GraphQL, identity=host gh login, reset_at="), "{error}");
    assert_eq!(runner.calls.lock().expect("calls").len(), 4);
    assert_eq!(daemon.change_request_observation_source.observe(limited).await.expect_err("cached rate limit"), error);
    assert_eq!(runner.calls.lock().expect("calls").len(), 4, "rate-limited repository waits for reset");

    runner.block_one.store(true, std::sync::atomic::Ordering::SeqCst);
    let started = runner.one_started.notified();
    let source = Arc::clone(&daemon.change_request_observation_source);
    let first = first.clone();
    let blocked = tokio::spawn(async move { source.observe_for_completion(&first).await });
    started.await;
    tokio::time::timeout(Duration::from_secs(1), daemon.change_request_observation_source.observe_for_completion(limited))
        .await
        .expect("second repository must not wait for first repository")
        .expect_err("second repository remains rate limited");
    runner.release_one.notify_one();
    blocked.await.expect("first observation task").expect("first repository resumes");
}

#[tokio::test]
async fn claim_message_pr_is_observed_and_repeated_conflicting_refusal_escalates() {
    #[derive(Default)]
    struct DeliveredTurns(std::sync::Mutex<Vec<crate::leaf_engine::TurnDeliveryRequest>>);
    #[async_trait]
    impl crate::leaf_engine::TurnDeliveryActuator for DeliveredTurns {
        async fn deliver(&self, request: &crate::leaf_engine::TurnDeliveryRequest) -> Result<flotilla_resources::TurnDeliveryRung, String> {
            self.0.lock().expect("turns").push(request.clone());
            Ok(flotilla_resources::TurnDeliveryRung::WarmSession)
        }

        async fn hold(&self, _: &crate::leaf_engine::TurnDeliveryRequest, _: &flotilla_resources::HoldAct, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"refused-claim-test\"\n").expect("daemon config");
    let runner = Arc::new(BatchedObservationRunner {
        calls: std::sync::Mutex::new(Vec::new()),
        rate_limit_two: std::sync::atomic::AtomicBool::new(false),
        block_one: std::sync::atomic::AtomicBool::new(false),
        one_started: tokio::sync::Notify::new(),
        release_one: tokio::sync::Notify::new(),
        conflicting: std::sync::atomic::AtomicBool::new(true),
    });
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery_with_runner(false, runner),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh")))
        .expect("gh discovery");
    let turns = Arc::new(DeliveredTurns::default());
    daemon.leaf_subscriptions.set_turn_delivery_actuator(turns.clone()).await;
    let repository = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository");
    let repository_key = repository.key();
    backend.using::<Repository>("flotilla").create(&test_meta(&repository_key.to_string()), &repository).await.expect("repository");
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .create(
            &test_meta("refused-claim"),
            &ConvoySpec::builder()
                .workflow_ref("implement-review".to_string())
                .repositories(vec![flotilla_resources::ConvoyRepositorySpec::builder()
                    .url("https://github.com/flotilla-org/flotilla".to_string())
                    .repo_ref(repository_key)
                    .source_ref("2205/escalate".to_string())
                    .target_ref("main".to_string())
                    .workspace_slug("flotilla".to_string())
                    .subpaths(Vec::new())
                    .build()])
                .build(),
        )
        .await
        .expect("convoy");
    let ready = flotilla_resources::CrewCompletionExpectation::Condition(flotilla_resources::CompletionCondition::ChangeRequest {
        field_path: ".ready".to_string(),
        operator: flotilla_protocol::LeafOperator::Equal,
        literal: "true".to_string(),
        optional_when_absent: false,
    });
    let coder = CrewSpec::builder()
        .role("coder".to_string())
        .source(CrewSource::Tool { command: "test".to_string() })
        .completion_conditions(vec![ready])
        .build();
    let bosun = CrewSpec::builder().role("bosun".to_string()).source(CrewSource::Tool { command: "test".to_string() }).build();
    convoys
        .update_status("refused-claim", &convoy.metadata.resource_version, &ConvoyStatus {
            phase: flotilla_resources::ConvoyPhase::Active,
            workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                stall_nudges: Default::default(),
                supervision: Some(vec![flotilla_resources::SupervisionTarget::ConvoyCrew {
                    vessel: "work".to_string(),
                    role: "bosun".to_string(),
                }]),
                exit: None,
                turn_delivery: indexmap::IndexMap::from([(
                    "conflicting".to_string(),
                    flotilla_resources::TurnDeliveryRule::builder()
                        .on("$cr.mergeable == conflicting".parse().expect("conflict leaf"))
                        .to(flotilla_resources::TurnDeliveryTarget::builder().vessel("work".to_string()).role("coder".to_string()).build())
                        .brief(
                            "Rebase onto the current base branch and file a fresh settlement claim; the previous claim is superseded."
                                .to_string(),
                        )
                        .hold(flotilla_resources::HoldAct::ChangeRequestComment { body: "paused".to_string() })
                        .build(),
                )]),
                vessels: vec![VesselRequirement::builder().name("work".to_string()).crew(vec![coder, bosun]).build()],
            }),
            work: BTreeMap::from([(
                "work".to_string(),
                flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Running).build(),
            )]),
            crew_work: BTreeMap::from([(
                "work".to_string(),
                BTreeMap::from([
                    ("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                    ("bosun".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                ]),
            )]),
            ..Default::default()
        })
        .await
        .expect("active status");
    backend
        .using::<Vessel>("flotilla")
        .create(&test_meta("refused-claim-vessel"), &VesselSpec {
            convoy_ref: "refused-claim".to_string(),
            vessel_name: "work".to_string(),
            placement_policy_ref: "test".to_string(),
            adopted_checkout_refs: BTreeMap::new(),
        })
        .await
        .expect("vessel");
    let session = backend
        .clone()
        .using::<ResourceTerminalSession>("flotilla")
        .create(
            &InputMeta::builder()
                .name("refused-claim-session".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "refused-claim".to_string()),
                    (VESSEL_LABEL.to_string(), "work".to_string()),
                    (ROLE_LABEL.to_string(), "coder".to_string()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "env".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: None, model: None },
                    brief: flotilla_resources::TerminalBrief {
                        artifact_digest: None,
                        path: "brief.md".to_string(),
                        content: "work".to_string(),
                        copies: vec![],
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "refused-claim".to_string(),
                        vessel_ref: "refused-claim-vessel".to_string(),
                    }),
                    message: None,
                },
                cwd: "/repo".to_string(),
                pool: "passthrough".to_string(),
            },
        )
        .await
        .expect("session");
    backend
        .using::<ResourceTerminalSession>("flotilla")
        .update_status(&session.metadata.name, &session.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            attention: Some(TerminalAttention {
                state: TerminalAttentionState::Idle,
                as_of: Utc::now(),
                source: TerminalAttentionSource::Hook,
            }),
            ..Default::default()
        })
        .await
        .expect("idle session");
    let context = CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".to_string()),
        convoy: Some("refused-claim".to_string()),
        vessel_ref: Some("refused-claim-vessel".to_string()),
        role: Some("coder".to_string()),
    };
    let claim = || {
        daemon.crew_complete_with_disposition_internal(
            &context,
            Some("https://github.com/flotilla-org/flotilla/pull/2200".to_string()),
            None,
            Some("https://github.com/flotilla-org/flotilla/pull/2200#issuecomment-1".to_string()),
        )
    };
    let first = claim().await.expect_err("conflicting PR must refuse claim");
    assert!(first.contains("cr/github.com/flotilla-org/flotilla/2200") && first.contains(".ready"), "{first}");
    let refused_status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
    assert!(
        refused_status.subjects.iter().any(|entry| entry.subject.id == "2200"),
        "the rejected completion still discovered a PR in the convoy's repository"
    );
    assert_ne!(refused_status.crew_work["work"]["coder"].phase, flotilla_resources::CrewWorkPhase::Done);
    let observed = backend
        .using::<ResourceChangeRequest>("flotilla")
        .get(&change_request_record_name("github.com", "flotilla-org/flotilla", 2200))
        .await
        .expect("claim-time observation");
    assert_eq!(observed.status.expect("status").mergeable.value, Some(flotilla_resources::ObservedMergeability::Conflicting));

    let (tx, _rx) = flotilla_resources::controller::WorkQueueSender::channel();
    let task = tokio::spawn(daemon.reconciler_wake_watch().spawn(backend.clone(), "flotilla".to_string(), tx));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if turns.0.lock().expect("turns").iter().any(|request| {
                request.source == "stall-nudge-1" && request.brief.contains("PR #2200 is conflicting") && request.brief.contains(".ready")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("refusal nudge");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if turns
                .0
                .lock()
                .expect("turns")
                .iter()
                .any(|request| request.source == "conflicting" && request.brief.contains("PR #2200 is conflicting"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("active conflict delivery");
    let second = claim().await.expect_err("same conflicting PR must refuse again");
    assert_eq!(second, first);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
            if status.stalled.as_ref().is_some_and(|stall| {
                stall.rung == flotilla_resources::StallRung::Bosun && stall.evidence.contains("settlement claim refused 2 times")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("supervision escalation");
    assert_eq!(turns.0.lock().expect("turns").iter().filter(|request| request.source.starts_with("stall-nudge")).count(), 1);
    task.abort();
    let (tx, _rx) = flotilla_resources::controller::WorkQueueSender::channel();
    let restarted = tokio::spawn(daemon.reconciler_wake_watch().spawn(backend.clone(), "flotilla".to_string(), tx));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        turns.0.lock().expect("turns").iter().filter(|request| request.source == "conflicting").count(),
        1,
        "a persisted turn episode prevents duplicate Active conflict delivery after restart"
    );
    restarted.abort();
}

#[test]
fn observation_service_matching_preserves_http_and_authority_port() {
    assert!(forge_service_matches("http://forgejo.local:3000", "forgejo.local:3000"));
    assert!(forge_service_matches("https://github.com/", "github.com"));
    assert!(!forge_service_matches("http://forgejo.local:3000", "forgejo.local:3001"));
}

#[tokio::test]
async fn contained_codex_to_claude_handoff_stages_credentials_for_the_latent_reviewer() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"two-crew-contained-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    let repository = RepositoryKey("github.com-flotilla-org-flotilla".to_string());
    let requirement = VesselRequirement::builder()
        .name("work".to_string())
        .credential_refs(BTreeSet::from(["claude-max".to_string(), "github-crew-pr".to_string()]))
        .credential_scopes(BTreeMap::from([
            ("claude-max".to_string(), BTreeSet::from([repository.clone()])),
            ("github-crew-pr".to_string(), BTreeSet::from([repository])),
        ]))
        .crew(vec![
            CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: Some("codex".to_string()), model: None },
                    prompt: None,
                    brief_template: None,
                })
                .build(),
            CrewSpec::builder()
                .role("reviewer".to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code-review".to_string(), adapter: Some("claude-code".to_string()), model: None },
                    prompt: Some("Review the coder's implementation.".to_string()),
                    brief_template: None,
                })
                .build(),
        ])
        .build();

    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .create(&test_meta("convoy-two-crew"), &ConvoySpec::builder().workflow_ref("implement-review".to_string()).build())
        .await
        .expect("convoy");
    convoys
        .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus {
            phase: flotilla_resources::ConvoyPhase::Active,
            workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                stall_nudges: Default::default(),
                supervision: None,
                exit: None,
                turn_delivery: Default::default(),
                vessels: vec![requirement.clone()],
            }),
            crew_work: BTreeMap::from([(
                "work".to_string(),
                BTreeMap::from([
                    ("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                    ("reviewer".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Pending).build()),
                ]),
            )]),
            ..Default::default()
        })
        .await
        .expect("active convoy");
    backend
        .clone()
        .using::<Vessel>("flotilla")
        .create(&test_meta("convoy-two-crew-work"), &flotilla_resources::VesselSpec {
            convoy_ref: "convoy-two-crew".to_string(),
            vessel_name: "work".to_string(),
            placement_policy_ref: "contained".to_string(),
            adopted_checkout_refs: BTreeMap::new(),
        })
        .await
        .expect("vessel");

    let requested = CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".to_string()),
        convoy: Some("convoy-two-crew".to_string()),
        vessel_ref: Some("convoy-two-crew-work".to_string()),
        role: Some("coder".to_string()),
    };
    let error =
        daemon.crew_handoff_internal(&requested, "reviewer", "Please review the implementation.").await.expect_err("missing anchor");
    assert!(error.contains("no active session to anchor"), "{error}");
    let status = convoys.get("convoy-two-crew").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending);

    let coder_identity = TerminalSessionIdentity::builder()
        .vessel_ref("convoy-two-crew-work".to_string())
        .convoy("convoy-two-crew".to_string())
        .vessel("work".to_string())
        .role("coder".to_string())
        .vessel_index(0)
        .crew_index(0)
        .build();
    let coder_meta = terminal_meta_with_vessel_credentials(coder_identity.input_meta(), &requirement);
    let coder = backend
        .clone()
        .using::<ResourceTerminalSession>("flotilla")
        .create(&coder_meta, &ResourceTerminalSessionSpec {
            env_ref: "contained-env".to_string(),
            role: "coder".to_string(),
            source: TerminalSessionSource::Agent {
                selector: Selector { capability: "code".to_string(), adapter: Some("codex".to_string()), model: None },
                brief: flotilla_resources::TerminalBrief {
                    artifact_digest: None,
                    path: ".flotilla/briefs/coder.md".to_string(),
                    content: "Implement the issue.".to_string(),
                    copies: Vec::new(),
                },
                context: Box::new(flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".to_string(),
                    convoy: "convoy-two-crew".to_string(),
                    vessel_ref: "convoy-two-crew-work".to_string(),
                }),
                message: None,
            },
            cwd: "/workspace".to_string(),
            pool: "contained".to_string(),
        })
        .await
        .expect("eager coder terminal");

    let reviewer_identity = TerminalSessionIdentity::builder()
        .vessel_ref("convoy-two-crew-work".to_string())
        .convoy("convoy-two-crew".to_string())
        .vessel("work".to_string())
        .role("reviewer".to_string())
        .vessel_index(0)
        .crew_index(1)
        .build();
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    let failed_reviewer = sessions
        .create(&reviewer_identity.input_meta(), &ResourceTerminalSessionSpec { role: "reviewer".to_string(), ..coder.spec.clone() })
        .await
        .expect("failed reviewer target");
    let failed_reviewer = sessions
        .update_status(&failed_reviewer.metadata.name, &failed_reviewer.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Failed,
            ..Default::default()
        })
        .await
        .expect("failed phase");
    let error = daemon.crew_handoff_internal(&requested, "reviewer", "Please review the implementation.").await.expect_err("failed target");
    assert!(error.contains("failed provisioning"), "{error}");
    let status = convoys.get("convoy-two-crew").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending);
    sessions.delete(&failed_reviewer.metadata.name).await.expect("remove failed target");
    let probe = Arc::new(SessionStagingProbe {
        backend: backend.clone(),
        session: coder.metadata.name.clone(),
        environment: "contained-env".to_string(),
        fail_next: std::sync::atomic::AtomicBool::new(true),
        invalidate_next: std::sync::atomic::AtomicBool::new(false),
        staged: std::sync::atomic::AtomicUsize::new(0),
    });
    daemon.set_work_credential_reconciler(probe.clone()).await;
    let error =
        daemon.crew_handoff_internal(&requested, "reviewer", "Please review the implementation.").await.expect_err("staging failure");
    assert!(error.contains("credential staging failed"), "{error}");
    let status = convoys.get("convoy-two-crew").await.expect("convoy").status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending);
    assert!(sessions.get("terminal-convoy-two-crew-work-reviewer").await.is_err());

    daemon.crew_handoff_internal(&requested, "reviewer", "Please review the implementation.").await.expect("handoff to latent reviewer");
    assert_eq!(probe.staged.load(std::sync::atomic::Ordering::SeqCst), 1);

    let reviewer = backend
        .using::<ResourceTerminalSession>("flotilla")
        .get("terminal-convoy-two-crew-work-reviewer")
        .await
        .expect("latent reviewer terminal");
    let TerminalSessionSource::Agent { selector, brief, .. } = &reviewer.spec.source else {
        panic!("reviewer must be an agent session");
    };
    assert_eq!(selector.adapter.as_deref(), Some("claude-code"));
    assert!(brief.content.contains("- Minted credential repository scope:"));
    assert!(brief.content.contains("  - `github-crew-pr`:\n    - `github.com-flotilla-org-flotilla`"));
    assert_eq!(reviewer.spec.env_ref, "contained-env");
    assert_eq!(reviewer.metadata.annotations.get(CREDENTIAL_REFS_ANNOTATION), Some(&r#"["claude-max","github-crew-pr"]"#.to_string()));
    assert_eq!(
        reviewer.metadata.annotations.get(CREDENTIAL_SCOPES_ANNOTATION),
        Some(&r#"{"claude-max":["github.com-flotilla-org-flotilla"],"github-crew-pr":["github.com-flotilla-org-flotilla"]}"#.to_string())
    );
    assert_eq!(reviewer.metadata.annotations, coder_meta.annotations);

    let convoy = convoys.get("convoy-two-crew").await.expect("convoy");
    let mut terminal_status = convoy.status.expect("status");
    terminal_status.phase = flotilla_resources::ConvoyPhase::Landed;
    convoys.update_status("convoy-two-crew", &convoy.metadata.resource_version, &terminal_status).await.expect("landed convoy");
    let error = daemon.crew_handoff_internal(&requested, "reviewer", "Late handoff").await.expect_err("terminal handoff");
    assert!(error.contains("terminal"), "{error}");
    assert_eq!(convoys.get("convoy-two-crew").await.expect("convoy").status.expect("status"), terminal_status);
    let reviewer_after = sessions.get(&reviewer.metadata.name).await.expect("reviewer");
    assert_eq!(reviewer_after.metadata.resource_version, reviewer.metadata.resource_version);
}

pub(super) async fn create_identity_convoy(backend: &ResourceBackend, record: &str, role: &str, project: Option<&str>) {
    let labels = BTreeMap::from([
        (PROJECT_LABEL.to_string(), project.unwrap_or_default().to_string()),
        (ROLE_LABEL.to_string(), role.to_string()),
        (GENERATION_LABEL.to_string(), "1".to_string()),
    ]);
    let mut spec = ConvoySpec::builder().workflow_ref("review".to_string()).role(role.to_string()).generation(1).build();
    spec.project_ref = project.map(str::to_string);
    backend
        .clone()
        .using::<ResourceConvoy>("flotilla")
        .create(&InputMeta::builder().name(record.to_string()).labels(labels).build(), &spec)
        .await
        .expect("convoy");
}

async fn seed_convoy_routing_row(
    daemon: &InProcessDaemon,
    record: &str,
    role: Option<&str>,
    project: Option<&str>,
    phase: flotilla_protocol::ConvoyPhase,
) {
    let resource = flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", record).on_host(daemon.host_name.clone());
    let mut row = flotilla_protocol::ConvoyRow::builder()
        .resource(resource.clone())
        .maybe_address_role(role.map(str::to_string))
        .name(role.unwrap_or(record).to_string())
        .workflow_ref("review".to_string())
        .phase(phase)
        .build();
    row.project_ref = project.map(str::to_string);
    daemon.aggregator_projection_state().await.write().await.local_rows.insert(resource, row);
}

#[test]
fn convoy_role_addresses_reject_malformed_values() {
    assert_eq!(parse_role_address("reviewer"), Ok(("reviewer", None)));
    assert_eq!(parse_role_address("reviewer@flotilla"), Ok(("reviewer", Some("flotilla"))));
    assert_eq!(parse_role_address("reviewer@"), Ok(("reviewer", Some(""))));
    for value in ["@project", "a@b@c"] {
        assert_eq!(parse_role_address(value), Err(format!("invalid convoy address `{value}`: expected role@project")));
    }
    assert_eq!(parse_role_address(""), Err("convoy role cannot be empty".to_string()));
}

#[test]
fn qualified_role_address_is_a_typed_project_role_pair() {
    assert_eq!(
        RoleAddress::from_str("governor@andamento"),
        Ok(RoleAddress { project: "andamento".to_string(), role: "governor".to_string() })
    );
    for value in ["governor", "@andamento", "governor@", "governor@andamento@extra"] {
        assert!(RoleAddress::from_str(value).is_err(), "{value} must not produce a qualified address");
    }
}

#[test]
fn managed_terminal_changes_are_field_scoped_and_deduplicated() {
    let id = flotilla_protocol::AttachableId::new("pane-1");
    let running = ManagedTerminal {
        set_id: flotilla_protocol::AttachableSetId::new("set-1"),
        role: "server".to_string(),
        command: "npm start".to_string(),
        working_directory: "/work/flotilla".into(),
        status: flotilla_protocol::TerminalStatus::Running,
        attention: None,
    };
    let current = HashMap::from([(id.clone(), running.clone())]);
    assert!(matches!(
        managed_terminal_changes(None, &current).as_slice(),
        [Change::ManagedTerminal { key, op: EntryOp::Added(terminal) }] if key == &id && terminal == &running
    ));
    assert!(managed_terminal_changes(Some(&current), &current).is_empty());

    let mut exited = running;
    exited.status = flotilla_protocol::TerminalStatus::Exited(7);
    exited.attention = Some(flotilla_protocol::PaneExitAttention { exit_code: 7 });
    let updated = HashMap::from([(id.clone(), exited.clone())]);
    assert!(matches!(
        managed_terminal_changes(Some(&current), &updated).as_slice(),
        [Change::ManagedTerminal { key, op: EntryOp::Updated(terminal) }] if key == &id && terminal == &exited
    ));
    assert!(matches!(
        managed_terminal_changes(Some(&updated), &HashMap::new()).as_slice(),
        [Change::ManagedTerminal { key, op: EntryOp::Removed }] if key == &id
    ));
}

#[tokio::test]
async fn managed_terminal_refresh_assigns_nested_cwd_to_most_specific_repo() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"pane-owner-test\"\n").expect("daemon config");
    let canonical_outer = temp.path().join("private").join("outer");
    let canonical_inner = canonical_outer.join("nested");
    let configured_outer = temp.path().join("outer-alias");
    let configured_inner = configured_outer.join("nested");
    std::fs::create_dir_all(&canonical_inner).expect("nested repository roots");
    std::os::unix::fs::symlink(&canonical_outer, &configured_outer).expect("configured repository alias");

    let pool = Arc::new(FakeTerminalPool::new());
    let terminal_id = flotilla_protocol::AttachableId::new("pane-1");
    let working_directory = ExecutionEnvironmentPath::new(canonical_inner.join("app"));
    let metadata = ManagedSessionMetadata::builder()
        .set_id(flotilla_protocol::AttachableSetId::new("set-1"))
        .attachable_id(terminal_id.clone())
        .checkout("nested".to_string())
        .role("server".to_string())
        .index(0)
        .working_directory(working_directory.clone())
        .build();
    pool.add_sessions(vec![TerminalSession {
        session_name: managed_session_name(&metadata),
        status: flotilla_protocol::TerminalStatus::Exited(7),
        command: Some("npm start".to_string()),
        working_directory: Some(working_directory),
        screen_activity: None,
    }])
    .await;
    let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_terminal_pool(pool.clone()));
    let daemon = InProcessDaemon::new(
        vec![configured_outer, configured_inner.clone()],
        Arc::new(ConfigStore::with_base(temp.path())),
        discovery,
        HostName::new("local-host"),
    )
    .await;
    let mut events = daemon.subscribe();

    daemon.refresh_managed_terminal_attention().await;

    let deltas = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            DaemonEvent::RepoDelta(delta) => Some(delta),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deltas.len(), 1, "one pane must be attributed to one repository");
    assert_eq!(deltas[0].repo_identity, fallback_repo_identity(&configured_inner));
    assert!(matches!(
        deltas[0].changes.as_slice(),
        [Change::ManagedTerminal { key, op: EntryOp::Added(terminal) }]
            if key == &terminal_id && terminal.attention == Some(flotilla_protocol::PaneExitAttention { exit_code: 7 })
    ));
}

pub(super) fn test_meta(name: &str) -> InputMeta {
    InputMeta::builder().name(name.to_string()).build()
}

#[tokio::test]
async fn convoy_role_resolution_can_disambiguate_a_projectless_convoy() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    create_identity_convoy(&backend, "convoy-one", "reviewer", None).await;
    create_identity_convoy(&backend, "convoy-two", "reviewer", Some("beta")).await;

    assert_eq!(
        resolve_local_convoy_name(&backend, "flotilla", "reviewer").await,
        Err("convoy role `reviewer` is ambiguous; use one of: reviewer@, reviewer@beta".to_string())
    );
    assert_eq!(resolve_local_convoy_name(&backend, "flotilla", "reviewer@").await, Ok("convoy-one".to_string()));
    assert_eq!(resolve_local_convoy_name(&backend, "flotilla", "reviewer@beta").await, Ok("convoy-two".to_string()));
}

#[tokio::test]
async fn convoy_resolution_falls_back_to_a_unique_terminal_generation() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    create_identity_convoy(&backend, "convoy-one", "reviewer", Some("flotilla")).await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys.get("convoy-one").await.expect("terminal convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Landed,
            ..Default::default()
        })
        .await
        .expect("mark convoy terminal");

    assert_eq!(resolve_local_convoy_name(&backend, "flotilla", "reviewer@flotilla").await, Ok("convoy-one".to_string()));
}

#[tokio::test]
async fn convoy_resolution_refuses_multiple_terminal_generations() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    create_identity_convoy(&backend, "convoy-one", "reviewer", Some("flotilla")).await;
    create_identity_convoy(&backend, "convoy-two", "reviewer", Some("flotilla")).await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    for name in ["convoy-one", "convoy-two"] {
        let created = convoys.get(name).await.expect("terminal convoy");
        convoys
            .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Failed,
                ..Default::default()
            })
            .await
            .expect("mark convoy terminal");
    }

    assert_eq!(
        resolve_local_convoy_name(&backend, "flotilla", "reviewer@flotilla").await,
        Err("convoy address `reviewer@flotilla` matches multiple terminal records; use an exact record name: convoy-one, convoy-two"
            .to_string())
    );
}

#[tokio::test]
async fn refused_convoy_reclaim_leaves_runtime_children_untouched() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    let repository = RepositoryKey("github.com-acme-standing".to_string());
    let convoy_name = "failed-before-checkout";
    let vessel_name = "failed-before-checkout-work";
    let terminal_name = "terminal-failed-before-checkout-work-coder";
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &test_meta(convoy_name),
            &ConvoySpec::builder()
                .workflow_ref("quartermaster".to_string())
                .adopted_checkout_refs(BTreeMap::from([(repository, "checkout-never-provisioned".to_string())]))
                .build(),
        )
        .await
        .expect("convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Failed,
            ..Default::default()
        })
        .await
        .expect("failed convoy");
    backend
        .clone()
        .using::<Vessel>("flotilla")
        .create(
            &InputMeta::builder()
                .name(vessel_name.to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_name.to_string())]))
                .build(),
            &flotilla_resources::VesselSpec {
                convoy_ref: convoy_name.to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "contained".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("vessel");
    backend
        .clone()
        .using::<ResourceTerminalSession>("flotilla")
        .create(
            &InputMeta::builder()
                .name(terminal_name.to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_name.to_string())]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "environment-still-live".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/workspace".to_string(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("terminal session");

    let refusal = daemon.reap_convoy_internal("flotilla", convoy_name, false).await.expect_err("unsafe reclaim must be refused");

    assert!(refusal.contains("missing checkout integration evidence"));
    assert!(convoys.get(convoy_name).await.is_ok(), "refusal must retain the convoy");
    assert!(backend.clone().using::<Vessel>("flotilla").get(vessel_name).await.is_ok(), "refusal must retain the vessel");
    assert!(
        backend.clone().using::<ResourceTerminalSession>("flotilla").get(terminal_name).await.is_ok(),
        "refusal must retain the terminal session"
    );

    let principal = PrincipalRef::implicit_for_namespace("flotilla");
    daemon
        .abandon_convoy_internal("flotilla", convoy_name, "operator accepts the unprovisioned checkout", Some(&principal))
        .await
        .expect("the refused shape must remain recoverable through convoy abandon");
    let abandoned = convoys.get(convoy_name).await.expect("abandon retains the convoy record");
    assert_eq!(abandoned.status.expect("abandoned status").phase, ConvoyPhase::Abandoned);
}

#[tokio::test]
async fn convoy_routing_falls_back_to_a_unique_terminal_generation_and_refuses_multiple() {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture().await;
    seed_convoy_routing_row(&daemon, "convoy-one", Some("reviewer"), Some("flotilla"), flotilla_protocol::ConvoyPhase::Landed).await;
    let action = flotilla_protocol::CommandAction::ConvoyDelete { namespace: None, name: "reviewer@flotilla".to_string(), force: false };

    let target = daemon.resolve_existing_convoy_target(&action).await.expect("route sole terminal generation").expect("routing target");
    assert_eq!(target.home, daemon.host_name);

    seed_convoy_routing_row(&daemon, "convoy-two", Some("reviewer"), Some("flotilla"), flotilla_protocol::ConvoyPhase::Failed).await;
    assert_eq!(
        daemon.resolve_existing_convoy_target(&action).await,
        Err("convoy address `reviewer@flotilla` matches multiple terminal records; use an exact record name: convoy-one, convoy-two"
            .to_string())
    );
}

#[tokio::test]
async fn convoy_routing_prefers_an_exact_terminal_pre_identity_record() {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture().await;
    seed_convoy_routing_row(&daemon, "pre-identity-record", None, None, flotilla_protocol::ConvoyPhase::Landed).await;
    let action = flotilla_protocol::CommandAction::ConvoyDelete { namespace: None, name: "pre-identity-record".to_string(), force: false };

    let target = daemon.resolve_existing_convoy_target(&action).await.expect("route exact terminal record").expect("routing target");
    assert_eq!(target.home, daemon.host_name);
}

#[tokio::test]
async fn convoy_routing_does_not_treat_a_legacy_display_name_as_role_identity() {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture().await;
    seed_convoy_routing_row(&daemon, "reviewer", None, Some("flotilla"), flotilla_protocol::ConvoyPhase::Landed).await;
    seed_convoy_routing_row(&daemon, "convoy-one", Some("reviewer"), Some("flotilla"), flotilla_protocol::ConvoyPhase::Landed).await;
    let action = flotilla_protocol::CommandAction::ConvoyDelete { namespace: None, name: "reviewer@flotilla".to_string(), force: false };

    let target = daemon.resolve_existing_convoy_target(&action).await.expect("route explicit role identity").expect("routing target");
    assert_eq!(target.home, daemon.host_name);
}

#[tokio::test]
async fn convoy_resolution_prefers_an_exact_unlabelled_record_name() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("pre-identity-record".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("review".to_string()).build(),
        )
        .await
        .expect("pre-identity convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Landed,
            ..Default::default()
        })
        .await
        .expect("mark convoy terminal");

    assert_eq!(resolve_local_convoy_name(&backend, "flotilla", "pre-identity-record").await, Ok("pre-identity-record".to_string()));
}

#[tokio::test]
async fn convoy_explain_addresses_an_exact_terminal_pre_identity_record() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("pre-identity-record".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("review".to_string()).build(),
        )
        .await
        .expect("pre-identity convoy");
    convoys
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Failed,
            ..Default::default()
        })
        .await
        .expect("mark convoy terminal");

    let explanation = daemon.explain_convoy_internal(None, "pre-identity-record").await.expect("explain terminal record");
    assert_eq!(explanation.convoy, "pre-identity-record");
    assert_eq!(explanation.phase, "Failed");
}

#[tokio::test]
async fn convoy_explain_refuses_multiple_terminal_generations() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    create_identity_convoy(&backend, "convoy-one", "reviewer", Some("flotilla")).await;
    create_identity_convoy(&backend, "convoy-two", "reviewer", Some("flotilla")).await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    for name in ["convoy-one", "convoy-two"] {
        let created = convoys.get(name).await.expect("terminal convoy");
        convoys
            .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Landed,
                ..Default::default()
            })
            .await
            .expect("mark convoy terminal");
    }

    assert_eq!(
        daemon.explain_convoy_internal(None, "reviewer@flotilla").await,
        Err("convoy address `reviewer@flotilla` matches multiple terminal records; use an exact record name: convoy-one, convoy-two"
            .to_string())
    );
}

#[tokio::test]
async fn convoy_explain_rejects_projectless_and_project_bound_role_ambiguity() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    create_identity_convoy(&backend, "convoy-one", "reviewer", None).await;
    create_identity_convoy(&backend, "convoy-two", "reviewer", Some("beta")).await;

    assert_eq!(
        daemon.explain_convoy_internal(None, "reviewer").await.expect_err("bare role must be ambiguous"),
        "convoy role `reviewer` is ambiguous; use one of: reviewer@, reviewer@beta"
    );
}

#[tokio::test]
async fn projectless_convoys_do_not_share_an_identity_bucket_with_a_project_named_standalone() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let record = convoy_record_name();
    let generation = allocate_convoy_generation(&backend, "flotilla", None, "worker").await.expect("projectless identity");
    let labels = BTreeMap::from([
        (PROJECT_LABEL.to_string(), String::new()),
        (ROLE_LABEL.to_string(), "worker".to_string()),
        (GENERATION_LABEL.to_string(), generation.to_string()),
    ]);
    let spec = ConvoySpec::builder().workflow_ref("work".to_string()).role("worker".to_string()).generation(generation).build();
    convoys.create(&InputMeta::builder().name(record).labels(labels).build(), &spec).await.expect("projectless convoy");

    assert!(allocate_convoy_generation(&backend, "flotilla", Some("standalone"), "worker").await.is_ok());
}

pub(super) async fn standing_ensure_fixture() -> (Arc<InProcessDaemon>, ResourceBackend, Arc<VirtualClock>, tempfile::TempDir) {
    standing_ensure_fixture_for("local", true).await
}

async fn standing_ensure_fixture_for(
    host: &str,
    materialize_ensure: bool,
) -> (Arc<InProcessDaemon>, ResourceBackend, Arc<VirtualClock>, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), format!("machine_id = \"standing-{host}\"\n")).expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let now = Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).single().expect("timestamp");
    let clock = Arc::new(VirtualClock::new(now));
    let daemon = InProcessDaemon::new_with_resource_backend_and_clock(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new(host),
        backend.clone(),
        clock.clone(),
    )
    .await;
    let repository_spec = RepositorySpec::remote("https://github.com/acme/standing").expect("repository spec");
    let repository_key = repository_spec.key();
    backend.using::<Repository>("flotilla").create(&test_meta(&repository_key.to_string()), &repository_spec).await.expect("repository");
    if materialize_ensure {
        backend
            .definitions::<Project>("flotilla")
            .create(
                &test_meta("standing-project"),
                &ProjectSpec::builder()
                    .display_name("Standing Project".to_string())
                    .default_workflow_ref("quartermaster".to_string())
                    .repositories(vec![ProjectRepositorySpec {
                        repo: repository_key.clone(),
                        alias: Some("app".to_string()),
                        roles: BTreeSet::from([ProjectRepositoryRole::Code]),
                        subpath: None,
                        default_branch: Some("main".to_string()),
                    }])
                    .build(),
            )
            .await
            .expect("project");
    }
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(
            &InputMeta::builder()
                .name(crate::ops_entry::materialized_workflow_name("standing-project", "quartermaster"))
                .annotations(BTreeMap::from([(MATERIALIZED_PROJECT_ANNOTATION.to_string(), "standing-project".to_string())]))
                .build(),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .repository_refs(vec![repository_key.clone()])
                    .crew(Vec::new())
                    .build()])
                .build(),
        )
        .await
        .expect("standing workflow");
    if materialize_ensure {
        backend
            .definitions::<ConvoyEnsure>("flotilla")
            .create(
                &InputMeta::builder()
                    .name("quartermaster".to_string())
                    .annotations(BTreeMap::from([
                        (MATERIALIZED_PROJECT_ANNOTATION.to_string(), "standing-project".to_string()),
                        (SOURCE_REPOSITORY_ANNOTATION.to_string(), repository_key.to_string()),
                        (SOURCE_COMMIT_ANNOTATION.to_string(), "abc123".to_string()),
                        (SOURCE_ENTRY_PATH_ANNOTATION.to_string(), "ops/quartermaster.md".to_string()),
                    ]))
                    .build(),
                &ConvoyEnsureSpec {
                    project_ref: "standing-project".to_string(),
                    role: "quartermaster".to_string(),
                    driver_ref: None,
                    workflow_ref: "quartermaster".to_string(),
                    placement_policy: None,
                    escalation_reason: None,
                    repositories: vec![repository_key],
                    presents_as: Some("fleet".to_string()),
                    agent_overrides: Vec::new(),
                },
            )
            .await
            .expect("ensure declaration");
    }
    (daemon, backend, clock, temp)
}

struct VerifiedDeadBacking;

#[async_trait]
impl StandingConvoyBackingInspector for VerifiedDeadBacking {
    async fn verify_backing_dead(&self, _convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String> {
        Ok(())
    }
}

struct RecordlessBacking;

#[async_trait]
impl StandingConvoyBackingInspector for RecordlessBacking {
    async fn verify_backing_dead(&self, _convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String> {
        Err("no backing environment evidence is available".to_string())
    }
}

async fn fail_ensured_generation(backend: &ResourceBackend, clock: &VirtualClock) -> String {
    let convoy_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .and_then(|status| status.convoy_ref)
        .expect("live generation");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys.get(&convoy_ref).await.expect("generation");
    convoys
        .update_status(&convoy_ref, &convoy.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Failed,
            message: Some("placement failed".to_string()),
            started_at: Some(clock.now()),
            finished_at: Some(clock.now()),
            ..Default::default()
        })
        .await
        .expect("fail generation");
    convoy_ref
}

async fn fail_latest_ensured_generation(backend: &ResourceBackend, clock: &VirtualClock) -> String {
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .list()
        .await
        .expect("generations")
        .items
        .into_iter()
        .max_by_key(|convoy| convoy.spec.generation)
        .expect("latest generation");
    clock.set(convoy.metadata.creation_timestamp);
    convoys
        .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Failed,
            message: Some("placement failed".to_string()),
            started_at: Some(clock.now()),
            finished_at: Some(clock.now()),
            ..Default::default()
        })
        .await
        .expect("fail generation");
    convoy.metadata.name
}

async fn configure_standing_ensure_agent(backend: &ResourceBackend, overrides: Vec<flotilla_protocol::AgentOverride>) {
    create_docker_placement(backend, "standing-agent", "standing-agent-host", BTreeSet::new()).await;
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts.get("standing-agent-host").await.expect("standing agent host");
    let mut status = host.status.expect("standing agent host status");
    status.disk_free_bytes = Some(100 * 1024 * 1024 * 1024);
    status.admission_free_space_floor_bytes = Some(20 * 1024 * 1024 * 1024);
    hosts.update_status(&host.metadata.name, &host.metadata.resource_version, &status).await.expect("standing agent host capacity");
    let workflows = backend.using::<WorkflowTemplate>("flotilla");
    let mut workflow = workflows.get("standing-project--quartermaster").await.expect("standing workflow");
    workflow.spec.vessels[0].crew = vec![CrewSpec::builder()
        .role("governor".to_string())
        .source(CrewSource::Agent {
            selector: Selector { capability: "governor".to_string(), adapter: Some("codex".to_string()), model: None },
            prompt: None,
            brief_template: None,
        })
        .build()];
    workflows
        .update(&InputMeta::from(&workflow.metadata), &workflow.metadata.resource_version, &workflow.spec)
        .await
        .expect("update standing workflow");

    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let mut ensure = ensures.get("quartermaster").await.expect("standing ensure");
    ensure.spec.placement_policy = Some("standing-agent".to_string());
    ensure.spec.agent_overrides = overrides;
    ensures
        .update(&InputMeta::from(&ensure.metadata), &ensure.metadata.resource_version, &ensure.spec)
        .await
        .expect("update standing ensure");
}

async fn admitted_standing_workflow(daemon: &InProcessDaemon, backend: &ResourceBackend) -> WorkflowTemplateSpec {
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("admit standing convoy");
    let convoy = backend
        .using::<ResourceConvoy>("flotilla")
        .list()
        .await
        .expect("list admitted convoys")
        .items
        .into_iter()
        .next()
        .expect("admitted convoy");
    let snapshot = convoy.metadata.annotations.get(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION).expect("workflow snapshot annotation");
    backend.using::<WorkflowTemplate>("flotilla").get(snapshot).await.expect("workflow snapshot").spec
}

#[tokio::test]
async fn standing_ensure_applies_agent_overrides_to_the_admitted_workflow_snapshot() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    configure_standing_ensure_agent(&backend, vec![flotilla_protocol::AgentOverride {
        capability: "governor".to_string(),
        adapter: "codex".to_string(),
        model: Some("fable".to_string()),
    }])
    .await;

    let workflow = admitted_standing_workflow(&daemon, &backend).await;
    let CrewSource::Agent { selector, .. } = &workflow.vessels[0].crew[0].source else { panic!("governor must remain an agent") };
    assert_eq!(selector.adapter.as_deref(), Some("codex"));
    assert_eq!(selector.model.as_deref(), Some("fable"));
}

#[tokio::test]
async fn standing_ensure_without_agent_overrides_preserves_the_workflow_selector() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    configure_standing_ensure_agent(&backend, Vec::new()).await;

    let workflow = admitted_standing_workflow(&daemon, &backend).await;
    let CrewSource::Agent { selector, .. } = &workflow.vessels[0].crew[0].source else { panic!("governor must remain an agent") };
    assert_eq!(selector.adapter.as_deref(), Some("codex"));
    assert_eq!(selector.model, None);
}

#[derive(Default)]
struct RecordingBriefArtifacts {
    writes: tokio::sync::Mutex<Vec<(String, String, Vec<u8>)>>,
}

#[async_trait]
impl BriefArtifactWriter for RecordingBriefArtifacts {
    async fn put_brief(&self, _namespace: &str, convoy: &str, role: &str, subject: &str, content: &[u8]) -> Result<String, String> {
        assert_eq!(subject, convoy);
        self.writes.lock().await.push((convoy.to_string(), role.to_string(), content.to_vec()));
        Ok(format!("{:x}", Sha256::digest(content)))
    }
}

#[tokio::test]
async fn standing_readmission_writes_a_new_brief_artifact() {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    configure_standing_ensure_agent(&backend, Vec::new()).await;
    let writer = Arc::new(RecordingBriefArtifacts::default());
    daemon.set_brief_artifact_writer(writer.clone()).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    let first = fail_ensured_generation(&backend, &clock).await;
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("record dead generation");
    clock.advance(ChronoDuration::seconds(30));
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("readmit governor");
    let writes = writer.writes.lock().await;
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].0, first);
    assert_ne!(writes[0].0, writes[1].0);
    for (convoy, role, body) in writes.iter() {
        assert_eq!(role, "governor");
        assert!(String::from_utf8_lossy(body).contains(convoy));
        let admitted = backend.using::<ResourceConvoy>("flotilla").get(convoy).await.expect("admitted convoy");
        assert!(admitted.metadata.annotations.contains_key(BRIEF_ARTIFACTS_ANNOTATION));
    }
}

#[tokio::test]
async fn admission_rejects_agent_roles_reused_across_vessels_before_writing_briefs() {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture().await;
    let writer = Arc::new(RecordingBriefArtifacts::default());
    daemon.set_brief_artifact_writer(writer.clone()).await;
    let agent = || {
        CrewSpec::builder()
            .role("coder".to_string())
            .source(CrewSource::Agent { selector: Selector::for_capability("coding"), prompt: None, brief_template: None })
            .build()
    };
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![
            VesselRequirement::builder().name("implement".to_string()).crew(vec![agent()]).build(),
            VesselRequirement::builder().name("verify".to_string()).crew(vec![agent()]).build(),
        ])
        .build();
    let spec = ConvoySpec::builder().workflow_ref("workflow".to_string()).build();
    let error = daemon.write_admission_briefs("flotilla", "convoy-ambiguous", &spec, &workflow).await.expect_err("duplicate role");
    assert!(error.contains("agent role `coder` occurs in vessels `implement` and `verify`"), "{error}");
    assert!(writer.writes.lock().await.is_empty(), "admission must not publish a partial set of briefs");
}

#[tokio::test]
async fn standing_ensure_holds_after_three_failed_generations_and_resumes_when_attention_is_cleared() {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");

    for delay in [30, 60] {
        fail_ensured_generation(&backend, &clock).await;
        daemon
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("record failed generation");
        clock.advance(ChronoDuration::seconds(delay));
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("admit replacement");
    }

    fail_ensured_generation(&backend, &clock).await;
    assert_eq!(
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("exhaust retry budget"),
        vec!["ConvoyEnsure/quartermaster exhausted restart budget"]
    );
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let held = ensures.get("quartermaster").await.expect("held ensure");
    assert_eq!(held.status.as_ref().expect("status").restart_count, 3);
    assert_eq!(held.status.as_ref().expect("status").hold_reason, Some(ConvoyEnsureHoldReason::RestartLimit));
    let demands = backend.using::<ResourceDemand>("flotilla");
    let demand = demands.get("ensure-attention-quartermaster").await.expect("restart escalation");
    assert!(demand.spec.expiry.is_some(), "restart exhaustion must carry an escalation deadline");

    clock.advance(ChronoDuration::hours(6));
    assert!(daemon
        .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
        .await
        .expect("remain held")
        .is_empty());
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 3);

    apply_resource_status_patch(&demands, "ensure-attention-quartermaster", &DemandStatusPatch::Acknowledge {
        as_of: clock.now(),
        authority: "operator".to_string(),
    })
    .await
    .expect("operator acknowledges escalation");
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("clear hold");
    assert!(
        matches!(demands.get("ensure-attention-quartermaster").await, Err(ResourceError::NotFound { .. })),
        "clearing a restart hold must retire its resolved demand before another hold can reuse the name"
    );
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("schedule fresh episode");
    clock.advance(ChronoDuration::seconds(30));
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("resume admission");
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 4);
}

#[tokio::test]
async fn reconcile_now_resets_backoff_and_admits_the_next_ensure_generation_immediately() {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    fail_ensured_generation(&backend, &clock).await;
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("record backoff");
    let backed_off = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("backed-off ensure");
    assert_eq!(backed_off.status.as_ref().expect("status").restart_count, 1);
    assert!(backed_off.status.as_ref().expect("status").retry_at.is_some());

    let outcome = daemon.reconcile_convoy_ensure_now("flotilla", "quartermaster", &VerifiedDeadBacking).await.expect("forced admission");

    assert_eq!(outcome, "started quartermaster@standing-project");
    let reconciled = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("reconciled ensure");
    let status = reconciled.status.expect("status");
    assert_eq!(status.restart_count, 0);
    assert_eq!(status.retry_at, None);
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 2);
}

#[tokio::test]
async fn reconcile_now_acknowledges_recordless_teardown_and_readmits_the_ensure() {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let failed_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .and_then(|status| status.convoy_ref)
        .expect("live generation");
    let failed = convoys.get(&failed_ref).await.expect("generation");
    convoys
        .update_status(&failed_ref, &failed.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Failed,
            provisioning: Some(ConvoyProvisioningState::Started { started_at: clock.now() }),
            message: Some("clone failed before the work environment was provisioned".to_string()),
            finished_at: Some(clock.now()),
            ..Default::default()
        })
        .await
        .expect("record clone-failed generation");

    assert_eq!(
        daemon
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &RecordlessBacking)
            .await
            .expect("automatic reconcile holds when bypass deletion removed the backing records"),
        vec!["ConvoyEnsure/quartermaster held for operator attention"]
    );

    let outcome = daemon
        .reconcile_convoy_ensure_now("flotilla", "quartermaster", &RecordlessBacking)
        .await
        .expect("reconcile-now acknowledges the record wipe");

    assert_eq!(outcome, "started quartermaster@standing-project");
    let generations = convoys.list().await.expect("standing generations").items;
    assert_eq!(generations.len(), 2);
    assert!(generations.iter().any(|convoy| convoy.metadata.name == failed_ref && convoy.spec.generation == 1));
    assert!(generations.iter().any(|convoy| convoy.spec.generation == 2));
    assert!(matches!(
        backend.using::<ResourceDemand>("flotilla").get("ensure-attention-quartermaster").await,
        Err(ResourceError::NotFound { .. })
    ));
}

#[tokio::test]
async fn reconcile_now_clears_an_active_restart_limit_and_admits_in_one_pass() {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    for delay in [30, 60] {
        fail_ensured_generation(&backend, &clock).await;
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("record failure");
        clock.advance(ChronoDuration::seconds(delay));
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("restart");
    }
    fail_ensured_generation(&backend, &clock).await;
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("exhaust restart budget");

    let outcome = daemon.reconcile_convoy_ensure_now("flotilla", "quartermaster", &VerifiedDeadBacking).await.expect("forced restart");

    assert_eq!(outcome, "started quartermaster@standing-project");
    let status = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("ensure").status.expect("status");
    assert_eq!(status.restart_count, 0);
    assert_eq!(status.retry_at, None);
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 4);
    assert!(matches!(
        backend.using::<ResourceDemand>("flotilla").get("ensure-attention-quartermaster").await,
        Err(ResourceError::NotFound { .. })
    ));
}

#[tokio::test]
async fn concurrent_periodic_and_explicit_ensure_admission_creates_only_one_live_generation() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    let (left, right) = tokio::join!(
        daemon.reconcile_convoy_ensures_once("flotilla"),
        daemon.reconcile_convoy_ensure_now("flotilla", "quartermaster", &RecordlessBacking)
    );
    left.expect("left reconcile");
    right.expect("right reconcile");

    let live = backend
        .using::<ResourceConvoy>("flotilla")
        .list()
        .await
        .expect("convoys")
        .items
        .into_iter()
        .filter(|convoy| convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal()))
        .collect::<Vec<_>>();
    assert_eq!(live.len(), 1);
    assert_eq!(
        backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("ensure").status.unwrap().convoy_ref,
        Some(live[0].metadata.name.clone())
    );
}

#[tokio::test]
async fn reconcile_now_waits_for_periodic_backing_inspection_and_keeps_one_generation() {
    struct PausedBacking {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    #[async_trait]
    impl StandingConvoyBackingInspector for PausedBacking {
        async fn verify_backing_dead(&self, _convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String> {
            self.entered.notify_one();
            self.release.notified().await;
            Err("backing is still live".to_string())
        }
    }

    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    fail_ensured_generation(&backend, &clock).await;
    let backing = Arc::new(PausedBacking { entered: Default::default(), release: Default::default() });
    let periodic = tokio::spawn({
        let daemon = Arc::clone(&daemon);
        let backing = Arc::clone(&backing);
        async move { daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &*backing).await }
    });
    backing.entered.notified().await;
    let mut forced = Box::pin(daemon.reconcile_convoy_ensure_now("flotilla", "quartermaster", &RecordlessBacking));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut forced).await.is_err(),
        "explicit reconciliation must wait for the periodic transaction"
    );
    backing.release.notify_one();
    periodic.await.expect("periodic task").expect("record backing hold");
    forced.await.expect("forced recovery");
    daemon.reconcile_convoy_ensure_now("flotilla", "quartermaster", &RecordlessBacking).await.expect("idempotent forced pass");
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("subsequent periodic pass");

    let convoys = backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations");
    assert_eq!(convoys.items.len(), 2, "one failed generation and one replacement");
    let live =
        convoys.items.iter().filter(|convoy| convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal())).collect::<Vec<_>>();
    assert_eq!(live.len(), 1);
    let status = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("ensure").status.expect("status");
    assert_eq!(status.convoy_ref.as_deref(), Some(live[0].metadata.name.as_str()));
    assert_eq!(status.last_failure, None, "a stale periodic pass must not overwrite the forced recovery");
    assert_eq!(status.retry_at, None);
}

#[tokio::test]
async fn generation_allocation_sees_live_and_terminal_replicated_convoys() {
    let (daemon, source, clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let writer = backend.replica_writer::<ResourceConvoy>(NodeId::new("other-root"), "flotilla");
    writer.replace(&source.using::<ResourceConvoy>("flotilla").list().await.expect("source convoys"), Utc::now()).await.expect("replicate");
    let error = allocate_convoy_generation(&backend, "flotilla", Some("standing-project"), "quartermaster")
        .await
        .expect_err("remote live generation blocks admission");
    assert!(error.contains("generation 1 already exists"), "{error}");
    assert!(error.contains("as of root other-root, last synced"), "{error}");
    assert!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("local reconciliation view").items.is_empty());

    fail_ensured_generation(&source, &clock).await;
    writer
        .replace(&source.using::<ResourceConvoy>("flotilla").list().await.expect("source history"), Utc::now())
        .await
        .expect("replicate terminal history");
    assert_eq!(
        allocate_convoy_generation(&backend, "flotilla", Some("standing-project"), "quartermaster").await.expect("next generation"),
        2
    );
}

#[tokio::test]
async fn ensure_dependency_fingerprint_tracks_replicated_placement_changes() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    let mut ensure = backend.definitions::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("ensure");
    ensure.spec.placement_policy = Some("remote-policy".to_string());
    let absent = daemon.ensure_admission_dependency_hash("flotilla", &ensure).await.expect("absent fingerprint");
    let source = ResourceBackend::InMemory(InMemoryBackend::default());
    let mut policy = placement_policy(&source, "remote-policy", "other-host").await;
    let writer = backend.replica_writer::<PlacementPolicy>(NodeId::new("other-root"), "flotilla");
    writer
        .replace(&source.using::<PlacementPolicy>("flotilla").list().await.expect("policies"), Utc::now())
        .await
        .expect("replicate policy");
    let present = daemon.ensure_admission_dependency_hash("flotilla", &ensure).await.expect("replica fingerprint");
    assert_ne!(absent, present, "arrival must invalidate an admission refusal");
    policy.spec.priority = 42;
    source
        .using::<PlacementPolicy>("flotilla")
        .update(&InputMeta::from(&policy.metadata), &policy.metadata.resource_version, &policy.spec)
        .await
        .expect("change policy");
    writer
        .replace(&source.using::<PlacementPolicy>("flotilla").list().await.expect("updated policies"), Utc::now())
        .await
        .expect("replicate change");
    assert_ne!(present, daemon.ensure_admission_dependency_hash("flotilla", &ensure).await.expect("changed fingerprint"));
}

#[tokio::test]
async fn replicated_ensure_is_not_reconciled_away_from_its_project_home() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    let project = backend.using::<Project>("flotilla").get("standing-project").await.expect("local project");
    let ensure = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("local ensure");

    backend.using::<ConvoyEnsure>("flotilla").delete("quartermaster").await.expect("remove local ensure");
    backend.using::<Project>("flotilla").delete("standing-project").await.expect("remove local project");

    let remote_root = NodeId::new("remote-root");
    let origin = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(remote_root.clone());
    origin.using::<Project>("flotilla").create(&InputMeta::from(&project.metadata), &project.spec).await.expect("remote project");
    origin.using::<ConvoyEnsure>("flotilla").create(&InputMeta::from(&ensure.metadata), &ensure.spec).await.expect("remote ensure");
    backend
        .replica_writer::<Project>(remote_root.clone(), "flotilla")
        .replace(&origin.using::<Project>("flotilla").list().await.expect("remote projects"), Utc::now())
        .await
        .expect("replicate project");
    backend
        .replica_writer::<ConvoyEnsure>(remote_root, "flotilla")
        .replace(&origin.using::<ConvoyEnsure>("flotilla").list().await.expect("remote ensures"), Utc::now())
        .await
        .expect("replicate ensure");

    assert!(backend.definitions::<Project>("flotilla").get("standing-project").await.is_ok(), "replicated project should be visible");
    assert!(backend.definitions::<ConvoyEnsure>("flotilla").get("quartermaster").await.is_ok(), "replicated ensure should be visible");
    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("skip remote ensure").is_empty());
    assert!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("local convoys").items.is_empty());
}

async fn set_ensure_driver(backend: &ResourceBackend, driver_ref: &str) {
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let ensure = ensures.get("quartermaster").await.expect("ensure");
    let mut spec = ensure.spec;
    spec.driver_ref = Some(driver_ref.to_string());
    ensures.update(&InputMeta::from(&ensure.metadata), &ensure.metadata.resource_version, &spec).await.expect("set ensure driver");
}

#[tokio::test]
async fn driver_reconcile_now_acknowledges_recordless_teardown_and_readmits_the_ensure() {
    let (driver, backend, clock, _temp) = standing_ensure_fixture_for("udder", true).await;
    let driver_id = driver.local_host_id().expect("driver host identity").to_string();
    set_ensure_driver(&backend, &driver_id).await;
    let ensure = backend.definitions::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("driver ensure");
    driver.reconcile_driver_convoy_ensure("flotilla", &ensure, &RecordlessBacking, false).await.expect("initial driver admission");
    fail_latest_ensured_generation(&backend, &clock).await;

    let refusal = driver
        .reconcile_driver_convoy_ensure("flotilla", &ensure, &RecordlessBacking, false)
        .await
        .expect_err("automatic driver reconcile must hold on missing backing evidence");
    assert!(refusal.contains("no backing environment evidence is available"), "unexpected refusal: {refusal}");

    let outcome = driver
        .reconcile_convoy_ensure_now("flotilla", "quartermaster", &RecordlessBacking)
        .await
        .expect("operator acknowledges the driver generation record wipe");

    assert_eq!(outcome, "started quartermaster@standing-project");
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("standing generations").items.len(), 2);
}

#[tokio::test]
async fn declared_driver_derives_bounded_backoff_from_its_homed_generations() {
    let (authority, authority_backend, _authority_clock, _authority_temp) = standing_ensure_fixture_for("kiwi", true).await;
    let (driver, driver_backend, driver_clock, _driver_temp) = standing_ensure_fixture_for("udder", false).await;
    let (other, other_backend, _other_clock, _other_temp) = standing_ensure_fixture_for("feta", false).await;
    let driver_id = driver.local_host_id().expect("driver host identity").to_string();
    set_ensure_driver(&authority_backend, &driver_id).await;

    let authority_root = authority.node_id().clone();
    for backend in [&driver_backend, &other_backend] {
        backend
            .replica_writer::<Project>(authority_root.clone(), "flotilla")
            .replace(&authority_backend.using::<Project>("flotilla").list().await.expect("authority projects"), Utc::now())
            .await
            .expect("replicate projects");
        backend
            .replica_writer::<ConvoyEnsure>(authority_root.clone(), "flotilla")
            .replace(&authority_backend.using::<ConvoyEnsure>("flotilla").list().await.expect("authority ensures"), Utc::now())
            .await
            .expect("replicate ensures");
    }

    let host_origin = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(driver.node_id().clone());
    let hosts = host_origin.using::<ResourceHost>("flotilla");
    let host = hosts
        .create(&test_meta(&driver_id), &HostSpec {
            display_name: "udder".to_string(),
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("driver host");
    hosts
        .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus { ready: true, ..Default::default() })
        .await
        .expect("ready driver host");
    let host_snapshot = hosts.list().await.expect("driver host snapshot");
    for backend in [&authority_backend, &driver_backend, &other_backend] {
        backend
            .replica_writer::<ResourceHost>(driver.node_id().clone(), "flotilla")
            .replace(&host_snapshot, Utc::now())
            .await
            .expect("replicate driver host");
    }

    assert!(authority.reconcile_convoy_ensures_once("flotilla").await.expect("authority skip").is_empty());
    assert_eq!(driver.reconcile_convoy_ensures_once("flotilla").await.expect("driver admission").len(), 1);
    assert!(other.reconcile_convoy_ensures_once("flotilla").await.expect("non-driver skip").is_empty());
    assert!(driver.reconcile_convoy_ensures_once("flotilla").await.expect("steady-state driver pass").is_empty());
    assert_eq!(driver_backend.using::<ResourceConvoy>("flotilla").list().await.expect("driver convoys").items.len(), 1);
    assert!(authority_backend.using::<ResourceConvoy>("flotilla").list().await.expect("authority convoys").items.is_empty());
    assert!(other_backend.using::<ResourceConvoy>("flotilla").list().await.expect("other convoys").items.is_empty());
    assert!(driver_backend.using::<ConvoyEnsure>("flotilla").list().await.expect("driver local ensures").items.is_empty());

    let reaped = fail_latest_ensured_generation(&driver_backend, &driver_clock).await;
    assert!(driver
        .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
        .await
        .expect("failed generation starts backoff")
        .is_empty());
    driver_backend.using::<ResourceConvoy>("flotilla").delete(&reaped).await.expect("operator reaps failed husk");
    assert_eq!(
        driver
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("reaping resets derived failure count")
            .len(),
        1
    );

    for delay in [30, 60] {
        fail_latest_ensured_generation(&driver_backend, &driver_clock).await;
        assert!(driver
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("backoff pass")
            .is_empty());
        driver_clock.advance(ChronoDuration::seconds(delay - 1));
        assert!(driver
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("retry not yet due")
            .is_empty());
        driver_clock.advance(ChronoDuration::seconds(1));
        assert_eq!(
            driver
                .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
                .await
                .expect("admit replacement")
                .len(),
            1
        );
    }

    fail_latest_ensured_generation(&driver_backend, &driver_clock).await;
    assert_eq!(
        driver
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("escalate bounded failures"),
        vec!["ConvoyEnsure/quartermaster exhausted restart budget"]
    );
    let demands = driver_backend.using::<ResourceDemand>("flotilla");
    let demand = demands.get("ensure-attention-quartermaster").await.expect("driver-homed escalation");
    assert!(demand.spec.expiry.is_some());
    driver_clock.advance(ChronoDuration::hours(1));
    assert!(driver
        .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
        .await
        .expect("unresolved escalation blocks admission")
        .is_empty());
    assert_eq!(driver_backend.using::<ResourceConvoy>("flotilla").list().await.expect("bounded generations").items.len(), 3);

    let ensure = authority_backend.definitions::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("authority ensure");
    assert_eq!(
        driver
            .reconcile_driver_convoy_ensure("flotilla", &ensure, &VerifiedDeadBacking, true)
            .await
            .expect("forced reconcile bypasses active driver escalation"),
        Some("started quartermaster@standing-project".to_string())
    );
    assert!(matches!(demands.get("ensure-attention-quartermaster").await, Err(ResourceError::NotFound { .. })));
    assert_eq!(driver_backend.using::<ResourceConvoy>("flotilla").list().await.expect("resumed generations").items.len(), 4);
    assert!(
        authority_backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("authority ensure").status.is_none(),
        "driver admission must not persist control state on its ensure definition"
    );
}

#[tokio::test]
async fn unavailable_declared_driver_surfaces_named_admission_conditions_without_fallback() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    set_ensure_driver(&backend, "missing-driver").await;
    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("unknown driver skip").is_empty());
    let ensure = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("conditioned ensure");
    let condition = ensure
        .status
        .as_ref()
        .expect("ensure status")
        .conditions
        .iter()
        .find(|condition| condition.condition_type == DRIVER_ADMISSION_CONDITION_TYPE)
        .expect("driver admission condition");
    assert_eq!(condition.reason, "UnknownDriver");
    assert!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("convoys").items.is_empty());

    let hosts = backend.using::<ResourceHost>("flotilla");
    hosts
        .create(&test_meta("missing-driver"), &HostSpec {
            display_name: "missing-driver".to_string(),
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("known but unreachable driver");
    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("unreachable driver skip").is_empty());
    let ensure = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("conditioned ensure");
    let condition = ensure
        .status
        .expect("ensure status")
        .conditions
        .into_iter()
        .find(|condition| condition.condition_type == DRIVER_ADMISSION_CONDITION_TYPE)
        .expect("driver admission condition");
    assert_eq!(condition.reason, "DriverUnreachable");
}

#[tokio::test]
async fn declared_driver_admission_refusals_retry_indefinitely_without_strikes_or_human_gate() {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    let driver_id = daemon.local_host_id().expect("driver host identity").to_string();
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts
        .create(&test_meta(&driver_id), &HostSpec {
            display_name: "local".to_string(),
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("driver host");
    hosts
        .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus { ready: true, ..Default::default() })
        .await
        .expect("ready driver host");
    set_ensure_driver(&backend, &driver_id).await;
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let ensure = ensures.get("quartermaster").await.expect("ensure");
    let mut spec = ensure.spec.clone();
    spec.workflow_ref = "missing-workflow".to_string();
    ensures.update(&InputMeta::from(&ensure.metadata), &ensure.metadata.resource_version, &spec).await.expect("make admission fail");
    let ensure = ensures.get("quartermaster").await.expect("updated ensure");
    ensures
        .update_status(&ensure.metadata.name, &ensure.metadata.resource_version, &ConvoyEnsureStatus {
            convoy_ref: Some("stale-convoy".to_string()),
            running_since: Some(clock.now() - ChronoDuration::days(1)),
            ..Default::default()
        })
        .await
        .expect("seed stale pre-driver status");

    let first = daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("first admission refusal");
    assert!(first.contains("retry at"), "{first}");
    let retrying_ensure = ensures.get("quartermaster").await.expect("ensure");
    let retry_resource_version = retrying_ensure.metadata.resource_version.clone();
    let status = retrying_ensure.status.expect("driver-managed status");
    assert_eq!(status.convoy_ref, None);
    assert_eq!(status.running_since, None);
    assert_eq!(status.restart_count, 0);
    assert!(status.retry_at.is_some());

    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("backoff suppresses retry").is_empty());
    assert_eq!(
        ensures.get("quartermaster").await.expect("unchanged ensure").metadata.resource_version,
        retry_resource_version,
        "a driver retry deadline must not cause a no-op legacy-status write"
    );
    clock.advance(ChronoDuration::seconds(30));
    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("second admission refusal").contains("retry at"));
    for expected_delay in [120, 120, 120] {
        clock.advance(ChronoDuration::seconds(expected_delay));
        assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("admission keeps retrying").contains("retry at"));
        let status = ensures.get("quartermaster").await.expect("ensure").status.expect("retry status");
        assert_eq!(status.restart_count, 0);
        assert_eq!(status.retry_at.expect("deadline") - clock.now(), ChronoDuration::seconds(120));
    }
    assert!(matches!(
        backend.using::<ResourceDemand>("flotilla").get("ensure-attention-quartermaster").await,
        Err(ResourceError::NotFound { .. })
    ));

    let ensure = ensures.get("quartermaster").await.expect("failed ensure");
    let mut recovered_spec = ensure.spec.clone();
    recovered_spec.workflow_ref = "quartermaster".to_string();
    ensures.update(&InputMeta::from(&ensure.metadata), &ensure.metadata.resource_version, &recovered_spec).await.expect("repair admission");
    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("workflow dependency change resumes admission").len(), 1);
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("recovered convoy").items.len(), 1);
}

#[tokio::test]
async fn resolved_default_branch_dependency_change_retries_admission_before_deadline() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    let projects = backend.definitions::<Project>("flotilla");
    let project = projects.get("standing-project").await.expect("project");
    let mut project_spec = project.spec.clone();
    project_spec.repositories[0].default_branch = None;
    projects.apply(&InputMeta::from(&project.metadata), &project_spec).await.expect("require discovered default branch");

    let refusal = daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("unresolved default branch refuses admission");
    assert!(refusal.contains("no resolved default branch"), "{refusal}");
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let status = ensures.get("quartermaster").await.expect("ensure").status.expect("retry status");
    assert_eq!(status.restart_count, 0);
    assert!(status.retry_at.is_some());

    let repository =
        backend.using::<Repository>("flotilla").list().await.expect("repositories").items.into_iter().next().expect("repository");
    let source = ResourceBackend::InMemory(InMemoryBackend::default());
    let source_repositories = source.using::<Repository>("flotilla");
    let source_repository =
        source_repositories.create(&test_meta(&repository.metadata.name), &repository.spec).await.expect("replica repository source");
    source_repositories
        .update_status(
            &source_repository.metadata.name,
            &source_repository.metadata.resource_version,
            &flotilla_resources::RepositoryStatus { default_branch: Some("main".to_string()), ..Default::default() },
        )
        .await
        .expect("resolve default branch on another root");
    backend
        .replica_writer::<Repository>(NodeId::new("readiness-root"), "flotilla")
        .replace(&source_repositories.list().await.expect("repository source snapshot"), Utc::now())
        .await
        .expect("replicate resolved default branch");

    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("dependency change bypasses deadline"), vec![
        "started quartermaster@standing-project"
    ]);
}

#[tokio::test]
async fn orphaned_ensure_reports_its_absent_parent_project() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    backend.definitions::<Project>("flotilla").delete("standing-project").await.expect("remove parent project");

    let error = daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("orphaned ensure must remain visible");
    assert_eq!(error, "ConvoyEnsure/quartermaster: parent Project/standing-project is absent");
    assert!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("local convoys").items.is_empty());
}

#[tokio::test]
async fn statusless_ensured_generation_is_live_even_when_address_labels_are_missing() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");

    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let ensure = ensures.get("quartermaster").await.expect("ensure");
    let convoy_ref = ensure.status.as_ref().and_then(|status| status.convoy_ref.clone()).expect("admitted generation");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys.get(&convoy_ref).await.expect("statusless generation");
    convoys
        .update(
            &InputMeta::builder().name(convoy.metadata.name.clone()).annotations(convoy.metadata.annotations.clone()).build(),
            &convoy.metadata.resource_version,
            &convoy.spec,
        )
        .await
        .expect("simulate generation whose address labels have not materialized");
    ensures
        .update_status(&ensure.metadata.name, &ensure.metadata.resource_version, &ConvoyEnsureStatus {
            observed_config_hash: ensure.status.and_then(|status| status.observed_config_hash),
            ..Default::default()
        })
        .await
        .expect("simulate lost ensure status update");

    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("rediscover admitted generation"), vec![
        "ConvoyEnsure/quartermaster observed running"
    ]);
    assert_eq!(convoys.list().await.expect("convoys").items.len(), 1);
    assert_eq!(ensures.get("quartermaster").await.expect("ensure").status.and_then(|status| status.convoy_ref), Some(convoy_ref));
}

#[tokio::test]
async fn foreign_statusless_generation_at_ensure_address_blocks_admission_without_labels() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    backend
        .using::<ResourceConvoy>("flotilla")
        .create(
            &test_meta("foreign-generation"),
            &ConvoySpec::builder()
                .workflow_ref("quartermaster".to_string())
                .role("quartermaster".to_string())
                .generation(1)
                .project_ref("standing-project".to_string())
                .build(),
        )
        .await
        .expect("foreign statusless generation");

    let error = daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("foreign live address must block ensure admission");
    assert!(error.contains("live convoy quartermaster@standing-project already exists outside this ensure"), "{error}");
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("convoys").items.len(), 1);
}

#[tokio::test]
async fn changing_ensure_config_starts_a_fresh_retry_episode() {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    for delay in [30, 60] {
        fail_ensured_generation(&backend, &clock).await;
        daemon
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("record failed generation");
        clock.advance(ChronoDuration::seconds(delay));
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("admit replacement");
    }
    fail_ensured_generation(&backend, &clock).await;
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("exhaust retry budget");

    let definitions = backend.definitions::<ConvoyEnsure>("flotilla");
    let ensure = definitions.get("quartermaster").await.expect("ensure definition");
    let mut changed_spec = ensure.spec.clone();
    changed_spec.presents_as = Some("updated-fleet".to_string());
    definitions.apply(&InputMeta::from(&ensure.metadata), &changed_spec).await.expect("change ensure config");

    daemon
        .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
        .await
        .expect("config change opens a fresh episode");
    let status = definitions.get("quartermaster").await.expect("ensure").status.expect("status");
    assert_eq!(status.restart_count, 1);
    assert_eq!(status.hold_reason, None);
    assert!(backend.using::<ResourceDemand>("flotilla").list().await.expect("demands").items.is_empty());
    clock.advance(ChronoDuration::seconds(30));
    daemon
        .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
        .await
        .expect("admit after config change backoff");
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 4);
}

#[tokio::test]
async fn standing_ensure_admission_uses_default_branch_observed_only_on_non_driver_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"root-b\"\n").expect("daemon config");
    let target = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("root-b"),
        target.clone(),
    )
    .await;
    let driver_ref = daemon.canonical_local_host_id().expect("root B host identity").to_string();
    target
        .using::<ResourceHost>("flotilla")
        .create(&test_meta(&driver_ref), &HostSpec {
            display_name: "root-b".to_string(),
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("root B host resource");
    let source = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    let repository_spec = RepositorySpec::remote("https://github.com/acme/cross-root").expect("repository spec");
    let repository_key = repository_spec.key();
    let source_repository = source
        .using::<Repository>("flotilla")
        .create(&test_meta(&repository_key.to_string()), &repository_spec)
        .await
        .expect("source repository");
    source
        .using::<Repository>("flotilla")
        .update_status(&source_repository.metadata.name, &source_repository.metadata.resource_version, &RepositoryStatus {
            default_branch: Some("main".to_string()),
            ..Default::default()
        })
        .await
        .expect("source default branch observation");
    target
        .using::<Repository>("flotilla")
        .create(&test_meta(&repository_key.to_string()), &repository_spec)
        .await
        .expect("driver repository without a local status observation");
    source
        .definitions::<Project>("flotilla")
        .create(
            &test_meta("cross-root-project"),
            &ProjectSpec::builder()
                .display_name("Cross Root Project".to_string())
                .default_workflow_ref("cross-root-workflow".to_string())
                .repositories(vec![ProjectRepositorySpec {
                    repo: repository_key.clone(),
                    alias: None,
                    roles: BTreeSet::from([ProjectRepositoryRole::Code]),
                    subpath: None,
                    default_branch: None,
                }])
                .build(),
        )
        .await
        .expect("source project");
    target
        .using::<WorkflowTemplate>("flotilla")
        .create(
            &test_meta("cross-root-workflow"),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .repository_refs(vec![repository_key.clone()])
                    .crew(Vec::new())
                    .build()])
                .build(),
        )
        .await
        .expect("driver-local workflow");
    source
        .definitions::<ConvoyEnsure>("flotilla")
        .create(
            &InputMeta::builder()
                .name("cross-root".to_string())
                .annotations(BTreeMap::from([
                    (MATERIALIZED_PROJECT_ANNOTATION.to_string(), "cross-root-project".to_string()),
                    (SOURCE_REPOSITORY_ANNOTATION.to_string(), repository_key.to_string()),
                    (SOURCE_COMMIT_ANNOTATION.to_string(), "abc123".to_string()),
                    (SOURCE_ENTRY_PATH_ANNOTATION.to_string(), "ops/cross-root.md".to_string()),
                ]))
                .build(),
            &ConvoyEnsureSpec {
                project_ref: "cross-root-project".to_string(),
                role: "quartermaster".to_string(),
                driver_ref: Some(driver_ref),
                workflow_ref: "cross-root-workflow".to_string(),
                placement_policy: None,
                escalation_reason: None,
                repositories: vec![repository_key.clone()],
                presents_as: None,
                agent_overrides: Vec::new(),
            },
        )
        .await
        .expect("source ensure");

    let origin = NodeId::new("root-a");
    target
        .replica_writer::<Repository>(origin.clone(), "flotilla")
        .replace(&source.using::<Repository>("flotilla").list().await.expect("source repositories"), Utc::now())
        .await
        .expect("replicate repositories");
    target
        .replica_writer::<Project>(origin.clone(), "flotilla")
        .replace(&source.using::<Project>("flotilla").list().await.expect("source projects"), Utc::now())
        .await
        .expect("replicate projects");
    target
        .replica_writer::<ConvoyEnsure>(origin, "flotilla")
        .replace(&source.using::<ConvoyEnsure>("flotilla").list().await.expect("source ensures"), Utc::now())
        .await
        .expect("replicate ensures");

    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("cross-root ensure admission"), vec![
        "started quartermaster@cross-root-project"
    ]);
    let convoy = target
        .using::<ResourceConvoy>("flotilla")
        .list()
        .await
        .expect("list admitted convoys")
        .items
        .into_iter()
        .next()
        .expect("admitted convoy on root B");
    assert_eq!(convoy.spec.project_ref.as_deref(), Some("cross-root-project"));

    let conflicting_source = ResourceBackend::InMemory(InMemoryBackend::default());
    let conflicting_repository = conflicting_source
        .using::<Repository>("flotilla")
        .create(&test_meta(&repository_key.to_string()), &repository_spec)
        .await
        .expect("conflicting source repository");
    conflicting_source
        .using::<Repository>("flotilla")
        .update_status(&conflicting_repository.metadata.name, &conflicting_repository.metadata.resource_version, &RepositoryStatus {
            default_branch: Some("trunk".to_string()),
            ..Default::default()
        })
        .await
        .expect("conflicting default branch observation");
    target
        .replica_writer::<Repository>(NodeId::new("root-c"), "flotilla")
        .replace(&conflicting_source.using::<Repository>("flotilla").list().await.expect("conflicting repositories"), Utc::now())
        .await
        .expect("replicate conflicting repository status");

    let error = daemon
        .snapshot_project_repositories("flotilla", "cross-root-project", None)
        .await
        .expect_err("different non-driver observations must fail admission closed");
    assert!(error.contains("conflicting observed default branches"), "unexpected readiness error: {error}");
}

#[tokio::test]
async fn standing_backing_inspection_holds_empty_evidence_after_provisioning_started() {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoy_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .expect("ensure status")
        .convoy_ref
        .expect("convoy ref");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys.get(&convoy_ref).await.expect("standing convoy");
    let convoy = convoys
        .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Failed,
            provisioning: Some(ConvoyProvisioningState::Started { started_at: clock.now() }),
            finished_at: Some(clock.now()),
            ..Default::default()
        })
        .await
        .expect("record post-provisioning failure");

    let refusal = daemon
        .verify_standing_convoy_resource_backing_dead(&convoy)
        .await
        .expect_err("missing backing evidence after provisioning must remain conservative");

    assert_eq!(refusal, "no backing environment evidence is available");

    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("hold without backing evidence"), vec![
        "ConvoyEnsure/quartermaster held for operator attention"
    ]);
    let events = backend.using::<Event>("flotilla").list().await.expect("list object events").items;
    assert!(events.iter().any(|event| {
        event.spec.regarding.name == convoy_ref
            && event.spec.reason == "BackingEvidenceRefused"
            && event.spec.message.contains("no backing environment evidence is available")
    }));
}

#[tokio::test]
async fn duplicate_operational_entry_refusal_records_a_project_event() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;

    daemon
        .project_service()
        .record_project_operational_refusal("flotilla", "standing-project", "duplicate materialized WorkflowTemplate `quartermaster`")
        .await;

    let events = backend.using::<Event>("flotilla").list().await.expect("events").items;
    assert!(events.iter().any(|event| {
        event.spec.regarding.name == "standing-project"
            && event.spec.reason == "DuplicateOperationalEntryRefused"
            && event.spec.message == "duplicate materialized WorkflowTemplate `quartermaster`"
    }));
}

#[tokio::test]
async fn standing_ensure_retries_convoy_that_failed_before_provisioning() {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .expect("ensure status")
        .convoy_ref
        .expect("convoy ref");
    let convoy = convoys.get(&convoy_ref).await.expect("standing convoy");
    convoys
        .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Failed,
            provisioning: Some(ConvoyProvisioningState::NotStarted),
            message: Some("workflow validation failed".to_string()),
            finished_at: Some(clock.now()),
            ..Default::default()
        })
        .await
        .expect("record pre-provisioning failure");

    let events = daemon.reconcile_convoy_ensures_once("flotilla").await.expect("reconcile terminal convoy");

    assert!(events.iter().any(|event| event.contains("backing off")), "unexpected events: {events:?}");
    let ensure = backend.definitions::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("ensure");
    assert!(ensure.status.expect("ensure status").retry_at.is_some());
    assert!(backend.using::<ResourceDemand>("flotilla").list().await.expect("demands").items.is_empty());
}

#[tokio::test]
async fn standing_ensure_holds_failed_convoy_while_backing_is_live_then_restarts_after_verified_death() {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let first_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .expect("ensure status")
        .convoy_ref
        .expect("convoy ref");
    let first = convoys.get(&first_ref).await.expect("standing convoy");
    let now = clock.now();
    convoys
        .update_status(&first.metadata.name, &first.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Failed,
            message: Some("provider registry unavailable".to_string()),
            started_at: Some(now),
            finished_at: Some(now),
            observed_workflow_ref: Some("quartermaster".to_string()),
            ..Default::default()
        })
        .await
        .expect("fail convoy after resolution loss");
    let environments = backend.using::<ResourceEnvironment>("flotilla");
    let environment = environments
        .create(
            &InputMeta::builder()
                .name("quartermaster-work".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), first_ref.clone())]))
                .build(),
            &ResourceEnvironmentSpec {
                host_direct: None,
                docker: Some(flotilla_resources::DockerEnvironmentSpec {
                    host_ref: "local".to_string(),
                    image: "standing:latest".to_string(),
                    declared_agent_adapters: BTreeSet::new(),
                    required_agent_adapters: BTreeSet::new(),
                    pull_policy: Default::default(),
                    mounts: Vec::new(),
                    env: BTreeMap::new(),
                }),
            },
        )
        .await
        .expect("backing environment");
    environments
        .update_status(&environment.metadata.name, &environment.metadata.resource_version, &ResourceEnvironmentStatus {
            phase: EnvironmentPhase::Ready,
            ready: true,
            docker_container_id: Some("live-container".to_string()),
            ..Default::default()
        })
        .await
        .expect("mark backing live");

    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("hold live backing"), vec![
        "ConvoyEnsure/quartermaster held for operator attention"
    ]);
    clock.advance(ChronoDuration::hours(1));
    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("continue holding").is_empty());
    assert!(convoys.get(&first_ref).await.is_ok(), "failed convoy and its live container must survive");
    let held = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("held ensure");
    assert_eq!(held.status.as_ref().expect("status").restart_count, 0);
    assert!(
        held.status.as_ref().expect("status").last_failure.as_deref().is_some_and(|failure| failure.contains("not verified dead")),
        "unexpected ensure status: {:?}",
        held.status
    );
    assert_eq!(backend.using::<ResourceDemand>("flotilla").list().await.expect("attention demands").items.len(), 1);

    let environment = environments.get("quartermaster-work").await.expect("backing environment");
    environments
        .update_status(&environment.metadata.name, &environment.metadata.resource_version, &ResourceEnvironmentStatus {
            phase: EnvironmentPhase::Failed,
            ready: false,
            message: Some("Docker container live-container is not running".to_string()),
            ..Default::default()
        })
        .await
        .expect("verify backing dead");
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("record crash backoff");
    clock.advance(ChronoDuration::seconds(30));
    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("restart dead backing"), vec![
        "started quartermaster@standing-project"
    ]);
    let generations = convoys.list().await.expect("standing generations").items;
    assert_eq!(generations.len(), 2);
    assert!(generations.iter().any(|convoy| convoy.metadata.name == first_ref && convoy.spec.generation == 1));
    assert!(generations.iter().any(|convoy| convoy.spec.generation == 2));
    assert_eq!(backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("ensure").status.unwrap().restart_count, 1);
}

#[tokio::test]
async fn convoy_teardown_removes_its_managed_presentations() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoy_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .and_then(|status| status.convoy_ref)
        .expect("convoy ref");
    let presentations = backend.using::<ResourcePresentation>("flotilla");
    presentations
        .create(
            &InputMeta::builder()
                .name("quartermaster-work".to_string())
                .labels(BTreeMap::from([
                    (AUTHORITY_LABEL.to_string(), LifecycleAuthority::Managed.as_label_value().to_string()),
                    (CONVOY_LABEL.to_string(), convoy_ref.clone()),
                ]))
                .build(),
            &flotilla_resources::PresentationSpec {
                convoy_ref: convoy_ref.clone(),
                presentation_policy_ref: "default".to_string(),
                name: "quartermaster".to_string(),
                process_selector: BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_ref.clone())]),
            },
        )
        .await
        .expect("presentation");

    daemon.reap_convoy_internal("flotilla", &convoy_ref, true).await.expect("convoy teardown");

    assert!(matches!(presentations.get("quartermaster-work").await, Err(ResourceError::NotFound { .. })));
    assert!(matches!(backend.using::<ResourceConvoy>("flotilla").get(&convoy_ref).await, Err(ResourceError::NotFound { .. })));
}

#[tokio::test]
async fn forced_convoy_delete_retains_force_intent_until_checkout_finalizes() {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoy_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .and_then(|status| status.convoy_ref)
        .expect("convoy ref");
    backend
        .clone()
        .using::<ResourceCheckout>("flotilla")
        .create(
            &InputMeta::builder()
                .name("checkout-at-risk".to_string())
                .labels(BTreeMap::from([
                    (AUTHORITY_LABEL.to_string(), LifecycleAuthority::Managed.as_label_value().to_string()),
                    (CONVOY_LABEL.to_string(), convoy_ref.clone()),
                ]))
                .finalizers(vec!["flotilla.work/checkout-cleanup".to_string()])
                .build(),
            &ResourceCheckoutSpec::Observed(flotilla_resources::ObservedCheckoutSpec {
                r#ref: "feature/work".to_string(),
                path: "/tmp/checkout-at-risk".to_string(),
                repo_ref: RepositoryKey("repo-a".to_string()),
                host_ref: "host-test".to_string(),
                is_main: false,
            }),
        )
        .await
        .expect("checkout");

    daemon.reap_convoy_internal("flotilla", &convoy_ref, true).await.expect("force delete");

    let convoy = backend.using::<ResourceConvoy>("flotilla").get(&convoy_ref).await.expect("convoy must await checkout");
    assert_eq!(convoy.metadata.annotations.get(flotilla_resources::FORCE_TEARDOWN_ANNOTATION).map(String::as_str), Some("true"));
    assert!(convoy.metadata.deletion_timestamp.is_some());
}

#[tokio::test]
async fn abandoned_ensure_generation_survives_a_stale_reconcile_write_and_is_superseded() {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let first_ref =
        ensures.get("quartermaster").await.expect("ensure").status.and_then(|status| status.convoy_ref).expect("first convoy ref");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let first = convoys.get(&first_ref).await.expect("first generation");
    let workflow_snapshot_ref =
        first.metadata.annotations.get(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION).cloned().expect("workflow archive pointer");

    let principal = PrincipalRef::implicit_for_namespace("flotilla");
    daemon
        .abandon_convoy_internal("flotilla", &first_ref, "operator requested replacement", Some(&principal))
        .await
        .expect("abandon generation");

    // This patch represents a reconcile that read the generation while it was
    // still active and lost the optimistic write race to the abandon command.
    // Retrying it against the newer status must not resurrect the generation.
    apply_resource_status_patch(&convoys, &first_ref, &controller_patches::roll_up_phase(ConvoyPhase::Active, Some(clock.now()), None))
        .await
        .expect("stale reconcile write");

    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("observe abandoned generation");
    clock.advance(ChronoDuration::seconds(30));
    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("supersede abandoned generation"), vec![
        "started quartermaster@standing-project"
    ]);

    let generations = convoys.list().await.expect("standing generations").items;
    assert_eq!(generations.len(), 2);
    let abandoned = generations.iter().find(|convoy| convoy.metadata.name == first_ref).expect("abandoned history");
    let abandoned_status = abandoned.status.as_ref().expect("abandoned status");
    assert_eq!(abandoned.spec.generation, 1);
    assert_eq!(abandoned_status.phase, ConvoyPhase::Abandoned);
    assert_eq!(abandoned_status.message.as_deref(), Some("abandoned by human override: operator requested replacement"));
    assert_eq!(abandoned.metadata.annotations.get(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION), Some(&workflow_snapshot_ref));
    assert!(generations.iter().any(|convoy| convoy.spec.generation == 2 && convoy.metadata.name != first_ref));
}

#[tokio::test]
async fn standing_ensure_does_not_capture_another_projects_bare_workflow_but_accepts_a_global_builtin() {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture().await;
    let own_name = crate::ops_entry::materialized_workflow_name("standing-project", "quartermaster");
    let own = backend.definitions::<WorkflowTemplate>("flotilla").get(&own_name).await.expect("own workflow");
    backend.definitions::<WorkflowTemplate>("flotilla").delete(&own_name).await.expect("remove own workflow");
    backend
        .definitions::<WorkflowTemplate>("flotilla")
        .apply(
            &InputMeta::builder()
                .name("quartermaster".to_string())
                .annotations(BTreeMap::from([(MATERIALIZED_PROJECT_ANNOTATION.to_string(), "other-project".to_string())]))
                .build(),
            &own.spec,
        )
        .await
        .expect("other project's workflow");

    let error = daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("cross-project bare name must not resolve");
    assert!(error.contains("workflow template quartermaster is materialized by another project"), "unexpected error: {error}");

    backend
        .definitions::<WorkflowTemplate>("flotilla")
        .apply(&test_meta("quartermaster"), &own.spec)
        .await
        .expect("global builtin workflow");
    clock.advance(ChronoDuration::minutes(1));
    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("global workflow admits"), vec![
        "started quartermaster@standing-project"
    ]);
}

#[tokio::test]
async fn off_home_driver_admits_an_ensure_from_replicated_project_definitions() {
    let (_home, home_backend, _clock, _home_temp) = standing_ensure_fixture().await;
    let driver_temp = tempfile::tempdir().expect("driver tempdir");
    std::fs::write(driver_temp.path().join("daemon.toml"), "machine_id = \"driver-test\"\n").expect("driver config");
    let driver_backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("driver-root"));
    let home_root = NodeId::new("home-root");
    driver_backend
        .replica_writer::<Project>(home_root.clone(), "flotilla")
        .replace(&home_backend.using::<Project>("flotilla").list().await.expect("home projects"), Utc::now())
        .await
        .expect("replicate projects");
    driver_backend
        .replica_writer::<WorkflowTemplate>(home_root.clone(), "flotilla")
        .replace(&home_backend.using::<WorkflowTemplate>("flotilla").list().await.expect("home workflows"), Utc::now())
        .await
        .expect("replicate workflows");
    for repository in home_backend.using::<Repository>("flotilla").list().await.expect("home repositories").items {
        driver_backend
            .using::<Repository>("flotilla")
            .create(&InputMeta::from(&repository.metadata), &repository.spec)
            .await
            .expect("driver repository observation");
    }
    let driver = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(driver_temp.path())),
        fake_discovery(false),
        HostName::new("driver"),
        driver_backend.clone(),
    )
    .await;
    let ensure = home_backend.definitions::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("home ensure");

    driver.start_ensured_convoy("flotilla", &ensure).await.expect("driver admits replicated template");

    let admitted = driver_backend.using::<ResourceConvoy>("flotilla").list().await.expect("driver convoys").items;
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0].metadata.labels.get(PROJECT_LABEL).map(String::as_str), Some("standing-project"));
}

#[tokio::test]
async fn operator_reap_restarts_immediately_without_burning_budget_and_past_due_retry_survives_restart() {
    let (daemon, backend, clock, temp) = standing_ensure_fixture().await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let ensure = ensures.get("quartermaster").await.expect("ensure");
    let first_ref = ensure.status.as_ref().and_then(|status| status.convoy_ref.clone()).expect("first convoy ref");
    ensures
        .update_status(&ensure.metadata.name, &ensure.metadata.resource_version, &ConvoyEnsureStatus {
            convoy_ref: Some(first_ref.clone()),
            restart_count: 7,
            running_since: Some(clock.now()),
            retry_at: None,
            last_failure: None,
            hold_reason: None,
            observed_config_hash: None,
            conditions: Vec::new(),
            retry: None,
            stalled: None,
        })
        .await
        .expect("seed crash budget");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    convoys.delete(&first_ref).await.expect("operator reap");

    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("prompt resurrection"), vec![
        "started quartermaster@standing-project"
    ]);
    assert_eq!(ensures.get("quartermaster").await.expect("ensure").status.unwrap().restart_count, 7);

    let materialized_name = crate::ops_entry::materialized_workflow_name("standing-project", "quartermaster");
    backend.definitions::<WorkflowTemplate>("flotilla").delete(&materialized_name).await.expect("temporary resolution loss");
    let second_ref =
        ensures.get("quartermaster").await.expect("ensure").status.and_then(|status| status.convoy_ref).expect("second convoy ref");
    convoys.delete(&second_ref).await.expect("second operator reap");
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("unresolved workflow schedules retry");
    let retrying = ensures.get("quartermaster").await.expect("retrying ensure");
    assert_eq!(retrying.status.as_ref().expect("status").restart_count, 7);
    let retry_at = retrying.status.as_ref().expect("status").retry_at.expect("durable retry time");

    let repository_key = backend.using::<Repository>("flotilla").list().await.expect("repositories").items[0].spec.key();
    backend
        .definitions::<WorkflowTemplate>("flotilla")
        .apply(
            &InputMeta::builder()
                .name(materialized_name)
                .annotations(BTreeMap::from([(MATERIALIZED_PROJECT_ANNOTATION.to_string(), "standing-project".to_string())]))
                .build(),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .repository_refs(vec![repository_key])
                    .crew(Vec::new())
                    .build()])
                .build(),
        )
        .await
        .expect("restore workflow resolution");
    let restarted_daemon = InProcessDaemon::new_with_resource_backend_and_clock(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::local(),
        backend.clone(),
        clock.clone(),
    )
    .await;
    assert!(restarted_daemon.reconcile_convoy_ensures_once("flotilla").await.expect("retry not due").is_empty());
    clock.set(retry_at);
    assert_eq!(restarted_daemon.reconcile_convoy_ensures_once("flotilla").await.expect("past-due retry"), vec![
        "started quartermaster@standing-project"
    ]);
    assert_eq!(ensures.get("quartermaster").await.expect("ensure").status.unwrap().restart_count, 7);
}

#[tokio::test]
async fn capability_admission_resolves_display_name_kind_and_policy_host_refs() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"canonical-admission-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("udder"),
        backend.clone(),
    )
    .await;
    let host_id = daemon.local_host_id().expect("host id").to_string();
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts
        .create(&test_meta(&host_id), &HostSpec { display_name: "udder".into(), connection: Default::default(), ..HostSpec::default() })
        .await
        .expect("host");
    hosts
        .update_status(&host_id, &host.metadata.resource_version, &HostStatus {
            heartbeat_at: Some(Utc::now()),
            ready: true,
            fulfilment_facts: BTreeMap::from([("udder-kind".into(), FulfilmentFacts {
                gui_session_logged_in: true,
                observed_at: Utc::now(),
                ..Default::default()
            })]),
            ..Default::default()
        })
        .await
        .expect("host facts");
    for name in ["collision-a", "collision-b"] {
        hosts
            .create(&test_meta(name), &HostSpec { display_name: "collision".into(), connection: Default::default(), ..HostSpec::default() })
            .await
            .expect("ambiguous host");
    }
    placement_policy(&backend, "a-collision", "collision").await;
    backend
        .clone()
        .using::<FulfilmentKind>("flotilla")
        .create(
            &test_meta("a-collision"),
            &FulfilmentKindSpec::builder()
                .host_ref("collision".to_string())
                .pool("passthrough".to_string())
                .grants(BTreeSet::from([FulfilmentGrant::GuiSession]))
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
        )
        .await
        .expect("ambiguous kind");
    placement_policy(&backend, "udder-kind", "udder").await;
    backend
        .clone()
        .using::<FulfilmentKind>("flotilla")
        .create(
            &test_meta("udder-kind"),
            &FulfilmentKindSpec::builder()
                .host_ref("udder".to_string())
                .pool("passthrough".to_string())
                .grants(BTreeSet::from([FulfilmentGrant::GuiSession]))
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
        )
        .await
        .expect("legacy kind");

    let (placement, _) = daemon
        .resolve_capability_placement(
            "flotilla",
            "project",
            &[],
            &WorkflowTemplateSpec::builder().vessels(Vec::new()).build(),
            &BTreeSet::from([CapabilityNeed::GuiSession]),
            &flotilla_protocol::ConvoyStartIntent::builder().project_ref("project".to_string()).build(),
        )
        .await
        .expect("display-name kind should admit");
    let allocation = placement.allocation.expect("allocation");
    assert_eq!(allocation.candidates[0].host, host_id);
    assert!(allocation.candidates[0].host_ready);
}

#[tokio::test]
async fn fulfilment_list_joins_host_facts_and_fleet_health_shows_local_kinds() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"fulfilment-join\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        backend.clone(),
    )
    .await;
    let host_id = daemon.local_host_id().expect("local host ID").to_string();
    let kinds = backend.clone().using::<FulfilmentKind>("flotilla");
    for (name, host_ref) in [("local-kind", host_id.as_str()), ("unmatched-kind", "absent-host")] {
        kinds
            .create(
                &test_meta(name),
                &FulfilmentKindSpec::builder()
                    .host_ref(host_ref.to_string())
                    .pool("cleat".to_string())
                    .grants(BTreeSet::from([FulfilmentGrant::Platform("linux".to_string())]))
                    .realisation(FulfilmentRealisation::HostDirect)
                    .build(),
            )
            .await
            .expect("create kind");
    }
    let before = daemon.fulfilment_list_internal().await.expect("list before heartbeat");
    assert_eq!(before.kinds.len(), 2);
    assert!(before.kinds.iter().all(|kind| kind.harnesses.is_empty() && kind.gui_session_logged_in.is_none()));

    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts
        .create(&test_meta(&host_id), &HostSpec {
            display_name: "local-host".to_string(),
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("create host");
    hosts
        .update_status(&host_id, &host.metadata.resource_version, &HostStatus {
            heartbeat_at: Some(Utc::now()),
            ready: true,
            fulfilment_facts: BTreeMap::from([("local-kind".to_string(), FulfilmentFacts {
                harnesses: BTreeMap::from([("claude-code".to_string(), HarnessFacts {
                    version: "2.1.282".to_string(),
                    ..Default::default()
                })]),
                gui_session_logged_in: true,
                ..Default::default()
            })]),
            ..Default::default()
        })
        .await
        .expect("publish facts");
    let listed = daemon.fulfilment_list_internal().await.expect("list facts");
    assert_eq!(listed.kinds.iter().find(|kind| kind.name == "local-kind").expect("local kind").harnesses["claude-code"].version, "2.1.282");
    assert!(listed.kinds.iter().find(|kind| kind.name == "unmatched-kind").expect("unmatched kind").harnesses.is_empty());
    let fleet = daemon.fleet_health_internal().await.expect("fleet health");
    let local = fleet.hosts.iter().find(|host| host.host == HostName::new("local-host")).expect("local host row");
    assert_eq!(local.fulfilments.len(), 1);
    assert_eq!(local.fulfilments[0].name, "local-kind");
    assert_eq!(local.fulfilments[0].gui_session_logged_in, Some(true));
}

#[tokio::test]
async fn self_targeted_admission_uses_live_local_host_over_stale_self_origin_replica() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"local-host\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        backend.clone(),
    )
    .await;
    let host_id = daemon.local_host_id().expect("local host identity").to_string();

    let stale_source = ResourceBackend::InMemory(InMemoryBackend::default());
    stale_source
        .using::<ResourceHost>("flotilla")
        .create(&test_meta(&host_id), &HostSpec {
            display_name: "local-host".to_string(),
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("stale self-origin host");
    backend
        .replica_writer::<ResourceHost>(daemon.node_id.clone(), "flotilla")
        .replace(&stale_source.using::<ResourceHost>("flotilla").list().await.expect("stale host list"), Utc::now())
        .await
        .expect("seed stale self-origin replica");

    let hosts = backend.using::<ResourceHost>("flotilla");
    let local = hosts
        .create(&test_meta(&host_id), &HostSpec {
            display_name: "local-host".to_string(),
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("authoritative local host");
    hosts
        .update_status(&host_id, &local.metadata.resource_version, &HostStatus {
            disk_free_bytes: Some(100 * 1024 * 1024 * 1024),
            admission_free_space_floor_bytes: Some(20 * 1024 * 1024 * 1024),
            ..HostStatus::default()
        })
        .await
        .expect("publish live local capacity");
    backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("self-targeted"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: host_id.clone(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
        )
        .await
        .expect("self-targeted placement policy");

    daemon
        .check_remote_placement_free_space_floor(
            "flotilla",
            Some(&PlacementDecision {
                minimal_alternatives: Vec::new(),
                escalation_reason: None,
                policy_name: "self-targeted".to_string(),
                target_host: PlacementTargetHost {
                    reference: flotilla_protocol::CanonicalHostId::resolved(host_id),
                    display_name: "local-host".to_string(),
                },
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
                allocation: None,
            }),
        )
        .await
        .expect("healthy authoritative local capacity should admit self-targeted placement");
}

#[tokio::test]
async fn resource_host_routing_refuses_unresolved_host_ref() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"local-host\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        ResourceBackend::InMemory(InMemoryBackend::default()),
    )
    .await;

    let error = daemon
        .target_host_for_resource_ref("flotilla", "unregistered-host-id")
        .await
        .expect_err("unknown host refs must not cross the canonical identity boundary");

    assert_eq!(error, "references unknown host `unregistered-host-id`");
}

#[tokio::test]
async fn self_targeted_admission_resolves_display_name_policy_to_live_local_host() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"local-host\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local-host"),
        backend.clone(),
    )
    .await;
    let host_id = daemon.local_host_id().expect("local host identity").to_string();
    let hosts = backend.using::<ResourceHost>("flotilla");
    let local = hosts
        .create(&test_meta(&host_id), &HostSpec {
            display_name: "local-host".to_string(),
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("authoritative local host");
    hosts
        .update_status(&host_id, &local.metadata.resource_version, &HostStatus {
            disk_free_bytes: Some(100 * 1024 * 1024 * 1024),
            admission_free_space_floor_bytes: Some(20 * 1024 * 1024 * 1024),
            ..HostStatus::default()
        })
        .await
        .expect("publish live local capacity");
    let policy = backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("self-targeted"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: "local-host".to_string(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
        )
        .await
        .expect("self-targeted placement policy");

    let target = placement_target_host(&backend, "flotilla", &policy).await.expect("resolve display-name host reference");
    assert_eq!(target.reference.as_str(), host_id);
    assert_eq!(daemon.remote_placement_host("flotilla", Some("self-targeted")).await.expect("resolve host-direct routing"), None);
    daemon
        .check_remote_placement_free_space_floor(
            "flotilla",
            Some(&PlacementDecision {
                minimal_alternatives: Vec::new(),
                escalation_reason: None,
                policy_name: "self-targeted".to_string(),
                target_host: target,
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
                allocation: None,
            }),
        )
        .await
        .expect("healthy authoritative local capacity should admit self-targeted placement");
}

#[tokio::test]
async fn default_remote_placement_resolves_replicated_credentials_before_admission() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"local-host\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("kiwi"),
        backend.clone(),
    )
    .await;
    backend
        .definitions::<Project>("flotilla")
        .create(
            &test_meta("andamento"),
            &ProjectSpec::builder().display_name("Andamento".to_string()).default_workflow_ref("govern".to_string()).build(),
        )
        .await
        .expect("project");
    backend
        .definitions::<CredentialSpec>("flotilla")
        .create(&test_meta("claude-max"), &CredentialSpecSpec {
            consumer: CredentialConsumer::ClaudeOauth { account_email: "governor@example.com".to_string() },
            source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
            lifecycle: CredentialLifecycle::Static,
            placement: CredentialPlacementRequirements::default(),
        })
        .await
        .expect("credential declaration");
    backend
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("andamento-governor"),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["andamento".to_string()])).build())
                .credentials(BTreeSet::from(["claude-max".to_string()]))
                .build(),
        )
        .await
        .expect("credential grant");
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(
            &test_meta("govern"),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .crew(vec![CrewSpec::builder()
                        .role("governor".to_string())
                        .source(CrewSource::Agent {
                            selector: Selector { capability: "code".to_string(), adapter: Some("claude-code".to_string()), model: None },
                            prompt: None,
                            brief_template: None,
                        })
                        .build()])
                    .build()])
                .build(),
        )
        .await
        .expect("workflow");
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts
        .create(&test_meta("udder-id"), &HostSpec {
            display_name: "udder".to_string(),
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("remote host");
    hosts
        .update_status("udder-id", &host.metadata.resource_version, &HostStatus {
            capabilities: [
                (AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(["claude-code"])),
                (flotilla_resources::HELD_CREDENTIALS_CAPABILITY.to_string(), serde_json::json!(["claude-max"])),
                ("docker".to_string(), serde_json::json!(true)),
                ("os".to_string(), serde_json::json!("linux")),
            ]
            .into_iter()
            .collect(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            ..HostStatus::default()
        })
        .await
        .expect("remote host capabilities");
    let placement_source = ResourceBackend::InMemory(InMemoryBackend::default());
    placement_source
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("docker-udder-id"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .docker_per_vessel(flotilla_resources::DockerPerVesselPlacementPolicySpec {
                    host_ref: "udder-id".to_string(),
                    image: "crew:latest".to_string().into(),
                    pull_policy: Default::default(),
                    agent_adapters: BTreeSet::from(["claude-code".to_string()]),
                    default_cwd: None,
                    env: BTreeMap::new(),
                    checkout: flotilla_resources::DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
                })
                .build(),
        )
        .await
        .expect("remote placement");
    backend
        .replica_writer::<PlacementPolicy>(NodeId::new("udder-root"), "flotilla")
        .replace(&placement_source.using::<PlacementPolicy>("flotilla").list().await.expect("list remote placement policies"), Utc::now())
        .await
        .expect("replicate remote placement policy");

    let intent = flotilla_protocol::ConvoyStartIntent::builder().project_ref("andamento".to_string()).build();
    let (_, mut resolved_workflow) = daemon
        .resolve_convoy_admission_workflow(
            "flotilla",
            "andamento",
            &backend.definitions::<Project>("flotilla").get("andamento").await.expect("project").spec,
            &[],
            &intent,
        )
        .await
        .expect("resolve admission workflow");
    let placement =
        backend.including_replicas::<PlacementPolicy>("flotilla").get("docker-udder-id").await.expect("placement replica").object;
    resolve_workflow_credentials(&backend, "flotilla", Some("andamento"), &[], &mut resolved_workflow)
        .await
        .expect("resolve replicated credential grant");
    assert_eq!(resolved_workflow.vessels[0].credential_refs, BTreeSet::from(["claude-max".to_string()]));
    validate_workflow_agent_adapters(&backend, "flotilla", &resolved_workflow, Some(&placement), false)
        .await
        .expect("placement should provide agent adapter");
    validate_workflow_credentials(&backend, "flotilla", &resolved_workflow, Some(&placement))
        .await
        .expect("placement should hold resolved credential");

    assert!(matches!(backend.using::<ResourceConvoy>("flotilla").list().await, Ok(list) if list.items.is_empty()));
}

#[tokio::test]
async fn placement_candidates_and_refusals_agree_across_roots() {
    let kiwi = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("kiwi-root"));
    let feta = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("feta-root"));
    placement_policy(&kiwi, "kiwi-policy", "kiwi").await;
    placement_policy(&feta, "feta-policy", "feta").await;
    let synced_at = Utc::now();
    for (destination, source, origin) in [(&kiwi, &feta, "feta-root"), (&feta, &kiwi, "kiwi-root")] {
        destination
            .replica_writer::<PlacementPolicy>(NodeId::new(origin), "flotilla")
            .replace(&source.using::<PlacementPolicy>("flotilla").list().await.expect("local policies"), synced_at)
            .await
            .expect("replicate policies");
    }
    let workflow = WorkflowTemplateSpec::builder().vessels(Vec::new()).build();
    let left = default_convoy_placement_policy(&kiwi, "flotilla", None, &[], &workflow, None).await.expect("kiwi candidates");
    let right = default_convoy_placement_policy(&feta, "flotilla", None, &[], &workflow, None).await.expect("feta candidates");
    assert_eq!(left.refused_candidates, right.refused_candidates);
    assert_eq!(left.refused_candidates.iter().map(|candidate| candidate.policy_name.as_str()).collect::<Vec<_>>(), vec![
        "feta-policy",
        "kiwi-policy"
    ]);
    for backend in [&kiwi, &feta] {
        assert_eq!(backend.using::<PlacementPolicy>("flotilla").list().await.expect("controller view").items.len(), 1);
    }
    let replica = kiwi.including_replicas::<PlacementPolicy>("flotilla").get("feta-policy").await.expect("replica provenance");
    assert_eq!(replica.provenance, ResourceProvenance::Replica { origin_root: NodeId::new("feta-root"), last_synced_at: synced_at });
}

#[tokio::test]
async fn placement_decision_prefers_local_home_copy_over_same_name_replica() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let hosts = backend.using::<ResourceHost>("flotilla");
    for host in ["local-host", "replica-host"] {
        hosts
            .create(&test_meta(host), &HostSpec { display_name: host.to_string(), connection: Default::default(), ..HostSpec::default() })
            .await
            .expect("placement host");
    }

    backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("shared-policy"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .priority(0)
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: "local-host".to_string(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
        )
        .await
        .expect("local home policy");
    let replica_source = ResourceBackend::InMemory(InMemoryBackend::default());
    replica_source
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("shared-policy"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .priority(100)
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: "replica-host".to_string(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
        )
        .await
        .expect("replica policy source");
    backend
        .replica_writer::<PlacementPolicy>(NodeId::new("remote-root"), "flotilla")
        .replace(&replica_source.using::<PlacementPolicy>("flotilla").list().await.expect("list replica policy"), Utc::now())
        .await
        .expect("replicate colliding policy");

    let resolution = default_convoy_placement_policy(
        &backend,
        "flotilla",
        None,
        &[],
        &WorkflowTemplateSpec::builder().vessels(Vec::new()).build(),
        None,
    )
    .await
    .expect("resolve placement");
    let selected = resolution.selected.expect("select local home policy");

    assert_eq!(placement_host_ref(&selected), Some("local-host"));
    assert!(resolution.viable_not_selected.is_empty(), "same-name replica must not remain as a second candidate");
}

async fn placement_policy(backend: &ResourceBackend, name: &str, host_ref: &str) -> ResourceObject<PlacementPolicy> {
    backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta(name),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .host_direct(HostDirectPlacementPolicySpec {
                    host_ref: host_ref.to_string(),
                    checkout: HostDirectPlacementPolicyCheckout::Worktree,
                })
                .build(),
        )
        .await
        .expect("placement policy")
}

#[tokio::test]
async fn placement_target_host_rejects_unknown_display_name() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let policy = placement_policy(&backend, "unknown-host", "missing-host").await;
    let error = placement_target_host(&backend, "flotilla", &policy).await.expect_err("unknown host alias must be rejected");
    assert_eq!(error, "placement `unknown-host` references unknown host `missing-host`");
}

#[tokio::test]
async fn placement_target_host_rejects_ambiguous_display_name() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let hosts = backend.using::<ResourceHost>("flotilla");
    for host_id in ["host-id-a", "host-id-b"] {
        hosts
            .create(&test_meta(host_id), &HostSpec {
                display_name: "shared-name".to_string(),
                connection: Default::default(),
                ..HostSpec::default()
            })
            .await
            .expect("host");
    }
    let policy = placement_policy(&backend, "ambiguous-host", "shared-name").await;
    let error = placement_target_host(&backend, "flotilla", &policy).await.expect_err("ambiguous host alias must be rejected");
    assert_eq!(error, "placement `ambiguous-host` host reference `shared-name` is ambiguous");
}

async fn create_host_direct_placement(backend: &ResourceBackend, policy_name: &str, host_ref: &str, agent_adapters: BTreeSet<String>) {
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts
        .create(&test_meta(host_ref), &HostSpec {
            display_name: host_ref.to_string(),
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("host create");
    hosts
        .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus {
            capabilities: [(AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(agent_adapters))].into_iter().collect(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            ..HostStatus::default()
        })
        .await
        .expect("host status update");
    placement_policy(backend, policy_name, host_ref).await;
}

#[tokio::test]
async fn agentless_ssh_host_is_selected_for_trusted_work_and_routes_to_its_owner() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts
        .create(&test_meta("ssh-host"), &HostSpec {
            display_name: "beaufort".to_string(),
            connection: flotilla_resources::HostConnection::AgentlessSsh {
                owning_daemon: "owner-host".to_string(),
                destination: "crew@beaufort.example".to_string(),
            },
            ..HostSpec::default()
        })
        .await
        .expect("SSH Host");
    hosts
        .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus {
            capabilities: BTreeMap::from([
                (AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!([])),
                ("agentless".to_string(), serde_json::json!(true)),
                ("transport".to_string(), serde_json::json!("ssh")),
                ("placement".to_string(), serde_json::json!("host_direct_only")),
                ("owning_daemon".to_string(), serde_json::json!("owner-host")),
            ]),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            ..HostStatus::default()
        })
        .await
        .expect("SSH observation");
    let policy = placement_policy(&backend, "host-direct-ssh-host", "ssh-host").await;
    let trusted = WorkflowTemplateSpec::builder()
        .vessels(vec![flotilla_resources::VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![flotilla_resources::CrewSpec::builder()
                .role("shell".to_string())
                .source(flotilla_resources::CrewSource::Tool { command: "sh".to_string() })
                .build()])
            .build()])
        .build();
    let accepted = default_convoy_placement_policy(&backend, "flotilla", None, &[], &trusted, None).await.expect("placement scoring");
    assert_eq!(accepted.selected.expect("SSH host selected").metadata.name, policy.metadata.name);
    let target = placement_target_host(&backend, "flotilla", &policy).await.expect("target host");
    assert_eq!(placement_actuator_host_ref(&backend, "flotilla", &target).await.expect("owner").as_str(), "owner-host");

    backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("docker-ssh-host"),
            &PlacementPolicySpec::builder()
                .pool("cleat".to_string())
                .docker_per_vessel(flotilla_resources::DockerPerVesselPlacementPolicySpec {
                    host_ref: "ssh-host".to_string(),
                    image: "crew:latest".to_string().into(),
                    pull_policy: Default::default(),
                    agent_adapters: BTreeSet::from(["codex".to_string()]),
                    default_cwd: None,
                    env: BTreeMap::new(),
                    checkout: flotilla_resources::DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
                })
                .build(),
        )
        .await
        .expect("synthetic Docker policy for SSH host");

    let agent_workflow = flotilla_resources::single_agent_workflow_spec();
    let refused = default_convoy_placement_policy(&backend, "flotilla", None, &[], &agent_workflow, None)
        .await
        .expect_err("agentless SSH host cannot run a local agent adapter");
    assert!(refused.contains("agent adapter `codex`"), "{refused}");

    backend.using::<PlacementPolicy>("flotilla").delete("docker-ssh-host").await.expect("remove synthetic Docker policy");

    let observed = hosts.get("ssh-host").await.expect("SSH Host");
    let mut unreachable = observed.status.expect("observed SSH status");
    unreachable.ready = false;
    hosts.update_status("ssh-host", &observed.metadata.resource_version, &unreachable).await.expect("failed SSH probe");
    let unavailable =
        default_convoy_placement_policy(&backend, "flotilla", None, &[], &trusted, None).await.expect("score unreachable host");
    assert!(unavailable.selected.is_none());
    assert!(unavailable.refused_candidates.iter().any(|candidate| candidate.reason.contains("not ready")));
}

fn trusted_codex_workflow() -> WorkflowTemplateSpec {
    flotilla_resources::single_agent_workflow_spec()
}

#[tokio::test]
async fn default_placement_prefers_local_host_referenced_by_display_name() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    create_host_direct_placement(&backend, "host-direct-a-remote", "remote-host", BTreeSet::from(["codex".to_string()])).await;
    let hosts = backend.using::<ResourceHost>("flotilla");
    let local = hosts
        .create(&test_meta("local-host-id"), &HostSpec {
            display_name: "local-host".to_string(),
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("local host");
    hosts
        .update_status(&local.metadata.name, &local.metadata.resource_version, &HostStatus {
            capabilities: [(AGENT_ADAPTERS_CAPABILITY.to_string(), serde_json::json!(["codex"]))].into_iter().collect(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            ..HostStatus::default()
        })
        .await
        .expect("local host status");
    placement_policy(&backend, "host-direct-z-local", "local-host").await;

    let local_host_id = flotilla_protocol::CanonicalHostId::resolved("local-host-id");
    let resolution = default_convoy_placement_policy(&backend, "flotilla", None, &[], &trusted_codex_workflow(), Some(&local_host_id))
        .await
        .expect("default placement");
    assert_eq!(resolution.selected.expect("viable placement").metadata.name, "host-direct-z-local");
    assert_eq!(resolution.viable_not_selected[0].reason, "fallback ordering preferred local policy `host-direct-z-local`");
}

#[tokio::test]
async fn default_placement_refuses_unknown_host_without_blocking_tool_workflow() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    placement_policy(&backend, "a-unknown-host", "deleted-host").await;
    create_host_direct_placement(&backend, "z-clean", "clean-host", BTreeSet::new()).await;
    let workflow = flotilla_resources::WorkflowTemplateSpec::builder()
        .vessels(vec![flotilla_resources::VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![flotilla_resources::CrewSpec::builder()
                .role("watcher".to_string())
                .source(flotilla_resources::CrewSource::Tool { command: "tail -f log".to_string() })
                .build()])
            .build()])
        .build();

    let resolution = default_convoy_placement_policy(&backend, "flotilla", None, &[], &workflow, None).await.expect("clean candidate");
    assert_eq!(resolution.selected.expect("clean placement").metadata.name, "z-clean");
    assert_eq!(resolution.refused_candidates[0].policy_name, "a-unknown-host");
}

#[tokio::test]
async fn default_placement_error_lists_each_refusal_and_failed_host_condition_reason() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    for (policy_name, host_name, condition_reason, condition_message) in [
        ("host-direct-feta", "feta", "StoredObjectDecodeFailed", "ConvoyEnsure/quarantined-record failed typed decode"),
        ("host-direct-udder", "udder", "RestartBudgetExhausted", "resource controller stopped after repeated failures"),
    ] {
        create_host_direct_placement(&backend, policy_name, host_name, BTreeSet::from(["codex".to_string()])).await;
        let hosts = backend.using::<ResourceHost>("flotilla");
        let host = hosts.get(host_name).await.expect("host");
        hosts
            .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus {
                capabilities: host.status.expect("host status").capabilities,
                daemon_generation: Some(format!("{host_name}-generation")),
                heartbeat_at: Some(Utc::now()),
                ready: false,
                conditions: vec![HostCondition::builder()
                    .condition_type("test")
                    .value(ConditionValue::False)
                    .reason(condition_reason)
                    .message(condition_message)
                    .observed_at(Utc::now())
                    .build()],
                ..HostStatus::default()
            })
            .await
            .expect("degraded host status");
    }

    let error = default_convoy_placement_policy(&backend, "flotilla", None, &[], &trusted_codex_workflow(), None)
        .await
        .expect_err("all placement candidates should be refused");

    assert_eq!(
        error,
        "no placement policy satisfies adapter `codex`; candidates:\n\
- `host-direct-feta`: placement `host-direct-feta` host `feta` generation `feta-generation` is not ready: \
StoredObjectDecodeFailed: ConvoyEnsure/quarantined-record failed typed decode\n\
- `host-direct-udder`: placement `host-direct-udder` host `udder` generation `udder-generation` is not ready: \
RestartBudgetExhausted: resource controller stopped after repeated failures"
    );
}

#[tokio::test]
async fn default_placement_accepts_a_host_with_an_authorship_collision() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    create_host_direct_placement(&backend, "host-direct-feta", "feta", BTreeSet::from(["codex".to_string()])).await;
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts.get("feta").await.expect("host");
    hosts
        .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus {
            capabilities: host.status.expect("host status").capabilities,
            heartbeat_at: Some(Utc::now()),
            ready: true,
            conditions: vec![HostCondition::builder()
                .condition_type("ResourceReplication/AuthorshipCollision")
                .value(ConditionValue::False)
                .reason("HomeBoundRecordAuthoredAtMultipleRoots")
                .message("Convoy/flotilla/standing-collision is authored at multiple roots")
                .observed_at(Utc::now())
                .blocks_readiness(false)
                .build()],
            ..HostStatus::default()
        })
        .await
        .expect("host status with advisory collision");

    let resolution = default_convoy_placement_policy(&backend, "flotilla", None, &[], &trusted_codex_workflow(), None)
        .await
        .expect("standing authorship collisions must not freeze dispatch placement");

    assert_eq!(resolution.selected.expect("viable placement").metadata.name, "host-direct-feta");
}

pub(super) async fn create_test_environment(daemon: &InProcessDaemon, name: &str, host_ref: &str) -> String {
    daemon
        .resource_backend()
        .using::<ResourceEnvironment>("flotilla")
        .create(&test_meta(name), &ResourceEnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: host_ref.to_string(), repo_default_dir: "/tmp".to_string() }),
            docker: None,
        })
        .await
        .expect("environment");
    name.to_string()
}

pub(super) async fn create_running_session(daemon: &InProcessDaemon, env_ref: &str, name: &str, convoy: &str, role: &str) {
    let terminals = daemon.resource_backend().using::<ResourceTerminalSession>("flotilla");
    let created = terminals
        .create(
            &InputMeta::builder()
                .name(name.to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), convoy.to_string()),
                    (VESSEL_LABEL.to_string(), "work".to_string()),
                    (VESSEL_REF_LABEL.to_string(), format!("{convoy}-work")),
                    (ROLE_LABEL.to_string(), role.to_string()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: env_ref.to_string(),
                role: role.to_string(),
                source: TerminalSessionSource::Tool { command: "bash".to_string() },
                cwd: "/repo".to_string(),
                pool: "passthrough".to_string(),
            },
        )
        .await
        .expect("terminal session");
    terminals
        .update_status(name, &created.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            session_id: Some(format!("session-{name}")),
            ..Default::default()
        })
        .await
        .expect("running session");
}

#[tokio::test]
async fn fleet_list_falls_back_per_row_for_an_ambiguous_host_alias() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        ResourceBackend::InMemory(InMemoryBackend::default()),
    )
    .await;
    let local_host = daemon.local_host_id().expect("local host id").to_string();
    let local_env = create_test_environment(&daemon, "local-env", &local_host).await;
    let ambiguous_env = create_test_environment(&daemon, "ambiguous-env", "shared-host").await;
    let hosts = daemon.resource_backend().using::<ResourceHost>("flotilla");
    for host_id in ["shared-host-id-a", "shared-host-id-b"] {
        hosts
            .create(&test_meta(host_id), &HostSpec {
                display_name: "shared-host".to_string(),
                connection: Default::default(),
                ..HostSpec::default()
            })
            .await
            .expect("ambiguous host");
    }
    create_running_session(&daemon, &ambiguous_env, "terminal-ambiguous", "convoy-ambiguous", "watcher").await;
    create_running_session(&daemon, &local_env, "terminal-local", "convoy-local", "watcher").await;

    let rows = daemon.fleet_list_internal().await.expect("fleet list").rows;
    let hosts_by_convoy = rows.into_iter().map(|row| (row.convoy, row.host)).collect::<BTreeMap<_, _>>();
    assert_eq!(hosts_by_convoy.get("convoy-ambiguous"), Some(&HostName::new("shared-host")));
    assert_eq!(hosts_by_convoy.get("convoy-local"), Some(&daemon.host_name));
}

#[tokio::test]
async fn fleet_list_scopes_rows_to_the_live_convoy_project() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        ResourceBackend::InMemory(InMemoryBackend::default()),
    )
    .await;
    let host = daemon.local_host_id().expect("local host id").to_string();
    let env = create_test_environment(&daemon, "local-env", &host).await;
    for (convoy, project) in [("convoy-one", "island-one"), ("convoy-two", "island-two")] {
        create_identity_convoy(&daemon.resource_backend(), convoy, convoy, Some(project)).await;
        if convoy == "convoy-one" {
            let terminals = daemon.resource_backend().using::<ResourceTerminalSession>("flotilla");
            let name = "terminal-convoy-one";
            let created = terminals
                .create(
                    &InputMeta::builder()
                        .name(name.to_string())
                        .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy.to_string())]))
                        .build(),
                    &ResourceTerminalSessionSpec {
                        env_ref: env.clone(),
                        role: "coder".to_string(),
                        source: TerminalSessionSource::Agent {
                            selector: Selector { capability: "code".to_string(), adapter: Some("codex".to_string()), model: None },
                            brief: flotilla_resources::TerminalBrief {
                                artifact_digest: None,
                                path: "brief.md".to_string(),
                                content: "Work".to_string(),
                                copies: vec![],
                            },
                            context: Box::new(flotilla_resources::TerminalCrewContext {
                                namespace: "flotilla".to_string(),
                                convoy: convoy.to_string(),
                                vessel_ref: "vessel-one".to_string(),
                            }),
                            message: None,
                        },
                        cwd: "/repo".to_string(),
                        pool: "passthrough".to_string(),
                    },
                )
                .await
                .expect("agent session");
            terminals
                .update_status(name, &created.metadata.resource_version, &ResourceTerminalSessionStatus {
                    phase: ResourceTerminalSessionPhase::Running,
                    session_id: Some("session-one".to_string()),
                    crew: Some(flotilla_resources::CrewSessionStatus {
                        id: "crew-one".to_string(),
                        adapter: "codex".to_string(),
                        model: None,
                        stance: "coder".to_string(),
                    }),
                    ..Default::default()
                })
                .await
                .expect("agent status");
        } else {
            create_running_session(&daemon, &env, &format!("terminal-{convoy}"), convoy, "coder").await;
        }
    }

    let fleet = daemon.scoped_fleet_list(None, None, None).await.expect("fleet");
    assert_eq!(fleet.rows.len(), 2);
    let scoped = daemon.scoped_fleet_list(None, None, Some("convoy-one")).await.expect("crew convoy scope");
    assert_eq!(scoped.rows.len(), 1);
    assert_eq!(scoped.rows[0].convoy_ref.as_deref(), Some("convoy-one"));
    let by_crew = daemon.scoped_fleet_list(None, Some("crew-one"), None).await.expect("crew identity scope");
    assert_eq!(by_crew.rows.len(), 1);
    assert_eq!(by_crew.rows[0].convoy_ref.as_deref(), Some("convoy-one"));
    assert!(daemon.scoped_fleet_list(None, Some("missing"), None).await.is_err());
    let explicit = daemon.scoped_fleet_list(Some("island-two"), None, Some("convoy-one")).await.expect("explicit scope");
    assert_eq!(explicit.rows.len(), 1);
    assert_eq!(explicit.rows[0].convoy_ref.as_deref(), Some("convoy-two"));
    let explicit_with_stale_crew = daemon.scoped_fleet_list(Some("island-two"), Some("missing"), None).await.expect("explicit scope wins");
    assert_eq!(explicit_with_stale_crew.rows.len(), 1);
}

async fn create_docker_placement(backend: &ResourceBackend, policy_name: &str, host_ref: &str, held_credentials: BTreeSet<String>) {
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts
        .create(&test_meta(host_ref), &HostSpec {
            display_name: host_ref.to_string(),
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("host create");
    hosts
        .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus {
            capabilities: [
                (flotilla_resources::HELD_CREDENTIALS_CAPABILITY.to_string(), serde_json::json!(held_credentials)),
                ("docker".to_string(), serde_json::json!(true)),
                ("os".to_string(), serde_json::json!("linux")),
            ]
            .into_iter()
            .collect(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            resource_store: None,
            ..HostStatus::default()
        })
        .await
        .expect("host status update");
    backend
        .clone()
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta(policy_name),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .docker_per_vessel(flotilla_resources::DockerPerVesselPlacementPolicySpec {
                    host_ref: host_ref.to_string(),
                    image: "crew:latest".to_string().into(),
                    pull_policy: Default::default(),
                    agent_adapters: BTreeSet::from(["codex".to_string()]),
                    default_cwd: None,
                    env: BTreeMap::new(),
                    checkout: flotilla_resources::DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
                })
                .build(),
        )
        .await
        .expect("placement create");
}

#[tokio::test]
async fn docker_placement_refuses_hosts_missing_runtime_or_linux_before_selection() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    create_docker_placement(&backend, "docker-kiwi", "kiwi", BTreeSet::new()).await;
    let hosts = backend.using::<ResourceHost>("flotilla");
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()])
        .build();

    for (docker, os, missing) in [(false, "macos", "docker capability"), (true, "macos", "Linux host capability")] {
        let host = hosts.get("kiwi").await.expect("host");
        let mut status = host.status.expect("status");
        status.capabilities.insert("docker".to_string(), serde_json::json!(docker));
        status.capabilities.insert("os".to_string(), serde_json::json!(os));
        hosts.update_status("kiwi", &host.metadata.resource_version, &status).await.expect("update capability");

        for policy in [Some("docker-kiwi"), None] {
            let error = daemon
                .resolve_convoy_placement("flotilla", None, &[], &workflow, policy, false)
                .await
                .expect_err("ineligible host must refuse admission");
            assert!(error.contains("host `kiwi`"), "{error}");
            assert!(error.contains(missing), "{error}");
        }
    }
}

#[tokio::test]
async fn grant_resolution_scopes_roles_trust_and_permissions_independently_of_isolation() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    let own = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("own repository");
    let fork = RepositorySpec::remote("https://github.com/example/flotilla")
        .expect("fork repository")
        .with_upstream("https://github.com/flotilla-org/flotilla", flotilla_resources::RepositoryRelation::Fork)
        .expect("upstream");
    for repository in [&own, &fork] {
        backend
            .using::<Repository>("flotilla")
            .create(&test_meta(&repository.key().to_string()), repository)
            .await
            .expect("create repository");
    }
    backend
        .definitions::<CredentialSpec>("flotilla")
        .create(&test_meta("github-app"), &CredentialSpecSpec {
            consumer: CredentialConsumer::GithubApp {
                actor_login: None,
                installation_id: Some(1),
                installation_repository: None,
                permissions: Some(BTreeMap::from([
                    ("contents".to_string(), "write".to_string()),
                    ("actions".to_string(), "read".to_string()),
                ])),
            },
            source: CredentialSource::GithubApp { app_id_path: "app-id".to_string(), private_key_path: "key".to_string() },
            lifecycle: CredentialLifecycle::Refreshable,
            placement: CredentialPlacementRequirements::default(),
        })
        .await
        .expect("declaration");
    for (name, roles, trust, permissions) in [
        (
            "coder-contents",
            BTreeSet::from(["coder".to_string()]),
            Some(RepositoryTrust::Own),
            BTreeMap::from([("contents".to_string(), "write".to_string())]),
        ),
        (
            "coder-actions",
            BTreeSet::from(["coder".to_string()]),
            Some(RepositoryTrust::Own),
            BTreeMap::from([("actions".to_string(), "write".to_string())]),
        ),
        ("reviewer", BTreeSet::from(["reviewer".to_string()]), None, BTreeMap::from([("contents".to_string(), "read".to_string())])),
    ] {
        backend
            .definitions::<CredentialGrant>("flotilla")
            .create(
                &test_meta(name),
                &CredentialGrantSpec::builder()
                    .selector(
                        CredentialGrantSelector::builder()
                            .projects(BTreeSet::from(["flotilla".to_string()]))
                            .roles(roles)
                            .maybe_repository_trust(trust)
                            .build(),
                    )
                    .credentials(BTreeSet::from(["github-app".to_string()]))
                    .permissions(BTreeMap::from([("github-app".to_string(), permissions)]))
                    .build(),
            )
            .await
            .expect("grant");
    }
    let resolve = |role: &str, repository: &RepositorySpec| {
        let role = role.to_string();
        let repository = repository.clone();
        let backend = backend.clone();
        async move {
            let mut workflow = WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .crew(vec![CrewSpec::builder().role(role).source(CrewSource::Tool { command: "true".to_string() }).build()])
                    .build()])
                .build();
            let repositories = [ConvoyRepositorySpec::builder()
                .url("https://github.com/flotilla-org/flotilla".to_string())
                .repo_ref(repository.key())
                .source_ref("main".to_string())
                .target_ref("work".to_string())
                .workspace_slug("flotilla".to_string())
                .subpaths(Vec::new())
                .build()];
            resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &repositories, &mut workflow)
                .await
                .expect("resolve grants");
            workflow.vessels.remove(0)
        }
    };
    let contained = resolve("coder", &own).await;
    let direct = resolve("coder", &own).await;
    assert_eq!(contained.credential_refs, direct.credential_refs);
    assert_eq!(contained.credential_permissions, direct.credential_permissions);
    assert_eq!(
        contained.credential_permissions["github-app"],
        BTreeMap::from([("contents".to_string(), "write".to_string()), ("actions".to_string(), "read".to_string()),])
    );
    let reviewer = resolve("reviewer", &own).await;
    assert_eq!(reviewer.credential_permissions["github-app"], BTreeMap::from([("contents".to_string(), "read".to_string())]));
    let fork_coder = resolve("coder", &fork).await;
    assert!(fork_coder.credential_refs.is_empty());
}

#[tokio::test]
async fn contained_claude_requires_and_accepts_a_project_selected_oauth_grant() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(&test_meta("claude-max"), &CredentialSpecSpec {
            consumer: CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
            source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
            lifecycle: CredentialLifecycle::Static,
            placement: CredentialPlacementRequirements::default(),
        })
        .await
        .expect("create Claude credential declaration");
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: Some("claude-code".to_string()), model: None },
                    prompt: None,
                    brief_template: None,
                })
                .build()])
            .build()])
        .build();

    let mut without_grant = workflow.clone();
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut without_grant)
        .await
        .expect("resolve default-deny grants");
    let error = validate_workflow_credentials(&backend, "flotilla", &without_grant, None)
        .await
        .expect_err("contained Claude must not reach interactive login without OAuth");
    assert_eq!(error, "agent adapter `claude-code` requires credential `claude-max`, but no matching CredentialGrant selected it");

    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("claude-max-contained"),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["flotilla".to_string()])).build())
                .credentials(BTreeSet::from(["claude-max".to_string()]))
                .build(),
        )
        .await
        .expect("create project-selected Claude grant");
    let mut with_grant = workflow;
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut with_grant)
        .await
        .expect("resolve matching Claude grant");
    assert_eq!(with_grant.vessels[0].credential_refs, BTreeSet::from(["claude-max".to_string()]));

    create_docker_placement(&backend, "docker-claude", "host-a", BTreeSet::from(["claude-max".to_string()])).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("docker-claude").await.expect("get placement");
    validate_workflow_credentials(&backend, "flotilla", &with_grant, Some(&placement))
        .await
        .expect("matching held OAuth grant admits contained Claude");
}

#[tokio::test]
async fn docker_placement_selects_credentials_for_the_effective_contained_stance() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(&test_meta("github-crew-pr"), &CredentialSpecSpec {
            consumer: CredentialConsumer::Gh,
            source: CredentialSource::Env { name: "GITHUB_TOKEN".to_string() },
            lifecycle: CredentialLifecycle::Static,
            placement: CredentialPlacementRequirements::default(),
        })
        .await
        .expect("create GitHub credential declaration");
    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("github-contained"),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["flotilla".to_string()])).build())
                .credentials(BTreeSet::from(["github-crew-pr".to_string()]))
                .build(),
        )
        .await
        .expect("create contained GitHub grant");
    create_docker_placement(&backend, "docker-crew", "host-a", BTreeSet::from(["github-crew-pr".to_string()])).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("docker-crew").await.expect("get Docker placement");
    let mut workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()])
        .build();

    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut workflow)
        .await
        .expect("resolve credentials against effective stance");

    assert_eq!(workflow.vessels[0].credential_refs, BTreeSet::from(["github-crew-pr".to_string()]));
    validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect("contained grant held by the placement admits dispatch");
}

#[tokio::test]
async fn project_grant_entitlement_is_independent_of_vessel_stance() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(&test_meta("github-crew-pr"), &CredentialSpecSpec {
            consumer: CredentialConsumer::Gh,
            source: CredentialSource::Env { name: "GITHUB_TOKEN".to_string() },
            lifecycle: CredentialLifecycle::Static,
            placement: CredentialPlacementRequirements::default(),
        })
        .await
        .expect("create GitHub credential declaration");
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()])
        .build();

    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("github-project"),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["flotilla".to_string()])).build())
                .credentials(BTreeSet::from(["github-crew-pr".to_string()]))
                .build(),
        )
        .await
        .expect("create project grant");

    let mut host_direct = workflow.clone();

    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut host_direct).await.expect("resolve host-direct grant");

    let mut contained = workflow;
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut contained).await.expect("resolve contained grant");
    assert_eq!(host_direct.vessels[0].credential_refs, BTreeSet::from(["github-crew-pr".to_string()]));
    assert_eq!(host_direct.vessels[0].credential_refs, contained.vessels[0].credential_refs);
}

#[tokio::test]
async fn remote_placement_uses_replicated_host_capabilities() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("kiwi-root"));
    let now = Utc::now();
    let feta = ResourceBackend::InMemory(InMemoryBackend::default());
    let feta_hosts = feta.using::<ResourceHost>("flotilla");
    let fresh = feta_hosts
        .create(&test_meta("feta-host"), &HostSpec {
            display_name: "feta".to_string(),
            connection: Default::default(),
            ..HostSpec::default()
        })
        .await
        .expect("create fresh feta self-report");
    feta_hosts
        .update_status(&fresh.metadata.name, &fresh.metadata.resource_version, &HostStatus {
            capabilities: [(
                flotilla_resources::HELD_CREDENTIALS_CAPABILITY.to_string(),
                serde_json::json!(BTreeSet::from(["claude-max".to_string()])),
            )]
            .into_iter()
            .collect(),
            heartbeat_at: Some(now - chrono::Duration::seconds(1)),
            ready: true,
            daemon_generation: Some("fresh-feta-generation".to_string()),
            daemon_started_at: Some(now - chrono::Duration::minutes(1)),
            ..HostStatus::default()
        })
        .await
        .expect("write fresh feta capabilities");
    backend
        .replica_writer::<ResourceHost>(NodeId::new("feta-root"), "flotilla")
        .replace(&feta_hosts.list().await.expect("list feta self-report"), Utc::now())
        .await
        .expect("replicate fresh feta self-report to kiwi");

    let sources = backend.including_replicas::<ResourceHost>("flotilla").list().await.expect("list host sources");
    assert_eq!(sources.items.len(), 1, "a Host should have only its home-authored source");

    let placement = backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("feta-docker"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .docker_per_vessel(flotilla_resources::DockerPerVesselPlacementPolicySpec {
                    host_ref: "feta-host".to_string(),
                    image: "crew:latest".to_string().into(),
                    pull_policy: Default::default(),
                    agent_adapters: BTreeSet::new(),
                    default_cwd: None,
                    env: BTreeMap::new(),
                    checkout: flotilla_resources::DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
                })
                .build(),
        )
        .await
        .expect("create feta placement");
    let mut workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()])
        .build();
    workflow.vessels[0].credential_refs = BTreeSet::from(["claude-max".to_string()]);

    validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect("kiwi-to-feta admission should use feta's fresh self-report");

    workflow.vessels[0].credential_refs.insert("github-crew-pr".to_string());
    let error = validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect_err("a real missing credential should still refuse admission");
    assert_eq!(
        error,
        "workflow requires credential `github-crew-pr`, which placement `feta-docker` host `feta` generation `fresh-feta-generation` does not hold"
    );
}

#[tokio::test]
async fn trusted_claude_requires_and_accepts_a_project_selected_oauth_grant() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(&test_meta("claude-max"), &CredentialSpecSpec {
            consumer: CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
            source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
            lifecycle: CredentialLifecycle::Static,
            placement: CredentialPlacementRequirements::default(),
        })
        .await
        .expect("create Claude credential declaration");
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: Some("claude-code".to_string()), model: None },
                    prompt: None,
                    brief_template: None,
                })
                .build()])
            .build()])
        .build();

    let mut without_grant = workflow.clone();
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut without_grant)
        .await
        .expect("resolve default-deny grants");
    let error = validate_workflow_credentials(&backend, "flotilla", &without_grant, None)
        .await
        .expect_err("trusted Claude must not reach ambient login without delivered OAuth");
    assert_eq!(error, "agent adapter `claude-code` requires credential `claude-max`, but no matching CredentialGrant selected it");

    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("claude-max-trusted"),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["flotilla".to_string()])).build())
                .credentials(BTreeSet::from(["claude-max".to_string()]))
                .build(),
        )
        .await
        .expect("create project-selected trusted Claude grant");
    let mut with_grant = workflow;
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut with_grant)
        .await
        .expect("resolve matching trusted Claude grant");
    assert_eq!(with_grant.vessels[0].credential_refs, BTreeSet::from(["claude-max".to_string()]));

    create_docker_placement(&backend, "host-claude", "host-a", BTreeSet::from(["claude-max".to_string()])).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("host-claude").await.expect("get placement");
    let expired_ambient = CredentialExpiry::builder().refresh_expires_at("2026-07-30T00:00:00Z".parse().expect("timestamp")).build();
    set_host_credential_expiry(
        &backend,
        "host-a",
        BTreeMap::from([(flotilla_resources::AMBIENT_CLAUDE_CREDENTIAL_SCOPE.to_string(), expired_ambient)]),
    )
    .await;
    validate_workflow_credentials(&backend, "flotilla", &with_grant, Some(&placement))
        .await
        .expect("delivered OAuth admits trusted Claude despite an expired ambient login");
}

#[tokio::test]
async fn ambient_only_adapter_is_refused_when_the_host_login_expired() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent { selector: Selector::for_capability("ambient-only"), prompt: None, brief_template: None })
                .build()])
            .build()])
        .build();
    create_host_direct_placement(&backend, "ambient-host", "host-a", BTreeSet::new()).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("ambient-host").await.expect("get placement");
    let expired = CredentialExpiry::builder().refresh_expires_at("2020-02-01T00:00:00Z".parse().expect("timestamp")).build();
    set_host_credential_expiry(
        &backend,
        "host-a",
        BTreeMap::from([(flotilla_resources::AMBIENT_CLAUDE_CREDENTIAL_SCOPE.to_string(), expired)]),
    )
    .await;
    let capabilities = CapabilityTable::seeded().with_ambient_only_test_requirement("ambient-only");

    let error = validate_workflow_credentials_with_capabilities(&backend, "flotilla", &workflow, Some(&placement), &capabilities)
        .await
        .expect_err("expired ambient-only authentication must refuse dispatch");

    assert_eq!(
        error,
        "vessel `work` depends on the ambient claude login on host `host-a`, which expired on 2020-02-01 — log in again on that host or grant a delivered claude credential"
    );
}

async fn set_host_credential_expiry(backend: &ResourceBackend, host_ref: &str, expiry: BTreeMap<String, CredentialExpiry>) {
    let hosts = backend.clone().using::<ResourceHost>("flotilla");
    let host = hosts.get(host_ref).await.expect("host resource");
    let mut status = host.status.expect("host status");
    status.capabilities.insert(flotilla_resources::CREDENTIAL_EXPIRY_CAPABILITY.to_string(), serde_json::json!(expiry));
    hosts.update_status(host_ref, &host.metadata.resource_version, &status).await.expect("update host status");
}

#[tokio::test]
async fn dispatch_against_an_expired_credential_is_refused_with_the_credential_and_host_named() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(&test_meta("claude-max"), &CredentialSpecSpec {
            consumer: CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
            source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
            lifecycle: CredentialLifecycle::Static,
            placement: CredentialPlacementRequirements::default(),
        })
        .await
        .expect("create Claude credential declaration");
    let mut workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: Some("claude-code".to_string()), model: None },
                    prompt: None,
                    brief_template: None,
                })
                .build()])
            .build()])
        .build();
    workflow.vessels[0].credential_refs = BTreeSet::from(["claude-max".to_string()]);
    create_docker_placement(&backend, "docker-claude", "host-a", BTreeSet::from(["claude-max".to_string()])).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("docker-claude").await.expect("get placement");

    let near_expiry = CredentialExpiry::builder().refresh_expires_at(Utc::now() + chrono::Duration::days(3)).build();
    set_host_credential_expiry(&backend, "host-a", BTreeMap::from([("claude-max".to_string(), near_expiry)])).await;
    validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect("near-expiry material still admits dispatch");

    let expired = CredentialExpiry::builder()
        .expires_at("2020-01-01T00:00:00Z".parse().expect("timestamp"))
        .refresh_expires_at("2020-02-01T00:00:00Z".parse().expect("timestamp"))
        .build();
    set_host_credential_expiry(&backend, "host-a", BTreeMap::from([("claude-max".to_string(), expired)])).await;
    let error = validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect_err("expired credential must refuse dispatch");
    assert_eq!(error, "credential `claude-max` expired on host `host-a` on 2020-02-01 — refresh its material before dispatching");
}

#[tokio::test]
async fn image_baseline_admission_fails_without_agents_and_pins_resolved_image() {
    use flotilla_resources::{CrewImageBaseline, CrewImageBaselineSpec, DockerImageSource};

    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    create_docker_placement(&backend, "crew-policy", "host-a", BTreeSet::new()).await;
    let policies = backend.using::<PlacementPolicy>("flotilla");
    let mut policy = policies.get("crew-policy").await.expect("policy");
    policy.spec.docker_per_vessel.as_mut().expect("docker").image =
        DockerImageSource::Baseline { image_baseline_ref: "fleet-crew".to_string() };
    policies.update(&InputMeta::from(&policy.metadata), &policy.metadata.resource_version, &policy.spec).await.expect("reference baseline");
    let workflow = WorkflowTemplateSpec::builder().vessels(Vec::new()).build();
    let error = daemon
        .resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false)
        .await
        .expect_err("missing baseline must fail");
    assert!(error.contains("image-baseline `fleet-crew` missing/unresolved"), "{error}");
    let error = daemon
        .resolve_convoy_placement("flotilla", None, &[], &workflow, None, false)
        .await
        .expect_err("default selection must reject missing baseline");
    assert!(error.contains("image-baseline `fleet-crew` missing/unresolved"), "{error}");
    let baselines = backend.definitions::<CrewImageBaseline>("flotilla");
    baselines.apply(&test_meta("fleet-crew"), &CrewImageBaselineSpec { image: "crew:v1".to_string() }).await.expect("baseline");
    let admitted = daemon.resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false).await.expect("admit");
    baselines.apply(&test_meta("fleet-crew"), &CrewImageBaselineSpec { image: "crew:v2".to_string() }).await.expect("bump");
    assert_eq!(admitted.selected.expect("placement").spec.docker_per_vessel.expect("docker").image, DockerImageSource::from("crew:v1"));
    let next = daemon.resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false).await.expect("next admission");
    assert_eq!(next.selected.expect("placement").spec.docker_per_vessel.expect("docker").image, DockerImageSource::from("crew:v2"));
}

#[tokio::test]
async fn checkout_vcs_discovery_is_cached_per_checkout() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"checkout-vcs-cache-test\"\n").expect("daemon config");
    let first_path = temp.path().join("first");
    let second_path = temp.path().join("second");
    let mut discovery = fake_discovery(false);
    discovery.factories.vcs = vec![
        Box::new(FakeVcsFactory::new(FakeVcsState::builder(&first_path).build())),
        Box::new(FakeVcsFactory::new(FakeVcsState::builder(&second_path).build())),
    ];
    let daemon =
        InProcessDaemon::new(Vec::new(), Arc::new(ConfigStore::with_base(temp.path())), discovery, HostName::new("local-host")).await;

    let first = daemon.local_vcs_for_checkout(&first_path).await.expect("first checkout VCS");
    let first_again = daemon.local_vcs_for_checkout(&first_path).await.expect("cached first checkout VCS");
    let second = daemon.local_vcs_for_checkout(&second_path).await.expect("second checkout VCS");

    assert!(Arc::ptr_eq(&first, &first_again));
    assert!(!Arc::ptr_eq(&first, &second));
}

async fn stall_test_daemon() -> (Arc<InProcessDaemon>, ResourceBackend, tempfile::TempDir, tokio::task::JoinHandle<()>) {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"stall-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    let (sender, mut receiver) = flotilla_resources::controller::WorkQueueSender::channel();
    let watch = daemon.reconciler_wake_watch();
    let watch_backend = backend.clone();
    let task = tokio::spawn(async move {
        let drain = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
        watch.spawn(watch_backend, "flotilla".to_string(), sender).await.expect("stall watch");
        drain.abort();
    });
    (daemon, backend, temp, task)
}

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
        .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus {
            crew_work: BTreeMap::from([(
                "work".into(),
                BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
            )]),
            ..Default::default()
        })
        .await
        .expect("crew status");
    backend
        .clone()
        .using::<Vessel>("flotilla")
        .create(&test_meta("operator-failure-vessel"), &VesselSpec {
            convoy_ref: "operator-failure".into(),
            vessel_name: "work".into(),
            placement_policy_ref: "test".into(),
            adopted_checkout_refs: BTreeMap::new(),
        })
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

async fn wait_for_stall(backend: &ResourceBackend, name: &str, expected: bool) -> ConvoyStatus {
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            let status = backend.clone().using::<ResourceConvoy>("flotilla").get(name).await.expect("convoy").status.expect("status");
            if status.stalled.is_some() == expected {
                return status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("stall judgement timed out")
}

fn stall_workflow_snapshot(crew: Vec<flotilla_resources::CrewSpec>) -> flotilla_resources::WorkflowSnapshot {
    flotilla_resources::WorkflowSnapshot {
        exit: None,
        turn_delivery: Default::default(),
        stall_nudges: Default::default(),
        supervision: None,
        vessels: vec![flotilla_resources::VesselRequirement::builder().name("work".into()).crew(crew).build()],
    }
}

fn claim_crew(role: &str) -> flotilla_resources::CrewSpec {
    flotilla_resources::CrewSpec::builder()
        .role(role.to_string())
        .source(flotilla_resources::CrewSource::Tool { command: "test".into() })
        .completion_conditions(vec![flotilla_resources::CrewCompletionExpectation::artifact_exists(
            role,
            "decision-ledger",
            flotilla_resources::ArtifactSubjectBinding::Convoy,
        )])
        .build()
}

async fn stall_test_session(backend: &ResourceBackend, convoy: &str, name: &str, role: &str, attention: TerminalAttention) {
    let sessions = backend.clone().using::<ResourceTerminalSession>("flotilla");
    let session = sessions
        .create(
            &InputMeta::builder()
                .name(name.into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), convoy.into()),
                    (VESSEL_LABEL.into(), "work".into()),
                    (ROLE_LABEL.into(), role.into()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "env".into(),
                role: role.into(),
                source: TerminalSessionSource::Tool { command: "test".into() },
                cwd: "/tmp".into(),
                pool: "test".into(),
            },
        )
        .await
        .expect("session");
    sessions
        .update_status(name, &session.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            attention: Some(attention),
            ..Default::default()
        })
        .await
        .expect("attention");
}

#[tokio::test]
async fn active_idle_crew_stalls_and_working_crew_clears() {
    let (daemon, backend, _temp, watch) = stall_test_daemon().await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created =
        convoys.create(&test_meta("idle-crew"), &ConvoySpec::builder().workflow_ref("test".into()).build()).await.expect("convoy");
    convoys
        .update_status("idle-crew", &created.metadata.resource_version, &ConvoyStatus {
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
        })
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
                pool: "test".into(),
            },
        )
        .await
        .expect("session");
    let mut idle = sessions
        .update_status("idle-session", &session.metadata.resource_version, &ResourceTerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            attention: Some(TerminalAttention {
                state: TerminalAttentionState::Idle,
                as_of: Utc::now() - chrono::Duration::seconds(3),
                source: TerminalAttentionSource::Screen,
            }),
            ..Default::default()
        })
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
    assert_eq!(stalled.evidence, "idle");
    assert_eq!(stalled.source, flotilla_resources::StallEvidenceSource::Screen);
    let mut events = daemon.event_tx.subscribe();
    let subscription = daemon
        .leaf_subscriptions
        .subscribe_wait(uuid::Uuid::new_v4(), flotilla_protocol::WaitSubscriptionRequest {
            namespace: "flotilla".into(),
            leaves: vec!["convoy/idle-crew .status.stalled == true".parse().expect("stall leaf")],
            freshness_demand: None,
        })
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
        .update_status("stale-crew", &created.metadata.resource_version, &ConvoyStatus {
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
        })
        .await
        .expect("active status");
    stall_test_session(&backend, "stale-crew", "stale-session", "coder", TerminalAttention {
        state: TerminalAttentionState::Working,
        as_of: Utc::now() - chrono::Duration::minutes(3),
        source: TerminalAttentionSource::Hook,
    })
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
        .update_status("landed-claim", &created.metadata.resource_version, &ConvoyStatus {
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
        })
        .await
        .expect("landing status");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if daemon
                .leaf_subscriptions
                .rows()
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
        .update_status(&record_name, &record.metadata.resource_version, &flotilla_resources::ChangeRequestStatus {
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
        })
        .await
        .expect("fresh observation");
    stall_test_session(&backend, "landed-claim", "idle-landing-session", "coder", TerminalAttention {
        state: TerminalAttentionState::Idle,
        as_of: Utc::now(),
        source: TerminalAttentionSource::Hook,
    })
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
        .update_status("unarmed-landing", &created.metadata.resource_version, &ConvoyStatus {
            phase: flotilla_resources::ConvoyPhase::Landing,
            workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                stall_nudges: Default::default(),
                supervision: None,
                exit: None,
                turn_delivery: Default::default(),
                vessels: Vec::new(),
            }),
            ..Default::default()
        })
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
        .update_status("stale-observation", &created.metadata.resource_version, &ConvoyStatus {
            phase: flotilla_resources::ConvoyPhase::Landing,
            workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
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
        })
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
        .update_status(&record_name, &record.metadata.resource_version, &flotilla_resources::ChangeRequestStatus {
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
        })
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
        .update_status("standing", &created.metadata.resource_version, &ConvoyStatus {
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
        })
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
                    pool: "test".into(),
                },
            )
            .await
            .expect("session");
        sessions
            .update_status(name, &session.metadata.resource_version, &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state,
                    as_of: Utc::now() - chrono::Duration::seconds(10),
                    source: TerminalAttentionSource::Screen,
                }),
                ..Default::default()
            })
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
        .update_status("replicated", &created.metadata.resource_version, &ConvoyStatus {
            phase: flotilla_resources::ConvoyPhase::Landing,
            ..Default::default()
        })
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
