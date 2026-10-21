//! Records writes (ADR 0064 "Records"): an effect appends to or changes its resource's records
//! through the handle, its edge declares the field, and the home applies the writes once the log
//! takes the edge.

use flotilla_schema::{RecordsFull, Timestamp};
use serde::{Deserialize, Serialize};

use flotilla_state_machines::{
    edges, states, EdgeDef, EdgeEventOf, Ended, Fault, Handle, Home, InputRead, Log, LogError, Machine, MachineDef, MemoryLog, RecordWrite,
    Resource, StoredState, Version, WriteError, WrongState,
};

states! {
    pub enum Ledger => LedgerLabel {
        Open { closed_by: Option<String> } = "op",
        Closed { closed_by: Option<String> } = "cl",
    }
}

/// Appends a note: a records write and no state change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub text: String,
}

/// Writes a note its declaration does not name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stray;

/// A gate that writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sneak;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Close;

edges! {
    pub enum LedgerEdge => LedgerEdgeKind {
        Note(Note) = "no",
        Stray(Stray) = "st",
        Sneak(Sneak) = "sn",
        Close(Close) = "cl",
    }
}

/// The ledger's records: its notes, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notes(pub Vec<(u64, String)>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteWrite(pub String);

impl RecordWrite<Notes> for NoteWrite {
    fn field(&self) -> &'static str {
        "notes"
    }

    fn apply(&self, notes: &mut Notes, version: u64) -> Result<(), RecordsFull> {
        if notes.0.len() >= 3 {
            return Err(RecordsFull);
        }
        notes.0.push((version, self.0.clone()));
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerRefusal {
    Ended,
    WrongState,
}

impl From<Ended<LedgerLabel>> for LedgerRefusal {
    fn from(_: Ended<LedgerLabel>) -> Self {
        LedgerRefusal::Ended
    }
}

impl From<WrongState<LedgerLabel, LedgerEdgeKind>> for LedgerRefusal {
    fn from(_: WrongState<LedgerLabel, LedgerEdgeKind>) -> Self {
        LedgerRefusal::WrongState
    }
}

pub struct LedgerMachine;

impl Machine for LedgerMachine {
    const NAME: &'static str = "Ledger";
    type Request = ();
    type Spec = Notes;
    type State = Ledger;
    type Edge = LedgerEdge;
    type Write = NoteWrite;
    type Refusal = LedgerRefusal;
    type World = ();

    fn definition() -> MachineDef<Self> {
        use LedgerLabel::*;
        MachineDef::new(Open)
            .edge(EdgeDef::<Self, Note>::new().from([Open]).to([Open]).writes(["notes"]).ungated("any note is kept").effect(
                |h, state, note| {
                    h.write(NoteWrite(note.text.clone()));
                    state
                },
            ))
            .edge(EdgeDef::<Self, Stray>::new().from([Open]).to([Open]).writes(["elsewhere"]).ungated("refused as a fault").effect(
                |h, state, _| {
                    h.write(NoteWrite("stray".into()));
                    state
                },
            ))
            .edge(
                EdgeDef::<Self, Sneak>::new()
                    .from([Open])
                    .to([Open])
                    .writes(["notes"])
                    .gate("refused as a fault", gate_sneak)
                    .effect(|_, state, _| state),
            )
            .edge(
                EdgeDef::<Self, Close>::new()
                    .from([Open])
                    .to([Closed])
                    .ungated("anyone closes")
                    .effect(|_, _, _| Ledger::Closed { closed_by: None }),
            )
            .end(Closed, "closed")
    }

    fn admit(_: &mut flotilla_state_machines::Admission<'_, Self>, _: ()) -> Result<(Notes, Ledger), LedgerRefusal> {
        Ok((Notes(Vec::new()), Ledger::Open { closed_by: None }))
    }
}

fn gate_sneak(h: &mut Handle<'_, LedgerMachine>, _: &Sneak) -> Result<(), LedgerRefusal> {
    h.write(NoteWrite("sneaked".into()));
    Ok(())
}

fn ledger() -> Home<LedgerMachine> {
    let mut home = Home::new("home").expect("valid declaration");
    home.create("l", (), &(), Timestamp(0)).expect("created");
    home
}

fn notes(home: &Home<LedgerMachine>) -> Vec<(u64, String)> {
    home.get("l").expect("exists").spec().0.clone()
}

// An edge that writes records and changes no state is no inert edge.
#[test]
fn a_records_edge_validates() {
    assert_eq!(LedgerMachine::definition().validate(), Ok(()));
}

// The home applies an effect's writes once the log takes the edge, each appended row stamped
// with the write's version; the state is untouched.
#[test]
fn writes_are_applied_with_the_edges_version() {
    let mut home = ledger();
    home.apply("l", Note { text: "one".into() }, &(), Timestamp(1)).expect("noted");
    home.apply("l", Note { text: "two".into() }, &(), Timestamp(2)).expect("noted");
    assert_eq!(notes(&home), [(2, "one".to_owned()), (3, "two".to_owned())]);
    assert_eq!(home.get("l").expect("exists").version(), Version(3));
    assert_eq!(home.get("l").expect("exists").since(), Timestamp(0), "a records edge keeps since");
}

// A write to a field the edge does not declare, or any write by a gate, is a declaration fault:
// nothing is written and the version stays.
#[test]
fn undeclared_writes_are_faults() {
    let mut home = ledger();
    let stray = home.apply("l", Stray, &(), Timestamp(1)).err();
    assert_eq!(stray, Some(WriteError::Fault(Fault::Writes { edge: LedgerEdgeKind::Stray, fields: vec!["notes"] })));
    let sneak = home.apply("l", Sneak, &(), Timestamp(1)).err();
    assert_eq!(sneak, Some(WriteError::Fault(Fault::Writes { edge: LedgerEdgeKind::Sneak, fields: vec!["notes"] })));
    assert_eq!((notes(&home), home.get("l").expect("exists").version()), (vec![], Version(1)));
}

// Writes that do not fit refuse the whole write before the log takes it.
#[test]
fn a_full_record_table_refuses_the_write() {
    let mut home = ledger();
    for text in ["a", "b", "c"] {
        home.apply("l", Note { text: text.into() }, &(), Timestamp(1)).expect("noted");
    }
    assert!(matches!(home.apply("l", Note { text: "d".into() }, &(), Timestamp(1)), Err(WriteError::Log(_))));
    assert_eq!((notes(&home).len(), home.log().last_sequence()), (3, 4));
}

/// A log that persists edges but has no way to store records writes.
struct EdgesOnly(MemoryLog<LedgerMachine>);

impl Log<LedgerMachine> for EdgesOnly {
    fn create(
        &mut self,
        writer: &str,
        at: Timestamp,
        resource: &Resource<LedgerMachine>,
        reads: &[InputRead],
    ) -> Result<Version, LogError> {
        self.0.create(writer, at, resource, reads)
    }

    fn edge(
        &mut self,
        previous: &StoredState<LedgerMachine>,
        next: &Resource<LedgerMachine>,
        event: &EdgeEventOf<LedgerMachine>,
    ) -> Result<Version, LogError> {
        self.0.edge(previous, next, event)
    }
}

// A log that does not store records writes refuses an edge that makes them rather than drop them,
// and the resource stays as it was; edges without writes still pass.
#[test]
fn a_log_without_records_refuses_writes() {
    let mut home = Home::<LedgerMachine, EdgesOnly>::with_log("home", EdgesOnly(MemoryLog::new())).expect("valid declaration");
    home.create("l", (), &(), Timestamp(0)).expect("created");
    assert!(matches!(home.apply("l", Note { text: "one".into() }, &(), Timestamp(1)), Err(WriteError::Log(_))));
    let resource = home.get("l").expect("exists");
    assert_eq!((resource.spec().0.len(), resource.version()), (0, Version(1)));
    home.apply("l", Close, &(), Timestamp(2)).expect("closed");
}
