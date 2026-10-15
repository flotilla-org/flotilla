//! The lifecycle-shaped fixture (TerminalSession), transcribed from the #3208 §4.3 sketch: the
//! shared lifecycle edge is generic over each kind's identity type, and TerminalSession supplies
//! `TerminalIdentity`.
//!
//! Modelling decisions the storage list forces:
//! - The edge log is an [`EdgeEvent`] header plus one document, whose `LifecycleEdge<I>` arguments
//!   carry the identity (`Realise { identity }`): the same generic shape serves Vessel,
//!   Environment and the others without a column per identity field.
//! - The realised identity on the resource is a flattened `Option<TerminalIdentity>` with a
//!   presence flag, rather than columns read from the latest edge.
//! - An agent source's brief is frozen inside a data enum variant: a hash column guarded by the
//!   variant's CHECK.
//! - Labels are a set: a child table whose key is the value itself.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Edge, EdgeEvent, Frozen, Records, Ref, State, Stored};
use hegel::{generators as gs, TestCase};
use serde::{Deserialize, Serialize};

use super::{
    appended,
    attempt::{Attempt, Cause},
    code_enum,
    convoy::map,
    event, key, list, name, payload, records, reference, stamp, text, timestamp, Fixture, Payload, Pick, SHAPES,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct TerminalIdentity {
    pub session_id: String,
    pub pid: Option<i64>,
    pub endpoint: Option<String>,
}

/// One lifecycle edge event; generic over the identity a `Realise` edge carries.
pub type LifecycleEvent<I> = EdgeEvent<LifecycleState, LifecycleEdgeName, Payload<LifecycleEdge<I>>>;

/// The edge column of a lifecycle's edge log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum LifecycleEdgeName {
    #[store(code = 1)]
    Realise,
    #[store(code = 2)]
    Vanish,
    #[store(code = 3)]
    Release,
    #[store(code = 4)]
    GiveUp,
}

/// The lifecycle edges' arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LifecycleEdge<I> {
    Realise { identity: I },
    Vanish,
    Release,
    GiveUp { cause: Cause },
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

impl<I> LifecycleEdge<I> {
    /// Every edge in turn when covering; `identity` draws the kind's own identity.
    pub fn generate(tc: &TestCase, pick: Pick, identity: impl FnOnce() -> I) -> LifecycleEdge<I> {
        match pick.variant(tc, 4, |index| index) {
            0 => LifecycleEdge::Realise { identity: identity() },
            1 => LifecycleEdge::Vanish,
            2 => LifecycleEdge::Release,
            _ => LifecycleEdge::GiveUp { cause: Cause::generate(tc, pick) },
        }
    }

    pub fn name(&self) -> LifecycleEdgeName {
        match self {
            LifecycleEdge::Realise { .. } => LifecycleEdgeName::Realise,
            LifecycleEdge::Vanish => LifecycleEdgeName::Vanish,
            LifecycleEdge::Release => LifecycleEdgeName::Release,
            LifecycleEdge::GiveUp { .. } => LifecycleEdgeName::GiveUp,
        }
    }
}

fn args(tc: &TestCase, pick: Pick) -> LifecycleEdge<TerminalIdentity> {
    LifecycleEdge::generate(tc, pick, || TerminalIdentity::generate(tc, pick))
}

/// An event of any edge between any labels.
fn edge_event(tc: &TestCase, pick: Pick) -> LifecycleEvent<TerminalIdentity> {
    use LifecycleState::*;
    let (args, labels) = (args(tc, pick), [Requested, Realised, Gone, GaveUp]);
    event(tc, args.name(), code_enum(tc, pick, &labels), code_enum(tc, pick.nth(1), &labels), payload(tc, pick, args))
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
            events: records(tc, pick, 3, |pick| edge_event(tc, pick)),
            attempts: records(tc, pick, 2, |pick| attempt(tc, pick)),
            lifecycle: State::new(code_enum(tc, pick, &[Requested, Realised, Gone, GaveUp]), timestamp(tc)),
        }
    }

    /// Spec, env and labels drawn afresh; the lifecycle, the realised identity, events and
    /// attempts kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> TerminalSession {
        TerminalSession {
            realised: self.realised.clone(),
            events: self.events.clone(),
            attempts: self.attempts.clone(),
            lifecycle: self.lifecycle,
            ..TerminalSession::generate(tc, pick)
        }
    }

    /// The lifecycle moved and a lifecycle event appended; a `Realise` sets the realised identity
    /// (a flattened optional struct), and sometimes an attempt is appended.
    fn edge(&self, tc: &TestCase, pick: Pick) -> (Edge<TerminalSession>, TerminalSession) {
        use LifecycleState::*;
        let args = args(tc, pick);
        let lifecycle = State::new(code_enum(tc, pick.nth(1), &[Requested, Realised, Gone, GaveUp]), timestamp(tc));
        let event = event(tc, args.name(), self.lifecycle.value, lifecycle.value, payload(tc, pick, args));
        let mut edge = Edge::new(TerminalSession::LIFECYCLE, &lifecycle, TerminalSession::EVENTS, &event).expect("edge");
        let mut next = TerminalSession { lifecycle, ..self.clone() };
        if let LifecycleEdge::Realise { identity } = &event.payload.args {
            let realised = Some(identity.clone());
            edge = edge.set(TerminalSession::REALISED, &realised).expect("realised");
            next.realised = realised;
        }
        if pick.nth(2).some(tc, 8) {
            let attempt = attempt(tc, pick);
            edge = edge.append(TerminalSession::ATTEMPTS, &attempt).expect("attempt");
            next.attempts = appended(tc, &self.attempts, attempt);
        }
        next.events = appended(tc, &self.events, event);
        (edge, next)
    }

    fn stamped(&self, before: Option<&TerminalSession>, version: u64) -> TerminalSession {
        TerminalSession {
            events: stamp(&self.events, before.map(|before| &before.events), version, |event, _| event.clone()),
            attempts: stamp(&self.attempts, before.map(|before| &before.attempts), version, |attempt, _| attempt.clone()),
            ..self.clone()
        }
    }
}
