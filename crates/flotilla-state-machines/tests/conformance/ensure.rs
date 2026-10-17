//! Ensure against `conformance/ensure.qnt`.
//!
//! Fulfiller `i` is the reference `f<i>`; the Ensure's policy is exhausted
//! after two consecutive failures in the current window
//! ([`AttemptPolicy::immediate`]`(2)`), the spec's `MAX_FAILURES`.
//!
//! Event mapping, each with a fresh [`MemoryEnsureWorld`]. The harness
//! derives `Create` and `Bind` evidence from a world of fulfiller phases in
//! which unrecorded fulfillers were never created; each probe gets a
//! canonical world realising its booleans over the from-state's recorded set:
//! - `Record(i)`: the record edge.
//! - `Create{id, noLive, absent}`: nothing observed when both hold; `f<id>`
//!   Gone for `noLive and not absent`; another recorded fulfiller Requested
//!   for `not noLive and absent`; `f<id>` Requested when neither holds.
//! - `Attempted({id, outcome})`: the attempt edge naming `f<id>`, with
//!   `Succeeded`, or a failure (a fixed cause) retryable as the spec says.
//! - `Bind{id, realised, canReceive}`: `f<id>` Realised (else Requested) with
//!   `canReceive`; the inbox has drained.
//! - `Unbind{reason, observed, gone}`: `f<observed>` Gone (else Realised).
//! - `Hold`, `Resume{authorised}`, `Retire{authorised, teardownComplete}`:
//!   those edges, reading authority and teardown.
//!
//! State mapping: the spec's `Work = {recorded, created, history, window}` is
//! the state's `recorded` and `created`, with `history` the Ensure's
//! `Attempted` log entries in order and `window` the number of them before
//! the last `Resume` entry; `Fulfilled { bound, .. }` is
//! `Fulfilled({work, bound})`, and the other labels carry their work.
//! Fulfiller references `f<i>` encode as `i`. The remaining difference: the
//! state keeps the window's tally (`failures`, `last_failure`) instead of the
//! history, so encoding also checks that tally against the spec's
//! `failureCount` and the window's last outcome.

use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::ensure::{
        Attempted, Bind, Create, Drain, Ensure, EnsureEdge, EnsureRefusal, EnsureSpec, EnsureState, Fulfiller, FulfillerPhase,
        FulfillerStatus, Fulfillers, Hold, MemoryEnsureWorld, Record, Resume, Retire, Teardown, Unbind, UnbindReason,
    },
    AttemptPolicy, Cause, CauseCategory, Home, Outcome, Ref, Resource, WriteError,
};
use serde_json::{json, Value};

use crate::{
    graph::{self, refusal, tick, Conformance, Report, Step},
    itf::{boolean, int, number, payload, set_of, tag, unit, variant},
};

const NAME: &str = "e";
const INBOX: &str = "inbox";
const OPERATOR: &str = "operator";

fn f(id: i64) -> Ref<Fulfiller> {
    Ref::new(format!("f{id}"))
}

/// Fulfiller `f<i>`'s spec id `i`.
fn id_of(name: &str) -> i64 {
    name.strip_prefix('f').and_then(|id| id.parse().ok()).unwrap_or_else(|| panic!("conformance fulfiller name {name}"))
}

fn ids(fulfillers: &Fulfillers) -> Value {
    set_of(fulfillers.iter().map(|fulfiller| int(id_of(fulfiller.name()))))
}

fn status(phase: FulfillerPhase, can_receive: bool) -> FulfillerStatus {
    FulfillerStatus { phase, can_receive }
}

fn outcome<T>(result: Result<T, WriteError<Ensure>>) -> Step {
    let name = match result {
        Ok(_) => return Step::Accepted,
        Err(WriteError::Refused(r)) => match r {
            EnsureRefusal::Ended(_) => "Ended",
            EnsureRefusal::WrongState => "WrongState",
            EnsureRefusal::NotRecorded => "NotRecorded",
            EnsureRefusal::FulfillerLive => "FulfillerLive",
            EnsureRefusal::MissingEvidence => "MissingEvidence",
            EnsureRefusal::NotExhausted => "NotExhausted",
            EnsureRefusal::Unauthorised => "Unauthorised",
        },
        Err(other) => return refusal(&format!("{other:?}")),
    };
    refusal(name)
}

/// The spec's `AttemptOutcome`.
fn outcome_value(outcome: &Outcome) -> Value {
    match outcome {
        Outcome::Succeeded => variant("Succeeded", unit()),
        Outcome::Failed { retryable, .. } => variant("Failed", json!({ "retryable": retryable })),
    }
}

/// The spec's `Work`: the state's fulfillers, and the history and window
/// from the Ensure's log. Checks the state's tally against the window.
fn work(e: &Resource<Ensure>) -> Value {
    let mut history = Vec::new();
    let mut window = 0;
    for row in e.events().iter() {
        match &row.record.payload.edge {
            EnsureEdge::Attempted(attempted) => history.push(attempted),
            EnsureEdge::Resume(_) => window = history.len(),
            _ => {}
        }
    }
    let state = e.state();
    let current = &history[window..];
    let since_success = current.iter().rev().take_while(|a| matches!(a.outcome, Outcome::Failed { .. })).count();
    assert_eq!(state.failures() as usize, since_success, "the tally counts the window's failures since its last success");
    let last_retryable = current.last().and_then(|a| match a.outcome {
        Outcome::Failed { retryable, .. } => Some(retryable),
        Outcome::Succeeded => None,
    });
    assert_eq!(state.last_failure().map(|f| f.retryable), last_retryable, "the tally keeps the window's last failure");
    let history: Vec<Value> =
        history.iter().map(|a| json!({ "id": int(id_of(a.fulfiller.name())), "outcome": outcome_value(&a.outcome) })).collect();
    json!({ "recorded": ids(state.recorded()), "created": ids(state.created()), "history": history, "window": int(window as i64) })
}

struct EnsureConformance;

impl Conformance for EnsureConformance {
    const MACHINE: &'static str = "ensure";
    type Subject = Home<Ensure>;

    fn create(_: &Value) -> Home<Ensure> {
        let mut home = Home::new("home").expect("valid declaration");
        home.create(NAME, EnsureSpec { inbox: INBOX.into(), policy: AttemptPolicy::immediate(2) }, Timestamp(0)).expect("created");
        home
    }

    fn apply(home: &mut Home<Ensure>, event: &Value) -> Step {
        let o = payload(event);
        let mut world = MemoryEnsureWorld::default();
        let authority = Ref::new(OPERATOR);
        let at = tick(1);
        match tag(event) {
            "Record" => outcome(home.apply(NAME, Record { fulfiller: f(number(o)) }, &world, at)),
            "Create" => {
                let id = number(&o["id"]);
                match (boolean(&o["noLive"]), boolean(&o["absent"])) {
                    (true, true) => {}
                    (true, false) => {
                        world.fulfillers.put(&f(id), status(FulfillerPhase::Gone, false));
                    }
                    (false, true) => {
                        let recorded = home.get(NAME).expect("exists").state().recorded();
                        let Some(other) = recorded.iter().find(|fulfiller| **fulfiller != f(id)).cloned() else {
                            return Step::Unrealisable("a live fulfiller other than an absent one, with none other recorded");
                        };
                        world.fulfillers.put(&other, status(FulfillerPhase::Requested, false));
                    }
                    (false, false) => {
                        world.fulfillers.put(&f(id), status(FulfillerPhase::Requested, false));
                    }
                }
                outcome(home.apply(NAME, Create { fulfiller: f(id) }, &world, at))
            }
            "Attempted" => {
                let attempt = match tag(&o["outcome"]) {
                    "Succeeded" => Outcome::Succeeded,
                    "Failed" => Outcome::Failed {
                        cause: Cause::new(CauseCategory::Transient, "attempt failed"),
                        retryable: boolean(&payload(&o["outcome"])["retryable"]),
                    },
                    other => panic!("attempt outcome {other}"),
                };
                outcome(home.apply(NAME, Attempted { fulfiller: f(number(&o["id"])), outcome: attempt }, &world, at))
            }
            "Bind" => {
                let id = number(&o["id"]);
                let phase = if boolean(&o["realised"]) { FulfillerPhase::Realised } else { FulfillerPhase::Requested };
                world.fulfillers.put(&f(id), status(phase, boolean(&o["canReceive"])));
                world.drains.put(&Ref::<Drain>::new(INBOX), true);
                outcome(home.apply(NAME, Bind { fulfiller: f(id) }, &world, at))
            }
            "Unbind" => {
                let id = number(&o["observed"]);
                let phase = if boolean(&o["gone"]) { FulfillerPhase::Gone } else { FulfillerPhase::Realised };
                world.fulfillers.put(&f(id), status(phase, true));
                let reason = match tag(&o["reason"]) {
                    "Gone" => UnbindReason::Gone,
                    "Replaced" => UnbindReason::Replaced,
                    other => panic!("unbind reason {other}"),
                };
                outcome(home.apply(NAME, Unbind { reason, fulfiller: f(id) }, &world, at))
            }
            "Hold" => outcome(home.apply(NAME, Hold, &world, at)),
            "Resume" => {
                world.authorities.put(&authority, boolean(&o["authorised"]));
                outcome(home.apply(NAME, Resume { by: OPERATOR.into(), authority }, &world, at))
            }
            "Retire" => {
                world.authorities.put(&authority, boolean(&o["authorised"]));
                world.teardowns.put(&Ref::<Teardown>::new(NAME), boolean(&o["teardownComplete"]));
                outcome(home.apply(NAME, Retire { by: OPERATOR.into(), authority, teardown: Ref::new(NAME) }, &world, at))
            }
            other => panic!("ensure event {other}"),
        }
    }

    fn encode(home: &Home<Ensure>) -> Value {
        let e = home.get(NAME).expect("exists");
        match e.state() {
            EnsureState::Unfulfilled { .. } => variant("Unfulfilled", work(e)),
            EnsureState::Fulfilled { bound, .. } => variant("Fulfilled", json!({ "work": work(e), "bound": int(id_of(bound.name())) })),
            EnsureState::Held { .. } => variant("Held", work(e)),
            EnsureState::Retired { .. } => variant("Retired", work(e)),
        }
    }

    fn version(home: &Home<Ensure>) -> u64 {
        home.get(NAME).expect("exists").version().0
    }
}

// Every reachable Ensure state is reached in Rust by the spec's path, and
// every probe agrees: the same move, or the same refusal with nothing
// recorded.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn ensure_follows_the_full_graph() {
    let mut report = Report::new("ensure");
    graph::check::<EnsureConformance>(&graph::load(EnsureConformance::MACHINE), &mut report);
    report.finish();
}
