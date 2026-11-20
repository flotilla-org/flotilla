//! A convoy's phases, derived from what other records say (#3108 Convoy conversion).
//!
//! The old store keeps a convoy's phase, each work's phase and each crew member's phase as
//! status the reconciler writes, one step per pass. On the engine they are not stored: every
//! reader derives them from the same facts with [`derive`], so a phase cannot lag its inputs or
//! disagree between readers. The facts are:
//! - how the convoy ended, if it has (the engine Convoy's state);
//! - each work's Vessel, by work name;
//! - each seat's reading, from its promises, launch, delivered task, verdict and stall.
//!
//! Completion is promises only. A work is Complete when its seats own at least one promise and
//! every one is kept or retracted; an operator's override is a verdict that keeps them. A work
//! whose seats own none (a tool-only vessel) completes nothing: it follows its Vessel, and a
//! dependant waits only until it runs. The convoy is Landing when it owns at least one promise
//! and every one is settled, across all seats, tool seats included.
//!
//! Otherwise the derivation is the fixpoint the old reconciler reaches from the same facts. The
//! steps it took one pass at a time (Ready, then Launching, then Running) collapse, and a phase
//! that reopens (Landing back to Active on a new turn) follows its inputs, with no write.

use std::collections::BTreeMap;

use super::{ConvoyPhase, CrewWorkPhase, WorkPhase, WorkflowSnapshot};
use crate::vessel::VesselPhase;

/// How a convoy ended, from the engine Convoy's state; `None` while it is Open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// Delivered: Landed.
    Delivered,
    /// Abandoned because its work failed.
    Failed,
    /// Abandoned by an operator.
    Abandoned,
    /// Retired, so ended; how it ended is not held here.
    NotHeld,
}

/// What a work's Vessel says, by work name. A work with no entry has never had a Vessel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VesselReading {
    /// The Vessel exists, at this phase.
    At(VesselPhase),
    /// The work's Vessel existed and is gone: lost, it is replaced within the convoy.
    Gone,
}

/// One seat's reading, from the records about it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CrewReading {
    /// The promises it owns.
    pub promises: u32,
    /// How many of them are neither kept nor retracted.
    pub open: u32,
    /// It handed its work to another seat.
    pub handed_back: bool,
    /// A supervisor or operator failed it.
    pub failed: bool,
    /// An open stall is raised about it.
    pub stalled: bool,
    /// Its session is launched and has its task: an eager role from launch, any other once a
    /// task is delivered.
    pub working: bool,
}

/// The facts one convoy's phases are derived from.
#[derive(Debug, Clone, Copy)]
pub struct ConvoyFacts<'a> {
    pub ended: Option<Ended>,
    pub snapshot: &'a WorkflowSnapshot,
    pub vessels: &'a BTreeMap<String, VesselReading>,
    /// Seat readings by work, then role, including seats a handoff added. A role with no
    /// reading is Pending and owns nothing.
    pub crew: &'a BTreeMap<String, BTreeMap<String, CrewReading>>,
}

impl CrewReading {
    fn done(&self) -> bool {
        self.promises > 0 && self.open == 0
    }
}

/// The promises a set of seats owns, and how many are open.
fn owed<'a>(readings: impl IntoIterator<Item = &'a CrewReading>) -> (u32, u32) {
    readings.into_iter().fold((0, 0), |(promises, open), reading| (promises + reading.promises, open + reading.open))
}

/// The phases [`derive`] reads from a convoy's facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvoyReading {
    pub phase: ConvoyPhase,
    pub work: BTreeMap<String, WorkPhase>,
    pub crew: BTreeMap<String, BTreeMap<String, CrewWorkPhase>>,
}

/// Derives a convoy's phase, its works' and its crew's from `facts`.
pub fn derive(facts: &ConvoyFacts<'_>) -> ConvoyReading {
    let crew = crew_phases(facts);
    let mut work = BTreeMap::new();
    // Requirements are in dependency order in a valid snapshot; a dependency not yet derived
    // reads as unsatisfied, which holds the dependant Pending as the old reconciler does.
    for requirement in &facts.snapshot.vessels {
        let phase = work_phase(facts, &requirement.name, &requirement.depends_on, crew.get(&requirement.name), &work);
        work.insert(requirement.name.clone(), phase);
    }
    let phase = convoy_phase(facts.ended, &work, owed(facts.crew.values().flat_map(BTreeMap::values)));
    let work = match facts.ended {
        // An ended convoy's unfinished works read as the end made them.
        Some(ended) => work
            .into_iter()
            .map(|(name, phase)| {
                let phase = match (ended, phase) {
                    (_, phase) if phase.is_terminal() => phase,
                    // How it ended is not held here: an unfinished work keeps what its Vessel says.
                    (Ended::NotHeld, phase) => phase,
                    (Ended::Abandoned, _) => WorkPhase::Abandoned,
                    (Ended::Failed | Ended::Delivered, _) => WorkPhase::Cancelled,
                };
                (name, phase)
            })
            .collect(),
        None => work,
    };
    ConvoyReading { phase, work, crew }
}

fn crew_phases(facts: &ConvoyFacts<'_>) -> BTreeMap<String, BTreeMap<String, CrewWorkPhase>> {
    facts
        .snapshot
        .vessels
        .iter()
        .map(|requirement| {
            let interrupted = matches!(
                facts.vessels.get(&requirement.name),
                Some(VesselReading::At(VesselPhase::Interrupted | VesselPhase::Lost | VesselPhase::Released) | VesselReading::Gone)
            );
            let readings = facts.crew.get(&requirement.name);
            let added =
                readings.into_iter().flat_map(BTreeMap::keys).filter(|role| !requirement.crew.iter().any(|member| member.role == **role));
            let roles = requirement
                .crew
                .iter()
                .map(|member| &member.role)
                .chain(added)
                .map(|role| {
                    let reading = readings.and_then(|readings| readings.get(role)).copied().unwrap_or_default();
                    let phase = if reading.failed {
                        CrewWorkPhase::Failed
                    } else if reading.done() {
                        CrewWorkPhase::Done
                    } else if reading.handed_back {
                        CrewWorkPhase::HandedBack
                    } else if facts.ended.is_some() {
                        // The launch ended under a seat that had not finished: the seat did not
                        // fail, it reads the end.
                        CrewWorkPhase::Ended
                    } else if reading.stalled {
                        CrewWorkPhase::Stalled
                    } else if interrupted {
                        CrewWorkPhase::Interrupted
                    } else if reading.working {
                        CrewWorkPhase::Working
                    } else {
                        CrewWorkPhase::Pending
                    };
                    (role.clone(), phase)
                })
                .collect();
            (requirement.name.clone(), roles)
        })
        .collect()
}

fn work_phase(
    facts: &ConvoyFacts<'_>,
    name: &str,
    depends_on: &[String],
    crew: Option<&BTreeMap<String, CrewWorkPhase>>,
    derived: &BTreeMap<String, WorkPhase>,
) -> WorkPhase {
    let crew = crew.filter(|crew| !crew.is_empty());
    // A failed seat fails its work, standing or not: a standing role's generation ends failed,
    // a strike, and its next launch replaces it.
    if crew.is_some_and(|crew| crew.values().any(|phase| *phase == CrewWorkPhase::Failed)) {
        return WorkPhase::Failed;
    }
    let (promises, open) = owed(facts.crew.get(name).into_iter().flat_map(BTreeMap::values));
    if promises > 0 && open == 0 {
        return WorkPhase::Complete;
    }
    if !depends_on.iter().all(|dependency| satisfied(facts, dependency, derived)) {
        return WorkPhase::Pending;
    }
    match facts.vessels.get(name) {
        None | Some(VesselReading::At(VesselPhase::Pending | VesselPhase::Provisioning)) => WorkPhase::Ready,
        Some(VesselReading::At(VesselPhase::Ready)) => {
            if crew.is_some_and(|crew| crew.values().any(|phase| *phase == CrewWorkPhase::Stalled)) {
                WorkPhase::Stalled
            } else {
                WorkPhase::Running
            }
        }
        // A released vessel is as one gone: the work's next launch replaces it.
        Some(VesselReading::At(VesselPhase::Interrupted | VesselPhase::Lost | VesselPhase::Released) | VesselReading::Gone) => {
            WorkPhase::Interrupted
        }
        // A failed Vessel fails the convoy, standing or not: a standing role's next launch
        // replaces it, at the cost of a strike.
        Some(VesselReading::At(VesselPhase::Failed)) => WorkPhase::Failed,
    }
}

/// A dependency is satisfied when it is Complete, or, owing no promise, once it runs.
fn satisfied(facts: &ConvoyFacts<'_>, dependency: &str, derived: &BTreeMap<String, WorkPhase>) -> bool {
    match derived.get(dependency) {
        Some(WorkPhase::Complete) => true,
        Some(WorkPhase::Running) => owed(facts.crew.get(dependency).into_iter().flat_map(BTreeMap::values)).0 == 0,
        _ => false,
    }
}

fn convoy_phase(ended: Option<Ended>, work: &BTreeMap<String, WorkPhase>, (promises, open): (u32, u32)) -> ConvoyPhase {
    match ended {
        Some(Ended::Delivered) => return ConvoyPhase::Landed,
        Some(Ended::Failed) => return ConvoyPhase::Failed,
        Some(Ended::Abandoned) => return ConvoyPhase::Abandoned,
        Some(Ended::NotHeld) => return ConvoyPhase::Ended,
        None => {}
    }
    if work.values().any(|phase| *phase == WorkPhase::Failed) {
        ConvoyPhase::Failed
    } else if work.values().any(|phase| *phase == WorkPhase::Interrupted) {
        ConvoyPhase::Interrupted
    } else if promises > 0 && open == 0 {
        ConvoyPhase::Landing
    } else if work.values().any(|phase| *phase != WorkPhase::Pending) {
        ConvoyPhase::Active
    } else {
        ConvoyPhase::Pending
    }
}

#[cfg(test)]
mod tests;
