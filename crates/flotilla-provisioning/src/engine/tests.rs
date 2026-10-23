use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
};

use chrono::{DateTime, Utc};
use flotilla_resources::{
    Checkout as OldCheckout, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutStatusPatch, CheckoutWorktreeSpec, Clone as OldClone,
    ClonePhase, CloneSpec, CloneStatus, CloneStatusPatch, ObjectMeta, RepositoryKey, Resource as OldResource, ResourceError,
    ResourceObject, StatusPatch, CONVOY_LABEL,
};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
    Home,
};
use flotilla_store::controller::{ReconcileOutcome, Reconciler};

use super::{checkout::CheckoutBinding, clone::CloneBinding, Binding, Engine, LifecycleHome, Lifecycles, Step};
use crate::stored::{
    checkout::{CheckoutRequest, Source},
    environment::{DockerImage, DockerRecipe},
    Holder,
};

const HOME: &str = "home";

/// An old reconciler that reports the scripted patches in order, counting its prepares.
struct Scripted<T: OldResource> {
    patches: StdMutex<VecDeque<Option<T::StatusPatch>>>,
    prepared: AtomicUsize,
    finalized: AtomicUsize,
}

impl<T: OldResource> Scripted<T> {
    fn new(patches: impl IntoIterator<Item = Option<T::StatusPatch>>) -> Self {
        Self { patches: StdMutex::new(patches.into_iter().collect()), prepared: AtomicUsize::new(0), finalized: AtomicUsize::new(0) }
    }
}

impl<T: OldResource> Reconciler for Scripted<T>
where
    T::StatusPatch: Clone + Send,
{
    type Resource = T;
    type Prepared = Option<T::StatusPatch>;

    async fn prepare(&self, _obj: &ResourceObject<T>) -> Result<Self::Prepared, ResourceError> {
        self.prepared.fetch_add(1, Ordering::SeqCst);
        Ok(self.patches.lock().expect("script").pop_front().flatten())
    }

    fn reconcile(&self, _obj: &ResourceObject<T>, prepared: &Self::Prepared, _now: DateTime<Utc>) -> ReconcileOutcome<T> {
        let mut outcome = ReconcileOutcome::new(prepared.clone());
        outcome.requeue_after = Some(std::time::Duration::from_secs(30));
        outcome
    }

    async fn run_finalizer(&self, _obj: &ResourceObject<T>) -> Result<(), ResourceError> {
        self.finalized.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        None
    }
}

fn meta(name: &str) -> ObjectMeta {
    ObjectMeta {
        name: name.into(),
        namespace: "flotilla".into(),
        resource_version: "1".into(),
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
        owner_references: vec![],
        finalizers: vec![],
        deletion_timestamp: None,
        creation_timestamp: DateTime::UNIX_EPOCH,
        merge: None,
    }
}

fn clone_object(phase: Option<ClonePhase>) -> ResourceObject<OldClone> {
    ResourceObject {
        metadata: meta("clone-a"),
        spec: CloneSpec {
            repo_ref: RepositoryKey("github.com/org/repo".into()),
            url: "https://github.com/org/repo".into(),
            env_ref: "env-a".into(),
            path: "/clones/repo".into(),
        },
        status: phase.map(|phase| CloneStatus { phase, ..CloneStatus::default() }),
    }
}

fn worktree(convoy: Option<&str>) -> ResourceObject<OldCheckout> {
    let mut metadata = meta("checkout-a");
    if let Some(convoy) = convoy {
        metadata.labels.insert(CONVOY_LABEL.into(), convoy.into());
    }
    ResourceObject {
        metadata,
        spec: CheckoutSpec::Worktree(CheckoutWorktreeSpec {
            repo_ref: RepositoryKey("github.com/org/repo".into()),
            env_ref: "env-a".into(),
            r#ref: "feature".into(),
            base_ref: Some("main".into()),
            target_path: "/work/feature".into(),
            clone_ref: "clone-a".into(),
        }),
        status: None,
    }
}

fn lifecycles<B: Binding>() -> Arc<dyn Lifecycles<B::Kind>>
where
    <B::Kind as LifecycleKind>::Request: Send + Sync,
{
    Arc::new(LifecycleHome::new(HOME, Home::<Lifecycle<B::Kind>>::new(HOME).expect("declaration")))
}

/// Runs one reconcile through the wrapper and applies its patch to `obj`, as the controller does.
async fn reconcile<R, B>(engine: &Engine<R, B>, obj: &mut ResourceObject<R::Resource>) -> ReconcileOutcome<R::Resource>
where
    R: Reconciler,
    R::Prepared: Send + Sync,
    B: Binding<Old = R::Resource>,
    <R::Resource as OldResource>::Status: Default,
    <R::Resource as OldResource>::StatusPatch: StatusPatch<<R::Resource as OldResource>::Status> + Clone,
{
    let prepared = engine.prepare(obj).await.expect("prepare");
    let outcome = engine.reconcile(obj, &prepared, Utc::now());
    if let Some(patch) = &outcome.patch {
        patch.apply(obj.status.get_or_insert_with(Default::default));
    }
    outcome
}

// Behaviour (#3629 layer 3): a clone first reported Ready is requested and realised in the engine
// under its path, and the old patch is written because the engine took it.
#[tokio::test]
async fn a_ready_clone_is_requested_and_realised_in_the_engine() {
    let home = lifecycles::<CloneBinding>();
    let engine = Engine::<_, CloneBinding>::new(
        Scripted::<OldClone>::new([Some(CloneStatusPatch::MarkReady { default_branch: Some("main".into()) })]),
        Some(home.clone()),
    );
    let mut obj = clone_object(None);
    let outcome = reconcile(&engine, &mut obj).await;
    assert_eq!(outcome.patch, Some(CloneStatusPatch::MarkReady { default_branch: Some("main".into()) }));
    let stored = home.get("clone-a").expect("requested");
    assert_eq!(stored.state(), &LifecycleState::Realised { identity: "/clones/repo".into() });
    assert_eq!(stored.spec().environment.name, "env-a");
    assert_eq!(stored.spec().environment.home, HOME);
}

// Behaviour: the engine decides when a clone gives up. Transient failures pass through as long as
// the clone is wanted (Clone's policy has no cap); a failure that cannot succeed gives it up, the
// requeue is dropped, and the old reconciler is never run on it again.
#[tokio::test]
async fn the_engine_gives_a_clone_up_and_never_reconciles_it_again() {
    let home = lifecycles::<CloneBinding>();
    let retrying = || Some(CloneStatusPatch::MarkRetrying { message: "network unreachable".into() });
    let failed = CloneStatusPatch::MarkFailed { message: "repository not found".into(), failed_at: DateTime::UNIX_EPOCH };
    let engine = Engine::<_, CloneBinding>::new(
        Scripted::<OldClone>::new((0..20).map(|_| retrying()).chain([Some(failed.clone()), retrying()])),
        Some(home.clone()),
    );
    let mut obj = clone_object(Some(ClonePhase::Pending));
    for failure in 1..=20 {
        let outcome = reconcile(&engine, &mut obj).await;
        assert_eq!(outcome.patch, retrying(), "failure {failure} is retried");
        assert!(matches!(home.get("clone-a").expect("held").state(), LifecycleState::Requested { failures, .. } if *failures == failure));
    }
    let outcome = reconcile(&engine, &mut obj).await;
    assert_eq!(outcome.patch, Some(failed));
    assert_eq!(outcome.requeue_after, None);
    assert!(
        matches!(home.get("clone-a").expect("held").state(), LifecycleState::GaveUp { cause } if cause.detail == "repository not found")
    );
    assert_eq!(obj.status.as_ref().map(|status| status.phase), Some(ClonePhase::Failed));

    let prepared = engine.inner().prepared.load(Ordering::SeqCst);
    let outcome = reconcile(&engine, &mut obj).await;
    assert_eq!(outcome.patch, None, "the copy already holds");
    assert_eq!(engine.inner().prepared.load(Ordering::SeqCst), prepared, "an ended resource is not reconciled again");
}

// Behaviour: a resource written before the switch (past its initial phase, not held by the
// engine) stays on the old path until the import: its patch passes and nothing is requested.
#[tokio::test]
async fn a_resource_the_engine_does_not_hold_stays_on_the_old_path() {
    let home = lifecycles::<CloneBinding>();
    let patch = CloneStatusPatch::MarkFailed { message: "gone".into(), failed_at: DateTime::UNIX_EPOCH };
    let engine = Engine::<_, CloneBinding>::new(Scripted::<OldClone>::new([Some(patch.clone())]), Some(home.clone()));
    let mut obj = clone_object(Some(ClonePhase::Ready));
    assert_eq!(reconcile(&engine, &mut obj).await.patch, Some(patch));
    assert!(home.get("clone-a").is_none());
}

// Behaviour: a patch with no lifecycle meaning (cloning, an observation) requests nothing.
#[tokio::test]
async fn an_in_progress_patch_requests_nothing() {
    let home = lifecycles::<CloneBinding>();
    let engine = Engine::<_, CloneBinding>::new(Scripted::<OldClone>::new([Some(CloneStatusPatch::MarkCloning)]), Some(home.clone()));
    let mut obj = clone_object(None);
    assert_eq!(reconcile(&engine, &mut obj).await.patch, Some(CloneStatusPatch::MarkCloning));
    assert!(home.get("clone-a").is_none());
}

// Behaviour: a finished teardown releases the engine's resource.
#[tokio::test]
async fn a_finished_teardown_releases_the_resource() {
    let home = lifecycles::<CloneBinding>();
    let engine = Engine::<_, CloneBinding>::new(
        Scripted::<OldClone>::new([Some(CloneStatusPatch::MarkReady { default_branch: None })]),
        Some(home.clone()),
    );
    let mut obj = clone_object(None);
    reconcile(&engine, &mut obj).await;
    engine.run_finalizer(&obj).await.expect("teardown");
    assert_eq!(engine.inner().finalized.load(Ordering::SeqCst), 1);
    assert_eq!(home.get("clone-a").expect("held").state(), &LifecycleState::Gone);
}

// Behaviour: a checkout observed absent vanishes; observed again, it is not revived (no
// incarnations: its owner requests a replacement), so the old status stays Gone and the old
// reconciler is not run on it.
#[tokio::test]
async fn a_vanished_checkout_is_not_revived() {
    let home = lifecycles::<CheckoutBinding>();
    let ready = CheckoutStatusPatch::MarkReady { path: "/work/feature".into(), commit: None, branch_provenance: Default::default() };
    let engine = Engine::<_, CheckoutBinding>::new(
        Scripted::<OldCheckout>::new([Some(ready.clone()), Some(CheckoutStatusPatch::MarkGone), Some(ready)]),
        Some(home.clone()),
    );
    let mut obj = worktree(Some("convoy-a"));
    reconcile(&engine, &mut obj).await;
    assert_eq!(reconcile(&engine, &mut obj).await.patch, Some(CheckoutStatusPatch::MarkGone));
    assert_eq!(home.get("checkout-a").expect("held").state(), &LifecycleState::Gone);
    assert_eq!(reconcile(&engine, &mut obj).await.patch, None);
    assert_eq!(engine.inner().prepared.load(Ordering::SeqCst), 2);
    assert_eq!(obj.status.as_ref().map(|status| status.phase), Some(CheckoutPhase::Gone));
}

// Behaviour: when the old status drifts from the engine (here, a Ready checkout the old status
// shows Failed), an observation-only reconcile writes the engine's copy.
#[tokio::test]
async fn the_copy_restores_the_engine_state_over_a_drifted_old_status() {
    let home = lifecycles::<CheckoutBinding>();
    let ready =
        CheckoutStatusPatch::MarkReady { path: "/work/feature".into(), commit: Some("abc".into()), branch_provenance: Default::default() };
    let engine = Engine::<_, CheckoutBinding>::new(
        Scripted::<OldCheckout>::new([Some(ready), Some(CheckoutStatusPatch::ObserveDegradation { message: None })]),
        Some(home.clone()),
    );
    let mut obj = worktree(Some("convoy-a"));
    reconcile(&engine, &mut obj).await;
    obj.status = Some(CheckoutStatus { phase: CheckoutPhase::Failed, commit: Some("abc".into()), ..CheckoutStatus::default() });
    let outcome = reconcile(&engine, &mut obj).await;
    assert!(
        matches!(outcome.patch, Some(CheckoutStatusPatch::MarkReady { ref path, ref commit, .. }) if path == "/work/feature" && commit.as_deref() == Some("abc"))
    );
}

// Behaviour (#3719 look-over ruling 4): a checkout's request names who it is held for, its
// convoy (`convoy:<name>`), and its environment and clone at this home.
#[test]
fn a_checkout_request_names_its_holder() {
    let request = CheckoutBinding::request(&worktree(Some("convoy-a")), HOME).expect("request");
    assert_eq!(request.holder, Some(Holder::convoy("convoy-a")));
    assert_eq!(request.clone.as_ref().map(|clone| clone.name.as_str()), Some("clone-a"));
    assert_eq!(request.source, Source::Worktree);
    assert_eq!(request.environment.home, HOME);
}

// Behaviour (#3729 review): an empty holder label names no holder, and a host-direct environment
// with no checkout root is refused before it is requested.
#[test]
fn an_empty_label_holds_nothing_and_a_rootless_host_direct_environment_is_refused() {
    use flotilla_resources::{CONVOY_LABEL, VESSEL_REF_LABEL};

    use super::environment::EnvironmentBinding;
    let mut checkout = worktree(None);
    checkout.metadata.labels.insert(CONVOY_LABEL.into(), String::new());
    assert_eq!(CheckoutBinding::request(&checkout, HOME).expect("request").holder, None);
    let mut environment = environment_object(false);
    environment.metadata.labels.insert(VESSEL_REF_LABEL.into(), String::new());
    assert_eq!(EnvironmentBinding::request(&environment, HOME).expect("request").holder, None);
    environment.spec.host_direct.as_mut().expect("host direct").repo_default_dir = String::new();
    assert!(EnvironmentBinding::request(&environment, HOME).expect_err("refused").contains("checkout root"));
}

// Behaviour (#3719 look-over ruling 4): a checkout held for no one is still the engine's: the
// holder carries no ownership, so nothing refuses it.
#[tokio::test]
async fn a_checkout_held_for_no_one_is_the_engines() {
    let request = CheckoutBinding::request(&worktree(None), HOME).expect("request");
    assert_eq!(request.holder, None);
    let home = lifecycles::<CheckoutBinding>();
    let ready = CheckoutStatusPatch::MarkReady { path: "/work/feature".into(), commit: None, branch_provenance: Default::default() };
    let engine = Engine::<_, CheckoutBinding>::new(Scripted::<OldCheckout>::new([Some(ready.clone())]), Some(home.clone()));
    let mut obj = worktree(None);
    assert_eq!(reconcile(&engine, &mut obj).await.patch, Some(ready));
    assert_eq!(home.get("checkout-a").expect("held").state(), &LifecycleState::Realised { identity: "/work/feature".into() });
}

// Behaviour: steps for each kind's patches.
#[test]
fn patches_read_as_lifecycle_steps() {
    let obj = clone_object(None);
    assert_eq!(CloneBinding::steps(&obj, &CloneStatusPatch::MarkCloning), vec![]);
    assert_eq!(
        CloneBinding::steps(&obj, &CloneStatusPatch::MarkReady { default_branch: None }),
        vec![Step::Realised { identity: "/clones/repo".into() }]
    );
    let checkout = worktree(Some("c"));
    assert_eq!(CheckoutBinding::steps(&checkout, &CheckoutStatusPatch::MarkGone), vec![Step::Vanished]);
}

// Behaviour: with the switch off the wrapper is the old reconciler: its patch and requeue pass
// unchanged and nothing is requested.
#[tokio::test]
async fn without_lifecycles_the_old_reconciler_is_unchanged() {
    let engine =
        Engine::<_, CloneBinding>::new(Scripted::<OldClone>::new([Some(CloneStatusPatch::MarkRetrying { message: "x".into() })]), None);
    let mut obj = clone_object(None);
    let outcome = reconcile(&engine, &mut obj).await;
    assert_eq!(outcome.patch, Some(CloneStatusPatch::MarkRetrying { message: "x".into() }));
    assert_eq!(outcome.requeue_after, Some(std::time::Duration::from_secs(30)));
}

// Behaviour (#3719 review, look-over ruling 4): the checkout writer refuses a worktree without its
// clone; who it is held for, or whether it is held at all, is no check.
#[test]
fn the_checkout_writer_refuses_a_worktree_without_its_clone() {
    let valid = CheckoutBinding::request(&worktree(Some("convoy-a")), HOME).expect("valid");
    let orphan = CheckoutRequest { clone: None, ..valid.clone() };
    assert!(orphan.validate().expect_err("no clone").contains("names its clone"));
    assert_eq!(CheckoutRequest { holder: Some(Holder::ensure("standing")), ..valid.clone() }.validate(), Ok(()));
    assert_eq!(CheckoutRequest { holder: None, ..valid }.validate(), Ok(()));
}

fn environment_object(docker: bool) -> ResourceObject<flotilla_resources::Environment> {
    use flotilla_resources::{DockerEnvironmentSpec, EnvironmentSpec, HostDirectEnvironmentSpec};
    let provider = "host:feta/provider:docker".parse().expect("provider");
    let spec = if docker {
        EnvironmentSpec {
            provider_address: Some(provider),
            host_direct: None,
            docker: Some(DockerEnvironmentSpec {
                host_ref: String::new(),
                image: "registry/flotilla@sha256:00".into(),
                image_build_ref: Some("image-00-amd64".into()),
                declared_agent_adapters: Default::default(),
                required_agent_adapters: Default::default(),
                pull_policy: Default::default(),
                memory_policy: Default::default(),
                mounts: vec![],
                // A declared value, and what the vessel reconciler adds at launch today.
                env: [
                    ("A".to_owned(), "1".to_owned()),
                    (flotilla_resources::CREDENTIAL_REFS_ENV.to_owned(), "[\"github\"]".to_owned()),
                    ("FLOTILLA_CREW_SKILLS".to_owned(), "[]".to_owned()),
                ]
                .into(),
            }),
            cloud_sandbox: None,
        }
    } else {
        EnvironmentSpec {
            provider_address: Some("host:feta/provider:host-direct".parse().expect("provider")),
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: String::new(), repo_default_dir: "/work".into() }),
            docker: None,
            cloud_sandbox: None,
        }
    };
    ResourceObject { metadata: meta("env-a"), spec, status: None }
}

// Behaviour (#3738 on main): a cloud sandbox has no stored recipe in the look-over's shape, so it is
// no engine request and the old path keeps it.
#[test]
fn a_cloud_sandbox_environment_stays_on_the_old_path() {
    use super::environment::EnvironmentBinding;
    use flotilla_resources::{CloudSandboxEnvironmentSpec, CloudSandboxSize};
    let mut cloud = environment_object(false);
    cloud.spec.host_direct = None;
    cloud.spec.cloud_sandbox = Some(CloudSandboxEnvironmentSpec {
        image: "registry/flotilla@sha256:00".into(),
        size: CloudSandboxSize { cpus: 2, memory_bytes: 1 << 30, disk_bytes: 1 << 33 },
        lifetime_cap_seconds: None,
        env: Default::default(),
    });
    assert!(EnvironmentBinding::request(&cloud, HOME).expect_err("not held").contains("cloud sandbox"));
}

// Behaviour: an environment is realised under its container, or the host-direct root; its recipe is
// its provider's typed recipe (one image choice, nothing derived at launch, #3719 rulings 5-6), and
// its request names its image build at this home.
#[test]
fn an_environment_is_realised_under_its_container_or_root() {
    use super::environment::EnvironmentBinding;
    use flotilla_resources::EnvironmentStatusPatch;
    let ready = |container: Option<&str>| EnvironmentStatusPatch::MarkReady {
        configured_limits: None,
        docker_container_id: container.map(str::to_owned),
        image_ref: None,
        local_image_id: None,
        registry_digest: None,
    };
    assert_eq!(
        EnvironmentBinding::steps(&environment_object(true), &ready(Some("c0ffee"))),
        vec![Step::Realised { identity: "c0ffee".into() }]
    );
    assert_eq!(EnvironmentBinding::steps(&environment_object(false), &ready(None)), vec![Step::Realised { identity: "/work".into() }]);
    let mut held = environment_object(true);
    held.metadata.labels.insert(flotilla_resources::VESSEL_REF_LABEL.into(), "convoy-a-implement".into());
    let request = EnvironmentBinding::request(&held, HOME).expect("request");
    assert_eq!(request.holder, Some(Holder::vessel("convoy-a-implement")), "held for its vessel");
    assert_eq!(request.image_build.map(|build| (build.name, build.home)), Some(("image-00-amd64".into(), HOME.into())));
    // The Docker provider's recipe, read back by the provider: built, so the build is the column
    // alone, and with no credential grant or skills, which are composed at launch.
    let recipe = request.recipe.value().read::<DockerRecipe>().expect("the docker recipe");
    assert_eq!(recipe.image, DockerImage::Built);
    assert_eq!(recipe.env.keys().collect::<Vec<_>>(), ["A"], "the credential grant and the skills are not stored");
    let mut pinned = environment_object(true);
    pinned.spec.docker.as_mut().expect("docker").image_build_ref = None;
    let request = EnvironmentBinding::request(&pinned, HOME).expect("request");
    assert_eq!(request.image_build, None);
    assert_eq!(
        request.recipe.value().read::<DockerRecipe>().expect("recipe").image,
        DockerImage::Pinned { reference: "registry/flotilla@sha256:00".into() }
    );
}

// Behaviour: a Ready environment whose backing the observer reports failed has vanished; the
// copy then accepts Failed as Gone, so the old status does not flip between Lost and Failed.
#[tokio::test]
async fn a_realised_environment_whose_backing_failed_vanishes() {
    use super::{decide, environment::EnvironmentBinding};
    use flotilla_resources::{EnvironmentPhase, EnvironmentStatusPatch};
    let home = lifecycles::<EnvironmentBinding>();
    let mut obj = environment_object(true);
    let ready = EnvironmentStatusPatch::MarkReady {
        configured_limits: None,
        docker_container_id: Some("c0ffee".into()),
        image_ref: None,
        local_image_id: None,
        registry_digest: None,
    };
    let decision = decide::<EnvironmentBinding>(home.as_ref(), &obj, Some(ready), Utc::now());
    decision.patch.expect("ready").apply(obj.status.get_or_insert_with(Default::default));
    let failed = EnvironmentStatusPatch::MarkFailed { message: "container exited".into() };
    let decision = decide::<EnvironmentBinding>(home.as_ref(), &obj, Some(failed.clone()), Utc::now());
    assert!(decision.ended);
    assert_eq!(decision.patch, Some(failed.clone()));
    failed.apply(obj.status.get_or_insert_with(Default::default));
    assert_eq!(home.get("env-a").expect("held").state(), &LifecycleState::Gone);
    assert_eq!(obj.status.as_ref().map(|status| status.phase), Some(EnvironmentPhase::Failed));
    assert_eq!(EnvironmentBinding::copy(&obj, &home.get("env-a").expect("held")), None);
}

// Behaviour: an image build's infrastructure failures are retried until ImageBuild's policy
// (five attempts) is spent, a recipe failure gives up at once, and a build is realised under
// its digest.
#[tokio::test]
async fn image_build_failures_follow_the_build_policy() {
    use super::{decide, image_build::ImageBuildBinding};
    use flotilla_resources::{
        ImageBuild as OldImageBuild, ImageBuildFailure, ImageBuildReservation, ImageBuildResourceSpec, ImageBuildSpec,
        ImageBuildStatusPatch, ImageDelivery,
    };
    let object = || ResourceObject::<OldImageBuild> {
        metadata: meta("image-a"),
        spec: ImageBuildResourceSpec::Current(Box::new(ImageBuildSpec {
            identity: "sha256:feed".into(),
            architecture: "amd64".into(),
            layers: vec![],
            host_ref: "feta".into(),
            delivery: ImageDelivery::Local,
            reservation: ImageBuildReservation { cpu: 1, disk_bytes: 1 },
        })),
        status: None,
    };
    let failed = |class| ImageBuildStatusPatch::Failed {
        at: DateTime::UNIX_EPOCH,
        failure: ImageBuildFailure { class, reason: "boom".into() },
        probes: vec![],
        log_ref: String::new(),
    };
    let home = lifecycles::<ImageBuildBinding>();
    let obj = object();
    for attempt in 1..=5 {
        let infrastructure = failed(flotilla_resources::ImageBuildFailureClass::Infrastructure);
        let decision = decide::<ImageBuildBinding>(home.as_ref(), &obj, Some(infrastructure.clone()), Utc::now());
        assert_eq!(decision.ended, attempt == 5, "attempt {attempt}");
        if attempt < 5 {
            assert_eq!(decision.patch, Some(infrastructure), "retried");
        } else {
            // The old status would still retry it: the engine's copy settles it as failed instead.
            assert!(
                matches!(decision.patch, Some(ImageBuildStatusPatch::Failed { ref failure, .. }) if failure.class == flotilla_resources::ImageBuildFailureClass::Recipe)
            );
        }
    }
    assert!(matches!(home.get("image-a").expect("held").state(), LifecycleState::GaveUp { .. }));

    let home = lifecycles::<ImageBuildBinding>();
    let decision =
        decide::<ImageBuildBinding>(home.as_ref(), &obj, Some(failed(flotilla_resources::ImageBuildFailureClass::Recipe)), Utc::now());
    assert!(decision.ended, "a recipe failure gives up at once");

    let home = lifecycles::<ImageBuildBinding>();
    let built =
        ImageBuildStatusPatch::Built { at: DateTime::UNIX_EPOCH, digest: "sha256:beef".into(), probes: vec![], log_ref: "log".into() };
    assert_eq!(decide::<ImageBuildBinding>(home.as_ref(), &obj, Some(built.clone()), Utc::now()).patch, Some(built));
    assert_eq!(home.get("image-a").expect("held").state(), &LifecycleState::Realised { identity: "sha256:beef".into() });
}

// Behaviour (#3729 review): when the controller's old-status write fails after the engine took a
// step, the next reconcile's report is a new try made by the old reconciler's prepare, so the
// engine counts it once more; the copy then brings the old status level with the engine.
#[tokio::test]
async fn a_lost_status_write_leaves_the_engine_counting_each_try_once() {
    let home = lifecycles::<CloneBinding>();
    let retrying = || Some(CloneStatusPatch::MarkRetrying { message: "network unreachable".into() });
    let engine = Engine::<_, CloneBinding>::new(Scripted::<OldClone>::new([retrying(), retrying()]), Some(home.clone()));
    let obj = clone_object(Some(ClonePhase::Pending));
    // Two reconciles whose patches are never written: the old status stays Pending.
    for tries in 1..=2u32 {
        let prepared = engine.prepare(&obj).await.expect("prepare");
        assert_eq!(engine.reconcile(&obj, &prepared, Utc::now()).patch, retrying());
        assert!(matches!(home.get("clone-a").expect("held").state(), LifecycleState::Requested { failures, .. } if *failures == tries));
    }
    assert_eq!(engine.inner().prepared.load(Ordering::SeqCst), 2, "each count is a try the old reconciler made");
}

fn session_object(phase: flotilla_resources::TerminalSessionPhase) -> ResourceObject<flotilla_resources::TerminalSession> {
    use flotilla_resources::{TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus};
    ResourceObject {
        metadata: meta("terminal-vessel-a-coder"),
        spec: TerminalSessionSpec {
            env_ref: "env-vessel-a".into(),
            role: "coder".into(),
            source: TerminalSessionSource::Tool { command: "bash".into() },
            cwd: "/workspace".into(),
            pool: "cleat".into(),
            env: Default::default(),
        },
        status: Some(TerminalSessionStatus { phase, ..Default::default() }),
    }
}

// Behaviour (#3720 look-over, ruling 1): a crew's session is held for its vessel, by holder
// address, with no convoy of its own; a tool session has no holder.
#[test]
fn a_crew_session_is_held_for_its_vessel() {
    use flotilla_resources::{Selector, TerminalBrief, TerminalCrewContext, TerminalSessionPhase, TerminalSessionSource};

    use super::terminal_session::TerminalSessionBinding;
    use crate::stored::Holder;
    let tool = session_object(TerminalSessionPhase::Starting);
    assert_eq!(TerminalSessionBinding::request(&tool, HOME).expect("request").holder, None);
    let mut agent = tool;
    agent.spec.source = TerminalSessionSource::Agent {
        selector: Selector { capability: "coding".into(), adapter: None, model: None },
        brief: TerminalBrief { path: "/w/brief.md".into(), content: "# brief".into(), artifact_digest: None, copies: vec![] },
        context: Box::new(TerminalCrewContext { namespace: "flotilla".into(), convoy: "convoy-a".into(), vessel_ref: "vessel-a".into() }),
        message: None,
    };
    assert_eq!(TerminalSessionBinding::request(&agent, HOME).expect("request").holder, Some(Holder::vessel("vessel-a")));
}

// Behaviour (#3629 Q2, layer 7): each launch of a terminal session is a resource of its own. The
// first runs as `#1`; stopped, it is released; resumed in place by the old path, the next run is
// `#2`, and `#1` stays Gone. A lost session stays Realised (unobservable is a judgement); when the
// old path relaunches it, the lost launch is observed absent and the relaunch is `#3`.
#[tokio::test]
async fn each_launch_of_a_session_is_its_own_resource() {
    use flotilla_resources::{TerminalSessionPhase, TerminalSessionStatusPatch};

    use super::{launch, terminal_session::TerminalSessionBinding};
    let running = |id: &str| TerminalSessionStatusPatch::MarkRunning {
        configured_limits: None,
        session_id: id.into(),
        pid: None,
        started_at: DateTime::UNIX_EPOCH,
        crew: None,
        launch_command: "bash".into(),
        delivered_message_id: None,
    };
    let stopped = TerminalSessionStatusPatch::MarkStopped {
        stopped_at: DateTime::UNIX_EPOCH,
        inner_command_status: None,
        inner_exit_code: Some(0),
        message: None,
    };
    let lost = TerminalSessionStatusPatch::MarkLost { reason: "daemon generation dead".into(), lost_at: DateTime::UNIX_EPOCH };
    let home = lifecycles::<TerminalSessionBinding>();
    let engine = Engine::<_, TerminalSessionBinding>::new(
        Scripted::<flotilla_resources::TerminalSession>::new([
            Some(running("s1")),
            Some(stopped),
            Some(running("s2")),
            Some(lost),
            Some(TerminalSessionStatusPatch::MarkStarting),
            Some(running("s3")),
        ]),
        Some(home.clone()),
    );
    let name = "terminal-vessel-a-coder";
    let state = |n| home.get(&launch(name, n)).map(|resource| resource.state().clone());

    let mut obj = session_object(TerminalSessionPhase::Starting);
    reconcile(&engine, &mut obj).await;
    assert_eq!(state(1), Some(LifecycleState::Realised { identity: "s1".into() }));
    reconcile(&engine, &mut obj).await;
    assert_eq!(state(1), Some(LifecycleState::Gone), "stopped: released");
    assert_eq!(obj.status.as_ref().map(|status| status.phase), Some(TerminalSessionPhase::Stopped));
    // Ended, and not starting again: the old reconciler is not asked, and nothing is written.
    assert_eq!(reconcile(&engine, &mut obj).await.patch, None);
    assert_eq!(state(1), Some(LifecycleState::Gone));

    // The old path resumes it in place (crew_ops' MarkStarting): the next run is a new launch.
    TerminalSessionStatusPatch::MarkStarting.apply(obj.status.as_mut().expect("status"));
    reconcile(&engine, &mut obj).await;
    assert_eq!(state(1), Some(LifecycleState::Gone), "never revived");
    assert_eq!(state(2), Some(LifecycleState::Realised { identity: "s2".into() }));

    reconcile(&engine, &mut obj).await;
    assert_eq!(obj.status.as_ref().map(|status| status.phase), Some(TerminalSessionPhase::Lost));
    assert_eq!(state(2), Some(LifecycleState::Realised { identity: "s2".into() }), "unobservable stays Realised");
    reconcile(&engine, &mut obj).await;
    assert_eq!(state(2), Some(LifecycleState::Gone), "relaunched: the lost launch observed absent");
    reconcile(&engine, &mut obj).await;
    assert_eq!(state(3), Some(LifecycleState::Realised { identity: "s3".into() }));
}

// Behaviour (layer 7): a launch that fails before it runs gives up at once; a later start of the
// same old session is a new launch.
#[tokio::test]
async fn a_failed_launch_gives_up_and_the_next_start_is_a_new_launch() {
    use flotilla_resources::{TerminalSessionPhase, TerminalSessionStatusPatch};

    use super::{launch, terminal_session::TerminalSessionBinding};
    let home = lifecycles::<TerminalSessionBinding>();
    let failed = TerminalSessionStatusPatch::MarkFailed { message: "cleat refused".into(), stopped_at: None };
    let engine = Engine::<_, TerminalSessionBinding>::new(
        Scripted::<flotilla_resources::TerminalSession>::new([Some(failed.clone())]),
        Some(home.clone()),
    );
    let mut obj = session_object(TerminalSessionPhase::Starting);
    assert_eq!(reconcile(&engine, &mut obj).await.patch, Some(failed));
    assert!(matches!(home.get(&launch("terminal-vessel-a-coder", 1)).expect("held").state(), LifecycleState::GaveUp { .. }));
    // Failed is ended for the old path too: the next reconcile writes nothing.
    assert_eq!(reconcile(&engine, &mut obj).await.patch, None);
}

// Behaviour (#3747 review): the latest launch is found however many there are, and a launch key
// parses at its last `#`, so an old name holding a `#` of its own is kept whole.
#[test]
fn the_latest_launch_is_found_and_an_old_name_may_hold_a_hash() {
    use flotilla_resources::TerminalSessionPhase;

    use super::{at, current, launch, launched, next_launch, terminal_session::TerminalSessionBinding};
    let home = lifecycles::<TerminalSessionBinding>();
    let mut obj = session_object(TerminalSessionPhase::Starting);
    obj.metadata.name = "odd#5".into();
    let request = TerminalSessionBinding::request(&obj, HOME).expect("request");
    assert_eq!(current::<TerminalSessionBinding>(home.as_ref(), &obj).0, "odd#5#1");
    for n in 1..=13 {
        home.request(&launch("odd#5", n), request.clone(), at(DateTime::UNIX_EPOCH)).expect("request");
        assert_eq!(current::<TerminalSessionBinding>(home.as_ref(), &obj).0, launch("odd#5", n));
    }
    assert_eq!(next_launch("odd#5#13"), "odd#5#14");
    assert_eq!(launched("odd#5#13"), "odd#5");
}
