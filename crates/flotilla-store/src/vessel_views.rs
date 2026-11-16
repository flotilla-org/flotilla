//! Vessels in the old Vessel shape, read from the engine's launches (#3108).
//!
//! The engine holds each launch ([`VesselRecords`]); readers not yet converted to records see each
//! launch as a [`Vessel`] read view named by the launch, joined with its convoy, the environment
//! held for it, its terminal sessions and its convoy's checkouts at its home. Nothing here writes:
//! launches are requested and stepped through [`VesselRecords`] by their home.
//!
//! Installed as the backend's [`EngineRecords`] for `Vessel`, it serves the old store's typed
//! reads, lists and watches of the kind; its writes are refused.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use futures::{stream::BoxStream, StreamExt};

use flotilla_protocol::{LifecycleReading, NodeId};
use flotilla_resources::{
    checkout_records::{latest_launches, CheckoutChange, CheckoutHolder, CheckoutRecord},
    convoy_records::ConvoyChange,
    engine_records::{EngineObject, EngineRecords},
    environment_records::{EnvironmentChange, EnvironmentRecord},
    lifecycle_reads::{ChangeCursor, LifecycleReads},
    terminal_records::{TerminalChange, TerminalRecord},
    vessel_records::{VesselChange, VesselJoins, VesselRecord, VesselRecords},
    Convoy, ReadResourceObject, ReadWatchEvent, ResourceError, ResourceObject, ResourceProvenance, Vessel,
};

use crate::{
    held_listing::{ticks, watched, Unwatched, Watched},
    ResourceBackend, WeakResourceBackend,
};

const KIND: &str = "Vessel";
/// Where a vessel is seen, for a refused write.
const EXPLAIN: &str = "see it with `flotilla convoy explain`";

/// Read views of the vessels in one namespace.
///
/// Its listing is kept from pass to pass while its changes are watched: each pass reads and
/// composes again only the launches the changes since the last pass touched. A launch that ended
/// and left the listing is read by name alone, never composed again by a pass.
#[derive(Clone)]
pub struct VesselViews {
    /// Weak: the backend holds these views as its Vessel port.
    backend: WeakResourceBackend,
    namespace: String,
    reads: Arc<dyn LifecycleReads>,
    listing: Arc<tokio::sync::Mutex<Listing>>,
    dirt: Arc<std::sync::Mutex<Dirt>>,
}

/// What one launch is joined with, read for it.
struct Joined {
    environment: Option<EnvironmentRecord>,
    terminals: Vec<TerminalRecord>,
    checkouts: Vec<CheckoutRecord>,
}

/// A work at a home: its home, its convoy and the convoy's home, and what its launches are of.
type Work = (String, String, String, String);

/// Each work's newest launch at every home, as the last pass composed it.
#[derive(Default)]
struct Listing {
    /// Whether `works` is as of the last pass: false until a pass reads every launch afresh.
    current: bool,
    /// Where the next pass reads the records' changes since this one; none before the first.
    cursors: Option<Cursors>,
    works: BTreeMap<Work, Entry>,
    /// The work of each launch in `works`, by its name.
    names: BTreeMap<String, Work>,
}

/// A work's newest launch.
struct Entry {
    record: VesselRecord,
    /// What its convoy's checkouts are held for, while it is listed.
    holder: Option<CheckoutHolder>,
    /// Its view, while it is listed.
    view: Option<EngineObject>,
}

/// What the changes since the last pass named. The records' changes are read from the log at each
/// pass; the convoys' are told by their watch, so are known only while one is watched, and with
/// none watched a pass reads every launch afresh.
#[derive(Default)]
struct Dirt {
    /// Change streams being watched.
    watching: usize,
    /// Anything may have changed: a home's log was read afresh, or changes were missed.
    all: bool,
    vessels: BTreeSet<(String, String)>,
    environments: BTreeSet<String>,
    terminals: BTreeSet<(String, String)>,
    checkouts: BTreeSet<String>,
    convoys: BTreeSet<String>,
}

/// Cursors over the changes of the records a vessel's view is composed from.
struct Cursors {
    vessels: Box<dyn ChangeCursor<VesselChange>>,
    environments: Box<dyn ChangeCursor<EnvironmentChange>>,
    terminals: Box<dyn ChangeCursor<TerminalChange>>,
    checkouts: Box<dyn ChangeCursor<CheckoutChange>>,
    /// The convoys' launches, where their log is held: a pass then sees a convoy written before it,
    /// which their watch tells only later. Their notes, held in memory, still come by the watch.
    convoys: Option<Box<dyn ChangeCursor<ConvoyChange>>>,
}

impl Cursors {
    /// Cursors from now on, where `reads` holds a log of each.
    fn start(reads: &dyn LifecycleReads) -> Result<Option<Self>, String> {
        let (Some(vessels), Some(environments), Some(terminals), Some(checkouts)) =
            (reads.vessels()?.cursor(), reads.environments()?.cursor(), reads.terminals()?.cursor(), reads.checkouts()?.cursor())
        else {
            return Ok(None);
        };
        let convoys = reads.convoys().ok().and_then(|convoys| convoys.cursor());
        Ok(Some(Self { vessels, environments, terminals, checkouts, convoys }))
    }

    /// Marks in `dirt` what changed since the last pass; a change that could not be read marks
    /// everything.
    fn pass(&mut self, dirt: &mut Dirt) {
        dirt.all |= read(&mut self.vessels, |change| match change {
            VesselChange::Changed { name, home } => {
                dirt.vessels.insert((name, home));
                false
            }
            VesselChange::Rescanned => true,
        });
        dirt.all |= read(&mut self.environments, |change| match change {
            EnvironmentChange::Changed(name) => {
                dirt.environments.insert(name);
                false
            }
            EnvironmentChange::Rescanned => true,
        });
        dirt.all |= read(&mut self.terminals, |change| match change {
            TerminalChange::Changed { name, home } => {
                dirt.terminals.insert((name, home));
                false
            }
            TerminalChange::Rescanned => true,
        });
        dirt.all |= read(&mut self.checkouts, |change| match change {
            CheckoutChange::Changed(name) => {
                dirt.checkouts.insert(name);
                false
            }
            CheckoutChange::Rescanned => true,
        });
        if let Some(convoys) = &mut self.convoys {
            dirt.all |= read(convoys, |change| match change {
                ConvoyChange::Changed { name, .. } => {
                    dirt.convoys.insert(name);
                    false
                }
                ConvoyChange::Rescanned => true,
            });
        }
    }
}

/// Hands each change `cursor` reads to `mark`; whether any asked for everything, or the changes
/// could not be read.
fn read<C>(cursor: &mut Box<dyn ChangeCursor<C>>, mut mark: impl FnMut(C) -> bool) -> bool {
    match cursor.pass() {
        Ok(changes) => changes.into_iter().fold(false, |all, change| all | mark(change)),
        Err(_) => true,
    }
}

impl Dirt {
    fn is_clean(&self) -> bool {
        !self.all
            && self.vessels.is_empty()
            && self.environments.is_empty()
            && self.terminals.is_empty()
            && self.checkouts.is_empty()
            && self.convoys.is_empty()
    }
}

impl Watched for Dirt {
    fn unwatch(&mut self) {
        self.watching -= 1;
        self.all = true;
    }
}

/// What a pass read for the launches its dirt named.
#[derive(Default)]
struct Touched {
    /// Launches that changed.
    vessels: Vec<VesselRecord>,
    /// Launches whose environment or terminal sessions changed.
    names: BTreeSet<String>,
    /// Holders whose checkouts changed.
    holders: BTreeSet<CheckoutHolder>,
}

impl VesselViews {
    pub fn new(backend: ResourceBackend, namespace: &str, reads: Arc<dyn LifecycleReads>) -> Self {
        Self {
            backend: backend.downgrade(),
            namespace: namespace.to_owned(),
            reads,
            listing: Arc::default(),
            dirt: Arc::new(std::sync::Mutex::new(Dirt { all: true, ..Dirt::default() })),
        }
    }

    fn backend(&self) -> Result<ResourceBackend, ResourceError> {
        self.backend.upgrade().ok_or_else(|| ResourceError::other("the resource backend serving these vessels is gone"))
    }

    /// Each work's newest launch at every home, as read sources: this home's are local, other
    /// homes' replicas of their home. A released launch is listed no more, as an old Vessel was
    /// deleted at its convoy's teardown; one that ended otherwise is listed while its convoy is
    /// live, its end shown.
    async fn listed(&self) -> Result<Vec<EngineObject>, ResourceError> {
        let mut listing = self.listing.lock().await;
        let (mut dirt, watched) = {
            let mut held = self.dirt.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let watching = held.watching;
            let dirt = std::mem::take(&mut *held);
            held.watching = watching;
            (dirt, watching > 0)
        };
        let cursors = listing.cursors.take();
        let (cursors, dirt) = self
            .blocking(move |reads| match cursors {
                Some(mut cursors) => {
                    cursors.pass(&mut dirt);
                    Ok((Some(cursors), dirt))
                }
                // Started before the full read: a change after it is read again at the next pass.
                None => {
                    dirt.all = true;
                    Ok((Cursors::start(reads)?, dirt))
                }
            })
            .await?;
        let follows = cursors.is_some();
        listing.cursors = cursors;
        let result = if dirt.all || !listing.current || !watched || !follows {
            self.fresh(&mut listing, watched && follows).await
        } else if !dirt.is_clean() {
            self.pass(&mut listing, dirt).await
        } else {
            Ok(())
        };
        if let Err(error) = result {
            // What it holds is not known to be current: the next pass reads afresh.
            listing.current = false;
            return Err(error);
        }
        Ok(listing.works.values().filter_map(|entry| entry.view.clone()).collect())
    }

    /// Reads and composes every launch afresh into `listing`, kept from pass to pass when `keep`.
    async fn fresh(&self, listing: &mut Listing, keep: bool) -> Result<(), ResourceError> {
        let records = newest(self.blocking(|reads| reads.vessels()?.every()).await?);
        let entries = self.entries(records).await?;
        listing.current = keep;
        listing.works.clear();
        listing.names.clear();
        for (work, entry) in entries {
            listing.names.insert(entry.record.name.clone(), work.clone());
            listing.works.insert(work, entry);
        }
        Ok(())
    }

    /// Composes again the launches `dirt` touched in `listing`, and takes in their new launches.
    async fn pass(&self, listing: &mut Listing, dirt: Dirt) -> Result<(), ResourceError> {
        let Dirt { vessels, environments, terminals, checkouts, convoys, .. } = dirt;
        let touched = self
            .blocking(move |reads| {
                let mut touched = Touched::default();
                if !vessels.is_empty() {
                    let records = reads.vessels()?;
                    for (name, home) in &vessels {
                        touched.vessels.extend(records.get(name, Some(home))?);
                    }
                }
                if !environments.is_empty() {
                    let records = reads.environments()?;
                    for name in &environments {
                        touched.names.extend(records.get(name, None)?.and_then(|environment| environment.vessel));
                    }
                }
                if !terminals.is_empty() {
                    let records = reads.terminals()?;
                    for (name, home) in &terminals {
                        touched.names.extend(records.get(name, Some(home))?.and_then(|terminal| terminal.vessel));
                    }
                }
                if !checkouts.is_empty() {
                    let records = reads.checkouts()?;
                    for name in &checkouts {
                        touched.holders.extend(records.get(name, None)?.and_then(|checkout| checkout.holder));
                    }
                }
                Ok(touched)
            })
            .await?;
        let mut again: BTreeSet<Work> = touched.names.iter().filter_map(|name| listing.names.get(name).cloned()).collect();
        again.extend(listing.works.iter().filter_map(|(work, entry)| {
            let held = entry.holder.as_ref().is_some_and(|holder| touched.holders.contains(holder));
            (held || convoys.contains(&entry.record.convoy)).then(|| work.clone())
        }));
        let mut records: BTreeMap<Work, VesselRecord> = BTreeMap::new();
        for record in touched.vessels {
            let work = work_of(&record);
            if listing.works.get(&work).is_some_and(|held| held.record.launch_number() > record.launch_number()) {
                continue;
            }
            again.remove(&work);
            records.insert(work, record);
        }
        records.extend(again.into_iter().filter_map(|work| listing.works.get(&work).map(|entry| (work, entry.record.clone()))));
        for (work, entry) in self.entries(records.into_values().collect()).await? {
            if let Some(replaced) = listing.works.get(&work).filter(|held| held.record.name != entry.record.name) {
                listing.names.remove(&replaced.record.name);
            }
            listing.names.insert(entry.record.name.clone(), work.clone());
            listing.works.insert(work, entry);
        }
        Ok(())
    }

    /// `records` as entries of the listing: only those listed are joined and composed.
    async fn entries(&self, records: Vec<VesselRecord>) -> Result<Vec<(Work, Entry)>, ResourceError> {
        let live: Vec<_> = records.iter().filter(|record| record.reading != LifecycleReading::Released).cloned().collect();
        let convoys = self.convoys(&live).await?;
        let listed = |record: &VesselRecord| {
            record.reading != LifecycleReading::Released
                && (!record.ended()
                    || convoys
                        .get(&(record.convoy.clone(), record.convoy_home.clone()))
                        .is_some_and(|convoy| convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal())))
        };
        let (shown, hidden): (Vec<_>, Vec<_>) = records.into_iter().partition(|record| listed(record));
        let here = self.vessels().map_err(ResourceError::other)?.home().to_owned();
        let holders: Vec<_> = shown
            .iter()
            .map(|record| {
                convoys
                    .get(&(record.convoy.clone(), record.convoy_home.clone()))
                    .and_then(flotilla_resources::convoy::engine::checkout_holder)
            })
            .collect();
        let views = self.compose_with(shown.clone(), convoys).await?;
        let mut entries = Vec::with_capacity(views.len() + hidden.len());
        for ((record, holder), view) in shown.into_iter().zip(holders).zip(views) {
            let home = match &view.provenance {
                ResourceProvenance::Local => here.clone(),
                ResourceProvenance::Replica { origin_root, .. } => origin_root.as_str().to_owned(),
            };
            let view = engine_object(view, home).map_err(ResourceError::other)?;
            entries.push((work_of(&record), Entry { record, holder, view: Some(view) }));
        }
        entries.extend(hidden.into_iter().map(|record| (work_of(&record), Entry { record, holder: None, view: None })));
        Ok(entries)
    }

    /// Launch `name` at this home, else at whichever home holds it.
    pub async fn find(&self, name: &str) -> Result<Option<ReadResourceObject<Vessel>>, ResourceError> {
        let name = name.to_owned();
        let record = self.blocking(move |reads| reads.vessels()?.find(&name)).await?;
        Ok(self.compose(record.into_iter().collect()).await?.pop())
    }

    /// `records` as read views.
    pub async fn compose(&self, records: Vec<VesselRecord>) -> Result<Vec<ReadResourceObject<Vessel>>, ResourceError> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let convoys = self.convoys(&records).await?;
        self.compose_with(records, convoys).await
    }

    /// `records` as read views, with `convoys`, the convoys they serve.
    async fn compose_with(
        &self,
        records: Vec<VesselRecord>,
        convoys: BTreeMap<(String, String), ResourceObject<Convoy>>,
    ) -> Result<Vec<ReadResourceObject<Vessel>>, ResourceError> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        // A convoy's checkouts are held for the engine launch its generation became; one whose
        // convoy is unknown or not yet admitted is joined with none.
        let wanted: Vec<_> = records
            .iter()
            .map(|record| {
                let convoy = convoys.get(&(record.convoy.clone(), record.convoy_home.clone()));
                (record.clone(), convoy.and_then(flotilla_resources::convoy::engine::checkout_holder))
            })
            .collect();
        let joined = self
            .blocking(move |reads| Ok(wanted.iter().map(|(record, holder)| join(reads, record, holder.as_ref())).collect::<Vec<_>>()))
            .await?;
        let here = self.reads.vessels().map_err(ResourceError::other)?.home().to_owned();
        Ok(records
            .into_iter()
            .zip(joined)
            .map(|(record, joined)| {
                let joins = VesselJoins {
                    convoy: convoys.get(&(record.convoy.clone(), record.convoy_home.clone())),
                    environment: joined.environment.as_ref(),
                    terminals: &joined.terminals,
                    checkouts: &joined.checkouts,
                };
                let object = record.read_view(&self.namespace, joins);
                let provenance = if record.home == here {
                    ResourceProvenance::Local
                } else {
                    ResourceProvenance::Replica { origin_root: NodeId::new(record.home.clone()), last_synced_at: record.since }
                };
                ReadResourceObject { object, provenance }
            })
            .collect())
    }

    /// The convoys `records` serve, by name and home, local or replicated. One that cannot be
    /// found is left out: its vessels read without it.
    async fn convoys(&self, records: &[VesselRecord]) -> Result<BTreeMap<(String, String), ResourceObject<Convoy>>, ResourceError> {
        let convoys = self.backend()?.including_replicas::<Convoy>(&self.namespace);
        let mut held = BTreeMap::new();
        for record in records {
            let key = (record.convoy.clone(), record.convoy_home.clone());
            if held.contains_key(&key) {
                continue;
            }
            match convoys.get(&record.convoy).await {
                Ok(source) => {
                    held.insert(key, source.object);
                }
                Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(held)
    }

    async fn blocking<T: Send + 'static>(
        &self,
        read: impl FnOnce(&dyn LifecycleReads) -> Result<T, String> + Send + 'static,
    ) -> Result<T, ResourceError> {
        let reads = Arc::clone(&self.reads);
        tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || read(reads.as_ref())))
            .await
            .map_err(|error| ResourceError::other(error.to_string()))?
            .map_err(ResourceError::other)
    }

    fn vessels(&self) -> Result<&dyn VesselRecords, String> {
        self.reads.vessels()
    }
}

impl std::fmt::Debug for VesselViews {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("VesselViews").field("namespace", &self.namespace).finish_non_exhaustive()
    }
}

/// The newest launch of each work at each home: a launch a later one replaced reads only by its
/// name.
fn newest(records: Vec<VesselRecord>) -> Vec<VesselRecord> {
    let mut newest = BTreeMap::<Work, VesselRecord>::new();
    for record in records {
        let work = work_of(&record);
        if newest.get(&work).is_none_or(|held| held.launch_number() < record.launch_number()) {
            newest.insert(work, record);
        }
    }
    newest.into_values().collect()
}

fn work_of(record: &VesselRecord) -> Work {
    (record.home.clone(), record.convoy.clone(), record.convoy_home.clone(), record.launch_of.clone())
}

/// What `record` is joined with, as its records read here, its convoy's checkouts held for
/// `holder`. A join that cannot be read is left
/// out: what it would show is unknown, not bad.
fn join(reads: &dyn LifecycleReads, record: &VesselRecord, holder: Option<&CheckoutHolder>) -> Joined {
    let environment = reads.environments().and_then(|environments| match record.environment() {
        Some(identity) => environments.get(identity, Some(&record.home)),
        None => Ok(environments.held(&record.name)?.into_iter().find(|environment| !environment.ended())),
    });
    let terminals = reads.terminals().and_then(|terminals| terminals.of_vessel(&record.name));
    let checkouts = match holder {
        Some(holder) => reads.checkouts().and_then(|checkouts| checkouts.held(holder)),
        None => Ok(Vec::new()),
    };
    let unknown = |what: &str, error: &String| tracing::debug!(vessel = %record.name, %error, "a vessel's {what} could not be read");
    Joined {
        environment: environment.inspect_err(|error| unknown("environment", error)).ok().flatten(),
        terminals: terminals.inspect_err(|error| unknown("terminal sessions", error)).unwrap_or_default(),
        checkouts: checkouts
            .inspect_err(|error| unknown("checkouts", error))
            .map(|checkouts| latest_launches(checkouts).into_values().filter(|checkout| checkout.home == record.home).collect())
            .unwrap_or_default(),
    }
}

/// `source` as the engine port hands it to the old store: a change of the launch or of what it is
/// joined with moves its version.
fn engine_object(source: ReadResourceObject<Vessel>, home: String) -> Result<EngineObject, String> {
    crate::held_listing::view_object(source.object, home)
}

#[async_trait::async_trait]
impl EngineRecords for VesselViews {
    fn kinds(&self) -> &'static [&'static str] {
        &[KIND]
    }

    /// The launches' kinds and those of everything their views join, the convoys among them.
    fn codes(&self, _kind: &str) -> Vec<flotilla_resources::engine_records::KindCode> {
        use flotilla_resources::lifecycle_reads::{record_codes, RecordSet};
        let sets = [RecordSet::Vessels, RecordSet::Environments, RecordSet::Terminals, RecordSet::Checkouts, RecordSet::Convoys];
        record_codes(self.reads.as_ref(), &sets)
    }

    /// The notes of the convoys the views join.
    fn unlogged_changes(&self, _kind: &str) -> Option<u64> {
        Some(self.reads.convoys().map_or(0, |convoys| convoys.memory().changed()))
    }

    fn home(&self) -> String {
        self.vessels().map(|vessels| vessels.home().to_owned()).unwrap_or_default()
    }

    async fn get(&self, _kind: &str, name: &str) -> Result<Option<EngineObject>, String> {
        let here = self.home();
        let Some(source) = self.find(name).await.map_err(|error| error.to_string())? else { return Ok(None) };
        let home = match &source.provenance {
            ResourceProvenance::Local => here,
            ResourceProvenance::Replica { origin_root, .. } => origin_root.as_str().to_owned(),
        };
        engine_object(source, home).map(Some)
    }

    /// Each work's newest launch at each home; launch names are unique, so each is listed once.
    async fn list(&self, _kind: &str) -> Result<Vec<EngineObject>, String> {
        self.listed().await.map_err(|error| error.to_string())
    }

    async fn sources(&self, _kind: &str) -> Result<Vec<EngineObject>, String> {
        self.listed().await.map_err(|error| error.to_string())
    }

    /// The one launch `name` names: launch names are unique across homes.
    async fn copies(&self, kind: &str, name: &str) -> Result<Vec<EngineObject>, String> {
        Ok(self.get(kind, name).await?.into_iter().collect())
    }

    async fn apply(
        &self,
        _kind: &str,
        name: &str,
        _labels: &BTreeMap<String, String>,
        _annotations: &BTreeMap<String, String>,
        _spec: serde_json::Value,
    ) -> Result<EngineObject, String> {
        Err(format!("vessel {name} is launched by its convoy's home, not written; {EXPLAIN}"))
    }

    async fn set_status(&self, _kind: &str, name: &str, _status: serde_json::Value) -> Result<EngineObject, String> {
        Err(format!("vessel {name}'s status is its launch's, not written; {EXPLAIN}"))
    }

    async fn delete(&self, _kind: &str, name: &str) -> Result<(), String> {
        Err(format!("vessel {name} is released by its home when its convoy no longer needs it, not deleted; {EXPLAIN}"))
    }

    /// A tick at each change of a launch, or of the convoys, environments, terminal sessions and
    /// checkouts its view is joined with, at any home. The next pass composes again what each
    /// change touched.
    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let convoys = self
            .backend()
            .map_err(|error| error.to_string())?
            .including_replicas::<Convoy>(&self.namespace)
            .watch()
            .await
            .map_err(|error| error.to_string())?;
        {
            let mut held = self.dirt.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            held.watching += 1;
            // The convoys' changes before this are unknown.
            held.all = true;
        }
        let dirt = Arc::clone(&self.dirt);
        let unwatched = Unwatched(Arc::clone(&dirt));
        let streams = vec![
            ticks(self.vessels()?.changes()),
            ticks(self.reads.environments()?.changes()),
            ticks(self.reads.terminals()?.changes()),
            ticks(self.reads.checkouts()?.changes()),
            convoys
                .map(move |event| {
                    let mut dirt = dirt.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    match event {
                        Ok(ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read) | ReadWatchEvent::Deleted(read)) => {
                            dirt.convoys.insert(read.object.metadata.name);
                        }
                        Ok(ReadWatchEvent::DeletedByName { tombstone, .. }) => {
                            dirt.convoys.insert(tombstone.name);
                        }
                        Err(_) => dirt.all = true,
                    }
                })
                .boxed(),
        ];
        Ok(watched(streams, unwatched))
    }
}
