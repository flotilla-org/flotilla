//! Message (spec `machines/message.qnt`; scenarios `inbox_delivery`, `message_race`;
//! ADR 0064 "Inboxes").

use flotilla_schema::{CodeEnum, Decoder, Encoder, Position, Timestamp, Value};
use flotilla_state_machines::{
    machines::message::{
        AppliedThrough, Binding, Condition, Deliver, DropMessage, DropReason, Expectation, InboxBinding, MemoryMessageWorld, Message,
        MessageEdge, MessageEdgeKind, MessageLabel, MessageRefusal, MessageSpec, MessageState, Receipts, Receiver, Settle, Settlement,
        Supersedes, Supersession,
    },
    Coded, Diagram, EdgeEventOf, EdgePayload, Home, InputRead, Kind, Machine, Receipt, Ref, Source, Transport, Version, WriteError,
};

const INBOX: &str = "project:p/role:r";

fn t(micros: i64) -> Timestamp {
    Timestamp::from_micros(micros)
}

fn condition() -> Ref<Condition> {
    Ref::new("build-green")
}

fn spec(expectation: Expectation, deadline: Option<i64>) -> MessageSpec {
    MessageSpec { inbox: INBOX.into(), expectation, deadline: deadline.map(t), unbound_after: 10 }
}

fn home(spec: MessageSpec) -> Home<Message> {
    let mut home = Home::<Message>::new("home").expect("valid declaration");
    home.create("m", spec, &MemoryMessageWorld::default(), Timestamp(0)).expect("created");
    home
}

fn session(name: &str) -> Receiver {
    Receiver { session: name.into(), host: "consumer".into() }
}

fn deliver(name: &str, transport: Transport) -> Deliver {
    Deliver { receiver: session(name), transport }
}

fn receipt(world: &mut MemoryMessageWorld, session: &str, transport: Transport) {
    let own = Ref::<Receipts>::new("m");
    let mut receipts = world.receipts.lookup(&own).map(|(_, by_session)| by_session.clone()).unwrap_or_default();
    receipts.insert(session.into(), transport);
    world.receipts.put(&own, receipts);
}

fn drop(reason: DropReason) -> DropMessage {
    DropMessage { reason }
}

fn refused(refusal: MessageRefusal) -> Option<WriteError<Message>> {
    Some(WriteError::Refused(refusal))
}

fn state(home: &Home<Message>) -> &MessageState {
    home.get("m").expect("exists").state()
}

fn bound(world: &mut MemoryMessageWorld) {
    world.binding.put(&Ref::new(INBOX), InboxBinding::Bound { session: "s1".into() });
}

/// A message expecting a reply, delivered to `s1`.
fn awaiting(deadline: Option<i64>) -> (Home<Message>, MemoryMessageWorld) {
    let mut home = home(spec(Expectation::Reply, deadline));
    let mut world = MemoryMessageWorld::default();
    receipt(&mut world, "s1", Transport::Turn);
    home.apply("m", deliver("s1", Transport::Turn), &world, t(1)).expect("delivered");
    (home, world)
}

// The declaration obeys the engine's rules.
#[test]
fn the_declaration_is_valid() {
    assert_eq!(Message::definition().validate(), Ok(()));
}

// Label and edge codes are stable: renames never change them.
#[test]
fn codes_are_pinned() {
    let labels: Vec<_> = MessageLabel::ALL.iter().map(|s| (s.name(), s.code())).collect();
    assert_eq!(labels, [("Pending", 1), ("Awaiting", 2), ("Settled", 3), ("Dropped", 4)]);
    let edges: Vec<_> = MessageEdgeKind::ALL.iter().map(|k| (k.name(), k.code())).collect();
    assert_eq!(edges, [("deliver", 1), ("settle", 2), ("drop", 3)]);
}

// The diagram shows deliver's two targets, settle and drop from both live
// states, with each edge's changed data. No edge keeps its label, so both
// views are the same.
#[test]
fn the_diagram_is_pinned() {
    let diagram = "---\ntitle: Message\n---\nstateDiagram-v2\n  direction LR\n  [*] --> Pending\n\
                   \x20 Pending --> Settled: deliver [receiver]\n\
                   \x20 Pending --> Awaiting: deliver [receiver]\n\
                   \x20 Pending --> Settled: settle\n\
                   \x20 Awaiting --> Settled: settle\n\
                   \x20 Pending --> Dropped: drop [reason, dropped_receiver, at, watermark]\n\
                   \x20 Awaiting --> Dropped: drop [reason, dropped_receiver, at, watermark]\n\
                   \x20 Settled --> [*]\n  note right of Settled: the message was settled\n\
                   \x20 Dropped --> [*]\n  note right of Dropped: the message was dropped\n";
    let definition = Message::definition();
    assert_eq!(definition.mermaid(Diagram::Full), diagram);
    assert_eq!(definition.mermaid(Diagram::Labels), diagram);
}

// Deliver needs the receiver's own receipt with the edge's transport: none,
// another session's, or another transport refuses IncorrectReceipt.
#[test]
fn deliver_needs_the_receivers_receipt() {
    let mut home = home(spec(Expectation::Reply, None));
    let mut world = MemoryMessageWorld::default();
    assert_eq!(home.apply("m", deliver("s1", Transport::Turn), &world, t(1)).err(), refused(MessageRefusal::IncorrectReceipt));
    receipt(&mut world, "s2", Transport::Turn);
    assert_eq!(home.apply("m", deliver("s1", Transport::Turn), &world, t(1)).err(), refused(MessageRefusal::IncorrectReceipt));
    assert_eq!(home.apply("m", deliver("s2", Transport::Notify), &world, t(1)).err(), refused(MessageRefusal::IncorrectReceipt));
    assert_eq!(*state(&home), MessageState::Pending);
}

// With a reply or outcome expected, delivery awaits it with the one
// receiver; a second delivery is refused. A receipt from a session no
// longer bound is accepted: there is no binding check.
#[test]
fn deliver_records_one_receiver_and_awaits() {
    let mut home = home(spec(Expectation::Reply, None));
    let mut world = MemoryMessageWorld::default();
    world.binding.put(&Ref::new(INBOX), InboxBinding::Bound { session: "replacement".into() });
    receipt(&mut world, "old", Transport::Mute);
    home.apply("m", deliver("old", Transport::Mute), &world, t(1)).expect("delivered");
    receipt(&mut world, "replacement", Transport::Turn);
    assert_eq!(home.apply("m", deliver("replacement", Transport::Turn), &world, t(2)).err(), refused(MessageRefusal::WrongState));
    assert_eq!(*state(&home), MessageState::Awaiting { receiver: session("old") });
    assert_eq!(home.get("m").expect("exists").events().len(), 1, "one delivery");
}

// With nothing expected, delivery settles the message, which then refuses
// every edge and record as Ended.
#[test]
fn delivering_an_expectation_free_message_settles_it() {
    let mut home = home(spec(Expectation::Nothing, Some(5)));
    let mut world = MemoryMessageWorld::default();
    receipt(&mut world, "s1", Transport::Notify);
    home.apply("m", deliver("s1", Transport::Notify), &world, t(1)).expect("delivered");
    assert_eq!(*state(&home), MessageState::Settled);
    let ended = refused(MessageRefusal::Ended);
    assert_eq!(home.apply("m", deliver("s1", Transport::Notify), &world, t(2)).err(), ended);
    assert_eq!(home.apply("m", drop(DropReason::Expired), &world, t(9)).err(), ended);
    let settle = Settle { by: Settlement::OutcomeObserved };
    assert_eq!(home.apply("m", settle, &world, t(2)).err(), ended);
    assert_eq!(home.record_receipt("m", Receipt { message: "m".into(), transport: Transport::Turn, at: t(3) }).err(), ended);
}

// Answered settles only after delivery (WrongState while Pending), only a
// message expecting a reply (UnsupportedSettlement), and only on an observed
// reply (MissingEvidence), in that order.
#[test]
fn an_answer_settles_an_awaiting_reply() {
    let answered = || Settle { by: Settlement::Answered };
    let mut world = MemoryMessageWorld::default();
    receipt(&mut world, "s1", Transport::Turn);

    let mut outcome = home(spec(Expectation::Outcome { condition: condition() }, None));
    assert_eq!(outcome.apply("m", answered(), &world, t(1)).err(), refused(MessageRefusal::WrongState));
    outcome.apply("m", deliver("s1", Transport::Turn), &world, t(1)).expect("delivered");
    assert_eq!(outcome.apply("m", answered(), &world, t(2)).err(), refused(MessageRefusal::UnsupportedSettlement));

    let mut reply = home(spec(Expectation::Reply, None));
    assert_eq!(reply.apply("m", answered(), &world, t(1)).err(), refused(MessageRefusal::WrongState));
    reply.apply("m", deliver("s1", Transport::Turn), &world, t(1)).expect("delivered");
    assert_eq!(reply.apply("m", answered(), &world, t(2)).err(), refused(MessageRefusal::MissingEvidence));
    world.reply.put(&Ref::new("m"), ());
    reply.apply("m", answered(), &world, t(3)).expect("settled");
    assert_eq!(*state(&reply), MessageState::Settled);
}

// An observed outcome settles a message expecting it, even while Pending (the
// consumer never sees the turn); it is unsupported for any other expectation.
#[test]
fn an_observed_outcome_settles_from_pending() {
    let observed = || Settle { by: Settlement::OutcomeObserved };
    let mut world = MemoryMessageWorld::default();
    let mut reply = home(spec(Expectation::Reply, None));
    assert_eq!(reply.apply("m", observed(), &world, t(1)).err(), refused(MessageRefusal::UnsupportedSettlement));
    let mut nothing = home(spec(Expectation::Nothing, None));
    assert_eq!(nothing.apply("m", observed(), &world, t(1)).err(), refused(MessageRefusal::UnsupportedSettlement));

    let mut outcome = home(spec(Expectation::Outcome { condition: condition() }, None));
    assert_eq!(outcome.apply("m", observed(), &world, t(1)).err(), refused(MessageRefusal::MissingEvidence));
    world.condition.put(&condition(), false);
    assert_eq!(outcome.apply("m", observed(), &world, t(1)).err(), refused(MessageRefusal::MissingEvidence));
    world.condition.put(&condition(), true);
    outcome.apply("m", observed(), &world, t(2)).expect("settled");
    assert_eq!(*state(&outcome), MessageState::Settled);
}

// Superseded needs a supersession naming this message as the older one.
#[test]
fn supersession_must_name_this_message() {
    let mut home = home(spec(Expectation::Reply, None));
    let mut world = MemoryMessageWorld::default();
    let by: Ref<Supersession> = Ref::new("m2");
    let superseded = || drop(DropReason::Superseded { by: Ref::new("m2") });
    assert_eq!(home.apply("m", superseded(), &world, t(1)).err(), refused(MessageRefusal::MissingEvidence));
    world.supersession.put(&by, Supersedes { older: "other".into() });
    assert_eq!(home.apply("m", superseded(), &world, t(1)).err(), refused(MessageRefusal::MissingEvidence));
    world.supersession.put(&by, Supersedes { older: "m".into() });
    home.apply("m", superseded(), &world, t(1)).expect("dropped");
    assert_eq!(
        *state(&home),
        MessageState::Dropped { reason: DropReason::Superseded { by }, dropped_receiver: None, at: t(1), watermark: None }
    );
}

// Withdrawn and ConsumerUnreachable are deliberate drops that need authority,
// from Pending or Awaiting; the drop records the reason, its decider and the
// receiver it had.
#[test]
fn deliberate_drops_need_authority() {
    let mut world = MemoryMessageWorld::default();
    world.authority.put(&Ref::new("no"), false);
    world.authority.put(&Ref::new("yes"), true);
    receipt(&mut world, "s1", Transport::Turn);
    let withdrawn = |a: &str| drop(DropReason::Withdrawn { by: "sender".into(), authority: Ref::new(a) });
    let unreachable = |a: &str| drop(DropReason::ConsumerUnreachable { by: "supervisor".into(), authority: Ref::new(a) });
    for (reason, delivered) in [(withdrawn as fn(&str) -> DropMessage, false), (unreachable, true)] {
        let mut home = home(spec(Expectation::Reply, None));
        if delivered {
            home.apply("m", deliver("s1", Transport::Turn), &world, t(1)).expect("delivered");
        }
        assert_eq!(home.apply("m", reason("no"), &world, t(2)).err(), refused(MessageRefusal::Unauthorised));
        assert_eq!(home.apply("m", reason("absent"), &world, t(2)).err(), refused(MessageRefusal::Unauthorised));
        home.apply("m", reason("yes"), &world, t(2)).expect("dropped");
        let receiver = delivered.then(|| session("s1"));
        assert_eq!(
            *state(&home),
            MessageState::Dropped { reason: reason("yes").reason, dropped_receiver: receiver, at: t(2), watermark: None }
        );
    }
}

// Expiry (spec Expired, `message_race`): refused without a deadline or before
// it has passed; refused when the expected evidence is visible; with no
// consumer bound and no receiver the watermark is not consulted, and the drop
// records none.
#[test]
fn expiry_needs_a_passed_deadline_and_no_expected_evidence() {
    let mut no_deadline = home(spec(Expectation::Reply, None));
    let mut world = MemoryMessageWorld::default();
    assert_eq!(no_deadline.apply("m", drop(DropReason::Expired), &world, t(9)).err(), refused(MessageRefusal::MissingEvidence));

    let mut reply = home(spec(Expectation::Reply, Some(5)));
    world.reply.put(&Ref::new("m"), ());
    assert_eq!(reply.apply("m", drop(DropReason::Expired), &world, t(6)).err(), refused(MessageRefusal::ExpectationMet));
    let mut outcome = home(spec(Expectation::Outcome { condition: condition() }, Some(5)));
    world.condition.put(&condition(), true);
    assert_eq!(outcome.apply("m", drop(DropReason::Expired), &world, t(6)).err(), refused(MessageRefusal::ExpectationMet));
    world.condition.put(&condition(), false);
    outcome.apply("m", drop(DropReason::Expired), &world, t(6)).expect("expired");
    assert_eq!(*state(&outcome), MessageState::Dropped { reason: DropReason::Expired, dropped_receiver: None, at: t(6), watermark: None });
    let reads = &outcome.get("m").expect("exists").events().last().expect("dropped").record.payload.reads;
    assert!(reads.iter().all(|read| read.kind != <AppliedThrough as Kind>::NAME), "{reads:?}");
}

// With a consumer bound, or a receiver, expiry waits until the consumer's
// stream is applied past the deadline (expiry never beats a reply written in
// time), and the drop records the watermark it relied on.
#[test]
fn expiry_with_a_consumer_waits_for_the_watermark_and_records_it() {
    let mut home = home(spec(Expectation::Reply, Some(5)));
    let mut world = MemoryMessageWorld::default();
    bound(&mut world);
    assert_eq!(home.apply("m", drop(DropReason::Expired), &world, t(7)).err(), refused(MessageRefusal::PrefixBeforeDeadline));
    world.applied_through.put(&Ref::new(INBOX), t(6));
    let event = home.apply("m", drop(DropReason::Expired), &world, t(7)).expect("expired");
    let kinds: Vec<_> = event.payload.reads.iter().map(|read| (read.kind.as_ref(), read.name.as_str())).collect();
    assert!(kinds.contains(&(<Binding as Kind>::NAME, INBOX)), "{kinds:?}");
    assert!(kinds.contains(&(<AppliedThrough as Kind>::NAME, INBOX)), "{kinds:?}");
    assert_eq!(
        *state(&home),
        MessageState::Dropped { reason: DropReason::Expired, dropped_receiver: None, at: t(7), watermark: Some(t(6)) }
    );

    // A receiver counts as a consumer even with the inbox unbound.
    let (mut delivered, mut world) = awaiting(Some(5));
    world.binding.put(&Ref::new(INBOX), InboxBinding::Unbound { since: t(1) });
    assert_eq!(delivered.apply("m", drop(DropReason::Expired), &world, t(7)).err(), refused(MessageRefusal::PrefixBeforeDeadline));
    world.applied_through.put(&Ref::new(INBOX), t(9));
    delivered.apply("m", drop(DropReason::Expired), &world, t(10)).expect("expired");
    let expired =
        MessageState::Dropped { reason: DropReason::Expired, dropped_receiver: Some(session("s1")), at: t(10), watermark: Some(t(9)) };
    assert_eq!(*state(&delivered), expired);
}

// Undeliverable (spec, `inbox_delivery`): only from Pending; never with an
// observed receipt from any session; otherwise only when no consumer has been
// bound past the threshold (counted from unbind) or the consumer published a
// delivery failure. An undeliverable message has no receiver.
#[test]
fn undeliverable_needs_no_receipt_and_an_unbound_inbox_or_a_failure() {
    let (mut delivered, world) = awaiting(None);
    assert_eq!(delivered.apply("m", drop(DropReason::Undeliverable), &world, t(20)).err(), refused(MessageRefusal::WrongState));

    let mut pending = home(spec(Expectation::Reply, None));
    let mut world = MemoryMessageWorld::default();
    world.binding.put(&Ref::new(INBOX), InboxBinding::Unbound { since: t(0) });
    receipt(&mut world, "other", Transport::Mute);
    assert_eq!(pending.apply("m", drop(DropReason::Undeliverable), &world, t(20)).err(), refused(MessageRefusal::ReceiptConfirmed));
    world.receipts.remove(&Ref::new("m"));
    bound(&mut world);
    assert_eq!(pending.apply("m", drop(DropReason::Undeliverable), &world, t(20)).err(), refused(MessageRefusal::MissingEvidence));
    world.delivery_failure.put(&Ref::new("m"), ());
    pending.apply("m", drop(DropReason::Undeliverable), &world, t(20)).expect("dropped");
    assert_eq!(
        *state(&pending),
        MessageState::Dropped { reason: DropReason::Undeliverable, dropped_receiver: None, at: t(20), watermark: None }
    );
}

// The time rules' boundaries, as named cases: expiry needs `now` strictly past
// the deadline and, with a consumer bound, the watermark strictly past it too;
// the unbound timeout needs strictly more than `unbound_after` since unbind.
#[test]
fn time_rules_are_strict_at_their_boundaries() {
    let expire_at = |now: i64, watermark: i64| {
        let mut home = home(spec(Expectation::Reply, Some(5)));
        let mut world = MemoryMessageWorld::default();
        bound(&mut world);
        world.applied_through.put(&Ref::new(INBOX), t(watermark));
        home.apply("m", drop(DropReason::Expired), &world, t(now)).err()
    };
    assert_eq!(expire_at(5, 9), refused(MessageRefusal::MissingEvidence));
    assert_eq!(expire_at(6, 5), refused(MessageRefusal::PrefixBeforeDeadline));
    assert_eq!(expire_at(6, 6), None);

    let undeliverable_at = |now: i64| {
        let mut home = home(spec(Expectation::Reply, None));
        let mut world = MemoryMessageWorld::default();
        world.binding.put(&Ref::new(INBOX), InboxBinding::Unbound { since: t(3) });
        home.apply("m", drop(DropReason::Undeliverable), &world, t(now)).err()
    };
    assert_eq!(undeliverable_at(13), refused(MessageRefusal::MissingEvidence));
    assert_eq!(undeliverable_at(14), None);
}

// An edge event is the schema crate's stored row as it is: the header holds
// the edge kind's and labels' codes, and the inline document carries the
// typed edge and every read, so a decoded event equals the one the home
// appended.
#[test]
fn edge_events_round_trip_through_the_schema_codec() {
    let (mut home, mut world) = awaiting(None);
    world.reply.put(&Ref::new("m"), ());
    home.apply("m", Settle { by: Settlement::Answered }, &world, t(2)).expect("settled");
    let events = home.get("m").expect("exists").events();
    assert_eq!(events.iter().map(|row| (row.position, row.version)).collect::<Vec<_>>(), [(Position(0), 2), (Position(1), 3)]);
    for row in events {
        let rows = Encoder::encode(&row.record).expect("encodes");
        let header: Vec<_> = rows.tables[0][0].values.iter().take(3).cloned().collect();
        let codes = [row.record.edge.code(), row.record.from.code(), row.record.to.code()].map(Value::Int);
        assert_eq!(header, codes);
        assert_eq!(Decoder::decode::<EdgeEventOf<Message>>(rows).expect("decodes"), row.record);
    }
    let delivered = &events.rows()[0].record;
    assert_eq!((delivered.edge.code(), delivered.from.code(), delivered.to.code()), (1, 1, 2));
    assert_eq!(delivered.payload.edge, deliver("s1", Transport::Turn).into());
    assert_eq!(delivered.payload.reads, [InputRead { kind: "message_receipts".into(), name: "m".into(), version: Some(Version(1)) }]);
    assert_eq!(delivered.writer, "home");
}

// A payload's document is the edge's serde form: a reference is its name
// alone, and every read keeps its kind, name and version (or absence).
#[test]
fn payload_documents_name_references_and_keep_reads() {
    let reason = DropReason::Withdrawn { by: "ann".into(), authority: Ref::new("ops") };
    let reads = vec![
        InputRead { kind: "authority".into(), name: "ops".into(), version: Some(Version(4)) },
        InputRead { kind: "binding".into(), name: INBOX.into(), version: None },
    ];
    let payload = EdgePayload { edge: MessageEdge::from(drop(reason)), reads };
    let json = serde_json::to_value(&payload).expect("serialises");
    assert_eq!(
        json,
        serde_json::json!({
            "edge": { "Drop": { "reason": { "Withdrawn": { "by": "ann", "authority": "ops" } } } },
            "reads": [
                { "kind": "authority", "name": "ops", "version": 4 },
                { "kind": "binding", "name": INBOX, "version": null },
            ],
        })
    );
    assert_eq!(serde_json::from_value::<EdgePayload<MessageEdge>>(json).expect("deserialises"), payload);
}
