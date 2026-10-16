//! `TerminalSession`: a lifecycle kind (`flotilla_state_machines::machines::lifecycle`) with a
//! concrete request, declared here because a lifecycle kind's stored row belongs to whoever owns
//! its request (#3319). The engine supplies the storable state (`LifecycleState`) and history
//! (`LifecycleEvent`, `Attempt`); the request's columns are this test kind's own, transcribed from
//! the #3208 §4.3 sketch:
//! - `vessel` is a reference to the `Vessel` kind (a target kind of the test registry), so a
//!   lifecycle row also exercises references to kinds and their identity rows;
//! - an agent source's brief is frozen inside a data enum variant: a hash column guarded by the
//!   variant's CHECK;
//! - labels are a set: a child table whose key is the value itself.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Edge, Frozen, HistoryOf, Records, Ref, State, StateOf, Stored, Timestamp};
use flotilla_state_machines::{
    machines::lifecycle::{
        Absence, GiveUp, Lifecycle, LifecycleEdge, LifecycleEvent, LifecycleKind, LifecycleState, LifecycleWorld, MemoryLifecycleWorld,
        Observed, Realisation, Realise, Release, Teardown, Vanish,
    },
    Attempt, EdgeEventOf, MachineRow, Resource, RowParts,
};
use hegel::{generators as gs, TestCase};
use serde::{Deserialize, Serialize};

use super::{
    engine::{self, Candidate},
    ensure::{attempt, cause},
    key, list, name, records, reference, stamp_flat, text, timestamp, Fixture, Pick,
};

/// A minimal target kind for `TerminalSession::vessel`.
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Vessel", code = 7)]
pub struct Vessel {
    pub name: String,
}

/// The test lifecycle kind: its name, request and the default retry policy.
pub struct Terminal;

impl LifecycleKind for Terminal {
    const NAME: &'static str = "TerminalSession";
    type Request = TerminalRequest;
}

/// What a caller asks for: the session's request, its immutable spec.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalRequest {
    pub vessel: Ref<Vessel>,
    pub role: String,
    pub ordinal: u16,
    pub pool: String,
    pub cwd: String,
    pub source: TerminalSource,
    pub env: BTreeMap<String, String>,
    pub labels: BTreeSet<String>,
}

/// A vessel's sessions in ordinal order (the #3208 §4.3 `(vessel_id, role, ordinal)` key, without
/// uniqueness), and a pool's live sessions: an interned `eq` column under a lifecycle predicate.
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(
    kind = "TerminalSession",
    code = 6,
    index(name = "by_vessel", eq(vessel), order(ordinal)),
    index(name = "live_by_pool", eq(pool), when(state in [Requested, Realised]))
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
    #[store(state)]
    pub state: State<LifecycleState>,
    #[store(history)]
    pub events: Records<LifecycleEvent>,
    #[store(history)]
    pub attempts: Records<Attempt>,
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

impl TerminalSession {
    pub fn request(&self) -> TerminalRequest {
        TerminalRequest {
            vessel: self.vessel.clone(),
            role: self.role.clone(),
            ordinal: self.ordinal,
            pool: self.pool.clone(),
            cwd: self.cwd.clone(),
            source: self.source.clone(),
            env: self.env.clone(),
            labels: self.labels.clone(),
        }
    }
}

impl MachineRow for TerminalSession {
    type Machine = Lifecycle<Terminal>;
    const STATE_FIELD: StateOf<Self, State<LifecycleState>> = TerminalSession::STATE;
    const EDGE_LOG: HistoryOf<Self, EdgeEventOf<Lifecycle<Terminal>>> = TerminalSession::EVENTS;

    fn of(resource: &Resource<Lifecycle<Terminal>>, since: Timestamp) -> TerminalSession {
        let TerminalRequest { vessel, role, ordinal, pool, cwd, source, env, labels } = resource.spec().clone();
        TerminalSession {
            vessel,
            role,
            ordinal,
            pool,
            cwd,
            source,
            env,
            labels,
            state: State::new(resource.state().clone(), since),
            events: resource.events().clone(),
            attempts: resource.attempts().clone(),
        }
    }

    fn state(&self) -> &State<LifecycleState> {
        &self.state
    }

    fn parts(&self) -> RowParts<Lifecycle<Terminal>> {
        RowParts {
            spec: self.request(),
            state: self.state.value.clone(),
            events: self.events.clone(),
            attempts: self.attempts.clone(),
            receipts: Records::new(),
        }
    }
}

/// Covering indexes: the label is digit 0 (base 4), the source digit 2 (base 2).
pub const COVERING: usize = 48;

pub fn state_value(tc: &TestCase, pick: Pick) -> LifecycleState {
    match pick.digit(tc, 4, 1) {
        0 => LifecycleState::Requested,
        1 => LifecycleState::Realised { identity: text(tc) },
        2 => LifecycleState::Gone,
        _ => LifecycleState::GaveUp { cause: cause(tc, pick) },
    }
}

fn edges(tc: &TestCase, pick: Pick) -> Vec<LifecycleEdge> {
    vec![
        Realise { identity: text(tc), observed: flotilla_state_machines::Ref::new(name(tc)) }.into(),
        Release { teardown: flotilla_state_machines::Ref::new(name(tc)) }.into(),
        Vanish { observed: flotilla_state_machines::Ref::new(name(tc)) }.into(),
        GiveUp { cause: cause(tc, pick) }.into(),
    ]
}

/// Drawn keys collide (and so dedupe); covering keys are distinct.
pub fn map<V>(tc: &TestCase, pick: Pick, max: usize, mut value: impl FnMut(Pick) -> V) -> BTreeMap<String, V> {
    (0..pick.count(tc, max))
        .map(|n| {
            let key = match pick {
                Pick::Draw => key(tc),
                Pick::Cover(_) => format!("k{n}"),
            };
            (key, value(pick.nth(n)))
        })
        .collect()
}

pub fn generate(tc: &TestCase, pick: Pick) -> TerminalSession {
    let source = match pick.digit(tc, 2, 4) {
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
        state: State::new(state_value(tc, pick), timestamp(tc)),
        events: records(tc, pick, 3, |pick| engine::event::<Lifecycle<Terminal>>(tc, pick, &edges(tc, pick))),
        attempts: records(tc, pick.nth(1), 3, |pick| attempt(tc, pick)),
    }
}

fn boxed(world: MemoryLifecycleWorld) -> Box<dyn LifecycleWorld> {
    Box::new(world)
}

/// Every edge a live session may take: realising under an observed identity, releasing once torn
/// down, vanishing when observed absent, and giving up with its last attempt's cause.
pub fn candidates(tc: &TestCase, row: &TerminalSession) -> Vec<Candidate<Lifecycle<Terminal>>> {
    let now = timestamp(tc);
    let mut candidates = Vec::new();
    let (identity, observed) = (text(tc), flotilla_state_machines::Ref::<Realisation>::new(name(tc)));
    let mut world = MemoryLifecycleWorld::default();
    world.realisations.put(&observed, Observed { identity: identity.clone() });
    candidates.push((Realise { identity, observed }.into(), boxed(world), now));
    let teardown = flotilla_state_machines::Ref::<Teardown>::new(name(tc));
    let mut world = MemoryLifecycleWorld::default();
    world.teardowns.put(&teardown, true);
    candidates.push((Release { teardown }.into(), boxed(world), now));
    let absent = flotilla_state_machines::Ref::<Absence>::new(name(tc));
    let mut world = MemoryLifecycleWorld::default();
    world.absences.put(&absent, true);
    candidates.push((Vanish { observed: absent }.into(), boxed(world), now));
    let last = row.attempts.last().and_then(|row| match &row.record.outcome {
        flotilla_state_machines::Outcome::Failed { cause, .. } => Some(cause.clone()),
        flotilla_state_machines::Outcome::Succeeded => None,
    });
    if let Some(cause) = last {
        candidates.push((GiveUp { cause }.into(), boxed(MemoryLifecycleWorld::default()), now));
    }
    candidates
}

impl Fixture for TerminalSession {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> TerminalSession {
        generate(tc, pick)
    }

    /// The request drawn afresh; the state, events and attempts kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> TerminalSession {
        TerminalSession { state: self.state.clone(), events: self.events.clone(), attempts: self.attempts.clone(), ..generate(tc, pick) }
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<TerminalSession>, TerminalSession)> {
        engine::drive(tc, pick, self, candidates(tc, self))
    }

    fn stamped(&self, before: Option<&TerminalSession>, version: u64) -> TerminalSession {
        TerminalSession {
            events: stamp_flat(&self.events, before.map(|before| &before.events), version),
            attempts: stamp_flat(&self.attempts, before.map(|before| &before.attempts), version),
            ..self.clone()
        }
    }
}
