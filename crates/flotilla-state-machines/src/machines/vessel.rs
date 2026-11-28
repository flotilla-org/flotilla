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

use flotilla_schema::{Described, Stored};

use super::{
    convoy::{ConvoyId, Priority, VesselClass},
    lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
};
use crate::{
    placement::{Dimension, Host, Lease, Request, VesselId},
    AttemptPolicy, Resource,
};

/// Vessels as a lifecycle kind: the spec is the granted lease.
pub struct VesselKind;

impl LifecycleKind for VesselKind {
    const NAME: &'static str = "Vessel";
    type Request = VesselSpec;
    // A vessel is no tree level: what runs on it reaches it by key for these badges.
    const DESCRIBED: &'static [Described] =
        &[Described::Role { column: "host", role: "badge" }, Described::Role { column: "class", role: "badge" }];
    fn launch(request: &VesselSpec) -> Option<(&str, u64)> {
        Some((&request.launch_of, request.launch))
    }
    fn numbered(request: VesselSpec, launch: u64) -> VesselSpec {
        VesselSpec { launch, ..request }
    }
    /// A vessel's home tries to make it ready under the local controllers' retry limit and
    /// backoff (`flotilla_resources::controller_retry::LOCAL_RETRY_*`), not unbounded.
    fn policy() -> AttemptPolicy {
        VESSEL_POLICY
    }
}

/// [`VesselKind::policy`]: 10 failed tries, 2 s after the first, doubling to 5 min.
pub const VESSEL_POLICY: AttemptPolicy = AttemptPolicy { max_failures: 10, base: 2 * SECOND, factor: 2, cap: 300 * SECOND };

const SECOND: i64 = 1_000_000;

/// The Vessel machine: the shared lifecycle over a lease.
pub type Vessel = Lifecycle<VesselKind>;

/// A granted lease as the vessel's spec, which declares the stored Vessel
/// (kind code `ve`): the launch it is, the request's convoy, class and
/// priority and the host as columns, and its shape as a child table. Indexed
/// for a host's live vessels (the ledger's view) and for a convoy's vessels by
/// work, in launch order.
///
/// Each vessel is a launch named `<convoy>-<work>#<n>` (owner's ruling, #3108):
/// a vessel lost to its backing is replaced by the next launch of its work in
/// the same convoy, and an ended launch is never revived. A convoy's name is
/// unique only at its own home, so its vessels are found by the convoy's
/// identity and the launch name together (`by_launch`).
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "Vessel",
    code = "ve",
    state = LifecycleState,
    index(name = "live_by_host", eq(host), when(state in [Requested, Realised])),
    // A convoy's vessels, a work's launches together in launch order: the newest is the last.
    index(name = "by_launch", eq(convoy, launch_of), order(launch))
)]
pub struct VesselSpec {
    /// The name this is a launch of, `<convoy>-<work>`, interned (domain `ln`).
    #[store(intern = "ln")]
    pub launch_of: String,
    /// The home sequence the launch's create took; 0 until its log numbers it.
    pub launch: u64,
    /// The convoy the lease serves, at its own home.
    pub convoy: ConvoyId,
    pub class: VesselClass,
    #[store(child)]
    pub shape: BTreeMap<Dimension, u32>,
    pub priority: Priority,
    /// The placement host: the host's id as the fleet names it, interned
    /// text (`hs`). It is not a stored resource, so a plain column rather
    /// than a `Ref`.
    pub host: Host,
}

impl VesselSpec {
    /// The lease the vessel was created with.
    pub fn lease(&self) -> Lease {
        let request =
            Request { convoy: self.convoy.clone(), class: self.class.clone(), shape: self.shape.clone(), priority: self.priority };
        Lease { request, host: self.host.clone() }
    }
}

impl VesselSpec {
    /// The spec of a launch of `launch_of` granted `lease`, numbered when its create is logged.
    pub fn of(launch_of: impl Into<String>, lease: Lease) -> VesselSpec {
        let Lease { request: Request { convoy, class, shape, priority }, host } = lease;
        VesselSpec { launch_of: launch_of.into(), launch: 0, convoy, class, shape, priority, host }
    }
}

/// The host ledger's view: each live vessel among `vessels` (Requested or
/// Realised) with its lease, by the vessel's name. A Gone or GaveUp vessel
/// holds no lease.
pub fn live_leases<'a>(vessels: impl IntoIterator<Item = &'a Resource<Vessel>> + 'a) -> impl Iterator<Item = (VesselId, Lease)> + 'a {
    vessels.into_iter().filter(|vessel| vessel.state().live()).map(|vessel| (VesselId::new(vessel.name()), vessel.spec().lease()))
}
