//! The shared lifecycle against `conformance/lifecycle.qnt`.
//!
//! Evidence mapping, per spec event, onto a fresh [`MemoryLifecycleWorld`]:
//! - `Realise{present, matches, identity}`: a realisation is observed iff
//!   `present`, under the edge's identity iff `matches`.
//! - `Release{teardownComplete}`: the teardown reads `teardownComplete`.
//! - `Vanish{absent}`: the absence reads `absent`.
//! - `Attempted(outcome)`: an [`Attempted`] edge with that outcome.
//! - `GiveUp{cause}`: a [`GiveUp`] edge with that cause; the probe kind's
//!   policy gives up after the spec's `MAX_FAILURES` (2) failures.
//!
//! State mapping: `Requested(history)` is `Requested { .. }` with `history`
//! the resource's `Attempted` log entries in order. The state keeps only the
//! tally of that history (D2: a decision reads no log), so encoding also
//! checks the tally against the history's failures since its last success
//! and its last outcome. `Gone` is one to one, `Realised { identity }` is
//! `Realised(identity)`, and `GaveUp { cause }` is `GaveUp(cause.detail)`.
//! The spec's cause is a string, while a Rust [`Cause`] also has a category,
//! shared across kinds. The category is not part of the spec's rule, so the
//! adaptor records every cause as [`CauseCategory::Transient`] and encodes
//! the detail only.

use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::lifecycle::{
        Attempted, GiveUp, Lifecycle, LifecycleEdge, LifecycleKind, LifecycleRefusal, LifecycleState, MemoryLifecycleWorld, Observed,
        Realise, Release, Vanish,
    },
    AttemptPolicy, Cause, CauseCategory, Home, Outcome, Ref, Version, WriteError,
};
use serde_json::{json, Value};

use crate::{
    graph::{self, refusal, tick, Conformance, Report, Step},
    itf::{boolean, payload, tag, text, unit, variant},
};

/// A lifecycle kind whose requests give up after the spec's `MAX_FAILURES`.
struct Probe;
impl LifecycleKind for Probe {
    const NAME: &'static str = "Probe";
    type Request = ();
    fn policy() -> AttemptPolicy {
        AttemptPolicy::immediate(2)
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

/// The spec's `AttemptOutcome`.
fn outcome_value(outcome: &Outcome) -> Value {
    match outcome {
        Outcome::Succeeded => variant("Succeeded", unit()),
        Outcome::Failed { cause, retryable } => variant("AttemptFailed", json!({ "retryable": retryable, "cause": cause.detail })),
    }
}

/// A lifecycle state value in the spec's shape, given the resource's
/// attempts in order; the vessel adaptor shares it with none. Checks the
/// Requested tally against those attempts.
pub fn encode_state(state: &LifecycleState, attempts: &[&Outcome]) -> Value {
    match state {
        LifecycleState::Requested { failures, last_failure } => {
            let since_success = attempts.iter().rev().take_while(|o| matches!(o, Outcome::Failed { .. })).count();
            assert_eq!(*failures as usize, since_success, "the tally counts the failures since the last success");
            let last = attempts.last().and_then(|o| match o {
                Outcome::Failed { cause, retryable } => Some((cause, *retryable)),
                Outcome::Succeeded => None,
            });
            assert_eq!(last_failure.as_ref().map(|f| (&f.cause, f.retryable)), last, "the tally keeps the last outcome's failure");
            variant("Requested", Value::Array(attempts.iter().map(|o| outcome_value(o)).collect()))
        }
        LifecycleState::Realised { identity } => variant("Realised", Value::from(identity.as_str())),
        LifecycleState::Gone { ending } => variant("Gone", flotilla_quint_testkit::lifecycle::ending_value(ending)),
        LifecycleState::GaveUp { cause } => variant("GaveUp", Value::from(cause.detail.as_str())),
    }
}

struct LifecycleConformance;

impl Conformance for LifecycleConformance {
    const MACHINE: &'static str = "lifecycle";
    type Subject = Home<P>;

    fn create(_: &Value) -> Home<P> {
        let mut home = Home::<P>::new("home").expect("valid declaration");
        home.create(NAME, (), &MemoryLifecycleWorld::default(), Timestamp(0)).expect("created");
        home
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
                outcome(home.apply(
                    NAME,
                    Vanish { cause: flotilla_quint_testkit::lifecycle::vanish_cause(&o["cause"]), observed: Ref::new(NAME) },
                    &world,
                    at,
                ))
            }
            "Attempted" => {
                let attempted = match tag(o) {
                    "Succeeded" => Outcome::Succeeded,
                    _ => {
                        let failed = payload(o);
                        Outcome::Failed { cause: cause(text(&failed["cause"])), retryable: boolean(&failed["retryable"]) }
                    }
                };
                outcome(home.apply(NAME, Attempted { outcome: attempted }, &world, at))
            }
            "GiveUp" => outcome(home.apply(NAME, GiveUp { cause: cause(text(&o["cause"])) }, &world, at)),
            other => panic!("lifecycle event {other}"),
        }
    }

    fn encode(home: &Home<P>) -> Value {
        let resource = home.get(NAME).expect("exists");
        let attempts: Vec<&Outcome> = home
            .log()
            .history(NAME, Version(0), usize::MAX)
            .into_iter()
            .filter_map(|(_, event)| match &event.payload.edge {
                LifecycleEdge::Attempted(attempted) => Some(&attempted.outcome),
                _ => None,
            })
            .collect();
        encode_state(resource.state(), &attempts)
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
