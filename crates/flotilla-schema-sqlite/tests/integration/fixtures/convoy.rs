//! The engine's Convoy (`Resource<Convoy>`, declared by `ConvoySpec`, kind code `cv`):
//! generators, and the worlds that let each edge pass its gate.

use std::collections::BTreeMap;

use flotilla_schema::{Edge, State, Timestamp};
use flotilla_state_machines::{
    machines::convoy::{
        Abandon, Convoy, ConvoySpec, ConvoyState, ConvoyWorld, Deliver, Keep, MemoryConvoyWorld, Priority, Promise, PromiseId,
        PromiseState, Readmit, Retract, VesselClass,
    },
    Ref, Resource,
};
use hegel::{generators as gs, TestCase};

use super::{
    code_enum,
    engine::{self, resource, Candidate},
    name, text, timestamp, Fixture, Pick,
};

/// Covering indexes: the label is digit 0 (base 3); collections take every size in turn.
pub const COVERING: usize = 48;

/// Keys from a small domain when drawn, so maps collide and dedupe; distinct when covering.
fn small(tc: &TestCase, pick: Pick, n: usize) -> u32 {
    match pick {
        Pick::Draw => tc.draw(gs::integers::<u32>().min_value(0).max_value(3)),
        Pick::Cover(_) => u32::try_from(n).expect("small fixture"),
    }
}

fn map<K: Ord, V>(tc: &TestCase, pick: Pick, max: usize, mut entry: impl FnMut(Pick, usize) -> (K, V)) -> BTreeMap<K, V> {
    (0..pick.count(tc, max)).map(|n| entry(pick.nth(n), n)).collect()
}

pub fn resolution(tc: &TestCase, pick: Pick) -> BTreeMap<PromiseId, PromiseState> {
    use PromiseState::*;
    map(tc, pick, 3, |pick, n| (PromiseId(small(tc, pick, n)), code_enum(tc, pick, &[Pending, Kept, Retracted])))
}

pub fn quota(tc: &TestCase, pick: Pick) -> BTreeMap<VesselClass, u32> {
    map(tc, pick.nth(1), 2, |pick, n| (VesselClass(small(tc, pick, n)), tc.draw(gs::integers::<u32>())))
}

/// A state of every label in turn when covering, its collections of every size.
pub fn state_value(tc: &TestCase, pick: Pick) -> ConvoyState {
    let (resolution, quota) = (resolution(tc, pick), quota(tc, pick));
    match pick.digit(tc, 3, 1) {
        0 => ConvoyState::Open { resolution, quota },
        1 => ConvoyState::Delivered { resolution, quota },
        _ => ConvoyState::Abandoned { resolution, quota, reason: text(tc) },
    }
}

fn authority(tc: &TestCase) -> Ref<flotilla_state_machines::machines::convoy::Authority> {
    Ref::new(name(tc))
}

/// The project, promise definitions and priority, drawn or covered.
pub fn spec(tc: &TestCase, pick: Pick) -> ConvoySpec {
    ConvoySpec {
        project: name(tc),
        promises: map(tc, pick.nth(2), 2, |pick, n| (PromiseId(small(tc, pick, n)), Promise { kind: name(tc), target: name(tc) })),
        priority: Priority(tc.draw(gs::integers::<u32>())),
    }
}

pub fn generate(tc: &TestCase, pick: Pick) -> Resource<Convoy> {
    resource(spec(tc, pick), State::new(state_value(tc, pick), timestamp(tc)))
}

/// A fixed open convoy for example tests: one pending promise under a quota of one.
pub fn sample() -> Resource<Convoy> {
    let promise = PromiseId(0);
    let spec = ConvoySpec {
        project: "p".into(),
        promises: [(promise, Promise { kind: "change".into(), target: "main".into() })].into(),
        priority: Priority(1),
    };
    let open = ConvoyState::Open { resolution: [(promise, PromiseState::Pending)].into(), quota: [(VesselClass(0), 1)].into() };
    resource(spec, State::new(open, Timestamp(0)))
}

fn boxed(world: MemoryConvoyWorld) -> Box<dyn ConvoyWorld> {
    Box::new(world)
}

fn authorised(authority: &Ref<flotilla_state_machines::machines::convoy::Authority>) -> Box<dyn ConvoyWorld> {
    let mut world = MemoryConvoyWorld::default();
    world.authority.put(authority, true);
    boxed(world)
}

/// Every edge an open convoy may take: keeping or retracting a pending promise, delivering,
/// abandoning with a reason, and readmitting with every count raised or kept.
pub fn candidates(tc: &TestCase, pick: Pick, convoy: &Resource<Convoy>) -> Vec<Candidate<Convoy>> {
    let now = timestamp(tc);
    let mut candidates = Vec::new();
    let pending: Vec<PromiseId> = convoy.state().outstanding().into_iter().collect();
    if !pending.is_empty() {
        let promise = pending[pick.nth(3).variant(tc, pending.len(), |index| index)];
        candidates.push((Keep { promise }.into(), boxed(MemoryConvoyWorld::default()), now));
        let authority = authority(tc);
        candidates.push((Retract { promise, by: name(tc), authority: authority.clone() }.into(), authorised(&authority), now));
    }
    candidates.push((Deliver.into(), boxed(MemoryConvoyWorld::default()), now));
    let authority = authority(tc);
    let reason = format!("{}.", text(tc));
    candidates.push((Abandon { by: name(tc), reason, authority: authority.clone() }.into(), authorised(&authority), now));
    let raised = convoy
        .state()
        .quota()
        .iter()
        .map(|(class, count)| (*class, count.saturating_add(tc.draw(gs::integers::<u32>().max_value(2)))))
        .collect();
    let authority = Ref::new(name(tc));
    candidates.push((Readmit { quota: raised, by: name(tc), authority: authority.clone() }.into(), authorised(&authority), now));
    candidates
}

pub fn edge(tc: &TestCase, pick: Pick, convoy: &Resource<Convoy>) -> Option<(Edge<Resource<Convoy>>, Resource<Convoy>)> {
    engine::drive(tc, pick, convoy, candidates(tc, pick, convoy))
}

impl Fixture for Resource<Convoy> {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> Resource<Convoy> {
        generate(tc, pick)
    }

    /// The project, promise definitions and priority drawn afresh; the state and edge log kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> Resource<Convoy> {
        self.clone().with_spec(spec(tc, pick))
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<Resource<Convoy>>, Resource<Convoy>)> {
        edge(tc, pick, self)
    }

    fn stamped(&self, _before: Option<&Resource<Convoy>>, _version: u64) -> Resource<Convoy> {
        self.clone()
    }
}
