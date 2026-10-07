use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use flotilla_protocol::qualified_path::HostId;
use flotilla_resources::{
    ChangeRequestStatus, ConvoySpec, ConvoyStatus, CrewWorkState, InMemoryBackend, Selector, SystemClock, TerminalSessionSpec, WorkState,
    WorkflowSnapshot,
};

use super::*;
use crate::{
    change_request_observer::{ChangeRequestObservationSource, ChangeRequestRef, ChangeRequestRefreshCadence, ChangeRequestRefresher},
    environment_manager::EnvironmentManager,
    event_sink::RecordingEventSink,
    providers::{change_request::ObservationError, discovery::test_support::fake_discovery},
};

// Stand-in for forge I/O; these crew scenarios never request forge observations.
struct UnusedForge;

#[async_trait]
impl ChangeRequestObservationSource for UnusedForge {
    async fn observe(&self, _: &ChangeRequestRef) -> Result<ChangeRequestStatus, ObservationError> {
        panic!("crew delivery must not query the forge")
    }
}

// Stand-in for credential controller I/O. Assert staging precedes terminal delivery.
struct StagingProbe {
    backend: ResourceBackend,
    fail: AtomicBool,
    calls: AtomicUsize,
}

#[async_trait]
impl WorkCredentialReconciler for StagingProbe {
    async fn reconcile(&self, namespace: &str, environment: &str) -> Result<(), String> {
        assert_eq!(environment, "resume-env");
        let session = self.backend.using::<ResourceTerminalSession>(namespace).get("session").await.expect("session");
        assert!(matches!(session.spec.source, TerminalSessionSource::Agent { message: None, .. }));
        let convoy = self.backend.using::<ResourceConvoy>(namespace).get("crew").await.expect("convoy");
        assert_eq!(convoy.status.expect("status").crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.swap(false, Ordering::SeqCst) {
            Err("staging unavailable".into())
        } else {
            Ok(())
        }
    }
}

async fn fixture(phase: CrewWorkPhase) -> (Arc<CrewService>, ResourceBackend, Arc<StagingProbe>, tempfile::TempDir) {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let config_dir = tempfile::tempdir().expect("config directory");
    let config = Arc::new(ConfigStore::with_base(config_dir.path()));
    let discovery = Arc::new(fake_discovery(false));
    let environment = EnvironmentId::new("local");
    let environment_manager = Arc::new(EnvironmentManager::new_local(&discovery, environment.clone(), HostId::new("host")).await);
    let namespace = Arc::new(std::sync::RwLock::new("flotilla".into()));
    let providers = Arc::new(
        CheckoutProviders::builder()
            .resource_backend(backend.clone())
            .observed_resource_backend(ResourceBackend::InMemory(InMemoryBackend::default()))
            .config(config.clone())
            .discovery(discovery)
            .environment_manager(environment_manager.clone())
            .local_environment_id(environment.clone())
            .provisioning_namespace(namespace.clone())
            .build(),
    );
    let refresher = ChangeRequestRefresher::new(
        "host".into(),
        backend.clone(),
        "host".into(),
        Arc::new(UnusedForge),
        ChangeRequestRefreshCadence::default(),
    );
    let subscriptions = LeafSubscriptionTable::new(backend.clone(), Arc::new(RecordingEventSink::default()), refresher);
    let probe = Arc::new(StagingProbe { backend: backend.clone(), fail: AtomicBool::new(true), calls: AtomicUsize::new(0) });
    let reconciler: Arc<dyn WorkCredentialReconciler> = probe.clone();
    let crew = Arc::new(
        CrewService::builder()
            .resource_backend(backend.clone())
            .leaf_subscriptions(subscriptions)
            .work_credential_reconciler(RwLock::new(Some(reconciler)))
            .clock(Arc::new(SystemClock))
            .provisioning_namespace(namespace)
            .config(config)
            .host_name(HostName::new("host"))
            .brief_artifact_writer(Arc::new(RwLock::new(None)))
            .checkout_providers(providers)
            .local_environment_id(environment)
            .build(),
    );
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .create(
            &InputMeta::builder().name("crew".into()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).role("crew".into()).build(),
        )
        .await
        .expect("convoy");
    convoys
        .update_status(
            "crew",
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Landing,
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    vessels: vec![flotilla_resources::VesselRequirement::builder()
                        .name("work".into())
                        .crew(vec![flotilla_resources::CrewSpec::builder()
                            .role("coder".into())
                            .source(CrewSource::Agent {
                                selector: Selector { capability: "code".into(), adapter: None, model: None },
                                prompt: None,
                                brief_template: None,
                            })
                            .build()])
                        .build()],
                    exit: None,
                    turn_delivery: Default::default(),
                    stall_nudges: Default::default(),
                    supervision: None,
                }),
                work: BTreeMap::from([("work".into(), WorkState::builder().phase(flotilla_resources::WorkPhase::Complete).build())]),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(phase).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("status");
    backend
        .using::<ResourceTerminalSession>("flotilla")
        .create(
            &InputMeta::builder()
                .name("session".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "crew".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                    (ROLE_LABEL.into(), "coder".into()),
                ]))
                .build(),
            &TerminalSessionSpec {
                env_ref: "resume-env".into(),
                role: "coder".into(),
                source: TerminalSessionSource::Agent {
                    selector: Selector { capability: "code".into(), adapter: None, model: None },
                    brief: TerminalBrief { artifact_digest: None, path: "brief.md".into(), content: "original".into(), copies: vec![] },
                    context: Box::new(TerminalCrewContext {
                        namespace: "flotilla".into(),
                        convoy: "crew".into(),
                        vessel_ref: "vessel".into(),
                    }),
                    message: None,
                },
                cwd: "/repo".into(),
                env: Default::default(),
                pool: "passthrough".into(),
            },
        )
        .await
        .expect("session");
    (crew, backend, probe, config_dir)
}

// Resume must restore work on credential failure and stage credentials before retry delivery (#2221).
#[tokio::test]
async fn resume_restores_work_until_credentials_are_staged() {
    let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Done).await;
    let error = crew.resume("flotilla", "crew", "continue", None, None).await.expect_err("credential failure");
    assert_eq!(error, "staging unavailable");
    let convoy = backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("convoy");
    assert_eq!(convoy.status.expect("status").crew_work["work"]["coder"].phase, CrewWorkPhase::Done);
    assert_eq!(
        crew.resume("flotilla", "crew", "continue", None, None).await.expect("retry"),
        ConvoyResumeOutcome::Queued { displaced: None }
    );
    let session = backend.using::<ResourceTerminalSession>("flotilla").get("session").await.expect("session");
    assert!(matches!(session.spec.source, TerminalSessionSource::Agent { message: None, .. }));
    let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.body, "continue");
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
}

// Working crew without fresh idle evidence queues replacements; withdrawal is idempotent (#2221).
#[hegel::test]
fn queued_resume_replaces_and_withdraws_without_staging(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Include empty sequences, duplicate briefs, empty invalid prompts, and repeated withdrawals.
    let steps = tc.draw(gs::integers::<usize>().min_value(0).max_value(8));
    let operations: Vec<u8> = (0..steps).map(|_| tc.draw(gs::integers::<u8>().min_value(0).max_value(3))).collect();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Working).await;
        let mut expected = None;
        for operation in operations {
            match operation {
                0 | 1 => {
                    let prompt = if operation == 0 { "first" } else { "second" };
                    assert_eq!(
                        crew.resume("flotilla", "crew", prompt, None, None).await.expect("queue"),
                        ConvoyResumeOutcome::Queued { displaced: expected.clone() }
                    );
                    expected = Some(prompt.to_string());
                }
                2 => {
                    assert_eq!(crew.withdraw_pending_brief("flotilla", "crew").await.expect("withdraw"), expected.take());
                }
                _ => {
                    crew.resume("flotilla", "crew", "", None, None).await.expect_err("empty prompt refused");
                }
            }
            let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
            let pending: Vec<_> =
                records.iter().filter(|message| message.status.as_ref().is_none_or(|status| !status.phase.is_terminal())).collect();
            assert_eq!(pending.first().map(|message| message.spec.body.clone()), expected);
            assert!(pending.len() <= 1, "replacement and withdrawal leave at most one pending operator message");
            assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
            let session = backend.using::<ResourceTerminalSession>("flotilla").get("session").await.expect("session");
            assert!(matches!(session.spec.source, TerminalSessionSource::Agent { message: None, .. }));
        }
    });
}

#[test]
fn crew_message_sender_headers_snapshot() {
    // Keep the pre-extraction snapshot names and paths; the rendered contract is unchanged.
    let mut settings = insta::Settings::clone_current();
    settings.set_prepend_module_to_snapshot(false);
    settings.set_snapshot_path("../snapshots");
    let _guard = settings.bind_to_scope();
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
        insta::assert_snapshot!(format!("flotilla_core__in_process__tests__{name}"), frame_crew_message(&sender, "Example message."));
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

// One-generation envelopes must preserve every previous sender shape during unrelated writes.
// Generate payloads containing delimiters and vary nullable operator attribution; cover every tag.
#[hegel::test]
fn legacy_sender_envelopes_preserve_stored_shapes(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let payload = format!("actor/{}]\n{{}}", tc.draw(gs::integers::<usize>().min_value(0).max_value(100)));
    let principal =
        if tc.draw(gs::booleans()) { serde_json::json!({"namespace": "flotilla", "name": payload}) } else { serde_json::Value::Null };
    let shapes = [
        serde_json::json!({"kind": "unknown"}),
        serde_json::json!({"kind": "flotilla-nudge"}),
        serde_json::json!({"kind": "flotilla-turn", "source": payload}),
        serde_json::json!({"kind": "flotilla-escalation", "from": payload}),
        serde_json::json!({"kind": "operator-resume", "principal": principal}),
        serde_json::json!({"kind": "operator-follow-up", "principal": principal}),
        serde_json::json!({"kind": "governor", "name": payload}),
        serde_json::json!({"kind": "bosun", "name": payload}),
        serde_json::json!({"kind": "handoff", "from": payload}),
    ];
    for shape in shapes {
        let sender: CrewMessageSender = serde_json::from_value(shape.clone()).expect("previous-generation sender");
        let envelope = pending_crew_message(sender.clone(), "legacy input");
        let encoded = serde_json::to_value(&envelope).expect("preserve envelope");
        assert_eq!(encoded["sender"], shape);
        let decoded: TerminalCrewMessage = serde_json::from_value(encoded).expect("decode preserved envelope");
        assert_eq!(decoded.sender, sender);
    }
}

// The delivery port must refuse both operations after its owning service stops;
// retaining the actuator cannot keep the service alive (#2221).
#[tokio::test]
async fn actuator_refuses_delivery_and_hold_after_service_stops() {
    use crate::leaf_engine::{CrewTurnIntent, TurnDeliveryActuator};
    let (crew, _backend, _probe, _config) = fixture(CrewWorkPhase::Done).await;
    let actuator = Arc::new(CrewTurnDeliveryActuator { crew: Arc::downgrade(&crew) });
    crew.set_turn_delivery_actuator(actuator.clone()).await;
    drop(crew);
    assert!(actuator.crew.upgrade().is_none(), "subscriptions and actuator must not form a strong cycle");
    let request = CrewTurnIntent {
        receiver: None,
        namespace: "flotilla".into(),
        convoy: "crew".into(),
        source: "rule".into(),
        vessel: "work".into(),
        role: "coder".into(),
        brief: "continue".into(),
        subject_revision: "head".into(),
        subject: None,
        sender: "system:nudge".into(),
        relation: flotilla_resources::MessageRelation::System,
        references: Vec::new(),
        message_subject: None,
        expectation: Default::default(),
    };
    assert_eq!(actuator.deliver(&request).await.expect_err("stopped delivery"), "daemon stopped before turn delivery");
    assert_eq!(
        actuator.hold(&request, &HoldAct::State, "stopped").await.expect_err("stopped hold"),
        "daemon stopped before turn-delivery hold"
    );
}

// #1986: each orientation query follows live Project membership and roles while
// the crew, convoy, and admission snapshot remain intact.
#[hegel::test]
fn orientation_follows_live_project(tc: hegel::TestCase) {
    use flotilla_resources::{ProjectRepositoryRole, ProjectRepositorySpec, ProjectSpec, VesselSpec};
    use hegel::generators as gs;
    // Sequences cover empty membership, additions/removals, duplicate repo keys
    // with distinct subpaths, and every role (including multiple roles).
    let replicated = tc.draw(gs::booleans());
    let steps = tc.draw(gs::integers::<usize>().min_value(2).max_value(5));
    let operations: Vec<_> = (0..steps)
        .map(|_| (tc.draw(gs::integers::<usize>().min_value(0).max_value(3)), tc.draw(gs::integers::<usize>().min_value(0).max_value(7))))
        .collect();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
        backend
            .using::<Vessel>("flotilla")
            .create(
                &InputMeta::builder().name("vessel".into()).build(),
                &VesselSpec {
                    convoy_ref: "crew".into(),
                    vessel_name: "work".into(),
                    placement_policy_ref: "test".into(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
            )
            .await
            .expect("vessel");
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        let mut convoy = convoys.get("crew").await.expect("convoy");
        convoy.spec.project_ref = Some("island".into());
        let admission = convoys
            .update(&InputMeta::from(&convoy.metadata), &convoy.metadata.resource_version, &convoy.spec)
            .await
            .expect("project reference");
        let sessions = backend.using::<ResourceTerminalSession>("flotilla");
        let session = sessions.get("session").await.expect("session");
        sessions
            .update_status(
                "session",
                &session.metadata.resource_version,
                &flotilla_resources::TerminalSessionStatus {
                    crew: Some(flotilla_resources::CrewSessionStatus {
                        id: "governor-id".into(),
                        adapter: "codex".into(),
                        model: None,
                        stance: "trusted-implicit".into(),
                    }),
                    ..Default::default()
                },
            )
            .await
            .expect("crew identity");
        let repository_spec = flotilla_resources::RepositorySpec::remote("https://github.com/example/live").expect("repository spec");
        let repository_key = repository_spec.key();
        backend
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name(repository_key.0.clone()).build(), &repository_spec)
            .await
            .expect("live Repository");
        let origin_root = flotilla_protocol::NodeId::new("project-home");
        let origin = if replicated {
            ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(origin_root.clone())
        } else {
            backend.clone()
        };
        let projects = origin.using::<Project>("flotilla");
        let writer = backend.replica_writer::<Project>(origin_root, "flotilla");
        let context = CrewCommandContext::builder().convoy("crew".into()).vessel_ref("vessel".into()).role("coder".into()).build();
        let unavailable = crew.crew_list_internal(&context).await.expect("crew state with missing Project");
        assert!(unavailable.project.is_none());
        assert!(unavailable.project_error.as_deref().expect("explicit charter error").contains("live Project"));
        assert_eq!(unavailable.members[0].role, "coder");
        let mut spec = ProjectSpec::builder().display_name("Island".into()).default_workflow_ref("workflow".into()).build();
        let mut project = projects.create(&InputMeta::builder().name("island".into()).build(), &spec).await.expect("project");
        for (count, role_bits) in operations {
            spec.repositories = (0..count)
                .map(|index| {
                    ProjectRepositorySpec::builder()
                        .repo(if index % 2 == 0 { repository_key.clone() } else { RepositoryKey("missing-repo".into()) })
                        .alias(format!("member-{index}"))
                        .subpath(format!("part-{index}"))
                        .default_branch("main".into())
                        .roles(
                            [ProjectRepositoryRole::Code, ProjectRepositoryRole::Ops, ProjectRepositoryRole::Knowledge]
                                .into_iter()
                                .enumerate()
                                .filter_map(|(bit, role)| (role_bits & (1 << bit) != 0).then_some(role))
                                .collect(),
                        )
                        .build()
                })
                .collect();
            project = projects
                .update(&InputMeta::from(&project.metadata), &project.metadata.resource_version, &spec)
                .await
                .expect("live membership edit");
            if replicated {
                writer.replace(&projects.list().await.expect("source projects"), Utc::now()).await.expect("replicate live Project");
            }
            let ambient = CrewCommandContext::builder().crew_id("governor-id".into()).build();
            let result = crew.crew_list_internal(&ambient).await.expect("orientation through crew identity");
            assert_eq!(result, crew.crew_list_internal(&context).await.expect("explicit orientation"));
            assert!(result.project_error.is_none(), "successful orientation clears the charter error");
            let charter = result.project.expect("live charter");
            assert_eq!((charter.namespace.as_str(), charter.name.as_str()), ("flotilla", "island"));
            assert_eq!(charter.repositories.len(), spec.repositories.len());
            for (actual, expected) in charter.repositories.iter().zip(&spec.repositories) {
                assert_eq!(actual.key, expected.repo);
                assert_eq!(actual.roles, expected.roles);
                assert_eq!(actual.alias, expected.alias);
                assert_eq!(actual.subpath, expected.subpath);
                assert_eq!(actual.default_branch, expected.default_branch);
                if actual.key == repository_key {
                    assert_eq!(actual.remotes, repository_spec.remotes());
                } else {
                    assert!(actual.remotes.is_empty(), "missing Repository retains declared membership");
                }
            }
            let current = convoys.get("crew").await.expect("unchanged convoy");
            assert_eq!(current.metadata.resource_version, admission.metadata.resource_version);
            assert_eq!(current.spec, admission.spec);
            assert_eq!(current.status, admission.status);
        }
        projects.delete("island").await.expect("remove Project");
        if replicated {
            writer.replace(&projects.list().await.expect("source projects"), Utc::now()).await.expect("replicate removal");
        }
        let removed = crew.crew_list_internal(&context).await.expect("crew state after Project removal");
        assert!(removed.project.is_none(), "a removed charter must not fall back to admission");
        assert!(removed.project_error.is_some());
        assert_eq!(removed.members, unavailable.members);
        let mut current = convoys.get("crew").await.expect("convoy");
        current.spec.project_ref = None;
        convoys
            .update(&InputMeta::from(&current.metadata), &current.metadata.resource_version, &current.spec)
            .await
            .expect("unscoped convoy");
        let unscoped = crew.crew_list_internal(&context).await.expect("unscoped query");
        assert!(unscoped.project.is_none());
        assert!(unscoped.project_error.is_none());
    });
}

// A delivered-open subject suppresses the next intent. Admission returns the
// predecessor's durable reference so producers cannot latch an absent Message.
#[tokio::test]
async fn turn_admission_returns_the_canonical_suppressed_message() {
    use flotilla_resources::{Message, MessageExpectation, MessageReference, MessageStatusPatch, ResolvedMessageReceiver};

    use crate::leaf_engine::CrewTurnIntent;
    let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Working).await;
    probe.fail.store(false, Ordering::SeqCst);
    let reference = MessageReference::ControlRecord {
        resource: ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "crew"),
        revision: "revision".into(),
    };
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy("crew".into())
        .source("first".into())
        .vessel("work".into())
        .role("coder".into())
        .brief("reply to this subject".into())
        .subject_revision("revision".into())
        .sender("system:turn-rules".into())
        .references(vec![reference.clone()])
        .message_subject(reference)
        .expectation(MessageExpectation::Reply)
        .build();
    let first = crew.deliver_turn(&request).await.expect("first admission");
    flotilla_resources::apply_status_patch(
        &backend.using::<Message>("flotilla"),
        &first.message.name,
        &MessageStatusPatch::Delivered {
            receiver: ResolvedMessageReceiver::builder()
                .crew_id("crew-id".into())
                .session("session".into())
                .delivered_at(Utc::now())
                .evidence("transport acceptance".into())
                .build(),
            at: Utc::now(),
        },
    )
    .await
    .expect("open receipt");
    let before = backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status;
    let staging_calls = probe.calls.load(Ordering::SeqCst);
    let mut successor = request.clone();
    successor.source = "second".into();
    successor.brief = "duplicate turn".into();
    let suppressed = crew.deliver_turn(&successor).await.expect("suppression admission");
    assert!(first.new_turn);
    assert!(!suppressed.new_turn);
    assert_eq!(suppressed.message, first.message);
    assert_eq!(
        backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status,
        before,
        "suppression must not reopen workflow state for an absent new turn"
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), staging_calls);
    assert_eq!(backend.using::<Message>("flotilla").list().await.expect("inbox").items.len(), 1);
}

// A queued follow-up continues workflow only after Message acceptance evidence;
// the authority watch consumes its reference without creating another input.
#[tokio::test]
async fn follow_up_reference_releases_from_message_evidence() {
    use flotilla_resources::{Message, MessageStatusPatch, ResolvedMessageReceiver};
    let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
    crew.resume("flotilla", "crew", "continue after this turn", Some("work"), Some("coder")).await.unwrap();
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let pending = convoys.get("crew").await.unwrap().status.unwrap().crew_work["work"]["coder"].pending_follow_up.clone().unwrap();
    crew.reconcile_pending_supervisor_turns_once("flotilla").await.unwrap();
    assert_eq!(convoys.get("crew").await.unwrap().status.unwrap().crew_work["work"]["coder"].pending_follow_up.as_ref(), Some(&pending));
    flotilla_resources::apply_status_patch(
        &backend.using::<Message>("flotilla"),
        &pending.name,
        &MessageStatusPatch::Delivered {
            receiver: ResolvedMessageReceiver::builder()
                .crew_id("crew-id".into())
                .session("session".into())
                .delivered_at(Utc::now())
                .evidence("transport acceptance".into())
                .build(),
            at: Utc::now(),
        },
    )
    .await
    .unwrap();
    crew.reconcile_pending_supervisor_turns_once("flotilla").await.unwrap();
    let state = convoys.get("crew").await.unwrap().status.unwrap().crew_work["work"]["coder"].clone();
    assert!(state.pending_follow_up.is_none());
    assert_eq!(state.resume_brief_id.as_deref(), Some(pending.name.as_str()));
    assert_eq!(state.message.as_deref(), Some("continue after this turn"));
    assert_eq!(backend.using::<Message>("flotilla").list().await.unwrap().items.len(), 1);
}

// Follow-up references resolve their target namespace, even when a same-name
// Message in the Convoy namespace has different delivery evidence and intent.
#[tokio::test]
async fn follow_up_reference_uses_target_namespace() {
    use flotilla_resources::{Message, MessageStatusPatch, ResolvedMessageReceiver};
    let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
    crew.resume("flotilla", "crew", "local decoy", Some("work"), Some("coder")).await.expect("queue local follow-up");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys.get("crew").await.expect("convoy");
    let mut status = convoy.status.expect("crew status");
    let pending = status
        .crew_work
        .get_mut("work")
        .expect("vessel")
        .get_mut("coder")
        .expect("coder")
        .pending_follow_up
        .as_mut()
        .expect("queued reference");
    pending.namespace = "target".into();
    let name = pending.name.clone();
    convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("retarget follow-up");
    let local = backend.using::<Message>("flotilla").get(&name).await.expect("local decoy");
    let mut intent = local.spec;
    intent.body = "target continuation".into();
    let messages = backend.using::<Message>("target");
    messages.create(&InputMeta::builder().name(name.clone()).build(), &intent).await.expect("target message");
    flotilla_resources::apply_status_patch(
        &messages,
        &name,
        &MessageStatusPatch::Delivered {
            receiver: ResolvedMessageReceiver::builder()
                .crew_id("crew-id".into())
                .session("session".into())
                .delivered_at(Utc::now())
                .evidence("transport acceptance".into())
                .build(),
            at: Utc::now(),
        },
    )
    .await
    .expect("target delivery evidence");
    crew.reconcile_pending_supervisor_turns_once("flotilla").await.expect("continue from target");
    let state = convoys.get("crew").await.expect("continued convoy").status.expect("continued status").crew_work["work"]["coder"].clone();
    assert!(state.pending_follow_up.is_none());
    assert_eq!(state.message.as_deref(), Some("target continuation"));
}

// Receiver admission can suppress a turn before its Message has replicated to
// the producer. The acknowledgement must restore speculative workflow activation.
#[tokio::test]
async fn remote_suppression_restores_workflow_before_replication() {
    use flotilla_resources::{
        Message, MessageExpectation, MessageInbox, MessageReference, MessageRelation, MessageSpec, MessageStatusPatch,
        ResolvedMessageReceiver,
    };

    use crate::leaf_engine::{CrewTurnIntent, ResourceIntentPublisher};
    // Boundary stand-in for the ordinary cross-host resource mutation endpoint.
    struct ReceiverPublisher(MessageInbox);
    #[async_trait]
    impl ResourceIntentPublisher for ReceiverPublisher {
        async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<ResourceRef, String> {
            assert_eq!(document["kind"], "Message");
            let spec: MessageSpec = serde_json::from_value(document["spec"].clone()).unwrap();
            let admission = self
                .0
                .accept(&InputMeta::builder().name(document["metadata"]["name"].as_str().unwrap().into()).build(), &spec, Utc::now())
                .await
                .unwrap();
            let record = match admission {
                flotilla_resources::MessageAdmission::Accepted(record) => record,
                flotilla_resources::MessageAdmission::Suppressed { predecessor } => predecessor,
            };
            Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, record.metadata.name))
        }
    }
    let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Done).await;
    probe.fail.store(false, Ordering::SeqCst);
    let remote = ResourceBackend::InMemory(Default::default());
    let inbox = MessageInbox::new(remote.clone(), "flotilla");
    let subject = MessageReference::ControlRecord {
        resource: ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "crew"),
        revision: "revision".into(),
    };
    let existing = MessageSpec::builder()
        .sender("system:turn-rules".into())
        .receiver("flotilla/crew/work/coder".into())
        .relation(MessageRelation::System)
        .body("prior open request".into())
        .references(vec![subject.clone()])
        .subject(subject.clone())
        .expectation(MessageExpectation::Reply)
        .build();
    inbox.accept(&InputMeta::builder().name("canonical".into()).build(), &existing, Utc::now()).await.unwrap();
    flotilla_resources::apply_status_patch(
        &remote.using::<Message>("flotilla"),
        "canonical",
        &MessageStatusPatch::Delivered {
            receiver: ResolvedMessageReceiver::builder()
                .crew_id("crew-id".into())
                .session("session".into())
                .delivered_at(Utc::now())
                .evidence("accepted at receiver".into())
                .build(),
            at: Utc::now(),
        },
    )
    .await
    .unwrap();
    let publisher: Arc<dyn ResourceIntentPublisher> = Arc::new(ReceiverPublisher(inbox));
    crew.set_resource_intent_publisher(Arc::downgrade(&publisher));
    let before = backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status;
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy("crew".into())
        .source("second".into())
        .vessel("work".into())
        .role("coder".into())
        .brief("duplicate request".into())
        .subject_revision("revision".into())
        .sender("system:turn-rules".into())
        .references(vec![subject.clone()])
        .message_subject(subject)
        .expectation(MessageExpectation::Reply)
        .build();
    let admission = crew.deliver_turn(&request).await.unwrap();
    assert!(!admission.new_turn);
    assert_eq!(admission.message.name, "canonical");
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status, before);
    assert_eq!(remote.using::<Message>("flotilla").list().await.unwrap().items.len(), 1);
    assert!(backend.using::<Message>("flotilla").list().await.unwrap().items.is_empty());
}

// The receiver's ordinary resource mutation result is canonical even before
// replication. Suppression must preserve settled workflow state in both cases.
#[tokio::test]
async fn turn_admission_uses_canonical_receiver_result_without_reopening_work() {
    use flotilla_resources::{
        Message, MessageExpectation, MessageInbox, MessageReference, MessageRelation, MessageSpec, MessageStatusPatch,
        ResolvedMessageReceiver,
    };

    use crate::leaf_engine::{CrewTurnIntent, ResourceIntentPublisher};
    struct ReceiverPublisher(MessageInbox);
    #[async_trait]
    impl ResourceIntentPublisher for ReceiverPublisher {
        async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<ResourceRef, String> {
            assert_eq!(document["kind"], "Message");
            let spec = serde_json::from_value(document["spec"].clone()).unwrap();
            let admission = self
                .0
                .accept(&InputMeta::builder().name(document["metadata"]["name"].as_str().unwrap().into()).build(), &spec, Utc::now())
                .await
                .unwrap();
            let record = match admission {
                flotilla_resources::MessageAdmission::Accepted(record) => record,
                flotilla_resources::MessageAdmission::Suppressed { predecessor } => predecessor,
            };
            Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, record.metadata.name))
        }
    }
    for visible in [true, false] {
        let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Done).await;
        probe.fail.store(false, Ordering::SeqCst);
        let receiver = if visible { backend.clone() } else { ResourceBackend::InMemory(Default::default()) };
        let inbox = MessageInbox::new(receiver.clone(), "flotilla");
        let subject =
            MessageReference::ChangeRequest { service: "github".into(), scope: "org/repo".into(), number: 1, revision: "head".into() };
        let intent = MessageSpec::builder()
            .sender("system:turn-rules".into())
            .receiver("flotilla/crew/work/coder".into())
            .relation(MessageRelation::System)
            .body("prior open request".into())
            .references(vec![subject.clone()])
            .subject(subject.clone())
            .expectation(MessageExpectation::Reply)
            .build();
        inbox.accept(&InputMeta::builder().name("canonical".into()).build(), &intent, Utc::now()).await.unwrap();
        flotilla_resources::apply_status_patch(
            &receiver.using::<Message>("flotilla"),
            "canonical",
            &MessageStatusPatch::Delivered {
                receiver: ResolvedMessageReceiver::builder()
                    .crew_id("crew-id".into())
                    .session("session".into())
                    .delivered_at(Utc::now())
                    .evidence("receiver receipt".into())
                    .build(),
                at: Utc::now(),
            },
        )
        .await
        .unwrap();
        let publisher: Arc<dyn ResourceIntentPublisher> = Arc::new(ReceiverPublisher(inbox));
        if !visible {
            crew.set_resource_intent_publisher(Arc::downgrade(&publisher));
        }
        let before = backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status;
        let staging = probe.calls.load(Ordering::SeqCst);
        let request = CrewTurnIntent::builder()
            .namespace("flotilla".into())
            .convoy("crew".into())
            .source("review".into())
            .vessel("work".into())
            .role("coder".into())
            .brief("duplicate request".into())
            .subject_revision("head".into())
            .references(vec![subject.clone()])
            .message_subject(subject)
            .sender("system:turn-rules".into())
            .build();
        let admission = crew.deliver_turn(&request).await.unwrap();
        assert!(!admission.new_turn);
        assert_eq!(admission.message.name, "canonical");
        assert_eq!(backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status, before);
        assert_eq!(receiver.using::<Message>("flotilla").list().await.unwrap().items.len(), 1);
        if visible {
            assert_eq!(probe.calls.load(Ordering::SeqCst), staging);
        } else {
            assert!(probe.calls.load(Ordering::SeqCst) > staging, "the remote case must exercise speculative activation");
            assert!(backend.using::<Message>("flotilla").list().await.unwrap().items.is_empty());
        }
    }
}

#[tokio::test]
async fn unknown_controller_sender_has_a_creatable_system_reply() {
    use flotilla_resources::Message;

    use crate::{in_process::InProcessDaemon, leaf_engine::CrewTurnIntent};

    let (crew, backend, probe, config) = fixture(CrewWorkPhase::Done).await;
    probe.fail.store(false, Ordering::SeqCst);
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy("crew".into())
        .source("legacy".into())
        .vessel("work".into())
        .role("coder".into())
        .brief("legacy controller turn".into())
        .subject_revision("legacy-head".into())
        .sender(flotilla_resources::legacy_message_sender(&CrewMessageSender::Unknown).0)
        .build();
    let admission = crew.deliver_turn(&request).await.expect("controller intent");
    let original = backend.using::<Message>("flotilla").get(&admission.message.name).await.expect("receiver intent");
    std::fs::write(config.path().join("daemon.toml"), "machine_id = \"legacy-message-reply-test\"\n").expect("reply daemon identity");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(config.path())),
        fake_discovery(false),
        HostName::new("host"),
        backend.clone(),
    )
    .await;
    let reply = serde_json::json!({
        "apiVersion":"flotilla.work/v1", "kind":"Message", "metadata":{"name":"legacy-reply"},
        "spec":{"sender":original.spec.receiver,"receiver":original.spec.sender,"relation":"peer",
                "body":"acknowledged","in_reply_to":original.metadata.name}
    });
    assert_eq!(
        daemon.message_creation_origin("flotilla", &reply).await.expect("correlated system reply has a home"),
        Some(daemon.node_id().clone())
    );
    assert_eq!(original.spec.sender, "system:legacy");
    daemon.apply_intent_document("flotilla", reply).await.expect("create correlated system reply");
    assert!(backend.using::<Message>("flotilla").get("legacy-reply").await.is_ok());
}

// Failed durable admission must not reopen settled work. Exercise an external
// publication error and a receiver collision through the real crew service.
#[tokio::test]
async fn turn_publication_errors_restore_the_owned_activation() {
    use crate::leaf_engine::{CrewTurnIntent, ResourceIntentPublisher};
    struct FailedPublisher;
    #[async_trait]
    impl ResourceIntentPublisher for FailedPublisher {
        async fn publish(self: Arc<Self>, _: &str, _: serde_json::Value) -> Result<ResourceRef, String> {
            Err("receiver publication unavailable".into())
        }
    }
    struct AdmissionCollision {
        probe: Arc<StagingProbe>,
        backend: ResourceBackend,
    }
    #[async_trait]
    impl WorkCredentialReconciler for AdmissionCollision {
        async fn reconcile(&self, namespace: &str, environment: &str) -> Result<(), String> {
            self.probe.reconcile(namespace, environment).await?;
            let receiver = "flotilla/crew/work/coder";
            let sender = "system:legacy";
            let name = flotilla_resources::message_record_name(receiver, sender, "turn-delivery:failure:failure-head");
            let spec = flotilla_resources::MessageSpec::builder()
                .sender(sender.into())
                .receiver(receiver.into())
                .relation(flotilla_resources::MessageRelation::System)
                .body("concurrent intent".into())
                .build();
            flotilla_resources::MessageInbox::new(self.backend.clone(), namespace)
                .accept(&InputMeta::builder().name(name).build(), &spec, Utc::now())
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
    }
    for external in [false, true] {
        let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Done).await;
        probe.fail.store(false, Ordering::SeqCst);
        let publisher: Arc<dyn ResourceIntentPublisher> = Arc::new(FailedPublisher);
        if external {
            crew.set_resource_intent_publisher(Arc::downgrade(&publisher));
        } else {
            *crew.work_credential_reconciler.write().await =
                Some(Arc::new(AdmissionCollision { probe: probe.clone(), backend: backend.clone() }));
        }
        let before = backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("before admission").status;
        let request = CrewTurnIntent::builder()
            .namespace("flotilla".into())
            .convoy("crew".into())
            .source("failure".into())
            .vessel("work".into())
            .role("coder".into())
            .subject_revision("failure-head".into())
            .brief("continue".into())
            .sender(flotilla_resources::legacy_message_sender(&CrewMessageSender::Unknown).0)
            .build();
        let error = crew.deliver_turn(&request).await.expect_err("durable admission fails");
        assert!(error.contains(if external { "receiver publication unavailable" } else { "different intent" }), "{error}");
        assert!(probe.calls.load(Ordering::SeqCst) > 0, "exercise activation before admission");
        assert_eq!(backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("after failed admission").status, before);
        let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("receiver inbox").items;
        assert_eq!(records.len(), usize::from(!external));
        if !external {
            assert_eq!(records[0].spec.body, "concurrent intent");
        }
    }
}

#[tokio::test]
async fn resume_and_withdraw_wait_for_follow_up_replication() {
    for withdraw in [false, true] {
        let (crew, backend, _, _config) = fixture(CrewWorkPhase::Done).await;
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        apply_resource_status_patch(
            &convoys,
            "crew",
            &ConvoyStatusPatch::QueueMessageFollowUp {
                vessel: "work".into(),
                role: "coder".into(),
                message: Some(ResourceRef::new("flotilla.work/v1", "Message", "flotilla", "not-replicated")),
            },
        )
        .await
        .expect("pending continuation");
        let before = convoys.get("crew").await.expect("before command").status;
        let error = if withdraw {
            crew.withdraw_pending_brief("flotilla", "crew").await.expect_err("withdraw awaits replication")
        } else {
            crew.resume("flotilla", "crew", "replacement", None, None).await.expect_err("resume awaits replication")
        };
        assert!(error.contains("follow-up Message awaits replication"), "{error}");
        assert_eq!(convoys.get("crew").await.expect("unchanged workflow").status, before);
        assert!(backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items.is_empty());
    }
}

#[tokio::test]
async fn replica_withdraw_preserves_receiver_submission_on_race() {
    use crate::leaf_engine::ResourceIntentPublisher;
    struct SubmissionRace {
        receiver: ResourceBackend,
    }
    #[async_trait]
    impl ResourceIntentPublisher for SubmissionRace {
        async fn publish(self: Arc<Self>, _: &str, _: serde_json::Value) -> Result<ResourceRef, String> {
            panic!("withdraw never publishes intent");
        }
        async fn patch_status(
            self: Arc<Self>,
            namespace: &str,
            kind: &str,
            name: &str,
            status: serde_json::Value,
            expected: &str,
        ) -> Result<(), String> {
            let messages = self.receiver.using::<flotilla_resources::Message>(namespace);
            let current = messages.get(name).await.expect("receiver intent");
            let mut next = current.status.expect("receiver status");
            next.submission = Some(
                flotilla_resources::MessageSubmission::builder()
                    .batch_id("racing-batch".into())
                    .crew_id("actual-crew".into())
                    .session("actual-session".into())
                    .started_at(Utc::now())
                    .members(vec![name.into()])
                    .build(),
            );
            messages.update_status(name, &current.metadata.resource_version, &next).await.expect("receiver begins submission");
            flotilla_resources::patch_resource_status_if_version(&self.receiver, namespace, kind, name, status, expected)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
    }
    let (crew, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    let receiver = ResourceBackend::InMemory(InMemoryBackend::default());
    let spec = flotilla_resources::MessageSpec::builder()
        .sender("principal:operator".into())
        .receiver("flotilla/crew/work/coder".into())
        .relation(flotilla_resources::MessageRelation::Supervisor)
        .body("retain this brief".into())
        .build();
    flotilla_resources::MessageInbox::new(receiver.clone(), "flotilla")
        .accept(&InputMeta::builder().name("racing-message".into()).build(), &spec, Utc::now())
        .await
        .expect("receiver admission");
    backend
        .replica_writer::<flotilla_resources::Message>(flotilla_protocol::NodeId::new("receiver"), "flotilla")
        .replace(&receiver.using::<flotilla_resources::Message>("flotilla").list().await.expect("receiver snapshot"), Utc::now())
        .await
        .expect("lagging sender replica");
    let publisher: Arc<dyn ResourceIntentPublisher> = Arc::new(SubmissionRace { receiver: receiver.clone() });
    crew.set_resource_intent_publisher(Arc::downgrade(&publisher));
    let error = crew.withdraw_pending_brief("flotilla", "crew").await.expect_err("stale withdrawal refused");
    assert!(error.contains("status changed"), "{error}");
    assert!(error.contains("retain this brief"), "partial failure retains the body: {error}");
    let retained = receiver.using::<flotilla_resources::Message>("flotilla").get("racing-message").await.expect("retained intent");
    let status = retained.status.expect("receiver evidence");
    assert!(!status.phase.is_terminal());
    assert_eq!(status.submission.expect("submission preserved").session, "actual-session");
}

#[tokio::test]
async fn fresh_idle_admission_keeps_completion_and_delivery_evidence_receiver_owned() {
    for completion_pending in [false, true] {
        for degraded in [false, true] {
            let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Working).await;
            probe.fail.store(false, Ordering::SeqCst);
            let sessions = backend.using::<ResourceTerminalSession>("flotilla");
            let holder = sessions.get("session").await.expect("holder");
            let now = Utc::now();
            let status = flotilla_resources::TerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                session_id: Some("actual-session".into()),
                attention: Some(flotilla_resources::TerminalAttention {
                    state: TerminalAttentionState::Idle,
                    as_of: now,
                    source: flotilla_resources::TerminalAttentionSource::Screen,
                }),
                completion_pending: completion_pending.then(|| CrewCompletionPending {
                    message: Some("previous completion".into()),
                    disposition: None,
                    decision_ledger_ref: None,
                    force: false,
                    principal_ref: None,
                    attempted_at: now,
                    authority: "crew".into(),
                    last_error: "awaiting settlement".into(),
                }),
                degraded: degraded.then(|| flotilla_resources::TerminalSessionDegradedCondition {
                    reason: flotilla_resources::TERMINAL_DELIVERY_UNCONFIRMED_REASON.into(),
                    message: "held earlier input".into(),
                    message_id: Some("earlier".into()),
                    consecutive_failures: 1,
                    observed_at: now,
                }),
                ..Default::default()
            };
            sessions
                .update_status("session", &holder.metadata.resource_version, &status)
                .await
                .expect("fresh idle with legacy obligations");
            crew.resume("flotilla", "crew", "next admitted brief", None, None).await.expect("admit at idle boundary");
            assert_eq!(sessions.get("session").await.expect("unchanged receiver evidence").status, Some(status));
            let records = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
            assert_eq!(records.len(), 1);
            assert!(records[0].status.as_ref().expect("admission status").resolved_receiver.is_none());
            assert_eq!(probe.calls.load(Ordering::SeqCst), 1, "workflow resumes but does not deliver");
        }
    }
}

// A controller replay must keep the original Message identity and settled
// workflow state even after terminal audit retention has removed its body.
#[tokio::test]
async fn turn_admission_replays_compacted_message_without_reopening_work() {
    use flotilla_resources::{Message, MessageInbox, MessagePhase, MessageRelation, MessageSpec};

    use crate::leaf_engine::CrewTurnIntent;
    let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Done).await;
    probe.fail.store(false, Ordering::SeqCst);
    let intent = MessageSpec::builder()
        .sender("system:turn-rules".into())
        .receiver("flotilla/crew/work/coder".into())
        .relation(MessageRelation::System)
        .body("review".into())
        .build();
    let name = flotilla_resources::message_record_name(&intent.receiver, &intent.sender, "turn-delivery:review:revision");
    let inbox = MessageInbox::new(backend.clone(), "flotilla").with_audit_retention_days(1);
    inbox.accept(&InputMeta::builder().name(name.clone()).build(), &intent, Utc::now()).await.unwrap();
    let messages = backend.using::<Message>("flotilla");
    let record = messages.get(&name).await.unwrap();
    let mut status = record.status.unwrap();
    status.phase = MessagePhase::Expired;
    status.since = Utc::now() - chrono::Duration::days(2);
    messages.update_status(&name, &record.metadata.resource_version, &status).await.unwrap();
    assert_eq!(inbox.compact_audit(Utc::now()).await.unwrap(), 1);
    let before = backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status;
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy("crew".into())
        .source("review".into())
        .vessel("work".into())
        .role("coder".into())
        .brief("review".into())
        .subject_revision("revision".into())
        .sender("system:turn-rules".into())
        .build();
    let replay = crew.deliver_turn(&request).await.unwrap();
    assert!(!replay.new_turn);
    assert_eq!(replay.message.name, name);
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").get("crew").await.unwrap().status, before);
}

// A live capability source is queried on every orientation, and changes create
// superseding system Messages without delivering duplicate launch observations.
#[tokio::test]
async fn capabilities_use_live_deliveries_and_supersede_changed_cards() {
    use flotilla_resources::{Environment, EnvironmentSpec, HostDirectEnvironmentSpec, Message, Vessel, VesselSpec};

    use crate::crew_capabilities::{CredentialCapability, SessionCapabilitySource};
    // In-memory port stands in for credential mint/delivery I/O.
    struct Delivery(RwLock<Vec<CredentialCapability>>, RwLock<BTreeMap<String, String>>);
    #[async_trait]
    impl SessionCapabilitySource for Delivery {
        async fn credentials(&self, environment: &str, references: &BTreeSet<String>) -> Result<Vec<CredentialCapability>, String> {
            assert_eq!(environment, "resume-env");
            assert_eq!(references, &BTreeSet::from(["github".into()]));
            Ok(self.0.read().await.clone())
        }
        async fn endpoints(&self, _: &str, _: &str) -> Result<BTreeMap<String, String>, String> {
            Ok(self.1.read().await.clone())
        }
    }
    let (crew, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    backend
        .using::<Vessel>("flotilla")
        .create(
            &InputMeta::builder().name("vessel".into()).build(),
            &VesselSpec {
                convoy_ref: "crew".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("vessel");
    backend
        .using::<Environment>("flotilla")
        .create(
            &InputMeta::builder().name("resume-env".into()).build(),
            &EnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/repo".into() }),
                docker: None,
            },
        )
        .await
        .expect("environment");
    let source = Arc::new(Delivery(
        RwLock::new(vec![CredentialCapability::builder()
            .name("github".into())
            .repositories(vec!["flotilla-org/flotilla".into()])
            .permissions(BTreeMap::from([("workflows".into(), "write".into()), ("contents".into(), "write".into())]))
            .build()]),
        RwLock::new(BTreeMap::new()),
    ));
    *crew.capability_source.write().await = Some(source.clone());
    let context = CrewCommandContext::builder()
        .namespace("flotilla".into())
        .convoy("crew".into())
        .vessel_ref("vessel".into())
        .role("coder".into())
        .build();
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("session").await.expect("session");
    let mut meta = InputMeta::from(&session.metadata);
    meta.annotations.insert(CREDENTIAL_REFS_ANNOTATION.into(), "[\"github\"]".into());
    sessions.update(&meta, &session.metadata.resource_version, &session.spec).await.expect("session grant references");
    let healthy = sessions.get("session").await.expect("healthy session");
    let mut status = healthy.status.clone().unwrap_or_default();
    status.phase = ResourceTerminalSessionPhase::Running;
    sessions.update_status("session", &healthy.metadata.resource_version, &status).await.expect("running healthy session");
    // A malformed running session sorts before the healthy session and must
    // not starve it of observations. Its different role cannot receive this card.
    let mut broken_spec = healthy.spec.clone();
    broken_spec.role = "broken".into();
    let broken = sessions
        .create(
            &InputMeta::builder()
                .name("000-malformed".into())
                .annotations(BTreeMap::from([(CREDENTIAL_REFS_ANNOTATION.into(), "not json".into())]))
                .build(),
            &broken_spec,
        )
        .await
        .expect("broken session");
    sessions.update_status("000-malformed", &broken.metadata.resource_version, &status).await.expect("running broken session");
    let first = crew.crew_capabilities_internal(&context).await.expect("first live card");
    assert!(first.contains("You can push `.github/workflows`"));
    let session = sessions.get("session").await.expect("session");
    crate::crew_capabilities::observe_card(&backend, "flotilla", &session, &first).await.expect("launch baseline");
    assert!(backend.using::<Message>("flotilla").list().await.expect("messages").items.is_empty());
    for (index, permissions) in [BTreeMap::from([("contents".into(), "read".into())]), BTreeMap::new()].into_iter().enumerate() {
        source.0.write().await[0].permissions = Some(permissions);
        let card = crew.crew_capabilities_internal(&context).await.expect("refreshed card");
        assert!(!card.contains("You can push `.github/workflows`"));
        let session = sessions.get("session").await.expect("session");
        if index == 0 {
            // A concurrent holder status write can race successful publication.
            // Change the source again before retrying: recover the published
            // revision, then supersede it rather than collide or deliver twice.
            let mut status = session.status.clone().unwrap_or_default();
            status.session_id = Some("concurrent-terminal-observation".into());
            sessions.update_status("session", &session.metadata.resource_version, &status).await.expect("concurrent status");
            assert!(crate::crew_capabilities::observe_card(&backend, "flotilla", &session, &card).await.is_err());
            continue;
        }
        let error = crate::crew_capabilities::refresh_cards(&backend, "flotilla", source.as_ref()).await.expect_err("aggregate error");
        assert!(error.contains("000-malformed"));
        assert!(error.contains("invalid session credential references"));
        assert_eq!(
            sessions.get("session").await.expect("healthy refresh").metadata.annotations["flotilla.work/capabilities-revision"],
            "2"
        );
        let session = sessions.get("session").await.expect("session");
        crate::crew_capabilities::observe_card(&backend, "flotilla", &session, &card).await.expect("supersede recovered revision");
        crate::crew_capabilities::observe_card(&backend, "flotilla", &sessions.get("session").await.expect("session"), &card)
            .await
            .expect("duplicate observation");
    }
    source.1.write().await.insert("pinned service".into(), "127.0.0.1:7423".into());
    let card = crew.crew_capabilities_internal(&context).await.expect("endpoint change");
    assert!(card.contains("`127.0.0.1:7423` → pinned service"));
    crate::crew_capabilities::observe_card(&backend, "flotilla", &sessions.get("session").await.expect("session"), &card)
        .await
        .expect("environment notification");
    source.0.write().await.clear();
    let card = crew.crew_capabilities_internal(&context).await.expect("revoked credential");
    assert!(!card.contains("Credential `github`"));
    crate::crew_capabilities::observe_card(&backend, "flotilla", &sessions.get("session").await.expect("session"), &card)
        .await
        .expect("revocation notification");
    let messages = backend.using::<Message>("flotilla").list().await.expect("messages").items;
    assert_eq!(messages.len(), 4);
    let latest = messages.iter().find(|message| message.spec.body == card).expect("superseding card");
    assert!(messages.iter().any(|message| Some(&message.metadata.name) == latest.spec.supersedes.as_ref()));
    assert_eq!(latest.spec.sender, "system:capabilities");
    assert_eq!(latest.spec.body, crew.crew_capabilities_internal(&context).await.expect("live card"));
    // Terminal transport is the I/O boundary. A busy holder gets no input;
    // after its turn boundary only the latest card is submitted, once.
    struct Transport {
        working: AtomicBool,
        submitted: Mutex<Vec<flotilla_resources::MessageBatch>>,
    }
    #[async_trait]
    impl flotilla_resources::MessageTransport for Transport {
        async fn observe(
            &self,
            _: &ResourceObject<ResourceTerminalSession>,
            _: Option<&flotilla_resources::MessageSubmission>,
        ) -> Result<flotilla_resources::MessageObservation, String> {
            let working = self.working.load(Ordering::SeqCst);
            Ok(flotilla_resources::MessageObservation {
                ready: !working,
                working,
                evidence: Some("terminal boundary".into()),
                ..Default::default()
            })
        }
        async fn submit(&self, batch: &flotilla_resources::MessageBatch) -> flotilla_resources::MessageTransportOutcome {
            self.submitted.lock().await.push(batch.clone());
            flotilla_resources::MessageTransportOutcome::Accepted { evidence: "accepted".into() }
        }
        async fn poll(&self, _: &flotilla_resources::MessageBatch) -> flotilla_resources::MessageTransportOutcome {
            flotilla_resources::MessageTransportOutcome::Accepted { evidence: "accepted".into() }
        }
    }
    let session = sessions.get("session").await.expect("session");
    sessions
        .update_status(
            "session",
            &session.metadata.resource_version,
            &flotilla_resources::TerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                session_id: Some("terminal".into()),
                crew: Some(
                    flotilla_resources::CrewSessionStatus::builder()
                        .id("crew-id".into())
                        .adapter("codex".into())
                        .stance("trusted".into())
                        .build(),
                ),
                ..Default::default()
            },
        )
        .await
        .expect("running holder");
    let transport = Transport { working: AtomicBool::new(true), submitted: Mutex::new(Vec::new()) };
    let inbox = flotilla_resources::MessageInbox::new(backend, "flotilla");
    inbox.reconcile_delivery(&transport, Utc::now()).await.expect("busy holder");
    assert!(transport.submitted.lock().await.is_empty());
    transport.working.store(false, Ordering::SeqCst);
    inbox.reconcile_delivery(&transport, Utc::now()).await.expect("turn boundary");
    inbox.reconcile_delivery(&transport, Utc::now()).await.expect("idempotent receipt");
    let submitted = transport.submitted.lock().await;
    assert_eq!(submitted.len(), 1);
    assert!(submitted[0].text.contains(&card));
    assert!(submitted[0].text.contains("capabilities@5"));
}

// Handoffs route to the addressed vessel, preserve typed carries, and admit once per command.
// Generated scenarios cover local and remote receiver homes, latent and completed work,
// duplicate text, and invalid targets. The receiver has no terminal in either initial phase.
#[hegel::test]
fn cross_vessel_handoff_publishes_typed_messages(tc: hegel::TestCase) {
    use flotilla_resources::{Message, MessageInbox, MessageReference, MessageRelation, MessageSpec, VesselSpec};
    use hegel::generators as gs;

    use crate::leaf_engine::ResourceIntentPublisher;
    let remote = tc.draw(gs::booleans());
    let publication_fails = tc.draw(gs::booleans());
    let receiver_phase = if tc.draw(gs::booleans()) { CrewWorkPhase::Done } else { CrewWorkPhase::Pending };
    let repeats = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
    // Stand-in for the cross-host resource mutation endpoint; admission uses the real inbox.
    struct ReceiverHome {
        inbox: MessageInbox,
        fail: bool,
    }
    #[async_trait]
    impl ResourceIntentPublisher for ReceiverHome {
        async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<ResourceRef, String> {
            if self.fail {
                return Err("receiver publication unavailable".into());
            }
            assert_eq!(document["kind"], "Message");
            let spec: MessageSpec = serde_json::from_value(document["spec"].clone()).expect("Message intent");
            let admission = self
                .inbox
                .accept(
                    &InputMeta::builder().name(document["metadata"]["name"].as_str().expect("message name").into()).build(),
                    &spec,
                    Utc::now(),
                )
                .await
                .map_err(|error| error.to_string())?;
            let record = match admission {
                flotilla_resources::MessageAdmission::Accepted(record) => record,
                flotilla_resources::MessageAdmission::Suppressed { predecessor } => predecessor,
            };
            Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, record.metadata.name))
        }
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Working).await;
        probe.fail.store(false, Ordering::SeqCst);
        backend
            .using::<Vessel>("flotilla")
            .create(
                &InputMeta::builder().name("vessel".into()).build(),
                &VesselSpec {
                    convoy_ref: "crew".into(),
                    vessel_name: "work".into(),
                    placement_policy_ref: "test".into(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
            )
            .await
            .expect("caller vessel");
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        let convoy = convoys.get("crew").await.expect("convoy");
        let mut status = convoy.status.clone().expect("status");
        let mut receiver_vessel = status.workflow_snapshot.as_ref().expect("snapshot").vessels[0].clone();
        receiver_vessel.name = "review".into();
        receiver_vessel.crew[0].role = "reviewer".into();
        status.workflow_snapshot.as_mut().expect("snapshot").vessels.push(receiver_vessel);
        status
            .crew_work
            .insert("review".into(), BTreeMap::from([("reviewer".into(), CrewWorkState::builder().phase(receiver_phase).build())]));
        convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("second vessel");
        let home = if remote { ResourceBackend::InMemory(Default::default()) } else { backend.clone() };
        let publisher: Arc<dyn ResourceIntentPublisher> =
            Arc::new(ReceiverHome { inbox: MessageInbox::new(home.clone(), "flotilla"), fail: publication_fails });
        if remote || publication_fails {
            crew.set_resource_intent_publisher(Arc::downgrade(&publisher));
        }
        let context = CrewCommandContext::builder().convoy("crew".into()).vessel_ref("vessel".into()).role("coder".into()).build();
        let carry = MessageReference::ControlRecord {
            resource: ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "crew"),
            revision: convoy.metadata.resource_version.clone(),
        };
        if publication_fails {
            let before = convoys.get("crew").await.expect("before handoff").status;
            for _ in 0..repeats {
                crew.handoff_with_carries(&context, "crew/review/reviewer", "review this", vec![carry.clone()])
                    .await
                    .expect_err("receiver publication fails");
                assert_eq!(convoys.get("crew").await.expect("restored authority").status, before);
                assert!(home.using::<Message>("flotilla").list().await.expect("inbox").items.is_empty());
            }
            return;
        }
        for index in 0..repeats {
            let target = if index % 2 == 0 { "crew/review/reviewer" } else { "flotilla/crew/review/reviewer" };
            crew.handoff_with_carries(&context, target, "review this", vec![carry.clone()]).await.expect("handoff");
            assert_eq!(
                convoys.get("crew").await.expect("activated convoy").status.expect("status").crew_work["review"]["reviewer"].phase,
                CrewWorkPhase::Working
            );
            let records = home.using::<Message>("flotilla").list().await.expect("receiver inbox").items;
            assert_eq!(records.len(), index + 1, "each handoff is distinct even when its body repeats");
            for record in records {
                assert_eq!(record.spec.sender, "flotilla/crew/work/coder");
                assert_eq!(record.spec.receiver, "flotilla/crew/review/reviewer");
                assert_eq!(record.spec.relation, MessageRelation::Peer);
                assert_eq!(record.spec.references, vec![carry.clone()]);
                assert_eq!(record.spec.body, "review this");
            }
        }
        for invalid in ["crew/missing/reviewer", "other/review/reviewer", "principal:operator", "coder"] {
            crew.handoff(&context, invalid, "review").await.expect_err("invalid crew target");
        }
        crew.handoff(&context, "crew/review/reviewer", "").await.expect_err("empty handoff");
        assert_eq!(home.using::<Message>("flotilla").list().await.expect("inbox").items.len(), repeats);
    });
}

// An authenticated operator can send supervisor guidance to crew that has no stall.
#[tokio::test]
async fn operator_supervision_messages_working_crew() {
    let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
    let principal = PrincipalRef { namespace: "flotilla".into(), name: "operator-alice".into() };
    crew.supervise(CrewSupervisionRequest {
        namespace: "flotilla",
        convoy_name: "crew",
        vessel: "work",
        role: "coder",
        operation: flotilla_protocol::CrewSupervisionAction::Resume,
        message: "please follow up",
        actor_crew_id: None,
        principal: Some(&principal),
    })
    .await
    .expect("operator guidance");
    let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.sender, "principal:operator-alice");
    assert_eq!(messages[0].spec.receiver, "flotilla/crew/work/coder");
    assert_eq!(messages[0].spec.relation, flotilla_resources::MessageRelation::Supervisor);
    assert_eq!(messages[0].spec.body, "please follow up");
    assert!(backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("convoy").status.expect("status").stalled.is_none());
}

// Legacy PendingBrief adoption is replay-safe and retains both attribution and continuation intent.
#[tokio::test]
async fn legacy_pending_brief_is_adopted_once_before_withdrawal() {
    let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys.get("crew").await.expect("convoy");
    let mut status = convoy.status.expect("status");
    // Decode the exact previous-generation record rather than invoking a retired producer.
    status.turn_deliveries.entry("operator".into()).or_default().pending_brief = Some(
        serde_json::from_value(serde_json::json!({
            "vessel": "work", "role": "coder", "content": "legacy follow-up", "queued_at": "2026-10-01T00:00:00Z",
            "sender": {"kind": "operator-resume", "principal": {"namespace": "flotilla", "name": "alice"}}
        }))
        .expect("legacy PendingBrief"),
    );
    convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("stored legacy queue");
    for _ in 0..2 {
        crew.reconcile_pending_supervisor_turns_once("flotilla").await.expect("adoption");
    }
    let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].spec.sender, "principal:alice");
    assert_eq!(messages[0].spec.body, "legacy follow-up");
    let status = convoys.get("crew").await.expect("convoy").status.expect("status");
    assert!(status.pending_brief().is_none());
    assert_eq!(status.crew_work["work"]["coder"].pending_follow_up.as_ref().expect("continuation").name, messages[0].metadata.name);
    assert_eq!(crew.withdraw_pending_brief("flotilla", "crew").await.expect("withdraw adopted Message"), Some("legacy follow-up".into()));
}

// Every governor ruling is a Message reply to the exact crew's open escalation.
#[hegel::test]
fn governor_rulings_reply_to_the_source_escalation(tc: hegel::TestCase) {
    use flotilla_protocol::CrewSupervisionAction;
    use flotilla_resources::{
        Message, MessageExpectation, MessageInbox, MessageReference, MessageRelation, MessageSpec, StallRung, StallSupervisor,
    };
    use hegel::generators as gs;
    let escalation_visible = tc.draw(gs::booleans());
    // Exercise resume, conversion to failed, and escalation, including their distinct workflow effects.
    let action = match tc.draw(gs::integers::<usize>().min_value(0).max_value(2)) {
        0 => CrewSupervisionAction::Resume,
        1 => CrewSupervisionAction::Fail,
        _ => CrewSupervisionAction::Escalate,
    };
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let (crew, backend, probe, _config) = fixture(CrewWorkPhase::Working).await;
        probe.fail.store(false, Ordering::SeqCst);
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        apply_resource_status_patch(
            &convoys,
            "crew",
            &convoy_external_patches::mark_crew_stalled(
                "crew".into(),
                "work".into(),
                "coder".into(),
                Utc::now(),
                flotilla_protocol::StallReason::Scope,
                None,
                "blocked".into(),
            ),
        )
        .await
        .expect("declare stall");
        let convoy = convoys.get("crew").await.expect("source convoy");
        let mut status = convoy.status.expect("source status");
        let stalled = status.stalled.as_mut().expect("stall");
        stalled.rung = StallRung::Governor;
        stalled.supervisor = Some(StallSupervisor { convoy: "governor".into(), vessel: "watch".into(), role: "governor".into() });
        convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("supervisor ownership");
        convoys
            .create(
                &InputMeta::builder().name("governor".into()).build(),
                &ConvoySpec::builder().workflow_ref("workflow".into()).role("governor".into()).build(),
            )
            .await
            .expect("governor convoy");
        let sessions = backend.using::<ResourceTerminalSession>("flotilla");
        let source_session = sessions.get("session").await.expect("source session");
        let mut spec = source_session.spec;
        spec.role = "governor".into();
        let governor_session = sessions
            .create(
                &InputMeta::builder()
                    .name("governor-session".into())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.into(), "governor".into()),
                        (VESSEL_LABEL.into(), "watch".into()),
                        (ROLE_LABEL.into(), "governor".into()),
                    ]))
                    .build(),
                &spec,
            )
            .await
            .expect("governor session");
        sessions
            .update_status(
                "governor-session",
                &governor_session.metadata.resource_version,
                &flotilla_resources::TerminalSessionStatus {
                    crew: Some(flotilla_resources::CrewSessionStatus {
                        id: "governor-id".into(),
                        adapter: "codex".into(),
                        model: None,
                        stance: "trusted-implicit".into(),
                    }),
                    ..Default::default()
                },
            )
            .await
            .expect("supervisor identity");
        let escalation = MessageSpec::builder()
            .sender("flotilla/crew/work/coder".into())
            .receiver("flotilla/governor/watch/governor".into())
            .relation(MessageRelation::Supervisor)
            .body("blocked".into())
            .expectation(MessageExpectation::Reply)
            .references(vec![MessageReference::ControlRecord {
                resource: ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "crew"),
                revision: convoy.metadata.resource_version,
            }])
            .build();
        if escalation_visible {
            MessageInbox::new(backend.clone(), "flotilla")
                .accept(&InputMeta::builder().name("source-escalation".into()).build(), &escalation, Utc::now())
                .await
                .expect("escalation");
        }
        // No visible escalation and no supervision index must produce an unlinked ruling,
        // rather than inventing a record ID.
        crew.supervise(CrewSupervisionRequest {
            namespace: "flotilla",
            convoy_name: "crew",
            vessel: "work",
            role: "coder",
            operation: action,
            message: "ruling",
            actor_crew_id: Some("governor-id"),
            principal: None,
        })
        .await
        .expect("governor ruling");
        let records = backend.using::<Message>("flotilla").list().await.expect("messages").items;
        assert_eq!(records.len(), 1 + usize::from(escalation_visible));
        let reply = records.iter().find(|record| record.metadata.name != "source-escalation").expect("ruling Message");
        assert_eq!(reply.spec.sender, "flotilla/governor/watch/governor");
        assert_eq!(reply.spec.receiver, "flotilla/crew/work/coder");
        assert_eq!(reply.spec.in_reply_to.as_deref(), escalation_visible.then_some("source-escalation"));
        assert_eq!(reply.spec.relation, MessageRelation::Supervisor);
        let status = convoys.get("crew").await.expect("convoy").status.expect("status");
        match action {
            CrewSupervisionAction::Resume => assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working),
            CrewSupervisionAction::Fail => assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Failed),
            CrewSupervisionAction::Escalate => assert_eq!(status.stalled.expect("escalated stall").rung, StallRung::Operator),
        }
    });
}

// A durable ruling survives an authority mutation failure as a notification;
// Message delivery never applies Fail/Escalate workflow transitions itself.
#[tokio::test]
async fn supervisor_decision_survives_authority_disappearing_after_publication() {
    use flotilla_resources::{Message, MessageInbox, MessageSpec};

    use crate::leaf_engine::ResourceIntentPublisher;
    // Boundary stand-in: the receiver admits intent, then a concurrent authority
    // teardown removes the convoy before the supervisor's state patch can read it.
    struct ReceiverAndTeardown(ResourceBackend);
    #[async_trait]
    impl ResourceIntentPublisher for ReceiverAndTeardown {
        async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<ResourceRef, String> {
            let name = document["metadata"]["name"].as_str().expect("name").to_string();
            let spec: MessageSpec = serde_json::from_value(document["spec"].clone()).expect("intent");
            MessageInbox::new(self.0.clone(), namespace)
                .accept(&InputMeta::builder().name(name.clone()).build(), &spec, Utc::now())
                .await
                .map_err(|error| error.to_string())?;
            self.0.using::<ResourceConvoy>(namespace).delete("crew").await.map_err(|error| error.to_string())?;
            Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, name))
        }
    }
    for action in [flotilla_protocol::CrewSupervisionAction::Fail, flotilla_protocol::CrewSupervisionAction::Escalate] {
        let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
        apply_resource_status_patch(
            &backend.using::<ResourceConvoy>("flotilla"),
            "crew",
            &convoy_external_patches::mark_crew_stalled(
                "crew".into(),
                "work".into(),
                "coder".into(),
                Utc::now(),
                flotilla_protocol::StallReason::Scope,
                None,
                "blocked".into(),
            ),
        )
        .await
        .expect("stall");
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        let convoy = convoys.get("crew").await.expect("source");
        let mut status = convoy.status.expect("status");
        status.stalled.as_mut().expect("stall").rung = flotilla_resources::StallRung::Governor;
        convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("governor rung");
        let publisher: Arc<dyn ResourceIntentPublisher> = Arc::new(ReceiverAndTeardown(backend.clone()));
        crew.set_resource_intent_publisher(Arc::downgrade(&publisher));
        let principal = PrincipalRef::implicit_for_namespace("flotilla");
        crew.supervise(CrewSupervisionRequest {
            namespace: "flotilla",
            convoy_name: "crew",
            vessel: "work",
            role: "coder",
            operation: action,
            message: "durable ruling",
            actor_crew_id: None,
            principal: Some(&principal),
        })
        .await
        .expect_err("authority patch cannot read removed convoy");
        let records = backend.using::<Message>("flotilla").list().await.expect("decisions").items;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].spec.body, "durable ruling");
        assert_eq!(records[0].spec.receiver, "flotilla/crew/work/coder");
        assert_eq!(records[0].spec.expectation, flotilla_resources::MessageExpectation::None);
    }
}

// #2758: holds persist convoy/session facts and admit exactly one plain supervisor
// Message across retries, with no forge collaborator available. Cover local and project
// supervisors, no PR binding, and retries carrying newer head identities.
#[hegel::test]
fn hold_state_and_single_supervisor_message(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let local_supervisor = tc.draw(gs::booleans());
    let address_supervisor = tc.draw(gs::booleans());
    let retries = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let (crew, backend, _, _dir) = fixture(CrewWorkPhase::Done).await;
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        if local_supervisor {
            let current = convoys.get("crew").await.expect("convoy");
            let mut status = current.status.expect("status");
            status.crew_work.get_mut("work").expect("work").insert("bosun".into(),
                CrewWorkState::builder().phase(CrewWorkPhase::Working).build());
            convoys.update_status("crew", &current.metadata.resource_version, &status).await.expect("supervisor");
        }
        if address_supervisor {
            let current = convoys.get("crew").await.expect("convoy");
            let mut status = current.status.expect("status");
            status.workflow_snapshot.as_mut().expect("workflow").supervision = Some(vec![
                flotilla_resources::SupervisionTarget::Address { address: "flotilla/attended".into() },
            ]);
            convoys.update_status("crew", &current.metadata.resource_version, &status).await.expect("address policy");
        }
        let mut request = crate::leaf_engine::CrewTurnIntent::builder()
            .namespace("flotilla".into()).convoy("crew".into()).source("checks-settled".into())
            .vessel("work".into()).role("coder".into()).brief("continue".into())
            .subject_revision("head-one".into()).sender("system:turn-rules".into()).build();
        for retry in 0..retries {
            request.subject_revision = format!("head-{retry}");
            crew.execute_turn_delivery_hold(&request, &HoldAct::State, "episode limit").await.expect("hold");
        }
        let convoy = convoys.get("crew").await.expect("convoy");
        assert_eq!(convoy.status.expect("status").attention.expect("attention").reason, "episode limit");
        let session = backend.using::<ResourceTerminalSession>("flotilla").get("session").await.expect("session");
        assert_eq!(session.status.expect("status").turn_delivery_hold.expect("hold").reason, "episode limit");
        let messages = backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("messages");
        assert_eq!(messages.items.len(), 1, "retries cannot duplicate supervisor signals");
        let message = &messages.items[0];
        assert_eq!(message.spec.receiver, if address_supervisor { "flotilla/attended" } else if local_supervisor { "flotilla/crew/work/bosun" } else { "flotilla/governor" });
        assert_eq!(message.spec.relation, flotilla_resources::MessageRelation::System);
        assert_eq!(message.spec.expectation, flotilla_resources::MessageExpectation::None);
        assert!(message.spec.body.contains("episode limit"));
        assert!(matches!(message.spec.references.last(), Some(flotilla_resources::MessageReference::ControlRecord { resource, .. }) if resource.name == "crew"));
    });
}

// #2758: the convoy keeps its hold through a receiver-home outage; session state
// is patched at its authority before the single supervisor signal is published.
#[tokio::test]
async fn remote_session_hold_retries_through_existing_mutation_router() {
    use crate::leaf_engine::{CrewTurnIntent, ResourceIntentPublisher};
    // Stand-in for the inter-host command channel; both endpoint stores and inbox
    // admission are real in-memory collaborators, including version validation.
    struct Router {
        source: ResourceBackend,
        receiver: ResourceBackend,
        fail: AtomicBool,
    }
    #[async_trait]
    impl ResourceIntentPublisher for Router {
        async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<ResourceRef, String> {
            let spec: flotilla_resources::MessageSpec = serde_json::from_value(document["spec"].clone()).expect("Message intent");
            let name = document["metadata"]["name"].as_str().expect("Message name");
            flotilla_resources::MessageInbox::new(self.source.clone(), namespace)
                .accept(&InputMeta::builder().name(name.into()).build(), &spec, Utc::now())
                .await
                .map_err(|error| error.to_string())?;
            Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, name))
        }
        async fn patch_status(
            self: Arc<Self>,
            namespace: &str,
            kind: &str,
            name: &str,
            status: serde_json::Value,
            expected: &str,
        ) -> Result<(), String> {
            assert_eq!(kind, "TerminalSession");
            if self.fail.swap(false, Ordering::SeqCst) {
                return Err("receiver unavailable".into());
            }
            flotilla_resources::patch_resource_status_if_version(&self.receiver, namespace, kind, name, status, expected)
                .await
                .map_err(|error| error.to_string())?;
            self.source
                .replica_writer::<ResourceTerminalSession>(flotilla_protocol::NodeId::new("receiver"), namespace)
                .replace(
                    &self.receiver.using::<ResourceTerminalSession>(namespace).list().await.map_err(|error| error.to_string())?,
                    Utc::now(),
                )
                .await
                .map_err(|error| error.to_string())
        }
    }
    let (crew, backend, _, _dir) = fixture(CrewWorkPhase::Done).await;
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let source = sessions.get("session").await.expect("source session");
    let receiver = ResourceBackend::InMemory(InMemoryBackend::default());
    let remote = receiver
        .using::<ResourceTerminalSession>("flotilla")
        .create(&InputMeta::from(&source.metadata), &source.spec)
        .await
        .expect("remote session");
    receiver
        .using::<ResourceTerminalSession>("flotilla")
        .update_status("session", &remote.metadata.resource_version, &source.status.clone().unwrap_or_default())
        .await
        .expect("remote status");
    sessions.delete("session").await.expect("retire local record");
    backend
        .replica_writer::<ResourceTerminalSession>(flotilla_protocol::NodeId::new("receiver"), "flotilla")
        .replace(&receiver.using::<ResourceTerminalSession>("flotilla").list().await.expect("remote snapshot"), Utc::now())
        .await
        .expect("replica");
    let router: Arc<dyn ResourceIntentPublisher> =
        Arc::new(Router { source: backend.clone(), receiver: receiver.clone(), fail: AtomicBool::new(true) });
    crew.set_resource_intent_publisher(Arc::downgrade(&router));
    let request = CrewTurnIntent::builder()
        .namespace("flotilla".into())
        .convoy("crew".into())
        .source("checks-settled".into())
        .vessel("work".into())
        .role("coder".into())
        .brief("continue".into())
        .subject_revision("head".into())
        .sender("system:turn-rules".into())
        .build();
    assert_eq!(crew.execute_turn_delivery_hold(&request, &HoldAct::State, "limit").await.expect_err("outage"), "receiver unavailable");
    let held = backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("convoy");
    let raised_at = held.status.expect("status").turn_delivery_holds()[0].raised_at;
    assert!(backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items.is_empty());
    for _ in 0..2 {
        crew.execute_turn_delivery_hold(&request, &HoldAct::State, "limit").await.expect("retry hold");
    }
    let session = receiver.using::<ResourceTerminalSession>("flotilla").get("session").await.expect("session");
    assert_eq!(session.status.expect("status").turn_delivery_hold.expect("hold").raised_at, raised_at);
    assert_eq!(backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("inbox").items.len(), 1);
}
