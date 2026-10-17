//! The engine's Ensure (`Resource<Ensure>`, declared by `EnsureSpec`, kind code `en`):
//! generators, and the worlds that let each edge pass its gate.

use std::collections::BTreeSet;

use flotilla_schema::{Edge, State, Timestamp};
use flotilla_state_machines::{
    machines::ensure::{
        Attempted, Authority, Bind, Create, Ensure, EnsureEdge, EnsureSpec, EnsureState, EnsureWorld, Fulfiller, FulfillerPhase,
        FulfillerStatus, Hold, MemoryEnsureWorld, Record, Resume, Retire, Teardown, Unbind, UnbindReason,
    },
    AttemptPolicy, Cause, CauseCategory, Failure, Outcome, Ref, Resource,
};
use hegel::{generators as gs, TestCase};

use super::{
    code_enum,
    engine::{self, resource, Candidate},
    name, records, stamp_flat, text, timestamp, Fixture, Pick,
};

/// Covering indexes: the label is digit 0 (base 4) and the tally digits 2 and 3 (base 2); sets take
/// every size in turn, and attempts both outcomes and every cause category.
pub const COVERING: usize = 40;

fn fulfillers(tc: &TestCase, pick: Pick) -> BTreeSet<Ref<Fulfiller>> {
    (0..pick.count(tc, 3)).map(|n| Ref::new(format!("f{}", pick.nth(n).digit(tc, 4, 1)))).collect()
}

/// A state of every label in turn when covering.
pub fn state_value(tc: &TestCase, pick: Pick) -> EnsureState {
    // Shifted picks, so the sets' sizes do not move with the label's digit.
    let (recorded, created) = (fulfillers(tc, pick.nth(1)), fulfillers(tc, pick.nth(2)));
    let (failures, last_failure) = tally(tc, pick, 4);
    match pick.digit(tc, 4, 1) {
        0 => EnsureState::Unfulfilled { recorded, created, failures, last_failure },
        1 => EnsureState::Fulfilled { recorded, created, failures, last_failure, bound: Ref::new(name(tc)) },
        2 => EnsureState::Held { recorded, created, failures, last_failure },
        _ => EnsureState::Retired { recorded, created, failures, last_failure },
    }
}

pub fn cause(tc: &TestCase, pick: Pick) -> Cause {
    use CauseCategory::*;
    Cause { category: code_enum(tc, pick, &[Transient, Refused, Misconfigured, MissingCapability, External]), detail: text(tc) }
}

/// A coherent attempt tally: a latest failure (the base 2 digit at `place`), retryable or not (the
/// next digit), behind one to three consecutive failures, or none. Covering reaches a tally that
/// a failure that cannot be retried exhausts.
pub fn tally(tc: &TestCase, pick: Pick, place: usize) -> (u32, Option<Failure>) {
    if pick.digit(tc, 2, place) != 0 {
        return (0, None);
    }
    let failure = Failure { at: timestamp(tc), cause: cause(tc, pick), retryable: pick.digit(tc, 2, place * 2) == 0 };
    (tc.draw(gs::integers::<u32>().min_value(1).max_value(3)), Some(failure))
}

/// Both outcomes in turn when covering, every cause category across rounds.
pub fn outcome(tc: &TestCase, pick: Pick) -> Outcome {
    match pick.variant(tc, 2, |index| index) {
        0 => Outcome::Succeeded,
        _ => Outcome::Failed { cause: cause(tc, pick), retryable: pick.some(tc, 8) },
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

fn edges(tc: &TestCase, pick: Pick) -> Vec<EnsureEdge> {
    let fulfiller = || Ref::new(name(tc));
    vec![
        Record { fulfiller: fulfiller() }.into(),
        Create { fulfiller: fulfiller() }.into(),
        Bind { fulfiller: fulfiller() }.into(),
        Unbind { reason: UnbindReason::Replaced, fulfiller: fulfiller() }.into(),
        Attempted { fulfiller: fulfiller(), outcome: outcome(tc, pick) }.into(),
        Hold.into(),
        Resume { by: name(tc), authority: Ref::new(name(tc)) }.into(),
        Retire { by: name(tc), authority: Ref::new(name(tc)), teardown: Ref::new(name(tc)) }.into(),
    ]
}

pub fn spec(tc: &TestCase) -> EnsureSpec {
    EnsureSpec { inbox: name(tc), policy: policy(tc) }
}

pub fn generate(tc: &TestCase, pick: Pick) -> Resource<Ensure> {
    resource(spec(tc), State::new(state_value(tc, pick), timestamp(tc)))
        .with_events(records(tc, pick, 2, |pick| engine::event::<Ensure>(tc, pick, &edges(tc, pick))))
}

/// A fixed spec for example tests: `inbox`, one immediate attempt.
pub fn sample_spec() -> EnsureSpec {
    EnsureSpec { inbox: "inbox".into(), policy: AttemptPolicy::immediate(1) }
}

/// An unfulfilled Ensure with nothing recorded and no history.
pub fn sample() -> Resource<Ensure> {
    let unfulfilled = EnsureState::Unfulfilled { recorded: BTreeSet::new(), created: BTreeSet::new(), failures: 0, last_failure: None };
    resource(sample_spec(), State::new(unfulfilled, Timestamp(0)))
}

fn boxed(world: MemoryEnsureWorld) -> Box<dyn EnsureWorld> {
    Box::new(world)
}

/// Every edge the Ensure's state may take: recording a fulfiller; creating or binding a recorded
/// one (realised, able to receive, its inbox drained); unbinding the bound one once gone; an
/// attempt of either outcome; holding on the window it has spent; and resuming and retiring on
/// authority, retiring once torn down.
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
        world.drains.put(&Ref::new(ensure.spec().inbox.clone()), true);
        candidates.push((Bind { fulfiller }.into(), boxed(world), now));
    }
    if let Some(bound) = state.bound() {
        let mut world = MemoryEnsureWorld::default();
        world.fulfillers.put(bound, FulfillerStatus { phase: FulfillerPhase::Gone, can_receive: false });
        let reason = code_enum(tc, pick, &[UnbindReason::Gone, UnbindReason::Replaced]);
        candidates.push((Unbind { reason, fulfiller: bound.clone() }.into(), boxed(world), now));
    }
    let attempted = Attempted { fulfiller: Ref::new(name(tc)), outcome: outcome(tc, pick.nth(4)) };
    candidates.push((attempted.into(), boxed(MemoryEnsureWorld::default()), now));
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

    /// The inbox and policy drawn afresh; the state and edge log kept.
    fn evolve(&self, tc: &TestCase, _pick: Pick) -> Resource<Ensure> {
        self.clone().with_spec(spec(tc))
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<Resource<Ensure>>, Resource<Ensure>)> {
        engine::drive(tc, pick, self, candidates(tc, pick, self))
    }

    fn stamped(&self, before: Option<&Resource<Ensure>>, version: u64) -> Resource<Ensure> {
        self.clone().with_events(stamp_flat(self.events(), before.map(|before| before.events()), version))
    }
}
