//! The single writer: an in-memory home for one machine's resources.
//!
//! Only [`Home`] changes a resource. It applies an edge in a fixed order (the
//! end check, the from-label, the gate, the effect), writes the next state and
//! the edge event through its [`Log`] in one write, and takes the version the
//! log gives that write. Refusals return to the caller and record nothing: no
//! row, no version change. Every write is an edge: attempts and receipts too
//! (R4). A resource holds no history (R1): its events are the log's entries,
//! read in pages, and nothing that decides reads them. Reactions to an event
//! (teardown, dead letters, follow-up messages) belong to subscribers outside
//! the engine.

use std::{collections::BTreeMap, fmt};

use flotilla_schema::{EdgeEvent, State, Timestamp};

use crate::{
    Admission, DefError, Edge, EdgeEventOf, EdgeKind, EdgePayload, Fault, Handle, InputRead, Judged, Label, Log, LogError, Machine,
    MachineDef, MemoryLog, StateValue, StepError, StoredState, Version,
};

/// One resource at its home: its name, spec, current state (with the time its label was entered)
/// and version. This is the stored kind (#3392 ruling D5): the derive lays out the spec and the
/// state, and the store keeps the name and version. Its history is no part of it (#3373 ruling
/// R1): each edge is an entry in the home's log, so loading the current state loads no history.
pub struct Resource<M: Machine> {
    pub(crate) name: String,
    pub(crate) spec: M::Spec,
    pub(crate) state: StoredState<M>,
    pub(crate) version: Version,
}

impl<M: Machine> Resource<M> {
    /// A resource as storage holds it: what a store read or a test places at a home
    /// ([`Home::restore`]). Only a home changes it afterwards.
    pub fn stored(name: impl Into<String>, spec: M::Spec, state: StoredState<M>, version: Version) -> Self {
        Resource { name: name.into(), spec, state, version }
    }

    /// The same resource with another spec: a spec edit, as storage applies one.
    pub fn with_spec(self, spec: M::Spec) -> Self {
        Resource { spec, ..self }
    }

    /// The same resource in another stored state, as storage holds one.
    pub fn with_state(self, state: StoredState<M>) -> Self {
        Resource { state, ..self }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn spec(&self) -> &M::Spec {
        &self.spec
    }

    /// The current state: what gates read and the next edge changes.
    pub fn state(&self) -> &M::State {
        &self.state.value
    }

    /// When the current state's label was entered: the creation time, or the time of the edge
    /// that entered it. An edge that keeps the label keeps it.
    pub fn since(&self) -> Timestamp {
        self.state.since
    }

    /// The state as stored: the value and [`Resource::since`].
    pub fn stored_state(&self) -> &StoredState<M> {
        &self.state
    }

    /// The current state's label.
    pub fn label(&self) -> Label<M> {
        self.state.value.label()
    }

    /// The sequence its home's log gave the write that left it so (#3373 ruling R2): it rises on
    /// every accepted write, and is the store's `home_seq` for a home over a store.
    pub fn version(&self) -> Version {
        self.version
    }

    /// Runs a judgement through a recording handle. The resource is unchanged.
    pub fn judge<T>(&self, world: &M::World, now: Timestamp, judgement: impl FnOnce(&mut Handle<'_, M>) -> T) -> Judged<T> {
        let mut handle = Handle::on(self, world, now);
        let value = judgement(&mut handle);
        handle.judged(value)
    }
}

impl<M: Machine> Clone for Resource<M>
where
    M::Spec: Clone,
{
    fn clone(&self) -> Self {
        Resource { name: self.name.clone(), spec: self.spec.clone(), state: self.state.clone(), version: self.version }
    }
}

impl<M: Machine> PartialEq for Resource<M>
where
    M::Spec: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        (&self.name, &self.spec, &self.state, self.version) == (&other.name, &other.spec, &other.state, other.version)
    }
}

impl<M: Machine> fmt::Debug for Resource<M>
where
    M::Spec: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resource")
            .field("name", &self.name)
            .field("spec", &self.spec)
            .field("state", &self.state.value)
            .field("since", &self.state.since)
            .field("version", &self.version)
            .finish()
    }
}

/// A created resource and the reads its admission relied on.
pub struct Created<'a, M: Machine> {
    pub resource: &'a Resource<M>,
    pub reads: Vec<InputRead>,
}

impl<M: Machine> fmt::Debug for Created<'_, M>
where
    M::Spec: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Created").field("resource", &self.resource).field("reads", &self.reads).finish()
    }
}

/// Why a write did not happen.
pub enum WriteError<M: Machine> {
    /// No resource of that name at this home.
    Unknown(String),
    /// Creation of an existing name.
    Exists(String),
    /// The machine refused: admission, an end, the wrong state or a gate.
    Refused(M::Refusal),
    /// A declaration fault; see [`Fault`].
    Fault(Fault<Label<M>, EdgeKind<M>>),
    /// The log did not take the write; the resource is as it was.
    Log(LogError),
}

impl<M: Machine> WriteError<M> {
    /// The machine's refusal, when this is one.
    pub fn refusal(&self) -> Option<&M::Refusal> {
        match self {
            WriteError::Refused(refusal) => Some(refusal),
            _ => None,
        }
    }
}

impl<M: Machine> From<StepError<M>> for WriteError<M> {
    fn from(error: StepError<M>) -> Self {
        match error {
            StepError::Refused(refusal) => WriteError::Refused(refusal),
            StepError::Fault(fault) => WriteError::Fault(fault),
        }
    }
}

impl<M: Machine> PartialEq for WriteError<M> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (WriteError::Unknown(a), WriteError::Unknown(b)) | (WriteError::Exists(a), WriteError::Exists(b)) => a == b,
            (WriteError::Refused(a), WriteError::Refused(b)) => a == b,
            (WriteError::Fault(a), WriteError::Fault(b)) => a == b,
            (WriteError::Log(a), WriteError::Log(b)) => a == b,
            _ => false,
        }
    }
}

impl<M: Machine> fmt::Debug for WriteError<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Unknown(name) => f.debug_tuple("Unknown").field(name).finish(),
            WriteError::Exists(name) => f.debug_tuple("Exists").field(name).finish(),
            WriteError::Refused(refusal) => f.debug_tuple("Refused").field(refusal).finish(),
            WriteError::Fault(fault) => f.debug_tuple("Fault").field(fault).finish(),
            WriteError::Log(error) => f.debug_tuple("Log").field(error).finish(),
        }
    }
}

/// The single writer for one machine's resources at one home, writing through
/// the log `L`: the store's per-home log, or an in-memory [`MemoryLog`].
pub struct Home<M: Machine, L: Log<M> = MemoryLog<M>> {
    writer: String,
    definition: MachineDef<M>,
    resources: BTreeMap<String, Resource<M>>,
    log: L,
}

impl<M: Machine> Home<M> {
    /// A home writing as `writer`, the identity every edge event it writes
    /// carries, to an in-memory log of its own. Refuses a declaration that
    /// breaks the rules.
    pub fn new(writer: impl Into<String>) -> Result<Self, Vec<DefError>> {
        Home::with_log(writer, MemoryLog::new())
    }
}

impl<M: Machine, L: Log<M>> Home<M, L> {
    /// A home writing as `writer` through `log`.
    pub fn with_log(writer: impl Into<String>, log: L) -> Result<Self, Vec<DefError>> {
        let definition = M::definition();
        definition.validate()?;
        Ok(Home { writer: writer.into(), definition, resources: BTreeMap::new(), log })
    }

    /// The log this home writes through: where its resources' history is read.
    pub fn log(&self) -> &L {
        &self.log
    }

    pub fn log_mut(&mut self) -> &mut L {
        &mut self.log
    }

    pub fn writer(&self) -> &str {
        &self.writer
    }

    pub fn definition(&self) -> &MachineDef<M> {
        &self.definition
    }

    pub fn get(&self, name: &str) -> Option<&Resource<M>> {
        self.resources.get(name)
    }

    /// Removes only this home's read replica value. The log remains authoritative;
    /// a storage adapter may restore the value before the next keyed operation.
    /// The resident duplicate check (`WriteError::Exists`) is no longer
    /// authoritative for evicted names: the log must enforce source uniqueness
    /// if a caller tries to create that name again.
    pub fn evict(&mut self, name: &str) -> Option<Resource<M>> {
        self.resources.remove(name)
    }

    pub fn resources(&self) -> impl Iterator<Item = &Resource<M>> {
        self.resources.values()
    }

    /// Admits `request` at `now`, reading `world` through the recording
    /// admission handle, and creates the resource in the state admission
    /// returns, which must carry the declared initial label, entered at `now`.
    /// The log takes the create, with the reads admission relied on, and gives
    /// the resource its version. Returns the resource and those reads.
    pub fn create(
        &mut self,
        name: impl Into<String>,
        request: M::Request,
        world: &M::World,
        now: Timestamp,
    ) -> Result<Created<'_, M>, WriteError<M>> {
        let name = name.into();
        if self.resources.contains_key(&name) {
            return Err(WriteError::Exists(name));
        }
        let mut admission = Admission::new(world, now);
        let (spec, state) = M::admit(&mut admission, request).map_err(WriteError::Refused)?;
        if state.label() != self.definition.initial() {
            return Err(WriteError::Fault(Fault::Initial { state: state.label() }));
        }
        let resource = Resource::stored(name.clone(), spec, State::new(state, now), Version(0));
        let reads = admission.into_reads();
        let resource = self.log.create_resource(&self.writer, now, resource, &reads).map_err(WriteError::Log)?;
        Ok(Created { resource: self.resources.entry(name).or_insert(resource), reads })
    }

    /// Places a resource read from storage at this home, as stored: its spec
    /// and state, at its version. Nothing is checked against the
    /// declaration: the resource holds what earlier writes left. The storage
    /// port (a later layer) is its caller; tests use it to apply an edge to a
    /// stored resource.
    pub fn restore(&mut self, resource: Resource<M>) -> Result<&Resource<M>, WriteError<M>> {
        if self.resources.contains_key(&resource.name) {
            return Err(WriteError::Exists(resource.name));
        }
        Ok(self.resources.entry(resource.name.clone()).or_insert(resource))
    }

    /// Applies `edge` at `now`, reading `world` through the recording handle.
    /// The next state is entered at `now` when the label changes (otherwise at
    /// the time the label was entered), and the event's payload holds the edge
    /// and the gate's and effect's reads. The log takes the write (the
    /// previous state, the resource in its next state and the event) and gives
    /// its version, which the resource adopts (#3373 ruling R2): the home
    /// mints none. Returns the event. When the log refuses, the resource is
    /// left as it was, with [`WriteError::Log`].
    pub fn apply(
        &mut self,
        name: &str,
        edge: impl Into<M::Edge>,
        world: &M::World,
        now: Timestamp,
    ) -> Result<EdgeEventOf<M>, WriteError<M>> {
        let edge = edge.into();
        let resource = self.resources.get(name).ok_or_else(|| WriteError::Unknown(name.to_string()))?;
        let mut handle = Handle::on(resource, world, now);
        let next = self.definition.step(&mut handle, &edge)?;
        let reads = handle.judged(()).reads;
        // The step borrowed the resource immutably; this second lookup of the
        // same name under `&mut self` cannot miss.
        let resource = self.resources.get_mut(name).ok_or_else(|| WriteError::Unknown(name.to_string()))?;
        let from = resource.label();
        let to = next.label();
        let since = if from == to { resource.state.since } else { now };
        let previous = std::mem::replace(&mut resource.state, State::new(next, since));
        let event = EdgeEvent { edge: edge.kind(), from, to, writer: self.writer.clone(), at: now, payload: EdgePayload { edge, reads } };
        match self.log.edge(&previous, resource, &event) {
            Ok(version) => {
                resource.version = version;
                Ok(event)
            }
            Err(error) => {
                resource.state = previous;
                Err(WriteError::Log(error))
            }
        }
    }

    /// Runs a judgement over one resource; see [`Resource::judge`].
    pub fn judge<T>(
        &self,
        name: &str,
        world: &M::World,
        now: Timestamp,
        judgement: impl FnOnce(&mut Handle<'_, M>) -> T,
    ) -> Result<Judged<T>, WriteError<M>> {
        let resource = self.get(name).ok_or_else(|| WriteError::Unknown(name.to_string()))?;
        Ok(resource.judge(world, now, judgement))
    }
}
