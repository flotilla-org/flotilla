//! Message (spec `machines/message.qnt`; scenarios `inbox_delivery`, `message_race`;
//! ADR 0064 "Inboxes").

use flotilla_schema::{Address, Cause, Cbor, Code, CodeEnum, EdgeHeader, Effect, Event, KindCode, Origin, Timestamp};
use flotilla_state_machines::{
    edges,
    machines::{
        agent_session::{AgentSessionEdge, Receive},
        consumer::{Consumer, ConsumerKind},
        message::{
            pending_for, sent, AppliedThrough, Binding, Condition, Deliver, DropMessage, DropReason, Expectation, InboxBinding,
            MemoryMessageWorld, Message, MessageBody, MessageEdge, MessageEdgeKind, MessageLabel, MessageRef, MessageReference,
            MessageRefusal, MessageSpec, MessageState, ReceiptLog, Receipts, Receiver, Relation, Settle, Settlement, Supersedes,
            Supersession,
        },
    },
    states, Admission, Coded, Diagram, EdgeDef, EdgePayload, Ended, Home, InputRead, Kind, Machine, MachineDef, Ref, Source, Table,
    Transport, Version, WriteError, WrongState,
};
use serde::{Deserialize, Serialize};

const INBOX: &str = "project:p/role:r";

fn t(micros: i64) -> Timestamp {
    Timestamp::from_micros(micros)
}

/// The project-rooted address of `name`.
fn a(name: &str) -> Address {
    Address::project(name).expect("valid name")
}

fn inbox() -> Address {
    INBOX.parse().expect("canonical inbox")
}

fn condition() -> Address {
    a("build-green")
}

/// The world's key for [`condition`]: its canonical text.
fn condition_ref() -> Ref<Condition> {
    Ref::new(condition().as_str())
}

fn spec(expectation: Expectation, deadline: Option<i64>) -> MessageSpec {
    let body = MessageBody::text("hello").expect("small body");
    MessageSpec {
        expectation,
        deadline: deadline.map(t),
        unbound_after: 10,
        ..MessageSpec::new(a("sender"), inbox(), Relation::Peer, body)
    }
}

fn home(spec: MessageSpec) -> Home<Message> {
    let mut home = Home::<Message>::new("home").expect("valid declaration");
    home.create("m", spec, &MemoryMessageWorld::default(), Timestamp(0)).expect("created");
    home
}

fn session(name: &str) -> Receiver {
    Receiver { kind: ConsumerKind::AgentSession, id: name.into(), host: "consumer".into() }
}

fn deliver(name: &str, transport: Transport) -> Deliver {
    Deliver { receiver: session(name), transport }
}

fn receipt(world: &mut MemoryMessageWorld, session: &str, transport: Transport) {
    let own = Ref::<Receipts>::new("m");
    let mut receipts = world.receipts.lookup(&own).map(|(_, by_session)| by_session.clone()).unwrap_or_default();
    receipts.insert(self::session(session), transport);
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
    world.binding.put(&Ref::new(INBOX), InboxBinding::Bound { consumers: [session("s1")].into() });
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
    assert_eq!(
        labels,
        [("Pending", Code::new("pe")), ("Awaiting", Code::new("aw")), ("Settled", Code::new("se")), ("Dropped", Code::new("dr"))]
    );
    let edges: Vec<_> = MessageEdgeKind::ALL.iter().map(|k| (k.name(), k.code())).collect();
    assert_eq!(edges, [("deliver", Code::new("de")), ("settle", Code::new("se")), ("drop", Code::new("dr"))]);
}

// The diagram shows deliver's two targets, settle and drop from both live
// states, with each edge's changed data. No edge keeps its label, so both
// views are the same.
#[test]
fn the_diagram_is_pinned() {
    let diagram = "---\ntitle: Message\n---\nstateDiagram-v2\n  direction LR\n  [*] --> Pending\n\
                   \x20 Pending --> Settled: deliver [receiver]\n\
                   \x20 Pending --> Awaiting: deliver [receiver]\n\
                   \x20 Pending --> Settled: settle [receiver]\n\
                   \x20 Awaiting --> Settled: settle [receiver]\n\
                   \x20 Pending --> Dropped: drop [reason, receiver, watermark]\n\
                   \x20 Awaiting --> Dropped: drop [reason, receiver, watermark]\n\
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
    world.binding.put(&Ref::new(INBOX), InboxBinding::Bound { consumers: [session("replacement")].into() });
    receipt(&mut world, "old", Transport::Mute);
    home.apply("m", deliver("old", Transport::Mute), &world, t(1)).expect("delivered");
    receipt(&mut world, "replacement", Transport::Turn);
    assert_eq!(home.apply("m", deliver("replacement", Transport::Turn), &world, t(2)).err(), refused(MessageRefusal::WrongState));
    assert_eq!(*state(&home), MessageState::Awaiting { receiver: session("old") });
    assert_eq!(home.log().history("m", Version(0), usize::MAX).len(), 1, "one delivery");
}

// With nothing expected, delivery settles the message, keeping its receiver
// (G5); it then refuses every edge as Ended.
#[test]
fn delivering_an_expectation_free_message_settles_it() {
    let mut home = home(spec(Expectation::Nothing, Some(5)));
    let mut world = MemoryMessageWorld::default();
    receipt(&mut world, "s1", Transport::Notify);
    home.apply("m", deliver("s1", Transport::Notify), &world, t(1)).expect("delivered");
    assert_eq!(*state(&home), MessageState::Settled { receiver: Some(session("s1")) }, "a settled message keeps its receiver");
    let ended = refused(MessageRefusal::Ended);
    assert_eq!(home.apply("m", deliver("s1", Transport::Notify), &world, t(2)).err(), ended);
    assert_eq!(home.apply("m", drop(DropReason::Expired), &world, t(9)).err(), ended);
    let settle = Settle { by: Settlement::OutcomeObserved };
    assert_eq!(home.apply("m", settle, &world, t(2)).err(), ended);
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
    assert_eq!(*state(&reply), MessageState::Settled { receiver: Some(session("s1")) });
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
    world.condition.put(&condition_ref(), false);
    assert_eq!(outcome.apply("m", observed(), &world, t(1)).err(), refused(MessageRefusal::MissingEvidence));
    world.condition.put(&condition_ref(), true);
    outcome.apply("m", observed(), &world, t(2)).expect("settled");
    assert_eq!(*state(&outcome), MessageState::Settled { receiver: None }, "settled from Pending: no receiver");
}

// Superseded needs a supersession naming this message as the older one.
#[test]
fn supersession_must_name_this_message() {
    let mut home = home(spec(Expectation::Reply, None));
    let mut world = MemoryMessageWorld::default();
    let by = a("m2");
    let superseded = || drop(DropReason::Superseded { by: by.clone() });
    assert_eq!(home.apply("m", superseded(), &world, t(1)).err(), refused(MessageRefusal::MissingEvidence));
    world.supersession.put(&Ref::<Supersession>::new(by.as_str()), Supersedes { older: "other".into() });
    assert_eq!(home.apply("m", superseded(), &world, t(1)).err(), refused(MessageRefusal::MissingEvidence));
    world.supersession.put(&Ref::<Supersession>::new(by.as_str()), Supersedes { older: "m".into() });
    home.apply("m", superseded(), &world, t(1)).expect("dropped");
    assert_eq!(*state(&home), MessageState::Dropped { reason: DropReason::Superseded { by }, receiver: None, watermark: None });
}

// Withdrawn and ConsumerUnreachable are deliberate drops that need authority,
// from Pending or Awaiting; the drop records the reason, its decider and the
// receiver it had.
#[test]
fn deliberate_drops_need_authority() {
    let mut world = MemoryMessageWorld::default();
    world.authority.put(&Ref::new(a("no").as_str()), false);
    world.authority.put(&Ref::new(a("yes").as_str()), true);
    receipt(&mut world, "s1", Transport::Turn);
    let withdrawn = |name: &str| drop(DropReason::Withdrawn { by: a("sender"), authority: a(name) });
    let unreachable = |name: &str| drop(DropReason::ConsumerUnreachable { by: a("supervisor"), authority: a(name) });
    for (reason, delivered) in [(withdrawn as fn(&str) -> DropMessage, false), (unreachable, true)] {
        let mut home = home(spec(Expectation::Reply, None));
        if delivered {
            home.apply("m", deliver("s1", Transport::Turn), &world, t(1)).expect("delivered");
        }
        assert_eq!(home.apply("m", reason("no"), &world, t(2)).err(), refused(MessageRefusal::Unauthorised));
        assert_eq!(home.apply("m", reason("absent"), &world, t(2)).err(), refused(MessageRefusal::Unauthorised));
        home.apply("m", reason("yes"), &world, t(2)).expect("dropped");
        let receiver = delivered.then(|| session("s1"));
        assert_eq!(*state(&home), MessageState::Dropped { reason: reason("yes").reason, receiver, watermark: None });
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
    world.condition.put(&condition_ref(), true);
    assert_eq!(outcome.apply("m", drop(DropReason::Expired), &world, t(6)).err(), refused(MessageRefusal::ExpectationMet));
    world.condition.put(&condition_ref(), false);
    outcome.apply("m", drop(DropReason::Expired), &world, t(6)).expect("expired");
    assert_eq!(*state(&outcome), MessageState::Dropped { reason: DropReason::Expired, receiver: None, watermark: None });
    let history = outcome.log().history("m", Version(0), usize::MAX);
    let (_, dropped) = history.last().expect("dropped");
    assert_eq!(dropped.at, t(6), "the drop's time is its event's, the state's since (G4)");
    let reads = &dropped.payload.reads;
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
    assert_eq!(*state(&home), MessageState::Dropped { reason: DropReason::Expired, receiver: None, watermark: Some(t(6)) });

    // A receiver counts as a consumer even with the inbox unbound.
    let (mut delivered, mut world) = awaiting(Some(5));
    world.binding.put(&Ref::new(INBOX), InboxBinding::Unbound { since: t(1) });
    assert_eq!(delivered.apply("m", drop(DropReason::Expired), &world, t(7)).err(), refused(MessageRefusal::PrefixBeforeDeadline));
    world.applied_through.put(&Ref::new(INBOX), t(9));
    delivered.apply("m", drop(DropReason::Expired), &world, t(10)).expect("expired");
    let expired = MessageState::Dropped { reason: DropReason::Expired, receiver: Some(session("s1")), watermark: Some(t(9)) };
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
    assert_eq!(*state(&pending), MessageState::Dropped { reason: DropReason::Undeliverable, receiver: None, watermark: None });
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

// An edge event is the header and payload of its log entry: the header holds the edge kind's and
// labels' codes, and the inline document carries the typed edge and every read, so a decoded
// payload equals the one the home appended.
#[test]
fn edge_events_round_trip_through_the_schema_codec() {
    let (mut home, mut world) = awaiting(None);
    world.reply.put(&Ref::new("m"), ());
    let previous = home.get("m").expect("exists").stored_state().clone();
    let event = home.apply("m", Settle { by: Settlement::Answered }, &world, t(2)).expect("settled");
    let resource = home.get("m").expect("exists");
    let events = home.log().history("m", Version(0), usize::MAX);
    assert_eq!(events.iter().map(|(version, _)| *version).collect::<Vec<_>>(), [Version(2), Version(3)], "the log's sequence");
    assert_eq!(resource.version(), Version(3), "the home adopts the log's version");
    let header = resource.stored_edge(&previous, &event).expect("edge").into_parts().event;
    assert_eq!((header.edge, header.from, header.to), (event.edge.code(), event.from.code(), event.to.code()));
    assert_eq!(header.payload::<EdgePayload<MessageEdge>>().expect("decodes"), event.payload);
    let delivered = events[0].1;
    assert_eq!((delivered.edge.code(), delivered.from.code(), delivered.to.code()), (Code::new("de"), Code::new("pe"), Code::new("aw")));
    assert_eq!(delivered.payload.edge, deliver("s1", Transport::Turn).into());
}

// A payload's document is the edge's serde form: its code and arguments, never
// its variant name, so renaming an edge leaves stored payloads decodable; a
// reference is its name alone, and every read keeps its kind, name and version
// (or absence).
#[test]
fn payload_documents_name_references_and_keep_reads() {
    let reason = DropReason::Withdrawn { by: a("ann"), authority: a("ops") };
    let reads = vec![
        InputRead { kind: "authority".into(), name: "project:ops".into(), version: Some(Version(4)) },
        InputRead { kind: "binding".into(), name: INBOX.into(), version: None },
    ];
    let payload = EdgePayload { edge: MessageEdge::from(drop(reason)), reads };
    let json = serde_json::to_value(&payload).expect("serialises");
    assert_eq!(
        json,
        serde_json::json!({
            "edge": ["dr", { "reason": { "Withdrawn": { "by": "project:ann", "authority": "project:ops" } } }],
            "reads": [
                { "kind": "authority", "name": "project:ops", "version": 4 },
                { "kind": "binding", "name": INBOX, "version": null },
            ],
        })
    );
    assert_eq!(serde_json::from_value::<EdgePayload<MessageEdge>>(json).expect("deserialises"), payload);
    let unknown = serde_json::json!({ "edge": ["zz", {}], "reads": [] });
    let refused = serde_json::from_value::<EdgePayload<MessageEdge>>(unknown).expect_err("no edge has code zz");
    assert!(refused.to_string().contains("unknown MessageEdge code \"zz\""), "{refused}");
    // A document is exactly [code, arguments]: a third element is refused, as JSON and as the
    // stored CBOR.
    let trailing = serde_json::json!({ "edge": ["dr", { "reason": "Expired" }, "junk"], "reads": [] });
    let refused = serde_json::from_value::<EdgePayload<MessageEdge>>(trailing.clone()).expect_err("a third element");
    assert!(refused.to_string().contains("invalid length 3, expected a MessageEdge as [code, arguments]"), "{refused}");
    let stored = Origin::new("w", t(0)).with_payload(&trailing).expect("encodes").payload.expect("a payload");
    let header = EdgeHeader {
        edge: Code::new("dr"),
        from: Code::new("pe"),
        to: Code::new("dr"),
        writer: "w".into(),
        at: t(0),
        payload: stored,
        about: None,
    };
    assert!(header.payload::<EdgePayload<MessageEdge>>().is_err(), "a stored payload with a third element is refused");
}

/// A reference to `name` at `revision`, the subject or not.
fn reference(name: &str, revision: &str, subject: bool) -> MessageReference {
    MessageReference { address: a(name), revision: revision.into(), subject }
}

// Admission (#3424 rulings 8 and 10): at most one reference is the subject, every revision is
// non-empty, and the messages a new one replies to and supersedes exist. Each of those is read
// through the recording handle, so the create carries the reads it relied on, cross-home
// identities included; a refused admission creates nothing.
#[test]
fn admission_checks_references_and_reads_the_messages_named() {
    let create = |spec: MessageSpec, world: &MemoryMessageWorld| {
        let mut home = Home::<Message>::new("home").expect("valid declaration");
        home.create("m", spec, world, t(0)).map(|created| created.reads)
    };
    let base = spec(Expectation::Reply, None);
    let world = MemoryMessageWorld::default();
    let two_subjects = MessageSpec { references: vec![reference("a", "1", true), reference("b", "2", true)], ..base.clone() };
    assert_eq!(create(two_subjects, &world).err(), refused(MessageRefusal::SubjectsConflict));
    let empty = MessageSpec { references: vec![reference("a", "1", true), reference("b", "", false)], ..base.clone() };
    assert_eq!(create(empty, &world).err(), refused(MessageRefusal::EmptyRevision));
    let one_subject = MessageSpec { references: vec![reference("a", "1", true), reference("b", "2", false)], ..base.clone() };
    assert_eq!(one_subject.subject(), Some(&reference("a", "1", true)));
    assert_eq!(create(one_subject, &world), Ok(Vec::new()), "references need no reads");

    let earlier = MessageRef::new("earlier", "elsewhere");
    let older = MessageRef::new("older", "home");
    let reply = MessageSpec { in_reply_to: Some(earlier.clone()), supersedes: Some(older.clone()), ..base };
    assert_eq!(create(reply.clone(), &world).err(), refused(MessageRefusal::UnknownMessage));
    let world = MemoryMessageWorld::default().with_sent(&earlier);
    assert_eq!(create(reply.clone(), &world).err(), refused(MessageRefusal::UnknownMessage), "the superseded message too");
    let world = world.with_sent(&older);
    let reads = create(reply, &world).expect("admitted");
    let names: Vec<_> = reads.iter().map(|read| (read.kind.as_ref(), read.name.as_str(), read.version.is_some())).collect();
    assert_eq!(names, [("message", "earlier@elsewhere", true), ("message", "older@home", true)]);
    assert_eq!(sent(&earlier), Ref::new("earlier@elsewhere"));
}

/// A replicated spec with two subjects (admitted elsewhere, or by an older writer) names none.
#[test]
fn a_second_subject_reads_as_no_subject() {
    let spec = MessageSpec { references: vec![reference("a", "1", true), reference("b", "2", true)], ..spec(Expectation::Nothing, None) };
    assert_eq!(spec.subject(), None);
}

// A delivery condition observed not to hold lets the turn-rules role withdraw the message
// (#3424 ruling 9; spec `hasCondition and conditionFailed`): without authority, the withdrawal
// needs the envelope's condition read false. A condition unobserved or holding is no evidence,
// and a message without one is withdrawn only by authority.
#[test]
fn a_lapsed_delivery_condition_withdraws_the_message() {
    let wanted = a("still-wanted");
    let turn_rules = DropReason::Withdrawn { by: a("turn-rules"), authority: wanted.clone() };
    let mut without = home(spec(Expectation::Reply, None));
    let mut world = MemoryMessageWorld::default();
    world.condition.put(&Ref::new(wanted.as_str()), false);
    assert_eq!(without.apply("m", drop(turn_rules.clone()), &world, t(1)).err(), refused(MessageRefusal::Unauthorised));

    let mut home = home(MessageSpec { delivery_condition: Some(wanted.clone()), ..spec(Expectation::Reply, None) });
    let mut world = MemoryMessageWorld::default();
    assert_eq!(home.apply("m", drop(turn_rules.clone()), &world, t(1)).err(), refused(MessageRefusal::Unauthorised), "unobserved");
    world.condition.put(&Ref::new(wanted.as_str()), true);
    assert_eq!(home.apply("m", drop(turn_rules.clone()), &world, t(1)).err(), refused(MessageRefusal::Unauthorised), "still holds");
    world.condition.put(&Ref::new(wanted.as_str()), false);
    let event = home.apply("m", drop(turn_rules.clone()), &world, t(2)).expect("withdrawn");
    let reads: Vec<_> = event.payload.reads.iter().map(|read| (read.kind.as_ref(), read.name.as_str())).collect();
    assert!(reads.contains(&(<Condition as Kind>::NAME, wanted.as_str())), "{reads:?}");
    assert_eq!(*state(&home), MessageState::Dropped { reason: turn_rules, receiver: None, watermark: None });
}

// A shared inbox binds a set of consumers (#3424 ruling 3). The message is pending for each bound
// consumer until that consumer's receipt; the first deliver the home accepts claims it for every
// one of them; undeliverable needs nobody bound, so one consumer still bound refuses it.
#[test]
fn a_shared_inbox_is_pending_for_each_consumer_until_its_receipt() {
    let agent = session("s1");
    let controller = Receiver { kind: ConsumerKind::Controller, id: "approve".into(), host: "consumer".into() };
    let stranger = session("s9");
    let mut home = home(spec(Expectation::Reply, None));
    let mut world = MemoryMessageWorld::default();
    world.binding.put(&Ref::new(INBOX), InboxBinding::Bound { consumers: [agent.clone(), controller.clone()].into() });
    let pending = |home: &Home<Message>, world: &MemoryMessageWorld, who: &Receiver| {
        home.judge("m", world, t(1), |h| pending_for(h, who)).expect("exists").value
    };
    assert!(pending(&home, &world, &agent) && pending(&home, &world, &controller));
    assert!(!pending(&home, &world, &stranger), "not bound, so not pending for it");
    assert_eq!(home.apply("m", drop(DropReason::Undeliverable), &world, t(99)).err(), refused(MessageRefusal::MissingEvidence));

    let own = Ref::<Receipts>::new("m");
    world.receipts.put(&own, [(controller.clone(), Transport::Turn)].into());
    assert!(!pending(&home, &world, &controller), "the controller has its receipt");
    assert!(pending(&home, &world, &agent), "the agent has none yet");
    world.receipts.put(&own, [(controller.clone(), Transport::Turn), (agent.clone(), Transport::Turn)].into());
    home.apply("m", Deliver { receiver: controller.clone(), transport: Transport::Turn }, &world, t(2)).expect("claimed");
    assert_eq!(
        home.apply("m", deliver("s1", Transport::Turn), &world, t(3)).err(),
        refused(MessageRefusal::WrongState),
        "the first accepted deliver claims it"
    );
    assert!(!pending(&home, &world, &agent), "claimed: pending for nobody");
    assert_eq!(*state(&home), MessageState::Awaiting { receiver: controller });
}

// A deterministic controller consuming an inbox through its own resource (#3424 ruling 2): a
// running probe that takes a message by a `Receive` edge about it, or retires.
states! {
    enum ProbeState => ProbeLabel {
        Running = "ru",
        Retired = "re",
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ProbeReceive {
    message: String,
    transport: Transport,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Retire {
    why: String,
}

edges! {
    enum ProbeEdge => ProbeEdgeKind {
        Receive(ProbeReceive) = "rv",
        Retire(Retire) = "rt",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProbeRefusal;

impl From<Ended<ProbeLabel>> for ProbeRefusal {
    fn from(_: Ended<ProbeLabel>) -> Self {
        ProbeRefusal
    }
}

impl From<WrongState<ProbeLabel, ProbeEdgeKind>> for ProbeRefusal {
    fn from(_: WrongState<ProbeLabel, ProbeEdgeKind>) -> Self {
        ProbeRefusal
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default, flotilla_schema::Stored)]
#[store(kind = "Probe", code = "pb", state = ProbeState)]
struct ProbeSpec {}

struct Probe;

impl Machine for Probe {
    const NAME: &'static str = "Probe";
    type Request = ();
    type Spec = ProbeSpec;
    type State = ProbeState;
    type Edge = ProbeEdge;
    type Refusal = ProbeRefusal;
    type World = Table<Receipts>;

    fn definition() -> MachineDef<Self> {
        use ProbeLabel::*;
        MachineDef::new(Running)
            .edge(
                EdgeDef::<Self, ProbeReceive>::new()
                    .from([Running])
                    .to([Running])
                    .about(|receive| &receive.message)
                    .ungated("the controller took the message")
                    .effect(|_, state, _| state),
            )
            .edge(
                EdgeDef::<Self, Retire>::new()
                    .from([Running])
                    .to([Retired])
                    .ungated("retiring needs no evidence")
                    .effect(|_, _, _| ProbeState::Retired),
            )
            .end(Retired, "the probe retired")
    }

    fn admit(_: &mut Admission<'_, Probe>, _: ()) -> Result<(ProbeSpec, ProbeState), ProbeRefusal> {
        Ok((ProbeSpec {}, ProbeState::Running))
    }
}

impl Consumer for Probe {
    const CONSUMER: ConsumerKind = ConsumerKind::Controller;
    const RECEIVE: ProbeEdgeKind = ProbeEdgeKind::Receive;

    fn receipt(edge: &ProbeEdge) -> Option<(&str, Transport)> {
        match edge {
            ProbeEdge::Receive(receive) => Some((&receive.message, receive.transport)),
            ProbeEdge::Retire(_) => None,
        }
    }
}

/// A log entry of `kind`'s resource `name` at `home`: edge `edge`, whose payload is `payload`.
fn entry(kind: &str, name: &str, home: &str, edge: &str, payload: &impl Cbor) -> Event {
    Event {
        kind: KindCode::new(kind),
        name: name.into(),
        home: home.into(),
        home_seq: 1,
        prev_seq: 0,
        cause: Cause::Edge { edge: Code::new(edge), from: Code::new("ru"), to: Code::new("ru") },
        writer: "w".into(),
        at: t(1),
        about: None,
        payload: Origin::new("w", t(1)).with_payload(payload).expect("encodes").payload,
        effect: Effect::default(),
    }
}

// The receipt reader accepts any consumer kind that declares a `Receive` (#3424 ruling 2): an
// agent session's from the start, a controller's once its kind is added, each naming the
// consumer by kind, the entry's resource and its home. Entries of a kind not accepted, and the
// accepted kinds' other edges, are no receipts, and their payloads are never decoded as one: a
// retirement whose payload is not even the probe's edge document is skipped, not refused.
#[test]
fn the_receipt_reader_takes_every_accepted_consumer_kinds_receive() {
    let session = entry(
        "as",
        "s1",
        "kiwi",
        "rv",
        &EdgePayload { edge: AgentSessionEdge::Receive(Receive { message: "m".into(), transport: Transport::Turn }), reads: vec![] },
    );
    let probe = entry(
        "pb",
        "approve",
        "rata",
        "rv",
        &EdgePayload { edge: ProbeEdge::Receive(ProbeReceive { message: "m".into(), transport: Transport::Notify }), reads: vec![] },
    );
    let retired = entry("pb", "approve", "rata", "rt", &"not an edge document".to_owned());
    let other = entry(
        "pb",
        "other",
        "rata",
        "rv",
        &EdgePayload { edge: ProbeEdge::Receive(ProbeReceive { message: "n".into(), transport: Transport::Turn }), reads: vec![] },
    );
    let entries = [session, probe, retired, other];
    let agent = Receiver { kind: ConsumerKind::AgentSession, id: "s1".into(), host: "kiwi".into() };
    let controller = Receiver { kind: ConsumerKind::Controller, id: "approve".into(), host: "rata".into() };

    let mut sessions_only = ReceiptLog::new();
    sessions_only.insert("m", &entries).expect("the probe's entries are skipped");
    let found = sessions_only.lookup(&Ref::new("m")).map(|(version, receipts)| (version, receipts.clone()));
    assert_eq!(found, Some((Version(1), [(agent.clone(), Transport::Turn)].into())));

    let mut both = ReceiptLog::new().consumer::<Probe>();
    both.insert("m", &entries).expect("a retirement is not decoded");
    let found = both.lookup(&Ref::new("m")).map(|(version, receipts)| (version, receipts.clone()));
    assert_eq!(found, Some((Version(2), [(agent, Transport::Turn), (controller, Transport::Notify)].into())));

    let broken = entry("pb", "approve", "rata", "rv", &"not an edge document".to_owned());
    assert!(ReceiptLog::new().consumer::<Probe>().insert("m", [&broken]).is_err(), "a Receive that does not decode is refused");
}
