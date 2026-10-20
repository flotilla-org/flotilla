//! Load harness for the dummy fleet (#3206): today's control plane as a
//! `FleetTarget`. Each home is a full in-process daemon composed as `flotillad`
//! composes one (SQLite store, `DaemonRuntime` at production cadences, the
//! dummy fleet installed through the composition-root switch); several homes
//! replicate over the production peer runtime on in-memory transports.
//!
//! `load_driver_smoke` runs a few convoys in CI. `load_baseline` is ignored by
//! default and measures a configured run:
//!
//! ```sh
//! FLOTILLA_LOAD_RUNNING=1000 FLOTILLA_LOAD_DAEMONS=1 FLOTILLA_LOAD_WINDOW_SECS=1800 \
//! FLOTILLA_LOAD_REPORT=/tmp/load-1000 \
//!   cargo test --release -p flotilla-daemon --locked --test dummy_fleet_load load_baseline -- --ignored --nocapture
//! ```
//!
//! `FLOTILLA_LOAD_WARMUP_ENDED` defaults to the running count; `MAX_RAMP_SECS`
//! bounds warm-up too. `FLOTILLA_LOAD_DRAIN_SECS` (900) stops admissions then
//! waits for outcomes and cleanup. `FLOTILLA_LOAD_UNCHANGED_SECS` (600) bounds
//! progress independently of production stall timers. `FLOTILLA_LOAD_TIMELINE=true`
//! logs semantic changes and retains them in JSON; `FLOTILLA_LOAD_DUMP_UNFINISHED`
//! (true) prints the post-drain tail. Both Markdown and JSON retain tail ages.
//!
//! `FLOTILLA_LOAD_TRACE=1` records controller errors/restarts and slow Message
//! receipt timestamps, including the durable submission instant, to stderr.
//!
//! `FLOTILLA_LOAD_PRIMITIVES=true` adds `[primitives]` to each home's
//! `daemon.toml`, so Message runs on the ADR 0064 engine (#3317), and seeds the
//! designated fleet project its roles live under. Turn delivery is then timed
//! from each engine Message's creation entry to the AgentSession receipt edge
//! about it, followed on the decided store's log.
//!
//! `FLOTILLA_LOAD_MAX_RESIDENT_GIB` stops admitting convoys once the process
//! holds that much memory, so a plan the host cannot hold reports how far it
//! got. Progress lines every 30 s survive a run the host kills.
//!
//! Dirty-run JSON includes the full tracked `git diff HEAD`; inspect it before
//! sharing a report, or run from a clean commit to omit the diff.
//!
//! This is its own test target: it reads the process's resident memory, which
//! other tests in a shared binary would distort.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_config::{ConfigStore, DaemonConfig};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon::{
    dummy_fleet::{attach_dummy_fleet, install_dummy_fleet},
    runtime::{DaemonRuntime, RuntimeOptions},
};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_daemon_testkit::server::{spawn_in_memory_request_mesh_with_replication_metrics, InMemoryRequestMesh};
use flotilla_discovery_testkit::fake_discovery;
use flotilla_dummy_fleet::{
    catalog::{self, CrewMix, DUMMY_PLACEMENT_POLICY},
    CrewCommandPort, DaemonCommandPort, DummyFleet,
};
use flotilla_load_driver::{ConvoyObservation, ConvoyState, FleetTarget, HomeSample, LoadPlan, TargetSample};
use flotilla_observation::forge_observation::NO_READY_FORGE_OBSERVER;
use flotilla_protocol::{commands::CommandValue, CommandAction, HostName};
use flotilla_resources::{
    Checkout, Convoy, ConvoyPhase, FleetDesignation, FleetDesignationSpec, Forge, Host, InputMeta, Message, PlacementPolicy, Project,
    ProjectSpec, ReadWatchEvent, Repository, ResourceProvenance, TerminalSession, Vessel, WatchEvent, WatchStart, WorkflowTemplate,
    FLEET_DESIGNATION_NAME,
};
use flotilla_schema::{Cause, CodeEnum, KindCode, KindSpec};
use flotilla_schema_sqlite::stream::{Frame, Service};
use flotilla_state_machines::machines::{agent_session::AgentSessionEdgeKind, message::MessageSpec};
use flotilla_store::{
    load_metrics::{LatencyHistogram, ReplicationLoadMetrics},
    ResourceBackend,
};
use flotilla_store_sqlite::SqliteBackend;
use futures::StreamExt;

const NAMESPACE: &str = "flotilla";

/// What each home's watches maintain: the driver's convoys by name, and the
/// latencies of turn delivery and of convoys replicated here.
#[derive(Default)]
struct HomeView {
    convoys: Mutex<HashMap<String, (String, ConvoyObservation)>>,
    /// Where each open convoy is: its phase and its crews' work phases.
    places: Mutex<HashMap<String, String>>,
    changes: Mutex<Vec<ConvoyObservation>>,
    observer_failure_sightings: AtomicU64,
    turn_delivery: LatencyHistogram,
    replication_lag: LatencyHistogram,
}

/// Admission instants of every home's convoys, by resource name; a replica's
/// arrival elsewhere is measured against them.
type Admissions = Arc<Mutex<HashMap<String, tokio::time::Instant>>>;

struct Home {
    daemon: Arc<InProcessDaemon>,
    fleet: Arc<DummyFleet>,
    runtime: DaemonRuntime,
    store: SqliteBackend,
    database: PathBuf,
    port: Arc<DaemonCommandPort>,
    replication: Arc<ReplicationLoadMetrics>,
    view: Arc<HomeView>,
}

struct InProcessFleet {
    homes: Vec<Home>,
    /// Refuse dispatches once the process holds this much resident memory.
    memory_limit: Option<u64>,
    _mesh: Option<InMemoryRequestMesh>,
    _dir: tempfile::TempDir,
}

fn state_of(phase: ConvoyPhase) -> ConvoyState {
    match phase {
        ConvoyPhase::Landed => ConvoyState::Delivered,
        ConvoyPhase::Abandoned => ConvoyState::Abandoned,
        ConvoyPhase::Failed => ConvoyState::Failed,
        _ => ConvoyState::Open,
    }
}

async fn start_home(root: &std::path::Path, index: usize, dummy_fleet: &str, admissions: Admissions) -> Home {
    let config_dir = root.join(format!("home-{index}"));
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let switch = if load_primitives() { "[primitives]\n" } else { "" };
    std::fs::write(config_dir.join("daemon.toml"), format!("machine_id = \"load-{index}\"\n\n{switch}[dummy_fleet]\n{dummy_fleet}"))
        .expect("daemon.toml");
    let config = Arc::new(ConfigStore::with_base(config_dir));
    let daemon_config: DaemonConfig = config.load_daemon_config().expect("daemon config");
    let mut discovery = fake_discovery(false);
    let fleet = install_dummy_fleet(&daemon_config, &mut discovery).expect("dummy fleet configured");
    // The daemon's own store, opened where flotillad opens it.
    let database = config.state_dir().as_path().join("resources.sqlite");
    std::fs::create_dir_all(database.parent().expect("state dir")).expect("state dir");
    let store = SqliteBackend::open_async(&database).await.expect("sqlite store");
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        discovery,
        HostName::new(format!("load-{index}")),
        ResourceBackend::new(store.clone()),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    attach_dummy_fleet(&fleet, &daemon).expect("attach fleet");
    // Production heartbeat and controller resync cadences.
    let options = RuntimeOptions { namespace: NAMESPACE.into(), ..RuntimeOptions::default() };
    let runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), config, None, options).await.expect("runtime");
    let backend = daemon.resource_backend();
    seed(&backend, &fleet.config().forge_service_url, daemon.message_primitives().is_some()).await;
    let view = Arc::new(HomeView::default());
    watch_convoys(&backend, Arc::clone(&view), admissions).await;
    watch_messages(&backend, Arc::clone(&view)).await;
    if let Some(primitives) = daemon.message_primitives() {
        watch_engine_messages(Arc::clone(primitives.service()), Arc::clone(&view));
    }
    let port = DaemonCommandPort::new(&(Arc::clone(&daemon) as Arc<dyn DaemonHandle>));
    Home { daemon, fleet, runtime, store, database, port, replication: Arc::default(), view }
}

async fn seed(backend: &ResourceBackend, forge_service_url: &str, primitives: bool) {
    if primitives {
        // The engine path's fleet roles live under the designated project.
        backend
            .clone()
            .using::<Project>(NAMESPACE)
            .create(
                &InputMeta::builder().name("fleet".to_string()).build(),
                &ProjectSpec::builder()
                    .display_name("Fleet".to_string())
                    .default_workflow_ref(CrewMix::Coder.workflow_name().to_string())
                    .repositories(Vec::new())
                    .build(),
            )
            .await
            .expect("fleet project");
        backend
            .clone()
            .definitions::<FleetDesignation>(NAMESPACE)
            .create(
                &InputMeta::builder().name(FLEET_DESIGNATION_NAME.to_string()).build(),
                &FleetDesignationSpec { project: "fleet".into(), image_cache: None },
            )
            .await
            .expect("fleet designation");
    }
    let host = backend.clone().using::<Host>(NAMESPACE).list().await.expect("hosts").items.into_iter().next().expect("local host");
    let forge = catalog::forge(forge_service_url);
    backend
        .clone()
        .using::<Forge>(NAMESPACE)
        .create(&InputMeta::builder().name(forge.forge_id.clone()).build(), &forge)
        .await
        .expect("dummy forge");
    let repository = catalog::repository(&forge).expect("dummy repository");
    let repositories = backend.clone().using::<Repository>(NAMESPACE);
    repositories.create(&InputMeta::builder().name(repository.key().to_string()).build(), &repository).await.expect("repository");
    backend
        .clone()
        .using::<PlacementPolicy>(NAMESPACE)
        .create(
            &InputMeta::builder().name(DUMMY_PLACEMENT_POLICY.to_string()).labels(catalog::placement_policy_labels()).build(),
            &catalog::placement_policy(&host.metadata.name),
        )
        .await
        .expect("dummy placement policy");
    for mix in CrewMix::ALL {
        backend
            .clone()
            .using::<WorkflowTemplate>(NAMESPACE)
            .create(&InputMeta::builder().name(mix.workflow_name().to_string()).build(), &mix.workflow(load_long_runner_turns()))
            .await
            .expect("dummy workflow");
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        while repositories.get(&repository.key().to_string()).await.ok().and_then(|repository| repository.status?.default_branch).is_none()
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the dummy forge resolves the default branch");
}

/// Track the driver's local convoys, and time replicas arriving from other homes.
async fn watch_convoys(backend: &ResourceBackend, view: Arc<HomeView>, admissions: Admissions) {
    let mut events = backend.including_replicas::<Convoy>(NAMESPACE).watch().await.expect("convoy watch");
    tokio::spawn(async move {
        let mut replicated = HashSet::new();
        while let Some(event) = events.next().await {
            let Ok(ReadWatchEvent::Added(source) | ReadWatchEvent::Modified(source)) = event else { continue };
            let convoy = source.object;
            match source.provenance {
                ResourceProvenance::Local => {
                    if let Some(stalled) = convoy.status.as_ref().and_then(|status| status.stalled.as_ref()) {
                        if stalled.evidence.contains(NO_READY_FORGE_OBSERVER) {
                            view.observer_failure_sightings.fetch_add(1, Ordering::Relaxed);
                            println!("observer_failure convoy={} evidence={}", convoy.metadata.name, stalled.evidence);
                        }
                    }
                    admissions.lock().expect("admissions").entry(convoy.metadata.name.clone()).or_insert_with(tokio::time::Instant::now);
                    let state = convoy.status.as_ref().map_or(ConvoyState::Open, |status| state_of(status.phase));
                    let observation = ConvoyObservation {
                        name: convoy.spec.role.clone(),
                        state,
                        progress: format!("resource={} {}", convoy.metadata.name, place_of(&convoy)),
                        stall: convoy.status.as_ref().and_then(|status| status.stalled.as_ref()).map(|stall| stall.evidence.clone()),
                        expects_stall: false,
                        expected_end: ConvoyState::Delivered,
                    };
                    {
                        let mut convoys = view.convoys.lock().expect("convoys");
                        if convoys.get(&convoy.spec.role).is_none_or(|(_, previous)| *previous != observation) {
                            view.changes.lock().expect("changes").push(observation.clone());
                        }
                        convoys.insert(convoy.spec.role.clone(), (convoy.metadata.name.clone(), observation));
                    }
                    let mut places = view.places.lock().expect("places");
                    if state.ended() {
                        places.remove(&convoy.spec.role);
                    } else {
                        places.insert(convoy.spec.role.clone(), place_of(&convoy));
                    }
                }
                ResourceProvenance::Replica { .. } => {
                    if replicated.insert(convoy.metadata.name.clone()) {
                        if let Some(admitted) = admissions.lock().expect("admissions").get(&convoy.metadata.name) {
                            view.replication_lag.record(admitted.elapsed());
                        }
                    }
                }
            }
        }
    });
}

/// Time each Message from creation until its receipt is recorded.
async fn watch_messages(backend: &ResourceBackend, view: Arc<HomeView>) {
    let mut events = backend.clone().using::<Message>(NAMESPACE).watch(WatchStart::Now).await.expect("message watch");
    tokio::spawn(async move {
        let mut delivered = HashSet::new();
        while let Some(event) = events.next().await {
            let Ok(WatchEvent::Added(message) | WatchEvent::Modified(message)) = event else { continue };
            let Some(receipt) = message.status.as_ref().and_then(|status| status.resolved_receiver.as_ref()) else { continue };
            if delivered.insert(message.metadata.name.clone()) {
                let latency = receipt.delivered_at - message.metadata.creation_timestamp;
                view.turn_delivery.record(latency.to_std().unwrap_or_default());
                if latency >= chrono::Duration::seconds(5) {
                    tracing::info!(name = %message.metadata.name, receiver = %message.spec.receiver,
                        created_at = %message.metadata.creation_timestamp, submitted_at = ?message.status.as_ref().and_then(|s| s.submission.as_ref()).map(|s| s.started_at),
                        delivered_at = %receipt.delivered_at, latency_ms = latency.num_milliseconds(), "load slow message receipt");
                }
            }
        }
    });
}

/// Time each engine Message (`[primitives]`) from its creation entry to the
/// AgentSession receipt edge about it, following the decided store's log as a
/// peer does, so the measurement never lists the replica.
fn watch_engine_messages(service: Arc<Service>, view: Arc<HomeView>) {
    const MESSAGE: KindCode = <MessageSpec as KindSpec>::CODE;
    let receive = AgentSessionEdgeKind::Receive.code();
    tokio::spawn(async move {
        let mut cursor = None;
        // A message never received (an abandoned convoy's) stays here for the
        // rest of the run: bounded by the messages one run creates.
        let mut created = HashMap::new();
        loop {
            let round = tokio::task::spawn_blocking({
                let service = Arc::clone(&service);
                move || {
                    let mut frames = Vec::new();
                    let now = flotilla_schema::Timestamp::from_datetime(&Utc::now());
                    let next = service.serve(cursor, now, |bytes| {
                        frames.push(Frame::decode(bytes)?);
                        Ok(())
                    })?;
                    Ok::<_, flotilla_schema_sqlite::StoreError>((next, frames))
                }
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|round| round.map_err(|error| error.to_string()));
            // A failed round is retried from the same cursor; a run whose log
            // never reads records no samples, which `load_baseline` refuses.
            let (next, frames) = match round {
                Ok(round) => round,
                Err(error) => {
                    tracing::warn!(%error, "load engine log round failed");
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
            };
            cursor = Some(next);
            for frame in frames {
                let Frame::Change(event) = frame else { continue };
                match (&event.cause, &event.about) {
                    (Cause::Create, _) if event.kind == MESSAGE => {
                        created.insert(event.identity(), event.at);
                    }
                    (Cause::Edge { edge, .. }, Some(message)) if *edge == receive => {
                        if let Some(at) = created.remove(message) {
                            let micros = event.at.since(at).max(0);
                            view.turn_delivery.record(Duration::from_micros(micros as u64));
                            if micros >= 5_000_000 {
                                tracing::info!(name = %message.name, latency_ms = micros / 1000, "load slow engine message receipt");
                            }
                        }
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
}

fn place_of(convoy: &flotilla_resources::ResourceObject<Convoy>) -> String {
    let Some(status) = convoy.status.as_ref() else { return "admitted".into() };
    let mut place = format!("{:?}", status.phase);
    if status.stalled.is_some() {
        place.push_str(" · stalled");
    }
    let crews =
        status.crew_work.values().flat_map(|roles| roles.iter().map(|(role, work)| format!("{role} {:?}", work.phase))).collect::<Vec<_>>();
    if !crews.is_empty() {
        place.push_str(" · ");
        place.push_str(&crews.join(", "));
    }
    place
}

fn resident_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse::<u64>().ok().map(|kib| kib * 1024)
}

fn database_bytes(database: &std::path::Path) -> Option<u64> {
    let size = |path: PathBuf| std::fs::metadata(path).map(|metadata| metadata.len()).unwrap_or(0);
    let wal = PathBuf::from(format!("{}-wal", database.display()));
    database.exists().then(|| size(database.to_path_buf()) + size(wal))
}

impl InProcessFleet {
    async fn start(homes: usize, dummy_fleet: &str) -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let admissions = Admissions::default();
        let mut started = Vec::with_capacity(homes);
        for index in 0..homes {
            started.push(start_home(dir.path(), index, dummy_fleet, Arc::clone(&admissions)).await);
        }
        let mesh = if homes > 1 {
            let daemons = started.iter().map(|home| Arc::clone(&home.daemon)).collect();
            let metrics = started.iter().map(|home| Arc::clone(&home.replication)).collect();
            Some(spawn_in_memory_request_mesh_with_replication_metrics(daemons, metrics).await.expect("peer mesh"))
        } else {
            None
        };
        let memory_limit = std::env::var("FLOTILLA_LOAD_MAX_RESIDENT_GIB")
            .ok()
            .and_then(|gib| gib.parse::<u64>().ok())
            .map(|gib| gib * 1024 * 1024 * 1024);
        Self { homes: started, memory_limit, _mesh: mesh, _dir: dir }
    }
}

#[async_trait]
impl FleetTarget for InProcessFleet {
    fn homes(&self) -> usize {
        self.homes.len()
    }

    async fn dispatch(&self, home: usize, name: &str, mix: CrewMix) -> Result<(), String> {
        // Stop admitting before the process outgrows the host, so a run that
        // cannot reach its plan reports how far it got.
        if let (Some(limit), Some(resident)) = (self.memory_limit, resident_bytes()) {
            if resident >= limit {
                return Err(format!("resident memory limit reached ({} MiB)", limit / (1024 * 1024)));
            }
        }
        let home = &self.homes[home];
        let forge = catalog::forge(&catalog_forge_url());
        let action = CommandAction::ConvoyCreate {
            name: name.into(),
            workflow_ref: mix.workflow_name().into(),
            inputs: Vec::new(),
            repository_url: Some(catalog::repository_url(&forge)),
            r#ref: Some("main".into()),
            project_ref: None,
            placement_policy: Some(DUMMY_PLACEMENT_POLICY.into()),
            adopted_checkout: None,
        };
        match home.port.execute(action).await? {
            CommandValue::ConvoyCreated { .. } => Ok(()),
            CommandValue::Error { message } => Err(message),
            other => Err(format!("unexpected admission result {other:?}")),
        }
    }

    async fn convoys(&self, home: usize) -> Result<Vec<ConvoyObservation>, String> {
        let convoys = self.homes[home].view.convoys.lock().expect("convoys");
        let mut observations = std::mem::take(&mut *self.homes[home].view.changes.lock().expect("changes"));
        observations.extend(convoys.values().map(|(_, observation)| observation.clone()));
        Ok(observations)
    }

    async fn abandon(&self, home: usize, name: &str, reason: &str) -> Result<(), String> {
        let home = &self.homes[home];
        let resource = home.view.convoys.lock().expect("convoys").get(name).map(|(resource, _)| resource.clone());
        let resource = resource.ok_or_else(|| format!("convoy {name} is not known"))?;
        let action = CommandAction::ConvoyAbandon { namespace: Some(NAMESPACE.into()), name: resource, reason: reason.into() };
        match home.port.execute(action).await? {
            CommandValue::Error { message } => Err(message),
            _ => Ok(()),
        }
    }

    async fn open_breakdown(&self) -> Result<std::collections::BTreeMap<String, u64>, String> {
        let mut breakdown = std::collections::BTreeMap::new();
        for home in &self.homes {
            for place in home.view.places.lock().expect("places").values() {
                *breakdown.entry(place.clone()).or_default() += 1;
            }
        }
        Ok(breakdown)
    }

    async fn audit(&self) -> Result<Vec<String>, String> {
        let mut errors = Vec::new();
        for home in &self.homes {
            let snapshot = home.fleet.snapshot();
            if snapshot.command_errors != 0 {
                errors.push(format!("crew command errors: {} {:?}", snapshot.command_errors, home.fleet.command_error_samples()));
            }
            let ended = home
                .view
                .convoys
                .lock()
                .expect("convoys")
                .values()
                .filter(|(_, o)| o.state.ended())
                .map(|(resource, observation)| {
                    // Progress evidence is retired at the terminal observation;
                    // the final audit still checks for a later terminal stall.
                    if let Some(stall) = &observation.stall {
                        errors.push(format!("{resource}: terminal convoy retains a stall: {stall}"));
                    }
                    resource.clone()
                })
                .collect::<HashSet<_>>();
            for convoy in &ended {
                errors.extend(home.fleet.remaining_for(convoy).into_iter().map(|artifact| format!("{convoy}: {artifact}")));
            }
            let backend = home.daemon.resource_backend();
            for vessel in backend.clone().using::<Vessel>(NAMESPACE).list().await.map_err(|e| e.to_string())?.items {
                if ended.contains(&vessel.spec.convoy_ref) {
                    errors.push(format!("{}: vessel {} remains", vessel.spec.convoy_ref, vessel.metadata.name));
                }
            }
            for terminal in backend.clone().using::<TerminalSession>(NAMESPACE).list().await.map_err(|e| e.to_string())?.items {
                if let Some(convoy) = terminal.metadata.labels.get("flotilla.work/convoy").filter(|c| ended.contains(*c)) {
                    errors.push(format!("{convoy}: session {} remains", terminal.metadata.name));
                }
            }
            for checkout in backend.using::<Checkout>(NAMESPACE).list().await.map_err(|e| e.to_string())?.items {
                if let Some(convoy) = checkout.metadata.labels.get("flotilla.work/convoy").filter(|c| ended.contains(*c)) {
                    errors.push(format!(
                        "{convoy}: checkout {} remains: status={:?} deletion={:?} finalizers={:?}",
                        checkout.metadata.name, checkout.status, checkout.metadata.deletion_timestamp, checkout.metadata.finalizers
                    ));
                }
            }
        }
        Ok(errors)
    }

    async fn sample(&self) -> Result<TargetSample, String> {
        let homes = self
            .homes
            .iter()
            .map(|home| HomeSample {
                store: Some(home.store.load_metrics().snapshot()),
                controllers: home.runtime.controller_metrics.snapshot(),
                replication: home.replication.snapshot(),
                replication_lag: home.view.replication_lag.snapshot(),
                turn_delivery: home.view.turn_delivery.snapshot(),
                database_bytes: database_bytes(&home.database),
            })
            .collect();
        Ok(TargetSample { homes, resident_bytes: resident_bytes() })
    }
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|value| value.parse().ok()).unwrap_or(default)
}

fn load_long_runner_turns() -> u32 {
    env_or("FLOTILLA_LOAD_LONG_RUNNER_TURNS", 5)
}

fn load_primitives() -> bool {
    env_or("FLOTILLA_LOAD_PRIMITIVES", false)
}

fn catalog_forge_url() -> String {
    flotilla_config::dummy_fleet::DEFAULT_DUMMY_FORGE_SERVICE_URL.to_string()
}

fn dummy_fleet_settings() -> String {
    format!(
        "[dummy_fleet.agent]\nturn_delay_ms = {}\nlong_runner_turns = {}\n[dummy_fleet.change_request]\nmerge_after_ms = {}\n",
        env_or("FLOTILLA_LOAD_TURN_DELAY_MS", 1_500u64),
        load_long_runner_turns(),
        env_or("FLOTILLA_LOAD_MERGE_AFTER_MS", 0u64)
    )
}

fn smoke_plan() -> LoadPlan {
    LoadPlan {
        running: 10,
        warmup_ended: 0,
        drain: Duration::from_secs(180),
        ramp_per_second: 6.0,
        dispatch_concurrency: 6,
        window: Duration::from_secs(25),
        max_ramp: Duration::from_secs(30),
        convoy_deadline: Duration::from_secs(120),
        tick: Duration::from_millis(200),
        series_every: Duration::from_secs(5),
        ..LoadPlan::default()
    }
}

// Behaviour (#3206): the driver keeps convoys of every mix running against the
// current control plane through the dummy fleet, and each delivers.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn load_driver_smoke() {
    let fleet = InProcessFleet::start(1, &dummy_fleet_settings()).await;
    let plan = smoke_plan();
    let report = flotilla_load_driver::run(&fleet, &plan).await.expect("load report");
    eprintln!("{}", report.markdown());
    assert!(report.ramp.reached, "{:?}", report.ramp);
    assert_eq!(report.dispatch.errors, 0, "{:?}", report.dispatch.error_samples);
    assert!(report.correctness_errors.is_empty(), "{:?}", report.correctness_errors);
    assert_eq!(report.total.delivered, report.dispatch.admitted, "every admitted convoy delivers");
    for mix in CrewMix::ALL {
        assert!(report.total.by_mix.get(&format!("{mix:?}")).is_some_and(|outcome| outcome.delivered > 0), "{mix:?}: {:?}", report.total);
    }
    assert_eq!((report.window.failed, report.window.abandoned), (0, 0), "{:?}", report.window);
    let home = &report.home_metrics[0];
    assert!(home.store.as_ref().is_some_and(|store| store.write_execution.count > 0), "the store records writes");
    assert!(home.controllers.contains_key("Convoy"), "controller loops report: {:?}", home.controllers.keys().collect::<Vec<_>>());
    assert!(home.turn_delivery.count > 0, "turns are delivered and receipted");
}

// Measures one configured run; see the module documentation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "load measurement: run explicitly with FLOTILLA_LOAD_* settings"]
async fn load_baseline() {
    if std::env::var_os("FLOTILLA_LOAD_TRACE").is_some() {
        tracing_subscriber::fmt()
            .with_env_filter("flotilla_daemon::runtime::message=warn,flotilla_daemon::runtime::terminal=warn,flotilla_daemon::supervisor=warn,dummy_fleet_load=info")
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .try_init()
            .expect("load tracing");
    }
    let homes = env_or("FLOTILLA_LOAD_DAEMONS", 1usize);
    let fleet = InProcessFleet::start(homes, &dummy_fleet_settings()).await;
    let plan = LoadPlan {
        running: env_or("FLOTILLA_LOAD_RUNNING", 100),
        ramp_per_second: env_or("FLOTILLA_LOAD_RAMP_PER_SECOND", 20.0),
        dispatch_concurrency: env_or("FLOTILLA_LOAD_DISPATCH_CONCURRENCY", 16),
        window: Duration::from_secs(env_or("FLOTILLA_LOAD_WINDOW_SECS", 1800)),
        warmup_ended: env_or("FLOTILLA_LOAD_WARMUP_ENDED", env_or("FLOTILLA_LOAD_RUNNING", 100)),
        drain: Duration::from_secs(env_or("FLOTILLA_LOAD_DRAIN_SECS", 900)),
        unchanged_bound: Duration::from_secs(env_or("FLOTILLA_LOAD_UNCHANGED_SECS", 600)),
        timeline: env_or("FLOTILLA_LOAD_TIMELINE", false),
        dump_unfinished: env_or("FLOTILLA_LOAD_DUMP_UNFINISHED", true),
        max_ramp: Duration::from_secs(env_or("FLOTILLA_LOAD_MAX_RAMP_SECS", 900)),
        convoy_deadline: Duration::from_secs(env_or("FLOTILLA_LOAD_DEADLINE_SECS", 1800)),
        tick: Duration::from_millis(env_or("FLOTILLA_LOAD_TICK_MS", 500)),
        series_every: Duration::from_secs(env_or("FLOTILLA_LOAD_SERIES_SECS", 15)),
        ..LoadPlan::default()
    };
    // Capture source identity before the run, so a later checkout change does
    // not relabel a measurement. Preserve dirty patches in the JSON report.
    // Deliberate harness/build identity tooling: the Git boundary permits this
    // test target to invoke Git directly, rather than going through VCS adapters.
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git").args(args).output().expect("source identity");
        assert!(output.status.success(), "git source identity failed");
        String::from_utf8(output.stdout).expect("git UTF-8").trim().to_string()
    };
    let revision = git(&["rev-parse", "HEAD"]);
    let source_diff = git(&["diff", "HEAD"]);
    let source_status = git(&["status", "--porcelain"]);
    let started = Utc::now();
    let load_start = std::fs::read_to_string("/proc/loadavg").unwrap_or_else(|_| "unavailable".into());
    let fleet = Arc::new(fleet);
    // Progress survives a run the host kills: open convoys, memory and store size.
    let progress = {
        let fleet = Arc::clone(&fleet);
        tokio::spawn(async move {
            let began = tokio::time::Instant::now();
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let open = fleet.homes.iter().map(|home| home.view.places.lock().expect("places").len()).sum::<usize>();
                let database = fleet.homes.iter().filter_map(|home| database_bytes(&home.database)).sum::<u64>();
                for (index, home) in fleet.homes.iter().enumerate() {
                    let source = flotilla_protocol::IssueSource { service: catalog_forge_url(), scope: catalog::DUMMY_REPOSITORY.into() };
                    let owner =
                        flotilla_observation::forge_observation::source_owner(&home.daemon.resource_backend(), NAMESPACE, &source).await;
                    println!("observer home={index} owner={owner:?}");
                    let hosts = home.daemon.resource_backend().using::<Host>(NAMESPACE).list().await.expect("liveness hosts");
                    for host in hosts.items {
                        if let Some(status) = host.status {
                            println!(
                                "liveness t={:.0}s home={index} ready={} heartbeat_age_ms={:?} conditions={:?}",
                                began.elapsed().as_secs_f64(),
                                status.ready,
                                status.heartbeat_at.map(|at| (Utc::now() - at).num_milliseconds()),
                                status.conditions,
                            );
                        }
                    }
                }
                println!(
                    "progress t={:.0}s open={open} resident={} MiB database={} MiB",
                    began.elapsed().as_secs_f64(),
                    resident_bytes().unwrap_or(0) / (1024 * 1024),
                    database / (1024 * 1024)
                );
            }
        })
    };
    let mut report = flotilla_load_driver::run(&*fleet, &plan).await.expect("load report");
    progress.abort();
    report.metadata.insert("started_utc".into(), started.to_string());
    report.metadata.insert("commit".into(), revision);
    report.metadata.insert("source_status".into(), source_status);
    report.metadata.insert("source_diff".into(), source_diff);
    report.metadata.insert("dummy_fleet".into(), dummy_fleet_settings());
    report.metadata.insert("primitives".into(), load_primitives().to_string());
    report.metadata.insert(
        "turn_delivery_definition".into(),
        if load_primitives() {
            "engine Message creation entry -> AgentSession Receive edge about it (decided store log)"
        } else {
            "old-store Message creation_timestamp -> status.resolved_receiver.delivered_at"
        }
        .into(),
    );
    report.metadata.insert("effective_dummy_config".into(), format!("{:?}", fleet.homes[0].fleet.config()));
    report.metadata.insert("host_load_start".into(), load_start.trim().into());
    report
        .metadata
        .insert("host_load_end".into(), std::fs::read_to_string("/proc/loadavg").unwrap_or_else(|_| "unavailable".into()).trim().into());
    report.metadata.insert("trace".into(), std::env::var_os("FLOTILLA_LOAD_TRACE").is_some().to_string());
    report.metadata.insert("report_prefix".into(), std::env::var("FLOTILLA_LOAD_REPORT").unwrap_or_default());
    report.metadata.insert("max_resident_bytes".into(), format!("{:?}", fleet.memory_limit));
    let observer_failures: u64 = fleet.homes.iter().map(|home| home.view.observer_failure_sightings.load(Ordering::Relaxed)).sum();
    println!("observer_failure_sightings={observer_failures}");
    let markdown = report.markdown();
    println!("{markdown}");
    if let Ok(prefix) = std::env::var("FLOTILLA_LOAD_REPORT") {
        std::fs::write(format!("{prefix}.md"), format!("Run started {started}.\n\n{markdown}")).expect("markdown report");
        std::fs::write(format!("{prefix}.json"), serde_json::to_string_pretty(&report).expect("json")).expect("json report");
    }
    assert!(report.correctness_errors.is_empty(), "{:?}", report.correctness_errors);
    assert_eq!(report.total.delivered, report.dispatch.admitted, "every admitted convoy delivers");
    assert_eq!(report.dispatch.errors, 0, "{:?}", report.dispatch);
    assert_eq!(report.total.failed + report.total.abandoned, 0, "all load scripts should deliver: {:?}", report.total);
    if load_primitives() {
        for home in &report.home_metrics {
            assert!(home.turn_delivery.count > 0, "switch on, the engine log reader recorded no turn delivery");
        }
    }
    assert_eq!(observer_failures, 0, "load produced no-ready-forge-observer stall evidence; see observer_failure lines");
}

// Compare the receiver stage of a pass on one frozen, production dummy-fleet
// database. A single explicit address should cost only its convoy and crew,
// regardless of retained fleet history. Ignored because this starts a full fleet.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "receiver decode measurement: starts the dummy fleet"]
async fn receiver_snapshot_decode_measurement() {
    use flotilla_messaging::MessageReceiverSnapshot;
    use flotilla_resources::TerminalSession;
    use flotilla_store_testkit::ReadCountsBackendExt;

    let fleet = InProcessFleet::start(1, &dummy_fleet_settings()).await;
    // This probe freezes active crew records for decoder comparisons. Reuse
    // the small fleet settings while deliberately leaving its tail open.
    let plan = LoadPlan { drain: Duration::ZERO, ..smoke_plan() };
    let report = flotilla_load_driver::run(&fleet, &plan).await.expect("load report");
    assert!(report.ramp.reached, "{:?}", report.ramp);
    assert!(report.window.delivered >= 3, "{:?}", report.window);
    // VACUUM INTO captures a consistent database while the fleet is running;
    // receiver reads below use an isolated copy without controller traffic.
    let frozen = fleet._dir.path().join("receiver-measurement.sqlite");
    let live = rusqlite::Connection::open(&fleet.homes[0].database).expect("live database");
    live.execute("VACUUM INTO ?1", [frozen.to_str().expect("path")]).expect("freeze database");
    let store = SqliteBackend::open(&frozen).expect("frozen store");
    let backend = ResourceBackend::new(store).with_read_counts();
    let terminal = backend
        .including_replicas::<TerminalSession>(NAMESPACE)
        .list()
        .await
        .expect("terminals")
        .items
        .into_iter()
        .find(|source| source.object.metadata.labels.contains_key("flotilla.work/convoy"))
        .expect("crew terminal");
    let labels = &terminal.object.metadata.labels;
    let convoy = &labels["flotilla.work/convoy"];
    let address = format!("{NAMESPACE}/{convoy}/{}/{}", labels["flotilla.work/vessel"], labels["flotilla.work/role"]);
    let before = backend.read_snapshot();
    MessageReceiverSnapshot::load(&backend, NAMESPACE).await.expect("full snapshot");
    let full = backend.read_snapshot().since(&before);
    let before = backend.read_snapshot();
    let scoped = MessageReceiverSnapshot::load_for_addresses(&backend, NAMESPACE, &[address]).await.expect("scoped snapshot");
    let targeted = backend.read_snapshot().since(&before);
    let db = rusqlite::Connection::open(&frozen).expect("frozen database");
    // body_json is the exact UTF-8 input passed to the SQLite typed decoder.
    let bytes = |kind: &str, name: Option<&str>| -> u64 {
        db.query_row("SELECT COALESCE(SUM(length(CAST(body_json AS BLOB))),0) FROM resource_objects WHERE namespace=?1 AND kind=?2 AND (?3 IS NULL OR name=?3)",
            rusqlite::params![NAMESPACE, kind, name], |row| row.get(0)).expect("stored bytes")
    };
    let full_bytes: u64 = ["Convoy", "TerminalSession", "ConvoyEnsure"].into_iter().map(|kind| bytes(kind, None)).sum();
    let scoped_bytes = bytes("Convoy", Some(convoy))
        + scoped.terminals().iter().map(|source| bytes("TerminalSession", Some(&source.object.metadata.name))).sum::<u64>();
    let rows = |reads: &flotilla_store_testkit::ReadSnapshot| -> usize {
        ["Convoy", "TerminalSession", "ConvoyEnsure"]
            .into_iter()
            .map(|kind| reads.objects_by_kind().get(kind).copied().unwrap_or_default())
            .sum()
    };
    eprintln!(
        "receiver stage per pass: before rows={} bytes={full_bytes}; after rows={} bytes={scoped_bytes}",
        rows(&full),
        rows(&targeted)
    );
    assert_eq!(rows(&targeted), 2, "one convoy and one crew terminal: {targeted}");
    assert!(rows(&full) > rows(&targeted));
    assert!(full_bytes > scoped_bytes);
}
