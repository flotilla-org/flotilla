//! The Environment controller (#3108) over a decided store, with a recording provider: requests go
//! through the environment port, and the outcomes are read back through it and from the provider.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_engine_store::{open_home, open_home_with, StoreLog};
use flotilla_environment::environment::EnvironmentProvisionError;
use flotilla_protocol::{ConfiguredResourceLimits, LifecycleReading};
use flotilla_provisioning::{
    engine::{
        controller::{Backing, BackingEvent, Controller},
        environment_actuator::{EnvironmentActuator, LaunchEnvironment},
        environments::EngineEnvironments,
        launch_log::{LaunchLog, StagedFacts},
        reads::StoreReads,
        LifecycleHome, Lifecycles, Step,
    },
    reconcilers::environment::{CloudProvisioning, EnvironmentProvisioning, EnvironmentRuntime, LocalProvisioning},
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
    environment_records::EnvironmentRecords, CloudSandboxEnvironmentSpec, CloudSandboxSize, DockerEnvironmentSpec, DockerImagePullPolicy,
    EnvironmentProviderDescriptor, EnvironmentSpec, HostDirectEnvironmentSpec, ImageLayerSource, ImageLayerSpec, ImageLayerTier,
    SystemClock, CREDENTIAL_REFS_ENV,
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

/// What the provider was asked, in order, and what it answers.
#[derive(Default)]
struct Asked {
    provisioned: Vec<(String, EnvironmentSpec)>,
    /// Each teardown asked, whether or not it succeeded.
    destroyed: Vec<(String, String)>,
    /// Backings torn down with no recorded identity.
    unrecorded: Vec<String>,
    cleaned: Vec<String>,
    /// The next provisions fail with these, in order.
    failures: VecDeque<EnvironmentProvisionError>,
    /// Every teardown fails with this.
    teardown_failure: Option<String>,
}

/// The provider process boundary: a container for a Docker spec, a sandbox for a cloud one, the
/// root for a host-direct one, and an opaque backing for a provider given neither.
#[derive(Default)]
struct RecordingProvider {
    asked: Mutex<Asked>,
    /// Told each time a teardown is asked: a failed one commits nothing to wait on.
    torn_down: Notify,
}

impl RecordingProvider {
    fn asked(&self) -> std::sync::MutexGuard<'_, Asked> {
        self.asked.lock().expect("asked")
    }

    fn failing(failure: EnvironmentProvisionError) -> Self {
        let provider = Self::default();
        provider.asked().failures.push_back(failure);
        provider
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
        let mut asked = self.asked();
        asked.provisioned.push((name.to_owned(), spec.clone()));
        if let Some(failure) = asked.failures.pop_front() {
            return Err(failure);
        }
        if let Some(cloud) = &spec.cloud_sandbox {
            return Ok(EnvironmentProvisioning::Cloud(CloudProvisioning {
                backing_id: format!("sandbox-{name}"),
                image_ref: cloud.image.clone(),
                provider_descriptor: Box::new(EnvironmentProviderDescriptor {
                    instance: "daytona".into(),
                    kind: flotilla_protocol::provider::ProviderKind::new(
                        flotilla_protocol::provider::EnvironmentClass::CloudSandbox,
                        "daytona",
                    ),
                    image_digest: cloud.image.clone(),
                    host_version: "1".into(),
                    module_version: "1".into(),
                    capabilities: Default::default(),
                    agent_connection: None,
                }),
            }));
        }
        Ok(EnvironmentProvisioning::Local(match (&spec.docker, &spec.host_direct) {
            (Some(docker), _) => LocalProvisioning {
                configured_limits: Some(ConfiguredResourceLimits { cpus: Some(4), ..ConfiguredResourceLimits::default() }),
                backing_id: Some(format!("ctr-{name}")),
                image_ref: Some(docker.image.clone()),
                local_image_id: Some("sha256:local".into()),
                registry_digest: None,
            },
            // A host-direct backing reports no identity: it is realised under its root.
            (None, Some(_)) => LocalProvisioning::default(),
            (None, None) => LocalProvisioning { backing_id: Some(format!("opaque-{name}")), ..LocalProvisioning::default() },
        }))
    }

    async fn destroy(&self, environment_ref: &str, backing_id: &str) -> Result<(), String> {
        let answer = {
            let mut asked = self.asked();
            asked.destroyed.push((environment_ref.to_owned(), backing_id.to_owned()));
            asked.teardown_failure.clone().map_or(Ok(()), Err)
        };
        self.torn_down.notify_one();
        answer
    }

    async fn destroy_unrecorded(&self, environment_ref: &str) -> Result<(), String> {
        self.asked().unrecorded.push(environment_ref.to_owned());
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
    backing: tokio::sync::mpsc::Sender<BackingEvent>,
}

fn now() -> Timestamp {
    Timestamp::from_datetime(&chrono::Utc::now())
}

impl World {
    /// A decided store, its Environment controller started, and `observation` as this host's facts.
    fn start(observation: Option<HostObservation>) -> Self {
        Self::with(RecordingProvider::default(), observation)
    }

    /// As [`Self::start`], over `provider`.
    fn with(provider: RecordingProvider, observation: Option<HostObservation>) -> Self {
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
            Arc::new(move |_: &flotilla_schema_sqlite::stream::Committed<'_>| commits.notify_waiters())
        };
        service.on_commit(Arc::downgrade(&listener));
        if let Some(observation) = observation {
            write_observation(&service, observation);
        }
        let provider = Arc::new(provider);
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
            backing,
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

    /// The cause `name` gave up with, once it has.
    fn gave_up(&self, name: &str) -> Option<String> {
        match self.reading(name) {
            Some(LifecycleReading::GaveUp { cause }) => Some(cause),
            _ => None,
        }
    }

    /// Requests an environment held for another vessel, and waits for it: the controller has
    /// passed every change made before.
    async fn passed(&self, marker: &str) {
        self.vessel(marker);
        let name = format!("env-{marker}");
        self.environments.request(&name, &docker("crew@sha256:pinned", None), Some(marker)).expect("request");
        self.until(&name, |world| world.ready(&name)).await;
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
        self.build_to(name, Delivery::Local);
    }

    fn build_to(&self, name: &str, delivery: Delivery) {
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
            delivery,
            reservation: Reservation { cpu: 2, disk_bytes: 1 << 30 },
        };
        self.builds.request(name, request, now()).expect("build");
    }

    fn checkout(&self, name: &str, environment: &str) {
        let request = CheckoutRequest {
            launch_of: name.into(),
            launch: 0,
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

fn cloud(image: &str, image_build_ref: Option<&str>) -> EnvironmentSpec {
    EnvironmentSpec {
        provider_address: Some("account:daytona/provider:daytona".parse().expect("address")),
        cloud_sandbox: Some(CloudSandboxEnvironmentSpec {
            probe_needs: Default::default(),
            image: image.into(),
            image_build_ref: image_build_ref.map(str::to_owned),
            size: CloudSandboxSize { cpus: 2, memory_bytes: 1024, disk_bytes: 1024 },
            lifetime_cap_seconds: None,
            env: Default::default(),
        }),
        ..EnvironmentSpec::default()
    }
}

fn host_direct(root: &str) -> EnvironmentSpec {
    EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: HOME.into(), repo_default_dir: root.into() }),
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
    // #2650: the limits the provider applied are recorded, not recomputed from host configuration.
    assert_eq!(facts.configured_limits, Some(ConfiguredResourceLimits { cpus: Some(4), ..ConfiguredResourceLimits::default() }));
    assert_eq!(world.environments.held("v1").expect("held").len(), 1);
    // Requested again under its name, it is left as it is.
    assert!(!world.environments.request("env-v1", &docker("crew@sha256:pinned", None), Some("v1")).expect("request"));
}

// Behaviour: an environment whose image is built waits while its build runs, and is launched on
// the build's digest once the build is realised, never on another image. A locally delivered image
// is never pulled; a registry digest is pinned, so it is pulled if absent whatever the policy says.
#[rstest::rstest]
#[case::local(Delivery::Local, DockerImagePullPolicy::Never)]
#[case::registry(Delivery::Registry { repository: "registry.test/crew".into() }, DockerImagePullPolicy::IfNotPresent)]
#[tokio::test]
async fn a_built_environment_waits_for_its_build(#[case] delivery: Delivery, #[case] pull: DockerImagePullPolicy) {
    let world = World::start(None);
    world.vessel("v1");
    world.build_to("b1", delivery);
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
    assert_eq!(docker.pull_policy, pull);
    assert_eq!(launched.len(), 1);
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

    let c1 = world.checkouts.newest_launch("c1").expect("a launch");
    world.checkouts.step(c1.name(), &Step::Released, now()).expect("checkout ended");
    world.until("env-v1 released", |world| world.released("env-v1")).await;
    assert_eq!(world.provider.asked().destroyed, [("env-v1".to_owned(), "ctr-env-v1".to_owned())]);
    assert!(world.environments.live().expect("live").iter().all(|record| record.name != "env-v1"));
}

// Behaviour: a provider's refusal gives the environment up with the provider's reason, for a
// container and for the host's own root alike; it is not tried again.
#[rstest::rstest]
#[case::container(docker("crew@sha256:pinned", None))]
#[case::host_direct(host_direct("/repos"))]
#[tokio::test]
async fn a_provider_refusal_gives_the_environment_up(#[case] spec: EnvironmentSpec) {
    let refusal = "central Codex credential is not provisioned on this host";
    let world = World::with(RecordingProvider::failing(EnvironmentProvisionError::Failed(refusal.into())), None);
    world.vessel("v1");
    world.environments.request("env-v1", &spec, Some("v1")).expect("request");
    world.until("env-v1 given up", |world| world.gave_up("env-v1").is_some()).await;
    assert!(world.gave_up("env-v1").expect("cause").contains(refusal));
    world.passed("v2").await;
    assert_eq!(world.provider.provisioned("env-v1").len(), 1, "a refusal is not tried again");
}

// Behaviour: a failed teardown leaves the environment realised under its backing, with the
// provider asked to destroy it under its identity.
#[tokio::test]
async fn a_failed_teardown_keeps_the_environment_realised() {
    let world = World::start(None);
    world.vessel("v1");
    world.environments.request("env-v1", &docker("crew@sha256:pinned", None), Some("v1")).expect("request");
    world.until("env-v1 ready", |world| world.ready("env-v1")).await;
    world.provider.asked().teardown_failure = Some("failed to parse provisioned mount metadata: corrupt label".into());
    world.vessels.step("v1", &Step::Released, now()).expect("vessel ended");
    tokio::time::timeout(Duration::from_secs(10), world.provider.torn_down.notified()).await.expect("teardown asked");
    world.passed("v2").await;
    // A later change may try the release again; each try destroys the same backing.
    let destroyed = world.provider.asked().destroyed.clone();
    assert!(
        !destroyed.is_empty() && destroyed.iter().all(|asked| *asked == ("env-v1".to_owned(), "ctr-env-v1".to_owned())),
        "{destroyed:?}"
    );
    let record = world.environments.get("env-v1", None).expect("read").expect("held");
    assert_eq!(record.identity(), Some("ctr-env-v1"), "still realised under its backing");
}

// Behaviour: an environment whose vessel ends before it is realised is released through the
// provider's teardown of a backing with no recorded identity, which a launch may have left.
#[tokio::test]
async fn an_unrealised_environment_s_end_tears_down_an_unrecorded_backing() {
    let world = World::start(None);
    world.vessel("v1");
    world.build("b1");
    world.environments.request("env-v1", &docker("", Some("b1")), Some("v1")).expect("request");
    world.vessels.step("v1", &Step::Released, now()).expect("vessel ended");
    world.until("env-v1 released", |world| world.released("env-v1")).await;
    assert_eq!(world.provider.asked().unrecorded, ["env-v1"]);
    assert!(world.provider.provisioned("env-v1").is_empty());
}

// Behaviour: an environment observed absent is lost, with the reason its probe gave, and is never
// launched again.
#[tokio::test]
async fn a_lost_environment_is_not_launched_again() {
    let world = World::start(None);
    world.vessel("v1");
    world.environments.request("env-v1", &docker("crew@sha256:pinned", None), Some("v1")).expect("request");
    world.until("env-v1 ready", |world| world.ready("env-v1")).await;
    let observed = Backing::Absent { reason: Some("cgroup_oom".into()) };
    world.backing.send(BackingEvent { name: "env-v1".into(), observed }).await.expect("observed");
    let lost = Some(LifecycleReading::Lost { reason: Some("cgroup_oom".into()) });
    world.until("env-v1 lost", |world| world.reading("env-v1") == lost).await;
    world.passed("v2").await;
    assert_eq!(world.provider.provisioned("env-v1").len(), 1);
    assert!(world.provider.asked().destroyed.is_empty());
}

// Behaviour: a build that ends without an image fails its environment with the build's ending; it
// never falls back to another image.
#[tokio::test]
async fn a_build_that_gave_up_gives_its_environment_up() {
    let world = World::start(None);
    world.vessel("v1");
    world.build("b1");
    world.environments.request("env-v1", &docker("", Some("b1")), Some("v1")).expect("request");
    world
        .builds
        .step(
            "b1",
            &Step::Attempted(flotilla_state_machines::Outcome::Failed {
                cause: flotilla_state_machines::Cause::new(flotilla_state_machines::CauseCategory::Refused, "probe `harness-codex` failed"),
                retryable: false,
            }),
            now(),
        )
        .expect("build gave up");
    world.until("env-v1 given up", |world| world.gave_up("env-v1").is_some()).await;
    assert!(world.gave_up("env-v1").expect("cause").contains("ImageBuild b1 ended without an image"));
    assert!(world.provider.provisioned("env-v1").is_empty());
}

// Behaviour (#3719 rulings 5-6): an environment's recipe is its provider's typed recipe, so a
// request naming a provider but no recipe is refused, and nothing is launched.
#[tokio::test]
async fn a_request_with_no_recipe_is_refused() {
    let world = World::start(None);
    let neutral =
        EnvironmentSpec { provider_address: Some("account:cloud/provider:sandbox".parse().expect("address")), ..Default::default() };
    let refusal = world.environments.request("neutral", &neutral, None).expect_err("refused");
    assert!(refusal.contains("exactly one of docker, host_direct and cloud_sandbox"), "{refusal}");
    assert!(world.environments.get("neutral", None).expect("read").is_none());
}

// Behaviour (#3620): a cloud sandbox is realised under its opaque backing, with the provider's
// descriptor as a fact of its launch, and its teardown destroys that backing.
#[tokio::test]
async fn a_cloud_sandbox_is_realised_and_destroyed_under_its_backing() {
    let world = World::start(None);
    world.vessel("v1");
    world.environments.request("cloud", &cloud("image@sha256:digest", None), Some("v1")).expect("request");
    world.until("cloud ready", |world| world.ready("cloud")).await;
    let record = world.environments.get("cloud", None).expect("read").expect("held");
    assert_eq!(record.identity(), Some("sandbox-cloud"));
    let facts = record.launch.expect("facts");
    assert_eq!(facts.image_ref.as_deref(), Some("image@sha256:digest"));
    // A vessel stamps its crews' image from these: the digest identifies the image.
    assert_eq!(facts.local_image_id.as_deref(), Some("sha256:digest"));
    assert_eq!(facts.registry_digest.as_deref(), Some("image@sha256:digest"));
    assert_eq!(facts.provider_descriptor.expect("descriptor").instance, "daytona");
    world.vessels.step("v1", &Step::Released, now()).expect("vessel ended");
    world.until("cloud released", |world| world.released("cloud")).await;
    assert_eq!(world.provider.asked().destroyed, [("cloud".to_owned(), "sandbox-cloud".to_owned())]);
}

// Behaviour: a built cloud sandbox waits for its build, then launches on the build's registry
// digest; a build delivered only to its builder's store gives it up, since a cloud provider reads
// images from the fleet image cache alone.
#[rstest::rstest]
#[case::registry(Delivery::Registry { repository: "registry.test/crew".into() })]
#[case::local(Delivery::Local)]
#[tokio::test]
async fn a_built_cloud_sandbox_launches_on_the_registry_digest(#[case] delivery: Delivery) {
    let world = World::start(None);
    world.vessel("v1");
    world.build_to("b1", delivery.clone());
    world.environments.request("cloud", &cloud("", Some("b1")), Some("v1")).expect("request");
    let digest = "registry.test/crew@sha256:built";
    world.builds.step("b1", &Step::Realised { identity: digest.into() }, now()).expect("built");
    match delivery {
        Delivery::Registry { .. } => {
            world.until("cloud ready", |world| world.ready("cloud")).await;
            assert_eq!(world.provider.provisioned("cloud")[0].cloud_sandbox.as_ref().expect("cloud").image, digest);
        }
        Delivery::Local => {
            world.until("cloud given up", |world| world.gave_up("cloud").is_some()).await;
            assert!(world.gave_up("cloud").expect("cause").contains("fleet image cache"));
            assert!(world.provider.provisioned("cloud").is_empty());
        }
    }
}

// Behaviour (#3765 B1): a launch that may have left a backing it could not adopt is retryable and
// keeps the environment requested; any other failure gives it up, whatever its wording.
#[hegel::test(test_cases = 20)]
fn an_unadopted_backing_is_retried_and_other_failures_are_not(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let length = tc.draw(gs::integers::<usize>().min_value(0).max_value(64));
    let mut diagnostic: String = (0..length).map(|_| char::from(tc.draw(gs::integers::<u8>().min_value(32).max_value(126)))).collect();
    if tc.draw(gs::booleans()) {
        diagnostic = format!("adoption setup failed; backing retained for retry: {diagnostic}");
    }
    let retry = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let failure = if retry {
            EnvironmentProvisionError::RetryAdoption(diagnostic.clone())
        } else {
            EnvironmentProvisionError::Failed(diagnostic.clone())
        };
        let world = World::with(RecordingProvider::failing(failure), None);
        world.vessel("v1");
        world.environments.request("cloud", &cloud("image@sha256:digest", None), Some("v1")).expect("request");
        world
            .until("a settled attempt", |world| {
                matches!(world.reading("cloud"), Some(LifecycleReading::Retrying { .. } | LifecycleReading::GaveUp { .. }))
            })
            .await;
        match world.reading("cloud").expect("held") {
            LifecycleReading::Retrying { failures: 1, cause, .. } if retry => {
                assert!(cause.contains(&format!("adoption failed; backing retained for retry: {diagnostic}")), "{cause}")
            }
            LifecycleReading::GaveUp { cause } if !retry => assert!(cause.contains(&diagnostic), "{cause}"),
            other => panic!("retry {retry}: {other:?}"),
        }
        assert!(world.provider.asked().destroyed.is_empty(), "a failed adoption keeps its backing");
    });
}

// Behaviour: a controller acts only on the launches homed at its own daemon. An environment homed
// at another daemon, replicated here, is never launched or torn down by this one, whether it
// arrives with its home or changes later: its home's controller owns it.
#[tokio::test]
async fn an_environment_homed_elsewhere_is_never_acted_on() {
    use flotilla_provisioning_testkit::{replicate_home, EnvironmentFixture, OtherHome};
    let world = World::start(None);
    let other = OtherHome::open("beta");
    let beta = EnvironmentFixture::over("beta", other.service().store());
    beta.request("env-beta", &docker("crew@sha256:pinned", None), None);
    replicate_home(other.service(), "beta", &world.service);
    let replicated = world.environments.find("env-beta").expect("read").expect("replicated here");
    assert_eq!(replicated.home, "beta");
    world.passed("v2").await;
    // Later writes at its home arrive as changes, not with a new home: another launch requested
    // there, and an edge of the first.
    beta.request("env-beta-2", &docker("crew@sha256:pinned", None), None);
    beta.retry("env-beta", "beta's own attempt");
    follow_home(other.service(), "beta", &world.service);
    world.passed("v3").await;
    for name in ["env-beta", "env-beta-2"] {
        assert!(world.provider.provisioned(name).is_empty(), "another daemon's launch {name} is not launched here");
    }
    assert!(world.provider.asked().destroyed.is_empty() && world.provider.asked().unrecorded.is_empty());
    assert!(
        matches!(world.environments.find("env-beta").expect("read").expect("held").reading, LifecycleReading::Retrying { failures: 1, .. }),
        "only its home's edges reach it"
    );
}

/// One round of `home`'s log after what `target` holds of it: its next entries, as a peer's
/// follower receives them.
fn follow_home(from: &Service, home: &str, target: &Service) {
    let after = target.last_sequence(home).expect("target's cursor");
    let mut frames = Vec::new();
    from.serve(Some(after), now(), |frame| {
        frames.push(frame.to_vec());
        Ok(())
    })
    .expect("serve home");
    target
        .with_store(|store| {
            flotilla_schema_sqlite::stream::Follower::new(store).receive(home, |sink| frames.iter().try_for_each(|frame| sink(frame)))
        })
        .expect("receive home");
}
