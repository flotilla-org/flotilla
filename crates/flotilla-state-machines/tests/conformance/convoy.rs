//! Convoy against `conformance/convoy.qnt`.
//!
//! Admission: each `Admitted({request, result})` probe is a [`Home::create`]
//! of the [`ConvoyRequest`], refused `NoPromises` or reaching the same `Open`
//! work.
//!
//! Event mapping, each with a fresh [`MemoryConvoyWorld`]:
//! - `Keep(p)`: the `keep` edge; no world.
//! - `AcceptRetraction{promise, authorised}`: the `retract` edge, its
//!   authority reading `authorised`.
//! - `Deliver`: the `deliver` edge.
//! - `Abandon{authorised, reason}` and `Readmit{quota, authorised}`: those
//!   edges, their authority reading `authorised`.
//!
//! State mapping, one to one: the spec's `Work = {spec, promises, quota}` is
//! `{ spec: Resource::spec(), promises: state.resolution(), quota: state.quota() }`,
//! so `Open { promises, quota }` is `Open(work)`, `Delivered { .. }` is
//! `Delivered(work)` and `Abandoned { reason, .. }` is `Abandoned({work, reason})`.
//! The immutable [`ConvoySpec`] (project, promises, priority) is the spec's
//! `ConvoySpec` field for field.

use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::convoy::{
        Abandon, Convoy, ConvoyRefusal, ConvoyRequest, ConvoySpec, ConvoyState, Deliver, Keep, MemoryConvoyWorld, Priority, Promise,
        PromiseId, PromiseState, Quota, Readmit, Retract, VesselClass,
    },
    Home, Ref, WriteError,
};
use serde_json::{json, Value};

use crate::{
    graph::{self, bare, refusal, tick, Conformance, Report, Step},
    itf::{address, boolean, int, map_of, normalise, number, pairs, payload, set_of, show, tag, text, variant},
};

const NAME: &str = "c";
const OPERATOR: &str = "operator";

fn id(v: &Value) -> u32 {
    u32::try_from(number(v)).expect("model id")
}

fn quota_of(v: &Value) -> Quota {
    pairs(v).iter().map(|p| (VesselClass(id(&p[0])), id(&p[1]))).collect()
}

fn request_of(request: &Value) -> ConvoyRequest {
    let promises = pairs(&request["promises"])
        .iter()
        .map(|p| (PromiseId(id(&p[0])), Promise { kind: text(&p[1]["kind"]).into(), target: address(&p[1]["target"]) }))
        .collect();
    ConvoyRequest {
        project: address(&request["project"]),
        promises,
        quota: quota_of(&request["quota"]),
        priority: Priority(id(&request["priority"])),
    }
}

fn quota_value(quota: &Quota) -> Value {
    map_of(quota.iter().map(|(class, count)| (int(class.0.into()), int((*count).into()))))
}

fn spec_value(spec: &ConvoySpec) -> Value {
    let promises =
        spec.promises.iter().map(|(p, promise)| (int(p.0.into()), json!({ "kind": promise.kind, "target": promise.target.name() })));
    json!({ "project": spec.project.name(), "promises": map_of(promises), "priority": int(spec.priority.0.into()) })
}

fn refusal_of(refusal: ConvoyRefusal) -> Value {
    let name = match &refusal {
        ConvoyRefusal::PromisesOutstanding(ids) => {
            return variant("PromisesOutstanding", set_of(ids.iter().map(|p| int(p.0.into()))));
        }
        ConvoyRefusal::NoPromises => "NoPromises",
        ConvoyRefusal::UnknownProject => "UnknownProject",
        ConvoyRefusal::InvalidQuota => "InvalidQuota",
        ConvoyRefusal::NotOpen => "NotOpen",
        ConvoyRefusal::UnknownPromise => "UnknownPromise",
        ConvoyRefusal::PromiseSettled => "PromiseSettled",
        ConvoyRefusal::Unauthorised => "Unauthorised",
        ConvoyRefusal::QuotaNotIncrease => "QuotaNotIncrease",
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

/// The spec's `Work`: the immutable spec, and the state's resolution and quota in force.
fn work(spec: &ConvoySpec, state: &ConvoyState) -> Value {
    let resolved = state.resolution().iter().map(|(p, resolution)| {
        let name = match resolution {
            PromiseState::Pending => "Pending",
            PromiseState::Kept => "Kept",
            PromiseState::Retracted => "Retracted",
        };
        (int(p.0.into()), bare(name))
    });
    json!({ "spec": spec_value(spec), "promises": map_of(resolved), "quota": quota_value(state.quota()) })
}

/// The request that admitted an initial `Open(work)`: its spec, with the
/// quota in force as the admitted quota.
fn admitted_request(work: &Value) -> ConvoyRequest {
    let spec = &work["spec"];
    let request = json!({ "project": spec["project"], "promises": spec["promises"], "quota": work["quota"], "priority": spec["priority"] });
    request_of(&request)
}

/// The world admission reads: the request's project exists when the spec's
/// `projectKnown` says so.
fn projects(request: &ConvoyRequest, known: bool) -> MemoryConvoyWorld {
    let world = MemoryConvoyWorld::default();
    if known {
        world.with_project(&request.project)
    } else {
        world
    }
}

struct ConvoyConformance;

impl Conformance for ConvoyConformance {
    const MACHINE: &'static str = "convoy";
    // Quota counts are unsigned, so an invalid quota is unrepresentable.
    const NEVER_REFUSED: &'static [&'static str] = &["InvalidQuota"];
    type Subject = Home<Convoy>;

    fn create(state: &Value) -> Home<Convoy> {
        let mut home = Home::<Convoy>::new("home").expect("valid declaration");
        let request = admitted_request(payload(state));
        // An admitted convoy's project was known.
        home.create(NAME, request.clone(), &projects(&request, true), Timestamp(0)).expect("admitted");
        home
    }

    fn apply(home: &mut Home<Convoy>, event: &Value) -> Step {
        let o = payload(event);
        let mut world = MemoryConvoyWorld::default();
        let authority = Ref::new(OPERATOR);
        let at = tick(1);
        match tag(event) {
            "Keep" => outcome(home.apply(NAME, Keep { promise: PromiseId(id(o)) }, &world, at)),
            "AcceptRetraction" => {
                world.authority.put(&authority, boolean(&o["authorised"]));
                outcome(home.apply(NAME, Retract { promise: PromiseId(id(&o["promise"])), by: OPERATOR.into(), authority }, &world, at))
            }
            "Deliver" => outcome(home.apply(NAME, Deliver, &world, at)),
            "Abandon" => {
                world.authority.put(&authority, boolean(&o["authorised"]));
                outcome(home.apply(NAME, Abandon { by: OPERATOR.into(), reason: text(&o["reason"]).into(), authority }, &world, at))
            }
            "Readmit" => {
                world.authority.put(&authority, boolean(&o["authorised"]));
                outcome(home.apply(NAME, Readmit { quota: quota_of(&o["quota"]), by: OPERATOR.into(), authority }, &world, at))
            }
            other => panic!("convoy event {other}"),
        }
    }

    fn encode(home: &Home<Convoy>) -> Value {
        let c = home.get(NAME).expect("exists");
        let work = work(c.spec(), c.state());
        match c.state() {
            ConvoyState::Open { .. } => variant("Open", work),
            ConvoyState::Delivered { .. } => variant("Delivered", work),
            ConvoyState::Abandoned { reason, .. } => variant("Abandoned", json!({ "work": work, "reason": reason })),
        }
    }

    fn version(home: &Home<Convoy>) -> u64 {
        home.get(NAME).expect("exists").version().0
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
        let rust = match home.create(NAME, request.clone(), &world, Timestamp(0)) {
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

// Admission refuses a request without promises or naming an unknown project,
// and opens any other with every promise pending; every reachable convoy state is reached in Rust by the
// spec's path; and every probe agrees, including the outstanding promises a
// refused delivery names.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn convoy_follows_the_full_graph() {
    let nodes = graph::load(ConvoyConformance::MACHINE);
    let mut report = Report::new("convoy");
    check_admissions(&nodes, &mut report);
    graph::check::<ConvoyConformance>(&nodes, &mut report);
    report.finish();
}
