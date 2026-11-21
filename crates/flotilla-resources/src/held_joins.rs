//! The Vessel and terminal session launches the views join, held once per read port (#3108).
//!
//! The TerminalSession views compose from Vessel launches and session launches (each with its
//! launch facts). [`HeldJoins`] holds them once, for every view of one read port:
//! the first read fills it with one read of each kind, and each pass after decodes each launch
//! that changed once, by name and home. A view reads which launches changed from the journal,
//! from where it last read, and composes from what is held: no view reads these kinds itself.
//!
//! It holds live work, not history: an ended launch is evicted once no view needs it. Each view
//! reading the journal registers a [`JoinsReader`] and tells it the sequence it has composed
//! through; a launch is kept until every reader has composed its last change. Beyond
//! that, an ended launch is kept while a view could still compose from it:
//! - a session launch while its Vessel launch is live: the TerminalSession views list an ended
//!   session until it is evicted, which the journal tells them;
//! - a Vessel launch while any session launch held names it, whose holder it is;
//! - the newest launch of a session at a home until no older one is held, so an older launch
//!   never stands in for it.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, PoisonError,
    },
};

use crate::{
    lifecycle_reads::{ChangeCursor, LifecycleReads, Told},
    terminal_records::{launched, TerminalChange, TerminalRecord},
    vessel_records::{VesselChange, VesselRecord},
};

/// How many changes the journal keeps: a view further behind composes everything again from
/// what is held.
const JOURNAL: usize = 4096;

/// A launch, by name and home.
type Key = (String, String);

/// The launches the views join, held once for a read port.
#[derive(Default)]
pub struct HeldJoins {
    state: Mutex<State>,
    /// The commits its cursors were told of, kept apart from the state so a view can tell it is
    /// current without waiting on a pass; none until the cursors start, or where they cannot tell.
    told: Mutex<Option<Vec<Told>>>,
    /// The journal sequence the next change takes, stored as each read ends: the journal grows
    /// with no commit counted when a read evicts or reads everything again.
    journaled: AtomicU64,
}

#[derive(Default)]
struct State {
    held: Option<Held>,
    /// Each registered view's sequence: it has composed every change before it. A view's
    /// [`JoinsReader`] holds the other reference; once it is dropped the next read forgets the view.
    readers: BTreeMap<u64, Arc<AtomicU64>>,
    next_reader: u64,
}

/// How many launches are held.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeldCount {
    pub vessels: usize,
    pub terminals: usize,
}

/// A launch that changed, with what it is joined to: a view composes again what it touches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinChange {
    /// Vessel launch `name` at `home` changed; it serves convoy `convoy` homed at `convoy_home`.
    Vessel { name: String, home: String, convoy: String, convoy_home: String },
    /// Session launch `name` at `home` changed; it is held for `vessel`, if any.
    Terminal { name: String, home: String, vessel: Option<String> },
    /// Ended session launch `name` at `home` is held no more: no view lists it.
    TerminalEvicted { name: String, home: String },
}

/// What a view reads of the journal since its position.
pub enum JoinChanges<'a> {
    /// The launches that changed, oldest first.
    Changed(Vec<&'a JoinChange>),
    /// The journal no longer holds them, or everything was read again: compose everything.
    Everything,
}

/// A held launch and the journal sequence of its last change: none where a full read held it.
struct Launch<R> {
    record: R,
    changed: Option<u64>,
}

struct Held {
    /// Where the next pass reads the changes from; none where the port holds no log, and each
    /// pass then reads every launch afresh.
    cursors: Option<Cursors>,
    /// Every Vessel launch held, by name and home.
    vessels: BTreeMap<Key, Launch<VesselRecord>>,
    /// Every session launch held, by name and home.
    terminals: BTreeMap<Key, Launch<TerminalRecord>>,
    /// The sessions held for each Vessel launch name, by name and home.
    by_vessel: BTreeMap<String, Vec<Key>>,
    /// The ended launches held: those an eviction looks at.
    ended_vessels: BTreeSet<Key>,
    ended_terminals: BTreeSet<Key>,
    /// The commits the cursors were told of before the last pass or full read that completed:
    /// while the count has not moved, a pass would read nothing, and none is made.
    passed: Option<u64>,
    /// Something changed that may let an ended launch go.
    unchecked: bool,
    /// The sequence every view had composed before at the last eviction; none where no view was
    /// registered.
    through: Option<u64>,
    /// The changes since `first`, oldest first.
    journal: VecDeque<JoinChange>,
    /// The sequence of the journal's first change; everything before it was dropped or read again.
    first: u64,
    /// This home, as the vessels port names it.
    here: String,
}

struct Cursors {
    vessels: Option<Box<dyn ChangeCursor<VesselChange>>>,
    terminals: Option<Box<dyn ChangeCursor<TerminalChange>>>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl HeldJoins {
    /// Reads what is held, once the changes since the last pass are taken in: filled on first use.
    /// Runs on the caller's thread, and reads the store while it passes.
    /// A read made while no commit was counted since the last pass completed makes no pass: the
    /// count is read before the pass, which then reads every commit counted.
    pub fn read<T>(&self, reads: &dyn LifecycleReads, read: impl FnOnce(Joins<'_>) -> T) -> Result<T, String> {
        let told = self.cursors_told();
        let mut state = locked(&self.state);
        let State { held, readers, .. } = &mut *state;
        match held.as_mut() {
            Some(current) if told.is_some() && current.passed == told => {}
            Some(current) => {
                if let Err(error) = current.pass(reads) {
                    tracing::warn!(%error, "held joins: a change did not read; reading them all again");
                    current.refill(reads)?;
                }
                current.passed = told;
            }
            None => {
                // Follow before filling, so no change between the fill and the first pass is missed.
                let cursors = Cursors::start(reads);
                let told = cursors.as_ref().and_then(Cursors::told);
                let passed = told.as_ref().map(|told| told.iter().map(Told::get).fold(0, u64::saturating_add));
                *locked(&self.told) = told;
                let mut filled = Held {
                    cursors,
                    vessels: BTreeMap::new(),
                    terminals: BTreeMap::new(),
                    by_vessel: BTreeMap::new(),
                    ended_vessels: BTreeSet::new(),
                    ended_terminals: BTreeSet::new(),
                    passed,
                    unchecked: false,
                    through: None,
                    journal: VecDeque::new(),
                    first: 0,
                    here: reads.vessels().map(|vessels| vessels.home().to_owned()).unwrap_or_default(),
                };
                filled.refill(reads)?;
                *held = Some(filled);
            }
        }
        let held = held.as_mut().expect("held above");
        self.evict(held, readers);
        let read = read(Joins { held });
        self.journaled.store(held.next(), Ordering::Release);
        Ok(read)
    }

    /// Evicts what no view needs now, without a pass: a view that has composed further may let
    /// launches go while no read follows, its own reads served from what it last published.
    fn settle(&self) {
        let mut state = locked(&self.state);
        let State { held, readers, .. } = &mut *state;
        if let Some(held) = held.as_mut() {
            self.evict(held, readers);
            self.journaled.store(held.next(), Ordering::Release);
        }
    }

    fn evict(&self, held: &mut Held, readers: &mut BTreeMap<u64, Arc<AtomicU64>>) {
        // A view whose reader was dropped has left: it no longer holds back an eviction.
        let registered = readers.len();
        readers.retain(|_, composed| Arc::strong_count(composed) > 1);
        if readers.len() < registered {
            held.unchecked = true;
        }
        held.evict(readers.values().map(|composed| composed.load(Ordering::Acquire)).min());
    }

    /// How many commits that may change the launches held its cursors have been told of, counted
    /// before each writer's call returns, plus the journal's sequence as the last read left it: a
    /// view whose read began at this count reads every such commit counted and every change
    /// journaled before it, evictions among them. None before the first read, or where the
    /// cursors cannot tell. Takes no lock a pass holds.
    pub fn told(&self) -> Option<u64> {
        let journaled = self.journaled.load(Ordering::Acquire);
        self.cursors_told().map(|told| told.saturating_add(journaled))
    }

    /// The commits its cursors were told of, without the journal: a launch held changes only with
    /// one of these, and the journal grows without one only to evict an ended launch no view needs
    /// or to read every launch again as it was. None where [`Self::told`] is none.
    pub fn commits(&self) -> Option<u64> {
        self.cursors_told()
    }

    /// The commits the cursors were told of, summed.
    fn cursors_told(&self) -> Option<u64> {
        locked(&self.told).as_ref().map(|told| told.iter().map(Told::get).fold(0, u64::saturating_add))
    }

    /// How many launches are held.
    pub fn held(&self) -> HeldCount {
        let state = locked(&self.state);
        state
            .held
            .as_ref()
            .map_or_else(HeldCount::default, |held| HeldCount { vessels: held.vessels.len(), terminals: held.terminals.len() })
    }

    fn register(&self) -> Arc<AtomicU64> {
        let mut state = locked(&self.state);
        let reader = state.next_reader;
        state.next_reader = state.next_reader.saturating_add(1);
        // It composes everything held at its first read.
        let composed = Arc::new(AtomicU64::new(state.held.as_ref().map_or(0, Held::next)));
        state.readers.insert(reader, Arc::clone(&composed));
        composed
    }
}

/// A view's place in the held joins' journal: while it is registered, no launch is evicted whose
/// last change it has not composed. Leaves when dropped. Dropping takes no lock, so a view may be
/// dropped while its store is locked: the next read forgets it.
pub struct JoinsReader {
    reads: Arc<dyn LifecycleReads>,
    composed: Arc<AtomicU64>,
}

impl JoinsReader {
    /// Registers a view of `reads`' held joins.
    pub fn register(reads: Arc<dyn LifecycleReads>) -> Result<Self, String> {
        let composed = reads.joins()?.register();
        Ok(Self { reads, composed })
    }

    /// [`HeldJoins::read`] of this view's port.
    pub fn read<T>(&self, read: impl FnOnce(Joins<'_>) -> T) -> Result<T, String> {
        self.reads.joins()?.read(self.reads.as_ref(), read)
    }

    /// The view has composed every change before sequence `sequence`: what it no longer needs is
    /// evicted now, as the views' reads need not read the joins again. Takes the joins' lock where
    /// the sequence moves, so is not called inside a read of them.
    pub fn composed(&self, sequence: u64) {
        if self.composed.fetch_max(sequence, Ordering::Release) < sequence {
            if let Ok(joins) = self.reads.joins() {
                joins.settle();
            }
        }
    }
}

impl Cursors {
    fn start(reads: &dyn LifecycleReads) -> Option<Self> {
        let vessels = match reads.vessels() {
            Ok(vessels) => Some(vessels.cursor()?),
            Err(_) => None,
        };
        let terminals = match reads.terminals() {
            Ok(terminals) => Some(terminals.cursor()?),
            Err(_) => None,
        };
        Some(Self { vessels, terminals })
    }

    /// Each cursor's count of the commits it was told of; none where any cannot tell.
    fn told(&self) -> Option<Vec<Told>> {
        let vessels = self.vessels.as_ref().map(|cursor| cursor.told());
        let terminals = self.terminals.as_ref().map(|cursor| cursor.told());
        [vessels, terminals].into_iter().flatten().collect()
    }

    fn position(&self) -> u64 {
        let vessels = self.vessels.as_ref().map_or(0, |cursor| cursor.position());
        let terminals = self.terminals.as_ref().map_or(0, |cursor| cursor.position());
        vessels.saturating_add(terminals)
    }
}

impl Held {
    /// The sequence the next change takes.
    fn next(&self) -> u64 {
        self.first.saturating_add(self.journal.len() as u64)
    }

    /// Takes in the changes since the last pass, each changed launch read once.
    fn pass(&mut self, reads: &dyn LifecycleReads) -> Result<(), String> {
        let Some(cursors) = self.cursors.as_mut() else { return self.refill(reads) };
        let mut vessels = Vec::new();
        let mut terminals = Vec::new();
        let mut rescanned = false;
        if let Some(cursor) = cursors.vessels.as_mut() {
            for change in cursor.pass()? {
                match change {
                    VesselChange::Changed { name, home } => vessels.push((name, home)),
                    VesselChange::Rescanned => rescanned = true,
                }
            }
        }
        if let Some(cursor) = cursors.terminals.as_mut() {
            for change in cursor.pass()? {
                match change {
                    TerminalChange::Changed { name, home } => terminals.push((name, home)),
                    TerminalChange::Rescanned => rescanned = true,
                }
            }
        }
        if rescanned {
            return self.refill(reads);
        }
        if !vessels.is_empty() {
            let port = reads.vessels()?;
            for (name, home) in vessels {
                let record = port.get(&name, Some(&home))?;
                self.set_vessel((name, home), record);
            }
        }
        if !terminals.is_empty() {
            let port = reads.terminals()?;
            for (name, home) in terminals {
                let record = port.get(&name, Some(&home))?;
                self.set_terminal((name, home), record);
            }
        }
        Ok(())
    }

    /// Reads every launch again: each kind once. Every view composes everything again.
    fn refill(&mut self, reads: &dyn LifecycleReads) -> Result<(), String> {
        let vessels = match reads.vessels() {
            Ok(port) => port.every()?,
            Err(_) => Vec::new(),
        };
        let terminals = match reads.terminals() {
            Ok(port) => port.every()?,
            Err(_) => Vec::new(),
        };
        self.vessels.clear();
        self.terminals.clear();
        self.by_vessel.clear();
        self.ended_vessels.clear();
        self.ended_terminals.clear();
        self.unchecked = true;
        for record in vessels {
            self.hold_vessel(record, None);
        }
        for record in terminals {
            self.hold_terminal(record, None);
        }
        // Past every sequence handed out: each view reads that everything changed.
        self.first = self.next().saturating_add(1);
        self.journal.clear();
        Ok(())
    }

    fn set_vessel(&mut self, key: Key, record: Option<VesselRecord>) {
        let held = self.drop_vessel(&key);
        let (convoy, convoy_home) = match record.as_ref().or(held.as_ref()) {
            Some(record) => (record.convoy.clone(), record.convoy_home.clone()),
            // Neither held nor read: nothing composed from it.
            None => return,
        };
        let changed = self.next();
        if let Some(record) = record {
            self.hold_vessel(record, Some(changed));
        }
        self.journal(JoinChange::Vessel { name: key.0, home: key.1, convoy, convoy_home });
    }

    fn set_terminal(&mut self, key: Key, record: Option<TerminalRecord>) {
        let held = self.drop_terminal(&key);
        let vessel = match record.as_ref().or(held.as_ref()) {
            Some(record) => record.vessel.clone(),
            None => return,
        };
        let changed = self.next();
        if let Some(record) = record {
            self.hold_terminal(record, Some(changed));
        }
        self.journal(JoinChange::Terminal { name: key.0, home: key.1, vessel });
    }

    fn hold_vessel(&mut self, record: VesselRecord, changed: Option<u64>) {
        let key = (record.name.clone(), record.home.clone());
        if record.ended() {
            self.ended_vessels.insert(key.clone());
        }
        self.vessels.insert(key, Launch { record, changed });
    }

    fn hold_terminal(&mut self, record: TerminalRecord, changed: Option<u64>) {
        let key = (record.name.clone(), record.home.clone());
        if let Some(vessel) = record.vessel.clone() {
            self.by_vessel.entry(vessel).or_default().push(key.clone());
        }
        if record.ended() {
            self.ended_terminals.insert(key.clone());
        }
        self.terminals.insert(key, Launch { record, changed });
    }

    /// Stops holding Vessel launch `key`.
    fn drop_vessel(&mut self, key: &Key) -> Option<VesselRecord> {
        let held = self.vessels.remove(key)?.record;
        self.ended_vessels.remove(key);
        Some(held)
    }

    fn drop_terminal(&mut self, key: &Key) -> Option<TerminalRecord> {
        let held = self.terminals.remove(key)?.record;
        if let Some(vessel) = held.vessel.as_ref() {
            remove_key(&mut self.by_vessel, vessel, key);
        }
        self.ended_terminals.remove(key);
        Some(held)
    }

    fn journal(&mut self, change: JoinChange) {
        self.journal.push_back(change);
        if self.journal.len() > JOURNAL {
            self.journal.pop_front();
            self.first = self.first.saturating_add(1);
        }
        self.unchecked = true;
    }

    /// Evicts each ended launch no view needs, every view having composed before sequence
    /// `through`: none where no view is registered.
    fn evict(&mut self, through: Option<u64>) {
        if !self.unchecked && through == self.through {
            return;
        }
        self.unchecked = false;
        self.through = through;
        let composed = |changed: Option<u64>| match (changed, through) {
            (Some(changed), Some(through)) => changed < through,
            _ => true,
        };
        // A launch evicted can let another go: a newest launch once no older one is held, a
        // Vessel launch once no session names it.
        loop {
            let terminals: Vec<Key> = self
                .ended_terminals
                .iter()
                .filter(|key| self.terminals.get(*key).is_some_and(|launch| composed(launch.changed) && !self.terminal_needed(launch)))
                .cloned()
                .collect();
            for key in terminals.iter().cloned() {
                self.drop_terminal(&key);
                let (name, home) = key;
                self.journal(JoinChange::TerminalEvicted { name, home });
            }
            let vessels: Vec<Key> = self
                .ended_vessels
                .iter()
                .filter(|key| self.vessels.get(*key).is_some_and(|launch| composed(launch.changed) && !self.vessel_needed(launch)))
                .cloned()
                .collect();
            for key in &vessels {
                self.drop_vessel(key);
            }
            if terminals.is_empty() && vessels.is_empty() {
                break;
            }
        }
    }

    /// Whether a view could still compose from ended session launch `launch`.
    fn terminal_needed(&self, launch: &Launch<TerminalRecord>) -> bool {
        let record = &launch.record;
        // A live Vessel launch's sessions are all listed.
        if record.vessel.as_deref().is_some_and(|vessel| self.named_vessels(vessel).any(|(_, held)| !held.record.ended())) {
            return true;
        }
        let session = launched(&record.name);
        let mut others = self.session_launches(session, &record.home).filter(|other| other.name != record.name);
        // An older launch: nothing composes it in place of the newest. The newest, ended: kept
        // until no older one is held.
        !others.clone().any(|other| other.launch_number() > record.launch_number()) && others.next().is_some()
    }

    /// Whether a view could still compose from ended Vessel launch `launch`: a session held
    /// names it as its holder.
    fn vessel_needed(&self, launch: &Launch<VesselRecord>) -> bool {
        self.by_vessel.get(&launch.record.name).is_some_and(|sessions| !sessions.is_empty())
    }

    /// The Vessel launches held named `name`, at every home.
    fn named_vessels(&self, name: &str) -> impl Iterator<Item = (&Key, &Launch<VesselRecord>)> + '_ {
        let name = name.to_owned();
        self.vessels.range((name.clone(), String::new())..).take_while(move |((held, _), _)| *held == name)
    }

    /// The launches held of session `session` at `home`.
    fn session_launches<'a>(&'a self, session: &'a str, home: &'a str) -> impl Iterator<Item = &'a TerminalRecord> + Clone + 'a {
        let prefix = format!("{session}#");
        self.terminals
            .range((prefix.clone(), String::new())..)
            .take_while(move |((name, _), _)| name.starts_with(&prefix))
            .map(|(_, launch)| &launch.record)
            .filter(move |record| record.home == home && launched(&record.name) == session)
    }
}

fn remove_key(map: &mut BTreeMap<String, Vec<Key>>, at: &str, key: &Key) {
    if let Some(keys) = map.get_mut(at) {
        keys.retain(|held| held != key);
        if keys.is_empty() {
            map.remove(at);
        }
    }
}

/// What [`HeldJoins`] holds, as one read sees it.
pub struct Joins<'a> {
    held: &'a mut Held,
}

impl Joins<'_> {
    /// Where a view that has read everything held stands in the journal.
    pub fn sequence(&self) -> u64 {
        self.held.next()
    }

    /// How far the passes have read the log; none is read where no log is held.
    pub fn position(&self) -> u64 {
        self.held.cursors.as_ref().map_or(0, Cursors::position)
    }

    /// Whether the passes follow a log; where none is held, each pass reads everything again.
    pub fn follows(&self) -> bool {
        self.held.cursors.is_some()
    }

    /// The changes since sequence `after`.
    pub fn since(&self, after: u64) -> JoinChanges<'_> {
        match after.checked_sub(self.held.first) {
            Some(skip) => JoinChanges::Changed(self.held.journal.iter().skip(skip as usize).collect()),
            None => JoinChanges::Everything,
        }
    }

    /// Every session launch held for Vessel launch `vessel`, at every home.
    pub fn sessions_of(&self, vessel: &str) -> Vec<TerminalRecord> {
        let keys = self.held.by_vessel.get(vessel).map(Vec::as_slice).unwrap_or_default();
        keys.iter().filter_map(|key| self.held.terminals.get(key).map(|launch| launch.record.clone())).collect()
    }

    /// Vessel launch `name` held at `home`.
    pub fn vessel(&self, name: &str, home: &str) -> Option<&VesselRecord> {
        self.held.vessels.get(&(name.to_owned(), home.to_owned())).map(|launch| &launch.record)
    }

    /// Vessel launch `name` as a session held for it at `home` reads it: at that home, where a
    /// vessel usually runs, then at this one, then wherever it is held.
    pub fn holder(&self, name: &str, home: &str) -> Option<&VesselRecord> {
        self.vessel(name, home)
            .or_else(|| self.vessel(name, &self.held.here))
            .or_else(|| self.held.named_vessels(name).map(|(_, launch)| &launch.record).next())
    }

    /// Session launch `name` held at `home`.
    pub fn terminal(&self, name: &str, home: &str) -> Option<&TerminalRecord> {
        self.held.terminals.get(&(name.to_owned(), home.to_owned())).map(|launch| &launch.record)
    }

    /// Every session launch held, at every home.
    pub fn terminals(&self) -> impl Iterator<Item = &TerminalRecord> {
        self.held.terminals.values().map(|launch| &launch.record)
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, thread, time::Duration};

    use super::*;

    struct Reads {
        joins: HeldJoins,
    }

    impl LifecycleReads for Reads {
        fn home(&self) -> Result<&str, String> {
            Ok("here")
        }

        fn try_reading(
            &self,
            _: crate::lifecycle_reads::ProvisioningKind,
            _: &str,
            _: Option<&str>,
        ) -> Result<Option<flotilla_protocol::LifecycleReading>, String> {
            Ok(None)
        }

        fn joins(&self) -> Result<&HeldJoins, String> {
            Ok(&self.joins)
        }
    }

    // A read holds the joins' lock while it reads the store, and a view's last owner may drop it
    // while holding the store: dropping a reader must not wait on that read, or the two deadlock.
    #[test]
    fn a_reader_drops_while_a_read_holds_the_joins() {
        let reads: Arc<dyn LifecycleReads> = Arc::new(Reads { joins: HeldJoins::default() });
        let leaving = JoinsReader::register(Arc::clone(&reads)).expect("register");
        let staying = JoinsReader::register(Arc::clone(&reads)).expect("register");
        staying
            .read(|_| {
                let (dropped, wait) = mpsc::channel();
                thread::spawn(move || {
                    drop(leaving);
                    let _ = dropped.send(());
                });
                wait.recv_timeout(Duration::from_secs(10)).expect("a reader drops while the joins are read");
            })
            .expect("read");
        // The next read forgets the dropped reader.
        staying.read(|_| ()).expect("read");
        assert_eq!(locked(&reads.joins().expect("joins").state).readers.len(), 1);
    }
}
