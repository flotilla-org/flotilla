//! The Environment controller (#3108) over a decided store, with a recording provider: requests go
//! through the environment port, and the outcomes are read back through it and from the provider.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_engine_store::{open_home, open_home_with, StoreLog};
use flotilla_environment::environment::EnvironmentProvisionError;
use flotilla_protocol::{ConfiguredResourceLimits, LifecycleReading};
use flotilla_provisioning::{
    engine::{
        controller::Controller,
        environment_actuator::{EnvironmentActuator, LaunchEnvironment},
        environments::EngineEnvironments,
        launch_log::{LaunchLog, StagedFacts},
        reads::StoreReads,
        LifecycleHome, Lifecycles, Step,
    },
    reconcilers::environment::{EnvironmentProvisioning, EnvironmentRuntime, LocalProvisioning},
    stored::{
        checkout::{CheckoutKind, CheckoutRequest, Source},
        decided_registry,
        environment::EnvironmentKind,
        host::{HostObservation, Provider, Readiness},
        image_build::{Architecture, Delivery, ImageBuildKind, ImageBuildRequest, Reservation},
        Holder,
    },
};
use flotilla_resources::{
    environment_records::EnvironmentRecords, DockerEnvironmentSpec, DockerImagePullPolicy, EnvironmentSpec, HostDirectEnvironmentSpec,
    ImageLayerSource, ImageLayerSpec, ImageLayerTier, SystemClock, CREDENTIAL_REFS_ENV,
};
use flotilla_schema::{Frozen, NewChange, Ref, Timestamp};
use flotilla_schema_sqlite::stream::{CommitListener, Service};
use flotilla_state_machines::machines::{
    convoy::{ConvoyId, Priority, VesselClass},
    vessel::{VesselKind, VesselSpec},
};
use flotilla_state_machines::placement::Host;
use tokio::sync::Notify;

const HOME: &str = "alpha";

/// What the provider was asked, in order.
#[derive(Default)]
struct Asked {
    provisioned: Vec<(String, EnvironmentSpec)>,
    destroyed: Vec<(String, String)>,
    cleaned: Vec<String>,
}

#[derive(Default)]
struct RecordingProvider(Mutex<Asked>);

impl RecordingProvider {
    fn asked(&self) -> std::sync::MutexGuard<'_, Asked> {
        self.0.lock().expect("asked")
    }

    fn provisioned(&self, name: &str) -> Vec<EnvironmentSpec> {
        self.asked().provisioned.iter().filter(|(asked, _)| asked == name).map(|(_, spec)| spec.clone()).collect()
    }
}

#[async_trait]
impl EnvironmentRuntime for RecordingProvider {
    async fn provision_environment(
        &self,
        name: &str,
        spec: &EnvironmentSpec,
    ) -> Result<EnvironmentProvisioning, EnvironmentProvisionError> {
        self.asked().provisioned.push((name.to_owned(), spec.clone()));
        Ok(EnvironmentProvisioning::Local(match &spec.docker {
            Some(docker) => LocalProvisioning {
                configured_limits: Some(ConfiguredResourceLimits { cpus: Some(4), ..ConfiguredResourceLimits::default() }),
                backing_id: Some(format!("ctr-{name}")),
                image_ref: Some(docker.image.clone()),
                local_image_id: Some("sha256:local".into()),
                registry_digest: None,
            },
            // A host-direct backing reports no identity: it is realised under its root.
            None => LocalProvisioning::default(),
        }))
    }

    async fn destroy(&self, environment_ref: &str, backing_id: &str) -> Result<(), String> {
        self.asked().destroyed.push((environment_ref.to_owned(), backing_id.to_owned()));
        Ok(())
    }

    async fn cleanup(&self, environment_ref: &str) -> Result<(), String> {
        self.asked().cleaned.push(environment_ref.to_owned());
        Ok(())
    }
}

/// Each vessel's launch-derived env: its credential names.
struct Credentials;

#[async_trait]
impl LaunchEnvironment for Credentials {
    async fn launch_env(&self, vessel: &str) -> Result<BTreeMap<String, String>, String> {
        Ok(BTreeMap::from([(CREDENTIAL_REFS_ENV.to_owned(), format!("[\"{vessel}-token\"]"))]))
    }
}

struct World {
    _directory: tempfile::TempDir,
    service: Arc<Service>,
    provider: Arc<RecordingProvider>,
    environments: Arc<EngineEnvironments>,
    vessels: Arc<dyn Lifecycles<VesselKind>>,
    builds: Arc<dyn Lifecycles<ImageBuildKind>>,
    checkouts: Arc<dyn Lifecycles<CheckoutKind>>,
    commits: Arc<Notify>,
    _listener: Arc<CommitListener>,
    _backing: tokio::sync::mpsc::Sender<flotilla_provisioning::engine::controller::BackingEvent>,
}

fn now() -> Timestamp {
    Timestamp::from_datetime(&chrono::Utc::now())
}

impl World {
    /// A decided store, its Environment controller started, and `observation` as this host's facts.
    fn start(observation: Option<HostObservation>) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let service =
            Arc::new(Service::open(&directory.path().join("decided.sqlite"), HOME, decided_registry().expect("registry")).expect("store"));
        let log = || StoreLog::new(service.store());
        let staged = StagedFacts::default();
        let lifecycles: Arc<dyn Lifecycles<EnvironmentKind>> =
            Arc::new(LifecycleHome::new(HOME, open_home_with(&log(), LaunchLog::new(log(), staged.clone())).expect("environments")));
        let commits = Arc::new(Notify::new());
        let listener: Arc<CommitListener> = {
            let commits = Arc::clone(&commits);
            Arc::new(move || commits.notify_waiters())
        };
        service.on_commit(Arc::downgrade(&listener));
        if let Some(observation) = observation {
            write_observation(&service, observation);
        }
        let provider = Arc::new(RecordingProvider::default());
        let actuator = Arc::new(EnvironmentActuator::new(
            Arc::clone(&provider),
            StoreReads::new(service.store()),
            HOME,
            staged,
            Arc::new(Credentials),
        ));
        let controller =
            Controller::start(Arc::clone(&lifecycles), actuator, Arc::new(SystemClock), Arc::clone(&service)).expect("controller");
        let (backing, receiver) = tokio::sync::mpsc::channel(8);
        tokio::spawn(controller.run(receiver));
        Self {
            environments: Arc::new(EngineEnvironments::new(lifecycles, service.store())),
            vessels: Arc::new(LifecycleHome::new(HOME, open_home(log()).expect("vessels"))),
            builds: Arc::new(LifecycleHome::new(HOME, open_home(log()).expect("builds"))),
            checkouts: Arc::new(LifecycleHome::new(HOME, open_home(log()).expect("checkouts"))),
            _directory: directory,
            service,
            provider,
            commits,
            _listener: listener,
            _backing: backing,
        }
    }

    fn reading(&self, name: &str) -> Option<LifecycleReading> {
        self.environments.get(name, None).expect("read").map(|record| record.reading)
    }

    fn ready(&self, name: &str) -> bool {
        matches!(self.reading(name), Some(LifecycleReading::Ready { .. }))
    }

    fn released(&self, name: &str) -> bool {
        self.reading(name) == Some(LifecycleReading::Released)
    }

    /// Waits for `holds`, rechecked at each commit.
    async fn until(&self, what: &str, holds: impl Fn(&Self) -> bool) {
        let wait = async {
            loop {
                let committed = self.commits.notified();
                tokio::pin!(committed);
                committed.as_mut().enable();
                if holds(self) {
                    return;
                }
                committed.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait).await.unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    fn vessel(&self, name: &str) {
        let lease = VesselSpec {
            convoy: ConvoyId::new("convoy", HOME),
            class: VesselClass::new("pinned"),
            shape: BTreeMap::new(),
            priority: Priority(0),
            host: Host::new(HOME),
        };
        self.vessels.request(name, lease, now()).expect("lease");
    }

    fn build(&self, name: &str) {
        let layer = ImageLayerSpec::builder()
            .name("base".into())
            .version("1.0.0".into())
            .tier(ImageLayerTier::Base)
            .source(ImageLayerSource::Stdlib)
            .from("docker.io/library/ubuntu@sha256:00".into())
            .fragment("RUN true".into())
            .probe("true".into())
            .build();
        let request = ImageBuildRequest {
            composition: "sha256:feed".into(),
            architecture: Architecture::Amd64,
            layers: Frozen::new(vec![layer]).expect("freeze"),
            delivery: Delivery::Local,
            reservation: Reservation { cpu: 2, disk_bytes: 1 << 30 },
        };
        self.builds.request(name, request, now()).expect("build");
    }

    fn checkout(&self, name: &str, environment: &str) {
        let request = CheckoutRequest {
            repository: "repo".into(),
            environment: Ref::new(environment, HOME),
            branch: "work".into(),
            base: None,
            path: "/w".into(),
            source: Source::FreshClone { url: "https://example/repo".into() },
            clone: None,
            holder: Some(Holder::convoy("convoy")),
        };
        self.checkouts.request(name, request, now()).expect("checkout");
    }
}

fn write_observation(service: &Service, observation: HostObservation) {
    let store = service.store();
    let mut store = store.lock().expect("store");
    let mut write = store.write().expect("write");
    let change = write.open_change(&NewChange::new("probe", now())).expect("change");
    match write.resolve(<HostObservation as flotilla_schema::Kind>::CODE, HOME, HOME).expect("resolve") {
        Some(id) => {
            let current = write.load::<HostObservation>(id).expect("load").expect("held");
            write.update(&current, &observation, change).expect("update");
        }
        None => {
            write.insert(HOME, &observation, change).expect("insert");
        }
    }
    write.commit().expect("commit");
}

fn docker(image: &str, image_build_ref: Option<&str>) -> EnvironmentSpec {
    EnvironmentSpec {
        provider_address: Some(format!("host:{HOME}/provider:docker").parse().expect("address")),
        docker: Some(DockerEnvironmentSpec {
            host_ref: HOME.into(),
            image: image.into(),
            image_build_ref: image_build_ref.map(str::to_owned),
            declared_agent_adapters: Default::default(),
            required_agent_adapters: Default::default(),
            pull_policy: DockerImagePullPolicy::IfNotPresent,
            mounts: vec![],
            // A declared value is stored; a launch-derived one never is.
            env: BTreeMap::from([("DECLARED".to_owned(), "1".to_owned()), (CREDENTIAL_REFS_ENV.to_owned(), "[\"stale\"]".to_owned())]),
        }),
        ..EnvironmentSpec::default()
    }
}

fn provider_readiness(readiness: Readiness) -> HostObservation {
    let provider = Provider { class: None, readiness, workspace_root: "/".into(), material_modes: vec![], checkout_strategies: vec![] };
    HostObservation { providers: BTreeMap::from([("docker".to_owned(), provider)]), ..HostObservation::default() }
}

// Behaviour: a vessel's environment is launched with its declared env and the env derived from the
// vessel at launch, and is realised under the provider's container with the facts the provider
// reported. The derived env is never stored: the record shows only what was declared.
#[tokio::test]
async fn a_vessel_s_environment_launches_with_its_derived_env_and_its_facts() {
    let world = World::start(None);
    world.vessel("v1");
    assert!(world.environments.request("env-v1", &docker("crew@sha256:pinned", None), Some("v1")).expect("request"));
    world.until("env-v1 ready", |world| world.ready("env-v1")).await;

    let launched = world.provider.provisioned("env-v1");
    let env = &launched[0].docker.as_ref().expect("docker").env;
    assert_eq!(env.get("DECLARED").map(String::as_str), Some("1"));
    assert_eq!(env.get(CREDENTIAL_REFS_ENV).map(String::as_str), Some("[\"v1-token\"]"), "composed at launch from the vessel");

    let record = world.environments.get("env-v1", None).expect("read").expect("held");
    assert_eq!(record.identity(), Some("ctr-env-v1"));
    assert_eq!(record.vessel.as_deref(), Some("v1"));
    let stored = &record.spec.docker.as_ref().expect("docker").env;
    assert_eq!(stored.get("DECLARED").map(String::as_str), Some("1"));
    assert!(!stored.contains_key(CREDENTIAL_REFS_ENV), "a launch-derived entry is not stored");
    let facts = record.launch.expect("facts");
    assert_eq!(facts.image_ref.as_deref(), Some("crew@sha256:pinned"));
    assert_eq!(facts.local_image_id.as_deref(), Some("sha256:local"));
    assert_eq!(facts.configured_limits.and_then(|limits| limits.cpus), Some(4));
    assert_eq!(world.environments.held("v1").expect("held").len(), 1);
    // Requested again under its name, it is left as it is.
    assert!(!world.environments.request("env-v1", &docker("crew@sha256:pinned", None), Some("v1")).expect("request"));
}

// Behaviour: an environment whose image is built waits while its build runs, and is launched on
// the build's digest once the build is realised; a locally delivered image is never pulled.
#[tokio::test]
async fn a_built_environment_waits_for_its_build() {
    let world = World::start(None);
    world.vessel("v1");
    world.build("b1");
    world.environments.request("env-v1", &docker("", Some("b1")), Some("v1")).expect("request");
    // Another environment realised after it: the controller has passed it.
    world.vessel("v2");
    world.environments.request("env-v2", &docker("crew@sha256:pinned", None), Some("v2")).expect("request");
    world.until("env-v2 ready", |world| world.ready("env-v2")).await;
    assert!(world.provider.provisioned("env-v1").is_empty(), "nothing is launched while the build runs");
    assert_eq!(world.reading("env-v1"), Some(LifecycleReading::InProgress), "no attempt is spent waiting");

    world.builds.step("b1", &Step::Realised { identity: "sha256:built".into() }, now()).expect("built");
    world.until("env-v1 ready", |world| world.ready("env-v1")).await;
    let launched = world.provider.provisioned("env-v1");
    let docker = launched[0].docker.as_ref().expect("docker");
    assert_eq!(docker.image, "sha256:built");
    assert_eq!(docker.pull_policy, DockerImagePullPolicy::Never);
}

// Behaviour: while the host reports its provider unready nothing is launched there; the provider
// turning ready is the event that launches it.
#[tokio::test]
async fn an_unready_provider_holds_the_launch_until_it_is_ready() {
    let world = World::start(Some(provider_readiness(Readiness::MissingTool { reason: "docker missing".into() })));
    world.vessel("v1");
    world.environments.request("env-v1", &docker("crew@sha256:pinned", None), Some("v1")).expect("request");
    // A host-direct environment uses another provider, which the host does not report: it runs.
    let host = EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: HOME.into(), repo_default_dir: "/repos".into() }),
        ..EnvironmentSpec::default()
    };
    world.environments.request("host-direct-alpha", &host, None).expect("request");
    world.until("host-direct ready", |world| world.ready("host-direct-alpha")).await;
    assert!(world.provider.provisioned("env-v1").is_empty(), "nothing is launched on an unready provider");

    write_observation(&world.service, provider_readiness(Readiness::Ready));
    world.until("env-v1 ready", |world| world.ready("env-v1")).await;
}

// Behaviour: the host's own environment, held for no vessel, is realised under its root and never
// released.
#[tokio::test]
async fn a_host_direct_environment_is_realised_under_its_root() {
    let world = World::start(None);
    let host = EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: HOME.into(), repo_default_dir: "/repos".into() }),
        ..EnvironmentSpec::default()
    };
    world.environments.request("host-direct-alpha", &host, None).expect("request");
    world.until("ready", |world| world.ready("host-direct-alpha")).await;
    let record = world.environments.find("host-direct-alpha").expect("read").expect("held");
    assert_eq!(record.identity(), Some("/repos"));
    assert_eq!(record.spec.host_direct.as_ref().map(|host| host.repo_default_dir.as_str()), Some("/repos"));
    assert_eq!(world.environments.live().expect("live").len(), 1);
}

// Behaviour: a vessel's end releases its environment, but only once no live checkout is made in it:
// the checkout's teardown runs there. The container is destroyed under its identity.
#[tokio::test]
async fn a_vessel_s_end_releases_its_environment_after_its_checkouts() {
    let world = World::start(None);
    world.vessel("v1");
    world.environments.request("env-v1", &docker("crew@sha256:pinned", None), Some("v1")).expect("request");
    world.until("env-v1 ready", |world| world.ready("env-v1")).await;
    world.checkout("c1", "env-v1");
    world.vessels.step("v1", &Step::Released, now()).expect("vessel ended");
    // Another environment realised after the vessel ended: the controller has passed env-v1.
    world.vessel("v2");
    world.environments.request("env-v2", &docker("crew@sha256:pinned", None), Some("v2")).expect("request");
    world.until("env-v2 ready", |world| world.ready("env-v2")).await;
    assert!(world.ready("env-v1"), "held while a checkout is made in it");
    assert!(world.provider.asked().destroyed.is_empty());

    world.checkouts.step("c1", &Step::Released, now()).expect("checkout ended");
    world.until("env-v1 released", |world| world.released("env-v1")).await;
    assert_eq!(world.provider.asked().destroyed, [("env-v1".to_owned(), "ctr-env-v1".to_owned())]);
    assert!(world.environments.live().expect("live").iter().all(|record| record.name != "env-v1"));
}
