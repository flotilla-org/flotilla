//! The recording handle and typed reads.
//!
//! Gates, chosen targets and judgements see the world only through a
//! [`Handle`]. A typed read `handle.read(&reference)` records the reference
//! and the version it saw, or an absence, before it returns. A gate's reads
//! become its edge event's inputs, and a judgement's reads become its
//! dependency set (#3211), with no way for either to read unrecorded.
//!
//! What a machine may read is part of its declaration: [`Machine::World`] is
//! usually a trait object whose supertraits are the [`Source`]s it needs, so a
//! home that cannot serve a read does not compile.

use std::{collections::BTreeMap, fmt, marker::PhantomData};

use crate::{Machine, Time};

/// Something a gate or judgement can read: a resource kind, or a family of
/// records such as one session's receipts, each named by a [`Ref`].
pub trait Kind: 'static {
    /// The stable name recorded with each read.
    const NAME: &'static str;
    type Value;
}

/// A typed reference to one value of kind `K`.
pub struct Ref<K: Kind> {
    name: String,
    kind: PhantomData<fn() -> K>,
}

impl<K: Kind> Ref<K> {
    pub fn new(name: impl Into<String>) -> Self {
        Ref { name: name.into(), kind: PhantomData }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl<K: Kind> Clone for Ref<K> {
    fn clone(&self) -> Self {
        Ref::new(self.name.clone())
    }
}

impl<K: Kind> PartialEq for Ref<K> {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl<K: Kind> Eq for Ref<K> {}

impl<K: Kind> PartialOrd for Ref<K> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<K: Kind> Ord for Ref<K> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl<K: Kind> std::hash::Hash for Ref<K> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl<K: Kind> fmt::Debug for Ref<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", K::NAME, self.name)
    }
}

/// A value's version at its home: it rises on every write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub u64);

impl Version {
    /// The following version. Saturates: a single writer issuing 2^64
    /// versions to one value is not a case to wrap or panic over.
    pub fn next(self) -> Version {
        Version(self.0.saturating_add(1))
    }
}

/// Serves typed reads of kind `K` from the home's local, versioned state.
pub trait Source<K: Kind> {
    fn lookup(&self, reference: &Ref<K>) -> Option<(Version, &K::Value)>;
}

/// An in-memory [`Source`]: named values, each version rising on every write.
pub struct Table<K: Kind> {
    rows: BTreeMap<String, (Version, K::Value)>,
    clock: Version,
}

impl<K: Kind> Default for Table<K> {
    fn default() -> Self {
        Table { rows: BTreeMap::new(), clock: Version::default() }
    }
}

impl<K: Kind> Table<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes `value`; its version is newer than any this table has issued.
    pub fn put(&mut self, reference: &Ref<K>, value: K::Value) -> Version {
        self.clock = self.clock.next();
        self.rows.insert(reference.name.clone(), (self.clock, value));
        self.clock
    }

    pub fn remove(&mut self, reference: &Ref<K>) -> Option<K::Value> {
        self.rows.remove(&reference.name).map(|(_, value)| value)
    }
}

impl<K: Kind> Source<K> for Table<K> {
    fn lookup(&self, reference: &Ref<K>) -> Option<(Version, &K::Value)> {
        self.rows.get(&reference.name).map(|(version, value)| (*version, value))
    }
}

/// One recorded read: what was read and the version seen, or `None` when the
/// read found nothing. A gate relying on an absence relied on an input too.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputRead {
    pub kind: &'static str,
    pub name: String,
    pub version: Option<Version>,
}

/// A judgement's value, the moment it could next change by time alone, and
/// the reads it depended on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judged<T> {
    pub value: T,
    /// `None` when only a change to one of `reads` can change the value.
    pub valid_until: Option<Time>,
    pub reads: Vec<InputRead>,
}

/// The only way a gate, a chosen target or a judgement sees anything beyond
/// its edge argument. Every read through it is recorded.
pub struct Handle<'a, M: Machine> {
    spec: &'a M::Spec,
    state: M::State,
    world: &'a M::World,
    now: Time,
    reads: Vec<InputRead>,
    valid_until: Option<Time>,
}

impl<'a, M: Machine> Handle<'a, M> {
    /// A handle on one resource, given its spec and state, at `now`.
    pub fn new(spec: &'a M::Spec, state: M::State, world: &'a M::World, now: Time) -> Self {
        Handle { spec, state, world, now, reads: Vec::new(), valid_until: None }
    }

    /// The resource's own spec. It never changes, so reading it records nothing.
    pub fn spec(&self) -> &'a M::Spec {
        self.spec
    }

    /// The resource's current state.
    pub fn state(&self) -> M::State {
        self.state
    }

    /// The time the caller supplied.
    pub fn now(&self) -> Time {
        self.now
    }

    /// Reads `reference` and records the version seen, or the absence.
    pub fn read<K: Kind>(&mut self, reference: &Ref<K>) -> Option<&'a K::Value>
    where
        M::World: Source<K>,
    {
        let world: &'a M::World = self.world;
        let found = world.lookup(reference);
        self.reads.push(InputRead { kind: K::NAME, name: reference.name.clone(), version: found.map(|(version, _)| version) });
        found.map(|(_, value)| value)
    }

    /// Declares that the value being computed may change at `moment` by time
    /// alone. The earliest declared moment wins.
    pub fn until(&mut self, moment: Time) {
        self.valid_until = Some(self.valid_until.map_or(moment, |current| current.min(moment)));
    }

    /// The reads recorded so far, in order.
    pub fn reads(&self) -> &[InputRead] {
        &self.reads
    }

    /// Finishes a judgement, returning `value` with its validity and inputs.
    pub fn judged<T>(self, value: T) -> Judged<T> {
        Judged { value, valid_until: self.valid_until, reads: self.reads }
    }
}
