//! Ensure's edges. Fulfiller creation/release are owner reactions, not extra edges.
use crate::attempt::{retry, AttemptPolicy};
use crate::base::ObjRef;
use crate::home::Resource;
use crate::machine::{EdgeDef, GateFailure, Machine, MachineDef, Named, StateSet};
use crate::view::{Fact, Gate};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EnsureState {
    Unfulfilled,
    Fulfilled,
    Held,
    Retired,
}
impl Named for EnsureState {
    fn name(&self) -> &'static str {
        match self {
            Self::Unfulfilled => "Unfulfilled",
            Self::Fulfilled => "Fulfilled",
            Self::Held => "Held",
            Self::Retired => "Retired",
        }
    }
}
impl StateSet for EnsureState {
    fn all() -> &'static [Self] {
        &[Self::Unfulfilled, Self::Fulfilled, Self::Held, Self::Retired]
    }
}
#[derive(Clone, Debug)]
pub struct EnsureSpec {
    pub policy: AttemptPolicy,
}
#[derive(Clone, Debug)]
pub enum UnbindReason {
    Gone,
    Replaced,
}
#[derive(Clone, Debug)]
pub enum EnsureEdge {
    Bind { fulfiller: ObjRef, recorded: ObjRef },
    Unbind { reason: UnbindReason, observed: ObjRef },
    GiveUp,
    Resume { by: String, authority: ObjRef },
    Retire { by: String, authority: ObjRef },
}
impl Named for EnsureEdge {
    fn name(&self) -> &'static str {
        match self {
            Self::Bind { .. } => "bind",
            Self::Unbind { .. } => "unbind",
            Self::GiveUp => "give up",
            Self::Resume { .. } => "resume",
            Self::Retire { .. } => "retire",
        }
    }
}
#[derive(Clone, Debug)]
pub struct Ensure;
fn gate_bind(g: &Gate<'_>, _: &Resource<Ensure>, edge: &EnsureEdge) -> Result<(), GateFailure> {
    let EnsureEdge::Bind { fulfiller, recorded } = edge else { unreachable!() };
    let is_recorded = matches!(g.observe(recorded).map(|o| o.fact), Some(Fact::FulfillerRecorded { fulfiller: f }) if f == *fulfiller);
    let ready = matches!(g.observe(fulfiller).map(|o| o.fact), Some(Fact::Fulfiller { realised: true, can_receive: true, .. }));
    if is_recorded && ready {
        Ok(())
    } else {
        Err("fulfiller must be recorded, Realised and able to receive".into())
    }
}
fn gate_unbind(g: &Gate<'_>, r: &Resource<Ensure>, edge: &EnsureEdge) -> Result<(), GateFailure> {
    let EnsureEdge::Unbind { observed, .. } = edge else { unreachable!() };
    let bound = r.last_event("bind").and_then(|event| match &event.edge {
        EnsureEdge::Bind { fulfiller, .. } => Some(fulfiller),
        _ => None,
    });
    if bound != Some(observed) {
        return Err("observation must name the bound fulfiller".into());
    }

    if matches!(g.observe(observed).map(|o| o.fact), Some(Fact::Fulfiller { gone: true, .. })) {
        Ok(())
    } else {
        Err("old fulfiller must be observed Gone before replacement".into())
    }
}
fn gate_give_up(g: &Gate<'_>, r: &Resource<Ensure>, _: &EnsureEdge) -> Result<(), GateFailure> {
    let attempts = g.own_attempts(&r.attempts);
    let window =
        r.last_event("resume").and_then(|ev| ev.inputs.iter().find(|i| i.subject.kind == "attempts")).map_or(0, |i| i.version as usize);
    if retry(&attempts[window..], &r.spec.policy).exhausted() {
        Ok(())
    } else {
        Err("current attempt window is not exhausted".into())
    }
}
fn gate_resume(g: &Gate<'_>, r: &Resource<Ensure>, edge: &EnsureEdge) -> Result<(), GateFailure> {
    g.own_attempts(&r.attempts);
    gate_retire(g, r, edge)
}
fn gate_retire(g: &Gate<'_>, _: &Resource<Ensure>, edge: &EnsureEdge) -> Result<(), GateFailure> {
    let (EnsureEdge::Resume { by, authority } | EnsureEdge::Retire { by, authority }) = edge else { unreachable!() };
    if matches!(g.observe(authority).map(|o| o.fact), Some(Fact::Authorised { party }) if party == *by) {
        Ok(())
    } else {
        Err("authorised party required".into())
    }
}
impl Machine for Ensure {
    const NAME: &'static str = "Ensure";
    type Spec = EnsureSpec;
    type State = EnsureState;
    type Edge = EnsureEdge;
    fn initial() -> EnsureState {
        EnsureState::Unfulfilled
    }
    fn definition() -> MachineDef<Self> {
        use EnsureState::*;
        MachineDef::new()
            .edge(EdgeDef::new("bind").from([Unfulfilled]).to(Fulfilled).gate("recorded Realised fulfiller can receive", gate_bind))
            .edge(EdgeDef::new("unbind").from([Fulfilled]).to(Unfulfilled).gate("old fulfiller Gone", gate_unbind))
            .edge(EdgeDef::new("give up").from([Unfulfilled]).to(Held).gate("current window exhausted", gate_give_up))
            .edge(EdgeDef::new("resume").from([Held]).to(Unfulfilled).gate("authorised new attempt window", gate_resume))
            .edge(EdgeDef::new("retire").from([Unfulfilled, Fulfilled, Held]).to(Retired).gate("authorised retirement", gate_retire))
            .end(Retired, "the ensure is retired")
    }
}
