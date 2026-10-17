//! The stored kind of every machine: [`Resource<M>`] itself (ADR 0064 Storage,
//! #3373 rulings 1 to 5, #3392 ruling D5). There is no separate row type.
//!
//! The storage derive targets a machine's spec, which carries the kind's name,
//! code, indexes and interning and names its state
//! (`#[store(kind = "Message", code = "ms", state = MessageState)]`), and its
//! state. The holder supplies the rest: its root row is the spec's columns,
//! then the state as the field `state` (one label column, `state_since` set
//! only when the label changes, and the label's data); the store keeps the
//! name (the identity row) and the version. Edge events, attempts and receipts
//! among them (R4), are no part of the row or the resource (#3373 ruling R1):
//! each is the header and payload of its write's entry in the store's
//! per-home log (#3392 G8), read in pages and decoded by
//! [`edge_event`](crate::edge_event), and the kind declares its machine's edge
//! codes, the only ones its entries may carry. [`Resource::stored_edge`] turns
//! an edge the home applied into the stored edge that persists it, and the
//! version `apply_edge` returns is the one the home adopts
//! ([`Log`](crate::Log), R2). Nothing here calls a store: the storage port, a
//! `Log` with the home deciding and `apply_edge` persisting in one
//! transaction, is a later layer.

use flotilla_schema::{
    adapt::{Adapter, AsColumns},
    Cbor, CodeEnum, ColumnOf, DecodeError, Decoder, Edge, EdgeError, Element, EncodeError, Encoder, Identity, Kind, KindCode, KindRegistry,
    KindSpec, Labelled, Layout, RegistryError, State, StateData, StateOf, TableSet,
};

use crate::{
    machines::{agent_session::AgentSession, convoy::Convoy, ensure::Ensure, message::Message, vessel::Vessel},
    EdgeEventOf, EdgeKind, EdgePayload, Label, Machine, Resource, Version,
};

/// The production kind registry (#3373 ruling 6): every stored kind the engine
/// declares, each under the code it holds for good. Codes are stored with every
/// identity and change, so a production code is allocated once, here, and never
/// reused or moved; a kind declared elsewhere (a lifecycle kind's request beside
/// its owner) registers into this registry under a new code. Test kinds are
/// registered beside these, so the registry refuses any test kind that takes a
/// production code.
pub fn kinds() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    registry.register::<Resource<Message>>()?;
    registry.register::<Resource<AgentSession>>()?;
    registry.register::<Resource<Convoy>>()?;
    registry.register::<Resource<Vessel>>()?;
    registry.register::<Resource<Ensure>>()?;
    Ok(registry)
}

/// Machine `M`'s state as stored: the value and the time its label was entered.
pub type StoredState<M> = State<<M as Machine>::State>;

impl<M> Element for Resource<M>
where
    M: Machine,
    M::Spec: KindSpec<State = M::State>,
    M::State: StateData + Labelled<Label = Label<M>>,
    EdgePayload<M::Edge>: Cbor,
{
    const TABLES: usize = <M::Spec as Element>::TABLES + <AsColumns as Adapter<StoredState<M>>>::TABLES;

    fn layout_row(layout: &mut Layout) {
        <M::Spec as Element>::layout_row(layout);
        <AsColumns as Adapter<StoredState<M>>>::layout(layout, "state", true);
    }

    fn encode_row(&self, e: &mut Encoder) -> Result<(), EncodeError> {
        self.spec.encode_row(e)?;
        <AsColumns as Adapter<StoredState<M>>>::encode(&self.state, e).map_err(|error| error.at("state"))
    }

    /// The name and version are the store's ([`Kind::with_identity`]): a
    /// resource decoded from rows alone has an empty name and version 0.
    fn decode_row(d: &mut Decoder) -> Result<Self, DecodeError> {
        let spec = <M::Spec as Element>::decode_row(d)?;
        let state = <AsColumns as Adapter<StoredState<M>>>::decode(d)
            .map_err(|error| error.at("state"))?
            .ok_or_else(|| DecodeError::missing("state"))?;
        Ok(Resource { name: String::new(), spec, state, version: Version(0) })
    }
}

impl<M> Kind for Resource<M>
where
    M: Machine,
    M::Spec: KindSpec<State = M::State>,
    M::State: StateData + Labelled<Label = Label<M>>,
    EdgePayload<M::Edge>: Cbor,
{
    const KIND: &'static str = <M::Spec as KindSpec>::KIND;
    const CODE: KindCode = <M::Spec as KindSpec>::CODE;

    fn table_set() -> TableSet {
        let mut layout = Layout::root(Self::KIND, Self::CODE);
        Self::layout_row(&mut layout);
        <M::Spec as KindSpec>::layout_indexes(&mut layout);
        layout.edges(<EdgeKind<M> as CodeEnum>::CODES, <EdgeKind<M> as CodeEnum>::NAMES);
        layout.finish()
    }

    fn with_identity(self, name: &str, version: u64) -> Self {
        Resource { name: name.to_owned(), version: Version(version), ..self }
    }
}

impl<M> Resource<M>
where
    M: Machine,
    M::Spec: KindSpec<State = M::State>,
    M::State: StateData + Labelled<Label = Label<M>>,
    EdgePayload<M::Edge>: Cbor,
{
    /// Edge handle of the state field `state`.
    pub const STATE: StateOf<Self, StoredState<M>> = StateOf::new(
        ColumnOf::new("state", <AsColumns as Adapter<StoredState<M>>>::TABLES, <AsColumns as Adapter<StoredState<M>>>::encode),
        M::State::DATA,
    );
    /// The stored edge for `event`, which the home appended when it moved this
    /// resource from `previous` to its current state: the state as one unit
    /// (its label, its since, which moves only when the label changes, and
    /// every datum), each collection datum row by row, logged as the event.
    ///
    /// An edge whose declaration names a master ([`EdgeDef::about`]) is stored
    /// with [`Resource::stored_edge_about`], which also logs the master.
    ///
    /// [`EdgeDef::about`]: crate::EdgeDef::about
    pub fn stored_edge(&self, previous: &StoredState<M>, event: &EdgeEventOf<M>) -> Result<Edge<Self>, EdgeError> {
        Edge::transition(Self::STATE, previous, &self.state, event)
    }

    /// [`Resource::stored_edge`], with the edge's master as its log entry's
    /// `about` (#3392 G11) when its declaration names one: `master` gives the
    /// identity (kind, name, home) of the master the edge names, such as the
    /// message a session's receipt is for, so the master's gates read the
    /// entries about it. The engine names a master by its name alone; the
    /// writer knows its kind and home.
    pub fn stored_edge_about(
        &self,
        previous: &StoredState<M>,
        event: &EdgeEventOf<M>,
        master: impl FnOnce(&str) -> Identity,
    ) -> Result<Edge<Self>, EdgeError> {
        let edge = self.stored_edge(previous, event)?;
        Ok(match M::definition().master(&event.payload.edge) {
            Some(name) => edge.about(master(&name)),
            None => edge,
        })
    }
}
