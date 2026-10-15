//! The single writer: an in-memory home for one machine's resources.
//!
//! Only [`Home`] changes a resource. It applies an edge in a fixed order (the
//! end check, the from-label, the gate, the effect), replaces the current
//! state with the effect's result and appends the edge event in the same
//! write. Refusals return to the caller and record nothing: no row, no
//! version change. Records on an ended resource are refused. Reactions to an
//! event (teardown, dead letters, follow-up messages) belong to subscribers
//! outside the engine.

use std::{collections::BTreeMap, fmt};

use crate::{
    Attempt, DefError, EdgeEvent, EdgeKind, Ended, Fault, Handle, Judged, Label, Machine, MachineDef, Position, Receipt, RecordLog,
    StateValue, StepError, Time, Version, Writer, WrongState,
};

/// One resource at its home: its spec, current state, version and records.
pub struct Resource<M: Machine> {
    name: String,
    spec: M::Spec,
    state: M::State,
    version: Version,
    events: RecordLog<EdgeEvent<M>>,
    attempts: RecordLog<Attempt>,
    receipts: RecordLog<Receipt>,
}

impl<M: Machine> Resource<M> {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn spec(&self) -> &M::Spec {
        &self.spec
    }

    /// The current state: what gates read and the next edge changes.
    pub fn state(&self) -> &M::State {
        &self.state
    }

    /// The current state's label.
    pub fn label(&self) -> Label<M> {
        self.state.label()
    }

    /// Rises on every accepted write: an edge or a record.
    pub fn version(&self) -> Version {
        self.version
    }

    /// The history of applied edges, for showing and auditing what happened.
    /// No gate, effect or judgement reads it: the current state is the answer
    /// to what is true now.
    pub fn events(&self) -> &RecordLog<EdgeEvent<M>> {
        &self.events
    }

    pub fn attempts(&self) -> &RecordLog<Attempt> {
        &self.attempts
    }

    pub fn receipts(&self) -> &RecordLog<Receipt> {
        &self.receipts
    }

    /// Runs a judgement through a recording handle. The resource is unchanged.
    pub fn judge<T>(&self, world: &M::World, now: Time, judgement: impl FnOnce(&mut Handle<'_, M>) -> T) -> Judged<T> {
        let mut handle = Handle::on(self, world, now);
        let value = judgement(&mut handle);
        handle.judged(value)
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
            .field("state", &self.state)
            .field("version", &self.version)
            .field("events", &self.events.len())
            .field("attempts", &self.attempts.len())
            .field("receipts", &self.receipts.len())
            .finish()
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
        }
    }
}

/// The single writer for one machine's resources at one home.
pub struct Home<M: Machine> {
    writer: Writer,
    definition: MachineDef<M>,
    resources: BTreeMap<String, Resource<M>>,
}

impl<M: Machine> Home<M> {
    /// A home writing as `writer`. Refuses a declaration that breaks the rules.
    pub fn new(writer: impl Into<String>) -> Result<Self, Vec<DefError>> {
        let definition = M::definition();
        definition.validate()?;
        Ok(Home { writer: Writer(writer.into()), definition, resources: BTreeMap::new() })
    }

    pub fn writer(&self) -> &Writer {
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

    /// Admits `request` and creates the resource in the state admission
    /// returns, which must carry the declared initial label.
    pub fn create(&mut self, name: impl Into<String>, request: M::Request) -> Result<&Resource<M>, WriteError<M>> {
        let name = name.into();
        if self.resources.contains_key(&name) {
            return Err(WriteError::Exists(name));
        }
        let (spec, state) = M::admit(request).map_err(WriteError::Refused)?;
        if state.label() != self.definition.initial() {
            return Err(WriteError::Fault(Fault::Initial { state: state.label() }));
        }
        let resource = Resource {
            events: RecordLog::new(&name),
            attempts: RecordLog::new(&name),
            receipts: RecordLog::new(&name),
            name: name.clone(),
            spec,
            state,
            version: Version(1),
        };
        Ok(self.resources.entry(name).or_insert(resource))
    }

    /// Applies `edge` at `now`, reading `world` through the recording handle.
    /// On success the resource holds the next state, its version rises, and
    /// the returned event (with the gate's and effect's reads) is appended to
    /// its edge events.
    pub fn apply(&mut self, name: &str, edge: impl Into<M::Edge>, world: &M::World, now: Time) -> Result<&EdgeEvent<M>, WriteError<M>> {
        let edge = edge.into();
        let resource = self.resources.get(name).ok_or_else(|| WriteError::Unknown(name.to_string()))?;
        let mut handle = Handle::on(resource, world, now);
        let next = self.definition.step(&mut handle, &edge)?;
        let reads = handle.judged(()).reads;
        // The step borrowed the resource immutably; this second lookup of the
        // same name under `&mut self` cannot miss.
        let resource = self.resources.get_mut(name).ok_or_else(|| WriteError::Unknown(name.to_string()))?;
        let from = resource.state.label();
        let to = next.label();
        resource.version = resource.version.next();
        resource.state = next;
        let event = EdgeEvent { edge, from, to, writer: self.writer.clone(), at: now, reads };
        Ok(&resource.events.append(resource.version, event).record)
    }

    /// Appends an attempt. Refused on an ended resource, with the end's
    /// reason, and in a state the machine does not record attempts in.
    pub fn record_attempt(&mut self, name: &str, attempt: Attempt) -> Result<Position, WriteError<M>> {
        let accepts = |definition: &MachineDef<M>, label| definition.accepts_attempts(label);
        let resource = self.writable(name, accepts)?;
        resource.version = resource.version.next();
        Ok(resource.attempts.append(resource.version, attempt).position)
    }

    /// Appends a receipt. Refused on an ended resource, with the end's reason.
    pub fn record_receipt(&mut self, name: &str, receipt: Receipt) -> Result<Position, WriteError<M>> {
        let resource = self.writable(name, |_, _| true)?;
        resource.version = resource.version.next();
        Ok(resource.receipts.append(resource.version, receipt).position)
    }

    fn writable(&mut self, name: &str, accepts: impl Fn(&MachineDef<M>, Label<M>) -> bool) -> Result<&mut Resource<M>, WriteError<M>> {
        let resource = self.resources.get_mut(name).ok_or_else(|| WriteError::Unknown(name.to_string()))?;
        let label = resource.state.label();
        if let Some(reason) = self.definition.end_reason(label) {
            return Err(WriteError::Refused(Ended { state: label, reason }.into()));
        }
        if !accepts(&self.definition, label) {
            return Err(WriteError::Refused(WrongState { state: label, edge: None }.into()));
        }
        Ok(resource)
    }

    /// Runs a judgement over one resource; see [`Resource::judge`].
    pub fn judge<T>(
        &self,
        name: &str,
        world: &M::World,
        now: Time,
        judgement: impl FnOnce(&mut Handle<'_, M>) -> T,
    ) -> Result<Judged<T>, WriteError<M>> {
        let resource = self.get(name).ok_or_else(|| WriteError::Unknown(name.to_string()))?;
        Ok(resource.judge(world, now, judgement))
    }
}
