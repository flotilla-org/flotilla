//! Declaring a machine.
//!
//! A machine is a marker type implementing [`Machine`]. Its states are a unit
//! enum, its edges a data-carrying enum (the typed arguments), and its shape is
//! a [`MachineDef`] built once: edges with from-states, a target (fixed or
//! chosen), a mandatory gate, and end states with a visible reason.
//!
//! [`MachineDef::validate`] enforces the rules every machine follows (naming,
//! ends are ends, no dead states, no ungated edges) and
//! [`MachineDef::mermaid`] renders the legibility-test diagram from the same
//! declaration the engine enforces.

use std::collections::BTreeSet;
use std::fmt::Debug;
use std::hash::Hash;

use crate::attempt::AttemptPolicy;
use crate::home::Resource;
use crate::view::Gate;

pub trait Named {
    fn name(&self) -> &'static str;
}

pub trait StateSet: Copy + Eq + Hash + Debug + Named + 'static {
    fn all() -> &'static [Self];
}

/// Why a gate said no. Visible to the caller and in the refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateFailure(pub String);

impl From<&str> for GateFailure {
    fn from(s: &str) -> Self {
        GateFailure(s.to_string())
    }
}

impl From<String> for GateFailure {
    fn from(s: String) -> Self {
        GateFailure(s)
    }
}

/// A gate sees the resource (spec, state, records) and the home's view through
/// the recording [`Gate`] handle, and the edge being attempted.
pub type GateFn<M> = fn(&Gate<'_>, &Resource<M>, &<M as Machine>::Edge) -> Result<(), GateFailure>;

/// `Clone + Debug` are supertraits only so that generic structs can derive.
pub trait Machine: Clone + Debug + Sized + 'static {
    const NAME: &'static str;
    type Spec: Clone + Debug;
    type State: StateSet;
    /// Edges with their typed arguments, one variant per edge.
    type Edge: Clone + Debug + Named;

    fn initial() -> Self::State;
    fn definition() -> MachineDef<Self>;
    /// Policy for the resource's attempts, if it has any.
    fn attempt_policy() -> Option<AttemptPolicy> {
        None
    }
}

pub enum Target<M: Machine> {
    To(M::State),
    /// The target depends on the spec and the edge's arguments. `may_reach`
    /// is declared so the diagram and the validator can see every outcome.
    Chosen {
        may_reach: Vec<M::State>,
        pick: fn(&M::Spec, &M::Edge) -> M::State,
    },
}

pub struct EdgeDef<M: Machine> {
    pub name: &'static str,
    pub from: Vec<M::State>,
    pub target: Option<Target<M>>,
    pub gate: Option<(&'static str, GateFn<M>)>,
}

impl<M: Machine> EdgeDef<M> {
    pub fn new(name: &'static str) -> Self {
        EdgeDef { name, from: Vec::new(), target: None, gate: None }
    }

    pub fn from(mut self, states: impl IntoIterator<Item = M::State>) -> Self {
        self.from.extend(states);
        self
    }

    pub fn to(mut self, state: M::State) -> Self {
        self.target = Some(Target::To(state));
        self
    }

    pub fn to_chosen(mut self, may_reach: Vec<M::State>, pick: fn(&M::Spec, &M::Edge) -> M::State) -> Self {
        self.target = Some(Target::Chosen { may_reach, pick });
        self
    }

    /// `doc` is the human sentence shown beside the edge; `gate` enforces it.
    pub fn gate(mut self, doc: &'static str, gate: GateFn<M>) -> Self {
        self.gate = Some((doc, gate));
        self
    }

    pub fn reachable_targets(&self) -> Vec<M::State> {
        match &self.target {
            Some(Target::To(s)) => vec![*s],
            Some(Target::Chosen { may_reach, .. }) => may_reach.clone(),
            None => Vec::new(),
        }
    }
}

pub struct EndDef<M: Machine> {
    pub state: M::State,
    /// Shown to anyone whose late write is refused.
    pub reason: &'static str,
}

pub struct MachineDef<M: Machine> {
    pub edges: Vec<EdgeDef<M>>,
    pub ends: Vec<EndDef<M>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefError {
    DuplicateEdge(&'static str),
    NameClash(&'static str),
    UngatedEdge(&'static str),
    EdgeWithoutTarget(&'static str),
    EdgeWithoutSource(&'static str),
    EdgeLeavesEnd {
        edge: &'static str,
        end: &'static str,
    },
    EndWithoutReason(&'static str),
    UnreachableState(&'static str),
    /// Not an end, yet nothing leaves it: declare it an end or give it an edge.
    DeadState(&'static str),
}

impl<M: Machine> Default for MachineDef<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: Machine> MachineDef<M> {
    pub fn new() -> Self {
        MachineDef { edges: Vec::new(), ends: Vec::new() }
    }

    pub fn edge(mut self, edge: EdgeDef<M>) -> Self {
        self.edges.push(edge);
        self
    }

    pub fn end(mut self, state: M::State, reason: &'static str) -> Self {
        self.ends.push(EndDef { state, reason });
        self
    }

    pub fn find_edge(&self, name: &str) -> Option<&EdgeDef<M>> {
        self.edges.iter().find(|e| e.name == name)
    }

    pub fn end_reason(&self, state: M::State) -> Option<&'static str> {
        self.ends.iter().find(|e| e.state == state).map(|e| e.reason)
    }

    pub fn validate(&self) -> Result<(), Vec<DefError>> {
        let mut errs = Vec::new();
        let mut seen = BTreeSet::new();
        for e in &self.edges {
            if !seen.insert(e.name) {
                errs.push(DefError::DuplicateEdge(e.name));
            }
            if M::State::all().iter().any(|s| s.name().eq_ignore_ascii_case(e.name)) {
                errs.push(DefError::NameClash(e.name));
            }
            if e.gate.is_none() {
                errs.push(DefError::UngatedEdge(e.name));
            }
            if e.target.is_none() {
                errs.push(DefError::EdgeWithoutTarget(e.name));
            }
            if e.from.is_empty() {
                errs.push(DefError::EdgeWithoutSource(e.name));
            }
            for end in &self.ends {
                if e.from.contains(&end.state) {
                    errs.push(DefError::EdgeLeavesEnd { edge: e.name, end: end.state.name() });
                }
            }
        }
        for end in &self.ends {
            if end.reason.trim().is_empty() {
                errs.push(DefError::EndWithoutReason(end.state.name()));
            }
        }
        // Reachability from the initial state.
        let mut reached = BTreeSet::new();
        let mut frontier = vec![M::initial()];
        while let Some(s) = frontier.pop() {
            if !reached.insert(s.name()) {
                continue;
            }
            for e in self.edges.iter().filter(|e| e.from.contains(&s)) {
                frontier.extend(e.reachable_targets());
            }
        }
        for s in M::State::all() {
            if !reached.contains(s.name()) {
                errs.push(DefError::UnreachableState(s.name()));
            }
            let is_end = self.ends.iter().any(|e| e.state == *s);
            if !is_end && !self.edges.iter().any(|e| e.from.contains(s)) {
                errs.push(DefError::DeadState(s.name()));
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    /// The legibility test: the declaration drawn as a diagram.
    pub fn mermaid(&self) -> String {
        let mut out = String::from("stateDiagram-v2\n  direction LR\n");
        out.push_str(&format!("  [*] --> {}\n", M::initial().name()));
        for e in &self.edges {
            let chosen = matches!(e.target, Some(Target::Chosen { .. }));
            for from in &e.from {
                for to in e.reachable_targets() {
                    let label = if chosen { format!("{} (target chosen)", e.name) } else { e.name.to_string() };
                    out.push_str(&format!("  {} --> {}: {}\n", from.name(), to.name(), label));
                }
            }
        }
        for end in &self.ends {
            out.push_str(&format!("  {} --> [*]\n", end.state.name()));
        }
        out
    }

    /// Edge-by-edge gate documentation, as a GUI would show beside the diagram.
    pub fn gate_docs(&self) -> Vec<(&'static str, &'static str)> {
        self.edges.iter().filter_map(|e| e.gate.map(|(doc, _)| (e.name, doc))).collect()
    }
}
