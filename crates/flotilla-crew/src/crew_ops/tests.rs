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
            .checkout_vcs(Arc::new(NoCheckoutVcs))
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
