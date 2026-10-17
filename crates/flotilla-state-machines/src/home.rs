//! The single writer: an in-memory home for one machine's resources.
//!
//! Only [`Home`] changes a resource. It applies an edge in a fixed order (the
//! end check, the from-label, the gate, the effect), replaces the current
//! state with the effect's result and appends the edge event in the same
//! write. Refusals return to the caller and record nothing: no row, no
//! version change. Every write is an edge: attempts and receipts too (R4).
//! Reactions to an
//! event (teardown, dead letters, follow-up messages) belong to subscribers
//! outside the engine.

use std::{collections::BTreeMap, fmt};

use flotilla_schema::{EdgeEvent, Records, RecordsFull, State, Timestamp};

use crate::{
    Admission, DefError, Edge, EdgeEventOf, EdgeKind, EdgePayload, Fault, Handle, InputRead, Judged, Label, Machine, MachineDef,
    StateValue, StepError, StoredState, Version,
};

/// One resource at its home: its name, spec, current state (with the time its label was entered),
/// version and edge events. This is the stored kind (#3392 ruling D5): the derive lays out the spec
/// and the state, the resource its edge log, and the store keeps the name and version.
pub struct Resource<M: Machine> {
    pub(crate) name: String,
    pub(crate) spec: M::Spec,
    pub(crate) state: StoredState<M>,
    pub(crate) version: Version,
    pub(crate) events: Records<EdgeEventOf<M>>,
}

impl<M: Machine> Resource<M> {
    /// A resource as storage holds it, with no edge events: what a store read or a test places at a
    /// home ([`Home::restore`]). Only a home changes it afterwards.
    pub fn stored(name: impl Into<String>, spec: M::Spec, state: StoredState<M>, version: Version) -> Self {
        Resource { name: name.into(), spec, state, version, events: Records::new() }
    }

    pub fn with_events(self, events: Records<EdgeEventOf<M>>) -> Self {
        Resource { events, ..self }
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

    /// Rises on every accepted edge.
    pub fn version(&self) -> Version {
        self.version
    }

    /// The history of applied edges, attempts and receipts among them, for
    /// showing and auditing what happened. No gate, effect or judgement of
    /// this resource reads it: the current state is the answer to what is
    /// true now.
    pub fn events(&self) -> &Records<EdgeEventOf<M>> {
        &self.events
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
        Resource {
            name: self.name.clone(),
            spec: self.spec.clone(),
            state: self.state.clone(),
            version: self.version,
            events: self.events.clone(),
        }
    }
}

impl<M: Machine> PartialEq for Resource<M>
where
    M::Spec: PartialEq,
    EdgeEventOf<M>: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        (&self.name, &self.spec, &self.state, self.version, &self.events)
            == (&other.name, &other.spec, &other.state, other.version, &other.events)
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
            .field("events", &self.events.len())
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
    /// One of the resource's record tables has used every position.
    Full(RecordsFull),
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
            (WriteError::Full(a), WriteError::Full(b)) => a == b,
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
            WriteError::Full(full) => f.debug_tuple("Full").field(full).finish(),
        }
    }
}

/// The single writer for one machine's resources at one home.
pub struct Home<M: Machine> {
    writer: String,
    definition: MachineDef<M>,
    resources: BTreeMap<String, Resource<M>>,
}

impl<M: Machine> Home<M> {
    /// A home writing as `writer`, the identity every edge event it appends
    /// carries. Refuses a declaration that breaks the rules.
    pub fn new(writer: impl Into<String>) -> Result<Self, Vec<DefError>> {
        let definition = M::definition();
        definition.validate()?;
        Ok(Home { writer: writer.into(), definition, resources: BTreeMap::new() })
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

    pub fn resources(&self) -> impl Iterator<Item = &Resource<M>> {
        self.resources.values()
    }

    /// Admits `request` at `now`, reading `world` through the recording
    /// admission handle, and creates the resource in the state admission
    /// returns, which must carry the declared initial label, entered at `now`.
    /// Returns the resource and the reads admission relied on, for the
    /// create's event.
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
        let resource = Resource::stored(name.clone(), spec, State::new(state, now), Version(1));
        let reads = admission.into_reads();
        Ok(Created { resource: self.resources.entry(name).or_insert(resource), reads })
    }

    /// Places a resource read from storage at this home, as stored: its spec,
    /// state and edge events, at its version. Nothing is checked against the
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
    /// On success the resource holds the next state, entered at `now` when the
    /// label changes (otherwise at the time the label was entered), its version
    /// rises, and the returned event (whose payload holds the edge and the
    /// gate's and effect's reads) is appended to its edge events, stamped with
    /// the new version. [`Resource::stored_edge`] gives the stored edge persisting it.
    /// A resource whose edge log has used every position refuses with
    /// [`WriteError::Full`] and is left unchanged.
    pub fn apply(
        &mut self,
        name: &str,
        edge: impl Into<M::Edge>,
        world: &M::World,
        now: Timestamp,
    ) -> Result<&EdgeEventOf<M>, WriteError<M>> {
        let edge = edge.into();
        let resource = self.resources.get(name).ok_or_else(|| WriteError::Unknown(name.to_string()))?;
        let mut handle = Handle::on(resource, world, now);
        let next = self.definition.step(&mut handle, &edge)?;
        let reads = handle.judged(()).reads;
        // The step borrowed the resource immutably; this second lookup of the
        // same name under `&mut self` cannot miss.
        let resource = self.resources.get_mut(name).ok_or_else(|| WriteError::Unknown(name.to_string()))?;
        if resource.events.is_full() {
            return Err(WriteError::Full(RecordsFull));
        }
        let from = resource.label();
        let to = next.label();
        let since = if from == to { resource.state.since } else { now };
        resource.version = resource.version.next();
        resource.state = State::new(next, since);
        let event = EdgeEvent { edge: edge.kind(), from, to, writer: self.writer.clone(), at: now, payload: EdgePayload { edge, reads } };
        Ok(&resource.events.append(resource.version.0, event).map_err(WriteError::Full)?.record)
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
