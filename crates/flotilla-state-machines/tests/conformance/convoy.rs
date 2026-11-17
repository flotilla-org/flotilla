//! Convoy against `conformance/convoy.qnt`.
//!
//! Admission: each `Admitted({request, result})` probe is a [`Home::create`]
//! of the [`ConvoyRequest`], refused with the same variant or reaching the
//! same `Open` work.
//!
//! Event mapping, each with a fresh [`MemoryConvoyWorld`]:
//! - `Declare`, `Reject`, `Keep(Observed)`, `Keep(Verdict)`, `Retract` and
//!   `Reassign`: those edges, field for field.
//! - `Submit{durable}`: the `submit` edge, its durability reading `durable`.
//! - `RetireDuty`, `AddSeat`, `Resupervise`, `RetireSeat`, `FailSeat`,
//!   `Link`, `Unlink`, `Abandon` and `Readmit`: those edges, their authority
//!   reading `authorised`.
//! - `Deliver`: the `deliver` edge, with its disposition.
//!
//! Value mapping:
//! - A seat is the [`SeatName`] whose role is the spec's text and whose
//!   discriminator is empty.
//! - The project, a promise's target, a submission's reference and a
//!   supervisor are addresses ([`address`]).
//! - A vessel class is its number as text.
//! - A subject is an address; the spec's link source `ByOperator` is
//!   [`LinkSource::Operator`], and its abandon causes `WorkFailed` and
//!   `OperatorDecision` are [`AbandonCause::Failed`] and
//!   [`AbandonCause::Operator`].
//! - A duty's terms are its note.
//! - A submission's revision is fixed: the spec's reference is all it compares.
//!
//! State mapping: the spec's `Work = {spec, promises, seats, retired, quota,
//! retiredSeats, subjects}` is the spec's project, duties and priority, the
//! promise and seat records keyed by id and name, the state's retired duties
//! and quota, and the subjects linked now (the spec keeps no unlinked rows).
//! The state is `{work, phase}`: `Open { .. }` is phase `Open`,
//! `Delivered { disposition, .. }` is `Delivered(disposition)` and
//! `Abandoned { cause, reason, .. }` is `Abandoned({cause, reason})`. The
//! frozen promise kinds, each link's source and each row's stamps and times
//! are not in the spec.

use flotilla_schema::{Frozen, Timestamp};
use flotilla_state_machines::{
    machines::convoy::{
        Abandon, AbandonCause, AddSeat, AdmittedPromise, Authority, Convoy, ConvoyRefusal, ConvoyRequest, ConvoySpec, ConvoyState, Declare,
        Deliver, Duty, DutyId, DutyScope, FailSeat, Keep, Link, LinkSource, MemoryConvoyWorld, Priority, Progress, PromiseId, PromiseKinds,
        PromiseSource, Quota, Readmit, Reassign, Reject, ReplaceSeat, Resupervise, RetireDuty, RetireSeat, Retract, Seat, SeatName, Submit,
        Terms, Unlink, VesselClass,
    },
    Home, Ref, Resource, WriteError,
};
use serde_json::{json, Map, Value};

use crate::{
    graph::{self, bare, refusal, tick, Conformance, Report, Step},
    itf::{address, boolean, int, list, map_of, normalise, number, pairs, payload, set, set_of, show, tag, text, variant},
};

/// The name every request is a launch of; its convoy is that name's newest launch.
const LAUNCH_OF: &str = "c";
const DECIDER: &str = "decider";
const REVISION: &str = "r";

fn id(v: &Value) -> u32 {
    u32::try_from(number(v)).expect("model id")
}

fn seat(v: &Value) -> SeatName {
    SeatName::new(text(v), "")
}

fn seat_text(seat: &SeatName) -> Value {
    assert!(seat.seat.is_empty(), "model seats have no discriminator: {seat:?}");
    json!(seat.role)
}

/// A map keyed by text: TLC prints it as a record (`{"a": ..}`), and an empty
/// one as an empty sequence.
fn named(v: &Value) -> Vec<(&str, &Value)> {
    match v {
        Value::Object(fields) => fields.iter().map(|(key, value)| (key.as_str(), value)).collect(),
        Value::Array(items) if items.is_empty() => Vec::new(),
        _ => pairs(v).iter().map(|p| (text(&p[0]), &p[1])).collect(),
    }
}

fn class(n: i64) -> VesselClass {
    VesselClass::new(n.to_string())
}

fn quota_of(v: &Value) -> Quota {
    pairs(v).iter().map(|p| (class(number(&p[0])), id(&p[1]))).collect()
}

fn quota_value(quota: &Quota) -> Value {
    map_of(quota.iter().map(|(class, count)| (int(class.0.parse().expect("model class")), int((*count).into()))))
}

fn source_of(v: &Value) -> PromiseSource {
    match tag(v) {
        "Template" => PromiseSource::Template,
        "Dispatch" => PromiseSource::Dispatch,
        "Crew" => PromiseSource::Crew,
        other => panic!("source {other}"),
    }
}

fn source_name(source: PromiseSource) -> &'static str {
    match source {
        PromiseSource::Template => "Template",
        PromiseSource::Dispatch => "Dispatch",
        PromiseSource::Crew => "Crew",
    }
}

fn link_source_of(v: &Value) -> LinkSource {
    match tag(v) {
        "Claim" => LinkSource::Claim,
        "ByOperator" => LinkSource::Operator,
        "Relay" => LinkSource::Relay,
        "Branch" => LinkSource::Branch,
        other => panic!("link source {other}"),
    }
}

fn cause_of(v: &Value) -> AbandonCause {
    match tag(v) {
        "WorkFailed" => AbandonCause::Failed,
        "OperatorDecision" => AbandonCause::Operator,
        other => panic!("abandon cause {other}"),
    }
}

fn cause_name(cause: AbandonCause) -> &'static str {
    match cause {
        AbandonCause::Failed => "WorkFailed",
        AbandonCause::Operator => "OperatorDecision",
    }
}

fn authority_of(v: &Value) -> Authority {
    match tag(v) {
        "Operator" => Authority::Operator,
        "ConvoySupervisor" => Authority::ConvoySupervisor,
        "Dispatcher" => Authority::Dispatcher,
        "Owner" => Authority::Owner,
        "OwnerSupervisor" => Authority::OwnerSupervisor,
        other => panic!("authority {other}"),
    }
}

fn duty_of(v: &Value) -> Duty {
    let scope = match tag(&v["scope"]) {
        "Lifetime" => DutyScope::Lifetime,
        "WhileOpen" => DutyScope::WhileOpen { promises: set(payload(&v["scope"])).iter().map(|p| PromiseId(id(p))).collect() },
        other => panic!("scope {other}"),
    };
    Duty { owner: seat(&v["owner"]), scope, terms: Terms { note: Some(text(&v["terms"]).into()), ..Terms::default() } }
}

fn duty_value(duty: &Duty) -> Value {
    let scope = match &duty.scope {
        DutyScope::Lifetime => bare("Lifetime"),
        DutyScope::WhileOpen { promises } => variant("WhileOpen", set_of(promises.iter().map(|p| int(p.0.into())))),
    };
    json!({ "owner": seat_text(&duty.owner), "scope": scope, "terms": duty.terms.note.as_deref().unwrap_or_default() })
}

fn seat_of(name: &str, v: &Value) -> Seat {
    let parent = match tag(&v["parent"]) {
        "Root" => None,
        _ => Some(seat(payload(&v["parent"]))),
    };
    let failed = text(&v["failed"]);
    Seat {
        failed: (!failed.is_empty()).then(|| failed.into()),
        retired: false,
        replacements: id(&v["replacements"]),
        role: name.into(),
        seat: String::new(),
        parent,
        supervised_by: address(&v["supervisedBy"]),
    }
}

fn seat_value(seat: &Seat) -> Value {
    let parent = match &seat.parent {
        None => bare("Root"),
        Some(parent) => variant("Under", seat_text(parent)),
    };
    json!({
        "parent": parent,
        "supervisedBy": seat.supervised_by.name(),
        "replacements": int(seat.replacements.into()),
        "failed": seat.failed.as_deref().unwrap_or_default(),
    })
}

fn kinds() -> Frozen<PromiseKinds> {
    Frozen::new(PromiseKinds::default()).expect("freeze")
}

fn request_of(request: &Value) -> ConvoyRequest {
    let promises = pairs(&request["promises"])
        .iter()
        .map(|p| {
            let admitted = &p[1];
            // The spec names promises by id; a label is per-convoy data it does not model.
            let promise = AdmittedPromise {
                label: format!("p{}", id(&p[0])),
                kind: text(&admitted["promise"]["kind"]).into(),
                target: address(&admitted["promise"]["target"]),
                source: source_of(&admitted["source"]),
                owner: seat(&admitted["owner"]),
            };
            (PromiseId(id(&p[0])), promise)
        })
        .collect();
    let duties = pairs(&request["duties"]).iter().map(|p| (DutyId(id(&p[0])), duty_of(&p[1]))).collect();
    // Parents before the seats under them, as admission takes them.
    let mut seats: Vec<Seat> = named(&request["seats"]).into_iter().map(|(name, v)| seat_of(name, v)).collect();
    seats.sort_by_key(|seat| seat.parent.is_some());
    ConvoyRequest {
        launch_of: LAUNCH_OF.into(),
        workflow_ref: "w".into(),
        admission: flotilla_schema::Frozen::new(flotilla_state_machines::machines::convoy::AdmissionBytes::new(Vec::new()))
            .expect("freeze"),
        project: address(&request["project"]),
        promises,
        duties,
        seats,
        quota: quota_of(&request["quota"]),
        priority: Priority(id(&request["priority"])),
        kinds: kinds(),
    }
}

fn refusal_of(refusal: ConvoyRefusal) -> Value {
    let name = match &refusal {
        ConvoyRefusal::PromisesOutstanding(ids) => {
            return variant("PromisesOutstanding", set_of(ids.iter().map(|p| int(p.0.into()))));
        }
        ConvoyRefusal::DutyHolds(ids) => return variant("DutyHolds", set_of(ids.iter().map(|d| int(d.0.into())))),
        ConvoyRefusal::NoObligations => "NoObligations",
        ConvoyRefusal::UnknownProject => "UnknownProject",
        ConvoyRefusal::InvalidQuota => "InvalidQuota",
        ConvoyRefusal::NotOpen => "NotOpen",
        ConvoyRefusal::UnknownPromise => "UnknownPromise",
        ConvoyRefusal::PromiseSettled => "PromiseSettled",
        ConvoyRefusal::Unauthorised => "Unauthorised",
        ConvoyRefusal::QuotaNotIncrease => "QuotaNotIncrease",
        ConvoyRefusal::PromiseExists => "PromiseExists",
        ConvoyRefusal::NotOwner => "NotOwner",
        ConvoyRefusal::NotDurable => "NotDurable",
        ConvoyRefusal::NotSubmitted => "NotSubmitted",
        ConvoyRefusal::StaleEvidence => "StaleEvidence",
        ConvoyRefusal::UnknownDuty => "UnknownDuty",
        ConvoyRefusal::UnknownSeat => "UnknownSeat",
        ConvoyRefusal::SeatExists => "SeatExists",
        ConvoyRefusal::SeatRetired => "SeatRetired",
        ConvoyRefusal::SeatsFull => "SeatsFull",
        ConvoyRefusal::ReplacementsExhausted => "ReplacementsExhausted",
        ConvoyRefusal::SeatFailed => "SeatFailed",
        ConvoyRefusal::SubjectLinked => "SubjectLinked",
        ConvoyRefusal::UnknownSubject => "UnknownSubject",
        ConvoyRefusal::SeatCycle => "SeatCycle",
        ConvoyRefusal::NotEnded => "NotEnded",
        ConvoyRefusal::Held(holds) => return variant("Held", set_of(holds.iter().map(|hold| json!(hold)))),
        ConvoyRefusal::HoldsUnread => "HoldsUnread",
    };
    bare(name)
}

fn outcome<T>(result: Result<T, WriteError<Convoy>>) -> Step {
    match result {
        Ok(_) => Step::Accepted,
        Err(WriteError::Refused(r)) => Step::Refused(refusal_of(r)),
        Err(other) => refusal(&format!("{other:?}")),
    }
}

fn progress_name(progress: Progress) -> &'static str {
    match progress {
        Progress::Pending => "Pending",
        Progress::Submitted => "Submitted",
        Progress::Kept => "Kept",
        Progress::Retracted => "Retracted",
    }
}

/// The spec's `Work`: the spec's fixed fields, the records keyed by id and
/// name, and the state's retired duties and quota in force.
fn work(spec: &ConvoySpec, state: &ConvoyState) -> Value {
    let promises = spec.promises.iter().map(|row| {
        let promise = &row.record;
        let submissions: Vec<Value> = promise.submissions.iter().map(|row| json!({ "reference": row.record.address.name() })).collect();
        let record = json!({
            "promise": { "kind": promise.kind, "target": promise.target.name() },
            "source": bare(source_name(promise.source)),
            "owner": seat_text(&promise.owner),
            "state": bare(progress_name(promise.progress.value)),
            "submissions": submissions,
        });
        (int(promise.id.0.into()), record)
    });
    let seats: Map<String, Value> = spec.seats.iter().map(|row| (row.record.role.clone(), seat_value(&row.record))).collect();
    let duties = spec.duties.iter().map(|(d, duty)| (int(d.0.into()), duty_value(duty)));
    json!({
        "spec": { "project": spec.project.name(), "duties": map_of(duties), "priority": int(spec.priority.0.into()) },
        "promises": map_of(promises),
        "seats": Value::Object(seats),
        "retiredSeats": set_of(spec.seats.iter().filter(|r| r.record.retired).map(|r| seat_text(&r.record.name()))),
        "subjects": set_of(spec.linked().into_iter().map(|subject| json!(subject.name()))),
        "retired": set_of(state.retired().iter().map(|d| int(d.0.into()))),
        "quota": quota_value(state.quota()),
    })
}

/// The request that admitted an initial open `work`: its fixed fields, its
/// promises as admitted and its seats, with the quota in force as the
/// admitted quota.
fn admitted_request(work: &Value) -> ConvoyRequest {
    let spec = &work["spec"];
    let promises: Vec<Value> = pairs(&work["promises"])
        .iter()
        .map(|p| {
            assert!(list(&p[1]["submissions"]).is_empty(), "an initial convoy's promises have no submissions");
            json!([p[0], { "promise": p[1]["promise"], "source": p[1]["source"], "owner": p[1]["owner"] }])
        })
        .collect();
    let request = json!({
        "project": spec["project"],
        "promises": { "#map": promises },
        "duties": spec["duties"],
        "seats": work["seats"],
        "quota": work["quota"],
        "priority": spec["priority"],
    });
    request_of(&request)
}

/// The world admission reads: the request's project exists when the spec's
/// `projectKnown` says so.
fn projects(request: &ConvoyRequest, known: bool) -> MemoryConvoyWorld {
    let world = MemoryConvoyWorld::default().with_limits(4, 1);
    if known {
        world.with_project(&request.project)
    } else {
        world
    }
}

/// The convoy under test: a create names it as a launch of [`LAUNCH_OF`].
fn convoy(home: &Home<Convoy>) -> &Resource<Convoy> {
    home.newest_launch(LAUNCH_OF, None).expect("exists")
}

struct ConvoyConformance;

impl Conformance for ConvoyConformance {
    const MACHINE: &'static str = "convoy";
    // Quota counts are unsigned, so an invalid quota is unrepresentable.
    const NEVER_REFUSED: &'static [&'static str] = &["InvalidQuota"];
    type Subject = Home<Convoy>;

    fn create(state: &Value) -> Home<Convoy> {
        let mut home = Home::<Convoy>::new("home").expect("valid declaration");
        let request = admitted_request(&state["work"]);
        // An admitted convoy's project was known.
        home.create(LAUNCH_OF, request.clone(), &projects(&request, true), Timestamp(0)).expect("admitted");
        home
    }

    fn apply(home: &mut Home<Convoy>, event: &Value) -> Step {
        let o = payload(event);
        let name = convoy(home).name().to_owned();
        let mut world = MemoryConvoyWorld::default().with_limits(4, 1);
        let authority = Ref::new(DECIDER);
        let by = SeatName::new(DECIDER, "");
        let at = tick(1);
        let promise = || PromiseId(id(&o["promise"]));
        // The authorised edges' authority reads the spec's `authorised`.
        if !o["authorised"].is_null() {
            world.authority.put(&authority, boolean(&o["authorised"]));
        }
        match tag(event) {
            "Declare" => {
                let definition = &o["definition"];
                let declare = Declare {
                    promise: promise(),
                    label: format!("p{}", promise().0),
                    kind: text(&definition["kind"]).into(),
                    target: address(&definition["target"]),
                    owner: seat(&o["owner"]),
                };
                outcome(home.apply(&name, declare, &world, at))
            }
            "Submit" => {
                let reference = address(&o["reference"]);
                let durable = Ref::new(reference.as_str());
                world.durable.put(&durable, boolean(&o["durable"]));
                let submit = Submit { promise: promise(), by: seat(&o["by"]), address: reference, revision: REVISION.into(), durable };
                outcome(home.apply(&name, submit, &world, at))
            }
            "Reject" => outcome(home.apply(&name, Reject { promise: promise() }, &world, at)),
            "Keep" => {
                let k = payload(o);
                let keep = match tag(o) {
                    "Observed" => Keep::Observed { promise: PromiseId(id(&k["promise"])), submission: id(&k["submission"]) },
                    _ => Keep::Verdict {
                        promise: PromiseId(id(&k["promise"])),
                        by: seat(&k["by"]),
                        authority: authority_of(&k["authority"]),
                        reason: text(&k["reason"]).into(),
                    },
                };
                outcome(home.apply(&name, keep, &world, at))
            }
            "Retract" => {
                let retract = Retract {
                    promise: promise(),
                    by: seat(&o["by"]),
                    authority: authority_of(&o["authority"]),
                    reason: text(&o["reason"]).into(),
                };
                outcome(home.apply(&name, retract, &world, at))
            }
            "Reassign" => {
                let reassign =
                    Reassign { promise: promise(), to: seat(&o["to"]), by: seat(&o["by"]), authority: authority_of(&o["authority"]) };
                outcome(home.apply(&name, reassign, &world, at))
            }
            "RetireDuty" => outcome(home.apply(&name, RetireDuty { duty: DutyId(id(&o["duty"])), by, authority }, &world, at)),
            "AddSeat" => {
                let add = AddSeat {
                    seat: seat(&o["seat"]),
                    parent: seat(&o["parent"]),
                    supervised_by: address(&o["supervisedBy"]),
                    by,
                    authority,
                };
                outcome(home.apply(&name, add, &world, at))
            }
            "Resupervise" => {
                let change = Resupervise { seat: seat(&o["seat"]), supervised_by: address(&o["supervisedBy"]), by, authority };
                outcome(home.apply(&name, change, &world, at))
            }
            "RetireSeat" => outcome(home.apply(&name, RetireSeat { seat: seat(&o["seat"]), by, authority }, &world, at)),
            "FailSeat" => {
                let fail = FailSeat { seat: seat(&o["seat"]), reason: text(&o["reason"]).into(), by, authority };
                outcome(home.apply(&name, fail, &world, at))
            }
            "ReplaceSeat" => outcome(home.apply(&name, ReplaceSeat(seat(o)), &world, at)),
            "Link" => {
                let link = Link { subject: address(&o["subject"]), source: link_source_of(&o["source"]), by, authority };
                outcome(home.apply(&name, link, &world, at))
            }
            "Unlink" => outcome(home.apply(&name, Unlink { subject: address(&o["subject"]), by, authority }, &world, at)),
            "Deliver" => outcome(home.apply(&name, Deliver { disposition: text(&o["disposition"]).into() }, &world, at)),
            "Abandon" => {
                let abandon = Abandon { by, cause: cause_of(&o["cause"]), reason: text(&o["reason"]).into(), authority };
                outcome(home.apply(&name, abandon, &world, at))
            }
            "Readmit" => outcome(home.apply(&name, Readmit { quota: quota_of(&o["quota"]), by, authority }, &world, at)),
            other => panic!("convoy event {other}"),
        }
    }

    fn encode(home: &Home<Convoy>) -> Value {
        let c = convoy(home);
        let phase = match c.state() {
            ConvoyState::Open { .. } => bare("Open"),
            ConvoyState::Delivered { disposition, .. } => variant("Delivered", json!(disposition)),
            ConvoyState::Abandoned { cause, reason, .. } => {
                variant("Abandoned", json!({ "cause": bare(cause_name(*cause)), "reason": reason }))
            }
            ConvoyState::Retired => bare("Retired"),
        };
        json!({ "work": work(c.spec(), c.state()), "phase": phase })
    }

    fn version(home: &Home<Convoy>) -> u64 {
        convoy(home).version().0
    }
}

/// Every admission probe: refused with the same variant, or the same Open work.
fn check_admissions(nodes: &[Value], report: &mut Report) {
    for node in nodes.iter().filter(|node| tag(&node["probe"]) == "Admitted") {
        let probe = payload(&node["probe"]);
        let (request, result) = (&probe["request"], &probe["result"]);
        let mut home = Home::<Convoy>::new("home").expect("valid declaration");
        let request_value = request;
        let request = request_of(request_value);
        let world = projects(&request, boolean(&request_value["projectKnown"]));
        let rust = match home.create(LAUNCH_OF, request.clone(), &world, Timestamp(0)) {
            Ok(_) => variant("Moved", ConvoyConformance::encode(&home)),
            Err(WriteError::Refused(r)) => variant("Refused", refusal_of(r)),
            Err(other) => panic!("admission of {}: {other:?}", show(request_value)),
        };
        report.pairs += 1;
        if normalise(&rust) != normalise(result) {
            report.disagreements.push(format!("admit {}: spec {}, rust {}", show(request_value), show(result), show(&rust)));
        }
    }
}

// Admission refuses a request with no obligation, an unknown project or an
// owner that is no seat, and opens any other with every promise Pending;
// every reachable convoy state is reached in Rust by the spec's path; and
// every probe agrees, including the promises and duties a refused delivery
// names.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn convoy_follows_the_full_graph() {
    let nodes = graph::load(ConvoyConformance::MACHINE);
    let mut report = Report::new("convoy");
    check_admissions(&nodes, &mut report);
    graph::check::<ConvoyConformance>(&nodes, &mut report);
    report.finish();
}
