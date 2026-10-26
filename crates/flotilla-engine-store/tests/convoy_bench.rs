//! Convoy load benchmark (#3587 layer 4): ignored by default.
//!
//! ```text
//! cargo test -p flotilla-engine-store --release --locked --test convoy_bench -- --ignored --nocapture
//! ```
//!
//! Two temp-file stores in WAL mode with `synchronous = NORMAL`: the local home and a remote one.
//! 10,000 convoys, nine in ten homed remotely and replicated into the local store through its
//! change stream, one in ten local. Each convoy is admitted with two seats, two template promises,
//! a lifetime duty and two quota classes over one of 100 projects, and has one submission on
//! promise 0, a change request of its own.
//!
//! Per-event cost: each local convoy takes two engine edges through `Home::apply` over the store's
//! log, one transaction each: a `submit` on promise 1 (a submission row, the promise's progress and
//! the log entry) and the observed `keep` of promise 0 (the promise's progress and the log entry).
//!
//! Per-query reads, on a read connection, each statement's VM steps and full-scan steps counted by
//! SQLite (`sqlite3_stmt_status`):
//! - a project's open convoys, the `open_by_project` index;
//! - the read path from a change-request event to its promise (#3627 look-over ruling 5): the
//!   submissions naming the change request's address, through `by_address`, joined to their
//!   promise and to the convoy's root row to keep Open convoys; then the same lookup with the index
//!   dropped, for comparison.
//!
//! Promise kinds (#3587 layer 5): each admission resolves the stock kinds with a charter that
//! tightens `pr` and freezes the kind it used; the frozen document is content-addressed, so the
//! store keeps one object for all 10,000 convoys. Per event, the `pr` kind's ready and kept rules
//! are judged over in-memory observations.

use std::{
    env, fs,
    path::PathBuf,
    process,
    sync::{Arc, Mutex},
    time::Instant,
};

use flotilla_engine_store::{convoy_registry, StoreLog};
use flotilla_schema::{Address, Frozen, Timestamp};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    kinds::{stock, Catalog, KindDefinition, Observation, Observations, Rule, Template},
    machines::convoy::{
        AdmittedPromise, Convoy, ConvoyRequest, ConvoySpec, Duty, DutyId, DutyScope, Keep, MemoryConvoyWorld, Priority, PromiseId,
        PromiseKinds, PromiseSource, Seat, SeatName, Submit, Terms, VesselClass,
    },
    Home, Ref,
};
use rusqlite::{Connection, StatementStatus};

const CONVOYS: usize = 10_000;
/// One convoy in this many is local; the rest are replicas.
const LOCAL_ONE_IN: usize = 10;
const PROJECTS: usize = 100;

struct Database(PathBuf);

impl Drop for Database {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

fn connection(name: &str) -> (Database, Connection) {
    let path = env::temp_dir().join(format!("flotilla-convoy-bench-{}-{name}.db", process::id()));
    let database = Database(path);
    drop(Database(database.0.clone()));
    let conn = Connection::open(&database.0).expect("open");
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0)).expect("wal");
    assert_eq!(mode, "wal");
    conn.pragma_update(None, "synchronous", "NORMAL").expect("synchronous");
    (database, conn)
}

fn address(text: &str) -> Address {
    Address::project(text).expect("address")
}

fn project(n: usize) -> Address {
    address(&format!("project-{}", n % PROJECTS))
}

fn change(n: usize, promise: u32) -> Address {
    address(&format!("change-{n}-{promise}"))
}

fn seat(role: &str) -> SeatName {
    SeatName::new(role, "")
}

/// The stock kinds under a charter that also requires an approving review before `pr` is kept.
fn catalog() -> Catalog {
    let tightened = KindDefinition {
        name: "pr".into(),
        extends: Some("pr".into()),
        kept: vec!["$cr .review == approved".parse::<Template>().expect("template")],
        ..KindDefinition::default()
    };
    Catalog::resolve(&stock(), &[tightened]).expect("kinds resolve")
}

fn frozen_kinds() -> Frozen<PromiseKinds> {
    catalog().freeze(["pr"]).expect("freeze")
}

/// A merged, approved, passing change request.
struct Merged;

impl Observations for Merged {
    fn observe(&self, observation: Observation, _: &Address) -> Option<String> {
        let value = match observation {
            Observation::ChangeRequestState => "merged",
            Observation::ChangeRequestChecks => "pass",
            Observation::ChangeRequestReview => "approved",
            Observation::ChangeRequestMergeable => "true",
            _ => return None,
        };
        Some(value.to_owned())
    }
}

fn request(n: usize) -> ConvoyRequest {
    let promise = |label: &str| AdmittedPromise {
        label: label.into(),
        kind: "pr".into(),
        target: address("target"),
        source: PromiseSource::Template,
        owner: seat("coder"),
    };
    ConvoyRequest {
        project: project(n),
        promises: [(PromiseId(0), promise("template/pr")), (PromiseId(1), promise("l2-quint"))].into(),
        duties: [(DutyId(0), Duty { owner: seat("lead"), scope: DutyScope::Lifetime, terms: Terms::default() })].into(),
        seats: vec![
            Seat {
                retired: false,
                replacements: 0,
                role: "lead".into(),
                seat: String::new(),
                parent: None,
                supervised_by: address("supervisor"),
            },
            Seat {
                retired: false,
                replacements: 0,
                role: "coder".into(),
                seat: String::new(),
                parent: Some(seat("lead")),
                supervised_by: address("lead"),
            },
        ],
        quota: [(VesselClass::new("standard"), 2), (VesselClass::new("large"), 1)].into(),
        priority: Priority(1),
        kinds: frozen_kinds(),
    }
}

/// Every project exists, and every change request is durable.
fn world() -> MemoryConvoyWorld {
    let mut world = MemoryConvoyWorld::default();
    for n in 0..PROJECTS {
        world.projects.put(&Ref::new(project(n).as_str()), ());
    }
    for n in 0..CONVOYS {
        for promise in [0, 1] {
            world.durable.put(&Ref::new(change(n, promise).as_str()), true);
        }
    }
    world
}

fn submit(n: usize, promise: u32) -> Submit {
    let address = change(n, promise);
    let durable = Ref::new(address.as_str());
    Submit { promise: PromiseId(promise), by: seat("coder"), address, revision: "abc".into(), durable }
}

/// Admits `names` at the store's home and submits promise 0 of each.
fn seed(home: &mut Home<Convoy, StoreLog>, world: &MemoryConvoyWorld, names: impl Iterator<Item = usize>) {
    for n in names {
        let name = format!("convoy-{n}");
        home.create(&name, request(n), world, Timestamp(1)).expect("admitted");
        home.apply(&name, submit(n, 0), world, Timestamp(2)).expect("submitted");
    }
}

struct Measured {
    micros: f64,
    vm_steps: f64,
    fullscan_steps: f64,
    rows: f64,
}

/// `sql` run once per argument: the mean time, VM steps, full-scan steps and rows returned.
fn measure(conn: &Connection, sql: &str, arguments: &[String]) -> Measured {
    let mut statement = conn.prepare(sql).expect("prepare");
    let (mut steps, mut fullscan, mut rows) = (0i64, 0i64, 0usize);
    let started = Instant::now();
    for argument in arguments {
        statement.reset_status(StatementStatus::VmStep);
        statement.reset_status(StatementStatus::FullscanStep);
        rows += statement.query_map([argument], |_| Ok(())).expect("query").count();
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
        "{what:>34}: {:>8.1} µs/query {:>9.1} VM steps {:>8.1} full-scan steps {:>6.1} rows",
        m.micros, m.vm_steps, m.fullscan_steps, m.rows
    );
}

const OPEN_BY_PROJECT: &str = "SELECT c.id FROM convoy c \
     WHERE c.project = (SELECT id FROM strings WHERE domain = 'ad' AND s = ?1) AND c.state IN ('op') ORDER BY c.id";

/// The read path from a change-request event to its promise: the submissions naming the change
/// request, their promise's id and progress, in Open convoys.
const PROMISE_BY_SUBMISSION: &str = "SELECT s.convoy, p.id, p.progress FROM convoy_promises_submissions s \
     JOIN convoy c ON c.id = s.convoy \
     JOIN convoy_promises p ON p.convoy = s.convoy AND p.pos = s.promises_pos \
     WHERE s.address = (SELECT id FROM strings WHERE domain = 'ad' AND s = ?1) AND c.state IN ('op')";

/// A crew's or a view's read of one promise by its label (#3587 layer 6 labels ruling): within one
/// convoy, through the unique `by_label` index.
const PROMISE_BY_LABEL: &str = "SELECT p.id, p.progress FROM convoy_promises p \
     WHERE p.convoy = CAST(?1 AS INTEGER) AND p.label = (SELECT id FROM strings WHERE domain = 'pl' AND s = 'template/pr')";

// Behaviour (#3587 layer 4): the 10,000-convoy load, mostly replicas: the per-event cost of the
// engine's records edges through the store, and the reads of a project's open convoys and of the
// change-request read path, which the `by_address` index keeps to a few steps per query.
#[test]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
fn convoy_load() {
    let world = world();
    let (_remote_db, remote_conn) = connection("remote");
    let remote = Arc::new(Mutex::new(Store::open(remote_conn, "remote", convoy_registry().expect("registry")).expect("store")));
    let (local_db, local_conn) = connection("local");
    let local = Arc::new(Mutex::new(Store::open(local_conn, "local", convoy_registry().expect("registry")).expect("store")));

    let started = Instant::now();
    let mut remote_home = Home::<Convoy, StoreLog>::with_log("remote", StoreLog::new(remote.clone())).expect("declaration");
    seed(&mut remote_home, &world, (0..CONVOYS).filter(|n| n % LOCAL_ONE_IN != 0));
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
    let mut home = Home::<Convoy, StoreLog>::with_log("local", StoreLog::new(local.clone())).expect("declaration");
    let locals: Vec<usize> = (0..CONVOYS).filter(|n| n % LOCAL_ONE_IN == 0).collect();
    seed(&mut home, &world, locals.iter().copied());
    eprintln!(
        "{:>34}: {} convoys ({} local, {} replicated through {replicated} events) in {:.1?}",
        "seeded",
        CONVOYS,
        locals.len(),
        CONVOYS - locals.len(),
        started.elapsed()
    );

    let started = Instant::now();
    for n in &locals {
        home.apply(&format!("convoy-{n}"), submit(*n, 1), &world, Timestamp(3)).expect("submitted");
    }
    let submit_micros = started.elapsed().as_secs_f64() * 1e6 / locals.len() as f64;
    let started = Instant::now();
    for n in &locals {
        home.apply(&format!("convoy-{n}"), Keep::Observed { promise: PromiseId(0), submission: 1 }, &world, Timestamp(4)).expect("kept");
    }
    let keep_micros = started.elapsed().as_secs_f64() * 1e6 / locals.len() as f64;
    eprintln!("{:>34}: {submit_micros:>8.1} µs/event (a submission row, the progress, the log entry)", "submit, one transaction");
    eprintln!("{:>34}: {keep_micros:>8.1} µs/event (the progress, the log entry)", "observed keep, one transaction");

    let page_started = Instant::now();
    let mut paged = 0;
    for n in 0..PROJECTS {
        paged += local
            .lock()
            .expect("local")
            .read()
            .expect("read")
            .page(&ConvoySpec::open_by_project(&project(n)), None, 1_000)
            .expect("page")
            .ids
            .len();
    }
    assert_eq!(paged, CONVOYS, "every convoy is Open");
    eprintln!(
        "{:>34}: {:>8.1} µs/query ({} convoys each)",
        "open_by_project, store page",
        page_started.elapsed().as_secs_f64() * 1e6 / PROJECTS as f64,
        CONVOYS / PROJECTS
    );

    let started = Instant::now();
    for _ in 0..CONVOYS {
        std::hint::black_box(frozen_kinds());
    }
    let freeze_micros = started.elapsed().as_secs_f64() * 1e6 / CONVOYS as f64;
    eprintln!("{:>34}: {freeze_micros:>8.1} µs/admission (stock and charter kinds resolved, pr frozen)", "kinds resolve and freeze");
    let pr = catalog().get("pr").expect("pr").clone();
    let started = Instant::now();
    for n in 0..CONVOYS {
        let address = change(n, 0);
        assert_eq!(pr.holds(Rule::Kept, &address, "abc", &Merged), Some(true));
        assert_eq!(pr.holds(Rule::Ready, &address, "abc", &Merged), Some(false));
    }
    let judge_micros = started.elapsed().as_secs_f64() * 1e6 / CONVOYS as f64;
    eprintln!("{:>34}: {judge_micros:>8.1} µs/event (pr kept and ready rules)", "kinds judged");

    let read = Connection::open(&local_db.0).expect("read connection");
    let (objects, bytes): (i64, i64) = read
        .query_row("SELECT count(*), coalesce(sum(length(bytes)), 0) FROM objects", [], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("objects");
    assert_eq!(objects, 1, "every convoy shares the one frozen kinds document");
    eprintln!("{:>34}: {objects} object, {bytes} bytes for {CONVOYS} convoys", "frozen kinds stored");
    let projects: Vec<String> = (0..PROJECTS).map(|n| project(n).as_str().to_owned()).collect();
    report("open_by_project", &measure(&read, OPEN_BY_PROJECT, &projects));
    // Mostly replicas, as the events arrive: every tenth change request is a local convoy's.
    let changes: Vec<String> = (0..CONVOYS).step_by(7).map(|n| change(n, 0).as_str().to_owned()).collect();
    let indexed = measure(&read, PROMISE_BY_SUBMISSION, &changes);
    assert_eq!(indexed.rows, 1.0, "each change request names one promise");
    assert_eq!(indexed.fullscan_steps, 0.0, "the lookup scans no table");
    report("promise by submission, by_address", &indexed);
    let labels: i64 = read.query_row("SELECT count(*) FROM strings WHERE domain = 'pl'", [], |row| row.get(0)).expect("labels");
    assert_eq!(labels, 2, "each label is interned once, whatever the number of convoys");
    eprintln!("{:>34}: {labels} strings for {} promises", "promise labels interned", 2 * CONVOYS);
    let mut ids = read.prepare("SELECT id FROM convoy WHERE id % 7 = 0").expect("prepare");
    let convoys: Vec<String> = ids.query_map([], |row| row.get::<_, i64>(0)).expect("ids").map(|id| id.expect("id").to_string()).collect();
    let by_label = measure(&read, PROMISE_BY_LABEL, &convoys);
    assert_eq!(by_label.rows, 1.0, "one promise of each convoy holds the label");
    assert_eq!(by_label.fullscan_steps, 0.0, "the lookup scans no table");
    report("promise by label, by_label", &by_label);
    read.execute_batch("DROP INDEX convoy_promises_submissions_by_address").expect("drop");
    let scanned = measure(&read, PROMISE_BY_SUBMISSION, &changes[..changes.len().min(100)]);
    report("promise by submission, no index", &scanned);
}
