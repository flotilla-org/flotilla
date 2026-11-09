//! A convoy's phases, derived from what other records say (#3108 Convoy conversion).
//!
//! The old store keeps a convoy's phase, each work's phase and each crew member's phase as
//! status the reconciler writes, one step per pass. On the engine they are not stored: every
//! reader derives them from the same facts with [`derive`], so a phase cannot lag its inputs or
//! disagree between readers. The facts are:
//! - how the convoy ended, if it has (the engine Convoy's state);
//! - each work's Vessel, by work name;
//! - each crew member's reading, from its promises, launch, delivered task, verdict and stall;
//! - which works an operator forced complete.
//!
//! The derivation is the fixpoint the old reconciler reaches from the same facts. The steps it
//! took one pass at a time (Ready, then Launching, then Running) collapse, and a phase that
//! reopens (Landing back to Active on a new turn) follows its inputs, with no write.

use std::collections::{BTreeMap, BTreeSet};

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
}

/// What a work's Vessel says, by work name. A work with no entry has never had a Vessel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VesselReading {
    /// The Vessel exists, at this phase.
    At(VesselPhase),
    /// The work's Vessel existed and is gone: a standing convoy re-creates it.
    Gone,
}

/// One crew member's reading, from the records about its seat.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CrewReading {
    /// Its promises say it is done.
    pub done: bool,
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
    /// A standing convoy's: a failed Vessel or crew member stalls its work rather than failing it.
    pub standing: bool,
    pub snapshot: &'a WorkflowSnapshot,
    pub vessels: &'a BTreeMap<String, VesselReading>,
    /// Crew readings by work, then role. A role with no reading is Pending.
    pub crew: &'a BTreeMap<String, BTreeMap<String, CrewReading>>,
    /// Works an operator forced complete.
    pub forced: &'a BTreeSet<String>,
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
    // reads as not Complete, which holds the dependant Pending as the old reconciler does.
    for requirement in &facts.snapshot.vessels {
        let phase = work_phase(facts, &requirement.name, &requirement.depends_on, crew.get(&requirement.name), &work);
        work.insert(requirement.name.clone(), phase);
    }
    let phase = convoy_phase(facts.ended, &work);
    let work = match facts.ended {
        // An ended convoy's unfinished works read as the end made them.
        Some(ended) => work
            .into_iter()
            .map(|(name, phase)| {
                let phase = match (ended, phase) {
                    (_, phase) if phase.is_terminal() => phase,
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
                Some(VesselReading::At(VesselPhase::Interrupted | VesselPhase::Lost) | VesselReading::Gone)
            );
            let readings = facts.crew.get(&requirement.name);
            let roles = requirement
                .crew
                .iter()
                .map(|member| {
                    let reading = readings.and_then(|readings| readings.get(&member.role)).copied().unwrap_or_default();
                    let phase = if reading.failed {
                        CrewWorkPhase::Failed
                    } else if reading.done {
                        CrewWorkPhase::Done
                    } else if reading.handed_back {
                        CrewWorkPhase::HandedBack
                    } else if reading.stalled {
                        CrewWorkPhase::Stalled
                    } else if interrupted {
                        CrewWorkPhase::Interrupted
                    } else if reading.working {
                        CrewWorkPhase::Working
                    } else {
                        CrewWorkPhase::Pending
                    };
                    (member.role.clone(), phase)
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
    if facts.forced.contains(name) {
        return WorkPhase::Complete;
    }
    let crew = crew.filter(|crew| !crew.is_empty());
    if crew.is_some_and(|crew| crew.values().any(|phase| *phase == CrewWorkPhase::Failed)) {
        return if facts.standing { WorkPhase::Stalled } else { WorkPhase::Failed };
    }
    if crew.is_some_and(|crew| crew.values().all(|phase| *phase == CrewWorkPhase::Done)) {
        return WorkPhase::Complete;
    }
    if !depends_on.iter().all(|dependency| derived.get(dependency) == Some(&WorkPhase::Complete)) {
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
        Some(VesselReading::At(VesselPhase::Interrupted | VesselPhase::Lost) | VesselReading::Gone) => WorkPhase::Interrupted,
        // A standing convoy's failed Vessel is retried as an interruption.
        Some(VesselReading::At(VesselPhase::Failed)) => {
            if facts.standing {
                WorkPhase::Interrupted
            } else {
                WorkPhase::Failed
            }
        }
    }
}

fn convoy_phase(ended: Option<Ended>, work: &BTreeMap<String, WorkPhase>) -> ConvoyPhase {
    match ended {
        Some(Ended::Delivered) => return ConvoyPhase::Landed,
        Some(Ended::Failed) => return ConvoyPhase::Failed,
        Some(Ended::Abandoned) => return ConvoyPhase::Abandoned,
        None => {}
    }
    if work.values().any(|phase| *phase == WorkPhase::Failed) {
        ConvoyPhase::Failed
    } else if work.values().any(|phase| *phase == WorkPhase::Interrupted) {
        ConvoyPhase::Interrupted
    } else if !work.is_empty() && work.values().all(|phase| *phase == WorkPhase::Complete) {
        ConvoyPhase::Landing
    } else if work.values().any(|phase| *phase != WorkPhase::Pending) {
        ConvoyPhase::Active
    } else {
        ConvoyPhase::Pending
    }
}

#[cfg(test)]
mod tests;
