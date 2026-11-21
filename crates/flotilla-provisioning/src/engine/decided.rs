//! The ports serving the decided store's own kinds (#3108): what a daemon installs on its resource
//! backend as soon as the store opens, and a test's engine backend installs the same way.

use std::sync::Arc;

use flotilla_resources::{
    account_access::AccountRecords,
    engine_records::{ChangeRequestBranches, LateSourceObservers, ObservationFailures},
    manifest_roots::ManifestRoots,
    standing_roles::StandingRoles,
    Clock,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::EnginePort;

use super::{
    access_deliveries::EngineAccessDeliveries, account::EngineAccounts, artifacts::EngineArtifacts, definitions::EngineDefinitions,
    hosts::EngineHosts, kinds::EngineKinds, manifest_root::EngineManifestRoots, observations::EngineObservations, projects::EngineProjects,
    repositories::EngineRepositories, standing_roles::EngineStandingRoles,
};

/// The ports over one decided store, each also held as the reader its other consumers take.
pub struct DecidedPorts {
    /// Every port but the observations', to install on a backend before anything reads it.
    pub records: Vec<Arc<dyn EnginePort>>,
    /// ChangeRequest and Issue, which the forge observers write only on change; installed beside
    /// `records`.
    pub observations: Arc<EngineObservations>,
    pub observation_failures: Arc<dyn ObservationFailures>,
    /// The change requests the forge observers found on their repositories' own branches.
    pub change_request_branches: Arc<dyn ChangeRequestBranches>,
    /// Which sources this home observes, which the observation records consult before a write.
    pub source_observers: Arc<LateSourceObservers>,
    pub accounts: Arc<dyn AccountRecords>,
    pub manifest_roots: Arc<dyn ManifestRoots>,
    /// Each standing role's running values, written by the home that admits it.
    pub standing_roles: Arc<dyn StandingRoles>,
    /// Host, also installed among `records`: the planners read it through its port alone.
    pub hosts: Arc<EngineHosts>,
}

/// Opens the ports over `service`, whose own Host is `own_host`.
pub fn open_ports(service: &Arc<Service>, own_host: &str, clock: Arc<dyn Clock>) -> Result<DecidedPorts, String> {
    let clock = || Arc::clone(&clock);
    // #3824 layers 1 and 2c: the account kinds and ManifestRoot, which the generic resource verbs
    // reach through their records as well.
    let accounts = Arc::new(EngineAccounts::open(Arc::clone(service), clock())?);
    let manifest_roots = Arc::new(EngineManifestRoots::open(Arc::clone(service), clock())?);
    // ChangeRequest and Issue: written by their source's observer, read at every home.
    let source_observers = Arc::new(LateSourceObservers::default());
    let observations = Arc::new(EngineObservations::open(Arc::clone(service), clock())?.observed_by(source_observers.clone()));
    // Host, which every placement reads and this home's observers write only on change.
    let hosts = Arc::new(EngineHosts::open(Arc::clone(service), own_host.to_owned(), clock())?);
    let records: Vec<Arc<dyn EnginePort>> = vec![
        Arc::clone(&accounts) as Arc<dyn EnginePort>,
        Arc::clone(&manifest_roots) as Arc<dyn EnginePort>,
        // #3824 layer 2b-ii: the definition kinds.
        Arc::new(EngineDefinitions::open(Arc::clone(service), clock())?),
        // #3824 layer 3a and 3b: Repository, and Project with its operations' record.
        Arc::new(EngineRepositories::open(Arc::clone(service), clock())?),
        Arc::new(EngineProjects::open(Arc::clone(service), clock())?),
        Arc::new(EngineArtifacts::open(Arc::clone(service), clock())?),
        Arc::new(EngineAccessDeliveries::open(Arc::clone(service), clock())?),
        // FulfilmentKind, which admission and the vessel reconciler read and admission's snapshots write.
        Arc::new(EngineKinds::open(Arc::clone(service), clock())?),
        Arc::clone(&hosts) as Arc<dyn EnginePort>,
    ];
    Ok(DecidedPorts {
        records,
        observation_failures: observations.clone(),
        change_request_branches: observations.clone(),
        observations,
        source_observers,
        accounts,
        manifest_roots,
        standing_roles: Arc::new(EngineStandingRoles::open(Arc::clone(service), clock())),
        hosts,
    })
}
