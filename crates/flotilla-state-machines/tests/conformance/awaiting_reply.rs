//! AwaitingReply against `conformance/awaiting_reply.qnt`.
//!
//! The harness's variable is the whole `Record = {request, state}`, and its
//! probes step from and to the bare `State`. A step never changes the
//! request, so each probe's request is its node's, and the graph is checked
//! over records: every probe's `from` and `Moved` target become `{request,
//! state}` with that request, and an initial node's record is its state.
//! Nothing is dropped, so no spec state is projected.
//!
//! The request is the record's [`AwaitingReplySpec`]: message `n` is
//! `m<n>` at the receiver's home; `sender`, `recipient` and `subject` are the
//! project addresses of those names; `kind` is itself; the expectation is
//! [`Awaited`] by constructor; and `At(d)` is a deadline of `d` model ticks
//! (times map through [`tick`]).
//!
//! Event mapping, each with a fresh [`MemoryAwaitingReplyWorld`]:
//! - `Answer{reply, inReplyTo, present}`: the answer edge names reply
//!   `m<reply>`; when `present`, it landed in the sender's inbox replying to
//!   message `inReplyTo`, and otherwise nothing landed.
//! - `Expire{now, replyPresent}`: the expire edge at `now`; when
//!   `replyPresent`, a reply to the awaited message landed in the sender's
//!   inbox.
//! - `Withdraw{authorised, reason}`: the withdraw edge with `reason`, its
//!   authority reading `authorised`.
//!
//! State mapping: `Open` and `Withdrawn { reason }` are the spec's
//! constructors. The spec's `Answered({reply})` and `Expired({at})` carry what
//! Rust keeps outside the label: the reply is the answering `Answer` edge's,
//! read from the record's log, and the time is the state's `since`.

use flotilla_schema::{Address, Timestamp};
use flotilla_state_machines::{
    machines::{
        awaiting_reply::{
            Answer, Awaited, AwaitingReply, AwaitingReplyEdge, AwaitingReplyRefusal, AwaitingReplySpec, AwaitingReplyState, Expire,
            MemoryAwaitingReplyWorld, Withdraw,
        },
        message::MessageRef,
    },
    Home, Ref, Version, WriteError,
};
use serde_json::{json, Value};

use crate::{
    graph::{self, bare, refusal, tick, Conformance, Report, Step, UNIT},
    itf::{boolean, int, number, payload, tag, text, variant},
};

const NAME: &str = "r";
const OWNER: &str = "owner";

/// Message `n`, homed at its receiver.
fn message(n: i64) -> MessageRef {
    MessageRef::new(format!("m{n}"), "receiver")
}

/// Message `m<n>`'s spec number `n`.
fn number_of(message: &MessageRef) -> i64 {
    message.name.strip_prefix('m').and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("conformance message {}", message.name))
}

/// The project address of a harness name.
fn named(name: &str) -> Address {
    Address::project(name).expect("conformance name")
}

fn spec_of(request: &Value) -> AwaitingReplySpec {
    AwaitingReplySpec {
        message: message(number(&request["message"])),
        sender: named(text(&request["sender"])),
        recipient: named(text(&request["recipient"])),
        subject: Some(named(text(&request["subject"]))),
        kind: text(&request["kind"]).into(),
        awaited: match tag(&request["expectation"]) {
            "Reply" => Awaited::Reply,
            "Outcome" => Awaited::Outcome,
            other => panic!("expectation {other}"),
        },
        deadline: match tag(&request["deadline"]) {
            "Never" => None,
            "At" => Some(tick(number(payload(&request["deadline"])))),
            other => panic!("deadline {other}"),
        },
    }
}

fn request_value(spec: &AwaitingReplySpec) -> Value {
    json!({
        "message": int(number_of(&spec.message)),
        "sender": spec.sender.name(),
        "recipient": spec.recipient.name(),
        "subject": spec.subject.as_ref().expect("the harness's subject").name(),
        "kind": spec.kind,
        "expectation": bare(match spec.awaited {
            Awaited::Reply => "Reply",
            Awaited::Outcome => "Outcome",
        }),
        "deadline": spec.deadline.map_or_else(|| bare("Never"), |deadline| variant("At", int(deadline.0 / UNIT))),
    })
}

/// The graph over records: each probe's state and target carry its node's request.
fn records(nodes: Vec<Value>) -> Vec<Value> {
    let record = |request: &Value, state: &Value| json!({ "request": request, "state": state });
    nodes
        .into_iter()
        .map(|mut node| {
            let request = node["record"]["request"].clone();
            match tag(&node["probe"]) {
                "Started" => node["state"] = node["record"].clone(),
                "Stepped" => {
                    let probe = &mut node["probe"]["value"];
                    probe["from"] = record(&request, &probe["from"]);
                    if tag(&probe["result"]) == "Moved" {
                        probe["result"]["value"] = record(&request, &probe["result"]["value"]);
                    }
                }
                other => panic!("awaiting reply probe {other}"),
            }
            node
        })
        .collect()
}

fn outcome<T>(result: Result<T, WriteError<AwaitingReply>>) -> Step {
    let name = match result {
        Ok(_) => return Step::Accepted,
        Err(WriteError::Refused(r)) => match r {
            AwaitingReplyRefusal::NotOpen => "NotOpen",
            AwaitingReplyRefusal::ReplyAbsent => "ReplyAbsent",
            AwaitingReplyRefusal::NotAReply => "NotAReply",
            AwaitingReplyRefusal::NoDeadline => "NoDeadline",
            AwaitingReplyRefusal::NotDue => "NotDue",
            AwaitingReplyRefusal::ReplyPresent => "ReplyPresent",
            AwaitingReplyRefusal::Unauthorised => "Unauthorised",
        },
        Err(other) => return refusal(&format!("{other:?}")),
    };
    refusal(name)
}

/// The reply the record's `Answer` edge named.
fn answering_reply(home: &Home<AwaitingReply>) -> i64 {
    let history = home.log().history(NAME, Version(0), usize::MAX);
    let reply = history.iter().find_map(|(_, event)| match &event.arguments.edge {
        AwaitingReplyEdge::Answer(answer) => Some(&answer.reply),
        _ => None,
    });
    number_of(reply.expect("an answered record's log holds its answer"))
}

struct AwaitingReplyConformance;

impl Conformance for AwaitingReplyConformance {
    const MACHINE: &'static str = "awaiting_reply";
    type Subject = Home<AwaitingReply>;

    fn create(record: &Value) -> Home<AwaitingReply> {
        let mut home = Home::<AwaitingReply>::new("sender-home").expect("valid declaration");
        home.create(NAME, spec_of(&record["request"]), &MemoryAwaitingReplyWorld::default(), Timestamp(0)).expect("created");
        home
    }

    fn apply(home: &mut Home<AwaitingReply>, event: &Value) -> Step {
        let o = payload(event);
        let spec = home.get(NAME).expect("exists").spec().clone();
        let mut world = MemoryAwaitingReplyWorld::default();
        match tag(event) {
            "Answer" => {
                let reply = message(number(&o["reply"]));
                if boolean(&o["present"]) {
                    world.land(&reply, &spec.sender, Some(&message(number(&o["inReplyTo"]))));
                }
                outcome(home.apply(NAME, Answer { reply }, &world, tick(1)))
            }
            "Expire" => {
                if boolean(&o["replyPresent"]) {
                    world.land(&message(2), &spec.sender, Some(&spec.message));
                }
                outcome(home.apply(NAME, Expire, &world, tick(number(&o["now"]))))
            }
            "Withdraw" => {
                world.authority.put(&Ref::new(OWNER), boolean(&o["authorised"]));
                let withdraw = Withdraw { by: spec.sender.clone(), reason: text(&o["reason"]).into(), authority: Ref::new(OWNER) };
                outcome(home.apply(NAME, withdraw, &world, tick(1)))
            }
            other => panic!("awaiting reply event {other}"),
        }
    }

    fn encode(home: &Home<AwaitingReply>) -> Value {
        let record = home.get(NAME).expect("exists");
        let state = match record.state() {
            AwaitingReplyState::Open => bare("Open"),
            AwaitingReplyState::Answered => variant("Answered", json!({ "reply": int(answering_reply(home)) })),
            AwaitingReplyState::Expired => variant("Expired", json!({ "at": int(record.since().0 / UNIT) })),
            AwaitingReplyState::Withdrawn { reason } => variant("Withdrawn", json!({ "reason": reason })),
        };
        json!({ "request": request_value(record.spec()), "state": state })
    }

    fn version(home: &Home<AwaitingReply>) -> u64 {
        home.get(NAME).expect("exists").version().0
    }
}

// Every reachable record is reached in Rust by the spec's path, and every
// probe agrees: the same move to the same record (an answer naming its reply,
// an expiry its time, a withdrawal its reason), or the same refusal with
// nothing recorded.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn awaiting_reply_follows_the_full_graph() {
    let mut report = Report::new("awaiting_reply");
    graph::check::<AwaitingReplyConformance>(&records(graph::load(AwaitingReplyConformance::MACHINE)), &mut report);
    report.finish();
}
