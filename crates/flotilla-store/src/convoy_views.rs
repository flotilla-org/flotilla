//! Convoys in the old Convoy shape, read from the engine's launches (#3108).
//!
//! The engine holds each launch of a convoy ([`ConvoyRecords`]); readers not yet converted to
//! records see each launch as a [`Convoy`] read view named by the launch, its phases derived from
//! the launch, its works' Vessels and its seats' sessions, and, at the launch's own home, joined
//! with what that home observes of it. Nothing here writes: the convoy verbs write edges, and the
//! home its notes.
//!
//! The views are held, not composed on each read: a follower recomposes a launch's view when its
//! record, its home's notes, one of its works' Vessels or one of its seats' sessions changes, and
//! every read is of the views held. A read first brings the records up to the store and reads in
//! the launch and note changes not yet read, so a writer reads back what it has written. Installed as the backend's [`EngineRecords`] for `Convoy`, it
//! serves the old store's typed reads, lists and watches of the kind; its writes are refused.

use std::{
    collections::{BTreeMap, BTreeSet},
    hash::{Hash, Hasher},
    sync::{Arc, Mutex, PoisonError},
};

use futures::{stream::BoxStream, StreamExt};

use flotilla_protocol::{LifecycleReading, NodeId};
use flotilla_resources::{
    convoy::view::VesselReading,
    convoy_records::{ConvoyChange, ConvoyJoins, ConvoyRecord, ConvoyRecords, WorkFacts},
    engine_records::{EngineObject, EngineRecords},
    lifecycle_reads::LifecycleReads,
    terminal_records::{newest_launches, TerminalChange},
    vessel_resource_name, Convoy, ReadResourceObject, ReadWatchEvent, ResourceError, ResourceObject, ResourceProvenance, Vessel,
    VesselPhase,
};

use crate::ResourceBackend;

const KIND: &str = "Convoy";

/// Read views of the convoys in one namespace.
#[derive(Clone)]
pub struct ConvoyViews {
    backend: ResourceBackend,
    namespace: String,
    reads: Arc<dyn LifecycleReads>,
    held: Arc<Held>,
}

/// The views held, by `(home, launch)`, and what each joins, kept current by one follower.
#[derive(Default)]
struct Held {
    state: tokio::sync::Mutex<Option<HeldViews>>,
    ticks: Mutex<Option<tokio::sync::broadcast::Sender<()>>>,
}

#[derive(Default)]
struct HeldViews {
    views: BTreeMap<(String, String), ReadResourceObject<Convoy>>,
    /// The launches each work Vessel is joined into, by Vessel name.
    by_vessel: BTreeMap<String, BTreeSet<(String, String)>>,
    /// The launches' record and notes changes not yet read into the views.
    pending: Option<Pending>,
}

/// The launches' record and notes changes, drained by whoever holds the views: a reader once the
/// records are brought up to the store, so it reads what the store holds, and the follower when
/// one arrives.
struct Pending {
    launches: tokio::sync::broadcast::Receiver<ConvoyChange>,
    notes: tokio::sync::broadcast::Receiver<String>,
}

/// How many view changes a slow watcher may fall behind by before it reads everything again.
const TICKS: usize = 256;

impl ConvoyViews {
    pub fn new(backend: ResourceBackend, namespace: &str, reads: Arc<dyn LifecycleReads>) -> Self {
        Self { backend, namespace: namespace.to_owned(), reads, held: Arc::default() }
    }

    /// Every launch at every home, as read sources: this home's are local, other homes' replicas
    /// of their home.
    pub async fn sources(&self) -> Result<Vec<ReadResourceObject<Convoy>>, ResourceError> {
        self.with_held(|held| held.views.values().cloned().collect()).await
    }

    /// This home's launches.
    pub async fn here(&self) -> Result<Vec<ResourceObject<Convoy>>, ResourceError> {
        let here = self.convoys().map_err(ResourceError::other)?.home().to_owned();
        self.with_held(move |held| held.views.iter().filter(|((home, _), _)| *home == here).map(|(_, view)| view.object.clone()).collect())
            .await
    }

    /// Launch `name` at this home, as a resolver's `get` reads it.
    pub async fn get(&self, name: &str) -> Result<ResourceObject<Convoy>, ResourceError> {
        self.find(name, None).await?.map(|source| source.object).ok_or_else(|| ResourceError::not_found(name))
    }

    /// Launch `name` held at `home`, or at this home when `None`.
    pub async fn find(&self, name: &str, home: Option<&str>) -> Result<Option<ReadResourceObject<Convoy>>, ResourceError> {
        let home = match home {
            Some(home) => home.to_owned(),
            None => self.convoys().map_err(ResourceError::other)?.home().to_owned(),
        };
        let key = (home, name.to_owned());
        self.with_held(move |held| held.views.get(&key).cloned()).await
    }

    /// The newest launch of `of` in `project` held at `home`, or at this home when `None`.
    pub async fn newest(&self, of: &str, project: &str, home: Option<&str>) -> Result<Option<ReadResourceObject<Convoy>>, ResourceError> {
        let (of, project, home) = (of.to_owned(), project.to_owned(), home.map(str::to_owned));
        let record = self.blocking(move |reads| reads.convoys()?.newest(&of, &project, home.as_deref())).await?;
        let Some(record) = record else { return Ok(None) };
        self.find(&record.name, Some(&record.home)).await
    }

    /// Reads the views held, filling them on first use and starting the follower that keeps them.
    /// Every launch and note written before the read is read into them first.
    async fn with_held<T>(&self, read: impl FnOnce(&HeldViews) -> T) -> Result<T, ResourceError> {
        let mut state = self.held.state.lock().await;
        if state.is_none() {
            // Follow before filling, so no change between the fill and the first event is missed.
            let events = self.events().await?;
            let convoys = self.convoys().map_err(ResourceError::other)?;
            let pending = Pending { launches: convoys.changes(), notes: convoys.memory().subscribe() };
            let mut held = HeldViews { pending: Some(pending), ..HeldViews::default() };
            self.refill(&mut held).await?;
            *state = Some(held);
            tokio::spawn(self.clone().follow(events));
        }
        let held = state.as_mut().expect("filled above");
        self.blocking(|reads| reads.convoys()?.catch_up()).await?;
        if let Err(error) = self.drain(held).await {
            tracing::warn!(%error, "convoy views: a change did not read; reading them all again");
            self.refill(held).await?;
        }
        Ok(read(held))
    }

    /// The changes the views follow: each launch's record and notes, its seats' sessions and its
    /// works' Vessels.
    async fn events(&self) -> Result<BoxStream<'static, Followed>, ResourceError> {
        let convoys = self.convoys().map_err(ResourceError::other)?;
        let terminals = self.reads.terminals().map_err(ResourceError::other)?;
        let vessels = self.backend.including_replicas::<Vessel>(&self.namespace).watch().await?;
        let vessels = vessels
            .map(|event| match event {
                Ok(ReadWatchEvent::Added(source) | ReadWatchEvent::Modified(source) | ReadWatchEvent::Deleted(source)) => {
                    Followed::Vessel(source.object.metadata.name)
                }
                Ok(ReadWatchEvent::DeletedByName { tombstone, .. }) => Followed::Vessel(tombstone.name),
                Err(_) => Followed::Everything,
            })
            .boxed();
        Ok(futures::stream::select_all([
            // Launch and note changes are read from the views' own receivers; these wake the follower.
            received(convoys.changes(), |_| Followed::Pending),
            received(convoys.memory().subscribe(), |_| Followed::Pending),
            received(terminals.changes(), |change| match change {
                TerminalChange::Changed { name, home } => Followed::Session { name, home },
                TerminalChange::Rescanned => Followed::Everything,
            }),
            vessels,
        ])
        .boxed())
    }

    /// Recomposes the views each followed change touches, and ticks the watchers.
    async fn follow(self, mut events: BoxStream<'static, Followed>) {
        while let Some(event) = events.next().await {
            let mut state = self.held.state.lock().await;
            let Some(held) = state.as_mut() else { return };
            if let Err(error) = self.apply(held, event).await {
                // The views are read again in full; a failed read leaves them as they were.
                tracing::warn!(%error, "convoy views: a change did not read; reading them all again");
                if let Err(error) = self.refill(held).await {
                    tracing::warn!(%error, "convoy views: reading them all again failed");
                }
            }
            drop(state);
            if let Some(ticks) = self.held.ticks.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                let _ = ticks.send(());
            }
        }
    }

    async fn apply(&self, held: &mut HeldViews, event: Followed) -> Result<(), ResourceError> {
        let launches: Vec<(String, String)> = match event {
            Followed::Everything => return self.refill(held).await,
            Followed::Pending => return self.drain(held).await,
            Followed::Vessel(vessel) => held.by_vessel.get(&vessel).into_iter().flatten().cloned().collect(),
            Followed::Session { name, home } => {
                let vessel =
                    self.blocking(move |reads| Ok(reads.terminals()?.get(&name, Some(&home))?.and_then(|record| record.vessel))).await?;
                vessel.and_then(|vessel| held.by_vessel.get(&vessel)).into_iter().flatten().cloned().collect()
            }
        };
        self.recompose(held, launches).await
    }

    /// The launch and note changes not yet read, read into the views.
    async fn drain(&self, held: &mut HeldViews) -> Result<(), ResourceError> {
        use tokio::sync::broadcast::error::TryRecvError;
        let Some(pending) = held.pending.as_mut() else { return Ok(()) };
        let here = self.convoys().map_err(ResourceError::other)?.home().to_owned();
        let mut launches = BTreeSet::new();
        let mut everything = false;
        loop {
            match pending.launches.try_recv() {
                Ok(ConvoyChange::Changed { name, home }) => drop(launches.insert((home, name))),
                Ok(ConvoyChange::Rescanned) | Err(TryRecvError::Lagged(_)) => everything = true,
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        loop {
            match pending.notes.try_recv() {
                Ok(name) => drop(launches.insert((here.clone(), name))),
                Err(TryRecvError::Lagged(_)) => everything = true,
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        if everything {
            return self.refill(held).await;
        }
        self.recompose(held, launches.into_iter().collect()).await
    }

    /// Composes `launches` again from their records; one with no record leaves the views.
    async fn recompose(&self, held: &mut HeldViews, launches: Vec<(String, String)>) -> Result<(), ResourceError> {
        for (home, name) in launches {
            let (key_home, key_name) = (home.clone(), name.clone());
            let record = self.blocking(move |reads| reads.convoys()?.get(&key_name, Some(&key_home))).await?;
            held.remove(&(home.clone(), name.clone()));
            if let Some(record) = record {
                let composed = self.compose(vec![record]).await?;
                held.insert(composed);
            }
        }
        Ok(())
    }

    /// Reads every launch again.
    async fn refill(&self, held: &mut HeldViews) -> Result<(), ResourceError> {
        let records = self.blocking(|reads| reads.convoys()?.every()).await?;
        let composed = self.compose(records).await?;
        *held = HeldViews { pending: held.pending.take(), ..HeldViews::default() };
        held.insert(composed);
        Ok(())
    }

    /// `records` as read views, each with the Vessels its works join.
    async fn compose(&self, records: Vec<ConvoyRecord>) -> Result<Vec<Composed>, ResourceError> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let vessels = self.vessels(&records).await?;
        let live = self.live_sessions(&records).await?;
        let convoys = self.reads.convoys().map_err(ResourceError::other)?;
        let here = convoys.home().to_owned();
        let none = BTreeSet::new();
        Ok(records
            .into_iter()
            .map(|record| {
                let local = record.home == here;
                let notes = local.then(|| convoys.memory().get(&record.name));
                let works: BTreeMap<String, WorkFacts> = record
                    .admission
                    .workflow_snapshot
                    .vessels
                    .iter()
                    .filter_map(|work| {
                        let vessel = vessels.get(&vessel_resource_name(&record.name, &work.name))?;
                        Some((work.name.clone(), vessel.clone()))
                    })
                    .collect();
                let joined = record.admission.workflow_snapshot.vessels.iter().map(|work| work_vessel(&record, &work.name)).collect();
                let working = record.working(live.get(&record.name).unwrap_or(&none), notes.as_ref());
                let object = record.read_view(&self.namespace, ConvoyJoins { works: &works, working: &working, notes: notes.as_ref() });
                let provenance = if local {
                    ResourceProvenance::Local
                } else {
                    let synced = record.ended.as_ref().map_or(record.admitted_at, |(_, at)| *at);
                    ResourceProvenance::Replica { origin_root: NodeId::new(record.home.clone()), last_synced_at: synced }
                };
                Composed { home: record.home.clone(), view: ReadResourceObject { object, provenance }, vessels: joined }
            })
            .collect())
    }

    /// The Vessels of `records`' works, by Vessel name, local or replicated.
    async fn vessels(&self, records: &[ConvoyRecord]) -> Result<BTreeMap<String, WorkFacts>, ResourceError> {
        let vessels = self.backend.including_replicas::<Vessel>(&self.namespace);
        let mut held = BTreeMap::new();
        for record in records {
            for work in &record.admission.workflow_snapshot.vessels {
                let name = work_vessel(record, &work.name);
                match vessels.get(&name).await {
                    Ok(source) => {
                        let object = source.object;
                        let status = object.status.as_ref();
                        let phase = status.map_or(VesselPhase::Pending, |status| status.phase);
                        let facts = WorkFacts {
                            reading: VesselReading::At(phase),
                            ready_at: status.and_then(|status| status.ready_at),
                            started_at: status.and_then(|status| status.started_at),
                            finished_at: None,
                            message: status.and_then(|status| status.message.clone()),
                        };
                        held.insert(name, facts);
                    }
                    // A work with no Vessel has never had one.
                    Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(held)
    }

    /// The seats `(work, role)` of each of `records`, by launch, whose newest session launch is
    /// realised.
    async fn live_sessions(&self, records: &[ConvoyRecord]) -> Result<BTreeMap<String, BTreeSet<(String, String)>>, ResourceError> {
        let wanted: Vec<(String, String, String)> = records
            .iter()
            .flat_map(|record| {
                record
                    .admission
                    .workflow_snapshot
                    .vessels
                    .iter()
                    .map(|work| (record.name.clone(), work.name.clone(), work_vessel(record, &work.name)))
            })
            .collect();
        self.blocking(move |reads| {
            let terminals = reads.terminals()?;
            let mut live: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
            for (convoy, work, vessel) in wanted {
                for record in newest_launches(terminals.of_vessel(&vessel)?).into_values() {
                    if matches!(record.reading, LifecycleReading::Ready { .. }) {
                        live.entry(convoy.clone()).or_default().insert((work.clone(), record.role));
                    }
                }
            }
            Ok(live)
        })
        .await
    }

    async fn blocking<T: Send + 'static>(
        &self,
        read: impl FnOnce(&dyn LifecycleReads) -> Result<T, String> + Send + 'static,
    ) -> Result<T, ResourceError> {
        let reads = Arc::clone(&self.reads);
        tokio::task::spawn_blocking(move || read(reads.as_ref()))
            .await
            .map_err(|error| ResourceError::other(error.to_string()))?
            .map_err(ResourceError::other)
    }

    fn convoys(&self) -> Result<&dyn ConvoyRecords, String> {
        self.reads.convoys()
    }

    async fn engine_sources(&self) -> Result<Vec<EngineObject>, String> {
        let sources = self.sources().await.map_err(|error| error.to_string())?;
        sources
            .into_iter()
            .map(|source| {
                let home = match &source.provenance {
                    ResourceProvenance::Local => self.convoys()?.home().to_owned(),
                    ResourceProvenance::Replica { origin_root, .. } => origin_root.as_str().to_owned(),
                };
                engine_object(source, home)
            })
            .collect()
    }
}

impl std::fmt::Debug for ConvoyViews {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ConvoyViews").field("namespace", &self.namespace).finish_non_exhaustive()
    }
}

/// `source` as the engine port hands it to the old store: its version a digest of what it shows,
/// so a watcher sees each change of the launch, of what it joins, or of its home's notes.
fn engine_object(source: ReadResourceObject<Convoy>, home: String) -> Result<EngineObject, String> {
    let object = source.object;
    let spec = serde_json::to_value(&object.spec).map_err(|error| error.to_string())?;
    let status = object.status.as_ref().map(serde_json::to_value).transpose().map_err(|error| error.to_string())?;
    let mut digest = std::hash::DefaultHasher::new();
    (&object.metadata.resource_version, spec.to_string(), status.as_ref().map(ToString::to_string), &object.metadata.labels)
        .hash(&mut digest);
    Ok(EngineObject {
        name: object.metadata.name,
        home,
        version: digest.finish(),
        labels: object.metadata.labels,
        annotations: object.metadata.annotations,
        spec,
        status,
    })
}

#[async_trait::async_trait]
impl EngineRecords for ConvoyViews {
    fn kinds(&self) -> &'static [&'static str] {
        &[KIND]
    }

    fn home(&self) -> String {
        self.convoys().map(|convoys| convoys.home().to_owned()).unwrap_or_default()
    }

    async fn get(&self, _kind: &str, name: &str) -> Result<Option<EngineObject>, String> {
        let home = self.home();
        match self.find(name, None).await.map_err(|error| error.to_string())? {
            Some(source) => engine_object(source, home).map(Some),
            // Another home's launch of this name reads where this home holds none.
            None => Ok(self.list(KIND).await?.into_iter().find(|object| object.name == name)),
        }
    }

    /// Each launch, this home's where it holds one of the name, else the lexically first home's.
    async fn list(&self, _kind: &str) -> Result<Vec<EngineObject>, String> {
        let home = self.home();
        let mut chosen = BTreeMap::<String, EngineObject>::new();
        for object in self.engine_sources().await? {
            let keep = chosen.get(&object.name).is_some_and(|held| held.home == home || (object.home != home && held.home <= object.home));
            if !keep {
                chosen.insert(object.name.clone(), object);
            }
        }
        Ok(chosen.into_values().collect())
    }

    async fn sources(&self, _kind: &str) -> Result<Vec<EngineObject>, String> {
        self.engine_sources().await
    }

    async fn apply(
        &self,
        _kind: &str,
        name: &str,
        _labels: &BTreeMap<String, String>,
        _annotations: &BTreeMap<String, String>,
        _spec: serde_json::Value,
    ) -> Result<EngineObject, String> {
        Err(format!("convoy {name} is admitted on the engine, not written"))
    }

    async fn set_status(&self, _kind: &str, name: &str, _status: serde_json::Value) -> Result<EngineObject, String> {
        Err(format!("convoy {name}'s status is derived from the engine's records; it is not written"))
    }

    async fn delete(&self, _kind: &str, name: &str) -> Result<(), String> {
        Err(format!("convoy {name} ends on the engine; it is not deleted"))
    }

    /// A tick each time the views held change: a launch's record at any home, what this home
    /// observes of one, or a Vessel or terminal session its works join.
    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        // Hold the views first, so the follower that ticks is running.
        self.with_held(|_| ()).await.map_err(|error| error.to_string())?;
        let ticks = self
            .held
            .ticks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(|| tokio::sync::broadcast::channel(TICKS).0)
            .subscribe();
        Ok(received(ticks, |_| ()))
    }
}

/// What the views follow.
enum Followed {
    /// A launch's record, a record about it or this home's notes on it changed: read the views'
    /// pending changes.
    Pending,
    /// Terminal session launch `name` held at `home` changed.
    Session { name: String, home: String },
    /// The Vessel of this name changed.
    Vessel(String),
    /// Anything may have changed, or a follower fell behind: read everything again.
    Everything,
}

/// A launch's view and the Vessels its works join.
struct Composed {
    home: String,
    view: ReadResourceObject<Convoy>,
    vessels: Vec<String>,
}

impl HeldViews {
    fn insert(&mut self, composed: Vec<Composed>) {
        for Composed { home, view, vessels } in composed {
            let key = (home, view.object.metadata.name.clone());
            for vessel in vessels {
                self.by_vessel.entry(vessel).or_default().insert(key.clone());
            }
            self.views.insert(key, view);
        }
    }

    fn remove(&mut self, key: &(String, String)) {
        self.views.remove(key);
        self.by_vessel.retain(|_, launches| {
            launches.remove(key);
            !launches.is_empty()
        });
    }
}

/// The Vessel work `work` of `record` joins.
fn work_vessel(record: &ConvoyRecord, work: &str) -> String {
    vessel_resource_name(&record.name, work)
}

/// Each message on `changes` as `map` makes it; a lag is everything changed.
fn received<T: Clone + Send + 'static, E: Send + 'static + From<Lagged>>(
    changes: tokio::sync::broadcast::Receiver<T>,
    map: impl Fn(T) -> E + Send + Sync + 'static,
) -> BoxStream<'static, E> {
    futures::stream::unfold((changes, map), |(mut changes, map)| async move {
        match changes.recv().await {
            Ok(change) => Some((map(change), (changes, map))),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Some((E::from(Lagged), (changes, map))),
            Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
        }
    })
    .boxed()
}

/// A follower fell behind its channel.
struct Lagged;

impl From<Lagged> for Followed {
    fn from(_: Lagged) -> Self {
        Followed::Everything
    }
}

impl From<Lagged> for () {
    fn from(_: Lagged) -> Self {}
}
