//! message_race's applied stream is its receipt evidence, as in the engine
//! replay: the model deliberately has no consumer resources in this scenario.
use super::itf::{self, at, boolean, int, list, number, payload, slot, tag, text, variant};
use crate::{address, Factory};
use flotilla_inbox::InboxBackend;
use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::{
        consumer::ConsumerKind,
        message::{
            self, DropReason, Expectation, InboxBinding, MemoryMessageWorld, Message, MessageBody, MessageSpec, MessageState, Receiver,
            Relation,
        },
    },
    Ref, Resource, Transport,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, sync::atomic::Ordering};

const UNIT: i64 = 600_000_000;
const INBOX: &str = "project:inbox";
const OUTCOME: &str = "project:outcome";
const NEWER: &str = "project:newer";
const OWNER: &str = "project:owner";
fn moment(t: i64) -> Timestamp {
    Timestamp(t * UNIT)
}
fn ticks(t: Timestamp) -> i64 {
    t.micros() / UNIT
}
fn constant(name: &str) -> Value {
    variant(name, itf::unit())
}
fn receiver() -> Receiver {
    Receiver { kind: ConsumerKind::AgentSession, id: "1".into(), host: "consumer".into() }
}
fn session_number(receiver: &Receiver) -> i64 {
    receiver.id.parse().expect("model consumer")
}
fn reason_name(reason: &DropReason) -> &'static str {
    match reason {
        DropReason::Undeliverable => "Undeliverable",
        DropReason::Expired => "Expired",
        DropReason::Superseded { .. } => "Superseded",
        DropReason::Withdrawn { .. } => "Withdrawn",
        DropReason::ConsumerUnreachable { .. } => "ConsumerUnreachable",
    }
}
fn model(state: &Value) -> &Value {
    let store = state.as_object().expect("state").iter().find(|(key, _)| key.ends_with("::messages")).expect("message store").1;
    slot(at(store, 1)).expect("message")
}
/// The spec's `Envelope = { expectation, hasDeadline, deadline, hasCondition }`,
/// from the message's spec. Without a deadline Rust has no value, so it renders 0.
fn envelope_value(spec: &MessageSpec) -> Value {
    let expectation = match spec.expectation {
        Expectation::Nothing => "Nothing",
        Expectation::Reply => "Reply",
        Expectation::Outcome { .. } => "Outcome",
    };
    json!({
        "expectation": constant(expectation),
        "hasDeadline": spec.deadline.is_some(),
        "deadline": int(spec.deadline.map_or(0, ticks)),
        "hasCondition": spec.delivery_condition.is_some(),
    })
}

/// The model's message store with each deadline-less envelope's unused
/// `deadline` read as 0, as [`envelope_value`] renders it.
fn without_unset_deadline(v: &Value) -> Value {
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
fn message_value(resource: &Resource<Message>) -> Value {
    let envelope = envelope_value(resource.spec());
    match resource.state() {
        MessageState::Pending => variant("Pending", envelope),
        MessageState::Awaiting { receiver } => variant(
            "Awaiting",
            json!({ "envelope": envelope, "receiver": { "consumer": int(receiver.id.parse::<i64>().expect("model consumer")), "host": receiver.host } }),
        ),
        MessageState::Settled { receiver } => {
            variant("Settled", json!({ "envelope": envelope, "receiver": int(receiver.as_ref().map_or(0, session_number)) }))
        }
        MessageState::Dropped { reason, receiver, watermark } => variant(
            "Dropped",
            json!({
                "reason": constant(reason_name(reason)),
                "envelope": envelope,
                "receiver": int(receiver.as_ref().map_or(0, session_number)),
                "watermark": int(watermark.map_or(0, ticks)),
                "bound": watermark.is_some(),
                "receipt": false,
            }),
        ),
    }
}

/// Replay all generated message_race ITF files, checking state after every
/// world or machine step. The caller chooses the factory, so both backends run
/// exactly this mapping and oracle.
pub fn replay<F: Factory + Default>() {
    let directory = std::env::var("QUINT_TRACE_DIR").expect("QUINT_TRACE_DIR from quint run");
    let mut paths: Vec<_> = fs::read_dir(directory)
        .expect("trace directory")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| {
            path.file_name().expect("name").to_string_lossy().starts_with("message_race-")
                && path.extension().is_some_and(|ext| ext == "json")
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "message_race traces required");
    let factory = F::default();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    for path in paths {
        let trace: Value = serde_json::from_slice(&fs::read(&path).expect("trace")).expect("ITF");
        let states = list(&trace["states"]);
        let envelope = payload(model(&states[0]));
        let expectation = match tag(&envelope["expectation"]) {
            "Nothing" => Expectation::Nothing,
            "Reply" => Expectation::Reply,
            "Outcome" => Expectation::Outcome { condition: address(OUTCOME) },
            other => panic!("expectation {other}"),
        };
        let spec = MessageSpec {
            expectation: expectation.clone(),
            deadline: boolean(&envelope["hasDeadline"]).then(|| moment(number(&envelope["deadline"]))),
            unbound_after: UNIT,
            ..MessageSpec::new(address("project:sender"), address(INBOX), Relation::Peer, MessageBody::text("turn").expect("body"))
        };
        let harness = factory.create();
        runtime.block_on(async {
            let message = harness.backend.enqueue(spec).await.expect("enqueue");
            harness.backend.bind(address(INBOX), receiver()).await.expect("always-bound consumer");
            for (index, pair) in states.windows(2).enumerate() {
                let before = &pair[0];
                let after = &pair[1];
                let now = moment(number(&before["now"]));
                harness.evidence.clock.store(now.micros(), Ordering::SeqCst);
                let source = &before["source"];
                let rows = list(&source["rows"]);
                let applied = usize::try_from(number(&source["applied"])).expect("applied prefix");
                let seen = &rows[..applied];
                let has = |kind: &str| seen.iter().any(|row| text(&row["kind"]) == kind);
                let mut world = MemoryMessageWorld::default();
                world.binding.put(&Ref::new(INBOX), InboxBinding::Bound { consumers: [receiver()].into() });
                if has("receipt") {
                    world.receipts.put(&Ref::new(&message), BTreeMap::from([(receiver(), Transport::Turn)]));
                }
                if has("reply") {
                    world.reply.put(&Ref::new(&message), ());
                }
                if has("failure") {
                    world.delivery_failure.put(&Ref::new(&message), ());
                }
                world.condition.put(&Ref::new(OUTCOME), has("outcome"));
                let watermark =
                    seen.iter().filter(|row| text(&row["kind"]) == "keepalive").map(|row| number(&row["at"])).max().unwrap_or(0);
                world.applied_through.put(&Ref::new(INBOX), moment(watermark));
                world.authority.put(&Ref::new(OWNER), true);
                world.supersession.put(&Ref::new(NEWER), message::Supersedes { older: message.clone() });
                *harness.evidence.world.lock().expect("evidence") = world;
                let action = text(&after["mbt::actionTaken"]);
                match action {
                    "deliver" => harness
                        .backend
                        .deliver(&message, message::Deliver { receiver: receiver(), transport: Transport::Turn })
                        .await
                        .expect("model accepted deliver"),
                    "settle" => harness
                        .backend
                        .settle(
                            &message,
                            message::Settle {
                                by: if expectation == Expectation::Reply {
                                    message::Settlement::Answered
                                } else {
                                    message::Settlement::OutcomeObserved
                                },
                            },
                        )
                        .await
                        .expect("model accepted settle"),
                    "drop" => {
                        let reason = match tag(&payload(model(after))["reason"]) {
                            "Expired" => DropReason::Expired,
                            "Undeliverable" => DropReason::Undeliverable,
                            "Superseded" => DropReason::Superseded { by: address(NEWER) },
                            "Withdrawn" => DropReason::Withdrawn { by: address(OWNER), authority: address(OWNER) },
                            "ConsumerUnreachable" => DropReason::ConsumerUnreachable { by: address(OWNER), authority: address(OWNER) },
                            other => panic!("reason {other}"),
                        };
                        harness.backend.drop(&message, reason).await.expect("model accepted drop");
                    }
                    "publish" | "applySource" | "disconnect" | "connect" | "tick" => {}
                    other => panic!("race action {other}"),
                }
                let actual = message_value(&factory.inspect(&harness.backend, &message));
                let expected = without_unset_deadline(model(after));
                assert_eq!(itf::normalise(&actual), itf::normalise(&expected), "{} step {} ({action})", path.display(), index + 1);
            }
        });
    }
}
