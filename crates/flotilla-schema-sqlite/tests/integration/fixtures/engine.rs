//! Driving the engine for the engine kinds' fixtures: an edge on a stored resource is one the
//! engine applied, through `Home`, to that resource.

use std::borrow::Cow;

use flotilla_schema::{Cbor, Edge, Kind, KindSpec, Labelled, State, StateData, Timestamp};
use flotilla_state_machines::{Coded, EdgeEventOf, EdgePayload, Home, InputRead, Label, Machine, Resource, Version};
use hegel::{generators as gs, TestCase};

use super::{code_enum, list, name, timestamp, Pick};

/// The resource's name at a fixture's home: the worlds key the resource's own evidence (a
/// message's receipts, its reply) by it.
pub const NAME: &str = "r";

/// One edge to try, with the world that makes its gate pass and the time it is applied at.
pub type Candidate<M> = (<M as Machine>::Edge, Box<<M as Machine>::World>, Timestamp);

/// A generated resource: `spec` in `state`, with no records. Its name and version are the
/// store's, which a load gives back ([`Kind::with_identity`]).
pub fn resource<M: Machine>(spec: M::Spec, state: State<M::State>) -> Resource<M> {
    Resource::stored(String::new(), spec, state, Version(0))
}

/// The first of `candidates` (rotated by `pick`, so every candidate leads in turn) that the engine
/// applies to `resource`, as the stored edge persisting it and the resource it leads to; `None`
/// when the engine refuses every candidate. Each candidate runs at a home of its own holding only
/// the restored resource, written by a drawn writer.
pub fn drive<M>(
    tc: &TestCase,
    pick: Pick,
    resource: &Resource<M>,
    mut candidates: Vec<Candidate<M>>,
) -> Option<(Edge<Resource<M>>, Resource<M>)>
where
    M: Machine,
    M::Spec: KindSpec<State = M::State> + Clone,
    M::State: StateData + Labelled<Label = Label<M>>,
    EdgePayload<M::Edge>: Cbor,
{
    if candidates.is_empty() {
        return None;
    }
    let lead = pick.variant(tc, candidates.len(), |index| index);
    candidates.rotate_left(lead);
    let writer = name(tc);
    candidates.into_iter().find_map(|(edge, world, now)| step(resource, &writer, edge, &*world, now))
}

/// `edge` applied at `now` by a home writing as `writer` and holding only `resource` (as
/// [`NAME`]), as the stored edge persisting it and the resource it leads to; `None` when the
/// engine refuses it.
pub fn step<M>(
    resource: &Resource<M>,
    writer: &str,
    edge: impl Into<M::Edge>,
    world: &M::World,
    now: Timestamp,
) -> Option<(Edge<Resource<M>>, Resource<M>)>
where
    M: Machine,
    M::Spec: KindSpec<State = M::State> + Clone,
    M::State: StateData + Labelled<Label = Label<M>>,
    EdgePayload<M::Edge>: Cbor,
{
    let mut home = Home::<M>::new(writer).expect("the machine's declaration validates");
    let previous = resource.stored_state().clone();
    let version = resource.version().0;
    home.restore(resource.clone().with_identity(NAME, version)).unwrap_or_else(|_| panic!("an empty home takes the resource"));
    let event = home.apply(NAME, edge, world, now).ok()?.clone();
    let next = home.get(NAME).expect("restored").clone();
    let edge = next.stored_edge(&previous, &event).expect("an applied edge is storable");
    Some((edge, next))
}

/// A history event as a generator leaves it: any of `edges` between any labels. History is never
/// read to decide, so a stored log need not be a path through the machine.
pub fn event<M: Machine>(tc: &TestCase, pick: Pick, edges: &[M::Edge]) -> EdgeEventOf<M> {
    let edge = edges[pick.variant(tc, edges.len(), |index| index)].clone();
    let labels = Label::<M>::ALL;
    flotilla_schema::EdgeEvent {
        edge: flotilla_state_machines::Edge::kind(&edge),
        from: code_enum(tc, pick, labels),
        to: code_enum(tc, pick.nth(1), labels),
        writer: name(tc),
        at: timestamp(tc),
        payload: EdgePayload { edge, reads: reads(tc, pick) },
    }
}

/// Up to two recorded reads, decoded text and all.
pub fn reads(tc: &TestCase, pick: Pick) -> Vec<InputRead> {
    list(tc, pick, 2, |_| InputRead {
        kind: Cow::Owned(name(tc)),
        name: name(tc),
        version: tc.draw(gs::booleans()).then(|| Version(tc.draw(gs::integers::<u64>()))),
    })
}

/// A time at which the engine's arithmetic on `base` cannot overflow: `base` plus `by`, saturated.
pub fn after(base: Timestamp, by: i64) -> Timestamp {
    base.saturating_add(by)
}
