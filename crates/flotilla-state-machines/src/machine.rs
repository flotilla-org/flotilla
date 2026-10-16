//! Declaring a machine and taking one pure step.
//!
//! A [`MachineDef`] is built once from [`EdgeDef`]s. Each edge names the
//! labels it leaves and may reach and the data it changes, and is typed on its
//! argument struct: its gate and its effect receive `&Deliver`, never the
//! whole edge enum. The gate reads the current state and the world through
//! the handle and refuses or allows; the effect produces the next state. The
//! definition stores them behind the edge's kind, so [`MachineDef::step`]
//! finds the declaration from the edge's kind and checks the effect's result
//! against it: the next label must be declared, and so must every datum the
//! edge changes.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Debug},
    marker::PhantomData,
};

use crate::{code::changed, Coded, Edge, EdgeArg, Handle, StateValue};

/// A declared machine: a marker type naming its request, spec, state, edges,
/// refusal and readable world.
pub trait Machine: Sized + 'static {
    /// The machine's name in diagrams and errors.
    const NAME: &'static str;
    /// What a caller asks for when it creates a resource.
    type Request;
    /// What never changes after admission.
    type Spec;
    /// The current state: a label plus its data.
    type State: StateValue;
    type Edge: Edge;
    /// Why a write was refused. Engine-detected refusals (an end, the wrong
    /// state) convert into it, so a caller sees one type per machine, with the
    /// same variants as the machine's specification.
    type Refusal: Clone + Debug + PartialEq + From<Ended<Label<Self>>> + From<WrongState<Label<Self>, EdgeKind<Self>>>;
    /// What gates, effects and judgements may read; usually a trait object
    /// whose supertraits are the [`Source`](crate::Source)s the machine needs.
    type World: ?Sized;

    fn definition() -> MachineDef<Self>;

    /// Admission: whether `request` may be created at all, and if so its
    /// immutable spec and initial state, whose label must be the declared one.
    fn admit(request: Self::Request) -> Result<(Self::Spec, Self::State), Self::Refusal>;
}

/// The label enum of machine `M`'s states.
pub type Label<M> = <<M as Machine>::State as StateValue>::Label;

/// The kind enum of machine `M`'s edges.
pub type EdgeKind<M> = <<M as Machine>::Edge as Edge>::Kind;

/// A write reached a resource in an end state. Carries the end's reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ended<L> {
    pub state: L,
    pub reason: &'static str,
}

/// An edge was attempted from a state it does not leave. `edge` is `None`
/// for a record that the state does not accept, such as an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrongState<L, K> {
    pub state: L,
    pub edge: Option<K>,
}

type GateFn<M> = dyn Fn(&mut Handle<'_, M>, &<M as Machine>::Edge) -> Option<Result<(), <M as Machine>::Refusal>>;
type EffectFn<M> = dyn Fn(&mut Handle<'_, M>, <M as Machine>::State, &<M as Machine>::Edge) -> Option<<M as Machine>::State>;

/// What an edge checks before it moves.
pub enum Check<M: Machine> {
    /// A gate and the sentence it enforces.
    Gate { doc: &'static str, gate: Box<GateFn<M>> },
    /// No gate, and the stated reason none is needed.
    Ungated { reason: &'static str },
}

/// One edge under construction, typed on its argument struct `A`.
pub struct EdgeDef<M: Machine, A> {
    from: Vec<Label<M>>,
    to: Vec<Label<M>>,
    changes: Vec<&'static str>,
    check: Option<Check<M>>,
    effect: Option<Box<EffectFn<M>>>,
    arg: PhantomData<fn(&A)>,
}

impl<M: Machine, A: EdgeArg<Edge = M::Edge>> EdgeDef<M, A> {
    pub fn new() -> Self {
        EdgeDef { from: Vec::new(), to: Vec::new(), changes: Vec::new(), check: None, effect: None, arg: PhantomData }
    }

    /// Labels this edge leaves. May be called more than once.
    pub fn from(mut self, labels: impl IntoIterator<Item = Label<M>>) -> Self {
        self.from.extend(labels);
        self
    }

    /// Labels this edge may reach. May be called more than once.
    pub fn to(mut self, labels: impl IntoIterator<Item = Label<M>>) -> Self {
        self.to.extend(labels);
        self
    }

    /// The data this edge changes: each datum of the next state that is new
    /// or holds another value. Changing an undeclared datum is a declaration
    /// fault, so an edge into a label that carries data names the data it
    /// writes there, even when the label changes too. A datum carried into
    /// the next label with the same value is not a change (`Armed { shots: 1 }`
    /// to `Spent { shots: 1 }`), and data the next label does not carry goes
    /// with the old label.
    pub fn changes(mut self, data: impl IntoIterator<Item = &'static str>) -> Self {
        self.changes.extend(data);
        self
    }

    /// `doc` is the sentence shown beside the edge; `gate` enforces it,
    /// reading the current state and the world through the handle.
    pub fn gate(mut self, doc: &'static str, gate: fn(&mut Handle<'_, M>, &A) -> Result<(), M::Refusal>) -> Self {
        // Definitions are looked up by kind, so the projection cannot miss. Debug
        // builds stop on a lookup bug; release builds report a declaration fault,
        // as the effect's projection does.
        let gate = move |handle: &mut Handle<'_, M>, edge: &M::Edge| {
            let arg = A::project(edge);
            debug_assert!(arg.is_some(), "edge {:?} reached the gate declared for {:?}", edge.kind(), A::KIND);
            arg.map(|arg| gate(handle, arg))
        };
        self.check = Some(Check::Gate { doc, gate: Box::new(gate) });
        self
    }

    /// Declares that the edge needs no gate, and why. [`MachineDef::validate`]
    /// refuses a blank reason, so an ungated edge is always a stated choice.
    pub fn ungated(mut self, reason: &'static str) -> Self {
        self.check = Some(Check::Ungated { reason });
        self
    }

    /// Produces the next state from the current one and the argument. It may
    /// read through the handle like a gate, and never refuses: the gate has.
    pub fn effect(mut self, effect: fn(&mut Handle<'_, M>, M::State, &A) -> M::State) -> Self {
        let effect = move |handle: &mut Handle<'_, M>, state: M::State, edge: &M::Edge| {
            let arg = A::project(edge);
            debug_assert!(arg.is_some(), "edge {:?} reached the effect declared for {:?}", edge.kind(), A::KIND);
            arg.map(|arg| effect(handle, state, arg))
        };
        self.effect = Some(Box::new(effect));
        self
    }
}

impl<M: Machine, A: EdgeArg<Edge = M::Edge>> Default for EdgeDef<M, A> {
    fn default() -> Self {
        Self::new()
    }
}

/// One declared edge, after its argument type has been checked.
pub struct DeclaredEdge<M: Machine> {
    pub kind: EdgeKind<M>,
    pub from: Vec<Label<M>>,
    pub to: Vec<Label<M>>,
    pub changes: Vec<&'static str>,
    check: Option<Check<M>>,
    effect: Option<Box<EffectFn<M>>>,
}

impl<M: Machine> DeclaredEdge<M> {
    /// The sentence the gate enforces, for a gated edge.
    pub fn gate_doc(&self) -> Option<&'static str> {
        match &self.check {
            Some(Check::Gate { doc, .. }) => Some(doc),
            _ => None,
        }
    }

    /// The stated reason, for an edge declared ungated.
    pub fn ungated_reason(&self) -> Option<&'static str> {
        match &self.check {
            Some(Check::Ungated { reason }) => Some(reason),
            _ => None,
        }
    }
}

/// An end state and the reason shown to anyone whose late write is refused.
pub struct EndDef<L> {
    pub state: L,
    pub reason: &'static str,
}

/// How [`MachineDef::mermaid`] draws a machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Diagram {
    /// Every edge, including those that change only data and keep the label.
    Full,
    /// Only edges that change the label.
    Labels,
}

/// A machine's declaration: its initial label, edges and ends.
pub struct MachineDef<M: Machine> {
    initial: Label<M>,
    edges: Vec<DeclaredEdge<M>>,
    ends: Vec<EndDef<Label<M>>>,
    by_kind: BTreeMap<EdgeKind<M>, usize>,
    attempts_in: Option<Vec<Label<M>>>,
}

/// A rule a declaration breaks.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DefError {
    /// An edge kind declared twice.
    DuplicateEdge(&'static str),
    /// An edge kind of the edge enum with no declaration.
    UndeclaredEdge(&'static str),
    /// A label and an edge share a word (case and underscores ignored).
    NameClash(&'static str),
    /// Neither a gate nor an ungated declaration.
    UngatedEdge(&'static str),
    /// Declared ungated with a blank reason.
    UngatedWithoutReason(&'static str),
    EdgeWithoutEffect(&'static str),
    EdgeWithoutTarget(&'static str),
    EdgeWithoutSource(&'static str),
    /// Neither changes data nor leaves its label: it could never alter the state.
    InertEdge(&'static str),
    /// A changed datum that no target label carries.
    UnknownData {
        edge: &'static str,
        data: &'static str,
    },
    EdgeLeavesEnd {
        edge: &'static str,
        end: &'static str,
    },
    EndWithoutReason(&'static str),
    DuplicateEnd(&'static str),
    UnreachableState(&'static str),
    /// Not an end, yet nothing leaves it.
    DeadState(&'static str),
    DuplicateStateCode(u16),
    DuplicateEdgeCode(u16),
}

impl fmt::Display for DefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

/// A declaration fault found while stepping: the engine refuses the write
/// rather than store a state its declaration does not allow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault<L, K> {
    /// An edge kind with no declaration, check or effect;
    /// [`MachineDef::validate`] catches these.
    Undeclared { edge: K },
    /// The effect produced a label outside the edge's declared targets.
    Target { edge: K, target: L },
    /// The effect changed data the edge does not declare.
    Changes { edge: K, data: Vec<&'static str> },
    /// Admission produced a label other than the declared initial one.
    Initial { state: L },
}

/// Why a pure step did not move.
pub enum StepError<M: Machine> {
    /// The machine refused: an end, the wrong state, or a gate.
    Refused(M::Refusal),
    Fault(Fault<Label<M>, EdgeKind<M>>),
}

impl<M: Machine> Clone for StepError<M> {
    fn clone(&self) -> Self {
        match self {
            StepError::Refused(refusal) => StepError::Refused(refusal.clone()),
            StepError::Fault(fault) => StepError::Fault(fault.clone()),
        }
    }
}

impl<M: Machine> PartialEq for StepError<M> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (StepError::Refused(a), StepError::Refused(b)) => a == b,
            (StepError::Fault(a), StepError::Fault(b)) => a == b,
            _ => false,
        }
    }
}

impl<M: Machine> Debug for StepError<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StepError::Refused(refusal) => f.debug_tuple("Refused").field(refusal).finish(),
            StepError::Fault(fault) => f.debug_tuple("Fault").field(fault).finish(),
        }
    }
}

impl<M: Machine> MachineDef<M> {
    pub fn new(initial: Label<M>) -> Self {
        MachineDef { initial, edges: Vec::new(), ends: Vec::new(), by_kind: BTreeMap::new(), attempts_in: None }
    }

    /// Adds an edge, typed on its argument struct. A second declaration of the
    /// same kind is a [`DefError::DuplicateEdge`]; [`Self::step`] assumes a
    /// definition that validates, which is why a home refuses one that does not.
    pub fn edge<A: EdgeArg<Edge = M::Edge>>(mut self, edge: EdgeDef<M, A>) -> Self {
        self.by_kind.entry(A::KIND).or_insert(self.edges.len());
        self.edges.push(DeclaredEdge {
            kind: A::KIND,
            from: edge.from,
            to: edge.to,
            changes: edge.changes,
            check: edge.check,
            effect: edge.effect,
        });
        self
    }

    pub fn end(mut self, state: Label<M>, reason: &'static str) -> Self {
        self.ends.push(EndDef { state, reason });
        self
    }

    /// Attempts may be recorded only under these labels. Without this
    /// declaration they may be recorded under any label but an end.
    pub fn attempts_in(mut self, labels: impl IntoIterator<Item = Label<M>>) -> Self {
        self.attempts_in = Some(labels.into_iter().collect());
        self
    }

    /// Whether an attempt may be recorded under `label` (ends aside).
    pub fn accepts_attempts(&self, label: Label<M>) -> bool {
        self.attempts_in.as_ref().is_none_or(|labels| labels.contains(&label))
    }

    pub fn initial(&self) -> Label<M> {
        self.initial
    }

    pub fn edges(&self) -> &[DeclaredEdge<M>] {
        &self.edges
    }

    pub fn ends(&self) -> &[EndDef<Label<M>>] {
        &self.ends
    }

    pub fn find(&self, kind: EdgeKind<M>) -> Option<&DeclaredEdge<M>> {
        self.by_kind.get(&kind).map(|index| &self.edges[*index])
    }

    /// The end's reason when `label` is an end.
    pub fn end_reason(&self, label: Label<M>) -> Option<&'static str> {
        self.ends.iter().find(|end| end.state == label).map(|end| end.reason)
    }

    /// Checks every declaration rule and reports all violations, sorted.
    pub fn validate(&self) -> Result<(), Vec<DefError>> {
        let mut errors = BTreeSet::new();
        let squash = |name: &str| name.replace('_', "").to_ascii_lowercase();
        let label_words: BTreeSet<String> = Label::<M>::ALL.iter().map(|label| squash(label.name())).collect();
        let mut declared = BTreeSet::new();
        for edge in &self.edges {
            let name = edge.kind.name();
            if !declared.insert(edge.kind) {
                errors.insert(DefError::DuplicateEdge(name));
            }
            if label_words.contains(&squash(name)) {
                errors.insert(DefError::NameClash(name));
            }
            match &edge.check {
                None => {
                    errors.insert(DefError::UngatedEdge(name));
                }
                Some(Check::Ungated { reason }) if reason.trim().is_empty() => {
                    errors.insert(DefError::UngatedWithoutReason(name));
                }
                Some(_) => {}
            }
            if edge.effect.is_none() {
                errors.insert(DefError::EdgeWithoutEffect(name));
            }
            if edge.to.is_empty() {
                errors.insert(DefError::EdgeWithoutTarget(name));
            }
            if edge.from.is_empty() {
                errors.insert(DefError::EdgeWithoutSource(name));
            }
            if edge.changes.is_empty()
                && !edge.from.is_empty()
                && !edge.to.is_empty()
                && edge.from.iter().all(|from| edge.to.iter().all(|to| from == to))
            {
                errors.insert(DefError::InertEdge(name));
            }
            for data in &edge.changes {
                if !edge.to.iter().any(|label| M::State::fields(*label).contains(data)) {
                    errors.insert(DefError::UnknownData { edge: name, data });
                }
            }
            for end in &self.ends {
                if edge.from.contains(&end.state) {
                    errors.insert(DefError::EdgeLeavesEnd { edge: name, end: end.state.name() });
                }
            }
        }
        for kind in EdgeKind::<M>::ALL {
            if !declared.contains(kind) {
                errors.insert(DefError::UndeclaredEdge(kind.name()));
            }
        }
        let mut ends = BTreeSet::new();
        for end in &self.ends {
            if !ends.insert(end.state) {
                errors.insert(DefError::DuplicateEnd(end.state.name()));
            }
            if end.reason.trim().is_empty() {
                errors.insert(DefError::EndWithoutReason(end.state.name()));
            }
        }
        let mut reached = BTreeSet::new();
        let mut frontier = vec![self.initial];
        while let Some(label) = frontier.pop() {
            if reached.insert(label) {
                frontier.extend(self.edges.iter().filter(|edge| edge.from.contains(&label)).flat_map(|edge| edge.to.iter().copied()));
            }
        }
        for label in Label::<M>::ALL {
            if !reached.contains(label) {
                errors.insert(DefError::UnreachableState(label.name()));
            }
            if !ends.contains(label) && !self.edges.iter().any(|edge| edge.from.contains(label)) {
                errors.insert(DefError::DeadState(label.name()));
            }
        }
        let mut codes = BTreeSet::new();
        for label in Label::<M>::ALL {
            if !codes.insert(label.code()) {
                errors.insert(DefError::DuplicateStateCode(label.code()));
            }
        }
        let mut codes = BTreeSet::new();
        for kind in EdgeKind::<M>::ALL {
            if !codes.insert(kind.code()) {
                errors.insert(DefError::DuplicateEdgeCode(kind.code()));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.into_iter().collect())
        }
    }

    /// The legibility test: the declaration drawn as a Mermaid state diagram.
    /// Each transition names its edge and the data it changes, and each end
    /// carries its reason. [`Diagram::Labels`] leaves out the edges that keep
    /// their label and change only data.
    pub fn mermaid(&self, diagram: Diagram) -> String {
        let mut out = format!("---\ntitle: {}\n---\nstateDiagram-v2\n  direction LR\n  [*] --> {}\n", M::NAME, self.initial.name());
        for edge in &self.edges {
            let changes = if edge.changes.is_empty() { String::new() } else { format!(" [{}]", edge.changes.join(", ")) };
            for from in &edge.from {
                for to in &edge.to {
                    if diagram == Diagram::Labels && from == to {
                        continue;
                    }
                    out.push_str(&format!("  {} --> {}: {}{}\n", from.name(), to.name(), edge.kind.name(), changes));
                }
            }
        }
        for end in &self.ends {
            out.push_str(&format!("  {} --> [*]\n  note right of {}: {}\n", end.state.name(), end.state.name(), end.reason));
        }
        out
    }

    /// One pure step, for a definition that [validates](Self::validate), in
    /// the engine's fixed order: the end check (refusing with the end's
    /// reason), the from-label, the gate, then the effect, whose next state
    /// must have a declared label and change only declared data. Reads go
    /// through `handle`; on success they are the step's inputs.
    pub fn step(&self, handle: &mut Handle<'_, M>, edge: &M::Edge) -> Result<M::State, StepError<M>> {
        let state = handle.state();
        let label = state.label();
        let kind = edge.kind();
        if let Some(reason) = self.end_reason(label) {
            return Err(StepError::Refused(Ended { state: label, reason }.into()));
        }
        let undeclared = || StepError::Fault(Fault::Undeclared { edge: kind });
        let declared = self.find(kind).ok_or_else(undeclared)?;
        if !declared.from.contains(&label) {
            return Err(StepError::Refused(WrongState { state: label, edge: Some(kind) }.into()));
        }
        match &declared.check {
            Some(Check::Gate { gate, .. }) => gate(handle, edge).ok_or_else(undeclared)?.map_err(StepError::Refused)?,
            Some(Check::Ungated { .. }) => {}
            None => return Err(undeclared()),
        }
        let effect = declared.effect.as_ref().ok_or_else(undeclared)?;
        let next = effect(handle, state.clone(), edge).ok_or_else(undeclared)?;
        if !declared.to.contains(&next.label()) {
            return Err(StepError::Fault(Fault::Target { edge: kind, target: next.label() }));
        }
        let stray: Vec<&'static str> = changed(state, &next).into_iter().filter(|data| !declared.changes.contains(data)).collect();
        if !stray.is_empty() {
            return Err(StepError::Fault(Fault::Changes { edge: kind, data: stray }));
        }
        Ok(next)
    }
}
