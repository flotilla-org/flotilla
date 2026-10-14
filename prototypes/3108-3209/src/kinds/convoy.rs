//! Decided convoy: admission is non-vacuous; abandonment is an authorised decision.
use crate::base::ObjRef;
use crate::home::Resource;
use crate::machine::{EdgeDef, GateFailure, Machine, MachineDef, Named, StateSet};
use crate::view::{Fact, Gate};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConvoyState {
    Open,
    Delivered,
    Abandoned,
}
impl Named for ConvoyState {
    fn name(&self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Delivered => "Delivered",
            Self::Abandoned => "Abandoned",
        }
    }
}
impl StateSet for ConvoyState {
    fn all() -> &'static [Self] {
        &[Self::Open, Self::Delivered, Self::Abandoned]
    }
}
#[derive(Clone, Debug)]
pub struct ConvoySpec {
    pub promises: Vec<ObjRef>,
}
#[derive(Clone, Debug)]
pub enum ConvoyEdge {
    Deliver,
    Abandon { by: String, reason: String, authority: ObjRef },
}
impl Named for ConvoyEdge {
    fn name(&self) -> &'static str {
        match self {
            Self::Deliver => "deliver",
            Self::Abandon { .. } => "abandon",
        }
    }
}
#[derive(Clone, Debug)]
pub struct Convoy;
fn gate_deliver(g: &Gate<'_>, r: &Resource<Convoy>, _: &ConvoyEdge) -> Result<(), GateFailure> {
    for p in &r.spec.promises {
        match g.observe(p).map(|o| o.fact) {
            Some(Fact::Promise { kept, accepted_retraction }) if kept || accepted_retraction => {}
            _ => return Err("a promise is neither kept nor retracted with acceptance".into()),
        }
    }
    Ok(())
}
fn gate_abandon(g: &Gate<'_>, _: &Resource<Convoy>, edge: &ConvoyEdge) -> Result<(), GateFailure> {
    let ConvoyEdge::Abandon { by, reason, authority } = edge else { unreachable!() };
    match g.observe(authority).map(|o| o.fact) {
        Some(Fact::Authorised { party }) if party == *by && !reason.is_empty() => Ok(()),
        _ => Err("abandonment requires an authorised party and reason".into()),
    }
}
impl Machine for Convoy {
    const NAME: &'static str = "Convoy";
    type Spec = ConvoySpec;
    type State = ConvoyState;
    type Edge = ConvoyEdge;
    fn initial() -> ConvoyState {
        ConvoyState::Open
    }
    fn admit(spec: &ConvoySpec) -> Result<(), GateFailure> {
        if spec.promises.is_empty() {
            Err("a convoy must promise something".into())
        } else {
            Ok(())
        }
    }
    fn definition() -> MachineDef<Self> {
        use ConvoyState::*;
        MachineDef::new()
            .edge(EdgeDef::new("deliver").from([Open]).to(Delivered).gate("all promises kept or accepted retractions", gate_deliver))
            .edge(EdgeDef::new("abandon").from([Open]).to(Abandoned).gate("authorised decision", gate_abandon))
            .end(Delivered, "the work was delivered")
            .end(Abandoned, "an authorised party abandoned the work")
    }
}
