//! Vessel: a placement lease's lifecycle (spec `machines/vessel.qnt`, ADR 0064
//! "Shared lifecycle" and "Admission grants a cap, never a reservation").
//!
//! A vessel resource is created when [`placement`](crate::placement) grants
//! its lease, so its request is the [`Lease`]: the whole placement request and
//! the host it is leased on. Its machine is the shared
//! [`Lifecycle`]; this module names the kind and
//! declares its stored row.
//!
//! The stored kind is [`VesselRow`] (code 7). The vessel is homed at the host
//! that owns the capacity, while its convoy is homed where it was admitted, so
//! the row refers to the convoy by a `Ref` carrying the convoy's kind, name and
//! home (#3392 G3). The lease's shape is a child table, and the live vessels
//! of a host or of a convoy are indexed: the placement ledger is a resident
//! view over those rows (its leases), not a table of its own.
//!
//! The other lifecycle kinds (Environment, Checkout, Clone, ImageBuild,
//! TerminalSession) declare their rows beside the requests that production
//! crates own, when each kind moves to this store; the engine supplies the
//! state, edge-event and attempt types they store.

use std::collections::BTreeMap;

use flotilla_schema::{HistoryOf, Records, State, StateOf, Stored, Timestamp};

use super::{
    convoy::{ConvoyId, Priority, VesselClass},
    lifecycle::{Lifecycle, LifecycleEvent, LifecycleKind, LifecycleState},
};
use crate::{
    placement::{Dimension, Host, Lease, Request},
    Attempt, EdgeEventOf, MachineRow, Resource, RowParts,
};

/// Vessels as a lifecycle kind: the spec is the granted lease.
pub struct VesselKind;

impl LifecycleKind for VesselKind {
    const NAME: &'static str = "Vessel";
    type Request = Lease;
}

/// The Vessel machine: the shared lifecycle over a lease.
pub type Vessel = Lifecycle<VesselKind>;

/// The stored Vessel (kind code 7): the lease's request and host as columns,
/// its shape as a child table, the lifecycle state, the edge log and the
/// attempts. Indexed for a host's live vessels and for a convoy's vessels.
#[derive(Clone, Debug, PartialEq, Stored)]
#[store(
    kind = "Vessel",
    code = 7,
    index(name = "live_by_host", eq(host), when(state in [Requested, Realised])),
    index(name = "by_convoy", eq(convoy))
)]
pub struct VesselRow {
    /// The convoy whose quota the lease counts against, at its own home.
    pub convoy: ConvoyId,
    pub class: VesselClass,
    #[store(child)]
    pub shape: BTreeMap<Dimension, u32>,
    pub priority: Priority,
    pub host: Host,
    #[store(state)]
    pub state: State<LifecycleState>,
    #[store(history)]
    pub edges: Records<LifecycleEvent>,
    #[store(history)]
    pub attempts: Records<Attempt>,
}

impl VesselRow {
    /// The lease the vessel was created with: its immutable spec.
    pub fn lease(&self) -> Lease {
        let request = Request { convoy: self.convoy.clone(), class: self.class, shape: self.shape.clone(), priority: self.priority };
        Lease { request, host: self.host }
    }
}

impl MachineRow for VesselRow {
    type Machine = Vessel;
    const STATE_FIELD: StateOf<Self, State<LifecycleState>> = VesselRow::STATE;
    const EDGE_LOG: HistoryOf<Self, EdgeEventOf<Vessel>> = VesselRow::EDGES;

    fn of(resource: &Resource<Vessel>, since: Timestamp) -> VesselRow {
        let Lease { request: Request { convoy, class, shape, priority }, host } = resource.spec().clone();
        VesselRow {
            convoy,
            class,
            shape,
            priority,
            host,
            state: State::new(resource.state().clone(), since),
            edges: resource.events().clone(),
            attempts: resource.attempts().clone(),
        }
    }

    fn state(&self) -> &State<LifecycleState> {
        &self.state
    }

    fn parts(&self) -> RowParts<Vessel> {
        RowParts {
            spec: self.lease(),
            state: self.state.value.clone(),
            events: self.edges.clone(),
            attempts: self.attempts.clone(),
            receipts: Records::new(),
        }
    }
}
