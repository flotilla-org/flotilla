//! Crew kinds load benchmark (#3629 layer 6): ignored by default.
//!
//! ```text
//! cargo test -p flotilla-provisioning --release --locked --test crew_bench -- --ignored --nocapture
//! ```
//!
//! `CREW_BENCH_CONVOYS` sets the load (default 10,000), so a run can be staged upward under a
//! memory limit. The run ends by reporting the process's peak resident memory.
//!
//! Two temp-file stores in WAL mode with `synchronous = NORMAL`: the local home and a remote one.
//! 10,000 convoys' worth of crew kinds, nine in ten homed remotely and replicated into the local
//! store through its change stream, one in ten local. Each convoy has one vessel, leased through
//! the placement composition on one of 100 hosts and realised, and two terminal sessions in it (an
//! agent and a tool), realised. 100 standing roles each hold an Ensure.
//!
//! Per-event cost on the local home, one transaction each: a vessel's placement (the lease's
//! grant and the vessel's create), its realise, a session's create and realise, and an Ensure's
//! failed attempt. The placement ledger's rebuild from the stored live vessels, as a restart does
//! it, is timed too. The engine's write paths (layer 7) are timed through `decide`: a session's
//! launches, an Ensure's patches, and a pinned vessel's lease granted, realised and lost (Q6).
//!
//! Per-query reads, on a read connection, each statement's VM steps and full-scan steps counted by
//! SQLite: a convoy's vessels (`vessel.by_convoy`), a host's live vessels (`vessel.live_by_host`,
//! the ledger's view), a vessel's sessions (`terminal_session.by_holder`), a convoy's sessions
//! (its vessels by convoy, then their sessions by holder) and a role's Ensure (`ensure.by_inbox`).

use std::{
    collections::BTreeMap,
    env, fs,
    path::PathBuf,
    process,
    sync::{Arc, Mutex},
    time::Instant,
};

use flotilla_engine_store::{open_home, register_crew, StoreLog};
use flotilla_provisioning::{
    engine::{
        decide, launch,
        reads::ProvisioningReads,
        terminal_session::TerminalSessionBinding,
        vessel::{PlacementLifecycles, VesselBinding},
        LifecycleHome, Lifecycles,
    },
    stored::{
        self,
        terminal_session::{Launch, TerminalSession, TerminalSessionKind, TerminalSessionRequest},
        Holder,
    },
};
use flotilla_resources::{ConvoyEnsureStatusPatch, StatusPatch, TerminalSessionStatusPatch, VesselStatusPatch};
use flotilla_schema::{Address, KindRegistry, Ref as StoredRef, Timestamp};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    machines::{
        convoy::{ConvoyId, Priority, VesselClass},
        ensure::{Attempted as EnsureAttempted, Create, Ensure, EnsureSpec, Fulfiller, MemoryEnsureWorld, Record},
        lifecycle::{LifecycleState, MemoryLifecycleWorld, Observed, Realise},
        vessel::{Vessel, VesselKind},
    },
    placement::{Dimension, Host, Placements, Request, VesselId},
    AttemptPolicy, Cause, CauseCategory, Home, Outcome, Ref,
};
use rusqlite::{Connection, StatementStatus};

const LOCAL_ONE_IN: usize = 10;
const HOSTS: u32 = 100;
const ROLES: usize = 100;

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

fn session(n: usize, which: &str, home: &str) -> TerminalSessionRequest {
    TerminalSessionRequest {
        environment: StoredRef::new(format!("env-{n}"), home),
        holder: Some(Holder::vessel(&format!("vessel-{n}"))),
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
    ensures: Home<Ensure, StoreLog>,
}

impl Crew {
    fn new(home: &str, store: &Arc<Mutex<Store>>) -> Self {
        let log = || StoreLog::new(store.clone());
        let (placements, unplaced) = Placements::new(Home::with_log(home, log()).expect("declaration"), hosts()).expect("hosts");
        assert!(unplaced.is_empty());
        Crew {
            placements,
            sessions: Home::with_log(home, log()).expect("declaration"),
            ensures: Home::with_log(home, log()).expect("declaration"),
        }
    }

    /// Convoy `n`'s vessel placed and realised, and its two sessions realised.
    fn seed(&mut self, n: usize, home: &str, at: i64) {
        let id = VesselId::new(format!("vessel-{n}"));
        let request = Request {
            convoy: convoy(n, home),
            class: VesselClass::new("crew"),
            shape: BTreeMap::from([(CORES, 4), (MEMORY, 8)]),
            priority: Priority(1),
        };
        self.placements.set_caps(convoy(n, home), [(VesselClass::new("crew"), 1)].into());
        self.placements.request(id.clone(), request).expect("request");
        self.placements.place(&id, Host::new(format!("host-{}", (n as u32) % HOSTS)), Timestamp(at)).expect("placed");
        self.placements
            .apply(&id, Realise { identity: id.0.clone(), observed: Ref::new(id.0.as_str()) }.into(), &realised(&id.0), Timestamp(at + 1))
            .expect("realised");
        for which in ["agent", "tool"] {
            let name = format!("session-{n}-{which}");
            self.sessions.create(&name, session(n, which, home), &MemoryLifecycleWorld::default(), Timestamp(at)).expect("created");
            self.sessions
                .apply(&name, Realise { identity: name.clone(), observed: Ref::new(name.as_str()) }, &realised(&name), Timestamp(at + 1))
                .expect("realised");
        }
    }
}

fn inbox(role: usize) -> Address {
    format!("project:p{role}/role:standing").parse().expect("inbox")
}

/// Convoy `n`'s crew session as the old store holds it, starting.
fn old_session(n: usize) -> flotilla_resources::ResourceObject<flotilla_resources::TerminalSession> {
    flotilla_resources::ResourceObject {
        metadata: meta(&format!("terminal-vessel-{n}-coder")),
        spec: flotilla_resources::TerminalSessionSpec {
            env_ref: format!("env-{n}"),
            role: "coder".into(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "claude".into() },
            cwd: "/workspace".into(),
            pool: "cleat".into(),
            env: BTreeMap::new(),
        },
        status: None,
    }
}

/// Standing role `role`'s ConvoyEnsure, untouched.
fn standing(role: usize) -> flotilla_resources::ResourceObject<flotilla_resources::ConvoyEnsure> {
    flotilla_resources::ResourceObject {
        metadata: meta(&format!("standing-{role}")),
        spec: flotilla_resources::ConvoyEnsureSpec::builder()
            .project_ref(format!("p{role}"))
            .role("ops".into())
            .repositories(vec![])
            .build(),
        status: None,
    }
}

/// Local convoy `n`'s old Vessel, pinned to one of the hosts, before provisioning.
fn old_vessel(n: usize) -> flotilla_resources::ResourceObject<flotilla_resources::Vessel> {
    flotilla_resources::ResourceObject {
        metadata: meta(&format!("pinned-{n}")),
        spec: flotilla_resources::VesselSpec {
            convoy_ref: format!("convoy-{n}"),
            vessel_name: "implement".into(),
            placement_policy_ref: "docker".into(),
            adopted_checkout_refs: BTreeMap::new(),
        },
        status: None,
    }
}

/// The old Vessel patches of a pinned vessel's life: provisioning on its host, ready, lost.
fn pinned_life(n: usize) -> [VesselStatusPatch; 3] {
    use flotilla_protocol::{CanonicalHostId, PlacementDecision, PlacementTargetHost};
    let host = format!("host-{}", (n as u32) % HOSTS);
    let decision = PlacementDecision::builder()
        .policy_name("docker".into())
        .target_host(PlacementTargetHost { reference: CanonicalHostId::resolved(host.clone()), display_name: host })
        .build();
    [
        VesselStatusPatch::MarkProvisioning {
            observed_policy_ref: "docker".into(),
            observed_policy_version: "1".into(),
            placement_decision: Some(decision),
            started_at: chrono::DateTime::UNIX_EPOCH,
            message: None,
            image: None,
        },
        VesselStatusPatch::MarkReady {
            configured_limits: None,
            placement_decision: None,
            environment_ref: Some(format!("env-{n}")),
            image_ref: None,
            local_image_id: None,
            registry_digest: None,
            checkout_refs: BTreeMap::new(),
            terminal_session_refs: vec![],
            requested_stance: Default::default(),
            effective_stance: Default::default(),
            ready_at: chrono::DateTime::UNIX_EPOCH,
            image: None,
        },
        VesselStatusPatch::MarkLost { message: "environment disappeared".into() },
    ]
}

fn meta(name: &str) -> flotilla_resources::ObjectMeta {
    flotilla_resources::ObjectMeta {
        name: name.into(),
        namespace: "flotilla".into(),
        resource_version: "1".into(),
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
        owner_references: vec![],
        finalizers: vec![],
        deletion_timestamp: None,
        creation_timestamp: chrono::DateTime::UNIX_EPOCH,
        merge: None,
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
    let remote = Arc::new(Mutex::new(Store::open(remote_conn, "remote", registry()).expect("store")));
    let (local_db, local_conn) = connection("local");
    let local = Arc::new(Mutex::new(Store::open(local_conn, "local", registry()).expect("store")));

    let started = Instant::now();
    let mut remote_crew = Crew::new("remote", &remote);
    for n in (0..convoys).filter(|n| n % LOCAL_ONE_IN != 0) {
        remote_crew.seed(n, "remote", 1);
    }
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
    let locals: Vec<usize> = (0..convoys).filter(|n| n % LOCAL_ONE_IN == 0).collect();
    let mut crew = Crew::new("local", &local);
    let seeding = Instant::now();
    for n in &locals {
        crew.seed(*n, "local", 1);
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
        crew.placements.set_caps(convoy(*n, "local"), [(VesselClass::new("crew"), 2)].into());
        let request = Request {
            convoy: convoy(*n, "local"),
            class: VesselClass::new("crew"),
            shape: BTreeMap::from([(CORES, 4)]),
            priority: Priority(1),
        };
        crew.placements.request(id.clone(), request).expect("request");
    }
    let started = Instant::now();
    for (n, id) in locals.iter().zip(&ids) {
        crew.placements.place(id, Host::new(format!("host-{}", (*n as u32 + 1) % HOSTS)), Timestamp(10)).expect("placed");
    }
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
        crew.sessions.create(&name, session(*n, "review", "local"), &MemoryLifecycleWorld::default(), Timestamp(12)).expect("created");
    }
    per_event("session create, one transaction", locals.len(), started, "the row, its env rows, identity and log entry");

    // Standing roles: an Ensure each, a fulfiller recorded and created, one failed attempt.
    let policy = AttemptPolicy { max_failures: 3, base: 1_000_000, factor: 2, cap: 60_000_000 };
    for role in 0..ROLES {
        let name = format!("ensure-{role}");
        crew.ensures
            .create(&name, EnsureSpec { inbox: inbox(role), policy }, &MemoryEnsureWorld::default(), Timestamp(13))
            .expect("created");
        let fulfiller = Ref::<Fulfiller>::new(format!("standing-{role}"));
        crew.ensures.apply(&name, Record { fulfiller: fulfiller.clone() }, &MemoryEnsureWorld::default(), Timestamp(14)).expect("recorded");
        crew.ensures.apply(&name, Create { fulfiller }, &MemoryEnsureWorld::default(), Timestamp(15)).expect("created");
    }
    let started = Instant::now();
    for role in 0..ROLES {
        let failed = Outcome::Failed { cause: Cause::new(CauseCategory::Transient, "admission refused"), retryable: true };
        crew.ensures
            .apply(
                &format!("ensure-{role}"),
                EnsureAttempted { fulfiller: Ref::new(format!("standing-{role}")), outcome: failed },
                &MemoryEnsureWorld::default(),
                Timestamp(16),
            )
            .expect("attempt");
    }
    per_event("ensure attempt, one transaction", ROLES, started, "the tally and the log entry");

    // The crew reconcilers' write path (#3629 layer 7): each local convoy's crew session run,
    // stopped and resumed through the engine as TerminalSession decides it, a new launch each run.
    let launches: Arc<dyn Lifecycles<TerminalSessionKind>> =
        Arc::new(LifecycleHome::new("local", Home::with_log("local", StoreLog::new(local.clone())).expect("declaration")));
    let mut objects: Vec<_> = locals.iter().map(|n| old_session(*n)).collect();
    let started = Instant::now();
    for (round, id) in [(0, "s1"), (1, "s2")] {
        for obj in objects.iter_mut() {
            let running = TerminalSessionStatusPatch::MarkRunning {
                configured_limits: None,
                session_id: format!("{}-{id}", obj.metadata.name),
                pid: None,
                started_at: chrono::DateTime::UNIX_EPOCH,
                crew: None,
                launch_command: "claude".into(),
                delivered_message_id: None,
            };
            let decided =
                decide::<TerminalSessionBinding>(launches.as_ref(), obj, Some(running), chrono::Utc::now()).patch.expect("running");
            decided.apply(obj.status.get_or_insert_with(Default::default));
            if round == 0 {
                let stopped = TerminalSessionStatusPatch::MarkStopped {
                    stopped_at: chrono::DateTime::UNIX_EPOCH,
                    inner_command_status: None,
                    inner_exit_code: Some(0),
                    message: None,
                };
                let decided =
                    decide::<TerminalSessionBinding>(launches.as_ref(), obj, Some(stopped), chrono::Utc::now()).patch.expect("stopped");
                decided.apply(obj.status.get_or_insert_with(Default::default));
                TerminalSessionStatusPatch::MarkStarting.apply(obj.status.get_or_insert_with(Default::default));
            }
        }
    }
    per_event(
        "session run, stop, resumed run",
        objects.len() * 3,
        started,
        "launch #1 requested and realised, released; launch #2 requested and realised",
    );
    assert!(objects.iter().all(|obj| launches.get(&launch(&obj.metadata.name, 2)).is_some()));

    // The read ports (#3629 layer 8): a session's current launch by its record name, and a
    // vessel's sessions (which seats it serves) through the index.
    let reads = flotilla_provisioning::engine::reads::StoreReads::new(local.clone());
    let started = Instant::now();
    for obj in &objects {
        let view = reads.terminal_session(&obj.metadata.name, "local").expect("read").expect("held");
        assert_eq!(view.name, launch(&obj.metadata.name, 2), "the current launch");
    }
    eprintln!("{:>40}: {:>8.1} µs/query", "port: a session's current launch", started.elapsed().as_secs_f64() * 1e6 / objects.len() as f64);
    let started = Instant::now();
    for n in &locals {
        assert!(reads.held_sessions(&Holder::vessel(&format!("vessel-{n}"))).expect("index").len() >= 2);
    }
    eprintln!("{:>40}: {:>8.1} µs/query", "port: a vessel's served sessions", started.elapsed().as_secs_f64() * 1e6 / locals.len() as f64);

    // Standing roles' Ensures decided through the engine: a convoy bound, failed, a replacement bound.
    let ensures = flotilla_core::primitives::ensures::EngineEnsurePath::open(local.clone(), Arc::new(flotilla_resources::SystemClock))
        .expect("ensures");
    let started = Instant::now();
    for role in 0..ROLES {
        let mut obj = standing(role);
        for patch in [
            ConvoyEnsureStatusPatch::Running { convoy_ref: format!("standing-{role}-1"), observed_at: chrono::Utc::now() },
            ConvoyEnsureStatusPatch::BackingOff { retry_at: chrono::Utc::now(), failure: "convoy failed".into() },
            ConvoyEnsureStatusPatch::Running { convoy_ref: format!("standing-{role}-2"), observed_at: chrono::Utc::now() },
        ] {
            let decided = ensures.decide_now(&obj, patch).expect("decided").expect("taken");
            decided.apply(obj.status.get_or_insert_with(Default::default));
        }
    }
    per_event("ensure patch decided", ROLES * 3, started, "its edges (record, create, attempt, bind or unbind), one transaction each");

    // The Vessel write path (#3629 layer 7, Q6): each local convoy's pinned vessel granted as its
    // provisioning starts, realised, then lost, through the engine. Its lease is freed, so the
    // restart below holds only the seeded leases.
    let pinned: Arc<dyn Lifecycles<VesselKind>> =
        Arc::new(PlacementLifecycles::new("local", open_home::<Vessel>(StoreLog::new(local.clone())).expect("home")).expect("placements"));
    let started = Instant::now();
    for n in &locals {
        let mut obj = old_vessel(*n);
        for patch in pinned_life(*n) {
            let decided = decide::<VesselBinding>(pinned.as_ref(), &obj, Some(patch), chrono::Utc::now()).patch.expect("taken");
            decided.apply(obj.status.get_or_insert_with(Default::default));
        }
    }
    per_event(
        "pinned vessel provisioned, ready, lost",
        locals.len() * 3,
        started,
        "the lease granted on its pinned host; realised; vanished, the lease freed",
    );

    // The Vessel read ports (#3629 layer 8, Vessel): a vessel's lease by its name, and a convoy's
    // vessels through the index.
    let started = Instant::now();
    for n in &locals {
        let view = reads.vessel(&format!("vessel-{n}"), "local").expect("read").expect("held");
        assert!(matches!(view.state, LifecycleState::Realised { .. }));
    }
    eprintln!("{:>40}: {:>8.1} µs/query", "port: a vessel's lease", started.elapsed().as_secs_f64() * 1e6 / locals.len() as f64);
    let started = Instant::now();
    for n in &locals {
        assert!(!reads.convoy_vessels(&convoy(*n, "local")).expect("index").is_empty());
    }
    eprintln!("{:>40}: {:>8.1} µs/query", "port: a convoy's vessels", started.elapsed().as_secs_f64() * 1e6 / locals.len() as f64);

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
    let holder_keys: Vec<Vec<String>> = (0..convoys).step_by(7).map(|n| vec![format!("vessel:vessel-{n}")]).collect();
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
    let inboxes: Vec<Vec<String>> = (0..ROLES).map(|role| vec![inbox(role).to_string()]).collect();
    let ensures =
        measure(&read, "SELECT e.id FROM ensure e WHERE e.inbox = (SELECT id FROM strings WHERE domain = 'ad' AND s = ?1)", &inboxes);
    assert_eq!(ensures.rows, 1.0);
    assert_eq!(ensures.fullscan_steps, 0.0);
    report("a role's ensure", &ensures);
    match peak_resident_kib() {
        Some(kib) => eprintln!("{:>40}: {} MiB", "peak resident memory", kib / 1024),
        None => eprintln!("{:>40}: n/a", "peak resident memory"),
    }
}
