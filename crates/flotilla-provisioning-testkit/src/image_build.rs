//! In-process stand-ins for ImageBuild execution (ADR 0062): the build-service port, the host's
//! materials, and a controller step over the real ImageBuild reconciler. Shared by provisioning's
//! ImageBuild tests and core's image-check tests.
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
use flotilla_environment::environment::{
    image::{BuiltImage, FailedImageBuild, ImageBuildRequest, ImageDeliveryTarget, SecretFile},
    registry_auth::{RegistryAuthMaterial, RegistryConfig},
    ImageBuildService, RegistryAuth,
};
use flotilla_protocol::{EnvironmentId, HostSummary, NodeId, NodeInfo, SystemInfo};
use flotilla_provisioning::reconcilers::{image_build::registry_build_cache, ImageBuildMaterials, ImageBuildReconciler};
use flotilla_resources::{
    image_identity, Host, HostImageAction, HostSpec, HostStatus, ImageBuild, ImageBuildCapacity, ImageBuildFailure, ImageBuildFailureClass,
    ImageBuildPhase, ImageBuildReservation, ImageBuildSpec, ImageDelivery, ImageLayerProbe, ImageProbeOutcome, InputMeta,
    IMAGE_BUILD_MAX_ATTEMPTS,
};
use flotilla_store::{apply_status_patch, controller::Reconciler, ResourceBackend};
use flotilla_store_testkit::VirtualClock;
use tokio::sync::Notify;

pub const NS: &str = "test";
pub const HOST: &str = "builder";
pub const WRAPPER: &str = "flotilla_run_with_preludes exec \"$@\"";
pub const REPOSITORY: &str = "registry.test/flotilla/crew-images";

pub fn digest(fill: char) -> String {
    format!("sha256:{}", fill.to_string().repeat(64))
}

pub async fn host(backend: &ResourceBackend, architecture: &str, capacity: Option<ImageBuildCapacity>) {
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

pub fn builder(slots: u32) -> Option<ImageBuildCapacity> {
    Some(ImageBuildCapacity::Builder { architecture: "amd64".into(), slots, reservation: ImageBuildReservation { cpu: 2, disk_bytes: 1 } })
}

/// What a test wants the service to do for one call.
#[derive(Clone)]
pub enum Behaviour {
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
pub struct BuildService {
    pub expected: Mutex<BTreeMap<String, ImageBuildSpec>>,
    pub behaviour: Mutex<Behaviour>,
    pub calls: AtomicUsize,
    pub running: AtomicUsize,
    pub peak: AtomicUsize,
    pub release: Notify,
    pub dropped: AtomicUsize,
}

impl BuildService {
    pub fn new(behaviour: Behaviour) -> Arc<Self> {
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

    pub fn expect(&self, spec: &ImageBuildSpec) {
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
pub struct Materials {
    pub granted: BTreeSet<String>,
    pub push_auths: AtomicUsize,
    pub logs: Mutex<BTreeMap<String, String>>,
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

pub fn reconciler(
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
pub async fn tick(backend: &ResourceBackend, reconciler: &ImageBuildReconciler, clock: &VirtualClock, name: &str) {
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
pub async fn settle(backend: &ResourceBackend, reconciler: &ImageBuildReconciler, clock: &VirtualClock, name: &str) {
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
