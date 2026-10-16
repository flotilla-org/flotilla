//! The shared lifecycle against `specs/primitives/machines/lifecycle.qnt`.

use flotilla_schema::{CodeEnum, Timestamp};
use flotilla_state_machines::{
    changed,
    machines::lifecycle::{
        Absence, GiveUp, Lifecycle, LifecycleEdgeKind, LifecycleKind, LifecycleLabel, LifecycleRefusal, LifecycleState,
        MemoryLifecycleWorld, Observed, Realise, Release, Teardown, Vanish,
    },
    Attempt, AttemptPolicy, Cause, CauseCategory, Coded, Diagram, Home, InputRead, Machine, Ref, StateValue, Version, WriteError,
};

/// A kind for tests with an opaque request: one failure exhausts, as in the
/// conformance mapping.
struct Probe;
impl LifecycleKind for Probe {
    const NAME: &'static str = "Probe";
    type Request = String;
    fn policy() -> AttemptPolicy {
        AttemptPolicy::immediate(1)
    }
}

/// A second kind on the default policy, to show kinds share one declaration.
struct Other;
impl LifecycleKind for Other {
    const NAME: &'static str = "Other";
    type Request = (u32, u32);
}

type P = Lifecycle<Probe>;

fn at(micros: i64) -> Timestamp {
    Timestamp::from_micros(micros)
}

fn cause(detail: &str) -> Cause {
    Cause::new(CauseCategory::Transient, detail)
}

fn home() -> Home<P> {
    let mut home = Home::new("kiwi").expect("valid declaration");
    home.create("r", "opaque request".to_string()).expect("created");
    home
}

fn state(h: &Home<P>) -> &LifecycleState {
    h.get("r").expect("exists").state()
}

fn realise(identity: &str) -> Realise {
    Realise { identity: identity.into(), observed: Ref::new("r") }
}

fn release() -> Release {
    Release { teardown: Ref::new("r") }
}

fn vanish() -> Vanish {
    Vanish { observed: Ref::new("r") }
}

fn observed(identity: &str) -> MemoryLifecycleWorld {
    let mut world = MemoryLifecycleWorld::default();
    world.realisations.put(&Ref::new("r"), Observed { identity: identity.into() });
    world
}

fn refused(refusal: LifecycleRefusal) -> Option<WriteError<P>> {
    Some(WriteError::Refused(refusal))
}

// Every kind shares one declaration, and it follows the engine's rules; the
// machine takes its name from the kind.
#[test]
fn every_kind_validates() {
    assert_eq!(Lifecycle::<Probe>::definition().validate(), Ok(()));
    assert_eq!(Lifecycle::<Other>::definition().validate(), Ok(()));
    assert_eq!((Lifecycle::<Probe>::NAME, Lifecycle::<Other>::NAME), ("Probe", "Other"));
}

// Codes are stable identities: a rename must never change them. Each label
// carries the spec's data.
#[test]
fn codes_and_names_are_pinned() {
    let labels: Vec<_> = LifecycleLabel::ALL.iter().map(|s| (s.name(), s.code(), LifecycleState::fields(*s))).collect();
    let expected: [(&str, i64, &[&str]); 4] =
        [("Requested", 1, &[]), ("Realised", 2, &["identity"]), ("Gone", 3, &[]), ("GaveUp", 4, &["cause"])];
    assert_eq!(labels, expected);
    let edges: Vec<_> = LifecycleEdgeKind::ALL.iter().map(|k| (k.name(), k.code())).collect();
    assert_eq!(edges, [("realise", 1), ("release", 2), ("vanish", 3), ("give_up", 4)]);
}

// The diagram shows the ADR's table: realise, release from both live states,
// vanish from Realised, give up from Requested, the data each changes, and
// both ends. No edge keeps its label, so both drawings are the same.
#[test]
fn the_diagram_is_pinned() {
    let expected = "---
title: Probe
---
stateDiagram-v2
  direction LR
  [*] --> Requested
  Requested --> Realised: realise [identity]
  Requested --> Gone: release
  Realised --> Gone: release
  Realised --> Gone: vanish
  Requested --> GaveUp: give_up [cause]
  Gone --> [*]
  note right of Gone: the thing is gone
  GaveUp --> [*]
  note right of GaveUp: the request gave up
";
    assert_eq!(P::definition().mermaid(Diagram::Full), expected);
    assert_eq!(P::definition().mermaid(Diagram::Labels), expected);
}

// Admission keeps the opaque request as the spec and starts Requested.
#[test]
fn admission_starts_requested() {
    let h = home();
    let r = h.get("r").expect("exists");
    assert_eq!((r.spec().as_str(), r.state()), ("opaque request", &LifecycleState::Requested));
    assert!(r.state().live() && !r.state().realised() && !r.state().gone());
}

// Realise needs the home's observation under the edge's identity, and the
// state then carries that identity.
#[test]
fn realise_needs_a_matching_observation() {
    let mut h = home();
    assert_eq!(h.apply("r", realise("a"), &MemoryLifecycleWorld::default(), at(1)).err(), refused(LifecycleRefusal::MissingEvidence));
    assert_eq!(h.apply("r", realise("a"), &observed("b"), at(1)).err(), refused(LifecycleRefusal::MissingEvidence));
    let event = h.apply("r", realise("a"), &observed("a"), at(2)).expect("realised");
    assert_eq!(event.payload.reads, [InputRead { kind: "realisation".into(), name: "r".into(), version: Some(Version(1)) }]);
    assert_eq!(state(&h), &LifecycleState::Realised { identity: "a".into() });
    assert_eq!(state(&h).identity(), Some("a"));
    assert!(state(&h).live() && state(&h).realised() && !state(&h).gone());
    // Realise leaves only Requested.
    assert_eq!(h.apply("r", realise("b"), &observed("b"), at(3)).err(), refused(LifecycleRefusal::WrongState));
}

// Release from Requested or Realised needs a completed teardown.
#[test]
fn release_needs_a_complete_teardown() {
    for realise_first in [false, true] {
        let mut h = home();
        if realise_first {
            h.apply("r", realise("a"), &observed("a"), at(1)).expect("realised");
        }
        let mut world = MemoryLifecycleWorld::default();
        assert_eq!(h.apply("r", release(), &world, at(2)).err(), refused(LifecycleRefusal::MissingEvidence));
        world.teardowns.put(&Ref::<Teardown>::new("r"), false);
        assert_eq!(h.apply("r", release(), &world, at(2)).err(), refused(LifecycleRefusal::MissingEvidence));
        world.teardowns.put(&Ref::<Teardown>::new("r"), true);
        h.apply("r", release(), &world, at(3)).expect("released");
        assert_eq!(state(&h), &LifecycleState::Gone);
        assert!(state(&h).gone() && !state(&h).live());
    }
}

// Leaving Realised for Gone changes no data: the identity goes with the
// label, so `release` declares no changes and the engine accepts it.
#[test]
fn going_leaves_the_identity_behind_without_changing_data() {
    let realised = LifecycleState::Realised { identity: "a".into() };
    assert!(changed(&realised, &LifecycleState::Gone).is_empty());
    assert_eq!(changed(&LifecycleState::Requested, &realised).into_iter().collect::<Vec<_>>(), ["identity"]);
    let declared = P::definition();
    let release = declared.find(LifecycleEdgeKind::Release).expect("declared");
    assert!(release.changes.is_empty());
}

// Vanish leaves only Realised, and needs the thing observed absent.
#[test]
fn vanish_needs_an_observed_absence_from_realised() {
    let mut h = home();
    let mut world = MemoryLifecycleWorld::default();
    world.absences.put(&Ref::<Absence>::new("r"), true);
    assert_eq!(h.apply("r", vanish(), &world, at(1)).err(), refused(LifecycleRefusal::WrongState));
    h.apply("r", realise("a"), &observed("a"), at(1)).expect("realised");
    let mut present = MemoryLifecycleWorld::default();
    present.absences.put(&Ref::<Absence>::new("r"), false);
    assert_eq!(h.apply("r", vanish(), &present, at(2)).err(), refused(LifecycleRefusal::MissingEvidence));
    assert_eq!(h.apply("r", vanish(), &MemoryLifecycleWorld::default(), at(2)).err(), refused(LifecycleRefusal::MissingEvidence));
    h.apply("r", vanish(), &world, at(3)).expect("vanished");
    assert_eq!(state(&h), &LifecycleState::Gone);
}

// Give up needs the retry judgement exhausted with the edge's cause as the
// last failure, and the state then carries that cause.
#[test]
fn give_up_needs_exhausted_attempts_with_that_cause() {
    let world = MemoryLifecycleWorld::default();
    let mut h = home();
    let give_up = GiveUp { cause: cause("boom") };
    assert_eq!(h.apply("r", give_up.clone(), &world, at(1)).err(), refused(LifecycleRefusal::MissingEvidence));
    h.record_attempt("r", Attempt::failed(at(1), cause("other"), true)).expect("recorded");
    assert_eq!(h.apply("r", give_up.clone(), &world, at(2)).err(), refused(LifecycleRefusal::MissingEvidence));
    h.record_attempt("r", Attempt::succeeded(at(2))).expect("recorded");
    assert_eq!(h.apply("r", give_up.clone(), &world, at(3)).err(), refused(LifecycleRefusal::MissingEvidence));
    h.record_attempt("r", Attempt::failed(at(3), cause("boom"), true)).expect("recorded");
    h.apply("r", give_up, &world, at(4)).expect("gave up");
    assert_eq!(state(&h), &LifecycleState::GaveUp { cause: cause("boom") });
    assert_eq!(state(&h).cause(), Some(&cause("boom")));
    // Give up leaves only Requested.
    let mut h = home();
    h.apply("r", realise("a"), &observed("a"), at(1)).expect("realised");
    assert_eq!(h.apply("r", GiveUp { cause: cause("boom") }, &world, at(2)).err(), refused(LifecycleRefusal::WrongState));
}

// The default policy allows three consecutive failures: two leave give-up
// refused, the third exhausts it.
#[test]
fn the_default_policy_gives_up_after_three_failures() {
    let world = MemoryLifecycleWorld::default();
    let mut h: Home<Lifecycle<Other>> = Home::new("kiwi").expect("valid declaration");
    h.create("r", (1, 2)).expect("created");
    for n in 1..=3 {
        let refusal = h.apply("r", GiveUp { cause: cause("boom") }, &world, at(n)).err();
        assert_eq!(refusal, Some(WriteError::Refused(LifecycleRefusal::MissingEvidence)), "after {} failures", n - 1);
        h.record_attempt("r", Attempt::failed(at(n), cause("boom"), true)).expect("recorded");
    }
    h.apply("r", GiveUp { cause: cause("boom") }, &world, at(4)).expect("gave up");
}

// Gone and GaveUp are ends: every edge and every attempt is refused with the end's reason.
#[test]
fn ends_refuse_everything() {
    let mut world = observed("a");
    world.teardowns.put(&Ref::<Teardown>::new("r"), true);
    world.absences.put(&Ref::<Absence>::new("r"), true);
    let mut gone_home = home();
    gone_home.apply("r", release(), &world, at(1)).expect("released");
    let mut gave_up = home();
    gave_up.record_attempt("r", Attempt::failed(at(1), cause("boom"), true)).expect("recorded");
    gave_up.apply("r", GiveUp { cause: cause("boom") }, &world, at(1)).expect("gave up");
    for (mut h, reason) in [(gone_home, "the thing is gone"), (gave_up, "the request gave up")] {
        let ended = refused(LifecycleRefusal::Ended(reason));
        assert_eq!(h.apply("r", realise("a"), &world, at(2)).err(), ended);
        assert_eq!(h.apply("r", release(), &world, at(2)).err(), ended);
        assert_eq!(h.apply("r", vanish(), &world, at(2)).err(), ended);
        assert_eq!(h.apply("r", GiveUp { cause: cause("boom") }, &world, at(2)).err(), ended);
        assert_eq!(h.record_attempt("r", Attempt::succeeded(at(2))).err(), ended);
    }
}

// Attempts are recorded only while Requested.
#[test]
fn attempts_only_while_requested() {
    let mut h = home();
    h.apply("r", realise("a"), &observed("a"), at(1)).expect("realised");
    assert_eq!(h.record_attempt("r", Attempt::succeeded(at(2))).err(), refused(LifecycleRefusal::WrongState));
}
