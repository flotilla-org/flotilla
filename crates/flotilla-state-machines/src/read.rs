//! The recording handle and typed reads.
//!
//! Gates, effects and judgements see the world only through a
//! [`Handle`]. A typed read `handle.read(&reference)` records the reference
//! and the version it saw, or an absence, before it returns. A gate's reads
//! become its edge event's inputs, and a judgement's reads become its
//! dependency set (#3211), with no way for either to read unrecorded.
//!
//! What a machine may read is part of its declaration: [`Machine::World`] is
//! usually a trait object whose supertraits are the [`Source`]s it needs, so a
//! home that cannot serve a read does not compile.

use std::{borrow::Cow, collections::BTreeMap, fmt, marker::PhantomData};

use flotilla_schema::{RecordRow, Timestamp};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Attempt, Machine, Receipt, Resource};

/// Something a gate or judgement can read: a resource kind, or a family of
/// records such as one session's receipts, each named by a [`Ref`].
pub trait Kind: 'static {
    /// The stable name recorded with each read.
    const NAME: &'static str;
    type Value;
}

/// A typed reference to one value of kind `K`. Serde sees its name alone:
/// the kind is the type's.
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

impl<K: Kind> Serialize for Ref<K> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.name)
    }
}

impl<'de, K: Kind> Deserialize<'de> for Ref<K> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Ref::new)
    }
}

/// A value's version at its home: it rises on every write. A record row's
/// `version` is the owner's `Version.0`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
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

/// The [`InputRead::kind`] of a read of the resource's own attempts.
pub const OWN_ATTEMPTS: &str = "attempts";
/// The [`InputRead::kind`] of a read of the resource's own receipts.
pub const OWN_RECEIPTS: &str = "receipts";

/// One recorded read: what was read and the version seen, or `None` when the
/// read found nothing. A gate relying on an absence relied on an input too.
/// `kind` is a [`Kind::NAME`] (or [`OWN_ATTEMPTS`], [`OWN_RECEIPTS`]) when
/// recorded and owned text when decoded from an edge event's document.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct InputRead {
    pub kind: Cow<'static, str>,
    pub name: String,
    pub version: Option<Version>,
}

/// A judgement's value, the moment it could next change by time alone, and
/// the reads it depended on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judged<T> {
    pub value: T,
    /// `None` when only a change to one of `reads` can change the value.
    pub valid_until: Option<Timestamp>,
    pub reads: Vec<InputRead>,
}

/// The only way a gate, an effect or a judgement sees anything beyond its
/// edge argument and the current state. Every read through it is recorded.
pub struct Handle<'a, M: Machine> {
    own: Option<&'a Resource<M>>,
    spec: &'a M::Spec,
    state: &'a M::State,
    world: &'a M::World,
    now: Timestamp,
    reads: Vec<InputRead>,
    valid_until: Option<Timestamp>,
}

impl<'a, M: Machine> Handle<'a, M> {
    /// A handle for a pure step or judgement over a spec and state alone,
    /// with no records: own-record reads see empty logs, and record them
    /// under the empty name with version 0, which is exactly what they saw.
    pub fn new(spec: &'a M::Spec, state: &'a M::State, world: &'a M::World, now: Timestamp) -> Self {
        Handle { own: None, spec, state, world, now, reads: Vec::new(), valid_until: None }
    }

    /// A handle on a resource and its records, at `now`.
    pub fn on(resource: &'a Resource<M>, world: &'a M::World, now: Timestamp) -> Self {
        Handle { own: Some(resource), spec: resource.spec(), state: resource.state(), world, now, reads: Vec::new(), valid_until: None }
    }

    /// The resource's own name, or empty for a pure handle. Fixed, so unrecorded.
    pub fn name(&self) -> &'a str {
        self.own.map_or("", |own| own.name())
    }

    /// The resource's own spec. It never changes, so reading it records nothing.
    pub fn spec(&self) -> &'a M::Spec {
        self.spec
    }

    /// The resource's current state: what gates check and effects change.
    /// It is the resource's own, so reading it records nothing.
    pub fn state(&self) -> &'a M::State {
        self.state
    }

    /// The time the caller supplied.
    pub fn now(&self) -> Timestamp {
        self.now
    }

    /// Reads `reference` and records the version seen, or the absence.
    pub fn read<K: Kind>(&mut self, reference: &Ref<K>) -> Option<&'a K::Value>
    where
        M::World: Source<K>,
    {
        let world: &'a M::World = self.world;
        let found = world.lookup(reference);
        self.record(InputRead { kind: Cow::Borrowed(K::NAME), name: reference.name.clone(), version: found.map(|(version, _)| version) });
        found.map(|(_, value)| value)
    }

    /// Records `read` once: a gate and an effect reading the same value at
    /// the same version relied on one input.
    fn record(&mut self, read: InputRead) {
        if !self.reads.contains(&read) {
            self.reads.push(read);
        }
    }

    fn own_read(&mut self, kind: &'static str, length: usize) {
        let name = self.name().to_string();
        self.record(InputRead { kind: Cow::Borrowed(kind), name, version: Some(Version(length as u64)) });
    }

    /// The resource's own attempts, every window. Recorded with the log's
    /// length as its version.
    pub fn attempts(&mut self) -> &'a [RecordRow<Attempt>] {
        let rows = self.own.map_or(&[][..], |own| own.attempts().rows());
        self.own_read(OWN_ATTEMPTS, rows.len());
        rows
    }

    /// The resource's own receipts. Recorded like [`Handle::attempts`].
    pub fn receipts(&mut self) -> &'a [RecordRow<Receipt>] {
        let rows = self.own.map_or(&[][..], |own| own.receipts().rows());
        self.own_read(OWN_RECEIPTS, rows.len());
        rows
    }

    /// Declares that the value being computed may change at `moment` by time
    /// alone. The earliest declared moment wins.
    pub fn until(&mut self, moment: Timestamp) {
        self.valid_until = Some(self.valid_until.map_or(moment, |current| current.min(moment)));
    }

    /// The distinct reads recorded so far, in order.
    pub fn reads(&self) -> &[InputRead] {
        &self.reads
    }

    /// Finishes a judgement, returning `value` with its validity and inputs.
    pub fn judged<T>(self, value: T) -> Judged<T> {
        Judged { value, valid_until: self.valid_until, reads: self.reads }
    }
}
