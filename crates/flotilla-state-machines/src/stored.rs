//! The stored kind of every machine: [`Resource<M>`] itself (ADR 0064 Storage,
//! #3373 rulings 1 to 5, #3392 ruling D5). There is no separate row type.
//!
//! The storage derive targets a machine's spec, which carries the kind's name,
//! code, indexes and interning and names its state
//! (`#[store(kind = "Message", code = "ms", state = MessageState)]`), and its
//! state. The holder supplies the rest: its root row is the spec's columns,
//! then the state as the field `state` (one label column, `state_since` set
//! only when the label changes, and the label's data), then the edge log
//! `edges` as `history`, attempts and receipts among its edges (R4); the
//! store keeps the name (the identity row) and the version.
//! [`Resource::stored_edge`] turns an edge the home applied into the stored edge that
//! persists it. Nothing here calls a store: the storage port, with the home
//! deciding and `apply_edge` persisting in one transaction, is a later layer.

use flotilla_schema::{
    adapt::{Adapter, AsColumns, AsHistory},
    Cbor, ColumnOf, DecodeError, Decoder, Edge, EdgeError, Element, EncodeError, Encoder, HistoryOf, Kind, KindCode, KindSpec, Labelled,
    Layout, Records, State, StateData, StateOf, TableSet,
};

use crate::{EdgeEventOf, EdgePayload, Label, Machine, Resource, Version};

/// Machine `M`'s state as stored: the value and the time its label was entered.
pub type StoredState<M> = State<<M as Machine>::State>;

impl<M> Element for Resource<M>
where
    M: Machine,
    M::Spec: KindSpec<State = M::State>,
    M::State: StateData + Labelled<Label = Label<M>>,
    EdgePayload<M::Edge>: Cbor,
{
    const TABLES: usize = <M::Spec as Element>::TABLES
        + <AsColumns as Adapter<StoredState<M>>>::TABLES
        + <AsHistory as Adapter<Records<EdgeEventOf<M>>>>::TABLES;

    fn layout_row(layout: &mut Layout) {
        <M::Spec as Element>::layout_row(layout);
        <AsColumns as Adapter<StoredState<M>>>::layout(layout, "state", true);
        <AsHistory as Adapter<Records<EdgeEventOf<M>>>>::layout(layout, "edges", true);
    }

    fn encode_row(&self, e: &mut Encoder) -> Result<(), EncodeError> {
        self.spec.encode_row(e)?;
        <AsColumns as Adapter<StoredState<M>>>::encode(&self.state, e).map_err(|error| error.at("state"))?;
        <AsHistory as Adapter<Records<EdgeEventOf<M>>>>::encode(&self.events, e).map_err(|error| error.at("edges"))
    }

    /// The name and version are the store's ([`Kind::with_identity`]): a
    /// resource decoded from rows alone has an empty name and version 0.
    fn decode_row(d: &mut Decoder) -> Result<Self, DecodeError> {
        let spec = <M::Spec as Element>::decode_row(d)?;
        let state = <AsColumns as Adapter<StoredState<M>>>::decode(d)
            .map_err(|error| error.at("state"))?
            .ok_or_else(|| DecodeError::missing("state"))?;
        let events = <AsHistory as Adapter<Records<EdgeEventOf<M>>>>::decode(d)
            .map_err(|error| error.at("edges"))?
            .ok_or_else(|| DecodeError::missing("edges"))?;
        Ok(Resource { name: String::new(), spec, state, version: Version(0), events })
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
    /// Edge handle of the edge log `edges`.
    pub const EDGES: HistoryOf<Self, EdgeEventOf<M>> = HistoryOf::new("edges");

    /// The stored edge for `event`, which the home appended when it moved this
    /// resource from `previous` to its current state: the label and its since
    /// only when the label changes, each datum only where it differs, and the
    /// event appended to the edge log.
    pub fn stored_edge(&self, previous: &StoredState<M>, event: &EdgeEventOf<M>) -> Result<Edge<Self>, EdgeError> {
        Edge::transition(Self::STATE, previous, &self.state, Self::EDGES, event)
    }
}
