//! Declaring a machine and taking one pure step.
//!
//! A [`MachineDef`] is built once from [`EdgeDef`]s. Each edge is typed on its
//! argument struct: its gate and any chosen target receive `&Deliver`, never
//! the whole edge enum. The definition stores them behind the edge's kind, so
//! [`MachineDef::step`] finds the declaration from the edge's kind and passes
//! the argument the declaration was typed on.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Debug},
    marker::PhantomData,
};

use crate::{Coded, Edge, EdgeArg, Handle};

/// A declared machine: a marker type naming its spec, states, edges,
/// refusal and readable world.
pub trait Machine: Sized + 'static {
    /// The machine's name in diagrams and errors.
    const NAME: &'static str;
    /// What a caller asks for when it creates a resource. Immutable.
    type Spec;
    type State: Coded;
    type Edge: Edge;
    /// Why a write was refused. Engine-detected refusals (an end, the wrong
    /// state) convert into it, so a caller sees one type per machine, with the
    /// same variants as the machine's specification.
    type Refusal: Clone + Debug + PartialEq + From<Ended<Self::State>> + From<WrongState<Self::State, EdgeKind<Self>>>;
    /// What gates, targets and judgements may read; usually a trait object
    /// whose supertraits are the [`Source`](crate::Source)s the machine needs.
    type World: ?Sized;

    fn definition() -> MachineDef<Self>;

    /// Admission: whether a resource with this spec may be created at all.
    fn admit(_spec: &Self::Spec) -> Result<(), Self::Refusal> {
        Ok(())
    }
}

/// The kind enum of machine `M`'s edges.
pub type EdgeKind<M> = <<M as Machine>::Edge as Edge>::Kind;

/// A write reached a resource in an end state. Carries the end's reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ended<S> {
    pub state: S,
    pub reason: &'static str,
}

/// An edge was attempted from a state it does not leave. `edge` is `None`
/// for a record that the state does not accept, such as an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrongState<S, K> {
    pub state: S,
    pub edge: Option<K>,
}

type GateFn<M> = dyn Fn(&mut Handle<'_, M>, &<M as Machine>::Edge) -> Result<(), <M as Machine>::Refusal>;
type PickFn<M> = dyn Fn(&mut Handle<'_, M>, &<M as Machine>::Edge) -> Option<<M as Machine>::State>;

/// Where an edge goes: one fixed state, or a state chosen among a declared set.
pub enum Target<M: Machine> {
    To(M::State),
    /// `may_reach` is declared so validation and the diagram see every
    /// outcome; the engine refuses a pick outside it.
    Chosen {
        may_reach: Vec<M::State>,
        pick: Box<PickFn<M>>,
    },
}

/// One edge under construction, typed on its argument struct `A`.
pub struct EdgeDef<M: Machine, A> {
    from: Vec<M::State>,
    target: Option<Target<M>>,
    gate: Option<(&'static str, Box<GateFn<M>>)>,
    arg: PhantomData<fn(&A)>,
}

impl<M: Machine, A: EdgeArg<Edge = M::Edge>> EdgeDef<M, A> {
    pub fn new() -> Self {
        EdgeDef { from: Vec::new(), target: None, gate: None, arg: PhantomData }
    }

    /// States this edge leaves. May be called more than once.
    pub fn from(mut self, states: impl IntoIterator<Item = M::State>) -> Self {
        self.from.extend(states);
        self
    }

    pub fn to(mut self, state: M::State) -> Self {
        self.target = Some(Target::To(state));
        self
    }

    /// The target is chosen within `may_reach` by `pick`, which reads through
    /// the handle like a gate.
    pub fn to_chosen(mut self, may_reach: impl IntoIterator<Item = M::State>, pick: fn(&mut Handle<'_, M>, &A) -> M::State) -> Self {
        let pick = move |handle: &mut Handle<'_, M>, edge: &M::Edge| A::project(edge).map(|arg| pick(handle, arg));
        self.target = Some(Target::Chosen { may_reach: may_reach.into_iter().collect(), pick: Box::new(pick) });
        self
    }

    /// `doc` is the sentence shown beside the edge; `gate` enforces it.
    pub fn gate(mut self, doc: &'static str, gate: fn(&mut Handle<'_, M>, &A) -> Result<(), M::Refusal>) -> Self {
        let gate = move |handle: &mut Handle<'_, M>, edge: &M::Edge| match A::project(edge) {
            Some(arg) => gate(handle, arg),
            // Unreachable by construction: definitions are looked up by kind.
            None => Err(WrongState { state: handle.state(), edge: Some(edge.kind()) }.into()),
        };
        self.gate = Some((doc, Box::new(gate)));
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
    pub from: Vec<M::State>,
    pub target: Option<Target<M>>,
    gate: Option<(&'static str, Box<GateFn<M>>)>,
}

impl<M: Machine> DeclaredEdge<M> {
    /// Every state this edge may reach.
    pub fn targets(&self) -> Vec<M::State> {
        match &self.target {
            Some(Target::To(state)) => vec![*state],
            Some(Target::Chosen { may_reach, .. }) => may_reach.clone(),
            None => Vec::new(),
        }
    }

    pub fn gate_doc(&self) -> Option<&'static str> {
        self.gate.as_ref().map(|(doc, _)| *doc)
    }
}

/// An end state and the reason shown to anyone whose late write is refused.
pub struct EndDef<S> {
    pub state: S,
    pub reason: &'static str,
}

/// A machine's declaration: its initial state, edges and ends.
pub struct MachineDef<M: Machine> {
    initial: M::State,
    edges: Vec<DeclaredEdge<M>>,
    ends: Vec<EndDef<M::State>>,
    by_kind: BTreeMap<EdgeKind<M>, usize>,
}

/// A rule a declaration breaks.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DefError {
    /// An edge kind declared twice.
    DuplicateEdge(&'static str),
    /// An edge kind of the edge enum with no declaration.
    UndeclaredEdge(&'static str),
    /// A state and an edge share a word (case and underscores ignored).
    NameClash(&'static str),
    UngatedEdge(&'static str),
    EdgeWithoutTarget(&'static str),
    EdgeWithoutSource(&'static str),
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

/// Why a pure step did not move.
pub enum StepError<M: Machine> {
    /// The machine refused: an end, the wrong state, or a gate.
    Refused(M::Refusal),
    /// A declaration fault: a chosen target outside `may_reach`, or an edge
    /// kind with no declaration. [`MachineDef::validate`] catches the latter.
    Undeclared { edge: EdgeKind<M>, target: Option<M::State> },
}

impl<M: Machine> Clone for StepError<M> {
    fn clone(&self) -> Self {
        match self {
            StepError::Refused(refusal) => StepError::Refused(refusal.clone()),
            StepError::Undeclared { edge, target } => StepError::Undeclared { edge: *edge, target: *target },
        }
    }
}

impl<M: Machine> PartialEq for StepError<M> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (StepError::Refused(a), StepError::Refused(b)) => a == b,
            (StepError::Undeclared { edge: a, target: x }, StepError::Undeclared { edge: b, target: y }) => a == b && x == y,
            _ => false,
        }
    }
}

impl<M: Machine> Debug for StepError<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StepError::Refused(refusal) => f.debug_tuple("Refused").field(refusal).finish(),
            StepError::Undeclared { edge, target } => f.debug_struct("Undeclared").field("edge", edge).field("target", target).finish(),
        }
    }
}

impl<M: Machine> MachineDef<M> {
    pub fn new(initial: M::State) -> Self {
        MachineDef { initial, edges: Vec::new(), ends: Vec::new(), by_kind: BTreeMap::new() }
    }

    /// Adds an edge, typed on its argument struct.
    pub fn edge<A: EdgeArg<Edge = M::Edge>>(mut self, edge: EdgeDef<M, A>) -> Self {
        self.by_kind.entry(A::KIND).or_insert(self.edges.len());
        self.edges.push(DeclaredEdge { kind: A::KIND, from: edge.from, target: edge.target, gate: edge.gate });
        self
    }

    pub fn end(mut self, state: M::State, reason: &'static str) -> Self {
        self.ends.push(EndDef { state, reason });
        self
    }

    pub fn initial(&self) -> M::State {
        self.initial
    }

    pub fn edges(&self) -> &[DeclaredEdge<M>] {
        &self.edges
    }

    pub fn ends(&self) -> &[EndDef<M::State>] {
        &self.ends
    }

    pub fn find(&self, kind: EdgeKind<M>) -> Option<&DeclaredEdge<M>> {
        self.by_kind.get(&kind).map(|index| &self.edges[*index])
    }

    /// The end's reason when `state` is an end.
    pub fn end_reason(&self, state: M::State) -> Option<&'static str> {
        self.ends.iter().find(|end| end.state == state).map(|end| end.reason)
    }

    /// Checks every declaration rule and reports all violations, sorted.
    pub fn validate(&self) -> Result<(), Vec<DefError>> {
        let mut errors = BTreeSet::new();
        let squash = |name: &str| name.replace('_', "").to_ascii_lowercase();
        let state_words: BTreeSet<String> = M::State::ALL.iter().map(|state| squash(state.name())).collect();
        let mut declared = BTreeSet::new();
        for edge in &self.edges {
            let name = edge.kind.name();
            if !declared.insert(edge.kind) {
                errors.insert(DefError::DuplicateEdge(name));
            }
            if state_words.contains(&squash(name)) {
                errors.insert(DefError::NameClash(name));
            }
            if edge.gate.is_none() {
                errors.insert(DefError::UngatedEdge(name));
            }
            if edge.target.is_none() {
                errors.insert(DefError::EdgeWithoutTarget(name));
            }
            if edge.from.is_empty() {
                errors.insert(DefError::EdgeWithoutSource(name));
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
        while let Some(state) = frontier.pop() {
            if reached.insert(state) {
                frontier.extend(self.edges.iter().filter(|edge| edge.from.contains(&state)).flat_map(DeclaredEdge::targets));
            }
        }
        for state in M::State::ALL {
            if !reached.contains(state) {
                errors.insert(DefError::UnreachableState(state.name()));
            }
            if !ends.contains(state) && !self.edges.iter().any(|edge| edge.from.contains(state)) {
                errors.insert(DefError::DeadState(state.name()));
            }
        }
        let mut codes = BTreeSet::new();
        for state in M::State::ALL {
            if !codes.insert(state.code()) {
                errors.insert(DefError::DuplicateStateCode(state.code()));
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

    /// The legibility test: the declaration drawn as a Mermaid state diagram,
    /// with each edge's gate sentence as a note.
    pub fn mermaid(&self) -> String {
        let mut out = format!("---\ntitle: {}\n---\nstateDiagram-v2\n  direction LR\n  [*] --> {}\n", M::NAME, self.initial.name());
        for edge in &self.edges {
            let chosen = matches!(edge.target, Some(Target::Chosen { .. }));
            for from in &edge.from {
                for to in edge.targets() {
                    let suffix = if chosen { " (chosen)" } else { "" };
                    out.push_str(&format!("  {} --> {}: {}{}\n", from.name(), to.name(), edge.kind.name(), suffix));
                }
            }
        }
        for end in &self.ends {
            out.push_str(&format!("  {} --> [*]\n  note right of {}: {}\n", end.state.name(), end.state.name(), end.reason));
        }
        out
    }

    /// One pure step, in the engine's fixed order: the end check (refusing
    /// with the end's reason), the from-state, the gate, then the target.
    /// Reads go through `handle`; on success they are the step's inputs.
    pub fn step(&self, handle: &mut Handle<'_, M>, edge: &M::Edge) -> Result<M::State, StepError<M>> {
        let state = handle.state();
        let kind = edge.kind();
        if let Some(reason) = self.end_reason(state) {
            return Err(StepError::Refused(Ended { state, reason }.into()));
        }
        let Some(declared) = self.find(kind) else {
            return Err(StepError::Undeclared { edge: kind, target: None });
        };
        if !declared.from.contains(&state) {
            return Err(StepError::Refused(WrongState { state, edge: Some(kind) }.into()));
        }
        let Some((_, gate)) = &declared.gate else {
            return Err(StepError::Undeclared { edge: kind, target: None });
        };
        gate(handle, edge).map_err(StepError::Refused)?;
        match &declared.target {
            Some(Target::To(target)) => Ok(*target),
            Some(Target::Chosen { may_reach, pick }) => match pick(handle, edge) {
                Some(target) if may_reach.contains(&target) => Ok(target),
                target => Err(StepError::Undeclared { edge: kind, target }),
            },
            None => Err(StepError::Undeclared { edge: kind, target: None }),
        }
    }
}
