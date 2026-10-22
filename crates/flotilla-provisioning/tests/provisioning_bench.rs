//! Provisioning kinds load benchmark (#3629 layer 2): ignored by default.
//!
//! ```text
//! cargo test -p flotilla-provisioning --release --locked --test provisioning_bench -- --ignored --nocapture
//! ```
//!
//! `PROVISIONING_BENCH_CONVOYS` sets the load (default 10,000), so a run can be staged upward
//! under a memory limit. The run ends by reporting the process's peak resident memory.
//!
//! Two temp-file stores in WAL mode with `synchronous = NORMAL`: the local home and a remote one,
//! as in the Convoy benchmark. 10,000 convoys' worth of provisioning, nine in ten homed remotely
//! and replicated into the local store through its change stream, one in ten local. Each convoy has
//! a Docker Environment (its recipe a frozen document with two mounts, three env values and two adapters) on one of 100 providers, built
//! from one of 50 ImageBuilds; a Clone of one of 20 repositories; and two worktree Checkouts owned
//! by its convoy. Every resource is Realised.
//!
//! Per-event cost, on the local home, one transaction each through `Home` over the store's log:
//! a third Checkout's create, its failed attempt, its realise and its release.
//!
//! Per-query reads, on a read connection, each statement's VM steps and full-scan steps counted by
//! SQLite (`sqlite3_stmt_status`): an environment's checkouts (`by_environment`), an environment's
//! clone of a repository (`clone.by_environment`), a provider's live environments
//! (`live_by_provider`), and one resource by key; and the same reads through the store's typed
//! index pages.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::PathBuf,
    process,
    sync::{Arc, Mutex},
    time::Instant,
};

use flotilla_engine_store::StoreLog;
use flotilla_provisioning::stored::{
    self,
    checkout::{Checkout, CheckoutRequest, Source},
    clone::{Clone, CloneRequest},
    environment::{Environment, EnvironmentRequest, Recipe},
    image_build::{Architecture, Delivery, ImageBuild, ImageBuildRequest, Reservation},
    Holder,
};
use flotilla_resources::{ImageLayerSource, ImageLayerSpec, ImageLayerTier};
use flotilla_schema::{Address, Frozen, KindRegistry, Ref as StoredRef, Timestamp};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    machines::lifecycle::{Attempted, LifecycleKind, MemoryLifecycleWorld, Observed, Realise, Release},
    Cause, CauseCategory, Home, Outcome, Ref,
};
use rusqlite::{Connection, StatementStatus};

/// The ceiling: 10,000 convoys, unless `PROVISIONING_BENCH_CONVOYS` stages a smaller run.
fn convoys() -> usize {
    env::var("PROVISIONING_BENCH_CONVOYS").ok().map_or(10_000, |n| n.parse().expect("PROVISIONING_BENCH_CONVOYS is a count"))
}
/// One convoy in this many is local; the rest are replicas.
const LOCAL_ONE_IN: usize = 10;
const PROVIDERS: usize = 100;
const BUILDS: usize = 50;
const REPOSITORIES: usize = 20;

struct Database(PathBuf);

impl Drop for Database {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

fn connection(name: &str) -> (Database, Connection) {
    let path = env::temp_dir().join(format!("flotilla-provisioning-bench-{}-{name}.db", process::id()));
    let database = Database(path);
    drop(Database(database.0.clone()));
    let conn = Connection::open(&database.0).expect("open");
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0)).expect("wal");
    assert_eq!(mode, "wal");
    conn.pragma_update(None, "synchronous", "NORMAL").expect("synchronous");
    (database, conn)
}

fn registry() -> KindRegistry {
    stored::registry().expect("registry")
}

fn provider(n: usize) -> Address {
    format!("host:host-{}/provider:docker", n % PROVIDERS).parse().expect("provider")
}

fn repository(n: usize) -> String {
    format!("repo-{}", n % REPOSITORIES)
}

fn layers() -> Frozen<Vec<ImageLayerSpec>> {
    let layer = ImageLayerSpec::builder()
        .name("base".into())
        .version("1.0.0".into())
        .tier(ImageLayerTier::Base)
        .source(ImageLayerSource::Stdlib)
        .from("docker.io/library/ubuntu@sha256:00".into())
        .fragment("RUN true".into())
        .probe("true".into())
        .build();
    Frozen::new(vec![layer]).expect("freeze")
}

fn environment(n: usize, home: &str) -> EnvironmentRequest {
    EnvironmentRequest {
        provider: provider(n),
        image_build: Some(StoredRef::new(format!("image-{}", n % BUILDS), home)),
        recipe: Frozen::new(Recipe::of(&docker_recipe(n)).expect("recipe")).expect("freeze"),
        holder: None,
    }
}

/// Convoy `n`'s Docker recipe: the provider's own document, two mounts, three env values and two
/// adapters.
fn docker_recipe(n: usize) -> flotilla_resources::DockerEnvironmentSpec {
    use flotilla_resources::{EnvironmentMount, EnvironmentMountMode};
    flotilla_resources::DockerEnvironmentSpec {
        host_ref: String::new(),
        image: format!("registry/flotilla@sha256:{}", n % BUILDS),
        image_build_ref: Some(format!("image-{}", n % BUILDS)),
        declared_agent_adapters: BTreeSet::from(["claude-code".into(), "codex".into()]),
        required_agent_adapters: BTreeSet::from(["claude-code".into()]),
        pull_policy: Default::default(),
        memory_policy: Default::default(),
        mounts: vec![
            EnvironmentMount { source_path: format!("/work/{n}"), target_path: "/workspace".into(), mode: EnvironmentMountMode::Rw },
            EnvironmentMount { source_path: "/cache".into(), target_path: "/cache".into(), mode: EnvironmentMountMode::Ro },
        ],
        env: BTreeMap::from([("A".into(), "1".into()), ("B".into(), "2".into()), ("C".into(), n.to_string())]),
    }
}

fn checkout(n: usize, which: &str, home: &str) -> CheckoutRequest {
    CheckoutRequest {
        repository: repository(n),
        environment: StoredRef::new(format!("env-{n}"), home),
        branch: format!("convoy-{n}-{which}"),
        base: Some("main".into()),
        path: format!("/workspace/{which}"),
        source: Source::Worktree,
        clone: Some(StoredRef::new(format!("clone-{n}"), home)),
        holder: Some(Holder::convoy(&format!("convoy-{n}"))),
    }
}

/// The homes of one store, one per kind, sharing its log.
struct Homes {
    environments: Home<Environment, StoreLog>,
    builds: Home<ImageBuild, StoreLog>,
    clones: Home<Clone, StoreLog>,
    checkouts: Home<Checkout, StoreLog>,
}

impl Homes {
    fn new(home: &str, store: &Arc<Mutex<Store>>) -> Self {
        let log = || StoreLog::new(store.clone());
        Homes {
            environments: Home::with_log(home, log()).expect("declaration"),
            builds: Home::with_log(home, log()).expect("declaration"),
            clones: Home::with_log(home, log()).expect("declaration"),
            checkouts: Home::with_log(home, log()).expect("declaration"),
        }
    }
}

/// Creates `name` and realises it under `name`.
fn realised<K: LifecycleKind>(
    home: &mut Home<flotilla_state_machines::machines::lifecycle::Lifecycle<K>, StoreLog>,
    name: &str,
    spec: K::Request,
) where
    StoreLog: flotilla_state_machines::Log<flotilla_state_machines::machines::lifecycle::Lifecycle<K>>,
{
    let mut world = MemoryLifecycleWorld::default();
    world.realisations.put(&Ref::new(name), Observed { identity: name.into() });
    home.create(name, spec, &world, Timestamp(1)).expect("created");
    home.apply(name, Realise { identity: name.into(), observed: Ref::new(name) }, &world, Timestamp(2)).expect("realised");
}

/// Seeds the image builds, then each convoy's environment, clone and two checkouts, all realised.
fn seed(homes: &mut Homes, home: &str, convoys: impl Iterator<Item = usize>) {
    for k in 0..BUILDS {
        let build = ImageBuildRequest {
            composition: format!("sha256:{k}"),
            architecture: Architecture::Amd64,
            layers: layers(),
            delivery: Delivery::Registry { repository: "registry/flotilla".into() },
            reservation: Reservation { cpu: 4, disk_bytes: 1 << 34 },
        };
        realised(&mut homes.builds, &format!("image-{k}"), build);
    }
    for n in convoys {
        realised(&mut homes.environments, &format!("env-{n}"), environment(n, home));
        let clone = CloneRequest {
            repository: repository(n),
            url: format!("https://example/{}", repository(n)),
            environment: StoredRef::new(format!("env-{n}"), home),
            path: format!("/clones/{}", repository(n)),
        };
        realised(&mut homes.clones, &format!("clone-{n}"), clone);
        for which in ["a", "b"] {
            realised(&mut homes.checkouts, &format!("checkout-{n}-{which}"), checkout(n, which, home));
        }
    }
}

struct Measured {
    micros: f64,
    vm_steps: f64,
    fullscan_steps: f64,
    rows: f64,
}

/// `sql` run once per argument list: the mean time, VM steps, full-scan steps and rows returned.
fn measure(conn: &Connection, sql: &str, arguments: &[Vec<String>]) -> Measured {
    let mut statement = conn.prepare(sql).expect("prepare");
    let (mut steps, mut fullscan, mut rows) = (0i64, 0i64, 0usize);
    let started = Instant::now();
    for argument in arguments {
        statement.reset_status(StatementStatus::VmStep);
        statement.reset_status(StatementStatus::FullscanStep);
        rows += statement.query_map(rusqlite::params_from_iter(argument), |_| Ok(())).expect("query").count();
        steps += i64::from(statement.get_status(StatementStatus::VmStep));
        fullscan += i64::from(statement.get_status(StatementStatus::FullscanStep));
    }
    let count = arguments.len() as f64;
    Measured {
        micros: started.elapsed().as_secs_f64() * 1e6 / count,
        vm_steps: steps as f64 / count,
        fullscan_steps: fullscan as f64 / count,
        rows: rows as f64 / count,
    }
}

fn report(what: &str, m: &Measured) {
    eprintln!(
        "{what:>40}: {:>8.1} µs/query {:>9.1} VM steps {:>8.1} full-scan steps {:>6.1} rows",
        m.micros, m.vm_steps, m.fullscan_steps, m.rows
    );
}

/// The identity id of `(kind, name, home)`.
const IDENTITY: &str = "(SELECT i.id FROM resource_identity i \
     WHERE i.kind = ?1 AND i.name = (SELECT id FROM strings WHERE domain = 'rn' AND s = ?2) \
     AND i.home = (SELECT id FROM strings WHERE domain = 'hm' AND s = ?3))";

fn per_event(what: &str, count: usize, started: Instant, writes: &str) {
    eprintln!("{what:>40}: {:>8.1} µs/event ({writes})", started.elapsed().as_secs_f64() * 1e6 / count as f64);
}

// Behaviour (#3629 layer 2): the 10,000-convoy load of provisioning kinds, mostly replicas: the
// per-event cost of lifecycle writes through the decided store, and keyed or indexed reads that
// scan no table and read a handful of rows whatever the store holds.
#[test]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
fn provisioning_load() {
    let convoys = convoys();
    let (_remote_db, remote_conn) = connection("remote");
    let remote = Arc::new(Mutex::new(Store::open(remote_conn, "remote", registry()).expect("store")));
    let (local_db, local_conn) = connection("local");
    let local = Arc::new(Mutex::new(Store::open(local_conn, "local", registry()).expect("store")));

    let started = Instant::now();
    let mut remote_homes = Homes::new("remote", &remote);
    seed(&mut remote_homes, "remote", (0..convoys).filter(|n| n % LOCAL_ONE_IN != 0));
    let mut after = 0;
    let mut replicated = 0;
    loop {
        let events = remote.lock().expect("remote").read().expect("read").changes_since("remote", after, 1_000).expect("changes");
        let Some(last) = events.last() else { break };
        after = last.home_seq;
        let mut store = local.lock().expect("local");
        let mut write = store.write().expect("write");
        for event in &events {
            write.apply(event).expect("replicated");
        }
        write.commit().expect("commit");
        replicated += events.len();
    }
    let mut homes = Homes::new("local", &local);
    let locals: Vec<usize> = (0..convoys).filter(|n| n % LOCAL_ONE_IN == 0).collect();
    seed(&mut homes, "local", locals.iter().copied());
    eprintln!(
        "{:>40}: {convoys} convoys ({} local, {} replicated through {replicated} events) in {:.1?}",
        "seeded",
        locals.len(),
        convoys - locals.len(),
        started.elapsed()
    );

    let world = |name: &str| {
        let mut world = MemoryLifecycleWorld::default();
        world.realisations.put(&Ref::new(name), Observed { identity: name.into() });
        world.teardowns.put(&Ref::new(name), true);
        world
    };
    let names: Vec<String> = locals.iter().map(|n| format!("checkout-{n}-c")).collect();
    let started = Instant::now();
    for (n, name) in locals.iter().zip(&names) {
        homes.checkouts.create(name, checkout(*n, "c", "local"), &world(name), Timestamp(3)).expect("created");
    }
    per_event("checkout create, one transaction", names.len(), started, "the row, its identity and the log entry");
    let failure = Outcome::Failed { cause: Cause::new(CauseCategory::Transient, "worktree add failed"), retryable: true };
    let started = Instant::now();
    for name in &names {
        homes.checkouts.apply(name, Attempted { outcome: failure.clone() }, &world(name), Timestamp(4)).expect("attempt");
    }
    per_event("failed attempt, one transaction", names.len(), started, "the tally and the log entry");
    let started = Instant::now();
    for name in &names {
        homes
            .checkouts
            .apply(name, Realise { identity: name.clone(), observed: Ref::new(name.as_str()) }, &world(name), Timestamp(5))
            .expect("realised");
    }
    per_event("realise, one transaction", names.len(), started, "the state and the log entry");
    let started = Instant::now();
    for name in &names {
        homes.checkouts.apply(name, Release { teardown: Ref::new(name.as_str()) }, &world(name), Timestamp(6)).expect("released");
    }
    per_event("release, one transaction", names.len(), started, "the state and the log entry");

    // Typed index pages through the store, as the read ports will issue them.
    let started = Instant::now();
    let mut paged = 0;
    for n in (0..convoys).step_by(7) {
        let home = if n % LOCAL_ONE_IN == 0 { "local" } else { "remote" };
        let query = CheckoutRequest::by_environment(&StoredRef::new(format!("env-{n}"), home));
        paged += local.lock().expect("local").read().expect("read").page(&query, None, 100).expect("page").ids.len();
    }
    let queries = convoys.div_ceil(7);
    assert!(paged >= 2 * queries, "each environment has its two checkouts");
    eprintln!("{:>40}: {:>8.1} µs/query", "by_environment, store page", started.elapsed().as_secs_f64() * 1e6 / queries as f64);
    let started = Instant::now();
    let mut live = 0;
    for n in 0..PROVIDERS {
        live += local
            .lock()
            .expect("local")
            .read()
            .expect("read")
            .page(&EnvironmentRequest::live_by_provider(&provider(n)), None, 1_000)
            .expect("page")
            .ids
            .len();
    }
    assert_eq!(live, convoys, "every environment is live on its provider");
    eprintln!(
        "{:>40}: {:>8.1} µs/query ({} environments each)",
        "live_by_provider, store page",
        started.elapsed().as_secs_f64() * 1e6 / PROVIDERS as f64,
        convoys / PROVIDERS
    );

    let read = Connection::open(&local_db.0).expect("read connection");
    let (objects, _): (i64, i64) = read
        .query_row("SELECT count(*), coalesce(sum(length(bytes)), 0) FROM objects", [], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("objects");
    // One frozen composition shared by every build, and each environment's own recipe (its mount
    // and env values name the convoy).
    assert_eq!(objects, 1 + convoys as i64, "every build shares the one frozen composition");
    let keys = |kind: &str, name: &dyn Fn(usize) -> String| -> Vec<Vec<String>> {
        (0..convoys)
            .step_by(7)
            .map(|n| vec![kind.to_owned(), name(n), if n % LOCAL_ONE_IN == 0 { "local".into() } else { "remote".into() }])
            .collect()
    };
    let environments = keys("ev", &|n| format!("env-{n}"));
    let by_environment = measure(&read, &format!("SELECT c.id FROM checkout c WHERE c.environment = {IDENTITY}"), &environments);
    // Two checkouts each, and a local convoy's released third one: its row stays, Gone.
    let sampled = (0..convoys).step_by(7);
    let expected = sampled.clone().map(|n| if n % LOCAL_ONE_IN == 0 { 3 } else { 2 }).sum::<usize>() as f64 / sampled.count() as f64;
    assert_eq!(by_environment.rows, expected);
    assert_eq!(by_environment.fullscan_steps, 0.0, "the lookup scans no table");
    report("an environment's checkouts", &by_environment);
    let clones: Vec<Vec<String>> =
        environments.iter().zip((0..convoys).step_by(7)).map(|(key, n)| [key.clone(), vec![repository(n)]].concat()).collect();
    let clone = measure(
        &read,
        &format!("SELECT c.id FROM clone c WHERE c.environment = {IDENTITY} AND c.repository = (SELECT id FROM strings WHERE domain = 'rk' AND s = ?4)"),
        &clones,
    );
    assert_eq!(clone.rows, 1.0);
    assert_eq!(clone.fullscan_steps, 0.0);
    report("an environment's clone of a repository", &clone);
    let providers: Vec<Vec<String>> = (0..PROVIDERS).map(|n| vec![provider(n).to_string()]).collect();
    let live = measure(
        &read,
        "SELECT e.id FROM environment e WHERE e.provider = (SELECT id FROM strings WHERE domain = 'ad' AND s = ?1) AND e.state IN ('rq', 'rl')",
        &providers,
    );
    assert_eq!(live.fullscan_steps, 0.0);
    report("a provider's live environments", &live);
    let keyed = measure(&read, &format!("SELECT e.state, e.identity FROM environment e WHERE e.id = {IDENTITY}"), &environments);
    assert_eq!(keyed.rows, 1.0);
    assert_eq!(keyed.fullscan_steps, 0.0);
    report("one environment by key", &keyed);
    match peak_resident_kib() {
        Some(kib) => eprintln!("{:>40}: {} MiB", "peak resident memory", kib / 1024),
        None => eprintln!("{:>40}: n/a", "peak resident memory"),
    }
}

/// The process's peak resident set (`VmHWM`), in KiB, where `/proc` reports it.
fn peak_resident_kib() -> Option<u64> {
    fs::read_to_string("/proc/self/status").ok().and_then(|status| {
        status.lines().find_map(|line| line.strip_prefix("VmHWM:").and_then(|kib| kib.trim().trim_end_matches("kB").trim().parse().ok()))
    })
}
