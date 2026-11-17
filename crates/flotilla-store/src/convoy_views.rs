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
//! another kind's views: the Vessel views read these, so these read the Vessel records, as the
//! read port's [`flotilla_resources::held_joins::HeldJoins`] holds them for every view. A read
//! first passes the convoys' cursor and the held joins over the log and reads in the note changes
//! not yet read, so a writer reads back what it has written.
//!
//! Installed as the backend's [`KindRecords`] for `Convoy`, it serves the typed reads, lists and
//! watches of the kind; its writes are refused.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, PoisonError},
};

use chrono::DateTime;
use futures::{stream::BoxStream, FutureExt, StreamExt};

use flotilla_protocol::{LifecycleReading, NodeId};
use flotilla_resources::{
    convoy::view::VesselReading,
    convoy_records::{ConvoyChange, ConvoyJoins, ConvoyNotes, ConvoyRecord, ConvoyRecords, WorkFacts},
    engine_records::{IndexCover, KindRecords, LogPosition, Record, Since, UnloggedJournal},
    held_joins::{JoinChange, JoinChanges, JoinsReader},
    lifecycle_reads::{ChangeCursor, LifecycleReads},
    terminal_records::{newest_launches, TerminalRecord},
    vessel_records::{interrupted_message, VesselRecord},
    Convoy, ConvoySpec, ConvoyStatus, ReadResourceObject, ResourceError, ResourceObject, ResourceProvenance,
};
use flotilla_schema::reads;

use crate::held_listing::{ticks, ViewIndex};

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
    /// The measured reads waiting on a fill or pass of the views, which count its reads.
    waiters: reads::Waiters,
}

/// The caller the follower's reads count for.
const FOLLOWER: &str = "Convoy views";

struct HeldViews {
    views: BTreeMap<(String, String), HeldView>,
    /// Where the next read takes the records' changes from; none where no log is held, and each
    /// read then reads every launch afresh.
    following: Option<Arc<Mutex<Following>>>,
    /// This home's note changes not yet read into the views.
    notes: tokio::sync::broadcast::Receiver<String>,
    /// How many note changes have been read, missed ones counted: with the cursors' positions,
    /// how far the views have read.
    notes_read: u64,
    /// How far the cursors had read the log at the pass whose launches were last taken into
    /// `owed`.
    cursors_read: u64,
    /// The launches the cursors have passed whose views are not yet composed again. A read
    /// dropped while it composes leaves them here for the next, since the cursors will not name
    /// them again.
    owed: BTreeSet<(String, String)>,
    /// A read of every launch was begun and not finished.
    refill_owed: bool,
    /// The launches held as unreadable, by `(home, launch)`, at the version last told.
    unreadable: BTreeMap<(String, String), u64>,
    /// The launches whose view changed, in this run: what a reading resumes from.
    changed: UnloggedJournal,
    /// The views held by each index key their labels and declared fields give, as the store's
    /// query index names them: a selecting query reads only the views it may select.
    index: ViewIndex<(String, String)>,
    /// The launches held, by `(home, launch)`, that have not ended. One that cannot be read
    /// counts: nothing says it ended.
    open: BTreeSet<(String, String)>,
    /// What each ended launch's works were last composed from, by `(home, launch)`: its Vessel
    /// launches are released, and once the held joins evict them a view composed again keeps
    /// these.
    ended_works: BTreeMap<(String, String), BTreeMap<String, WorkFacts>>,
}

/// A launch's view and the version it is handed out at.
#[derive(Clone)]
struct HeldView {
    view: ReadResourceObject<Convoy>,
    version: u64,
}

/// The cursor and what it has passed, shared with the blocking read that passes it: a read
/// dropped while the cursor passes leaves both here for the next, instead of losing the cursor,
/// after which every read would read every launch again, or the launches it named.
struct Following {
    cursors: Cursors,
    /// How far the cursor and the held joins had read the log at their last pass, read under the
    /// same lock.
    through: u64,
    /// Launches passed and not yet owed by the views.
    passed: BTreeSet<(String, String)>,
    /// The cursors could not tell what changed: every launch is read again.
    rescanned: bool,
}

/// Where the views read the changes of the records a convoy's view is composed from: the convoy
/// launches' own cursor, and the held joins' journal for their Vessel and session launches.
struct Cursors {
    convoys: Box<dyn ChangeCursor<ConvoyChange>>,
    /// The views' place in the held joins' journal.
    reader: JoinsReader,
    /// The journal's sequence the views have read through.
    joins_seen: u64,
    /// How far the held joins had read the log, at that sequence.
    joins_position: u64,
}

impl Cursors {
    /// Cursors from now on, where `reads` holds a log of its convoys.
    fn start(reads: Arc<dyn LifecycleReads>) -> Result<Option<Self>, String> {
        let Some(convoys) = reads.convoys()?.cursor() else { return Ok(None) };
        let reader = JoinsReader::register(reads, true)?;
        Ok(Some(Self { convoys, reader, joins_seen: 0, joins_position: 0 }))
    }

    /// How far the cursors have read the log.
    fn position(&self) -> u64 {
        self.convoys.position().saturating_add(self.joins_position)
    }

    /// The views have composed every launch whose joins changed before sequence `seen`: the held
    /// joins may evict what they no longer need.
    fn composed(&self, seen: u64) {
        self.reader.composed(seen);
    }

    /// The launches, by `(home, name)`, whose records or joins changed since the last pass; none
    /// when anything may have.
    fn pass(&mut self) -> Result<Option<BTreeSet<(String, String)>>, String> {
        let mut launches = BTreeSet::new();
        let mut rescanned = false;
        for change in self.convoys.pass()? {
            match change {
                ConvoyChange::Changed { name, home } => drop(launches.insert((home, name))),
                ConvoyChange::Rescanned => rescanned = true,
            }
        }
        // The convoy launch a Vessel launch, or the Vessel launch a session, is held for.
        let joined = self.reader.read(|joins| {
            let touched = match joins.since(self.joins_seen) {
                JoinChanges::Everything => None,
                JoinChanges::Changed(changes) => Some(
                    changes
                        .into_iter()
                        .filter_map(|change| match change {
                            JoinChange::Vessel { convoy, convoy_home, .. } => Some((convoy_home.clone(), convoy.clone())),
                            JoinChange::Terminal { home, vessel, .. } => {
                                let vessel = joins.holder(vessel.as_deref()?, home)?;
                                Some((vessel.convoy_home.clone(), vessel.convoy.clone()))
                            }
                        })
                        .collect::<Vec<_>>(),
                ),
            };
            self.joins_seen = joins.sequence();
            self.joins_position = joins.position();
            touched
        })?;
        match (rescanned, joined) {
            (false, Some(joined)) => {
                launches.extend(joined);
                Ok(Some(launches))
            }
            _ => Ok(None),
        }
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
        self.with_held(|held| held.views.values().map(|held| held.view.clone()).collect()).await
    }

    /// This home's launches.
    pub async fn here(&self) -> Result<Vec<ResourceObject<Convoy>>, ResourceError> {
        let here = self.convoys().map_err(ResourceError::other)?.home().to_owned();
        self.with_held(move |held| {
            held.views.iter().filter(|((home, _), _)| *home == here).map(|(_, held)| held.view.object.clone()).collect()
        })
        .await
    }

    /// Launch `name` at this home, as a resolver's `get` reads it.
    pub async fn get(&self, name: &str) -> Result<ResourceObject<Convoy>, ResourceError> {
        self.find(name, None).await?.map(|source| source.object).ok_or_else(|| ResourceError::not_found(name))
    }

    /// Launch `name` held at `home`, or at this home when `None`.
    pub async fn find(&self, name: &str, home: Option<&str>) -> Result<Option<ReadResourceObject<Convoy>>, ResourceError> {
        Ok(self.find_held(name, home).await?.map(|held| held.view))
    }

    /// Launch `name` held at `home`, or at this home when `None`, with its version.
    async fn find_held(&self, name: &str, home: Option<&str>) -> Result<Option<HeldView>, ResourceError> {
        let home = match home {
            Some(home) => home.to_owned(),
            None => self.convoys().map_err(ResourceError::other)?.home().to_owned(),
        };
        let key = (home, name.to_owned());
        self.with_held(move |held| held.views.get(&key).cloned()).await
    }

    /// This home's launches that have not ended, from the views held: a read composes again only
    /// the launches changed since the last, shared with every other reader of the views. One that
    /// cannot be read counts as open: nothing says it ended.
    pub async fn open(&self) -> Result<BTreeSet<String>, ResourceError> {
        let here = self.convoys().map_err(ResourceError::other)?.home().to_owned();
        self.with_held(move |held| held.open.iter().filter(|(home, _)| *home == here).map(|(_, name)| name.clone()).collect()).await
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
    /// A measured read counts the fill or pass it waits on, the follower's included.
    async fn with_held<T>(&self, read: impl FnOnce(&HeldViews) -> T) -> Result<T, ResourceError> {
        let waiters = &self.held.waiters;
        waiters
            .wait(async {
                let mut state = self.held.state.lock().await;
                waiters.serve(self.read_held(&mut state, read)).await
            })
            .await
    }

    /// [`Self::with_held`] once the views are taken.
    async fn read_held<T>(&self, state: &mut Option<HeldViews>, read: impl FnOnce(&HeldViews) -> T) -> Result<T, ResourceError> {
        if state.is_none() {
            // Follow before filling, so no change between the fill and the first pass is missed.
            let wakes = self.wakes()?;
            let notes = self.convoys().map_err(ResourceError::other)?.memory().subscribe();
            let reads = Arc::clone(&self.reads);
            let cursors = self.blocking(move |_| Cursors::start(reads)).await?;
            let cursors_read = cursors.as_ref().map_or(0, Cursors::position);
            let mut held = HeldViews {
                views: BTreeMap::new(),
                following: cursors.map(|cursors| {
                    Arc::new(Mutex::new(Following { cursors, through: cursors_read, passed: BTreeSet::new(), rescanned: false }))
                }),
                notes,
                notes_read: 0,
                cursors_read,
                owed: BTreeSet::new(),
                refill_owed: false,
                unreadable: BTreeMap::new(),
                changed: UnloggedJournal::default(),
                index: ViewIndex::default(),
                open: BTreeSet::new(),
                ended_works: BTreeMap::new(),
            };
            self.refill(&mut held).await?;
            *state = Some(held);
            tokio::spawn(reads::attributed(reads::caller(FOLLOWER), self.clone().follow(wakes)));
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
    ///
    /// A read that takes the views before the follower has woken for a write composes that write's
    /// launches itself, so a writer reads back what it wrote. That read's cost grows with the
    /// writes not yet followed, not with the launches held: an occasional small cost in a read
    /// right after a write is this read-your-writes, not a regression.
    async fn follow(self, mut wakes: BoxStream<'static, ()>) {
        let mut ended = false;
        while !ended && wakes.next().await.is_some() {
            let mut state = self.held.state.lock().await;
            let Some(held) = state.as_mut() else { return };
            let waiters = &self.held.waiters;
            waiters.serve(self.follow_wakes(held, &mut wakes, &mut ended)).await;
            drop(state);
            if let Some(ticks) = self.held.ticks.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                let _ = ticks.send(());
            }
        }
    }

    /// Passes the views over one wake's changes and every wake waiting behind it.
    async fn follow_wakes(&self, held: &mut HeldViews, wakes: &mut BoxStream<'static, ()>, ended: &mut bool) {
        loop {
            if let Err(error) = self.pass(held).await {
                // The views are read again in full; a failed read leaves them as they were.
                tracing::warn!(%error, "convoy views: a change did not read; reading them all again");
                if let Err(error) = self.refill(held).await {
                    tracing::warn!(%error, "convoy views: reading them all again failed");
                }
            }
            // The changes made during the pass are read before a waiting read takes the views, so
            // it composes none of them; one pass reads every wake waiting.
            let mut woken = false;
            while let Some(wake) = wakes.next().now_or_never() {
                *ended |= wake.is_none();
                woken |= wake.is_some();
                if *ended {
                    break;
                }
            }
            if !woken {
                break;
            }
        }
    }

    /// Reads into the views the changes since the last pass: the records' from their cursors, and
    /// this home's notes.
    async fn pass(&self, held: &mut HeldViews) -> Result<(), ResourceError> {
        use tokio::sync::broadcast::error::TryRecvError;
        let Some(following) = held.following.clone() else { return self.refill(held).await };
        let passing = Arc::clone(&following);
        // A failed pass still moved the cursors: the views take how far they read before failing.
        let (failed, seen) = self
            .blocking(move |_| {
                let mut following = passing.lock().unwrap_or_else(PoisonError::into_inner);
                let passed = following.cursors.pass();
                following.through = following.cursors.position();
                let seen = following.cursors.joins_seen;
                Ok((
                    match passed {
                        Ok(Some(launches)) => {
                            following.passed.extend(launches);
                            None
                        }
                        Ok(None) => {
                            following.rescanned = true;
                            None
                        }
                        Err(error) => Some(error),
                    },
                    seen,
                ))
            })
            .await?;
        let (passed, through, rescanned) = {
            let mut following = following.lock().unwrap_or_else(PoisonError::into_inner);
            (std::mem::take(&mut following.passed), following.through, std::mem::take(&mut following.rescanned))
        };
        held.owed.extend(passed);
        held.cursors_read = through;
        if let Some(error) = failed {
            return Err(ResourceError::other(error));
        }
        if rescanned {
            return self.refill(held).await;
        }
        let here = self.convoys().map_err(ResourceError::other)?.home().to_owned();
        loop {
            match held.notes.try_recv() {
                Ok(name) => {
                    held.notes_read = held.notes_read.saturating_add(1);
                    held.owed.insert((here.clone(), name));
                }
                Err(TryRecvError::Lagged(missed)) => {
                    held.notes_read = held.notes_read.saturating_add(missed);
                    return self.refill(held).await;
                }
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        self.recompose(held).await?;
        // Every launch whose joins changed before `seen` is composed again.
        following.lock().unwrap_or_else(PoisonError::into_inner).cursors.composed(seen);
        Ok(())
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
                Some(record) => Some(self.compose(vec![record], held).await?.0),
                None => None,
            };
            let key = (home, name);
            match composed {
                Some(composed) => held.insert(composed).map_err(ResourceError::other)?,
                None => held.remove(&key),
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
        let (mut composed, joined) = self.compose(listing.records, held).await?;
        // Every launch was composed from the joins as held: their journal is read through there.
        if let (Some(following), (seen, position)) = (held.following.as_ref(), joined) {
            let mut following = following.lock().unwrap_or_else(PoisonError::into_inner);
            following.cursors.joins_seen = seen;
            following.cursors.joins_position = position;
        }
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
                ended: None,
            }
        }));
        // Each view read again keeps its version while it shows the same; the rest leave.
        let read =
            composed.iter().map(|composed| (composed.home.clone(), composed.view.object.metadata.name.clone())).collect::<BTreeSet<_>>();
        held.retain(&read);
        let listed = listing.unreadable.iter().map(|launch| (launch.home.clone(), launch.name.clone())).collect::<BTreeSet<_>>();
        held.unreadable.retain(|key, _| listed.contains(key));
        held.insert(composed).map_err(ResourceError::other)?;
        held.owed.clear();
        held.refill_owed = false;
        if let Some(following) = held.following.as_ref() {
            following.lock().unwrap_or_else(PoisonError::into_inner).cursors.composed(joined.0);
        }
        self.convoys().map_err(ResourceError::other)?.memory().set_unreadable(held.unreadable.len() as u64);
        Ok(())
    }

    /// `records` as read views, each joined with its works' Vessel launches and their sessions as
    /// the read port holds them, with the joins' journal sequence and log position they were
    /// composed at.
    ///
    /// An ended launch's Vessel launches are released, so the held joins evict them once they end:
    /// a work whose launches are no longer held keeps what `held` last composed of it.
    async fn compose(&self, records: Vec<ConvoyRecord>, held: &HeldViews) -> Result<(Vec<Composed>, (u64, u64)), ResourceError> {
        let prior: Vec<_> = records
            .iter()
            .map(|record| record.ended.as_ref().and_then(|_| held.ended_works.get(&(record.home.clone(), record.name.clone())).cloned()))
            .collect();
        let (records, joined, at) = self
            .blocking(move |reads| {
                let convoys = reads.convoys()?;
                let here = convoys.home().to_owned();
                reads.joins()?.read(reads, |mut joins| {
                    let joined = records
                        .iter()
                        .zip(prior)
                        .map(|(record, prior)| {
                            let notes = (record.home == here).then(|| convoys.memory().get(&record.name));
                            let mut joined =
                                join(record, notes.as_ref(), joins.vessels_of(&record.name), |vessel| joins.sessions_of(vessel));
                            for (work, facts) in prior.unwrap_or_default() {
                                joined.works.entry(work).or_insert(facts);
                            }
                            (notes, joined)
                        })
                        .collect::<Vec<_>>();
                    for record in records.iter().filter(|record| record.ended.is_some()) {
                        joins.release(&record.name, &record.home);
                    }
                    (records, joined, (joins.sequence(), joins.position()))
                })
            })
            .await?;
        let here = self.convoys().map_err(ResourceError::other)?.home().to_owned();
        let composed = records
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
                Composed {
                    home: record.home.clone(),
                    view: ReadResourceObject { object, provenance },
                    unreadable: None,
                    ended: record.ended.as_ref().map(|_| joined.works),
                }
            })
            .collect();
        Ok((composed, at))
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

    fn convoys(&self) -> Result<&dyn ConvoyRecords, String> {
        self.reads.convoys()
    }

    async fn engine_sources(&self) -> Result<Vec<Record<Convoy>>, String> {
        self.engine_records(|_| true).await
    }

    /// Every home's launch `name`, as the engine port's records: only those are converted.
    async fn engine_copies(&self, name: &str) -> Result<Vec<Record<Convoy>>, String> {
        let name = name.to_owned();
        self.engine_records(move |held_name| held_name == name).await
    }

    /// The launches `named` selects, as the engine port's records, each at its home.
    async fn engine_records(&self, named: impl Fn(&str) -> bool + Send + 'static) -> Result<Vec<Record<Convoy>>, String> {
        let held: Vec<(String, HeldView)> = self
            .with_held(move |held| {
                held.views.iter().filter(|((_, name), _)| named(name)).map(|((home, _), held)| (home.clone(), held.clone())).collect()
            })
            .await
            .map_err(|error| error.to_string())?;
        Ok(held.into_iter().map(|(home, held)| view_record(held, home)).collect())
    }
}

impl std::fmt::Debug for ConvoyViews {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ConvoyViews").field("namespace", &self.namespace).finish_non_exhaustive()
    }
}

/// `held`, held at `home`, as the engine port hands it out. Each is counted as a Convoy converted
/// to the port's shape.
fn view_record(held: HeldView, home: String) -> Record<Convoy> {
    flotilla_schema::reads::converted("Convoy", 1);
    crate::held_listing::view_record(held.view.object, home, held.version)
}

impl crate::EnginePort for ConvoyViews {
    fn install(self: Arc<Self>, backend: &crate::ResourceBackend) {
        // The views are composed when they open, as startup work: no read pays the first
        // composition. Without a runtime the first read composes them.
        if tokio::runtime::Handle::try_current().is_ok() {
            let views = Arc::clone(&self);
            tokio::spawn(reads::inherit(async move {
                if let Err(error) = views.with_held(|_| ()).await {
                    tracing::warn!(%error, "convoy views: composing them at open failed; the first read composes them");
                }
            }));
        }
        backend.install_engine_kind::<Convoy>(self);
    }
}

#[async_trait::async_trait]
impl KindRecords<Convoy> for ConvoyViews {
    fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
        use flotilla_resources::lifecycle_reads::{record_codes, RecordSet};
        record_codes(self.reads.as_ref(), &[RecordSet::Convoys, RecordSet::Vessels, RecordSet::Terminals])
    }

    /// From the views' follower, which composes them after the log and this home's notes change.
    fn ticks(&self) -> flotilla_resources::engine_records::Ticks {
        flotilla_resources::engine_records::Ticks::Followed
    }

    /// This home's notes on its launches.
    fn unlogged_changes(&self) -> Option<u64> {
        Some(self.reads.convoys().map_or(0, |convoys| convoys.memory().changed()))
    }

    fn writes(&self) -> bool {
        false
    }

    fn home(&self) -> String {
        self.convoys().map(|convoys| convoys.home().to_owned()).unwrap_or_default()
    }

    async fn get(&self, name: &str) -> Result<Option<Record<Convoy>>, String> {
        let home = self.home();
        match self.find_held(name, None).await.map_err(|error| error.to_string())? {
            Some(held) => Ok(Some(view_record(held, home))),
            // Another home's launch of this name reads where this home holds none: the lexically
            // first home's, as a listing chooses.
            None => Ok(self.engine_copies(name).await?.into_iter().min_by(|a, b| a.home.cmp(&b.home))),
        }
    }

    /// Each launch, this home's where it holds one of the name, else the lexically first home's.
    async fn list(&self) -> Result<Vec<Record<Convoy>>, String> {
        let home = self.home();
        let mut chosen = BTreeMap::<String, Record<Convoy>>::new();
        for object in self.engine_sources().await? {
            let keep = chosen.get(&object.name).is_some_and(|held| held.home == home || (object.home != home && held.home <= object.home));
            if !keep {
                chosen.insert(object.name.clone(), object);
            }
        }
        Ok(chosen.into_values().collect())
    }

    async fn sources(&self) -> Result<Vec<Record<Convoy>>, String> {
        self.engine_sources().await
    }

    async fn copies(&self, name: &str) -> Result<Vec<Record<Convoy>>, String> {
        self.engine_copies(name).await
    }

    /// The views `cover` selects, from the views' index, this home's alone when `local`: only
    /// those are converted.
    async fn covered(&self, cover: &IndexCover, local: bool) -> Option<Result<Vec<Record<Convoy>>, String>> {
        let cover = cover.clone();
        let here = match self.convoys() {
            Ok(convoys) => convoys.home().to_owned(),
            Err(error) => return Some(Err(error)),
        };
        let held = self.with_held(move |held| {
            held.index
                .covered(&cover)
                .into_iter()
                .filter(|(home, _)| !local || *home == here)
                .filter_map(|launch| Some((launch.0.clone(), held.views.get(&launch)?.clone())))
                .collect::<Vec<_>>()
        });
        Some(
            held.await
                .map_err(|error| error.to_string())
                .map(|held| held.into_iter().map(|(home, held)| view_record(held, home)).collect()),
        )
    }

    async fn positioned(&self) -> Result<(Vec<Record<Convoy>>, LogPosition), String> {
        let (held, position) = self
            .with_held(|held| (held.views.iter().map(|((home, _), held)| (home.clone(), held.clone())).collect::<Vec<_>>(), held.read()))
            .await
            .map_err(|error| error.to_string())?;
        Ok((held.into_iter().map(|(home, held)| view_record(held, home)).collect(), position))
    }

    async fn position(&self) -> Result<LogPosition, String> {
        self.with_held(HeldViews::read).await.map_err(|error| error.to_string())
    }

    /// Named from the views' own changes, which take in this home's notes: within this run only.
    async fn changed_since(&self, position: &LogPosition) -> Result<Since, String> {
        let position = position.clone();
        self.with_held(move |held| held.since(&position)).await.map_err(|error| error.to_string())
    }

    async fn apply(
        &self,
        name: &str,
        _labels: &BTreeMap<String, String>,
        _annotations: &BTreeMap<String, String>,
        _spec: &ConvoySpec,
    ) -> Result<Record<Convoy>, String> {
        Err(format!("convoy {name} is admitted on the engine, not written"))
    }

    async fn set_status(&self, name: &str, _status: &ConvoyStatus) -> Result<Record<Convoy>, String> {
        Err(format!("convoy {name}'s status is derived from the engine's records; it is not written"))
    }

    async fn delete(&self, name: &str) -> Result<(), String> {
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
    /// Where the launch's record says it ended, what its works were composed from.
    ended: Option<BTreeMap<String, WorkFacts>>,
}

impl HeldViews {
    /// Where a reader stands that has read the views held.
    fn read(&self) -> LogPosition {
        LogPosition { homes: BTreeMap::new(), unlogged: Some(self.changed.read()) }
    }

    /// The launches whose view changed since `position`.
    fn since(&self, position: &LogPosition) -> Since {
        match self.changed.since(position.unlogged.as_ref()) {
            Ok(names) => Since::Changed { names, through: self.read() },
            Err(reason) => Since::Expired(reason),
        }
    }

    /// How far the views have read: the log, by their cursors, and this home's notes.
    fn position(&self) -> u64 {
        self.cursors_read.saturating_add(self.notes_read)
    }

    /// Holds each of `composed`, keeping the version of a view that shows what its held one shows:
    /// all of it, its launch's write in its resource version among it.
    fn insert(&mut self, composed: Vec<Composed>) -> Result<(), String> {
        let position = self.position();
        for Composed { home, view, unreadable, ended } in composed {
            let key = (home, view.object.metadata.name.clone());
            match ended {
                Some(works) => {
                    self.open.remove(&key);
                    self.ended_works.insert(key.clone(), works);
                }
                None => {
                    self.open.insert(key.clone());
                    self.ended_works.remove(&key);
                }
            }
            match unreadable {
                Some((version, reason)) => {
                    // Told once for each version of the launch, not at each read of them all.
                    if self.unreadable.insert(key.clone(), version) != Some(version) {
                        tracing::warn!(launch = %key.1, home = %key.0, version, %reason, "convoy launch cannot be read");
                    }
                }
                None => drop(self.unreadable.remove(&key)),
            }
            let held = self.views.get(&key);
            let same = held.is_some_and(|held| {
                held.view.object.metadata == view.object.metadata
                    && held.view.object.spec == view.object.spec
                    && held.view.object.status == view.object.status
            });
            let version = crate::held_listing::version(held.map(|held| held.version), same, position)?;
            if !same {
                self.changed.record([key.1.clone()]);
                self.index.set(&key, crate::object_index_keys(&view.object).map_err(|error| error.to_string())?);
            }
            self.views.insert(key, HeldView { view, version });
        }
        Ok(())
    }

    fn remove(&mut self, key: &(String, String)) {
        if self.views.remove(key).is_some() {
            self.changed.record([key.1.clone()]);
            self.index.remove(key);
        }
        self.unreadable.remove(key);
        self.open.remove(key);
        self.ended_works.remove(key);
    }

    /// Keeps only the views `keep` names.
    fn retain(&mut self, keep: &BTreeSet<(String, String)>) {
        let gone: Vec<_> = self.views.keys().filter(|key| !keep.contains(*key)).cloned().collect();
        for key in &gone {
            self.remove(key);
        }
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

/// What `record` is joined with: its convoy's Vessel `launches` and each launch's sessions from
/// `sessions_of`, its home's `notes` telling which crew agents are wanted running.
fn join(
    record: &ConvoyRecord,
    notes: Option<&ConvoyNotes>,
    launches: Vec<VesselRecord>,
    sessions_of: impl Fn(&str) -> Vec<TerminalRecord>,
) -> Joined {
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
        let sessions = sessions_of(&vessel.name);
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
    joined
}
