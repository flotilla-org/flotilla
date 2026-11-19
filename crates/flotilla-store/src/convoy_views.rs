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
//! The views hold launches that have not retired. One that retires is held with how it ended
//! until its watchers have read that, and leaves on their next read, as an old Convoy was deleted;
//! one born retired is taken in for that alone. A read by name of a launch not held composes it
//! from the store, and nothing of it is kept.
//!
//! Installed as the backend's [`KindRecords`] for `Convoy`, it serves the typed reads, lists and
//! watches of the kind; its writes are refused.
//!
//! A read takes no lock while the views are current: each pass or fill that completes publishes
//! what the views show as an immutable [`Published`], with the commit counts it read through, and
//! a read whose counts that covers reads it as published. Only a read the views have not yet
//! caught up with takes the views' lock, and there it waits for the pass running, which may serve
//! it, or runs one; it never waits on another read of the views.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, PoisonError, RwLock,
    },
};

use chrono::DateTime;
use futures::{stream::BoxStream, FutureExt, StreamExt};

use flotilla_protocol::{LifecycleReading, NodeId};
use flotilla_resources::{
    convoy::view::VesselReading,
    convoy_records::{ConvoyChange, ConvoyJoins, ConvoyListing, ConvoyNotes, ConvoyRecord, ConvoyRecords, UnreadableLaunch, WorkFacts},
    engine_records::{IndexCover, KindRecords, LogPosition, Record, Since, UnloggedJournal, UnloggedRead, WriteError},
    held_joins::{JoinChange, JoinChanges, JoinsReader},
    lifecycle_reads::{ChangeCursor, LifecycleReads, Told},
    terminal_records::{newest_launches, TerminalRecord},
    vessel_records::{interrupted_message, VesselRecord},
    Convoy, ConvoySpec, ConvoyStatus, ReadResourceObject, ResourceError, ResourceObject, ResourceProvenance,
};
use flotilla_schema::reads;
use tracing::{info_span, Instrument};

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
    /// What passes and fills the views: one at a time, and only a read that needs one waits here.
    state: tokio::sync::Mutex<Option<HeldViews>>,
    /// What the views showed when the last pass or fill completed, read without the state's lock.
    /// None before the first fill.
    published: RwLock<Option<Arc<Published>>>,
    /// The launches whose view changed, in this run: what a reading resumes from. Written by a
    /// pass, under the state's lock; a read takes it only to name the changes since a position.
    changed: Arc<Mutex<UnloggedJournal>>,
    /// How many view changes the watchers have read: one that asks what changed since a position
    /// has read every change before it. Moved by reads without the state's lock.
    delivered: Arc<AtomicU64>,
    /// A read without the lock let a retired launch leave: the follower lets it go and publishes,
    /// so the reads after it are not sent to the lock to.
    due: tokio::sync::Notify,
    ticks: Mutex<Option<tokio::sync::broadcast::Sender<()>>>,
    /// The measured reads waiting on a fill or pass of the views, which count its reads.
    waiters: reads::Waiters,
    /// The commits the convoys' cursor was told of, read without the state's lock: with the held
    /// joins' count and this home's note count, whether the views hold every commit made before a
    /// read (see [`HeldViews::current`]). Unset where the cursor cannot tell.
    told: std::sync::OnceLock<Told>,
}

/// The commits told to the convoys' cursor and to the held joins' cursors, and how many times this
/// home's notes changed.
type ToldCounts = (u64, u64, u64);

/// Whether views read through the counts `through` hold every change counted in `told`.
fn covers(through: Option<ToldCounts>, told: Option<ToldCounts>) -> bool {
    matches!((through, told), (Some(through), Some(told)) if through.0 >= told.0 && through.1 >= told.1 && through.2 >= told.2)
}

/// The caller the follower's reads count for.
const FOLLOWER: &str = "Convoy views";

/// Launches, by `(home, launch)`.
type Launch = (String, String);

/// What the views show to readers: replaced, never changed, once published.
#[derive(Clone, Default)]
struct Shown {
    views: BTreeMap<Launch, Arc<HeldView>>,
    /// The views held by each index key their labels and declared fields give, as the store's
    /// query index names them: a selecting query reads only the views it may select.
    index: ViewIndex<Launch>,
    /// The launches held that have not ended. One that cannot be read counts: nothing says it
    /// ended.
    open: BTreeSet<Launch>,
    /// The retired launches held, each with the count of the view change its watchers must read
    /// before it leaves.
    retiring: BTreeMap<Launch, u64>,
}

/// The views as the last pass or fill that completed left them, with the commit counts they hold
/// every change of.
struct Published {
    shown: Arc<Shown>,
    /// The counts read before that pass or fill: none where they cannot be told, and no read is
    /// then served from here.
    through: Option<ToldCounts>,
    /// Where a reader stands that has read these views.
    read: UnloggedRead,
    /// How far the views had read: the log, by their cursors, and this home's notes.
    position: u64,
    /// How many entries the views held, over every launch they kept anything of.
    entries: usize,
}

impl Published {
    /// Whether these views serve a read made after the commit counts `told` were read: they hold
    /// every change counted, and no retired launch is due to leave them, as one is once its
    /// watchers have read it (through `delivered`) or once nothing watches.
    fn serves(&self, told: Option<ToldCounts>, delivered: u64, watched: impl FnOnce() -> bool) -> bool {
        covers(self.through, told)
            && (self.shown.retiring.is_empty() || (self.shown.retiring.values().all(|told| *told > delivered) && watched()))
    }

    /// Where a reader stands that has read these views.
    fn log_position(&self) -> LogPosition {
        LogPosition { homes: BTreeMap::new(), unlogged: Some(self.read.clone()) }
    }
}

struct HeldViews {
    /// What the views show, shared with the views last published: a pass changes a copy.
    shown: Arc<Shown>,
    /// [`Held::changed`].
    changed: Arc<Mutex<UnloggedJournal>>,
    /// [`Held::delivered`].
    delivered: Arc<AtomicU64>,
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
    /// The launches owed whose own record changed, not only what they join: a retired one not held
    /// is taken in only when its record changed, so its watchers are told how it ended once.
    logged: BTreeSet<(String, String)>,
    /// A read of every launch was begun and not finished.
    refill_owed: bool,
    /// The launches held as unreadable, by `(home, launch)`, at the version last told.
    unreadable: BTreeMap<(String, String), u64>,
    /// What each ended launch's works were last composed from, by `(home, launch)`: its Vessel
    /// launches are released, and once the held joins evict them a view composed again keeps
    /// these.
    ended_works: BTreeMap<(String, String), BTreeMap<String, WorkFacts>>,
    /// The commit counts read before the last pass or fill that completed: the views hold every
    /// commit counted. None where the counts cannot be told, and each read then passes.
    through: Option<ToldCounts>,
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
    /// Those of them whose own record changed.
    logged: BTreeSet<(String, String)>,
    /// The cursors could not tell what changed: every launch is read again.
    rescanned: bool,
}

/// Launches, by `(home, launch)`.
type Launches = BTreeSet<(String, String)>;

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

    /// The launches, by `(home, name)`, whose records or joins changed since the last pass, and
    /// those of them whose records changed; none when anything may have.
    fn pass(&mut self) -> Result<Option<(Launches, Launches)>, String> {
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
                            // Evicted once no view composes from it.
                            JoinChange::TerminalEvicted { .. } => None,
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
                let logged = launches.clone();
                launches.extend(joined);
                Ok(Some((launches, logged)))
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
        self.with_shown(|shown| shown.views.values().map(|held| held.view.clone()).collect()).await
    }

    /// This home's launches.
    pub async fn here(&self) -> Result<Vec<ResourceObject<Convoy>>, ResourceError> {
        let here = self.convoys().map_err(ResourceError::other)?.home().to_owned();
        self.with_shown(move |shown| {
            shown.views.iter().filter(|((home, _), _)| *home == here).map(|(_, held)| held.view.object.clone()).collect()
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
        let key = (home.clone(), name.to_owned());
        match self.with_shown(move |shown| shown.views.get(&key).map(|held| HeldView::clone(held))).await? {
            Some(held) => Ok(Some(held)),
            None => Ok(self.stored(name, Some(&home), BTreeSet::new()).await?.pop()),
        }
    }

    /// This home's launches that have not ended, from the views held: a read composes again only
    /// the launches changed since the last, shared with every other reader of the views. One that
    /// cannot be read counts as open: nothing says it ended.
    pub async fn open(&self) -> Result<BTreeSet<String>, ResourceError> {
        let here = self.convoys().map_err(ResourceError::other)?.home().to_owned();
        self.with_shown(move |shown| shown.open.iter().filter(|(home, _)| *home == here).map(|(_, name)| name.clone()).collect()).await
    }

    /// How many entries the views hold, over every launch they keep anything of: what grows with
    /// the launches held.
    pub async fn held(&self) -> Result<usize, ResourceError> {
        Ok(self.published().await?.entries)
    }

    /// The newest launch of `of` in `project` held at `home`, or at this home when `None`.
    pub async fn newest(&self, of: &str, project: &str, home: Option<&str>) -> Result<Option<ReadResourceObject<Convoy>>, ResourceError> {
        let (of, project, home) = (of.to_owned(), project.to_owned(), home.map(str::to_owned));
        let record = self.blocking(move |reads| reads.convoys()?.newest(&of, &project, home.as_deref())).await?;
        let Some(record) = record else { return Ok(None) };
        self.find(&record.name, Some(&record.home)).await
    }

    /// Waits for the views' first composition, begun when they were installed, or composes them
    /// where none has begun. Startup awaits it before serving, so no request pays it: the first
    /// composition reads every launch, and a read waiting on it counts those reads.
    /// A composition that fails is logged, and the first read composes them.
    pub async fn opened(&self) {
        if let Err(error) = self.published().await {
            tracing::warn!(%error, "convoy views: composing them at open failed; the first read composes them");
        }
    }

    /// Reads the views as published, once they hold every change written before the read
    /// ([`Self::published`]).
    async fn with_shown<T>(&self, read: impl FnOnce(&Shown) -> T) -> Result<T, ResourceError> {
        Ok(read(&self.published().await?.shown))
    }

    /// The views as published, once they hold every change written before the read: the commits
    /// counted before it are read in by a pass, one that began after them or its own. While the
    /// views last published hold every commit counted, the read takes them without a lock and
    /// reads no store; otherwise it waits for the pass running or runs one (see
    /// [`Self::catch_up`]).
    async fn published(&self) -> Result<Arc<Published>, ResourceError> {
        // Before anything is read: a pass that began after this read the commits counted here.
        let told = self.told();
        if let Some(published) = self.serving(told) {
            return Ok(published);
        }
        let waiters = &self.held.waiters;
        waiters
            .wait(async {
                let mut state = self.held.state.lock().instrument(info_span!("convoy_views.lock_wait")).await;
                // A pass or eviction published while this read waited may serve it: then it
                // passes nothing and publishes nothing.
                if let Some(published) = self.serving(told) {
                    return Ok(published);
                }
                Ok(waiters.serve(self.read_held(&mut state, told, |_| ())).await?.0)
            })
            .await
    }

    /// A watcher has read `published` through view change `read`: a retired launch it has now read
    /// is due to leave, and the follower lets it go.
    fn delivered(&self, published: &Published, read: u64) {
        let before = self.held.delivered.fetch_max(read, Ordering::SeqCst);
        if published.shown.retiring.values().any(|told| *told > before && *told <= read) {
            self.held.due.notify_one();
        }
    }

    /// The views last published, where they serve a read made after the commit counts `told`
    /// were read.
    fn serving(&self, told: Option<ToldCounts>) -> Option<Arc<Published>> {
        let published = self.held.published.read().unwrap_or_else(PoisonError::into_inner).clone()?;
        let delivered = self.held.delivered.load(Ordering::SeqCst);
        published.serves(told, delivered, || self.watched()).then_some(published)
    }

    /// Takes the views, filling them on first use and starting the follower that keeps them, and
    /// passes them unless a pass that read the commits counted in `told` has completed, as one may
    /// while this waits. Then `then` runs on the views, those due to leave leave, and the views are
    /// published. The lock is held for the pass, if any, and no read of the views is made under it:
    /// a read that waits here waits on a pass, never on another read.
    /// A measured read counts the fill or pass it waits on, the follower's included.
    async fn catch_up<T>(
        &self,
        told: Option<ToldCounts>,
        then: impl FnOnce(&mut HeldViews) -> T,
    ) -> Result<(Arc<Published>, T), ResourceError> {
        let waiters = &self.held.waiters;
        waiters
            .wait(async {
                let mut state = self.held.state.lock().instrument(info_span!("convoy_views.lock_wait")).await;
                waiters.serve(self.read_held(&mut state, told, then)).await
            })
            .await
    }

    /// The commits told so far to the convoys' cursor and the held joins' cursors, and how many
    /// times this home's notes changed; none where either cursor cannot tell.
    fn told(&self) -> Option<ToldCounts> {
        let convoys = self.held.told.get()?.get();
        let joins = self.reads.joins().ok()?.told()?;
        let notes = self.convoys().ok()?.memory().changed();
        Some((convoys, joins, notes))
    }

    /// Publishes `held` as the views readers read.
    fn publish(&self, held: &HeldViews) -> Arc<Published> {
        let published = Arc::new(held.published());
        *self.held.published.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&published));
        published
    }

    /// [`Self::catch_up`] once the views are taken, with the commits `told` before it waited.
    async fn read_held<T>(
        &self,
        state: &mut Option<HeldViews>,
        told: Option<ToldCounts>,
        then: impl FnOnce(&mut HeldViews) -> T,
    ) -> Result<(Arc<Published>, T), ResourceError> {
        if state.is_none() {
            // Follow before filling, so no change between the fill and the first pass is missed.
            let wakes = self.wakes()?;
            let notes = self.convoys().map_err(ResourceError::other)?.memory().subscribe();
            let reads = Arc::clone(&self.reads);
            let cursors = self.blocking(move |_| Cursors::start(reads)).await?;
            if let Some(told) = cursors.as_ref().and_then(|cursors| cursors.convoys.told()) {
                // Set once: the views are filled once, and their cursors kept.
                let _ = self.held.told.set(told);
            }
            let cursors_read = cursors.as_ref().map_or(0, Cursors::position);
            let mut held = HeldViews {
                shown: Arc::default(),
                changed: Arc::clone(&self.held.changed),
                delivered: Arc::clone(&self.held.delivered),
                following: cursors.map(|cursors| {
                    Arc::new(Mutex::new(Following {
                        cursors,
                        through: cursors_read,
                        passed: BTreeSet::new(),
                        logged: BTreeSet::new(),
                        rescanned: false,
                    }))
                }),
                notes,
                notes_read: 0,
                cursors_read,
                owed: BTreeSet::new(),
                logged: BTreeSet::new(),
                refill_owed: false,
                unreadable: BTreeMap::new(),
                ended_works: BTreeMap::new(),
                through: None,
            };
            self.refill(&mut held).await?;
            *state = Some(held);
            tokio::spawn(reads::attributed(reads::caller(FOLLOWER), self.clone().follow(wakes)));
        }
        let held = state.as_mut().expect("filled above");
        if !held.current(told) {
            if let Err(error) = self.pass(held).instrument(info_span!("convoy_views.pass", by = "read")).await {
                tracing::warn!(%error, "convoy views: a change did not read; reading them all again");
                self.refill(held).await?;
            }
        }
        let then = then(held);
        held.evict(self.watched());
        Ok((self.publish(held), then))
    }

    /// Whether anything watches the views' changes: with nothing watching, a retired launch
    /// leaves at once.
    fn watched(&self) -> bool {
        self.held.ticks.lock().unwrap_or_else(PoisonError::into_inner).as_ref().is_some_and(|ticks| ticks.receiver_count() > 0)
    }

    /// Ticks the watchers.
    fn tick(&self) {
        if let Some(ticks) = self.held.ticks.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            let _ = ticks.send(());
        }
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

    /// Recomposes the views each wake's changes touch, and ticks the watchers. One pass reads
    /// every wake waiting, and the views are let go between passes, so a read waits on at most
    /// the pass running.
    ///
    /// A read that takes the views before the follower has passed over a write composes that
    /// write's launches itself, so a writer reads back what it wrote. That read's cost grows with
    /// the writes not yet followed, not with the launches held, and a pass that began after the
    /// read's commits were counted serves it.
    async fn follow(self, mut wakes: BoxStream<'static, ()>) {
        loop {
            // A commit, or a retired launch a read let leave.
            let woken = tokio::select! {
                wake = wakes.next() => wake.is_some(),
                () = self.held.due.notified() => true,
            };
            if !woken {
                return;
            }
            let mut ended = false;
            while let Some(wake) = wakes.next().now_or_never() {
                if wake.is_none() {
                    ended = true;
                    break;
                }
            }
            let told = self.told();
            let mut state = self.held.state.lock().instrument(info_span!("convoy_views.lock_wait", by = "follower")).await;
            let Some(held) = state.as_mut() else { return };
            let waiters = &self.held.waiters;
            waiters.serve(self.follow_wake(held, told)).await;
            self.publish(held);
            drop(state);
            self.tick();
            if ended {
                return;
            }
        }
    }

    /// Passes the views over the changes since the last pass, unless a read already has.
    async fn follow_wake(&self, held: &mut HeldViews, told: Option<ToldCounts>) {
        if !held.current(told) {
            if let Err(error) = self.pass(held).instrument(info_span!("convoy_views.pass", by = "follower")).await {
                // The views are read again in full; a failed read leaves them as they were.
                tracing::warn!(%error, "convoy views: a change did not read; reading them all again");
                if let Err(error) = self.refill(held).await {
                    tracing::warn!(%error, "convoy views: reading them all again failed");
                }
            }
        }
        held.evict(self.watched());
    }

    /// Reads into the views the changes since the last pass: the records' from their cursors, and
    /// this home's notes.
    async fn pass(&self, held: &mut HeldViews) -> Result<(), ResourceError> {
        use tokio::sync::broadcast::error::TryRecvError;
        let Some(following) = held.following.clone() else { return self.refill(held).await };
        // Before the cursors read the log: the commits counted are among what they read.
        let told = self.told();
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
                        Ok(Some((launches, logged))) => {
                            following.passed.extend(launches);
                            following.logged.extend(logged);
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
            .instrument(info_span!("convoy_views.cursors"))
            .await?;
        let (passed, logged, through, rescanned) = {
            let mut following = following.lock().unwrap_or_else(PoisonError::into_inner);
            (
                std::mem::take(&mut following.passed),
                std::mem::take(&mut following.logged),
                following.through,
                std::mem::take(&mut following.rescanned),
            )
        };
        held.owed.extend(passed);
        held.logged.extend(logged);
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
        if !held.owed.is_empty() || held.refill_owed {
            let span = info_span!("convoy_views.recompose", launches = held.owed.len());
            self.recompose(held).instrument(span).await?;
        }
        // Every launch whose joins changed before `seen` is composed again.
        following.lock().unwrap_or_else(PoisonError::into_inner).cursors.composed(seen);
        held.through = told;
        Ok(())
    }

    /// Composes the launches owed again from their records; one with no record leaves the views,
    /// and one that cannot be read is held as unreadable, with why. A retired one not held is
    /// taken in only where its own record changed: it retired since the last pass, and its
    /// watchers are yet to read how it ended. The launches owed are read in one read of the store
    /// and composed together; each stays owed until its view is held.
    async fn recompose(&self, held: &mut HeldViews) -> Result<(), ResourceError> {
        if held.refill_owed {
            return self.refill(held).await;
        }
        let here = self.convoys().map_err(ResourceError::other)?.home().to_owned();
        let owed: Vec<(String, String)> = held.owed.iter().cloned().collect();
        let keys = owed.clone();
        let listings = self
            .blocking(move |reads| {
                let convoys = reads.convoys()?;
                keys.iter().map(|(home, name)| convoys.named(name, Some(home))).collect::<Result<Vec<_>, _>>()
            })
            .await?;
        let mut records = Vec::new();
        let mut composed = Vec::new();
        let mut gone = Vec::new();
        for (key, ConvoyListing { records: read, unreadable }) in owed.iter().zip(listings) {
            match (read.into_iter().next(), unreadable.first()) {
                (Some(record), _) if record.retired && !held.shown.views.contains_key(key) && !held.logged.contains(key) => gone.push(key),
                (Some(record), _) => records.push(record),
                (None, Some(launch)) => composed.push(Composed {
                    home: launch.home.clone(),
                    view: unreadable_view(&self.namespace, &here, launch),
                    unreadable: Some((launch.version, launch.reason.clone())),
                    ended: None,
                    retired: false,
                }),
                (None, None) => gone.push(key),
            }
        }
        if !records.is_empty() {
            composed.extend(self.compose(records, held).await?.0);
        }
        held.insert(composed).map_err(ResourceError::other)?;
        for key in gone {
            held.remove(key);
        }
        for key in &owed {
            held.owed.remove(key);
            held.logged.remove(key);
        }
        self.convoys().map_err(ResourceError::other)?.memory().set_unreadable(held.unreadable.len() as u64);
        Ok(())
    }

    /// Reads every launch again. A launch held, or owed for its record's change, that the listing
    /// leaves out is read by name: one that retired is held with how it ended until its watchers
    /// have read it, as a retired one held already is. Until it has, the whole views are owed.
    async fn refill(&self, held: &mut HeldViews) -> Result<(), ResourceError> {
        self.refill_all(held).instrument(info_span!("convoy_views.refill")).await
    }

    /// [`Self::refill`], unspanned.
    async fn refill_all(&self, held: &mut HeldViews) -> Result<(), ResourceError> {
        // Before the listing: the commits counted are among what it reads.
        let told = self.told();
        held.refill_owed = true;
        let unlisted: BTreeSet<_> =
            held.shown.views.keys().chain(held.logged.iter()).filter(|key| !held.shown.retiring.contains_key(*key)).cloned().collect();
        let listing = self
            .blocking(move |reads| {
                let convoys = reads.convoys()?;
                let mut listing = convoys.every()?;
                let listed: BTreeSet<_> = listing.records.iter().map(|record| (record.home.clone(), record.name.clone())).collect();
                for (home, name) in unlisted.iter().filter(|key| !listed.contains(*key)) {
                    // One that cannot be read is not held as retiring: its watchers read it gone.
                    listing.records.extend(convoys.named(name, Some(home))?.records.into_iter().filter(|record| record.retired));
                }
                Ok(listing)
            })
            .await?;
        let (mut composed, joined) = self.compose(listing.records, held).await?;
        // Every launch was composed from the joins as held: their journal is read through there.
        if let (Some(following), (seen, position)) = (held.following.as_ref(), joined) {
            let mut following = following.lock().unwrap_or_else(PoisonError::into_inner);
            following.cursors.joins_seen = seen;
            following.cursors.joins_position = position;
        }
        let here = self.convoys().map_err(ResourceError::other)?.home().to_owned();
        // A launch that cannot be read stays listed, with why.
        composed.extend(listing.unreadable.iter().map(|launch| Composed {
            home: launch.home.clone(),
            view: unreadable_view(&self.namespace, &here, launch),
            unreadable: Some((launch.version, launch.reason.clone())),
            ended: None,
            retired: false,
        }));
        // Each view read again keeps its version while it shows the same; the rest leave, but for
        // the retiring, which leave once their watchers have read them.
        let mut read =
            composed.iter().map(|composed| (composed.home.clone(), composed.view.object.metadata.name.clone())).collect::<BTreeSet<_>>();
        read.extend(held.shown.retiring.keys().cloned());
        held.retain(&read);
        let listed = listing.unreadable.iter().map(|launch| (launch.home.clone(), launch.name.clone())).collect::<BTreeSet<_>>();
        held.unreadable.retain(|key, _| listed.contains(key));
        held.insert(composed).map_err(ResourceError::other)?;
        held.owed.clear();
        held.logged.clear();
        held.refill_owed = false;
        held.through = told;
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
            .map(|(record, (notes, joined))| Composed {
                home: record.home.clone(),
                view: view_of(&self.namespace, &here, &record, notes.as_ref(), &joined),
                unreadable: None,
                ended: record.ended.as_ref().map(|_| joined.works),
                retired: record.retired,
            })
            .collect();
        Ok((composed, at))
    }

    /// Launch `name` at `home`, or at every home when `None`, composed from the store for a read
    /// by name of a launch the views do not hold: a retired one, which reads with how it ended.
    /// The homes in `held`, whose launch of the name the views hold, are left out. Each is joined
    /// with its works' Vessel launches by their index, and their sessions; one that cannot be read
    /// is its unreadable view, with why. Nothing of it is kept.
    async fn stored(&self, name: &str, home: Option<&str>, held: BTreeSet<String>) -> Result<Vec<HeldView>, ResourceError> {
        let (name, home, namespace) = (name.to_owned(), home.map(str::to_owned), self.namespace.clone());
        let views = self
            .blocking(move |reads| {
                let convoys = reads.convoys()?;
                let here = convoys.home().to_owned();
                let ConvoyListing { mut records, mut unreadable } = convoys.named(&name, home.as_deref())?;
                records.retain(|record| !held.contains(&record.home));
                unreadable.retain(|launch| !held.contains(&launch.home));
                let mut views: Vec<_> = unreadable.iter().map(|launch| unreadable_view(&namespace, &here, launch)).collect();
                if records.is_empty() {
                    return Ok(views);
                }
                let (vessels, terminals) = (reads.vessels()?, reads.terminals()?);
                let launches = vessels.of_convoy(&name)?;
                let mut sessions = BTreeMap::new();
                for launch in &launches {
                    sessions.insert(launch.name.clone(), terminals.of_vessel(&launch.name)?);
                }
                views.extend(records.iter().map(|record| {
                    let notes = (record.home == here).then(|| convoys.memory().get(&record.name));
                    let joined = join(record, notes.as_ref(), launches.clone(), |vessel| sessions.get(vessel).cloned().unwrap_or_default());
                    view_of(&namespace, &here, record, notes.as_ref(), &joined)
                }));
                Ok(views)
            })
            .instrument(info_span!("convoy_views.stored"))
            .await?;
        if views.is_empty() {
            return Ok(Vec::new());
        }
        let position = self.published().await?.position;
        Ok(views.into_iter().map(|view| HeldView { view, version: position }).collect())
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

    /// What changed since `position`, named from `published` without the views' lock, and whether
    /// a retired launch is held; none where the read must take the lock: a retired launch is due
    /// to leave once the watcher has read through `position`, or a pass has changed the views
    /// since `published` and not yet published them.
    fn changed_since_published(
        &self,
        published: &Published,
        position: &LogPosition,
        told: Option<ToldCounts>,
        watched: bool,
    ) -> Option<(Since, bool)> {
        if let Some(read) = position.unlogged.as_ref().filter(|read| read.run == published.read.run && read.read <= published.read.read) {
            self.delivered(published, read.read);
        }
        if !published.serves(told, self.held.delivered.load(Ordering::SeqCst), || watched) {
            return None;
        }
        let changed = self.held.changed.lock().unwrap_or_else(PoisonError::into_inner);
        if changed.count() != published.read.read {
            return None;
        }
        let since = match changed.since(position.unlogged.as_ref()) {
            Ok(names) => Since::Changed { names, through: published.log_position() },
            Err(reason) => Since::Expired(reason),
        };
        Some((since, !published.shown.retiring.is_empty()))
    }

    /// The launches `named` selects, as the engine port's records, each at its home.
    async fn engine_records(&self, named: impl Fn(&str) -> bool + Send + 'static) -> Result<Vec<Record<Convoy>>, String> {
        let held: Vec<(String, HeldView)> = self
            .with_shown(move |shown| {
                shown
                    .views
                    .iter()
                    .filter(|((_, name), _)| named(name))
                    .map(|((home, _), held)| (home.clone(), HeldView::clone(held)))
                    .collect()
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
/// `record` as a read view at home `here`, joined as `joined` with its home's `notes`.
fn view_of(namespace: &str, here: &str, record: &ConvoyRecord, notes: Option<&ConvoyNotes>, joined: &Joined) -> ReadResourceObject<Convoy> {
    let working = record.working(&joined.live, notes);
    let object = record.read_view(namespace, ConvoyJoins { works: &joined.works, working: &working, notes });
    let provenance = if record.home == here {
        ResourceProvenance::Local
    } else {
        let synced = record.ended.as_ref().map_or(record.admitted_at, |(_, at)| *at);
        ResourceProvenance::Replica { origin_root: NodeId::new(record.home.clone()), last_synced_at: synced }
    };
    ReadResourceObject { object, provenance }
}

/// Launch `launch`, which cannot be read, as its read view at home `here`, with why; when another
/// home's last changed is not known.
fn unreadable_view(namespace: &str, here: &str, launch: &UnreadableLaunch) -> ReadResourceObject<Convoy> {
    let provenance = if launch.home == here {
        ResourceProvenance::Local
    } else {
        ResourceProvenance::Replica { origin_root: NodeId::new(launch.home.clone()), last_synced_at: DateTime::UNIX_EPOCH }
    };
    ReadResourceObject { object: launch.read_view(namespace), provenance }
}

fn view_record(held: HeldView, home: String) -> Record<Convoy> {
    flotilla_schema::reads::converted("Convoy", 1);
    crate::held_listing::view_record(held.view.object, home, held.version)
}

impl crate::EnginePort for ConvoyViews {
    fn install(self: Arc<Self>, backend: &crate::ResourceBackend) {
        // The views are composed when they open, as startup work, which startup awaits with
        // `opened`. Without a runtime the first read composes them.
        if tokio::runtime::Handle::try_current().is_ok() {
            let views = Arc::clone(&self);
            tokio::spawn(reads::inherit(async move { views.opened().await }));
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
        let key = (home.clone(), name.to_owned());
        let held = self.with_shown(move |shown| shown.views.get(&key).map(|held| HeldView::clone(held)));
        if let Some(held) = held.await.map_err(|error| error.to_string())? {
            return Ok(Some(view_record(held, home)));
        }
        // This home's launch of the name where it has one, else another home's: the lexically
        // first home's, as a listing chooses.
        Ok(self.copies(name).await?.into_iter().min_by(|a, b| (a.home != home, &a.home).cmp(&(b.home != home, &b.home))))
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

    /// Every home's launch of the name: each the views hold as held, and every other home's
    /// composed from the store, a retired one with how it ended.
    async fn copies(&self, name: &str) -> Result<Vec<Record<Convoy>>, String> {
        let mut copies = self.engine_copies(name).await?;
        let held: BTreeSet<String> = copies.iter().map(|copy| copy.home.clone()).collect();
        let stored = self.stored(name, None, held).await.map_err(|error| error.to_string())?;
        let here = self.home();
        copies.extend(stored.into_iter().map(|view| {
            let home = match &view.view.provenance {
                ResourceProvenance::Local => here.clone(),
                ResourceProvenance::Replica { origin_root, .. } => origin_root.as_str().to_owned(),
            };
            view_record(view, home)
        }));
        Ok(copies)
    }

    /// The newest launch holding the role `labels` name, in the project they name or in none,
    /// a retired one with how it ended: a plain name's last resort. Labels other than those two
    /// select nothing here.
    async fn newest_labelled(&self, labels: &BTreeMap<String, String>) -> Option<Result<Option<Record<Convoy>>, String>> {
        use flotilla_resources::labels::{PROJECT_LABEL, ROLE_LABEL};
        let role = labels.get(ROLE_LABEL)?.clone();
        if labels.keys().any(|label| label != ROLE_LABEL && label != PROJECT_LABEL) {
            return None;
        }
        let project = labels.get(PROJECT_LABEL).cloned();
        let newest = match self.blocking(move |reads| reads.convoys()?.newest_holding(&role, project.as_deref())).await {
            Ok(newest) => newest,
            Err(error) => return Some(Err(error.to_string())),
        };
        let Some(record) = newest else { return Some(Ok(None)) };
        Some(match self.stored(&record.name, Some(&record.home), BTreeSet::new()).await {
            Ok(views) => Ok(views.into_iter().next().map(|view| view_record(view, record.home.clone()))),
            Err(error) => Err(error.to_string()),
        })
    }

    /// The launches of the name the views hold: what a reader of the listing holds of it, never
    /// a retired one composed from the store.
    async fn sources_named(&self, name: &str) -> Result<Vec<Record<Convoy>>, String> {
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
        let held = self.with_shown(move |shown| {
            shown
                .index
                .covered(&cover)
                .into_iter()
                .filter(|(home, _)| !local || *home == here)
                .filter_map(|launch| Some((launch.0.clone(), HeldView::clone(shown.views.get(&launch)?))))
                .collect::<Vec<_>>()
        });
        Some(
            held.await
                .map_err(|error| error.to_string())
                .map(|held| held.into_iter().map(|(home, held)| view_record(held, home)).collect()),
        )
    }

    async fn positioned(&self) -> Result<(Vec<Record<Convoy>>, LogPosition), String> {
        let published = self.published().await.map_err(|error| error.to_string())?;
        // A listing reads every change before its position: its reader has read the retired,
        // which the follower then lets go.
        self.delivered(&published, published.read.read);
        let held = published.shown.views.iter().map(|((home, _), held)| view_record(HeldView::clone(held), home.clone())).collect();
        Ok((held, published.log_position()))
    }

    async fn position(&self) -> Result<LogPosition, String> {
        Ok(self.published().await.map_err(|error| error.to_string())?.log_position())
    }

    /// Named from the views' own changes, which take in this home's notes: within this run only.
    ///
    /// The watcher asking has read every change before its position: each retired launch it has
    /// read leaves, and is named here. One whose ending it reads now is named, and a tick brings
    /// the watcher back to read it gone.
    async fn changed_since(&self, position: &LogPosition) -> Result<Since, String> {
        let (position, watched) = (position.clone(), self.watched());
        let told = self.told();
        let read = match self.serving(told) {
            Some(published) => self.changed_since_published(&published, &position, told, watched),
            None => None,
        };
        let (since, retiring) = match read {
            Some(read) => read,
            None => {
                let (_, read) = self
                    .catch_up(told, move |held| {
                        held.delivered_to(&position);
                        held.evict(watched);
                        (held.since(&position), !held.shown.retiring.is_empty())
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                read
            }
        };
        if retiring {
            self.tick();
        }
        Ok(since)
    }

    async fn apply(
        &self,
        name: &str,
        _version: Option<u64>,
        _labels: &BTreeMap<String, String>,
        _annotations: &BTreeMap<String, String>,
        _spec: &ConvoySpec,
    ) -> Result<Record<Convoy>, WriteError> {
        Err(format!("convoy {name} is admitted on the engine, not written").into())
    }

    async fn set_status(
        &self,
        name: &str,
        _version: u64,
        _status: &ConvoyStatus,
    ) -> Result<Record<Convoy>, flotilla_resources::engine_records::WriteError> {
        Err(format!("convoy {name}'s status is derived from the engine's records; it is not written").into())
    }

    async fn delete(&self, name: &str) -> Result<(), String> {
        Err(format!("convoy {name} ends on the engine; it is not deleted"))
    }

    /// A tick each time the views held change: a launch's record at any home, what this home
    /// observes of one, or a Vessel or terminal session its works join.
    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        // Hold the views first, so the follower that ticks is running.
        self.published().await.map_err(|error| error.to_string())?;
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
    /// The launch retired: it is held until its watchers have read how it ended.
    retired: bool,
}

impl HeldViews {
    /// Whether the views hold every change made before the commit counts `told` were read: a
    /// pass that read the log and the notes at or after those counts completed, and nothing
    /// passed is left to compose. Each count moves before its writer's call returns, so a writer
    /// that reads after writing reads its write.
    fn current(&self, told: Option<ToldCounts>) -> bool {
        covers(self.through, told) && self.owed.is_empty() && !self.refill_owed
    }

    /// The views as readers read them until the next are published.
    fn published(&self) -> Published {
        Published {
            shown: Arc::clone(&self.shown),
            // Views a pass left owed hold nothing they can be shown to hold.
            through: self.through.filter(|_| self.owed.is_empty() && !self.refill_owed),
            read: self.journal().read(),
            position: self.position(),
            entries: self.entries(),
        }
    }

    fn journal(&self) -> std::sync::MutexGuard<'_, UnloggedJournal> {
        self.changed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The launches whose view changed since `position`.
    fn since(&self, position: &LogPosition) -> Since {
        let changed = self.journal();
        match changed.since(position.unlogged.as_ref()) {
            Ok(names) => Since::Changed { names, through: LogPosition { homes: BTreeMap::new(), unlogged: Some(changed.read()) } },
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
        if composed.is_empty() {
            return Ok(());
        }
        let position = self.position();
        // A copy of what was last published, where that still holds it: readers keep theirs.
        let shown = Arc::make_mut(&mut self.shown);
        let mut changed = self.changed.lock().unwrap_or_else(PoisonError::into_inner);
        for Composed { home, view, unreadable, ended, retired } in composed {
            let key = (home, view.object.metadata.name.clone());
            match ended {
                Some(works) => {
                    shown.open.remove(&key);
                    self.ended_works.insert(key.clone(), works);
                }
                None => {
                    shown.open.insert(key.clone());
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
            let held = shown.views.get(&key);
            let same = held.is_some_and(|held| {
                held.view.object.metadata == view.object.metadata
                    && held.view.object.spec == view.object.spec
                    && held.view.object.status == view.object.status
            });
            let version = crate::held_listing::version(held.map(|held| held.version), same, position)?;
            if !same {
                changed.record([key.1.clone()]);
                shown.index.set(&key, crate::object_index_keys(&view.object).map_err(|error| error.to_string())?);
            }
            // A watcher reading through the view's last change has read how it ended: an unchanged
            // view was last changed no later than now.
            match (retired, same) {
                (true, false) => drop(shown.retiring.insert(key.clone(), changed.count())),
                (true, true) => drop(shown.retiring.entry(key.clone()).or_insert(changed.count())),
                (false, _) => drop(shown.retiring.remove(&key)),
            }
            shown.views.insert(key, Arc::new(HeldView { view, version }));
        }
        Ok(())
    }

    fn remove(&mut self, key: &(String, String)) {
        self.unreadable.remove(key);
        self.ended_works.remove(key);
        let held = &self.shown;
        if !held.views.contains_key(key) && !held.open.contains(key) && !held.retiring.contains_key(key) {
            return;
        }
        let shown = Arc::make_mut(&mut self.shown);
        if shown.views.remove(key).is_some() {
            self.changed.lock().unwrap_or_else(PoisonError::into_inner).record([key.1.clone()]);
            shown.index.remove(key);
        }
        shown.open.remove(key);
        shown.retiring.remove(key);
    }

    /// A watcher has read the views through `position`, of this run.
    fn delivered_to(&mut self, position: &LogPosition) {
        let run = self.journal().read();
        if let Some(read) = position.unlogged.as_ref().filter(|read| read.run == run.run && read.read <= run.read) {
            self.delivered.fetch_max(read.read, Ordering::SeqCst);
        }
    }

    /// Lets go of each retired launch its watchers have read retired, or of every one when
    /// nothing watches.
    fn evict(&mut self, watched: bool) {
        let delivered = self.delivered.load(Ordering::SeqCst);
        let gone: Vec<_> =
            self.shown.retiring.iter().filter(|(_, told)| !watched || **told <= delivered).map(|(key, _)| key.clone()).collect();
        for key in &gone {
            self.remove(key);
        }
    }

    /// How many entries the views hold, over every launch they keep anything of.
    fn entries(&self) -> usize {
        let shown = &self.shown;
        shown.views.len() + shown.open.len() + self.ended_works.len() + shown.retiring.len() + self.unreadable.len() + shown.index.len()
    }

    /// Keeps only the views `keep` names.
    fn retain(&mut self, keep: &BTreeSet<(String, String)>) {
        let gone: Vec<_> = self.shown.views.keys().filter(|key| !keep.contains(*key)).cloned().collect();
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
