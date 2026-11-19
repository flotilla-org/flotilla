//! A resource table's position in the decided store's log, which a cursor resumes from (#3108).
//!
//! One follower per table kind, shared by every subscription to it, keeps for each home the
//! home's last sequence at the pass that found an entry touching the kind's stored kinds. The
//! position is their sum, so it moves exactly when an entry that can change the table's rows is
//! read, and a write to any other kind leaves it where it was. A follower starts at each home's
//! current sequence, so after a daemon restart a client's position is not found again: one
//! resync per client per restart.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, PoisonError},
};

use flotilla_engine_store::follow::{Followed, Follower};
use flotilla_schema::KindCode;
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};

/// A table kind's position: `sequence`, within the history counted by `resets`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Position {
    pub sequence: u64,
    /// How often a home's sequence fell (a replica restored afresh) since this daemon started:
    /// positions from before a fall count another history.
    pub resets: u64,
}

/// Every resource table kind's follower, for this daemon's life.
#[derive(Default)]
pub(crate) struct Positions(Mutex<BTreeMap<String, KindFollower>>);

/// One kind's follower and what it has found.
struct KindFollower {
    follower: Follower,
    /// Per home, its last sequence at the pass that last found an entry touching the kind.
    touched: BTreeMap<String, u64>,
    /// Per home, its last sequence at the last pass.
    seen: BTreeMap<String, u64>,
    resets: u64,
}

impl Positions {
    /// `kind`'s position now, its stored kinds `codes`. Read before the rows: every read of the
    /// rows after it applies at least the entries it counts.
    pub(crate) async fn read(self: &Arc<Self>, service: &Arc<Service>, kind: &str, codes: Vec<KindCode>) -> Result<Position, String> {
        let positions = Arc::clone(self);
        let kind = kind.to_owned();
        let homes = service.homes();
        super::decided::blocking(service, move |store| {
            let mut read = store.read()?;
            let mut kinds = positions.0.lock().unwrap_or_else(PoisonError::into_inner);
            match kinds.get_mut(&kind) {
                Some(followed) => followed.pass(&mut read, &homes),
                None => {
                    let followed = KindFollower::start(&mut read, &homes, codes)?;
                    let position = followed.position();
                    kinds.insert(kind, followed);
                    Ok(position)
                }
            }
        })
        .await
    }
}

impl KindFollower {
    fn start(read: &mut Read<'_>, homes: &[String], codes: Vec<KindCode>) -> Result<Self, StoreError> {
        let mut follower = Follower::new(codes);
        follower.start(read, homes)?;
        let seen = lasts(read, homes)?;
        Ok(Self { follower, touched: seen.clone(), seen, resets: 0 })
    }

    fn pass(&mut self, read: &mut Read<'_>, homes: &[String]) -> Result<Position, StoreError> {
        let lasts = lasts(read, homes)?;
        let fallen: Vec<String> = lasts
            .iter()
            .filter(|(home, last)| self.seen.get(*home).is_some_and(|seen| seen > *last))
            .map(|(home, _)| home.clone())
            .collect();
        if !fallen.is_empty() {
            self.resets += 1;
            self.follower.start(read, &fallen)?;
            for home in fallen {
                self.touched.insert(home.clone(), lasts[&home]);
            }
        }
        for followed in self.follower.pass(read, homes)? {
            // A home's log holds only its own records, so a changed record names the home read.
            let home = match followed {
                Followed::Changed(identity) => identity.home,
                Followed::Rescan(home) => home,
            };
            if let Some(last) = lasts.get(&home) {
                self.touched.insert(home, *last);
            }
        }
        self.seen = lasts;
        Ok(self.position())
    }

    fn position(&self) -> Position {
        Position { sequence: self.touched.values().sum(), resets: self.resets }
    }
}

fn lasts(read: &mut Read<'_>, homes: &[String]) -> Result<BTreeMap<String, u64>, StoreError> {
    homes.iter().map(|home| Ok((home.clone(), read.last_sequence(home)?))).collect()
}
