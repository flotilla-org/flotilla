//! The Vessel kind's home: each launch of a convoy's work, leased on the host it is placed on.
//!
//! Until a capacity model is ruled, a vessel's lease is the host its work was placed on at convoy
//! admission, recorded as granted: the engine judges no capacity, class or priority. A work
//! admitted with no placement is leased on the host of the home that admitted it.
//! [`PlacementLifecycles`] is the Vessel kind's home behind the engine's [`Lifecycles`] port. A
//! request queues the lease at the [`Placements`] ledger and places it on its pinned host at once,
//! which the ledger admits: the host joins with no capacity dimensions, the lease has no shape,
//! and its convoy is uncapped for the one class every pinned lease takes. The vessel resource is
//! created with the lease as its spec, and Gone frees the lease, as `vessel.qnt` composes them.
//!
//! [`EngineVessels`] is the [`VesselRecords`] port over it: a home's vessel requester requests
//! each launch, and its vessel planner, the single writer of its edges, steps it.
//!
//! | Planner's finding | Step | Edge |
//! |---|---|---|
//! | the vessel runs in its environment, first | [`VesselStep::Realised`] | Realise, under that environment |
//! | its provisioning failed before readiness | [`VesselStep::Failed`] | Attempted; GiveUp once the attempts run out |
//! | its environment disappeared, once realised | [`VesselStep::Lost`] | Vanish: Gone(Lost) |
//! | it broke, once realised | [`VesselStep::Broke`] | Vanish: Gone(Failed) |
//! | its convoy no longer needs it (the requester) | [`VesselStep::Released`] | Release |
//!
//! An ended launch is never provisioned again; a replacement is the work's next launch.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, PoisonError},
};

use chrono::{DateTime, Utc};
use flotilla_engine_store::follow::Followed;
use flotilla_resources::vessel_records::{VesselChange, VesselLaunchRequest, VesselRecord, VesselRecords, VesselStep};
use flotilla_schema::{Kind, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreHandle};
use flotilla_state_machines::{
    machines::{
        convoy::{ConvoyId, Priority, VesselClass},
        lifecycle::{Attempted, Lifecycle, LifecycleEdge, Realise, Release, Vanish, VanishCause},
        vessel::{live_leases, Vessel, VesselKind, VesselSpec},
    },
    placement::{Host, LedgerError, PlacementError, Placements, Units, VesselId},
    Cause, CauseCategory, Home, Log, Outcome, Ref, Resource,
};

use super::{
    evidence,
    reads::{indexed, keyed, requested_at, view, ReadError, StoreReads, View},
    EngineError, Lifecycles, Step,
};

/// The one class every pinned lease takes: the engine judges no class until a capacity model is
/// available.
pub const PINNED_CLASS: &str = "pinned";

/// The Vessel kind's home as the engine's [`Lifecycles`]: each request is placed on its pinned
/// host at once (see the module docs), and each step is a leased vessel's edge.
pub struct PlacementLifecycles<L: Log<Vessel>> {
    name: String,
    placements: Mutex<Placements<L>>,
}

impl<L: Log<Vessel>> PlacementLifecycles<L> {
    /// The placements of `home`, named `name`, with the leases of its live vessels put back on
    /// their pinned hosts. Each host holds exactly the shapes leased on it, so every live lease
    /// fits again: the engine judges no capacity, whatever shape a lease was granted with.
    pub fn new(name: impl Into<String>, home: Home<Vessel, L>) -> Result<Self, String> {
        let mut hosts: BTreeMap<Host, Units> = BTreeMap::new();
        for (_, lease) in live_leases(home.resources()) {
            let capacity = hosts.entry(lease.host).or_default();
            for (dimension, units) in lease.request.shape {
                let held = capacity.entry(dimension).or_default();
                *held = held.saturating_add(units);
            }
        }
        let (placements, unplaced) = Placements::new(home, hosts).map_err(|error| format!("vessel placements: {error:?}"))?;
        if let Some((id, error)) = unplaced.first() {
            return Err(format!("vessel `{}` could not be restored to its pinned host: {error:?}", id.0));
        }
        Ok(Self { name: name.into(), placements: Mutex::new(placements) })
    }

    /// The placements, for a read of the ledger or the home.
    pub fn with_placements<T>(&self, read: impl FnOnce(&mut Placements<L>) -> T) -> T {
        read(&mut self.placements.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl From<PlacementError> for EngineError {
    fn from(error: PlacementError) -> Self {
        match error {
            PlacementError::Write(error) => error.into(),
            PlacementError::Ledger(LedgerError::UnknownVessel(_)) => EngineError::Unknown,
            PlacementError::Ledger(error) => EngineError::Failed(format!("placement: {error:?}")),
        }
    }
}

impl<L> Lifecycles<VesselKind> for PlacementLifecycles<L>
where
    L: Log<Vessel> + Send,
{
    fn home(&self) -> &str {
        &self.name
    }

    fn get(&self, name: &str) -> Option<Resource<Lifecycle<VesselKind>>> {
        self.with_placements(|placements| placements.get(&VesselId::new(name)).cloned())
    }

    fn live(&self) -> Vec<Resource<Lifecycle<VesselKind>>> {
        self.with_placements(|placements| placements.home().resources().filter(|resource| resource.state().live()).cloned().collect())
    }

    fn request(&self, name: &str, spec: VesselSpec, at: Timestamp) -> Result<Resource<Lifecycle<VesselKind>>, EngineError> {
        let id = VesselId::new(name);
        let lease = spec.lease();
        self.with_placements(|placements| {
            // The ledger admits the pinned lease: its host joins with no dimensions, and its convoy
            // is uncapped for the pinned class.
            match placements.add_host(lease.host.clone(), Units::new()) {
                Ok(()) | Err(PlacementError::Ledger(LedgerError::DuplicateHost(_))) => {}
                Err(error) => return Err(error.into()),
            }
            // Only the pinned class's cap: a convoy's other classes, once a capacity model adds
            // them, keep theirs.
            placements.set_cap(lease.request.convoy.clone(), lease.request.class.clone(), u32::MAX);
            // A request a failed write left waiting is placed again, not queued twice.
            match placements.request(id.clone(), lease.request) {
                Ok(()) | Err(PlacementError::Ledger(LedgerError::DuplicateRequest(_))) => {}
                Err(error) => return Err(error.into()),
            }
            Ok(placements.place(&id, lease.host, at)?.clone())
        })
    }

    fn newest_launch(&self, of: &str) -> Option<Resource<Lifecycle<VesselKind>>> {
        self.with_placements(|placements| placements.home().newest_launch(of, None).cloned())
    }

    fn replace_launch(&self, _: &str, _: &str, _: VesselSpec, _: Timestamp) -> Result<String, EngineError> {
        Err(EngineError::Failed("a placement has no terminal launches".into()))
    }

    fn finish_launch_request(&self, _: &str, _: Timestamp) -> Result<(), EngineError> {
        Ok(())
    }

    fn step(&self, name: &str, step: &Step, at: Timestamp) -> Result<Resource<Lifecycle<VesselKind>>, EngineError> {
        let edge: LifecycleEdge = match step {
            Step::Realised { identity } => Realise { identity: identity.clone(), observed: Ref::new(name) }.into(),
            Step::Released => Release { teardown: Ref::new(name) }.into(),
            Step::Vanished { cause } => Vanish { cause: cause.clone(), observed: Ref::new(name) }.into(),
            Step::Attempted(outcome) => Attempted { outcome: outcome.clone() }.into(),
        };
        let world = evidence(name, step);
        self.with_placements(|placements| {
            let id = VesselId::new(name);
            if let Step::Attempted(outcome) = step {
                super::apply_attempt::<VesselKind, _, _>(
                    placements,
                    outcome,
                    |placements| placements.get(&id).map(|resource| resource.state().clone()),
                    |placements, edge| placements.apply(&id, edge, &world, at).map(|_| ()),
                )?;
            } else {
                placements.apply(&id, edge, &world, at)?;
            }
            placements
                .get(&id)
                .cloned()
                .ok_or_else(|| EngineError::Failed(format!("vessel `{name}` is missing after applying its lifecycle step")))
        })
    }
}

/// Wakes a vessel's convoy when the vessel's engine record changes, at any home: the convoy reads
/// each vessel's phase from the engine, so a record arriving from another home after the old
/// replica must wake it again (#3108). A home first heard from, or one restored from a snapshot,
/// names no records: each of its vessels' convoys is woken.
#[derive(Clone)]
pub struct VesselConvoyWake {
    service: std::sync::Arc<flotilla_schema_sqlite::stream::Service>,
}

impl VesselConvoyWake {
    pub fn new(service: std::sync::Arc<flotilla_schema_sqlite::stream::Service>) -> Self {
        Self { service }
    }
}

type VesselLifecycle = Resource<Lifecycle<VesselKind>>;

impl flotilla_store::controller::SecondaryWatch for VesselConvoyWake {
    type Primary = flotilla_resources::Convoy;

    fn clone_box(&self) -> Box<dyn flotilla_store::controller::SecondaryWatch<Primary = Self::Primary>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        _backend: flotilla_store::ResourceBackend,
        _namespace: String,
        sender: flotilla_store::controller::WorkQueueSender,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), flotilla_resources::ResourceError>> + Send>> {
        use flotilla_engine_store::follow::Followed;
        use flotilla_schema::Kind;

        Box::pin(async move {
            let mut watch = super::watch::EngineWatch::start(std::sync::Arc::clone(&self.service), [<VesselLifecycle as Kind>::CODE])
                .map_err(|error| flotilla_resources::ResourceError::other(error.to_string()))?;
            loop {
                let woken = watch
                    .next(|read, followed| {
                        let mut convoys = std::collections::BTreeSet::new();
                        for followed in followed {
                            match followed {
                                Followed::Changed(identity) => {
                                    let Some(id) = read.resolve(<VesselLifecycle as Kind>::CODE, &identity.name, &identity.home)? else {
                                        continue;
                                    };
                                    if let Some(loaded) = read.load::<VesselLifecycle>(id)? {
                                        convoys.insert(loaded.value.spec().convoy.name.clone());
                                    }
                                }
                                Followed::Rescan(home) => {
                                    let mut after = None;
                                    loop {
                                        let page = read.scan::<VesselLifecycle>(&home, after, 64)?;
                                        for id in page.ids {
                                            if let Some(loaded) = read.load::<VesselLifecycle>(id)? {
                                                convoys.insert(loaded.value.spec().convoy.name.clone());
                                            }
                                        }
                                        after = page.next;
                                        if after.is_none() {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        Ok(convoys)
                    })
                    .await;
                match woken {
                    Ok(convoys) => {
                        for convoy in convoys {
                            if sender.send(convoy).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    // The pass runs again at the next commit.
                    Err(error) => tracing::warn!(%error, "the vessel convoy wake could not read the log"),
                }
            }
        })
    }
}

/// The [`VesselRecords`] port over one decided store: this home's launches through its Vessel
/// home, the single writer of their edges, and every home's through keyed, indexed and scanned
/// reads.
pub struct EngineVessels {
    lifecycles: Arc<dyn Lifecycles<VesselKind>>,
    reads: StoreReads,
    homes: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    changes: tokio::sync::broadcast::Sender<VesselChange>,
}

/// How many changes a slow follower may fall behind by; one that lags rereads everything.
const CHANGES: usize = 256;

impl EngineVessels {
    /// The port over `store`, whose homes `homes` names.
    pub fn new(
        lifecycles: Arc<dyn Lifecycles<VesselKind>>,
        store: Arc<StoreHandle>,
        homes: impl Fn() -> Vec<String> + Send + Sync + 'static,
    ) -> Self {
        Self { lifecycles, reads: StoreReads::new(store), homes: Arc::new(homes), changes: tokio::sync::broadcast::channel(CHANGES).0 }
    }

    /// The port over `service`'s store and homes.
    pub fn over(lifecycles: Arc<dyn Lifecycles<VesselKind>>, service: Arc<Service>) -> Self {
        let store = service.store();
        Self::new(lifecycles, store, move || service.homes())
    }

    /// Follows the vessels' records in `service`'s log, at every home, telling
    /// [`VesselRecords::changes`] of each. It runs at the store's commits.
    pub fn follow(&self, service: Arc<Service>) -> Result<tokio::task::JoinHandle<()>, String> {
        let mut watch = super::watch::EngineWatch::start(service, [<Resource<Vessel> as Kind>::CODE]).map_err(|error| error.to_string())?;
        let changes = self.changes.clone();
        Ok(tokio::spawn(async move {
            loop {
                let followed = watch.next(|_, followed| Ok(followed.into_iter().map(vessel_change).collect::<Vec<_>>())).await;
                match followed {
                    // No follower is no reason to stop: one may subscribe later.
                    Ok(followed) => followed.into_iter().for_each(|change| drop(changes.send(change))),
                    Err(error) => tracing::warn!(%error, "the vessels' changes could not be read from the log; retrying"),
                }
            }
        }))
    }

    fn now() -> Timestamp {
        Timestamp::from_datetime(&Utc::now())
    }

    fn records(
        &self,
        what: &str,
        views: impl FnOnce(&mut Read<'_>) -> Result<Vec<View<VesselKind>>, ReadError>,
    ) -> Result<Vec<VesselRecord>, String> {
        self.reads
            .read(|read| views(read)?.into_iter().map(|view| record(read, view)).collect())
            .map_err(|error| format!("engine vessels {what}: {error}"))
    }

    fn scan(read: &mut Read<'_>, home: &str) -> Result<Vec<View<VesselKind>>, ReadError> {
        let mut views = Vec::new();
        let mut after = None;
        loop {
            let page = read.scan::<Resource<Vessel>>(home, after, 64)?;
            for id in page.ids {
                views.extend(view::<VesselKind>(read, id)?);
            }
            after = page.next;
            if after.is_none() {
                return Ok(views);
            }
        }
    }
}

impl VesselRecords for EngineVessels {
    fn cursor(&self) -> Option<Box<dyn flotilla_resources::lifecycle_reads::ChangeCursor<VesselChange>>> {
        let cursor = super::log_cursor::LogCursor::start(
            self.reads.clone(),
            Arc::clone(&self.homes),
            [<Resource<Vessel> as Kind>::CODE],
            vessel_change,
        );
        match cursor {
            Ok(cursor) => Some(Box::new(cursor)),
            Err(error) => {
                tracing::warn!(%error, "the log could not be followed from here; readers read afresh");
                None
            }
        }
    }

    fn home(&self) -> &str {
        self.lifecycles.home()
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<VesselChange> {
        self.changes.subscribe()
    }

    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<VesselRecord>, String> {
        let home = home.unwrap_or(self.home()).to_owned();
        self.reads
            .read(|read| keyed::<VesselKind>(read, name, &home)?.map(|view| record(read, view)).transpose())
            .map_err(|error| format!("engine Vessel/{name}: {error}"))
    }

    /// Read by name at this home, then at each other home: launch names are unique across homes.
    fn find(&self, name: &str) -> Result<Option<VesselRecord>, String> {
        let here = self.home().to_owned();
        let homes = (self.homes)();
        self.reads
            .read(|read| {
                for home in std::iter::once(&here).chain(homes.iter().filter(|home| **home != here)) {
                    if let Some(view) = keyed::<VesselKind>(read, name, home)? {
                        return record(read, view).map(Some);
                    }
                }
                Ok(None)
            })
            .map_err(|error| format!("engine Vessel/{name}: {error}"))
    }

    fn launches(&self, convoy: &str, convoy_home: &str, launch_of: &str, home: Option<&str>) -> Result<Vec<VesselRecord>, String> {
        let home = home.unwrap_or(self.home()).to_owned();
        let query = VesselSpec::by_launch(&ConvoyId::new(convoy, convoy_home), &launch_of.to_owned());
        let mut records = self.records(&format!("{launch_of} of convoy {convoy}@{convoy_home}"), |read| {
            Ok(indexed::<VesselKind, _>(read, &query)?.into_iter().filter(|view| view.home == home).collect())
        })?;
        records.sort_by_key(VesselRecord::launch_number);
        Ok(records)
    }

    fn newest(&self, convoy: &str, convoy_home: &str, launch_of: &str, home: Option<&str>) -> Result<Option<VesselRecord>, String> {
        let home = home.unwrap_or(self.home());
        let query = VesselSpec::by_launch(&ConvoyId::new(convoy, convoy_home), &launch_of.to_owned());
        self.reads
            .read(|read| match read.integer_index_keys_before(&query, home, i64::MAX, 1)?.first() {
                Some((id, _)) => view::<VesselKind>(read, *id)?.map(|view| record(read, view)).transpose(),
                None => Ok(None),
            })
            .map_err(|error| format!("engine Vessel {launch_of} of convoy {convoy}@{convoy_home}: {error}"))
    }

    fn at(&self, home: &str) -> Result<Vec<VesselRecord>, String> {
        self.records(&format!("at {home}"), |read| Self::scan(read, home))
    }

    fn every(&self) -> Result<Vec<VesselRecord>, String> {
        let homes = (self.homes)();
        self.records("at every home", |read| {
            let mut views = Vec::new();
            for home in &homes {
                views.extend(Self::scan(read, home)?);
            }
            Ok(views)
        })
    }

    fn request(&self, request: &VesselLaunchRequest) -> Result<String, String> {
        let launch_of = request.launch_of();
        let held = self.newest(&request.convoy, &request.convoy_home, &launch_of, None)?;
        if let Some(held) = held.filter(|held| !held.ended()) {
            return Ok(held.name);
        }
        let spec = VesselSpec {
            launch_of: launch_of.clone(),
            launch: 0,
            convoy: ConvoyId::new(request.convoy.clone(), request.convoy_home.clone()),
            class: VesselClass::new(PINNED_CLASS),
            shape: BTreeMap::new(),
            priority: Priority(0),
            host: Host::new(request.host.clone()),
        };
        self.lifecycles
            .request(&launch_of, spec, Self::now())
            .map(|launch| launch.name().to_owned())
            .map_err(|error| format!("request vessel {launch_of}: {error}"))
    }

    fn step(&self, name: &str, step: &VesselStep) -> Result<VesselRecord, String> {
        let step = match step {
            VesselStep::Realised { environment } => Step::Realised { identity: environment.clone() },
            VesselStep::Failed { cause, retryable } => {
                Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::External, cause), retryable: *retryable })
            }
            VesselStep::Lost { reason } => Step::Vanished { cause: VanishCause::Lost { reason: Some(reason.clone()) } },
            VesselStep::Broke { reason } => Step::Vanished { cause: VanishCause::Failed { reason: reason.clone() } },
            VesselStep::Released => Step::Released,
        };
        self.lifecycles.step(name, &step, Self::now()).map_err(|error| format!("vessel {name}: {error}"))?;
        self.get(name, None)?.ok_or_else(|| format!("vessel {name} is missing after its step"))
    }
}

/// `view` as a reader sees it.
fn record(read: &mut Read<'_>, view: View<VesselKind>) -> Result<VesselRecord, ReadError> {
    let time = |at: Timestamp| at.to_datetime().unwrap_or(DateTime::UNIX_EPOCH);
    let requested_at = requested_at::<Resource<Vessel>>(read, &view.name, &view.home)?.map(time).unwrap_or(DateTime::UNIX_EPOCH);
    let request = view.request;
    Ok(VesselRecord {
        convoy: request.convoy.name,
        convoy_home: request.convoy.home,
        launch_of: request.launch_of,
        host: request.host.0,
        requested_at,
        since: time(view.since),
        reading: view.judgement.reading(),
        name: view.name,
        home: view.home,
    })
}

/// What a followed write of the kinds tells readers.
fn vessel_change(followed: Followed) -> VesselChange {
    match followed {
        Followed::Changed(identity) => VesselChange::Changed { name: identity.name, home: identity.home },
        Followed::Rescan(_) => VesselChange::Rescanned,
    }
}
