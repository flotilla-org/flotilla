//! One Message envelope, evidence and value mapping for every replay subject.
use crate::{
    itf::{boolean, int, list, number, payload, tag, variant},
    replay::{at_address, constant, id_of, moment, slot_at, Stream, Trace, DELIVERY, INBOX, NEWER, OUTCOME, OWNER, TURN_RULES, UNIT},
};
use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::{
        consumer::ConsumerKind,
        message::{
            self, DropReason, Expectation, InboxBinding, MemoryMessageWorld, Message, MessageBody, MessageEdge, MessageSpec, MessageState,
            Receiver, Relation,
        },
    },
    Ref, Resource, Transport,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub fn drop_reason(v: &Value) -> DropReason {
    match tag(v) {
        "Undeliverable" => DropReason::Undeliverable,
        "Expired" => DropReason::Expired,
        "Superseded" => DropReason::Superseded { by: at_address(NEWER) },
        "Withdrawn" => DropReason::Withdrawn { by: at_address(OWNER), authority: at_address(OWNER) },
        "ConsumerUnreachable" => DropReason::ConsumerUnreachable { by: at_address(OWNER), authority: at_address(OWNER) },
        other => panic!("drop reason {other}"),
    }
}

pub fn reason_name(reason: &DropReason) -> &'static str {
    match reason {
        DropReason::Undeliverable => "Undeliverable",
        DropReason::Expired => "Expired",
        DropReason::Superseded { .. } => "Superseded",
        DropReason::Withdrawn { .. } => "Withdrawn",
        DropReason::ConsumerUnreachable { .. } => "ConsumerUnreachable",
    }
}

pub fn session_number(receiver: &message::Receiver) -> i64 {
    id_of(&receiver.id)
}

/// The consumer named by session `id`: an agent session on the consumer host.
pub fn consumer(id: impl Into<String>) -> message::Receiver {
    message::Receiver { kind: ConsumerKind::AgentSession, id: id.into(), host: "consumer".into() }
}

/// A message to the scenarios' inbox, expecting nothing; each replay sets the rest.
pub fn message_spec() -> MessageSpec {
    let body = MessageBody::text("turn").expect("small body");
    MessageSpec::new(at_address("project:sender"), at_address(INBOX), Relation::Peer, body)
}

/// The spec's `Envelope = { expectation, hasDeadline, deadline, hasCondition }`,
/// from the message's spec. Encode the immutable envelope at a model's tick scale. The full-graph
/// harness keeps deadline=1 when absent; scenario traces keep deadline=0.
pub fn envelope_value_at(spec: &MessageSpec, unit: i64, missing_deadline: i64) -> Value {
    let expectation = match spec.expectation {
        Expectation::Nothing => "Nothing",
        Expectation::Reply => "Reply",
        Expectation::Outcome { .. } => "Outcome",
    };
    json!({"expectation":constant(expectation),"hasDeadline":spec.deadline.is_some(),"deadline":int(spec.deadline.map_or(missing_deadline,|time|time.micros()/unit)),"hasCondition":spec.delivery_condition.is_some()})
}

/// The model's message store with each deadline-less envelope's unused
/// `deadline` read as 0, as [`envelope_value_at`] renders it.
pub fn without_unset_deadline(v: &Value) -> Value {
    match v {
        Value::Object(fields) => {
            let mut fields: serde_json::Map<String, Value> = fields.iter().map(|(k, v)| (k.clone(), without_unset_deadline(v))).collect();
            if fields.get("hasDeadline") == Some(&Value::Bool(false)) {
                fields.insert("deadline".into(), int(0));
            }
            Value::Object(fields)
        }
        Value::Array(items) => Value::Array(items.iter().map(without_unset_deadline).collect()),
        other => other.clone(),
    }
}

/// The spec's `Pending(Envelope) | Awaiting({ envelope, receiver }) |
/// Settled({ envelope, receiver }) | Dropped(Drop)`: the spec's envelope and
/// the state's data, with receiver 0 for none. A drop's `bound` is whether it recorded a watermark, and its
/// `receipt` is false (see the module doc).
pub fn message_value(resource: &Resource<Message>) -> Value {
    message_value_at(resource, UNIT, 0, Clone::clone)
}

pub fn message_value_at(
    resource: &Resource<Message>,
    unit: i64,
    missing_deadline: i64,
    canonical: impl Fn(&Receiver) -> Receiver,
) -> Value {
    let envelope = envelope_value_at(resource.spec(), unit, missing_deadline);
    match resource.state() {
        MessageState::Pending => variant("Pending", envelope),
        MessageState::Awaiting { receiver } => variant(
            "Awaiting",
            json!({ "envelope": envelope, "receiver": { "consumer": int(session_number(&canonical(receiver))), "host": canonical(receiver).host } }),
        ),
        MessageState::Settled { receiver } => variant(
            "Settled",
            json!({ "envelope": envelope, "receiver": int(receiver.as_ref().map_or(0, |receiver|session_number(&canonical(receiver)))) }),
        ),
        MessageState::Dropped { reason, receiver, watermark } => variant(
            "Dropped",
            json!({
                "reason": constant(reason_name(reason)),
                "envelope": envelope,
                "receiver": int(receiver.as_ref().map_or(0, |receiver|session_number(&canonical(receiver)))),
                "watermark": int(watermark.map_or(0, |time|time.micros()/unit)),
                "bound": watermark.is_some(),
                "receipt": false,
            }),
        ),
    }
}

pub fn deliver(receiver: message::Receiver) -> message::Deliver {
    message::Deliver { receiver, transport: Transport::Turn }
}

/// The turn-rules role's withdrawal of a message whose delivery condition lapsed.
pub fn withdrawn_by_turn_rules() -> DropReason {
    DropReason::Withdrawn { by: at_address(TURN_RULES), authority: at_address(DELIVERY) }
}

/// The small operational boundary shared by a bare machine and an inbox.
/// Observations describe the applied stream; a backend adapter records newly
/// observed receipts through its own Receive operation rather than injecting them.
pub trait MessageSubject {
    fn create(spec: MessageSpec) -> Self;
    fn name(&self) -> &str;
    fn receiver(&self) -> Receiver {
        consumer("1")
    }
    fn canonical_receiver(&self, receiver: &Receiver) -> Receiver {
        receiver.clone()
    }
    fn bind(&mut self);
    fn observe(&mut self, world: MemoryMessageWorld, now: Timestamp);
    fn pending(&mut self) -> bool;
    fn apply(&mut self, edge: MessageEdge);
    /// Probe expiry; a refused probe must leave the resource unchanged.
    fn expire(&mut self) -> bool;
    fn resource(&self) -> Resource<Message>;
}

pub fn spec_from_envelope(envelope: &Value) -> MessageSpec {
    MessageSpec {
        expectation: match tag(&envelope["expectation"]) {
            "Nothing" => Expectation::Nothing,
            "Reply" => Expectation::Reply,
            "Outcome" => Expectation::Outcome { condition: at_address(OUTCOME) },
            other => panic!("expectation {other}"),
        },
        deadline: boolean(&envelope["hasDeadline"]).then(|| moment(number(&envelope["deadline"]))),
        unbound_after: UNIT,
        delivery_condition: boolean(&envelope["hasCondition"]).then(|| at_address(DELIVERY)),
        ..message_spec()
    }
}

pub fn observed_world(
    name: &str,
    stream: &Stream,
    binding: InboxBinding,
    receipts: BTreeMap<Receiver, Transport>,
    delivery_condition: Option<bool>,
    authorised: bool,
) -> MemoryMessageWorld {
    let mut world = MemoryMessageWorld::default();
    world.receipts.put(&Ref::new(name), receipts);
    if stream.any("reply") {
        world.reply.put(&Ref::new(name), ());
    }
    world.condition.put(&Ref::new(OUTCOME), stream.any("outcome"));
    if stream.any("failure") {
        world.delivery_failure.put(&Ref::new(name), ());
    }
    world.applied_through.put(&Ref::new(INBOX), moment(stream.watermark()));
    world.binding.put(&Ref::new(INBOX), binding);
    if let Some(holds) = delivery_condition {
        world.condition.put(&Ref::new(DELIVERY), holds);
    }
    world.authority.put(&Ref::new(OWNER), authorised);
    if authorised {
        world.supersession.put(&Ref::new(NEWER), message::Supersedes { older: name.into() });
    }
    world
}

/// One Message race mapping for all subjects: applied receipt evidence,
/// bindings, pending reads, enabled edges and state equality after every step.
pub fn replay_race<S: MessageSubject>(trace: &Trace<'_>) -> usize {
    let initial = slot_at(&trace.states[0], "::messages", 1).expect("initial message");
    assert_eq!(tag(initial), "Pending");
    let spec = spec_from_envelope(payload(initial));
    let expectation = spec.expectation.clone();
    let deadline = spec.deadline;
    let mut subject = S::create(spec);
    subject.bind();
    let mut writes = 0;
    for step in trace.steps() {
        let stream = Stream::of(&step.before["source"]);
        let receiver = subject.receiver();
        let receipts = stream
            .sessions("receipt")
            .iter()
            .map(|who| {
                assert_eq!(*who, 1, "race has one consumer");
                (receiver.clone(), Transport::Turn)
            })
            .collect();
        let world =
            observed_world(subject.name(), &stream, InboxBinding::Bound { consumers: [receiver.clone()].into() }, receipts, None, true);
        subject.observe(world, moment(number(&step.before["now"])));
        let expected_pending = *subject.resource().state() == MessageState::Pending && !stream.any("receipt");
        assert_eq!(subject.pending(), expected_pending, "{step}: pending follows binding and receipts");
        if let Some(deadline) = deadline {
            let evidence = match expectation {
                Expectation::Reply => Some("reply"),
                Expectation::Outcome { .. } => Some("outcome"),
                Expectation::Nothing => None,
            };
            // Inspect every written row, even those not yet applied at home.
            // Probe the real subject, including when the model chose another edge.
            let in_time = stream.rows.iter().any(|row| Some(row.kind.as_str()) == evidence && moment(row.at) <= deadline);
            if in_time {
                assert!(!subject.expire(), "{step}: expiry beat evidence written before the deadline");
            }
        }
        let edge: Option<MessageEdge> = match step.action() {
            "deliver" => Some(deliver(receiver).into()),
            "settle" => Some(
                message::Settle {
                    by: if expectation == Expectation::Reply {
                        message::Settlement::Answered
                    } else {
                        message::Settlement::OutcomeObserved
                    },
                }
                .into(),
            ),
            "drop" => {
                let dropped = slot_at(step.after, "::messages", 1).expect("dropped message");
                let reason = &payload(dropped)["reason"];
                assert_eq!(step.pick("reason"), reason, "{step}: recorded chosen reason");
                Some(message::DropMessage { reason: drop_reason(reason) }.into())
            }
            "publish" | "applySource" | "disconnect" | "connect" | "tick" => None,
            other => panic!("{step}: unknown race action {other}"),
        };
        if let Some(edge) = edge {
            subject.apply(edge);
            writes += 1;
        }
        let resource = subject.resource();
        let actual = message_value_at(&resource, UNIT, 0, |receiver| subject.canonical_receiver(receiver));
        let expected = without_unset_deadline(slot_at(step.after, "::messages", 1).expect("model message"));
        crate::replay::same(&actual, &expected, &step, "message");
    }
    writes
}

/// Replay every message_race trace from QUINT_TRACE_DIR. Other scenario files
/// share that directory and belong to their own engine adapters.
pub fn replay_message_race<S: MessageSubject>() {
    let directory = std::env::var("QUINT_TRACE_DIR").expect("QUINT_TRACE_DIR from check.sh");
    let mut paths: Vec<_> = std::fs::read_dir(&directory)
        .expect("trace directory")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| {
            path.file_name().expect("name").to_string_lossy().starts_with("message_race-") && path.to_string_lossy().ends_with(".itf.json")
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "message_race traces required");
    for path in paths {
        let file = path.file_name().expect("name").to_string_lossy();
        let trace: Value = serde_json::from_slice(&std::fs::read(&path).expect("trace file")).expect("ITF");
        replay_race::<S>(&Trace { file: &file, states: list(&trace["states"]) });
    }
}
