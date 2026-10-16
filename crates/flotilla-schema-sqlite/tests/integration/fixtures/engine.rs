//! Driving the engine for the engine kinds' fixtures: an edge on a stored row is one the engine
//! applied, through `Home`, to the resource the row holds.

use std::borrow::Cow;

use flotilla_schema::{Cbor, Edge, Labelled, StateData, Timestamp};
use flotilla_state_machines::{Coded, EdgeEventOf, EdgePayload, Home, InputRead, Label, Machine, MachineRow, Version};
use hegel::{generators as gs, TestCase};

use super::{code_enum, list, name, timestamp, Pick};

/// The resource's name at a fixture's home: the worlds key the resource's own evidence (a
/// message's receipts, its reply) by it.
pub const NAME: &str = "r";

/// One edge to try, with the world that makes its gate pass and the time it is applied at.
pub type Candidate<M> = (<M as Machine>::Edge, Box<<M as Machine>::World>, Timestamp);

/// The first of `candidates` (rotated by `pick`, so every candidate leads in turn) that the engine
/// applies to `row`'s resource, as the stored edge persisting it and the row it leads to; `None`
/// when the engine refuses every candidate. Each candidate runs at a home of its own holding only
/// the restored row, written by a drawn writer.
pub fn drive<R>(tc: &TestCase, pick: Pick, row: &R, mut candidates: Vec<Candidate<R::Machine>>) -> Option<(Edge<R>, R)>
where
    R: MachineRow,
    <R::Machine as Machine>::State: StateData + Labelled<Label = Label<R::Machine>>,
    EdgePayload<<R::Machine as Machine>::Edge>: Cbor,
{
    if candidates.is_empty() {
        return None;
    }
    let lead = pick.variant(tc, candidates.len(), |index| index);
    candidates.rotate_left(lead);
    let writer = name(tc);
    candidates.into_iter().find_map(|(edge, world, now)| step(row, &writer, edge, &*world, now))
}

/// `edge` applied at `now` by a home writing as `writer` and holding only `row`'s resource, as the
/// stored edge persisting it and the row it leads to; `None` when the engine refuses it.
pub fn step<R>(
    row: &R,
    writer: &str,
    edge: impl Into<<R::Machine as Machine>::Edge>,
    world: &<R::Machine as Machine>::World,
    now: Timestamp,
) -> Option<(Edge<R>, R)>
where
    R: MachineRow,
    <R::Machine as Machine>::State: StateData + Labelled<Label = Label<R::Machine>>,
    EdgePayload<<R::Machine as Machine>::Edge>: Cbor,
{
    let mut home = Home::<R::Machine>::new(writer).expect("the machine's declaration validates");
    home.restore(NAME, Version(1), row.parts()).unwrap_or_else(|_| panic!("an empty home takes the row"));
    let event = home.apply(NAME, edge, world, now).ok()?.clone();
    let resource = home.get(NAME).expect("restored");
    let (edge, state) = row.edge(&event, resource.state().clone()).expect("an applied edge is storable");
    let next = R::of(resource, state.since);
    assert_eq!(next.state(), &state, "the row of the moved resource holds the edge's state");
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
