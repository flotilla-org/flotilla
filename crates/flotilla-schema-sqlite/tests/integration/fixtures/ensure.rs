//! The engine's Ensure (`Resource<Ensure>`, declared by `EnsureSpec`, kind code `en`):
//! generators, and the worlds that let each edge pass its gate.

use std::collections::BTreeSet;

use flotilla_schema::{Edge, State, Timestamp};
use flotilla_state_machines::{
    machines::ensure::{
        Authority, Bind, Create, Ensure, EnsureSpec, EnsureState, EnsureWorld, Fulfiller, FulfillerPhase, FulfillerStatus, Hold,
        MemoryEnsureWorld, Record, Resume, Retire, Teardown, Unbind, UnbindReason,
    },
    Ref, Resource,
};
use hegel::TestCase;

use super::{
    addr, address, code_enum,
    engine::{self, resource, Candidate},
    name, timestamp, Fixture, Pick,
};

/// Covering indexes: the label is digit 0 (base 4); sets take every size in turn.
pub const COVERING: usize = 40;

fn fulfillers(tc: &TestCase, pick: Pick) -> BTreeSet<Ref<Fulfiller>> {
    (0..pick.count(tc, 3)).map(|n| Ref::new(format!("f{}", pick.nth(n).digit(tc, 4, 1)))).collect()
}

/// A state of every label in turn when covering.
pub fn state_value(tc: &TestCase, pick: Pick) -> EnsureState {
    // Shifted picks, so the sets' sizes do not move with the label's digit.
    let (recorded, created) = (fulfillers(tc, pick.nth(1)), fulfillers(tc, pick.nth(2)));
    match pick.digit(tc, 4, 1) {
        0 => EnsureState::Unfulfilled { recorded, created },
        1 => EnsureState::Fulfilled { recorded, created, bound: Ref::new(name(tc)) },
        2 => EnsureState::Held { recorded, created },
        _ => EnsureState::Retired { recorded, created },
    }
}

pub fn spec(tc: &TestCase) -> EnsureSpec {
    EnsureSpec { inbox: address(tc) }
}

pub fn generate(tc: &TestCase, pick: Pick) -> Resource<Ensure> {
    resource(spec(tc), State::new(state_value(tc, pick), timestamp(tc)))
}

/// A fixed spec for example tests: `inbox`.
pub fn sample_spec() -> EnsureSpec {
    EnsureSpec { inbox: addr("inbox") }
}

/// An unfulfilled Ensure with nothing recorded.
pub fn sample() -> Resource<Ensure> {
    let unfulfilled = EnsureState::Unfulfilled { recorded: BTreeSet::new(), created: BTreeSet::new() };
    resource(sample_spec(), State::new(unfulfilled, Timestamp(0)))
}

fn boxed(world: MemoryEnsureWorld) -> Box<dyn EnsureWorld> {
    Box::new(world)
}

/// Every edge the Ensure's state may take: recording a fulfiller; creating or binding a recorded
/// one (realised, able to receive, its inbox drained); unbinding the bound one once gone; holding;
/// and resuming and retiring on authority, retiring once torn down.
pub fn candidates(tc: &TestCase, pick: Pick, ensure: &Resource<Ensure>) -> Vec<Candidate<Ensure>> {
    let now = timestamp(tc);
    let state = ensure.state();
    let mut candidates = Vec::new();
    candidates.push((Record { fulfiller: Ref::new(name(tc)) }.into(), boxed(MemoryEnsureWorld::default()), now));
    let recorded: Vec<&Ref<Fulfiller>> = state.recorded().iter().collect();
    if !recorded.is_empty() {
        let fulfiller = recorded[pick.nth(3).variant(tc, recorded.len(), |index| index)].clone();
        candidates.push((Create { fulfiller: fulfiller.clone() }.into(), boxed(MemoryEnsureWorld::default()), now));
        let mut world = MemoryEnsureWorld::default();
        world.fulfillers.put(&fulfiller, FulfillerStatus { phase: FulfillerPhase::Realised, can_receive: true });
        world.drains.put(&Ref::new(ensure.spec().inbox.as_str()), true);
        candidates.push((Bind { fulfiller }.into(), boxed(world), now));
    }
    if let Some(bound) = state.bound() {
        let mut world = MemoryEnsureWorld::default();
        world.fulfillers.put(bound, FulfillerStatus { phase: FulfillerPhase::Gone, can_receive: false });
        let reason = code_enum(tc, pick, &[UnbindReason::Gone, UnbindReason::Replaced]);
        candidates.push((Unbind { reason, fulfiller: bound.clone() }.into(), boxed(world), now));
    }
    candidates.push((Hold.into(), boxed(MemoryEnsureWorld::default()), now));
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

impl Fixture for Resource<Ensure> {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> Resource<Ensure> {
        generate(tc, pick)
    }

    /// The inbox drawn afresh; the state and edge log kept.
    fn evolve(&self, tc: &TestCase, _pick: Pick) -> Resource<Ensure> {
        self.clone().with_spec(spec(tc))
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<Resource<Ensure>>, Resource<Ensure>)> {
        engine::drive(tc, pick, self, candidates(tc, pick, self))
    }

    fn stamped(&self, _before: Option<&Resource<Ensure>>, _version: u64) -> Resource<Ensure> {
        self.clone()
    }
}
