//! The Vessel and terminal session launches the views join, held once per read port (#3108).
//!
//! The Convoy and TerminalSession views both compose from Vessel launches and session launches
//! (each with its launch facts). [`HeldJoins`] holds them once, for every view of one read port:
//! the first read fills it with one read of each kind, and each pass after decodes each launch
//! that changed once, by name and home. A view reads which launches changed from its journal,
//! from where it last read, and composes from what is held: no view reads these kinds itself.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Mutex, MutexGuard, PoisonError},
};

use crate::{
    lifecycle_reads::{ChangeCursor, LifecycleReads},
    terminal_records::{TerminalChange, TerminalRecord},
    vessel_records::{VesselChange, VesselRecord},
};

/// How many changes the journal keeps: a view further behind composes everything again from
/// what is held.
const JOURNAL: usize = 4096;

/// The launches the views join, held once for a read port.
#[derive(Default)]
pub struct HeldJoins {
    held: Mutex<Option<Held>>,
}

/// A launch that changed, with what it is joined to: a view composes again what it touches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinChange {
    /// Vessel launch `name` at `home` changed; it serves convoy `convoy` homed at `convoy_home`.
    Vessel { name: String, home: String, convoy: String, convoy_home: String },
    /// Session launch `name` at `home` changed; it is held for `vessel`, if any.
    Terminal { name: String, home: String, vessel: Option<String> },
}

/// What a view reads of the journal since its position.
pub enum JoinChanges<'a> {
    /// The launches that changed, oldest first.
    Changed(Vec<&'a JoinChange>),
    /// The journal no longer holds them, or everything was read again: compose everything.
    Everything,
}

struct Held {
    /// Where the next pass reads the changes from; none where the port holds no log, and each
    /// pass then reads every launch afresh.
    cursors: Option<Cursors>,
    /// Every Vessel launch, by name and home.
    vessels: BTreeMap<(String, String), VesselRecord>,
    /// Every session launch, by name and home.
    terminals: BTreeMap<(String, String), TerminalRecord>,
    /// The sessions held for each Vessel launch name, by name and home.
    by_vessel: BTreeMap<String, Vec<(String, String)>>,
    /// The Vessel launches serving each convoy name, by name and home.
    by_convoy: BTreeMap<String, Vec<(String, String)>>,
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
    pub fn read<T>(&self, reads: &dyn LifecycleReads, read: impl FnOnce(Joins<'_>) -> T) -> Result<T, String> {
        let mut held = locked(&self.held);
        match held.as_mut() {
            Some(current) => {
                if let Err(error) = current.pass(reads) {
                    tracing::warn!(%error, "held joins: a change did not read; reading them all again");
                    current.refill(reads)?;
                }
            }
            None => {
                // Follow before filling, so no change between the fill and the first pass is missed.
                let cursors = Cursors::start(reads);
                let mut filled = Held {
                    cursors,
                    vessels: BTreeMap::new(),
                    terminals: BTreeMap::new(),
                    by_vessel: BTreeMap::new(),
                    by_convoy: BTreeMap::new(),
                    journal: VecDeque::new(),
                    first: 0,
                    here: reads.vessels().map(|vessels| vessels.home().to_owned()).unwrap_or_default(),
                };
                filled.refill(reads)?;
                *held = Some(filled);
            }
        }
        Ok(read(Joins { held: held.as_ref().expect("held above") }))
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
        self.by_convoy.clear();
        for record in vessels {
            self.hold_vessel(record);
        }
        for record in terminals {
            self.hold_terminal(record);
        }
        // Past every sequence handed out: each view reads that everything changed.
        self.first = self.next().saturating_add(1);
        self.journal.clear();
        Ok(())
    }

    fn set_vessel(&mut self, key: (String, String), record: Option<VesselRecord>) {
        let held = self.vessels.remove(&key);
        if let Some(held) = held.as_ref() {
            remove_key(&mut self.by_convoy, &held.convoy, &key);
        }
        let (convoy, convoy_home) = match record.as_ref().or(held.as_ref()) {
            Some(record) => (record.convoy.clone(), record.convoy_home.clone()),
            // Neither held nor read: nothing composed from it.
            None => return,
        };
        if let Some(record) = record {
            self.hold_vessel(record);
        }
        self.journal(JoinChange::Vessel { name: key.0, home: key.1, convoy, convoy_home });
    }

    fn set_terminal(&mut self, key: (String, String), record: Option<TerminalRecord>) {
        let held = self.terminals.remove(&key);
        if let Some(vessel) = held.as_ref().and_then(|held| held.vessel.as_ref()) {
            remove_key(&mut self.by_vessel, vessel, &key);
        }
        let vessel = match record.as_ref().or(held.as_ref()) {
            Some(record) => record.vessel.clone(),
            None => return,
        };
        if let Some(record) = record {
            self.hold_terminal(record);
        }
        self.journal(JoinChange::Terminal { name: key.0, home: key.1, vessel });
    }

    fn hold_vessel(&mut self, record: VesselRecord) {
        let key = (record.name.clone(), record.home.clone());
        self.by_convoy.entry(record.convoy.clone()).or_default().push(key.clone());
        self.vessels.insert(key, record);
    }

    fn hold_terminal(&mut self, record: TerminalRecord) {
        let key = (record.name.clone(), record.home.clone());
        if let Some(vessel) = record.vessel.clone() {
            self.by_vessel.entry(vessel).or_default().push(key.clone());
        }
        self.terminals.insert(key, record);
    }

    fn journal(&mut self, change: JoinChange) {
        self.journal.push_back(change);
        if self.journal.len() > JOURNAL {
            self.journal.pop_front();
            self.first = self.first.saturating_add(1);
        }
    }
}

fn remove_key(map: &mut BTreeMap<String, Vec<(String, String)>>, at: &str, key: &(String, String)) {
    if let Some(keys) = map.get_mut(at) {
        keys.retain(|held| held != key);
        if keys.is_empty() {
            map.remove(at);
        }
    }
}

/// What [`HeldJoins`] holds, as one read sees it.
pub struct Joins<'a> {
    held: &'a Held,
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

    /// Every Vessel launch serving convoy `convoy`, at every home.
    pub fn vessels_of(&self, convoy: &str) -> Vec<VesselRecord> {
        let keys = self.held.by_convoy.get(convoy).map(Vec::as_slice).unwrap_or_default();
        keys.iter().filter_map(|key| self.held.vessels.get(key).cloned()).collect()
    }

    /// Every session launch held for Vessel launch `vessel`, at every home.
    pub fn sessions_of(&self, vessel: &str) -> Vec<TerminalRecord> {
        let keys = self.held.by_vessel.get(vessel).map(Vec::as_slice).unwrap_or_default();
        keys.iter().filter_map(|key| self.held.terminals.get(key).cloned()).collect()
    }

    /// Vessel launch `name` held at `home`.
    pub fn vessel(&self, name: &str, home: &str) -> Option<&VesselRecord> {
        self.held.vessels.get(&(name.to_owned(), home.to_owned()))
    }

    /// Vessel launch `name` as a session held for it at `home` reads it: at that home, where a
    /// vessel usually runs, then at this one, then wherever it is held.
    pub fn holder(&self, name: &str, home: &str) -> Option<&VesselRecord> {
        self.vessel(name, home).or_else(|| self.vessel(name, &self.held.here)).or_else(|| {
            let from = (name.to_owned(), String::new());
            self.held.vessels.range(from..).take_while(|((held, _), _)| held == name).map(|(_, record)| record).next()
        })
    }

    /// Session launch `name` held at `home`.
    pub fn terminal(&self, name: &str, home: &str) -> Option<&TerminalRecord> {
        self.held.terminals.get(&(name.to_owned(), home.to_owned()))
    }

    /// Every session launch at every home.
    pub fn terminals(&self) -> impl Iterator<Item = &TerminalRecord> {
        self.held.terminals.values()
    }
}
