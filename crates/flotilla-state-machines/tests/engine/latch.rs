//! A small machine for exercising the engine, shared by the engine tests.

use flotilla_state_machines::{edges, states, EdgeDef, Ended, Handle, Kind, Machine, MachineDef, Ref, Table, WrongState};

// A latch that is loaded with shots and armed, then either fires (spending
// itself when its spec says so) or is withdrawn with a reason.
states! {
    pub enum Latch => LatchLabel {
        Armed { shots: u32 } = 1,
        Fired { shots: u32, by: String } = 2,
        Spent { shots: u32 } = 3,
        Withdrawn { reason: String } = 4,
    }
}

/// Adds shots to an armed latch: an edge that changes only data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Load {
    pub shots: u32,
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
        Load(Load) = 13,
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
    NoShots,
    EmptyReason,
}

impl From<Ended<LatchLabel>> for LatchRefusal {
    fn from(ended: Ended<LatchLabel>) -> Self {
        LatchRefusal::Ended(ended.reason)
    }
}

impl From<WrongState<LatchLabel, LatchEdgeKind>> for LatchRefusal {
    fn from(_: WrongState<LatchLabel, LatchEdgeKind>) -> Self {
        LatchRefusal::WrongState
    }
}

pub struct LatchMachine;

impl Machine for LatchMachine {
    const NAME: &'static str = "Latch";
    /// Whether a firing spends the latch at once.
    type Request = bool;
    type Spec = bool;
    type State = Latch;
    type Edge = LatchEdge;
    type Refusal = LatchRefusal;
    type World = Table<Signal>;

    fn definition() -> MachineDef<Self> {
        use LatchLabel::*;
        MachineDef::new(Armed)
            .edge(
                EdgeDef::<Self, Load>::new()
                    .from([Armed])
                    .to([Armed])
                    .changes(["shots"])
                    .gate("at least one shot is added", gate_load)
                    .effect(load),
            )
            .edge(
                EdgeDef::<Self, Fire>::new()
                    .from([Armed])
                    .to([Fired, Spent])
                    .changes(["shots", "by"])
                    .gate("the signal is raised and a shot is loaded", gate_fire)
                    .effect(fire),
            )
            .edge(
                EdgeDef::<Self, Withdraw>::new()
                    .from([Armed, Fired])
                    .to([Withdrawn])
                    .changes(["reason"])
                    .gate("a reason is given", gate_withdraw)
                    .effect(|_, _, withdraw| Latch::Withdrawn { reason: withdraw.reason.clone() }),
            )
            .edge(
                EdgeDef::<Self, ReArm>::new().from([Fired]).to([Armed]).ungated("re-arming a fired latch needs no evidence").effect(re_arm),
            )
            .end(Spent, "the latch fired for good")
            .end(Withdrawn, "the latch was withdrawn")
            .attempts_in([Armed])
    }

    fn admit(spent: bool) -> Result<(bool, Latch), LatchRefusal> {
        Ok((spent, Latch::Armed { shots: 0 }))
    }
}

pub fn shots(latch: &Latch) -> u32 {
    match latch {
        Latch::Armed { shots } | Latch::Fired { shots, .. } | Latch::Spent { shots } => *shots,
        Latch::Withdrawn { .. } => 0,
    }
}

fn gate_load(_: &mut Handle<'_, LatchMachine>, load: &Load) -> Result<(), LatchRefusal> {
    if load.shots == 0 {
        Err(LatchRefusal::NoShots)
    } else {
        Ok(())
    }
}

fn load(_: &mut Handle<'_, LatchMachine>, state: Latch, load: &Load) -> Latch {
    Latch::Armed { shots: shots(&state) + load.shots }
}

// Typed on its own argument: no match on the edge enum. Reads the current
// state for the shot count and the world for the signal.
fn gate_fire(h: &mut Handle<'_, LatchMachine>, fire: &Fire) -> Result<(), LatchRefusal> {
    if shots(h.state()) == 0 {
        return Err(LatchRefusal::NoShots);
    }
    match h.read(&fire.signal) {
        Some(true) => Ok(()),
        _ => Err(LatchRefusal::NoSignal),
    }
}

fn fire(h: &mut Handle<'_, LatchMachine>, state: Latch, fire: &Fire) -> Latch {
    let shots = shots(&state) - 1;
    if *h.spec() {
        Latch::Spent { shots }
    } else {
        Latch::Fired { shots, by: fire.signal.name().to_string() }
    }
}

fn gate_withdraw(_: &mut Handle<'_, LatchMachine>, withdraw: &Withdraw) -> Result<(), LatchRefusal> {
    if withdraw.reason.trim().is_empty() {
        Err(LatchRefusal::EmptyReason)
    } else {
        Ok(())
    }
}

fn re_arm(_: &mut Handle<'_, LatchMachine>, state: Latch, _: &ReArm) -> Latch {
    Latch::Armed { shots: shots(&state) }
}
