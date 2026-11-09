//! Pins today's phase transitions (the old reconciler's fixpoints) against [`derive`].

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::workflow_template::{CrewSource, CrewSpec, VesselRequirement};

fn requirement(name: &str, depends_on: &[&str], roles: &[&str]) -> VesselRequirement {
    VesselRequirement::builder()
        .name(name.into())
        .depends_on(depends_on.iter().map(|dependency| dependency.to_string()).collect())
        .crew(
            roles
                .iter()
                .map(|role| CrewSpec::builder().role(role.to_string()).source(CrewSource::Tool { command: "true".into() }).build())
                .collect(),
        )
        .build()
}

fn snapshot(vessels: Vec<VesselRequirement>) -> WorkflowSnapshot {
    WorkflowSnapshot {
        cascade: None,
        exit: None,
        turn_delivery: Default::default(),
        stall_nudges: Default::default(),
        supervision: None,
        vessels,
    }
}

/// One convoy's facts, built up per test.
struct Case {
    ended: Option<Ended>,
    standing: bool,
    snapshot: WorkflowSnapshot,
    vessels: BTreeMap<String, VesselReading>,
    crew: BTreeMap<String, BTreeMap<String, CrewReading>>,
    forced: BTreeSet<String>,
}

impl Case {
    /// `build` (coder) then `review` (reviewer), review depending on build.
    fn two_works() -> Self {
        Case {
            ended: None,
            standing: false,
            snapshot: snapshot(vec![requirement("build", &[], &["coder"]), requirement("review", &["build"], &["reviewer"])]),
            vessels: BTreeMap::new(),
            crew: BTreeMap::new(),
            forced: BTreeSet::new(),
        }
    }

    fn vessel(mut self, work: &str, reading: VesselReading) -> Self {
        self.vessels.insert(work.into(), reading);
        self
    }

    fn crew(mut self, work: &str, role: &str, reading: CrewReading) -> Self {
        self.crew.entry(work.into()).or_default().insert(role.into(), reading);
        self
    }

    fn derive(&self) -> ConvoyReading {
        derive(&ConvoyFacts {
            ended: self.ended,
            standing: self.standing,
            snapshot: &self.snapshot,
            vessels: &self.vessels,
            crew: &self.crew,
            forced: &self.forced,
        })
    }
}

const WORKING: CrewReading = CrewReading { done: false, handed_back: false, failed: false, stalled: false, working: true };
const DONE: CrewReading = CrewReading { done: true, handed_back: false, failed: false, stalled: false, working: true };

fn ready() -> VesselReading {
    VesselReading::At(VesselPhase::Ready)
}

#[test]
fn an_admitted_convoy_is_active_with_its_first_work_ready_and_the_rest_pending() {
    let reading = Case::two_works().derive();
    // The old reconciler bootstraps every work Pending, then advances those with no
    // dependencies to Ready, which rolls the convoy up to Active.
    assert_eq!(reading.work["build"], WorkPhase::Ready);
    assert_eq!(reading.work["review"], WorkPhase::Pending);
    assert_eq!(reading.phase, ConvoyPhase::Active);
    assert_eq!(reading.crew["build"]["coder"], CrewWorkPhase::Pending);
}

#[test]
fn a_provisioning_vessel_keeps_its_work_ready_and_a_ready_one_runs_it() {
    let case = Case::two_works().vessel("build", VesselReading::At(VesselPhase::Provisioning));
    assert_eq!(case.derive().work["build"], WorkPhase::Ready);
    // Ready, then Launching, then Running on the old reconciler's passes: the fixpoint is Running.
    let case = case.vessel("build", ready()).crew("build", "coder", WORKING);
    let reading = case.derive();
    assert_eq!(reading.work["build"], WorkPhase::Running);
    assert_eq!(reading.crew["build"]["coder"], CrewWorkPhase::Working);
}

#[test]
fn a_completed_work_readies_its_dependant() {
    let reading = Case::two_works().vessel("build", ready()).crew("build", "coder", DONE).derive();
    assert_eq!(reading.work["build"], WorkPhase::Complete);
    assert_eq!(reading.work["review"], WorkPhase::Ready);
    assert_eq!(reading.phase, ConvoyPhase::Active);
}

#[test]
fn every_work_complete_is_landing_and_a_new_turn_reopens_it() {
    let case =
        Case::two_works().vessel("build", ready()).crew("build", "coder", DONE).vessel("review", ready()).crew("review", "reviewer", DONE);
    assert_eq!(case.derive().phase, ConvoyPhase::Landing);
    // A delivered turn (or a resume) sets the crew working again: the old reconciler wrote
    // Active and the work back to Running.
    let reading = case.crew("review", "reviewer", WORKING).derive();
    assert_eq!(reading.work["review"], WorkPhase::Running);
    assert_eq!(reading.phase, ConvoyPhase::Active);
}

#[test]
fn delivery_is_landed() {
    let mut case =
        Case::two_works().vessel("build", ready()).crew("build", "coder", DONE).vessel("review", ready()).crew("review", "reviewer", DONE);
    case.ended = Some(Ended::Delivered);
    let reading = case.derive();
    assert_eq!(reading.phase, ConvoyPhase::Landed);
    assert!(reading.work.values().all(|phase| *phase == WorkPhase::Complete));
}

#[test]
fn a_stalled_crew_member_stalls_its_running_work_and_recovers_with_it() {
    let stalled = CrewReading { stalled: true, ..WORKING };
    let case = Case::two_works().vessel("build", ready()).crew("build", "coder", stalled);
    let reading = case.derive();
    assert_eq!(reading.crew["build"]["coder"], CrewWorkPhase::Stalled);
    assert_eq!(reading.work["build"], WorkPhase::Stalled);
    assert_eq!(reading.phase, ConvoyPhase::Active);
    assert_eq!(case.crew("build", "coder", WORKING).derive().work["build"], WorkPhase::Running);
}

#[test]
fn an_interrupted_or_lost_vessel_interrupts_its_work_crew_and_convoy() {
    for phase in [VesselPhase::Interrupted, VesselPhase::Lost] {
        let reading = Case::two_works().vessel("build", VesselReading::At(phase)).crew("build", "coder", WORKING).derive();
        assert_eq!(reading.work["build"], WorkPhase::Interrupted, "{phase:?}");
        assert_eq!(reading.crew["build"]["coder"], CrewWorkPhase::Interrupted, "{phase:?}");
        assert_eq!(reading.phase, ConvoyPhase::Interrupted, "{phase:?}");
    }
    // Ready again, the work runs and the convoy is Active.
    let reading = Case::two_works().vessel("build", ready()).crew("build", "coder", WORKING).derive();
    assert_eq!(reading.phase, ConvoyPhase::Active);
}

#[test]
fn a_failed_vessel_fails_a_finite_convoy_and_cancels_its_unfinished_work() {
    let case = Case::two_works().vessel("build", VesselReading::At(VesselPhase::Failed));
    let reading = case.derive();
    assert_eq!(reading.work["build"], WorkPhase::Failed);
    assert_eq!(reading.phase, ConvoyPhase::Failed);
    // Once the engine records the failure, the unfinished work reads Cancelled, as fail_convoy wrote.
    let mut case = case;
    case.ended = Some(Ended::Failed);
    let reading = case.derive();
    assert_eq!(reading.phase, ConvoyPhase::Failed);
    assert_eq!(reading.work["build"], WorkPhase::Failed);
    assert_eq!(reading.work["review"], WorkPhase::Cancelled);
}

#[test]
fn a_failed_crew_member_fails_a_finite_work_and_stalls_a_standing_one() {
    let failed = CrewReading { failed: true, ..WORKING };
    let case = Case::two_works().vessel("build", ready()).crew("build", "coder", failed);
    let reading = case.derive();
    assert_eq!(reading.crew["build"]["coder"], CrewWorkPhase::Failed);
    assert_eq!(reading.work["build"], WorkPhase::Failed);
    assert_eq!(reading.phase, ConvoyPhase::Failed);
    let mut standing = case;
    standing.standing = true;
    let reading = standing.derive();
    assert_eq!(reading.work["build"], WorkPhase::Stalled);
    assert_eq!(reading.phase, ConvoyPhase::Active);
}

#[test]
fn a_standing_convoys_failed_or_vanished_vessel_is_an_interruption() {
    for reading in [VesselReading::At(VesselPhase::Failed), VesselReading::Gone] {
        let mut case = Case::two_works().vessel("build", reading);
        case.standing = true;
        let derived = case.derive();
        assert_eq!(derived.work["build"], WorkPhase::Interrupted, "{reading:?}");
        assert_eq!(derived.phase, ConvoyPhase::Interrupted, "{reading:?}");
    }
}

#[test]
fn an_operator_forced_work_is_complete_whatever_its_crew_says() {
    let mut case = Case::two_works().vessel("build", ready()).crew("build", "coder", WORKING);
    case.forced.insert("build".into());
    let reading = case.derive();
    assert_eq!(reading.work["build"], WorkPhase::Complete);
    assert_eq!(reading.work["review"], WorkPhase::Ready);
}

#[test]
fn an_abandoned_convoy_abandons_its_unfinished_work() {
    let mut case = Case::two_works().vessel("build", ready()).crew("build", "coder", DONE);
    case.ended = Some(Ended::Abandoned);
    let reading = case.derive();
    assert_eq!(reading.phase, ConvoyPhase::Abandoned);
    assert_eq!(reading.work["build"], WorkPhase::Complete);
    assert_eq!(reading.work["review"], WorkPhase::Abandoned);
}

#[test]
fn a_handed_back_member_neither_completes_nor_blocks_its_work() {
    // The old roll-up completes a work only when every member is Done; a member that handed
    // back is not Done, so the work keeps running on the member it handed to.
    let handed = CrewReading { handed_back: true, ..WORKING };
    let mut case = Case::two_works();
    case.snapshot = snapshot(vec![requirement("build", &[], &["coder", "fixer"])]);
    let reading = case.vessel("build", ready()).crew("build", "coder", handed).crew("build", "fixer", WORKING).derive();
    assert_eq!(reading.crew["build"]["coder"], CrewWorkPhase::HandedBack);
    assert_eq!(reading.work["build"], WorkPhase::Running);
}

#[test]
fn a_convoy_with_no_works_is_pending_never_landing() {
    let mut case = Case::two_works();
    case.snapshot = snapshot(Vec::new());
    let reading = case.derive();
    assert!(reading.work.is_empty());
    assert_eq!(reading.phase, ConvoyPhase::Pending);
}

#[test]
fn works_that_have_not_started_never_read_landing() {
    // Nothing launched: the first work is Ready, its dependant Pending.
    assert_eq!(Case::two_works().derive().phase, ConvoyPhase::Active);
    // Only dependants, none of whose dependencies are complete: every work Pending.
    let mut case = Case::two_works();
    case.snapshot = snapshot(vec![requirement("review", &["build"], &["reviewer"])]);
    assert_eq!(case.derive().phase, ConvoyPhase::Pending);
    // A work whose template has no crew completes only by verdict, so it never lands unstarted.
    let mut crewless = Case::two_works();
    crewless.snapshot = snapshot(vec![requirement("build", &[], &[])]);
    let reading = crewless.vessel("build", ready()).derive();
    assert_eq!(reading.work["build"], WorkPhase::Running);
    assert_ne!(reading.phase, ConvoyPhase::Landing);
}
