//! Convoys in the old Convoy shape, read from the engine's launches (#3108).
//!
//! The engine holds each launch of a convoy ([`ConvoyRecords`]); readers not yet converted to
//! records see each launch as a [`Convoy`] read view named by the launch, its phases derived from
//! the launch, its works' Vessels and its seats' sessions, and, at the launch's own home, joined
//! with what that home observes of it. Nothing here writes: the convoy verbs write edges, and the
//! home its notes.
//!
//! The views are held, not composed on each read: a follower recomposes a launch's view when its
//! record, its home's notes, one of its works' Vessel launches or one of their sessions changes,
//! and every read is of the views held. Each view is composed from records alone, never from
//! another kind's views: the Vessel views read these, so these read the Vessel records. A read
//! first passes the records' cursors over the log and reads in the note changes not yet read, so
//! a writer reads back what it has written.
//!
//! Installed as the backend's [`EngineRecords`] for `Convoy`, it serves the old store's typed
//! reads, lists and watches of the kind; its writes are refused.

use std::{
    collections::{BTreeMap, BTreeSet},
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, PoisonError,
    },
};

use chrono::DateTime;
use futures::{stream::BoxStream, StreamExt};

use flotilla_protocol::{LifecycleReading, NodeId};
use flotilla_resources::{
    convoy::view::VesselReading,
    convoy_records::{ConvoyChange, ConvoyJoins, ConvoyNotes, ConvoyRecord, ConvoyRecords, WorkFacts},
    engine_records::{EngineObject, EngineRecords},
    lifecycle_reads::{ChangeCursor, LifecycleReads},
    terminal_records::{newest_launches, TerminalChange},
    vessel_records::{interrupted_message, VesselChange},
    Convoy, ReadResourceObject, ResourceError, ResourceObject, ResourceProvenance,
};

use crate::held_listing::ticks;

const KIND: &str = "Convoy";

/// Read views of the convoys in one namespace.
#[derive(Clone)]
pub struct ConvoyViews {
    namespace: String,
    reads: Arc<dyn LifecycleReads>,
    held: Arc<Held>,
}

/// The views held, by `(home, launch)`, and what each joins, kept current by one follower.
#[derive(Default)]
struct Held {
    state: tokio::sync::Mutex<Option<HeldViews>>,
    ticks: Mutex<Option<tokio::sync::broadcast::Sender<()>>>,
    /// Views converted into the engine port's objects, the cost of its reads.
    converted: AtomicU64,
}

struct HeldViews {
    views: BTreeMap<(String, String), ReadResourceObject<Convoy>>,
    /// Where the next read takes the records' changes from; none where no log is held, and each
    /// read then reads every launch afresh.
    cursors: Option<Cursors>,
    /// This home's note changes not yet read into the views.
    notes: tokio::sync::broadcast::Receiver<String>,
    /// The launches the cursors have passed whose views are not yet composed again. A read
    /// dropped while it composes leaves them here for the next, since the cursors will not name
    /// them again.
    owed: BTreeSet<(String, String)>,
    /// A read of every launch was begun and not finished.
    refill_owed: bool,
    /// The launches held as unreadable, by `(home, launch)`, at the version last told.
    unreadable: BTreeMap<(String, String), u64>,
}

/// Cursors over the changes of the records a convoy's view is composed from.
struct Cursors {
    convoys: Box<dyn ChangeCursor<ConvoyChange>>,
    vessels: Box<dyn ChangeCursor<VesselChange>>,
    terminals: Box<dyn ChangeCursor<TerminalChange>>,
}

impl Cursors {
    /// Cursors from now on, where `reads` holds a log of each.
    fn start(reads: &dyn LifecycleReads) -> Result<Option<Self>, String> {
        let (Some(convoys), Some(vessels), Some(terminals)) =
            (reads.convoys()?.cursor(), reads.vessels()?.cursor(), reads.terminals()?.cursor())
        else {
            return Ok(None);
        };
        Ok(Some(Self { convoys, vessels, terminals }))
    }

    /// The launches, by `(home, name)`, whose records or joins changed since the last pass; none
    /// when anything may have.
    fn pass(&mut self, reads: &dyn LifecycleReads) -> Result<Option<BTreeSet<(String, String)>>, String> {
        let mut launches = BTreeSet::new();
        for change in self.convoys.pass()? {
            match change {
                ConvoyChange::Changed { name, home } => drop(launches.insert((home, name))),
                ConvoyChange::Rescanned => return Ok(None),
            }
        }
        let vessels = reads.vessels()?;
        // The convoy launch a Vessel launch, or the Vessel launch a session, is held for.
        let mut held_for = |vessel: &str, home: &str| -> Result<(), String> {
            let record = match vessels.get(vessel, Some(home))? {
                Some(record) => Some(record),
                None => vessels.get(vessel, None)?,
            };
            launches.extend(record.map(|record| (record.convoy_home, record.convoy)));
            Ok(())
        };
        for change in self.vessels.pass()? {
            match change {
                VesselChange::Changed { name, home } => held_for(&name, &home)?,
                VesselChange::Rescanned => return Ok(None),
            }
        }
        let terminals = reads.terminals()?;
        for change in self.terminals.pass()? {
            match change {
                TerminalChange::Changed { name, home } => {
                    if let Some(vessel) = terminals.get(&name, Some(&home))?.and_then(|session| session.vessel) {
                        held_for(&vessel, &home)?;
                    }
                }
                TerminalChange::Rescanned => return Ok(None),
            }
        }
        Ok(Some(launches))
    }
}

/// How many view changes a slow watcher may fall behind by before it reads everything again.
const TICKS: usize = 256;

impl ConvoyViews {
    pub fn new(namespace: &str, reads: Arc<dyn LifecycleReads>) -> Self {
        Self { namespace: namespace.to_owned(), reads, held: Arc::default() }
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
    /// Every change written before the read is read into them first.
    async fn with_held<T>(&self, read: impl FnOnce(&HeldViews) -> T) -> Result<T, ResourceError> {
        let mut state = self.held.state.lock().await;
        if state.is_none() {
            // Follow before filling, so no change between the fill and the first pass is missed.
            let wakes = self.wakes()?;
            let notes = self.convoys().map_err(ResourceError::other)?.memory().subscribe();
            let cursors = self.blocking(Cursors::start).await?;
            let mut held = HeldViews {
                views: BTreeMap::new(),
                cursors,
                notes,
                owed: BTreeSet::new(),
                refill_owed: false,
                unreadable: BTreeMap::new(),
            };
            self.refill(&mut held).await?;
            *state = Some(held);
            tokio::spawn(self.clone().follow(wakes));
        }
        let held = state.as_mut().expect("filled above");
        if let Err(error) = self.pass(held).await {
            tracing::warn!(%error, "convoy views: a change did not read; reading them all again");
            self.refill(held).await?;
        }
        Ok(read(held))
    }

    /// What wakes the follower: a change of a launch's record or notes, of a Vessel launch or of a
    /// session, at any home. Each wake passes the cursors, which read what changed.
    fn wakes(&self) -> Result<BoxStream<'static, ()>, ResourceError> {
        let convoys = self.convoys().map_err(ResourceError::other)?;
        let vessels = self.reads.vessels().map_err(ResourceError::other)?;
        let terminals = self.reads.terminals().map_err(ResourceError::other)?;
        Ok(futures::stream::select_all([
            ticks(convoys.changes()),
            ticks(convoys.memory().subscribe()),
            ticks(vessels.changes()),
            ticks(terminals.changes()),
        ])
        .boxed())
    }

    /// Recomposes the views each wake's changes touch, and ticks the watchers.
    async fn follow(self, mut wakes: BoxStream<'static, ()>) {
        while wakes.next().await.is_some() {
            let mut state = self.held.state.lock().await;
            let Some(held) = state.as_mut() else { return };
            if let Err(error) = self.pass(held).await {
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

    /// Reads into the views the changes since the last pass: the records' from their cursors, and
    /// this home's notes.
    async fn pass(&self, held: &mut HeldViews) -> Result<(), ResourceError> {
        use tokio::sync::broadcast::error::TryRecvError;
        let Some(mut cursors) = held.cursors.take() else { return self.refill(held).await };
        let (cursors, launches) = self
            .blocking(move |reads| {
                let launches = cursors.pass(reads);
                Ok((cursors, launches))
            })
            .await?;
        held.cursors = Some(cursors);
        let Some(launches) = launches.map_err(ResourceError::other)? else { return self.refill(held).await };
        held.owed.extend(launches);
        let here = self.convoys().map_err(ResourceError::other)?.home().to_owned();
        loop {
            match held.notes.try_recv() {
                Ok(name) => drop(held.owed.insert((here.clone(), name))),
                Err(TryRecvError::Lagged(_)) => return self.refill(held).await,
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        self.recompose(held).await
    }

    /// Composes the launches owed again from their records; one with no record leaves the views.
    /// Each stays owed until its view is held.
    async fn recompose(&self, held: &mut HeldViews) -> Result<(), ResourceError> {
        if held.refill_owed {
            return self.refill(held).await;
        }
        while let Some((home, name)) = held.owed.first().cloned() {
            let (key_home, key_name) = (home.clone(), name.clone());
            let record = self.blocking(move |reads| reads.convoys()?.get(&key_name, Some(&key_home))).await?;
            let composed = match record {
                Some(record) => Some(self.compose(vec![record]).await?),
                None => None,
            };
            let key = (home, name);
            held.remove(&key);
            if let Some(composed) = composed {
                held.insert(composed);
            }
            held.owed.remove(&key);
        }
        self.convoys().map_err(ResourceError::other)?.memory().set_unreadable(held.unreadable.len() as u64);
        Ok(())
    }

    /// Reads every launch again. Until it has, the whole views are owed.
    async fn refill(&self, held: &mut HeldViews) -> Result<(), ResourceError> {
        held.refill_owed = true;
        let listing = self.blocking(|reads| reads.convoys()?.every()).await?;
        let mut composed = self.compose(listing.records).await?;
        let here = self.convoys().map_err(ResourceError::other)?.home().to_owned();
        // A launch that cannot be read stays listed, with why; when another home's last changed
        // is not known.
        composed.extend(listing.unreadable.iter().map(|launch| {
            let provenance = if launch.home == here {
                ResourceProvenance::Local
            } else {
                ResourceProvenance::Replica { origin_root: NodeId::new(launch.home.clone()), last_synced_at: DateTime::UNIX_EPOCH }
            };
            Composed {
                home: launch.home.clone(),
                view: ReadResourceObject { object: launch.read_view(&self.namespace), provenance },
                unreadable: Some((launch.version, launch.reason.clone())),
            }
        }));
        held.views.clear();
        let listed = listing.unreadable.iter().map(|launch| (launch.home.clone(), launch.name.clone())).collect::<BTreeSet<_>>();
        held.unreadable.retain(|key, _| listed.contains(key));
        held.insert(composed);
        held.owed.clear();
        held.refill_owed = false;
        self.convoys().map_err(ResourceError::other)?.memory().set_unreadable(held.unreadable.len() as u64);
        Ok(())
    }

    /// `records` as read views, each joined with its works' Vessel launches and their sessions.
    async fn compose(&self, records: Vec<ConvoyRecord>) -> Result<Vec<Composed>, ResourceError> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let (records, joined) = self
            .blocking(move |reads| {
                let convoys = reads.convoys()?;
                let here = convoys.home().to_owned();
                let joined = records
                    .iter()
                    .map(|record| {
                        let notes = (record.home == here).then(|| convoys.memory().get(&record.name));
                        Ok((notes.clone(), join(reads, record, notes.as_ref())?))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Ok((records, joined))
            })
            .await?;
        let here = self.convoys().map_err(ResourceError::other)?.home().to_owned();
        Ok(records
            .into_iter()
            .zip(joined)
            .map(|(record, (notes, joined))| {
                let working = record.working(&joined.live, notes.as_ref());
                let object =
                    record.read_view(&self.namespace, ConvoyJoins { works: &joined.works, working: &working, notes: notes.as_ref() });
                let provenance = if record.home == here {
                    ResourceProvenance::Local
                } else {
                    let synced = record.ended.as_ref().map_or(record.admitted_at, |(_, at)| *at);
                    ResourceProvenance::Replica { origin_root: NodeId::new(record.home.clone()), last_synced_at: synced }
                };
                Composed { home: record.home.clone(), view: ReadResourceObject { object, provenance }, unreadable: None }
            })
            .collect())
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
        self.engine_objects(sources)
    }

    /// Every home's launch `name`, as the engine port's objects: only those are converted.
    async fn engine_copies(&self, name: &str) -> Result<Vec<EngineObject>, String> {
        let name = name.to_owned();
        let copies = self
            .with_held(move |held| {
                held.views.iter().filter(|((_, held_name), _)| *held_name == name).map(|(_, view)| view.clone()).collect()
            })
            .await
            .map_err(|error| error.to_string())?;
        self.engine_objects(copies)
    }

    /// `sources` as the engine port's objects, each at its home.
    fn engine_objects(&self, sources: Vec<ReadResourceObject<Convoy>>) -> Result<Vec<EngineObject>, String> {
        self.held.converted.fetch_add(sources.len() as u64, Ordering::Relaxed);
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

    /// How many views the engine port has converted into objects, for read-cost tests.
    pub fn views_converted(&self) -> u64 {
        self.held.converted.load(Ordering::Relaxed)
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
        created_at: object.metadata.creation_timestamp,
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
            Some(source) => {
                self.held.converted.fetch_add(1, Ordering::Relaxed);
                engine_object(source, home).map(Some)
            }
            // Another home's launch of this name reads where this home holds none: the lexically
            // first home's, as a listing chooses.
            None => Ok(self.engine_copies(name).await?.into_iter().min_by(|a, b| a.home.cmp(&b.home))),
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

    async fn copies(&self, _kind: &str, name: &str) -> Result<Vec<EngineObject>, String> {
        self.engine_copies(name).await
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
        Ok(crate::held_listing::ticks(ticks))
    }
}

/// A launch's view, held at `home`.
struct Composed {
    home: String,
    view: ReadResourceObject<Convoy>,
    /// For a launch that cannot be read, its version and why.
    unreadable: Option<(u64, String)>,
}

impl HeldViews {
    fn insert(&mut self, composed: Vec<Composed>) {
        for Composed { home, view, unreadable } in composed {
            let key = (home, view.object.metadata.name.clone());
            match unreadable {
                Some((version, reason)) => {
                    // Told once for each version of the launch, not at each read of them all.
                    if self.unreadable.insert(key.clone(), version) != Some(version) {
                        tracing::warn!(launch = %key.1, home = %key.0, version, %reason, "convoy launch cannot be read");
                    }
                }
                None => drop(self.unreadable.remove(&key)),
            }
            self.views.insert(key, view);
        }
    }

    fn remove(&mut self, key: &(String, String)) {
        self.views.remove(key);
        self.unreadable.remove(key);
    }
}

/// What one launch is joined with.
#[derive(Default)]
struct Joined {
    /// Each work's newest Vessel launch, by work. A work with none has never had one.
    works: BTreeMap<String, WorkFacts>,
    /// The seats `(work, role)` whose newest session launch is realised.
    live: BTreeSet<(String, String)>,
}

/// What `record` is joined with, as the Vessel and terminal session records read here, its home's
/// `notes` telling which crew agents are wanted running.
fn join(reads: &dyn LifecycleReads, record: &ConvoyRecord, notes: Option<&ConvoyNotes>) -> Result<Joined, String> {
    let launches = reads.vessels()?.of_convoy(&record.name)?;
    let terminals = reads.terminals()?;
    let mut joined = Joined::default();
    for requirement in &record.admission.workflow_snapshot.vessels {
        // A work replaced at another home has a launch at each: the newest requested serves it.
        let Some(vessel) = launches
            .iter()
            .filter(|vessel| vessel.convoy_home == record.home && vessel.work() == requirement.name)
            .max_by_key(|vessel| (vessel.requested_at, vessel.launch_number()))
        else {
            continue;
        };
        let sessions = terminals.of_vessel(&vessel.name)?;
        for session in newest_launches(sessions.iter().cloned()).into_values() {
            if matches!(session.reading, LifecycleReading::Ready { .. }) {
                joined.live.insert((requirement.name.clone(), session.role));
            }
        }
        let interrupted = vessel.interrupted_roles(requirement, &sessions, |index| {
            requirement.crew.get(index).is_some_and(|member| record.wanted_running(&requirement.name, &member.role, notes))
        });
        let facts = WorkFacts {
            reading: VesselReading::At(vessel.phase_with(&interrupted)),
            ready_at: vessel.environment().is_some().then_some(vessel.since),
            started_at: Some(vessel.requested_at),
            finished_at: None,
            message: interrupted_message(&interrupted).or_else(|| vessel.reading.reason()),
        };
        joined.works.insert(requirement.name.clone(), facts);
    }
    Ok(joined)
}
