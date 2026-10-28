//! Driving the engine for the engine kinds' fixtures: an edge on a stored resource is one the
//! engine applied, through `Home`, to that resource.

use flotilla_schema::{Cbor, Edge, Identity, Kind, KindSpec, Labelled, State, StateData, Timestamp};
use flotilla_state_machines::{
    machines::message::Message, EdgeEventOf, EdgePayload, Home, InputRead, Label, Log, LogError, Machine, Resource, StoredState,
    StoredWrite, Version,
};
use hegel::TestCase;

use super::{name, Pick, LOCAL};

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
    M::Write: StoredWrite<Resource<M>>,
    EdgePayload<M::Edge>: Cbor,
    M::Edge: Cbor,
{
    if candidates.is_empty() {
        return None;
    }
    let lead = pick.variant(tc, candidates.len(), |index| index);
    candidates.rotate_left(lead);
    let writer = name(tc);
    candidates.into_iter().find_map(|(edge, world, now)| step(resource, &writer, edge, &*world, now))
}

/// A log that keeps the stored edge persisting the last edge it took, with its records writes.
struct Capture<M: Machine> {
    edge: Option<Edge<Resource<M>>>,
}

impl<M> Log<M> for Capture<M>
where
    M: Machine,
    M::Spec: KindSpec<State = M::State>,
    M::State: StateData + Labelled<Label = Label<M>>,
    M::Write: StoredWrite<Resource<M>>,
    EdgePayload<M::Edge>: Cbor,
    M::Edge: Cbor,
{
    fn create(&mut self, _: &str, _: Timestamp, _: &Resource<M>, _: &[InputRead]) -> Result<Version, LogError> {
        Ok(Version(1))
    }

    fn edge(&mut self, writer: &str, previous: &StoredState<M>, next: &Resource<M>, event: &EdgeEventOf<M>) -> Result<Version, LogError> {
        self.edge_with_writes(writer, previous, next, event, &[])
    }

    fn edge_with_writes(
        &mut self,
        _: &str,
        previous: &StoredState<M>,
        next: &Resource<M>,
        event: &EdgeEventOf<M>,
        writes: &[M::Write],
    ) -> Result<Version, LogError> {
        // Every master an engine edge names in these fixtures is a message at this home.
        let master = |name: &str| Identity { kind: Resource::<Message>::CODE, name: name.to_owned(), home: LOCAL.to_owned() };
        let edge = next.stored_edge_about(previous, event, master).expect("an applied edge is storable");
        let edge = writes.iter().try_fold(edge, |edge, write| write.store(edge)).expect("a records write is storable");
        self.edge = Some(edge);
        // The store's next write takes its change's sequence, then its entry's (#4024).
        Ok(Version(next.version().0 + 2))
    }
}

/// `edge` applied at `now` by a home writing as `writer` and holding only `resource` (as
/// [`NAME`]), as the stored edge persisting it (its records writes included) and the resource it
/// leads to; `None` when the engine refuses it.
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
    M::Write: StoredWrite<Resource<M>>,
    EdgePayload<M::Edge>: Cbor,
    M::Edge: Cbor,
{
    let mut home = Home::<M, Capture<M>>::with_log(writer, Capture { edge: None }).expect("the machine's declaration validates");
    let version = resource.version().0;
    home.restore(resource.clone().with_identity(NAME, version)).unwrap_or_else(|_| panic!("an empty home takes the resource"));
    home.apply(NAME, edge, world, now).ok()?;
    let next = home.get(NAME).expect("restored").clone();
    let edge = home.log_mut().edge.take().expect("the log took the edge");
    Some((edge, next))
}

/// A time at which the engine's arithmetic on `base` cannot overflow: `base` plus `by`, saturated.
pub fn after(base: Timestamp, by: i64) -> Timestamp {
    base.saturating_add(by)
}
