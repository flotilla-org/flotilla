//! A small machine for exercising the engine, shared by the engine tests.

use serde::{Deserialize, Serialize};

use flotilla_state_machines::{
    attempt, edges, states, EdgeDef, Ended, Failure, Handle, Kind, Machine, MachineDef, Outcome, Ref, Table, WrongState,
};

// A latch that is loaded with shots and armed, then either fires (spending
// itself when its spec says so) or is withdrawn with a reason. While armed
// it keeps the tally of its attempts at firing; re-arming opens a new window.
states! {
    pub enum Latch => LatchLabel {
        Armed { shots: u32, failures: u32, #[store(flatten)] last_failure: Option<Failure> } = "ar",
        Fired { shots: u32, by: String } = "fi",
        Spent { shots: u32 } = "sp",
        Withdrawn { reason: String } = "wd",
    }
}

/// Adds shots to an armed latch: an edge that changes only data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Load {
    pub shots: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fire {
    pub signal: Ref<Signal>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Withdraw {
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReArm;

/// One attempt at firing, and how it ended: an edge that changes the tally.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Try {
    pub outcome: Outcome,
}

/// A note about `subject`, its master: an edge that changes nothing but its
/// log entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub subject: String,
}

edges! {
    pub enum LatchEdge => LatchEdgeKind {
        Fire(Fire) = "fi",
        Withdraw(Withdraw) = "wd",
        ReArm(ReArm) = "ra",
        Load(Load) = "lo",
        Try(Try) = "tr",
        Note(Note) = "no",
    }
}

/// An armed latch with `shots` and no attempts.
pub fn armed(shots: u32) -> Latch {
    Latch::Armed { shots, failures: 0, last_failure: None }
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
                EdgeDef::<Self, ReArm>::new()
                    .from([Fired])
                    .to([Armed])
                    .changes(["failures", "last_failure"])
                    .ungated("re-arming a fired latch needs no evidence")
                    .effect(re_arm),
            )
            .edge(
                EdgeDef::<Self, Try>::new()
                    .from([Armed])
                    .to([Armed])
                    .changes(["failures", "last_failure"])
                    .ungated("an attempt reports how a try ended")
                    .effect(try_once),
            )
            .edge(
                EdgeDef::<Self, Note>::new()
                    .from([Armed])
                    .to([Armed])
                    .about(|note| &note.subject)
                    .ungated("a note is its log entry")
                    .effect(|_, state, _| state),
            )
            .end(Spent, "the latch fired for good")
            .end(Withdrawn, "the latch was withdrawn")
    }

    fn admit(_: &mut flotilla_state_machines::Admission<'_, Self>, spent: bool) -> Result<(bool, Latch), LatchRefusal> {
        Ok((spent, armed(0)))
    }
}

pub fn shots(latch: &Latch) -> u32 {
    match latch {
        Latch::Armed { shots, .. } | Latch::Fired { shots, .. } | Latch::Spent { shots } => *shots,
        Latch::Withdrawn { .. } => 0,
    }
}

/// The armed latch's attempt tally; none in any other label.
pub fn tally(latch: &Latch) -> (u32, Option<&Failure>) {
    match latch {
        Latch::Armed { failures, last_failure, .. } => (*failures, last_failure.as_ref()),
        _ => (0, None),
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
    match state {
        Latch::Armed { shots, failures, last_failure } => Latch::Armed { shots: shots + load.shots, failures, last_failure },
        other => armed(shots(&other) + load.shots),
    }
}

fn try_once(h: &mut Handle<'_, LatchMachine>, state: Latch, edge: &Try) -> Latch {
    let (failures, last_failure) = attempt::tally(tally(&state).0, &edge.outcome, h.now());
    Latch::Armed { shots: shots(&state), failures, last_failure }
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
    armed(shots(&state))
}
