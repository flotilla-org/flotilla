//! Convoy (spec `machines/convoy.qnt`; ADR 0064 "A convoy ends when its work is delivered").

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Address, CodeEnum, Decoder, Encoder, Timestamp};
use flotilla_state_machines::{
    machines::convoy::{
        Abandon, Authority, Convoy, ConvoyEdgeKind, ConvoyLabel, ConvoyRefusal, ConvoyRequest, ConvoyState, Deliver, Keep,
        MemoryConvoyWorld, Priority, Promise, PromiseId,
        PromiseState::{self, Kept, Pending, Retracted},
        Quota, Readmit, Retract, VesselClass,
    },
    Coded, Diagram, EdgeEventOf, Home, InputRead, Kind, Machine, Ref, StateValue, Version, WriteError,
};

const YES: &str = "yes";
const NO: &str = "no";

fn quota(counts: &[(u32, u32)]) -> Quota {
    counts.iter().map(|(class, count)| (VesselClass(*class), *count)).collect()
}

fn promises(states: &[(u32, PromiseState)]) -> BTreeMap<PromiseId, PromiseState> {
    states.iter().map(|(id, state)| (PromiseId(*id), *state)).collect()
}

fn project() -> Address {
    Address::project("flotilla").expect("project address")
}

fn target() -> Address {
    "forge:github/repo:flotilla-org%2Fflotilla/cr:1".parse().expect("canonical address")
}

fn request(promises: &[u32], counts: &[(u32, u32)]) -> ConvoyRequest {
    ConvoyRequest {
        project: project(),
        promises: promises.iter().map(|id| (PromiseId(*id), Promise { kind: "opaque".into(), target: target() })).collect(),
        quota: quota(counts),
        priority: Priority(1),
    }
}

// The project exists; an authority `yes` that is authorised and one `no` that is not; anything
// else is absent.
fn world() -> MemoryConvoyWorld {
    let mut world = MemoryConvoyWorld::default().with_project(&project());
    world.authority.put(&Ref::new(YES), true);
    world.authority.put(&Ref::new(NO), false);
    world
}

fn convoy(request: ConvoyRequest) -> Home<Convoy> {
    let mut home = Home::<Convoy>::new("home").expect("valid declaration");
    home.create("c", request, &world(), at()).expect("admitted");
    home
}

fn state(home: &Home<Convoy>) -> &ConvoyState {
    home.get("c").expect("exists").state()
}

fn at() -> Timestamp {
    Timestamp::from_micros(1)
}

fn retract(promise: u32, authority: &str) -> Retract {
    Retract { promise: PromiseId(promise), by: "lead".into(), authority: Ref::new(authority) }
}

fn abandon(reason: &str, authority: &str) -> Abandon {
    Abandon { by: "lead".into(), reason: reason.into(), authority: Ref::new(authority) }
}

fn readmit(counts: &[(u32, u32)], authority: &str) -> Readmit {
    Readmit { quota: quota(counts), by: "lead".into(), authority: Ref::new(authority) }
}

fn refused(refusal: ConvoyRefusal) -> Option<WriteError<Convoy>> {
    Some(WriteError::Refused(refusal))
}

// The declaration obeys the engine's rules.
#[test]
fn the_declaration_is_valid() {
    assert_eq!(Convoy::definition().validate(), Ok(()));
}

// State and edge codes are stable: renames never change them. Every label
// carries the work; Abandoned also carries its reason.
#[test]
fn codes_are_pinned() {
    let labels: Vec<_> = ConvoyLabel::ALL.iter().map(|s| (s.name(), s.code(), ConvoyState::fields(*s))).collect();
    let expected: [(&str, i64, &[&str]); 3] = [
        ("Open", 1, &["resolution", "quota"]),
        ("Delivered", 2, &["resolution", "quota"]),
        ("Abandoned", 3, &["resolution", "quota", "reason"]),
    ];
    assert_eq!(labels, expected);
    let edges: Vec<_> = ConvoyEdgeKind::ALL.iter().map(|k| (k.name(), k.code())).collect();
    assert_eq!(edges, [("keep", 1), ("retract", 2), ("deliver", 3), ("abandon", 4), ("readmit", 5)]);
}

// The diagram is drawn from the declaration: every edge with the data it
// changes, and both ends. The label drawing leaves out the edges that keep
// Open and change only data.
#[test]
fn the_diagram_is_pinned() {
    let head = "---
title: Convoy
---
stateDiagram-v2
  direction LR
  [*] --> Open
";
    let tail = "  Delivered --> [*]
  note right of Delivered: the convoy delivered its promises
  Abandoned --> [*]
  note right of Abandoned: the convoy was abandoned
";
    let full = "  Open --> Open: keep [resolution]
  Open --> Open: retract [resolution]
  Open --> Delivered: deliver
  Open --> Abandoned: abandon [reason]
  Open --> Open: readmit [quota]
";
    let labels = "  Open --> Delivered: deliver
  Open --> Abandoned: abandon [reason]
";
    assert_eq!(Convoy::definition().mermaid(Diagram::Full), format!("{head}{full}{tail}"));
    assert_eq!(Convoy::definition().mermaid(Diagram::Labels), format!("{head}{labels}{tail}"));
}

// Every convoy promises something: admission refuses an empty promise map,
// and admits a nonempty one Open with every promise Pending under the
// admitted quota. The quota is state, not spec.
#[test]
fn admission_requires_a_promise() {
    let mut home: Home<Convoy> = Home::new("home").expect("valid");
    assert_eq!(home.create("empty", request(&[], &[(0, 1)]), &world(), at()).err(), refused(ConvoyRefusal::NoPromises));
    let admitted = home.create("c", request(&[0, 1], &[(0, 1)]), &world(), at()).expect("admitted").resource;
    assert_eq!(admitted.state(), &ConvoyState::Open { resolution: promises(&[(0, Pending), (1, Pending)]), quota: quota(&[(0, 1)]) });
    assert_eq!((admitted.spec().promises.len(), admitted.spec().priority), (2, Priority(1)));
}

// #3392 G1: the project is a definitions-class kind named by address, so its existence is a
// recorded read at admission, not a foreign key. An unknown project is refused and creates
// nothing; a known one is admitted, and the create carries the version admission saw.
#[test]
fn admission_reads_the_project_by_address() {
    let mut home: Home<Convoy> = Home::new("home").expect("valid");
    let empty = MemoryConvoyWorld::default();
    assert_eq!(home.create("c", request(&[0], &[]), &empty, at()).err(), refused(ConvoyRefusal::UnknownProject));
    assert!(home.get("c").is_none(), "a refused admission creates nothing");
    let other = MemoryConvoyWorld::default().with_project(&Address::project("elsewhere").expect("project"));
    assert_eq!(home.create("c", request(&[0], &[]), &other, at()).err(), refused(ConvoyRefusal::UnknownProject));
    let mut world = MemoryConvoyWorld::default();
    world.projects.put(&Ref::new("project:noise"), ());
    let version = world.projects.put(&Ref::new(project().as_str()), ());
    let created = home.create("c", request(&[0], &[]), &world, at()).expect("admitted");
    assert_eq!(created.reads, [InputRead { kind: "project".into(), name: "project:flotilla".into(), version: Some(version) }]);
    assert_eq!(created.resource.spec().project, project());
    assert_eq!(version, Version(2));
}

// Keep settles a known pending promise and stays Open; an unknown promise is
// refused, and a settled one (kept or retracted) refuses a further keep: Kept
// is final.
#[test]
fn keep_settles_a_pending_promise_once() {
    let mut home = convoy(request(&[0, 1], &[]));
    let w = world();
    assert_eq!(home.apply("c", Keep { promise: PromiseId(9) }, &w, at()).err(), refused(ConvoyRefusal::UnknownPromise));
    home.apply("c", Keep { promise: PromiseId(0) }, &w, at()).expect("kept");
    assert_eq!(state(&home), &ConvoyState::Open { resolution: promises(&[(0, Kept), (1, Pending)]), quota: quota(&[]) });
    assert_eq!(home.apply("c", Keep { promise: PromiseId(0) }, &w, at()).err(), refused(ConvoyRefusal::PromiseSettled));
    home.apply("c", retract(1, YES), &w, at()).expect("retracted");
    assert_eq!(home.apply("c", Keep { promise: PromiseId(1) }, &w, at()).err(), refused(ConvoyRefusal::PromiseSettled));
    assert_eq!(state(&home), &ConvoyState::Open { resolution: promises(&[(0, Kept), (1, Retracted)]), quota: quota(&[]) });
}

// A data-only edge keeps its label: keep moves Open to Open, and its event
// records the same label on both sides while the promises change.
#[test]
fn keep_changes_only_the_promises() {
    let mut home = convoy(request(&[0], &[(0, 2)]));
    let event = home.apply("c", Keep { promise: PromiseId(0) }, &world(), at()).expect("kept");
    assert_eq!((event.from, event.to), (ConvoyLabel::Open, ConvoyLabel::Open));
    assert_eq!(state(&home).quota(), &quota(&[(0, 2)]));
    assert_eq!(state(&home).resolution(), &promises(&[(0, Kept)]));
}

// AcceptRetraction checks authority first: an unauthorised or absent
// authority refuses Unauthorised even for an unknown or settled promise.
#[test]
fn retraction_checks_authority_before_the_promise() {
    let mut home = convoy(request(&[0], &[]));
    let w = world();
    assert_eq!(home.apply("c", retract(9, NO), &w, at()).err(), refused(ConvoyRefusal::Unauthorised));
    assert_eq!(home.apply("c", retract(0, "absent"), &w, at()).err(), refused(ConvoyRefusal::Unauthorised));
    assert_eq!(home.apply("c", retract(9, YES), &w, at()).err(), refused(ConvoyRefusal::UnknownPromise));
    home.apply("c", Keep { promise: PromiseId(0) }, &w, at()).expect("kept");
    assert_eq!(home.apply("c", retract(0, NO), &w, at()).err(), refused(ConvoyRefusal::Unauthorised));
    assert_eq!(home.apply("c", retract(0, YES), &w, at()).err(), refused(ConvoyRefusal::PromiseSettled));
}

// Deliver requires every promise kept or retracted, naming the outstanding
// ones, and carries the work over unchanged; once delivered the convoy is
// ended and refuses everything NotOpen.
#[test]
fn deliver_waits_for_every_promise() {
    let mut home = convoy(request(&[0, 1, 2], &[(0, 1)]));
    let w = world();
    home.apply("c", Keep { promise: PromiseId(1) }, &w, at()).expect("kept");
    let outstanding = BTreeSet::from([PromiseId(0), PromiseId(2)]);
    assert_eq!(home.apply("c", Deliver, &w, at()).err(), refused(ConvoyRefusal::PromisesOutstanding(outstanding)));
    home.apply("c", retract(0, YES), &w, at()).expect("retracted");
    home.apply("c", Keep { promise: PromiseId(2) }, &w, at()).expect("kept");
    home.apply("c", Deliver, &w, at()).expect("delivered");
    let delivered = ConvoyState::Delivered { resolution: promises(&[(0, Retracted), (1, Kept), (2, Kept)]), quota: quota(&[(0, 1)]) };
    assert_eq!(state(&home), &delivered);
    let not_open = refused(ConvoyRefusal::NotOpen);
    assert_eq!(home.apply("c", Deliver, &w, at()).err(), not_open);
    assert_eq!(home.apply("c", Keep { promise: PromiseId(9) }, &w, at()).err(), not_open);
    assert_eq!(home.apply("c", retract(0, NO), &w, at()).err(), not_open);
    assert_eq!(home.apply("c", abandon("why", YES), &w, at()).err(), not_open);
    assert_eq!(home.apply("c", readmit(&[(0, 1)], YES), &w, at()).err(), not_open);
}

// Abandon is always an authorised decision with a reason; an unauthorised
// decision or an empty reason is refused Unauthorised. The state carries the
// reason beside the work as it stood.
#[test]
fn abandon_is_an_authorised_decision_with_a_reason() {
    let mut home = convoy(request(&[0, 1], &[(0, 1)]));
    let w = world();
    home.apply("c", Keep { promise: PromiseId(1) }, &w, at()).expect("kept");
    assert_eq!(home.apply("c", abandon("no longer wanted", NO), &w, at()).err(), refused(ConvoyRefusal::Unauthorised));
    assert_eq!(home.apply("c", abandon("", YES), &w, at()).err(), refused(ConvoyRefusal::Unauthorised));
    assert_eq!(state(&home).reason(), None);
    home.apply("c", abandon("no longer wanted", YES), &w, at()).expect("abandoned");
    let abandoned = ConvoyState::Abandoned {
        resolution: promises(&[(0, Pending), (1, Kept)]),
        quota: quota(&[(0, 1)]),
        reason: "no longer wanted".into(),
    };
    assert_eq!(state(&home), &abandoned);
    assert_eq!(home.apply("c", Keep { promise: PromiseId(0) }, &w, at()).err(), refused(ConvoyRefusal::NotOpen));
}

// Readmission raises the quota: it needs authority, keeps the class domain
// and lowers no count, compared with the quota in force (the last readmit's),
// and it leaves the promises as they are.
#[test]
fn readmission_only_raises_the_quota() {
    let mut home = convoy(request(&[0], &[(0, 1), (1, 1)]));
    let w = world();
    assert_eq!(home.apply("c", readmit(&[(0, 2), (1, 1)], NO), &w, at()).err(), refused(ConvoyRefusal::Unauthorised));
    for (case, q) in [("missing class", vec![(0, 2)]), ("extra class", vec![(0, 2), (1, 1), (2, 1)]), ("lower", vec![(0, 0), (1, 3)])] {
        assert_eq!(home.apply("c", readmit(&q, YES), &w, at()).err(), refused(ConvoyRefusal::QuotaNotIncrease), "{case}");
    }
    home.apply("c", readmit(&[(0, 1), (1, 1)], YES), &w, at()).expect("unchanged quota is no decrease");
    home.apply("c", Keep { promise: PromiseId(0) }, &w, at()).expect("kept");
    home.apply("c", readmit(&[(0, 3), (1, 1)], YES), &w, at()).expect("raised");
    assert_eq!(state(&home), &ConvoyState::Open { resolution: promises(&[(0, Kept)]), quota: quota(&[(0, 3), (1, 1)]) });
    // Above the admitted quota but below the readmitted one.
    assert_eq!(home.apply("c", readmit(&[(0, 2), (1, 1)], YES), &w, at()).err(), refused(ConvoyRefusal::QuotaNotIncrease));
}

// Only `Authority` is read for an authorised decision: an absent authority is
// never authorised, and the read is recorded on the event.
#[test]
fn an_authorised_edge_records_its_authority_read() {
    let mut home = convoy(request(&[0], &[]));
    let w = world();
    let event = home.apply("c", retract(0, YES), &w, at()).expect("retracted");
    assert!(event.payload.reads.iter().any(|read| read.kind == <Authority as Kind>::NAME && read.name == YES));
}

// Every convoy edge's event is storable as it is: the schema codec's CBOR profile takes text map
// keys only, so a readmission's quota (keyed by vessel class) travels as a list of pairs, and each
// event decodes to the one the home appended.
#[test]
fn convoy_edge_events_round_trip_through_the_schema_codec() {
    let mut home = Home::<Convoy>::new("home").expect("valid");
    home.create("c", request(&[0, 1], &[(0, 1), (3, 2)]), &world(), at()).expect("admitted");
    let world = world();
    home.apply("c", readmit(&[(0, 2), (3, 2)], YES), &world, Timestamp(1)).expect("readmitted");
    home.apply("c", Keep { promise: PromiseId(0) }, &world, Timestamp(2)).expect("kept");
    home.apply("c", Retract { promise: PromiseId(1), by: "lead".into(), authority: Ref::new(YES) }, &world, Timestamp(3))
        .expect("retracted");
    home.apply("c", Deliver, &world, Timestamp(4)).expect("delivered");
    let events = home.get("c").expect("exists").events();
    assert_eq!(events.len(), 4);
    for row in events {
        let rows = Encoder::encode(&row.record).expect("encodes");
        assert_eq!(Decoder::decode::<EdgeEventOf<Convoy>>(rows).expect("decodes"), row.record);
    }
}
