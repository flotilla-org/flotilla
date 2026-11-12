//! In-process stand-ins for ImageBuild execution (ADR 0062, #3108): the build-service port, the
//! host's materials, and a builder on a real decided store running the engine's image build
//! launcher and controller over them. Shared by provisioning's ImageBuild tests and core's
//! image-check tests.
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
use chrono::{DateTime, Utc};
use flotilla_engine_store::{open_home, StoreLog};
use flotilla_environment::environment::{
    image::{BuiltImage, FailedImageBuild, ImageBuildRequest, ImageDeliveryTarget, SecretFile},
    registry_auth::{RegistryAuthMaterial, RegistryConfig},
    ImageBuildService, RegistryAuth,
};
use flotilla_protocol::LifecycleReading;

use crate::Ending;
use flotilla_provisioning::{
    engine::{
        controller::Controller,
        image_build_actuator::{registry_build_cache, ImageBuildActuator, ImageBuildMaterials},
        image_builds::{demand_of, launch_of, EngineImageBuilds, ImageBuildLauncher},
        reads::{LifecycleReadPort, StoreReads},
        LifecycleHome, Lifecycles, Step,
    },
    stored::{
        decided_registry,
        host::{HostObservation, HostRecord},
        image_build::{ImageBuildDemand, ImageBuildKind},
    },
};
use flotilla_resources::{
    image_build_records::{ImageBuildRecord, ImageBuildRecords},
    image_identity,
    lifecycle_reads::LifecycleReads,
    Clock, HostImageAction, HostSpec, ImageBuildCapacity, ImageBuildFailure, ImageBuildFailureClass, ImageBuildReservation, ImageBuildSpec,
    ImageDelivery, ImageLayerProbe, ImageProbeOutcome,
};
use flotilla_schema::{Kind, NewChange, Ref, Timestamp};
use flotilla_schema_sqlite::{
    stream::{CommitListener, Committed, Service},
    Store,
};
use flotilla_state_machines::{
    machines::lifecycle::{LifecycleState, VanishCause},
    Cause, CauseCategory, Outcome,
};
use tokio::{sync::Notify, task::JoinHandle};

pub const NS: &str = "test";
/// The builder host.
pub const HOST: &str = "builder";
/// The builder's daemon: the home its launches are written at.
pub const BUILDER_HOME: &str = "builder-node";
pub const WRAPPER: &str = "flotilla_run_with_preludes exec \"$@\"";
pub const REPOSITORY: &str = "registry.test/flotilla/crew-images";

pub fn digest(fill: char) -> String {
    format!("sha256:{}", fill.to_string().repeat(64))
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
    async fn keep_log(&self, _build: &str, name: &str, log: &str) -> Result<String, String> {
        self.logs.lock().expect("logs").insert(name.to_owned(), log.to_owned());
        Ok(name.to_owned())
    }
}

/// A clock that elapses with tokio's, so a paused test runtime skips the build backoff.
struct TokioClock {
    base: DateTime<Utc>,
    start: tokio::time::Instant,
}

impl Clock for TokioClock {
    fn now(&self) -> DateTime<Utc> {
        self.base + chrono::Duration::from_std(self.start.elapsed()).unwrap_or_default()
    }
}

/// A builder on a real decided store: its Host record and observation, the image build port, the
/// launcher and the ImageBuild controller over [`BuildService`] and [`Materials`].
pub struct Builder {
    pub service: Arc<Service>,
    pub lifecycles: Arc<dyn Lifecycles<ImageBuildKind>>,
    pub builds: Arc<EngineImageBuilds>,
    /// The engine's readings, serving [`Self::builds`].
    pub reads: Arc<dyn LifecycleReads>,
    pub build_service: Arc<BuildService>,
    pub materials: Arc<Materials>,
    commits: Arc<Notify>,
    _listener: Arc<CommitListener>,
    tasks: Vec<JoinHandle<()>>,
    _directory: tempfile::TempDir,
}

impl Drop for Builder {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Builder {
    /// A builder of `architecture` offering `capacity`, building through `build_service` with
    /// `materials`. Its launcher and controller run until it drops.
    pub fn start(
        architecture: &str,
        capacity: Option<ImageBuildCapacity>,
        build_service: Arc<BuildService>,
        materials: Arc<Materials>,
    ) -> Self {
        let mut builder = Self::idle(architecture, capacity, build_service, materials);
        builder.run();
        builder
    }

    /// [`Self::start`] with neither its launcher nor its controller running until [`Self::run`]:
    /// what is demanded meanwhile waits.
    pub fn idle(
        architecture: &str,
        capacity: Option<ImageBuildCapacity>,
        build_service: Arc<BuildService>,
        materials: Arc<Materials>,
    ) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let service = Arc::new(
            Service::open(&directory.path().join("decided.sqlite"), BUILDER_HOME, decided_registry().expect("registry")).expect("store"),
        );
        let commits = Arc::new(Notify::new());
        let listener: Arc<CommitListener> = {
            let commits = Arc::clone(&commits);
            Arc::new(move |_: &Committed<'_>| commits.notify_waiters())
        };
        service.on_commit(Arc::downgrade(&listener));
        let lifecycles: Arc<dyn Lifecycles<ImageBuildKind>> =
            Arc::new(LifecycleHome::new(BUILDER_HOME, open_home(StoreLog::new(service.store())).expect("image builds")));
        let builds = Arc::new(EngineImageBuilds::over(Arc::clone(&lifecycles), Arc::clone(&service)));
        let reads: Arc<dyn LifecycleReads> = Arc::new(
            LifecycleReadPort::new(StoreReads::new(service.store()), BUILDER_HOME.to_owned())
                .with_image_builds(Arc::clone(&builds) as Arc<dyn ImageBuildRecords>),
        );
        let builder = Self {
            service,
            lifecycles,
            builds,
            reads,
            build_service,
            materials,
            commits,
            _listener: listener,
            tasks: Vec::new(),
            _directory: directory,
        };
        builder.host(architecture, capacity);
        builder
    }

    /// Starts this builder's launcher and controller. Must be called within a Tokio runtime.
    pub fn run(&mut self) {
        let launcher = ImageBuildLauncher::start(Arc::clone(&self.lifecycles), Arc::clone(&self.service), HOST).expect("launcher");
        let actuator = Arc::new(ImageBuildActuator::new(
            Arc::clone(&self.build_service) as Arc<dyn ImageBuildService>,
            Arc::clone(&self.materials) as Arc<dyn ImageBuildMaterials>,
            Arc::clone(&self.service),
            HOST,
            WRAPPER,
        ));
        let clock = Arc::new(TokioClock { base: Utc::now(), start: tokio::time::Instant::now() });
        let controller = Controller::start(Arc::clone(&self.lifecycles), actuator, clock, Arc::clone(&self.service)).expect("controller");
        let (backing, backings) = tokio::sync::mpsc::channel(1);
        self.tasks.push(tokio::spawn(launcher.run()));
        // The sender is kept by the controller's task: a build's backing is never pushed.
        self.tasks.push(tokio::spawn(async move {
            let _backing = backing;
            controller.run(backings).await;
        }));
    }

    /// Writes this builder's Host record, offering `capacity`, and its observed `architecture`.
    pub fn host(&self, architecture: &str, capacity: Option<ImageBuildCapacity>) {
        write_host(&self.service.store(), BUILDER_HOME, HOST, architecture, capacity);
    }

    /// Demands `spec` here, as admission does, returning the build's name.
    pub fn demand(&self, spec: &ImageBuildSpec) -> String {
        self.build_service.expect(spec);
        let name = spec.resource_name();
        self.builds.demand(&name, spec).expect("demand");
        name
    }

    /// The builds this home demands, by name.
    pub fn demands(&self) -> Vec<String> {
        demands_at(self.service.store(), BUILDER_HOME)
    }

    /// Build `name`'s newest launch as its readers see it.
    pub fn record(&self, name: &str) -> Option<ImageBuildRecord> {
        self.builds.get(name).expect("read")
    }

    /// Waits for `holds`, rechecked at each commit and every 100 ms.
    pub async fn until(&self, what: &str, holds: impl Fn(&Self) -> bool) {
        let wait = async {
            loop {
                let committed = self.commits.notified();
                tokio::pin!(committed);
                committed.as_mut().enable();
                if holds(self) {
                    return;
                }
                tokio::select! {
                    () = committed => {}
                    () = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
        };
        // Long enough for every try's backoff, which a paused runtime skips.
        tokio::time::timeout(Duration::from_secs(24 * 3600), wait).await.unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    /// Waits for build `name`'s newest launch to settle: built, or given up.
    pub async fn settled(&self, name: &str) -> ImageBuildRecord {
        self.until(&format!("{name} to settle"), |builder| {
            builder
                .record(name)
                .is_some_and(|record| matches!(record.reading, LifecycleReading::Ready { .. } | LifecycleReading::GaveUp { .. }))
        })
        .await;
        self.record(name).expect("settled")
    }
}

/// The builds `home` demands, by name.
pub fn demands_at(store: Arc<Mutex<Store>>, home: &str) -> Vec<String> {
    let mut store = store.lock().expect("store");
    let mut read = store.read().expect("read");
    let mut names = Vec::new();
    let mut after = None;
    loop {
        let page = read.scan::<ImageBuildDemand>(home, after, 64).expect("demands");
        for id in page.ids {
            names.extend(read.load::<ImageBuildDemand>(id).expect("demand").map(|loaded| loaded.name));
        }
        match page.next {
            Some(next) => after = Some(next),
            None => return names,
        }
    }
}

/// Writes host `host`'s Host record at `home`, offering `capacity`, and its observed `architecture`.
pub fn write_host(store: &Mutex<Store>, home: &str, host: &str, architecture: &str, capacity: Option<ImageBuildCapacity>) {
    let record = HostRecord::new(&HostSpec { image_build_capacity: capacity, ..Default::default() });
    let observation = HostObservation { architecture: Some(architecture.to_owned()), ..Default::default() };
    let mut store = store.lock().expect("store");
    let mut write = store.write().expect("write");
    let change = write.open_change(&NewChange::new("probe", Timestamp::from_datetime(&Utc::now()))).expect("change");
    match write.resolve(<HostRecord as Kind>::CODE, host, home).expect("resolve") {
        Some(id) => {
            let current = write.load::<HostRecord>(id).expect("load").expect("held");
            write.update(&current, &record, change).expect("update host");
        }
        None => {
            write.insert(host, &record, change).expect("insert host");
        }
    }
    match write.resolve(<HostObservation as Kind>::CODE, host, home).expect("resolve") {
        Some(id) => {
            let current = write.load::<HostObservation>(id).expect("load").expect("held");
            write.update(&current, &observation, change).expect("update observation");
        }
        None => {
            write.insert(host, &observation, change).expect("insert observation");
        }
    }
    write.commit().expect("commit");
}

/// One home's image builds on a decided store, for the tests of their readers: each is demanded
/// and launched at this home, as its own builder, and stepped as the ImageBuild controller would
/// step it.
pub struct ImageBuildFixture {
    pub home: String,
    pub store: Arc<Mutex<Store>>,
    pub lifecycles: Arc<dyn Lifecycles<ImageBuildKind>>,
    pub builds: Arc<EngineImageBuilds>,
    /// The engine's readings, serving [`Self::builds`].
    pub reads: Arc<dyn LifecycleReads>,
}

impl ImageBuildFixture {
    /// A fixture whose builds are demanded and built at `home`.
    pub fn at(home: &str) -> Self {
        Self::over(home, Arc::new(Mutex::new(crate::memory_store(home))))
    }

    /// A fixture over `store`, whose home is `home`.
    pub fn over(home: &str, store: Arc<Mutex<Store>>) -> Self {
        let only = home.to_owned();
        Self::over_homes(home, store, move || vec![only.clone()])
    }

    /// A fixture over a daemon's decided store `service`, whose builder hosts are found at every
    /// home it holds, as the daemon's runtime finds them: another home's Host arrives as its log
    /// is replicated in.
    pub fn over_service(service: &Arc<Service>) -> Self {
        let homes = Arc::clone(service);
        Self::over_homes(service.home(), service.store(), move || homes.homes())
    }

    fn over_homes(home: &str, store: Arc<Mutex<Store>>, homes: impl Fn() -> Vec<String> + Send + Sync + 'static) -> Self {
        let lifecycles: Arc<dyn Lifecycles<ImageBuildKind>> =
            Arc::new(LifecycleHome::new(home, open_home(StoreLog::new(Arc::clone(&store))).expect("image builds")));
        let builds = Arc::new(EngineImageBuilds::new(Arc::clone(&lifecycles), Arc::clone(&store), homes));
        let reads: Arc<dyn LifecycleReads> = Arc::new(
            LifecycleReadPort::new(StoreReads::new(Arc::clone(&store)), home.to_owned())
                .with_image_builds(Arc::clone(&builds) as Arc<dyn ImageBuildRecords>),
        );
        Self { home: home.to_owned(), store, lifecycles, builds, reads }
    }

    /// Writes host `host`'s Host record here, so a build can be demanded of it.
    pub fn host(&self, host: &str, architecture: &str) {
        write_host(&self.store, &self.home, host, architecture, None);
    }

    /// Demands `spec` here, as admission does, returning the build.
    pub fn demand(&self, spec: &ImageBuildSpec) -> String {
        let name = spec.resource_name();
        self.builds.demand(&name, spec).expect("demand");
        name
    }

    /// The builds this home demands, by name.
    pub fn demands(&self) -> Vec<String> {
        demands_at(Arc::clone(&self.store), &self.home)
    }

    /// Launches demanded build `name` here, as its builder's launcher would.
    pub fn launch(&self, name: &str) {
        let spec = self.builds.demanded(name).expect("read").expect("demanded");
        let demand = demand_of(&spec, Ref::new(spec.host_ref.clone(), self.home.clone())).expect("demand");
        self.lifecycles.request(name, launch_of(name, &demand), Timestamp::from_datetime(&Utc::now())).expect("launch");
    }

    /// Demands `spec` and launches it here, returning the build.
    pub fn put(&self, spec: &ImageBuildSpec) -> String {
        let name = self.demand(spec);
        self.launch(&name);
        name
    }

    /// Ends build `name`'s newest launch as `ending` says, building it first where the ending
    /// needs a built image.
    pub fn finish(&self, name: &str, ending: Ending<'_>) {
        let launch = self.lifecycles.newest_launch(name).expect("launched");
        let realised = matches!(launch.state(), LifecycleState::Realised { .. });
        let launch = launch.name().to_owned();
        let realise = |identity: &str| {
            if !realised {
                self.step(&launch, Step::Realised { identity: identity.to_owned() });
            }
        };
        match ending {
            Ending::Ready(identity) => self.step(&launch, Step::Realised { identity: identity.to_owned() }),
            Ending::Lost(identity) => {
                realise(identity);
                self.step(&launch, Step::Vanished { cause: VanishCause::lost() });
            }
            Ending::Failed(reason) => {
                realise("built");
                self.step(&launch, Step::Vanished { cause: VanishCause::Failed { reason: reason.into() } });
            }
            Ending::Released => self.step(&launch, Step::Released),
            Ending::GaveUp(cause) => {
                self.step(&launch, Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Refused, cause), retryable: false }))
            }
        }
    }

    /// Poisons the store's lock, so every read through it fails.
    pub fn poison(&self) {
        flotilla_store_testkit::poison_mutex(Arc::clone(&self.store));
    }

    fn step(&self, launch: &str, step: Step) {
        self.lifecycles.step(launch, &step, Timestamp::from_datetime(&Utc::now())).expect("image build step");
    }
}
