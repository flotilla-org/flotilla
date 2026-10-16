//! The shared lifecycle against `conformance/lifecycle.qnt`.
//!
//! Evidence mapping, per spec event, onto a fresh [`MemoryLifecycleWorld`]:
//! - `Realise{present, matches, identity}`: a realisation is observed iff
//!   `present`, under the edge's identity iff `matches`.
//! - `Release{teardownComplete}`: the teardown reads `teardownComplete`.
//! - `Vanish{absent}`: the absence reads `absent`.
//! - `GiveUp{exhausted, lastCauseMatches, cause}`: when `exhausted`, one failed
//!   attempt is recorded first (the probe kind gives up after one failure),
//!   with the edge's cause iff `lastCauseMatches`; otherwise no attempt.
//!
//! State mapping: `Requested` and `Gone` one to one, `Realised { identity }`
//! is `Realised(identity)`, and `GaveUp { cause }` is `GaveUp(cause.detail)`.
//! The one remaining difference: the spec's cause is a string, while a Rust
//! [`Cause`] also has a category, shared across kinds. The category is not
//! part of the spec's rule, so the adaptor records every cause as
//! [`CauseCategory::Transient`] and encodes the detail only.

use flotilla_state_machines::{
    machines::lifecycle::{
        GiveUp, Lifecycle, LifecycleKind, LifecycleRefusal, LifecycleState, MemoryLifecycleWorld, Observed, Realise, Release, Vanish,
    },
    Attempt, AttemptPolicy, Cause, CauseCategory, Home, Ref, WriteError,
};
use serde_json::Value;

use crate::{
    graph::{self, bare, refusal, tick, Conformance, Report, Step},
    itf::{boolean, payload, tag, text, variant},
};

/// A lifecycle kind whose requests give up after one failure, so the spec's
/// `exhausted` is one recorded failed attempt.
struct Probe;
impl LifecycleKind for Probe {
    const NAME: &'static str = "Probe";
    type Request = ();
    fn policy() -> AttemptPolicy {
        AttemptPolicy::immediate(1)
    }
}

type P = Lifecycle<Probe>;

const NAME: &str = "r";

fn cause(detail: &str) -> Cause {
    Cause::new(CauseCategory::Transient, detail)
}

fn outcome<T>(result: Result<T, WriteError<P>>) -> Step {
    match result {
        Ok(_) => Step::Accepted,
        Err(WriteError::Refused(LifecycleRefusal::Ended(_))) => refusal("Ended"),
        Err(WriteError::Refused(LifecycleRefusal::WrongState)) => refusal("WrongState"),
        Err(WriteError::Refused(LifecycleRefusal::MissingEvidence)) => refusal("MissingEvidence"),
        Err(other) => refusal(&format!("{other:?}")),
    }
}

/// A lifecycle state value in the spec's shape; the vessel adaptor shares it.
pub fn encode_state(state: &LifecycleState) -> Value {
    match state {
        LifecycleState::Requested => bare("Requested"),
        LifecycleState::Realised { identity } => variant("Realised", Value::from(identity.as_str())),
        LifecycleState::Gone => bare("Gone"),
        LifecycleState::GaveUp { cause } => variant("GaveUp", Value::from(cause.detail.as_str())),
    }
}

struct LifecycleConformance;

impl Conformance for LifecycleConformance {
    const MACHINE: &'static str = "lifecycle";
    type Subject = Home<P>;

    fn create(_: &Value) -> Home<P> {
        let mut home = Home::new("home").expect("valid declaration");
        home.create(NAME, ()).expect("created");
        home
    }

    fn prepare(home: &mut Home<P>, event: &Value) {
        if tag(event) == "GiveUp" && boolean(&payload(event)["exhausted"]) {
            let o = payload(event);
            let detail = text(&o["cause"]);
            let last = if boolean(&o["lastCauseMatches"]) { detail.to_string() } else { format!("not-{detail}") };
            // Outside Requested this is refused like the edge; the edge's refusal is compared.
            let _ = home.record_attempt(NAME, Attempt::failed(tick(1), cause(&last), true));
        }
    }

    fn apply(home: &mut Home<P>, event: &Value) -> Step {
        let o = payload(event);
        let mut world = MemoryLifecycleWorld::default();
        let at = tick(1);
        match tag(event) {
            "Realise" => {
                let identity = text(&o["identity"]);
                if boolean(&o["present"]) {
                    let seen = if boolean(&o["matches"]) { identity.to_string() } else { format!("not-{identity}") };
                    world.realisations.put(&Ref::new(NAME), Observed { identity: seen });
                }
                outcome(home.apply(NAME, Realise { identity: identity.into(), observed: Ref::new(NAME) }, &world, at))
            }
            "Release" => {
                world.teardowns.put(&Ref::new(NAME), boolean(&o["teardownComplete"]));
                outcome(home.apply(NAME, Release { teardown: Ref::new(NAME) }, &world, at))
            }
            "Vanish" => {
                world.absences.put(&Ref::new(NAME), boolean(&o["absent"]));
                outcome(home.apply(NAME, Vanish { observed: Ref::new(NAME) }, &world, at))
            }
            "GiveUp" => outcome(home.apply(NAME, GiveUp { cause: cause(text(&o["cause"])) }, &world, at)),
            other => panic!("lifecycle event {other}"),
        }
    }

    fn encode(home: &Home<P>) -> Value {
        encode_state(home.get(NAME).expect("exists").state())
    }

    fn version(home: &Home<P>) -> u64 {
        home.get(NAME).expect("exists").version().0
    }
}

// Every reachable lifecycle state is reached in Rust by the spec's path, and
// every (state, event) probe moves to the same state value (the identity
// realised under, the cause given up with) or refuses with the same variant,
// recording nothing.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn lifecycle_follows_the_full_graph() {
    let mut report = Report::new("lifecycle");
    graph::check::<LifecycleConformance>(&graph::load(LifecycleConformance::MACHINE), &mut report);
    report.finish();
}
