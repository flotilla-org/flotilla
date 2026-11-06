//! The Environment controller's provider work (#3108): the shared [`Controller`] realises an
//! environment through its provider, and releases it once its vessel has ended and no checkout is
//! made in it.
//!
//! - **Realise** builds the spec the provider takes from the stored request: a built image's
//!   digest and delivery are its ImageBuild's, and the env entries derived at launch are composed
//!   from the holder vessel ([`LaunchEnvironment`]). The provider adopts a backing already there.
//!   Its report becomes the launch's facts, staged for the Realise edge ([`StagedFacts`]).
//! - **Waits**: it is tried only while its provider is ready at its host
//!   (`HostObservation.providers[instance].readiness`) and its ImageBuild, if any, has finished.
//! - **Release** is held while a live Checkout is made in it (`checkout.by_environment`): the
//!   checkout's teardown runs commands there. Each checkout's commit looks again.
//! - **Loss** is pushed by the host's environment probe as a [`super::controller::BackingEvent`];
//!   at start a realised environment is taken as present, and the probe reports it if not.
//!
//! [`Controller`]: super::controller::Controller

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use flotilla_environment::environment::EnvironmentProvisionError;
use flotilla_resources::{DockerImagePullPolicy, EnvironmentSpec};
use flotilla_schema::{Address, Identity, Kind, KindCode, Ref};
use flotilla_schema_sqlite::{Read, StoreError};
use flotilla_state_machines::{
    machines::{
        lifecycle::{Lifecycle, LifecycleState},
        vessel::VesselKind,
    },
    Cause, CauseCategory, Resource,
};

use super::{
    controller::{Actuator, Backing, ProviderFailure, Usability},
    environments::{launch_facts, spec_of},
    launch_log::StagedFacts,
    reads::{keyed, StoreReads},
};
use crate::{
    reconcilers::environment::{EnvironmentProvisioning, EnvironmentRuntime},
    stored::{
        checkout::{CheckoutKind, CheckoutRequest},
        environment::{EnvironmentKind, EnvironmentRequest},
        host::{HostObservation, Readiness},
        image_build::{Delivery, ImageBuildKind},
        Holder,
    },
};
use flotilla_resources::environment_records::LaunchFacts;

/// The env entries derived at launch for a vessel's environment: its credential grants and its
/// crew's skills, composed from the vessel and its convoy each time it is launched.
#[async_trait]
pub trait LaunchEnvironment: Send + Sync {
    async fn launch_env(&self, vessel: &str) -> Result<BTreeMap<String, String>, String>;
}

/// No env derived at launch: environments held for no vessel, and tests.
pub struct NoLaunchEnvironment;

#[async_trait]
impl LaunchEnvironment for NoLaunchEnvironment {
    async fn launch_env(&self, _: &str) -> Result<BTreeMap<String, String>, String> {
        Ok(BTreeMap::new())
    }
}

/// The Environment kind's [`Actuator`] over the host's environment providers.
pub struct EnvironmentActuator<R> {
    runtime: Arc<R>,
    reads: StoreReads,
    home: String,
    staged: StagedFacts,
    launch: Arc<dyn LaunchEnvironment>,
}

impl<R> EnvironmentActuator<R> {
    pub fn new(
        runtime: Arc<R>,
        reads: StoreReads,
        home: impl Into<String>,
        staged: StagedFacts,
        launch: Arc<dyn LaunchEnvironment>,
    ) -> Self {
        Self { runtime, reads, home: home.into(), staged, launch }
    }
}

const IMAGE_BUILD: KindCode = <Resource<Lifecycle<ImageBuildKind>> as Kind>::CODE;
const CHECKOUT: KindCode = <Resource<Lifecycle<CheckoutKind>> as Kind>::CODE;
const VESSEL: KindCode = <Resource<Lifecycle<VesselKind>> as Kind>::CODE;
const OBSERVATION: KindCode = <HostObservation as Kind>::CODE;

fn failed(detail: impl Into<String>, retryable: bool) -> ProviderFailure {
    let category = if retryable { CauseCategory::Transient } else { CauseCategory::External };
    ProviderFailure { cause: Cause::new(category, detail.into()), retryable }
}

/// The vessel `holder` names, if it is a vessel's.
fn vessel_of(holder: &Option<Holder>) -> Option<&str> {
    holder.as_ref().and_then(|holder| holder.0.strip_prefix("vessel:"))
}

/// The host and instance of a host's provider; an account's provider has no host.
pub(crate) fn host_provider(provider: &Address) -> Option<(&str, &str)> {
    match provider.segments() {
        [host, instance] if host.kind().as_str() == "host" && instance.kind().as_str() == "provider" => {
            Some((host.value(), instance.value()))
        }
        _ => None,
    }
}

/// The lifecycle state of the `K` resource `name` at `home`.
pub(crate) fn state_of<K>(read: &mut Read<'_>, name: &str, home: &str) -> Result<Option<LifecycleState>, StoreError>
where
    K: flotilla_state_machines::machines::lifecycle::LifecycleKind,
    Resource<Lifecycle<K>>: Kind,
{
    let Some(id) = read.resolve(Resource::<Lifecycle<K>>::CODE, name, home)? else { return Ok(None) };
    Ok(read.load::<Resource<Lifecycle<K>>>(id)?.map(|loaded| loaded.value.state().clone()))
}

impl<R> EnvironmentActuator<R> {
    /// The image the provider runs for a built environment: its build's digest, and how the
    /// provider gets it. The controller tries only once the build has finished.
    fn built(&self, build: &Ref<Resource<Lifecycle<ImageBuildKind>>>) -> Result<(String, Delivery), ProviderFailure> {
        let view = self
            .reads
            .read(|read| keyed::<ImageBuildKind>(read, &build.name, &build.home))
            .map_err(|error| failed(format!("read ImageBuild {}: {error}", build.name), true))?
            .ok_or_else(|| failed(format!("ImageBuild {} awaiting replicated build status", build.name), true))?;
        match view.state {
            LifecycleState::Realised { identity } => Ok((identity, view.request.delivery)),
            LifecycleState::Requested { .. } => Err(failed(format!("ImageBuild {} is not built yet", build.name), true)),
            _ => Err(failed(format!("ImageBuild {} ended without an image: {}", build.name, view.judgement.reading().label()), false)),
        }
    }

    /// The spec the provider takes for `request`, with what is fixed at launch filled in.
    async fn launch_spec(&self, request: &EnvironmentRequest) -> Result<EnvironmentSpec, ProviderFailure> {
        let mut spec = spec_of(request).map_err(|error| failed(error, false))?;
        let built = match &request.image_build {
            Some(build) => Some(self.built(build)?),
            None => None,
        };
        if let Some(docker) = spec.docker.as_mut() {
            if let Some((digest, delivery)) = built {
                docker.image = digest;
                // A local build exists only on its builder. A registry digest is pinned, so pulling
                // it if absent is always safe; the provider pulls with its pull credential.
                docker.pull_policy = match delivery {
                    Delivery::Local => DockerImagePullPolicy::Never,
                    Delivery::Registry { .. } => DockerImagePullPolicy::IfNotPresent,
                };
            }
            if let Some(vessel) = vessel_of(&request.holder) {
                let derived = self.launch.launch_env(vessel).await.map_err(|error| failed(error, true))?;
                docker.env.extend(derived);
            }
        } else if let Some(cloud) = spec.cloud_sandbox.as_mut() {
            if let Some((digest, delivery)) = built {
                // A cloud provider reads the image from a registry, never from a builder's local store.
                if delivery == Delivery::Local {
                    let build = request.image_build.as_ref().map(|build| build.name.as_str()).unwrap_or_default();
                    return Err(failed(
                        format!("ImageBuild {build} is not delivered to the fleet image cache, where a cloud sandbox reads its image"),
                        false,
                    ));
                }
                cloud.image = digest;
            }
        }
        Ok(spec)
    }
}

/// The identity a provision is realised under, and the facts it fixes.
fn realised(name: &str, spec: &EnvironmentSpec, provisioning: EnvironmentProvisioning) -> (String, LaunchFacts) {
    match provisioning {
        // A cloud sandbox runs its image by digest, from a registry: the digest is its image's
        // identity, and the pinned reference is the registry digest.
        EnvironmentProvisioning::Cloud(cloud) => (
            cloud.backing_id,
            LaunchFacts {
                local_image_id: cloud.image_ref.rsplit_once('@').map(|(_, digest)| digest.to_owned()),
                registry_digest: Some(cloud.image_ref.clone()),
                image_ref: Some(cloud.image_ref),
                provider_descriptor: Some(*cloud.provider_descriptor),
                configured_limits: None,
            },
        ),
        EnvironmentProvisioning::Local(local) => {
            // A container, or the host-direct root.
            let identity = local
                .backing_id
                .or_else(|| spec.host_direct.as_ref().map(|host| host.repo_default_dir.clone()))
                .unwrap_or_else(|| name.to_owned());
            (
                identity,
                LaunchFacts {
                    image_ref: local.image_ref,
                    local_image_id: local.local_image_id,
                    registry_digest: local.registry_digest,
                    configured_limits: local.configured_limits,
                    provider_descriptor: None,
                },
            )
        }
    }
}

#[async_trait]
impl<R> Actuator<EnvironmentKind> for EnvironmentActuator<R>
where
    R: EnvironmentRuntime + 'static,
{
    async fn observe(&self, name: &str, _: &EnvironmentRequest) -> Result<Backing, Cause> {
        // A realised environment is present until the host's probe reports it gone; a requested
        // one is realised again, which adopts a backing that is already there.
        let state = self
            .reads
            .read(|read| Ok(state_of::<EnvironmentKind>(read, name, &self.home)?))
            .map_err(|error| Cause::new(CauseCategory::Transient, format!("read environment {name}: {error}")))?;
        Ok(match state {
            Some(LifecycleState::Realised { identity }) => Backing::Present { identity },
            _ => Backing::Absent { reason: None },
        })
    }

    async fn realise(&self, name: &str, request: &EnvironmentRequest) -> Result<String, ProviderFailure> {
        let spec = self.launch_spec(request).await?;
        let provisioning = match self.runtime.provision_environment(name, &spec).await {
            Ok(provisioning) => provisioning,
            // The backing is retained; another attempt adopts it.
            Err(EnvironmentProvisionError::RetryAdoption(message)) => {
                return Err(failed(format!("adoption failed; backing retained for retry: {message}"), true))
            }
            Err(error) if spec.docker.is_some() && error.to_string().starts_with("waiting on build:") => {
                return Err(failed(error.to_string(), true))
            }
            Err(error) => return Err(failed(error.to_string(), false)),
        };
        let (identity, facts) = realised(name, &spec, provisioning);
        let facts = launch_facts(&facts).map_err(|error| failed(error, false))?;
        self.staged.stage(name, facts);
        Ok(identity)
    }

    async fn release(&self, name: &str, request: &EnvironmentRequest, identity: Option<&str>) -> Result<(), ProviderFailure> {
        let host_direct = spec_of(request).map_err(|error| failed(error, false))?.host_direct.is_some();
        let released = match identity {
            // The host-direct root is not a backing: its provider only cleans up.
            _ if host_direct => self.runtime.cleanup(name).await,
            Some(backing) => self.runtime.destroy(name, backing).await,
            // A backing whose identity was never recorded.
            None => self.runtime.destroy_unrecorded(name).await,
        };
        released.map_err(|error| failed(format!("environment teardown failed: {error}"), true))
    }

    fn followed(&self) -> Vec<KindCode> {
        vec![IMAGE_BUILD, VESSEL, CHECKOUT, OBSERVATION]
    }

    fn dependencies(&self, request: &EnvironmentRequest) -> Vec<Identity> {
        let mut depends = Vec::new();
        if let Some(vessel) = vessel_of(&request.holder) {
            depends.push(Identity { kind: VESSEL, name: vessel.to_owned(), home: self.home.clone() });
        }
        if let Some(build) = &request.image_build {
            depends.push(Identity { kind: IMAGE_BUILD, name: build.name.clone(), home: build.home.clone() });
        }
        depends
    }

    fn holder_ended(&self, read: &mut Read<'_>, request: &EnvironmentRequest) -> Result<bool, StoreError> {
        // The vessel's lease is homed here, on its pinned host; its environment is requested only
        // once the lease exists, so an absent lease has ended.
        let Some(vessel) = vessel_of(&request.holder) else { return Ok(false) };
        Ok(state_of::<VesselKind>(read, vessel, &self.home)?.is_none_or(|state| !state.live()))
    }

    fn usability(&self, read: &mut Read<'_>, request: &EnvironmentRequest) -> Result<Vec<Usability>, StoreError> {
        let mut facts = Vec::new();
        if let Some((host, instance)) = host_provider(&request.provider) {
            if let Some(id) = read.resolve(OBSERVATION, host, &self.home)? {
                if let Some(observation) = read.load::<HostObservation>(id)? {
                    // A provider the host has not reported is not judged here: provisioning reports it.
                    if let Some(provider) = observation.value.providers.get(instance) {
                        facts.push(Usability {
                            fact: Identity { kind: OBSERVATION, name: host.to_owned(), home: self.home.clone() },
                            usable: provider.readiness == Readiness::Ready,
                        });
                    }
                }
            }
        }
        if let Some(build) = &request.image_build {
            // A build still being made holds the launch; a finished one, built or not, lets it run.
            let building = state_of::<ImageBuildKind>(read, &build.name, &build.home)?
                .is_none_or(|state| matches!(state, LifecycleState::Requested { .. }));
            facts.push(Usability {
                fact: Identity { kind: IMAGE_BUILD, name: build.name.clone(), home: build.home.clone() },
                usable: !building,
            });
        }
        Ok(facts)
    }

    fn release_blockers(&self, read: &mut Read<'_>, name: &str, _: &EnvironmentRequest) -> Result<Vec<Identity>, StoreError> {
        let query = CheckoutRequest::by_environment(&Ref::new(name.to_owned(), self.home.clone()));
        let mut blockers = Vec::new();
        let mut after = None;
        loop {
            let page = read.page(&query, after.as_ref(), 256)?;
            for id in page.ids {
                let Some(loaded) = read.load::<Resource<Lifecycle<CheckoutKind>>>(id)? else { continue };
                if loaded.value.state().live() {
                    blockers.push(Identity { kind: CHECKOUT, name: loaded.name, home: loaded.home });
                }
            }
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(blockers),
            }
        }
    }
}
