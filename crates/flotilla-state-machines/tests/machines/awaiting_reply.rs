//! AwaitingReply against `specs/primitives/machines/awaiting_reply.qnt`.

use flotilla_schema::{Address, Code, CodeEnum, Timestamp};
use flotilla_state_machines::{
    machines::{
        awaiting_reply::{
            Answer, Awaited, AwaitingReply, AwaitingReplyEdgeKind, AwaitingReplyLabel, AwaitingReplyRefusal, AwaitingReplySpec,
            AwaitingReplyState, Expire, MemoryAwaitingReplyWorld, Withdraw,
        },
        message::MessageRef,
    },
    Coded, Diagram, Home, InputRead, Machine, Ref, Version, WriteError,
};

const SENDER: &str = "project:p/role:sender";

fn at(micros: i64) -> Timestamp {
    Timestamp::from_micros(micros)
}

fn address(text: &str) -> Address {
    text.parse().expect("canonical address")
}

fn awaited() -> MessageRef {
    MessageRef::new("m1", "receiver-home")
}

fn reply() -> MessageRef {
    MessageRef::new("m2", "replier-home")
}

fn spec(deadline: Option<Timestamp>) -> AwaitingReplySpec {
    AwaitingReplySpec {
        message: awaited(),
        sender: address(SENDER),
        recipient: address("project:p/role:recipient"),
        subject: None,
        kind: "stall".into(),
        awaited: Awaited::Reply,
        deadline,
    }
}

fn home(deadline: Option<Timestamp>) -> Home<AwaitingReply> {
    let mut home = Home::<AwaitingReply>::new("sender-home").expect("valid declaration");
    home.create("r", spec(deadline), &MemoryAwaitingReplyWorld::default(), Timestamp(0)).expect("created");
    home
}

/// A world whose inbox `inbox` holds `message`, replying to `in_reply_to`.
fn landed(message: &MessageRef, inbox: &str, in_reply_to: Option<&MessageRef>) -> MemoryAwaitingReplyWorld {
    let mut world = MemoryAwaitingReplyWorld::default();
    world.land(message, &address(inbox), in_reply_to);
    world
}

fn authorised(authorised: bool) -> MemoryAwaitingReplyWorld {
    let mut world = MemoryAwaitingReplyWorld::default();
    world.authority.put(&Ref::new("owner"), authorised);
    world
}

fn withdraw(reason: &str) -> Withdraw {
    Withdraw { by: address(SENDER), reason: reason.into(), authority: Ref::new("owner") }
}

fn refused(refusal: AwaitingReplyRefusal) -> Option<WriteError<AwaitingReply>> {
    Some(WriteError::Refused(refusal))
}

fn state(home: &Home<AwaitingReply>) -> &AwaitingReplyState {
    home.get("r").expect("exists").state()
}

// The declaration obeys the engine's rules.
#[test]
fn the_declaration_validates() {
    assert_eq!(AwaitingReply::definition().validate(), Ok(()));
}

// Codes are stable identities: a rename must never change them.
#[test]
fn codes_and_names_are_pinned() {
    let labels: Vec<_> = AwaitingReplyLabel::ALL.iter().map(|s| (s.name(), s.code())).collect();
    let codes = [("Open", "op"), ("Answered", "an"), ("Expired", "ex"), ("Withdrawn", "wd")];
    assert_eq!(labels, codes.map(|(name, code)| (name, Code::new(code))));
    let edges: Vec<_> = AwaitingReplyEdgeKind::ALL.iter().map(|k| (k.name(), k.code())).collect();
    let codes = [("answer", "an"), ("expire", "ex"), ("withdraw", "wd")];
    assert_eq!(edges, codes.map(|(name, code)| (name, Code::new(code))));
}

// The diagram shows Open leaving once, to one of the three ends.
#[test]
fn mermaid_is_pinned() {
    let head = "---\ntitle: AwaitingReply\n---\nstateDiagram-v2\n  direction LR\n  [*] --> Open\n";
    let edges = "  Open --> Answered: answer\n  Open --> Expired: expire\n  Open --> Withdrawn: withdraw [reason]\n";
    let ends = "  Answered --> [*]\n  note right of Answered: a reply answered the record\n  Expired --> [*]\n  note right of Expired: the record expired unanswered\n  Withdrawn --> [*]\n  note right of Withdrawn: the sender withdrew the record\n";
    assert_eq!(AwaitingReply::definition().mermaid(Diagram::Full), format!("{head}{edges}{ends}"));
}

// The send writes the record Open, exactly as sent, at the send's time.
#[test]
fn the_send_writes_the_record_open() {
    let h = home(Some(at(5)));
    let record = h.get("r").expect("exists");
    assert_eq!((record.spec(), record.state(), record.since()), (&spec(Some(at(5))), &AwaitingReplyState::Open, Timestamp(0)));
}

// Answer needs the reply in the sender's own inbox (a reply landed in another
// inbox is absent), then replying to the awaited message; it records the read.
#[test]
fn an_answer_needs_a_reply_to_the_message_in_the_senders_inbox() {
    let mut h = home(None);
    let answer = || Answer { reply: reply() };
    let absent = refused(AwaitingReplyRefusal::ReplyAbsent);
    assert_eq!(h.apply("r", answer(), &MemoryAwaitingReplyWorld::default(), at(1)).err(), absent);
    assert_eq!(h.apply("r", answer(), &landed(&reply(), "project:p/role:other", Some(&awaited())), at(1)).err(), absent);
    let other = MessageRef::new("m9", "receiver-home");
    let not_a_reply = refused(AwaitingReplyRefusal::NotAReply);
    assert_eq!(h.apply("r", answer(), &landed(&reply(), SENDER, Some(&other)), at(1)).err(), not_a_reply);
    assert_eq!(h.apply("r", answer(), &landed(&reply(), SENDER, None), at(1)).err(), not_a_reply);
    let event = h.apply("r", answer(), &landed(&reply(), SENDER, Some(&awaited())), at(2)).expect("answered");
    assert_eq!(event.arguments.reads, [InputRead { kind: "landed".into(), name: "m2@replier-home".into(), version: Some(Version(1)) }]);
    assert_eq!((state(&h), h.get("r").expect("exists").since()), (&AwaitingReplyState::Answered, at(2)));
}

// Expire needs a deadline, then the clock strictly past it, then no reply in
// the sender's inbox: a reply present wins over a passed deadline, and a reply
// in another inbox does not.
#[test]
fn expiry_follows_the_deadline_and_never_beats_a_reply() {
    let none = MemoryAwaitingReplyWorld::default();
    assert_eq!(home(None).apply("r", Expire, &none, at(9)).err(), refused(AwaitingReplyRefusal::NoDeadline));
    let mut h = home(Some(at(5)));
    assert_eq!(h.apply("r", Expire, &none, at(4)).err(), refused(AwaitingReplyRefusal::NotDue));
    assert_eq!(h.apply("r", Expire, &none, at(5)).err(), refused(AwaitingReplyRefusal::NotDue), "at the deadline is not past it");
    let present = landed(&reply(), SENDER, Some(&awaited()));
    assert_eq!(h.apply("r", Expire, &present, at(6)).err(), refused(AwaitingReplyRefusal::ReplyPresent));
    let elsewhere = landed(&reply(), "project:p/role:other", Some(&awaited()));
    let event = h.apply("r", Expire, &elsewhere, at(6)).expect("expired");
    assert_eq!(event.arguments.reads, [InputRead { kind: "replies".into(), name: "m1@receiver-home".into(), version: Some(Version(1)) }]);
    assert_eq!((state(&h), h.get("r").expect("exists").since()), (&AwaitingReplyState::Expired, at(6)));
}

// Withdraw needs the authority and a reason; Withdrawn carries the reason.
#[test]
fn a_withdrawal_needs_authority_and_a_reason() {
    let mut h = home(None);
    let unauthorised = refused(AwaitingReplyRefusal::Unauthorised);
    assert_eq!(h.apply("r", withdraw("superseded"), &MemoryAwaitingReplyWorld::default(), at(1)).err(), unauthorised);
    assert_eq!(h.apply("r", withdraw("superseded"), &authorised(false), at(1)).err(), unauthorised);
    assert_eq!(h.apply("r", withdraw(""), &authorised(true), at(1)).err(), unauthorised);
    h.apply("r", withdraw("superseded"), &authorised(true), at(2)).expect("withdrawn");
    assert_eq!(*state(&h), AwaitingReplyState::Withdrawn { reason: "superseded".into() });
}

// Once ended, every edge refuses NotOpen, whatever its evidence, and changes nothing.
#[test]
fn an_ended_record_refuses_everything_with_not_open() {
    let present = landed(&reply(), SENDER, Some(&awaited()));
    let mut h = home(Some(at(5)));
    h.apply("r", Answer { reply: reply() }, &present, at(1)).expect("answered");
    let before = h.get("r").expect("exists").clone();
    let not_open = refused(AwaitingReplyRefusal::NotOpen);
    assert_eq!(h.apply("r", Answer { reply: reply() }, &present, at(9)).err(), not_open);
    assert_eq!(h.apply("r", Expire, &MemoryAwaitingReplyWorld::default(), at(9)).err(), not_open);
    assert_eq!(h.apply("r", withdraw("superseded"), &authorised(true), at(9)).err(), not_open);
    assert_eq!(h.get("r"), Some(&before));
}
