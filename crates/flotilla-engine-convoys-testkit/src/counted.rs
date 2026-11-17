//! A home's lifecycle reads with the engine's convoys added, counting the keyed gets made through
//! them for read-cost tests: a listing, index or scan meter alone would also pass a reader that
//! got every record in the fleet by key.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use flotilla_engine_convoys::convoy_records::EngineConvoyRecords;
use flotilla_protocol::LifecycleReading;
use flotilla_resources::{
    agent_activity::AgentSessionReads,
    checkout_records::CheckoutRecords,
    clone_records::CloneRecords,
    convoy_records::{ConvoyChange, ConvoyListing, ConvoyMemory, ConvoyRecord, ConvoyRecords},
    environment_records::{EnvironmentChange, EnvironmentObservations, EnvironmentRecord, EnvironmentRecords},
    held_joins::HeldJoins,
    image_build_records::ImageBuildRecords,
    lifecycle_reads::{ChangeCursor, LifecycleReads, ProvisioningKind},
    terminal_records::{TerminalChange, TerminalLaunchRequest, TerminalObservations, TerminalRecord, TerminalRecords, TerminalRenewal},
    vessel_records::{VesselChange, VesselLaunchRequest, VesselRecord, VesselRecords, VesselStep},
    EnvironmentSpec,
};

/// The keyed gets made so far, by kind: `get`, `find` and `newest` for vessels and convoys, `get`
/// and `find` for environments, `get` for terminals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyedGets {
    pub vessels: u64,
    pub convoys: u64,
    pub environments: u64,
    pub terminals: u64,
}

impl std::ops::Sub for KeyedGets {
    type Output = KeyedGets;

    fn sub(self, before: KeyedGets) -> KeyedGets {
        KeyedGets {
            vessels: self.vessels - before.vessels,
            convoys: self.convoys - before.convoys,
            environments: self.environments - before.environments,
            terminals: self.terminals - before.terminals,
        }
    }
}

#[derive(Default)]
pub(crate) struct Meter {
    vessels: AtomicU64,
    convoys: AtomicU64,
    environments: AtomicU64,
    terminals: AtomicU64,
    /// Convoy listings, `at` a home or at `every` home.
    convoy_listings: AtomicU64,
}

impl Meter {
    pub(crate) fn convoy_listings(&self) -> u64 {
        self.convoy_listings.load(Ordering::Relaxed)
    }

    pub(crate) fn gets(&self) -> KeyedGets {
        KeyedGets {
            vessels: self.vessels.load(Ordering::Relaxed),
            convoys: self.convoys.load(Ordering::Relaxed),
            environments: self.environments.load(Ordering::Relaxed),
            terminals: self.terminals.load(Ordering::Relaxed),
        }
    }

    fn count(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// A test's lifecycle reads, with the engine's convoys added, every keyed get counted on `meter`.
pub(crate) struct WithConvoys {
    reads: Arc<dyn LifecycleReads>,
    vessels: CountedVessels,
    convoys: CountedConvoys,
    environments: CountedEnvironments,
    terminals: CountedTerminals,
    joins: HeldJoins,
}

impl WithConvoys {
    pub(crate) fn new(reads: Arc<dyn LifecycleReads>, records: EngineConvoyRecords, meter: Arc<Meter>) -> Self {
        Self {
            vessels: CountedVessels { reads: Arc::clone(&reads), meter: Arc::clone(&meter) },
            convoys: CountedConvoys { records, meter: Arc::clone(&meter) },
            environments: CountedEnvironments { reads: Arc::clone(&reads), meter: Arc::clone(&meter) },
            terminals: CountedTerminals { reads: Arc::clone(&reads), meter },
            joins: HeldJoins::default(),
            reads,
        }
    }
}

impl LifecycleReads for WithConvoys {
    fn home(&self) -> Result<&str, String> {
        self.reads.home()
    }

    fn try_reading(&self, kind: ProvisioningKind, name: &str, origin: Option<&str>) -> Result<Option<LifecycleReading>, String> {
        self.reads.try_reading(kind, name, origin)
    }

    fn vessels(&self) -> Result<&dyn VesselRecords, String> {
        self.reads.vessels()?;
        Ok(&self.vessels)
    }

    fn terminals(&self) -> Result<&dyn TerminalRecords, String> {
        self.reads.terminals()?;
        Ok(&self.terminals)
    }

    fn environments(&self) -> Result<&dyn EnvironmentRecords, String> {
        self.reads.environments()?;
        Ok(&self.environments)
    }

    fn convoys(&self) -> Result<&dyn ConvoyRecords, String> {
        Ok(&self.convoys)
    }

    // The kinds no read-cost test counts yet are served as the test's reads serve them.
    fn clones(&self) -> Result<&dyn CloneRecords, String> {
        self.reads.clones()
    }

    fn checkouts(&self) -> Result<&dyn CheckoutRecords, String> {
        self.reads.checkouts()
    }

    fn image_builds(&self) -> Result<&dyn ImageBuildRecords, String> {
        self.reads.image_builds()
    }

    fn agent_sessions(&self) -> Result<&dyn AgentSessionReads, String> {
        self.reads.agent_sessions()
    }

    /// Its own, so what fills them is read through the counted ports.
    fn joins(&self) -> Result<&HeldJoins, String> {
        Ok(&self.joins)
    }
}

/// Served only once the reads are known to serve vessels, so the port is there.
struct CountedVessels {
    reads: Arc<dyn LifecycleReads>,
    meter: Arc<Meter>,
}

impl CountedVessels {
    fn port(&self) -> &dyn VesselRecords {
        self.reads.vessels().expect("the reads serve vessels")
    }
}

impl VesselRecords for CountedVessels {
    fn home(&self) -> &str {
        self.port().home()
    }

    fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
        self.port().codes()
    }

    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<VesselRecord>, String> {
        Meter::count(&self.meter.vessels);
        self.port().get(name, home)
    }

    fn launches(&self, convoy: &str, convoy_home: &str, launch_of: &str, home: Option<&str>) -> Result<Vec<VesselRecord>, String> {
        self.port().launches(convoy, convoy_home, launch_of, home)
    }

    fn newest(&self, convoy: &str, convoy_home: &str, launch_of: &str, home: Option<&str>) -> Result<Option<VesselRecord>, String> {
        Meter::count(&self.meter.vessels);
        self.port().newest(convoy, convoy_home, launch_of, home)
    }

    fn at(&self, home: &str) -> Result<Vec<VesselRecord>, String> {
        self.port().at(home)
    }

    fn every(&self) -> Result<Vec<VesselRecord>, String> {
        self.port().every()
    }

    fn find(&self, name: &str) -> Result<Option<VesselRecord>, String> {
        Meter::count(&self.meter.vessels);
        self.port().find(name)
    }

    fn of_convoy(&self, convoy: &str) -> Result<Vec<VesselRecord>, String> {
        self.port().of_convoy(convoy)
    }

    fn request(&self, request: &VesselLaunchRequest) -> Result<String, String> {
        self.port().request(request)
    }

    fn step(&self, name: &str, step: &VesselStep) -> Result<VesselRecord, String> {
        self.port().step(name, step)
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<VesselChange> {
        self.port().changes()
    }

    fn cursor(&self) -> Option<Box<dyn ChangeCursor<VesselChange>>> {
        self.port().cursor()
    }
}

struct CountedConvoys {
    records: EngineConvoyRecords,
    meter: Arc<Meter>,
}

impl ConvoyRecords for CountedConvoys {
    fn home(&self) -> &str {
        self.records.home()
    }

    fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
        self.records.codes()
    }

    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<ConvoyRecord>, String> {
        Meter::count(&self.meter.convoys);
        self.records.get(name, home)
    }

    fn named(&self, name: &str) -> Result<Vec<ConvoyRecord>, String> {
        Meter::count(&self.meter.convoys);
        self.records.named(name)
    }

    fn newest(&self, of: &str, project: &str, home: Option<&str>) -> Result<Option<ConvoyRecord>, String> {
        Meter::count(&self.meter.convoys);
        self.records.newest(of, project, home)
    }

    fn at(&self, home: &str) -> Result<ConvoyListing, String> {
        Meter::count(&self.meter.convoy_listings);
        self.records.at(home)
    }

    fn every(&self) -> Result<ConvoyListing, String> {
        Meter::count(&self.meter.convoy_listings);
        self.records.every()
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<ConvoyChange> {
        self.records.changes()
    }

    fn cursor(&self) -> Option<Box<dyn ChangeCursor<ConvoyChange>>> {
        self.records.cursor()
    }

    fn memory(&self) -> &ConvoyMemory {
        self.records.memory()
    }
}

/// Served only once the reads are known to serve environments, so the port is there.
struct CountedEnvironments {
    reads: Arc<dyn LifecycleReads>,
    meter: Arc<Meter>,
}

impl CountedEnvironments {
    fn port(&self) -> &dyn EnvironmentRecords {
        self.reads.environments().expect("the reads serve environments")
    }
}

impl EnvironmentRecords for CountedEnvironments {
    fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
        self.port().codes()
    }

    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<EnvironmentRecord>, String> {
        Meter::count(&self.meter.environments);
        self.port().get(name, home)
    }

    fn find(&self, name: &str) -> Result<Option<EnvironmentRecord>, String> {
        Meter::count(&self.meter.environments);
        self.port().find(name)
    }

    fn live(&self) -> Result<Vec<EnvironmentRecord>, String> {
        self.port().live()
    }

    fn every(&self) -> Result<Vec<EnvironmentRecord>, String> {
        self.port().every()
    }

    fn held(&self, vessel: &str) -> Result<Vec<EnvironmentRecord>, String> {
        self.port().held(vessel)
    }

    fn request(&self, name: &str, spec: &EnvironmentSpec, vessel: Option<&str>) -> Result<bool, String> {
        self.port().request(name, spec, vessel)
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<EnvironmentChange> {
        self.port().changes()
    }

    fn cursor(&self) -> Option<Box<dyn ChangeCursor<EnvironmentChange>>> {
        self.port().cursor()
    }

    fn observations(&self) -> &EnvironmentObservations {
        self.port().observations()
    }
}

/// Served only once the reads are known to serve terminals, so the port is there.
struct CountedTerminals {
    reads: Arc<dyn LifecycleReads>,
    meter: Arc<Meter>,
}

impl CountedTerminals {
    fn port(&self) -> &dyn TerminalRecords {
        self.reads.terminals().expect("the reads serve terminals")
    }
}

impl TerminalRecords for CountedTerminals {
    fn home(&self) -> &str {
        self.port().home()
    }

    fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
        self.port().codes()
    }

    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<TerminalRecord>, String> {
        Meter::count(&self.meter.terminals);
        self.port().get(name, home)
    }

    fn at(&self, home: &str) -> Result<Vec<TerminalRecord>, String> {
        self.port().at(home)
    }

    fn every(&self) -> Result<Vec<TerminalRecord>, String> {
        self.port().every()
    }

    fn of_vessel(&self, vessel: &str) -> Result<Vec<TerminalRecord>, String> {
        self.port().of_vessel(vessel)
    }

    fn of_environment(&self, environment: &str, home: &str) -> Result<Vec<TerminalRecord>, String> {
        self.port().of_environment(environment, home)
    }

    fn request(&self, request: TerminalLaunchRequest) -> Result<String, String> {
        self.port().request(request)
    }

    fn renew(&self, session: &str) -> Result<TerminalRenewal, String> {
        self.port().renew(session)
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<TerminalChange> {
        self.port().changes()
    }

    fn cursor(&self) -> Option<Box<dyn ChangeCursor<TerminalChange>>> {
        self.port().cursor()
    }

    fn observations(&self) -> &TerminalObservations {
        self.port().observations()
    }
}
