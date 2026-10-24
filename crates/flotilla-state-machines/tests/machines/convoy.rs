//! Convoy (spec `machines/convoy.qnt`; ADR 0064 "A convoy ends when its work is delivered", "Records").

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Address, Code, CodeEnum, Frozen, Timestamp};
use flotilla_state_machines::{
    machines::convoy::{
        Abandon, AddSeat, AdmittedPromise, Authority, Convoy, ConvoyEdgeKind, ConvoyLabel, ConvoyRefusal, ConvoyRequest, ConvoySpec,
        ConvoyState, Declare, Deliver, Duty, DutyId, DutyScope, Keep, MemoryConvoyWorld, Priority, Progress, PromiseId, PromiseKinds,
        PromiseSource, Quota, Readmit, Reassign, Reject, Resupervise, RetireDuty, Retract, Seat, SeatName, Submit, Terms, VesselClass,
    },
    Coded, Diagram, Home, InputRead, Machine, Ref, StateValue, Version, WriteError,
};

const YES: &str = "yes";
const NO: &str = "no";

fn quota(counts: &[(&str, u32)]) -> Quota {
    counts.iter().map(|(class, count)| (VesselClass::new(*class), *count)).collect()
}

fn project() -> Address {
    Address::project("p").expect("project")
}

fn address(text: &str) -> Address {
    Address::project(text).expect("address")
}

fn seat(role: &str) -> SeatName {
    SeatName::new(role, "")
}

fn seat_row(role: &str, parent: Option<&str>) -> Seat {
    Seat {
        retired: false,
        replacements: 0,
        role: role.into(),
        seat: String::new(),
        parent: parent.map(seat),
        supervised_by: address("supervisor"),
    }
}

fn admitted(id: u32, source: PromiseSource, owner: &str) -> AdmittedPromise {
    AdmittedPromise { label: format!("p{id}"), kind: "pr".into(), target: address("target"), source, owner: seat(owner) }
}

fn lifetime(owner: &str) -> Duty {
    Duty { owner: seat(owner), scope: DutyScope::Lifetime, terms: Terms::default() }
}

/// A convoy whose seat `coder` owns the given promises, under `lead`.
fn request(promises: &[(u32, PromiseSource)]) -> ConvoyRequest {
    ConvoyRequest {
        project: project(),
        promises: promises.iter().map(|(id, source)| (PromiseId(*id), admitted(*id, *source, "coder"))).collect(),
        duties: BTreeMap::new(),
        seats: vec![seat_row("lead", None), seat_row("coder", Some("lead"))],
        quota: quota(&[("standard", 1)]),
        priority: Priority(1),
        kinds: Frozen::new(PromiseKinds::default()).expect("freeze"),
    }
}

// The project exists; authority and durability `yes` hold and `no` do not;
// anything else is absent.
fn world() -> MemoryConvoyWorld {
    let mut world = MemoryConvoyWorld::default().with_project(&project());
    for (name, holds) in [(YES, true), (NO, false)] {
        world.authority.put(&Ref::new(name), holds);
        world.durable.put(&Ref::new(name), holds);
    }
    world
}

fn convoy(request: ConvoyRequest) -> Home<Convoy> {
    let mut home = Home::new("home").expect("valid declaration");
    let world: &<Convoy as Machine>::World = &world();
    home.create("c", request, world, Timestamp(0)).expect("admitted");
    home
}

fn at() -> Timestamp {
    Timestamp(5)
}

fn apply(
    home: &mut Home<Convoy>,
    edge: impl Into<flotilla_state_machines::machines::convoy::ConvoyEdge>,
) -> Result<(), WriteError<Convoy>> {
    home.apply("c", edge, &world(), at()).map(drop)
}

fn refused(refusal: ConvoyRefusal) -> Result<(), WriteError<Convoy>> {
    Err(WriteError::Refused(refusal))
}

fn progress(home: &Home<Convoy>, id: u32) -> Progress {
    home.get("c").expect("exists").spec().promise(PromiseId(id)).expect("promise").1.progress.value
}

fn submit(id: u32, by: &str, durable: &str) -> Submit {
    Submit { promise: PromiseId(id), by: seat(by), address: address("change"), revision: "abc".into(), durable: Ref::new(durable) }
}

fn retract(id: u32, by: &str, authority: Authority) -> Retract {
    Retract { promise: PromiseId(id), by: seat(by), authority, reason: "decision".into() }
}

fn verdict(id: u32, authority: Authority) -> Keep {
    Keep::Verdict { promise: PromiseId(id), by: seat("decider"), authority, reason: "decision".into() }
}

// The declaration obeys the engine's rules.
#[test]
fn the_declaration_is_valid() {
    assert_eq!(Convoy::definition().validate(), Ok(()));
}

// State and edge codes are stable: renames never change them. Every label
// carries the quota in force and the retired duties; Abandoned also carries
// its reason. Codes kept from the earlier machine stay (`ke`, `rt`, `de`,
// `ab`, `ra`).
#[test]
fn codes_are_pinned() {
    let labels: Vec<_> = ConvoyLabel::ALL.iter().map(|s| (s.name(), s.code(), ConvoyState::fields(*s))).collect();
    let expected: [(&str, Code, &[&str]); 3] = [
        ("Open", Code::new("op"), &["quota", "retired"]),
        ("Delivered", Code::new("de"), &["quota", "retired"]),
        ("Abandoned", Code::new("ab"), &["quota", "retired", "reason"]),
    ];
    assert_eq!(labels, expected);
    let edges: Vec<_> = ConvoyEdgeKind::ALL.iter().map(|k| (k.name(), k.code().as_str().to_owned())).collect();
    let expected = [
        ("declare", "dc"),
        ("submit", "su"),
        ("reject", "rj"),
        ("keep", "ke"),
        ("retract", "rt"),
        ("reassign", "rs"),
        ("retire_duty", "rd"),
        ("add_seat", "as"),
        ("resupervise", "sv"),
        ("retire_seat", "sr"),
        ("rollover", "ro"),
        ("replace_seat", "rp"),
        ("deliver", "de"),
        ("abandon", "ab"),
        ("readmit", "ra"),
    ];
    assert_eq!(edges, expected.map(|(name, code)| (name, code.to_owned())));
}

// The diagram is drawn from the declaration: every edge with the data it
// changes and the records it writes, and both ends.
#[test]
fn the_diagram_is_pinned() {
    let full = Convoy::definition().mermaid(Diagram::Full);
    for line in [
        "  Open --> Open: declare writes promises\n",
        "  Open --> Open: submit writes promises\n",
        "  Open --> Open: retire_duty [retired]\n",
        "  Open --> Open: add_seat writes seats\n",
        "  Open --> Open: resupervise writes seats\n",
        "  Open --> Delivered: deliver\n",
        "  Open --> Abandoned: abandon [reason]\n",
        "  Open --> Open: readmit [quota]\n",
        "  note right of Delivered: the convoy delivered its work\n",
    ] {
        assert!(full.contains(line), "missing {line:?} in\n{full}");
    }
    let labels = Convoy::definition().mermaid(Diagram::Labels);
    assert!(!labels.contains("Open --> Open"), "{labels}");
}

// A convoy needs an obligation: a promise, or a convoy-lifetime duty alone.
// A duty while promises are open is not one.
#[test]
fn admission_requires_an_obligation() {
    let mut home = Home::<Convoy>::new("home").expect("valid declaration");
    let mut empty = request(&[]);
    assert_eq!(home.create("a", empty.clone(), &world(), Timestamp(0)).err(), Some(WriteError::Refused(ConvoyRefusal::NoObligations)));
    empty.duties = [(DutyId(0), Duty { scope: DutyScope::WhileOpen { promises: BTreeSet::new() }, ..lifetime("coder") })].into();
    assert_eq!(home.create("b", empty.clone(), &world(), Timestamp(0)).err(), Some(WriteError::Refused(ConvoyRefusal::NoObligations)));
    empty.duties = [(DutyId(0), lifetime("coder"))].into();
    let created = home.create("c", empty, &world(), Timestamp(0)).expect("a lifetime duty is an obligation");
    assert_eq!(created.resource.state(), &ConvoyState::Open { quota: quota(&[("standard", 1)]), retired: BTreeSet::new() });
}

// #3392 G1: the project is read by address at admission, and the create
// carries the version it saw; an unknown project creates nothing.
#[test]
fn admission_reads_the_project_by_address() {
    let mut home = Home::<Convoy>::new("home").expect("valid declaration");
    let unknown = MemoryConvoyWorld::default();
    let refusal = home.create("c", request(&[(0, PromiseSource::Template)]), &unknown, Timestamp(0)).err();
    assert_eq!(refusal, Some(WriteError::Refused(ConvoyRefusal::UnknownProject)));
    assert!(home.get("c").is_none());
    let mut known = MemoryConvoyWorld::default();
    let version = known.projects.put(&Ref::new(project().as_str()), ());
    let created = home.create("c", request(&[(0, PromiseSource::Template)]), &known, Timestamp(0)).expect("admitted");
    assert_eq!(
        created.reads,
        [
            InputRead { kind: "project".into(), name: "project:p".into(), version: Some(version) },
            InputRead { kind: "convoy-limits".into(), name: "configured".into(), version: None }
        ]
    );
}

// Every owner and every parent is an admitted seat, and seat names are unique.
#[test]
fn admission_refuses_an_owner_that_is_no_seat() {
    let mut home = Home::<Convoy>::new("home").expect("valid declaration");
    let mut orphan = request(&[(0, PromiseSource::Template)]);
    orphan.promises.insert(PromiseId(1), admitted(1, PromiseSource::Template, "nobody"));
    let mut unparented = request(&[(0, PromiseSource::Template)]);
    unparented.seats.push(seat_row("reviewer", Some("nobody")));
    let mut twice = request(&[(0, PromiseSource::Template)]);
    twice.seats.push(seat_row("coder", None));
    let mut unowned = request(&[]);
    unowned.duties = [(DutyId(0), lifetime("nobody"))].into();
    for (name, bad) in [("a", orphan), ("b", unparented), ("c", twice), ("d", unowned)] {
        assert_eq!(home.create(name, bad, &world(), Timestamp(0)).err(), Some(WriteError::Refused(ConvoyRefusal::UnknownSeat)), "{name}");
    }
}

// Admission's seats form a tree: a seat that is its own parent, or two seats under each other,
// are refused, where a chain of parents is admitted in any order.
#[test]
fn admission_refuses_a_seat_cycle() {
    let mut home = Home::<Convoy>::new("home").expect("valid declaration");
    let cycle = |seats: Vec<Seat>| ConvoyRequest { seats, ..request(&[(0, PromiseSource::Template)]) };
    let own = cycle(vec![seat_row("coder", Some("coder"))]);
    let pair = cycle(vec![seat_row("lead", Some("coder")), seat_row("coder", Some("lead"))]);
    for (name, bad) in [("a", own), ("b", pair)] {
        assert_eq!(home.create(name, bad, &world(), Timestamp(0)).err(), Some(WriteError::Refused(ConvoyRefusal::SeatCycle)), "{name}");
    }
    let chain = cycle(vec![seat_row("coder", Some("lead")), seat_row("reviewer", Some("coder")), seat_row("lead", None)]);
    home.create("c", chain, &world(), Timestamp(0)).expect("a chain in any order");
}

// The crew declares a promise owned by a seat; an id ever used is refused,
// retracted ones included, and so is an owner that is no seat.
#[test]
fn declare_adds_a_crew_promise_once() {
    let mut home = convoy(request(&[(0, PromiseSource::Template)]));
    let declare = |id: u32, owner: &str| Declare {
        promise: PromiseId(id),
        label: format!("p{id}"),
        kind: "pr".into(),
        target: address("t"),
        owner: seat(owner),
    };
    assert_eq!(apply(&mut home, declare(1, "nobody")), refused(ConvoyRefusal::UnknownSeat));
    apply(&mut home, declare(1, "coder")).expect("declared");
    let promise = home.get("c").expect("exists").spec().promise(PromiseId(1)).expect("row").1.clone();
    assert_eq!((promise.source, promise.progress.value, promise.progress.since), (PromiseSource::Crew, Progress::Pending, at()));
    assert_eq!(apply(&mut home, declare(1, "coder")), refused(ConvoyRefusal::PromiseExists));
    apply(&mut home, retract(1, "coder", Authority::Owner)).expect("the owner retracts before submitting");
    assert_eq!(apply(&mut home, declare(1, "coder")), refused(ConvoyRefusal::PromiseExists));
}

// Only the owner submits, and only a durable reference. A submission is a
// row under the promise; submitting again keeps the earlier row, and the
// promise stays Submitted from the first.
#[test]
fn the_owner_submits_durable_references() {
    let mut home = convoy(request(&[(0, PromiseSource::Template)]));
    assert_eq!(apply(&mut home, submit(0, "lead", YES)), refused(ConvoyRefusal::NotOwner));
    assert_eq!(apply(&mut home, submit(0, "coder", NO)), refused(ConvoyRefusal::NotDurable));
    assert_eq!(apply(&mut home, submit(0, "coder", "absent")), refused(ConvoyRefusal::NotDurable));
    assert_eq!(apply(&mut home, submit(9, "coder", YES)), refused(ConvoyRefusal::UnknownPromise));
    apply(&mut home, submit(0, "coder", YES)).expect("submitted");
    home.apply("c", submit(0, "coder", YES), &world(), Timestamp(9)).expect("submitted again");
    let promise = home.get("c").expect("exists").spec().promise(PromiseId(0)).expect("row").1.clone();
    assert_eq!(promise.submissions.len(), 2);
    assert_eq!(promise.submissions.get(flotilla_schema::Position(1)).expect("second").version, 3, "stamped with the write's version");
    assert_eq!((promise.progress.value, promise.progress.since), (Progress::Submitted, at()));
}

// Reject returns a Submitted promise to Pending and keeps its submissions.
#[test]
fn reject_returns_a_submission() {
    let mut home = convoy(request(&[(0, PromiseSource::Template)]));
    assert_eq!(apply(&mut home, Reject { promise: PromiseId(0) }), refused(ConvoyRefusal::NotSubmitted));
    apply(&mut home, submit(0, "coder", YES)).expect("submitted");
    apply(&mut home, Reject { promise: PromiseId(0) }).expect("rejected");
    assert_eq!(progress(&home, 0), Progress::Pending);
    assert_eq!(home.get("c").expect("exists").spec().promise(PromiseId(0)).expect("row").1.submissions.len(), 1);
}

// Observed keeping names the current submission of a Submitted promise; a
// verdict follows the promise's source and needs a reason; Kept is final.
#[test]
fn keep_reads_current_evidence_or_a_verdict() {
    let mut home = convoy(request(&[(0, PromiseSource::Template), (1, PromiseSource::Dispatch)]));
    let observed = |submission: u32| Keep::Observed { promise: PromiseId(0), submission };
    assert_eq!(apply(&mut home, observed(0)), refused(ConvoyRefusal::StaleEvidence));
    apply(&mut home, submit(0, "coder", YES)).expect("submitted");
    apply(&mut home, submit(0, "coder", YES)).expect("submitted again");
    assert_eq!(apply(&mut home, observed(1)), refused(ConvoyRefusal::StaleEvidence));
    apply(&mut home, observed(2)).expect("kept on the current submission");
    assert_eq!(apply(&mut home, verdict(0, Authority::Operator)), refused(ConvoyRefusal::PromiseSettled));
    assert_eq!(apply(&mut home, verdict(1, Authority::ConvoySupervisor)), refused(ConvoyRefusal::Unauthorised));
    let unreasoned = Keep::Verdict { promise: PromiseId(1), by: seat("decider"), authority: Authority::Dispatcher, reason: String::new() };
    assert_eq!(apply(&mut home, unreasoned), refused(ConvoyRefusal::Unauthorised));
    apply(&mut home, verdict(1, Authority::Dispatcher)).expect("the dispatcher keeps its promise");
    assert_eq!((progress(&home, 0), progress(&home, 1)), (Progress::Kept, Progress::Kept));
}

// A crew promise is its owner's to retract before any submission, and its
// owner's supervisor's after; a template promise is the convoy supervisor's.
#[test]
fn retraction_follows_the_source() {
    let mut home = convoy(request(&[(0, PromiseSource::Template)]));
    let declare = Declare { promise: PromiseId(1), label: "p1".into(), kind: "pr".into(), target: address("t"), owner: seat("coder") };
    apply(&mut home, declare).expect("declared");
    assert_eq!(apply(&mut home, retract(0, "coder", Authority::Owner)), refused(ConvoyRefusal::Unauthorised));
    assert_eq!(apply(&mut home, retract(1, "lead", Authority::Owner)), refused(ConvoyRefusal::Unauthorised), "not its owner");
    assert_eq!(apply(&mut home, retract(1, "lead", Authority::OwnerSupervisor)), refused(ConvoyRefusal::Unauthorised), "nothing submitted");
    apply(&mut home, submit(1, "coder", YES)).expect("submitted");
    assert_eq!(apply(&mut home, retract(1, "coder", Authority::Owner)), refused(ConvoyRefusal::Unauthorised), "submitted");
    apply(&mut home, retract(1, "lead", Authority::OwnerSupervisor)).expect("the supervisor retracts");
    apply(&mut home, retract(0, "lead", Authority::ConvoySupervisor)).expect("the convoy supervisor retracts");
    assert_eq!((progress(&home, 0), progress(&home, 1)), (Progress::Retracted, Progress::Retracted));
}

// A handoff moves the promise and its submissions to another seat; only the
// owner, its supervisor or an operator hands off, and only to a seat.
#[test]
fn reassign_moves_an_open_promise_to_a_seat() {
    let mut home = convoy(request(&[(0, PromiseSource::Template)]));
    apply(&mut home, submit(0, "coder", YES)).expect("submitted");
    let reassign = |to: &str, by: &str, authority| Reassign { promise: PromiseId(0), to: seat(to), by: seat(by), authority };
    assert_eq!(apply(&mut home, reassign("lead", "lead", Authority::Owner)), refused(ConvoyRefusal::Unauthorised));
    assert_eq!(apply(&mut home, reassign("lead", "x", Authority::Dispatcher)), refused(ConvoyRefusal::Unauthorised));
    assert_eq!(apply(&mut home, reassign("nobody", "coder", Authority::Owner)), refused(ConvoyRefusal::UnknownSeat));
    apply(&mut home, reassign("lead", "coder", Authority::Owner)).expect("handed off");
    let promise = home.get("c").expect("exists").spec().promise(PromiseId(0)).expect("row").1.clone();
    assert_eq!((promise.owner, promise.progress.value, promise.submissions.len()), (seat("lead"), Progress::Submitted, 1));
    let version = home.get("c").expect("exists").version();
    apply(&mut home, reassign("lead", "lead", Authority::Owner)).expect("to the current owner");
    assert_eq!(home.get("c").expect("exists").version(), Version(version.0 + 1), "an accepted edge that writes nothing");
}

// Fan-out adds a seat under an existing one; a name is never reused; a
// supervision change re-points one seat. Both are authorised decisions.
#[test]
fn seats_are_added_and_resupervised() {
    let mut home = convoy(request(&[(0, PromiseSource::Template)]));
    let add = |name: &str, parent: &str, authority: &str| AddSeat {
        seat: seat(name),
        parent: seat(parent),
        supervised_by: address("lead"),
        by: seat("lead"),
        authority: Ref::new(authority),
    };
    assert_eq!(apply(&mut home, add("reviewer", "lead", NO)), refused(ConvoyRefusal::Unauthorised));
    assert_eq!(apply(&mut home, add("coder", "lead", YES)), refused(ConvoyRefusal::SeatExists));
    assert_eq!(apply(&mut home, add("reviewer", "nobody", YES)), refused(ConvoyRefusal::UnknownSeat));
    apply(&mut home, add("reviewer", "lead", YES)).expect("added");
    let change =
        |name: &str| Resupervise { seat: seat(name), supervised_by: address("operator"), by: seat("lead"), authority: Ref::new(YES) };
    assert_eq!(apply(&mut home, change("nobody")), refused(ConvoyRefusal::UnknownSeat));
    apply(&mut home, change("reviewer")).expect("re-pointed");
    let spec = home.get("c").expect("exists").spec().clone();
    let reviewer = spec.seat(&seat("reviewer")).expect("row").1;
    assert_eq!((reviewer.parent.clone(), reviewer.supervised_by.clone()), (Some(seat("lead")), address("operator")));
    assert_eq!(spec.seats.len(), 3);
}

// Deliver waits for every promise, naming the open ones, then for every
// lifetime duty, naming the holding ones; an authorised retirement releases
// a duty. Once delivered the convoy refuses everything NotOpen.
#[test]
fn deliver_waits_for_promises_then_duties() {
    let mut with_duty = request(&[(0, PromiseSource::Template)]);
    with_duty.duties = [(DutyId(0), lifetime("lead"))].into();
    let mut home = convoy(with_duty);
    assert_eq!(apply(&mut home, Deliver), refused(ConvoyRefusal::PromisesOutstanding([PromiseId(0)].into())));
    apply(&mut home, verdict(0, Authority::Operator)).expect("kept");
    assert_eq!(apply(&mut home, Deliver), refused(ConvoyRefusal::DutyHolds([DutyId(0)].into())));
    let retire = |duty: u32, authority: &str| RetireDuty { duty: DutyId(duty), by: seat("lead"), authority: Ref::new(authority) };
    assert_eq!(apply(&mut home, retire(0, NO)), refused(ConvoyRefusal::Unauthorised));
    assert_eq!(apply(&mut home, retire(1, YES)), refused(ConvoyRefusal::UnknownDuty));
    apply(&mut home, retire(0, YES)).expect("retired");
    assert_eq!(apply(&mut home, retire(0, YES)), refused(ConvoyRefusal::UnknownDuty), "already retired");
    apply(&mut home, Deliver).expect("delivered");
    let state = home.get("c").expect("exists").state().clone();
    assert_eq!(state, ConvoyState::Delivered { quota: quota(&[("standard", 1)]), retired: [DutyId(0)].into() });
    assert_eq!(apply(&mut home, submit(0, "coder", YES)), refused(ConvoyRefusal::NotOpen));
}

// Abandon is an authorised decision with a reason, kept beside the work.
#[test]
fn abandon_is_an_authorised_decision() {
    let mut home = convoy(request(&[(0, PromiseSource::Template)]));
    let abandon = |reason: &str, authority: &str| Abandon { by: seat("lead"), reason: reason.into(), authority: Ref::new(authority) };
    assert_eq!(apply(&mut home, abandon("decision", NO)), refused(ConvoyRefusal::Unauthorised));
    assert_eq!(apply(&mut home, abandon("", YES)), refused(ConvoyRefusal::Unauthorised));
    apply(&mut home, abandon("decision", YES)).expect("abandoned");
    assert_eq!(home.get("c").expect("exists").state().reason(), Some("decision"));
}

// Readmission raises the quota over the same classes, never lowering a count.
#[test]
fn readmission_only_raises_the_quota() {
    let mut home = convoy(request(&[(0, PromiseSource::Template)]));
    let readmit =
        |counts: &[(&str, u32)], authority: &str| Readmit { quota: quota(counts), by: seat("lead"), authority: Ref::new(authority) };
    assert_eq!(apply(&mut home, readmit(&[("standard", 2)], NO)), refused(ConvoyRefusal::Unauthorised));
    assert_eq!(apply(&mut home, readmit(&[("standard", 0)], YES)), refused(ConvoyRefusal::QuotaNotIncrease));
    assert_eq!(apply(&mut home, readmit(&[("standard", 2), ("large", 1)], YES)), refused(ConvoyRefusal::QuotaNotIncrease));
    apply(&mut home, readmit(&[("standard", 2)], YES)).expect("readmitted");
    assert_eq!(home.get("c").expect("exists").state().quota(), &quota(&[("standard", 2)]));
}

// ADR 0047: the previous generation's stored record representation has no
// retired/replacement/floor fields; new writes include their current values.
#[test]
fn previous_seat_and_convoy_records_decode_with_growth_defaults() {
    let row = seat_row("a", None);
    let mut previous = serde_json::to_value(&row).expect("encode seat");
    previous.as_object_mut().unwrap().remove("retired");
    previous.as_object_mut().unwrap().remove("replacements");
    let decoded: Seat = serde_json::from_value(previous).expect("N→N+1 seat");
    assert!(!decoded.retired);
    assert_eq!(decoded.replacements, 0);
    let spec = ConvoySpec {
        project: project(),
        promise_floor: 0,
        priority: Priority(0),
        kinds: Frozen::new(PromiseKinds::default()).unwrap(),
        duties: BTreeMap::new(),
        seats: flotilla_schema::Records::new(),
        promises: flotilla_schema::Records::new(),
    };
    let mut previous = serde_json::to_value(&spec).expect("encode convoy spec");
    previous.as_object_mut().unwrap().remove("promise_floor");
    let decoded: ConvoySpec = serde_json::from_value(previous).expect("N→N+1 convoy spec");
    assert_eq!(decoded.promise_floor, 0);
}

#[test]
fn admission_starts_fresh_seat_lifetimes() {
    let mut req = request(&[(0, PromiseSource::Template)]);
    for seat in &mut req.seats {
        seat.retired = true;
        seat.replacements = u32::MAX;
    }
    let home = convoy(req);
    assert!(home.get("c").unwrap().spec().seats.iter().all(|r| !r.record.retired && r.record.replacements == 0));
}
