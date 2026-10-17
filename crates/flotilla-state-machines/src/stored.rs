//! A machine's stored row: the kind the storage derive declares beside each
//! machine (ADR 0064 Storage, #3373 rulings 1 to 5).
//!
//! Each machine's row holds the spec's fields as columns and child tables, the
//! state under `#[store(state)]` (one label column, `<state>_since` set only
//! when the label changes, and the label's data), and the edge log and any
//! attempts or receipts as `history`. [`MachineRow`] is the smallest
//! conversion between a [`Resource`] and its row: [`MachineRow::of`] writes a
//! resource's row, [`MachineRow::parts`] reads one back for
//! [`Home::restore`], and [`MachineRow::edge`] turns an edge the home applied
//! into the stored edge that persists it. Nothing here calls a store: the
//! storage port, with the home deciding and `apply_edge` persisting in one
//! transaction, is a later layer.

use flotilla_schema::{
    Cbor, Edge, EdgeError, HistoryOf, Kind, KindRegistry, Labelled, RegistryError, State, StateData, StateOf, Timestamp,
};

use crate::{
    machines::{agent_session::AgentSessionRow, convoy::ConvoyRow, ensure::EnsureRow, message::MessageRow, vessel::VesselRow},
    EdgeEventOf, EdgePayload, Label, Machine, Resource, RowParts,
};

/// The production kind registry (#3373 ruling 6): every stored kind the engine
/// declares, each under the code it holds for good. Codes are stored with every
/// identity and change, so a production code is allocated once, here, and never
/// reused or moved; a kind declared elsewhere (a lifecycle kind's row beside
/// its request) registers into this registry under a new code. Test kinds are
/// registered beside these, so the registry refuses any test kind that takes a
/// production code.
pub fn kinds() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    registry.register::<MessageRow>()?;
    registry.register::<AgentSessionRow>()?;
    registry.register::<ConvoyRow>()?;
    registry.register::<VesselRow>()?;
    registry.register::<EnsureRow>()?;
    Ok(registry)
}

/// Machine `M`'s state as stored: the value and the time its label was entered.
pub type StoredState<M> = State<<M as Machine>::State>;

/// A machine's stored row. The engine's kinds implement it beside their
/// machines; a lifecycle kind's row is declared by whoever owns its request.
pub trait MachineRow: Kind + Clone {
    type Machine: Machine;
    /// The row's `#[store(state)]` field.
    const STATE_FIELD: StateOf<Self, StoredState<Self::Machine>>;
    /// The row's edge log, a `#[store(history)]` field.
    const EDGE_LOG: HistoryOf<Self, EdgeEventOf<Self::Machine>>;

    /// The resource's row, its label entered at `since`. The engine keeps no
    /// such time: the writer that persists the resource supplies it.
    fn of(resource: &Resource<Self::Machine>, since: Timestamp) -> Self;

    /// The stored state: the value and the time its label was entered.
    fn state(&self) -> &StoredState<Self::Machine>;

    /// What [`Home::restore`](crate::Home::restore) needs to place this row's
    /// resource at a home. A row without attempts or receipts gives empty ones.
    fn parts(&self) -> RowParts<Self::Machine>;

    /// The stored edge for `event`, which the home appended when it moved the
    /// resource from this row's state to `next`: the label and its since only
    /// when the label changes (since is then the event's time), each datum
    /// only where it differs, and the event appended to the edge log. Also
    /// returns the stored state the edge writes.
    fn edge(
        &self,
        event: &EdgeEventOf<Self::Machine>,
        next: <Self::Machine as Machine>::State,
    ) -> Result<(Edge<Self>, StoredState<Self::Machine>), EdgeError>
    where
        <Self::Machine as Machine>::State: StateData + Labelled<Label = Label<Self::Machine>>,
        EdgePayload<<Self::Machine as Machine>::Edge>: Cbor,
    {
        let previous = self.state();
        let since = if event.from == event.to { previous.since } else { event.at };
        let next = State::new(next, since);
        let edge = Edge::transition(Self::STATE_FIELD, previous, &next, Self::EDGE_LOG, event)?;
        Ok((edge, next))
    }
}
