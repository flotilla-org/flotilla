//! Fixture kinds and helpers.
//!
//! The stored kinds are the engine's machine declarations (#3319, #3373 ruling 4): Message,
//! AgentSession, Convoy and Ensure from `flotilla_state_machines::machines`, and `TerminalSession`,
//! a lifecycle kind whose request is declared here (`terminal`); each is stored as the engine's
//! `Resource`, declared by its spec (#3392 ruling D5). Their edges are real transitions: each
//! fixture's [`Fixture::edge`] builds a home holding the stored resource, a world that makes a
//! drawn edge pass its gate, applies the edge through the engine, and turns the result into the
//! stored edge ([`engine::drive`]). Every stored transition is one the engine's rules allow. The
//! resources' specs, states and history are generated freely: history is never read to decide,
//! and a stored resource need not be one the engine could have reached. A generated resource has
//! an empty name and version 0; a load gives it the store's (`Kind::with_identity`), so tests
//! compare loads with models identified the same way.
//!
//! The synthetic kinds cover the derive features no engine kind uses yet: the label-plus-data
//! `Ticket` (`ticket`), and `Referral` and `Dossier` (`synthetic`, whose module doc lists each
//! feature with its expected first real user).
//!
//! Each fixture generates values in two modes ([`Pick`]): drawn by Hegel, or fixed by a covering
//! index, so that a fixture's covering indexes `0..COVERING` reach every enum variant, nested ones
//! included, every label, and both empty and non-empty collections. [`Fixture::evolve`] generates
//! a spec edit: spec fields and child tables change freely, while states and `records` and
//! `history` rows stay as they are. [`Fixture::edge`] generates an edge, the only write that moves
//! those. Generated rows carry arbitrary version stamps; [`Fixture::stamped`] gives the stamps the
//! store writes instead.

pub mod agent_session;
pub mod convoy;
pub mod engine;
pub mod ensure;
pub mod message;
pub mod synthetic;
pub mod terminal;
pub mod ticket;

use std::fmt::Debug;

use chrono::{DateTime, Utc};
use flotilla_schema::{Edge, EdgeEvent, Identity, Kind, KindCode, KindRegistry, Position, Records, Ref, Timestamp};
use flotilla_schema_sqlite::Store;
pub use flotilla_state_machines::{
    machines::{agent_session::AgentSession, convoy::Convoy, ensure::Ensure, message::Message},
    Resource,
};
use hegel::{generators as gs, TestCase};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

pub use self::{
    synthetic::{Dossier, Referral},
    terminal::{TerminalSession, Vessel},
    ticket::Ticket,
};

pub const LOCAL: &str = "local";

/// A synthetic kind's edge document: the edge's arguments and the resources its gate read, never
/// expanded into columns or tables. (The engine kinds' documents are `EdgePayload`.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payload<A> {
    pub args: A,
    pub reads: Vec<GateRead>,
}

/// A resource a gate read, at the version it read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateRead {
    pub observed: Identity,
    pub version: u64,
}

/// `args` with up to two drawn gate reads.
pub fn payload<A>(tc: &TestCase, pick: Pick, args: A) -> Payload<A> {
    Payload { args, reads: list(tc, pick, 2, |_| GateRead { observed: identity(tc), version: seq(tc) }) }
}

/// An edge event from `from` to `to`, with a drawn writer and time.
pub fn event<L, E, A>(tc: &TestCase, edge: E, from: L, to: L, payload: Payload<A>) -> EdgeEvent<L, E, Payload<A>> {
    EdgeEvent { edge, from, to, writer: name(tc), at: timestamp(tc), payload }
}

/// A generated fixture kind.
pub trait Fixture: Kind + Clone + PartialEq + Debug {
    /// Covering indexes `0..COVERING` reach every variant, every label and both empty and
    /// non-empty collections.
    const COVERING: usize;
    fn generate(tc: &TestCase, pick: Pick) -> Self;
    /// A spec edit of `self`: spec fields drawn afresh by `pick`, states and records kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> Self;
    /// An edge on `self` drawn by `pick`, and the value it leads to (appended rows with arbitrary
    /// stamps; see [`Fixture::stamped`]). `None` when no edge applies: an engine kind in an end
    /// state, or one whose stored data no gate accepts.
    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<Self>, Self)>;
    /// The value a load returns: times floored to microseconds.
    fn stored(&self) -> Self {
        self.clone()
    }
    /// `self` as written at `version`: every records and history row that `before` (the value the
    /// write replaced; `None` for an insert) lacks is stamped with `version`.
    fn stamped(&self, before: Option<&Self>, version: u64) -> Self;
}

pub fn registry() -> KindRegistry {
    let mut registry = KindRegistry::new();
    registry.register::<Resource<Message>>().expect("fixture kinds register");
    registry.register::<Resource<AgentSession>>().expect("fixture kinds register");
    registry.register::<Resource<Convoy>>().expect("fixture kinds register");
    registry.register::<TerminalSession>().expect("fixture kinds register");
    registry.register::<Vessel>().expect("fixture kinds register");
    registry.register::<Ticket>().expect("fixture kinds register");
    registry.register::<Resource<Ensure>>().expect("fixture kinds register");
    registry.register::<Referral>().expect("fixture kinds register");
    registry.register::<Dossier>().expect("fixture kinds register");
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

    /// A choice in `0..radix`: the covering index's digit at `place` (the product of the radixes
    /// of the digits below it), so independent choices of one value take every combination.
    pub fn digit(self, tc: &TestCase, radix: usize, place: usize) -> usize {
        match self {
            Pick::Draw => tc.draw(gs::integers::<usize>().min_value(0).max_value(radix - 1)),
            Pick::Cover(index) => (index / place) % radix,
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
}

/// A list of `pick.count(max)` elements, each with its own shifted pick.
pub fn list<T>(tc: &TestCase, pick: Pick, max: usize, mut element: impl FnMut(Pick) -> T) -> Vec<T> {
    (0..pick.count(tc, max)).map(|n| element(pick.nth(n))).collect()
}

/// A version stamp as a generator leaves it: anywhere in the storable range, since the store
/// replaces it with the appending write's sequence.
pub fn version(tc: &TestCase) -> u64 {
    tc.draw(gs::integers::<u64>().min_value(0).max_value(i64::MAX as u64))
}

/// Records of `pick.count(max)` elements, each with its own shifted pick and a drawn stamp.
pub fn records<T>(tc: &TestCase, pick: Pick, max: usize, element: impl FnMut(Pick) -> T) -> Records<T> {
    let mut records = Records::new();
    for record in list(tc, pick, max, element) {
        records.append(version(tc), record);
    }
    records
}

/// `records` with `record` appended, stamped arbitrarily.
pub fn appended<T: Clone>(tc: &TestCase, records: &Records<T>, record: T) -> Records<T> {
    let mut next = records.clone();
    next.append(version(tc), record);
    next
}

/// `records` with the row at `position` replaced through `f`, keeping its stamp: how an edge
/// changes a records row.
pub fn replaced<T: Clone>(records: &Records<T>, position: Position, mut f: impl FnMut(&T) -> T) -> Records<T> {
    let mut next = Records::new();
    for row in records {
        next.append(row.version, if row.position == position { f(&row.record) } else { row.record.clone() });
    }
    next
}

/// A position of a non-empty `records`, drawn or covered.
pub fn position<T>(tc: &TestCase, pick: Pick, records: &Records<T>) -> Position {
    let index = pick.variant(tc, records.len(), |index| index);
    Position(u32::try_from(index).expect("small fixture"))
}

/// `records` as written at `version`: rows at positions `before` lacks take `version`; each
/// record passes through `inner` with its counterpart in `before`, for records nested in it.
pub fn stamp<T>(records: &Records<T>, before: Option<&Records<T>>, version: u64, mut inner: impl FnMut(&T, Option<&T>) -> T) -> Records<T> {
    let mut stamped = Records::new();
    for row in records {
        let previous = before.and_then(|before| before.get(row.position));
        stamped.append(previous.map_or(version, |_| row.version), inner(&row.record, previous.map(|previous| &previous.record)));
    }
    stamped
}

/// `records` as written at `version`, its rows holding no records of their own.
pub fn stamp_flat<T: Clone>(records: &Records<T>, before: Option<&Records<T>>, version: u64) -> Records<T> {
    stamp(records, before, version, |record, _| record.clone())
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
    Identity { kind: KindCode::new(tc.draw(gs::sampled_from(vec!["a", "k1", "zz_9", "abcdefgh"]))), name: name(tc), home: home(tc) }
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
