//! Crew kinds load benchmark (#3629 layer 6): ignored by default.
//!
//! ```text
//! cargo test -p flotilla-core --release --locked --test crew_bench -- --ignored --nocapture
//! ```
//!
//! `CREW_BENCH_CONVOYS` sets the load (default 10,000), so a run can be staged upward under a
//! memory limit. The run ends by reporting the process's peak resident memory.
//!
//! Two temp-file stores in WAL mode with `synchronous = NORMAL`: the local home and a remote one.
//! 10,000 convoys' worth of crew kinds, nine in ten homed remotely and replicated into the local
//! store through its change stream, one in ten local. Each convoy has one vessel, leased through
//! the placement composition on one of 100 hosts and realised, and two terminal sessions in it (an
//! agent and a tool), realised.
//!
//! Per-event cost on the local home, one transaction each: a vessel's placement (the lease's
//! grant and the vessel's create), its realise, and a session's create and realise. The placement
//! ledger's rebuild from the stored live vessels, as a restart does it, is timed too. The engine's
//! write paths (layer 7) are timed: a session's launches through the terminal port, and through
//! the vessel port a pinned vessel's lease granted, realised and lost (Q6).
//!
//! Per-query reads, on a read connection, each statement's VM steps and full-scan steps counted by
//! SQLite: a convoy's vessels (`vessel.by_launch`, by its convoy), a host's live vessels (`vessel.live_by_host`,
//! the ledger's view), a vessel's sessions (`terminal_session.by_holder`), and a convoy's sessions
//! (its vessels by convoy, then their sessions by holder).

use std::{collections::BTreeMap, env, fs, path::PathBuf, process, sync::Arc, time::Instant};

use flotilla_engine_store::{open_home, register_crew, StoreLog};
use flotilla_provisioning::{
    engine::{
        terminals::EngineTerminals,
        vessel::{EngineVessels, PlacementLifecycles},
        LifecycleHome, Lifecycles, Step,
    },
    stored::{
        self,
        terminal_session::{Launch, TerminalSession, TerminalSessionKind, TerminalSessionRequest},
        Holder,
    },
};
use flotilla_resources::{
    terminal_records::{TerminalLaunchRequest, TerminalLaunchSource, TerminalRecords, TerminalRenewal, PROCESS_EXITED},
    vessel_records::{VesselLaunchRequest, VesselRecords, VesselStep},
};
use flotilla_schema::{KindRegistry, Ref as StoredRef, Timestamp};
use flotilla_schema_sqlite::{Store, StoreHandle};
use flotilla_state_machines::{
    machines::{
        convoy::{ConvoyId, Priority, VesselClass},
        lifecycle::{MemoryLifecycleWorld, Observed, Realise, VanishCause},
        vessel::{Vessel, VesselKind},
    },
    placement::{Dimension, Host, Placements, Request, VesselId},
    Home, Ref,
};
use rusqlite::{Connection, StatementStatus};

const LOCAL_ONE_IN: usize = 10;
const HOSTS: u32 = 100;

fn convoys() -> usize {
    env::var("CREW_BENCH_CONVOYS").ok().map_or(10_000, |n| n.parse().expect("CREW_BENCH_CONVOYS is a count"))
}

struct Database(PathBuf);

impl Drop for Database {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

fn connection(name: &str) -> (Database, Connection) {
    let path = env::temp_dir().join(format!("flotilla-crew-bench-{}-{name}.db", process::id()));
    let database = Database(path);
    drop(Database(database.0.clone()));
    let conn = Connection::open(&database.0).expect("open");
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0)).expect("wal");
    assert_eq!(mode, "wal");
    conn.pragma_update(None, "synchronous", "NORMAL").expect("synchronous");
    (database, conn)
}

fn registry() -> KindRegistry {
    let mut registry = stored::registry().expect("provisioning");
    registry.register::<flotilla_state_machines::Resource<TerminalSession>>().expect("session");
    register_crew(&mut registry).expect("crew");
    registry
}

const CORES: Dimension = Dimension(0);
const MEMORY: Dimension = Dimension(1);

fn hosts() -> Vec<(Host, BTreeMap<Dimension, u32>)> {
    (0..HOSTS).map(|host| (Host::new(format!("host-{host}")), BTreeMap::from([(CORES, 4_096), (MEMORY, 65_536)]))).collect()
}

fn convoy(n: usize, home: &str) -> ConvoyId {
    ConvoyId::new(format!("convoy-{n}"), home)
}

/// Convoy `n`'s session `which`, held for its vessel's launch `vessel`.
fn session(n: usize, which: &str, home: &str, vessel: &str) -> TerminalSessionRequest {
    TerminalSessionRequest {
        launch_of: format!("session-{n}-{which}"),
        launch: 0,
        environment: StoredRef::new(format!("env-{n}"), home),
        holder: Some(Holder::vessel(vessel)),
        role: which.into(),
        pool: "cleat".into(),
        cwd: "/workspace".into(),
        env: BTreeMap::from([("FLOTILLA_ROLE".into(), which.into())]),
        source: Launch::Tool { command: format!("{which} --serve") },
    }
}

fn realised(name: &str) -> MemoryLifecycleWorld {
    let mut world = MemoryLifecycleWorld::default();
    world.realisations.put(&Ref::new(name), Observed { identity: name.into() });
    world
}

/// One home's crew writers over its store.
struct Crew {
    placements: Placements<StoreLog>,
    sessions: Home<TerminalSession, StoreLog>,
}

impl Crew {
    fn new(home: &str, store: &Arc<StoreHandle>) -> Self {
        let log = || StoreLog::new(store.clone());
        let (placements, unplaced) = Placements::new(Home::with_log(home, log()).expect("declaration"), hosts()).expect("hosts");
        assert!(unplaced.is_empty());
        Crew { placements, sessions: Home::with_log(home, log()).expect("declaration") }
    }

    /// Convoy `n`'s vessel placed and realised, and its two sessions realised. Returns the
    /// vessel's launch name.
    fn seed(&mut self, n: usize, home: &str, at: i64) -> String {
        let id = VesselId::new(format!("vessel-{n}"));
        let request = Request {
            convoy: convoy(n, home),
            class: VesselClass::new("crew"),
            shape: BTreeMap::from([(CORES, 4), (MEMORY, 8)]),
            priority: Priority(1),
        };
        self.placements.request(id.clone(), request).expect("request");
        let vessel = self.placements.place(&id, Host::new(format!("host-{}", (n as u32) % HOSTS)), Timestamp(at)).expect("placed");
        let id = VesselId::new(vessel.name());
        self.placements
            .apply(&id, Realise { identity: id.0.clone(), observed: Ref::new(id.0.as_str()) }.into(), &realised(&id.0), Timestamp(at + 1))
            .expect("realised");
        for which in ["agent", "tool"] {
            let name = format!("session-{n}-{which}");
            let created = self.sessions.create(&name, session(n, which, home, &id.0), &MemoryLifecycleWorld::default(), Timestamp(at));
            let name = created.expect("created").resource.name().to_owned();
            self.sessions
                .apply(&name, Realise { identity: name.clone(), observed: Ref::new(name.as_str()) }, &realised(&name), Timestamp(at + 1))
                .expect("realised");
        }
        id.0
    }
}

/// Convoy `n`'s crew session, as its vessel requests it.
/// Convoy `n`'s crew session, held for its vessel's launch `vessel`.
fn crew_session(n: usize, vessel: &str) -> TerminalLaunchRequest {
    TerminalLaunchRequest {
        session: format!("terminal-vessel-{n}-coder"),
        environment: format!("env-{n}"),
        vessel: Some(vessel.to_owned()),
        role: "coder".into(),
        pool: "cleat".into(),
        cwd: "/workspace".into(),
        env: BTreeMap::new(),
        source: TerminalLaunchSource::Tool { command: "claude".into() },
    }
}

/// Local convoy `n`'s pinned vessel launch, on one of the hosts.
fn pinned(n: usize) -> VesselLaunchRequest {
    VesselLaunchRequest {
        convoy: format!("convoy-{n}"),
        convoy_home: "local".into(),
        work: "implement".into(),
        host: format!("host-{}", (n as u32) % HOSTS),
    }
}

struct Measured {
    micros: f64,
    vm_steps: f64,
    fullscan_steps: f64,
    rows: f64,
}

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

fn per_event(what: &str, count: usize, started: Instant, writes: &str) {
    eprintln!("{what:>40}: {:>8.1} µs/event ({writes})", started.elapsed().as_secs_f64() * 1e6 / count as f64);
}

/// The identity id of `(kind, name, home)`.
const IDENTITY: &str = "(SELECT i.id FROM resource_identity i \
     WHERE i.kind = ?1 AND i.name = (SELECT id FROM strings WHERE domain = 'rn' AND s = ?2) \
     AND i.home = (SELECT id FROM strings WHERE domain = 'hm' AND s = ?3))";

fn peak_resident_kib() -> Option<u64> {
    fs::read_to_string("/proc/self/status").ok().and_then(|status| {
        status.lines().find_map(|line| line.strip_prefix("VmHWM:").and_then(|kib| kib.trim().trim_end_matches("kB").trim().parse().ok()))
    })
}

// Behaviour (#3629 layer 6): the 10,000-convoy load of crew kinds, mostly replicas: per-event
// cost of placements, lifecycle and attempt writes through the decided store, the ledger's
// rebuild, and keyed or indexed reads that scan no table.
#[test]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
fn crew_load() {
    let convoys = convoys();
    let (_remote_db, remote_conn) = connection("remote");
    let remote = Arc::new(StoreHandle::new(Store::open(remote_conn, "remote", registry()).expect("store")));
    let (local_db, local_conn) = connection("local");
    let local = Arc::new(StoreHandle::new(Store::open(local_conn, "local", registry()).expect("store")));

    let started = Instant::now();
    let mut remote_crew = Crew::new("remote", &remote);
    // Each convoy's vessel launch, by convoy.
    let mut vessels = BTreeMap::new();
    for n in (0..convoys).filter(|n| n % LOCAL_ONE_IN != 0) {
        vessels.insert(n, remote_crew.seed(n, "remote", 1));
    }
    let mut after = 0;
    let mut replicated = 0;
    loop {
        let (events, changes) = {
            let mut remote = remote.lock().expect("remote");
            let mut read = remote.read().expect("read");
            let events = read.changes_since("remote", after, 1_000).expect("changes");
            let changes = read.changes_of(&events).expect("their changes");
            (events, changes)
        };
        let Some(last) = events.last() else { break };
        after = last.home_seq;
        let mut store = local.lock().expect("local");
        let mut write = store.write().expect("write");
        // A receiver records each entry's change before the entry (#4024).
        for change in &changes {
            write.record_change(change).expect("change");
        }
        for event in &events {
            write.apply(event).expect("replicated");
        }
        write.commit().expect("commit");
        replicated += events.len();
    }
    let locals: Vec<usize> = (0..convoys).filter(|n| n % LOCAL_ONE_IN == 0).collect();
    let mut crew = Crew::new("local", &local);
    let seeding = Instant::now();
    for n in &locals {
        vessels.insert(*n, crew.seed(*n, "local", 1));
    }
    eprintln!(
        "{:>40}: {convoys} convoys ({} local, {} replicated through {replicated} events) in {:.1?}",
        "seeded",
        locals.len(),
        convoys - locals.len(),
        started.elapsed()
    );
    per_event("convoy seeded locally", locals.len(), seeding, "place, realise, two sessions created and realised: 6 transactions");

    // A fresh vessel per local convoy: the lease's grant and the vessel's create, then its realise.
    let ids: Vec<VesselId> = locals.iter().map(|n| VesselId::new(format!("vessel-{n}-b"))).collect();
    for (n, id) in locals.iter().zip(&ids) {
        let request = Request {
            convoy: convoy(*n, "local"),
            class: VesselClass::new("crew"),
            shape: BTreeMap::from([(CORES, 4)]),
            priority: Priority(1),
        };
        crew.placements.request(id.clone(), request).expect("request");
    }
    let started = Instant::now();
    let ids: Vec<VesselId> = locals
        .iter()
        .zip(&ids)
        .map(|(n, id)| {
            let placed = crew.placements.place(id, Host::new(format!("host-{}", (*n as u32 + 1) % HOSTS)), Timestamp(10)).expect("placed");
            VesselId::new(placed.name())
        })
        .collect();
    per_event("placement, one transaction", ids.len(), started, "the lease checked at the ledger; the vessel's row and log entry");
    let started = Instant::now();
    for id in &ids {
        crew.placements
            .apply(id, Realise { identity: id.0.clone(), observed: Ref::new(id.0.as_str()) }.into(), &realised(&id.0), Timestamp(11))
            .expect("realised");
    }
    per_event("vessel realise, one transaction", ids.len(), started, "the state and the log entry");
    let started = Instant::now();
    for n in &locals {
        let name = format!("session-{n}-review");
        crew.sessions
            .create(&name, session(*n, "review", "local", &vessels[n]), &MemoryLifecycleWorld::default(), Timestamp(12))
            .expect("created");
    }
    per_event("session create, one transaction", locals.len(), started, "the row, its env rows, identity and log entry");

    // The crew sessions' write path (#3629 layer 7): each local convoy's crew session requested
    // and realised as the TerminalSession controller steps it, its process exited, and renewed
    // into its next launch, realised.
    let launches: Arc<dyn Lifecycles<TerminalSessionKind>> =
        Arc::new(LifecycleHome::new("local", Home::with_log("local", StoreLog::new(local.clone())).expect("declaration")));
    let terminals = EngineTerminals::new(Arc::clone(&launches), local.clone(), || vec!["local".to_owned()]);
    let requests: Vec<_> = locals.iter().map(|n| crew_session(*n, &vessels[n])).collect();
    let started = Instant::now();
    for request in &requests {
        let first = terminals.request(request.clone()).expect("requested");
        launches.step(&first, &Step::Realised { identity: format!("{first}-s1") }, Timestamp(20)).expect("realised");
        let exited = VanishCause::Lost { reason: Some(format!("{PROCESS_EXITED}: exit 0")) };
        launches.step(&first, &Step::Vanished { cause: exited }, Timestamp(21)).expect("exited");
        let TerminalRenewal::Started(second) = terminals.renew(&request.session).expect("renewed") else { panic!("the launch ended") };
        launches.step(&second, &Step::Realised { identity: format!("{second}-s2") }, Timestamp(22)).expect("realised");
    }
    per_event(
        "session run, exit, renewed run",
        requests.len() * 3,
        started,
        "launch #1 requested and realised, vanished; launch #2 requested and realised",
    );

    // The read ports (#3629 layer 8): a session's current launch by its name, and a vessel's
    // sessions (which seats it serves) through the index.
    let started = Instant::now();
    for request in &requests {
        let view = terminals.get(&request.session, None).expect("read").expect("held");
        assert_eq!(Some(view.name), launches.newest_launch(&request.session).map(|held| held.name().to_owned()), "the current launch");
    }
    eprintln!(
        "{:>40}: {:>8.1} µs/query",
        "port: a session's current launch",
        started.elapsed().as_secs_f64() * 1e6 / requests.len() as f64
    );
    let started = Instant::now();
    for n in &locals {
        assert!(terminals.of_vessel(&vessels[n]).expect("index").len() >= 2);
    }
    eprintln!("{:>40}: {:>8.1} µs/query", "port: a vessel's served sessions", started.elapsed().as_secs_f64() * 1e6 / locals.len() as f64);

    // The Vessel write path (#3629 layer 7, Q6): each local convoy's pinned vessel requested (its
    // lease granted), realised, then lost, through the engine's vessel port. Its lease is freed,
    // so the restart below holds only the seeded leases.
    let pinned_vessels: Arc<dyn Lifecycles<VesselKind>> =
        Arc::new(PlacementLifecycles::new("local", open_home::<Vessel>(StoreLog::new(local.clone())).expect("home")).expect("placements"));
    let records = EngineVessels::new(pinned_vessels, local.clone(), || vec!["local".to_owned()]);
    let started = Instant::now();
    for n in &locals {
        let launch = records.request(&pinned(*n)).expect("requested");
        records.step(&launch, &VesselStep::Realised { environment: format!("env-{n}") }).expect("realised");
        records.step(&launch, &VesselStep::Lost { reason: "environment disappeared".into() }).expect("lost");
    }
    per_event(
        "pinned vessel requested, ready, lost",
        locals.len() * 3,
        started,
        "the lease granted on its pinned host; realised; vanished, the lease freed",
    );

    // The Vessel read ports (#3629 layer 8, Vessel): a vessel's launch by its name, and a work's
    // launches through the index.
    let started = Instant::now();
    for n in &locals {
        let record = records.get(&vessels[n], None).expect("read").expect("held");
        assert!(matches!(record.reading, flotilla_protocol::LifecycleReading::Ready { .. }), "{:?}", record.reading);
    }
    eprintln!("{:>40}: {:>8.1} µs/query", "port: a vessel's launch", started.elapsed().as_secs_f64() * 1e6 / locals.len() as f64);
    let started = Instant::now();
    for n in &locals {
        assert!(!records.launches(&format!("convoy-{n}"), "local", &format!("vessel-{n}"), Some("local")).expect("index").is_empty());
    }
    eprintln!("{:>40}: {:>8.1} µs/query", "port: a work's vessels", started.elapsed().as_secs_f64() * 1e6 / locals.len() as f64);

    // A restart: the vessels' home and the ledger's leases rebuilt from the store.
    let started = Instant::now();
    let home = open_home::<Vessel>(StoreLog::new(local.clone())).expect("home");
    let restored = home.resources().count();
    let (placements, unplaced) = Placements::new(home, hosts()).expect("hosts");
    assert!(unplaced.is_empty());
    assert_eq!(placements.ledger().leases().count(), 2 * locals.len(), "every live local vessel holds its lease again");
    eprintln!("{:>40}: {:.1?} ({restored} vessels)", "ledger rebuilt from the store", started.elapsed());

    let read = Connection::open(&local_db.0).expect("read connection");
    let sample = |kind: &str, name: &dyn Fn(usize) -> String| -> Vec<Vec<String>> {
        (0..convoys)
            .step_by(7)
            .map(|n| vec![kind.to_owned(), name(n), if n % LOCAL_ONE_IN == 0 { "local".into() } else { "remote".into() }])
            .collect()
    };
    let convoy_keys = sample("cv", &|n| format!("convoy-{n}"));
    let by_convoy = measure(&read, &format!("SELECT v.id FROM vessel v WHERE v.convoy = {IDENTITY}"), &convoy_keys);
    assert!(by_convoy.rows >= 1.0, "each convoy has its vessel");
    assert_eq!(by_convoy.fullscan_steps, 0.0, "the lookup scans no table");
    report("a convoy's vessels", &by_convoy);
    let host_keys: Vec<Vec<String>> = (0..HOSTS).map(|host| vec![format!("host-{host}")]).collect();
    let live = measure(
        &read,
        "SELECT v.id FROM vessel v WHERE v.host = (SELECT id FROM strings WHERE domain = 'hs' AND s = ?1) \
         AND v.state IN ('rq', 'rl')",
        &host_keys,
    );
    assert!(live.rows >= 1.0, "each host leases live vessels");
    assert_eq!(live.fullscan_steps, 0.0);
    report("a host's live vessels", &live);
    let holder_keys: Vec<Vec<String>> = (0..convoys).step_by(7).map(|n| vec![format!("vessel:{}", vessels[&n])]).collect();
    let sessions = measure(
        &read,
        "SELECT s.id FROM terminal_session s WHERE s.holder = (SELECT id FROM strings WHERE domain = 'hl' AND s = ?1)",
        &holder_keys,
    );
    assert!(sessions.rows >= 2.0);
    assert_eq!(sessions.fullscan_steps, 0.0);
    report("a vessel's sessions", &sessions);
    // A session's convoy is its vessel's (#3720 look-over): the convoy's vessels, then each one's
    // sessions by holder.
    let convoy_sessions = measure(
        &read,
        &format!(
            "SELECT s.id FROM vessel v JOIN resource_identity i ON i.id = v.id \
             JOIN strings h ON h.domain = 'hl' AND h.s = 'vessel:' || (SELECT n.s FROM strings n WHERE n.id = i.name) \
             JOIN terminal_session s ON s.holder = h.id WHERE v.convoy = {IDENTITY}"
        ),
        &convoy_keys,
    );
    assert!(convoy_sessions.rows >= 2.0, "each convoy has its sessions");
    assert_eq!(convoy_sessions.fullscan_steps, 0.0);
    report("a convoy's sessions", &convoy_sessions);
    match peak_resident_kib() {
        Some(kib) => eprintln!("{:>40}: {} MiB", "peak resident memory", kib / 1024),
        None => eprintln!("{:>40}: n/a", "peak resident memory"),
    }
}
