use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeZone, Utc};
use flotilla_protocol::PrincipalRef;

use super::{ConvoyEnd, ConvoyJoins, ConvoyNotes, ConvoyRecord, CrewNotes, SeatRecord, WorkFacts};
use crate::{
    convoy::{
        admission::AdmissionDocument,
        promises::{Promise, PromiseKind, PromiseSource, PromiseState},
        view::VesselReading,
        ConvoyAttention, ConvoyPhase, CrewWorkPhase, WorkPhase, WorkflowSnapshot,
    },
    labels::{PROJECT_LABEL, ROLE_LABEL},
    workflow_template::{CrewSource, CrewSpec, Selector, VesselRequirement},
    VesselPhase,
};

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("time")
}

fn agent(role: &str) -> CrewSpec {
    CrewSpec::builder()
        .role(role.to_owned())
        .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
        .build()
}

/// One work, `work`, crewed by `coder` (eager: the first agent) and `reviewer` (latent).
fn snapshot() -> WorkflowSnapshot {
    WorkflowSnapshot {
        cascade: None,
        exit: None,
        turn_delivery: Default::default(),
        stall_nudges: Default::default(),
        supervision: None,
        vessels: vec![VesselRequirement::builder().name("work".to_owned()).crew(vec![agent("coder"), agent("reviewer")]).build()],
    }
}

fn promise(role: &str, state: PromiseState) -> Promise {
    Promise {
        id: format!("template/{role}"),
        vessel: "work".into(),
        role: role.into(),
        source: PromiseSource::Template,
        kind: PromiseKind::Pr,
        state,
        retraction_reason: None,
        submissions: Vec::new(),
    }
}

fn record(coder: PromiseState) -> ConvoyRecord {
    ConvoyRecord {
        name: "fix-it#7".into(),
        home: "home-a".into(),
        version: 12,
        launch_of: "fix-it".into(),
        launch: 7,
        project: "flotilla".into(),
        workflow_ref: "single".into(),
        admission: AdmissionDocument {
            workflow_snapshot: snapshot(),
            placement_decision: None,
            placement_policy: None,
            role: "fixer".into(),
            ensure_admission: None,
            naming: None,
            dispatching_principal_ref: PrincipalRef { namespace: "flotilla".into(), name: "robert".into() },
            continuation: None,
            inputs: BTreeMap::new(),
            repositories: Vec::new(),
            r#ref: None,
            adopted_checkout_refs: BTreeMap::new(),
            subjects: Vec::new(),
            instruction: None,
            workflow_template: None,
            placement_snapshot: None,
            vessel_placements: BTreeMap::new(),
            brief_artifacts: false,
            adopted_checkout: None,
            provenance: BTreeMap::new(),
        },
        admitted_at: at(100),
        ended: None,
        seats: BTreeMap::new(),
        promises: BTreeMap::from([("work".into(), BTreeMap::from([("coder".into(), vec![promise("coder", coder)])]))]),
        subjects: Vec::new(),
        unlinked: Vec::new(),
    }
}

fn ready() -> BTreeMap<String, WorkFacts> {
    BTreeMap::from([(
        "work".into(),
        WorkFacts {
            reading: VesselReading::At(VesselPhase::Ready),
            ready_at: None,
            started_at: Some(at(110)),
            finished_at: None,
            message: None,
        },
    )])
}

fn seat(work: &str, role: &str) -> (String, String) {
    (work.to_owned(), role.to_owned())
}

// Behaviour: a launch reads as an old Convoy named by its launch, its phases derived from its
// records and the facts joined to it; its identity comes from the launch and its admission.
#[test]
fn a_launch_reads_its_phases_from_its_records_and_joins() {
    let record = record(PromiseState::Open);
    let live = BTreeSet::from([seat("work", "coder"), seat("work", "reviewer")]);
    let working = record.working(&live, None);
    assert_eq!(working, BTreeSet::from([seat("work", "coder")]), "only the eager seat works from its launch");

    let convoy = record.read_view("flotilla", ConvoyJoins { works: &ready(), working: &working, notes: None });
    assert_eq!(convoy.metadata.name, "fix-it#7");
    assert_eq!(convoy.metadata.resource_version, "12");
    assert_eq!(convoy.metadata.creation_timestamp, at(100));
    assert_eq!(convoy.metadata.labels.get(PROJECT_LABEL).map(String::as_str), Some("flotilla"));
    assert_eq!(convoy.metadata.labels.get(ROLE_LABEL).map(String::as_str), Some("fixer"));
    assert_eq!((convoy.spec.generation, convoy.spec.project_ref.as_deref()), (7, Some("flotilla")));
    let status = convoy.status.expect("status");
    assert_eq!(status.phase, ConvoyPhase::Active);
    assert_eq!(status.work["work"].phase, WorkPhase::Running);
    assert_eq!(status.work["work"].started_at, Some(at(110)));
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Pending);
    assert_eq!((status.started_at, status.finished_at), (Some(at(100)), None));
    assert_eq!(status.observed_workflow_ref.as_deref(), Some("single"));
}

// Behaviour: a latent seat works once its home has delivered its task, which only that home
// knows; its home's notes also carry what it observes, such as attention.
#[test]
fn a_latent_seat_works_once_its_task_is_delivered() {
    let record = record(PromiseState::Open);
    let live = BTreeSet::from([seat("work", "coder"), seat("work", "reviewer")]);
    let notes = ConvoyNotes {
        attention: Some(ConvoyAttention { source: "turn".into(), reason: "a turn is queued".into(), raised_at: at(120) }),
        crew: BTreeMap::from([(
            "work".into(),
            BTreeMap::from([("reviewer".into(), CrewNotes { task_assigned: true, ..CrewNotes::default() })]),
        )]),
        ..ConvoyNotes::default()
    };
    let working = record.working(&live, Some(&notes));
    assert_eq!(working, live);
    let status =
        record.read_view("flotilla", ConvoyJoins { works: &ready(), working: &working, notes: Some(&notes) }).status.expect("status");
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Working);
    assert!(status.crew_work["work"]["reviewer"].task_assigned);
    assert_eq!(status.attention.map(|attention| attention.reason), Some("a turn is queued".into()));
}

// Behaviour (#3108 C3'): an old status patch written to a home's notes keeps what the notes hold
// (here a handoff's task assignment) and drops what the view derives (the crew's phase), so the
// next view shows the note and its own phases.
#[test]
fn a_patch_keeps_only_what_the_notes_hold() {
    let record = record(PromiseState::Open);
    let live = BTreeSet::from([seat("work", "coder"), seat("work", "reviewer")]);
    let working = record.working(&live, None);
    let view = record.read_view("flotilla", ConvoyJoins { works: &ready(), working: &working, notes: None }).status.expect("status");
    let mut notes = ConvoyNotes::default();
    notes.apply(&view, &crate::external_patches::handoff_crew_work("work".into(), "coder".into(), "reviewer".into(), at(130), "go".into()));
    assert!(notes.crew["work"]["reviewer"].task_assigned, "the handoff assigned the reviewer its task");
    assert_eq!(notes.attention, None);
    let working = record.working(&live, Some(&notes));
    let status =
        record.read_view("flotilla", ConvoyJoins { works: &ready(), working: &working, notes: Some(&notes) }).status.expect("status");
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Working, "the view derives the phase from the note");
    assert!(status.crew_work["work"]["reviewer"].message.is_none(), "the patch's message is not a note");
}

// Behaviour: a seat's verdict fails its work and the convoy, with the verdict's reason on the
// seat; how a launch ended is read from its end, with the end's reason or disposition.
#[test]
fn a_launch_reads_its_verdicts_and_how_it_ended() {
    let mut failed = record(PromiseState::Open);
    failed.seats = BTreeMap::from([(
        "work".into(),
        BTreeMap::from([("coder".into(), SeatRecord { failed: Some("it looped".into()), ..SeatRecord::default() })]),
    )]);
    let none = BTreeSet::new();
    let status = failed.read_view("flotilla", ConvoyJoins { works: &ready(), working: &none, notes: None }).status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Failed);
    assert_eq!(status.crew_work["work"]["coder"].message.as_deref(), Some("it looped"));
    assert_eq!(status.work["work"].phase, WorkPhase::Failed);
    assert_eq!(status.phase, ConvoyPhase::Failed);

    failed.ended = Some((ConvoyEnd::Failed { reason: "coder failed".into() }, at(200)));
    let status = failed.read_view("flotilla", ConvoyJoins { works: &ready(), working: &none, notes: None }).status.expect("status");
    assert_eq!((status.phase, status.message.as_deref(), status.finished_at), (ConvoyPhase::Failed, Some("coder failed"), Some(at(200))));

    let mut delivered = record(PromiseState::Kept);
    delivered.ended = Some((ConvoyEnd::Delivered { disposition: "landed".into() }, at(300)));
    let status = delivered.read_view("flotilla", ConvoyJoins { works: &ready(), working: &none, notes: None }).status.expect("status");
    assert_eq!((status.phase, status.disposition.as_deref(), status.finished_at), (ConvoyPhase::Landed, Some("landed"), Some(at(300))));
    assert_eq!(status.work["work"].phase, WorkPhase::Complete);
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Done);
}

// Behaviour (#3108): a seat the launch's end overtook reads the end, with its disposition or
// reason, never Working or Failed; a seat that finished before the end keeps its own reading.
#[test]
fn a_seat_the_end_overtook_reads_the_end() {
    let none = BTreeSet::new();
    // Delivered: the reviewer owed no promise and was never given its task.
    let mut delivered = record(PromiseState::Kept);
    delivered.ended = Some((ConvoyEnd::Delivered { disposition: "merged".into() }, at(300)));
    let status = delivered.read_view("flotilla", ConvoyJoins { works: &ready(), working: &none, notes: None }).status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Done);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Ended);
    assert_eq!(status.crew_work["work"]["reviewer"].message.as_deref(), Some("merged"));

    // Abandoned while the coder was working: it did not fail.
    let mut abandoned = record(PromiseState::Open);
    abandoned.ended = Some((ConvoyEnd::Abandoned { reason: "operator stopped it".into() }, at(300)));
    let working = BTreeSet::from([seat("work", "coder")]);
    let status = abandoned.read_view("flotilla", ConvoyJoins { works: &ready(), working: &working, notes: None }).status.expect("status");
    assert_eq!(status.phase, ConvoyPhase::Abandoned);
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Ended);
    assert_eq!(status.crew_work["work"]["coder"].message.as_deref(), Some("operator stopped it"));

    // A seat that failed before the end stays Failed, with its own reason.
    let mut failed = record(PromiseState::Open);
    failed.seats = BTreeMap::from([(
        "work".into(),
        BTreeMap::from([("coder".into(), SeatRecord { failed: Some("it looped".into()), ..SeatRecord::default() })]),
    )]);
    failed.ended = Some((ConvoyEnd::Failed { reason: "coder failed".into() }, at(200)));
    let status = failed.read_view("flotilla", ConvoyJoins { works: &ready(), working: &none, notes: None }).status.expect("status");
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Failed);
    assert_eq!(status.crew_work["work"]["coder"].message.as_deref(), Some("it looped"));
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Ended);
}
