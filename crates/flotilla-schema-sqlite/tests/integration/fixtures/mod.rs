//! Fixture kinds, transcribed from the real resource types rather than imported, and helpers.
//!
//! Each fixture generates values in two modes ([`Pick`]): drawn by Hegel, or fixed by a covering
//! index, so that a fixture's covering indexes `0..COVERING` reach every enum variant, nested ones
//! included, and both empty and non-empty collections. [`Fixture::evolve`] generates an update the
//! storage rules allow: spec fields and child tables change freely, `records` rows are replaced in
//! place or appended, `history` rows are only appended.

pub mod attempt;
pub mod convoy;
pub mod message;
pub mod terminal;

use std::fmt::Debug;

use chrono::{DateTime, Utc};
use flotilla_schema::{Identity, Kind, KindCode, KindRegistry, Records, Ref, Timestamp};
use flotilla_schema_sqlite::Store;
use hegel::{generators as gs, TestCase};
use rusqlite::Connection;

pub use self::{
    convoy::{Convoy, Project},
    message::{AgentSession, Inbox, Message},
    terminal::{TerminalSession, Vessel},
};

pub const LOCAL: &str = "local";

/// A generated fixture kind.
pub trait Fixture: Kind + Clone + PartialEq + Debug {
    /// Covering indexes `0..COVERING` reach every variant and both empty and non-empty collections.
    const COVERING: usize;
    fn generate(tc: &TestCase, pick: Pick) -> Self;
    /// An update of `self` the storage rules allow, with spec fields drawn afresh by `pick`.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> Self;
    /// The value a load returns: times floored to microseconds.
    fn stored(&self) -> Self {
        self.clone()
    }
}

pub fn registry() -> KindRegistry {
    let mut registry = KindRegistry::new();
    registry.register::<Message>().expect("fixture kinds register");
    registry.register::<Inbox>().expect("fixture kinds register");
    registry.register::<AgentSession>().expect("fixture kinds register");
    registry.register::<Project>().expect("fixture kinds register");
    registry.register::<Convoy>().expect("fixture kinds register");
    registry.register::<TerminalSession>().expect("fixture kinds register");
    registry.register::<Vessel>().expect("fixture kinds register");
    registry
}

pub fn store() -> Store {
    Store::open(Connection::open_in_memory().expect("in-memory database"), LOCAL, registry()).expect("open store")
}

/// How a generator chooses each enum variant, option and collection size: drawn by Hegel, or
/// fixed by a covering index.
#[derive(Debug, Clone, Copy)]
pub enum Pick {
    Draw,
    Cover(usize),
}

/// Structural shapes per round of covering indexes; see each fixture's `COVERING`.
pub const SHAPES: usize = 16;

impl Pick {
    /// The variant index in `0..count`; `cover` maps a structural shape to it.
    pub fn variant(self, tc: &TestCase, count: usize, cover: impl FnOnce(usize) -> usize) -> usize {
        match self {
            Pick::Draw => tc.draw(gs::integers::<usize>().min_value(0).max_value(count - 1)),
            Pick::Cover(index) => cover(index % SHAPES) % count,
        }
    }

    /// Whether an option is present; shapes below `until` are present.
    pub fn some(self, tc: &TestCase, until: usize) -> bool {
        match self {
            Pick::Draw => tc.draw(gs::booleans()),
            Pick::Cover(index) => index % SHAPES < until,
        }
    }

    /// A collection size in `0..=max`: every size in turn when covering.
    pub fn count(self, tc: &TestCase, max: usize) -> usize {
        match self {
            Pick::Draw => tc.draw(gs::integers::<usize>().min_value(0).max_value(max)),
            Pick::Cover(index) => index % (max + 1),
        }
    }

    /// The pick for the `n`th element of a collection: covering shifts the index, so the elements
    /// of one value take different shapes.
    pub fn nth(self, n: usize) -> Pick {
        match self {
            Pick::Draw => Pick::Draw,
            Pick::Cover(index) => Pick::Cover(index + n),
        }
    }

    /// Whether to replace the `n`th existing record in place.
    pub fn replace(self, tc: &TestCase, n: usize) -> bool {
        match self {
            Pick::Draw => tc.draw(gs::booleans()),
            Pick::Cover(index) => (index + n).is_multiple_of(2),
        }
    }
}

/// A list of `pick.count(max)` elements, each with its own shifted pick.
pub fn list<T>(tc: &TestCase, pick: Pick, max: usize, mut element: impl FnMut(Pick) -> T) -> Vec<T> {
    (0..pick.count(tc, max)).map(|n| element(pick.nth(n))).collect()
}

/// Replaces one record in place, given the pick for its position.
pub type Replace<'a, T> = &'a dyn Fn(&T, Pick) -> T;

/// Records after an allowed update: each existing row kept or replaced in place by `replace`,
/// then up to two rows appended. `history` passes `None`: its rows never change.
pub fn evolve_records<T: Clone>(
    tc: &TestCase,
    pick: Pick,
    old: &Records<T>,
    replace: Option<Replace<'_, T>>,
    mut fresh: impl FnMut(Pick) -> T,
) -> Records<T> {
    let mut next = old.clone();
    if let Some(replace) = replace {
        for pos in 0..old.len() {
            if pick.replace(tc, pos) {
                next.replace(pos, replace(&old[pos], pick.nth(pos)));
            }
        }
    }
    for n in 0..pick.count(tc, 2) {
        next.push(fresh(pick.nth(old.len() + n)));
    }
    next
}

pub fn code_enum<E: Copy>(tc: &TestCase, pick: Pick, all: &[E]) -> E {
    match pick {
        Pick::Draw => all[tc.draw(gs::integers::<usize>().min_value(0).max_value(all.len() - 1))],
        Pick::Cover(index) => all[index % all.len()],
    }
}

pub fn seq(tc: &TestCase) -> u64 {
    tc.draw(gs::integers::<u64>().min_value(0).max_value(i64::MAX as u64))
}

/// Interned values collide often, so equal strings share `strings` rows.
pub fn name(tc: &TestCase) -> String {
    tc.draw(gs::sampled_from(vec!["alice".to_owned(), "bob".to_owned(), String::new(), "ünïcødé".to_owned()]))
}

/// Map keys and set values from a small alphabet, so they collide and dedupe.
pub fn key(tc: &TestCase) -> String {
    tc.draw(gs::text().alphabet("ab").max_size(2))
}

pub fn home(tc: &TestCase) -> String {
    tc.draw(gs::sampled_from(vec![LOCAL.to_owned(), "elsewhere".to_owned()]))
}

/// References to targets that mostly do not exist: writing one creates its identity row.
pub fn reference<K>(tc: &TestCase) -> Ref<K> {
    Ref::new(name(tc), home(tc))
}

pub fn identity(tc: &TestCase) -> Identity {
    Identity { kind: KindCode(tc.draw(gs::integers::<u32>())), name: name(tc), home: home(tc) }
}

pub fn timestamp(tc: &TestCase) -> Timestamp {
    Timestamp(tc.draw(gs::integers::<i64>()))
}

/// Text that spans the empty string, ASCII, multi-byte characters and SQL quoting characters.
pub fn text(tc: &TestCase) -> String {
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
        0 => tc.draw(gs::text().max_size(12)),
        1 => {
            tc.draw(gs::sampled_from(vec![String::new(), "'".to_owned(), "\"; DROP TABLE message; --".to_owned(), "ünïcødé ✓".to_owned()]))
        }
        _ => tc.draw(gs::text().alphabet("ab").max_size(3)),
    }
}

/// A time with sub-microsecond digits, before or after the epoch.
pub fn time(tc: &TestCase) -> DateTime<Utc> {
    let seconds = tc.draw(gs::integers::<i64>().min_value(-2_000_000_000).max_value(4_000_000_000));
    let nanos = tc.draw(gs::integers::<u32>().min_value(0).max_value(999_999_999));
    DateTime::from_timestamp(seconds, nanos).expect("in range")
}

/// What a stored time loads back as: floored to whole microseconds.
pub fn floor(time: DateTime<Utc>) -> DateTime<Utc> {
    Timestamp::from_datetime(&time).to_datetime().expect("in range")
}
