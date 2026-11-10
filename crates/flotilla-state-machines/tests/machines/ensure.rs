//! Ensure against `specs/primitives/machines/ensure.qnt`.

use flotilla_schema::{Code, CodeEnum, Timestamp};
use flotilla_state_machines::{
    machines::ensure::{
        Authority, Bind, Create, Drain, Ensure, EnsureEdgeKind, EnsureLabel, EnsureRefusal, EnsureSpec, EnsureState, Fulfiller,
        FulfillerPhase, FulfillerStatus, Fulfillers, Hold, MemoryEnsureWorld, Record, Resume, Retire, Teardown, Unbind, UnbindReason,
    },
    Coded, Diagram, Home, Machine, Ref, WriteError,
};

fn at(micros: i64) -> Timestamp {
    Timestamp::from_micros(micros)
}

/// The inbox the Ensure keeps bound; the world keys its drain by this text.
const INBOX: &str = "project:p/role:r";

fn home() -> Home<Ensure> {
    let mut home = Home::<Ensure>::new("kiwi").expect("valid declaration");
    home.create("e", EnsureSpec { inbox: INBOX.parse().expect("canonical inbox") }, &MemoryEnsureWorld::default(), Timestamp(0))
        .expect("created");
    home
}

fn f(name: &str) -> Ref<Fulfiller> {
    Ref::new(name)
}

fn set(names: &[&str]) -> Fulfillers {
    names.iter().map(|name| f(name)).collect()
}

fn status(phase: FulfillerPhase, can_receive: bool) -> FulfillerStatus {
    FulfillerStatus { phase, can_receive }
}

/// Unfulfilled with these fulfillers.
fn unfulfilled(recorded: &[&str], created: &[&str]) -> EnsureState {
    EnsureState::Unfulfilled { recorded: set(recorded), created: set(created) }
}

fn resume() -> Resume {
    Resume { by: "operator".into(), authority: Ref::new("operator") }
}

fn retire() -> Retire {
    Retire { by: "operator".into(), authority: Ref::new("operator"), teardown: Ref::new("e") }
}

fn authorised(world: &mut MemoryEnsureWorld, yes: bool) {
    world.authorities.put(&Ref::<Authority>::new("operator"), yes);
}

fn refused(refusal: EnsureRefusal) -> Option<WriteError<Ensure>> {
    Some(WriteError::Refused(refusal))
}

fn state(home: &Home<Ensure>) -> &EnsureState {
    home.get("e").expect("exists").state()
}

/// A home with fulfiller `a` recorded and bound, and a world in which it is realised.
fn bound_home() -> (Home<Ensure>, MemoryEnsureWorld) {
    let mut h = home();
    let mut world = MemoryEnsureWorld::default();
    h.apply("e", Record { fulfiller: f("a") }, &world, at(1)).expect("recorded");
    world.fulfillers.put(&f("a"), status(FulfillerPhase::Realised, true));
    world.drains.put(&Ref::<Drain>::new(INBOX), true);
    h.apply("e", Bind { fulfiller: f("a") }, &world, at(2)).expect("bound");
    (h, world)
}

// The declaration obeys the engine's rules, and `record` and `hold` are
// ungated by a stated choice rather than an always-true gate.
#[test]
fn the_declaration_validates_and_record_states_why_it_is_ungated() {
    let definition = Ensure::definition();
    assert_eq!(definition.validate(), Ok(()));
    let record = definition.find(EnsureEdgeKind::Record).expect("declared");
    assert_eq!(
        (record.gate_doc(), record.ungated_reason()),
        (None, Some("naming a fulfiller before asking for it promises nothing, so any may be recorded"))
    );
    let hold = definition.find(EnsureEdgeKind::Hold).expect("declared");
    assert_eq!(
        hold.ungated_reason(),
        Some("the keeper holds on the strike run it derives from its fulfillers' endings, which are their own record")
    );
    let ungated = [EnsureEdgeKind::Record, EnsureEdgeKind::Hold];
    for kind in EnsureEdgeKind::ALL.iter().filter(|kind| !ungated.contains(kind)) {
        assert!(definition.find(*kind).expect("declared").gate_doc().is_some(), "{kind:?} is gated");
    }
}

// Codes are stable identities: a rename must never change them.
#[test]
fn codes_and_names_are_pinned() {
    let labels: Vec<_> = EnsureLabel::ALL.iter().map(|s| (s.name(), s.code())).collect();
    assert_eq!(
        labels,
        [("Unfulfilled", Code::new("un")), ("Fulfilled", Code::new("fu")), ("Held", Code::new("he")), ("Retired", Code::new("rt"))]
    );
    let edges: Vec<_> = EnsureEdgeKind::ALL.iter().map(|k| (k.name(), k.code())).collect();
    let codes = [("record", "rc"), ("create", "cr"), ("bind", "bi"), ("unbind", "ub"), ("hold", "ho"), ("resume", "rs"), ("retire", "rt")];
    assert_eq!(edges, codes.map(|(name, code)| (name, Code::new(code))));
}

// The diagram shows the ADR's table with each edge's changed data, retire
// from every live state; the labels view leaves out record and create, which
// change only data.
#[test]
fn mermaid_is_pinned() {
    let head = "---\ntitle: Ensure\n---\nstateDiagram-v2\n  direction LR\n  [*] --> Unfulfilled\n";
    let data = "  Unfulfilled --> Unfulfilled: record [recorded]\n  Unfulfilled --> Unfulfilled: create [created]\n";
    let binds = "  Unfulfilled --> Fulfilled: bind [bound]\n  Fulfilled --> Unfulfilled: unbind\n";
    let moves = "  Unfulfilled --> Held: hold\n\
                 \x20 Held --> Unfulfilled: resume\n\
                 \x20 Unfulfilled --> Retired: retire\n\
                 \x20 Fulfilled --> Retired: retire\n\
                 \x20 Held --> Retired: retire\n";
    let ends = "  Retired --> [*]\n  note right of Retired: the ensure is retired\n";
    let definition = Ensure::definition();
    assert_eq!(definition.mermaid(Diagram::Full), format!("{head}{data}{binds}{moves}{ends}"));
    assert_eq!(definition.mermaid(Diagram::Labels), format!("{head}{binds}{moves}{ends}"));
}

// An Ensure is admitted Unfulfilled with empty work; recording changes only
// the recorded set and keeps the label.
#[test]
fn record_adds_to_the_recorded_set_and_keeps_the_label() {
    let mut h = home();
    assert_eq!(*state(&h), unfulfilled(&[], &[]));
    let world = MemoryEnsureWorld::default();
    let event = h.apply("e", Record { fulfiller: f("a") }, &world, at(1)).expect("recorded");
    assert_eq!((event.from, event.to), (EnsureLabel::Unfulfilled, EnsureLabel::Unfulfilled));
    assert!(event.arguments.reads.is_empty(), "an ungated record reads nothing");
    h.apply("e", Record { fulfiller: f("a") }, &world, at(2)).expect("recorded again");
    assert_eq!(*state(&h), unfulfilled(&["a"], &[]));
}

// An Ensure records each fulfiller before it asks for it, and creates one
// only when none it recorded is live and the fulfiller does not yet exist.
#[test]
fn create_needs_a_recorded_absent_fulfiller_and_none_live() {
    let mut h = home();
    let mut world = MemoryEnsureWorld::default();
    assert_eq!(h.apply("e", Create { fulfiller: f("a") }, &world, at(1)).err(), refused(EnsureRefusal::NotRecorded));
    h.apply("e", Record { fulfiller: f("a") }, &world, at(1)).expect("recorded");
    h.apply("e", Record { fulfiller: f("b") }, &world, at(1)).expect("recorded");
    // Another recorded fulfiller is live.
    world.fulfillers.put(&f("b"), status(FulfillerPhase::Requested, false));
    assert_eq!(h.apply("e", Create { fulfiller: f("a") }, &world, at(2)).err(), refused(EnsureRefusal::FulfillerLive));
    world.fulfillers.put(&f("b"), status(FulfillerPhase::Realised, true));
    assert_eq!(h.apply("e", Create { fulfiller: f("a") }, &world, at(2)).err(), refused(EnsureRefusal::FulfillerLive));
    // None live, but the target exists (gone): not absent.
    world.fulfillers.put(&f("b"), status(FulfillerPhase::GaveUp, false));
    world.fulfillers.put(&f("a"), status(FulfillerPhase::Gone, false));
    assert_eq!(h.apply("e", Create { fulfiller: f("a") }, &world, at(2)).err(), refused(EnsureRefusal::FulfillerLive));
    world.fulfillers.remove(&f("a"));
    h.apply("e", Create { fulfiller: f("a") }, &world, at(3)).expect("created");
    assert_eq!(*state(&h), unfulfilled(&["a", "b"], &["a"]));
}

// Bind needs a recorded fulfiller that is realised and can receive, after
// the inbox has drained; it carries the work over and names the bound one.
#[test]
fn bind_needs_a_realised_receiving_fulfiller_and_a_drained_inbox() {
    let mut h = home();
    let mut world = MemoryEnsureWorld::default();
    world.fulfillers.put(&f("a"), status(FulfillerPhase::Realised, true));
    world.drains.put(&Ref::<Drain>::new(INBOX), true);
    assert_eq!(h.apply("e", Bind { fulfiller: f("a") }, &world, at(1)).err(), refused(EnsureRefusal::NotRecorded));
    h.apply("e", Record { fulfiller: f("a") }, &world, at(1)).expect("recorded");
    for (phase, can_receive, drained) in [
        (FulfillerPhase::Requested, true, true),
        (FulfillerPhase::Gone, true, true),
        (FulfillerPhase::Realised, false, true),
        (FulfillerPhase::Realised, true, false),
    ] {
        let mut world = MemoryEnsureWorld::default();
        world.fulfillers.put(&f("a"), status(phase, can_receive));
        world.drains.put(&Ref::<Drain>::new(INBOX), drained);
        assert_eq!(h.apply("e", Bind { fulfiller: f("a") }, &world, at(2)).err(), refused(EnsureRefusal::MissingEvidence));
    }
    assert_eq!(h.apply("e", Bind { fulfiller: f("b") }, &world, at(2)).err(), refused(EnsureRefusal::NotRecorded));
    h.apply("e", Bind { fulfiller: f("a") }, &world, at(3)).expect("bound");
    let fulfilled = EnsureState::Fulfilled { recorded: set(&["a"]), created: set(&[]), bound: f("a") };
    assert_eq!(*state(&h), fulfilled);
    // Only Unfulfilled accepts record, create, bind and hold.
    assert_eq!(h.apply("e", Record { fulfiller: f("b") }, &world, at(4)).err(), refused(EnsureRefusal::WrongState));
    assert_eq!(h.apply("e", Bind { fulfiller: f("a") }, &world, at(4)).err(), refused(EnsureRefusal::WrongState));
    assert_eq!(h.apply("e", Hold, &world, at(4)).err(), refused(EnsureRefusal::WrongState));
}

// Unbind needs the bound fulfiller observed gone; it then frees the Ensure,
// keeping its work and binding nothing.
#[test]
fn unbind_needs_the_bound_fulfiller_gone() {
    let (mut h, mut world) = bound_home();
    let unbind = |name: &str| Unbind { reason: UnbindReason::Gone, fulfiller: f(name) };
    assert_eq!(h.apply("e", unbind("a"), &world, at(3)).err(), refused(EnsureRefusal::MissingEvidence));
    world.fulfillers.put(&f("b"), status(FulfillerPhase::Gone, false));
    assert_eq!(h.apply("e", unbind("b"), &world, at(3)).err(), refused(EnsureRefusal::MissingEvidence));
    world.fulfillers.put(&f("a"), status(FulfillerPhase::Gone, false));
    h.apply("e", Unbind { reason: UnbindReason::Replaced, fulfiller: f("a") }, &world, at(4)).expect("unbound");
    assert_eq!(*state(&h), unfulfilled(&["a"], &[]));
    assert_eq!(state(&h).bound(), None);
    assert_eq!(h.apply("e", unbind("a"), &world, at(5)).err(), refused(EnsureRefusal::WrongState));
}

// Hold is the keeper's judgement, taken from Unfulfilled: the Ensure counts
// no failures of its own, so it reads nothing and keeps the work. A bound
// Ensure is unbound first.
#[test]
fn hold_is_taken_from_unfulfilled_and_keeps_the_work() {
    let world = MemoryEnsureWorld::default();
    let mut h = home();
    h.apply("e", Record { fulfiller: f("a") }, &world, at(1)).expect("recorded");
    let event = h.apply("e", Hold, &world, at(2)).expect("held");
    assert_eq!((event.from, event.to), (EnsureLabel::Unfulfilled, EnsureLabel::Held));
    assert!(event.arguments.reads.is_empty(), "an ungated hold reads nothing");
    assert_eq!(*state(&h), EnsureState::Held { recorded: set(&["a"]), created: set(&[]) });
    assert_eq!(h.apply("e", Hold, &world, at(3)).err(), refused(EnsureRefusal::WrongState), "held once");
    let (mut bound, _) = bound_home();
    assert_eq!(bound.apply("e", Hold, &world, at(3)).err(), refused(EnsureRefusal::WrongState), "a bound Ensure is unbound first");
}

// Held lasts until an authorised resume, which keeps the work. Nothing else
// leaves Held but a retire.
#[test]
fn resume_needs_authority_and_keeps_the_work() {
    let mut h = home();
    let mut world = MemoryEnsureWorld::default();
    assert_eq!(h.apply("e", resume(), &world, at(1)).err(), refused(EnsureRefusal::WrongState));
    h.apply("e", Record { fulfiller: f("a") }, &world, at(1)).expect("recorded");
    h.apply("e", Hold, &world, at(2)).expect("held");
    assert_eq!(h.apply("e", Record { fulfiller: f("b") }, &world, at(3)).err(), refused(EnsureRefusal::WrongState));
    assert_eq!(h.apply("e", resume(), &world, at(3)).err(), refused(EnsureRefusal::Unauthorised));
    authorised(&mut world, false);
    assert_eq!(h.apply("e", resume(), &world, at(3)).err(), refused(EnsureRefusal::Unauthorised));
    authorised(&mut world, true);
    h.apply("e", resume(), &world, at(4)).expect("resumed");
    assert_eq!(*state(&h), unfulfilled(&["a"], &[]));
    h.apply("e", Hold, &world, at(5)).expect("held again");
}

// Retire needs authority first, then a complete teardown, from every live
// state, and keeps the work; Retired is an end that refuses everything with
// its reason.
#[test]
fn retire_needs_authority_then_teardown_and_ends() {
    let mut held = home();
    held.apply("e", Hold, &MemoryEnsureWorld::default(), at(2)).expect("held");
    for (mut h, recorded) in [(home(), set(&[])), (bound_home().0, set(&["a"])), (held, set(&[]))] {
        let mut world = MemoryEnsureWorld::default();
        world.teardowns.put(&Ref::<Teardown>::new("e"), true);
        assert_eq!(h.apply("e", retire(), &world, at(3)).err(), refused(EnsureRefusal::Unauthorised));
        authorised(&mut world, true);
        world.teardowns.put(&Ref::<Teardown>::new("e"), false);
        assert_eq!(h.apply("e", retire(), &world, at(3)).err(), refused(EnsureRefusal::MissingEvidence));
        world.teardowns.remove(&Ref::<Teardown>::new("e"));
        assert_eq!(h.apply("e", retire(), &world, at(3)).err(), refused(EnsureRefusal::MissingEvidence));
        world.teardowns.put(&Ref::<Teardown>::new("e"), true);
        h.apply("e", retire(), &world, at(4)).expect("retired");
        // The spec's `bound` is 0 outside Fulfilled: a retired Ensure binds nothing.
        assert_eq!(*state(&h), EnsureState::Retired { recorded, created: set(&[]) });
        let ended = refused(EnsureRefusal::Ended("the ensure is retired"));
        assert_eq!(h.apply("e", retire(), &world, at(5)).err(), ended);
        assert_eq!(h.apply("e", resume(), &world, at(5)).err(), ended);
        assert_eq!(h.apply("e", Record { fulfiller: f("z") }, &world, at(5)).err(), ended);
        assert_eq!(h.apply("e", Hold, &world, at(5)).err(), ended);
    }
}
