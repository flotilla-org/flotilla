use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_change_request::observation::ChangeRequestKey;
use flotilla_change_request::observation::ChangeRequestObservationSource;
use flotilla_config::ConfigStore;
use flotilla_protocol::{EnvironmentId, HostName};
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::{
    ChangeRequestStatus, Convoy as ResourceConvoy, ConvoySpec, ConvoyStatus, CrewSource, CrewWorkPhase, CrewWorkState, InputMeta, Selector,
    SystemClock, TerminalBrief, TerminalCrewContext, TerminalSession as ResourceTerminalSession, TerminalSessionSource,
    TerminalSessionSpec, WorkState, WorkflowSnapshot, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_testkit::ReadCountsBackendExt;
use flotilla_vcs::vcs::{CheckoutVcsResolver, Vcs};
use tokio::sync::RwLock;

use crate::crew_ops::frame_crew_message;
use crate::crew_ops::CrewService;
use crate::leaf_engine::LeafSubscriptionTable;
use crate::ports::WorkCredentialReconciler;
use flotilla_observation::change_request_observer::ChangeRequestRefreshCadence;
use flotilla_observation::change_request_observer::ChangeRequestRefresher;
use flotilla_protocol_testkit::event_sink::RecordingEventSink;
use flotilla_resources::ConvoyPhase;
use flotilla_resources::CrewMessageSender;

// Stand-in for forge I/O; these crew scenarios never request forge observations.
struct UnusedForge;

#[async_trait]
impl ChangeRequestObservationSource for UnusedForge {
    async fn observe(&self, _: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
        panic!("crew delivery must not query the forge")
    }
}

// These crew fixtures own no Checkouts, so no scenario resolves a checkout VCS.
struct NoCheckoutVcs;

#[async_trait]
impl CheckoutVcsResolver for NoCheckoutVcs {
    async fn vcs_for(&self, _: Option<&EnvironmentId>, path: &std::path::Path) -> Result<Arc<dyn Vcs>, String> {
        Err(format!("crew fixture has no checkout VCS for {}", path.display()))
    }

    async fn vcs_for_env_ref(&self, _: &str, path: &std::path::Path) -> Result<Arc<dyn Vcs>, String> {
        self.vcs_for(None, path).await
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
    fixture_with_message_path(phase, Default::default()).await
}

/// [`fixture`], with the `[primitives]` Message path the crew service and its
/// leaf subscriptions share, as the composition root wires them.
async fn fixture_with_message_path(
    phase: CrewWorkPhase,
    message_path: crate::ports::MessagePath,
) -> (Arc<CrewService>, ResourceBackend, Arc<StagingProbe>, tempfile::TempDir) {
    let backend = ResourceBackend::new(InMemoryBackend::default().with_read_counts());
    let config_dir = tempfile::tempdir().expect("config directory");
    let config = Arc::new(ConfigStore::with_base(config_dir.path()));
    let environment = EnvironmentId::new("local");
    let namespace = Arc::new(std::sync::RwLock::new("flotilla".into()));
    let refresher = ChangeRequestRefresher::new(
        "host".into(),
        backend.clone(),
        "host".into(),
        Arc::new(UnusedForge),
        ChangeRequestRefreshCadence::default(),
    );
    let subscriptions = LeafSubscriptionTable::new(backend.clone(), Arc::new(RecordingEventSink::default()), refresher)
        .with_message_path(message_path.clone());
    let probe = Arc::new(StagingProbe { backend: backend.clone(), fail: AtomicBool::new(true), calls: AtomicUsize::new(0) });
    let reconciler: Arc<dyn WorkCredentialReconciler> = probe.clone();
    let crew = Arc::new(
        CrewService::builder()
            .resource_backend(backend.clone())
            .accounts(Arc::new(flotilla_store::OldStoreAccounts::new(backend.clone())))
            .leaf_subscriptions(subscriptions)
            .work_credential_reconciler(RwLock::new(Some(reconciler)))
            .clock(Arc::new(SystemClock))
            .provisioning_namespace(namespace)
            .config(config)
            .host_name(HostName::new("host"))
            .brief_artifact_writer(Arc::new(RwLock::new(None)))
            .checkout_vcs(Arc::new(NoCheckoutVcs))
            .local_environment_id(environment)
            .message_path(message_path)
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

mod awaiting;
mod capabilities;
mod delivery;
mod handler_contract;
mod message_admission;
mod orientation;
mod sender_contract;
mod supervision;

mod read_queries;

// Retained terminal history must not be decoded during legacy adoption.
#[tokio::test]
async fn legacy_adoption_reads_only_live_convoys() {
    use flotilla_store_testkit::{measure_reads, ReadOperation};
    let (crew, backend, _, _temp) = fixture(CrewWorkPhase::Working).await;

    flotilla_store_testkit::fixtures::seed_convoy_history()
        .backend(&backend)
        .namespace("flotilla")
        .landed(300)
        .active(10)
        .call()
        .await
        .expect("convoy history");

    let (result, reads) = measure_reads(crew.adopt_legacy_convoy_queues_once("flotilla", None)).await;
    result.expect("adopt");
    reads.of(&backend).assert_no_lists("legacy adoption");
    // Two queries each read the 10 Active rows plus the fixture's live "crew" convoy.
    assert_eq!(reads.of(&backend).objects("Convoy", ReadOperation::Query), 22);
}

// Resume uses the held Environment's ending and cause even when every old phase disagrees.
// Unheld resources preserve the legacy refusal; requested, retrying and realised resources may resume.
#[tokio::test]
async fn resume_reads_environment_lifecycle_without_a_copy() {
    use flotilla_provisioning::engine::environment::EnvironmentBinding;
    use flotilla_provisioning_testkit::{object, store_object, Ending, LifecycleFixture};
    use flotilla_resources::{
        Environment, EnvironmentPhase, EnvironmentSpec, EnvironmentStatus, HostDirectEnvironmentSpec, Vessel, VesselSpec, VesselStatus,
    };
    let (mut crew, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    let env = store_object(
        &backend,
        object::<Environment>(
            "env",
            EnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/work".into() }),
                ..Default::default()
            },
            EnvironmentStatus { phase: EnvironmentPhase::Ready, message: Some("stale".into()), ..Default::default() },
        ),
    )
    .await;
    store_object(
        &backend,
        object::<Vessel>(
            &flotilla_resources::vessel_resource_name("crew", "work"),
            VesselSpec {
                convoy_ref: "crew".into(),
                vessel_name: "work".into(),
                fulfilment_kind_ref: "policy".into(),
                adopted_checkout_refs: BTreeMap::new(),
            },
            VesselStatus { environment_ref: Some("env".into()), ..Default::default() },
        ),
    )
    .await;
    let lifecycle = LifecycleFixture::<EnvironmentBinding>::new(false);
    Arc::get_mut(&mut crew).expect("unique service").lifecycle_reads = lifecycle.reads.clone();
    lifecycle.request(&env);
    crew.ensure_vessel_backing_available("flotilla", "crew", "work").await.expect("requested may resume");
    lifecycle.finish(&env, Ending::Ready("/work"));
    crew.ensure_vessel_backing_available("flotilla", "crew", "work").await.expect("ready may resume");
    lifecycle.finish(&env, Ending::Lost("/work"));
    let error = crew.ensure_vessel_backing_available("flotilla", "crew", "work").await.expect_err("lost cannot resume");
    assert!(error.contains("lost, recoverable") && !error.contains("stale"));
    lifecycle.poison();
    assert!(crew.ensure_vessel_backing_available("flotilla", "crew", "work").await.expect_err("checked read").contains("poisoned"));
}

// Teardown reads Checkout endings from the engine, including the selected replica's origin.
// Stale Gone must not authorize deletion of a live checkout; every old phase is left unchanged.
#[tokio::test]
async fn teardown_reads_checkout_endings_without_a_copy() {
    use flotilla_provisioning::engine::checkout::CheckoutBinding;
    use flotilla_resources::{Checkout, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutWorktreeSpec, RepositoryKey};

    for replica in [false, true] {
        let old_phase = CheckoutPhase::Gone;
        {
            for ending in ["released", "lost", "gave_up"] {
                let (mut crew, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
                let fixture = flotilla_provisioning_testkit::LifecycleFixture::<CheckoutBinding>::new(replica);

                Arc::get_mut(&mut crew).expect("unique service").lifecycle_reads = fixture.reads.clone();
                let author = if replica { ResourceBackend::new(InMemoryBackend::default()) } else { backend.clone() };
                let records = author.using::<Checkout>("flotilla");
                let mut checkout = records
                    .create(
                        &InputMeta::builder()
                            .name("checkout".into())
                            .labels(BTreeMap::from([(CONVOY_LABEL.into(), "crew".into())]))
                            .build(),
                        &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                            repo_ref: RepositoryKey("github.com/org/repo".into()),
                            env_ref: "env".into(),
                            r#ref: "branch".into(),
                            base_ref: None,
                            target_path: "/work".into(),
                            clone_ref: "source".into(),
                        }),
                    )
                    .await
                    .expect("checkout");
                let old_status = CheckoutStatus { phase: old_phase, ..Default::default() };
                records.update_status("checkout", &checkout.metadata.resource_version, &old_status).await.expect("status");
                let mut convoy = backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("convoy");
                convoy.spec.adopted_checkout_refs.insert(RepositoryKey("github.com/org/repo".into()), "checkout".into());
                if replica {
                    backend
                        .replica_writer::<Checkout>(flotilla_protocol::NodeId::new("home"), "flotilla")
                        .replace(&records.list().await.expect("snapshot"), chrono::Utc::now())
                        .await
                        .expect("replicate");
                }
                let sources = backend.including_replicas::<Checkout>("flotilla").list().await.expect("read sources");
                let selected =
                    flotilla_resources::select_convoy_child_sources(&convoy, &sources.items).remove("checkout").expect("selected");
                let origins = match selected.provenance {
                    flotilla_resources::ResourceProvenance::Local => BTreeMap::new(),
                    flotilla_resources::ResourceProvenance::Replica { origin_root, .. } => {
                        [("checkout".into(), origin_root.to_string())].into()
                    }
                };
                checkout = selected.object;
                let list = std::slice::from_ref(&checkout);
                assert_eq!(
                    crew.verify_convoy_teardown_gate_at_origins(&convoy, list, &origins, false).await.is_ok(),
                    old_phase == CheckoutPhase::Gone
                );
                fixture.request(&checkout);
                assert!(crew.verify_convoy_teardown_gate_at_origins(&convoy, list, &origins, false).await.is_err());
                let ending = match ending {
                    "gave_up" => flotilla_provisioning_testkit::Ending::GaveUp("failure"),
                    "released" => flotilla_provisioning_testkit::Ending::Released,
                    _ => flotilla_provisioning_testkit::Ending::Lost("/work"),
                };
                fixture.finish(&checkout, ending);
                for _ in 0..2 {
                    assert_eq!(
                        crew.verify_convoy_teardown_gate_at_origins(&convoy, std::slice::from_ref(&checkout), &origins, false)
                            .await
                            .is_ok(),
                        !matches!(ending, flotilla_provisioning_testkit::Ending::GaveUp(_))
                    );
                }
                assert_eq!(records.get("checkout").await.expect("record").status, Some(old_status));
                fixture.poison();
                assert!(crew
                    .verify_convoy_teardown_gate_at_origins(&convoy, std::slice::from_ref(&checkout), &origins, false)
                    .await
                    .expect_err("read failure")
                    .contains("poisoned"));
            }
        }
    }
}

// All explicit crew restart writers share captured launch identity; a delayed retry cannot reset a newer running launch.
#[tokio::test]
async fn crew_restart_writes_engine_intent_and_refuses_a_stale_predecessor() {
    use flotilla_provisioning::engine::{
        terminal_session::{EngineTerminalLaunches, TerminalSessionBinding},
        Lifecycles,
    };
    use flotilla_provisioning_testkit::{store_object, terminal_session, Ending, LifecycleFixture};
    use flotilla_resources::{ReadResourceObject, ResourceProvenance, TerminalSessionPhase};
    let (mut crew, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    let obj = store_object(&backend, terminal_session(TerminalSessionPhase::Stopped)).await;
    let lifecycle = LifecycleFixture::<TerminalSessionBinding>::new(false);
    lifecycle.request(&obj);
    lifecycle.finish(&obj, Ending::Released);
    Arc::get_mut(&mut crew).expect("unique service").lifecycle_reads = lifecycle.reads.clone();
    backend.install_terminal_launches(Arc::new(EngineTerminalLaunches { lifecycles: lifecycle.home.clone() }));
    let (selected, replaces) = crew
        .terminal_source(ReadResourceObject { object: obj, provenance: ResourceProvenance::Local })
        .await
        .expect("selected predecessor");
    assert_eq!(replaces.as_deref(), Some("terminal-a#1"));
    crew.restart_terminal("flotilla", &selected.object, replaces.as_deref()).await.expect("request next");
    crew.restart_terminal("flotilla", &selected.object, replaces.as_deref()).await.expect("retry same intent");
    let current = lifecycle.reads.try_terminal_launch("terminal-a", None).expect("read").expect("current");
    assert_eq!(current.name, "terminal-a#2");
    lifecycle.finish(&selected.object, Ending::Ready("next-runtime"));
    let version = backend.using::<ResourceTerminalSession>("flotilla").get("terminal-a").await.expect("before").metadata.resource_version;
    assert!(crew
        .restart_terminal("flotilla", &selected.object, replaces.as_deref())
        .await
        .expect_err("late writer")
        .contains("no longer latest"));
    assert_eq!(
        backend.using::<ResourceTerminalSession>("flotilla").get("terminal-a").await.expect("after").metadata.resource_version,
        version
    );
    assert!(lifecycle.home.get("terminal-a#3").is_none());
    lifecycle.poison();
    assert!(crew.terminal_source(selected).await.expect_err("checked error").contains("poisoned"));
}
