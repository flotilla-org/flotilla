//! Placement: admission grants a cap, never a reservation (spec
//! `machines/vessel.qnt`, scenario `admission_placement.qnt`, ADR 0064).
//!
//! [`place`] is the spec's `Place` rule over one placement snapshot: the
//! convoy holds a cap for the class, its live vessels are under it, the whole
//! request fits the host's available capacity in every dimension, and no
//! waiting request of higher priority fits there (even one whose own convoy is
//! capped). A lease holds its shape on one host until the vessel is Gone.
//! Nothing is pooled across hosts or shapes, and there is no borrowing.
//!
//! [`Ledger`] is a pure, in-memory host ledger that computes the snapshot and
//! holds the live leases. Releasing a lease removes it: the ledger keeps no
//! history. Every failure is an explicit error; nothing panics.

use std::collections::BTreeMap;

use flotilla_schema::Stored;

use crate::machines::convoy::{ConvoyId, Priority, Quota, VesselClass};

/// A host that capacity is leased on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Stored)]
pub struct Host(pub u32);

/// One capacity dimension, such as cores or memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Stored)]
pub struct Dimension(pub u32);

/// Units by capacity dimension: a request's shape, or a host's capacity.
pub type Units = BTreeMap<Dimension, u32>;

/// A vessel at the ledger: the vessel resource's name at its home.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VesselId(pub String);

impl VesselId {
    pub fn new(name: impl Into<String>) -> Self {
        VesselId(name.into())
    }
}

/// One vessel's placement request: a whole multidimensional shape.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Request {
    pub convoy: ConvoyId,
    pub class: VesselClass,
    /// Units needed, by capacity dimension.
    pub shape: Units,
    /// The convoy's priority.
    pub priority: Priority,
}

/// What a placement on `host` reads: local, versioned observations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evidence {
    pub host: Host,
    /// The host's unleased capacity, by dimension.
    pub available: Units,
    /// The other requests waiting for placement.
    pub waiting: Vec<Request>,
    /// Leased vessels of the request's convoy and class.
    pub live: u32,
    /// The convoy's cap for the class.
    pub cap: u32,
    /// Whether the convoy is admitted for the class.
    pub admitted: bool,
}

/// A granted placement: the whole request, leased on one host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub request: Request,
    pub host: Host,
}

/// The spec's vessel `Refusal`. [`place`] produces the middle four; the
/// lifecycle half of a vessel produces `WrongState` and `MissingEvidence`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlacementRefusal {
    WrongState,
    NotAdmitted,
    QuotaExceeded,
    DoesNotFit,
    HigherPriorityWaiting,
    MissingEvidence,
}

/// Whether every dimension of the request's shape is present in `available`
/// and no larger than it.
pub fn fits(request: &Request, available: &Units) -> bool {
    request.shape.iter().all(|(dimension, units)| available.get(dimension).is_some_and(|free| units <= free))
}

/// The spec's `Place` rule, checked in its order.
pub fn place(request: &Request, evidence: &Evidence) -> Result<Lease, PlacementRefusal> {
    if !evidence.admitted {
        return Err(PlacementRefusal::NotAdmitted);
    }
    if evidence.live >= evidence.cap {
        return Err(PlacementRefusal::QuotaExceeded);
    }
    if !fits(request, &evidence.available) {
        return Err(PlacementRefusal::DoesNotFit);
    }
    if evidence.waiting.iter().any(|waiting| waiting.priority > request.priority && fits(waiting, &evidence.available)) {
        return Err(PlacementRefusal::HigherPriorityWaiting);
    }
    Ok(Lease { request: request.clone(), host: evidence.host })
}

/// One vessel's placement state at the ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Vessel {
    Waiting(Request),
    Leased(Lease),
}

/// Why a ledger operation did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerError {
    UnknownVessel(VesselId),
    UnknownHost(Host),
    DuplicateHost(Host),
    /// A lease uses a dimension the host has no capacity in.
    UnknownDimension {
        host: Host,
        dimension: Dimension,
    },
    DuplicateRequest(VesselId),
    NotWaiting(VesselId),
    NotLeased(VesselId),
    /// Leases on the host exceed its capacity in the dimension.
    CapacityUnderflow {
        host: Host,
        dimension: Dimension,
    },
    Refused(PlacementRefusal),
}

/// A pure, in-memory placement ledger: host capacities, convoy caps and vessels.
#[derive(Clone, Debug, Default)]
pub struct Ledger {
    hosts: BTreeMap<Host, Units>,
    caps: BTreeMap<ConvoyId, Quota>,
    vessels: BTreeMap<VesselId, Vessel>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a host with its capacity by dimension.
    pub fn add_host(&mut self, host: Host, capacity: Units) -> Result<(), LedgerError> {
        if self.hosts.contains_key(&host) {
            return Err(LedgerError::DuplicateHost(host));
        }
        self.hosts.insert(host, capacity);
        Ok(())
    }

    /// Sets a convoy's quota, as admitted or readmitted. A class with no
    /// positive cap is not admitted.
    pub fn set_caps(&mut self, convoy: ConvoyId, quota: Quota) {
        self.caps.insert(convoy, quota);
    }

    /// Queues a vessel's request.
    pub fn request(&mut self, id: VesselId, request: Request) -> Result<(), LedgerError> {
        if self.vessels.contains_key(&id) {
            return Err(LedgerError::DuplicateRequest(id));
        }
        self.vessels.insert(id, Vessel::Waiting(request));
        Ok(())
    }

    pub fn vessel(&self, id: &VesselId) -> Option<&Vessel> {
        self.vessels.get(id)
    }

    /// The host's capacity less the shapes leased on it.
    pub fn available(&self, host: Host) -> Result<Units, LedgerError> {
        let mut available = self.hosts.get(&host).ok_or(LedgerError::UnknownHost(host))?.clone();
        for lease in self.leases().map(|(_, lease)| lease).filter(|lease| lease.host == host) {
            for (dimension, units) in &lease.request.shape {
                let free = available.get_mut(dimension).ok_or(LedgerError::UnknownDimension { host, dimension: *dimension })?;
                *free = free.checked_sub(*units).ok_or(LedgerError::CapacityUnderflow { host, dimension: *dimension })?;
            }
        }
        Ok(available)
    }

    /// The live leases, by vessel.
    pub fn leases(&self) -> impl Iterator<Item = (&VesselId, &Lease)> {
        self.vessels.iter().filter_map(|(id, vessel)| match vessel {
            Vessel::Leased(lease) => Some((id, lease)),
            Vessel::Waiting(_) => None,
        })
    }

    /// The convoy's cap for a class; zero when it holds none.
    pub fn cap(&self, convoy: &ConvoyId, class: VesselClass) -> u32 {
        self.caps.get(convoy).and_then(|quota| quota.get(&class)).copied().unwrap_or(0)
    }

    /// Leased vessels of the convoy and class, on any host.
    pub fn live(&self, convoy: &ConvoyId, class: VesselClass) -> u32 {
        let count = self.leases().filter(|(_, lease)| lease.request.convoy == *convoy && lease.request.class == class).count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    /// The snapshot a placement of waiting vessel `id` on `host` reads.
    /// Admission is a positive cap for the class, as in `admission_placement`.
    pub fn evidence(&self, id: &VesselId, host: Host) -> Result<Evidence, LedgerError> {
        let request = self.waiting(id)?;
        let available = self.available(host)?;
        let waiting = self
            .vessels
            .iter()
            .filter_map(|(other, vessel)| match vessel {
                Vessel::Waiting(request) if other != id => Some(request.clone()),
                _ => None,
            })
            .collect();
        let cap = self.cap(&request.convoy, request.class);
        Ok(Evidence { host, available, waiting, live: self.live(&request.convoy, request.class), cap, admitted: cap > 0 })
    }

    /// Places waiting vessel `id` on `host` when [`place`] allows it.
    pub fn place(&mut self, id: &VesselId, host: Host) -> Result<&Lease, LedgerError> {
        let evidence = self.evidence(id, host)?;
        let lease = place(self.waiting(id)?, &evidence).map_err(LedgerError::Refused)?;
        let vessel = self.vessels.get_mut(id).ok_or_else(|| LedgerError::UnknownVessel(id.clone()))?;
        *vessel = Vessel::Leased(lease);
        match vessel {
            Vessel::Leased(lease) => Ok(lease),
            Vessel::Waiting(_) => Err(LedgerError::NotLeased(id.clone())),
        }
    }

    /// Returns vessel `id`'s lease and forgets the vessel, freeing its shape.
    /// The ledger is pure: the caller calls this when the vessel's lifecycle
    /// reaches Gone, and not before.
    pub fn release(&mut self, id: &VesselId) -> Result<Lease, LedgerError> {
        match self.vessels.remove(id) {
            Some(Vessel::Leased(lease)) => Ok(lease),
            Some(waiting) => {
                self.vessels.insert(id.clone(), waiting);
                Err(LedgerError::NotLeased(id.clone()))
            }
            None => Err(LedgerError::UnknownVessel(id.clone())),
        }
    }

    fn waiting(&self, id: &VesselId) -> Result<&Request, LedgerError> {
        match self.vessels.get(id) {
            Some(Vessel::Waiting(request)) => Ok(request),
            Some(Vessel::Leased(_)) => Err(LedgerError::NotWaiting(id.clone())),
            None => Err(LedgerError::UnknownVessel(id.clone())),
        }
    }
}
