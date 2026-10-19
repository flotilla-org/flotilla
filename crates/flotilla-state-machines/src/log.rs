//! Where a home's writes go: its per-home log (#3392 G8), and the only
//! source of a resource's version (#3373 ruling R2).
//!
//! A [`Home`](crate::Home) decides; its [`Log`] persists each create and edge
//! and gives the write its sequence, which the resource adopts as its version.
//! Over the store, the log is `apply_edge` and `insert`, and the sequence is
//! the store's `home_seq`; the storage port that adapts one is a later layer.
//! [`MemoryLog`] keeps the same contract in memory, for tests, replay and
//! conformance.
//!
//! History is the log's entries, never part of a resource (#3373 ruling R1):
//! a resource's edges are read in pages, as a range over (resource, sequence),
//! by [`MemoryLog::history`] in memory and by the store's `changes` over its
//! `log_by_resource` index, whose entries [`edge_event`] decodes.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    ops::Bound::{Excluded, Included},
    sync::Arc,
};

use flotilla_schema::{Cause, Cbor, Code, Event, EventError, Timestamp};

use crate::{Coded, EdgeEventOf, EdgeKind, EdgePayload, InputRead, Label, Machine, Resource, StoredState, Version};

/// Persists a home's writes and gives each its sequence.
pub trait Log<M: Machine> {
    /// Persists the creation of `resource` by `writer` at `at`, with the reads
    /// its admission relied on as the entry's payload, and returns its version.
    /// The payload never copies the spec: the create's effect holds it, so a
    /// frozen body is stored at most twice (#3424 ruling 4). The resource's
    /// own version is not yet set.
    fn create(&mut self, writer: &str, at: Timestamp, resource: &Resource<M>, reads: &[InputRead]) -> Result<Version, LogError>;

    /// Storage-assigned creation metadata seam. The log may assign
    /// persistence metadata in its create transaction and returns the exact
    /// committed resource. It must preserve the admitted identity/state and
    /// admission inputs; a failure must not publish a resource to Home.
    ///
    /// The default preserves existing machines' immutable-spec create behavior.
    /// A specialized adapter must not change fields read by admission or gates.
    fn create_resource(
        &mut self,
        writer: &str,
        at: Timestamp,
        mut resource: Resource<M>,
        reads: &[InputRead],
    ) -> Result<Resource<M>, LogError> {
        resource.version = self.create(writer, at, &resource, reads)?;
        Ok(resource)
    }

    /// Persists `event`, which moved `next` out of `previous`, and returns the
    /// write's version. `next` holds its next state at the version the write
    /// must find stored, so a log over the store applies the edge under that
    /// stamp ([`Resource::stored_edge`]).
    fn edge(&mut self, previous: &StoredState<M>, next: &Resource<M>, event: &EdgeEventOf<M>) -> Result<Version, LogError>;
}

/// Why a log did not take a write: the store's refusal, such as a version
/// conflict or a full transaction, carried as it was raised.
#[derive(Clone)]
pub struct LogError(Arc<dyn Error + Send + Sync>);

impl LogError {
    pub fn new(error: impl Into<Box<dyn Error + Send + Sync>>) -> LogError {
        LogError(Arc::from(error.into()))
    }

    /// The error as the log raised it.
    pub fn source(&self) -> &(dyn Error + Send + Sync + 'static) {
        &*self.0
    }
}

impl fmt::Debug for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("LogError").field(&self.0.to_string()).finish()
    }
}

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Two log errors are equal when they say the same thing.
impl PartialEq for LogError {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_string() == other.0.to_string()
    }
}

/// An in-memory log: one home's sequence, and its resources' edge events by
/// (name, sequence).
pub struct MemoryLog<M: Machine> {
    sequence: u64,
    edges: BTreeMap<(String, u64), EdgeEventOf<M>>,
}

impl<M: Machine> MemoryLog<M> {
    pub fn new() -> Self {
        MemoryLog { sequence: 0, edges: BTreeMap::new() }
    }

    /// The highest sequence this log has given; 0 when none.
    pub fn last_sequence(&self) -> u64 {
        self.sequence
    }

    /// Up to `limit` edge events of `name` after version `after`, in sequence
    /// order, each with the version its write gave: the resource's history
    /// from `Version(0)`, paged by passing the last version back.
    pub fn history(&self, name: &str, after: Version, limit: usize) -> Vec<(Version, &EdgeEventOf<M>)> {
        let range = (Excluded((name.to_owned(), after.0)), Included((name.to_owned(), u64::MAX)));
        self.edges.range(range).take(limit).map(|((_, sequence), event)| (Version(*sequence), event)).collect()
    }

    /// The next sequence: above every one given, and above `version`, the
    /// version of a resource restored from a store that had already given it.
    fn next(&mut self, version: Version) -> Version {
        self.sequence = self.sequence.max(version.0).saturating_add(1);
        Version(self.sequence)
    }
}

impl<M: Machine> Default for MemoryLog<M> {
    fn default() -> Self {
        MemoryLog::new()
    }
}

impl<M: Machine> Log<M> for MemoryLog<M> {
    fn create(&mut self, _: &str, _: Timestamp, _: &Resource<M>, _: &[InputRead]) -> Result<Version, LogError> {
        Ok(self.next(Version(0)))
    }

    fn edge(&mut self, _: &StoredState<M>, next: &Resource<M>, event: &EdgeEventOf<M>) -> Result<Version, LogError> {
        let version = self.next(next.version());
        self.edges.insert((next.name().to_owned(), version.0), event.clone());
        Ok(version)
    }
}

/// Why a log entry is not an edge event of machine `M`.
#[derive(Debug)]
pub enum EntryError {
    /// A code in the entry's header names no label or edge of the machine.
    UnknownCode { machine: &'static str, what: &'static str, code: Code },
    /// An edge entry with no document.
    MissingPayload,
    /// The document is not the machine's edge payload.
    Payload(EventError),
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryError::UnknownCode { machine, what, code } => write!(f, "{code} is not a {what} code of {machine}"),
            EntryError::MissingPayload => f.write_str("the edge entry has no payload"),
            EntryError::Payload(error) => write!(f, "the edge payload does not decode: {error}"),
        }
    }
}

impl Error for EntryError {}

/// The edge event a store log entry records, as the home that wrote it
/// returned it: `None` for an entry that is no edge (a create, spec edit,
/// delete or expunge). The header gives the edge and label codes, the writer
/// and time; the document gives the edge and its reads.
pub fn edge_event<M: Machine>(entry: &Event) -> Result<Option<EdgeEventOf<M>>, EntryError>
where
    EdgePayload<M::Edge>: Cbor,
{
    let Cause::Edge { edge, from, to } = entry.cause else {
        return Ok(None);
    };
    let payload: EdgePayload<M::Edge> = entry.payload().ok_or(EntryError::MissingPayload)?.map_err(EntryError::Payload)?;
    Ok(Some(flotilla_schema::EdgeEvent {
        edge: coded::<M, EdgeKind<M>>("edge", edge)?,
        from: coded::<M, Label<M>>("label", from)?,
        to: coded::<M, Label<M>>("label", to)?,
        writer: entry.writer.clone(),
        at: entry.at,
        payload,
    }))
}

fn coded<M: Machine, C: Coded>(what: &'static str, code: Code) -> Result<C, EntryError> {
    C::ALL.iter().copied().find(|value| value.code() == code).ok_or(EntryError::UnknownCode { machine: M::NAME, what, code })
}
