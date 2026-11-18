use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_change_request::observation::ChangeRequestKey;
use flotilla_change_request::observation::ChangeRequestObservationSource;
use flotilla_config::ConfigStore;
use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
use flotilla_protocol::{EnvironmentId, HostName};
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::terminal_records::{TerminalLaunchRequest, TerminalLaunchSource};
use flotilla_resources::{
    ChangeRequestStatus, Convoy as ResourceConvoy, ConvoySpec, ConvoyStatus, CrewSource, CrewWorkPhase, InputMeta, Selector, SystemClock,
    TerminalBrief, WorkflowSnapshot,
};

use crate::engine_home::EngineHome;
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_testkit::ReadCountsBackendExt;
use flotilla_vcs::vcs::{CheckoutVcsResolver, Vcs};
use tokio::sync::RwLock;

use crate::crew_ops::CrewService;
use crate::leaf_engine::LeafSubscriptionTable;
use crate::ports::WorkCredentialReconciler;
use flotilla_observation::change_request_observer::ChangeRequestRefreshCadence;
use flotilla_observation::change_request_observer::ChangeRequestRefresher;
use flotilla_protocol_testkit::event_sink::RecordingEventSink;
use flotilla_resources::ConvoyPhase;

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
    /// The fixture's convoy launch.
    launch: std::sync::OnceLock<String>,
    fail: AtomicBool,
    calls: AtomicUsize,
}

#[async_trait]
impl WorkCredentialReconciler for StagingProbe {
    async fn reconcile(&self, namespace: &str, environment: &str) -> Result<(), String> {
        assert_eq!(environment, "resume-env");
        let launch = self.launch.get().expect("the fixture's launch");
        let convoy = self.backend.using::<ResourceConvoy>(namespace).get(launch).await.expect("convoy");
        // The coder's work is reopened first: its home notes the resume it is staging for.
        let coder = &convoy.status.expect("status").crew_work["work"]["coder"];
        assert!(coder.task_assigned && coder.resume_brief_id.is_some(), "staging follows the noted reopen: {coder:?}");
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.swap(false, Ordering::SeqCst) {
            Err("staging unavailable".into())
        } else {
            Ok(())
        }
    }
}

/// What a fixture's crew service needs kept alive, and the engine records it drives: its config
/// directory, and the engine at its home holding convoy `crew`'s launch, the launch's vessel for
/// `work` and the `coder`'s agent session in it.
struct Held {
    _config: tempfile::TempDir,
    engine: EngineHome,
    /// Convoy `crew`'s launch.
    launch: String,
    /// The launch's vessel for `work`, realised in `resume-env`.
    vessel: String,
    /// The coder's agent session `session`'s launch.
    session: String,
    /// The launches and crew ids of the convoys admitted before it.
    prelude: Vec<(String, String)>,
}

impl Held {
    /// The engine's Message path at the fixture's home.
    fn messages(&self) -> &EngineMessagesHome {
        &self.engine.messages
    }

    /// The fixture convoy's view.
    async fn convoy(&self, backend: &ResourceBackend) -> flotilla_resources::ResourceObject<ResourceConvoy> {
        self.engine.view(backend, &self.launch).await
    }

    /// The address of the fixture convoy's `role` in `work`.
    fn receiver(&self, work: &str, role: &str) -> String {
        flotilla_resources::crew_role_address("flotilla", &self.launch, work, role)
    }

    /// Admits convoy `name` for `role` with one vessel `work` crewed by agent `member`, the vessel
    /// realised in `resume-env` and `member` working in its session `session`; returns the launch
    /// and the session's launch (its crew id).
    async fn admit_crewed(&self, name: &str, role: &str, work: &str, member: &str, session: &str) -> (String, String) {
        admit_crewed(&self.engine, &Crewed { name, role, work, member, session }).await
    }

    /// `member`'s session `session` in the fixture's `work` vessel, running; returns the session's
    /// launch (its crew id). A member that is not the vessel's first agent works once its task is
    /// delivered ([`Self::assign`]).
    fn crew_session(&self, member: &str, session: &str) -> String {
        self.engine.session(&self.vessel, member, session, "resume-env")
    }

    /// `member` of `work` has its task, as a delivered turn notes it: it is wanted running.
    async fn assign(&self, backend: &ResourceBackend, work: &str, member: &str) {
        self.engine
            .note(backend, &self.launch, |status| {
                status.crew_work.get_mut(work).and_then(|crew| crew.get_mut(member)).expect("a declared member").task_assigned = true;
            })
            .await;
    }

    /// The coder declared itself stalled for `scope`, as its stall verb notes it; `change` then
    /// changes the noted stall.
    async fn stall(
        &self,
        backend: &ResourceBackend,
        change: impl FnOnce(&mut flotilla_resources::StalledCondition),
    ) -> flotilla_resources::ResourceObject<ResourceConvoy> {
        let launch = self.launch.clone();
        self.engine
            .note(backend, &self.launch, |status| {
                flotilla_resources::StatusPatch::apply(
                    &flotilla_resources::external_patches::mark_crew_stalled(
                        launch,
                        "work".into(),
                        "coder".into(),
                        chrono::Utc::now(),
                        flotilla_protocol::StallReason::Scope,
                        None,
                        "blocked".into(),
                    ),
                    status,
                );
                let mut stall = status.stalls.about(Some(("work", "coder"))).cloned().expect("the coder's stall");
                change(&mut stall);
                status.stalls.set(stall);
            })
            .await
    }
}

/// Another convoy at the fixture's home: `name` for `role`, with one vessel `work` crewed by
/// agent `member` working in its session `session`.
struct Crewed<'a> {
    name: &'a str,
    role: &'a str,
    work: &'a str,
    member: &'a str,
    session: &'a str,
}

/// Admits `crewed` at `engine`, its vessel realised in `resume-env` and its member's session
/// running there; returns the launch and the session's launch (its crew id).
async fn admit_crewed(engine: &EngineHome, crewed: &Crewed<'_>) -> (String, String) {
    let mut status = flotilla_engine_convoys_testkit::single_agent(crewed.member);
    status.workflow_snapshot.as_mut().expect("workflow").vessels[0].name = crewed.work.into();
    let launch =
        engine.admit(crewed.name, ConvoySpec::builder().workflow_ref("workflow".into()).role(crewed.role.into()).build(), status).await;
    let vessel = engine.vessel(&launch, crewed.work, "resume-env");
    let session = engine.session(&vessel, crewed.member, crewed.session, "resume-env");
    (launch, session)
}

async fn fixture(phase: CrewWorkPhase) -> (Arc<CrewService>, ResourceBackend, Arc<StagingProbe>, Held) {
    fixture_with_message_path(phase, Default::default()).await
}

/// [`fixture`], with the Message path the crew service and its leaf subscriptions share, as the
/// composition root wires them; the engine's path at this home unless `message_path` is already
/// set.
async fn fixture_with_message_path(
    phase: CrewWorkPhase,
    message_path: crate::ports::MessagePath,
) -> (Arc<CrewService>, ResourceBackend, Arc<StagingProbe>, Held) {
    build_fixture(phase, message_path, None, crew_spec(), &[], |_, _| {}).await
}

/// [`fixture`], its convoy admitted with `spec`.
async fn spec_fixture(phase: CrewWorkPhase, spec: ConvoySpec) -> (Arc<CrewService>, ResourceBackend, Arc<StagingProbe>, Held) {
    build_fixture(phase, Default::default(), None, spec, &[], |_, _| {}).await
}

/// [`fixture`], its convoy in Project `work`, declared at the home before anything watches it.
async fn project_fixture(phase: CrewWorkPhase) -> (Arc<CrewService>, ResourceBackend, Arc<StagingProbe>, Held) {
    let spec = ConvoySpec::builder().workflow_ref("workflow".into()).role("crew".into()).project_ref("work".into()).build();
    build_fixture(phase, Default::default(), Some("work"), spec, &[], |_, _| {}).await
}

/// The fixture convoy's spec: workflow `workflow`, for role `crew`.
fn crew_spec() -> ConvoySpec {
    ConvoySpec::builder().workflow_ref("workflow".into()).role("crew".into()).build()
}

/// A turn's subject: the fixture's convoy `launch`, admitted in project `flotilla`, at `revision`.
fn convoy_subject(launch: &str, revision: &str) -> flotilla_messaging::IntentReference {
    let address = flotilla_resources::convoy::engine::convoy_reference_address(launch, "flotilla").expect("convoy address");
    flotilla_messaging::IntentReference::subject(address, flotilla_address::Revision::new(revision).expect("revision"))
}

/// [`fixture`] admitted with the workflow `shape` makes of the fixture's: a convoy's workflow is
/// frozen when it is admitted.
async fn shaped_fixture(
    phase: CrewWorkPhase,
    shape: impl FnOnce(&mut WorkflowSnapshot),
) -> (Arc<CrewService>, ResourceBackend, Arc<StagingProbe>, Held) {
    prelude_fixture(phase, &[], |snapshot, _| shape(snapshot)).await
}

/// [`fixture`] at a home that admitted `prelude` first, its workflow shaped by `shape` from the
/// prelude's launches and crew ids.
async fn prelude_fixture(
    phase: CrewWorkPhase,
    prelude: &[Crewed<'_>],
    shape: impl FnOnce(&mut WorkflowSnapshot, &[(String, String)]),
) -> (Arc<CrewService>, ResourceBackend, Arc<StagingProbe>, Held) {
    build_fixture(phase, Default::default(), None, crew_spec(), prelude, shape).await
}

/// The fixture at a home whose fleet is designated and which declares `project`, its crew
/// service on `message_path`, or on the home's Message path when that is not set.
async fn build_fixture(
    phase: CrewWorkPhase,
    message_path: crate::ports::MessagePath,
    project: Option<&str>,
    spec: ConvoySpec,
    prelude: &[Crewed<'_>],
    shape: impl FnOnce(&mut WorkflowSnapshot, &[(String, String)]),
) -> (Arc<CrewService>, ResourceBackend, Arc<StagingProbe>, Held) {
    let backend = ResourceBackend::new(InMemoryBackend::default().with_read_counts());
    let config_dir = tempfile::tempdir().expect("config directory");
    let config = Arc::new(ConfigStore::with_base(config_dir.path()));
    let environment = EnvironmentId::new("local");
    let namespace = Arc::new(std::sync::RwLock::new("flotilla".into()));
    let engine = EngineHome::install(&backend, "flotilla", "host");
    designate_fleet(&backend, "flotilla").await;
    if let Some(project) = project {
        backend
            .definitions::<flotilla_resources::Project>("flotilla")
            .apply(
                &InputMeta::builder().name(project.into()).build(),
                &flotilla_resources::ProjectSpec::builder()
                    .display_name(project.into())
                    .default_workflow_ref("workflow".into())
                    .repositories(Vec::new())
                    .build(),
            )
            .await
            .expect("the convoy's project");
    }
    if message_path.get().is_none() {
        let _ = message_path.set(engine.messages.messages());
    }
    let refresher =
        ChangeRequestRefresher::new(backend.clone(), "host".into(), Arc::new(UnusedForge), ChangeRequestRefreshCadence::default());
    let subscriptions = LeafSubscriptionTable::new(backend.clone(), Arc::new(RecordingEventSink::default()), refresher, engine.reads())
        .with_message_path(message_path.clone())
        .with_convoy_engine(engine.path());
    let probe = Arc::new(StagingProbe {
        backend: backend.clone(),
        launch: std::sync::OnceLock::new(),
        fail: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
    });
    let reconciler: Arc<dyn WorkCredentialReconciler> = probe.clone();
    let crew = Arc::new(
        CrewService::builder()
            .resource_backend(backend.clone())
            .accounts(Arc::new(flotilla_store_testkit::OldStoreAccounts::new(backend.clone())))
            .leaf_subscriptions(subscriptions)
            .lifecycle_reads(engine.reads())
            .vessel_seats(Arc::new(flotilla_store::EngineVesselSeats::new(engine.reads())))
            .work_credential_reconciler(RwLock::new(Some(reconciler)))
            .clock(Arc::new(SystemClock))
            .provisioning_namespace(namespace)
            .config(config)
            .host_name(HostName::new("host"))
            .brief_artifact_writer(Arc::new(RwLock::new(None)))
            .checkout_vcs(Arc::new(NoCheckoutVcs))
            .local_environment_id(environment)
            .capability_cards(crate::crew_capabilities::CapabilityCardTracker::with_message_path(message_path.clone()))
            .message_path(message_path)
            .convoy_engine(engine.path())
            .build(),
    );
    // A Done coder has kept what it owed: it owes a decision ledger to have something to keep.
    let promises = match phase {
        CrewWorkPhase::Done => vec![flotilla_resources::TemplatePromise::decision_ledger("coder")],
        _ => Vec::new(),
    };
    let mut snapshot = WorkflowSnapshot {
        cascade: None,
        vessels: vec![flotilla_resources::VesselRequirement::builder()
            .name("work".into())
            .crew(vec![flotilla_resources::CrewSpec::builder()
                .role("coder".into())
                .promises(promises)
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
    };
    let mut admitted = Vec::new();
    for crewed in prelude {
        admitted.push(admit_crewed(&engine, crewed).await);
    }
    shape(&mut snapshot, &admitted);
    let launch = engine.admit("crew", spec, ConvoyStatus { workflow_snapshot: Some(snapshot), ..Default::default() }).await;
    probe.launch.set(launch.clone()).expect("one launch");
    // The coder works in its vessel's agent session: the first agent of a vessel starts with it.
    let vessel = engine.vessel(&launch, "work", "resume-env");
    let session = engine.launch_session(&TerminalLaunchRequest {
        session: "session".into(),
        environment: "resume-env".into(),
        vessel: Some(vessel.clone()),
        role: "coder".into(),
        pool: "passthrough".into(),
        cwd: "/repo".into(),
        env: Default::default(),
        source: TerminalLaunchSource::Agent {
            capability: "code".into(),
            adapter: None,
            model: None,
            brief: TerminalBrief { artifact_digest: None, path: "brief.md".into(), content: "original".into(), copies: vec![] },
        },
    });
    match phase {
        CrewWorkPhase::Working => {}
        CrewWorkPhase::Done => engine.keep(&backend, &launch, "work", "coder").await,
        // The session the coder works in was lost.
        CrewWorkPhase::Interrupted => engine.engine.terminals.lost(&session, "pool lost the session"),
        other => panic!("no fixture for a {other:?} coder"),
    }
    let held = Held { _config: config_dir, engine, launch, vessel, session, prelude: admitted };
    assert_eq!(held.convoy(&backend).await.status.expect("status").crew_work["work"]["coder"].phase, phase);
    (crew, backend, probe, held)
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

// Resume uses the Environment its vessel's launch realised in, by its ending and cause: requested
// and realised environments may resume.
#[tokio::test]
async fn resume_reads_environment_lifecycle_without_a_copy() {
    use flotilla_provisioning_testkit::Ending;
    use flotilla_resources::{EnvironmentSpec, HostDirectEnvironmentSpec};
    let (crew, _backend, _, held) = fixture(CrewWorkPhase::Working).await;
    let env = EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/work".into() }),
        ..Default::default()
    };
    let lifecycle = &held.engine.engine.environments;
    let launch = held.launch.as_str();
    lifecycle.request("resume-env", &env, None);
    crew.ensure_vessel_backing_available("flotilla", launch, "work").await.expect("requested may resume");
    lifecycle.finish("resume-env", Ending::Ready("/work"));
    crew.ensure_vessel_backing_available("flotilla", launch, "work").await.expect("ready may resume");
    lifecycle.finish("resume-env", Ending::Lost("/work"));
    let error = crew.ensure_vessel_backing_available("flotilla", launch, "work").await.expect_err("lost cannot resume");
    assert!(error.contains("lost, recoverable") && !error.contains("stale"));
    lifecycle.poison();
    assert!(crew.ensure_vessel_backing_available("flotilla", launch, "work").await.expect_err("checked read").contains("poisoned"));
}

// Teardown reads Checkout endings from the engine's records of the convoy's checkouts: a live or
// given-up launch refuses, and a released or lost one has ended and passes.
#[tokio::test]
async fn teardown_reads_checkout_endings_from_the_engine() {
    use flotilla_provisioning_testkit::{CheckoutFixture, Ending};
    use flotilla_resources::{CheckoutSpec, CheckoutWorktreeSpec, RepositoryKey};

    for ending in [Ending::Released, Ending::Lost("/work"), Ending::GaveUp("failure")] {
        let (crew, backend, _, held) = fixture(CrewWorkPhase::Working).await;
        let engine = CheckoutFixture::default();
        let mut convoy = held.convoy(&backend).await;
        convoy.spec.adopted_checkout_refs.insert(RepositoryKey("github.com/org/repo".into()), "checkout".into());
        engine.open_convoy("crew");
        let launch = engine.request(
            "checkout",
            &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: RepositoryKey("github.com/org/repo".into()),
                env_ref: "env".into(),
                r#ref: "branch".into(),
                base_ref: None,
                target_path: "/work".into(),
                clone_ref: "source".into(),
            }),
            Some(&engine.holder("crew")),
        );
        assert!(crew.verify_convoy_teardown_gate_for_checkouts(&convoy, &[engine.record(&launch)], false).await.is_err());
        let gave_up = matches!(ending, Ending::GaveUp(_));
        engine.finish(&launch, ending);
        for _ in 0..2 {
            assert_eq!(crew.verify_convoy_teardown_gate_for_checkouts(&convoy, &[engine.record(&launch)], false).await.is_ok(), !gave_up);
        }
    }
}

// A crew restart starts an ended session's next launch; a repeated or late restart while that
// launch lives starts no third.
#[tokio::test]
async fn crew_restart_starts_the_next_launch_once() {
    use flotilla_provisioning_testkit::TerminalsFixture;
    let (mut crew, _backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    let terminals = TerminalsFixture::default();
    Arc::get_mut(&mut crew).expect("unique service").lifecycle_reads = terminals.reads.clone();
    let request = TerminalsFixture::agent("v1", "coder", "env");
    let first = terminals.request(&request);
    terminals.ready(&first, "first-runtime", &Default::default());
    terminals.exited(&first, "agent exited with code 0");
    let stopped = terminals.record(&first).read_view("flotilla", None, None, None);
    crew.restart_terminal(&stopped).expect("request next");
    let next = terminals.record(&request.session).name;
    assert_ne!(next, first);
    crew.restart_terminal(&stopped).expect("retry same intent");
    terminals.ready(&next, "next-runtime", &Default::default());
    crew.restart_terminal(&stopped).expect("late writer");
    assert_eq!(terminals.record(&request.session).name, next, "no third launch");
    assert!(terminals.record(&request.session).is_running());
}
