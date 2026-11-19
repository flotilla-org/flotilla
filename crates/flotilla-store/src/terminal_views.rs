//! Terminal sessions in the old TerminalSession shape, read from the engine's launches (#3108).
//!
//! The engine holds each launch ([`TerminalRecords`]); readers not yet converted to records see
//! each session's newest launch per home as a [`TerminalSession`] read view, joined with its holder
//! Vessel's convoy and labels and, at the launch's own home, with what that home observes of it.
//! Nothing here writes: requests and renewals go through [`TerminalRecords`].
//!
//! Installed as the backend's [`KindRecords`] for `TerminalSession`, it serves the typed reads,
//! lists and watches of the kind; its writes are refused.
//!
//! A read takes no lock while the listing is current: each pass that completes publishes what the
//! listing holds as an immutable [`Published`], with the commit counts it read through, and a read
//! whose counts that covers reads it as published. Only a read the listing has not yet caught up
//! with takes the listing's lock, and there it waits for the pass running, which may serve it, or
//! runs one; it never waits on another read of the listing.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock},
};

use futures::{stream::BoxStream, StreamExt};
use tracing::{info_span, Instrument};

use flotilla_protocol::{LifecycleReading, NodeId};
use flotilla_resources::{
    engine_records::{IndexCover, KindRecords, LogPosition, Record, Since, UnloggedJournal, UnloggedRead, WriteError},
    environment_records::{EnvironmentChange, EnvironmentRecord},
    held_joins::{self, JoinChange, JoinChanges, JoinsReader},
    lifecycle_reads::{ChangeCursor, LifecycleReads, Told},
    terminal_records::{launched, newest_launches, TerminalHolderView, TerminalRecord, TerminalRecords},
    vessel_seats::VesselSeats,
    ReadResourceObject, ResourceError, ResourceObject, ResourceProvenance, TerminalSession, TerminalSessionSpec, TerminalSessionStatus,
};

use crate::held_listing::{drained, ticks, Joins, ViewIndex};

/// Read views of the terminal sessions in one namespace.
///
/// Its listing is kept from pass to pass: each pass reads from the log what changed since the
/// last, before it reads anything else, and composes again only the sessions that touched. A
/// read port that holds no log reads every launch afresh.
#[derive(Clone)]
pub struct TerminalViews {
    namespace: String,
    reads: Arc<dyn LifecycleReads>,
    /// What passes the listing: one pass at a time, and only a read that needs one waits here.
    listing: Arc<tokio::sync::Mutex<Listing>>,
    /// What the listing held when the last pass completed, read without the listing's lock.
    published: Arc<RwLock<Option<Arc<Published>>>>,
    /// The sessions whose view changed, in this run: what a reading resumes from. Written by a
    /// pass, under the listing's lock; a read takes it only to name the changes since a position.
    changed: Arc<Mutex<UnloggedJournal>>,
    /// The measured reads waiting on another read's pass of the listing, which count its reads.
    waiters: Arc<flotilla_schema::reads::Waiters>,
}

/// A session at a home, as [`newest_launches`] keys it: its home and its session name.
type Session = (String, String);

/// What a pass read through, counted before it read: the commits told to the held joins' cursors
/// and to the environments' cursor, and how many times this home's observations changed. The
/// environments' count is of the cursor started at `cursors`, the listing's count of cursor starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ToldCounts {
    cursors: u64,
    joins: u64,
    environments: u64,
    observations: u64,
}

impl ToldCounts {
    /// Whether a listing read through `self` holds every change counted in `told`.
    fn covers(&self, told: &ToldCounts) -> bool {
        self.cursors == told.cursors
            && self.joins >= told.joins
            && self.environments >= told.environments
            && self.observations >= told.observations
    }
}

/// The counts that tell a pass of a listing whose cursors are the `cursors`th started, with
/// `environments` their environments' count: none where the held joins cannot tell.
fn told_counts(reads: &dyn LifecycleReads, cursors: u64, environments: Option<&Told>) -> Option<ToldCounts> {
    Some(ToldCounts {
        cursors,
        joins: reads.joins().ok()?.told()?,
        environments: environments.map_or(0, Told::get),
        observations: reads.terminals().ok()?.observations().changed(),
    })
}

/// What the listing shows to readers: replaced, never changed, once published.
#[derive(Clone, Default)]
struct Shown {
    sessions: BTreeMap<Session, Arc<Entry>>,
    /// The listed views, under each of their index keys.
    index: ViewIndex<Session>,
}

/// The listing as the last pass that completed left it, with the commit counts it holds every
/// change of.
struct Published {
    shown: Arc<Shown>,
    /// The counts read before that pass: none where they cannot be told, or where a session's
    /// holder could not be read, which each pass composes again; no read is then served from here.
    through: Option<ToldCounts>,
    /// The environments' count the pass read, to count a read's commits by.
    environments: Option<Told>,
    /// Where a reader stands that has read this listing.
    read: UnloggedRead,
    /// How far the listing had read: the log and this home's observations, by its cursors.
    position: u64,
}

impl Published {
    /// The commit counts now, as this listing counts them.
    fn told(&self, reads: &dyn LifecycleReads) -> Option<ToldCounts> {
        told_counts(reads, self.through?.cursors, self.environments.as_ref())
    }

    /// Whether this listing serves a read made after the commit counts `told` were read.
    fn serves(&self, told: Option<&ToldCounts>) -> bool {
        matches!((self.through.as_ref(), told), (Some(through), Some(told)) if through.covers(told))
    }

    /// Where a reader stands that has read this listing.
    fn log_position(&self) -> LogPosition {
        LogPosition { homes: BTreeMap::new(), unlogged: Some(self.read.clone()) }
    }
}

/// Each session's newest launch at every home, as the last pass composed it.
struct Listing {
    /// Where the next pass reads the changes since this one; none before the first.
    cursors: Option<Cursors>,
    /// How many times cursors were started: a count told by one is no count of another's.
    started: u64,
    /// What the listing shows, shared with the listing last published: a pass changes a copy.
    shown: Arc<Shown>,
    /// [`TerminalViews::changed`].
    changed: Arc<Mutex<UnloggedJournal>>,
    /// The counts read before the last pass that completed; none where they cannot be told.
    through: Option<ToldCounts>,
}

impl Listing {
    fn new(changed: Arc<Mutex<UnloggedJournal>>) -> Self {
        Self { cursors: None, started: 0, shown: Arc::default(), changed, through: None }
    }

    fn journal(&self) -> MutexGuard<'_, UnloggedJournal> {
        self.changed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The listing as readers read it until the next is published.
    fn published(&self) -> Published {
        let resolved = !self.shown.sessions.values().any(|entry| entry.unresolved);
        Published {
            shown: Arc::clone(&self.shown),
            through: self.through.filter(|_| resolved && self.cursors.is_some()),
            environments: self.cursors.as_ref().and_then(Cursors::environments_told).flatten(),
            read: self.journal().read(),
            position: self.position(),
        }
    }

    /// Holds `entries` in place of every session held.
    fn replace(&mut self, entries: Vec<(Session, Entry)>) {
        let entries: BTreeMap<Session, Entry> = entries.into_iter().collect();
        let gone: Vec<Session> = self.shown.sessions.keys().filter(|session| !entries.contains_key(*session)).cloned().collect();
        if !gone.is_empty() {
            let shown = Arc::make_mut(&mut self.shown);
            let mut changed = self.changed.lock().unwrap_or_else(PoisonError::into_inner);
            for session in gone {
                shown.index.remove(&session);
                if shown.sessions.remove(&session).is_some_and(|entry| entry.view.is_some()) {
                    changed.record([session.1]);
                }
            }
        }
        self.extend(entries);
    }

    /// Holds each of `entries`, counting each whose view changed.
    fn extend(&mut self, entries: impl IntoIterator<Item = (Session, Entry)>) {
        let mut entries = entries.into_iter().peekable();
        if entries.peek().is_none() {
            return;
        }
        let shown = Arc::make_mut(&mut self.shown);
        let mut changed = self.changed.lock().unwrap_or_else(PoisonError::into_inner);
        for (session, mut entry) in entries {
            let version = |entry: &Entry| entry.view.as_ref().map(|view| view.version);
            if shown.sessions.get(&session).map(|held| version(held)) != Some(version(&entry)) {
                changed.record([session.1.clone()]);
            }
            match entry.view {
                Some(_) => shown.index.set(&session, std::mem::take(&mut entry.keys)),
                None => shown.index.remove(&session),
            }
            shown.sessions.insert(session, Arc::new(entry));
        }
    }

    /// Stops holding `session` where `launch` is its launch held: the held joins evicted it, so
    /// it ended and no view lists it.
    fn evict(&mut self, session: &Session, launch: &str) {
        if self.shown.sessions.get(session).is_some_and(|entry| entry.record.name == launch) {
            let shown = Arc::make_mut(&mut self.shown);
            shown.index.remove(session);
            if shown.sessions.remove(session).is_some_and(|entry| entry.view.is_some()) {
                self.changed.lock().unwrap_or_else(PoisonError::into_inner).record([session.1.clone()]);
            }
        }
    }

    /// The sessions whose view changed since `position`.
    fn since(&self, position: &LogPosition) -> Since {
        let changed = self.journal();
        match changed.since(position.unlogged.as_ref()) {
            Ok(names) => Since::Changed { names, through: LogPosition { homes: BTreeMap::new(), unlogged: Some(changed.read()) } },
            Err(reason) => Since::Expired(reason),
        }
    }

    /// How far the listing has read: the log and this home's observations, by its cursors.
    fn position(&self) -> u64 {
        self.cursors.as_ref().map_or(0, Cursors::position)
    }

    /// `entries`, composed again, with the versions their views are handed out at.
    fn stamped(&self, entries: Vec<(Session, Entry)>) -> Result<Vec<(Session, Entry)>, ResourceError> {
        let position = self.position();
        entries
            .into_iter()
            .map(|(session, mut entry)| {
                if let Some(view) = entry.view.as_mut() {
                    view.version = version(&self.shown, &session, &entry.record.name, view, position).map_err(ResourceError::other)?;
                }
                Ok((session, entry))
            })
            .collect()
    }
}

/// The records `published` lists that `keep` selects: only those are copied out of it.
fn listed_in(published: &Published, keep: impl Fn(&Record<TerminalSession>) -> bool) -> Vec<Record<TerminalSession>> {
    published.shown.sessions.values().filter_map(|entry| entry.view.as_ref().filter(|view| keep(view)).cloned()).collect()
}

/// `view`, of launch `launch` of `session`, at the version it is handed out at: the version of the
/// view `shown` holds while it is of the same launch and shows the same.
fn version(shown: &Shown, session: &Session, launch: &str, view: &Record<TerminalSession>, position: u64) -> Result<u64, String> {
    let held = shown.sessions.get(session).and_then(|held| Some((held.record.name == launch, held.view.as_ref()?)));
    let same = held.is_some_and(|(same_launch, held)| same_launch && crate::held_listing::shows_the_same(view, held));
    crate::held_listing::version(held.map(|(_, held)| held.version), same, position)
}

/// A session's newest launch.
struct Entry {
    record: TerminalRecord,
    /// Its view, while it is listed: a released launch is not.
    view: Option<Record<TerminalSession>>,
    /// Whether its holder vessel could not be read when it was composed: its holder is unknown,
    /// not absent, so each pass composes it again until it is read.
    unresolved: bool,
    /// Its view's index keys, until the listing indexes it.
    keys: Vec<(String, String)>,
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

/// Where the listing reads the changes of the records a session's view is composed from: the held
/// joins' journal for the session and Vessel launches, and cursors over the rest. A read port that
/// serves no environments joins none, so follows none.
struct Cursors {
    /// The listing's place in the held joins' journal.
    reader: JoinsReader,
    /// The held joins' journal sequence the listing has read through.
    joins_seen: u64,
    /// How far the held joins had read the log, at that sequence.
    joins_position: u64,
    environments: Option<Box<dyn ChangeCursor<EnvironmentChange>>>,
    /// What this home observes is in memory, told as it changes.
    observed: tokio::sync::broadcast::Receiver<String>,
    /// How many observation changes have been read.
    observed_read: u64,
}

impl Cursors {
    /// Cursors from now on, where `reads` holds a log of its sessions.
    fn start(reads: Arc<dyn LifecycleReads>) -> Result<Option<Self>, String> {
        let terminals = reads.terminals()?;
        if !reads.joins()?.read(reads.as_ref(), |joins| joins.follows())? {
            return Ok(None);
        }
        Ok(Some(Self {
            reader: JoinsReader::register(Arc::clone(&reads), false)?,
            joins_seen: 0,
            joins_position: 0,
            environments: reads.environments().ok().and_then(|environments| environments.cursor()),
            observed: terminals.observations().subscribe(),
            observed_read: 0,
        }))
    }

    /// How far the cursors have read: the log, and this home's observations.
    fn position(&self) -> u64 {
        [self.joins_position, self.observed_read]
            .into_iter()
            .chain(self.environments.as_ref().map(|environments| environments.position()))
            .fold(0, u64::saturating_add)
    }

    /// The commits the environments' cursor was told of: `Some(None)` where no environment is
    /// followed, and none where the cursor cannot tell.
    fn environments_told(&self) -> Option<Option<Told>> {
        match self.environments.as_ref() {
            Some(cursor) => cursor.told().map(Some),
            None => Some(None),
        }
    }

    /// The counts a pass of these cursors, the `started`th, reads through, read before it reads.
    fn told(&self, reads: &dyn LifecycleReads, started: u64) -> Option<ToldCounts> {
        told_counts(reads, started, self.environments_told()?.as_ref())
    }

    /// Where the listing stands in the held joins once it has composed from them as `joins` holds
    /// them.
    fn joined(&mut self, joins: &held_joins::Joins<'_>) {
        self.joins_seen = joins.sequence();
        self.joins_position = joins.position();
    }

    /// The listing has composed everything it has read of the journal: the held joins may evict
    /// what it no longer needs.
    fn composed(&self) {
        self.reader.composed(self.joins_seen);
    }

    /// What changed since the last pass; a change that could not be read marks everything.
    fn pass(&mut self) -> Dirt {
        let mut dirt = Dirt::default();
        let (seen, position) = (&mut self.joins_seen, &mut self.joins_position);
        let joined = self.reader.read(|joins| {
            match joins.since(*seen) {
                JoinChanges::Everything => dirt.all = true,
                JoinChanges::Changed(changes) => changes.into_iter().for_each(|change| match change {
                    JoinChange::Terminal { name, home, .. } | JoinChange::TerminalEvicted { name, home } => {
                        dirt.terminals.insert((name.clone(), home.clone()));
                    }
                    JoinChange::Vessel { name, .. } => drop(dirt.vessels.insert(name.clone())),
                }),
            }
            *seen = joins.sequence();
            *position = joins.position();
        });
        dirt.all |= joined.is_err();
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
        let changed = Arc::<Mutex<UnloggedJournal>>::default();
        Self {
            namespace: namespace.to_owned(),
            reads,
            listing: Arc::new(tokio::sync::Mutex::new(Listing::new(Arc::clone(&changed)))),
            published: Arc::default(),
            changed,
            waiters: Arc::default(),
        }
    }

    /// Each session's newest launch at every home, as the engine port lists it: this home's
    /// local, other homes' replicas of their home. A released launch is listed no more.
    async fn listed(&self) -> Result<Vec<Record<TerminalSession>>, ResourceError> {
        self.listed_where(|_| true).await
    }

    /// The listed records `keep` selects: only those are copied out of the listing.
    async fn listed_where(&self, keep: impl Fn(&Record<TerminalSession>) -> bool) -> Result<Vec<Record<TerminalSession>>, ResourceError> {
        let published = self.passed().await?;
        Ok(listed_in(&published, keep))
    }

    /// The views `cover` selects, from the listing's index, this home's alone when `local`: only
    /// those are copied out of the listing.
    async fn covered_views(&self, cover: &IndexCover, local: bool) -> Result<Vec<Record<TerminalSession>>, ResourceError> {
        let here = self.terminals().map_err(ResourceError::other)?.home().to_owned();
        let listing = self.passed().await?;
        let shown = &listing.shown;
        Ok(shown
            .index
            .covered(cover)
            .into_iter()
            .filter(|(home, _)| !local || *home == here)
            .filter_map(|session| shown.sessions.get(&session)?.view.clone())
            .collect())
    }

    /// The listing as published, once it holds every change written before the read: the commits
    /// counted before it are read in by a pass, one that began after them or its own. While the
    /// listing last published holds every commit counted, the read takes it without a lock and
    /// reads no store; otherwise it waits for the pass running or runs one. A measured read counts
    /// the pass it waits on, another read's included.
    async fn passed(&self) -> Result<Arc<Published>, ResourceError> {
        // Before anything is read: a pass that began after this read the commits counted here.
        let last = self.last_published();
        let told = last.as_ref().and_then(|last| last.told(self.reads.as_ref()));
        if let Some(last) = last.filter(|last| last.serves(told.as_ref())) {
            return Ok(last);
        }
        let waiters = &self.waiters;
        waiters
            .wait(async {
                let mut listing = self.listing.lock().instrument(info_span!("terminal_views.lock_wait")).await;
                // A pass that completed while this read waited may have read every commit counted.
                if let Some(last) = self.last_published().filter(|last| last.serves(told.as_ref())) {
                    return Ok(last);
                }
                waiters.serve(self.pass_listing(&mut listing)).instrument(info_span!("terminal_views.pass")).await?;
                Ok(self.publish(&listing))
            })
            .await
    }

    fn last_published(&self) -> Option<Arc<Published>> {
        self.published.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Publishes `listing` as the listing readers read.
    fn publish(&self, listing: &Listing) -> Arc<Published> {
        let published = Arc::new(listing.published());
        *self.published.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&published));
        published
    }

    /// Takes into `listing` what changed since its last pass.
    async fn pass_listing(&self, listing: &mut Listing) -> Result<(), ResourceError> {
        let held = listing.cursors.take();
        let reads = Arc::clone(&self.reads);
        let started = listing.started;
        let (cursors, dirt, told, started) = self
            .blocking(move |reads_now| match held {
                Some(mut cursors) => {
                    // Before the cursors read the log: the commits counted are among what they read.
                    let told = cursors.told(reads_now, started);
                    let dirt = cursors.pass();
                    Ok((Some(cursors), dirt, told, started))
                }
                // Started before the full read: a change after it is read again at the next pass,
                // and the commits counted after the start are among what the full read reads.
                None => {
                    let cursors = Cursors::start(reads)?;
                    let started = started.saturating_add(1);
                    let told = cursors.as_ref().and_then(|cursors| cursors.told(reads_now, started));
                    Ok((cursors, Dirt { all: true, ..Dirt::default() }, told, started))
                }
            })
            .await?;
        listing.started = started;
        // Set again once the pass completes.
        listing.through = None;
        let follows = cursors.is_some();
        listing.cursors = cursors;
        if dirt.all || !follows {
            let held = listing.cursors.take();
            let records = self
                .blocking(move |reads| {
                    reads.joins()?.read(reads, |joins| {
                        // Every session is composed from the joins as held: their journal is read
                        // through there.
                        let held = held.map(|mut cursors| {
                            cursors.joined(&joins);
                            cursors
                        });
                        (held, joins.terminals().cloned().collect::<Vec<_>>())
                    })
                })
                .await;
            let records = records.map(|(held, records)| {
                listing.cursors = held;
                records
            });
            let sessions = match records {
                // Each view read again keeps its version while it shows the same.
                Ok(records) => match self.entries(newest_launches(records).into_values().collect(), Joins::Every).await {
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
        } else if !dirt.is_clean() || listing.shown.sessions.values().any(|entry| entry.unresolved) {
            if let Err(error) = self.pass(listing, dirt).await {
                listing.cursors = None;
                return Err(error);
            }
        }
        if let Some(cursors) = listing.cursors.as_ref() {
            cursors.composed();
        }
        listing.through = told;
        Ok(())
    }

    /// Composes again the sessions `dirt` touched in `listing`, and takes in their new launches.
    async fn pass(&self, listing: &mut Listing, dirt: Dirt) -> Result<(), ResourceError> {
        let Dirt { terminals, observed, environments, vessels, .. } = dirt;
        let (changed, evicted) = self
            .blocking(move |reads| {
                reads.joins()?.read(reads, |joins| {
                    let (held, evicted): (Vec<_>, Vec<_>) =
                        terminals.into_iter().partition(|(name, home)| joins.terminal(name, home).is_some());
                    (held.iter().filter_map(|(name, home)| joins.terminal(name, home).cloned()).collect::<Vec<_>>(), evicted)
                })
            })
            .await?;
        // A launch the held joins no longer hold has ended and is listed no more.
        for (name, home) in evicted {
            listing.evict(&(home, launched(&name).to_owned()), &name);
        }
        let here = self.terminals().map_err(ResourceError::other)?.home().to_owned();
        let mut again: BTreeSet<Session> = listing
            .shown
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
            if listing.shown.sessions.get(&session).is_some_and(|held| held.record.launch_number() > record.launch_number()) {
                continue;
            }
            again.remove(&session);
            records.insert(session, record);
        }
        records.extend(
            again.into_iter().filter_map(|session| listing.shown.sessions.get(&session).map(|entry| (session, entry.record.clone()))),
        );
        let records: Vec<_> = records.into_values().collect();
        let span = info_span!("terminal_views.recompose", sessions = records.len());
        let entries = self.entries(records, Joins::Keyed).instrument(span).await?;
        let entries = listing.stamped(entries)?;
        listing.extend(entries);
        Ok(())
    }

    /// `records` as entries of the listing, their joins read as `joins` says: only those listed
    /// are composed.
    async fn entries(&self, records: Vec<TerminalRecord>, joins: Joins) -> Result<Vec<(Session, Entry)>, ResourceError> {
        let (released, shown): (Vec<_>, Vec<_>) = records.into_iter().partition(|record| record.reading == LifecycleReading::Released);
        let here = self.terminals().map_err(ResourceError::other)?.home().to_owned();
        let views = self.compose_with(shown.clone(), joins).await?;
        let mut entries = Vec::with_capacity(shown.len() + released.len());
        for (record, view) in shown.into_iter().zip(views) {
            let home = match &view.provenance {
                ResourceProvenance::Local => here.clone(),
                ResourceProvenance::Replica { origin_root, .. } => origin_root.as_str().to_owned(),
            };
            let keys = crate::object_index_keys(&view.object)?;
            // Its version is the listing's to give.
            let view = crate::held_listing::view_record(view.object, home, 0);
            let unresolved = record.vessel.is_some() && !view.labels.contains_key(flotilla_resources::VESSEL_LABEL);
            entries.push(((record.home.clone(), record.session_name().to_owned()), Entry { record, view: Some(view), unresolved, keys }));
        }
        entries.extend(released.into_iter().map(|record| {
            ((record.home.clone(), record.session_name().to_owned()), Entry { record, view: None, unresolved: false, keys: Vec::new() })
        }));
        Ok(entries)
    }

    /// How many sessions the listing holds: what grows with the sessions held.
    pub async fn held(&self) -> Result<usize, ResourceError> {
        Ok(self.passed().await?.shown.sessions.len())
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
        self.compose_with(records, Joins::Keyed).await
    }

    /// `records` as read views, their joins read as `joins` says.
    async fn compose_with(
        &self,
        records: Vec<TerminalRecord>,
        joins: Joins,
    ) -> Result<Vec<ReadResourceObject<TerminalSession>>, ResourceError> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let holders = self.holders(&records).instrument(info_span!("terminal_views.holders", sessions = records.len())).await;
        let environments = self.environments(&records, joins).instrument(info_span!("terminal_views.environments")).await;
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
    async fn environments(&self, records: &[TerminalRecord], joins: Joins) -> BTreeMap<(String, String), EnvironmentRecord> {
        let wanted: BTreeSet<(String, String)> =
            records.iter().map(|record| (record.environment.clone(), record.environment_home.clone())).collect();
        let read = self
            .blocking(move |reads| {
                let environments = reads.environments()?;
                if joins == Joins::Every {
                    return Ok(environments
                        .every()?
                        .into_iter()
                        .map(|record| ((record.name.clone(), record.home.clone()), record))
                        .filter(|(key, _)| wanted.contains(key))
                        .collect());
                }
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

    /// The holder Vessel launches `records` name, as the read port holds them
    /// ([`held_joins::Joins::holder`]). A launch outliving its vessel, or whose vessel cannot be read, reads
    /// without its convoy.
    async fn holders(&self, records: &[TerminalRecord]) -> BTreeMap<String, TerminalHolderView> {
        let wanted: BTreeSet<(String, String)> =
            records.iter().filter_map(|record| Some((record.vessel.clone()?, record.home.clone()))).collect();
        if wanted.is_empty() {
            return BTreeMap::new();
        }
        let read = self
            .blocking(move |reads| {
                reads.joins()?.read(reads, |joins| {
                    let mut holders = BTreeMap::new();
                    for (vessel, home) in wanted {
                        if holders.contains_key(&vessel) {
                            continue;
                        }
                        if let Some(record) = joins.holder(&vessel, &home) {
                            holders.insert(vessel, record.terminal_holder());
                        }
                    }
                    holders
                })
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
        let composed = self.compose(vec![record]).instrument(info_span!("terminal_views.compose"));
        let Some(view) = composed.await.map_err(|error| error.to_string())?.pop() else { return Ok(None) };
        let mut view = crate::held_listing::view_record(view.object, session.0.clone(), 0);
        // The listing as last published, as one read made now would take it; the listing itself
        // before the first is.
        view.version = match self.last_published() {
            Some(published) => version(&published.shown, &session, &launch, &view, published.position)?,
            None => {
                let listing = self.listing.lock().instrument(info_span!("terminal_views.lock_wait", by = "version")).await;
                version(&listing.shown, &session, &launch, &view, listing.position())?
            }
        };
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

    /// From the followers of the sessions, environments and vessels it joins.
    fn ticks(&self) -> flotilla_resources::engine_records::Ticks {
        flotilla_resources::engine_records::Ticks::Followed
    }

    /// This home's observations of its launches.
    fn unlogged_changes(&self) -> Option<u64> {
        Some(self.reads.terminals().map_or(0, |terminals| terminals.observations().changed()))
    }

    fn writes(&self) -> bool {
        false
    }

    fn home(&self) -> String {
        self.terminals().map(|terminals| terminals.home().to_owned()).unwrap_or_default()
    }

    /// This home's newest launch of `name`, read afresh so a home reads its own writes, else
    /// another home's as listed. A released launch reads as absent, as from a listing.
    async fn get(&self, name: &str) -> Result<Option<Record<TerminalSession>>, String> {
        let here = self.home();
        let owned = name.to_owned();
        let record = self.blocking(move |reads| reads.terminals()?.get(&owned, None)).instrument(info_span!("terminal_views.record"));
        let record = record.await.map_err(|error| error.to_string())?;
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
        let published = self.passed().await.map_err(|error| error.to_string())?;
        Ok((listed_in(&published, |_| true), published.log_position()))
    }

    async fn position(&self) -> Result<LogPosition, String> {
        Ok(self.passed().await.map_err(|error| error.to_string())?.log_position())
    }

    /// Named from the listing's own changes, which take in this home's observations: within this
    /// run only. Named from the journal while it holds no change the listing published does not:
    /// otherwise a pass is changing the listing, and the read waits for it.
    async fn changed_since(&self, position: &LogPosition) -> Result<Since, String> {
        let published = self.passed().await.map_err(|error| error.to_string())?;
        let since = |changed: &UnloggedJournal| match changed.since(position.unlogged.as_ref()) {
            Ok(names) => Since::Changed { names, through: LogPosition { homes: BTreeMap::new(), unlogged: Some(changed.read()) } },
            Err(reason) => Since::Expired(reason),
        };
        {
            let changed = self.changed.lock().unwrap_or_else(PoisonError::into_inner);
            if changed.count() == published.read.read {
                return Ok(since(&changed));
            }
        }
        let waiters = &self.waiters;
        let since = waiters.wait(async {
            let mut listing = self.listing.lock().instrument(info_span!("terminal_views.lock_wait", by = "changed_since")).await;
            waiters.serve(self.pass_listing(&mut listing)).instrument(info_span!("terminal_views.pass")).await?;
            self.publish(&listing);
            Ok::<_, ResourceError>(listing.since(position))
        });
        since.await.map_err(|error| error.to_string())
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

    async fn covered(&self, cover: &IndexCover, local: bool) -> Option<Result<Vec<Record<TerminalSession>>, String>> {
        Some(self.covered_views(cover, local).await.map_err(|error| error.to_string()))
    }

    async fn apply(
        &self,
        name: &str,
        _version: Option<u64>,
        _labels: &BTreeMap<String, String>,
        _annotations: &BTreeMap<String, String>,
        _spec: &TerminalSessionSpec,
    ) -> Result<Record<TerminalSession>, WriteError> {
        Err(format!("terminal session {name} is requested through the engine's terminal sessions, not written").into())
    }

    async fn set_status(
        &self,
        name: &str,
        _version: u64,
        _status: &TerminalSessionStatus,
    ) -> Result<Record<TerminalSession>, flotilla_resources::engine_records::WriteError> {
        Err(format!("terminal session {name}'s status is the engine's; it is not written").into())
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
