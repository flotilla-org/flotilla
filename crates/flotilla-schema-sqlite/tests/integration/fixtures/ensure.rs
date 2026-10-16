//! The engine's Ensure (`flotilla_state_machines::machines::ensure::EnsureRow`, kind code 9):
//! generators, and the worlds that let each edge pass its gate.

use std::collections::BTreeSet;

use flotilla_schema::{Edge, Records, State, Timestamp};
use flotilla_state_machines::{
    machines::ensure::{
        Authority, Bind, Create, Ensure, EnsureEdge, EnsureState, EnsureWorld, Fulfiller, FulfillerPhase, FulfillerStatus, GiveUp,
        MemoryEnsureWorld, Record, Resume, Retire, Teardown, Unbind, UnbindReason,
    },
    Attempt, AttemptPolicy, Cause, CauseCategory, Ref,
};
use hegel::{generators as gs, TestCase};

use super::{
    code_enum,
    engine::{self, Candidate},
    name, records, stamp_flat, text, timestamp, EnsureRow, Fixture, Pick,
};

/// Covering indexes: the label is digit 0 (base 4); sets take every size in turn, and attempts
/// both outcomes and every cause category.
pub const COVERING: usize = 40;

fn fulfillers(tc: &TestCase, pick: Pick) -> BTreeSet<Ref<Fulfiller>> {
    (0..pick.count(tc, 3)).map(|n| Ref::new(format!("f{}", pick.nth(n).digit(tc, 4, 1)))).collect()
}

/// A state of every label in turn when covering.
pub fn state_value(tc: &TestCase, pick: Pick) -> EnsureState {
    // Shifted picks, so the sets' sizes do not move with the label's digit.
    let (recorded, created) = (fulfillers(tc, pick.nth(1)), fulfillers(tc, pick.nth(2)));
    let window = tc.draw(gs::integers::<u32>().max_value(3));
    match pick.digit(tc, 4, 1) {
        0 => EnsureState::Unfulfilled { recorded, created, window },
        1 => EnsureState::Fulfilled { recorded, created, window, bound: Ref::new(name(tc)) },
        2 => EnsureState::Held { recorded, created, window },
        _ => EnsureState::Retired { recorded, created, window },
    }
}

pub fn cause(tc: &TestCase, pick: Pick) -> Cause {
    use CauseCategory::*;
    Cause { category: code_enum(tc, pick, &[Transient, Refused, Misconfigured, MissingCapability, External]), detail: text(tc) }
}

/// Both outcomes in turn when covering, every cause category across rounds.
pub fn attempt(tc: &TestCase, pick: Pick) -> Attempt {
    match pick.variant(tc, 2, |index| index) {
        0 => Attempt::succeeded(timestamp(tc)),
        _ => Attempt::failed(timestamp(tc), cause(tc, pick), pick.some(tc, 8)),
    }
}

pub fn policy(tc: &TestCase) -> AttemptPolicy {
    AttemptPolicy {
        max_failures: tc.draw(gs::integers::<u32>().max_value(3)),
        base: tc.draw(gs::integers::<i64>()),
        factor: tc.draw(gs::integers::<u32>()),
        cap: tc.draw(gs::integers::<i64>()),
    }
}

fn edges(tc: &TestCase) -> Vec<EnsureEdge> {
    let fulfiller = || Ref::new(name(tc));
    vec![
        Record { fulfiller: fulfiller() }.into(),
        Create { fulfiller: fulfiller() }.into(),
        Bind { fulfiller: fulfiller() }.into(),
        Unbind { reason: UnbindReason::Replaced, fulfiller: fulfiller() }.into(),
        GiveUp.into(),
        Resume { by: name(tc), authority: Ref::new(name(tc)) }.into(),
        Retire { by: name(tc), authority: Ref::new(name(tc)), teardown: Ref::new(name(tc)) }.into(),
    ]
}

pub fn generate(tc: &TestCase, pick: Pick) -> EnsureRow {
    EnsureRow {
        inbox: name(tc),
        policy: policy(tc),
        state: State::new(state_value(tc, pick), timestamp(tc)),
        edges: records(tc, pick, 2, |pick| engine::event::<Ensure>(tc, pick, &edges(tc))),
        attempts: records(tc, pick.nth(2), 3, |pick| attempt(tc, pick)),
    }
}

/// An unfulfilled Ensure with nothing recorded and no history.
pub fn sample() -> EnsureRow {
    EnsureRow {
        inbox: "inbox".into(),
        policy: AttemptPolicy::immediate(1),
        state: State::new(EnsureState::Unfulfilled { recorded: BTreeSet::new(), created: BTreeSet::new(), window: 0 }, Timestamp(0)),
        edges: Records::new(),
        attempts: Records::new(),
    }
}

fn boxed(world: MemoryEnsureWorld) -> Box<dyn EnsureWorld> {
    Box::new(world)
}

/// Every edge the Ensure's state may take: recording a fulfiller; creating or binding a recorded
/// one (realised, able to receive, its inbox drained); unbinding the bound one once gone; giving
/// up on the attempts it holds; and resuming and retiring on authority, retiring once torn down.
pub fn candidates(tc: &TestCase, pick: Pick, row: &EnsureRow) -> Vec<Candidate<Ensure>> {
    let now = timestamp(tc);
    let state = &row.state.value;
    let mut candidates = Vec::new();
    candidates.push((Record { fulfiller: Ref::new(name(tc)) }.into(), boxed(MemoryEnsureWorld::default()), now));
    let recorded: Vec<&Ref<Fulfiller>> = state.recorded().iter().collect();
    if !recorded.is_empty() {
        let fulfiller = recorded[pick.nth(3).variant(tc, recorded.len(), |index| index)].clone();
        candidates.push((Create { fulfiller: fulfiller.clone() }.into(), boxed(MemoryEnsureWorld::default()), now));
        let mut world = MemoryEnsureWorld::default();
        world.fulfillers.put(&fulfiller, FulfillerStatus { phase: FulfillerPhase::Realised, can_receive: true });
        world.drains.put(&Ref::new(row.inbox.clone()), true);
        candidates.push((Bind { fulfiller }.into(), boxed(world), now));
    }
    if let Some(bound) = state.bound() {
        let mut world = MemoryEnsureWorld::default();
        world.fulfillers.put(bound, FulfillerStatus { phase: FulfillerPhase::Gone, can_receive: false });
        let reason = code_enum(tc, pick, &[UnbindReason::Gone, UnbindReason::Replaced]);
        candidates.push((Unbind { reason, fulfiller: bound.clone() }.into(), boxed(world), now));
    }
    candidates.push((GiveUp.into(), boxed(MemoryEnsureWorld::default()), now));
    let authority: Ref<Authority> = Ref::new(name(tc));
    let mut world = MemoryEnsureWorld::default();
    world.authorities.put(&authority, true);
    candidates.push((Resume { by: name(tc), authority: authority.clone() }.into(), boxed(world), now));
    let teardown: Ref<Teardown> = Ref::new(name(tc));
    let mut world = MemoryEnsureWorld::default();
    world.authorities.put(&authority, true);
    world.teardowns.put(&teardown, true);
    candidates.push((Retire { by: name(tc), authority, teardown }.into(), boxed(world), now));
    candidates
}

impl Fixture for EnsureRow {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> EnsureRow {
        generate(tc, pick)
    }

    /// The inbox and policy drawn afresh; the state, edge log and attempts kept.
    fn evolve(&self, tc: &TestCase, _pick: Pick) -> EnsureRow {
        EnsureRow { inbox: name(tc), policy: policy(tc), ..self.clone() }
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<EnsureRow>, EnsureRow)> {
        engine::drive(tc, pick, self, candidates(tc, pick, self))
    }

    fn stamped(&self, before: Option<&EnsureRow>, version: u64) -> EnsureRow {
        EnsureRow {
            edges: stamp_flat(&self.edges, before.map(|before| &before.edges), version),
            attempts: stamp_flat(&self.attempts, before.map(|before| &before.attempts), version),
            ..self.clone()
        }
    }
}
