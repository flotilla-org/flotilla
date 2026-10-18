//! `TerminalSession`: a lifecycle kind (`flotilla_state_machines::machines::lifecycle`) with a
//! concrete request, declared here because a lifecycle kind's request is its spec, which declares
//! the stored kind wherever the request is owned (#3319, #3392 ruling D5). The stored kind is
//! `Resource<Lifecycle<Terminal>>`: the engine supplies the storable state (`LifecycleState`, with
//! the attempt tally while Requested); its history, attempts among its edges, is its log entries;
//! the request's columns are this test kind's own,
//! transcribed from the #3208 §4.3 sketch:
//! - `vessel` is a reference to the engine's stored `Vessel` kind, so a lifecycle row also
//!   exercises references to kinds and their identity rows;
//! - an agent source's brief is frozen inside a data enum variant: a hash column guarded by the
//!   variant's CHECK;
//! - labels are a set: a child table whose key is the value itself.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Edge, Frozen, Ref, State, Stored};
use flotilla_state_machines::{
    machines::lifecycle::{
        Absence, Attempted, GiveUp, Lifecycle, LifecycleKind, LifecycleState, LifecycleWorld, MemoryLifecycleWorld, Observed, Realisation,
        Realise, Release, Teardown, Vanish,
    },
    machines::vessel::Vessel,
    Resource,
};
use hegel::{generators as gs, TestCase};
use serde::{Deserialize, Serialize};

use super::{
    engine::{self, resource, Candidate},
    ensure::{cause, outcome, tally},
    key, list, name, reference, text, timestamp, Fixture, Pick,
};

/// The test lifecycle kind: its name, request and the default retry policy.
pub struct Terminal;

impl LifecycleKind for Terminal {
    const NAME: &'static str = "TerminalSession";
    type Request = TerminalRequest;
}

/// The stored TerminalSession.
pub type TerminalSession = Resource<Lifecycle<Terminal>>;

/// What a caller asks for: the session's request, its immutable spec, which declares the stored
/// kind. Indexed for a vessel's sessions in ordinal order (the #3208 §4.3 `(vessel_id, role,
/// ordinal)` key, without uniqueness), and a pool's live sessions: an interned `eq` column under a
/// lifecycle predicate.
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(
    kind = "TerminalSession",
    code = "ts",
    state = LifecycleState,
    index(name = "by_vessel", eq(vessel), order(ordinal)),
    index(name = "live_by_pool", eq(pool), when(state in [Requested, Realised]))
)]
pub struct TerminalRequest {
    pub vessel: Ref<Resource<Vessel>>,
    #[store(intern = "rl")]
    pub role: String,
    pub ordinal: u16,
    #[store(intern = "pl")]
    pub pool: String,
    pub cwd: String,
    pub source: TerminalSource,
    #[store(child)]
    pub env: BTreeMap<String, String>,
    #[store(child)]
    pub labels: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Stored)]
pub enum TerminalSource {
    #[store(code = "c1")]
    Tool { command: String },
    #[store(code = "c2")]
    Agent {
        #[store(intern = "sl")]
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

/// Covering indexes: the label is digit 0 (base 4), the source digit 2 (base 2), and Requested's
/// tally digits 3 and 4 (base 2).
pub const COVERING: usize = 48;

pub fn state_value(tc: &TestCase, pick: Pick) -> LifecycleState {
    match pick.digit(tc, 4, 1) {
        0 => {
            let (failures, last_failure) = tally(tc, pick, 8);
            LifecycleState::Requested { failures, last_failure }
        }
        1 => LifecycleState::Realised { identity: text(tc) },
        2 => LifecycleState::Gone,
        _ => LifecycleState::GaveUp { cause: cause(tc, pick) },
    }
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

pub fn request(tc: &TestCase, pick: Pick) -> TerminalRequest {
    let source = match pick.digit(tc, 2, 4) {
        0 => TerminalSource::Tool { command: text(tc) },
        _ => TerminalSource::Agent {
            selector: name(tc),
            brief: Frozen::new(Brief { text: name(tc), skills: list(tc, pick, 2, |_| name(tc)) }).expect("freeze"),
        },
    };
    TerminalRequest {
        vessel: reference(tc),
        role: name(tc),
        ordinal: tc.draw(gs::integers::<u16>()),
        pool: name(tc),
        cwd: text(tc),
        source,
        env: map(tc, pick, 3, |_| text(tc)),
        labels: list(tc, pick, 3, |_| key(tc)).into_iter().collect(),
    }
}

pub fn generate(tc: &TestCase, pick: Pick) -> TerminalSession {
    resource(request(tc, pick), State::new(state_value(tc, pick), timestamp(tc)))
}

fn boxed(world: MemoryLifecycleWorld) -> Box<dyn LifecycleWorld> {
    Box::new(world)
}

/// Every edge a live lifecycle resource in `state` may take: realising under an observed identity,
/// releasing once torn down, vanishing when observed absent, an attempt of either outcome, and
/// giving up with its last failure's cause.
pub fn candidates<K: LifecycleKind>(tc: &TestCase, state: &LifecycleState) -> Vec<Candidate<Lifecycle<K>>> {
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
    candidates.push((Attempted { outcome: outcome(tc, Pick::Draw) }.into(), boxed(MemoryLifecycleWorld::default()), now));
    let last = match state {
        LifecycleState::Requested { last_failure, .. } => last_failure.as_ref().map(|failure| failure.cause.clone()),
        _ => None,
    };
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

    /// The request drawn afresh; the state kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> TerminalSession {
        self.clone().with_spec(request(tc, pick))
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<TerminalSession>, TerminalSession)> {
        engine::drive(tc, pick, self, candidates::<Terminal>(tc, self.state()))
    }

    fn stamped(&self, _before: Option<&TerminalSession>, _version: u64) -> TerminalSession {
        self.clone()
    }
}
