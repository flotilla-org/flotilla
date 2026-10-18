//! Message against `conformance/message.qnt`.
//!
//! The harness envelope `{expectation, hasDeadline, deadline, hasCondition}`
//! is the message's spec: `Outcome` names a fixed condition, the deadline is
//! `deadline` model ticks when `hasDeadline`, and `hasCondition` is a fixed
//! delivery condition. Times map through [`tick`].
//!
//! Event mapping, each with a fresh [`MemoryMessageWorld`]:
//! - `Deliver(Receipt{observed, receiverMatches, transportMatches, receiver})`:
//!   the deliver edge names consumer `{consumer: n, host}` as the agent
//!   session `s<n>` on that host, by turn. Without `observed` no receipt
//!   exists; otherwise the receipt comes from that consumer iff
//!   `receiverMatches` (else another consumer), by turn iff
//!   `transportMatches` (else by notification). The receipt is the
//!   consumer's `Receive` entry about the message, read as the message's
//!   [`Receipts`].
//! - `Settle(Answered{observed})`: a reply exists iff `observed`;
//!   `Settle(OutcomeObserved{observed})`: the condition reads `observed`.
//! - `DropMessage(evidence)` at `now`, with only the fields its reason reads:
//!   `bound` is a `Bound` binding (else `Unbound` since `now`, too recent);
//!   `noConsumerPastThreshold` is `Unbound` since longer than `unbound_after`;
//!   `appliedThrough` is the inbox's `AppliedThrough` watermark; `met` is a
//!   reply (Reply) or a holding condition (Outcome); `receipt` is a receipt
//!   row; `failure` a `DeliveryFailure`; `supersedes` a `Supersession` naming
//!   this message (else another); `authorised` the drop's `Authority`;
//!   `conditionFailed` the delivery condition read `false` (a withdrawal
//!   names the delivery condition as its authority when the envelope has
//!   one, as the turn-rules role does, and the sender otherwise).
//!
//! State mapping: the envelope is the immutable [`MessageSpec`] (expectation,
//! the deadline as `hasDeadline` and `deadline`, and the delivery condition as
//! `hasCondition`); `Pending` carries it,
//! `Awaiting { receiver }` is `Awaiting({envelope, receiver})` with consumer
//! `s<n>` as `{consumer: n, host}`, `Settled { receiver }` is `Settled({envelope,
//! receiver})` with 0 for none, and `Dropped { reason, receiver,
//! watermark }` is the spec's whole drop record:
//! - `reason` by constructor (the evidence references a reason carries in
//!   Rust are the gate's inputs, not state the spec has);
//! - `receiver` the session number, or 0 with none;
//! - `watermark` and `bound` from `watermark: Option<Timestamp>`: `Some(w)` is
//!   `watermark: w, bound: true`, and `None` is `watermark: 0, bound: false`.
//!   Both describe an expiry with a consumer bound, the only drop that reads
//!   the watermark;
//! - `receipt` always false: the spec records the receipt only for an
//!   undeliverable drop, and that drop is accepted only without one, so no
//!   reachable drop record has it true and Rust stores nothing for it.
//!
//! One encoding choice remains: the harness envelope carries `deadline: 1`
//! even when `hasDeadline` is false, while a Rust deadline is an `Option`, so a
//! message without a deadline encodes the harness's constant.

use std::collections::BTreeMap;

use flotilla_schema::{Address, Timestamp};
use flotilla_state_machines::{
    machines::{
        consumer::ConsumerKind,
        message::{
            AppliedThrough, Binding, Condition, Deliver, DeliveryFailure, DropMessage, DropReason, Expectation, InboxBinding,
            MemoryMessageWorld, Message, MessageBody, MessageRefusal, MessageSpec, MessageState, Receipts, Receiver, Relation, Reply,
            Settle, Settlement, Supersedes, Supersession,
        },
    },
    Home, Ref, Transport, WriteError,
};
use serde_json::{json, Value};

use crate::{
    graph::{self, bare, refusal, tick, Conformance, Report, Step, UNIT},
    itf::{boolean, int, number, payload, tag, text, variant},
};

const NAME: &str = "m";
const INBOX: &str = "project:p/role:r";
const CONDITION: &str = "project:condition";
const DELIVERY: &str = "project:still-wanted";
const NEWER: &str = "project:newer";
const SENDER: &str = "project:sender";
const SUPERVISOR: &str = "project:supervisor";
/// The harness's deadline value: an envelope without a deadline still carries it.
const HARNESS_DEADLINE: i64 = 1;

/// A canonical address; gates read the world by its text.
fn at(text: &str) -> Address {
    text.parse().expect("canonical conformance address")
}

/// The receiver session for spec session number `n`.
fn session(n: i64) -> String {
    format!("s{n}")
}

fn session_number(receiver: &Receiver) -> i64 {
    receiver.id.trim_start_matches('s').parse().expect("conformance session name")
}

fn spec_of(envelope: &Value) -> MessageSpec {
    let expectation = match tag(&envelope["expectation"]) {
        "Nothing" => Expectation::Nothing,
        "Reply" => Expectation::Reply,
        "Outcome" => Expectation::Outcome { condition: at(CONDITION) },
        other => panic!("expectation {other}"),
    };
    MessageSpec {
        inbox: at(INBOX),
        expectation,
        deadline: boolean(&envelope["hasDeadline"]).then(|| tick(number(&envelope["deadline"]))),
        unbound_after: UNIT,
        delivery_condition: boolean(&envelope["hasCondition"]).then(|| at(DELIVERY)),
        ..MessageSpec::new(at(SENDER), at(INBOX), Relation::Peer, MessageBody::text("turn").expect("small body"))
    }
}

/// The consumer named by session `id`: an agent session on the consumer host.
fn consumer(id: &str) -> Receiver {
    Receiver { kind: ConsumerKind::AgentSession, id: id.into(), host: "consumer".into() }
}

/// The inbox's binding with consumer 1 bound.
fn bound() -> InboxBinding {
    InboxBinding::Bound { consumers: [consumer(&session(1))].into() }
}

/// An engine time in model ticks.
fn ticks(time: Timestamp) -> i64 {
    time.micros() / UNIT
}

fn envelope_of(spec: &MessageSpec) -> Value {
    let expectation = match spec.expectation {
        Expectation::Nothing => "Nothing",
        Expectation::Reply => "Reply",
        Expectation::Outcome { .. } => "Outcome",
    };
    let deadline = spec.deadline.map_or(HARNESS_DEADLINE, ticks);
    json!({
        "expectation": bare(expectation),
        "hasDeadline": spec.deadline.is_some(),
        "deadline": int(deadline),
        "hasCondition": spec.delivery_condition.is_some(),
    })
}

fn outcome<T>(result: Result<T, WriteError<Message>>) -> Step {
    let name = match result {
        Ok(_) => return Step::Accepted,
        Err(WriteError::Refused(r)) => match r {
            MessageRefusal::Ended => "Ended",
            MessageRefusal::WrongState => "WrongState",
            MessageRefusal::IncorrectReceipt => "IncorrectReceipt",
            MessageRefusal::UnsupportedSettlement => "UnsupportedSettlement",
            MessageRefusal::MissingEvidence => "MissingEvidence",
            MessageRefusal::PrefixBeforeDeadline => "PrefixBeforeDeadline",
            MessageRefusal::ExpectationMet => "ExpectationMet",
            MessageRefusal::ReceiptConfirmed => "ReceiptConfirmed",
            MessageRefusal::Unauthorised => "Unauthorised",
            MessageRefusal::SubjectsConflict | MessageRefusal::EmptyRevision | MessageRefusal::UnknownMessage => {
                panic!("an admission refusal from an edge: {r:?}")
            }
        },
        Err(other) => return refusal(&format!("{other:?}")),
    };
    refusal(name)
}

fn put_receipt(world: &mut MemoryMessageWorld, session: &str, transport: Transport) {
    world.receipts.put(&Ref::<Receipts>::new(NAME), BTreeMap::from([(consumer(session), transport)]));
}

fn drop_reason_of(spec: &MessageSpec, o: &Value) -> DropReason {
    match tag(&o["reason"]) {
        "Undeliverable" => DropReason::Undeliverable,
        "Expired" => DropReason::Expired,
        "Superseded" => DropReason::Superseded { by: at(NEWER) },
        "Withdrawn" => DropReason::Withdrawn { by: at(SENDER), authority: withdrawal_authority(spec) },
        "ConsumerUnreachable" => DropReason::ConsumerUnreachable { by: at(SUPERVISOR), authority: at(SUPERVISOR) },
        other => panic!("drop reason {other}"),
    }
}

/// Who withdraws: the turn-rules role, naming the delivery condition, when the
/// envelope has one; the sender otherwise.
fn withdrawal_authority(spec: &MessageSpec) -> Address {
    spec.delivery_condition.clone().unwrap_or_else(|| at(SENDER))
}

/// The drop's world: only the evidence its reason's rule reads.
fn drop_world(spec: &MessageSpec, o: &Value, now: Timestamp) -> Result<MemoryMessageWorld, &'static str> {
    let mut world = MemoryMessageWorld::default();
    let binding = Ref::<Binding>::new(INBOX);
    match tag(&o["reason"]) {
        "Expired" => {
            if boolean(&o["bound"]) {
                world.binding.put(&binding, bound());
            }
            world.applied_through.put(&Ref::<AppliedThrough>::new(INBOX), tick(number(&o["appliedThrough"])));
            if boolean(&o["met"]) {
                match spec.expectation {
                    Expectation::Nothing => return Err("met evidence for a Nothing expectation: nothing is expected, so none exists"),
                    Expectation::Reply => world.reply.put(&Ref::<Reply>::new(NAME), ()),
                    Expectation::Outcome { .. } => world.condition.put(&Ref::<Condition>::new(CONDITION), true),
                };
            }
        }
        "Undeliverable" => {
            let state = if boolean(&o["bound"]) {
                bound()
            } else if boolean(&o["noConsumerPastThreshold"]) {
                InboxBinding::Unbound { since: now.saturating_add(-2 * spec.unbound_after) }
            } else {
                InboxBinding::Unbound { since: now }
            };
            world.binding.put(&binding, state);
            if boolean(&o["receipt"]) {
                put_receipt(&mut world, &session(1), Transport::Turn);
            }
            if boolean(&o["failure"]) {
                world.delivery_failure.put(&Ref::<DeliveryFailure>::new(NAME), ());
            }
        }
        "Superseded" => {
            let older = if boolean(&o["supersedes"]) { NAME } else { "another" };
            world.supersession.put(&Ref::<Supersession>::new(NEWER), Supersedes { older: older.into() });
        }
        "Withdrawn" => {
            world.authority.put(&Ref::new(withdrawal_authority(spec).as_str()), boolean(&o["authorised"]));
            if boolean(&o["conditionFailed"]) {
                world.condition.put(&Ref::<Condition>::new(DELIVERY), false);
            }
        }
        "ConsumerUnreachable" => {
            world.authority.put(&Ref::new(SUPERVISOR), boolean(&o["authorised"]));
        }
        other => panic!("drop reason {other}"),
    }
    Ok(world)
}

struct MessageConformance;

impl Conformance for MessageConformance {
    const MACHINE: &'static str = "message";
    type Subject = Home<Message>;

    fn create(state: &Value) -> Home<Message> {
        let mut home = Home::<Message>::new("home").expect("valid declaration");
        home.create(NAME, spec_of(payload(state)), &MemoryMessageWorld::default(), Timestamp(0)).expect("created");
        home
    }

    fn apply(home: &mut Home<Message>, event: &Value) -> Step {
        let o = payload(event);
        let mut world = MemoryMessageWorld::default();
        match tag(event) {
            "Deliver" => {
                let who = session(number(&o["receiver"]["consumer"]));
                let receiver = Receiver { host: text(&o["receiver"]["host"]).into(), ..consumer(&who) };
                if boolean(&o["observed"]) {
                    let from = if boolean(&o["receiverMatches"]) { who } else { "another".into() };
                    let transport = if boolean(&o["transportMatches"]) { Transport::Turn } else { Transport::Notify };
                    put_receipt(&mut world, &from, transport);
                }
                outcome(home.apply(NAME, Deliver { receiver, transport: Transport::Turn }, &world, tick(1)))
            }
            "Settle" => {
                let observed = boolean(&payload(o)["observed"]);
                let by = match tag(o) {
                    "Answered" => {
                        if observed {
                            world.reply.put(&Ref::<Reply>::new(NAME), ());
                        }
                        Settlement::Answered
                    }
                    "OutcomeObserved" => {
                        world.condition.put(&Ref::<Condition>::new(CONDITION), observed);
                        Settlement::OutcomeObserved
                    }
                    other => panic!("settlement {other}"),
                };
                outcome(home.apply(NAME, Settle { by }, &world, tick(1)))
            }
            "DropMessage" => {
                let now = tick(number(&o["now"]));
                let spec = home.get(NAME).expect("exists").spec();
                let world = match drop_world(spec, o, now) {
                    Ok(world) => world,
                    Err(reason) => return Step::Unrealisable(reason),
                };
                let reason = drop_reason_of(spec, o);
                outcome(home.apply(NAME, DropMessage { reason }, &world, now))
            }
            other => panic!("message event {other}"),
        }
    }

    fn encode(home: &Home<Message>) -> Value {
        let m = home.get(NAME).expect("exists");
        let envelope = envelope_of(m.spec());
        match m.state() {
            MessageState::Pending => variant("Pending", envelope),
            MessageState::Settled { receiver } => {
                let receiver = int(receiver.as_ref().map_or(0, session_number));
                variant("Settled", json!({ "envelope": envelope, "receiver": receiver }))
            }
            MessageState::Awaiting { receiver } => {
                let receiver = json!({ "consumer": int(session_number(receiver)), "host": receiver.host });
                variant("Awaiting", json!({ "envelope": envelope, "receiver": receiver }))
            }
            MessageState::Dropped { reason, receiver, watermark } => {
                let reason = match reason {
                    DropReason::Undeliverable => "Undeliverable",
                    DropReason::Expired => "Expired",
                    DropReason::Superseded { .. } => "Superseded",
                    DropReason::Withdrawn { .. } => "Withdrawn",
                    DropReason::ConsumerUnreachable { .. } => "ConsumerUnreachable",
                };
                variant(
                    "Dropped",
                    json!({
                        "reason": bare(reason),
                        "envelope": envelope,
                        "receiver": int(receiver.as_ref().map_or(0, session_number)),
                        "watermark": int(watermark.map_or(0, ticks)),
                        "bound": watermark.is_some(),
                        "receipt": false,
                    }),
                )
            }
        }
    }

    fn version(home: &Home<Message>) -> u64 {
        home.get(NAME).expect("exists").version().0
    }
}

// Every reachable message state is reached in Rust by the spec's path, and
// every probe agrees: the same move to the same state value (a drop records
// the watermark and binding an expiry relied on), or the same refusal with
// nothing recorded. The harness only offers evidence a home could observe
// (no met expectation for a message expecting nothing), so no probe may be
// unrealisable.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn message_follows_the_full_graph() {
    let mut report = Report::new("message");
    graph::check::<MessageConformance>(&graph::load(MessageConformance::MACHINE), &mut report);
    report.finish();
}
