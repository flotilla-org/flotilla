use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use common::{
    controller_meta, create_convoy_with_single_task, create_host_direct_kind, create_ready_host_direct_environment, ControllerLoopHarness,
};
use flotilla_crew::reconcilers::vessel::{CrewSeats, VesselPlanning};
use flotilla_protocol::ConfiguredResourceLimits;
use flotilla_protocol::LifecycleReading;
use flotilla_provisioning::{
    engine::reads::{LifecycleReadPort, StoreReads},
    reconcilers::CloneRuntime,
};
use flotilla_provisioning_testkit::{
    clones::{MadeCheckouts, RunningClones},
    EngineFixture, VesselsFixture,
};
use flotilla_resources::checkout_records::CheckoutRecords as _;
use flotilla_resources::clone_records::CloneRecords;
use flotilla_resources::lifecycle_reads::LifecycleReads;
use flotilla_resources::terminal_records::{TerminalLaunchFacts, TerminalRecords as _};
use flotilla_resources::vessel_planning::VesselPlanner as _;
use flotilla_resources::vessel_records::{VesselRecords as _, VesselStep};
use flotilla_resources::{
    canonicalize_repo_url, CheckoutSpec, CheckoutWorktreeSpec, CloneSpec, Convoy, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus,
    CrewSource, CrewSpec, Host, HostSpec, HostStatus, InputMeta, Project, ProjectRepositoryRole, ProjectRepositorySpec, ProjectSpec,
    Repository, RepositorySpec, Vessel, VesselPhase, VesselRequirement, PLACEMENT_SNAPSHOT_ANNOTATION,
};
use flotilla_store::{controller::ControllerLoop, patch_resource_annotation, ResourceBackend, VesselViews};

use crate::common;

const NAMESPACE: &str = "flotilla";

/// The account records a planner reads: none of these tests binds an account.
fn old_store_accounts(backend: &ResourceBackend) -> Arc<dyn flotilla_resources::account_access::AccountRecords> {
    Arc::new(flotilla_store_testkit::OldStoreAccounts::new(backend.clone()))
}

#[derive(Default)]
struct FakeCloneRuntime;

#[async_trait]
impl CloneRuntime for FakeCloneRuntime {
    async fn clone_and_inspect(&self, _repo_url: &str, _target_path: &str) -> Result<Option<String>, String> {
        Ok(Some("main".to_string()))
    }

    async fn inspect_existing(&self, _target_path: &str) -> Result<Option<String>, String> {
        Ok(Some("main".to_string()))
    }
}

/// Refuses the first clone as the forge refused a stale credential, then clones as `FakeCloneRuntime`.
#[derive(Default)]
struct RefusingFirstCloneRuntime {
    refused: AtomicBool,
}

#[async_trait]
impl CloneRuntime for RefusingFirstCloneRuntime {
    async fn clone_and_inspect(&self, _repo_url: &str, _target_path: &str) -> Result<Option<String>, String> {
        if self.refused.swap(true, Ordering::SeqCst) {
            Ok(Some("main".to_string()))
        } else {
            Err("authentication failed: repository access denied".to_string())
        }
    }

    async fn inspect_existing(&self, _target_path: &str) -> Result<Option<String>, String> {
        Ok(Some("main".to_string()))
    }
}

#[tokio::test]
async fn controller_loops_drive_host_direct_workspace_to_ready() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_ready_host(&backend, "01HXYZ").await;
    let engine = RunningClones::start_with_checkouts("01HXYZ", Arc::new(FakeCloneRuntime), Arc::new(MadeCheckouts::default()));
    let environments = &engine.environments;
    create_ready_host_direct_environment(environments, "01HXYZ", "/Users/alice/dev/flotilla-repos");
    create_host_direct_kind(&backend, NAMESPACE, "kind-a", "01HXYZ", "cleat").await;
    let repository_url = "git@github.com:flotilla-org/flotilla.git";
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-a", "implement", repository_url, "feat/task-provisioning").await;
    let repository_spec = RepositorySpec::remote(canonicalize_repo_url(repository_url).expect("repository URL should canonicalize"))
        .expect("repository spec");
    backend
        .clone()
        .using::<Repository>(NAMESPACE)
        .delete(&repository_spec.key().to_string())
        .await
        .expect("destination should start without the convoy repository");
    place(&backend, "convoy-a", "kind-a").await;

    common::admit_on_engine(&backend, NAMESPACE, &engine.checkouts, "convoy-a").await;
    let (harness, vessels) = full_controller_harness(backend.clone(), &engine);
    let launch = vessels.request("convoy-a", "implement", "01HXYZ");

    let workspaces = backend.clone().using::<Vessel>(NAMESPACE);
    harness
        .wait_until(Duration::from_secs(3), || {
            // A vessel reads Ready before its crew's terminals are requested; its limits come
            // from the coder's terminal, so wait for that to run too.
            let terminals_running = start_requested_terminals(&engine);
            let workspaces = workspaces.clone();
            let launch = launch.clone();
            async move {
                terminals_running
                    && matches!(
                        workspaces.get(&launch).await.ok().and_then(|workspace| workspace.status).map(|status| status.phase),
                        Some(VesselPhase::Ready)
                    )
            }
        })
        .await;

    let workspace = workspaces.get(&launch).await.expect("workspace get should succeed");
    let ready_record = vessels.vessels.get(&launch, None).expect("vessel read");
    let status = workspace.status.expect("workspace status should be present");
    // #2650: host-direct configured limits survive launch into Vessel status;
    // no container quota is invented for a host-direct vessel.
    assert_eq!(
        status.configured_limits,
        Some(ConfiguredResourceLimits { cpus: None, cpu_shares: None, build_jobs: Some(4), linker_threads: Some(4) })
    );
    assert_eq!(status.phase, VesselPhase::Ready);
    assert_eq!(status.environment_ref.as_deref(), Some("host-direct-01HXYZ"));
    assert_eq!(status.checkout_refs.values().next().map(String::as_str), Some("checkout-convoy-a"));
    assert_eq!(status.terminal_session_refs, vec![format!("terminal-{launch}-coder")]);
    backend
        .clone()
        .using::<Repository>(NAMESPACE)
        .get(&repository_spec.key().to_string())
        .await
        .expect("missing repository should be materialized");

    tokio::time::sleep(Duration::from_millis(200)).await;
    let steady = workspaces.get(&launch).await.expect("ready workspace should remain readable");
    // The view's version is its launch's name; the launch itself is what must not change.
    assert_eq!(
        vessels.vessels.get(&launch, None).expect("vessel read"),
        ready_record,
        "steady-state reconciliation must not step the launch"
    );
    assert_eq!(steady.status.expect("steady workspace status").ready_at, status.ready_at);

    harness.shutdown().await;
}

#[tokio::test]
async fn a_project_gaining_a_member_leaves_a_running_convoys_checkouts_unchanged() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_ready_host(&backend, "01HXYZ").await;
    let engine = RunningClones::start_with_checkouts("01HXYZ", Arc::new(FakeCloneRuntime), Arc::new(MadeCheckouts::default()));
    let environments = &engine.environments;
    create_ready_host_direct_environment(environments, "01HXYZ", "/Users/alice/dev/flotilla-repos");
    create_host_direct_kind(&backend, NAMESPACE, "kind-a", "01HXYZ", "cleat").await;
    let repository_url = "https://github.com/flotilla-org/flotilla";
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-a", "implement", repository_url, "fix/members").await;
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let mut spec = convoy.spec.clone();
    spec.project_ref = "suite".to_string();
    convoys.update(&InputMeta::from(&convoy.metadata), &convoy.metadata.resource_version, &spec).await.expect("bind project");
    let admitted = convoy.spec.repositories.clone();
    let projects = backend.clone().using::<Project>(NAMESPACE);
    let member = |repo| {
        ProjectRepositorySpec::builder().repo(repo).roles([ProjectRepositoryRole::Code].into()).default_branch("main".into()).build()
    };
    projects
        .create(
            &controller_meta().name("suite").call(),
            &ProjectSpec::builder().display_name("Suite".into()).repositories(vec![member(admitted[0].repo_ref.clone())]).build(),
        )
        .await
        .expect("project create");
    place(&backend, "convoy-a", "kind-a").await;

    common::admit_on_engine(&backend, NAMESPACE, &engine.checkouts, "convoy-a").await;
    let (harness, launches) = full_controller_harness(backend.clone(), &engine);
    let launch = launches.request("convoy-a", "implement", "01HXYZ");
    let vessels = backend.clone().using::<Vessel>(NAMESPACE);
    harness
        .wait_until(Duration::from_secs(3), || {
            start_requested_terminals(&engine);
            let vessels = vessels.clone();
            let launch = launch.clone();
            async move {
                matches!(
                    vessels.get(&launch).await.ok().and_then(|vessel| vessel.status).map(|status| status.phase),
                    Some(VesselPhase::Ready)
                )
            }
        })
        .await;
    let checkout_launches =
        || engine.checkouts.checkouts.here().expect("checkouts").into_iter().map(|checkout| checkout.name).collect::<Vec<_>>();
    let before = checkout_launches();
    assert_eq!(before.len(), 1);
    assert_eq!(flotilla_resources::checkout_records::launched(&before[0]), "checkout-convoy-a");

    let added = RepositorySpec::remote("https://github.com/flotilla-org/binnacle").expect("added repository");
    flotilla_store::ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &added.key(), &added)
        .await
        .expect("added repository");
    let project = projects.get("suite").await.expect("project");
    let mut grown = project.spec.clone();
    grown.repositories.push(member(added.key()));
    projects.update(&InputMeta::from(&project.metadata), &project.metadata.resource_version, &grown).await.expect("project gains a member");

    let planning =
        VesselPlanning::new(backend.clone(), NAMESPACE, old_store_accounts(&backend)).with_lifecycle_reads(Arc::clone(&launches.reads));
    planning.plan(&launch).await.expect("vessel planned");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(checkout_launches(), before, "no checkout is requested for the new member");
    assert_eq!(convoys.get("convoy-a").await.expect("convoy").spec.repositories, admitted);

    harness.shutdown().await;
}

#[tokio::test]
async fn controller_materializes_a_missing_repository_for_a_multi_repository_convoy() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_ready_host(&backend, "01HXYZ").await;
    let engine = RunningClones::start_with_checkouts("01HXYZ", Arc::new(FakeCloneRuntime), Arc::new(MadeCheckouts::default()));
    let environments = &engine.environments;
    create_ready_host_direct_environment(environments, "01HXYZ", "/Users/alice/dev/flotilla-repos");
    create_host_direct_kind(&backend, NAMESPACE, "kind-multi", "01HXYZ", "cleat").await;

    let known = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("known repository");
    let missing = RepositorySpec::remote("https://github.com/flotilla-org/cleat").expect("missing repository");
    flotilla_store::ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &known.key(), &known)
        .await
        .expect("known repository should create");

    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(
            &controller_meta().name("convoy-multi").call(),
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
                    ConvoyRepositorySpec::builder()
                        .url("https://github.com/flotilla-org/flotilla".to_string())
                        .repo_ref(known.key())
                        .source_ref("main".to_string())
                        .target_ref("main".to_string())
                        .workspace_slug("flotilla".to_string())
                        .subpaths(Vec::new())
                        .build(),
                    ConvoyRepositorySpec::builder()
                        .url("https://github.com/flotilla-org/cleat".to_string())
                        .repo_ref(missing.key())
                        .source_ref("main".to_string())
                        .target_ref("main".to_string())
                        .workspace_slug("cleat".to_string())
                        .subpaths(Vec::new())
                        .build(),
                ],
                r#ref: Some("fix/multi".to_string()),
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
    convoys
        .update_status(
            "convoy-multi",
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
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
    place(&backend, "convoy-multi", "kind-multi").await;

    common::admit_on_engine(&backend, NAMESPACE, &engine.checkouts, "convoy-multi").await;
    let (harness, launches) = full_controller_harness(backend.clone(), &engine);
    let launch = launches.request("convoy-multi", "implement", "01HXYZ");
    let vessels = backend.clone().using::<Vessel>(NAMESPACE);
    harness
        .wait_until(Duration::from_secs(3), || {
            start_requested_terminals(&engine);
            let vessels = vessels.clone();
            let launch = launch.clone();
            async move {
                matches!(
                    vessels.get(&launch).await.ok().and_then(|vessel| vessel.status).map(|status| status.phase),
                    Some(VesselPhase::Ready)
                )
            }
        })
        .await;

    let repositories = backend.clone().using::<Repository>(NAMESPACE);
    repositories.get(&known.key().to_string()).await.expect("known repository should remain registered");
    repositories.get(&missing.key().to_string()).await.expect("missing repository should be materialized");
    let clones = engine.clones.here().expect("clones should read");
    assert_eq!(clones.len(), 2);
    assert!(clones.iter().all(|clone| clone.is_ready()), "{clones:?}");

    harness.shutdown().await;
}

#[tokio::test]
async fn clone_controller_marks_clone_ready() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let engine = RunningClones::start("01HXYZ", Arc::new(FakeCloneRuntime));
    create_ready_host_direct_environment(&engine.environments, "01HXYZ", "/Users/alice/dev/flotilla-repos");
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    let launch = engine
        .clones
        .request(
            &repository_spec,
            &CloneSpec {
                repo_ref: repository_spec.key(),
                url: "git@github.com:flotilla-org/flotilla.git".to_string(),
                env_ref: "host-direct-01HXYZ".to_string(),
                path: "/Users/alice/dev/flotilla".to_string(),
            },
            false,
        )
        .expect("clone request should succeed");

    let harness = ControllerLoopHarness::new(backend);
    harness
        .wait_until(Duration::from_secs(1), || {
            let ready = engine.clones.get(&launch, None).ok().flatten().is_some_and(|clone| clone.is_ready());
            async move { ready }
        })
        .await;

    let clone = engine.clones.get(&launch, None).expect("clone read should succeed").expect("clone held");
    assert_eq!(clone.reading, LifecycleReading::Ready { identity: "/Users/alice/dev/flotilla".to_string() });
    assert_eq!(clone.default_branch.as_deref(), Some("main"));

    harness.shutdown().await;
}

// Behaviour (#3108): a clone the forge refused gives up; a new checkout's demand for it requests
// its next launch, which clones again, and the checkout made from that launch becomes ready.
#[tokio::test]
async fn new_convoy_checkout_demand_redrives_a_clone_failed_on_old_auth() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let engine =
        RunningClones::start_with_checkouts("01HXYZ", Arc::new(RefusingFirstCloneRuntime::default()), Arc::new(MadeCheckouts::default()));
    create_ready_host_direct_environment(&engine.environments, "01HXYZ", "/Users/alice/dev/flotilla-repos");
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    flotilla_store::ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &repository_spec.key(), &repository_spec)
        .await
        .expect("repository create should succeed");
    let clone_spec = CloneSpec {
        repo_ref: repository_spec.key(),
        url: "git@github.com:flotilla-org/flotilla.git".to_string(),
        env_ref: "host-direct-01HXYZ".to_string(),
        path: "/Users/alice/dev/flotilla".to_string(),
    };
    let refused = engine.clones.request(&repository_spec, &clone_spec, false).expect("clone request should succeed");
    let harness = ControllerLoopHarness::new(backend.clone());
    harness
        .wait_until(Duration::from_secs(3), || {
            let gave_up = matches!(
                engine.clones.get(&refused, None).ok().flatten().map(|clone| clone.reading),
                Some(LifecycleReading::GaveUp { cause }) if cause.contains("authentication failed")
            );
            async move { gave_up }
        })
        .await;

    let launch = engine.clones.request(&repository_spec, &clone_spec, false).expect("new demand should request the next launch");
    assert_ne!(launch, refused, "an ended launch is never revived");
    engine.checkouts.open_convoy("convoy-new-demand");
    let checkout = engine.checkouts.request(
        "checkout-new-demand",
        &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
            repo_ref: repository_spec.key(),
            env_ref: "host-direct-01HXYZ".to_string(),
            r#ref: "fix/clone-failed-latch".to_string(),
            base_ref: Some("main".to_string()),
            target_path: "/Users/alice/dev/flotilla.fix-clone-failed-latch".to_string(),
            clone_ref: launch.clone(),
        }),
        Some(&engine.checkouts.holder("convoy-new-demand")),
    );

    harness
        .wait_until(Duration::from_secs(3), || {
            let clone_ready = engine.clones.get(&launch, None).ok().flatten().is_some_and(|clone| clone.is_ready());
            let checkout_ready = engine.checkouts.checkouts.get(&checkout, None).ok().flatten().is_some_and(|checkout| checkout.is_ready());
            async move { clone_ready && checkout_ready }
        })
        .await;

    harness.shutdown().await;
}

// The Vessel finalizer is gone (#3108): a vessel's home releases its launch, and the released
// launch leaves the checkout the engine holds for its convoy to the convoy.
#[tokio::test]
async fn a_released_vessel_leaves_the_convoys_checkout() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_convoy_with_single_task(
        &backend,
        NAMESPACE,
        "convoy-delete",
        "implement",
        "git@github.com:flotilla-org/flotilla.git",
        "feat/task-provisioning",
    )
    .await;
    place(&backend, "convoy-delete", "policy-delete").await;

    // The engine holds the convoy's checkout, realised.
    let engine = EngineFixture::default();
    engine.checkouts.open_convoy("convoy-delete");
    let checkout = engine.checkouts.put_ready(
        "checkout-convoy-delete",
        &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
            repo_ref: flotilla_resources::RepositoryKey(flotilla_resources::repo_key("https://github.com/flotilla-org/flotilla")),
            env_ref: "host-direct-01HXYZ".to_string(),
            r#ref: "feat/task-provisioning".to_string(),
            base_ref: None,
            target_path: "/Users/alice/dev/flotilla-repos/workspace-delete".to_string(),
            clone_ref: "clone-a".to_string(),
        }),
        Some(&engine.checkouts.holder("convoy-delete")),
    );
    let launch = engine.vessels.request("convoy-delete", "implement", "01HXYZ");
    backend.install_engine_records(Arc::new(VesselViews::new(backend.clone(), NAMESPACE, engine.reads())));
    backend.serve_engine_in(NAMESPACE);
    let workspaces = backend.clone().using::<Vessel>(NAMESPACE);
    let mut harness = ControllerLoopHarness::new(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: workspaces.clone(),
            secondaries: CrewSeats::secondary_watches(),
            reconciler: CrewSeats::new(Arc::new(
                VesselPlanning::new(backend.clone(), NAMESPACE, old_store_accounts(&backend)).with_lifecycle_reads(engine.reads()),
            )),
            backend: backend.clone(),
        }
        .run(),
    );

    engine.vessels.step(&launch, VesselStep::Released);
    harness
        .wait_until(Duration::from_secs(1), || {
            let workspaces = workspaces.clone();
            let launch = launch.clone();
            async move {
                matches!(
                    workspaces.get(&launch).await.ok().and_then(|workspace| workspace.status).map(|status| status.phase),
                    Some(VesselPhase::Released)
                )
            }
        })
        .await;
    // Resyncs run over the released launch.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(engine.checkouts.record(&checkout).is_ready(), "a released vessel leaves the convoy's checkout to the convoy");

    harness.shutdown().await;
}

// Behaviour (#3108): a vessel listing shows each work's newest launch as the old store showed its
// Vessel. A released launch is gone from it, as a Vessel deleted at teardown; one that failed is
// listed while its convoy is live, and leaves with the convoy's end. Any launch is still read by
// its name.
#[tokio::test]
async fn a_vessel_listing_leaves_out_released_launches_and_those_of_ended_convoys() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    for convoy in ["convoy-released", "convoy-failed"] {
        create_convoy_with_single_task(&backend, NAMESPACE, convoy, "implement", "git@github.com:flotilla-org/flotilla.git", "feat/x")
            .await;
    }
    let engine = EngineFixture::default();
    let released = engine.vessels.request("convoy-released", "implement", "01HXYZ");
    let failed = engine.vessels.request("convoy-failed", "implement", "01HXYZ");
    backend.install_engine_records(Arc::new(VesselViews::new(backend.clone(), NAMESPACE, engine.reads())));
    backend.serve_engine_in(NAMESPACE);
    let vessels = backend.clone().using::<Vessel>(NAMESPACE);
    let listed = || async {
        let mut names = vessels.list().await.expect("list").items.into_iter().map(|vessel| vessel.metadata.name).collect::<Vec<_>>();
        names.sort();
        names
    };
    assert_eq!(listed().await, [failed.clone(), released.clone()]);

    engine.vessels.step(&released, VesselStep::Released);
    engine.vessels.step(&failed, VesselStep::Failed { cause: "no room".into(), retryable: false });
    assert_eq!(listed().await, std::slice::from_ref(&failed), "a released launch is listed no more; a failed one is, its convoy live");
    assert!(vessels.get(&released).await.is_ok(), "a released launch is still read by its name");

    let convoys = backend.clone().using::<flotilla_resources::Convoy>(NAMESPACE);
    let convoy = convoys.get("convoy-failed").await.expect("convoy");
    let status =
        flotilla_resources::ConvoyStatus { phase: flotilla_resources::ConvoyPhase::Abandoned, ..convoy.status.clone().unwrap_or_default() };
    convoys.update_status("convoy-failed", &convoy.metadata.resource_version, &status).await.expect("convoy ended");
    assert!(listed().await.is_empty(), "an ended launch leaves the listing with its convoy's end");
}

/// Stands in for the TerminalSession controller, whose own tests cover it: each requested launch
/// is realised, given four build jobs as a host-direct launch is. Returns whether any launch was
/// requested: every one requested then runs.
fn start_requested_terminals(engine: &RunningClones) -> bool {
    let facts = TerminalLaunchFacts { build_jobs: Some(4), ..Default::default() };
    let mut requested = false;
    for record in engine.terminals.terminals.at(&engine.terminals.home).expect("terminal sessions") {
        if !record.ended() {
            requested = true;
            if !record.is_running() {
                engine.terminals.ready(&record.name, &format!("session-{}", record.session_name()), &facts);
            }
        }
    }
    requested
}

/// The Vessel controller over the engine's environments, clones, checkouts, terminal sessions
/// and vessels, on `engine`'s store, and its crew seats; the engine's Clone and Checkout
/// controllers run beside it, and what a vessel holds wakes it as it changes. The
/// vessels' fixture is returned with readings that serve them; `backend` reads each launch as
/// its old Vessel view (#3108).
fn full_controller_harness(backend: ResourceBackend, engine: &RunningClones) -> (ControllerLoopHarness, VesselsFixture) {
    let mut vessels = VesselsFixture::over_service(&engine.service);
    let reads: Arc<dyn LifecycleReads> = Arc::new(
        LifecycleReadPort::new(StoreReads::new(engine.service.store()), engine.service.home().to_owned())
            .with_environments(Arc::clone(&engine.environments.environments) as _)
            .with_clones(Arc::clone(&engine.clones) as _)
            .with_checkouts(Arc::clone(&engine.checkouts.checkouts) as _)
            .with_terminals(Arc::clone(&engine.terminals.terminals) as _)
            .with_vessels(Arc::clone(&vessels.vessels) as _),
    );
    vessels.reads = Arc::clone(&reads);
    backend.install_engine_records(Arc::new(flotilla_store::TerminalViews::new(NAMESPACE, Arc::clone(&reads))));
    backend.install_engine_records(Arc::new(VesselViews::new(backend.clone(), NAMESPACE, Arc::clone(&reads))));
    backend.serve_engine_in(NAMESPACE);
    let mut harness = ControllerLoopHarness::new(backend.clone());
    harness.hold(vessels.vessels.follow(Arc::clone(&engine.service)).expect("follow the vessels"));
    // This must progress through its holdings' commits rather than a periodic Vessel relist;
    // production resync is also deliberately slow.
    let planning = VesselPlanning::new(backend.clone(), NAMESPACE, old_store_accounts(&backend)).with_lifecycle_reads(Arc::clone(&reads));
    let running = flotilla_orchestration_testkit::run_vessels(
        &backend,
        NAMESPACE,
        planning,
        Arc::clone(&vessels.lifecycles),
        reads,
        Arc::clone(&engine.service),
    );
    running.into_tasks().into_iter().for_each(|task| harness.hold(task));
    (harness, vessels)
}

/// Places convoy `convoy` by fulfilment kind `kind`, as its admission pins it.
async fn place(backend: &ResourceBackend, convoy: &str, kind: &str) {
    patch_resource_annotation(backend, NAMESPACE, "Convoy", convoy, PLACEMENT_SNAPSHOT_ANNOTATION, kind).await.expect("place convoy");
}

async fn create_ready_host(backend: &ResourceBackend, name: &str) {
    let hosts = backend.clone().using::<Host>(NAMESPACE);
    let created = hosts.create(&controller_meta().name(name).call(), &HostSpec::default()).await.expect("host create should succeed");
    hosts
        .update_status(
            name,
            &created.metadata.resource_version,
            &HostStatus {
                capabilities: Default::default(),
                heartbeat_at: Some(Utc::now()),
                ready: true,
                resource_store: None,
                ..HostStatus::default()
            },
        )
        .await
        .expect("host status update should succeed");
}
