//! Vessel: a placement lease's lifecycle (spec `machines/vessel.qnt`, ADR 0064
//! "Shared lifecycle" and "Admission grants a cap, never a reservation").
//!
//! A vessel resource is created when [`placement`](crate::placement) grants
//! its lease, so its spec is the lease ([`VesselSpec`]): the whole placement
//! request and the host it is leased on. Its machine is the shared
//! [`Lifecycle`]; this module names the kind and declares its spec.
//!
//! The stored kind is `Resource<Vessel>` (code `ve`, #3392 ruling D5). The
//! vessel is homed at the host that owns the capacity, while its convoy is
//! homed where it was admitted, so the spec refers to the convoy by a `Ref`
//! carrying the convoy's kind, name and home (#3392 G3). The lease's shape is
//! a child table, and the live vessels of a host and the vessels of a convoy
//! are indexed. The host ledger is a view over the live vessels' leases
//! ([`live_leases`], [`Ledger::restore`](crate::placement::Ledger::restore)),
//! not a table of its own, so a restart loses no lease.
//!
//! The other lifecycle kinds (Environment, Checkout, Clone, ImageBuild,
//! TerminalSession) declare their specs beside the requests that production
//! crates own, when each kind moves to this store; the engine supplies the
//! state, edge-event and attempt types they store.

use std::collections::BTreeMap;

use flotilla_schema::Stored;

use super::{
    convoy::{ConvoyId, Priority, VesselClass},
    lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
};
use crate::{
    placement::{Dimension, Host, Lease, Request, VesselId},
    Resource,
};

/// Vessels as a lifecycle kind: the spec is the granted lease.
pub struct VesselKind;

impl LifecycleKind for VesselKind {
    const NAME: &'static str = "Vessel";
    type Request = VesselSpec;
}

/// The Vessel machine: the shared lifecycle over a lease.
pub type Vessel = Lifecycle<VesselKind>;

/// A granted lease as the vessel's spec, which declares the stored Vessel
/// (kind code `ve`): the request's convoy, class and priority and the host as
/// columns, and its shape as a child table. Indexed for a host's live vessels
/// (the ledger's view) and for a convoy's vessels.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "Vessel",
    code = "ve",
    state = LifecycleState,
    index(name = "live_by_host", eq(host), when(state in [Requested, Realised])),
    index(name = "by_convoy", eq(convoy))
)]
pub struct VesselSpec {
    /// The convoy whose quota the lease counts against, at its own home.
    pub convoy: ConvoyId,
    pub class: VesselClass,
    #[store(child)]
    pub shape: BTreeMap<Dimension, u32>,
    pub priority: Priority,
    /// The placement host: a capacity-ledger id, not a stored resource, so
    /// a plain column rather than a `Ref`.
    pub host: Host,
}

impl VesselSpec {
    /// The lease the vessel was created with.
    pub fn lease(&self) -> Lease {
        let request = Request { convoy: self.convoy.clone(), class: self.class, shape: self.shape.clone(), priority: self.priority };
        Lease { request, host: self.host }
    }
}

impl From<Lease> for VesselSpec {
    fn from(lease: Lease) -> VesselSpec {
        let Lease { request: Request { convoy, class, shape, priority }, host } = lease;
        VesselSpec { convoy, class, shape, priority, host }
    }
}

/// The host ledger's view: each live vessel among `vessels` (Requested or
/// Realised) with its lease, by the vessel's name. A Gone or GaveUp vessel
/// holds no lease.
pub fn live_leases<'a>(vessels: impl IntoIterator<Item = &'a Resource<Vessel>> + 'a) -> impl Iterator<Item = (VesselId, Lease)> + 'a {
    vessels.into_iter().filter(|vessel| vessel.state().live()).map(|vessel| (VesselId::new(vessel.name()), vessel.spec().lease()))
}
