//! A flotilla-shaped pure transition function for the #3109 spike.
//!
//! `step(&State, Event) -> Result<(State, Vec<Effect>), Refused>` follows the
//! "proposed" table in `../spec/convoy_landing.qnt`. Effects are returned as
//! data and never performed, the same shape as `Reconciler::reconcile`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Active,
    Landing,
    Landed,
    Abandoned,
}

impl Phase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Phase::Landed | Phase::Abandoned)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrewPhase {
    Working,
    Done,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromiseState {
    Open,
    Submitted,
    Kept,
    Retracted,
}

/// Claims frozen at the Active -> Landing edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LandingEntry {
    pub claims_epoch: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub phase: Phase,
    pub crew: CrewPhase,
    /// Bumped every time crew work (re)opens; richer than the spec's boolean.
    pub crew_epoch: u32,
    pub promise: PromiseState,
    pub pr_merged: bool,
    pub landing_entry: Option<LandingEntry>,
    pub vessel_live: bool,
}

impl Default for State {
    fn default() -> Self {
        State {
            phase: Phase::Active,
            crew: CrewPhase::Working,
            crew_epoch: 0,
            promise: PromiseState::Open,
            pr_merged: false,
            landing_entry: None,
            vessel_live: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Submit,
    Complete,
    Handoff,
    Merge,
    VerdictKept,
    VerdictRejected,
    TurnDelivery,
    Settle,
    Abandon,
    Teardown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    EmitLandingEvent,
    DeleteVessel,
    DeleteTerminalSession,
    DeliverBrief,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub event: Event,
    pub from: Phase,
}

pub fn step(s: &State, e: Event) -> Result<(State, Vec<Effect>), Refused> {
    use CrewPhase::*;
    use Phase::*;
    use PromiseState::*;
    let refuse = || Refused { event: e, from: s.phase };
    let promise_terminal = matches!(s.promise, Kept | Retracted);
    let mut n = s.clone();
    let mut fx = Vec::new();
    match e {
        Event::Submit if !s.phase.is_terminal() && s.crew == Working && s.promise == Open => n.promise = Submitted,
        Event::Complete if matches!(s.phase, Active | Landing) && s.crew == Working && promise_terminal => {
            n.crew = Done;
            if s.phase == Active {
                n.phase = Landing;
                n.landing_entry = Some(LandingEntry { claims_epoch: s.crew_epoch });
                fx.push(Effect::EmitLandingEvent);
            }
        }
        Event::Handoff if !s.phase.is_terminal() && s.crew == Done => {
            n.crew = Working;
            n.crew_epoch += 1;
            if !cfg!(feature = "mutant") && s.phase == Landing {
                n.phase = Active;
            }
            fx.push(Effect::DeliverBrief);
        }
        Event::Merge if s.promise == Submitted && !s.pr_merged => n.pr_merged = true,
        Event::VerdictKept if !s.phase.is_terminal() && s.promise == Submitted && s.pr_merged => n.promise = Kept,
        Event::VerdictRejected if !s.phase.is_terminal() && s.promise == Submitted && !s.pr_merged => n.promise = Open,
        Event::TurnDelivery if s.phase == Landing => {
            n.phase = Active;
            n.crew = Working;
            n.crew_epoch += 1;
            fx.push(Effect::DeliverBrief);
        }
        Event::Settle if s.phase == Landing && s.crew == Done && promise_terminal => n.phase = Landed,
        Event::Abandon if !s.phase.is_terminal() || (cfg!(feature = "mutant_abandon_landed") && s.phase == Landed) => {
            n.phase = Abandoned;
            if s.crew == Working {
                n.crew = Stopped;
            }
        }
        Event::Teardown if s.phase.is_terminal() && s.vessel_live => {
            n.vessel_live = false;
            fx.push(Effect::DeleteTerminalSession);
            fx.push(Effect::DeleteVessel);
        }
        _ => return Err(refuse()),
    }
    Ok((n, fx))
}
