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
//! history. The leases are a view over the live Vessel resources, which store
//! each lease as their spec, so a restarted ledger is rebuilt from them
//! ([`Ledger::restore`]). Every failure is an explicit error; nothing panics.

use std::collections::BTreeMap;

use flotilla_schema::{Stored, Timestamp};

use crate::{
    machines::{
        convoy::{ConvoyId, Priority, Quota, VesselClass},
        lifecycle::{LifecycleEdge, LifecycleRefusal, LifecycleState, LifecycleWorld, MemoryLifecycleWorld},
        vessel::{live_leases, Vessel as VesselMachine, VesselSpec},
    },
    Home, Log, Resource, WriteError,
};

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
    pub fn cap(&self, convoy: &ConvoyId, class: &VesselClass) -> u32 {
        self.caps.get(convoy).and_then(|quota| quota.get(class)).copied().unwrap_or(0)
    }

    /// Leased vessels of the convoy and class, on any host.
    pub fn live(&self, convoy: &ConvoyId, class: &VesselClass) -> u32 {
        let count = self.leases().filter(|(_, lease)| lease.request.convoy == *convoy && lease.request.class == *class).count();
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
        let cap = self.cap(&request.convoy, &request.class);
        Ok(Evidence { host, available, waiting, live: self.live(&request.convoy, &request.class), cap, admitted: cap > 0 })
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

    /// Puts back the lease of a live vessel granted before a restart, read from
    /// its stored spec ([`live_leases`](crate::machines::vessel::live_leases)):
    /// the ledger's leases are a view over the live Vessels, so it is rebuilt
    /// from them rather than kept. Refused when `id` is already known, or when
    /// the lease names an unknown host or does not fit the host's capacity
    /// beside the leases already restored. A refusal leaves the ledger as it
    /// was and names the vessel. What a restart does with such a vessel (a
    /// host's capacity shrank, or the host is gone) is the startup caller's
    /// decision, not the ledger's; see #3438.
    pub fn restore(&mut self, id: VesselId, lease: Lease) -> Result<(), LedgerError> {
        if self.vessels.contains_key(&id) {
            return Err(LedgerError::DuplicateRequest(id));
        }
        let host = lease.host;
        self.vessels.insert(id.clone(), Vessel::Leased(lease));
        if let Err(error) = self.available(host) {
            self.vessels.remove(&id);
            return Err(error);
        }
        Ok(())
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

/// Why a placement home did not take a write.
#[derive(Debug)]
pub enum PlacementError {
    /// The ledger refused it: an unknown vessel or host, or a placement refusal.
    Ledger(LedgerError),
    /// The vessel's resource refused it, or the log did not take it.
    Write(WriteError<VesselMachine>),
}

impl PlacementError {
    /// The spec's refusal, when this is one: a placement refusal, or any lifecycle refusal of a
    /// leased vessel, which `vessel.qnt` reports as `MissingEvidence`.
    pub fn refusal(&self) -> Option<PlacementRefusal> {
        match self {
            PlacementError::Ledger(LedgerError::Refused(refusal)) => Some(*refusal),
            PlacementError::Write(WriteError::Refused(_)) => Some(PlacementRefusal::MissingEvidence),
            _ => None,
        }
    }
}

/// The spec's composed vessel (`machines/vessel.qnt`) at one home: `Waiting → Leased → Gone`.
///
/// A request waits at the [`Ledger`] and is not stored: the stored Vessel is the lease (ADR 0064).
/// [`Placements::place`] grants the lease under the spec's refusals and creates the vessel
/// resource, Requested, in the same step. Its lifecycle then runs on that resource (realise,
/// release, vanish) until it is Gone, when the ledger frees the lease. A restart rebuilds the
/// ledger's leases from the live vessels the home holds.
pub struct Placements<L: Log<VesselMachine>> {
    ledger: Ledger,
    home: Home<VesselMachine, L>,
}

impl<L: Log<VesselMachine>> Placements<L> {
    /// Placement over `hosts`, with the leases of `home`'s live vessels put back. A live vessel
    /// whose lease no longer fits (a host shrank or is gone) is returned with the reason, and its
    /// lease is not held: what to do with it is the caller's decision (#3438).
    pub fn new(
        home: Home<VesselMachine, L>,
        hosts: impl IntoIterator<Item = (Host, Units)>,
    ) -> Result<(Self, Vec<(VesselId, LedgerError)>), LedgerError> {
        let mut ledger = Ledger::new();
        for (host, capacity) in hosts {
            ledger.add_host(host, capacity)?;
        }
        let mut unplaced = Vec::new();
        for (id, lease) in live_leases(home.resources()) {
            if let Err(error) = ledger.restore(id.clone(), lease) {
                unplaced.push((id, error));
            }
        }
        Ok((Placements { ledger, home }, unplaced))
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn home(&self) -> &Home<VesselMachine, L> {
        &self.home
    }

    /// The vessel resource `id` names, once its lease is granted.
    pub fn get(&self, id: &VesselId) -> Option<&Resource<VesselMachine>> {
        self.home.get(&id.0)
    }

    /// Sets a convoy's quota, as admitted or readmitted.
    pub fn set_caps(&mut self, convoy: ConvoyId, quota: Quota) {
        self.ledger.set_caps(convoy, quota);
    }

    /// Queues a vessel's request: Waiting, at the ledger only.
    pub fn request(&mut self, id: VesselId, request: Request) -> Result<(), PlacementError> {
        self.ledger.request(id, request).map_err(PlacementError::Ledger)
    }

    /// Places waiting vessel `id` on `host` (spec `Place`) and creates its resource at `now`,
    /// Requested. A refusal, or a log that does not take the create, leaves the ledger as it was.
    pub fn place(&mut self, id: &VesselId, host: Host, now: Timestamp) -> Result<&Resource<VesselMachine>, PlacementError> {
        let before = self.ledger.clone();
        let lease = self.ledger.place(id, host).map_err(PlacementError::Ledger)?.clone();
        let world = MemoryLifecycleWorld::default();
        if let Err(error) = self.home.create(id.0.clone(), VesselSpec::from(lease), &world, now) {
            self.ledger = before;
            return Err(PlacementError::Write(error));
        }
        self.home.get(&id.0).ok_or_else(|| PlacementError::Ledger(LedgerError::UnknownVessel(id.clone())))
    }

    /// A lifecycle edge on leased vessel `id`: realise, release or vanish (the spec's leased
    /// events). Reaching Gone frees the lease. A vessel's attempts and give-up are no edges of
    /// `vessel.qnt`, so they are refused as the wrong state.
    pub fn apply(
        &mut self,
        id: &VesselId,
        edge: LifecycleEdge,
        world: &(dyn LifecycleWorld + 'static),
        now: Timestamp,
    ) -> Result<&Resource<VesselMachine>, PlacementError> {
        if matches!(edge, LifecycleEdge::Attempted(_) | LifecycleEdge::GiveUp(_)) {
            return Err(PlacementError::Write(WriteError::Refused(LifecycleRefusal::WrongState)));
        }
        self.home.apply(&id.0, edge, world, now).map_err(PlacementError::Write)?;
        if self.home.get(&id.0).is_some_and(|vessel| *vessel.state() == LifecycleState::Gone) {
            self.ledger.release(id).map_err(PlacementError::Ledger)?;
        }
        self.home.get(&id.0).ok_or_else(|| PlacementError::Ledger(LedgerError::UnknownVessel(id.clone())))
    }
}

#[cfg(test)]
mod placements_tests {
    use super::*;
    use crate::{
        machines::lifecycle::{Attempted, Observed, Realise, Release},
        Cause, CauseCategory, MemoryLog, Outcome, Ref,
    };

    const HOST: Host = Host(1);

    fn request(convoy: &str, cores: u32) -> Request {
        Request {
            convoy: ConvoyId::new(convoy, "home"),
            class: VesselClass::new("crew"),
            shape: [(Dimension(0), cores)].into(),
            priority: Priority(1),
        }
    }

    fn placements(home: Home<VesselMachine, MemoryLog<VesselMachine>>) -> Placements<MemoryLog<VesselMachine>> {
        let (mut placements, unplaced) = Placements::new(home, [(HOST, [(Dimension(0), 4)].into())]).expect("host");
        assert!(unplaced.is_empty());
        placements.set_caps(ConvoyId::new("c", "home"), [(VesselClass::new("crew"), 2)].into());
        placements
    }

    // Behaviour (#3629 layer 6): a waiting request is not stored; a grant creates the vessel
    // resource Requested with the lease as its spec; a refusal (no fit) creates nothing and
    // leaves the ledger as it was; Gone frees the lease, so a second request fits again.
    #[test]
    fn a_grant_creates_the_vessel_and_gone_frees_its_lease() {
        let mut placements = placements(Home::new("home").expect("declaration"));
        let (a, b) = (VesselId::new("a"), VesselId::new("b"));
        placements.request(a.clone(), request("c", 3)).expect("request");
        assert!(placements.get(&a).is_none(), "a waiting request is not stored");
        let vessel = placements.place(&a, HOST, Timestamp(1)).expect("granted");
        assert_eq!(*vessel.state(), LifecycleState::requested());
        assert_eq!(vessel.spec().lease(), Lease { request: request("c", 3), host: HOST });

        placements.request(b.clone(), request("c", 3)).expect("request");
        assert_eq!(placements.place(&b, HOST, Timestamp(2)).expect_err("no room").refusal(), Some(PlacementRefusal::DoesNotFit));
        assert!(placements.get(&b).is_none());
        assert!(matches!(placements.ledger().vessel(&b), Some(Vessel::Waiting(_))), "still waiting");

        let mut world = MemoryLifecycleWorld::default();
        world.teardowns.put(&Ref::new("a"), true);
        placements.apply(&a, Release { teardown: Ref::new("a") }.into(), &world, Timestamp(3)).expect("released");
        assert!(placements.ledger().vessel(&a).is_none(), "the lease is freed");
        placements.place(&b, HOST, Timestamp(4)).expect("now it fits");
    }

    // Behaviour: a vessel takes no attempts and never gives up (`vessel.qnt` has neither edge).
    #[test]
    fn a_vessel_refuses_attempts() {
        let mut placements = placements(Home::new("home").expect("declaration"));
        let a = VesselId::new("a");
        placements.request(a.clone(), request("c", 1)).expect("request");
        placements.place(&a, HOST, Timestamp(1)).expect("granted");
        let failed = Outcome::Failed { cause: Cause::new(CauseCategory::Transient, "x"), retryable: true };
        let world = MemoryLifecycleWorld::default();
        let refused = placements.apply(&a, Attempted { outcome: failed }.into(), &world, Timestamp(2)).expect_err("refused");
        assert!(matches!(refused, PlacementError::Write(WriteError::Refused(LifecycleRefusal::WrongState))));
    }

    // Behaviour: a restart rebuilds the ledger's leases from the live vessels its home holds.
    #[test]
    fn a_restart_rebuilds_the_leases_from_the_live_vessels() {
        let mut placements = placements(Home::new("home").expect("declaration"));
        let a = VesselId::new("a");
        placements.request(a.clone(), request("c", 3)).expect("request");
        placements.place(&a, HOST, Timestamp(1)).expect("granted");
        let mut world = MemoryLifecycleWorld::default();
        world.realisations.put(&Ref::new("a"), Observed { identity: "a".into() });
        placements.apply(&a, Realise { identity: "a".into(), observed: Ref::new("a") }.into(), &world, Timestamp(2)).expect("realised");

        let mut home = Home::<VesselMachine>::new("home").expect("declaration");
        home.restore(placements.get(&a).expect("held").clone()).expect("restored");
        let restarted = self::placements(home);
        assert_eq!(restarted.ledger().available(HOST), Ok([(Dimension(0), 1)].into()), "the lease holds its shape again");
    }
}
