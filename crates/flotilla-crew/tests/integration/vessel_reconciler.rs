use flotilla_resources::ResumeIntent;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::Utc;
use common::{
    admit_on_engine, create_convoy_with_single_task, create_docker_kind, create_host_direct_kind, create_kind, create_ready_checkout,
    create_ready_clone, create_ready_docker_environment, create_ready_host_direct_environment, meta, DockerKindFixture,
    ReadyCheckoutFixture,
};
use flotilla_crew::reconcilers::vessel::WorktreeMetadataResolver;
use flotilla_crew::reconcilers::vessel::{CrewSeats, VesselPlanning};
use flotilla_protocol::LifecycleReading;
use flotilla_protocol::{CanonicalHostId, IssueRef, IssueSource, IssueState, PlacementDecision, PlacementTargetHost};
use flotilla_resources::{
    canonicalize_repo_url, clone_key, interactive_single_workflow_spec, Artifact, ArtifactSpec, BoundChangeRequest, CheckoutSpec,
    CheckoutWorktreeSpec, ClaimExit, Convoy, ConvoyIssue, ConvoyPhase, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, CrewSource,
    CrewSpec, CrewWorkPhase, CrewWorkState, DockerEnvironmentSpec, DockerImagePullPolicy, EnvironmentSpec, ExitDeclaration,
    FulfilmentKindSpec, FulfilmentRealisation, InputMeta, IssueSnapshot, ObservedCheckoutSpec, Repository, RepositorySpec, Selector,
    Stance, Vessel, VesselPhase, VesselRequirement, WorkPhase, WorkState, WorkflowSnapshot, CONVOY_LABEL, CREW_ORDINAL_LABEL, ROLE_LABEL,
    VESSEL_LABEL, VESSEL_ORDINAL_LABEL, VESSEL_REF_LABEL,
};
use flotilla_resources::{ResourceObject, BRIEF_ARTIFACTS_ANNOTATION, PLACEMENT_SNAPSHOT_ANNOTATION};
use flotilla_store::{
    controller::{ControllerLoop, Reconciler},
    ensure_repository, patch_resource_annotation, ResourceBackend, VesselViews,
};
use flotilla_vcs::vcs::WorktreeMetadata;
use rstest::rstest;
use tokio::time::Duration;

use crate::common;
use flotilla_provisioning_testkit::{Ending, EngineFixture};
use flotilla_resources::checkout_records::{latest_launches, CheckoutHolder, CheckoutRecord, CheckoutRecords as _};
use flotilla_resources::terminal_records::{TerminalLaunchRequest, TerminalLaunchSource, TerminalRecord, TerminalRecords as _};
use flotilla_resources::vessel_planning::{VesselPlan, VesselPlanner as _};
use flotilla_resources::vessel_records::{VesselRecords as _, VesselStep};

const NAMESPACE: &str = "flotilla";
const REPO_URL: &str = "https://github.com/flotilla-org/flotilla.git";
const GIT_REF: &str = "feat/task-provisioning";
const HOST_REF: &str = "01HXYZ";

// Process/VCS boundary fake: deliberately use a resolved admin name that is
// different from the target basename; the controller must not guess Git names.
struct TestWorktreeMetadata(Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>);
#[async_trait]
impl WorktreeMetadataResolver for TestWorktreeMetadata {
    async fn worktree_metadata(&self, env_ref: &str, target: &str) -> Result<WorktreeMetadata, String> {
        let checkout = self
            .0
            .checkouts()?
            .here()?
            .into_iter()
            .find(|checkout| checkout.spec.target_path() == Some(target) && checkout.spec.env_ref() == Some(env_ref))
            .expect("fixture checkout");
        let CheckoutSpec::Worktree(spec) = checkout.spec else { panic!("worktree fixture") };
        let clone = self.0.clones()?.get(&spec.clone_ref, None)?.ok_or_else(|| format!("no clone {}", spec.clone_ref))?;
        let common_dir = std::path::PathBuf::from(clone.spec.path).join(".git");
        let name = std::path::Path::new(target).file_name().expect("target name").to_string_lossy();
        let admin_dir = common_dir.join("worktrees").join(format!("resolved-{name}"));
        Ok(WorktreeMetadata { common_dir, admin_dir })
    }
}

/// What launch `vessel` requested of the engine: its environment, held for it.
fn requested_environment(engine: &EngineFixture, vessel: &str) -> Option<flotilla_resources::environment_records::EnvironmentRecord> {
    let requested = flotilla_resources::environment_records::EnvironmentRecords::find(
        engine.environments.environments.as_ref(),
        &launch_environment(vessel),
    )
    .expect("environment read")?;
    assert_eq!(requested.vessel.as_deref(), Some(vessel), "the environment is held for its vessel");
    Some(requested)
}

/// The environment launch `launch` requests for itself.
fn launch_environment(launch: &str) -> String {
    format!("env-{}", launch.replace('#', "-"))
}

/// Launch of `work` in `convoy`, requested at the engine's home with its convoy placed by `kind`,
/// read as the vessel the reconciler plans.
async fn launch_workspace(
    backend: &ResourceBackend,
    engine: &EngineFixture,
    convoy: &str,
    work: &str,
    kind: &str,
) -> ResourceObject<Vessel> {
    patch_resource_annotation(backend, NAMESPACE, "Convoy", convoy, PLACEMENT_SNAPSHOT_ANNOTATION, kind).await.expect("place convoy");
    let launch = engine.vessels.request(convoy, work, HOST_REF);
    read_vessel(backend, engine, &launch).await
}

/// Records that `convoy` adopts `checkouts`, by repository.
async fn adopt_checkouts(backend: &ResourceBackend, convoy: &str, checkouts: BTreeMap<flotilla_resources::RepositoryKey, String>) {
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let current = convoys.get(convoy).await.expect("convoy");
    let mut spec = current.spec.clone();
    spec.adopted_checkout_refs = checkouts;
    convoys.update(&InputMeta::from(&current.metadata), &current.metadata.resource_version, &spec).await.expect("adopt checkouts");
}

/// Launch `launch` read as a vessel, joined with its convoy and what the engine holds for it.
async fn read_vessel(backend: &ResourceBackend, engine: &EngineFixture, launch: &str) -> ResourceObject<Vessel> {
    backend.install_engine_records(Arc::new(VesselViews::new(backend.clone(), NAMESPACE, engine.reads())));
    backend.clone().using::<Vessel>(NAMESPACE).get(launch).await.expect("vessel view")
}

/// The cause of launch `launch`'s failure: a try it retries while what it needs may yet arrive, or
/// its give-up when that has failed for good.
fn failure(engine: &EngineFixture, launch: &str) -> String {
    match reading(engine, launch) {
        LifecycleReading::Retrying { cause, .. } | LifecycleReading::GaveUp { cause } => cause,
        other => panic!("expected a failure, read {other:?}"),
    }
}

/// The reading of launch `launch`.
fn reading(engine: &EngineFixture, launch: &str) -> LifecycleReading {
    engine.vessels.vessels.get(launch, None).expect("vessel read").expect("vessel launch").reading
}

/// The container recipe of what `vessel` requested of the engine.
fn requested_docker(engine: &EngineFixture, vessel: &str) -> Option<DockerEnvironmentSpec> {
    requested_environment(engine, vessel).and_then(|environment| environment.spec.docker)
}

/// The clones `engine` holds, each requested by a vessel or put there by the test.
fn requested_clones(engine: &EngineFixture) -> Vec<flotilla_resources::clone_records::CloneRecord> {
    flotilla_resources::clone_records::CloneRecords::here(engine.clones.clones.as_ref()).expect("clones read")
}

/// The checkouts `engine` holds, each its latest launch, by checkout name: what vessels requested
/// of it, or the test put there.
fn requested_checkouts(engine: &EngineFixture) -> BTreeMap<String, CheckoutRecord> {
    latest_launches(engine.checkouts.checkouts.here().expect("checkouts read"))
}

/// The vessel planner under test and its crew seats, reading the engine's records from `engine`.
struct TestVessels<'a> {
    planning: Arc<VesselPlanning>,
    seats: CrewSeats,
    backend: ResourceBackend,
    engine: &'a EngineFixture,
}

impl TestVessels<'_> {
    /// One pass over `vessel` as its home runs it: its plan, the edge its Vessel controller writes
    /// for that plan (whose writing the provisioning tests cover), and, once it is ready, its crews
    /// seated.
    async fn prepare(&self, vessel: &ResourceObject<Vessel>) -> Result<VesselPlan, String> {
        let launch = &vessel.metadata.name;
        let plan = self.planning.plan(launch).await?;
        let ready = matches!(reading(self.engine, launch), LifecycleReading::Ready { .. });
        let step = match &plan {
            VesselPlan::Ready { environment, .. } if !ready => Some(VesselStep::Realised { environment: environment.clone() }),
            VesselPlan::Lost { reason } if ready => Some(VesselStep::Lost { reason: reason.clone() }),
            VesselPlan::Failed { cause, .. } if ready => Some(VesselStep::Broke { reason: cause.clone() }),
            VesselPlan::Lost { reason } => Some(VesselStep::Failed { cause: reason.clone(), retryable: true }),
            VesselPlan::Failed { cause, retryable } => Some(VesselStep::Failed { cause: cause.clone(), retryable: *retryable }),
            VesselPlan::Waiting { .. } | VesselPlan::Ready { .. } => None,
        };
        if let Some(step) = step {
            self.engine.vessels.step(launch, step);
        }
        if matches!(reading(self.engine, launch), LifecycleReading::Ready { .. }) {
            let current = read_vessel(&self.backend, self.engine, launch).await;
            self.seats.prepare(&current).await.map_err(|error| error.to_string())?;
        }
        Ok(plan)
    }
}

/// The planner and seats under test, reading the engine's records from `engine`.
fn test_vessel_reconciler<'a>(backend: ResourceBackend, namespace: &str, engine: &'a EngineFixture) -> TestVessels<'a> {
    let planning = Arc::new(
        VesselPlanning::new(backend.clone(), namespace, Arc::new(flotilla_store_testkit::OldStoreAccounts::new(backend.clone())))
            .with_worktree_metadata(Arc::new(TestWorktreeMetadata(engine.reads())))
            .with_lifecycle_reads(engine.reads()),
    );
    TestVessels { seats: CrewSeats::new(Arc::clone(&planning)), planning, backend, engine }
}

#[tokio::test]
async fn landed_convoy_does_not_recreate_a_missing_checkout() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-landed", "implement", REPO_URL, GIT_REF).await;
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let mut status = convoy.status.expect("convoy status");
    status.phase = ConvoyPhase::Landed;
    convoys.update_status("convoy-landed", &convoy.metadata.resource_version, &status).await.expect("mark landed");
    create_host_direct_kind(&backend, NAMESPACE, "policy-landed", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/tmp/convoy-landed");
    admit_on_engine(&backend, NAMESPACE, &lease_engine.checkouts, "convoy-landed").await;
    let vessel = launch_workspace(&backend, lease_engine, "convoy-landed", "implement", "policy-landed").await;

    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    // Planned again, as each wake of its controller plans it, it still waits for its release.
    for _ in 0..2 {
        let plan = reconciler.prepare(&vessel).await.expect("landed vessel dependencies");
        assert!(matches!(plan, VesselPlan::Waiting { ref held, .. } if held.is_empty()), "{plan:?}");
    }
    assert!(requested_checkouts(lease_engine).is_empty(), "landed convoy requested its checkout");
}

// A convoy the engine ends while its vessel is provisioning, before its old record says so, holds
// no new checkout: the vessel waits to be released by its requester and never fails, so it fails
// no work.
#[tokio::test]
async fn a_convoy_ending_during_provisioning_leaves_its_vessel_unfailed() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-ending", "implement", REPO_URL, GIT_REF).await;
    create_host_direct_kind(&backend, NAMESPACE, "policy-ending", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/tmp/convoy-ending");
    admit_on_engine(&backend, NAMESPACE, &lease_engine.checkouts, "convoy-ending").await;
    let vessel = launch_workspace(&backend, lease_engine, "convoy-ending", "implement", "policy-ending").await;
    lease_engine.checkouts.deliver_convoy("convoy-ending");

    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(&vessel).await.expect("prepare");
    assert!(requested_checkouts(lease_engine).is_empty(), "an ended convoy holds no new checkout");
    assert_eq!(reading(lease_engine, &vessel.metadata.name), LifecycleReading::InProgress, "the vessel waits for its release");
}

#[tokio::test]
async fn repositoryless_vessel_runs_tools_without_provisioning_a_checkout() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoy = backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .create(
            &meta("convoy-scratch"),
            &ConvoySpec {
                continuation: None,
                subjects: Vec::new(),
                role: String::new(),
                generation: 1,
                workflow_ref: "scratch".to_string(),
                dispatching_principal_ref: Default::default(),
                inputs: BTreeMap::new(),
                placement_policy: None,
                repositories: Vec::new(),
                r#ref: None,
                project_ref: NAMESPACE.to_string(),
                adopted_checkout_refs: BTreeMap::new(),
                issues: Vec::new(),
                change_request: None,
                instruction: None,
                promises: Vec::new(),
            },
        )
        .await
        .expect("convoy should create");
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status(
            "convoy-scratch",
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![VesselRequirement {
                        needs: Default::default(),
                        name: "work".to_string(),
                        depends_on: Vec::new(),
                        repository_refs: None,
                        credential_refs: Default::default(),
                        credential_scopes: Default::default(),
                        credential_permissions: Default::default(),
                        crew: vec![CrewSpec::builder()
                            .role("shell".to_string())
                            .source(CrewSource::Tool { command: "bash".to_string() })
                            .build()],
                    }],
                }),
                ..Default::default()
            },
        )
        .await
        .expect("convoy status should update");
    create_host_direct_kind(&backend, NAMESPACE, "policy-scratch", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    let vessel = launch_workspace(&backend, lease_engine, "convoy-scratch", "work", "policy-scratch").await;

    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(&vessel).await.expect("deps should load");

    assert!(requested_checkouts(lease_engine).is_empty(), "a repositoryless vessel requests no checkout");
    assert!(requested_clones(lease_engine).is_empty(), "a repositoryless vessel requests no clone");
    assert!(requested_terminals(lease_engine).iter().any(|terminal| {
        terminal.cwd == "/Users/alice/dev/flotilla-repos"
            && matches!(&terminal.source, TerminalLaunchSource::Tool { command } if command == "bash")
    }));
}

#[tokio::test]
async fn host_direct_placement_is_not_refused_by_template_stance() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-contained", "implement", REPO_URL, GIT_REF).await;
    create_host_direct_kind(&backend, NAMESPACE, "policy-host", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    let workspace = launch_workspace(&backend, lease_engine, "convoy-contained", "implement", "policy-host").await;

    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("deps should load");

    let reading = reading(lease_engine, &workspace.metadata.name);
    assert!(
        !matches!(reading, LifecycleReading::Retrying { .. } | LifecycleReading::Failed { .. } | LifecycleReading::GaveUp { .. }),
        "{reading:?}"
    );
}

#[tokio::test]
async fn ready_vessel_records_requested_and_effective_stance() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-stance", "implement", REPO_URL, GIT_REF).await;
    create_host_direct_kind(&backend, NAMESPACE, "policy-stance", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    let clone_name =
        format!("clone-{}", clone_key(&canonicalize_repo_url(REPO_URL).expect("repo canonicalization"), &host_direct_env_name()));
    create_ready_clone(&lease_engine.clones, &clone_name, REPO_URL, &host_direct_env_name(), "/tmp/clone");
    let checkout_path = "/Users/alice/dev/flotilla-repos/workspace-stance";
    create_ready_checkout(
        &lease_engine.checkouts,
        "convoy-stance",
        ReadyCheckoutFixture::builder()
            .name("checkout-convoy-stance".to_string())
            .env_ref(host_direct_env_name())
            .git_ref(GIT_REF.to_string())
            .path(checkout_path.to_string())
            .maybe_worktree(Some(worktree_checkout_spec(&host_direct_env_name(), GIT_REF, checkout_path, &clone_name)))
            .build(),
    );
    let workspace = launch_workspace(&backend, lease_engine, "convoy-stance", "implement", "policy-stance").await;
    create_running_terminal(lease_engine, &workspace.metadata.name, &host_direct_env_name(), "coder", "cargo test", checkout_path, "cleat")
        .await;

    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("deps should load");

    assert!(matches!(reading(lease_engine, &workspace.metadata.name), LifecycleReading::Ready { .. }));
    let status = read_vessel(&backend, lease_engine, &workspace.metadata.name).await.status.expect("vessel status");
    assert_eq!(
        (status.configured_limits, status.requested_stance, status.effective_stance),
        (None, Some(Stance::Trusted), Some(Stance::Trusted))
    );
}

#[tokio::test]
async fn sequential_vessels_share_a_convoy_owned_worktree_checkout() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-shared", "implement", REPO_URL, GIT_REF).await;
    let mut status = convoy.status.clone().expect("convoy status");
    status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels.push(VesselRequirement {
        needs: Default::default(),
        name: "review".to_string(),
        depends_on: vec!["implement".to_string()],
        repository_refs: None,
        credential_refs: Default::default(),
        credential_scopes: Default::default(),
        credential_permissions: Default::default(),
        crew: Vec::new(),
    });
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-shared", &convoy.metadata.resource_version, &status)
        .await
        .expect("second vessel should be recorded");
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let current = convoys.get("convoy-shared").await.expect("convoy after status update");
    let mut spec = current.spec.clone();
    spec.change_request = Some(BoundChangeRequest {
        id: "1071".to_string(),
        repository_ref: spec.repositories[0].repo_ref.clone(),
        title: "Existing PR".to_string(),
    });
    convoys.update(&InputMeta::from(&current.metadata), &current.metadata.resource_version, &spec).await.expect("bind change request");
    create_host_direct_kind(&backend, NAMESPACE, "policy-shared", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    let clone_name =
        format!("clone-{}", clone_key(&canonicalize_repo_url(REPO_URL).expect("repo canonicalization"), &host_direct_env_name()));
    create_ready_clone(&lease_engine.clones, &clone_name, REPO_URL, &host_direct_env_name(), "/tmp/clone");
    let implement = launch_workspace(&backend, lease_engine, "convoy-shared", "implement", "policy-shared").await;
    let review = launch_workspace(&backend, lease_engine, "convoy-shared", "review", "policy-shared").await;

    let shared = admit_on_engine(&backend, NAMESPACE, &lease_engine.checkouts, "convoy-shared").await;

    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(&implement).await.expect("implement dependencies");
    let requested = requested_checkouts(lease_engine);
    let checkout = requested.get("checkout-convoy-shared").expect("first vessel should request the shared checkout");
    assert_eq!(checkout.holder, Some(CheckoutHolder::Convoy(shared)), "held for the convoy's launch, not the vessel");
    assert_eq!(requested.len(), 1);
    let launch = checkout.name.clone();
    lease_engine.checkouts.ready(&launch, None);

    reconciler.prepare(&review).await.expect("review dependencies");
    assert_eq!(
        requested_checkouts(lease_engine).get("checkout-convoy-shared").map(|record| record.name.as_str()),
        Some(launch.as_str()),
        "later vessels should reuse the convoy checkout"
    );

    // A removed worktree is lost; the next vessel pass requests the checkout's next launch.
    lease_engine.checkouts.finish(&launch, Ending::Lost(""));
    reconciler.prepare(&review).await.expect("gone checkout dependencies");
    let next = requested_checkouts(lease_engine).remove("checkout-convoy-shared").expect("requested again");
    assert_ne!(next.name, launch, "a lost checkout is requested as its next launch");
    assert!(!next.ended());
}

// Deleted stuck_provisioning_vessel_surfaces_the_statusless_checkout_it_is_retrying: the "provisioning is stalled" status message and its requeue timer are gone (#3108).

#[tokio::test]
async fn unrelated_convoys_with_the_same_branch_use_distinct_worktree_paths() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-one", "implement", REPO_URL, GIT_REF).await;
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-two", "implement", REPO_URL, GIT_REF).await;
    create_host_direct_kind(&backend, NAMESPACE, "policy-shared-branch", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    let clone_name =
        format!("clone-{}", clone_key(&canonicalize_repo_url(REPO_URL).expect("repo canonicalization"), &host_direct_env_name()));
    create_ready_clone(&lease_engine.clones, &clone_name, REPO_URL, &host_direct_env_name(), "/tmp/clone");
    let first = launch_workspace(&backend, lease_engine, "convoy-one", "implement", "policy-shared-branch").await;
    let second = launch_workspace(&backend, lease_engine, "convoy-two", "implement", "policy-shared-branch").await;

    admit_on_engine(&backend, NAMESPACE, &lease_engine.checkouts, "convoy-one").await;
    admit_on_engine(&backend, NAMESPACE, &lease_engine.checkouts, "convoy-two").await;
    let reconciler = test_vessel_reconciler(backend, NAMESPACE, lease_engine);
    reconciler.prepare(&first).await.expect("first dependencies");
    reconciler.prepare(&second).await.expect("second dependencies");
    let requested = requested_checkouts(lease_engine);
    let checkout_path = |convoy: &str| {
        requested
            .get(&format!("checkout-{convoy}"))
            .and_then(|record| record.path().map(str::to_string))
            .expect("convoy should request a checkout")
    };
    let first_path = checkout_path("convoy-one");
    let second_path = checkout_path("convoy-two");

    assert_eq!(first_path, "/Users/alice/dev/flotilla-repos/convoy-one/flotilla.feat-task-provisioning");
    assert_eq!(second_path, "/Users/alice/dev/flotilla-repos/convoy-two/flotilla.feat-task-provisioning");
    assert_ne!(first_path, second_path);
}

#[tokio::test]
async fn multi_repository_vessel_provisions_every_checkout_and_runs_crew_at_workspace_root() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let repository_specs = [
        RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("flotilla repository"),
        RepositorySpec::remote("https://github.com/flotilla-org/cleat").expect("cleat repository"),
    ];
    for repository in &repository_specs {
        ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &repository.key(), repository)
            .await
            .expect("repository should create");
    }
    let convoy = backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .create(
            &meta("convoy-multi"),
            &ConvoySpec {
                continuation: None,
                subjects: Vec::new(),
                role: String::new(),
                generation: 1,
                workflow_ref: "wf".to_string(),
                dispatching_principal_ref: Default::default(),
                inputs: BTreeMap::new(),
                placement_policy: None,
                repositories: vec![
                    ConvoyRepositorySpec {
                        url: "https://github.com/flotilla-org/cleat".to_string(),
                        repo_ref: repository_specs[1].key(),
                        source_ref: "main".to_string(),
                        target_ref: "main".to_string(),
                        workspace_slug: "cleat".to_string(),
                        subpaths: Vec::new(),
                    },
                    ConvoyRepositorySpec {
                        url: "https://github.com/flotilla-org/flotilla".to_string(),
                        repo_ref: repository_specs[0].key(),
                        source_ref: "main".to_string(),
                        target_ref: "main".to_string(),
                        workspace_slug: "flotilla".to_string(),
                        subpaths: Vec::new(),
                    },
                ],
                r#ref: Some("feature/multi".to_string()),
                project_ref: "flotilla-suite".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
                issues: vec![
                    ConvoyIssue {
                        reference: IssueRef {
                            source: IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() },
                            id: "732".into(),
                        },
                        repository_ref: Some(repository_specs[0].key()),
                        snapshot: IssueSnapshot {
                            title: "Start convoy from an issue".into(),
                            body: Some("Persist this exact issue body.".into()),
                            state: IssueState::Open,
                            labels: vec!["enhancement".into()],
                            as_of: "2026-07-18T09:30:00Z".parse().expect("timestamp"),
                        },
                    },
                    ConvoyIssue {
                        reference: IssueRef {
                            source: IssueSource { service: "https://github.com".into(), scope: "flotilla-org/cleat".into() },
                            id: "733".into(),
                        },
                        repository_ref: None,
                        snapshot: IssueSnapshot {
                            title: "Fix shared workflow setup".into(),
                            body: Some("This issue applies across the checked-out repositories.".into()),
                            state: IssueState::Open,
                            labels: vec!["enhancement".into()],
                            as_of: "2026-07-18T09:31:00Z".parse().expect("timestamp"),
                        },
                    },
                ],
                change_request: None,
                instruction: Some("Keep the public seam stable.".into()),
                promises: Vec::new(),
            },
        )
        .await
        .expect("convoy should create");
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status(
            "convoy-multi",
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![VesselRequirement {
                        needs: Default::default(),
                        name: "implement".to_string(),
                        depends_on: Vec::new(),
                        repository_refs: None,
                        credential_refs: Default::default(),
                        credential_scopes: Default::default(),
                        credential_permissions: Default::default(),
                        crew: vec![CrewSpec::builder()
                            .role("coder".to_string())
                            .source(CrewSource::Agent {
                                selector: Selector::for_capability("coding"),
                                prompt: Some("Work across both repositories.".to_string()),
                                brief_template: None,
                            })
                            .build()],
                    }],
                }),
                ..Default::default()
            },
        )
        .await
        .expect("convoy status should update");
    create_host_direct_kind(&backend, NAMESPACE, "policy-multi", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    for (repository, url) in [
        (&repository_specs[0], "https://github.com/flotilla-org/flotilla"),
        (&repository_specs[1], "https://github.com/flotilla-org/cleat"),
    ] {
        let clone_name = format!(
            "clone-{}",
            clone_key(
                match repository.identity() {
                    flotilla_resources::RepositoryIdentity::Remote { canonical_remote } => canonical_remote,
                    flotilla_resources::RepositoryIdentity::Forge { .. } =>
                        repository.live_remote().expect("forge Repository has a remote"),
                    flotilla_resources::RepositoryIdentity::Local { .. } => panic!("expected remote repository"),
                },
                &host_direct_env_name()
            )
        );
        create_ready_clone(&lease_engine.clones, &clone_name, url, &host_direct_env_name(), &format!("/clones/{}", repository.leaf_slug()));
    }
    let vessel = launch_workspace(&backend, lease_engine, "convoy-multi", "implement", "policy-multi").await;
    admit_on_engine(&backend, NAMESPACE, &lease_engine.checkouts, "convoy-multi").await;
    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);

    reconciler.prepare(&vessel).await.expect("deps should load");
    let requested = requested_checkouts(lease_engine);
    assert_eq!(
        requested.values().map(|record| record.path().expect("managed checkout path")).collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "/Users/alice/dev/flotilla-repos/convoy-multi/feature-multi/cleat",
            "/Users/alice/dev/flotilla-repos/convoy-multi/feature-multi/flotilla",
        ])
    );
    for record in requested.values() {
        lease_engine.checkouts.ready(&record.name, None);
    }

    let current = read_vessel(&backend, lease_engine, &vessel.metadata.name).await;
    reconciler.prepare(&current).await.expect("deps should reload");
    assert!(requested_terminals(lease_engine).iter().any(|terminal| {
        matches!(
            terminal,
            TerminalRecord { cwd, source, .. }
                if cwd == "/Users/alice/dev/flotilla-repos/convoy-multi/feature-multi"
                    && matches!(source, TerminalLaunchSource::Agent { brief, .. }
                        if brief.path == ".flotilla/briefs/coder.md"
                            && brief.copies == [
                                "/Users/alice/dev/flotilla-repos/convoy-multi/feature-multi/cleat",
                                "/Users/alice/dev/flotilla-repos/convoy-multi/feature-multi/flotilla"
                            ]
                            && brief.content.contains("https://github.com` / `flotilla-org/flotilla` / `732")
                            && brief.content.contains("https://github.com` / `flotilla-org/cleat` / `733")
                            && brief.content.contains("Snapshot as of `2026-07-18T09:30:00+00:00`")
                            && brief.content.contains("Persist this exact issue body.")
                            && brief.content.contains("This issue applies across the checked-out repositories.")
                            && brief.content.contains("Keep the public seam stable."))
        )
    }));

    let coder = requested_terminal(lease_engine, "coder").expect("coder requested");
    lease_engine.terminals.ready(&coder.name, "session-coder", &Default::default());
    let current = read_vessel(&backend, lease_engine, &vessel.metadata.name).await;
    reconciler.prepare(&current).await.expect("deps should reload");
    assert!(matches!(reading(lease_engine, &vessel.metadata.name), LifecycleReading::Ready { .. }));
    let status = read_vessel(&backend, lease_engine, &vessel.metadata.name).await.status.expect("vessel status");
    assert_eq!(status.checkout_refs.len(), 2);
}

#[tokio::test]
async fn multi_repository_docker_mounts_the_workspace_and_each_git_common_dir() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let repositories = [
        RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("flotilla repository"),
        RepositorySpec::remote("https://github.com/flotilla-org/cleat").expect("cleat repository"),
    ];
    for repository in &repositories {
        ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &repository.key(), repository)
            .await
            .expect("repository should create");
    }
    let convoy = backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .create(
            &meta("convoy-multi-docker"),
            &ConvoySpec {
                continuation: None,
                subjects: Vec::new(),
                role: String::new(),
                generation: 1,
                workflow_ref: "wf".to_string(),
                dispatching_principal_ref: Default::default(),
                inputs: BTreeMap::new(),
                placement_policy: None,
                repositories: vec![
                    ConvoyRepositorySpec {
                        url: "https://github.com/flotilla-org/flotilla".to_string(),
                        repo_ref: repositories[0].key(),
                        source_ref: "main".to_string(),
                        target_ref: "main".to_string(),
                        workspace_slug: "flotilla".to_string(),
                        subpaths: Vec::new(),
                    },
                    ConvoyRepositorySpec {
                        url: "https://github.com/flotilla-org/cleat".to_string(),
                        repo_ref: repositories[1].key(),
                        source_ref: "main".to_string(),
                        target_ref: "main".to_string(),
                        workspace_slug: "cleat".to_string(),
                        subpaths: Vec::new(),
                    },
                ],
                r#ref: Some("feature/multi".to_string()),
                project_ref: "flotilla-suite".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
                issues: Vec::new(),
                change_request: None,
                instruction: None,
                promises: Vec::new(),
            },
        )
        .await
        .expect("convoy should create");
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status(
            "convoy-multi-docker",
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![VesselRequirement {
                        needs: Default::default(),
                        name: "implement".to_string(),
                        depends_on: Vec::new(),
                        repository_refs: None,
                        credential_refs: Default::default(),
                        credential_scopes: Default::default(),
                        credential_permissions: Default::default(),
                        crew: vec![CrewSpec::builder()
                            .role("coder".to_string())
                            .source(CrewSource::Tool { command: "cargo test".to_string() })
                            .build()],
                    }],
                }),
                ..Default::default()
            },
        )
        .await
        .expect("convoy status should update");
    create_docker_kind(
        &backend,
        NAMESPACE,
        DockerKindFixture::builder()
            .name("policy-multi-docker".to_string())
            .host_ref(HOST_REF.to_string())
            .pool("cleat".to_string())
            .image("ghcr.io/flotilla/dev:latest".to_string())
            .build(),
    )
    .await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");

    // Each engine below holds both repositories' clones, which the checkouts name.
    let put_clones = |engine: &EngineFixture| {
        for slug in ["flotilla", "cleat"] {
            create_ready_clone(
                &engine.clones,
                &format!("clone-{slug}"),
                &format!("https://github.com/flotilla-org/{slug}"),
                &host_direct_env_name(),
                &format!("/Users/alice/dev/flotilla-repos/{slug}"),
            );
        }
    };
    // ... and both repositories' checkouts, held for the convoy and realised.
    let put_checkouts = |engine: &EngineFixture| {
        for (repository, slug) in [(&repositories[0], "flotilla"), (&repositories[1], "cleat")] {
            let path = format!("/Users/alice/dev/flotilla-repos/workspace-multi-docker/{slug}");
            create_ready_checkout(
                &engine.checkouts,
                "convoy-multi-docker",
                ReadyCheckoutFixture::builder()
                    .name(format!("checkout-convoy-multi-docker-{slug}"))
                    .env_ref(host_direct_env_name())
                    .git_ref("feature/multi".to_string())
                    .path(path.clone())
                    .worktree(CheckoutWorktreeSpec {
                        repo_ref: repository.key(),
                        env_ref: host_direct_env_name(),
                        r#ref: "feature/multi".to_string(),
                        base_ref: Some("main".to_string()),
                        target_path: path,
                        clone_ref: format!("clone-{slug}"),
                    })
                    .build(),
            );
        }
    };
    put_clones(lease_engine);
    put_checkouts(lease_engine);
    let vessel = launch_workspace(&backend, lease_engine, "convoy-multi-docker", "implement", "policy-multi-docker").await;

    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(&vessel).await.expect("deps should load");
    let docker = requested_docker(lease_engine, &vessel.metadata.name).expect("docker environment should be created");
    let mounts = docker.mounts.as_slice();
    assert_eq!(mounts.len(), 11);
    assert_eq!(mounts[0].source_path, "/Users/alice/dev/flotilla-repos/convoy-multi-docker/feature-multi");
    assert_eq!(mounts[0].target_path, "/workspace");
    assert_eq!(mounts[1].source_path, "/Users/alice/dev/flotilla-repos/cleat/.git");
    assert_eq!(mounts[1].target_path, "/Users/alice/dev/flotilla-repos/cleat/.git");
    for (clone, slice) in [("cleat", &mounts[1..6]), ("flotilla", &mounts[6..11])] {
        let common = format!("/Users/alice/dev/flotilla-repos/{clone}/.git");
        assert_eq!(slice[0].source_path, common);
        assert_eq!(slice[0].mode, flotilla_resources::EnvironmentMountMode::Rw);
        for (mount, name) in slice[1..].iter().zip(["config", "hooks", "worktrees"]) {
            assert_eq!(mount.source_path, format!("{common}/{name}"));
            assert_eq!(mount.target_path, mount.source_path);
            assert_eq!(mount.mode, flotilla_resources::EnvironmentMountMode::Ro);
        }
    }

    // #2682: writable exceptions are exactly the vessel's resolved admins;
    // all current and future siblings inherit the read-only parent.
    for slice in mounts[1..].as_chunks::<5>().0 {
        assert_eq!(slice[4].mode, flotilla_resources::EnvironmentMountMode::Rw);
        assert!(slice[4].source_path.starts_with(&format!("{}/worktrees/resolved-", slice[0].source_path)));
        assert_eq!(slice[4].target_path, slice[4].source_path);
    }

    // Removing or adding an own-admin overlay requires environment recreation;
    // no crew may be launched using the stale membership's writable grants.
    let docker = docker.clone();
    // A fleet roll leaves legacy mounts on a Ready vessel with running crews.
    // Report recreation without changing that phase or interrupting its session.
    // Each environment below is a fresh engine's: an ended launch is never launched again.
    let mut legacy = docker.clone();
    legacy.mounts.retain(|mount| !mount.source_path.contains("/worktrees"));
    let legacy_engine = &EngineFixture::default();
    create_ready_host_direct_environment(&legacy_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    put_clones(legacy_engine);
    put_checkouts(legacy_engine);
    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, legacy_engine);
    let running = legacy_engine.vessels.request("convoy-multi-docker", "implement", HOST_REF);
    let environment = create_ready_docker_environment(&legacy_engine.environments, &launch_environment(&running), legacy);
    legacy_engine.vessels.ready(&running, &environment.name);
    create_running_terminal(legacy_engine, &running, &environment.name, "coder", "codex", "/workspace", "cleat").await;
    let session = requested_terminal(legacy_engine, "coder").expect("running crew");
    let before = reading(legacy_engine, &running);
    let running = read_vessel(&backend, legacy_engine, &running).await;
    reconciler.prepare(&running).await.expect("legacy rollout");
    assert_eq!(requested_terminals(legacy_engine), vec![session.clone()], "rollout must not interrupt or launch crews");
    // The recreation it waits for is no longer written to the vessel's status (#3108): the launch stays as it was.
    assert_eq!(reading(legacy_engine, &running.metadata.name), before);

    let mut stale = docker.clone();
    stale.mounts.pop();
    let stale_engine = &EngineFixture::default();
    create_ready_host_direct_environment(&stale_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    put_clones(stale_engine);
    put_checkouts(stale_engine);
    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, stale_engine);
    let stale_vessel = stale_engine.vessels.request("convoy-multi-docker", "implement", HOST_REF);
    create_ready_docker_environment(&stale_engine.environments, &launch_environment(&stale_vessel), stale);
    let stale_vessel = read_vessel(&backend, stale_engine, &stale_vessel).await;
    reconciler.prepare(&stale_vessel).await.expect("stale environment");
    assert!(requested_terminals(stale_engine).is_empty(), "stale overlays must not launch crews");
    // Waiting to recreate the environment is no longer written to the vessel's status (#3108): it stays unrealised.
    assert_eq!(reading(stale_engine, &stale_vessel.metadata.name), LifecycleReading::InProgress);
    let mut reordered = docker;
    reordered.mounts.reverse();
    reordered.mounts.push(flotilla_resources::EnvironmentMount {
        source_path: "/policy/cache".into(),
        target_path: "/cache".into(),
        mode: flotilla_resources::EnvironmentMountMode::Rw,
    });
    let reordered_engine = &EngineFixture::default();
    create_ready_host_direct_environment(&reordered_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    put_clones(reordered_engine);
    put_checkouts(reordered_engine);
    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, reordered_engine);
    let reordered_vessel = reordered_engine.vessels.request("convoy-multi-docker", "implement", HOST_REF);
    create_ready_docker_environment(&reordered_engine.environments, &launch_environment(&reordered_vessel), reordered);
    let reordered_vessel = read_vessel(&backend, reordered_engine, &reordered_vessel).await;
    reconciler.prepare(&reordered_vessel).await.expect("matching membership");
    assert!(!requested_terminals(reordered_engine).is_empty(), "matching overlays allow crew launch");

    // A convoy that adopts a checkout for one of its repositories cannot run a multi-repository vessel.
    adopt_checkouts(
        &backend,
        "convoy-multi-docker",
        BTreeMap::from([(repositories[0].key(), "checkout-workspace-multi-docker-flotilla".to_string())]),
    )
    .await;
    let mixed_engine = &EngineFixture::default();
    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, mixed_engine);
    let mixed = mixed_engine.vessels.request("convoy-multi-docker", "implement", HOST_REF);
    let mixed = read_vessel(&backend, mixed_engine, &mixed).await;
    reconciler.prepare(&mixed).await.expect("mixed deps should load");
    assert_eq!(failure(mixed_engine, &mixed.metadata.name), "adopted checkouts are not supported for multi-repository vessel workspaces");
}

#[tokio::test]
async fn vessel_repository_scope_narrows_a_multi_repository_convoy() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let flotilla = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("flotilla repository");
    let cleat = RepositorySpec::remote("https://github.com/flotilla-org/cleat").expect("cleat repository");
    for repository in [&flotilla, &cleat] {
        ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &repository.key(), repository)
            .await
            .expect("repository should create");
    }
    let convoy = backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .create(
            &meta("convoy-scoped"),
            &ConvoySpec {
                continuation: None,
                subjects: Vec::new(),
                role: String::new(),
                generation: 1,
                workflow_ref: "wf".to_string(),
                dispatching_principal_ref: Default::default(),
                inputs: BTreeMap::new(),
                placement_policy: None,
                repositories: vec![
                    ConvoyRepositorySpec {
                        url: "https://github.com/flotilla-org/cleat".to_string(),
                        repo_ref: cleat.key(),
                        source_ref: "main".to_string(),
                        target_ref: "main".to_string(),
                        workspace_slug: "cleat".to_string(),
                        subpaths: Vec::new(),
                    },
                    ConvoyRepositorySpec {
                        url: "https://github.com/flotilla-org/flotilla".to_string(),
                        repo_ref: flotilla.key(),
                        source_ref: "main".to_string(),
                        target_ref: "main".to_string(),
                        workspace_slug: "flotilla".to_string(),
                        subpaths: Vec::new(),
                    },
                ],
                r#ref: Some("feature/scoped".to_string()),
                project_ref: "flotilla-suite".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
                issues: Vec::new(),
                change_request: None,
                instruction: None,
                promises: Vec::new(),
            },
        )
        .await
        .expect("convoy should create");
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status(
            "convoy-scoped",
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![VesselRequirement {
                        needs: Default::default(),
                        name: "implement".to_string(),
                        depends_on: Vec::new(),
                        repository_refs: Some(vec![cleat.key()]),
                        credential_refs: Default::default(),
                        credential_scopes: Default::default(),
                        credential_permissions: Default::default(),
                        crew: Vec::new(),
                    }],
                }),
                ..Default::default()
            },
        )
        .await
        .expect("convoy status should update");
    create_host_direct_kind(&backend, NAMESPACE, "policy-scoped", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    let vessel = launch_workspace(&backend, lease_engine, "convoy-scoped", "implement", "policy-scoped").await;

    admit_on_engine(&backend, NAMESPACE, &lease_engine.checkouts, "convoy-scoped").await;
    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(&vessel).await.expect("deps should load");
    let clone = requested_clones(lease_engine).into_iter().next().expect("scoped repository clone should be requested").spec;
    assert_eq!(clone.path, "/Users/alice/dev/flotilla-repos/cleat");
    let requested = requested_checkouts(lease_engine);
    assert_eq!(requested.keys().collect::<Vec<_>>(), ["checkout-convoy-scoped-cleat"], "only the scoped checkout is requested");
    let checkout = &requested["checkout-convoy-scoped-cleat"].spec;
    assert_eq!(checkout.repo_ref(), &cleat.key());
    assert_eq!(checkout.target_path(), Some("/Users/alice/dev/flotilla-repos/convoy-scoped/feature-scoped/cleat"));

    // A convoy adopting a checkout for the scoped repository runs its next launch in it.
    let adopted_engine = &EngineFixture::default();
    create_ready_host_direct_environment(&adopted_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    let adopted_path = "/Users/alice/dev/cleat-existing";
    adopted_engine.checkouts.put_ready(
        "adopted-cleat-scoped",
        &CheckoutSpec::Observed(ObservedCheckoutSpec {
            r#ref: "feature/scoped".to_string(),
            path: adopted_path.to_string(),
            repo_ref: cleat.key(),
            host_ref: HOST_REF.to_string(),
            is_main: false,
        }),
        None,
    );
    adopt_checkouts(&backend, "convoy-scoped", BTreeMap::from([(cleat.key(), "adopted-cleat-scoped".to_string())])).await;
    let adopted_vessel = adopted_engine.vessels.request("convoy-scoped", "implement", HOST_REF);
    let adopted_vessel = read_vessel(&backend, adopted_engine, &adopted_vessel).await;
    assert_eq!(adopted_vessel.spec.adopted_checkout_refs, BTreeMap::from([(cleat.key(), "adopted-cleat-scoped".to_string())]));

    let checkouts = requested_checkouts(adopted_engine);
    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, adopted_engine);
    reconciler.prepare(&adopted_vessel).await.expect("adopted deps should load");
    assert_eq!(requested_checkouts(adopted_engine), checkouts, "an adopted checkout requests no checkout");
    assert!(requested_clones(adopted_engine).is_empty(), "an adopted checkout requests no clone");
    assert!(matches!(reading(adopted_engine, &adopted_vessel.metadata.name), LifecycleReading::Ready { .. }));
}

#[tokio::test]
async fn contained_requirement_runs_in_contained_docker_placement() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-docker-stance", "implement", REPO_URL, GIT_REF).await;
    let status = convoy.status.expect("convoy should have status");

    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-docker-stance", &convoy.metadata.resource_version, &status)
        .await
        .expect("convoy status should update");
    create_docker_kind(
        &backend,
        NAMESPACE,
        DockerKindFixture::builder()
            .name("kind-docker-stance".to_string())
            .host_ref(HOST_REF.to_string())
            .pool("cleat".to_string())
            .image("ghcr.io/flotilla/dev:latest".to_string())
            .build(),
    )
    .await;
    let workspace = launch_workspace(&backend, lease_engine, "convoy-docker-stance", "implement", "kind-docker-stance").await;
    let launch = workspace.metadata.name.clone();
    create_ready_host_worktree(lease_engine, "convoy-docker-stance", &launch);
    let environment_ref = launch_environment(&launch);
    create_ready_docker_environment(&lease_engine.environments, &environment_ref, docker_worktree_environment(&launch, Default::default()));
    create_running_terminal(lease_engine, &launch, &environment_ref, "coder", "cargo test", "/workspace", "cleat").await;

    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("deps should load");

    assert!(matches!(reading(lease_engine, &launch), LifecycleReading::Ready { .. }));
    let status = read_vessel(&backend, lease_engine, &launch).await.status.expect("vessel status");
    assert_eq!((status.requested_stance, status.effective_stance), (Some(Stance::Contained), Some(Stance::Contained)));
    assert_eq!(status.image_ref.as_deref(), Some("ghcr.io/flotilla/dev:latest"));
    assert_eq!(status.local_image_id.as_deref(), Some("sha256:test-image"));
}

// The created environment takes the constant pull policy and is pinned to the
// provider instance admission froze into the placement decision.
#[tokio::test]
async fn docker_kind_environment_uses_constant_pull_policy_and_decided_instance() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-decided", "implement", REPO_URL, GIT_REF).await;
    let mut status = convoy.status.expect("convoy status");
    status.placement_decision = Some(PlacementDecision {
        provider_instance: Some("rootless".to_string()),
        provider_workspace_root: None,
        minimal_alternatives: Vec::new(),
        escalation_reason: None,
        kind: "kind-decided".to_string(),
        target_host: PlacementTargetHost { reference: CanonicalHostId::resolved(HOST_REF), display_name: HOST_REF.to_string() },
        refused_candidates: Vec::new(),
        viable_not_selected: Vec::new(),
        allocation: None,
    });
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-decided", &convoy.metadata.resource_version, &status)
        .await
        .expect("convoy status should update");
    create_docker_kind(
        &backend,
        NAMESPACE,
        DockerKindFixture::builder()
            .name("kind-decided".to_string())
            .host_ref(HOST_REF.to_string())
            .pool("cleat".to_string())
            .image("flotilla-dev-env:latest".to_string())
            .build(),
    )
    .await;
    let vessel = launch_workspace(&backend, lease_engine, "convoy-decided", "implement", "kind-decided").await;
    create_ready_host_worktree(lease_engine, "convoy-decided", &vessel.metadata.name);

    let environment = created_docker_environment(&backend, lease_engine, &vessel).await;
    assert_eq!(environment, ("rootless".to_string(), "flotilla-dev-env:latest".to_string(), DockerImagePullPolicy::Always));
}

// #4165: a cloud vessel clones into a directory of its own below its provider's
// workspace root, since vessels on the host's filesystem (drydock's `local`)
// share that root. One already cloned at the root itself keeps its checkout
// there, and its crew runs where the checkout is recorded. Its checkout carries
// the grants covering its repository (#4218).
#[tokio::test]
async fn a_cloud_vessel_clones_below_the_workspace_root_and_crews_run_at_the_recorded_checkout() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-cloud", "implement", REPO_URL, GIT_REF).await;
    let mut status = convoy.status.expect("convoy status");
    // #4218: the vessel holds a grant for this repository and one for another.
    let repository = convoy.spec.repositories[0].repo_ref.clone();
    let other = flotilla_resources::RepositoryKey("other".to_string());
    let requirement = &mut status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels[0];
    requirement.credential_refs = BTreeSet::from(["lab-forge".to_string(), "other-forge".to_string()]);
    requirement.credential_scopes = BTreeMap::from([
        ("lab-forge".to_string(), BTreeSet::from([repository.clone(), other.clone()])),
        ("other-forge".to_string(), BTreeSet::from([other.clone()])),
    ]);
    status.placement_decision = Some(PlacementDecision {
        provider_instance: Some("local".to_string()),
        provider_workspace_root: Some("/vessels".to_string()),
        minimal_alternatives: Vec::new(),
        escalation_reason: None,
        kind: "kind-cloud".to_string(),
        target_host: PlacementTargetHost { reference: CanonicalHostId::resolved(HOST_REF), display_name: HOST_REF.to_string() },
        refused_candidates: Vec::new(),
        viable_not_selected: Vec::new(),
        allocation: None,
    });
    backend.using::<Convoy>(NAMESPACE).update_status("convoy-cloud", &convoy.metadata.resource_version, &status).await.expect("decision");
    let image = format!("crew@sha256:{}", "a".repeat(64));
    create_kind(
        &backend,
        NAMESPACE,
        "kind-cloud",
        FulfilmentKindSpec::builder()
            .host_ref(HOST_REF.to_string())
            .pool("cleat".to_string())
            .realisation(FulfilmentRealisation::CloudSandboxPerVessel { image: Some(image.clone().into()), size: None })
            .build(),
    )
    .await;
    let vessel = launch_workspace(&backend, lease_engine, "convoy-cloud", "implement", "kind-cloud").await;
    let launch = vessel.metadata.name.clone();
    lease_engine.environments.put_ready(
        &launch_environment(&launch),
        &EnvironmentSpec {
            provider_address: Some("account:local/provider:local".parse().expect("address")),
            cloud_sandbox: Some(flotilla_resources::CloudSandboxEnvironmentSpec {
                image: image.clone(),
                image_build_ref: None,
                size: flotilla_resources::realisation_settings::CLOUD_SANDBOX_SIZE,
                lifetime_cap_seconds: None,
                probe_needs: Default::default(),
                env: Default::default(),
            }),
            ..Default::default()
        },
        Some(&launch),
        "sandbox-workspace-cloud",
        flotilla_resources::environment_records::LaunchFacts {
            image_ref: Some(image),
            local_image_id: Some(format!("sha256:{}", "a".repeat(64))),
            ..Default::default()
        },
    );

    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    let reconcile = |vessel| {
        let reconciler = &reconciler;
        async move {
            reconciler.prepare(vessel).await.expect("deps should load");
        }
    };
    admit_on_engine(&backend, NAMESPACE, &lease_engine.checkouts, "convoy-cloud").await;
    reconcile(&vessel).await;
    let requested = requested_checkouts(lease_engine);
    let (name, checkout) = requested
        .iter()
        .find_map(|(name, record)| match &record.spec {
            CheckoutSpec::FreshClone(spec) => Some((name.clone(), spec.clone())),
            _ => None,
        })
        .expect("a fresh clone should be requested");
    assert_eq!(checkout.target_path, format!("/vessels/{}", launch.replace('#', "-")));
    // The clone's grants are its holder convoy's to supply (#4218): the daemon's holder inputs
    // give a fresh clone the grants of the vessel whose environment it is cloned into.
    let _ = (repository, other);

    // The checkout was recorded at the root itself, by an earlier layout: that launch is the one
    // the engine holds now.
    lease_engine.checkouts.finish(&requested[&name].name, Ending::Released);
    create_ready_checkout(
        &lease_engine.checkouts,
        "convoy-cloud",
        ReadyCheckoutFixture::builder()
            .name(name)
            .env_ref(launch_environment(&launch))
            .git_ref(GIT_REF.to_string())
            .path("/vessels".to_string())
            .fresh_clone(flotilla_resources::FreshCloneCheckoutSpec { target_path: "/vessels".to_string(), ..checkout })
            .build(),
    );
    reconcile(&vessel).await;
    let cwd =
        requested_terminals(lease_engine).into_iter().map(|terminal| terminal.cwd).next().expect("a terminal session should be requested");
    assert_eq!(cwd, "/vessels");
}

// Discovery offers the canary adapter only where a vessel's environment enables it.
#[rstest]
#[case::canary("fleet-canary", Some("1"))]
#[case::ordinary("claude-code", None)]
#[tokio::test]
async fn docker_environment_enables_the_adapters_its_crew_requires(#[case] adapter: &str, #[case] enabled: Option<&str>) {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-adapter", "implement", REPO_URL, GIT_REF).await;
    let mut status = convoy.status.expect("convoy status");
    status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels[0].crew = vec![CrewSpec::builder()
        .role("probe".to_string())
        .source(CrewSource::Agent {
            selector: Selector { adapter: Some(adapter.to_string()), ..Selector::for_capability("code") },
            prompt: None,
            brief_template: None,
        })
        .build()];
    backend.using::<Convoy>(NAMESPACE).update_status("convoy-adapter", &convoy.metadata.resource_version, &status).await.expect("crew");
    create_docker_kind(
        &backend,
        NAMESPACE,
        DockerKindFixture::builder()
            .name("kind-adapter".to_string())
            .host_ref(HOST_REF.to_string())
            .pool("cleat".to_string())
            .image("flotilla-dev-env:latest".to_string())
            .build(),
    )
    .await;
    let vessel = launch_workspace(&backend, lease_engine, "convoy-adapter", "implement", "kind-adapter").await;
    create_ready_host_worktree(lease_engine, "convoy-adapter", &vessel.metadata.name);

    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(&vessel).await.expect("deps should load");
    let docker = requested_docker(lease_engine, &vessel.metadata.name).expect("docker environment should be created");
    assert_eq!(docker.required_agent_adapters, BTreeSet::from([adapter.to_string()]));
    assert_eq!(docker.env.get("FLOTILLA_FLEET_CANARY").map(String::as_str), enabled);
}

/// The provider instance, image and pull policy of the container environment `vessel` requests.
async fn created_docker_environment(
    backend: &ResourceBackend,
    lease_engine: &EngineFixture,
    vessel: &flotilla_resources::ResourceObject<Vessel>,
) -> (String, String, DockerImagePullPolicy) {
    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(vessel).await.expect("deps should load");
    let environment = requested_environment(lease_engine, &vessel.metadata.name).expect("docker environment should be created");
    // The provider instance is the provider its address names on the host.
    let instance = environment
        .spec
        .provider_address
        .as_ref()
        .and_then(|address| address.to_string().rsplit_once("/provider:").map(|(_, provider)| provider.to_owned()))
        .unwrap_or_default();
    let docker = environment.spec.docker.expect("docker environment");
    (instance, docker.image, docker.pull_policy)
}

#[tokio::test]
async fn missing_fulfilment_kind_marks_workspace_failed() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-a", "implement", REPO_URL, GIT_REF).await;
    let workspace = launch_workspace(&backend, lease_engine, "convoy-a", "implement", "kind-missing").await;

    let reconciler = test_vessel_reconciler(backend, NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("deps should load");

    assert!(failure(lease_engine, &workspace.metadata.name).contains("fulfilment kind kind-missing not found"));
}

#[tokio::test]
async fn reuses_existing_clone_by_deterministic_name() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-b", "implement", REPO_URL, GIT_REF).await;
    create_host_direct_kind(&backend, NAMESPACE, "policy-a", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");

    let canonical_repo = canonicalize_repo_url(REPO_URL).expect("repo canonicalization");
    let clone_name = format!("clone-{}", clone_key(&canonical_repo, &host_direct_env_name()));
    let launch =
        create_ready_clone(&lease_engine.clones, &clone_name, REPO_URL, &host_direct_env_name(), "/Users/alice/dev/flotilla-repos/clone");
    let workspace = launch_workspace(&backend, lease_engine, "convoy-b", "implement", "policy-a").await;

    admit_on_engine(&backend, NAMESPACE, &lease_engine.checkouts, "convoy-b").await;
    let reconciler = test_vessel_reconciler(backend, NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("deps should load");

    assert_eq!(requested_clones(lease_engine).len(), 1, "the ready clone is reused, not requested again");
    assert!(requested_checkouts(lease_engine).values().any(|record| matches!(
        &record.spec,
        CheckoutSpec::Worktree(worktree) if worktree.clone_ref == launch && worktree.base_ref.as_deref() == Some(GIT_REF)
    )));
}

#[tokio::test]
// Contained worktrees receive tracking defaults while the shared config and hooks stay read-only.
async fn docker_worktree_waits_for_checkout_before_creating_environment() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-c", "implement", REPO_URL, GIT_REF).await;
    create_docker_kind(
        &backend,
        NAMESPACE,
        DockerKindFixture::builder()
            .name("policy-worktree".to_string())
            .host_ref(HOST_REF.to_string())
            .pool("cleat".to_string())
            .image("ghcr.io/flotilla/dev:latest".to_string())
            .build(),
    )
    .await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");

    let canonical_repo = canonicalize_repo_url(REPO_URL).expect("repo canonicalization");
    let clone_name = format!("clone-{}", clone_key(&canonical_repo, &host_direct_env_name()));
    create_ready_clone(&lease_engine.clones, &clone_name, REPO_URL, &host_direct_env_name(), "/Users/alice/dev/flotilla-repos/clone");
    let workspace = launch_workspace(&backend, lease_engine, "convoy-c", "implement", "policy-worktree").await;

    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    admit_on_engine(&backend, NAMESPACE, &lease_engine.checkouts, "convoy-c").await;
    reconciler.prepare(&workspace).await.expect("deps should load");
    let requested = requested_checkouts(lease_engine);
    assert!(requested.contains_key("checkout-convoy-c"), "the checkout is requested first");
    assert!(requested_environment(lease_engine, &workspace.metadata.name).is_none());

    // An earlier layout made it at another path: its launch is realised there.
    lease_engine.checkouts.finish(&requested["checkout-convoy-c"].name, Ending::Released);
    create_ready_checkout(
        &lease_engine.checkouts,
        "convoy-c",
        ReadyCheckoutFixture::builder()
            .name("checkout-convoy-c".to_string())
            .env_ref(host_direct_env_name())
            .git_ref(GIT_REF.to_string())
            .path("/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-c".to_string())
            .maybe_worktree(Some(worktree_checkout_spec(
                &host_direct_env_name(),
                GIT_REF,
                "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-c",
                &clone_name,
            )))
            .build(),
    );
    let current = read_vessel(&backend, lease_engine, &workspace.metadata.name).await;
    reconciler.prepare(&current).await.expect("deps should reload");
    let requested = requested_docker(lease_engine, &workspace.metadata.name);

    assert!(matches!(
            &requested,
            Some(docker)
                if docker.mounts.as_slice() == [
                    flotilla_resources::EnvironmentMount {
                        source_path: "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-c".to_string(),
                        target_path: "/workspace".to_string(),
                        mode: flotilla_resources::EnvironmentMountMode::Rw,
                    },
                    flotilla_resources::EnvironmentMount {
                        source_path: "/Users/alice/dev/flotilla-repos/clone/.git".to_string(),
                        target_path: "/Users/alice/dev/flotilla-repos/clone/.git".to_string(),
                        mode: flotilla_resources::EnvironmentMountMode::Rw,
                    },
                    flotilla_resources::EnvironmentMount {
                        source_path: "/Users/alice/dev/flotilla-repos/clone/.git/config".to_string(),
                        target_path: "/Users/alice/dev/flotilla-repos/clone/.git/config".to_string(),
                        mode: flotilla_resources::EnvironmentMountMode::Ro,
                    },
                    flotilla_resources::EnvironmentMount {
                        source_path: "/Users/alice/dev/flotilla-repos/clone/.git/hooks".to_string(),
                        target_path: "/Users/alice/dev/flotilla-repos/clone/.git/hooks".to_string(),
                        mode: flotilla_resources::EnvironmentMountMode::Ro,
                    },
                    flotilla_resources::EnvironmentMount {
                        source_path: "/Users/alice/dev/flotilla-repos/clone/.git/worktrees".to_string(),
                        target_path: "/Users/alice/dev/flotilla-repos/clone/.git/worktrees".to_string(),
                        mode: flotilla_resources::EnvironmentMountMode::Ro,
                    },
                    flotilla_resources::EnvironmentMount {
                        source_path: "/Users/alice/dev/flotilla-repos/clone/.git/worktrees/resolved-github-com-flotilla-org-flotilla.workspace-c".to_string(),
                        target_path: "/Users/alice/dev/flotilla-repos/clone/.git/worktrees/resolved-github-com-flotilla-org-flotilla.workspace-c".to_string(),
                        mode: flotilla_resources::EnvironmentMountMode::Rw,
                    },
                ]
    ));

    #[cfg(unix)]
    let docker = requested.as_ref().expect("contained environment");
    #[cfg(unix)]
    assert_contained_tracking(&docker.env);
}

#[cfg(unix)]
fn assert_contained_tracking(env: &BTreeMap<String, String>) {
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
        process::Command,
    };

    // Restore permissions before TempDir drops, including when an assertion panics.
    struct RestoreDirectories(Vec<(PathBuf, fs::Permissions)>);
    impl Drop for RestoreDirectories {
        fn drop(&mut self) {
            for (path, permissions) in &self.0 {
                // Cleanup must preserve the original assertion failure.
                let _ = fs::set_permissions(path, permissions.clone());
            }
        }
    }

    // Real Git is required here: tracking writes and lock/rename are the process boundary behind #2516.
    let temp = tempfile::tempdir().expect("Git fixture");
    let root = temp.path();
    let git = |cwd: &Path, args: &[&str], contained: bool| {
        let mut command = Command::new("git");
        command
            .current_dir(cwd)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        if contained {
            command.envs(env);
        }
        let output = command.output().expect("run Git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).expect("Git UTF-8 output")
    };
    git(root, &["init", "--bare", "remote"], false);
    git(root, &["clone", "remote", "clone"], false);
    let clone = root.join("clone");
    git(&clone, &["commit", "--allow-empty", "-m", "base"], false);
    git(&clone, &["worktree", "add", "-b", GIT_REF, "../work"], false);
    let work = root.join("work");
    let config = clone.join(".git/config");
    let hooks = clone.join(".git/hooks");
    let hook = hooks.join("protected");
    fs::write(&hook, "host hook").expect("host hook");
    let original_config = fs::read(&config).expect("shared config");
    let _restore_directories = RestoreDirectories(
        [clone.join(".git"), hooks.clone()]
            .into_iter()
            .map(|path| {
                let permissions = fs::metadata(&path).expect("original directory permissions").permissions();
                (path, permissions)
            })
            .collect(),
    );
    // Directory permissions model the mount's lock/rename refusal; the existing
    // reconciliation assertion separately verifies the actual read-only overlays.
    for path in [&config, &hook] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o444)).expect("protect file");
    }
    for path in [clone.join(".git"), hooks.clone()] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o555)).expect("protect directory");
    }
    // A newly created file has the test process's effective uid. Root can bypass
    // permission bits, so its protection coverage comes from the read-only mount
    // contract above; tracking and unchanged-content checks still run below.
    if fs::metadata(&config).expect("fixture owner").uid() != 0 {
        assert!(fs::write(&config, "changed").is_err(), "shared config is unwritable");
        assert!(fs::write(&hook, "changed").is_err(), "shared hook is unwritable");
        assert!(fs::write(hooks.join("new-hook"), "changed").is_err(), "new hooks are refused");
    }

    // The chosen equivalent to push -u is provisioned tracking plus plain push,
    // including the first publication of a branch that does not exist remotely.
    git(&work, &["push"], true);
    assert_eq!(git(&work, &["rev-parse", "--abbrev-ref", "@{upstream}"], true).trim(), format!("origin/{GIT_REF}"));
    git(&work, &["commit", "--allow-empty", "-m", "crew change"], true);
    git(&work, &["push"], true);
    git(root, &["clone", "--branch", GIT_REF, "remote", "peer"], false);
    let peer = root.join("peer");
    git(&peer, &["commit", "--allow-empty", "-m", "peer change"], false);
    git(&peer, &["push"], false);
    git(&work, &["pull", "--ff-only"], true);
    assert_eq!(git(&work, &["rev-parse", "HEAD"], true), git(&peer, &["rev-parse", "HEAD"], false));
    assert_eq!(fs::read(&config).expect("read protected config"), original_config);
    assert_eq!(fs::read_to_string(&hook).expect("read protected hook"), "host hook");
}

#[tokio::test]
async fn docker_worktree_rejects_an_adopted_checkout_without_shared_clone_metadata() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-adopted-docker", "implement", REPO_URL, GIT_REF).await;
    let repo_ref = convoy.spec.repositories[0].repo_ref.clone();
    create_docker_kind(
        &backend,
        NAMESPACE,
        DockerKindFixture::builder()
            .name("policy-adopted-docker".to_string())
            .host_ref(HOST_REF.to_string())
            .pool("cleat".to_string())
            .image("ghcr.io/flotilla/dev:latest".to_string())
            .build(),
    )
    .await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    create_ready_adopted_checkout(lease_engine, "adopted-checkout-convoy-adopted-docker", "/Users/alice/dev/flotilla-existing");
    adopt_checkouts(&backend, "convoy-adopted-docker", BTreeMap::from([(repo_ref, "adopted-checkout-convoy-adopted-docker".to_string())]))
        .await;
    let workspace = launch_workspace(&backend, lease_engine, "convoy-adopted-docker", "implement", "policy-adopted-docker").await;

    let reconciler = test_vessel_reconciler(backend, NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("dependencies should resolve to a vessel failure");

    assert!(failure(lease_engine, &workspace.metadata.name)
        .contains("requires checkout adopted-checkout-convoy-adopted-docker to be a managed worktree"));
}

#[tokio::test]
async fn docker_worktree_reports_missing_shared_clone_metadata_as_a_vessel_failure() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-missing-clone", "implement", REPO_URL, GIT_REF).await;
    create_docker_kind(
        &backend,
        NAMESPACE,
        DockerKindFixture::builder()
            .name("policy-missing-clone".to_string())
            .host_ref(HOST_REF.to_string())
            .pool("cleat".to_string())
            .image("ghcr.io/flotilla/dev:latest".to_string())
            .build(),
    )
    .await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    create_ready_checkout(
        &lease_engine.checkouts,
        "convoy-missing-clone",
        ReadyCheckoutFixture::builder()
            .name("checkout-convoy-missing-clone".to_string())
            .env_ref(host_direct_env_name())
            .git_ref(GIT_REF.to_string())
            .path("/Users/alice/dev/flotilla-repos/convoy-missing-clone/flotilla.feat-task-provisioning".to_string())
            .maybe_worktree(Some(worktree_checkout_spec(
                &host_direct_env_name(),
                GIT_REF,
                "/Users/alice/dev/flotilla-repos/convoy-missing-clone/flotilla.feat-task-provisioning",
                "clone-missing",
            )))
            .build(),
    );
    let workspace = launch_workspace(&backend, lease_engine, "convoy-missing-clone", "implement", "policy-missing-clone").await;

    let reconciler = test_vessel_reconciler(backend, NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("dependencies should resolve to a vessel failure");

    assert!(failure(lease_engine, &workspace.metadata.name).contains("checkout-convoy-missing-clone refers to missing clone clone-missing"));
}

#[rstest]
#[case::host_direct(
    FulfilmentRealisation::HostDirect,
    "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-host",
    "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-host",
    None
)]
#[case::docker_worktree(
    FulfilmentRealisation::DockerPerVessel { image: Some("ghcr.io/flotilla/dev:latest".into()) },
    "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-docker-worktree",
    "/workspace",
    Some(docker_worktree_environment("workspace-docker-worktree", BTreeSet::from(["codex".to_string()]))),
)]
#[tokio::test]
async fn terminal_sessions_use_strategy_specific_cwd(
    #[case] realisation: FulfilmentRealisation,
    #[case] checkout_path: &str,
    #[case] expected_cwd: &str,
    #[case] docker_env: Option<DockerEnvironmentSpec>,
) {
    assert_terminal_cwd_for_strategy(realisation, checkout_path, expected_cwd, docker_env).await;
}

fn create_ready_host_worktree(lease_engine: &EngineFixture, convoy_name: &str, workspace_name: &str) {
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    let canonical_repo = canonicalize_repo_url(REPO_URL).expect("repo canonicalization");
    let clone_name = format!("clone-{}", clone_key(&canonical_repo, &host_direct_env_name()));
    create_ready_clone(&lease_engine.clones, &clone_name, REPO_URL, &host_direct_env_name(), "/Users/alice/dev/flotilla-repos/clone");
    let path = format!("/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.{workspace_name}");
    create_ready_checkout(
        &lease_engine.checkouts,
        convoy_name,
        ReadyCheckoutFixture::builder()
            .name(format!("checkout-{convoy_name}"))
            .env_ref(host_direct_env_name())
            .git_ref(GIT_REF.to_string())
            .path(path.clone())
            .maybe_worktree(Some(worktree_checkout_spec(&host_direct_env_name(), GIT_REF, &path, &clone_name)))
            .build(),
    );
}

fn docker_worktree_environment(workspace_name: &str, required_agent_adapters: BTreeSet<String>) -> DockerEnvironmentSpec {
    let mount = |source: String, target: &str, mode| flotilla_resources::EnvironmentMount {
        target_path: if target.is_empty() { source.clone() } else { target.to_string() },
        source_path: source,
        mode,
    };
    let clone = "/Users/alice/dev/flotilla-repos/clone/.git";
    DockerEnvironmentSpec {
        image_build_ref: None,
        host_ref: HOST_REF.to_string(),
        image: "ghcr.io/flotilla/dev:latest".to_string(),
        declared_agent_adapters: Default::default(),
        required_agent_adapters,
        pull_policy: Default::default(),
        mounts: vec![
            mount(
                format!("/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.{workspace_name}"),
                "/workspace",
                flotilla_resources::EnvironmentMountMode::Rw,
            ),
            mount(clone.to_string(), "", flotilla_resources::EnvironmentMountMode::Rw),
            mount(format!("{clone}/config"), "", flotilla_resources::EnvironmentMountMode::Ro),
            mount(format!("{clone}/hooks"), "", flotilla_resources::EnvironmentMountMode::Ro),
            mount(format!("{clone}/worktrees"), "", flotilla_resources::EnvironmentMountMode::Ro),
            mount(
                format!("{clone}/worktrees/resolved-github-com-flotilla-org-flotilla.{workspace_name}"),
                "",
                flotilla_resources::EnvironmentMountMode::Rw,
            ),
        ],
        env: Default::default(),
    }
}

#[tokio::test]
async fn child_failure_propagates_to_workspace_failure() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-f", "implement", REPO_URL, GIT_REF).await;
    create_host_direct_kind(&backend, NAMESPACE, "policy-f", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");

    let canonical_repo = canonicalize_repo_url(REPO_URL).expect("repo canonicalization");
    let clone_name = format!("clone-{}", clone_key(&canonical_repo, &host_direct_env_name()));
    create_ready_clone(&lease_engine.clones, &clone_name, REPO_URL, &host_direct_env_name(), "/Users/alice/dev/flotilla-repos/clone");
    create_ready_checkout(
        &lease_engine.checkouts,
        "convoy-f",
        ReadyCheckoutFixture::builder()
            .name("checkout-convoy-f".to_string())
            .env_ref(host_direct_env_name())
            .git_ref(GIT_REF.to_string())
            .path("/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-f".to_string())
            .maybe_worktree(Some(worktree_checkout_spec(
                &host_direct_env_name(),
                GIT_REF,
                "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-f",
                "clone-placeholder",
            )))
            .build(),
    );
    let workspace = launch_workspace(&backend, lease_engine, "convoy-f", "implement", "policy-f").await;
    let session = format!("terminal-{}-coder", workspace.metadata.name);
    let stopped = lease_engine.terminals.request(&flotilla_resources::terminal_records::TerminalLaunchRequest {
        session: session.clone(),
        environment: host_direct_env_name(),
        vessel: Some(workspace.metadata.name.clone()),
        role: "coder".to_string(),
        pool: "cleat".to_string(),
        cwd: "/workspace".to_string(),
        env: Default::default(),
        source: flotilla_resources::terminal_records::TerminalLaunchSource::Tool { command: "cargo test".to_string() },
    });
    lease_engine.terminals.ready(&stopped, &format!("session-{session}"), &Default::default());
    lease_engine.terminals.failed(&stopped, "boom");

    let reconciler = test_vessel_reconciler(backend, NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("deps should load");

    assert_eq!(failure(lease_engine, &workspace.metadata.name), "boom");
}

#[rstest]
#[case(WorkPhase::Running, CrewWorkPhase::Working)]
#[case(WorkPhase::Launching, CrewWorkPhase::Pending)]
#[tokio::test]
async fn disappeared_live_agent_session_interrupts_the_vessel_and_requests_a_restart(
    #[case] work_phase: WorkPhase,
    #[case] crew_phase: CrewWorkPhase,
) {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-recover", "implement", REPO_URL, GIT_REF).await;
    let mut status = convoy.status.expect("convoy status");
    status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels[0].crew[0].source = CrewSource::Agent {
        selector: Selector::for_capability("coding"),
        prompt: Some("Finish the issue.".to_string()),
        brief_template: None,
    };
    status.phase = ConvoyPhase::Active;
    status.work.insert("implement".to_string(), WorkState::builder().phase(work_phase).build());
    status
        .crew_work
        .insert("implement".to_string(), BTreeMap::from([("coder".to_string(), CrewWorkState::builder().phase(crew_phase).build())]));
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-recover", &convoy.metadata.resource_version, &status)
        .await
        .expect("agent crew status");
    create_host_direct_kind(&backend, NAMESPACE, "policy-recover", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    let clone_name =
        format!("clone-{}", clone_key(&canonicalize_repo_url(REPO_URL).expect("repo canonicalization"), &host_direct_env_name()));
    create_ready_clone(&lease_engine.clones, &clone_name, REPO_URL, &host_direct_env_name(), "/tmp/clone");
    let checkout_path = "/Users/alice/dev/flotilla-repos/convoy-recover";
    let ready_checkout = |checkouts: &flotilla_provisioning_testkit::CheckoutFixture| {
        create_ready_checkout(
            checkouts,
            "convoy-recover",
            ReadyCheckoutFixture::builder()
                .name("checkout-convoy-recover".to_string())
                .env_ref(host_direct_env_name())
                .git_ref(GIT_REF.to_string())
                .path(checkout_path.to_string())
                .maybe_worktree(Some(worktree_checkout_spec(&host_direct_env_name(), GIT_REF, checkout_path, &clone_name)))
                .build(),
        )
    };
    ready_checkout(&lease_engine.checkouts);
    let workspace = launch_workspace(&backend, lease_engine, "convoy-recover", "implement", "policy-recover").await;
    lease_engine.vessels.ready(&workspace.metadata.name, &host_direct_env_name());
    let mut request = flotilla_provisioning_testkit::TerminalsFixture::agent(&workspace.metadata.name, "coder", &host_direct_env_name());
    let terminal_name = request.session.clone();
    request.cwd = checkout_path.to_string();
    let first = lease_engine.terminals.request(&request);
    lease_engine.terminals.ready(&first, &terminal_name, &Default::default());
    lease_engine.terminals.exited(&first, "agent exited with code 0");
    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    let workspace = read_vessel(&backend, lease_engine, &workspace.metadata.name).await;
    reconciler.prepare(&workspace).await.expect("deps should load");

    let status = read_vessel(&backend, lease_engine, &workspace.metadata.name).await.status.expect("vessel status");
    assert_eq!(status.phase, VesselPhase::Interrupted);
    assert_eq!(status.interrupted_roles, BTreeSet::from(["coder".to_string()]));
    assert!(status.message.as_deref().is_some_and(|message| message.contains("disappeared") && message.contains("relaunching")));
    assert_eq!(
        lease_engine.terminals.record(&terminal_name).name,
        first,
        "the session must not restart before its interruption contradicts the durable Running work state"
    );

    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let convoy = convoys.get("convoy-recover").await.expect("convoy after vessel interruption");
    let mut status = convoy.status.expect("convoy status");
    status.work.get_mut("implement").expect("work").phase = WorkPhase::Interrupted;
    status.crew_work.get_mut("implement").expect("crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Interrupted;
    convoys.update_status("convoy-recover", &convoy.metadata.resource_version, &status).await.expect("persist work interruption");
    let workspace = read_vessel(&backend, lease_engine, &workspace.metadata.name).await;
    reconciler.prepare(&workspace).await.expect("recovery deps should load");

    let newest = lease_engine.terminals.record(&terminal_name);
    assert_ne!(newest.name, first, "the interrupted crew's next launch is requested");
    assert_eq!(newest.reading, flotilla_protocol::LifecycleReading::InProgress);
}

#[tokio::test]
async fn adopted_checkout_ref_reuses_checkout_without_creating_clone_or_checkout() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-adopted", "implement", REPO_URL, GIT_REF).await;
    let repo_ref = convoy.spec.repositories[0].repo_ref.clone();
    create_host_direct_kind(&backend, NAMESPACE, "policy-adopted", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    create_ready_adopted_checkout(lease_engine, "adopted-checkout-convoy-adopted", "/Users/alice/dev/flotilla-existing");
    adopt_checkouts(&backend, "convoy-adopted", BTreeMap::from([(repo_ref, "adopted-checkout-convoy-adopted".to_string())])).await;
    let workspace = launch_workspace(&backend, lease_engine, "convoy-adopted", "implement", "policy-adopted").await;

    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("deps should load");

    assert_eq!(requested_checkouts(lease_engine).len(), 1, "an adopted checkout requests no checkout");
    assert!(requested_clones(lease_engine).is_empty(), "an adopted checkout requests no clone");
    assert!(requested_terminals(lease_engine).iter().any(|terminal| {
        terminal.environment == host_direct_env_name() && terminal.pool == "cleat" && terminal.cwd == "/Users/alice/dev/flotilla-existing"
    }));

    let adopted = requested_checkouts(lease_engine).remove("adopted-checkout-convoy-adopted").expect("adopted checkout");
    lease_engine.checkouts.finish(&adopted.name, Ending::Lost(""));
    reconciler.prepare(&workspace).await.expect("gone checkout");
    let reading = reading(lease_engine, &workspace.metadata.name);
    assert!(reading.reason().is_some_and(|reason| reason.contains("adopted checkout") && reason.contains("gone")), "{reading:?}");
}

#[tokio::test]
async fn first_agent_is_provisioned_with_a_durable_crew_brief_while_later_agents_remain_latent() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-crew", "implement", REPO_URL, GIT_REF).await;
    let repo_ref = convoy.spec.repositories[0].repo_ref.clone();
    let mut status = convoy.status.expect("convoy status");
    status.workflow_snapshot.as_mut().expect("workflow snapshot").exit = Some(ExitDeclaration::Claim(ClaimExit));
    status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels[0].crew = vec![
        CrewSpec::builder()
            .role("coder".to_string())
            .source(CrewSource::Agent {
                selector: Selector::for_capability("coding"),
                prompt: Some("Implement issue 668.".to_string()),
                brief_template: None,
            })
            .build(),
        CrewSpec::builder()
            .role("reviewer".to_string())
            .source(CrewSource::Agent {
                selector: Selector::for_capability("review"),
                prompt: Some("Review the coder's work.".to_string()),
                brief_template: None,
            })
            .build(),
    ];
    status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels[0].credential_scopes =
        BTreeMap::from([("github-app".to_string(), BTreeSet::from([repo_ref.clone()]))]);
    status.crew_work.insert(
        "implement".into(),
        BTreeMap::from([
            ("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
            ("reviewer".into(), CrewWorkState::builder().phase(CrewWorkPhase::Pending).build()),
        ]),
    );
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-crew", &convoy.metadata.resource_version, &status)
        .await
        .expect("update convoy crew");
    create_host_direct_kind(&backend, NAMESPACE, "policy-crew", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    create_ready_adopted_checkout(lease_engine, "adopted-checkout-convoy-crew", "/Users/alice/dev/flotilla-existing");
    adopt_checkouts(&backend, "convoy-crew", BTreeMap::from([(repo_ref.clone(), "adopted-checkout-convoy-crew".to_string())])).await;
    let workspace = launch_workspace(&backend, lease_engine, "convoy-crew", "implement", "policy-crew").await;

    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("deps");

    let coder = requested_terminal(lease_engine, "coder").expect("coder session requested");
    let TerminalLaunchSource::Agent { capability, brief, .. } = &coder.source else {
        panic!("expected structured agent launch");
    };
    assert_eq!(capability, "coding");
    assert_eq!(brief.path, ".flotilla/briefs/coder.md");
    assert!(brief.content.contains("You are `coder` in convoy `convoy-crew`"));
    assert!(brief.content.contains("- `coder`: active"));
    assert!(brief.content.contains("- `reviewer`: latent"));
    assert!(brief.content.contains("flotilla crew reviewer handoff --message"));
    assert!(brief.content.contains("flotilla crew submit"));
    assert!(brief.content.contains("- Minted credential repository scope:"));
    assert!(brief.content.contains(&format!("  - `github-app`:\n    - `{repo_ref}` — {REPO_URL}")));
    assert!(brief.content.contains("park the verified commit"));
    assert!(brief.content.contains("## Assignment\n\nImplement issue 668.\n"));
    assert_eq!(coder.vessel.as_deref(), Some(workspace.metadata.name.as_str()));
    assert!(requested_terminal(lease_engine, "reviewer").is_none(), "a latent reviewer is not requested");

    // A cross-vessel handoff activates work before publishing its Message. The Vessel
    // controller must then provision the formerly latent receiver without embedding input.
    lease_engine.terminals.ready(&coder.name, "session-coder", &Default::default());
    flotilla_store::apply_status_patch(
        &backend.using::<Convoy>(NAMESPACE),
        "convoy-crew",
        &flotilla_resources::external_patches::resume_crew_work(
            "implement".into(),
            "reviewer".into(),
            Utc::now(),
            "review carried revision".into(),
            Some("handoff-message".into()),
            ResumeIntent::Reopen,
        ),
    )
    .await
    .expect("handoff activation");
    reconciler.prepare(&workspace).await.expect("activated deps");
    let reviewer = requested_terminal(lease_engine, "reviewer").expect("activated reviewer must be provisioned");
    let TerminalLaunchSource::Agent { brief, .. } = &reviewer.source else {
        panic!("reviewer agent launch");
    };
    assert!(brief.content.contains("- `reviewer`: active"));
}

#[tokio::test]
async fn vessel_waits_for_admission_brief_envelope_then_pins_its_digest() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-artifact", "implement", REPO_URL, GIT_REF).await;
    let repo_ref = convoy.spec.repositories[0].repo_ref.clone();
    let mut status = convoy.status.expect("convoy status");
    status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels[0].crew = vec![CrewSpec::builder()
        .role("coder".to_string())
        .source(CrewSource::Agent {
            selector: Selector::for_capability("coding"),
            prompt: Some("Implement this task.".to_string()),
            brief_template: None,
        })
        .build()];
    backend.using::<Convoy>(NAMESPACE).update_status("convoy-artifact", &convoy.metadata.resource_version, &status).await.expect("crew");
    patch_resource_annotation(&backend, NAMESPACE, "Convoy", "convoy-artifact", BRIEF_ARTIFACTS_ANNOTATION, "true")
        .await
        .expect("artifact admission marker");
    create_host_direct_kind(&backend, NAMESPACE, "policy-artifact", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    create_ready_adopted_checkout(lease_engine, "adopted-artifact", "/Users/alice/dev/flotilla-existing");
    adopt_checkouts(&backend, "convoy-artifact", BTreeMap::from([(repo_ref, "adopted-artifact".to_string())])).await;
    let vessel = launch_workspace(&backend, lease_engine, "convoy-artifact", "implement", "policy-artifact").await;
    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(&vessel).await.expect("wait for artifact");
    assert!(requested_terminals(lease_engine).is_empty(), "no session before its brief envelope");

    let name = flotilla_resources::address::record_name(
        &flotilla_resources::address::artifact_address("convoy-artifact", "coder", "brief", "convoy-artifact").expect("artifact address"),
    )
    .expect("record name");
    let digest = "a".repeat(64);
    backend
        .using::<Artifact>(NAMESPACE)
        .create(
            &meta(&name),
            &ArtifactSpec::builder()
                .convoy("convoy-artifact".to_string())
                .producer("coder".to_string())
                .kind("brief".to_string())
                .subject("convoy-artifact".to_string())
                .digest(digest.clone())
                .size(12)
                .media_type("text/markdown".to_string())
                .expires_at(Utc::now() + chrono::Duration::days(3650))
                .build(),
        )
        .await
        .expect("replicated brief envelope");
    reconciler.prepare(&vessel).await.expect("artifact available");
    let coder = requested_terminal(lease_engine, "coder").expect("session requested from brief artifact");
    let TerminalLaunchSource::Agent { brief, .. } = &coder.source else { panic!("agent session") };
    assert_eq!(brief.path, ".flotilla/briefs/coder.md");
    assert_eq!(brief.artifact_digest.as_deref(), Some(digest.as_str()));
    assert!(brief.content.is_empty());
}

#[tokio::test]
async fn repo_level_brief_template_override_changes_one_block_for_that_repo_convoy() {
    let lease_engine = &EngineFixture::default();
    let temp = tempfile::tempdir().expect("tempdir");
    let repo_path = temp.path().join("repo");
    let override_dir = repo_path.join(".flotilla/brief-templates");
    std::fs::create_dir_all(&override_dir).expect("override dir");
    std::fs::write(
        override_dir.join("crew.md"),
        "{% block delivery %}Repo-local delivery gate: stop after the fixture proves the override.{% endblock %}",
    )
    .expect("override file");

    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-repo-override", "implement", REPO_URL, GIT_REF).await;
    let repo_ref = convoy.spec.repositories[0].repo_ref.clone();
    let mut status = convoy.status.expect("convoy status");
    status.workflow_snapshot.as_mut().expect("workflow snapshot").exit = Some(ExitDeclaration::Claim(ClaimExit));
    status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels[0].crew = vec![CrewSpec::builder()
        .role("coder".to_string())
        .source(CrewSource::Agent { selector: Selector::for_capability("coding"), prompt: None, brief_template: None })
        .build()];
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-repo-override", &convoy.metadata.resource_version, &status)
        .await
        .expect("update convoy crew");
    create_host_direct_kind(&backend, NAMESPACE, "policy-repo-override", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    create_ready_adopted_checkout(
        lease_engine,
        "adopted-checkout-convoy-repo-override",
        repo_path.to_str().expect("repo path should be utf-8"),
    );
    adopt_checkouts(&backend, "convoy-repo-override", BTreeMap::from([(repo_ref, "adopted-checkout-convoy-repo-override".to_string())]))
        .await;
    let workspace = launch_workspace(&backend, lease_engine, "convoy-repo-override", "implement", "policy-repo-override").await;

    let reconciler = test_vessel_reconciler(backend, NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("deps");

    let coder = requested_terminal(lease_engine, "coder").expect("coder session requested");
    let TerminalLaunchSource::Agent { brief, .. } = &coder.source else {
        panic!("expected agent source");
    };
    assert!(brief.content.contains("Repo-local delivery gate: stop after the fixture proves the override."));
    assert!(brief.content.contains("## Assignment\n\nNo assignment was provided with this dispatch."));
    assert!(!brief.content.contains("For assignments that change a repository"));
}

#[tokio::test]
async fn issue_carrying_convoy_without_prompt_assigns_the_issue_in_the_brief() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository");
    let repository_key = repository_spec.key();
    ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &repository_key, &repository_spec).await.expect("repository create");
    let convoy = backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .create(
            &meta("convoy-issue-brief"),
            &ConvoySpec {
                continuation: None,
                subjects: Vec::new(),
                role: String::new(),
                generation: 1,
                workflow_ref: "wf".to_string(),
                dispatching_principal_ref: Default::default(),
                inputs: BTreeMap::new(),
                placement_policy: None,
                repositories: vec![ConvoyRepositorySpec {
                    url: "https://github.com/flotilla-org/flotilla".to_string(),
                    repo_ref: repository_key.clone(),
                    source_ref: "main".to_string(),
                    target_ref: "main".to_string(),
                    workspace_slug: "flotilla".to_string(),
                    subpaths: Vec::new(),
                }],
                r#ref: Some("feat/issue-brief".to_string()),
                project_ref: NAMESPACE.to_string(),
                adopted_checkout_refs: BTreeMap::new(),
                issues: vec![ConvoyIssue {
                    reference: IssueRef {
                        source: IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() },
                        id: "811".into(),
                    },
                    repository_ref: Some(repository_key.clone()),
                    snapshot: IssueSnapshot {
                        title: "Attention signal".into(),
                        body: Some("The issue body is the contract.".into()),
                        state: IssueState::Open,
                        labels: vec!["enhancement".into()],
                        as_of: "2026-07-22T09:30:00Z".parse().expect("timestamp"),
                    },
                }],
                change_request: None,
                instruction: None,
                promises: Vec::new(),
            },
        )
        .await
        .expect("convoy create");
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status(
            "convoy-issue-brief",
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: interactive_single_workflow_spec().vessels,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("convoy status update");
    create_host_direct_kind(&backend, NAMESPACE, "policy-issue-brief", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    create_ready_adopted_checkout(lease_engine, "adopted-checkout-issue-brief", "/Users/alice/dev/flotilla-existing");
    adopt_checkouts(&backend, "convoy-issue-brief", BTreeMap::from([(repository_key, "adopted-checkout-issue-brief".to_string())])).await;
    let workspace = launch_workspace(&backend, lease_engine, "convoy-issue-brief", "work", "policy-issue-brief").await;

    let reconciler = test_vessel_reconciler(backend, NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("deps");

    let coder = requested_terminal(lease_engine, "coder").expect("coder session requested");
    let TerminalLaunchSource::Agent { brief, .. } = &coder.source else {
        panic!("expected structured agent launch");
    };
    assert!(brief.content.contains("## Assignment\n\nYour assignment is the issue snapshot section below."));
    assert!(brief.content.contains("For interactive sessions, keep the user-facing loop tight"));
    assert!(brief.content.contains("## Issue snapshot"));
    assert!(brief.content.contains("The issue body is the contract."));
    assert!(!brief.content.contains("No assignment was provided"));
}

#[tokio::test]
async fn observed_checkout_at_managed_name_marks_workspace_failed() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-observed", "implement", REPO_URL, GIT_REF).await;
    create_host_direct_kind(&backend, NAMESPACE, "policy-observed", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");

    let canonical_repo = canonicalize_repo_url(REPO_URL).expect("repo canonicalization");
    let clone_name = format!("clone-{}", clone_key(&canonical_repo, &host_direct_env_name()));
    create_ready_clone(&lease_engine.clones, &clone_name, REPO_URL, &host_direct_env_name(), "/Users/alice/dev/flotilla-repos/clone");
    create_ready_adopted_checkout(lease_engine, "checkout-convoy-observed", "/Users/alice/dev/flotilla-existing");
    let workspace = launch_workspace(&backend, lease_engine, "convoy-observed", "implement", "policy-observed").await;

    let reconciler = test_vessel_reconciler(backend, NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("deps should load");

    assert_eq!(failure(lease_engine, &workspace.metadata.name), "checkout checkout-convoy-observed is adopted");
}

// Deleted run_finalizer_preserves_checkout, run_finalizer_preserves_adopted_checkout and
// run_finalizer_ignores_missing_children_and_cleans_partial_workspace: a vessel is now a read view with no finalizer (#3108).
// Deleted completed_convoy_does_not_repeat_vessel_delete_while_its_finalizer_is_pending: a convoy no longer deletes its vessels; their launches are released at their home (#3108).

#[tokio::test]
async fn terminal_session_request_names_its_vessel_and_credential_scopes() {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_labeled_processes(&backend, NAMESPACE, "convoy-labels", REPO_URL, GIT_REF).await;
    create_host_direct_kind(&backend, NAMESPACE, "policy-labels", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");

    let canonical_repo = canonicalize_repo_url(REPO_URL).expect("repo canonicalization");
    let clone_name = format!("clone-{}", clone_key(&canonical_repo, &host_direct_env_name()));
    create_ready_clone(&lease_engine.clones, &clone_name, REPO_URL, &host_direct_env_name(), "/Users/alice/dev/flotilla-repos/clone");
    create_ready_checkout(
        &lease_engine.checkouts,
        "convoy-labels",
        ReadyCheckoutFixture::builder()
            .name("checkout-convoy-labels".to_string())
            .env_ref(host_direct_env_name())
            .git_ref(GIT_REF.to_string())
            .path("/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-labels".to_string())
            .maybe_worktree(Some(worktree_checkout_spec(
                &host_direct_env_name(),
                GIT_REF,
                "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-labels",
                "clone-placeholder",
            )))
            .build(),
    );
    let workspace = launch_workspace(&backend, lease_engine, "convoy-labels", "review", "policy-labels").await;
    create_running_terminal(
        lease_engine,
        &workspace.metadata.name,
        &host_direct_env_name(),
        "build",
        "cargo check",
        "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-labels",
        "cleat",
    )
    .await;

    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("deps should load");

    let terminal = requested_terminal(lease_engine, "test").expect("terminal session should be requested");
    assert_eq!(terminal.vessel.as_deref(), Some(workspace.metadata.name.as_str()));
    // Its credentials are delivered at launch from its vessel's requirement.
    let requirement =
        flotilla_crew::reconcilers::vessel::vessel_requirement(&backend, NAMESPACE, &workspace.metadata.name).await.expect("requirement");
    assert_eq!(
        requirement.credential_scopes,
        BTreeMap::from([("github-app".to_string(), BTreeSet::from([RepositorySpec::remote(REPO_URL).expect("repository spec").key()]),)])
    );
}

async fn assert_terminal_cwd_for_strategy(
    realisation: FulfilmentRealisation,
    checkout_path: &str,
    expected_cwd: &str,
    docker_env: Option<DockerEnvironmentSpec>,
) {
    let lease_engine = &EngineFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-cwd", "implement", REPO_URL, GIT_REF).await;
    create_kind(
        &backend,
        NAMESPACE,
        "kind-cwd",
        FulfilmentKindSpec::builder().host_ref(HOST_REF.to_string()).pool("cleat".to_string()).realisation(realisation).build(),
    )
    .await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");

    let canonical_repo = canonicalize_repo_url(REPO_URL).expect("repo canonicalization");
    let clone_name = format!("clone-{}", clone_key(&canonical_repo, &host_direct_env_name()));
    create_ready_clone(&lease_engine.clones, &clone_name, REPO_URL, &host_direct_env_name(), "/Users/alice/dev/flotilla-repos/clone");
    create_ready_checkout(
        &lease_engine.checkouts,
        "convoy-cwd",
        ReadyCheckoutFixture::builder()
            .name("checkout-convoy-cwd".to_string())
            .env_ref(host_direct_env_name())
            .git_ref(GIT_REF.to_string())
            .path(checkout_path.to_string())
            .maybe_worktree(Some(worktree_checkout_spec(&host_direct_env_name(), GIT_REF, checkout_path, &clone_name)))
            .build(),
    );
    let workspace = launch_workspace(&backend, lease_engine, "convoy-cwd", "implement", "kind-cwd").await;
    if let Some(docker) = docker_env {
        create_ready_docker_environment(&lease_engine.environments, &launch_environment(&workspace.metadata.name), docker);
    }

    let reconciler = test_vessel_reconciler(backend, NAMESPACE, lease_engine);
    reconciler.prepare(&workspace).await.expect("deps should load");

    let terminal = requested_terminals(lease_engine).into_iter().next().expect("terminal session should be requested");
    assert_eq!(terminal.cwd, expected_cwd);
}

fn host_direct_env_name() -> String {
    format!("host-direct-{HOST_REF}")
}

fn worktree_checkout_spec(env_ref: &str, git_ref: &str, target_path: &str, clone_ref: &str) -> CheckoutWorktreeSpec {
    CheckoutWorktreeSpec {
        repo_ref: flotilla_resources::RepositoryKey(flotilla_resources::repo_key(
            &canonicalize_repo_url(REPO_URL).expect("repo canonicalization"),
        )),
        env_ref: env_ref.to_string(),
        r#ref: git_ref.to_string(),
        base_ref: None,
        target_path: target_path.to_string(),
        clone_ref: clone_ref.to_string(),
    }
}

async fn create_convoy_with_labeled_processes(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    repo_url: &str,
    git_ref: &str,
) -> flotilla_resources::ResourceObject<Convoy> {
    let repository_spec = RepositorySpec::remote(repo_url).expect("repository URL should be canonical");
    let repository_key = repository_spec.key();
    flotilla_store::ensure_repository(&backend.clone().using::<Repository>(namespace), &repository_key, &repository_spec)
        .await
        .expect("repository create should succeed");
    let convoys = backend.clone().using::<Convoy>(namespace);
    let convoy = convoys
        .create(
            &meta(name),
            &ConvoySpec {
                continuation: None,
                subjects: Vec::new(),
                role: String::new(),
                generation: 1,
                workflow_ref: "wf".to_string(),
                dispatching_principal_ref: Default::default(),
                inputs: Default::default(),
                placement_policy: None,
                repositories: vec![ConvoyRepositorySpec {
                    url: repo_url.to_string(),
                    repo_ref: repository_key.clone(),
                    source_ref: git_ref.to_string(),
                    target_ref: "landing-target".to_string(),
                    workspace_slug: repository_spec.leaf_slug(),
                    subpaths: Vec::new(),
                }],
                r#ref: Some(git_ref.to_string()),
                project_ref: NAMESPACE.to_string(),
                adopted_checkout_refs: BTreeMap::new(),
                issues: Vec::new(),
                change_request: None,
                instruction: None,
                promises: Vec::new(),
            },
        )
        .await
        .expect("convoy create should succeed");
    convoys
        .update_status(
            name,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![
                        VesselRequirement {
                            needs: Default::default(),
                            name: "implement".to_string(),
                            depends_on: Vec::new(),
                            repository_refs: None,
                            credential_refs: Default::default(),
                            credential_scopes: Default::default(),
                            credential_permissions: Default::default(),
                            crew: vec![CrewSpec::builder()
                                .role("coder".to_string())
                                .source(CrewSource::Tool { command: "cargo fmt --check".to_string() })
                                .build()],
                        },
                        VesselRequirement {
                            needs: Default::default(),
                            name: "review".to_string(),
                            depends_on: vec!["implement".to_string()],
                            repository_refs: None,
                            credential_refs: BTreeSet::from(["github-app".to_string()]),
                            credential_scopes: BTreeMap::from([("github-app".to_string(), BTreeSet::from([repository_key]))]),
                            credential_permissions: Default::default(),
                            crew: vec![
                                CrewSpec::builder()
                                    .role("build".to_string())
                                    .source(CrewSource::Tool { command: "cargo check".to_string() })
                                    .build(),
                                CrewSpec::builder()
                                    .role("test".to_string())
                                    .source(CrewSource::Tool { command: "cargo test".to_string() })
                                    .labels(BTreeMap::from([
                                        ("service".to_string(), "api".to_string()),
                                        ("team".to_string(), "platform".to_string()),
                                        (CONVOY_LABEL.to_string(), "wrong-convoy".to_string()),
                                        (VESSEL_LABEL.to_string(), "wrong-task".to_string()),
                                        (VESSEL_REF_LABEL.to_string(), "wrong-workspace".to_string()),
                                        (ROLE_LABEL.to_string(), "wrong-role".to_string()),
                                        (VESSEL_ORDINAL_LABEL.to_string(), "999".to_string()),
                                        (CREW_ORDINAL_LABEL.to_string(), "999".to_string()),
                                    ]))
                                    .build(),
                            ],
                        },
                    ],
                }),
                ..Default::default()
            },
        )
        .await
        .expect("convoy status update should succeed");
    convoys.get(name).await.expect("convoy get should succeed")
}

/// The tool session for `role` of launch `vessel` the engine holds running in `env_ref`.
async fn create_running_terminal(engine: &EngineFixture, vessel: &str, env_ref: &str, role: &str, command: &str, cwd: &str, pool: &str) {
    let name = format!("terminal-{vessel}-{role}");
    let launch = engine.terminals.request(&TerminalLaunchRequest {
        session: name.clone(),
        environment: env_ref.to_string(),
        vessel: Some(vessel.to_string()),
        role: role.to_string(),
        pool: pool.to_string(),
        cwd: cwd.to_string(),
        env: Default::default(),
        source: TerminalLaunchSource::Tool { command: command.to_string() },
    });
    engine.terminals.ready(&launch, &format!("session-{name}"), &Default::default());
}

/// The terminal sessions `engine` holds, each launch requested of it.
fn requested_terminals(engine: &EngineFixture) -> Vec<TerminalRecord> {
    engine.terminals.terminals.at("home").expect("terminals read")
}

/// The launch requested of `engine` for `role`, the first if several.
fn requested_terminal(engine: &EngineFixture, role: &str) -> Option<TerminalRecord> {
    requested_terminals(engine).into_iter().find(|record| record.role == role)
}

/// An adopted checkout the engine holds ready at `path`, as adoption left it, held for no convoy.
fn create_ready_adopted_checkout(engine: &EngineFixture, name: &str, path: &str) {
    engine.checkouts.put_ready(
        name,
        &CheckoutSpec::Observed(ObservedCheckoutSpec {
            r#ref: GIT_REF.to_string(),
            path: path.to_string(),
            repo_ref: RepositorySpec::remote(REPO_URL).expect("repository URL").key(),
            host_ref: HOST_REF.to_string(),
            is_main: false,
        }),
        None,
    );
}

// ADR 0062: a composed placement's Environment names the per-identity
// ImageBuild (the Environment reconciler supplies the digest) and declares the
// harnesses its roles use; the vessel reads the digest from its Environment once Ready.
// The composed identity is no longer recorded in the vessel's status (#3108).
#[tokio::test]
async fn composed_placement_names_its_image_build_then_reads_its_digest() {
    let lease_engine = &EngineFixture::default();
    use flotilla_resources::PlacementImageSource;
    let spec = flotilla_provisioning_testkit::image_build_spec();
    let identity = spec.identity.clone();
    // Admission demands the build before the vessel names it.
    lease_engine.image_builds.host("builder", "amd64");
    let image_build_ref = lease_engine.image_builds.demand(&spec);
    let digest = format!("registry.test/crew@sha256:{}", "d".repeat(64));
    let built = PlacementImageSource::Built { identity, image_build_ref: image_build_ref.clone() };
    let kind = FulfilmentKindSpec::builder()
        .host_ref(HOST_REF.to_string())
        .pool("cleat".to_string())
        .realisation(FulfilmentRealisation::DockerPerVessel { image: Some(built) })
        .build();

    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-composed", "implement", REPO_URL, GIT_REF).await;
    create_kind(&backend, NAMESPACE, "kind-composed", kind.clone()).await;
    create_ready_host_worktree(lease_engine, "convoy-composed", "vessel-composed");
    let vessel = launch_workspace(&backend, lease_engine, "convoy-composed", "implement", "kind-composed").await;
    let reconciler = test_vessel_reconciler(backend, NAMESPACE, lease_engine);
    reconciler.prepare(&vessel).await.expect("deps should load");
    let environment = requested_docker(lease_engine, &vessel.metadata.name).expect("composed placement creates its Environment");
    assert_eq!(environment.image_build_ref.as_deref(), Some(image_build_ref.as_str()));
    assert!(environment.image.is_empty(), "the Environment reconciler supplies the build's digest");
    assert_eq!(environment.declared_agent_adapters, environment.required_agent_adapters);
    assert_eq!(reading(lease_engine, &vessel.metadata.name), LifecycleReading::InProgress);

    let lease_engine = &EngineFixture::default();
    lease_engine.image_builds.host("builder", "amd64");
    lease_engine.image_builds.demand(&spec);
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-composed", "implement", REPO_URL, GIT_REF).await;
    create_kind(&backend, NAMESPACE, "kind-composed", kind).await;
    create_ready_host_worktree(lease_engine, "convoy-composed", "vessel-composed");
    let vessel = launch_workspace(&backend, lease_engine, "convoy-composed", "implement", "kind-composed").await;
    let environment_ref = launch_environment(&vessel.metadata.name);
    create_ready_docker_environment(
        &lease_engine.environments,
        &environment_ref,
        DockerEnvironmentSpec { image: digest.clone(), image_build_ref: Some(image_build_ref), ..environment },
    );
    create_running_terminal(lease_engine, &vessel.metadata.name, &environment_ref, "coder", "cargo test", "/workspace", "cleat").await;
    let reconciler = test_vessel_reconciler(backend.clone(), NAMESPACE, lease_engine);
    reconciler.prepare(&vessel).await.expect("deps should load");
    assert!(matches!(reading(lease_engine, &vessel.metadata.name), LifecycleReading::Ready { .. }));
    let status = read_vessel(&backend, lease_engine, &vessel.metadata.name).await.status.expect("vessel status");
    assert_eq!(status.image_ref.as_deref(), Some(digest.as_str()));
}

// #3108 writer (b): with periodic resync off, a vessel reaches Ready from wakes alone, never
// failing a try. Its launch's request starts its controller's planning, which requests its
// checkout; the checkout's realisation wakes it to Ready, and its readiness seats its crew.
// Local: the convoy is admitted here. Remote: the convoy is admitted at another home and arrives
// by replication.
#[rstest]
#[case::local(false)]
#[case::placed_remotely(true)]
#[tokio::test]
async fn a_vessel_reaches_ready_from_wakes_alone(#[case] remote: bool) {
    use flotilla_protocol::NodeId;
    use flotilla_resources::vessel_records::VesselLaunchRequest;

    const CONVOY: &str = "convoy-woken";
    // A decided store whose commits are followed, as the runtime's is: each kind's changes reach
    // its readers and wakes only through its follower.
    let store = flotilla_provisioning_testkit::OtherHome::open("home");
    let service = store.service();
    let lease_engine = &EngineFixture::over_service(service);
    let _followers = [
        lease_engine.environments.environments.follow(Arc::clone(service)).expect("follow environments"),
        lease_engine.checkouts.checkouts.follow(Arc::clone(service)).expect("follow checkouts"),
        lease_engine.terminals.terminals.follow(Arc::clone(service)).expect("follow terminals"),
        lease_engine.vessels.vessels.follow(Arc::clone(service)).expect("follow vessels"),
    ];
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("home"));
    let admitting_root = NodeId::new("kiwi");
    let admitting = if remote {
        ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(admitting_root.clone())
    } else {
        backend.clone()
    };
    // The convoy and its fulfilment kind, replicated here from their home when it is another.
    let replicate = || async {
        if remote {
            let convoys = admitting.clone().using::<Convoy>(NAMESPACE).list().await.expect("convoys");
            backend.replica_writer::<Convoy>(admitting_root.clone(), NAMESPACE).replace(&convoys, Utc::now()).await.expect("replicate");
            let kinds = admitting.clone().using::<flotilla_resources::FulfilmentKind>(NAMESPACE).list().await.expect("kinds");
            backend
                .replica_writer::<flotilla_resources::FulfilmentKind>(admitting_root.clone(), NAMESPACE)
                .replace(&kinds, Utc::now())
                .await
                .expect("replicate");
        }
    };
    create_convoy_with_single_task(&admitting, NAMESPACE, CONVOY, "implement", REPO_URL, GIT_REF).await;
    patch_resource_annotation(&admitting, NAMESPACE, "Convoy", CONVOY, PLACEMENT_SNAPSHOT_ANNOTATION, "policy-woken")
        .await
        .expect("place convoy");
    create_host_direct_kind(&admitting, NAMESPACE, "policy-woken", HOST_REF, "cleat").await;
    if remote {
        // The repository is read where the vessel is realised.
        let repository = RepositorySpec::remote(canonicalize_repo_url(REPO_URL).expect("canonical")).expect("repository");
        ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &repository.key(), &repository).await.expect("repository");
    }
    replicate().await;
    create_ready_host_direct_environment(&lease_engine.environments, HOST_REF, "/Users/alice/dev/flotilla-repos");
    let clone_name =
        format!("clone-{}", clone_key(&canonicalize_repo_url(REPO_URL).expect("repo canonicalization"), &host_direct_env_name()));
    create_ready_clone(&lease_engine.clones, &clone_name, REPO_URL, &host_direct_env_name(), "/tmp/clone");
    // Admitted where the convoy is, and its launch named on the record the vessel reads.
    admit_on_engine(&admitting, NAMESPACE, &lease_engine.checkouts, CONVOY).await;
    replicate().await;
    // The engine's vessels and terminal sessions served to the old store's readers and watches,
    // as the daemon serves them.
    backend.install_engine_records(Arc::new(VesselViews::new(backend.clone(), NAMESPACE, lease_engine.reads())));
    backend.install_engine_records(Arc::new(flotilla_store::TerminalViews::new(NAMESPACE, lease_engine.reads())));

    // The Vessel controller over the planner, told of its convoy's changes by the runtime's
    // rechecks, and the crew seats with their wakes; no periodic resync within the test.
    let accounts: Arc<dyn flotilla_resources::account_access::AccountRecords> =
        Arc::new(flotilla_store_testkit::OldStoreAccounts::new(backend.clone()));
    let planning = Arc::new(
        VesselPlanning::new(backend.clone(), NAMESPACE, Arc::clone(&accounts))
            .with_worktree_metadata(Arc::new(TestWorktreeMetadata(lease_engine.reads())))
            .with_lifecycle_reads(lease_engine.reads())
            // A convoy homed elsewhere is read from its replica, as the runtime's planner reads it.
            .with_federated_dependencies(&backend, CanonicalHostId::resolved(HOST_REF)),
    );
    let wake = flotilla_crew::reconcilers::vessel::ConvoyVesselWake::new(
        &backend,
        NAMESPACE,
        lease_engine.reads(),
        crate::common::detached_convoys(),
        accounts,
    );
    let (rechecks, recheck_events) = tokio::sync::mpsc::channel(64);
    let controller = flotilla_provisioning::engine::controller::Controller::start(
        Arc::clone(&lease_engine.vessels.lifecycles),
        Arc::new(flotilla_provisioning::engine::vessel_actuator::VesselActuator::new(Arc::clone(&planning) as _)),
        Arc::new(flotilla_resources::SystemClock),
        Arc::clone(service),
    )
    .expect("vessel controller");
    let (_unobserved, backings) = tokio::sync::mpsc::channel(1);
    let running = tokio::spawn(controller.run_with(backings, recheck_events));
    let rechecking = tokio::spawn(wake.clone().recheck(NAMESPACE.to_owned(), rechecks.clone()));
    let mut secondaries = CrewSeats::secondary_watches();
    secondaries.push(Box::new(wake));
    let mut harness = common::ControllerLoopHarness::new(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: backend.clone().using::<Vessel>(NAMESPACE),
            secondaries,
            reconciler: CrewSeats::new(planning).with_rechecks(rechecks),
            backend: backend.clone(),
        }
        .run(),
    );

    // The launch, as the vessel requester requests it for the placed work.
    let launch = lease_engine.vessels.request_for(&VesselLaunchRequest {
        convoy: CONVOY.to_owned(),
        convoy_home: if remote { admitting_root.as_str().to_owned() } else { lease_engine.vessels.home.clone() },
        work: "implement".to_owned(),
        host: HOST_REF.to_owned(),
    });
    let session = || requested_terminals(lease_engine).into_iter().find(|terminal| terminal.vessel.as_deref() == Some(launch.as_str()));

    // Its request starts its planning, which requests its checkout; the checkout's realisation
    // wakes it to Ready, and its readiness seats its crew.
    let checkout = || requested_checkouts(lease_engine).into_values().find(|checkout| !checkout.is_ready());
    harness.wait_until(Duration::from_secs(5), || async { checkout().is_some() }).await;
    assert!(session().is_none(), "a vessel waiting for its checkout requests no terminal session");
    assert!(matches!(reading(lease_engine, &launch), LifecycleReading::InProgress), "{:?}", reading(lease_engine, &launch));
    lease_engine.checkouts.ready(&checkout().expect("requested checkout").name, None);
    harness.wait_until(Duration::from_secs(5), || async { matches!(reading(lease_engine, &launch), LifecycleReading::Ready { .. }) }).await;
    harness.wait_until(Duration::from_secs(5), || async { session().is_some() }).await;
    harness.shutdown().await;
    running.abort();
    rechecking.abort();
}
