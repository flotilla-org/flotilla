//! A small machine for exercising the engine, shared by the engine tests.

use flotilla_state_machines::{edges, states, EdgeDef, Ended, Handle, Kind, Machine, MachineDef, Ref, Table, WrongState};

// A latch that is armed, then
// either fires (choosing between two outcomes) or is withdrawn.
states! {
    pub enum Latch { Armed = 1, Fired = 2, Spent = 3, Withdrawn = 4 }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fire {
    pub signal: Ref<Signal>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Withdraw {
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReArm;

edges! {
    pub enum LatchEdge => LatchEdgeKind {
        Fire(Fire) = 10,
        Withdraw(Withdraw) = 11,
        ReArm(ReArm) = 12,
    }
}

pub struct Signal;
impl Kind for Signal {
    const NAME: &'static str = "signal";
    type Value = bool;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LatchRefusal {
    Ended(&'static str),
    WrongState,
    NoSignal,
    EmptyReason,
}

impl From<Ended<Latch>> for LatchRefusal {
    fn from(ended: Ended<Latch>) -> Self {
        LatchRefusal::Ended(ended.reason)
    }
}

impl From<WrongState<Latch, LatchEdgeKind>> for LatchRefusal {
    fn from(_: WrongState<Latch, LatchEdgeKind>) -> Self {
        LatchRefusal::WrongState
    }
}

pub struct LatchMachine;

impl Machine for LatchMachine {
    const NAME: &'static str = "Latch";
    type Spec = bool; // whether a firing is spent at once
    type State = Latch;
    type Edge = LatchEdge;
    type Refusal = LatchRefusal;
    type World = Table<Signal>;

    fn definition() -> MachineDef<Self> {
        MachineDef::new(Latch::Armed)
            .edge(
                EdgeDef::<Self, Fire>::new()
                    .from([Latch::Armed])
                    .to_chosen([Latch::Fired, Latch::Spent], pick_fire)
                    .gate("the signal is raised", gate_fire),
            )
            .edge(
                EdgeDef::<Self, Withdraw>::new()
                    .from([Latch::Armed, Latch::Fired])
                    .to(Latch::Withdrawn)
                    .gate("a reason is given", gate_withdraw),
            )
            .edge(EdgeDef::<Self, ReArm>::new().from([Latch::Fired]).to(Latch::Armed).gate("always", |_, _| Ok(())))
            .end(Latch::Spent, "the latch fired for good")
            .end(Latch::Withdrawn, "the latch was withdrawn")
    }
}

// Typed on its own argument: no match on the edge enum.
fn gate_fire(h: &mut Handle<'_, LatchMachine>, fire: &Fire) -> Result<(), LatchRefusal> {
    match h.read(&fire.signal) {
        Some(true) => Ok(()),
        _ => Err(LatchRefusal::NoSignal),
    }
}

fn pick_fire(h: &mut Handle<'_, LatchMachine>, _: &Fire) -> Latch {
    if *h.spec() {
        Latch::Spent
    } else {
        Latch::Fired
    }
}

fn gate_withdraw(_: &mut Handle<'_, LatchMachine>, withdraw: &Withdraw) -> Result<(), LatchRefusal> {
    if withdraw.reason.trim().is_empty() {
        Err(LatchRefusal::EmptyReason)
    } else {
        Ok(())
    }
}
