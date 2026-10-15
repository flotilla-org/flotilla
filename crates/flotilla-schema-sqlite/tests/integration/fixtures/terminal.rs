//! The lifecycle-shaped fixture (TerminalSession), transcribed from the #3208 §4.3 sketch: the
//! shared lifecycle edge is generic over each kind's identity type, and TerminalSession supplies
//! `TerminalIdentity`.
//!
//! Modelling decisions the storage list forces:
//! - `LifecycleEdge<I>` carries the identity flattened into the edge row (`Realise { identity }`),
//!   so the same generic shape gives Vessel, Environment and the others their own columns.
//! - The realised identity on the resource is a flattened `Option<TerminalIdentity>` with a
//!   presence flag, rather than columns read from the latest edge.
//! - An agent source's brief is frozen inside a data enum variant: a hash column guarded by the
//!   variant's CHECK.
//! - Labels are a set: a child table whose key is the value itself.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Flat, Frozen, Records, Ref, State, Stored, Timestamp};
use hegel::{generators as gs, TestCase};
use serde::{Deserialize, Serialize};

use super::{
    attempt::{Attempt, Cause},
    code_enum,
    convoy::map,
    evolve_records, key, list, name, records, reference, stamp, text, timestamp, Fixture, Pick, SHAPES,
};

/// A minimal target kind for `TerminalSession::vessel`.
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Vessel", code = 7)]
pub struct Vessel {
    pub name: String,
}

/// A vessel's sessions in ordinal order (the #3208 §4.3 `(vessel_id, role, ordinal)` key, without
/// uniqueness), and a pool's live sessions: an interned `eq` column under a lifecycle predicate.
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(
    kind = "TerminalSession",
    code = 6,
    index(name = "by_vessel", eq(vessel), order(ordinal)),
    index(name = "live_by_pool", eq(pool), when(lifecycle in [Requested, Realised]))
)]
pub struct TerminalSession {
    pub vessel: Ref<Vessel>,
    #[store(intern)]
    pub role: String,
    pub ordinal: u16,
    #[store(intern)]
    pub pool: String,
    pub cwd: String,
    pub source: TerminalSource,
    #[store(child)]
    pub env: BTreeMap<String, String>,
    #[store(child)]
    pub labels: BTreeSet<String>,
    #[store(flatten)]
    pub realised: Option<TerminalIdentity>,
    #[store(history)]
    pub events: Records<LifecycleEvent<TerminalIdentity>>,
    #[store(history)]
    pub attempts: Records<Attempt<TerminalAction>>,
    #[store(state)]
    pub lifecycle: State<LifecycleState>,
}

#[derive(Debug, Clone, PartialEq, Stored)]
pub enum TerminalSource {
    #[store(code = 1)]
    Tool { command: String },
    #[store(code = 2)]
    Agent {
        #[store(intern)]
        selector: String,
        #[store(frozen)]
        brief: Frozen<Brief>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brief {
    pub text: String,
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub struct TerminalIdentity {
    pub session_id: String,
    pub pid: Option<i64>,
    pub endpoint: Option<String>,
}

/// One lifecycle edge event; generic over the identity a `Realise` edge carries.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub struct LifecycleEvent<I> {
    pub edge: LifecycleEdge<I>,
    pub at: Timestamp,
    #[store(intern)]
    pub writer: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub enum LifecycleEdge<I> {
    #[store(code = 1)]
    Realise {
        #[store(flatten)]
        identity: I,
    },
    #[store(code = 2)]
    Vanish,
    #[store(code = 3)]
    Release,
    #[store(code = 4)]
    GiveUp {
        #[store(flatten)]
        cause: Cause,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum LifecycleState {
    #[store(code = 1)]
    Requested,
    #[store(code = 2)]
    Realised,
    #[store(code = 3)]
    Gone,
    #[store(code = 4)]
    GaveUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum TerminalAction {
    #[store(code = 1)]
    Spawn,
    #[store(code = 2)]
    Attach,
    #[store(code = 3)]
    Kill,
}

impl TerminalIdentity {
    fn generate(tc: &TestCase, pick: Pick) -> TerminalIdentity {
        TerminalIdentity {
            session_id: text(tc),
            pid: pick.some(tc, 8).then(|| tc.draw(gs::integers::<i64>())),
            endpoint: pick.some(tc, 4).then(|| text(tc)),
        }
    }
}

impl<I: Flat> LifecycleEvent<I> {
    /// Every edge in turn when covering; `identity` draws the kind's own identity.
    pub fn generate(tc: &TestCase, pick: Pick, identity: impl FnOnce() -> I) -> LifecycleEvent<I> {
        let edge = match pick.variant(tc, 4, |index| index) {
            0 => LifecycleEdge::Realise { identity: identity() },
            1 => LifecycleEdge::Vanish,
            2 => LifecycleEdge::Release,
            _ => LifecycleEdge::GiveUp { cause: Cause::generate(tc, pick) },
        };
        LifecycleEvent { edge, at: timestamp(tc), writer: name(tc) }
    }
}

fn event(tc: &TestCase, pick: Pick) -> LifecycleEvent<TerminalIdentity> {
    LifecycleEvent::generate(tc, pick, || TerminalIdentity::generate(tc, pick))
}

fn attempt(tc: &TestCase, pick: Pick) -> Attempt<TerminalAction> {
    use TerminalAction::*;
    Attempt::generate(tc, pick, code_enum(tc, pick, &[Spawn, Attach, Kill]))
}

/// Every variant and option in each round of [`SHAPES`]; five rounds give every code enum every
/// code.
pub const COVERING: usize = SHAPES * 5;

impl Fixture for TerminalSession {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> TerminalSession {
        use LifecycleState::*;
        let source = match pick.variant(tc, 2, |index| index) {
            0 => TerminalSource::Tool { command: text(tc) },
            _ => TerminalSource::Agent {
                selector: name(tc),
                brief: Frozen::new(Brief { text: name(tc), skills: list(tc, pick, 2, |_| name(tc)) }).expect("freeze"),
            },
        };
        TerminalSession {
            vessel: reference(tc),
            role: name(tc),
            ordinal: tc.draw(gs::integers::<u16>()),
            pool: name(tc),
            cwd: text(tc),
            source,
            env: map(tc, pick, 3, |_| text(tc)),
            labels: list(tc, pick, 3, |_| key(tc)).into_iter().collect(),
            realised: pick.some(tc, 7).then(|| TerminalIdentity::generate(tc, pick.nth(1))),
            events: records(tc, pick, 3, |pick| event(tc, pick)),
            attempts: records(tc, pick, 2, |pick| attempt(tc, pick)),
            lifecycle: State::new(code_enum(tc, pick, &[Requested, Realised, Gone, GaveUp]), timestamp(tc)),
        }
    }

    /// Spec, env and labels drawn afresh; lifecycle events and attempts appended.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> TerminalSession {
        let spec = TerminalSession::generate(tc, pick);
        TerminalSession {
            events: evolve_records(tc, pick, &self.events, |event, _| event.clone(), |pick| event(tc, pick)),
            attempts: evolve_records(tc, pick, &self.attempts, |attempt, _| attempt.clone(), |pick| attempt(tc, pick)),
            ..spec
        }
    }

    fn stamped(&self, before: Option<&TerminalSession>, version: u64) -> TerminalSession {
        TerminalSession {
            events: stamp(&self.events, before.map(|before| &before.events), version, |event, _| event.clone()),
            attempts: stamp(&self.attempts, before.map(|before| &before.attempts), version, |attempt, _| attempt.clone()),
            ..self.clone()
        }
    }
}
