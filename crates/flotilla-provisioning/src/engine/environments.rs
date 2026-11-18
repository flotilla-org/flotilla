//! Environments on the engine (#3108): the request a provider's spec makes, the spec a stored
//! request shows its readers and providers, and the [`EnvironmentRecords`] port over the decided
//! store.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use flotilla_protocol::ConfiguredResourceLimits;
use flotilla_resources::{
    environment_records::{EnvironmentChange, EnvironmentObservations, EnvironmentRecord, EnvironmentRecords, LaunchFacts},
    CloudSandboxEnvironmentSpec, DockerEnvironmentSpec, EnvironmentSpec, HostDirectEnvironmentSpec,
};
use flotilla_schema::{Frozen, Kind, Ref, Timestamp};
use flotilla_schema_sqlite::{Read, StoreHandle};
use flotilla_state_machines::Resource;

use super::{
    reads::{indexed, keyed, view, ReadError, StoreReads, View},
    Lifecycles,
};
use crate::stored::{
    environment::{
        CloudSandboxRecipe, DockerImage, DockerRecipe, Environment, EnvironmentKind, EnvironmentRequest, HostDirectRecipe, Recipe,
        LAUNCH_DERIVED_ENV,
    },
    launch_facts::{EnvironmentLaunchFacts, LaunchLimits},
    Holder,
};

/// A provider's recipe, decoded by its shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecipeOf {
    Docker(DockerRecipe),
    HostDirect(HostDirectRecipe),
    Cloud(CloudSandboxRecipe),
}

impl RecipeOf {
    /// The recipe `recipe` holds. Each provider's recipe has fields the others lack (a
    /// host-direct root, Docker's adapters and pull policy, a cloud size), so its shape names it.
    pub fn of(recipe: &Recipe) -> Result<Self, String> {
        recipe
            .read::<HostDirectRecipe>()
            .map(Self::HostDirect)
            .or_else(|_| recipe.read::<DockerRecipe>().map(Self::Docker))
            .or_else(|_| recipe.read::<CloudSandboxRecipe>().map(Self::Cloud))
            .map_err(|_| "the recipe is none of the Docker, host-direct and cloud sandbox recipes".to_owned())
    }

    /// Whether the environment's teardown ends the processes run in it: a container or sandbox
    /// goes with them, while a host-direct environment's processes run on the host itself.
    pub fn teardown_ends_processes(&self) -> bool {
        match self {
            Self::Docker(_) | Self::Cloud(_) => true,
            Self::HostDirect(_) => false,
        }
    }
}

/// The request `spec` makes, held for `vessel`. Env entries derived at launch are left out: they
/// are composed again at each launch, never stored (look-over ruling 5). A built image names its
/// build at the builder's home, `builder_home` of the build's name, where its launches are.
pub fn request_of(
    spec: &EnvironmentSpec,
    vessel: Option<&str>,
    builder_home: impl Fn(&str) -> Result<String, String>,
) -> Result<EnvironmentRequest, String> {
    let provider = spec.resolved_provider_address(None)?.ok_or("an environment names no provider or host")?;
    let built = |name: &Option<String>| name.as_ref().map(|name| builder_home(name).map(|home| Ref::new(name.clone(), home))).transpose();
    // A composed image names its build, whose realised digest is read at creation.
    let image = |image_build: &Option<_>, reference: &str| match image_build {
        Some(_) => DockerImage::Built,
        None => DockerImage::Pinned { reference: reference.to_owned() },
    };
    let stored_env = |env: &BTreeMap<String, String>| {
        env.iter().filter(|(key, _)| !LAUNCH_DERIVED_ENV.contains(&key.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
    };
    let (recipe, image_build) = match (&spec.docker, &spec.host_direct, &spec.cloud_sandbox) {
        (Some(docker), None, None) => {
            let image_build = built(&docker.image_build_ref)?;
            let recipe = DockerRecipe {
                image: image(&image_build, &docker.image),
                declared_adapters: docker.declared_agent_adapters.clone(),
                required_adapters: docker.required_agent_adapters.clone(),
                pull_policy: docker.pull_policy,
                mounts: docker.mounts.clone(),
                env: stored_env(&docker.env),
            };
            recipe.validate(image_build.as_ref())?;
            (Recipe::of(&recipe)?, image_build)
        }
        (None, Some(host), None) => {
            let recipe = HostDirectRecipe { repo_default_dir: host.repo_default_dir.clone() };
            recipe.validate()?;
            (Recipe::of(&recipe)?, None)
        }
        (None, None, Some(cloud)) => {
            let image_build = built(&cloud.image_build_ref)?;
            let recipe = CloudSandboxRecipe {
                image: image(&image_build, &cloud.image),
                size: cloud.size.clone(),
                lifetime_cap_seconds: cloud.lifetime_cap_seconds,
                env: stored_env(&cloud.env),
                probe_needs: cloud.probe_needs.clone(),
            };
            recipe.validate(image_build.as_ref())?;
            (Recipe::of(&recipe)?, image_build)
        }
        _ => return Err("an environment is backed by exactly one of docker, host_direct and cloud_sandbox".into()),
    };
    let recipe = Frozen::new(recipe).map_err(|error| format!("freeze the provider's recipe: {error}"))?;
    Ok(EnvironmentRequest { provider, image_build, recipe, holder: vessel.map(Holder::vessel) })
}

/// The spec `request` shows its readers and providers. A built image's reference is empty: its
/// digest is the build's, read at launch.
pub fn spec_of(request: &EnvironmentRequest) -> Result<EnvironmentSpec, String> {
    let image_build_ref = request.image_build.as_ref().map(|build| build.name.clone());
    let reference = |image: &DockerImage| match image {
        DockerImage::Pinned { reference } => reference.clone(),
        DockerImage::Built => String::new(),
    };
    let mut spec =
        EnvironmentSpec { provider_address: Some(request.provider.clone()), host_direct: None, docker: None, cloud_sandbox: None };
    // The provider's host, when it is a host's provider; an account's cloud sandbox has none.
    let host = spec.host_ref().unwrap_or_default().to_owned();
    match RecipeOf::of(&request.recipe)? {
        RecipeOf::HostDirect(recipe) => {
            spec.host_direct = Some(HostDirectEnvironmentSpec { host_ref: host, repo_default_dir: recipe.repo_default_dir });
        }
        RecipeOf::Docker(recipe) => {
            spec.docker = Some(DockerEnvironmentSpec {
                host_ref: host,
                image: reference(&recipe.image),
                image_build_ref,
                declared_agent_adapters: recipe.declared_adapters,
                required_agent_adapters: recipe.required_adapters,
                pull_policy: recipe.pull_policy,
                mounts: recipe.mounts,
                env: recipe.env,
            });
        }
        RecipeOf::Cloud(recipe) => {
            spec.cloud_sandbox = Some(CloudSandboxEnvironmentSpec {
                probe_needs: recipe.probe_needs,
                image: reference(&recipe.image),
                image_build_ref,
                size: recipe.size,
                lifetime_cap_seconds: recipe.lifetime_cap_seconds,
                env: recipe.env,
            });
        }
    }
    Ok(spec)
}

/// The stored facts of a launch, from what its provider reported.
pub fn launch_facts(facts: &LaunchFacts) -> Result<EnvironmentLaunchFacts, String> {
    let limit = |value: Option<usize>| value.map(|value| value as u64);
    let limits = facts.configured_limits.clone().unwrap_or_default();
    Ok(EnvironmentLaunchFacts {
        image_ref: facts.image_ref.clone(),
        local_image_id: facts.local_image_id.clone(),
        registry_digest: facts.registry_digest.clone(),
        configured_limits: LaunchLimits {
            cpus: limit(limits.cpus),
            cpu_shares: limit(limits.cpu_shares),
            build_jobs: limit(limits.build_jobs),
            linker_threads: limit(limits.linker_threads),
        },
        provider_descriptor: facts
            .provider_descriptor
            .clone()
            .map(Frozen::new)
            .transpose()
            .map_err(|error| format!("freeze the provider descriptor: {error}"))?,
    })
}

/// A launch's facts as its readers see them. No configured limit known is no limits.
pub fn facts_of(facts: &EnvironmentLaunchFacts) -> LaunchFacts {
    let limits = &facts.configured_limits;
    let limit = |value: Option<u64>| value.map(|value| value as usize);
    let configured = ConfiguredResourceLimits {
        cpus: limit(limits.cpus),
        cpu_shares: limit(limits.cpu_shares),
        build_jobs: limit(limits.build_jobs),
        linker_threads: limit(limits.linker_threads),
    };
    LaunchFacts {
        image_ref: facts.image_ref.clone(),
        local_image_id: facts.local_image_id.clone(),
        registry_digest: facts.registry_digest.clone(),
        configured_limits: (configured != ConfiguredResourceLimits::default()).then_some(configured),
        provider_descriptor: facts.provider_descriptor.as_ref().map(|descriptor| (**descriptor).clone()),
    }
}

/// The facts of launch `name` at `home`, if it was realised.
fn stored_facts(read: &mut Read<'_>, name: &str, home: &str) -> Result<Option<LaunchFacts>, ReadError> {
    let Some(id) = read.resolve(EnvironmentLaunchFacts::CODE, name, home)? else { return Ok(None) };
    Ok(read.load::<EnvironmentLaunchFacts>(id)?.map(|loaded| facts_of(&loaded.value)))
}

/// The facts of every launch realised at `home`, by launch: one scan, joined by the caller.
fn facts_at(read: &mut Read<'_>, home: &str) -> Result<HashMap<String, LaunchFacts>, ReadError> {
    let mut facts = HashMap::new();
    let mut after = None;
    loop {
        let page = read.scan::<EnvironmentLaunchFacts>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<EnvironmentLaunchFacts>(id)? {
                facts.insert(loaded.name, facts_of(&loaded.value));
            }
        }
        after = page.next;
        if after.is_none() {
            return Ok(facts);
        }
    }
}

/// `view` as a reader sees it, with its launch's facts.
fn record(read: &mut Read<'_>, view: View<EnvironmentKind>) -> Result<EnvironmentRecord, ReadError> {
    let launch = stored_facts(read, &view.name, &view.home)?;
    record_with(view, launch)
}

/// `view` as a reader sees it, with `launch`, the facts its launch was realised with.
fn record_with(view: View<EnvironmentKind>, launch: Option<LaunchFacts>) -> Result<EnvironmentRecord, ReadError> {
    let spec = spec_of(&view.request).map_err(ReadError)?;
    let vessel = view.request.holder.as_ref().and_then(|holder| holder.0.strip_prefix("vessel:")).map(str::to_owned);
    let requested_at = view.created;
    Ok(EnvironmentRecord { reading: view.judgement.reading(), name: view.name, home: view.home, spec, vessel, requested_at, launch })
}

/// The [`EnvironmentRecords`] port over one decided store: this home's environments through its
/// Environment home, and every home's through keyed and indexed reads.
pub struct EngineEnvironments {
    lifecycles: Arc<dyn Lifecycles<EnvironmentKind>>,
    reads: StoreReads,
    homes: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    changes: tokio::sync::broadcast::Sender<EnvironmentChange>,
    observations: EnvironmentObservations,
}

/// How many changes a slow follower may fall behind by; one that lags rereads everything.
const CHANGES: usize = 256;

impl EngineEnvironments {
    /// The port over `store`, whose homes `homes` names.
    pub fn new(
        lifecycles: Arc<dyn Lifecycles<EnvironmentKind>>,
        store: Arc<StoreHandle>,
        homes: impl Fn() -> Vec<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            lifecycles,
            reads: StoreReads::new(store),
            homes: Arc::new(homes),
            changes: tokio::sync::broadcast::channel(CHANGES).0,
            observations: EnvironmentObservations::default(),
        }
    }

    /// The port over `service`'s store and homes.
    pub fn over(lifecycles: Arc<dyn Lifecycles<EnvironmentKind>>, service: Arc<flotilla_schema_sqlite::stream::Service>) -> Self {
        let store = service.store();
        Self::new(lifecycles, store, move || service.homes())
    }

    /// Follows the environments' records in `service`'s log, at every home, telling
    /// [`EnvironmentRecords::changes`] of each. It runs at the store's commits.
    pub fn follow(&self, service: Arc<flotilla_schema_sqlite::stream::Service>) -> Result<tokio::task::JoinHandle<()>, String> {
        let mut watch =
            super::watch::EngineWatch::start(service, [<Resource<Environment> as Kind>::CODE]).map_err(|error| error.to_string())?;
        let changes = self.changes.clone();
        Ok(tokio::spawn(async move {
            loop {
                let followed = watch.next(|_, followed| Ok(followed.into_iter().map(environment_change).collect::<Vec<_>>())).await;
                match followed {
                    // No follower is no reason to stop: one may subscribe later.
                    Ok(followed) => followed.into_iter().for_each(|change| drop(changes.send(change))),
                    Err(error) => tracing::warn!(%error, "the environments' changes could not be read from the log; retrying"),
                }
            }
        }))
    }

    fn home(&self) -> &str {
        self.lifecycles.home()
    }
}

impl EnvironmentRecords for EngineEnvironments {
    fn codes(&self) -> Vec<flotilla_schema::KindCode> {
        vec![<Resource<Environment> as Kind>::CODE, EnvironmentLaunchFacts::CODE]
    }

    fn cursor(&self) -> Option<Box<dyn flotilla_resources::lifecycle_reads::ChangeCursor<EnvironmentChange>>> {
        let cursor = super::log_cursor::LogCursor::start(
            self.reads.clone(),
            Arc::clone(&self.homes),
            [<Resource<Environment> as Kind>::CODE],
            environment_change,
        );
        match cursor {
            Ok(cursor) => Some(Box::new(cursor)),
            Err(error) => {
                tracing::warn!(%error, "the log could not be followed from here; readers read afresh");
                None
            }
        }
    }

    fn observations(&self) -> &EnvironmentObservations {
        &self.observations
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<EnvironmentChange> {
        self.changes.subscribe()
    }

    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<EnvironmentRecord>, String> {
        let home = home.unwrap_or(self.home()).to_owned();
        self.reads
            .read(|read| keyed::<EnvironmentKind>(read, name, &home)?.map(|view| record(read, view)).transpose())
            .map_err(|error| format!("engine Environment/{name}: {error}"))
    }

    fn find(&self, name: &str) -> Result<Option<EnvironmentRecord>, String> {
        let local = self.home().to_owned();
        self.reads
            .read(|read| {
                let mut found = None;
                for id in read.resolve_named(Resource::<Environment>::CODE, name)? {
                    let Some(view) = view::<EnvironmentKind>(read, id)? else { continue };
                    let here = view.home == local;
                    if found.is_none() || here {
                        found = Some(view);
                    }
                    if here {
                        break;
                    }
                }
                found.map(|view| record(read, view)).transpose()
            })
            .map_err(|error| format!("engine Environment/{name}: {error}"))
    }

    fn live(&self) -> Result<Vec<EnvironmentRecord>, String> {
        let names = self.lifecycles.live().into_iter().map(|resource| resource.name().to_owned()).collect::<Vec<_>>();
        let home = self.home().to_owned();
        self.reads
            .read(|read| {
                let mut records = Vec::new();
                for name in &names {
                    if let Some(view) = keyed::<EnvironmentKind>(read, name, &home)? {
                        records.push(record(read, view)?);
                    }
                }
                Ok(records)
            })
            .map_err(|error| format!("engine live environments: {error}"))
    }

    fn every(&self) -> Result<Vec<EnvironmentRecord>, String> {
        let homes = (self.homes)();
        self.reads
            .read(|read| {
                let mut records = Vec::new();
                for home in &homes {
                    let mut views = Vec::new();
                    let mut after = None;
                    loop {
                        let page = read.scan::<Resource<Environment>>(home, after, 64)?;
                        for id in page.ids {
                            views.extend(view::<EnvironmentKind>(read, id)?);
                        }
                        after = page.next;
                        if after.is_none() {
                            break;
                        }
                    }
                    // The home's launch facts in one scan, joined here.
                    let mut facts = facts_at(read, home)?;
                    for view in views {
                        let launch = facts.remove(&view.name);
                        records.push(record_with(view, launch)?);
                    }
                }
                Ok(records)
            })
            .map_err(|error| format!("engine environments at every home: {error}"))
    }

    fn held(&self, vessel: &str) -> Result<Vec<EnvironmentRecord>, String> {
        let holder = Holder::vessel(vessel);
        self.reads
            .read(|read| {
                let views = indexed::<EnvironmentKind, _>(read, &EnvironmentRequest::by_holder(&holder))?;
                views.into_iter().map(|view| record(read, view)).collect()
            })
            .map_err(|error| format!("engine environments held for vessel {vessel}: {error}"))
    }

    fn request(&self, name: &str, spec: &EnvironmentSpec, vessel: Option<&str>) -> Result<bool, String> {
        if self.lifecycles.get(name).is_some() {
            return Ok(false);
        }
        let homes = (self.homes)();
        let builder_home = |build: &str| {
            self.reads
                .read(|read| Ok(super::image_builds::builder_home(read, &homes, build)?))
                .map_err(|error| format!("find the builder of {build}: {error}"))?
                .ok_or_else(|| format!("image build {build} is demanded at no home this one follows"))
        };
        let request = request_of(spec, vessel, builder_home)?;
        let at = Timestamp::from_datetime(&chrono::Utc::now());
        self.lifecycles.request(name, request, at).map(|_| true).map_err(|error| format!("request environment {name}: {error}"))
    }
}

/// What a followed write of the kinds tells readers.
fn environment_change(followed: flotilla_engine_store::follow::Followed) -> EnvironmentChange {
    match followed {
        flotilla_engine_store::follow::Followed::Changed(identity) => EnvironmentChange::Changed(identity.name),
        flotilla_engine_store::follow::Followed::Rescan(_) => EnvironmentChange::Rescanned,
    }
}
