//! Placement reads only fit and priority (spec `machines/vessel.qnt`, scenario
//! `admission_placement.qnt`, ADR 0064).
//!
//! [`place`] is the spec's `Place` rule over one placement snapshot: the whole
//! request fits the host's available capacity in every dimension, and no
//! waiting request of higher priority fits there. A lease holds its shape on
//! one host until the vessel is Gone.
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
        convoy::{ConvoyId, Priority, VesselClass},
        lifecycle::{LifecycleEdge, LifecycleWorld, MemoryLifecycleWorld},
        vessel::{live_leases, Vessel as VesselMachine, VesselSpec},
    },
    Home, Log, Resource, WriteError,
};

/// A host that capacity is leased on: the host's id as the fleet names it (a placement's
/// target host), interned text in its own domain (`hs`). It is not a stored resource, so it is
/// a plain column rather than a `Ref`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Stored)]
#[store(intern = "hs")]
pub struct Host(pub String);

impl Host {
    pub fn new(id: impl Into<String>) -> Self {
        Host(id.into())
    }
}

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
}

/// A granted placement: the whole request, leased on one host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub request: Request,
    pub host: Host,
}

/// The spec's vessel `Refusal`. [`place`] produces the middle two; the
/// lifecycle half of a vessel produces `WrongState` and `MissingEvidence`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlacementRefusal {
    WrongState,
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
    if !fits(request, &evidence.available) {
        return Err(PlacementRefusal::DoesNotFit);
    }
    if evidence.waiting.iter().any(|waiting| waiting.priority > request.priority && fits(waiting, &evidence.available)) {
        return Err(PlacementRefusal::HigherPriorityWaiting);
    }
    Ok(Lease { request: request.clone(), host: evidence.host.clone() })
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

/// A pure, in-memory placement ledger: host capacities and vessels.
#[derive(Clone, Debug, Default)]
pub struct Ledger {
    hosts: BTreeMap<Host, Units>,
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
        let mut available = self.hosts.get(&host).ok_or_else(|| LedgerError::UnknownHost(host.clone()))?.clone();
        for lease in self.leases().map(|(_, lease)| lease).filter(|lease| lease.host == host) {
            for (dimension, units) in &lease.request.shape {
                let free = available
                    .get_mut(dimension)
                    .ok_or_else(|| LedgerError::UnknownDimension { host: host.clone(), dimension: *dimension })?;
                *free =
                    free.checked_sub(*units).ok_or_else(|| LedgerError::CapacityUnderflow { host: host.clone(), dimension: *dimension })?;
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

    /// The snapshot a placement of waiting vessel `id` on `host` reads.
    pub fn evidence(&self, id: &VesselId, host: Host) -> Result<Evidence, LedgerError> {
        self.waiting(id)?;
        let available = self.available(host.clone())?;
        let waiting = self
            .vessels
            .iter()
            .filter_map(|(other, vessel)| match vessel {
                Vessel::Waiting(request) if other != id => Some(request.clone()),
                _ => None,
            })
            .collect();
        Ok(Evidence { host, available, waiting })
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
        let host = lease.host.clone();
        self.vessels.insert(id.clone(), Vessel::Leased(lease));
        if let Err(error) = self.available(host) {
            self.vessels.remove(&id);
            return Err(error);
        }
        Ok(())
    }

    /// Moves vessel `from`'s entry to `to`, as a launch's create names the vessel it requested.
    pub fn rename(&mut self, from: &VesselId, to: VesselId) -> Result<(), LedgerError> {
        if self.vessels.contains_key(&to) {
            return Err(LedgerError::DuplicateRequest(to));
        }
        let vessel = self.vessels.remove(from).ok_or_else(|| LedgerError::UnknownVessel(from.clone()))?;
        self.vessels.insert(to, vessel);
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
    /// A placement refusal or a leased lifecycle's missing evidence.
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

    /// Adds a host with its capacity by dimension, as it joins after the ledger was built.
    pub fn add_host(&mut self, host: Host, capacity: Units) -> Result<(), PlacementError> {
        self.ledger.add_host(host, capacity).map_err(PlacementError::Ledger)
    }

    /// Queues a vessel's request: Waiting, at the ledger only.
    pub fn request(&mut self, id: VesselId, request: Request) -> Result<(), PlacementError> {
        self.ledger.request(id, request).map_err(PlacementError::Ledger)
    }

    /// Places waiting vessel `id` on `host` (spec `Place`) and creates its resource at `now`,
    /// Requested, as a launch of `id`: its log names it `<id>#<n>`, and the ledger holds the lease
    /// under that name from then on. A refusal leaves the ledger as it was; a log that does not
    /// take the create returns the lease and queues the request again, as it waited before.
    pub fn place(&mut self, id: &VesselId, host: Host, now: Timestamp) -> Result<&Resource<VesselMachine>, PlacementError> {
        let lease = self.ledger.place(id, host).map_err(PlacementError::Ledger)?.clone();
        let world = MemoryLifecycleWorld::default();
        let name = match self.home.create(id.0.clone(), VesselSpec::of(id.0.clone(), lease.clone()), &world, now) {
            Ok(created) => created.resource.name().to_owned(),
            Err(error) => {
                // Undo the grant: the lease returns and the request waits again, where it waited.
                self.ledger.release(id).map_err(PlacementError::Ledger)?;
                self.ledger.request(id.clone(), lease.request).map_err(PlacementError::Ledger)?;
                return Err(PlacementError::Write(error));
            }
        };
        let launch = VesselId::new(name);
        self.ledger.rename(id, launch.clone()).map_err(PlacementError::Ledger)?;
        self.home.get(&launch.0).ok_or_else(|| PlacementError::Ledger(LedgerError::UnknownVessel(launch)))
    }

    /// A lifecycle edge on a leased vessel. Every terminal state frees its lease; a failed
    /// provisioning attempt gives up with its cause before a backing has been realised.
    pub fn apply(
        &mut self,
        id: &VesselId,
        edge: LifecycleEdge,
        world: &(dyn LifecycleWorld + 'static),
        now: Timestamp,
    ) -> Result<&Resource<VesselMachine>, PlacementError> {
        self.home.apply(&id.0, edge, world, now).map_err(PlacementError::Write)?;
        if self.home.get(&id.0).is_some_and(|vessel| !vessel.state().live()) {
            // The edge is written; a lease the ledger never held (an unplaced vessel) is no error.
            match self.ledger.release(id) {
                Ok(_) | Err(LedgerError::UnknownVessel(_)) => {}
                Err(error) => return Err(PlacementError::Ledger(error)),
            }
        }
        self.home.get(&id.0).ok_or_else(|| PlacementError::Ledger(LedgerError::UnknownVessel(id.clone())))
    }
}

#[cfg(test)]
mod placements_tests {
    use super::*;
    use crate::{
        machines::lifecycle::{Attempted, Ending, GiveUp, LifecycleState, Observed, Realise, Release, Vanish, VanishCause},
        Cause, CauseCategory, MemoryLog, Outcome, Ref,
    };

    fn host() -> Host {
        Host::new("1")
    }

    fn request(convoy: &str, cores: u32) -> Request {
        Request {
            convoy: ConvoyId::new(convoy, "home"),
            class: VesselClass::new("crew"),
            shape: [(Dimension(0), cores)].into(),
            priority: Priority(1),
        }
    }

    fn placements(home: Home<VesselMachine, MemoryLog<VesselMachine>>) -> Placements<MemoryLog<VesselMachine>> {
        let (placements, unplaced) = Placements::new(home, [(host(), [(Dimension(0), 4)].into())]).expect("host");
        assert!(unplaced.is_empty());
        placements
    }

    // Behaviour (#3629 layer 6): a waiting request is not stored; a grant creates the vessel
    // resource Requested with the lease as its spec, as a numbered launch of the request; a
    // refusal (no fit) creates nothing and leaves the ledger as it was; Gone frees the lease, so a
    // second request fits again.
    #[test]
    fn a_grant_creates_the_vessel_and_gone_frees_its_lease() {
        let mut placements = placements(Home::new("home").expect("declaration"));
        let (a, b) = (VesselId::new("a"), VesselId::new("b"));
        placements.request(a.clone(), request("c", 3)).expect("request");
        assert!(placements.get(&a).is_none(), "a waiting request is not stored");
        let vessel = placements.place(&a, host(), Timestamp(1)).expect("granted");
        assert_eq!(*vessel.state(), LifecycleState::requested());
        assert_eq!(vessel.spec().lease(), Lease { request: request("c", 3), host: host() });
        assert_eq!((vessel.name(), vessel.spec().launch_of.as_str(), vessel.spec().launch), ("a#1", "a", 1));
        let a = VesselId::new(vessel.name());
        assert!(matches!(placements.ledger().vessel(&a), Some(Vessel::Leased(_))), "the lease is held under the launch's name");

        placements.request(b.clone(), request("c", 3)).expect("request");
        assert_eq!(placements.place(&b, host(), Timestamp(2)).expect_err("no room").refusal(), Some(PlacementRefusal::DoesNotFit));
        assert!(placements.get(&b).is_none());
        assert!(matches!(placements.ledger().vessel(&b), Some(Vessel::Waiting(_))), "still waiting");

        let mut world = MemoryLifecycleWorld::default();
        world.teardowns.put(&Ref::new("a"), true);
        placements.apply(&a, Release { teardown: Ref::new("a") }.into(), &world, Timestamp(3)).expect("released");
        assert!(placements.ledger().vessel(&a).is_none(), "the lease is freed");
        placements.place(&b, host(), Timestamp(4)).expect("now it fits");
    }

    // A failed provisioning attempt retains its cause, and giving up frees the granted lease.
    // One example suffices for this delegation to the shared lifecycle's failure gate.
    #[test]
    fn a_vessel_gives_up_with_its_provisioning_failure() {
        let mut placements = placements(Home::new("home").expect("declaration"));
        placements.request(VesselId::new("a"), request("c", 1)).expect("request");
        let a = VesselId::new(placements.place(&VesselId::new("a"), host(), Timestamp(1)).expect("granted").name());
        let world = MemoryLifecycleWorld::default();
        let unrealised = placements
            .apply(&a, Vanish { cause: VanishCause::lost(), observed: Ref::new("a") }.into(), &world, Timestamp(2))
            .expect_err("a Requested vessel cannot vanish");
        assert_eq!(unrealised.refusal(), Some(PlacementRefusal::MissingEvidence));
        let cause = Cause::new(CauseCategory::External, "provider refused provisioning");
        let failed = Outcome::Failed { cause: cause.clone(), retryable: false };
        placements.apply(&a, Attempted { outcome: failed }.into(), &world, Timestamp(3)).expect("attempted");
        assert!(placements.ledger().vessel(&a).is_some(), "a request still holds its lease");
        let vessel = placements.apply(&a, GiveUp { cause: cause.clone() }.into(), &world, Timestamp(4)).expect("gave up");
        assert_eq!(vessel.state(), &LifecycleState::GaveUp { cause });
        assert!(placements.ledger().vessel(&a).is_none(), "giving up returns the lease");
    }

    // Behaviour (#3733 review): a vessel the ledger could not hold after a restart (its host
    // shrank, #3438) still ends: its release is written and nothing errors on the lease it never
    // held.
    #[test]
    fn an_unplaced_vessel_still_ends() {
        let mut placements = placements(Home::new("home").expect("declaration"));
        placements.request(VesselId::new("a"), request("c", 3)).expect("request");
        let a = VesselId::new(placements.place(&VesselId::new("a"), host(), Timestamp(1)).expect("granted").name());
        let mut home = Home::<VesselMachine>::new("home").expect("declaration");
        home.restore(placements.get(&a).expect("held").clone()).expect("restored");
        // The host now offers two cores: the three-core lease no longer fits.
        let (mut restarted, unplaced) = Placements::new(home, [(host(), [(Dimension(0), 2)].into())]).expect("host");
        assert_eq!(unplaced.len(), 1);
        let mut world = MemoryLifecycleWorld::default();
        world.teardowns.put(&Ref::new("a"), true);
        let vessel = restarted.apply(&a, Release { teardown: Ref::new("a") }.into(), &world, Timestamp(2)).expect("released");
        assert_eq!(*vessel.state(), LifecycleState::Gone { ending: Ending::Released });
    }

    // Behaviour (#3733 review): a create the log refuses puts the request back where it waited,
    // without copying the ledger: the lease is returned and the vessel waits again.
    #[test]
    fn a_refused_create_returns_the_lease_and_the_request_waits_again() {
        let mut first = placements(Home::new("home").expect("declaration"));
        let a = VesselId::new("a");
        first.request(a.clone(), request("c", 3)).expect("request");
        let held = first.place(&a, host(), Timestamp(1)).expect("granted").clone();
        // A home restored with launch a#1 over a log whose sequence starts again numbers the next
        // launch of `a` #1 too: the log takes the create, and the home refuses the taken name.
        let mut home = Home::<VesselMachine>::new("home").expect("declaration");
        home.restore(held).expect("restored");
        let mut placements = self::placements(home);
        placements.request(a.clone(), request("c", 1)).expect("request again");
        let refused = placements.place(&a, host(), Timestamp(3)).expect_err("the launch's name is taken");
        assert!(matches!(refused, PlacementError::Write(WriteError::Exists(name)) if name == "a#1"));
        assert!(matches!(placements.ledger().vessel(&a), Some(Vessel::Waiting(_))), "waiting again");
        assert_eq!(placements.ledger().available(host()), Ok([(Dimension(0), 1)].into()), "the lease was returned");
    }

    // Behaviour: a restart rebuilds the ledger's leases from the live vessels its home holds.
    #[test]
    fn a_restart_rebuilds_the_leases_from_the_live_vessels() {
        let mut placements = placements(Home::new("home").expect("declaration"));
        placements.request(VesselId::new("a"), request("c", 3)).expect("request");
        let a = VesselId::new(placements.place(&VesselId::new("a"), host(), Timestamp(1)).expect("granted").name());
        let mut world = MemoryLifecycleWorld::default();
        world.realisations.put(&Ref::new("a"), Observed { identity: "a".into() });
        placements.apply(&a, Realise { identity: "a".into(), observed: Ref::new("a") }.into(), &world, Timestamp(2)).expect("realised");

        let mut home = Home::<VesselMachine>::new("home").expect("declaration");
        home.restore(placements.get(&a).expect("held").clone()).expect("restored");
        let restarted = self::placements(home);
        assert_eq!(restarted.ledger().available(host()), Ok([(Dimension(0), 1)].into()), "the lease holds its shape again");
        assert!(matches!(restarted.ledger().vessel(&a), Some(Vessel::Leased(_))), "under the launch's name");
    }
}
