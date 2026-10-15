//! Ensure against `conformance/ensure.qnt`.
//!
//! Fulfiller `i` is the reference `f<i>`; the Ensure's policy gives up after
//! one failure in the current window ([`AttemptPolicy::immediate`]`(1)`), the
//! spec's `history.length() - window >= 1`.
//!
//! Event mapping, each with a fresh [`MemoryEnsureWorld`]. The harness
//! derives `Create` and `Bind` evidence from a world of fulfiller phases in
//! which unrecorded fulfillers were never created; each probe gets a
//! canonical world realising its booleans over the from-state's recorded set:
//! - `Record(i)`: the record edge.
//! - `Create{id, noLive, absent}`: nothing observed when both hold; `f<id>`
//!   Gone for `noLive and not absent`; another recorded fulfiller Requested
//!   for `not noLive and absent`; `f<id>` Requested when neither holds.
//! - `Fail({id, limit})`: a failed attempt record whose cause detail names
//!   fulfiller `f<id>`. The spec refuses it
//!   `HistoryFull` once the history reaches `limit`: that is the model's bound
//!   on its history, which Rust (an unbounded attempt log) does not have, so
//!   those probes are counted and skipped.
//! - `Bind{id, realised, canReceive}`: `f<id>` Realised (else Requested) with
//!   `canReceive`; the inbox has drained.
//! - `Unbind{reason, observed, gone}`: `f<observed>` Gone (else Realised).
//! - `GiveUp`, `Resume{authorised}`, `Retire{authorised, teardownComplete}`:
//!   those edges, reading authority and teardown.
//!
//! State mapping: the spec's `Work = {recorded, created, history, window}` is
//! the state's `recorded`, `created` and `window` (the attempts before the
//! current window) with `history` read from the Ensure's attempt records, one
//! entry per attempt; `Fulfilled { bound, .. }` is `Fulfilled({work, bound})`,
//! and the other labels carry their work. Fulfiller references `f<i>` encode
//! as `i`. The one remaining difference: the spec's history lists the
//! fulfiller each failure named, while a Rust attempt has a time and an
//! outcome, so the adaptor writes the fulfiller's name as the failure's cause
//! detail and encodes the history from it.

use flotilla_state_machines::{
    machines::ensure::{
        Bind, Create, Drain, Ensure, EnsureRefusal, EnsureSpec, EnsureState, Fulfiller, FulfillerPhase, FulfillerStatus, Fulfillers,
        GiveUp, MemoryEnsureWorld, Record, Resume, Retire, Teardown, Unbind, UnbindReason,
    },
    Attempt, AttemptPolicy, Cause, CauseCategory, Home, Outcome, Ref, Resource, WriteError,
};
use serde_json::{json, Value};

use crate::{
    graph::{self, refusal, tick, Conformance, Report, Step},
    itf::{boolean, int, number, payload, set_of, tag, variant},
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
            EnsureRefusal::HistoryFull => "HistoryFull",
        },
        Err(other) => return refusal(&format!("{other:?}")),
    };
    refusal(name)
}

/// The spec's `Work`: the state's data, and the history from the attempt
/// records, each failure naming its fulfiller in its cause detail.
fn work(e: &Resource<Ensure>) -> Value {
    let history: Vec<Value> = e
        .attempts()
        .iter()
        .map(|row| match &row.record.outcome {
            Outcome::Failed { cause, .. } => int(id_of(&cause.detail)),
            Outcome::Succeeded => panic!("the harness records failures only"),
        })
        .collect();
    let state = e.state();
    let window = i64::try_from(state.window()).expect("model window");
    json!({ "recorded": ids(state.recorded()), "created": ids(state.created()), "history": history, "window": int(window) })
}

/// Why the probes the spec refuses at its history bound are skipped.
const HISTORY_FULL: &str = "the spec's HistoryFull model bound; Rust attempts are an unbounded log";

struct EnsureConformance;

impl Conformance for EnsureConformance {
    const MACHINE: &'static str = "ensure";
    // The spec's model bound on recorded failures; the attempt log is unbounded.
    const NEVER_REFUSED: &'static [&'static str] = &["HistoryFull"];
    const UNREALISABLE: &'static [(&'static str, usize)] = &[(HISTORY_FULL, 27)];
    type Subject = Home<Ensure>;

    fn create(_: &Value) -> Home<Ensure> {
        let mut home = Home::new("home").expect("valid declaration");
        home.create(NAME, EnsureSpec { inbox: INBOX.into(), policy: AttemptPolicy::immediate(1) }).expect("created");
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
            "Fail" => {
                let e = home.get(NAME).expect("exists");
                let limit = usize::try_from(number(&o["limit"])).expect("model limit");
                if matches!(e.state(), EnsureState::Unfulfilled { .. }) && e.attempts().len() >= limit {
                    return Step::Unrealisable(HISTORY_FULL);
                }
                let named = f(number(&o["id"]));
                let failed = Attempt::failed(at, Cause::new(CauseCategory::Transient, named.name()), true);
                outcome(home.record_attempt(NAME, failed))
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
            "GiveUp" => outcome(home.apply(NAME, GiveUp, &world, at)),
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
// recorded. Failures past the spec's model bound on its history are counted
// and skipped.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn ensure_follows_the_full_graph() {
    let mut report = Report::new("ensure");
    graph::check::<EnsureConformance>(&graph::load(EnsureConformance::MACHINE), &mut report);
    report.finish();
}
