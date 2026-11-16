//! Terminal sessions in the old TerminalSession shape, read from the engine's launches (#3108).
//!
//! The engine holds each launch ([`TerminalRecords`]); readers not yet converted to records see
//! each session's newest launch per home as a [`TerminalSession`] read view, joined with its holder
//! Vessel's convoy and labels and, at the launch's own home, with what that home observes of it.
//! Nothing here writes: requests and renewals go through [`TerminalRecords`].
//!
//! Installed as the backend's [`KindRecords`] for `TerminalSession`, it serves the typed reads,
//! lists and watches of the kind; its writes are refused.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use futures::{stream::BoxStream, StreamExt};

use flotilla_protocol::{LifecycleReading, NodeId};
use flotilla_resources::{
    engine_records::{KindRecords, LogPosition, Record, Since, UnloggedJournal},
    environment_records::{EnvironmentChange, EnvironmentRecord},
    lifecycle_reads::{ChangeCursor, LifecycleReads},
    terminal_records::{newest_launches, TerminalChange, TerminalHolderView, TerminalRecord, TerminalRecords},
    vessel_records::VesselChange,
    vessel_seats::VesselSeats,
    ReadResourceObject, ResourceError, ResourceObject, ResourceProvenance, TerminalSession, TerminalSessionSpec, TerminalSessionStatus,
};

use crate::held_listing::{drained, ticks};

/// Read views of the terminal sessions in one namespace.
///
/// Its listing is kept from pass to pass: each pass reads from the log what changed since the
/// last, before it reads anything else, and composes again only the sessions that touched. A
/// read port that holds no log reads every launch afresh.
#[derive(Clone)]
pub struct TerminalViews {
    namespace: String,
    reads: Arc<dyn LifecycleReads>,
    listing: Arc<tokio::sync::Mutex<Listing>>,
}

/// A session at a home, as [`newest_launches`] keys it: its home and its session name.
type Session = (String, String);

/// Each session's newest launch at every home, as the last pass composed it.
#[derive(Default)]
struct Listing {
    /// Where the next pass reads the changes since this one; none before the first.
    cursors: Option<Cursors>,
    sessions: BTreeMap<Session, Entry>,
    /// The sessions whose view changed, in this run: what a reading resumes from.
    changed: UnloggedJournal,
}

impl Listing {
    /// Holds `entries` in place of every session held.
    fn replace(&mut self, entries: Vec<(Session, Entry)>) {
        let entries: BTreeMap<Session, Entry> = entries.into_iter().collect();
        let gone: Vec<Session> = self.sessions.keys().filter(|session| !entries.contains_key(*session)).cloned().collect();
        for session in gone {
            if self.sessions.remove(&session).is_some_and(|entry| entry.view.is_some()) {
                self.changed.record([session.1]);
            }
        }
        self.extend(entries);
    }

    /// Holds each of `entries`, counting each whose view changed.
    fn extend(&mut self, entries: impl IntoIterator<Item = (Session, Entry)>) {
        for (session, entry) in entries {
            let shown = |entry: &Entry| entry.view.as_ref().map(|view| view.version);
            if self.sessions.get(&session).map(shown) != Some(shown(&entry)) {
                self.changed.record([session.1.clone()]);
            }
            self.sessions.insert(session, entry);
        }
    }

    /// Where a reader stands that has read the sessions held.
    fn read(&self) -> LogPosition {
        LogPosition { homes: BTreeMap::new(), unlogged: Some(self.changed.read()) }
    }

    /// How far the listing has read: the log and this home's observations, by its cursors.
    fn position(&self) -> u64 {
        self.cursors.as_ref().map_or(0, Cursors::position)
    }

    /// `view`, of launch `launch` of `session`, at the version it is handed out at: the held
    /// view's while it is of the same launch and shows the same.
    fn version(&self, session: &Session, launch: &str, view: &Record<TerminalSession>, position: u64) -> Result<u64, String> {
        let held = self.sessions.get(session).and_then(|held| Some((held.record.name == launch, held.view.as_ref()?)));
        let same = held.is_some_and(|(same_launch, held)| same_launch && crate::held_listing::shows_the_same(view, held));
        crate::held_listing::version(held.map(|(_, held)| held.version), same, position)
    }

    /// `entries`, composed again, with the versions their views are handed out at.
    fn stamped(&self, entries: Vec<(Session, Entry)>) -> Result<Vec<(Session, Entry)>, ResourceError> {
        let position = self.position();
        entries
            .into_iter()
            .map(|(session, mut entry)| {
                if let Some(view) = entry.view.as_mut() {
                    view.version = self.version(&session, &entry.record.name, view, position).map_err(ResourceError::other)?;
                }
                Ok((session, entry))
            })
            .collect()
    }
}

/// A session's newest launch.
struct Entry {
    record: TerminalRecord,
    /// Its view, while it is listed: a released launch is not.
    view: Option<Record<TerminalSession>>,
    /// Whether its holder vessel could not be read when it was composed: its holder is unknown,
    /// not absent, so each pass composes it again until it is read.
    unresolved: bool,
}

/// What changed since the last pass.
#[derive(Default)]
struct Dirt {
    /// Anything may have changed: a home's log was read afresh, or changes could not be read.
    all: bool,
    /// Launches that changed, by name and home.
    terminals: BTreeSet<(String, String)>,
    /// Launches whose observation at this home changed.
    observed: BTreeSet<String>,
    environments: BTreeSet<String>,
    vessels: BTreeSet<String>,
}

/// Cursors over the changes of the records a session's view is composed from. A read port that
/// serves no environments or vessels joins none, so follows none.
struct Cursors {
    terminals: Box<dyn ChangeCursor<TerminalChange>>,
    environments: Option<Box<dyn ChangeCursor<EnvironmentChange>>>,
    vessels: Option<Box<dyn ChangeCursor<VesselChange>>>,
    /// What this home observes is in memory, told as it changes.
    observed: tokio::sync::broadcast::Receiver<String>,
    /// How many observation changes have been read.
    observed_read: u64,
}

impl Cursors {
    /// Cursors from now on, where `reads` holds a log of its sessions.
    fn start(reads: &dyn LifecycleReads) -> Result<Option<Self>, String> {
        let terminals = reads.terminals()?;
        let Some(cursor) = terminals.cursor() else { return Ok(None) };
        Ok(Some(Self {
            terminals: cursor,
            environments: reads.environments().ok().and_then(|environments| environments.cursor()),
            vessels: reads.vessels().ok().and_then(|vessels| vessels.cursor()),
            observed: terminals.observations().subscribe(),
            observed_read: 0,
        }))
    }

    /// How far the cursors have read: the log, and this home's observations.
    fn position(&self) -> u64 {
        [self.terminals.position(), self.observed_read]
            .into_iter()
            .chain(self.environments.as_ref().map(|environments| environments.position()))
            .chain(self.vessels.as_ref().map(|vessels| vessels.position()))
            .fold(0, u64::saturating_add)
    }

    /// What changed since the last pass; a change that could not be read marks everything.
    fn pass(&mut self) -> Dirt {
        let mut dirt = Dirt::default();
        match self.terminals.pass() {
            Ok(changes) => changes.into_iter().for_each(|change| match change {
                TerminalChange::Changed { name, home } => {
                    dirt.terminals.insert((name, home));
                }
                TerminalChange::Rescanned => dirt.all = true,
            }),
            Err(_) => dirt.all = true,
        }
        if let Some(environments) = self.environments.as_mut() {
            match environments.pass() {
                Ok(changes) => changes.into_iter().for_each(|change| match change {
                    EnvironmentChange::Changed(name) => {
                        dirt.environments.insert(name);
                    }
                    EnvironmentChange::Rescanned => dirt.all = true,
                }),
                Err(_) => dirt.all = true,
            }
        }
        if let Some(vessels) = self.vessels.as_mut() {
            match vessels.pass() {
                Ok(changes) => changes.into_iter().for_each(|change| match change {
                    VesselChange::Changed { name, .. } => {
                        dirt.vessels.insert(name);
                    }
                    VesselChange::Rescanned => dirt.all = true,
                }),
                Err(_) => dirt.all = true,
            }
        }
        let observed_read = &mut self.observed_read;
        dirt.all |= drained(&mut self.observed, |name| {
            *observed_read = observed_read.saturating_add(1);
            dirt.observed.insert(name);
            false
        });
        dirt
    }
}

impl Dirt {
    fn is_clean(&self) -> bool {
        !self.all && self.terminals.is_empty() && self.observed.is_empty() && self.environments.is_empty() && self.vessels.is_empty()
    }
}

impl TerminalViews {
    pub fn new(namespace: &str, reads: Arc<dyn LifecycleReads>) -> Self {
        Self { namespace: namespace.to_owned(), reads, listing: Arc::default() }
    }

    /// Each session's newest launch at every home, as the engine port lists it: this home's
    /// local, other homes' replicas of their home. A released launch is listed no more.
    async fn listed(&self) -> Result<Vec<Record<TerminalSession>>, ResourceError> {
        self.listed_where(|_| true).await
    }

    /// The listed records `keep` selects: only those are copied out of the listing.
    async fn listed_where(&self, keep: impl Fn(&Record<TerminalSession>) -> bool) -> Result<Vec<Record<TerminalSession>>, ResourceError> {
        Ok(self.positioned_where(keep, |_| ()).await?.0)
    }

    /// [`Self::listed_where`], and what `at` reads of the listing they were copied from.
    async fn positioned_where<T>(
        &self,
        keep: impl Fn(&Record<TerminalSession>) -> bool,
        at: impl FnOnce(&Listing) -> T,
    ) -> Result<(Vec<Record<TerminalSession>>, T), ResourceError> {
        let mut listing = self.listing.lock().await;
        let held = listing.cursors.take();
        let (cursors, dirt) = self
            .blocking(move |reads| match held {
                Some(mut cursors) => {
                    let dirt = cursors.pass();
                    Ok((Some(cursors), dirt))
                }
                // Started before the full read: a change after it is read again at the next pass.
                None => Ok((Cursors::start(reads)?, Dirt { all: true, ..Dirt::default() })),
            })
            .await?;
        let follows = cursors.is_some();
        listing.cursors = cursors;
        if dirt.all || !follows {
            let records = self.blocking(|reads| reads.terminals()?.every()).await;
            let sessions = match records {
                // Each view read again keeps its version while it shows the same.
                Ok(records) => match self.entries(newest_launches(records).into_values().collect()).await {
                    Ok(entries) => listing.stamped(entries),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            };
            match sessions {
                Ok(sessions) => listing.replace(sessions),
                Err(error) => {
                    // What it holds is not known to be current: the next pass reads afresh.
                    listing.cursors = None;
                    return Err(error);
                }
            }
        } else if !dirt.is_clean() || listing.sessions.values().any(|entry| entry.unresolved) {
            if let Err(error) = self.pass(&mut listing, dirt).await {
                listing.cursors = None;
                return Err(error);
            }
        }
        let listed = listing.sessions.values().filter_map(|entry| entry.view.as_ref().filter(|view| keep(view)).cloned()).collect();
        Ok((listed, at(&listing)))
    }

    /// Composes again the sessions `dirt` touched in `listing`, and takes in their new launches.
    async fn pass(&self, listing: &mut Listing, dirt: Dirt) -> Result<(), ResourceError> {
        let Dirt { terminals, observed, environments, vessels, .. } = dirt;
        let changed = self
            .blocking(move |reads| {
                let records = reads.terminals()?;
                let mut changed = Vec::new();
                for (name, home) in &terminals {
                    changed.extend(records.get(name, Some(home))?);
                }
                Ok(changed)
            })
            .await?;
        let here = self.terminals().map_err(ResourceError::other)?.home().to_owned();
        let mut again: BTreeSet<Session> = listing
            .sessions
            .iter()
            .filter(|(_, entry)| {
                let record = &entry.record;
                entry.unresolved
                    || environments.contains(&record.environment)
                    || record.vessel.as_ref().is_some_and(|vessel| vessels.contains(vessel))
                    || (record.home == here && observed.contains(&record.name))
            })
            .map(|(session, _)| session.clone())
            .collect();
        let mut records = BTreeMap::new();
        for record in changed {
            let session = (record.home.clone(), record.session_name().to_owned());
            if listing.sessions.get(&session).is_some_and(|held| held.record.launch_number() > record.launch_number()) {
                continue;
            }
            again.remove(&session);
            records.insert(session, record);
        }
        records.extend(again.into_iter().filter_map(|session| listing.sessions.get(&session).map(|entry| (session, entry.record.clone()))));
        let entries = self.entries(records.into_values().collect()).await?;
        let entries = listing.stamped(entries)?;
        listing.extend(entries);
        Ok(())
    }

    /// `records` as entries of the listing: only those listed are composed.
    async fn entries(&self, records: Vec<TerminalRecord>) -> Result<Vec<(Session, Entry)>, ResourceError> {
        let (released, shown): (Vec<_>, Vec<_>) = records.into_iter().partition(|record| record.reading == LifecycleReading::Released);
        let here = self.terminals().map_err(ResourceError::other)?.home().to_owned();
        let views = self.compose(shown.clone()).await?;
        let mut entries = Vec::with_capacity(shown.len() + released.len());
        for (record, view) in shown.into_iter().zip(views) {
            let home = match &view.provenance {
                ResourceProvenance::Local => here.clone(),
                ResourceProvenance::Replica { origin_root, .. } => origin_root.as_str().to_owned(),
            };
            // Its version is the listing's to give.
            let view = crate::held_listing::view_record(view.object, home, 0);
            let unresolved = record.vessel.is_some() && !view.labels.contains_key(flotilla_resources::VESSEL_LABEL);
            entries.push(((record.home.clone(), record.session_name().to_owned()), Entry { record, view: Some(view), unresolved }));
        }
        entries.extend(
            released
                .into_iter()
                .map(|record| ((record.home.clone(), record.session_name().to_owned()), Entry { record, view: None, unresolved: false })),
        );
        Ok(entries)
    }

    /// This home's sessions' newest launches.
    pub async fn here(&self) -> Result<Vec<ResourceObject<TerminalSession>>, ResourceError> {
        let records = self
            .blocking(|reads| {
                let terminals = reads.terminals()?;
                terminals.at(terminals.home())
            })
            .await?;
        Ok(self.compose(newest_launches(records).into_values().collect()).await?.into_iter().map(|source| source.object).collect())
    }

    /// The newest launch of session `name` at this home, as a resolver's `get` reads it.
    pub async fn get(&self, name: &str) -> Result<ResourceObject<TerminalSession>, ResourceError> {
        self.find(name, None).await?.map(|source| source.object).ok_or_else(|| ResourceError::not_found(name))
    }

    /// The newest launch of session (or launch) `name` at `home`, or at this home when `None`.
    pub async fn find(&self, name: &str, home: Option<&str>) -> Result<Option<ReadResourceObject<TerminalSession>>, ResourceError> {
        let (name, home) = (name.to_owned(), home.map(str::to_owned));
        let record = self.blocking(move |reads| reads.terminals()?.get(&name, home.as_deref())).await?;
        Ok(self.compose(record.into_iter().collect()).await?.pop())
    }

    /// The newest launches held for `vessel`, at each home.
    pub async fn of_vessel(&self, vessel: &str) -> Result<Vec<ReadResourceObject<TerminalSession>>, ResourceError> {
        let vessel = vessel.to_owned();
        let records = self.blocking(move |reads| reads.terminals()?.of_vessel(&vessel)).await?;
        self.compose(newest_launches(records).into_values().collect()).await
    }

    /// `records` as read views.
    pub async fn compose(&self, records: Vec<TerminalRecord>) -> Result<Vec<ReadResourceObject<TerminalSession>>, ResourceError> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let holders = self.holders(&records).await;
        let environments = self.environments(&records).await;
        let terminals = self.reads.terminals().map_err(ResourceError::other)?;
        let here = terminals.home().to_owned();
        Ok(records
            .into_iter()
            .map(|record| {
                let holder = record.vessel.as_ref().and_then(|vessel| holders.get(vessel));
                let local = record.home == here;
                let observation = local.then(|| terminals.observations().get(&record.name)).flatten();
                let environment = environments.get(&(record.environment.clone(), record.environment_home.clone()));
                let object = record.read_view(&self.namespace, holder, observation.as_ref(), environment);
                let provenance = if local {
                    ResourceProvenance::Local
                } else {
                    ResourceProvenance::Replica { origin_root: NodeId::new(record.home.clone()), last_synced_at: record.since }
                };
                ReadResourceObject { object, provenance }
            })
            .collect())
    }

    /// The environments `records` run in, by name and home, as their records read here. One that
    /// cannot be read is left out: its health is unknown, not bad.
    async fn environments(&self, records: &[TerminalRecord]) -> BTreeMap<(String, String), EnvironmentRecord> {
        let wanted: BTreeSet<(String, String)> =
            records.iter().map(|record| (record.environment.clone(), record.environment_home.clone())).collect();
        let read = self
            .blocking(move |reads| {
                let environments = reads.environments()?;
                let mut held = BTreeMap::new();
                for (name, home) in wanted {
                    match environments.get(&name, Some(&home)) {
                        Ok(Some(record)) => {
                            held.insert((name, home), record);
                        }
                        Ok(None) => {}
                        Err(error) => tracing::debug!(%name, %home, %error, "a terminal's environment could not be read"),
                    }
                }
                Ok(held)
            })
            .await;
        read.unwrap_or_else(|error| {
            tracing::debug!(%error, "terminals' environments could not be read");
            BTreeMap::new()
        })
    }

    /// The holder Vessel launches `records` name: read at the session's home, where a vessel
    /// usually runs, then at this one, then wherever this store holds it. A launch outliving its
    /// vessel, or whose vessel cannot be read, reads without its convoy.
    async fn holders(&self, records: &[TerminalRecord]) -> BTreeMap<String, TerminalHolderView> {
        let wanted: BTreeSet<(String, String)> =
            records.iter().filter_map(|record| Some((record.vessel.clone()?, record.home.clone()))).collect();
        if wanted.is_empty() {
            return BTreeMap::new();
        }
        let read = self
            .blocking(move |reads| {
                let vessels = reads.vessels()?;
                let mut holders = BTreeMap::new();
                for (vessel, home) in wanted {
                    if holders.contains_key(&vessel) {
                        continue;
                    }
                    let record = match vessels.get(&vessel, Some(&home))? {
                        Some(record) => Some(record),
                        None => match vessels.get(&vessel, None)? {
                            Some(record) => Some(record),
                            None => vessels.every()?.into_iter().find(|record| record.name == vessel),
                        },
                    };
                    if let Some(record) = record {
                        holders.insert(vessel, record.terminal_holder());
                    }
                }
                Ok(holders)
            })
            .await;
        read.unwrap_or_else(|error| {
            tracing::debug!(%error, "terminals' vessels could not be read");
            BTreeMap::new()
        })
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
}

impl std::fmt::Debug for TerminalViews {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("TerminalViews").field("namespace", &self.namespace).finish_non_exhaustive()
    }
}

impl TerminalViews {
    fn terminals(&self) -> Result<&dyn TerminalRecords, String> {
        self.reads.terminals()
    }

    /// This home's launch `record`, composed afresh, as the engine port hands it out: at the
    /// listing's version for its launch, or the next where the listing has not read it again.
    async fn here_record(&self, record: TerminalRecord) -> Result<Option<Record<TerminalSession>>, String> {
        let launch = record.name.clone();
        let session = (record.home.clone(), record.session_name().to_owned());
        let Some(view) = self.compose(vec![record]).await.map_err(|error| error.to_string())?.pop() else { return Ok(None) };
        let mut view = crate::held_listing::view_record(view.object, session.0.clone(), 0);
        let listing = self.listing.lock().await;
        view.version = listing.version(&session, &launch, &view, listing.position())?;
        Ok(Some(view))
    }

    async fn engine_sources(&self) -> Result<Vec<Record<TerminalSession>>, String> {
        self.listed().await.map_err(|error| error.to_string())
    }
}

impl crate::EnginePort for TerminalViews {
    fn install(self: Arc<Self>, backend: &crate::ResourceBackend) {
        backend.install_engine_kind::<TerminalSession>(self);
    }
}

#[async_trait::async_trait]
impl KindRecords<TerminalSession> for TerminalViews {
    fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
        use flotilla_resources::lifecycle_reads::{record_codes, RecordSet};
        record_codes(self.reads.as_ref(), &[RecordSet::Terminals, RecordSet::Environments, RecordSet::Vessels])
    }

    /// This home's observations of its launches.
    fn unlogged_changes(&self) -> Option<u64> {
        Some(self.reads.terminals().map_or(0, |terminals| terminals.observations().changed()))
    }

    fn home(&self) -> String {
        self.terminals().map(|terminals| terminals.home().to_owned()).unwrap_or_default()
    }

    /// This home's newest launch of `name`, read afresh so a home reads its own writes, else
    /// another home's as listed. A released launch reads as absent, as from a listing.
    async fn get(&self, name: &str) -> Result<Option<Record<TerminalSession>>, String> {
        let here = self.home();
        let owned = name.to_owned();
        let record = self.blocking(move |reads| reads.terminals()?.get(&owned, None)).await.map_err(|error| error.to_string())?;
        if let Some(record) = record.filter(|record| record.home == here) {
            if record.reading == LifecycleReading::Released {
                return Ok(None);
            }
            return self.here_record(record).await;
        }
        let named = self.listed_where(|object| object.name == name && object.home != here).await.map_err(|error| error.to_string())?;
        Ok(named.into_iter().min_by(|a, b| a.home.cmp(&b.home)))
    }

    /// Each session's newest launch, this home's where it holds one, else the lexically first
    /// home's.
    async fn list(&self) -> Result<Vec<Record<TerminalSession>>, String> {
        let home = self.home();
        let mut chosen = BTreeMap::<String, Record<TerminalSession>>::new();
        for object in self.engine_sources().await? {
            let keep = chosen.get(&object.name).is_some_and(|held| held.home == home || (object.home != home && held.home <= object.home));
            if !keep {
                chosen.insert(object.name.clone(), object);
            }
        }
        Ok(chosen.into_values().collect())
    }

    async fn sources(&self) -> Result<Vec<Record<TerminalSession>>, String> {
        self.engine_sources().await
    }

    async fn positioned(&self) -> Result<(Vec<Record<TerminalSession>>, LogPosition), String> {
        self.positioned_where(|_| true, Listing::read).await.map_err(|error| error.to_string())
    }

    async fn position(&self) -> Result<LogPosition, String> {
        Ok(self.positioned_where(|_| false, Listing::read).await.map_err(|error| error.to_string())?.1)
    }

    /// Named from the listing's own changes, which take in this home's observations: within this
    /// run only.
    async fn changed_since(&self, position: &LogPosition) -> Result<Since, String> {
        let since = |listing: &Listing| match listing.changed.since(position.unlogged.as_ref()) {
            Ok(names) => Since::Changed { names, through: listing.read() },
            Err(reason) => Since::Expired(reason),
        };
        Ok(self.positioned_where(|_| false, since).await.map_err(|error| error.to_string())?.1)
    }

    /// Every home's listed copy of `name`, and this home's newest launch of it however it ended.
    async fn copies(&self, name: &str) -> Result<Vec<Record<TerminalSession>>, String> {
        let here = self.home();
        let mut copies = self.sources_named(name).await?;
        if !copies.iter().any(|object| object.home == here) {
            let owned = name.to_owned();
            let record = self.blocking(move |reads| reads.terminals()?.get(&owned, None)).await.map_err(|error| error.to_string())?;
            if let Some(record) = record {
                copies.extend(self.here_record(record).await?);
            }
        }
        Ok(copies)
    }

    /// Every home's listed copy of `name`: a released launch is listed no more.
    async fn sources_named(&self, name: &str) -> Result<Vec<Record<TerminalSession>>, String> {
        self.listed_where(|object| object.name == name).await.map_err(|error| error.to_string())
    }

    async fn apply(
        &self,
        name: &str,
        _labels: &BTreeMap<String, String>,
        _annotations: &BTreeMap<String, String>,
        _spec: &TerminalSessionSpec,
    ) -> Result<Record<TerminalSession>, String> {
        Err(format!("terminal session {name} is requested through the engine's terminal sessions, not written"))
    }

    async fn set_status(&self, name: &str, _status: &TerminalSessionStatus) -> Result<Record<TerminalSession>, String> {
        Err(format!("terminal session {name}'s status is the engine's; it is not written"))
    }

    async fn delete(&self, name: &str) -> Result<(), String> {
        Err(format!("terminal session {name} ends when its holder does; it is not deleted"))
    }

    /// A tick at each launch change at any home, at each change of what this home observes, and at
    /// each change of the environments and vessels a view is joined with: the next listing reads
    /// what changed from the log.
    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let terminals = self.terminals()?;
        let mut streams = vec![ticks(terminals.changes()), ticks(terminals.observations().subscribe())];
        streams.extend(self.reads.environments().ok().map(|environments| ticks(environments.changes())));
        streams.extend(self.reads.vessels().ok().map(|vessels| ticks(vessels.changes())));
        Ok(futures::stream::select_all(streams).boxed())
    }
}

/// Which seats a vessel serves (#3629 layer 8): the newest launch of each session the engine holds
/// for it, by role. A role launched at more than one home (a vessel placed anew) is served by its
/// most recently requested launch.
pub struct EngineVesselSeats {
    reads: Arc<dyn LifecycleReads>,
}

impl EngineVesselSeats {
    pub fn new(reads: Arc<dyn LifecycleReads>) -> Self {
        Self { reads }
    }
}

#[async_trait::async_trait]
impl VesselSeats for EngineVesselSeats {
    async fn seats(&self, namespace: &str, vessel_ref: &str) -> Result<BTreeMap<String, ReadResourceObject<TerminalSession>>, String> {
        let views = TerminalViews::new(namespace, Arc::clone(&self.reads));
        let mut seats = BTreeMap::<String, ReadResourceObject<TerminalSession>>::new();
        for view in views.of_vessel(vessel_ref).await.map_err(|error| error.to_string())? {
            let newer = seats
                .get(&view.object.spec.role)
                .is_none_or(|held| held.object.metadata.creation_timestamp < view.object.metadata.creation_timestamp);
            if newer {
                seats.insert(view.object.spec.role.clone(), view);
            }
        }
        Ok(seats)
    }
}
