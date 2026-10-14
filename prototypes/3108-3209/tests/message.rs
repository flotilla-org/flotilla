//! The Message machine: chosen targets, gates per drop reason, ends, delivery modes.

use state_machine_prototype::attempt::{Cause, CauseCategory, Outcome};
use state_machine_prototype::base::{ObjRef, Time};
use state_machine_prototype::home::{Home, Refusal};
use state_machine_prototype::kinds::message::*;
use state_machine_prototype::view::{Fact, InputRead, MemoryView, Transport};

fn spec(expectation: Expectation) -> MessageSpec {
    MessageSpec {
        sender: "coder".into(),
        inbox: "flotilla/C/reviewer".into(),
        body: "ready for review".into(),
        expectation,
        deadline: None,
    }
}

fn setup(expectation: Expectation) -> (Home<Message>, ObjRef, MemoryView) {
    let mut home = Home::<Message>::new("kiwi").unwrap();
    let id = home.create("m1", spec(expectation)).unwrap();
    let mut view = MemoryView::new();
    view.set_now(Time(1_000));
    (home, id, view)
}

fn receipt() -> ObjRef {
    ObjRef::new("receipt", "S/1")
}

fn deliver(transport: Transport) -> MessageEdge {
    MessageEdge::Deliver { receiver: "S".into(), receipt: receipt(), transport }
}

fn why<T>(r: Result<T, Refusal>) -> String {
    match r {
        Err(Refusal::Gate { why, .. }) => why.0,
        Err(other) => other.to_string(),
        Ok(_) => panic!("expected refusal"),
    }
}

fn see_receipt(view: &mut MemoryView, transport: Transport) {
    view.observe(&receipt(), "feta", Fact::Receipt { session: "S".into(), transport });
}

#[test]
fn deliver_goes_to_settled_or_awaiting_as_the_spec_expects() {
    for (exp, want) in [(Expectation::Nothing, MessageState::Settled), (Expectation::Reply, MessageState::Awaiting)] {
        let (mut home, id, mut view) = setup(exp);
        assert!(why(home.apply(&id, deliver(Transport::Turn), &view)).contains("no receipt"));
        see_receipt(&mut view, Transport::Turn);
        let ev = home.apply(&id, deliver(Transport::Turn), &view).unwrap();
        assert_eq!((ev.from, ev.to), (MessageState::Pending, want));
        assert_eq!(ev.inputs, vec![InputRead { subject: receipt(), version: 1 }]);
    }
}

#[test]
fn deliver_is_refused_for_a_receipt_of_another_session_or_transport() {
    let (mut home, id, mut view) = setup(Expectation::Nothing);
    see_receipt(&mut view, Transport::Muted);
    assert!(why(home.apply(&id, deliver(Transport::Turn), &view)).contains("another session or transport"));
    let other = MessageEdge::Deliver { receiver: "T".into(), receipt: receipt(), transport: Transport::Muted };
    assert!(why(home.apply(&id, other, &view)).contains("another session"));
}

#[test]
fn settle_requires_the_matching_observation() {
    let (mut home, id, mut view) = setup(Expectation::Reply);
    let reply = ObjRef::new("message", "r1");
    let settle = MessageEdge::Settle { by: Settlement::Answered { reply: reply.clone() } };
    assert!(matches!(home.apply(&id, settle.clone(), &view), Err(Refusal::WrongState { state: "Pending", .. })));
    see_receipt(&mut view, Transport::Turn);
    home.apply(&id, deliver(Transport::Turn), &view).unwrap();
    assert!(why(home.apply(&id, settle.clone(), &view)).contains("no reply"));
    // An outcome settlement does not fit a Reply expectation.
    let wrong = MessageEdge::Settle { by: Settlement::OutcomeObserved { observation: reply.clone() } };
    assert!(why(home.apply(&id, wrong, &view)).contains("does not match"));
    view.observe(&reply, "kiwi", Fact::Reply);
    home.apply(&id, settle, &view).unwrap();
    assert_eq!(home.get(&id).unwrap().state(), MessageState::Settled);
}

#[test]
fn a_demand_settles_when_its_condition_is_observed_met() {
    let cond = ObjRef::new("condition", "ci-green");
    let (mut home, id, mut view) = setup(Expectation::Outcome { condition: cond.clone() });
    see_receipt(&mut view, Transport::Turn);
    home.apply(&id, deliver(Transport::Turn), &view).unwrap();
    let settle = MessageEdge::Settle { by: Settlement::OutcomeObserved { observation: cond.clone() } };
    view.observe(&cond, "feta", Fact::OutcomeMet(false));
    assert!(why(home.apply(&id, settle.clone(), &view)).contains("outcome not observed"));
    view.observe(&cond, "feta", Fact::OutcomeMet(true));
    let ev = home.apply(&id, settle, &view).unwrap();
    assert_eq!(ev.inputs, vec![InputRead { subject: cond, version: 2 }]);
}

#[test]
fn drop_expired_needs_a_deadline_that_has_passed_and_an_unmet_expectation() {
    let cond = ObjRef::new("condition", "c");
    let mut s = spec(Expectation::Outcome { condition: cond.clone() });
    s.deadline = Some(Time(5_000));
    let mut home = Home::<Message>::new("kiwi").unwrap();
    let id = home.create("m1", s).unwrap();
    let mut view = MemoryView::new();
    view.set_now(Time(4_000));
    let drop = MessageEdge::Drop { reason: DropReason::Expired };
    assert!(why(home.apply(&id, drop.clone(), &view)).contains("deadline has not passed"));
    view.set_now(Time(6_000));
    view.observe(&cond, "feta", Fact::OutcomeMet(true));
    assert!(why(home.apply(&id, drop.clone(), &view)).contains("expectation is met"));
    view.observe(&cond, "feta", Fact::OutcomeMet(false));
    home.apply(&id, drop, &view).unwrap();
    assert_eq!(home.get(&id).unwrap().state(), MessageState::Dropped);

    // No deadline at all: expiry is meaningless.
    let (mut home, id, view) = setup(Expectation::Reply);
    assert!(why(home.apply(&id, MessageEdge::Drop { reason: DropReason::Expired }, &view)).contains("no deadline"));
}

#[test]
fn drop_undeliverable_only_from_pending_and_only_when_attempts_are_exhausted() {
    let (mut home, id, view) = setup(Expectation::Reply);
    let cause = Cause::new(CauseCategory::Transient, "session unreachable");
    let drop = MessageEdge::Drop { reason: DropReason::Undeliverable { cause: cause.clone() } };
    assert!(why(home.apply(&id, drop.clone(), &view)).contains("not exhausted"));
    for _ in 0..5 {
        home.record_attempt(&id, "deliver", "S", Outcome::Failure { cause: cause.clone(), retryable: true }, &view).unwrap();
    }
    home.apply(&id, drop.clone(), &view).unwrap();

    // From Awaiting the same reason is refused by its gate, though `drop` itself is allowed there.
    let (mut home, id, mut view2) = setup(Expectation::Reply);
    see_receipt(&mut view2, Transport::Turn);
    home.apply(&id, deliver(Transport::Turn), &view2).unwrap();
    for _ in 0..5 {
        home.record_attempt(&id, "deliver", "S", Outcome::Failure { cause: cause.clone(), retryable: true }, &view2).unwrap();
    }
    assert!(why(home.apply(&id, drop, &view2)).contains("only to a Pending"));
}

#[test]
fn superseded_needs_the_newer_message_to_name_this_one() {
    let (mut home, id, mut view) = setup(Expectation::Reply);
    let newer = ObjRef::new("Message", "m2");
    let drop = MessageEdge::Drop { reason: DropReason::Superseded { by: newer.clone() } };
    assert!(why(home.apply(&id, drop.clone(), &view)).contains("does not name"));
    view.observe(&newer, "kiwi", Fact::Supersedes { older: ObjRef::new("Message", "m-other") });
    assert!(why(home.apply(&id, drop.clone(), &view)).contains("does not name"));
    view.observe(&newer, "kiwi", Fact::Supersedes { older: id.clone() });
    home.apply(&id, drop, &view).unwrap();
}

#[test]
fn a_late_supersede_on_a_settled_message_is_refused_with_a_visible_reason() {
    let (mut home, id, mut view) = setup(Expectation::Nothing);
    see_receipt(&mut view, Transport::Turn);
    home.apply(&id, deliver(Transport::Turn), &view).unwrap();
    let newer = ObjRef::new("Message", "m2");
    view.observe(&newer, "kiwi", Fact::Supersedes { older: id.clone() });
    let err = home.apply(&id, MessageEdge::Drop { reason: DropReason::Superseded { by: newer } }, &view).unwrap_err();
    assert_eq!(err, Refusal::End { state: "Settled", attempted: "drop".into(), reason: "the message did its job" });
    assert_eq!(home.get(&id).unwrap().state(), MessageState::Settled);
}

#[test]
fn dropped_is_an_end_too() {
    let mut s = spec(Expectation::Reply);
    s.deadline = Some(Time(10));
    let mut home = Home::<Message>::new("kiwi").unwrap();
    let id = home.create("m", s).unwrap();
    let mut view = MemoryView::new();
    view.set_now(Time(2_000));
    home.apply(&id, MessageEdge::Drop { reason: DropReason::Expired }, &view).unwrap();
    assert!(matches!(home.apply(&id, deliver(Transport::Turn), &view), Err(Refusal::End { state: "Dropped", .. })));
}

#[test]
fn muting_changes_the_transport_not_the_outcome() {
    let rules = vec![
        Rule { sender: Some("bot".into()), expects_something: None, mode: Mode::Mute },
        Rule { sender: None, expects_something: Some(false), mode: Mode::Notify },
    ];
    let mut bot = spec(Expectation::Reply);
    bot.sender = "bot".into();
    assert_eq!(choose_mode(&rules, &bot), Mode::Mute);
    assert_eq!(choose_mode(&rules, &spec(Expectation::Nothing)), Mode::Notify);
    assert_eq!(choose_mode(&rules, &spec(Expectation::Reply)), Mode::Turn, "default");

    for (mode, note) in [
        (Mode::Turn, "delivered"),
        (Mode::Notify, "delivered (listed for the recipient to read)"),
        (Mode::Mute, "delivered (muted by recipient)"),
    ] {
        let (mut home, id, mut view) = setup(Expectation::Nothing);
        see_receipt(&mut view, mode.transport());
        home.apply(&id, deliver(mode.transport()), &view).unwrap();
        let r = home.get(&id).unwrap();
        assert_eq!(r.state(), MessageState::Settled, "every mode reaches an end");
        assert_eq!(delivery_note(r), Some(note));
    }
}

#[test]
fn undeliverable_sends_a_dead_letter_to_the_supervisor_unless_the_sender_is_the_supervisor() {
    let cause = Cause::new(CauseCategory::Transient, "unreachable");
    let (mut home, id, view) = setup(Expectation::Nothing);
    for _ in 0..5 {
        home.record_attempt(&id, "deliver", "S", Outcome::Failure { cause: cause.clone(), retryable: true }, &view).unwrap();
    }
    assert_eq!(dead_letter(home.get(&id).unwrap(), "gov"), None, "not dropped yet");
    home.apply(&id, MessageEdge::Drop { reason: DropReason::Undeliverable { cause } }, &view).unwrap();
    let r = home.get(&id).unwrap();
    let dl = dead_letter(r, "gov").expect("dead letter");
    assert_eq!((dl.sender.as_str(), dl.inbox.as_str()), ("system", "gov"));
    assert!(dl.body.contains("unreachable"));
    assert_eq!(dead_letter(r, "coder"), None, "the sender is the supervisor and already knows");
}
