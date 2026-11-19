//! Pins today's phase transitions (the old reconciler's fixpoints) against [`derive`].

use std::collections::BTreeMap;

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
    snapshot: WorkflowSnapshot,
    vessels: BTreeMap<String, VesselReading>,
    crew: BTreeMap<String, BTreeMap<String, CrewReading>>,
}

impl Case {
    /// `build` (coder) then `review` (reviewer), review depending on build.
    fn two_works() -> Self {
        Case {
            ended: None,
            snapshot: snapshot(vec![requirement("build", &[], &["coder"]), requirement("review", &["build"], &["reviewer"])]),
            vessels: BTreeMap::new(),
            crew: BTreeMap::new(),
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
        derive(&ConvoyFacts { ended: self.ended, snapshot: &self.snapshot, vessels: &self.vessels, crew: &self.crew })
    }
}

/// A working seat owing one open promise, and the same seat once it is kept.
const WORKING: CrewReading = CrewReading { promises: 1, open: 1, handed_back: false, failed: false, stalled: false, working: true };
const DONE: CrewReading = CrewReading { open: 0, ..WORKING };
/// A seat not yet launched, owing the promise admission declared for it.
const OWING: CrewReading = CrewReading { promises: 1, open: 1, handed_back: false, failed: false, stalled: false, working: false };

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
    let reading = Case::two_works().vessel("build", ready()).crew("build", "coder", DONE).crew("review", "reviewer", OWING).derive();
    assert_eq!(reading.work["build"], WorkPhase::Complete);
    assert_eq!(reading.work["review"], WorkPhase::Ready);
    assert_eq!(reading.phase, ConvoyPhase::Active);
}

#[test]
fn every_promise_settled_is_landing_and_an_open_one_reopens_it() {
    let case =
        Case::two_works().vessel("build", ready()).crew("build", "coder", DONE).vessel("review", ready()).crew("review", "reviewer", DONE);
    assert_eq!(case.derive().phase, ConvoyPhase::Landing);
    // A new promise, or a rejected submission back to Pending, reopens the work and the convoy.
    let reopened = CrewReading { promises: 2, open: 1, ..WORKING };
    let reading = case.crew("review", "reviewer", reopened).derive();
    assert_eq!(reading.work["review"], WorkPhase::Running);
    assert_eq!(reading.crew["review"]["reviewer"], CrewWorkPhase::Working);
    assert_eq!(reading.phase, ConvoyPhase::Active);
}

#[test]
fn a_work_completes_when_every_promise_its_seats_own_settles() {
    // Retracted promises are settled too: the reading counts only the open ones.
    let case = Case::two_works()
        .vessel("build", ready())
        .crew("build", "coder", CrewReading { promises: 3, open: 1, ..WORKING })
        .crew("review", "reviewer", OWING);
    assert_eq!(case.derive().work["build"], WorkPhase::Running);
    let reading = case.crew("build", "coder", CrewReading { promises: 3, open: 0, ..WORKING }).derive();
    assert_eq!(reading.work["build"], WorkPhase::Complete);
    assert_eq!(reading.crew["build"]["coder"], CrewWorkPhase::Done);
    // The dependant's promises are still open: Landing waits for every seat.
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
fn a_failed_crew_member_fails_its_work_and_convoy_standing_or_not() {
    // A standing role's generation ends failed too: a strike, and its next launch replaces it.
    let failed = CrewReading { failed: true, ..WORKING };
    let reading = Case::two_works().vessel("build", ready()).crew("build", "coder", failed).derive();
    assert_eq!(reading.crew["build"]["coder"], CrewWorkPhase::Failed);
    assert_eq!(reading.work["build"], WorkPhase::Failed);
    assert_eq!(reading.phase, ConvoyPhase::Failed);
}

#[test]
fn a_failed_vessel_fails_its_convoy_and_a_vanished_one_interrupts_it() {
    // Failed (after the Vessel's own finite retries) costs a standing role a strike: the convoy
    // fails and the role's next launch replaces it. Lost is replaced within the convoy.
    let derived = Case::two_works().vessel("build", VesselReading::At(VesselPhase::Failed)).derive();
    assert_eq!(derived.work["build"], WorkPhase::Failed);
    assert_eq!(derived.phase, ConvoyPhase::Failed);
    let derived = Case::two_works().vessel("build", VesselReading::Gone).derive();
    assert_eq!(derived.work["build"], WorkPhase::Interrupted);
    assert_eq!(derived.phase, ConvoyPhase::Interrupted);
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
fn a_handed_back_seat_owes_nothing_and_the_seat_it_handed_to_counts() {
    // The handoff moved the coder's promises to a seat the template does not name.
    let handed = CrewReading { promises: 0, open: 0, handed_back: true, ..WORKING };
    let mut case = Case::two_works();
    case.snapshot = snapshot(vec![requirement("build", &[], &["coder"])]);
    let case = case.vessel("build", ready()).crew("build", "coder", handed).crew("build", "fixer", WORKING);
    let reading = case.derive();
    assert_eq!(reading.crew["build"]["coder"], CrewWorkPhase::HandedBack);
    assert_eq!(reading.crew["build"]["fixer"], CrewWorkPhase::Working);
    assert_eq!(reading.work["build"], WorkPhase::Running);
    let reading = case.crew("build", "fixer", DONE).derive();
    assert_eq!(reading.work["build"], WorkPhase::Complete);
    assert_eq!(reading.phase, ConvoyPhase::Landing);
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
    // Running seats owing nothing yet: no promise exists, so nothing lands.
    let reading =
        Case::two_works().vessel("build", ready()).crew("build", "coder", CrewReading { promises: 0, open: 0, ..WORKING }).derive();
    assert_eq!(reading.work["build"], WorkPhase::Running);
    assert_eq!(reading.phase, ConvoyPhase::Active);
}

#[test]
fn a_work_owing_no_promise_completes_nothing_and_releases_its_dependant_once_it_runs() {
    // `build` is a tool vessel whose seat owns no promise; `review` owes one.
    let case = Case::two_works().vessel("build", VesselReading::At(VesselPhase::Provisioning)).crew("review", "reviewer", WORKING);
    assert_eq!(case.derive().work["review"], WorkPhase::Pending);
    let case = case.vessel("build", ready());
    let reading = case.derive();
    assert_eq!(reading.work["build"], WorkPhase::Running);
    assert_eq!(reading.work["review"], WorkPhase::Ready);
    // It gates nothing: the convoy lands on the promises the other seats own.
    let reading = case.vessel("review", ready()).crew("review", "reviewer", DONE).derive();
    assert_eq!(reading.work["build"], WorkPhase::Running);
    assert_eq!(reading.phase, ConvoyPhase::Landing);
}

#[test]
fn a_tool_seat_owning_a_ledger_lands_when_it_is_kept() {
    // The builtin `scratch`: one tool vessel, its `shell` seat owning a decision-ledger promise.
    let mut case = Case::two_works();
    case.snapshot = snapshot(vec![requirement("work", &[], &["shell"])]);
    let case = case.vessel("work", ready()).crew("work", "shell", WORKING);
    assert_eq!(case.derive().phase, ConvoyPhase::Active);
    let reading = case.crew("work", "shell", DONE).derive();
    assert_eq!(reading.work["work"], WorkPhase::Complete);
    assert_eq!(reading.phase, ConvoyPhase::Landing);
}
