//! Convoys read from the engine's launches in the old Convoy shape (#3108): each launch joined
//! with its works' Vessel launches and their sessions, its home's notes at that home only, and
//! every write refused.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};

use futures::StreamExt;

use chrono::{DateTime, TimeZone, Utc};
use flotilla_protocol::{LifecycleReading, PrincipalRef};
use flotilla_resources::{
    convoy::admission::AdmissionDocument,
    convoy_records::{ConvoyChange, ConvoyEnd, ConvoyListing, ConvoyMemory, ConvoyNotes, ConvoyRecord, ConvoyRecords, CrewNotes},
    engine_records::KindRecords,
    held_joins::HeldJoins,
    lifecycle_reads::{ChangeCursor, CursorSince, LifecycleReads, ProvisioningKind},
    promises::{Promise, PromiseKind, PromiseSource, PromiseState},
    terminal_records::{
        TerminalChange, TerminalLaunchRequest, TerminalLaunchSource, TerminalObservations, TerminalRecord, TerminalRecords, TerminalRenewal,
    },
    vessel_records::{VesselChange, VesselLaunchRequest, VesselRecord, VesselRecords, VesselStep},
    workflow_template::{CrewSource, CrewSpec, Selector, VesselRequirement},
    Convoy, ConvoyAttention, ConvoyPhase, CrewWorkPhase, WorkPhase, WorkflowSnapshot,
};
use flotilla_store::{
    live_convoys_query,
    load_metrics::{measure, ReadCounts},
    ConvoyViews, EnginePort, ResourceBackend, StoreQuery,
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
        seats: BTreeMap::new(),
        promises: BTreeMap::from([("work".into(), BTreeMap::from([("coder".into(), vec![promise])]))]),
        subjects: Vec::new(),
        unlinked: Vec::new(),
    }
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

/// A cursor over records that never change once read.
struct Unchanging;

impl<C> ChangeCursor<C> for Unchanging {
    fn pass(&mut self) -> Result<Vec<C>, String> {
        Ok(Vec::new())
    }

    fn position(&self) -> u64 {
        0
    }

    fn read_through(&self) -> BTreeMap<String, u64> {
        BTreeMap::new()
    }

    fn since(&self, _: &BTreeMap<String, u64>) -> Result<CursorSince, String> {
        Ok(CursorSince::Changed { written: Vec::new(), through: BTreeMap::new() })
    }
}

/// A gate a blocking read waits at while it is closed, telling `reaching` that it waits.
#[derive(Default)]
struct Gate {
    closed: Mutex<bool>,
    opened: Condvar,
    reaching: tokio::sync::Notify,
}

impl Gate {
    fn set(&self, closed: bool) {
        *self.closed.lock().expect("gate") = closed;
        self.opened.notify_all();
    }

    fn pass(&self) {
        let closed = self.closed.lock().expect("gate");
        if *closed {
            self.reaching.notify_one();
        }
        drop(self.opened.wait_while(closed, |closed| *closed).expect("gate"));
    }
}

/// A cursor that passes the changes a test writes into it, each once, each moving it on, after
/// its gate; while failing is set, its next pass fails instead and clears it.
struct Written {
    changes: Arc<Mutex<Vec<ConvoyChange>>>,
    passed: u64,
    gate: Arc<Gate>,
    failing: Arc<AtomicBool>,
}

impl ChangeCursor<ConvoyChange> for Written {
    fn pass(&mut self) -> Result<Vec<ConvoyChange>, String> {
        self.gate.pass();
        if self.failing.swap(false, Ordering::SeqCst) {
            return Err("the log did not read".into());
        }
        let changes = std::mem::take(&mut *self.changes.lock().expect("written changes"));
        self.passed += changes.len() as u64;
        Ok(changes)
    }

    fn position(&self) -> u64 {
        self.passed
    }

    fn read_through(&self) -> BTreeMap<String, u64> {
        BTreeMap::new()
    }

    fn since(&self, _: &BTreeMap<String, u64>) -> Result<CursorSince, String> {
        Ok(CursorSince::Expired("the written changes hold no log".into()))
    }
}

struct Convoys {
    records: Mutex<Vec<ConvoyRecord>>,
    memory: ConvoyMemory,
    /// How many times a launch's record was read.
    read: AtomicUsize,
    /// How many times every launch was listed.
    listed: AtomicUsize,
    /// The changes the cursor passes next; no follower is told of them, so only a read finds them.
    written: Arc<Mutex<Vec<ConvoyChange>>>,
    /// Where the cursor waits as it passes.
    passing: Arc<Gate>,
    /// The cursor's next pass fails.
    failing: Arc<AtomicBool>,
    /// While closed, a launch's record read waits, after it tells `reading`.
    gate: (Mutex<bool>, Condvar),
    reading: tokio::sync::Notify,
}

impl Convoys {
    fn new(records: Vec<ConvoyRecord>) -> Self {
        Self {
            records: Mutex::new(records),
            memory: ConvoyMemory::default(),
            read: AtomicUsize::new(0),
            listed: AtomicUsize::new(0),
            written: Arc::default(),
            passing: Arc::default(),
            failing: Arc::default(),
            gate: (Mutex::new(false), Condvar::new()),
            reading: tokio::sync::Notify::new(),
        }
    }

    fn records(&self) -> Vec<ConvoyRecord> {
        self.records.lock().expect("records").clone()
    }

    fn set_gate(&self, closed: bool) {
        *self.gate.0.lock().expect("gate") = closed;
        self.gate.1.notify_all();
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
        self.reading.notify_one();
        let closed = self.gate.0.lock().expect("gate");
        drop(self.gate.1.wait_while(closed, |closed| *closed).expect("gate"));
        let home = home.unwrap_or(HERE);
        Ok(self.records().into_iter().find(|record| record.name == name && record.home == home))
    }
    fn newest(&self, of: &str, _project: &str, home: Option<&str>) -> Result<Option<ConvoyRecord>, String> {
        let home = home.unwrap_or(HERE);
        Ok(self.records().into_iter().filter(|record| record.launch_of == of && record.home == home).max_by_key(|record| record.launch))
    }
    fn at(&self, home: &str) -> Result<ConvoyListing, String> {
        Ok(ConvoyListing { records: self.records().into_iter().filter(|record| record.home == home).collect(), unreadable: Vec::new() })
    }
    fn every(&self) -> Result<ConvoyListing, String> {
        let records = self.records();
        self.read.fetch_add(records.len(), Ordering::SeqCst);
        self.listed.fetch_add(1, Ordering::SeqCst);
        Ok(ConvoyListing { records, unreadable: Vec::new() })
    }
    fn memory(&self) -> &ConvoyMemory {
        &self.memory
    }
    fn cursor(&self) -> Option<Box<dyn ChangeCursor<ConvoyChange>>> {
        Some(Box::new(Written {
            changes: Arc::clone(&self.written),
            passed: 0,
            gate: Arc::clone(&self.passing),
            failing: Arc::clone(&self.failing),
        }))
    }
}

struct Terminals {
    records: Vec<TerminalRecord>,
    observations: TerminalObservations,
}

impl TerminalRecords for Terminals {
    fn home(&self) -> &str {
        HERE
    }

    fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
        Vec::new()
    }
    fn get(&self, name: &str, _home: Option<&str>) -> Result<Option<TerminalRecord>, String> {
        Ok(self.records.iter().find(|record| record.name == name).cloned())
    }
    fn at(&self, home: &str) -> Result<Vec<TerminalRecord>, String> {
        Ok(self.records.iter().filter(|record| record.home == home).cloned().collect())
    }
    fn every(&self) -> Result<Vec<TerminalRecord>, String> {
        Ok(self.records.clone())
    }
    fn of_vessel(&self, vessel: &str) -> Result<Vec<TerminalRecord>, String> {
        Ok(self.records.iter().filter(|record| record.vessel.as_deref() == Some(vessel)).cloned().collect())
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
    fn cursor(&self) -> Option<Box<dyn ChangeCursor<TerminalChange>>> {
        Some(Box::new(Unchanging))
    }
}

struct Vessels {
    records: Vec<VesselRecord>,
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
        Ok(self.records.iter().find(|record| record.name == name && record.home == home).cloned())
    }
    fn launches(&self, convoy: &str, convoy_home: &str, launch_of: &str, home: Option<&str>) -> Result<Vec<VesselRecord>, String> {
        let home = home.unwrap_or(HERE);
        Ok(self
            .records
            .iter()
            .filter(|record| {
                record.convoy == convoy && record.convoy_home == convoy_home && record.launch_of == launch_of && record.home == home
            })
            .cloned()
            .collect())
    }
    fn at(&self, home: &str) -> Result<Vec<VesselRecord>, String> {
        Ok(self.records.iter().filter(|record| record.home == home).cloned().collect())
    }
    fn every(&self) -> Result<Vec<VesselRecord>, String> {
        Ok(self.records.clone())
    }
    fn request(&self, _request: &VesselLaunchRequest) -> Result<String, String> {
        Err("not requested here".into())
    }
    fn step(&self, _name: &str, _step: &VesselStep) -> Result<VesselRecord, String> {
        Err("not stepped here".into())
    }
    fn cursor(&self) -> Option<Box<dyn ChangeCursor<VesselChange>>> {
        Some(Box::new(Unchanging))
    }
}

/// The ready launch of `convoy`'s work `work`, held here.
fn ready_vessel(convoy: &str, work: &str) -> VesselRecord {
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
        reading: LifecycleReading::Ready { identity: "env".into() },
    }
}

struct Reads {
    convoys: Convoys,
    vessels: Vessels,
    terminals: Terminals,
    joins: HeldJoins,
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

// Behaviour: a launch reads as a Convoy named by its launch, its work Running on its ready
// Vessel launch, its eager seat working on its live session and its latent one once its home delivered
// its task. Its home's notes show at that home; another home's launch reads without them. The
// views are held: a read reads no record, and a change reads again only the launch it touches.
#[tokio::test]
async fn a_launch_reads_with_its_vessels_sessions_and_home_notes() {
    let vessel = ready_vessel("fix-it#1", "work");
    let reads = Arc::new(Reads {
        joins: HeldJoins::default(),
        convoys: Convoys::new(vec![launch("fix-it#1", HERE), launch("other#1", "home-b")]),
        terminals: Terminals {
            records: vec![
                session(&vessel.name, "coder", LifecycleReading::Ready { identity: "s1".into() }),
                session(&vessel.name, "reviewer", LifecycleReading::Ready { identity: "s2".into() }),
            ],
            observations: TerminalObservations::default(),
        },
        vessels: Vessels { records: vec![vessel.clone()] },
    });
    let views = ConvoyViews::new("flotilla", reads.clone());

    let convoy = views.get("fix-it#1").await.expect("read");
    let status = convoy.status.expect("status");
    assert_eq!(status.phase, ConvoyPhase::Active);
    assert_eq!(status.work["work"].phase, WorkPhase::Running);
    assert_eq!((status.work["work"].started_at, status.work["work"].ready_at), (Some(at(110)), Some(at(115))));
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending, "a latent seat waits for its task");

    // Reads are of the views held: reading again reads no record.
    let read = reads.convoys.read.load(Ordering::SeqCst);
    views.get("fix-it#1").await.expect("read again");
    assert_eq!(reads.convoys.read.load(Ordering::SeqCst), read, "a read recomposes nothing");

    // A change recomposes the launch it touches, then ticks the watchers.
    let mut ticks = KindRecords::changes(&views).await.expect("follow");
    let (_, changed) = reads.convoys.memory.update("fix-it#1", |notes: &mut ConvoyNotes| {
        notes.attention = Some(ConvoyAttention { source: "turn".into(), reason: "queued".into(), raised_at: at(120) });
        notes.crew.entry("work".into()).or_default().insert("reviewer".into(), CrewNotes { task_assigned: true, ..CrewNotes::default() });
    });
    assert!(changed);
    tokio::time::timeout(Duration::from_secs(5), ticks.next()).await.expect("ticked").expect("following");
    assert_eq!(reads.convoys.read.load(Ordering::SeqCst), read + 1, "only the changed launch is read again");
    let status = views.get("fix-it#1").await.expect("read").status.expect("status");
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Working);
    assert_eq!(status.attention.map(|attention| attention.reason).as_deref(), Some("queued"));

    reads.convoys.memory.update("other#1", |notes: &mut ConvoyNotes| {
        notes.attention = Some(ConvoyAttention { source: "turn".into(), reason: "not here".into(), raised_at: at(120) });
    });
    tokio::time::timeout(Duration::from_secs(5), ticks.next()).await.expect("ticked").expect("following");
    let other = views.find("other#1", Some("home-b")).await.expect("read").expect("held");
    assert!(other.object.status.expect("status").attention.is_none(), "another home's launch reads without this home's notes");
    assert_eq!(views.sources().await.expect("sources").len(), 2);
    assert_eq!(views.here().await.expect("here").len(), 1);
}

// Behaviour: the views write nothing; a convoy is admitted, edged and ended on the engine.
#[tokio::test]
async fn the_views_refuse_writes() {
    let reads = Arc::new(Reads {
        joins: HeldJoins::default(),
        convoys: Convoys::new(vec![launch("fix-it#1", HERE)]),
        terminals: Terminals { records: Vec::new(), observations: TerminalObservations::default() },
        vessels: Vessels { records: Vec::new() },
    });
    let views = ConvoyViews::new("flotilla", reads);
    let none = BTreeMap::new();
    let held = KindRecords::get(&views, "fix-it#1").await.expect("get").expect("held");
    assert!(views.apply("fix-it#1", &none, &none, &held.spec).await.is_err());
    assert!(views.set_status("fix-it#1", &held.status.expect("status")).await.is_err());
    assert!(KindRecords::delete(&views, "fix-it#1").await.is_err());
    assert_eq!(KindRecords::list(&views).await.expect("list").len(), 1);
}

// Behaviour: a read dropped while it composes a changed launch again leaves that launch owed. The
// cursor has passed the change and will not name it again, so the next read composes it.
#[tokio::test]
async fn a_dropped_read_leaves_its_changes_for_the_next() {
    let reads = Arc::new(Reads {
        joins: HeldJoins::default(),
        convoys: Convoys::new(vec![launch("fix-it#1", HERE)]),
        terminals: Terminals { records: Vec::new(), observations: TerminalObservations::default() },
        vessels: Vessels { records: Vec::new() },
    });
    let views = ConvoyViews::new("flotilla", reads.clone());
    views.get("fix-it#1").await.expect("held");

    // The launch's record goes and the cursor names it; a read starts composing it and waits.
    reads.convoys.set_gate(true);
    reads.convoys.records.lock().expect("records").clear();
    reads.convoys.written.lock().expect("written").push(ConvoyChange::Changed { name: "fix-it#1".into(), home: HERE.into() });
    let reading = reads.convoys.reading.notified();
    let dropped = tokio::spawn({
        let views = views.clone();
        async move { views.get("fix-it#1").await.map(drop) }
    });
    reading.await;
    dropped.abort();
    assert!(dropped.await.expect_err("dropped").is_cancelled());
    reads.convoys.set_gate(false);

    assert!(views.get("fix-it#1").await.is_err(), "the next read composes the launch the dropped one owed");
}

/// Views over launches `names`, held here, joined with nothing.
fn views_of(names: &[&str]) -> (Arc<Reads>, ConvoyViews) {
    let reads = Arc::new(Reads {
        joins: HeldJoins::default(),
        convoys: Convoys::new(names.iter().map(|name| launch(name, HERE)).collect()),
        terminals: Terminals { records: Vec::new(), observations: TerminalObservations::default() },
        vessels: Vessels { records: Vec::new() },
    });
    let views = ConvoyViews::new("flotilla", reads.clone());
    (reads, views)
}

/// Each launch's version as the engine port lists it, by name.
async fn versions(views: &ConvoyViews) -> BTreeMap<String, u64> {
    KindRecords::list(views).await.expect("list").into_iter().map(|record| (record.name, record.version)).collect()
}

fn attention(notes: &mut ConvoyNotes) {
    notes.attention = Some(ConvoyAttention { source: "turn".into(), reason: "queued".into(), raised_at: at(120) });
}

// Behaviour (#3108): a note on one launch moves that launch's version alone. Another launch read
// again in the same pass, as a write to a record it joins makes it, shows the same and keeps its
// version: this home's notes are no part of a version they do not change.
#[tokio::test]
async fn a_note_on_one_launch_moves_only_its_version() {
    let (reads, views) = views_of(&["fix-it#1", "other#1"]);
    let before = versions(&views).await;

    reads.convoys.written.lock().expect("written").push(ConvoyChange::Changed { name: "other#1".into(), home: HERE.into() });
    assert!(reads.convoys.memory.update("fix-it#1", attention).1);
    let after = versions(&views).await;
    assert_ne!(after["fix-it#1"], before["fix-it#1"], "the noted launch changed");
    assert_eq!(after["other#1"], before["other#1"], "a launch read again that shows the same keeps its version");
}

// Behaviour (#3108): a launch that changes and changes back is told at three versions: a watcher
// that read the first sees the last as a change, as it is one.
#[tokio::test]
async fn a_change_and_its_undoing_are_each_told_at_their_own_version() {
    let (reads, views) = views_of(&["fix-it#1"]);
    let first = versions(&views).await["fix-it#1"];
    assert!(reads.convoys.memory.update("fix-it#1", attention).1);
    let noted = versions(&views).await["fix-it#1"];
    assert!(reads.convoys.memory.update("fix-it#1", |notes: &mut ConvoyNotes| *notes = ConvoyNotes::default()).1);
    let undone = versions(&views).await["fix-it#1"];
    assert!(first < noted && noted < undone, "versions {first}, {noted}, {undone} each move on");
}

// Behaviour (#3108): a launch read again at each of several passes, its record named by the log
// each time but showing the same, keeps the version it was first told at while the log moves on.
#[tokio::test]
async fn an_unchanged_launch_keeps_its_version_across_passes() {
    let (reads, views) = views_of(&["fix-it#1"]);
    let first = versions(&views).await["fix-it#1"];
    for _ in 0..3 {
        reads.convoys.written.lock().expect("written").push(ConvoyChange::Changed { name: "fix-it#1".into(), home: HERE.into() });
        assert_eq!(versions(&views).await["fix-it#1"], first);
    }
}

// Behaviour: a read dropped while the cursors pass keeps the cursors and what they passed. The
// next read composes the change the dropped one's pass named, and later reads go on following the
// cursors instead of listing every launch on each read (load_driver_smoke's driver dropped reads).
#[tokio::test]
async fn a_read_dropped_while_the_cursors_pass_keeps_them() {
    let reads = Arc::new(Reads {
        joins: HeldJoins::default(),
        convoys: Convoys::new(vec![launch("fix-it#1", HERE), launch("other#1", HERE)]),
        terminals: Terminals { records: Vec::new(), observations: TerminalObservations::default() },
        vessels: Vessels { records: Vec::new() },
    });
    let views = ConvoyViews::new("flotilla", reads.clone());
    views.get("fix-it#1").await.expect("held");
    let listed = reads.convoys.listed.load(Ordering::SeqCst);

    // The launch's record goes and the cursor names it; a read starts passing the cursors and waits.
    reads.convoys.passing.set(true);
    reads.convoys.records.lock().expect("records").retain(|record| record.name != "fix-it#1");
    reads.convoys.written.lock().expect("written").push(ConvoyChange::Changed { name: "fix-it#1".into(), home: HERE.into() });
    let reaching = reads.convoys.passing.reaching.notified();
    let dropped = tokio::spawn({
        let views = views.clone();
        async move { views.get("other#1").await.map(drop) }
    });
    reaching.await;
    dropped.abort();
    assert!(dropped.await.expect_err("dropped").is_cancelled());
    reads.convoys.passing.set(false);

    assert!(views.get("fix-it#1").await.is_err(), "the next read composes the change the dropped pass named");
    views.get("other#1").await.expect("read");
    views.get("other#1").await.expect("read again");
    assert_eq!(reads.convoys.listed.load(Ordering::SeqCst), listed, "no read lists every launch again");
}

// Behaviour: a pass that fails is read again in full once, since what it passed is not known, and
// keeps the cursors: later reads take changes from them instead of listing every launch again.
#[tokio::test]
async fn a_failed_pass_keeps_the_cursors() {
    let reads = Arc::new(Reads {
        joins: HeldJoins::default(),
        convoys: Convoys::new(vec![launch("fix-it#1", HERE), launch("other#1", HERE)]),
        terminals: Terminals { records: Vec::new(), observations: TerminalObservations::default() },
        vessels: Vessels { records: Vec::new() },
    });
    let views = ConvoyViews::new("flotilla", reads.clone());
    views.get("fix-it#1").await.expect("held");
    let listed = reads.convoys.listed.load(Ordering::SeqCst);

    // The launch's record goes and the next pass fails: the read lists every launch instead.
    reads.convoys.failing.store(true, Ordering::SeqCst);
    reads.convoys.records.lock().expect("records").retain(|record| record.name != "fix-it#1");
    assert!(views.get("fix-it#1").await.is_err(), "the failed pass's read lists the launch gone");
    assert_eq!(reads.convoys.listed.load(Ordering::SeqCst), listed + 1, "the failed pass's read lists every launch");

    // The other launch's record goes and only the cursor names it.
    reads.convoys.records.lock().expect("records").clear();
    reads.convoys.written.lock().expect("written").push(ConvoyChange::Changed { name: "other#1".into(), home: HERE.into() });
    assert!(views.get("other#1").await.is_err(), "the next read takes the change from the cursor");
    assert!(views.get("other#1").await.is_err(), "read again");
    assert_eq!(reads.convoys.listed.load(Ordering::SeqCst), listed + 1, "no later read lists every launch again");
}

/// The views converted by a read of one launch by name, with `others` unrelated launches held:
/// its copies, then another home's launch read where this home holds none.
async fn converted_by_one_read(others: usize) -> (u64, u64) {
    let mut launches: Vec<_> = (0..others).map(|n| launch(&format!("other-{n}#1"), HERE)).collect();
    launches.extend([launch("wanted#1", HERE), launch("wanted#1", "home-b"), launch("remote#1", "home-b")]);
    let reads = Arc::new(Reads {
        joins: HeldJoins::default(),
        convoys: Convoys::new(launches),
        terminals: Terminals { records: Vec::new(), observations: TerminalObservations::default() },
        vessels: Vessels { records: Vec::new() },
    });
    let views = ConvoyViews::new("flotilla", reads);
    assert_eq!(views.sources().await.expect("fill").len(), others + 3);

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

/// `name` held at `home`, ensured from `ensure`, and ended when `ended`.
fn ensured(name: &str, home: &str, ensure: &str, ended: bool) -> ConvoyRecord {
    let mut record = launch(name, home);
    record.admission.provenance.insert(flotilla_resources::ENSURED_FROM_ANNOTATION.into(), ensure.into());
    record.ended = ended.then(|| (ConvoyEnd::Delivered { disposition: "merged".into() }, at(200)));
    record
}

/// The launches a standing role's queries select, and the views each converted, with `others`
/// unrelated launches held: its live generations here and at every home, and its generations here.
async fn ensured_queries(others: usize) -> Vec<(usize, u64)> {
    let mut launches: Vec<_> = (0..others).map(|n| ensured(&format!("other-{n}#1"), HERE, "other", n % 2 == 0)).collect();
    launches.extend([
        ensured("wanted#1", HERE, "wanted", true),
        ensured("wanted#2", HERE, "wanted", false),
        ensured("wanted-b#1", "home-b", "wanted", false),
    ]);
    let reads = Arc::new(Reads {
        joins: HeldJoins::default(),
        convoys: Convoys::new(launches),
        terminals: Terminals { records: Vec::new(), observations: TerminalObservations::default() },
        vessels: Vessels { records: Vec::new() },
    });
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.serve_engine_in("flotilla");
    Arc::new(ConvoyViews::new("flotilla", reads)).install(&backend);
    backend.using::<Convoy>("flotilla").list().await.expect("the views fill");

    let field = format!("metadata.annotations.\"{}\"", flotilla_resources::ENSURED_FROM_ANNOTATION);
    let live = |everywhere: bool| StoreQuery {
        fields: BTreeMap::from([(field.clone(), "wanted".to_owned())]),
        include_replicas: everywhere,
        ..live_convoys_query()
    };
    let generations = StoreQuery { fields: BTreeMap::from([(field.clone(), "wanted".to_owned())]), ..StoreQuery::default() };
    let mut selected = Vec::new();
    for query in [live(false), live(true), generations] {
        let (rows, reads) = measure(backend.query_declared::<Convoy>("flotilla", &query)).await;
        let converted = reads.get("Convoy").map_or(0, |counts| counts.converted);
        selected.push((rows.expect("query").items.len(), converted));
    }
    selected
}

// Behaviour (#3108): a query selecting convoys by an annotation, as a standing role's passes read
// its generations, converts only the launches it may select, however many other launches are held.
#[tokio::test]
async fn a_selecting_query_converts_only_the_launches_it_selects() {
    let small = ensured_queries(10).await;
    let large = ensured_queries(400).await;
    assert_eq!(small, [(1, 1), (2, 2), (2, 2)], "live here, live everywhere, generations here: each converts only the launches it returns");
    assert_eq!(large, small, "the cost does not grow with the launches held");
}

/// `name` held here, placed on host `b1`, and ended when `ended`.
fn placed(name: &str, ended: bool) -> ConvoyRecord {
    let mut record = launch(name, HERE);
    record.admission.placement_decision = Some(
        flotilla_protocol::PlacementDecision::builder()
            .kind("host-direct-b1".to_owned())
            .target_host(flotilla_protocol::PlacementTargetHost {
                reference: flotilla_protocol::CanonicalHostId::resolved("b1"),
                display_name: "b1".into(),
            })
            .build(),
    );
    record.ended = ended.then(|| (ConvoyEnd::Delivered { disposition: "merged".into() }, at(200)));
    record
}

/// Host `b1`'s occupancy and the launches it converted, with `others` ended launches held besides
/// a live launch admitted there and an ended launch whose vessel there still tears down.
async fn occupancy_with(others: usize) -> (flotilla_resources::HostOccupancy, u64) {
    let mut launches: Vec<_> = (0..others).map(|n| placed(&format!("old-{n}#1"), true)).collect();
    launches.extend([placed("live#1", false), placed("tearing#1", true)]);
    let reads = Arc::new(Reads {
        joins: HeldJoins::default(),
        convoys: Convoys::new(launches),
        terminals: Terminals { records: Vec::new(), observations: TerminalObservations::default() },
        vessels: Vessels { records: Vec::new() },
    });
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.serve_engine_in("flotilla");
    Arc::new(ConvoyViews::new("flotilla", reads)).install(&backend);
    backend.using::<Convoy>("flotilla").list().await.expect("the views fill");
    // The vessel holds no decision of its own: its ended convoy's places it.
    let spec = flotilla_resources::VesselSpec {
        convoy_ref: "tearing#1".into(),
        vessel_name: "work".into(),
        fulfilment_kind_ref: "host-direct-b1".into(),
        adopted_checkout_refs: BTreeMap::new(),
    };
    backend
        .using::<flotilla_resources::Vessel>("flotilla")
        .create(&flotilla_resources::InputMeta::builder().name("tearing-work".to_owned()).build(), &spec)
        .await
        .expect("the vessel is written");

    let (occupancy, reads) = measure(flotilla_store::occupancy::host_occupancy(&backend, "flotilla", "b1")).await;
    (occupancy.expect("occupancy"), reads.get("Convoy").map_or(0, |counts| counts.converted))
}

// Behaviour (#3108): a host's occupancy counts the vessels live convoys admitted there and those
// an ended convoy still holds, converting only those launches, however many ended launches are held.
#[tokio::test]
async fn a_hosts_occupancy_converts_only_live_and_vessel_holding_launches() {
    let (small, converted) = occupancy_with(2).await;
    assert_eq!((small.placed, small.admitted.clone()), (1, vec![("live#1".to_owned(), 1)]), "the torn-down vessel and the live admission");
    let (large, converted_large) = occupancy_with(200).await;
    assert_eq!(large, small);
    assert_eq!((converted, converted_large), (2, 2), "the live launch and the vessel's ended one, at both sizes");
}
