//! Following the decided store's log for chosen kinds (#3824 layer 1).
//!
//! A reader that keeps resident state of some kinds, or wakes controllers when they change,
//! follows every home's log from a cursor: each pass reads only the entries written since the
//! last, and names the resources of its kinds they changed. A home whose sequence moved with
//! no entries left to read (a replica caught up from a snapshot, which logs nothing) is named
//! for a rescan of that home.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Identity, KindCode};
use flotilla_schema_sqlite::{stream::Service, LogReads, StoreError};

/// What one pass found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Followed {
    /// A resource of a followed kind was written (created, edited, moved, deleted or expunged).
    Changed(Identity),
    /// This home's state moved without entries to read: reread its resources of the kinds.
    Rescan(String),
}

/// What a reader that read through some sequences has missed since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resumed {
    /// The resources of the followed kinds written since, in each home's order, and the
    /// sequences they were read through.
    Followed { changed: Vec<Identity>, through: BTreeMap<String, u64> },
    /// The log no longer holds what was written since, or never held it here: why.
    Expired(String),
}

/// [`since`] over every home `service` holds.
pub fn since_on(service: &Service, kinds: impl IntoIterator<Item = KindCode>, at: &BTreeMap<String, u64>) -> Result<Resumed, StoreError> {
    let homes = service.homes();
    service.with_store(|store| since(&mut store.read()?, kinds, &homes, at))
}

/// [`position`] of every home `service` holds.
pub fn position_on(service: &Service) -> Result<BTreeMap<String, u64>, StoreError> {
    let homes = service.homes();
    service.with_store(|store| position(&mut store.read()?, &homes))
}

/// Each of `homes`' last sequence: where a reader reading now starts from.
pub fn position(read: &mut impl LogReads, homes: &[String]) -> Result<BTreeMap<String, u64>, StoreError> {
    homes.iter().map(|home| Ok((home.clone(), read.last_sequence(home)?))).collect()
}

/// The resources of `kinds` written on `homes` since `at`, each home's sequence a reader read
/// through, in the same read as the sequences they are read through. A home `at` does not name
/// is read from the start of its log. Expired when a home's log does not continue from where the
/// reader left it: caught up from a snapshot, expunged past it, or reset behind it.
pub fn since(
    read: &mut impl LogReads,
    kinds: impl IntoIterator<Item = KindCode>,
    homes: &[String],
    at: &BTreeMap<String, u64>,
) -> Result<Resumed, StoreError> {
    let mut cursors = BTreeMap::new();
    for home in homes {
        let last = read.last_sequence(home)?;
        let cursor = at.get(home).copied().unwrap_or(0);
        if cursor > last {
            return Ok(Resumed::Expired(format!("{home}'s log was reset behind sequence {cursor}")));
        }
        if cursor < read.resume_floor(home)? || !flotilla_schema_sqlite::stream::continues(read, home, cursor, last)? {
            return Ok(Resumed::Expired(format!(
                "{home}'s log no longer holds what followed sequence {cursor}: it was caught up from a snapshot or expunged"
            )));
        }
        cursors.insert(home.clone(), cursor);
    }
    let mut follower = Follower::at(kinds, cursors);
    let mut changed = Vec::new();
    for followed in follower.pass(read, homes)? {
        match followed {
            Followed::Changed(identity) => changed.push(identity),
            Followed::Rescan(home) => {
                return Ok(Resumed::Expired(format!("{home}'s log moved without entries to read: it was caught up from a snapshot")))
            }
        }
    }
    Ok(Resumed::Followed { changed, through: follower.cursors })
}

/// Cursors over every home's log, for a set of kinds.
#[derive(Clone, Debug)]
pub struct Follower {
    kinds: BTreeSet<KindCode>,
    cursors: BTreeMap<String, u64>,
}

/// Entries read per page.
const PAGE: u32 = 256;

impl Follower {
    pub fn new(kinds: impl IntoIterator<Item = KindCode>) -> Self {
        Self { kinds: kinds.into_iter().collect(), cursors: BTreeMap::new() }
    }

    /// Follows `kinds` from `cursors`, each home's sequence an earlier reader read through. A home
    /// it does not name is named for a rescan at its first pass, as for [`Follower::start`]; a
    /// home followed from the start of its log is named at sequence 0.
    pub fn at(kinds: impl IntoIterator<Item = KindCode>, cursors: BTreeMap<String, u64>) -> Self {
        Self { kinds: kinds.into_iter().collect(), cursors }
    }

    /// Each home's sequence it has read through.
    pub fn cursors(&self) -> &BTreeMap<String, u64> {
        &self.cursors
    }

    /// Starts every one of `homes` at its current sequence: what the caller read before this is
    /// the state it follows from.
    pub fn start(&mut self, read: &mut impl LogReads, homes: &[String]) -> Result<(), StoreError> {
        for home in homes {
            self.cursors.insert(home.clone(), read.last_sequence(home)?);
        }
        Ok(())
    }

    /// How far it has read: the sum over homes of the sequence each pass read through. Every
    /// entry it reads moves it, of whatever kind.
    pub fn position(&self) -> u64 {
        self.cursors.values().fold(0, |sum, read| sum.saturating_add(*read))
    }

    /// The resources of the followed kinds written on any of `homes` since the last pass, in
    /// each home's order. A home not seen before is named for a rescan and followed from now.
    pub fn pass(&mut self, read: &mut impl LogReads, homes: &[String]) -> Result<Vec<Followed>, StoreError> {
        let mut followed = Vec::new();
        for home in homes {
            let last = read.last_sequence(home)?;
            let Some(&cursor) = self.cursors.get(home) else {
                self.cursors.insert(home.clone(), last);
                followed.push(Followed::Rescan(home.clone()));
                continue;
            };
            let mut after = cursor;
            while after < last {
                let page = read.changes_since(home, after, PAGE)?;
                let Some(end) = page.last().map(|event| event.home_seq) else {
                    // This home's own sequence moves without entries only for an empty change (a
                    // launch number reserved), which wrote nothing, unless an expunge passed the
                    // cursor. Another home's moves so by a snapshot, which wrote what it holds.
                    if home == read.home() && read.resume_floor(home)? <= after {
                        after = last;
                        break;
                    }
                    followed.push(Followed::Rescan(home.clone()));
                    after = last;
                    break;
                };
                followed.extend(
                    page.into_iter()
                        .filter(|event| self.kinds.contains(&event.kind))
                        .map(|event| Followed::Changed(Identity { kind: event.kind, name: event.name, home: event.home })),
                );
                after = end;
            }
            self.cursors.insert(home.clone(), after.max(cursor));
        }
        Ok(followed)
    }
}
