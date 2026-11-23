//! Convoys read from the engine's launches in the old Convoy shape (#3108, #4412 C): each launch
//! composed on each read from its record, its works' Vessel launches and their sessions, and its
//! home's notes at that home only; every write refused. A get, a listing and a watch of the same
//! records give the same version, and what changed since a position is named from the log.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use futures::StreamExt;

use chrono::{DateTime, TimeZone, Utc};
use flotilla_protocol::{LifecycleReading, PrincipalRef};
use flotilla_resources::{
    convoy::admission::AdmissionDocument,
    convoy_records::{
        ConvoyChange, ConvoyEnd, ConvoyListing, ConvoyMemory, ConvoyNotes, ConvoyRecord, ConvoyRecords, CrewNotes, Ending, UnreadableLaunch,
    },
    engine_records::{KindRecords, Since},
    held_joins::HeldJoins,
    lifecycle_reads::{ChangeCursor, CursorSince, LifecycleReads, ProvisioningKind},
    promises::{Promise, PromiseKind, PromiseSource, PromiseState},
    terminal_records::{
        TerminalChange, TerminalLaunchRequest, TerminalLaunchSource, TerminalObservations, TerminalRecord, TerminalRecords, TerminalRenewal,
    },
    vessel_records::{VesselChange, VesselLaunchRequest, VesselRecord, VesselRecords, VesselStep},
    workflow_template::{CrewSource, CrewSpec, Selector, VesselRequirement},
    Convoy, ConvoyAttention, ConvoyPhase, CrewWorkPhase, ReadWatchEvent, WorkPhase, WorkflowSnapshot,
};
use flotilla_store::{
    load_metrics::{measure, ReadCounts},
    ConvoyViews, EnginePort, ResourceBackend,
};
use flotilla_store_memory::InMemoryBackend;

const HERE: &str = "home-a";

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("time")
}

fn agent(role: &str) -> CrewSpec {
    CrewSpec::builder()
        .role(role.to_owned())
        .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
        .build()
}

fn launch(name: &str, home: &str) -> ConvoyRecord {
    let snapshot = WorkflowSnapshot {
        cascade: None,
        exit: None,
        turn_delivery: Default::default(),
        stall_nudges: Default::default(),
        supervision: None,
        vessels: vec![VesselRequirement::builder().name("work".to_owned()).crew(vec![agent("coder"), agent("reviewer")]).build()],
    };
    let promise = Promise {
        id: "template/pr".into(),
        vessel: "work".into(),
        role: "coder".into(),
        source: PromiseSource::Template,
        kind: PromiseKind::Pr,
        state: PromiseState::Open,
        retraction_reason: None,
        submissions: Vec::new(),
    };
    ConvoyRecord {
        name: name.into(),
        home: home.into(),
        version: 3,
        launch_of: name.split('#').next().expect("launch_of").into(),
        launch: 1,
        project: "flotilla".into(),
        workflow_ref: "review".into(),
        admission: AdmissionDocument {
            workflow_snapshot: snapshot,
            placement_decision: None,
            placement_policy: None,
            role: String::new(),
            ensure_admission: None,
            naming: None,
            dispatching_principal_ref: PrincipalRef { namespace: "flotilla".into(), name: "robert".into() },
            continuation: None,
            inputs: BTreeMap::new(),
            repositories: Vec::new(),
            r#ref: None,
            adopted_checkout_refs: BTreeMap::new(),
            subjects: Vec::new(),
            instruction: None,
            workflow_template: None,
            placement_snapshot: None,
            vessel_placements: BTreeMap::new(),
            brief_artifacts: false,
            adopted_checkout: None,
            provenance: BTreeMap::new(),
        },
        admitted_at: at(100),
        ended: None,
        retired: false,
        seats: BTreeMap::new(),
        promises: BTreeMap::from([("work".into(), BTreeMap::from([("coder".into(), vec![promise])]))]),
        subjects: Vec::new(),
        unlinked: Vec::new(),
    }
}

/// `name` held here, abandoned as "stopped", and retired when `retired`.
fn abandoned(name: &str, home: &str, retired: bool) -> ConvoyRecord {
    let mut record = launch(name, home);
    record.ended = Some(Ending::Held(ConvoyEnd::Abandoned { reason: "stopped".into() }, at(200)));
    record.retired = retired;
    record
}

fn session(vessel: &str, role: &str, reading: LifecycleReading) -> TerminalRecord {
    TerminalRecord {
        name: format!("terminal-{vessel}-{role}#1"),
        home: HERE.into(),
        environment: "env".into(),
        environment_home: HERE.into(),
        vessel: Some(vessel.into()),
        role: role.into(),
        pool: "cleat".into(),
        cwd: "/work".into(),
        env: BTreeMap::new(),
        source: TerminalLaunchSource::Tool { command: "true".into() },
        requested_at: at(110),
        since: at(111),
        reading,
        facts: None,
    }
}

/// The launch of `convoy`'s work `work`, held here, in `reading`.
fn vessel(convoy: &str, work: &str, reading: LifecycleReading) -> VesselRecord {
    let launch_of = flotilla_resources::vessel_resource_name(flotilla_resources::convoy::launch_of(convoy), work);
    VesselRecord {
        name: format!("{launch_of}#1"),
        home: HERE.into(),
        convoy: convoy.into(),
        convoy_home: HERE.into(),
        launch_of,
        host: "host-a".into(),
        requested_at: at(110),
        since: at(115),
        reading,
    }
}

fn ready_vessel(convoy: &str, work: &str) -> VesselRecord {
    vessel(convoy, work, LifecycleReading::Ready { identity: "env".into() })
}

/// What a write touched, by kind: one log for the three kinds, as a home's log holds them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Written {
    Convoy,
    Vessel,
    Terminal,
}

/// One home's log of writes: each entry's sequence is its place, from 1.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<(Written, String, String)>>>);

impl Log {
    fn write(&self, kind: Written, name: &str, home: &str) {
        self.0.lock().expect("log").push((kind, name.into(), home.into()));
    }

    fn head(&self) -> u64 {
        self.0.lock().expect("log").len() as u64
    }
}

/// A cursor over one kind's entries in the log, which reads what was written since a position
/// and is never passed.
struct LogCursor {
    log: Log,
    kind: Written,
}

impl<C> ChangeCursor<C> for LogCursor {
    fn pass(&mut self) -> Result<Vec<C>, String> {
        Ok(Vec::new())
    }

    fn position(&self) -> u64 {
        self.log.head()
    }

    fn read_through(&self) -> BTreeMap<String, u64> {
        BTreeMap::from([(HERE.to_owned(), self.log.head())])
    }

    fn since(&self, at: &BTreeMap<String, u64>) -> Result<CursorSince, String> {
        let from = at.get(HERE).copied().unwrap_or(0) as usize;
        let log = self.log.0.lock().expect("log");
        let written =
            log.iter().skip(from).filter(|(kind, ..)| *kind == self.kind).map(|(_, name, home)| (name.clone(), home.clone())).collect();
        Ok(CursorSince::Changed { written, through: BTreeMap::from([(HERE.to_owned(), log.len() as u64)]) })
    }
}

struct Convoys {
    records: Mutex<Vec<ConvoyRecord>>,
    memory: ConvoyMemory,
    log: Log,
    /// How many times a launch's record was read, by name or in a listing.
    read: AtomicUsize,
    /// Launches held that cannot be read, which a listing leaves out as it does retired ones.
    unreadable: Mutex<Vec<UnreadableLaunch>>,
    told: tokio::sync::broadcast::Sender<ConvoyChange>,
}

impl Convoys {
    fn new(records: Vec<ConvoyRecord>, log: Log) -> Self {
        Self {
            records: Mutex::new(records),
            memory: ConvoyMemory::default(),
            log,
            read: AtomicUsize::new(0),
            unreadable: Mutex::default(),
            told: tokio::sync::broadcast::channel(16).0,
        }
    }

    fn records(&self) -> Vec<ConvoyRecord> {
        self.records.lock().expect("records").clone()
    }

    /// Writes `record` in place of its launch's, logs it and tells the followers, as a committed
    /// edge does.
    fn commit(&self, record: ConvoyRecord) {
        let change = ConvoyChange::Changed { name: record.name.clone(), home: record.home.clone() };
        let mut records = self.records.lock().expect("records");
        records.retain(|held| (&held.name, &held.home) != (&record.name, &record.home));
        self.log.write(Written::Convoy, &record.name, &record.home);
        records.push(record);
        drop(records);
        let _ = self.told.send(change);
    }
}

impl ConvoyRecords for Convoys {
    fn home(&self) -> &str {
        HERE
    }

    fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
        Vec::new()
    }
    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<ConvoyRecord>, String> {
        self.read.fetch_add(1, Ordering::SeqCst);
        let home = home.unwrap_or(HERE);
        Ok(self.records().into_iter().find(|record| record.name == name && record.home == home))
    }
    fn named(&self, name: &str, home: Option<&str>) -> Result<ConvoyListing, String> {
        let records = match home {
            Some(home) => self.get(name, Some(home))?.into_iter().collect(),
            None => {
                let records: Vec<_> = self.records().into_iter().filter(|record| record.name == name).collect();
                self.read.fetch_add(records.len(), Ordering::SeqCst);
                records
            }
        };
        let unreadable = self.unreadable.lock().expect("unreadable").clone();
        let unreadable =
            unreadable.into_iter().filter(|launch| launch.name == name && home.is_none_or(|home| launch.home == home)).collect();
        Ok(ConvoyListing { records, unreadable })
    }
    fn newest(&self, of: &str, _project: &str, home: Option<&str>) -> Result<Option<ConvoyRecord>, String> {
        let home = home.unwrap_or(HERE);
        Ok(self.records().into_iter().filter(|record| record.launch_of == of && record.home == home).max_by_key(|record| record.launch))
    }
    fn holds(&self, _: &str, _: Option<&str>) -> Result<Option<flotilla_resources::convoy_records::LaunchHolds>, String> {
        unreachable!("the views read no holds")
    }
    fn newest_holding(&self, role: &str, project: Option<&str>) -> Result<Option<ConvoyRecord>, String> {
        let held = |record: &ConvoyRecord| {
            record.role() == role && !record.project.is_empty() && project.is_none_or(|project| record.project == project)
        };
        Ok(self.records().into_iter().filter(held).max_by_key(|record| record.admitted_at))
    }
    // A listing leaves retired launches out, as the engine's does.
    fn at(&self, home: &str) -> Result<ConvoyListing, String> {
        let records = self.records().into_iter().filter(|record| record.home == home && !record.retired).collect();
        Ok(ConvoyListing { records, unreadable: Vec::new() })
    }
    fn every(&self) -> Result<ConvoyListing, String> {
        let records: Vec<_> = self.records().into_iter().filter(|record| !record.retired).collect();
        self.read.fetch_add(records.len(), Ordering::SeqCst);
        Ok(ConvoyListing { records, unreadable: Vec::new() })
    }
    fn memory(&self) -> &ConvoyMemory {
        &self.memory
    }
    fn changes(&self) -> tokio::sync::broadcast::Receiver<ConvoyChange> {
        self.told.subscribe()
    }
    fn cursor(&self) -> Option<Box<dyn ChangeCursor<ConvoyChange>>> {
        Some(Box::new(LogCursor { log: self.log.clone(), kind: Written::Convoy }))
    }
}

struct Terminals {
    records: Mutex<Vec<TerminalRecord>>,
    observations: TerminalObservations,
    log: Log,
    told: tokio::sync::broadcast::Sender<TerminalChange>,
}

impl Terminals {
    fn new(records: Vec<TerminalRecord>, log: Log) -> Self {
        Self {
            records: Mutex::new(records),
            observations: TerminalObservations::default(),
            log,
            told: tokio::sync::broadcast::channel(16).0,
        }
    }

    fn records(&self) -> Vec<TerminalRecord> {
        self.records.lock().expect("terminals").clone()
    }

    fn commit(&self, record: TerminalRecord) {
        let change = TerminalChange::Changed { name: record.name.clone(), home: record.home.clone() };
        let mut records = self.records.lock().expect("terminals");
        records.retain(|held| (&held.name, &held.home) != (&record.name, &record.home));
        self.log.write(Written::Terminal, &record.name, &record.home);
        records.push(record);
        drop(records);
        let _ = self.told.send(change);
    }
}

impl TerminalRecords for Terminals {
    fn home(&self) -> &str {
        HERE
    }

    fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
        Vec::new()
    }
    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<TerminalRecord>, String> {
        let home = home.unwrap_or(HERE);
        Ok(self.records().into_iter().find(|record| record.name == name && record.home == home))
    }
    fn named(&self, name: &str) -> Result<Vec<TerminalRecord>, String> {
        let newest = flotilla_resources::terminal_records::newest_launches(
            self.records().into_iter().filter(|record| record.name == name || record.session_name() == name),
        );
        Ok(newest.into_values().collect())
    }
    fn at(&self, home: &str) -> Result<Vec<TerminalRecord>, String> {
        Ok(self.records().into_iter().filter(|record| record.home == home).collect())
    }
    fn live(&self) -> Result<Vec<TerminalRecord>, String> {
        Ok(self.records().into_iter().filter(|record| record.home == HERE && !record.ended()).collect())
    }
    fn every(&self) -> Result<Vec<TerminalRecord>, String> {
        Ok(self.records())
    }
    fn of_vessel(&self, vessel: &str) -> Result<Vec<TerminalRecord>, String> {
        Ok(self.records().into_iter().filter(|record| record.vessel.as_deref() == Some(vessel)).collect())
    }
    fn of_environment(&self, _environment: &str, _home: &str) -> Result<Vec<TerminalRecord>, String> {
        Ok(Vec::new())
    }
    fn request(&self, _request: TerminalLaunchRequest) -> Result<String, String> {
        Err("not requested here".into())
    }
    fn renew(&self, _session: &str) -> Result<TerminalRenewal, String> {
        Err("not renewed here".into())
    }
    fn observations(&self) -> &TerminalObservations {
        &self.observations
    }
    fn changes(&self) -> tokio::sync::broadcast::Receiver<TerminalChange> {
        self.told.subscribe()
    }
    fn cursor(&self) -> Option<Box<dyn ChangeCursor<TerminalChange>>> {
        Some(Box::new(LogCursor { log: self.log.clone(), kind: Written::Terminal }))
    }
}

struct Vessels {
    records: Mutex<Vec<VesselRecord>>,
    log: Log,
}

impl Vessels {
    fn records(&self) -> Vec<VesselRecord> {
        self.records.lock().expect("vessels").clone()
    }

    fn commit(&self, record: VesselRecord) {
        let mut records = self.records.lock().expect("vessels");
        records.retain(|held| (&held.name, &held.home) != (&record.name, &record.home));
        self.log.write(Written::Vessel, &record.name, &record.home);
        records.push(record);
    }
}

impl VesselRecords for Vessels {
    fn home(&self) -> &str {
        HERE
    }

    fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
        Vec::new()
    }
    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<VesselRecord>, String> {
        let home = home.unwrap_or(HERE);
        Ok(self.records().into_iter().find(|record| record.name == name && record.home == home))
    }
    fn launches(&self, convoy: &str, convoy_home: &str, launch_of: &str, home: Option<&str>) -> Result<Vec<VesselRecord>, String> {
        let home = home.unwrap_or(HERE);
        Ok(self
            .records()
            .into_iter()
            .filter(|record| {
                record.convoy == convoy && record.convoy_home == convoy_home && record.launch_of == launch_of && record.home == home
            })
            .collect())
    }
    fn at(&self, home: &str) -> Result<Vec<VesselRecord>, String> {
        Ok(self.records().into_iter().filter(|record| record.home == home).collect())
    }
    fn every(&self) -> Result<Vec<VesselRecord>, String> {
        Ok(self.records())
    }
    fn request(&self, _request: &VesselLaunchRequest) -> Result<String, String> {
        Err("not requested here".into())
    }
    fn step(&self, _name: &str, _step: &VesselStep) -> Result<VesselRecord, String> {
        Err("not stepped here".into())
    }
    fn cursor(&self) -> Option<Box<dyn ChangeCursor<VesselChange>>> {
        Some(Box::new(LogCursor { log: self.log.clone(), kind: Written::Vessel }))
    }
}

struct Reads {
    convoys: Convoys,
    vessels: Vessels,
    terminals: Terminals,
    joins: HeldJoins,
}

impl Reads {
    fn new(convoys: Vec<ConvoyRecord>, vessels: Vec<VesselRecord>, terminals: Vec<TerminalRecord>) -> Arc<Self> {
        let log = Log::default();
        Arc::new(Self {
            convoys: Convoys::new(convoys, log.clone()),
            vessels: Vessels { records: Mutex::new(vessels), log: log.clone() },
            terminals: Terminals::new(terminals, log),
            joins: HeldJoins::default(),
        })
    }
}

impl LifecycleReads for Reads {
    fn home(&self) -> Result<&str, String> {
        Ok(HERE)
    }
    fn try_reading(&self, _kind: ProvisioningKind, _name: &str, _origin: Option<&str>) -> Result<Option<LifecycleReading>, String> {
        Ok(None)
    }
    fn terminals(&self) -> Result<&dyn TerminalRecords, String> {
        Ok(&self.terminals)
    }
    fn vessels(&self) -> Result<&dyn VesselRecords, String> {
        Ok(&self.vessels)
    }
    fn convoys(&self) -> Result<&dyn ConvoyRecords, String> {
        Ok(&self.convoys)
    }
    fn joins(&self) -> Result<&HeldJoins, String> {
        Ok(&self.joins)
    }
}

/// Views over launches `names`, held here, joined with nothing.
fn views_of(names: &[&str]) -> (Arc<Reads>, ConvoyViews) {
    let reads = Reads::new(names.iter().map(|name| launch(name, HERE)).collect(), Vec::new(), Vec::new());
    let views = ConvoyViews::new("flotilla", reads.clone());
    (reads, views)
}

/// `reads` served as a backend's Convoy kind.
fn installed(reads: &Arc<Reads>) -> ResourceBackend {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.serve_engine_in("flotilla");
    Arc::new(ConvoyViews::new("flotilla", reads.clone())).install(&backend);
    backend
}

/// Each launch's version as the engine port lists it, by name.
async fn versions(views: &ConvoyViews) -> BTreeMap<String, u64> {
    KindRecords::list(views).await.expect("list").into_iter().map(|record| (record.name, record.version)).collect()
}

fn attention(notes: &mut ConvoyNotes) {
    notes.attention = Some(ConvoyAttention { source: "turn".into(), reason: "queued".into(), raised_at: at(120) });
}

// Behaviour: a launch reads as a Convoy named by its launch, its work Running on its ready Vessel
// launch, its eager seat working on its live session and its latent one once its home delivered
// its task. Its home's notes show at that home; another home's launch reads without them.
#[tokio::test]
async fn a_launch_reads_with_its_vessels_sessions_and_home_notes() {
    let vessel = ready_vessel("fix-it#1", "work");
    let reads = Reads::new(
        vec![launch("fix-it#1", HERE), launch("other#1", "home-b")],
        vec![vessel.clone()],
        vec![
            session(&vessel.name, "coder", LifecycleReading::Ready { identity: "s1".into() }),
            session(&vessel.name, "reviewer", LifecycleReading::Ready { identity: "s2".into() }),
        ],
    );
    let views = ConvoyViews::new("flotilla", reads.clone());

    let convoy = KindRecords::get(&views, "fix-it#1").await.expect("read").expect("held");
    let status = convoy.status.expect("status");
    assert_eq!(status.phase, ConvoyPhase::Active);
    assert_eq!(status.work["work"].phase, WorkPhase::Running);
    assert_eq!((status.work["work"].started_at, status.work["work"].ready_at), (Some(at(110)), Some(at(115))));
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending, "a latent seat waits for its task");

    let mut ticks = KindRecords::changes(&views).await.expect("follow");
    let (_, changed) = reads.convoys.memory.update("fix-it#1", |notes: &mut ConvoyNotes| {
        attention(notes);
        notes.crew.entry("work".into()).or_default().insert("reviewer".into(), CrewNotes { task_assigned: true, ..CrewNotes::default() });
    });
    assert!(changed);
    tokio::time::timeout(Duration::from_secs(5), ticks.next()).await.expect("ticked").expect("following");
    let status = KindRecords::get(&views, "fix-it#1").await.expect("read").expect("held").status.expect("status");
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Working);
    assert_eq!(status.attention.map(|attention| attention.reason).as_deref(), Some("queued"));

    reads.convoys.memory.update("other#1", attention);
    let other = KindRecords::copies(&views, "other#1").await.expect("read");
    assert_eq!(other.iter().map(|copy| copy.home.as_str()).collect::<Vec<_>>(), ["home-b"]);
    assert!(other[0].status.as_ref().expect("status").attention.is_none(), "another home's launch reads without this home's notes");
    assert_eq!(KindRecords::sources(&views).await.expect("sources").len(), 2);
}

// Behaviour: the views write nothing; a convoy is admitted, edged and ended on the engine.
#[tokio::test]
async fn the_views_refuse_writes() {
    let (_, views) = views_of(&["fix-it#1"]);
    let none = BTreeMap::new();
    let held = KindRecords::get(&views, "fix-it#1").await.expect("get").expect("held");
    assert!(views.apply("fix-it#1", None, &none, &none, &held.spec).await.is_err());
    assert!(views.set_status("fix-it#1", held.version, &held.status.expect("status")).await.is_err());
    assert!(KindRecords::delete(&views, "fix-it#1").await.is_err());
    assert_eq!(KindRecords::list(&views).await.expect("list").len(), 1);
}

// Behaviour (#4412 C): a launch's version is what its view shows. A note on one launch moves that
// launch's version alone; a launch whose record is written again showing the same keeps it.
#[tokio::test]
async fn a_version_moves_only_with_what_its_launch_shows() {
    let (reads, views) = views_of(&["fix-it#1", "other#1"]);
    let before = versions(&views).await;

    reads.convoys.commit(launch("other#1", HERE));
    assert!(reads.convoys.memory.update("fix-it#1", attention).1);
    let after = versions(&views).await;
    assert_ne!(after["fix-it#1"], before["fix-it#1"], "the noted launch changed");
    assert_eq!(after["other#1"], before["other#1"], "a launch written again that shows the same keeps its version");
}

// Behaviour (#4412 C): a get, a listing and a watch of the same records agree on a launch's
// version, as a reader comparing a get against what it watched relies on.
#[tokio::test(start_paused = true)]
async fn a_get_a_listing_and_a_watch_agree_on_a_version() {
    let vessel = ready_vessel("fix-it#1", "work");
    let reads = Reads::new(
        vec![launch("fix-it#1", HERE)],
        vec![vessel.clone()],
        vec![session(&vessel.name, "coder", LifecycleReading::Ready { identity: "s1".into() })],
    );
    let backend = installed(&reads);
    let convoys = backend.including_replicas::<Convoy>("flotilla");
    let got = convoys.get("fix-it#1").await.expect("get").object.metadata.resource_version;
    let listed = convoys.list().await.expect("list").items.remove(0).object.metadata.resource_version;
    assert_eq!(got, listed);
    let mut watch = convoys.watch().await.expect("watch");

    // A session ending changes what the launch shows: the watch, a get and a listing read the same
    // new version.
    reads.terminals.commit(session(&vessel.name, "coder", LifecycleReading::Released));
    let next = match tokio::time::timeout(Duration::from_secs(5), watch.next()).await.expect("event").expect("watching").expect("event") {
        ReadWatchEvent::Modified(read) => read.object.metadata.resource_version,
        other => panic!("unexpected {other:?}"),
    };
    assert_ne!(next, got);
    assert_eq!(convoys.list().await.expect("list").items.remove(0).object.metadata.resource_version, next);
    assert_eq!(convoys.get("fix-it#1").await.expect("get").object.metadata.resource_version, next);
}

// Behaviour (#4412 C): what changed since a position names the launches whose records were
// written, and those whose Vessel launches or sessions were, each joined back to its launch by
// key, and those whose notes this home changed; a position taken after them names none.
#[tokio::test]
async fn changes_since_a_position_name_each_launch_a_write_touched() {
    let fixed = ready_vessel("fix-it#1", "work");
    let reads = Reads::new(
        vec![launch("fix-it#1", HERE), launch("other#1", HERE), launch("noted#1", HERE), launch("quiet#1", HERE)],
        vec![fixed.clone(), ready_vessel("other#1", "work")],
        vec![session(&fixed.name, "coder", LifecycleReading::Ready { identity: "s1".into() })],
    );
    let views = ConvoyViews::new("flotilla", reads.clone());
    let named = |since: Since| match since {
        Since::Changed { names, through } => (names.into_iter().collect::<Vec<_>>(), through),
        Since::Expired(reason) => panic!("expired: {reason}"),
    };

    let position = views.position().await.expect("position");
    reads.terminals.commit(session(&fixed.name, "coder", LifecycleReading::Released));
    reads.vessels.commit(vessel("other#1", "work", LifecycleReading::Released));
    reads.convoys.memory.update("noted#1", attention);
    let (names, through) = named(views.changed_since(&position).await.expect("since"));
    assert_eq!(names, ["fix-it#1", "noted#1", "other#1"], "the session's launch, the Vessel's launch and the noted launch");

    let (names, _) = named(views.changed_since(&through).await.expect("since"));
    assert!(names.is_empty(), "nothing written after: {names:?}");
}

/// The launches converted by a read of one launch by name, with `others` unrelated launches held:
/// its copies, then another home's launch read where this home holds none.
async fn converted_by_one_read(others: usize) -> (u64, u64) {
    let mut launches: Vec<_> = (0..others).map(|n| launch(&format!("other-{n}#1"), HERE)).collect();
    launches.extend([launch("wanted#1", HERE), launch("wanted#1", "home-b"), launch("remote#1", "home-b")]);
    let reads = Reads::new(launches, Vec::new(), Vec::new());
    let views = ConvoyViews::new("flotilla", reads);
    assert_eq!(views.sources().await.expect("listing").len(), others + 3);

    let converted = |reads: &ReadCounts| reads.get("Convoy").map_or(0, |counts| counts.converted);
    let (copies, reads) = measure(views.copies("wanted#1")).await;
    let copies = copies.expect("copies");
    assert_eq!(copies.iter().map(|copy| copy.home.as_str()).collect::<Vec<_>>(), [HERE, "home-b"]);
    let copies_cost = converted(&reads);

    let (remote, reads) = measure(KindRecords::get(&views, "remote#1")).await;
    assert_eq!(remote.expect("get").expect("held at home-b").home, "home-b");
    (copies_cost, converted(&reads))
}

// Behaviour: a read of one launch by name, as a generic Convoy get reads it, converts only that
// name's launches into objects, however many other launches are held (#3108 read-cost audit).
#[tokio::test]
async fn a_read_by_name_converts_only_that_launch() {
    let small = converted_by_one_read(10).await;
    let large = converted_by_one_read(400).await;
    assert_eq!(small, (2, 1), "the copies convert both homes' launches; a remote get converts one");
    assert_eq!(large, small, "the cost does not grow with the launches held");
}

// Behaviour (#3108 retirement): a retired launch still reads by name through the generic Convoy
// reads with how it ended, and a listing leaves it out.
#[tokio::test]
async fn a_retired_launch_reads_by_name_with_its_ending() {
    let reads = Reads::new(vec![abandoned("retired#1", HERE, true), launch("open#1", HERE)], Vec::new(), Vec::new());
    let backend = installed(&reads);
    let convoys = backend.using::<Convoy>("flotilla");

    let names: Vec<_> = convoys.list().await.expect("list").items.into_iter().map(|convoy| convoy.metadata.name).collect();
    assert_eq!(names, ["open#1"], "a listing leaves a retired launch out");
    let read = convoys.get("retired#1").await.expect("a retired launch reads by name");
    let status = read.status.expect("status");
    assert_eq!(status.phase, ConvoyPhase::Abandoned);
    assert_eq!(status.message.as_deref(), Some("stopped"));
    let copies = backend.including_replicas::<Convoy>("flotilla").get_all("retired#1").await.expect("copies").items;
    assert_eq!(copies.len(), 1, "every copy of the name reads it too");
}

// Behaviour (#3108): a read of every copy of a name fails nothing because one copy cannot be read:
// that copy reads as unreadable, with why, beside the others.
#[tokio::test]
async fn a_read_by_name_gives_an_unreadable_copy_beside_the_others() {
    let reads = Reads::new(vec![abandoned("gone#1", HERE, true), launch("open#1", HERE)], Vec::new(), Vec::new());
    reads.convoys.unreadable.lock().expect("unreadable").push(UnreadableLaunch {
        name: "gone#1".into(),
        home: "home-b".into(),
        version: 4,
        launch_of: "gone".into(),
        launch: 1,
        project: "flotilla".into(),
        workflow_ref: "review".into(),
        created_at: at(100),
        reason: "its end is not held here".into(),
    });
    let backend = installed(&reads);

    let copies = backend.including_replicas::<Convoy>("flotilla").get_all("gone#1").await.expect("no copy fails the read").items;
    let read: BTreeMap<_, _> = copies
        .iter()
        .map(|copy| {
            let unreadable = copy.object.metadata.annotations.get(flotilla_resources::CONVOY_UNREADABLE_ANNOTATION).cloned();
            let local = matches!(copy.provenance, flotilla_resources::ResourceProvenance::Local);
            (local, (copy.object.status.as_ref().map(|status| status.phase), unreadable))
        })
        .collect();
    assert_eq!(
        read,
        BTreeMap::from([(true, (Some(ConvoyPhase::Abandoned), None)), (false, (None, Some("its end is not held here".to_owned()))),]),
        "this home's copy with its ending, and home-b's unreadable with why"
    );
}

// Behaviour (#3108): a read by name shows every home's launch of the name, retired or not: a live
// launch at one home and a retired one of the same name at another read side by side.
#[tokio::test]
async fn a_read_by_name_gives_a_live_and_a_retired_launch_of_the_name() {
    let reads = Reads::new(vec![launch("shared#1", HERE), abandoned("shared#1", "home-b", true)], Vec::new(), Vec::new());
    let backend = installed(&reads);
    let convoys = backend.including_replicas::<Convoy>("flotilla");
    assert_eq!(convoys.list().await.expect("list").items.len(), 1, "the listing holds the open launch alone");

    let copies = convoys.get_all("shared#1").await.expect("every home's launch").items;
    let read: BTreeMap<_, _> = copies
        .iter()
        .map(|copy| {
            let local = matches!(copy.provenance, flotilla_resources::ResourceProvenance::Local);
            (local, copy.object.status.as_ref().map(|status| status.phase))
        })
        .collect();
    assert_eq!(
        read,
        BTreeMap::from([(true, Some(ConvoyPhase::Active)), (false, Some(ConvoyPhase::Abandoned))]),
        "this home's live launch, and home-b's retired one with its ending"
    );
}

/// The next event a watcher reads: its launch, and its phase and message, or `None` where it left.
/// Its bound is on paused time, which moves only while nothing the views do is under way.
async fn next_event<E: std::fmt::Debug>(
    watch: &mut (impl futures::Stream<Item = Result<ReadWatchEvent<Convoy>, E>> + Unpin),
) -> (String, Option<(ConvoyPhase, Option<String>)>) {
    let event = tokio::time::timeout(Duration::from_secs(5), watch.next()).await.expect("an event").expect("watching").expect("event");
    match event {
        ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read) => {
            let status = read.object.status.expect("status");
            (read.object.metadata.name, Some((status.phase, status.message)))
        }
        ReadWatchEvent::Deleted(read) => (read.object.metadata.name, None),
        ReadWatchEvent::DeletedByName { tombstone, .. } => (tombstone.name, None),
    }
}

// Behaviour (#3108 retirement): a launch that ends and then retires is read by its watcher with
// how it ended, then leaves; a listing holds nothing of it after, and a read by name still gives
// its ending.
#[tokio::test(start_paused = true)]
async fn a_launch_that_ends_then_retires_is_told_ended_then_leaves() {
    let reads = Reads::new(vec![launch("open#1", HERE)], Vec::new(), Vec::new());
    let backend = installed(&reads);
    let convoys = backend.including_replicas::<Convoy>("flotilla");
    let mut watch = convoys.watch().await.expect("watch");

    reads.convoys.commit(launch("fix-it#1", HERE));
    assert_eq!(next_event(&mut watch).await, ("fix-it#1".to_owned(), Some((ConvoyPhase::Active, None))));
    reads.convoys.commit(abandoned("fix-it#1", HERE, false));
    assert_eq!(next_event(&mut watch).await, ("fix-it#1".to_owned(), Some((ConvoyPhase::Abandoned, Some("stopped".to_owned())))));
    reads.convoys.commit(abandoned("fix-it#1", HERE, true));
    assert_eq!(next_event(&mut watch).await, ("fix-it#1".to_owned(), None), "it leaves once retired");

    let names: Vec<_> = convoys.list().await.expect("list").items.into_iter().map(|read| read.object.metadata.name).collect();
    assert_eq!(names, ["open#1"]);
    let read = backend.using::<Convoy>("flotilla").get("fix-it#1").await.expect("read by name");
    assert_eq!(read.status.expect("status").phase, ConvoyPhase::Abandoned);
}
