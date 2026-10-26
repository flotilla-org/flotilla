//! Following the decided store's log for chosen kinds (#3824 layer 1).
//!
//! A reader that keeps resident state of some kinds, or wakes controllers when they change,
//! follows every home's log from a cursor: each pass reads only the entries written since the
//! last, and names the resources of its kinds they changed. A home whose sequence moved with
//! no entries left to read (a replica caught up from a snapshot, which logs nothing) is named
//! for a rescan of that home.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Identity, KindCode};
use flotilla_schema_sqlite::{Read, StoreError};

/// What one pass found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Followed {
    /// A resource of a followed kind was written (created, edited, moved, deleted or expunged).
    Changed(Identity),
    /// This home's state moved without entries to read: reread its resources of the kinds.
    Rescan(String),
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

    /// Starts every one of `homes` at its current sequence: what the caller read before this is
    /// the state it follows from.
    pub fn start(&mut self, read: &mut Read<'_>, homes: &[String]) -> Result<(), StoreError> {
        for home in homes {
            self.cursors.insert(home.clone(), read.last_sequence(home)?);
        }
        Ok(())
    }

    /// The resources of the followed kinds written on any of `homes` since the last pass, in
    /// each home's order. A home not seen before is named for a rescan and followed from now.
    pub fn pass(&mut self, read: &mut Read<'_>, homes: &[String]) -> Result<Vec<Followed>, StoreError> {
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
                    // The sequence moved past entries this store does not hold: a snapshot.
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
