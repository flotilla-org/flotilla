//! The ImageBuild controller's provider work (#3108, ADR 0062): the shared [`Controller`] realises
//! an image build launch by building it through the host's [`ImageBuildService`], under the
//! built image's digest.
//!
//! - **Realise** resolves the build's push credential and the secrets its layers declare, runs the
//!   build and keeps its log as an artifact named for the launch and the try. Probe outcomes are in
//!   that log. A recipe failure names the owner of the failing layer in its cause.
//! - **Waits**, which are not tries: it is tried only while this host builds the launch's
//!   architecture (`Host.image_builds`, or this host's own architecture where undeclared), and at
//!   most as many at once as the host has build slots.
//! - **Failures**: an infrastructure failure (an unreachable registry, a lost log, a panicked
//!   build) is retryable at the kind's backoff, up to its five tries; a recipe failure gives up at
//!   once, since the same inputs cannot succeed.
//! - **Release**: once no home demands the build, its launch is released. The image stays in its
//!   cache; the image inventory owns removing it.
//!
//! [`Controller`]: super::controller::Controller

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_environment::environment::{
    image::{BuiltImage, FailedImageBuild, ImageBuildRequest as BuildRequest, ImageDeliveryTarget, SecretFile},
    ImageBuildService, RegistryAuth,
};
use flotilla_resources::{
    image_architecture::canonical_image_architecture, ImageBuildFailure, ImageBuildFailureClass, ImageBuildSpec, ImageDelivery,
    ImageLayerSource, ImageProbeOutcome,
};
use flotilla_schema::{Identity, Kind, KindCode, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use flotilla_state_machines::{machines::lifecycle::LifecycleState, Cause, CauseCategory};
use tokio::sync::Notify;

use super::{
    controller::{Actuator, Backing, Holding, ProviderFailure, Usability},
    environment_actuator::state_of,
    image_builds::{demands_of, spec_of},
    reads::StoreReads,
};
use crate::stored::{
    host::{HostObservation, HostRecord, ImageBuilds},
    image_build::{ImageBuildDemand, ImageBuildKind, ImageBuildRequest},
};

/// Host-held materials a build needs and the place its log is kept. The daemon implements it
/// with its credential store and blob store; crews never see any of it.
#[async_trait]
pub trait ImageBuildMaterials: Send + Sync {
    /// The push credential for `repository`, resolved freshly for one try.
    async fn push_auth(&self, repository: &str) -> Result<RegistryAuth, ImageBuildFailure>;
    /// A private file holding the named build secret's material. Unknown or ungranted names are
    /// recipe failures naming the secret.
    async fn secret(&self, name: &str) -> Result<SecretFile, ImageBuildFailure>;
    /// Keep one try's build log under `name` and return its reference.
    async fn keep_log(&self, build: &str, name: &str, log: &str) -> Result<String, String>;
}

/// The registry build cache for builds pushed to `repository` (ADR 0062 decision C4:
/// `…/buildcache` beside the images), one tag per architecture so concurrent builders of
/// different architectures do not overwrite each other's cache manifest.
pub fn registry_build_cache(repository: &str, architecture: &str) -> String {
    format!("{repository}/buildcache:{architecture}")
}

/// The artifact name of try `attempt` (from 1) of image build launch `launch` (`<build>#<n>`):
/// `<build logs of the launch>-<attempt>`.
pub fn build_log_name(launch: &str, attempt: u32) -> String {
    format!("{}-{attempt}", build_logs_of(launch))
}

/// The prefix of every try's build log of image build launch `launch`.
pub fn build_logs_of(launch: &str) -> String {
    format!("image-build-log-{}", launch.replace('#', "-"))
}

/// Every layer's probe outcome for a try that never reached its probes.
fn not_run_probes(spec: &ImageBuildSpec) -> Vec<flotilla_resources::ImageLayerProbe> {
    spec.layers
        .iter()
        .map(|layer| flotilla_resources::ImageLayerProbe { layer: layer.name.clone(), outcome: ImageProbeOutcome::NotRun })
        .collect()
}

/// Resolve this try's materials and drive the port. Only the secrets the layers declare are
/// resolved and passed.
async fn execute(
    service: &dyn ImageBuildService,
    materials: &dyn ImageBuildMaterials,
    launch: &str,
    spec: &ImageBuildSpec,
    probe_wrapper: &str,
    log: &mut String,
) -> Result<BuiltImage, FailedImageBuild> {
    let refuse = |failure: ImageBuildFailure| FailedImageBuild { failure, probes: not_run_probes(spec), failed_step: None };
    let declared = spec.layers.iter().flat_map(|layer| layer.build_secrets.iter().cloned()).collect::<BTreeSet<_>>();
    let mut secrets = BTreeMap::new();
    for name in declared {
        let file = materials.secret(&name).await.map_err(refuse)?;
        secrets.insert(name, file);
    }
    let delivery = match &spec.delivery {
        ImageDelivery::Registry { repository } => ImageDeliveryTarget::Push {
            repository: repository.clone(),
            cache_repository: registry_build_cache(repository, &spec.architecture),
            auth: materials.push_auth(repository).await.map_err(refuse)?,
        },
        ImageDelivery::Local => ImageDeliveryTarget::Load,
    };
    let request = BuildRequest::builder()
        .identity(&spec.identity)
        .architecture(&spec.architecture)
        .layers(&spec.layers)
        .delivery(delivery)
        .secrets(secrets)
        .probe_wrapper(probe_wrapper)
        .probe_cpus(spec.reservation.cpu)
        .build();
    let result = service.build(request, log).await;
    // A try's probe outcomes are kept in its log, a line per layer, and traced.
    let probes = match &result {
        Ok(built) => &built.probes,
        Err(failed) => &failed.probes,
    };
    for probe in probes {
        let outcome = match &probe.outcome {
            ImageProbeOutcome::Passed => "passed".to_owned(),
            ImageProbeOutcome::Failed { reason } => format!("failed: {reason}"),
            ImageProbeOutcome::NotRun => "not run".to_owned(),
        };
        tracing::info!(image_build = %launch, layer = %probe.layer, %outcome, "image layer probe");
        log.push_str(&format!("probe {}: {outcome}\n", probe.layer));
    }
    if let Err(failed) = &result {
        log.push_str(&format!("\n{}\n", failed.failure.reason));
    }
    result
}

/// Name the owner of a recipe failure in its reason (ADR 0062 decision A12): the first layer
/// whose probe failed, else the layer whose build step failed, else the composition as a whole.
pub fn attribute_recipe_failure(spec: &ImageBuildSpec, failed: &FailedImageBuild) -> ImageBuildFailure {
    let mut failure = failed.failure.clone();
    if failure.class != ImageBuildFailureClass::Recipe {
        return failure;
    }
    let layer = failed
        .probes
        .iter()
        .find(|probe| matches!(probe.outcome, ImageProbeOutcome::Failed { .. }))
        .map(|probe| probe.layer.as_str())
        .or(failed.failed_step.as_deref())
        .and_then(|name| spec.layers.iter().find(|layer| layer.name == name));
    let owner = match layer {
        Some(layer) => {
            let source = match (&layer.source, &layer.project) {
                (ImageLayerSource::Project, Some(project)) => format!("project `{project}`"),
                (ImageLayerSource::Project, None) => "a project".to_string(),
                (ImageLayerSource::Installation, _) => "the installation".to_string(),
                (ImageLayerSource::Stdlib, _) => "the flotilla stdlib".to_string(),
            };
            format!("layer `{}` {} from {source}", layer.name, layer.version)
        }
        None => format!("the composition {}", spec.identity),
    };
    failure.reason = format!("{} (owner: {owner})", failure.reason);
    failure
}

const HOST: KindCode = <HostRecord as Kind>::CODE;
const OBSERVATION: KindCode = <HostObservation as Kind>::CODE;
const DEMAND: KindCode = <ImageBuildDemand as Kind>::CODE;

/// The build slots this host offers for `architecture`, or `None` when it does not build that
/// architecture. An undeclared capacity is the single-host default of one slot for the host's own
/// architecture; a declared one may name another, built emulated: the operator's word.
fn slots(host: Option<&HostRecord>, observed_architecture: Option<&str>, architecture: &str) -> Option<u32> {
    match host?.image_builds.as_ref() {
        None => observed_architecture.is_none_or(|actual| canonical_image_architecture(actual) == architecture).then_some(1),
        Some(ImageBuilds::None) => None,
        Some(ImageBuilds::Builder { architecture: builds, slots, .. }) => {
            (canonical_image_architecture(builds) == architecture && *slots > 0).then_some(*slots)
        }
    }
}

/// The builds running here, bounded by the host's slots as each try reads them.
#[derive(Default)]
struct Running {
    count: Mutex<u32>,
    freed: Notify,
}

/// One running build's slot, freed when it drops.
struct Slot<'r>(&'r Running);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        *self.0.count.lock().unwrap_or_else(std::sync::PoisonError::into_inner) -= 1;
        self.0.freed.notify_waiters();
    }
}

impl Running {
    /// Waits for one of `slots`; waiting is no try.
    async fn take(&self, slots: u32) -> Slot<'_> {
        loop {
            let freed = self.freed.notified();
            {
                let mut count = self.count.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if *count < slots {
                    *count += 1;
                    return Slot(self);
                }
            }
            freed.await;
        }
    }
}

/// The ImageBuild kind's [`Actuator`] over this host's build service.
pub struct ImageBuildActuator {
    service: Arc<dyn ImageBuildService>,
    materials: Arc<dyn ImageBuildMaterials>,
    reads: StoreReads,
    homes: Box<dyn Fn() -> Vec<String> + Send + Sync>,
    home: String,
    host: String,
    probe_wrapper: Arc<str>,
    running: Running,
}

impl ImageBuildActuator {
    /// The actuator of `host`, whose daemon is `service`'s home.
    pub fn new(
        service: Arc<dyn ImageBuildService>,
        materials: Arc<dyn ImageBuildMaterials>,
        store: Arc<Service>,
        host: impl Into<String>,
        probe_wrapper: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            service,
            materials,
            reads: StoreReads::new(store.store()),
            home: store.home().to_owned(),
            homes: Box::new(move || store.homes()),
            host: host.into(),
            probe_wrapper: probe_wrapper.into(),
            running: Running::default(),
        }
    }

    /// The slots this host offers for `architecture`, as its records say now.
    fn host_slots(&self, read: &mut Read<'_>, architecture: &str) -> Result<Option<u32>, StoreError> {
        let host = match read.resolve(HOST, &self.host, &self.home)? {
            Some(id) => read.load::<HostRecord>(id)?.map(|loaded| loaded.value),
            None => None,
        };
        let observed = match read.resolve(OBSERVATION, &self.host, &self.home)? {
            Some(id) => read.load::<HostObservation>(id)?.and_then(|loaded| loaded.value.architecture),
            None => None,
        };
        Ok(slots(host.as_ref(), observed.as_deref(), architecture))
    }

    /// The try `name` runs next: one past its failed tries.
    fn attempt(&self, name: &str) -> u32 {
        let state = self.reads.read(|read| Ok(state_of::<ImageBuildKind>(read, name, &self.home)?));
        match state {
            Ok(Some(LifecycleState::Requested { failures, .. })) => failures + 1,
            _ => 1,
        }
    }
}

fn infrastructure(reason: String) -> ProviderFailure {
    ProviderFailure {
        cause: Cause::new(CauseCategory::Transient, reason),
        retryable: true,
        // Time alone may cure it (a registry back, a credential minted): the kind's backoff decides when.
        retry_at: Some(Timestamp::from_datetime(&chrono::Utc::now())),
    }
}

#[async_trait]
impl Actuator<ImageBuildKind> for ImageBuildActuator {
    async fn observe(&self, name: &str, _: &ImageBuildRequest) -> Result<Backing, Cause> {
        // A built image is kept where it was delivered; a requested build is built again.
        let state = self
            .reads
            .read(|read| Ok(state_of::<ImageBuildKind>(read, name, &self.home)?))
            .map_err(|error| Cause::new(CauseCategory::Transient, format!("read image build {name}: {error}")))?;
        Ok(match state {
            Some(LifecycleState::Realised { identity }) => Backing::Present { identity },
            _ => Backing::Absent { reason: None },
        })
    }

    async fn realise(&self, name: &str, request: &ImageBuildRequest) -> Result<String, ProviderFailure> {
        let spec = spec_of(request, &self.host);
        let slots = self
            .reads
            .read(|read| Ok(self.host_slots(read, &spec.architecture)?))
            .map_err(|error| infrastructure(format!("read this host's build slots: {error}")))?
            .ok_or_else(|| infrastructure(format!("this host does not build {} images", spec.architecture)))?;
        let _slot = self.running.take(slots).await;
        let attempt = self.attempt(name);
        let (service, materials, wrapper, owned, launch) =
            (Arc::clone(&self.service), Arc::clone(&self.materials), Arc::clone(&self.probe_wrapper), spec.clone(), name.to_owned());
        // Its own task, so a panicking build ends this try as an infrastructure failure.
        let built = tokio::spawn(async move {
            let mut log = String::new();
            let outcome = execute(service.as_ref(), materials.as_ref(), &launch, &owned, &wrapper, &mut log).await;
            (outcome, log)
        })
        .await;
        let (outcome, log) = match built {
            Ok(built) => built,
            Err(error) => return Err(infrastructure(format!("the image build job ended abnormally: {error}"))),
        };
        let kept = self.materials.keep_log(&request.launch_of, &build_log_name(name, attempt), &log).await;
        match (outcome, kept) {
            (Ok(built), Ok(_)) => Ok(built.digest),
            // ADR 0062 keeps every build log; a build whose log was lost is tried again, cheaply
            // from BuildKit's cache.
            (Ok(_), Err(error)) => Err(infrastructure(format!("the build log could not be kept: {error}"))),
            (Err(failed), _) => {
                let failure = attribute_recipe_failure(&spec, &failed);
                tracing::error!(role = "infra", image_build = %name, class = ?failure.class, attempt, reason = %failure.reason, "image build failed");
                Err(match failure.class {
                    ImageBuildFailureClass::Infrastructure => infrastructure(failure.reason),
                    ImageBuildFailureClass::Recipe => ProviderFailure::new(Cause::new(CauseCategory::Misconfigured, failure.reason), false),
                })
            }
        }
    }

    async fn release(&self, _: &str, _: &ImageBuildRequest, _: Option<&str>) -> Result<(), ProviderFailure> {
        // The image stays in its cache: the image inventory owns removing it.
        Ok(())
    }

    fn followed(&self) -> Vec<KindCode> {
        vec![HOST, OBSERVATION, DEMAND]
    }

    fn dependencies(&self, _: &ImageBuildRequest) -> Vec<Identity> {
        Vec::new()
    }

    fn holder(&self, read: &mut Read<'_>, _: &str, request: &ImageBuildRequest) -> Result<Holding, StoreError> {
        // Held while any home demands the build.
        Ok(Holding::ended(demands_of(read, &(self.homes)(), &request.launch_of)?.is_empty()))
    }

    fn usability(&self, read: &mut Read<'_>, request: &ImageBuildRequest) -> Result<Vec<Usability>, StoreError> {
        let spec = spec_of(request, &self.host);
        let builds = self.host_slots(read, &spec.architecture)?.is_some();
        let mut facts = vec![
            Usability { fact: Identity { kind: HOST, name: self.host.clone(), home: self.home.clone() }, usable: builds },
            Usability { fact: Identity { kind: OBSERVATION, name: self.host.clone(), home: self.home.clone() }, usable: builds },
        ];
        // Each demand is followed, so the last one's withdrawal releases the build.
        facts.extend(demands_of(read, &(self.homes)(), &request.launch_of)?.into_iter().map(|fact| Usability { fact, usable: true }));
        Ok(facts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stored::host::{Connection, Reservation};

    fn host(image_builds: Option<ImageBuilds>) -> HostRecord {
        HostRecord { display_name: "builder".into(), connection: Connection::Daemon, image_builds, max_vessels: None, single_tenant: false }
    }

    fn builder(architecture: &str, slots: u32) -> Option<ImageBuilds> {
        Some(ImageBuilds::Builder { architecture: architecture.into(), slots, reservation: Reservation { cpu: 1, disk_bytes: 1 } })
    }

    // A declared builder capacity is the operator's word: an arm64 host that declares amd64
    // builds amd64 (emulated), while one that declares nothing builds only its own architecture,
    // in one slot; an explicit None or zero slots builds nothing.
    #[test]
    fn a_host_builds_what_it_declares_or_its_own_architecture() {
        assert_eq!(slots(Some(&host(builder("amd64", 2))), Some("aarch64"), "amd64"), Some(2));
        assert_eq!(slots(Some(&host(None)), Some("aarch64"), "amd64"), None);
        assert_eq!(slots(Some(&host(None)), Some("x86_64"), "amd64"), Some(1));
        assert_eq!(slots(Some(&host(Some(ImageBuilds::None))), Some("x86_64"), "amd64"), None);
        assert_eq!(slots(Some(&host(builder("amd64", 0))), Some("x86_64"), "amd64"), None);
        assert_eq!(slots(None, Some("x86_64"), "amd64"), None);
    }

    #[test]
    fn a_build_log_is_named_for_its_launch_and_try() {
        assert_eq!(build_log_name("image-abc-amd64#7", 2), "image-build-log-image-abc-amd64-7-2");
    }
}
