//! Ensure against `specs/primitives/machines/ensure.qnt`.

use flotilla_schema::{Code, CodeEnum, Timestamp};
use flotilla_state_machines::{
    machines::ensure::{
        Attempted, Authority, Bind, Create, Drain, Ensure, EnsureEdgeKind, EnsureLabel, EnsureRefusal, EnsureSpec, EnsureState, Fulfiller,
        FulfillerPhase, FulfillerStatus, Fulfillers, Hold, MemoryEnsureWorld, Record, Resume, Retire, Teardown, Unbind, UnbindReason,
    },
    AttemptPolicy, Cause, CauseCategory, Coded, Diagram, Failure, Home, Machine, Outcome, Ref, WriteError,
};

fn at(micros: i64) -> Timestamp {
    Timestamp::from_micros(micros)
}

fn home() -> Home<Ensure> {
    let mut home = Home::new("kiwi").expect("valid declaration");
    home.create("e", EnsureSpec { inbox: "inbox".into(), policy: AttemptPolicy::immediate(2) }, Timestamp(0)).expect("created");
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

fn cause() -> Cause {
    Cause::new(CauseCategory::Transient, "create failed")
}

/// An attempt at fulfiller `a`.
fn attempted(outcome: Outcome) -> Attempted {
    Attempted { fulfiller: f("a"), outcome }
}

fn failed(retryable: bool) -> Attempted {
    attempted(Outcome::Failed { cause: cause(), retryable })
}

fn succeeded() -> Attempted {
    attempted(Outcome::Succeeded)
}

/// Unfulfilled with these fulfillers and no failures in the window.
fn unfulfilled(recorded: &[&str], created: &[&str]) -> EnsureState {
    EnsureState::Unfulfilled { recorded: set(recorded), created: set(created), failures: 0, last_failure: None }
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
    world.drains.put(&Ref::<Drain>::new("inbox"), true);
    h.apply("e", Bind { fulfiller: f("a") }, &world, at(2)).expect("bound");
    (h, world)
}

// The declaration obeys the engine's rules, and `record` and `attempted` are
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
    let attempted = definition.find(EnsureEdgeKind::Attempted).expect("declared");
    assert_eq!(attempted.ungated_reason(), Some("an attempt reports how a try ended; the hold gate judges the window it leaves"));
    let ungated = [EnsureEdgeKind::Record, EnsureEdgeKind::Attempted];
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
    let codes = [
        ("record", "rc"),
        ("create", "cr"),
        ("bind", "bi"),
        ("unbind", "ub"),
        ("hold", "ho"),
        ("resume", "rs"),
        ("retire", "rt"),
        ("attempted", "at"),
    ];
    assert_eq!(edges, codes.map(|(name, code)| (name, Code::new(code))));
}

// The diagram shows the ADR's table with each edge's changed data, retire
// from every live state; the labels view leaves out record, create and
// attempted, which change only data.
#[test]
fn mermaid_is_pinned() {
    let head = "---\ntitle: Ensure\n---\nstateDiagram-v2\n  direction LR\n  [*] --> Unfulfilled\n";
    let data = "  Unfulfilled --> Unfulfilled: record [recorded]\n  Unfulfilled --> Unfulfilled: create [created]\n";
    let binds = "  Unfulfilled --> Fulfilled: bind [bound]\n  Fulfilled --> Unfulfilled: unbind\n";
    let attempts = "  Unfulfilled --> Unfulfilled: attempted [failures, last_failure]\n";
    let moves = "  Unfulfilled --> Held: hold\n\
                 \x20 Held --> Unfulfilled: resume [failures, last_failure]\n\
                 \x20 Unfulfilled --> Retired: retire\n\
                 \x20 Fulfilled --> Retired: retire\n\
                 \x20 Held --> Retired: retire\n";
    let ends = "  Retired --> [*]\n  note right of Retired: the ensure is retired\n";
    let definition = Ensure::definition();
    assert_eq!(definition.mermaid(Diagram::Full), format!("{head}{data}{binds}{attempts}{moves}{ends}"));
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
    assert!(event.payload.reads.is_empty(), "an ungated record reads nothing");
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
    world.drains.put(&Ref::<Drain>::new("inbox"), true);
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
        world.drains.put(&Ref::<Drain>::new("inbox"), drained);
        assert_eq!(h.apply("e", Bind { fulfiller: f("a") }, &world, at(2)).err(), refused(EnsureRefusal::MissingEvidence));
    }
    assert_eq!(h.apply("e", Bind { fulfiller: f("b") }, &world, at(2)).err(), refused(EnsureRefusal::NotRecorded));
    h.apply("e", Bind { fulfiller: f("a") }, &world, at(3)).expect("bound");
    let fulfilled = EnsureState::Fulfilled { recorded: set(&["a"]), created: set(&[]), failures: 0, last_failure: None, bound: f("a") };
    assert_eq!(*state(&h), fulfilled);
    // Only Unfulfilled accepts record, create, bind, hold and attempts.
    assert_eq!(h.apply("e", Record { fulfiller: f("b") }, &world, at(4)).err(), refused(EnsureRefusal::WrongState));
    assert_eq!(h.apply("e", Bind { fulfiller: f("a") }, &world, at(4)).err(), refused(EnsureRefusal::WrongState));
    assert_eq!(h.apply("e", Hold, &world, at(4)).err(), refused(EnsureRefusal::WrongState));
    assert_eq!(h.apply("e", failed(true), &world, at(4)).err(), refused(EnsureRefusal::WrongState));
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

// Each attempt is an edge on the Ensure whose event carries its outcome, and
// whose effect keeps the window's tally: consecutive failures since the last
// success, and the latest failure with the edge's time.
#[test]
fn attempts_keep_the_windows_tally() {
    let mut h = home();
    let world = MemoryEnsureWorld::default();
    let event = h.apply("e", failed(true), &world, at(1)).expect("attempted").clone();
    assert_eq!((event.from, event.to), (EnsureLabel::Unfulfilled, EnsureLabel::Unfulfilled));
    assert!(event.payload.reads.is_empty(), "an ungated attempt reads nothing");
    let latest = Failure { at: at(1), cause: cause(), retryable: true };
    let tally = |h: &Home<Ensure>| (state(h).failures(), state(h).last_failure().cloned());
    assert_eq!(tally(&h), (1, Some(latest)));
    h.apply("e", succeeded(), &world, at(2)).expect("attempted");
    assert_eq!(tally(&h), (0, None), "a success clears the window's failures");
    h.apply("e", failed(true), &world, at(3)).expect("attempted");
    h.apply("e", failed(false), &world, at(4)).expect("attempted");
    assert_eq!(tally(&h), (2, Some(Failure { at: at(4), cause: cause(), retryable: false })));
    assert_eq!(h.get("e").expect("exists").events().len(), 4, "every attempt stays in the log");
}

// Hold needs an exhausted window (spec `exhausted`): the policy's consecutive
// failures since the last success, or a last failure that cannot be retried.
// A success between failures resets the count.
#[test]
fn hold_needs_an_exhausted_window() {
    let world = MemoryEnsureWorld::default();
    let mut h = home();
    assert_eq!(h.apply("e", Hold, &world, at(1)).err(), refused(EnsureRefusal::NotExhausted), "no attempt yet");
    h.apply("e", failed(true), &world, at(1)).expect("attempted");
    assert_eq!(h.apply("e", Hold, &world, at(2)).err(), refused(EnsureRefusal::NotExhausted), "one retryable failure of two");
    h.apply("e", succeeded(), &world, at(2)).expect("attempted");
    h.apply("e", failed(true), &world, at(3)).expect("attempted");
    assert_eq!(h.apply("e", Hold, &world, at(3)).err(), refused(EnsureRefusal::NotExhausted), "the success reset the count");
    h.apply("e", failed(true), &world, at(4)).expect("attempted");
    h.apply("e", Hold, &world, at(5)).expect("two consecutive failures");
    assert_eq!(
        *state(&h),
        EnsureState::Held {
            recorded: set(&[]),
            created: set(&[]),
            failures: 2,
            last_failure: Some(Failure { at: at(4), cause: cause(), retryable: true }),
        }
    );

    let mut h = home();
    h.apply("e", failed(false), &world, at(1)).expect("attempted");
    h.apply("e", Hold, &world, at(2)).expect("a failure that cannot be retried exhausts at once");
    let mut h = home();
    h.apply("e", failed(false), &world, at(1)).expect("attempted");
    h.apply("e", succeeded(), &world, at(2)).expect("attempted");
    assert_eq!(h.apply("e", Hold, &world, at(3)).err(), refused(EnsureRefusal::NotExhausted), "the last attempt succeeded");
}

// Binding and unbinding keep the window's tally (spec: only the suffix after
// the latest success counts, and only `resume` moves the window): failures
// before a bind still count after the unbind, until a success clears them.
#[test]
fn bind_and_unbind_keep_the_windows_tally() {
    let mut h = home();
    let mut world = MemoryEnsureWorld::default();
    h.apply("e", Record { fulfiller: f("a") }, &world, at(1)).expect("recorded");
    h.apply("e", failed(true), &world, at(2)).expect("attempted");
    world.fulfillers.put(&f("a"), status(FulfillerPhase::Realised, true));
    world.drains.put(&Ref::<Drain>::new("inbox"), true);
    h.apply("e", Bind { fulfiller: f("a") }, &world, at(3)).expect("bound");
    assert_eq!(state(&h).failures(), 1, "binding keeps the tally");
    world.fulfillers.put(&f("a"), status(FulfillerPhase::Gone, false));
    h.apply("e", Unbind { reason: UnbindReason::Gone, fulfiller: f("a") }, &world, at(4)).expect("unbound");
    assert_eq!(state(&h).failures(), 1, "unbinding keeps the tally");
    h.apply("e", failed(true), &world, at(5)).expect("attempted");
    h.apply("e", Hold, &world, at(6)).expect("the failure before the bind still counts");
    let mut h2 = home();
    h2.apply("e", failed(true), &world, at(1)).expect("attempted");
    h2.apply("e", succeeded(), &world, at(2)).expect("attempted");
    h2.apply("e", failed(true), &world, at(3)).expect("attempted");
    assert_eq!(h2.apply("e", Hold, &world, at(4)).err(), refused(EnsureRefusal::NotExhausted), "a success clears it");
}

// Held lasts until an authorised resume, which opens a new window: the old
// failures stay in the log but no longer exhaust it. Nothing else leaves
// Held but a retire.
#[test]
fn resume_opens_a_new_window() {
    let mut h = home();
    let mut world = MemoryEnsureWorld::default();
    assert_eq!(h.apply("e", resume(), &world, at(1)).err(), refused(EnsureRefusal::WrongState));
    h.apply("e", failed(false), &world, at(1)).expect("attempted");
    h.apply("e", Hold, &world, at(2)).expect("held");
    assert_eq!(h.apply("e", failed(true), &world, at(3)).err(), refused(EnsureRefusal::WrongState));
    assert_eq!(h.apply("e", succeeded(), &world, at(3)).err(), refused(EnsureRefusal::WrongState));
    assert_eq!(h.apply("e", resume(), &world, at(3)).err(), refused(EnsureRefusal::Unauthorised));
    authorised(&mut world, false);
    assert_eq!(h.apply("e", resume(), &world, at(3)).err(), refused(EnsureRefusal::Unauthorised));
    authorised(&mut world, true);
    h.apply("e", resume(), &world, at(4)).expect("resumed");
    assert_eq!(*state(&h), unfulfilled(&[], &[]));
    assert_eq!(h.apply("e", Hold, &world, at(5)).err(), refused(EnsureRefusal::NotExhausted));
    // A failure in the new window that cannot be retried exhausts it again.
    h.apply("e", failed(false), &world, at(6)).expect("attempted");
    h.apply("e", Hold, &world, at(7)).expect("held again");
    let attempts = h.get("e").expect("exists").events().iter().filter(|row| row.record.edge == EnsureEdgeKind::Attempted).count();
    assert_eq!(attempts, 2, "the old attempt stays in the log");
}

// Retire needs authority first, then a complete teardown, from every live
// state, and keeps the work; Retired is an end that refuses everything with
// its reason.
#[test]
fn retire_needs_authority_then_teardown_and_ends() {
    let mut held = home();
    held.apply("e", failed(false), &MemoryEnsureWorld::default(), at(1)).expect("attempted");
    held.apply("e", Hold, &MemoryEnsureWorld::default(), at(2)).expect("held");
    let failure = Some(Failure { at: at(1), cause: cause(), retryable: false });
    for (mut h, recorded, tally) in
        [(home(), set(&[]), (0, None)), (bound_home().0, set(&["a"]), (0, None)), (held, set(&[]), (1, failure))]
    {
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
        let (failures, last_failure) = tally;
        assert_eq!(*state(&h), EnsureState::Retired { recorded, created: set(&[]), failures, last_failure });
        let ended = refused(EnsureRefusal::Ended("the ensure is retired"));
        assert_eq!(h.apply("e", retire(), &world, at(5)).err(), ended);
        assert_eq!(h.apply("e", resume(), &world, at(5)).err(), ended);
        assert_eq!(h.apply("e", Record { fulfiller: f("z") }, &world, at(5)).err(), ended);
        assert_eq!(h.apply("e", Hold, &world, at(5)).err(), ended);
        assert_eq!(h.apply("e", failed(true), &world, at(5)).err(), ended);
    }
}
