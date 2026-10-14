//! The home: the only writer of its resources, applying edges through gates.
//!
//! This is the in-memory stand-in for the writer transaction. In the real
//! store the same steps run inside one SQLite transaction and the
//! [`EdgeEvent`] also lands in the per-origin change log.

use std::collections::BTreeMap;

use crate::attempt::{Attempt, Outcome};
use crate::base::{ObjRef, Time};
use crate::machine::{DefError, GateFailure, Machine, MachineDef, Named, Target};
use crate::record::{Column, ColumnType, Record, RecordKey, RecordLog};
use crate::view::{Gate, HomeView, InputRead};

/// A recorded transition: the typed edge, where it went, and what the gate relied on.
#[derive(Clone, Debug)]
pub struct EdgeEvent<M: Machine> {
    pub edge: M::Edge,
    pub from: M::State,
    pub to: M::State,
    /// References and versions the gate read.
    pub inputs: Vec<InputRead>,
    pub writer: String,
    pub at: Time,
}

// What the derive would generate: `edge` is a tagged enum (tag plus the sparse
// columns of the Edge enum's own derive), `inputs` a child collection.
impl<M: Machine> Record for EdgeEvent<M> {
    const TABLE: &'static str = "edge_events";
    const COLUMNS: &'static [Column] = &[
        Column { name: "edge", ty: ColumnType::Tag },
        Column { name: "from_state", ty: ColumnType::Tag },
        Column { name: "to_state", ty: ColumnType::Tag },
        Column { name: "writer", ty: ColumnType::Ref },
        Column { name: "at", ty: ColumnType::Time },
        Column { name: "inputs", ty: ColumnType::Child("edge_event_inputs") },
    ];
}

/// A resource: spec, current state (materialised from the event log), and the
/// records it owns. There is no field for a judgement.
#[derive(Clone, Debug)]
pub struct Resource<M: Machine> {
    pub id: ObjRef,
    pub spec: M::Spec,
    state: M::State,
    version: u64,
    pub events: RecordLog<EdgeEvent<M>>,
    pub attempts: RecordLog<Attempt>,
}

impl<M: Machine> Resource<M> {
    pub fn state(&self) -> M::State {
        self.state
    }

    /// Bumped by every edge and every record appended.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The most recent event of a named edge, for payloads that live in edge arguments.
    pub fn last_event(&self, edge: &str) -> Option<&EdgeEvent<M>> {
        self.events.rows().iter().rev().find(|e| e.edge.name() == edge)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    NoSuchResource(ObjRef),
    AlreadyExists(ObjRef),
    /// The resource is at an end; nothing leaves it. `reason` is the end's declared reason.
    End {
        state: &'static str,
        attempted: String,
        reason: &'static str,
    },
    WrongState {
        edge: &'static str,
        state: &'static str,
        allowed: Vec<&'static str>,
    },
    Gate {
        edge: &'static str,
        why: GateFailure,
    },
    /// The declared `pick` returned a state outside its `may_reach`.
    BadTarget {
        edge: &'static str,
        picked: &'static str,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NoSuchResource(r) => write!(f, "no such resource {r}"),
            Refusal::AlreadyExists(r) => write!(f, "{r} already exists"),
            Refusal::End { state, attempted, reason } => write!(f, "refused {attempted}: resource is {state}, an end state ({reason})"),
            Refusal::WrongState { edge, state, allowed } => {
                write!(f, "refused {edge}: not allowed from {state} (allowed from {})", allowed.join(", "))
            }
            Refusal::Gate { edge, why } => write!(f, "refused {edge}: gate failed: {}", why.0),
            Refusal::BadTarget { edge, picked } => write!(f, "edge {edge} picked undeclared target {picked}"),
        }
    }
}

pub struct Home<M: Machine> {
    def: MachineDef<M>,
    host: String,
    resources: BTreeMap<ObjRef, Resource<M>>,
}

impl<M: Machine> Home<M> {
    /// Refuses to start on a declaration that breaks the machine rules.
    pub fn new(host: impl Into<String>) -> Result<Self, Vec<DefError>> {
        let def = M::definition();
        def.validate()?;
        Ok(Home { def, host: host.into(), resources: BTreeMap::new() })
    }

    pub fn definition(&self) -> &MachineDef<M> {
        &self.def
    }

    pub fn create(&mut self, name: &str, spec: M::Spec) -> Result<ObjRef, Refusal> {
        let id = ObjRef::new(M::NAME, name);
        if self.resources.contains_key(&id) {
            return Err(Refusal::AlreadyExists(id));
        }
        let r = Resource {
            id: id.clone(),
            spec,
            state: M::initial(),
            version: 1,
            events: RecordLog::new(id.clone()),
            attempts: RecordLog::new(id.clone()),
        };
        self.resources.insert(id.clone(), r);
        Ok(id)
    }

    pub fn get(&self, id: &ObjRef) -> Option<&Resource<M>> {
        self.resources.get(id)
    }

    /// Apply an edge: end check, from-state check, gate, target, then record.
    pub fn apply(&mut self, id: &ObjRef, edge: M::Edge, view: &dyn HomeView) -> Result<&EdgeEvent<M>, Refusal> {
        let res = self.resources.get(id).ok_or_else(|| Refusal::NoSuchResource(id.clone()))?;
        self.refuse_if_ended(res, edge.name())?;
        let def = self.def.find_edge(edge.name()).expect("every Edge variant is declared; validated names");
        if !def.from.contains(&res.state) {
            return Err(Refusal::WrongState {
                edge: def.name,
                state: res.state.name(),
                allowed: def.from.iter().map(|s| s.name()).collect(),
            });
        }
        let gate = Gate::new(view);
        let (_, gate_fn) = def.gate.expect("validated: every edge is gated");
        gate_fn(&gate, res, &edge).map_err(|why| Refusal::Gate { edge: def.name, why })?;
        let to = match def.target.as_ref().expect("validated: every edge has a target") {
            Target::To(s) => *s,
            Target::Chosen { may_reach, pick } => {
                let picked = pick(&res.spec, &edge);
                if !may_reach.contains(&picked) {
                    return Err(Refusal::BadTarget { edge: def.name, picked: picked.name() });
                }
                picked
            }
        };
        let event = EdgeEvent { edge, from: res.state, to, inputs: gate.into_reads(), writer: self.host.clone(), at: view.now() };
        let res = self.resources.get_mut(id).expect("checked above");
        res.state = to;
        res.version += 1;
        let key = res.events.append(event);
        Ok(res.events.get(key.position).expect("just appended"))
    }

    /// Append an attempt record. An ended resource is closed to records too.
    pub fn record_attempt(
        &mut self,
        id: &ObjRef,
        what: &str,
        target: &str,
        outcome: Outcome,
        view: &dyn HomeView,
    ) -> Result<RecordKey, Refusal> {
        let res = self.resources.get(id).ok_or_else(|| Refusal::NoSuchResource(id.clone()))?;
        self.refuse_if_ended(res, "record attempt")?;
        let res = self.resources.get_mut(id).expect("checked above");
        res.version += 1;
        Ok(res.attempts.append(Attempt { what: what.into(), target: target.into(), at: view.now(), outcome }))
    }

    fn refuse_if_ended(&self, res: &Resource<M>, attempted: &str) -> Result<(), Refusal> {
        match self.def.end_reason(res.state) {
            Some(reason) => Err(Refusal::End { state: res.state.name(), attempted: attempted.to_string(), reason }),
            None => Ok(()),
        }
    }
}
