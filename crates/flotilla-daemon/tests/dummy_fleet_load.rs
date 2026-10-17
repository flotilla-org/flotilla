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
//! FLOTILLA_LOAD_RUNNING=1000 FLOTILLA_LOAD_DAEMONS=1 FLOTILLA_LOAD_WINDOW_SECS=300 \
//! FLOTILLA_LOAD_REPORT=/tmp/load-1000 \
//!   cargo test --release -p flotilla-daemon --locked --test dummy_fleet_load load_baseline -- --ignored --nocapture
//! ```
//!
//! `FLOTILLA_LOAD_MAX_RESIDENT_GIB` stops admitting convoys once the process
//! holds that much memory, so a plan the host cannot hold reports how far it
//! got. Progress lines every 30 s survive a run the host kills.
//!
//! This is its own test target: it reads the process's resident memory, which
//! other tests in a shared binary would distort.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
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
    CrewCommandPort, DaemonCommandPort,
};
use flotilla_load_driver::{ConvoyObservation, ConvoyState, FleetTarget, HomeSample, LoadPlan, TargetSample};
use flotilla_protocol::{commands::CommandValue, CommandAction, HostName};
use flotilla_resources::{
    Convoy, ConvoyPhase, Forge, Host, InputMeta, Message, PlacementPolicy, ReadWatchEvent, Repository, ResourceProvenance, WatchEvent,
    WatchStart, WorkflowTemplate,
};
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
    convoys: Mutex<HashMap<String, (String, ConvoyState)>>,
    /// Where each open convoy is: its phase and its crews' work phases.
    places: Mutex<HashMap<String, String>>,
    turn_delivery: LatencyHistogram,
    replication_lag: LatencyHistogram,
}

/// Admission instants of every home's convoys, by resource name; a replica's
/// arrival elsewhere is measured against them.
type Admissions = Arc<Mutex<HashMap<String, tokio::time::Instant>>>;

struct Home {
    daemon: Arc<InProcessDaemon>,
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
    std::fs::write(config_dir.join("daemon.toml"), format!("machine_id = \"load-{index}\"\n\n[dummy_fleet]\n{dummy_fleet}"))
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
    seed(&backend, &fleet.config().forge_service_url).await;
    let view = Arc::new(HomeView::default());
    watch_convoys(&backend, Arc::clone(&view), admissions).await;
    watch_messages(&backend, Arc::clone(&view)).await;
    let port = DaemonCommandPort::new(&(Arc::clone(&daemon) as Arc<dyn DaemonHandle>));
    Home { daemon, runtime, store, database, port, replication: Arc::default(), view }
}

async fn seed(backend: &ResourceBackend, forge_service_url: &str) {
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
                    admissions.lock().expect("admissions").entry(convoy.metadata.name.clone()).or_insert_with(tokio::time::Instant::now);
                    let state = convoy.status.as_ref().map_or(ConvoyState::Open, |status| state_of(status.phase));
                    view.convoys.lock().expect("convoys").insert(convoy.spec.role.clone(), (convoy.metadata.name.clone(), state));
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
            }
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
        Ok(convoys.iter().map(|(name, (_, state))| ConvoyObservation { name: name.clone(), state: *state }).collect())
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

    async fn sample(&self) -> Result<TargetSample, String> {
        for (index, home) in self.homes.iter().enumerate() {
            let active = home
                .daemon
                .resource_backend()
                .using::<Message>(NAMESPACE)
                .query(&flotilla_store::MessageQuery::active())
                .await
                .map_err(|error| error.to_string())?
                .len();
            eprintln!("delivery active messages: home={index} active={active}");
        }
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

// Behaviour (#3206): the driver keeps convoys of every mix running against the
// current control plane through the dummy fleet, and each delivers.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn load_driver_smoke() {
    let fleet = InProcessFleet::start(1, &dummy_fleet_settings()).await;
    let plan = LoadPlan {
        running: 6,
        ramp_per_second: 6.0,
        dispatch_concurrency: 6,
        window: Duration::from_secs(25),
        max_ramp: Duration::from_secs(30),
        convoy_deadline: Duration::from_secs(120),
        tick: Duration::from_millis(200),
        series_every: Duration::from_secs(5),
        ..LoadPlan::default()
    };
    let report = flotilla_load_driver::run(&fleet, &plan).await.expect("load report");
    eprintln!("{}", report.markdown());
    assert!(report.ramp.reached, "{:?}", report.ramp);
    assert_eq!(report.dispatch.errors, 0, "{:?}", report.dispatch.error_samples);
    assert!(report.window.delivered >= 3, "{:?}", report.window);
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
    let homes = env_or("FLOTILLA_LOAD_DAEMONS", 1usize);
    let fleet = InProcessFleet::start(homes, &dummy_fleet_settings()).await;
    let plan = LoadPlan {
        running: env_or("FLOTILLA_LOAD_RUNNING", 100),
        ramp_per_second: env_or("FLOTILLA_LOAD_RAMP_PER_SECOND", 20.0),
        dispatch_concurrency: env_or("FLOTILLA_LOAD_DISPATCH_CONCURRENCY", 16),
        window: Duration::from_secs(env_or("FLOTILLA_LOAD_WINDOW_SECS", 120)),
        max_ramp: Duration::from_secs(env_or("FLOTILLA_LOAD_MAX_RAMP_SECS", 900)),
        convoy_deadline: Duration::from_secs(env_or("FLOTILLA_LOAD_DEADLINE_SECS", 900)),
        tick: Duration::from_millis(env_or("FLOTILLA_LOAD_TICK_MS", 500)),
        series_every: Duration::from_secs(env_or("FLOTILLA_LOAD_SERIES_SECS", 15)),
        ..LoadPlan::default()
    };
    let started = Utc::now();
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
                println!(
                    "progress t={:.0}s open={open} resident={} MiB database={} MiB",
                    began.elapsed().as_secs_f64(),
                    resident_bytes().unwrap_or(0) / (1024 * 1024),
                    database / (1024 * 1024)
                );
            }
        })
    };
    let report = flotilla_load_driver::run(&*fleet, &plan).await.expect("load report");
    progress.abort();
    let markdown = report.markdown();
    println!("{markdown}");
    if let Ok(prefix) = std::env::var("FLOTILLA_LOAD_REPORT") {
        std::fs::write(format!("{prefix}.md"), format!("Run started {started}.\n\n{markdown}")).expect("markdown report");
        std::fs::write(format!("{prefix}.json"), serde_json::to_string_pretty(&report).expect("json")).expect("json report");
    }
}
