//! ADR 0062 ImageBuild execution through the build-service port. The port
//! and the host's materials are in-process stand-ins; the store, the
//! reconcilers and admission's get-or-create are real.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::{
    image_build::{attribute_recipe_failure, image_build_retry_backoff, registry_build_cache},
    DockerEnvironmentRuntime, DockerProvisioning, EnvironmentReconciler, ImageBuildMaterials, ImageBuildReconciler,
};
use flotilla_environment::environment::{
    image::{BuiltImage, FailedImageBuild, ImageBuildRequest, ImageDeliveryTarget, SecretFile},
    registry_auth::{RegistryAuthMaterial, RegistryConfig},
    ImageBuildService, RegistryAuth,
};
use flotilla_protocol::{EnvironmentId, HostSummary, NodeId, NodeInfo, SystemInfo};
use flotilla_resources::{
    image_identity, DockerEnvironmentSpec, DockerImagePullPolicy, Environment, EnvironmentPhase, EnvironmentSpec, FleetDesignation,
    FleetDesignationSpec, Host, HostImageAction, HostSpec, HostStatus, ImageBuild, ImageBuildCapacity, ImageBuildFailure,
    ImageBuildFailureClass, ImageBuildPhase, ImageBuildReservation, ImageBuildResourceSpec, ImageBuildSpec, ImageCacheBinding,
    ImageDelivery, ImageLayerProbe, ImageLayerSource, ImageLayerSpec, ImageLayerTier, ImageProbeOutcome, InputMeta, ResolvedImage,
    FLEET_DESIGNATION_NAME, IMAGE_BUILD_MAX_ATTEMPTS,
};
use flotilla_store::{apply_status_patch, controller::Reconciler, ResourceBackend};
use flotilla_store_testkit::VirtualClock;
use tokio::sync::Notify;

pub(crate) const NS: &str = "test";
pub(crate) const HOST: &str = "builder";
const WRAPPER: &str = "flotilla_run_with_preludes exec \"$@\"";
const REPOSITORY: &str = "registry.test/flotilla/crew-images";

fn layer(name: &str, tier: ImageLayerTier, secrets: &[&str]) -> ImageLayerSpec {
    ImageLayerSpec::builder()
        .name(name.into())
        .version("1.0".into())
        .tier(tier)
        .source(if tier == ImageLayerTier::Project { ImageLayerSource::Project } else { ImageLayerSource::Stdlib })
        .maybe_project((tier == ImageLayerTier::Project).then(|| "wheelhouse".to_string()))
        .maybe_from((tier == ImageLayerTier::Base).then(|| format!("ubuntu@sha256:{}", "1".repeat(64))))
        .fragment("FROM ${BASE}\nRUN true\n".into())
        .probe(format!("probe {name}"))
        .build_secrets(secrets.iter().map(|secret| secret.to_string()).collect())
        .build()
}

/// A composition varied by `seed` so distinct seeds have distinct identities.
fn layers(seed: usize, secrets: &[&str]) -> Vec<ImageLayerSpec> {
    let mut base = layer("base", ImageLayerTier::Base, &[]);
    base.version = format!("1.{seed}");
    vec![base, layer("rust", ImageLayerTier::Shared, secrets), layer("wheelhouse", ImageLayerTier::Project, &[])]
}

fn build_spec(layers: Vec<ImageLayerSpec>, architecture: &str, delivery: ImageDelivery, cpu: u32) -> ImageBuildSpec {
    ImageBuildSpec::builder()
        .identity(image_identity(&layers))
        .architecture(architecture.into())
        .layers(layers)
        .host_ref(HOST.into())
        .delivery(delivery)
        .reservation(ImageBuildReservation { cpu, disk_bytes: 1 })
        .build()
}

pub(crate) fn digest(fill: char) -> String {
    format!("sha256:{}", fill.to_string().repeat(64))
}

pub(crate) async fn host(backend: &ResourceBackend, architecture: &str, capacity: Option<ImageBuildCapacity>) {
    let hosts = backend.using::<Host>(NS);
    let obj = hosts
        .create(&InputMeta::builder().name(HOST.into()).build(), &HostSpec { image_build_capacity: capacity, ..Default::default() })
        .await
        .expect("host");
    let status = HostStatus::builder()
        .capabilities(Default::default())
        .ready(true)
        .description(
            HostSummary::builder()
                .environment_id(EnvironmentId::new(HOST))
                .node(NodeInfo::new(NodeId::new(HOST), HOST.to_string()))
                .system(SystemInfo { arch: Some(architecture.into()), ..Default::default() })
                .build(),
        )
        .build();
    hosts.update_status(HOST, &obj.metadata.resource_version, &status).await.expect("host status");
}

pub(crate) fn builder(slots: u32) -> Option<ImageBuildCapacity> {
    Some(ImageBuildCapacity::Builder { architecture: "amd64".into(), slots, reservation: ImageBuildReservation { cpu: 2, disk_bytes: 1 } })
}

async fn create(backend: &ResourceBackend, spec: &ImageBuildSpec) -> String {
    let name = spec.resource_name();
    backend.using::<ImageBuild>(NS).create(&InputMeta::builder().name(name.clone()).build(), &spec.clone().into()).await.expect("build");
    name
}

/// What a test wants the service to do for one call.
#[derive(Clone)]
pub(crate) enum Behaviour {
    Succeed,
    Fail {
        class: ImageBuildFailureClass,
        probes: Vec<ImageProbeOutcome>,
        failed_step: Option<String>,
    },
    /// Wait until released.
    Gate,
    /// The build job panics.
    Panic,
}

/// In-process stand-in for the build-service port. It enforces the request
/// contract: the identity recomputes from the ordered layers, which are the
/// ImageBuild's layers in order; the architecture and probe CPUs are the
/// build's; the delivery target matches the build's delivery (a push carries
/// the registry cache sibling and a credential admitted for that repository);
/// exactly the declared build secrets are passed; and the probe wrapper is
/// the configured prelude wrapper.
pub(crate) struct BuildService {
    expected: Mutex<BTreeMap<String, ImageBuildSpec>>,
    behaviour: Mutex<Behaviour>,
    pub(crate) calls: AtomicUsize,
    running: AtomicUsize,
    peak: AtomicUsize,
    release: Notify,
    dropped: AtomicUsize,
}

impl BuildService {
    pub(crate) fn new(behaviour: Behaviour) -> Arc<Self> {
        Arc::new(Self {
            expected: Mutex::new(BTreeMap::new()),
            behaviour: Mutex::new(behaviour),
            calls: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            release: Notify::new(),
            dropped: AtomicUsize::new(0),
        })
    }

    pub(crate) fn expect(&self, spec: &ImageBuildSpec) {
        self.expected.lock().expect("expected").insert(spec.identity.clone(), spec.clone());
    }
}

struct Running<'a>(&'a BuildService);
impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::SeqCst);
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl ImageBuildService for BuildService {
    async fn build(&self, request: ImageBuildRequest<'_>, log: &mut String) -> Result<BuiltImage, FailedImageBuild> {
        let spec = self.expected.lock().expect("expected").get(request.identity).cloned().expect("an admitted identity");
        assert_eq!(image_identity(request.layers), request.identity, "identity recomputes from the layers");
        assert_eq!(request.layers, spec.layers.as_slice(), "the build's ordered layers");
        assert_eq!(request.architecture, spec.architecture);
        assert_eq!(request.probe_cpus, spec.reservation.cpu);
        assert_eq!(request.probe_wrapper, WRAPPER);
        let declared: BTreeSet<_> = spec.layers.iter().flat_map(|layer| layer.build_secrets.iter().cloned()).collect();
        assert_eq!(request.secrets.keys().cloned().collect::<BTreeSet<_>>(), declared, "exactly the declared secrets");
        match (&request.delivery, &spec.delivery) {
            (ImageDeliveryTarget::Push { repository, cache_repository, auth }, ImageDelivery::Registry { repository: expected }) => {
                assert_eq!(repository, expected);
                assert_eq!(cache_repository, &format!("{expected}/buildcache:{}", spec.architecture));
                assert_eq!(cache_repository, &registry_build_cache(expected, &spec.architecture));
                auth.validate(repository, HostImageAction::ImagePush).expect("push credential admitted for the repository");
            }
            (ImageDeliveryTarget::Load, ImageDelivery::Local) => {}
            (target, delivery) => panic!("delivery {target:?} does not match {delivery:?}"),
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(running, Ordering::SeqCst);
        let _running = Running(self);
        log.push_str(&format!("built {}\n", request.identity));
        let behaviour = self.behaviour.lock().expect("behaviour").clone();
        let probes = |outcomes: &[ImageProbeOutcome]| {
            request
                .layers
                .iter()
                .enumerate()
                .map(|(index, layer)| ImageLayerProbe {
                    layer: layer.name.clone(),
                    outcome: outcomes.get(index).cloned().unwrap_or(ImageProbeOutcome::Passed),
                })
                .collect::<Vec<_>>()
        };
        if matches!(behaviour, Behaviour::Gate) {
            self.release.notified().await;
        }
        match behaviour {
            Behaviour::Panic => panic!("the build service panicked"),
            Behaviour::Succeed | Behaviour::Gate => Ok(BuiltImage {
                digest: match request.delivery {
                    ImageDeliveryTarget::Push { repository, .. } => format!("{repository}@{}", digest('d')),
                    ImageDeliveryTarget::Load => digest('d'),
                },
                probes: probes(&[]),
            }),
            Behaviour::Fail { class, probes: outcomes, failed_step } => Err(FailedImageBuild {
                failure: ImageBuildFailure { class, reason: "build failed".into() },
                probes: probes(&outcomes),
                failed_step,
            }),
        }
    }
}

/// Credential-store stand-in for a push credential's material.
struct PushMaterial;
impl RegistryAuthMaterial for PushMaterial {
    fn config(&self) -> Result<RegistryConfig, String> {
        Err("the stand-in service never lowers credentials".into())
    }
    fn authorize(&self, _: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        panic!("builds make no HTTP registry calls")
    }
}

/// Stand-in for the daemon's credential store and blob store.
#[derive(Default)]
pub(crate) struct Materials {
    granted: BTreeSet<String>,
    push_auths: AtomicUsize,
    logs: Mutex<BTreeMap<String, String>>,
}

#[async_trait]
impl ImageBuildMaterials for Materials {
    async fn push_auth(&self, repository: &str) -> Result<RegistryAuth, ImageBuildFailure> {
        self.push_auths.fetch_add(1, Ordering::SeqCst);
        Ok(RegistryAuth::new(repository.into(), HostImageAction::ImagePush, Arc::new(PushMaterial)))
    }
    async fn secret(&self, name: &str) -> Result<SecretFile, ImageBuildFailure> {
        if self.granted.contains(name) {
            Ok(SecretFile::at(PathBuf::from(format!("/private/{name}"))))
        } else {
            Err(ImageBuildFailure { class: ImageBuildFailureClass::Recipe, reason: format!("build secret `{name}` is not granted") })
        }
    }
    async fn keep_log(&self, build: &str, attempt: u32, log: &str) -> Result<String, String> {
        let reference = format!("image-build-log-{build}-{attempt}");
        self.logs.lock().expect("logs").insert(reference.clone(), log.to_string());
        Ok(reference)
    }
}

pub(crate) fn reconciler(
    backend: &ResourceBackend,
    service: &Arc<BuildService>,
    materials: &Arc<Materials>,
    clock: &Arc<VirtualClock>,
) -> ImageBuildReconciler {
    ImageBuildReconciler::new(
        Some(Arc::clone(service) as Arc<dyn ImageBuildService>),
        Arc::clone(materials) as Arc<dyn ImageBuildMaterials>,
        backend,
        NS,
        HOST,
        WRAPPER,
    )
    .with_clock(Arc::clone(clock) as Arc<dyn flotilla_resources::Clock>)
    .with_liveness_interval(Duration::from_millis(5))
}

/// One controller step for `name`, as the controller loop performs it.
pub(crate) async fn tick(backend: &ResourceBackend, reconciler: &ImageBuildReconciler, clock: &VirtualClock, name: &str) {
    let builds = backend.using::<ImageBuild>(NS);
    let Ok(obj) = builds.get(name).await else { return };
    let prepared = reconciler.prepare(&obj).await.expect("prepare");
    let outcome = reconciler.reconcile(&obj, &prepared, flotilla_resources::Clock::now(clock));
    if let Some(patch) = outcome.patch {
        apply_status_patch(&builds, name, &patch).await.expect("status patch");
    }
}

/// Step until `name` settles (Built, or Failed with no retry pending) or a
/// step budget runs out, letting background jobs run between steps.
pub(crate) async fn settle(backend: &ResourceBackend, reconciler: &ImageBuildReconciler, clock: &VirtualClock, name: &str) {
    for _ in 0..200 {
        tick(backend, reconciler, clock, name).await;
        let status = backend.using::<ImageBuild>(NS).get(name).await.expect("build").status.unwrap_or_default();
        let retrying = status.failure.as_ref().is_some_and(|failure| failure.class == ImageBuildFailureClass::Infrastructure)
            && status.attempt < IMAGE_BUILD_MAX_ATTEMPTS;
        if status.phase == ImageBuildPhase::Built || (status.phase == ImageBuildPhase::Failed && !retrying) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn status(backend: &ResourceBackend, name: &str) -> flotilla_resources::ImageBuildStatus {
    backend.using::<ImageBuild>(NS).get(name).await.expect("build").status.unwrap_or_default()
}

// ADR 0062: an image is built once per identity. Concurrent admissions for
// one composition get-or-create one ImageBuild, and any number of
// (concurrent and repeated) controller steps execute it exactly once.
// Generated span: 2..8 concurrent admissions, 1..6 concurrent steps per round,
// registry or local delivery.
#[hegel::test]
fn concurrent_demand_executes_one_build_per_identity(tc: hegel::TestCase) {
    let admissions = tc.draw(hegel::generators::integers::<usize>().min_value(2).max_value(8));
    let fanout = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(6));
    let registry = tc.draw(hegel::generators::booleans());
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        host(&backend, "x86_64", builder(2)).await;
        if registry {
            backend
                .definitions::<flotilla_resources::Project>(NS)
                .create(
                    &InputMeta::builder().name("fleet".into()).build(),
                    &flotilla_resources::ProjectSpec::builder().display_name("Fleet".into()).build(),
                )
                .await
                .expect("fleet project");
            backend
                .definitions::<FleetDesignation>(NS)
                .create(
                    &InputMeta::builder().name(FLEET_DESIGNATION_NAME.into()).build(),
                    &FleetDesignationSpec {
                        project: "fleet".into(),
                        image_cache: Some(ImageCacheBinding {
                            repository: REPOSITORY.into(),
                            pull_credential: "crew-image-registry-pull".into(),
                            push_credential: "crew-image-registry-push".into(),
                        }),
                    },
                )
                .await
                .expect("fleet designation");
        }
        let layers = layers(0, &[]);
        let resolved = ResolvedImage { identity: image_identity(&layers), layers, shadowed: Vec::new() };
        // Each admission plans at placement and creates at its commit; a
        // concurrent creator's copy is reused.
        let names = futures::future::join_all((0..admissions).map(|_| async {
            let planned = flotilla_core::image_build::plan_image_build(&backend, NS, &resolved, HOST).await?;
            flotilla_core::image_build::create_planned_builds(&backend, NS, std::slice::from_ref(&planned)).await?;
            Ok::<_, String>(planned.name)
        }))
        .await;
        let name = names[0].clone().expect("admission");
        assert!(names.iter().all(|joined| joined.as_ref() == Ok(&name)), "every admission joins one build");
        let builds = backend.using::<ImageBuild>(NS).list().await.expect("list").items;
        assert_eq!(builds.len(), 1);
        let spec = builds[0].spec.current().expect("current").clone();
        assert_eq!(spec.delivery, if registry { ImageDelivery::Registry { repository: REPOSITORY.into() } } else { ImageDelivery::Local });
        let service = BuildService::new(Behaviour::Succeed);
        service.expect(&spec);
        let materials = Arc::new(Materials::default());
        let clock = Arc::new(VirtualClock::new(Utc::now()));
        let reconciler = reconciler(&backend, &service, &materials, &clock);
        for _ in 0..40 {
            futures::future::join_all((0..fanout).map(|_| tick(&backend, &reconciler, &clock, &name))).await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let built = status(&backend, &name).await;
        assert_eq!(built.phase, ImageBuildPhase::Built);
        assert_eq!(built.attempt, 1);
        assert_eq!(service.calls.load(Ordering::SeqCst), 1, "one execution per identity");
        assert_eq!(materials.push_auths.load(Ordering::SeqCst), usize::from(registry));
        let log_ref = built.log_ref.expect("log kept");
        assert!(materials.logs.lock().expect("logs")[&log_ref].contains(&spec.identity), "the build log is kept as the artifact");
    });
}

// ADR 0062 A12: infrastructure failures return to Queued with exponential
// backoff (30 s doubling) and never start before `not_before`; after
// IMAGE_BUILD_MAX_ATTEMPTS attempts the build stays Failed. Each attempt
// resolves a fresh push credential.
#[tokio::test]
async fn infrastructure_failures_retry_with_backoff_up_to_the_attempt_ceiling() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    host(&backend, "amd64", builder(1)).await;
    let spec = build_spec(layers(0, &[]), "amd64", ImageDelivery::Registry { repository: REPOSITORY.into() }, 2);
    let name = create(&backend, &spec).await;
    let service =
        BuildService::new(Behaviour::Fail { class: ImageBuildFailureClass::Infrastructure, probes: Vec::new(), failed_step: None });
    service.expect(&spec);
    let materials = Arc::new(Materials::default());
    let clock = Arc::new(VirtualClock::new(Utc::now()));
    let reconciler = reconciler(&backend, &service, &materials, &clock);
    for attempt in 1..=IMAGE_BUILD_MAX_ATTEMPTS {
        for _ in 0..200 {
            tick(&backend, &reconciler, &clock, &name).await;
            let current = status(&backend, &name).await;
            let settled = current.phase == ImageBuildPhase::Queued
                || (current.phase == ImageBuildPhase::Failed && attempt == IMAGE_BUILD_MAX_ATTEMPTS);
            if current.attempt == attempt && settled {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(service.calls.load(Ordering::SeqCst), attempt as usize);
        let current = status(&backend, &name).await;
        assert_eq!(current.failure.as_ref().map(|failure| failure.class), Some(ImageBuildFailureClass::Infrastructure));
        if attempt == IMAGE_BUILD_MAX_ATTEMPTS {
            for _ in 0..5 {
                tick(&backend, &reconciler, &clock, &name).await;
            }
            clock.advance(chrono::Duration::hours(2));
            tick(&backend, &reconciler, &clock, &name).await;
            let exhausted = status(&backend, &name).await;
            assert_eq!(exhausted.phase, ImageBuildPhase::Failed, "exhausted attempts stay failed");
            assert_eq!(exhausted.attempt, IMAGE_BUILD_MAX_ATTEMPTS);
            break;
        }
        assert_eq!(current.phase, ImageBuildPhase::Queued, "an infrastructure failure returns to Queued");
        let not_before = current.not_before.expect("backoff");
        assert_eq!(not_before - current.finished_at.expect("finished"), image_build_retry_backoff(attempt));
        // Before the backoff elapses nothing starts.
        clock.set(not_before - chrono::Duration::seconds(1));
        tick(&backend, &reconciler, &clock, &name).await;
        assert_eq!(status(&backend, &name).await.phase, ImageBuildPhase::Queued);
        clock.set(not_before);
    }
    assert_eq!(service.calls.load(Ordering::SeqCst), IMAGE_BUILD_MAX_ATTEMPTS as usize);
    assert_eq!(materials.push_auths.load(Ordering::SeqCst), IMAGE_BUILD_MAX_ATTEMPTS as usize, "a fresh credential per attempt");
    let backoffs: Vec<_> = (1..IMAGE_BUILD_MAX_ATTEMPTS).map(|attempt| image_build_retry_backoff(attempt).num_seconds()).collect();
    assert_eq!(backoffs, [30, 60, 120, 240]);
}

// A panicked build job ends its attempt as an infrastructure failure, so
// the retry backoff and the attempt ceiling apply: it is never re-spawned
// as the same attempt, and after IMAGE_BUILD_MAX_ATTEMPTS it stays Failed.
#[tokio::test]
async fn panicked_build_jobs_are_bounded_infrastructure_failures() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    host(&backend, "amd64", builder(1)).await;
    let spec = build_spec(layers(0, &[]), "amd64", ImageDelivery::Local, 1);
    let name = create(&backend, &spec).await;
    let service = BuildService::new(Behaviour::Panic);
    service.expect(&spec);
    let materials = Arc::new(Materials::default());
    let clock = Arc::new(VirtualClock::new(Utc::now()));
    let reconciler = reconciler(&backend, &service, &materials, &clock);
    for attempt in 1..=IMAGE_BUILD_MAX_ATTEMPTS {
        for _ in 0..200 {
            tick(&backend, &reconciler, &clock, &name).await;
            let current = status(&backend, &name).await;
            let settled = current.phase == ImageBuildPhase::Queued
                || (current.phase == ImageBuildPhase::Failed && attempt == IMAGE_BUILD_MAX_ATTEMPTS);
            if current.attempt == attempt && settled {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let current = status(&backend, &name).await;
        assert_eq!(service.calls.load(Ordering::SeqCst), attempt as usize, "one execution per attempt");
        let failure = current.failure.clone().expect("failure");
        assert_eq!(failure.class, ImageBuildFailureClass::Infrastructure);
        assert!(failure.reason.contains("panic"), "{}", failure.reason);
        assert!(current.probes.iter().all(|probe| probe.outcome == ImageProbeOutcome::NotRun));
        if attempt < IMAGE_BUILD_MAX_ATTEMPTS {
            assert_eq!(current.phase, ImageBuildPhase::Queued, "a panicked attempt backs off before the next");
            assert_eq!(current.not_before.expect("backoff") - current.finished_at.expect("finished"), image_build_retry_backoff(attempt));
            clock.set(current.not_before.expect("backoff"));
        }
    }
    for _ in 0..5 {
        tick(&backend, &reconciler, &clock, &name).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    clock.advance(chrono::Duration::hours(2));
    tick(&backend, &reconciler, &clock, &name).await;
    assert_eq!(status(&backend, &name).await.phase, ImageBuildPhase::Failed, "exhausted attempts stay failed");
    assert_eq!(service.calls.load(Ordering::SeqCst), IMAGE_BUILD_MAX_ATTEMPTS as usize);
}

// ADR 0062 A12: a recipe failure is terminal for its identity (no retry of
// identical inputs), keeps every layer's probe outcome and the build log,
// and names the owner of the failing layer: the first failed probe's layer,
// else the failing build step's layer, else the composition.
#[tokio::test]
async fn recipe_failures_are_terminal_with_probe_evidence_and_an_owner() {
    let cases = [
        (
            vec![ImageProbeOutcome::Passed, ImageProbeOutcome::Passed, ImageProbeOutcome::Failed { reason: "no cc".into() }],
            Some("rust"),
            "layer `wheelhouse` 1.0 from project `wheelhouse`",
        ),
        (
            vec![ImageProbeOutcome::NotRun, ImageProbeOutcome::NotRun, ImageProbeOutcome::NotRun],
            Some("rust"),
            "layer `rust` 1.0 from the flotilla stdlib",
        ),
        (vec![ImageProbeOutcome::NotRun, ImageProbeOutcome::NotRun, ImageProbeOutcome::NotRun], None, "the composition sha256:"),
    ];
    for (seed, (outcomes, failed_step, owner)) in cases.into_iter().enumerate() {
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        host(&backend, "amd64", None).await;
        let spec = build_spec(layers(seed, &[]), "amd64", ImageDelivery::Local, 1);
        let name = create(&backend, &spec).await;
        let service = BuildService::new(Behaviour::Fail {
            class: ImageBuildFailureClass::Recipe,
            probes: outcomes.clone(),
            failed_step: failed_step.map(str::to_string),
        });
        service.expect(&spec);
        let materials = Arc::new(Materials::default());
        let clock = Arc::new(VirtualClock::new(Utc::now()));
        let reconciler = reconciler(&backend, &service, &materials, &clock);
        settle(&backend, &reconciler, &clock, &name).await;
        clock.advance(chrono::Duration::days(1));
        for _ in 0..10 {
            tick(&backend, &reconciler, &clock, &name).await;
        }
        let failed = status(&backend, &name).await;
        assert_eq!(failed.phase, ImageBuildPhase::Failed);
        assert_eq!(service.calls.load(Ordering::SeqCst), 1, "identical inputs never retry");
        let failure = failed.failure.expect("failure");
        assert_eq!(failure.class, ImageBuildFailureClass::Recipe);
        assert!(failure.reason.contains(&format!("(owner: {owner}")), "{}", failure.reason);
        let probes: Vec<_> = failed.probes.iter().map(|probe| (probe.layer.as_str(), probe.outcome.clone())).collect();
        assert_eq!(probes, vec![("base", outcomes[0].clone()), ("rust", outcomes[1].clone()), ("wheelhouse", outcomes[2].clone())]);
        assert!(materials.logs.lock().expect("logs").contains_key(&failed.log_ref.expect("log kept")));
    }
}

// Glue: attribution leaves infrastructure failures unowned.
#[test]
fn infrastructure_failures_name_no_owner() {
    let spec = build_spec(layers(0, &[]), "amd64", ImageDelivery::Local, 1);
    let failed = FailedImageBuild {
        failure: ImageBuildFailure { class: ImageBuildFailureClass::Infrastructure, reason: "no such host".into() },
        probes: Vec::new(),
        failed_step: Some("rust".into()),
    };
    assert_eq!(attribute_recipe_failure(&spec, &failed).reason, "no such host");
}

// A declared secret that is not granted refuses the build naming the secret,
// as a recipe failure before the service runs; granted ones reach the
// service, and only those the layers declare.
#[tokio::test]
async fn build_secrets_are_resolved_by_declared_name_only() {
    for granted in [false, true] {
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        host(&backend, "amd64", None).await;
        let spec = build_spec(layers(0, &["npm-token"]), "amd64", ImageDelivery::Local, 1);
        let name = create(&backend, &spec).await;
        let service = BuildService::new(Behaviour::Succeed);
        service.expect(&spec);
        let mut materials = Materials::default();
        materials.granted.insert("unrelated".into());
        if granted {
            materials.granted.insert("npm-token".into());
        }
        let materials = Arc::new(materials);
        let clock = Arc::new(VirtualClock::new(Utc::now()));
        let reconciler = reconciler(&backend, &service, &materials, &clock);
        settle(&backend, &reconciler, &clock, &name).await;
        let settled = status(&backend, &name).await;
        if granted {
            assert_eq!(settled.phase, ImageBuildPhase::Built);
        } else {
            assert_eq!(settled.phase, ImageBuildPhase::Failed);
            assert!(settled.failure.expect("refusal").reason.contains("`npm-token`"));
            assert_eq!(service.calls.load(Ordering::SeqCst), 0);
        }
    }
}

// Host.spec.image_build_capacity bounds concurrent builds: with `slots`
// slots and more queued identities, at most `slots` are Building (published
// before any build process runs), and the rest start as slots free up. An
// undeclared capacity is one slot; an explicit None builds nothing.
#[rstest::rstest]
#[case::declared(builder(2), 2)]
#[case::undeclared(None, 1)]
#[case::forbidden(Some(ImageBuildCapacity::None), 0)]
#[tokio::test]
async fn build_slots_bound_concurrent_builds(#[case] capacity: Option<ImageBuildCapacity>, #[case] slots: usize) {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    host(&backend, "amd64", capacity).await;
    let service = BuildService::new(Behaviour::Gate);
    let mut names = Vec::new();
    for seed in 0..4 {
        let spec = build_spec(layers(seed, &[]), "amd64", ImageDelivery::Local, 1);
        service.expect(&spec);
        names.push(create(&backend, &spec).await);
    }
    let materials = Arc::new(Materials::default());
    let clock = Arc::new(VirtualClock::new(Utc::now()));
    let reconciler = reconciler(&backend, &service, &materials, &clock);
    let building = || async {
        let mut count = 0;
        for name in &names {
            count += usize::from(status(&backend, name).await.phase == ImageBuildPhase::Building);
        }
        count
    };
    for _ in 0..3 {
        for name in &names {
            tick(&backend, &reconciler, &clock, name).await;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(building().await, slots);
    assert_eq!(service.running.load(Ordering::SeqCst), slots);
    for _ in 0..200 {
        service.release.notify_waiters();
        for name in &names {
            tick(&backend, &reconciler, &clock, name).await;
        }
        assert!(building().await <= slots, "never more Building than slots");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let built = futures::future::join_all(names.iter().map(|name| status(&backend, name))).await;
    let expected = if slots == 0 { ImageBuildPhase::Queued } else { ImageBuildPhase::Built };
    assert!(built.iter().all(|status| status.phase == expected));
    assert!(service.peak.load(Ordering::SeqCst) <= slots);
}

// Builds for another architecture than this host's, or for another host, are
// never executed here.
#[tokio::test]
async fn foreign_architectures_and_hosts_are_ignored() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    host(&backend, "amd64", builder(2)).await;
    let arm = build_spec(layers(0, &[]), "arm64", ImageDelivery::Local, 1);
    let mut elsewhere = build_spec(layers(1, &[]), "amd64", ImageDelivery::Local, 1);
    elsewhere.host_ref = "other".into();
    let service = BuildService::new(Behaviour::Succeed);
    let materials = Arc::new(Materials::default());
    let clock = Arc::new(VirtualClock::new(Utc::now()));
    let reconciler = reconciler(&backend, &service, &materials, &clock);
    for spec in [arm, elsewhere] {
        service.expect(&spec);
        let name = create(&backend, &spec).await;
        for _ in 0..5 {
            tick(&backend, &reconciler, &clock, &name).await;
        }
        assert!(backend.using::<ImageBuild>(NS).get(&name).await.expect("build").status.is_none());
    }
    assert_eq!(service.calls.load(Ordering::SeqCst), 0);
}

// Deleting a Building ImageBuild (the operator retry, or withdrawn demand)
// abandons its running job; nothing is recorded and the service future is
// dropped.
#[tokio::test]
async fn deleting_a_building_build_abandons_its_job() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    host(&backend, "amd64", None).await;
    let spec = build_spec(layers(0, &[]), "amd64", ImageDelivery::Local, 1);
    let name = create(&backend, &spec).await;
    let service = BuildService::new(Behaviour::Gate);
    service.expect(&spec);
    let materials = Arc::new(Materials::default());
    let clock = Arc::new(VirtualClock::new(Utc::now()));
    let reconciler = reconciler(&backend, &service, &materials, &clock);
    for _ in 0..3 {
        tick(&backend, &reconciler, &clock, &name).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(service.running.load(Ordering::SeqCst), 1);
    backend.using::<ImageBuild>(NS).delete(&name).await.expect("delete");
    for _ in 0..200 {
        if service.dropped.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(service.dropped.load(Ordering::SeqCst), 1, "the running attempt is abandoned");
    assert!(materials.logs.lock().expect("logs").is_empty(), "an abandoned attempt records nothing");
}

// ADR 0047: retired ADR 0053 per-stage records are garbage-collected by the
// controller; nothing reads them.
#[tokio::test]
async fn retired_records_are_deleted() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let builds = backend.using::<ImageBuild>(NS);
    builds.create(&InputMeta::builder().name("old-stage".into()).build(), &ImageBuildResourceSpec::Retired).await.expect("retired");
    let service = BuildService::new(Behaviour::Succeed);
    let materials = Arc::new(Materials::default());
    let clock = Arc::new(VirtualClock::new(Utc::now()));
    // Hosts without a build service still collect retired records.
    let reconciler = ImageBuildReconciler::new(None, materials as Arc<dyn ImageBuildMaterials>, &backend, NS, HOST, WRAPPER);
    tick(&backend, &reconciler, &clock, "old-stage").await;
    assert!(builds.list().await.expect("list").items.is_empty());
    assert_eq!(service.calls.load(Ordering::SeqCst), 0);
}

/// Docker runtime stand-in: records what the provider was asked to run.
#[derive(Default)]
struct Docker {
    provisioned: Mutex<Vec<(String, DockerImagePullPolicy)>>,
}
#[async_trait]
impl DockerEnvironmentRuntime for Docker {
    async fn provision(&self, _name: &str, spec: &DockerEnvironmentSpec) -> Result<DockerProvisioning, String> {
        self.provisioned.lock().expect("provisioned").push((spec.image.clone(), spec.pull_policy));
        Ok(DockerProvisioning {
            configured_limits: None,
            container_id: "container".into(),
            image_ref: spec.image.clone(),
            local_image_id: digest('d'),
            registry_digest: spec.image.split_once('@').map(|(_, digest)| digest.to_string()),
        })
    }
    async fn destroy(&self, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
}

// End to end: a composed Environment waits while its build runs, then
// becomes Ready on the built digest: `repository@digest` pulled with the
// authored policy for registry delivery, the local image ID with pull policy
// Never for single-host local mode.
#[rstest::rstest]
#[case::registry(ImageDelivery::Registry { repository: REPOSITORY.into() })]
#[case::local(ImageDelivery::Local)]
#[tokio::test]
async fn environment_becomes_ready_with_the_built_digest(#[case] delivery: ImageDelivery) {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    host(&backend, "amd64", builder(1)).await;
    let spec = build_spec(layers(0, &[]), "amd64", delivery.clone(), 2);
    let name = create(&backend, &spec).await;
    let environments = backend.using::<Environment>(NS);
    environments
        .create(
            &InputMeta::builder().name("env".into()).build(),
            &EnvironmentSpec {
                host_direct: None,
                docker: Some(DockerEnvironmentSpec {
                    image_build_ref: Some(name.clone()),
                    memory_policy: Default::default(),
                    host_ref: HOST.into(),
                    image: String::new(),
                    declared_agent_adapters: BTreeSet::new(),
                    required_agent_adapters: BTreeSet::new(),
                    pull_policy: DockerImagePullPolicy::IfNotPresent,
                    mounts: Vec::new(),
                    env: Default::default(),
                }),
            },
        )
        .await
        .expect("environment");
    let docker = Arc::new(Docker::default());
    let environment = EnvironmentReconciler::new(Arc::clone(&docker), backend.clone(), NS);
    let step_environment = || async {
        let env = environments.get("env").await.expect("environment");
        let prepared = environment.prepare(&env).await.expect("prepare");
        if let Some(patch) = environment.reconcile(&env, &prepared, Utc::now()).patch {
            apply_status_patch(&environments, "env", &patch).await.expect("environment patch");
        }
        environments.get("env").await.expect("environment").status.expect("status")
    };
    let waiting = step_environment().await;
    assert_eq!(waiting.phase, EnvironmentPhase::Provisioning);
    assert!(docker.provisioned.lock().expect("provisioned").is_empty(), "no image before the build");

    let service = BuildService::new(Behaviour::Succeed);
    service.expect(&spec);
    let materials = Arc::new(Materials::default());
    let clock = Arc::new(VirtualClock::new(Utc::now()));
    let reconciler = reconciler(&backend, &service, &materials, &clock);
    settle(&backend, &reconciler, &clock, &name).await;
    let built = status(&backend, &name).await.digest.expect("built digest");
    let ready = step_environment().await;
    assert_eq!(ready.phase, EnvironmentPhase::Ready);
    let expected = match delivery {
        ImageDelivery::Registry { .. } => (format!("{REPOSITORY}@{}", digest('d')), DockerImagePullPolicy::IfNotPresent),
        ImageDelivery::Local => (digest('d'), DockerImagePullPolicy::Never),
    };
    assert_eq!(built, expected.0);
    assert_eq!(*docker.provisioned.lock().expect("provisioned"), vec![expected]);
}
