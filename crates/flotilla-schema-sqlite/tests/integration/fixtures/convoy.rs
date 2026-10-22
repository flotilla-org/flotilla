//! The engine's Convoy (`Resource<Convoy>`, declared by `ConvoySpec`, kind code `cv`):
//! generators, and the worlds that let each edge pass its gate.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Edge, Frozen, Records, State, Timestamp};
use flotilla_state_machines::{
    machines::convoy::{
        Abandon, AddSeat, Authority, Convoy, ConvoySpec, ConvoyState, ConvoyWorld, Declare, Deliver, Duty, DutyId, DutyScope, Keep,
        MemoryConvoyWorld, Priority, Progress, Promise, PromiseId, PromiseKind, PromiseKinds, PromiseSource, Readmit, Reassign, Reject,
        Resupervise, RetireDuty, Retract, Seat, SeatName, Submission, Submit, Terms, VesselClass,
    },
    Ref, Resource,
};
use hegel::{generators as gs, TestCase};

use super::{
    address, code_enum,
    engine::{self, resource, Candidate},
    name, records, stamp, stamp_flat, text, timestamp, version, Fixture, Pick, SHAPES,
};

/// Covering indexes: every shape of [`SHAPES`] in each round, and the label by round (base 3), so
/// every label meets every shape; four rounds give every code enum every code.
pub const COVERING: usize = SHAPES * 4;

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

fn seat_name(tc: &TestCase) -> SeatName {
    SeatName::new(name(tc), name(tc))
}

fn seat(tc: &TestCase, pick: Pick) -> Seat {
    let parent = pick.some(tc, SHAPES / 2).then(|| seat_name(tc));
    Seat { role: name(tc), seat: name(tc), parent, supervised_by: address(tc) }
}

fn submission(tc: &TestCase) -> Submission {
    Submission { address: address(tc), revision: text(tc), by: seat_name(tc) }
}

fn promise(tc: &TestCase, pick: Pick, n: usize) -> Promise {
    use PromiseSource::*;
    Promise {
        id: PromiseId(small(tc, pick, n)),
        label: name(tc),
        kind: name(tc),
        target: address(tc),
        source: code_enum(tc, pick, &[Template, Dispatch, Crew]),
        owner: seat_name(tc),
        progress: State::new(
            code_enum(tc, pick.nth(1), &[Progress::Pending, Progress::Submitted, Progress::Kept, Progress::Retracted]),
            timestamp(tc),
        ),
        submissions: records(tc, pick.nth(2), 2, |_| submission(tc)),
    }
}

fn duty(tc: &TestCase, pick: Pick) -> Duty {
    let scope = match pick.variant(tc, 2, |index| index) {
        0 => DutyScope::Lifetime,
        _ => {
            DutyScope::WhileOpen { promises: map(tc, pick.nth(1), 2, |pick, n| (PromiseId(small(tc, pick, n)), ())).into_keys().collect() }
        }
    };
    let terms = Terms {
        reply_within: pick.some(tc, 5).then(|| tc.draw(gs::integers::<u64>())),
        heartbeat: None,
        note: pick.some(tc, 9).then(|| text(tc)),
    };
    Duty { owner: seat_name(tc), scope, terms }
}

/// The fixed fields and records, drawn or covered: seats with and without a parent, promises of
/// every source and progress with zero to two submissions, and duties of both scopes.
pub fn spec(tc: &TestCase, pick: Pick) -> ConvoySpec {
    let kinds = PromiseKinds { kinds: map(tc, pick.nth(3), 1, |_, _| (name(tc), PromiseKind::default())) };
    // Promise ids and labels are unique within a convoy.
    let mut promises = Records::new();
    let (mut ids, mut labels) = (BTreeSet::new(), BTreeSet::new());
    for n in 0..pick.count(tc, 2) {
        let row = promise(tc, pick.nth(n), n);
        if !ids.contains(&row.id) && labels.insert(row.label.clone()) {
            ids.insert(row.id);
            promises.append(version(tc), row).expect("room");
        }
    }
    ConvoySpec {
        project: address(tc),
        priority: Priority(tc.draw(gs::integers::<u32>())),
        kinds: Frozen::new(kinds).expect("freeze"),
        duties: map(tc, pick.nth(4), 2, |pick, n| (DutyId(small(tc, pick, n)), duty(tc, pick))),
        seats: records(tc, pick.nth(5), 2, |pick| seat(tc, pick)),
        promises,
    }
}

pub fn quota(tc: &TestCase, pick: Pick) -> BTreeMap<VesselClass, u32> {
    map(tc, pick.nth(1), 2, |_, n| (VesselClass::new(format!("{}{n}", name(tc))), tc.draw(gs::integers::<u32>())))
}

fn retired(tc: &TestCase, pick: Pick) -> BTreeSet<DutyId> {
    map(tc, pick.nth(2), 2, |pick, n| (DutyId(small(tc, pick, n)), ())).into_keys().collect()
}

/// A state of every label in turn when covering, its collections of every size.
pub fn state_value(tc: &TestCase, pick: Pick) -> ConvoyState {
    let (quota, retired) = (quota(tc, pick), retired(tc, pick));
    match pick.digit(tc, 3, SHAPES) {
        0 => ConvoyState::Open { quota, retired },
        1 => ConvoyState::Delivered { quota, retired },
        _ => ConvoyState::Abandoned { quota, retired, reason: text(tc) },
    }
}

pub fn generate(tc: &TestCase, pick: Pick) -> Resource<Convoy> {
    resource(spec(tc, pick), State::new(state_value(tc, pick), timestamp(tc)))
}

/// A fixed open convoy for example tests: one pending template promise owned by its one seat,
/// under a quota of one.
pub fn sample() -> Resource<Convoy> {
    let owner = SeatName::new("coder", "");
    let mut seats = Records::new();
    seats
        .append(0, Seat { role: "coder".into(), seat: String::new(), parent: None, supervised_by: "project:p".parse().expect("address") })
        .expect("room");
    let mut promises = Records::new();
    let promise = Promise {
        id: PromiseId(0),
        label: "template/pr".into(),
        kind: "change".into(),
        target: "forge:github/repo:o%2Fr/cr:1".parse().expect("address"),
        source: PromiseSource::Template,
        owner,
        progress: State::new(Progress::Pending, Timestamp(0)),
        submissions: Records::new(),
    };
    promises.append(0, promise).expect("room");
    let spec = ConvoySpec {
        project: "project:p".parse().expect("address"),
        priority: Priority(1),
        kinds: Frozen::new(PromiseKinds::default()).expect("freeze"),
        duties: BTreeMap::new(),
        seats,
        promises,
    };
    let open = ConvoyState::Open { quota: [(VesselClass::new("standard"), 1)].into(), retired: BTreeSet::new() };
    resource(spec, State::new(open, Timestamp(0)))
}

/// A world in which every authority and every reference is authorised and durable.
fn yes(authority: &str) -> Box<dyn ConvoyWorld> {
    let mut world = MemoryConvoyWorld::default();
    world.authority.put(&Ref::new(authority), true);
    world.durable.put(&Ref::new(authority), true);
    Box::new(world)
}

/// Every edge an open convoy may take: declaring a promise, each promise edge on an open
/// promise (by its owner, or an operator's verdict), the seat edges, retiring a holding duty,
/// delivering, abandoning, and readmitting with every count raised or kept.
pub fn candidates(tc: &TestCase, pick: Pick, convoy: &Resource<Convoy>) -> Vec<Candidate<Convoy>> {
    // One draw of each value, shared by the candidates: draws are what generation spends.
    let now = timestamp(tc);
    let spec = convoy.spec();
    let authority = name(tc);
    let by = seat_name(tc);
    let (word, place, prose) = (name(tc), address(tc), format!("{}.", text(tc)));
    let mut candidates: Vec<Candidate<Convoy>> = Vec::new();
    let seats: Vec<SeatName> = spec.seats.iter().map(|row| row.record.name()).collect();
    if let Some(owner) = seats.first() {
        let unused = (0..).map(PromiseId).find(|id| spec.promise(*id).is_none()).expect("an unused id");
        let label = (0..)
            .map(|n| format!("p{n}"))
            .find(|label| spec.promises.iter().all(|row| row.record.label != *label))
            .expect("an unused label");
        candidates.push((
            Declare { promise: unused, label, kind: word.clone(), target: place.clone(), owner: owner.clone() }.into(),
            yes(&authority),
            now,
        ));
    }
    let open: Vec<&Promise> = spec.promises.iter().map(|row| &row.record).filter(|promise| promise.open()).collect();
    if !open.is_empty() {
        let promise = open[pick.nth(3).variant(tc, open.len(), |index| index)];
        let id = promise.id;
        let durable = Ref::new(authority.as_str());
        let submit = Submit { promise: id, by: promise.owner.clone(), address: place.clone(), revision: prose.clone(), durable };
        candidates.push((submit.into(), yes(&authority), now));
        candidates.push((Reject { promise: id }.into(), yes(&authority), now));
        let current = u32::try_from(promise.submissions.len()).expect("small");
        candidates.push((Keep::Observed { promise: id, submission: current }.into(), yes(&authority), now));
        let reason = prose.clone();
        let verdict = Keep::Verdict { promise: id, by: by.clone(), authority: Authority::Operator, reason: reason.clone() };
        candidates.push((verdict.into(), yes(&authority), now));
        candidates.push((Retract { promise: id, by: by.clone(), authority: Authority::Operator, reason }.into(), yes(&authority), now));
        if let Some(to) = seats.last() {
            let reassign = Reassign { promise: id, to: to.clone(), by: by.clone(), authority: Authority::Operator };
            candidates.push((reassign.into(), yes(&authority), now));
        }
    }
    if let Some(parent) = seats.first() {
        let add = AddSeat {
            seat: SeatName::new(format!("{word}-fan"), word.clone()),
            parent: parent.clone(),
            supervised_by: place.clone(),
            by: by.clone(),
            authority: Ref::new(authority.as_str()),
        };
        candidates.push((add.into(), yes(&authority), now));
        let change =
            Resupervise { seat: parent.clone(), supervised_by: place.clone(), by: by.clone(), authority: Ref::new(authority.as_str()) };
        candidates.push((change.into(), yes(&authority), now));
    }
    if let Some(duty) = spec.holding(convoy.state().retired()).into_iter().next() {
        candidates.push((RetireDuty { duty, by: by.clone(), authority: Ref::new(authority.as_str()) }.into(), yes(&authority), now));
    }
    candidates.push((Deliver.into(), yes(&authority), now));
    let reason = prose;
    candidates.push((Abandon { by: by.clone(), reason, authority: Ref::new(authority.as_str()) }.into(), yes(&authority), now));
    let raised = convoy
        .state()
        .quota()
        .iter()
        .map(|(class, count)| (class.clone(), count.saturating_add(tc.draw(gs::integers::<u32>().max_value(2)))))
        .collect();
    candidates.push((Readmit { quota: raised, by, authority: Ref::new(authority.as_str()) }.into(), yes(&authority), now));
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

    /// The project, priority, kinds and duties drawn afresh; the records, state and edge log kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> Resource<Convoy> {
        let drawn = spec(tc, pick);
        let spec = ConvoySpec { seats: self.spec().seats.clone(), promises: self.spec().promises.clone(), ..drawn };
        self.clone().with_spec(spec)
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<Resource<Convoy>>, Resource<Convoy>)> {
        edge(tc, pick, self)
    }

    /// Seats, promises and each promise's submissions that `before` lacks are stamped `version`.
    fn stamped(&self, before: Option<&Resource<Convoy>>, version: u64) -> Resource<Convoy> {
        let spec = self.spec();
        let before_spec = before.map(Resource::spec);
        let seats = stamp_flat(&spec.seats, before_spec.map(|before| &before.seats), version);
        let promises = stamp(&spec.promises, before_spec.map(|before| &before.promises), version, |promise, before| Promise {
            submissions: stamp_flat(&promise.submissions, before.map(|before| &before.submissions), version),
            ..promise.clone()
        });
        self.clone().with_spec(ConvoySpec { seats, promises, ..spec.clone() })
    }
}
